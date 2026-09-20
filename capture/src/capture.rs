//! UHD device discovery, configuration, and IQ sample streaming.
//!
//! Wraps the `uhd` crate: find B210, configure RF, stream sc16 IQ samples.

use num_complex::Complex;
use sigproc_common::config::RfConfig;
use uhd::{
    ReceiveStreamer, StreamArgs, StreamCommand, StreamCommandType, StreamTime, TuneRequest, Usrp,
};

/// Maximum samples per read call from the USRP.
pub const SAMPLES_PER_READ: usize = 2048;

/// Info about a discovered UHD device.
#[derive(Debug)]
pub struct DeviceInfo {
    /// USRP device identification string (motherboard name).
    pub mboard_name: String,
    /// Number of RX channels available.
    pub num_rx_channels: usize,
}

/// Probe the system for UHD devices and return info about the first one found.
/// Returns an error if no devices are discovered.
pub fn probe_device(device_args: &str) -> anyhow::Result<DeviceInfo> {
    let addresses = Usrp::find(device_args)?;
    if addresses.is_empty() {
        anyhow::bail!("no UHD devices found");
    }

    let usrp = Usrp::open(device_args)?;
    let mboard_name = usrp.get_motherboard_name(0)?;
    let num_rx_channels = usrp.get_num_rx_channels()?;

    log::info!(
        "Found {} device(s): mboard={}, rx_channels={}",
        addresses.len(),
        mboard_name,
        num_rx_channels
    );

    Ok(DeviceInfo {
        mboard_name,
        num_rx_channels,
    })
}

/// Open a USRP, configure RF parameters, and return (device, negotiated_sample_rate).
/// Caller must then create a streamer with `create_rx_streamer()`.
pub fn open_usrp(device_args: &str, rf: &RfConfig) -> anyhow::Result<(Usrp, f64)> {
    let addresses = Usrp::find(device_args)?;
    if addresses.is_empty() {
        anyhow::bail!("no UHD devices found — is the B210 connected and powered?");
    }
    log::info!("UHD device addresses: {:?}", addresses);

    let mut usrp = Usrp::open(device_args)?;
    let mboard = usrp.get_motherboard_name(0)?;
    log::info!("Opened USRP: {}", mboard);

    // Configure RF parameters
    usrp.set_rx_sample_rate(rf.sample_rate, 0)?;
    usrp.set_rx_frequency(&TuneRequest::with_frequency(rf.center_freq), 0)?;
    usrp.set_rx_gain(rf.rx_gain, 0, "")?; // default gain element
    usrp.set_rx_antenna(&rf.antenna, 0)?;
    usrp.set_rx_bandwidth(rf.bandwidth, 0)?;

    let actual_rate = usrp.get_rx_sample_rate(0)?;
    let actual_freq = usrp.get_rx_frequency(0)?;
    let actual_gain = usrp.get_rx_gain(0, "")?;
    let actual_bw = usrp.get_rx_bandwidth(0)?;
    let actual_antenna = usrp.get_rx_antenna(0)?;

    log::info!("USRP configured:");
    log::info!(
        "  sample_rate: {} Hz (requested {})",
        actual_rate,
        rf.sample_rate
    );
    log::info!(
        "  center_freq: {} Hz (requested {})",
        actual_freq,
        rf.center_freq
    );
    log::info!(
        "  rx_gain:     {} dB (requested {})",
        actual_gain,
        rf.rx_gain
    );
    log::info!(
        "  bandwidth:   {} Hz (requested {})",
        actual_bw,
        rf.bandwidth
    );
    log::info!(
        "  antenna:     {} (requested {})",
        actual_antenna,
        rf.antenna
    );

    Ok((usrp, actual_rate))
}

/// Create and start a continuous RX streamer for sc16 IQ samples.
pub fn create_rx_streamer(usrp: &mut Usrp) -> anyhow::Result<ReceiveStreamer<'_, Complex<i16>>> {
    let mut streamer = usrp.get_rx_stream(&StreamArgs::<Complex<i16>>::new("sc16"))?;
    streamer.send_command(&StreamCommand {
        time: StreamTime::Now,
        command_type: StreamCommandType::StartContinuous,
    })?;
    log::info!("RX stream started (sc16, continuous)");
    Ok(streamer)
}

/// Read one batch of sc16 IQ samples from the streamer.
///
/// Returns `(timestamp_secs, interleaved_i16)` where samples are `[I0, Q0, I1, Q1, ...]`.
/// Returns `Ok(None)` if no samples arrived within the 0.1s timeout.
pub fn read_batch(
    streamer: &mut ReceiveStreamer<'_, Complex<i16>>,
) -> anyhow::Result<Option<(f64, Vec<i16>)>> {
    let mut buf = vec![Complex::new(0i16, 0i16); SAMPLES_PER_READ];
    let metadata = streamer.receive(&mut [&mut buf], 0.1, false)?;

    let num = metadata.samples();
    if num == 0 {
        return Ok(None);
    }

    if let Some(err) = metadata.last_error() {
        log::warn!("UHD RX error: {}", err);
    }

    let ts = metadata
        .time_spec()
        .map(|ts| ts.seconds as f64 + ts.fraction)
        .unwrap_or(0.0);

    // Complex<i16> → interleaved i16 for VITA49
    let mut samples = Vec::with_capacity(num * 2);
    for c in buf.iter().take(num) {
        samples.push(c.re);
        samples.push(c.im);
    }

    Ok(Some((ts, samples)))
}
