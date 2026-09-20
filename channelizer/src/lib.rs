//! The channelizer pipeline, separated from the binary so the whole path can be
//! exercised end-to-end without sockets or a cluster.
//!
//! Per input packet the service: parses VITA49 strictly, tracks the stream's
//! sample counter for loss, mixes the channel to baseband and decimates, then
//! re-frames the narrowband samples as VITA49 for the next hop.
//!
//! Three behaviours here are deliberate and load-bearing:
//!
//! 1. **A gap in the input advances the output timeline.** The narrowband stream
//!    is timestamped from a running output counter, so an intact input produces
//!    exactly contiguous output timestamps. When input samples are lost, the
//!    output counter skips the corresponding missing time instead of quietly
//!    closing the hole — otherwise downstream audio would splice across the loss
//!    and look perfectly continuous.
//! 2. **Level and offset gauges are not published until the filters have
//!    settled.** Averaged over the startup transient, an out-of-channel
//!    interferer that is genuinely suppressed by 115 dB reads as ~74 dB (see
//!    [`sigproc_common::dsp::Channelizer::settling_samples`]), so a short-block
//!    metric near startup would be wrong by a factor of a thousand in power.
//! 3. **A malformed packet is counted, not fatal.** One corrupt datagram must not
//!    take down a service that is holding a live channel open.

use serde::Serialize;
use sigproc_common::dsp::{
    estimate_frequency_keeping_dc, iq_to_sc16, power_dbfs, Channelizer, GapTracker, Iq, StagePlan,
};
use sigproc_common::metrics::Metrics;
use sigproc_common::vita49;
use std::sync::Arc;

/// Output samples accumulated before a narrowband packet is sent. Keeps packets
/// bounded instead of emitting one datagram per input packet (which, at 40x
/// decimation, would be ~50 samples each).
pub const OUTPUT_PACKET_SAMPLES: usize = 1024;

/// Output samples accumulated before level/offset are measured.
pub const MEASURE_BLOCK: usize = 4096;

/// Where narrowband samples go.
pub trait OutputSink {
    /// Send one narrowband packet: interleaved sc16, its first sample's
    /// timestamp in seconds, and the narrowband sample rate.
    fn send(&mut self, interleaved_sc16: &[i16], ts_secs: f64, sample_rate: f64);

    /// Packets dropped so far by this sink. Monotonic.
    fn dropped_packets(&self) -> u64;
}

impl OutputSink for vita49::Vita49Forwarder {
    fn send(&mut self, samples: &[i16], ts_secs: f64, sample_rate: f64) {
        vita49::Vita49Forwarder::send(self, samples, ts_secs, sample_rate);
    }

    fn dropped_packets(&self) -> u64 {
        vita49::Vita49Forwarder::dropped_packets(self)
    }
}

/// Collects output in memory. Used by the integration tests and by any caller
/// that wants the narrowband samples without a socket.
#[derive(Default, Debug)]
pub struct CollectingSink {
    /// One entry per sent packet: (interleaved sc16, first-sample timestamp).
    pub packets: Vec<(Vec<i16>, f64)>,
    /// Narrowband sample rate seen on the last send.
    pub rate: f64,
}

impl CollectingSink {
    /// All narrowband samples received, in order.
    pub fn samples(&self) -> Vec<Iq> {
        let mut all = Vec::new();
        for (sc16, _) in &self.packets {
            all.extend(sigproc_common::dsp::sc16_to_iq(sc16));
        }
        all
    }

    /// Timestamps of each packet, in order.
    pub fn timestamps(&self) -> Vec<f64> {
        self.packets.iter().map(|(_, ts)| *ts).collect()
    }
}

impl OutputSink for CollectingSink {
    fn send(&mut self, samples: &[i16], ts_secs: f64, sample_rate: f64) {
        self.rate = sample_rate;
        self.packets.push((samples.to_vec(), ts_secs));
    }

    fn dropped_packets(&self) -> u64 {
        0
    }
}

