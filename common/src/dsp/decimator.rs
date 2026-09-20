//! Integer multi-stage decimation: the channelizer's rate conversion.
//!
//! # Why stages, and why this rule
//!
//! Isolating a ~50 kHz channel out of a 2 MS/s stream with ONE FIR would need a
//! transition band that is a tiny fraction of the *input* rate — Kaiser's own
//! estimate puts that at roughly two thousand taps, i.e. ~4 GFLOP/s. Splitting
//! the rate change into stages makes each stage's filter cheap.
//!
//! The subtle part is that intermediate stages do **not** need to be
//! alias-free across their whole output band, only across the final channel:
//! content that folds down outside the wanted band is removed by the next
//! stage. So for every stage:
//!
//! * passband edge = the final channel half-width,
//! * stopband edge = (this stage's output rate) - (the final stopband edge),
//!
//! which pushes each intermediate stopband far from its passband and collapses
//! the tap count. The last stage is the exception and must genuinely stop at
//! `output_rate / 2`, because after it there is no further filtering.
//!
//! Worked example for 2 MS/s -> 50 kS/s (passband 20 kHz, stopband 25 kHz):
//! `[10, 4]`, needing ~65 taps for the 2 MS/s stage and ~201 for the 200 kS/s
//! stage — roughly 170 MMAC/s in total, which is what makes this real-time on a
//! modest node instead of a CPU bonfire. The planner prefers the largest usable
//! factor first precisely because that keeps later stages at lower rates.
//!
//! # State
//!
//! Mixer phase, every stage's delay line, and the decimation phase all persist
//! for the lifetime of the [`Channelizer`]. They are never reset at a VITA49
//! packet boundary — see the note in [`super::nco`] for why that matters.

use super::fir::{estimate_taps, kaiser_lowpass, FirDecimator};
use super::iq::{Iq, SC16_FULL_SCALE};
use super::nco::Nco;

/// Largest single-stage decimation factor the planner will use. Beyond this a
/// stage's own stopband requirement starts to cost more than an extra stage.
const MAX_STAGE_FACTOR: usize = 16;

/// One decimation stage: filter at `input_rate`, emit at `output_rate`.
#[derive(Debug, Clone, PartialEq)]
pub struct StagePlan {
    pub factor: usize,
    pub input_rate: f64,
    pub output_rate: f64,
    /// FIR -6 dB point (the midpoint of the transition band).
    pub cutoff_hz: f64,
    /// Frequency by which `stopband_db` attenuation must be reached.
    pub stopband_hz: f64,
    /// Kaiser tap count for this stage.
    pub taps: usize,
}

