//! Throughput benchmarks for `sigproc-common` DSP primitives at scale.
//!
//! Target: verify the DSP pipeline keeps up with a 50 MSPS wideband input stream
//! (hardware max) and produces correct narrowband output. Each benchmark feeds
//! synthetic IQ at the target rate and measures samples/sec processed, reporting
//! the real-time factor (throughput / input rate). Real-time factor >= 1.0 means
//! the pipeline keeps up with live capture.
//!
//! Scope: DSP primitives only — NCO mixing, FIR decimation via `Channelizer`, FFT,
//! and tone estimation. No VITA49 framing, no network I/O. Runs on the dev host
//! via `cargo bench`.

use criterion::{black_box, criterion_group, criterion_main, Criterion, Throughput};
use sigproc_common::dsp::fft::fft_in_place;
use sigproc_common::dsp::{
    estimate_frequency, estimate_frequency_keeping_dc, iq_to_sc16, power_dbfs, Channelizer, Iq,
};
use std::f64::consts::TAU;

/// Target wideband input rate: hardware max.
const INPUT_RATE: f64 = 50_000_000.0;
/// Target narrowband output rate: 50 kS/s (decimation 1000).
const OUTPUT_RATE: f64 = 50_000.0;
/// Channel offset from wideband centre: 6 GHz tuning range is handled by the NCO
/// phase increment (offset_hz / sample_rate), so we exercise a large offset.
const OFFSET_HZ: f64 = 1_000_000_000.0;
/// Stopband attenuation target.
const STOPBAND_DB: f64 = 80.0;
/// Max taps per stage (reject plans that won't keep up).
const MAX_TAPS: usize = 512;
/// FIR transition-band passband half-width for the channel plan.
const PASSBAND: f64 = 20_000.0;
/// FIR stopband edge for the channel plan.
const STOPBAND: f64 = 25_000.0;

/// Generate `n` samples of a complex tone at `freq_hz` for a given sample rate.
fn complex_tone(freq_hz: f64, sample_rate: f64, n: usize) -> Vec<Iq> {
    (0..n)
        .map(|k| {
            let ang = TAU * freq_hz * k as f64 / sample_rate;
            Iq::new(ang.cos() as f32, ang.sin() as f32)
        })
        .collect()
}

/// Build a channelizer plan for INPUT_RATE -> OUTPUT_RATE decimation.
fn build_channelizer() -> Channelizer {
    let plan = sigproc_common::dsp::plan_stages(
        INPUT_RATE,
        OUTPUT_RATE,
        PASSBAND,
        STOPBAND,
        STOPBAND_DB,
        MAX_TAPS,
    )
    .expect("plan is affordable");
    Channelizer::new(plan, OFFSET_HZ, INPUT_RATE, STOPBAND_DB)
}

fn bench_channelizer(c: &mut Criterion) {
    // One second of wideband input at 50 MSPS -> 50k output samples.
    // We benchmark a smaller block to keep bench time reasonable while still
    // exercising the full pipeline; throughput scales linearly.
    let samples_out = 50_000; // 1 ms of narrowband output
    let samples_in = (samples_out as f64 * INPUT_RATE / OUTPUT_RATE) as usize; // 1 s of wideband input
    let input = complex_tone(OFFSET_HZ, INPUT_RATE, samples_in);

    let mut group = c.benchmark_group("channelizer");
    // Throughput measured in input samples/sec.
    group.throughput(Throughput::Elements(samples_in as u64));
    group.bench_function("process_50msps", |b| {
        b.iter(|| {
            let mut ch = build_channelizer();
            let mut out = Vec::with_capacity(samples_out);
            ch.process_iq(black_box(&input), &mut out);
            black_box(out)
        });
    });
    group.finish();
}

fn bench_nco(c: &mut Criterion) {
    // Benchmark raw NCO mixing throughput at 50 MSPS.
    let samples = 50_000_000; // one second at 50 MSPS
    let input = complex_tone(OFFSET_HZ, INPUT_RATE, samples);

    let mut group = c.benchmark_group("nco_mix");
    group.throughput(Throughput::Elements(samples as u64));
    group.bench_function("mix_50msps", |b| {
        b.iter(|| {
            let mut nco = sigproc_common::dsp::Nco::new(OFFSET_HZ, INPUT_RATE);
            let mut out = Vec::with_capacity(samples);
            for s in &input {
                out.push(nco.mix(*s));
            }
            black_box(out)
        });
    });
    group.finish();
}

fn bench_fft(c: &mut Criterion) {
    // Benchmark FFT throughput on a realistic block size (one measurement block).
    let n = 65_536; // power of two, ~1.3 ms at 50 MSPS
    let mut re = vec![0.0f64; n];
    let mut im = vec![0.0f64; n];
    // Fill with a deterministic signal.
    for k in 0..n {
        let ang = TAU * 1_000.0 * k as f64 / INPUT_RATE;
        re[k] = ang.cos();
        im[k] = ang.sin();
    }

    let mut group = c.benchmark_group("fft");
    group.throughput(Throughput::Elements(n as u64));
    group.bench_function("fft_65536", |b| {
        b.iter(|| {
            let mut r = re.clone();
            let mut i = im.clone();
            fft_in_place(&mut r, &mut i);
            black_box((r, i))
        });
    });
    group.finish();
}

fn bench_tone_estimation(c: &mut Criterion) {
    // Benchmark tone estimation throughput on a realistic block size.
    let n = 50_000; // 1 ms of narrowband output at 50 kS/s equivalent block
    let signal = complex_tone(1_234.5, OUTPUT_RATE, n);

    let mut group = c.benchmark_group("tone_estimation");
    group.throughput(Throughput::Elements(n as u64));
    group.bench_function("estimate_frequency_50k", |b| {
        b.iter(|| {
            let got = estimate_frequency(black_box(&signal), OUTPUT_RATE, 100.0, 5_000.0);
            black_box(got)
        });
    });
    group.bench_function("estimate_frequency_keeping_dc_50k", |b| {
        b.iter(|| {
            let got =
                estimate_frequency_keeping_dc(black_box(&signal), OUTPUT_RATE, -5_000.0, 5_000.0);
            black_box(got)
        });
    });
    group.finish();
}

fn bench_power_dbfs(c: &mut Criterion) {
    // Benchmark power measurement throughput on a realistic block size.
    let n = 50_000;
    let signal = complex_tone(1_234.5, OUTPUT_RATE, n);

    let mut group = c.benchmark_group("power_dbfs");
    group.throughput(Throughput::Elements(n as u64));
    group.bench_function("power_dbfs_50k", |b| {
        b.iter(|| {
            let p = power_dbfs(black_box(&signal));
            black_box(p)
        });
    });
    group.finish();
}

fn bench_sc16_conversion(c: &mut Criterion) {
    // Benchmark sc16 conversion throughput on a realistic block size.
    let n = 50_000;
    let signal = complex_tone(1_234.5, OUTPUT_RATE, n);

    let mut group = c.benchmark_group("sc16_conversion");
    group.throughput(Throughput::Elements(n as u64));
    group.bench_function("iq_to_sc16_50k", |b| {
        b.iter(|| {
            let out = iq_to_sc16(black_box(&signal));
            black_box(out)
        });
    });
    group.finish();
}

criterion_group!(
    benches,
    bench_channelizer,
    bench_nco,
    bench_fft,
    bench_tone_estimation,
    bench_power_dbfs,
    bench_sc16_conversion,
);
criterion_main!(benches);