/// A detected discontinuity in the input stream, placed on both timelines.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct GapRecord {
    /// Input sample counter at which the stream resumed.
    pub at_input_sample: u64,
    /// Input samples missing before it.
    pub missing_samples: u64,
    /// Output sample index the discontinuity was applied at — the point in the
    /// narrowband timeline that must not be treated as continuous.
    pub at_output_sample: u64,
}

/// What one input packet produced.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PacketOutcome {
    pub samples_in: usize,
    pub samples_out: usize,
    /// Set when this packet revealed a discontinuity.
    pub gap: Option<GapRecord>,
    /// Set when the packet could not be parsed.
    pub malformed: bool,
}

/// Machine-readable result of a run, for the gates and for the pod log.
#[derive(Debug, Clone, Serialize)]
pub struct Report {
    /// True only when every packet parsed, none were lost downstream, and the
    /// input stream had no discontinuity.
    pub ok: bool,
    pub packets_received: u64,
    pub malformed_packets: u64,
    pub packets_dropped: u64,
    pub seq_gaps: u64,
    pub gap_samples: u64,
    pub samples_in: u64,
    pub samples_out: u64,
    pub input_rate_hz: f64,
    pub output_rate_hz: f64,
    pub decimation: usize,
    /// Input samples that must be discarded before level/offset are meaningful.
    pub settling_samples: usize,
    pub offset_hz: f64,
    pub passband_hz: f64,
    pub stopband_db: f64,
    /// `null` until the filters have settled and a measurement block has filled.
    pub output_power_dbfs: Option<f32>,
    pub peak_offset_hz: Option<f32>,
    pub discontinuities: Vec<GapRecord>,
}

/// Configuration the service needs beyond the stage plan.
#[derive(Debug, Clone)]
pub struct ServiceConfig {
    pub input_rate: f64,
    pub output_rate: f64,
    pub offset_hz: f64,
    pub passband_hz: f64,
    pub stopband_db: f64,
}

impl ServiceConfig {
    /// Derive the stage plan and service configuration from a `[channel]` section.
    ///
    /// Single source of truth for the mapping: the binary and the tests both go
    /// through here, so a test can never exercise a differently-planned chain than
    /// the one that ships.
    pub fn from_channel(
        channel: &sigproc_common::config::ChannelConfig,
        input_rate: f64,
    ) -> anyhow::Result<(Vec<StagePlan>, ServiceConfig)> {
        let plan = channel.plan(input_rate)?;
        let cfg = ServiceConfig {
            input_rate,
            output_rate: channel.output_rate_hz,
            offset_hz: channel.offset_hz,
            passband_hz: channel.passband(),
            stopband_db: channel.stopband_db,
        };
        Ok((plan, cfg))
    }
}

/// The channelizer pipeline: VITA49 in, filtered and decimated VITA49 out.
pub struct ChannelizerService<S: OutputSink> {
    channelizer: Channelizer,
    plan: Vec<StagePlan>,
    cfg: ServiceConfig,
    sink: S,
    metrics: Arc<Metrics>,
    gap_tracker: GapTracker,
    samples_in_total: u64,
    samples_out_total: u64,
    malformed: u64,
    discontinuities: Vec<GapRecord>,
    /// Narrowband samples waiting to be packetized.
    out_packet: Vec<Iq>,
    /// Output index of the first sample in `out_packet`.
    out_packet_first: u64,
    /// Rolling window for level/offset measurement.
    measure: Vec<Iq>,
    /// Sink drop count already mirrored into the metrics.
    sink_dropped_seen: u64,
    /// Scratch buffer reused per packet to avoid a per-packet allocation.
    produced: Vec<Iq>,
}

