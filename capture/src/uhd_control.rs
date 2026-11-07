//! Adapter for the sole UHD owner; the RX streamer must be dropped before use.
use sigproc_common::vita49::{ControlBackend, ControlUpdate, StreamContext};
use uhd::{TuneRequest, Usrp};

pub struct UhdControl<'a>(pub &'a mut Usrp);

pub fn readback(usrp: &Usrp) -> anyhow::Result<StreamContext> {
    Ok(StreamContext {
        rf_freq_hz: usrp.get_rx_frequency(0)?,
        sample_rate_hz: usrp.get_rx_sample_rate(0)?,
        bandwidth_hz: usrp.get_rx_bandwidth(0)?,
        gain_db: usrp.get_rx_gain(0, "")?,
    })
}

impl ControlBackend for UhdControl<'_> {
    fn apply_and_readback(&mut self, update: ControlUpdate) -> anyhow::Result<StreamContext> {
        if let Some(rate) = update.sample_rate_hz {
            self.0.set_rx_sample_rate(rate, 0)?;
        }
        if let Some(hz) = update.rf_freq_hz {
            self.0
                .set_rx_frequency(&TuneRequest::with_frequency(hz), 0)?;
        }
        if let Some(hz) = update.bandwidth_hz {
            self.0.set_rx_bandwidth(hz, 0)?;
        }
        if let Some(gain) = update.gain_db {
            self.0.set_rx_gain(gain, 0, "")?;
        }
        readback(self.0)
    }
}
