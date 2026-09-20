//! Signal processing primitives shared across the sigproc modules.
//!
//! Everything here is dependency-free (no UHD, no `num-complex`) and tested
//! without hardware, because the channelizer's correctness must be provable in
//! CI rather than by listening to a radio.
//!
//! * [`iq`] — the complex sample type, sc16 conversion, level in dBFS.
//! * [`fft`] — radix-2 FFT, for spectral measurement.
//! * [`tone`] — where is the tone? (peak channel offset, recovered AM tone).
//! * [`nco`] — the mixer that selects a channel.
//! * [`fir`] — Kaiser FIR design and the stateful decimating filter.
//! * [`decimator`] — multi-stage plans and the [`decimator::Channelizer`].
//! * [`gaps`] — VITA49 sequence-gap tracking (loss made explicit, not spliced).
//! * [`fma`] — the ARM64/NEON fused multiply-add kernels, moved verbatim from
//!   the single-crate era.

pub mod decimator;
pub mod fft;
pub mod fir;
pub mod fma;
pub mod gaps;
pub mod iq;
pub mod nco;
pub mod tone;

pub use decimator::{plan_stages, Channelizer, StagePlan};
pub use fma::{
    fmaf, fused_multiply_add_8lane, fused_multiply_add_kernel,
    fused_multiply_add_kernel_with_offset, test_fma_kernel,
};
pub use gaps::{GapMarker, GapTracker};
pub use iq::{envelope, iq_to_sc16, mean_power, power_dbfs, sc16_to_iq, Iq, SC16_FULL_SCALE};
pub use nco::Nco;
pub use tone::{envelope_tone_hz, estimate_frequency, estimate_frequency_keeping_dc};
