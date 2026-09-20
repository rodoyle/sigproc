//! Signal processing primitives shared across the sigproc modules.
//!
//! Only the fused multiply-add kernels live here today — moved verbatim from the
//! single-crate era, tests included, so the ARM64/NEON work is not lost in the
//! workspace split.
//!
//! M2 adds the channelizer building blocks: a complex NCO mixer, FIR low-pass
//! design, an integer decimator whose filter/NCO state persists across VITA49
//! packet boundaries, power/SNR estimators, and the sequence-gap tracker that
//! makes dropped packets visible instead of silently splicing output.

mod fma;

pub use fma::{
    fmaf, fused_multiply_add_8lane, fused_multiply_add_kernel,
    fused_multiply_add_kernel_with_offset, test_fma_kernel,
};