/// Plan a cascade of decimation stages from `input_rate` to `output_rate`.
///
/// * `passband_hz` — half-width of the channel to preserve.
/// * `stopband_hz` — frequency above which aliases must stay below the channel;
///   must not exceed `output_rate / 2`.
/// * `stopband_db` — attenuation target per stage.
/// * `max_taps` — reject a plan whose stages exceed this (a plan that "works"
///   at 4000 taps is a plan that will not keep up in a pod).
pub fn plan_stages(
    input_rate: f64,
    output_rate: f64,
    passband_hz: f64,
    stopband_hz: f64,
    stopband_db: f64,
    max_taps: usize,
) -> anyhow::Result<Vec<StagePlan>> {
    if !(input_rate > 0.0) || !(output_rate > 0.0) {
        anyhow::bail!("rates must be positive: input {input_rate}, output {output_rate}");
    }
    if output_rate > input_rate {
        anyhow::bail!("output_rate ({output_rate}) must not exceed input_rate ({input_rate})");
    }
    if !(passband_hz > 0.0) || stopband_hz <= passband_hz {
        anyhow::bail!("need 0 < passband_hz ({passband_hz}) < stopband_hz ({stopband_hz})");
    }
    if stopband_hz > output_rate / 2.0 + 1e-9 {
        anyhow::bail!(
            "stopband_hz ({stopband_hz}) must not exceed the output Nyquist \
             ({}) — content above it would alias into the channel",
            output_rate / 2.0
        );
    }

    let ratio = input_rate / output_rate;
    let rounded = ratio.round();
    if rounded < 1.0 || (ratio - rounded).abs() > 1e-9 {
        anyhow::bail!(
            "input_rate / output_rate must be a whole number, got {ratio} \
             ({input_rate} / {output_rate}). A fractional ratio needs a rational \
             resampler, which this channelizer deliberately does not have; pick an \
             output rate that divides the input rate (e.g. 50 kS/s from 2 MS/s, \
             2e6 / 40)."
        );
    }
    let mut remaining = rounded as usize;

    let mut stages: Vec<StagePlan> = Vec::new();
    let mut rate = input_rate;

    while remaining > 1 {
        // Prefer the largest usable factor: it pushes this stage's stopband out,
        // which is what keeps the tap count down.
        let mut chosen: Option<(usize, StagePlan)> = None;
        let mut failure: Option<anyhow::Error> = None;
        for factor in (2..=MAX_STAGE_FACTOR.min(remaining)).rev() {
            if !remaining.is_multiple_of(factor) {
                continue;
            }
            let stage_out = rate / factor as f64;
            let is_last = remaining / factor == 1;
            let stop = if is_last {
                stopband_hz
            } else {
                stage_out - stopband_hz
            };
            if stop <= passband_hz {
                failure = Some(anyhow::anyhow!(
                    "stage factor {factor} leaves no transition band (stopband {stop} Hz \
                     <= passband {passband_hz} Hz)"
                ));
                continue;
            }
            let taps = estimate_taps(stopband_db, stop - passband_hz, rate);
            let plan = StagePlan {
                factor,
                input_rate: rate,
                output_rate: stage_out,
                cutoff_hz: (passband_hz + stop) / 2.0,
                stopband_hz: stop,
                taps,
            };
            if taps > max_taps {
                failure = Some(anyhow::anyhow!(
                    "stage factor {factor} needs {taps} taps, over the {max_taps} budget \
                     (transition {:.0} Hz at {rate} Hz input)",
                    stop - passband_hz
                ));
                continue;
            }
            chosen = Some((factor, plan));
            break;
        }

        let Some((factor, plan)) = chosen else {
            let detail = failure.map(|e| e.to_string()).unwrap_or_else(|| {
                "no factor in 2..=MAX_STAGE_FACTOR divides the ratio".to_string()
            });
            anyhow::bail!(
                "cannot plan decimation by {remaining} from {rate} Hz to a {output_rate} Hz \
                 output within {max_taps} taps: {detail}. Choose an output rate whose ratio \
                 factors into small integers."
            );
        };

        rate = plan.output_rate;
        remaining /= factor;
        stages.push(plan);
    }

    let product: usize = stages.iter().map(|s| s.factor).product();
    debug_assert_eq!(product as f64, rounded);
    Ok(stages)
}

/// One stage's I and Q filters, advanced in lockstep.
struct ChannelStage {
    i: FirDecimator,
    q: FirDecimator,
}

/// Mix one channel to DC, filter, and decimate to the output rate.
pub struct Channelizer {
    nco: Nco,
    stages: Vec<ChannelStage>,
    plan: Vec<StagePlan>,
    output_rate: f64,
}

impl Channelizer {
    /// Build a channelizer from a stage plan. `offset_hz` is the channel centre
    /// relative to the wideband centre (may be negative).
    pub fn new(plan: Vec<StagePlan>, offset_hz: f64, input_rate: f64, stopband_db: f64) -> Self {
        assert!(!plan.is_empty(), "a channelizer needs at least one stage");
        let output_rate = plan.last().map(|s| s.output_rate).unwrap_or(input_rate);

        let stages = plan
            .iter()
            .map(|s| {
                let taps = kaiser_lowpass(s.taps, s.cutoff_hz, s.input_rate, stopband_db);
                ChannelStage {
                    i: FirDecimator::new(taps.clone(), s.factor),
                    q: FirDecimator::new(taps, s.factor),
                }
            })
            .collect();

        Channelizer {
            nco: Nco::new(offset_hz, input_rate),
            stages,
            plan,
            output_rate,
        }
    }

    /// The stage plan, in order.
    pub fn plan(&self) -> &[StagePlan] {
        &self.plan
    }

    /// Narrowband output sample rate.
    pub fn output_rate(&self) -> f64 {
        self.output_rate
    }

    /// Total decimation factor.
    pub fn decimation(&self) -> usize {
        self.plan.iter().map(|s| s.factor).product()
    }

    /// How many **input** samples the chain needs before its output is the
    /// steady-state response, rather than the filter startup transient.
    ///
    /// This is not a cosmetic detail. Measured on the 2 MS/s -> 50 kS/s plan, an
    /// out-of-channel interferer that is genuinely suppressed by 115 dB in steady
    /// state reads only ~74 dB of suppression if the average includes the
    /// transient, because the transient momentarily dominates a channel that
    /// would otherwise be nearly empty. Any level or isolation number taken from
    /// a short block near startup is wrong by ~40 dB unless it skips this many
    /// input samples (divide by [`Self::decimation`] for output samples).
    pub fn settling_samples(&self) -> usize {
        // Each stage's transient lasts its whole impulse response, expressed in
        // input samples by the decimation already applied ahead of it.
        let mut cum = 1usize;
        let mut total = 0usize;
        for stage in &self.plan {
            total += stage.taps * cum;
            cum = cum.saturating_mul(stage.factor);
        }
        total
    }