impl<S: OutputSink> ChannelizerService<S> {
    /// Build the service from a stage plan and channel parameters.
    pub fn new(plan: Vec<StagePlan>, cfg: ServiceConfig, sink: S, metrics: Arc<Metrics>) -> Self {
        let channelizer =
            Channelizer::new(plan.clone(), cfg.offset_hz, cfg.input_rate, cfg.stopband_db);
        ChannelizerService {
            channelizer,
            plan,
            cfg,
            sink,
            metrics,
            gap_tracker: GapTracker::new(),
            samples_in_total: 0,
            samples_out_total: 0,
            malformed: 0,
            discontinuities: Vec::new(),
            out_packet: Vec::with_capacity(OUTPUT_PACKET_SAMPLES),
            out_packet_first: 0,
            measure: Vec::with_capacity(MEASURE_BLOCK),
            sink_dropped_seen: 0,
            produced: Vec::new(),
        }
    }

    /// Input samples the filters need before their output is steady state.
    pub fn settling_samples(&self) -> usize {
        self.channelizer.settling_samples()
    }

    /// Record a packet that could not even be framed enough to reach
    /// [`Self::process_datagram`].
    ///
    /// Without this, a replay file containing an unframeable record would be
    /// reported as `ok` while the very same bytes arriving over UDP would be
    /// counted — the verdict would depend on how the data arrived, which is
    /// exactly the kind of difference a gate must not have.
    pub fn note_malformed(&mut self, reason: &str) {
        self.malformed += 1;
        log::warn!("malformed VITA49 packet dropped: {reason}");
    }

    /// Feed one raw VITA49 datagram or file packet.
    pub fn process_datagram(&mut self, buf: &[u8]) -> PacketOutcome {
        let packet = match vita49::parse(buf) {
            Ok(p) => p,
            Err(e) => {
                // Counted, not fatal: one corrupt datagram must not close a
                // channel that is otherwise live.
                self.note_malformed(&e.to_string());
                return PacketOutcome {
                    malformed: true,
                    ..Default::default()
                };
            }
        };

        let samples_in = packet.samples();
        let counter = packet.sample_counter(self.cfg.input_rate);
        let gap = self
            .gap_tracker
            .observe(counter, samples_in as u64)
            .map(|m| {
                // Place the discontinuity on the output timeline too, so downstream
                // consumers see a hole rather than a seamless splice.
                self.flush_output_packet();
                let missing_out = m
                    .missing_samples
                    .div_ceil(self.channelizer.decimation() as u64);
                let record = GapRecord {
                    at_input_sample: m.at_sample,
                    missing_samples: m.missing_samples,
                    at_output_sample: self.samples_out_total + missing_out,
                };
                self.samples_out_total += missing_out;
                self.metrics.record_gap(m.missing_samples);
                log::warn!(
                    "input discontinuity: {} samples missing before input sample {} \
                 (output timeline advanced by {missing_out} samples)",
                    m.missing_samples,
                    m.at_sample
                );
                self.discontinuities.push(record.clone());
                record
            });

        let interleaved = packet.iq_i16();
        self.process_interleaved(&interleaved);

        self.metrics.record_packet_received();
        self.metrics
            .record_samples(samples_in as u64, self.produced.len() as u64);
        self.samples_in_total += samples_in as u64;

        PacketOutcome {
            samples_in,
            samples_out: self.produced.len(),
            gap,
            malformed: false,
        }
    }

    /// Feed already-decoded interleaved sc16 (used by tests and by callers that
    /// have already split a file).
    pub fn process_interleaved(&mut self, interleaved_sc16: &[i16]) {
        // Take the scratch buffer out for the duration: flushing an output packet
        // needs `&mut self`, which cannot be held while iterating a field of it.
        let mut produced = std::mem::take(&mut self.produced);
        produced.clear();
        self.channelizer.process(interleaved_sc16, &mut produced);

        for sample in &produced {
            if self.out_packet.is_empty() {
                self.out_packet_first = self.samples_out_total + self.out_packet.len() as u64;
            }
            self.out_packet.push(*sample);
            self.samples_out_total += 1;
            if self.out_packet.len() >= OUTPUT_PACKET_SAMPLES {
                self.flush_output_packet();
            }
        }

        // Measurement only once the filters have settled.
        if self.samples_in_total + (interleaved_sc16.len() / 2) as u64
            >= self.settling_samples() as u64
        {
            self.measure.extend_from_slice(&produced);
            if self.measure.len() >= MEASURE_BLOCK {
                self.publish_measurements();
            }
        }

        self.produced = produced;
    }

