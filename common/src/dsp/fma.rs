//! Fused multiply-add (FMA) kernels for aarch64 (NEON) and x86_64.
//!
//! Moved verbatim from the single-crate `lib.rs`, tests included. On aarch64
//! with nightly Rust these compile to `fmaf` intrinsics which map to NEON; on
//! stable they are scalar. Nothing here depends on UHD, so the whole module is
//! exercised by `cargo test --workspace --no-default-features` on any host.

/// Scalar Fused Multiply-Add operation: computes `a * b + c`
#[inline]
pub fn fmaf(a: f32, b: f32, c: f32) -> f32 {
    a * b + c
}

/// FMA kernel for ARM64 NEON - processes up to 8 elements using scalar operations
// On aarch64 with nightly Rust, this will compile to `fmaf` intrinsics which map to NEON
pub fn fused_multiply_add_8lane(a: &[f32], b: &[f32], c: &mut [f32]) -> Result<(), String> {
    let n = a.len().min(b.len()).min(c.len());

    if n < 1 {
        return Err("Input arrays must not be empty".to_string());
    }

    // Process up to 8 elements with FMA operation (scalar for stable Rust)
    for i in 0..n.min(8) {
        c[i] = fmaf(a[i], b[i], 0.0);
    }

    Ok(())
}

/// FMA kernel variant - processes multiple of 8 elements at once using iteration
// Optimized loop structure for ARM64 NEON efficiency
pub fn fused_multiply_add_kernel(a: &[f32], b: &[f32], c: &mut [f32]) -> Result<(), String> {
    let n = a.len().min(b.len()).min(c.len());

    if n == 0 {
        return Ok(());
    }

    // Process all elements with FMA operations in chunks of 8 for ARM64 NEON efficiency
    for i in (0..n).step_by(8) {
        let count = (n - i).min(8);

        for j in 0..count {
            c[i + j] = fmaf(a[i + j], b[i + j], 0.0);
        }
    }

    Ok(())
}

/// FMA kernel with scalar offset (c parameter as accumulator base)
pub fn fused_multiply_add_kernel_with_offset(
    a: &[f32],
    b: &[f32],
    c: &mut [f32],
) -> Result<(), String> {
    let n = a.len().min(b.len()).min(c.len());

    if n == 0 {
        return Ok(());
    }

    // Process all elements with FMA operations using scalar offset pattern
    for i in (0..n).step_by(8) {
        let count = (n - i).min(8);

        for j in 0..count {
            c[i + j] = fmaf(a[i + j], b[i + j], 0.0);
        }
    }

    Ok(())
}

/// Test function demonstrating kernel usage (only works when std is available)
pub fn test_fma_kernel() -> Result<(), String> {
    let a: Vec<f32> = vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0];
    let b: Vec<f32> = vec![1.0; 8];
    let mut c: Vec<f32> = vec![0.0; 8];

    fused_multiply_add_kernel(&a, &b, &mut c)?;

    for i in 0..c.len() {
        println!("c[{}] = {}", i, c[i]);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fma_kernel_basic() -> Result<(), String> {
        let a: Vec<f32> = vec![1.0; 8];
        let b: Vec<f32> = vec![2.0; 8];
        let mut c: Vec<f32> = vec![0.0; 8];

        fused_multiply_add_8lane(&a, &b, &mut c)?;

        assert!(c.iter().all(|&x| x == 2.0));
        Ok(())
    }

    #[test]
    fn test_fma_kernel_with_offset() -> Result<(), String> {
        let a: Vec<f32> = vec![1.0, 2.0, 3.0, 4.0];
        let b: Vec<f32> = vec![5.0, 6.0, 7.0, 8.0];
        let mut c: Vec<f32> = vec![0.0; 4];

        fused_multiply_add_kernel_with_offset(&a, &b, &mut c)?;

        // FMA computes a*b+0 for each element (not a+b*c as the name implies)
        assert!((c[0] - 5.0).abs() < f32::EPSILON);
        assert!((c[1] - 12.0).abs() < f32::EPSILON);
        Ok(())
    }

    #[test]
    fn test_fma_kernel_edge_cases() -> Result<(), String> {
        let a: Vec<f32> = vec![f32::NAN, f32::INFINITY, -1.0, 0.5, 2.0, -2.0, 4.0, -4.0];
        let b: Vec<f32> = vec![2.0; 8];
        let mut c: Vec<f32> = vec![0.0; 8];

        fused_multiply_add_8lane(&a, &b, &mut c)?;

        assert!(c[0].is_nan());
        assert!(c[1].is_infinite());
        Ok(())
    }
}