    /// Run interleaved sc16 wideband samples through the chain, appending the
    /// surviving narrowband samples to `out`.
    pub fn process(&mut self, interleaved_sc16: &[i16], out: &mut Vec<Iq>) {
        for pair in interleaved_sc16.chunks_exact(2) {
            let sample = Iq::new(
                pair[0] as f32 / SC16_FULL_SCALE,
                pair[1] as f32 / SC16_FULL_SCALE,
            );
            if let Some(y) = self.process_sample(sample) {
                out.push(y);
            }
        }
    }

    /// Run normalized f32 IQ samples through the chain (the fixture and unit
    /// tests use this to avoid a pointless sc16 round-trip).
    pub fn process_iq(&mut self, iq: &[Iq], out: &mut Vec<Iq>) {
        for sample in iq {
            if let Some(y) = self.process_sample(*sample) {
                out.push(y);
            }
        }
    }

    /// One sample through mixer + stage cascade.
    #[inline]
    fn process_sample(&mut self, sample: Iq) -> Option<Iq> {
        let mut v = self.nco.mix(sample);
        for stage in self.stages.iter_mut() {
            match (stage.i.process(v.i), stage.q.process(v.q)) {
                (Some(i), Some(q)) => v = Iq::new(i, q),
                // Both filters share the decimation phase, so they always agree;
                // a mismatch would mean the stage is stepping them differently.
                _ => return None,
            }
        }
        Some(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp::iq::mean_power;
    use crate::dsp::iq::power_dbfs;
    use crate::dsp::tone::estimate_frequency;
    use std::f64::consts::TAU;

    const IN_RATE: f64 = 2_000_000.0;
    const OUT_RATE: f64 = 50_000.0;
    const PASSBAND: f64 = 20_000.0;
    const STOPBAND: f64 = 25_000.0;
    const STOP_DB: f64 = 80.0;
    const MAX_TAPS: usize = 512;

    fn default_plan() -> Vec<StagePlan> {
        plan_stages(IN_RATE, OUT_RATE, PASSBAND, STOPBAND, STOP_DB, MAX_TAPS).expect("plan")
    }

    fn tone(freq: f64, rate: f64, n: usize, amp: f32) -> Vec<Iq> {
        (0..n)
            .map(|k| {
                let ang = TAU * freq * k as f64 / rate;
                Iq::new(amp * ang.cos() as f32, amp * ang.sin() as f32)
            })
            .collect()
    }

    #[test]
    fn plans_the_documented_two_stage_cascade() {
        let plan = default_plan();
        let factors: Vec<usize> = plan.iter().map(|s| s.factor).collect();
        assert_eq!(factors, vec![10, 4], "40 = 10 x 4, largest factor first");
        assert_eq!(plan[0].input_rate, IN_RATE);
        assert_eq!(plan[0].output_rate, 200_000.0);
        assert_eq!(plan[1].input_rate, 200_000.0);
        assert_eq!(plan[1].output_rate, OUT_RATE);
        for s in &plan {
            assert!(s.taps <= MAX_TAPS, "{s:?}");
            assert!(
                !s.taps.is_multiple_of(2),
                "odd taps for integer group delay"
            );
            assert!(s.cutoff_hz > PASSBAND && s.cutoff_hz < s.stopband_hz);
        }
        // The reason largest-first is the right heuristic: the expensive
        // stage is the last one, and a bigger first factor drives its rate down.
        assert!(
            plan[1].input_rate < IN_RATE / 8.0,
            "last stage must run well below the input rate: {}",
            plan[1].input_rate
        );
    }

    /// The alias-protection rule, stated as an invariant: an intermediate stage
    /// only has to be clean where the final channel will land.
    #[test]
    fn intermediate_stage_stopbands_extend_past_their_output_nyquist() {
        let plan = default_plan();
        for (idx, s) in plan.iter().enumerate() {
            let is_last = idx == plan.len() - 1;
            if is_last {
                assert!(
                    s.stopband_hz <= OUT_RATE / 2.0 + 1e-9,
                    "the last stage must stop at its own Nyquist: {s:?}"
                );
            } else {
                assert!(
                    s.stopband_hz > s.output_rate / 2.0,
                    "intermediate stages only protect the final channel: {s:?}"
                );
                assert!(
                    (s.output_rate - s.stopband_hz - (OUT_RATE / 2.0)).abs() < 1e-9,
                    "stopband should sit one channel-half apart from the output rate"
                );
            }
        }
    }

    #[test]
    fn ratio_must_be_a_whole_number() {
        let err = plan_stages(IN_RATE, 48_000.0, PASSBAND, 24_000.0, STOP_DB, MAX_TAPS)
            .expect_err("48 kS/s is not an integer decimation of 2 MS/s")
            .to_string();
        assert!(err.contains("whole number"), "{err}");
        assert!(
            err.contains("resampler"),
            "the error must name the missing capability: {err}"
        );
    }

    /// A prime ratio can only be one huge stage — the planner must refuse it
    /// rather than emit a plan that cannot keep up.
    #[test]
    fn an_unaffordable_prime_ratio_is_rejected_with_guidance() {
        let out = IN_RATE / 41.0;
        let err = plan_stages(IN_RATE, out, PASSBAND, out / 2.0, STOP_DB, MAX_TAPS)
            .expect_err("41 needs ~2000 taps")
            .to_string();
        assert!(err.contains("taps"), "{err}");
        assert!(err.contains("output rate"), "guidance: {err}");
    }

    #[test]
    fn a_single_stage_plan_works_when_the_ratio_is_small() {
        let plan = plan_stages(250_000.0, 50_000.0, PASSBAND, STOPBAND, STOP_DB, MAX_TAPS)
            .expect("5 is a single cheap stage");
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].factor, 5);
        assert!(plan[0].stopband_hz <= 25_000.0 + 1e-9);
    }