    fn publish_measurements(&mut self) {
        let block = std::mem::take(&mut self.measure);
        self.measure = Vec::with_capacity(MEASURE_BLOCK);

        let power = power_dbfs(&block);
        if power.is_finite() {
            self.metrics.set_output_power_dbfs(power);
        }

        // Strongest component in the channel, *including* its carrier: an AM
        // carrier sits at DC, so a DC-removed estimate would report a sideband
        // (±the modulation tone) and make a perfectly centred channel look
        // 1 kHz off. 0 Hz here means "centred on the signal".
        let band = self.cfg.passband_hz;
        if let Some(offset) =
            estimate_frequency_keeping_dc(&block, self.cfg.output_rate, -band, band)
        {
            self.metrics.set_peak_offset_hz(offset as f32);
        }
    }

    fn flush_output_packet(&mut self) {
        if self.out_packet.is_empty() {
            return;
        }
        let sc16 = iq_to_sc16(&self.out_packet);
        let ts = self.out_packet_first as f64 / self.cfg.output_rate;
        self.sink.send(&sc16, ts, self.cfg.output_rate);
        self.out_packet.clear();
        self.out_packet_first = self.samples_out_total;

        let dropped = self.sink.dropped_packets();
        if dropped > self.sink_dropped_seen {
            self.metrics
                .record_packets_dropped(dropped - self.sink_dropped_seen);
            self.sink_dropped_seen = dropped;
        }
    }

    /// Number of narrowband samples emitted so far.
    pub fn samples_out(&self) -> u64 {
        self.samples_out_total
    }

    /// The discontinuities seen so far.
    pub fn discontinuities(&self) -> &[GapRecord] {
        &self.discontinuities
    }

    /// Finish the stream: flush the partial packet and produce the report,
    /// handing the sink back so a caller can inspect what was sent.
    pub fn finish_with_sink(mut self) -> (Report, S) {
        self.flush_output_packet();
        let metrics = &self.metrics;
        let dropped = self.sink.dropped_packets();
        if dropped > self.sink_dropped_seen {
            metrics.record_packets_dropped(dropped - self.sink_dropped_seen);
            self.sink_dropped_seen = dropped;
        }

        let output_power = metrics.output_power_dbfs();
        let peak_offset = metrics.peak_offset_hz();
        let report = Report {
            ok: self.malformed == 0 && dropped == 0 && metrics.seq_gaps() == 0,
            packets_received: metrics.packets_received(),
            malformed_packets: self.malformed,
            packets_dropped: dropped,
            seq_gaps: metrics.seq_gaps(),
            gap_samples: metrics.gap_samples(),
            samples_in: self.samples_in_total,
            samples_out: self.samples_out_total,
            input_rate_hz: self.cfg.input_rate,
            output_rate_hz: self.cfg.output_rate,
            decimation: self.channelizer.decimation(),
            settling_samples: self.channelizer.settling_samples(),
            offset_hz: self.cfg.offset_hz,
            passband_hz: self.cfg.passband_hz,
            stopband_db: self.cfg.stopband_db,
            output_power_dbfs: output_power.is_finite().then_some(output_power),
            peak_offset_hz: peak_offset.is_finite().then_some(peak_offset),
            discontinuities: self.discontinuities.clone(),
        };
        (report, self.sink)
    }

    /// Finish the stream, discarding the sink.
    pub fn finish(self) -> Report {
        self.finish_with_sink().0
    }

    /// The stage plan, for logging the resolved channel plan at startup.
    pub fn plan(&self) -> &[StagePlan] {
        &self.plan
    }
}