    #[test]
    fn output_rate_and_decimation_match_the_plan() {
        let mut ch = Channelizer::new(default_plan(), 200_000.0, IN_RATE, STOP_DB);
        assert_eq!(ch.decimation(), 40);
        assert_eq!(ch.output_rate(), OUT_RATE);

        // 1 s of wideband in -> exactly output_rate samples out.
        let input = tone(200_000.0, IN_RATE, 200_000, 0.5);
        let mut out = Vec::new();
        ch.process_iq(&input, &mut out);
        assert_eq!(out.len(), 5_000);
    }

    /// The state bug that would corrupt every tone measurement: identical input,
    /// different block boundaries, identical output.
    #[test]
    fn decimator_packet_size_invariance() {
        let input = tone(200_000.0 + 300.0, IN_RATE, 20_480, 0.5);

        let mut one_shot = Channelizer::new(default_plan(), 200_000.0, IN_RATE, STOP_DB);
        let mut want = Vec::new();
        one_shot.process_iq(&input, &mut want);

        for chunk in [1usize, 333, 2048, 4999, 20_480] {
            let mut chunked = Channelizer::new(default_plan(), 200_000.0, IN_RATE, STOP_DB);
            let mut got = Vec::new();
            for part in input.chunks(chunk) {
                chunked.process_iq(part, &mut got);
            }
            assert_eq!(got.len(), want.len(), "chunk={chunk}");
            for (a, b) in got.iter().zip(want.iter()) {
                assert!(
                    (a.i - b.i).abs() < 1e-6 && (a.q - b.q).abs() < 1e-6,
                    "chunk={chunk} diverged: {a:?} vs {b:?}"
                );
            }
        }
    }

    #[test]
    fn an_in_channel_tone_survives_at_full_level() {
        let mut ch = Channelizer::new(default_plan(), 200_000.0, IN_RATE, STOP_DB);
        let input = tone(200_000.0, IN_RATE, 200_000, 0.5);
        let mut out = Vec::new();
        ch.process_iq(&input, &mut out);

        let db = power_dbfs(&out);
        // A 0.5-amplitude tone is -6.02 dBFS at the input; unity DC gain through
        // the chain means it must come out at the same level.
        assert!((db - (-6.02)).abs() < 0.5, "output {db} dBFS");
    }

    /// The whole point of the module: a strong signal well outside the channel
    /// must not appear in it.
    ///
    /// Measured on an interferer-only input — with no wanted signal present there
    /// are no cross-terms, so the figure is the interferer's own residual — and
    /// after discarding the startup transient. Both choices matter: averaging
    /// over the transient reports 115 dB of isolation as ~74 dB.
    #[test]
    fn an_out_of_channel_interferer_is_rejected_by_more_than_100_db() {
        for interferer_khz in [600.0, 810.0, -600.0] {
            let mut ch = Channelizer::new(default_plan(), 200_000.0, IN_RATE, STOP_DB);
            let mut out = Vec::new();
            let f = 200_000.0 + interferer_khz * 1000.0;
            ch.process_iq(&tone(f, IN_RATE, 200_000, 5.0), &mut out);

            let skip = ch.settling_samples() / ch.decimation();
            let settled = &out[skip..];
            let p = mean_power(settled).expect("output") as f64;
            let attenuation_db = 10.0 * (25.0 / p.max(1e-30)).log10();
            println!("interferer {interferer_khz:+.0} kHz: {attenuation_db:.1} dB of attenuation");
            assert!(
                attenuation_db > 100.0,
                "interferer at {f} Hz: only {attenuation_db:.1} dB of attenuation"
            );
        }
    }

    /// Pins the reason [`Channelizer::settling_samples`] exists, so nobody
    /// "simplifies" the metric path by ignoring it: over the transient the same
    /// input looks ~40 dB worse.
    #[test]
    fn the_startup_transient_must_be_skipped_before_measuring_level() {
        let mut ch = Channelizer::new(default_plan(), 200_000.0, IN_RATE, STOP_DB);
        let mut out = Vec::new();
        ch.process_iq(
            &tone(200_000.0 + 600_000.0, IN_RATE, 200_000, 5.0),
            &mut out,
        );

        // What a metric does when it averages the whole capture, including the
        // filter startup: the transient dominates a channel that is otherwise
        // nearly empty. The peak of that transient lands tens of output samples
        // in (both filters are simultaneously filling), not at sample 0.
        let naive = 10.0 * (25.0 / mean_power(&out).expect("out") as f64).log10();
        let skip = ch.settling_samples() / ch.decimation();
        let settled = 10.0 * (25.0 / mean_power(&out[skip..]).expect("out") as f64).log10();
        println!(
            "whole-capture average {naive:.1} dB vs settled {settled:.1} dB \
             (skipping {} output samples)",
            skip
        );

        assert!(
            naive < 80.0,
            "the transient-inclusive figure should look poor: {naive:.1} dB"
        );
        assert!(
            settled > 100.0,
            "the settled window should be deep: {settled:.1} dB"
        );
        assert!(
            settled > naive + 20.0,
            "settling region must be skipped: naive {naive:.1} dB vs settled {settled:.1} dB"
        );
    }

    fn the_channel_is_centred_where_the_plan_says_it_is() {
        // A tone 300 Hz above the channel centre must appear 300 Hz above DC in
        // the narrowband output — this is what validates the mixer sign.
        let offset = 200_000.0;
        let mut ch = Channelizer::new(default_plan(), offset, IN_RATE, STOP_DB);
        let mut out = Vec::new();
        ch.process_iq(&tone(offset + 300.0, IN_RATE, 200_000, 0.5), &mut out);
        let got = estimate_frequency(&out, OUT_RATE, 50.0, 5_000.0).expect("tone");
        assert!((got - 300.0).abs() < 1.0, "measured {got} Hz");
    }

    #[test]
    fn channel_below_the_wideband_centre_is_selected_too() {
        let offset = -300_000.0;
        let mut ch = Channelizer::new(default_plan(), offset, IN_RATE, STOP_DB);
        let mut out = Vec::new();
        ch.process_iq(&tone(offset, IN_RATE, 200_000, 0.5), &mut out);
        let db = power_dbfs(&out);
        assert!((db - (-6.02)).abs() < 0.5, "output {db} dBFS");
    }

    /// Not a gate — an explicit measurement, run with `--release --ignored`,
    /// because the only honest way to know the channelizer keeps up with
    /// 2 MS/s is to time it.
    #[test]
    #[ignore = "timing measurement; run with --release --ignored"]
    fn measures_real_time_factor_for_one_second_of_wideband() {
        let mut ch = Channelizer::new(default_plan(), 200_000.0, IN_RATE, STOP_DB);
        let input = tone(200_000.0, IN_RATE, IN_RATE as usize, 0.5);
        let mut out = Vec::new();
        let start = std::time::Instant::now();
        ch.process_iq(&input, &mut out);
        let elapsed = start.elapsed().as_secs_f64();
        println!(
            "1 s of 2 MS/s wideband -> {} narrowband samples in {elapsed:.3} s \
             (real-time factor {:.3})",
            out.len(),
            1.0 / elapsed
        );
        assert!(
            elapsed < 1.0,
            "must process faster than real time: {elapsed:.3} s"
        );
    }
}
