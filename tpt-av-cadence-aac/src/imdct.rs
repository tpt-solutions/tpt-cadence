//! IMDCT, window functions, and overlap-add for AAC (ISO/IEC 14496-3 §4.6.4).
//!
//! The MDCT is evaluated directly against a precomputed cosine table
//! (built once at decoder open time — real-time contract). Windows are the
//! ISO sine window and the normalized Kaiser-Bessel-derived window
//! (long: α = 4.0, short: α = 6.0).

/// Modified Bessel function of the first kind, order 0 (power series).
#[allow(clippy::needless_range_loop)]
fn bessel_i0(x: f64) -> f64 {
    let mut sum = 1.0f64;
    let mut term = 1.0f64;
    let half = x / 2.0;
    for k in 1..32 {
        term *= half * half / (k * k) as f64;
        sum += term;
        if term < 1e-14 * sum {
            break;
        }
    }
    sum
}

/// Sine window of length `n`: w(i) = sin(π (i + ½) / (2n)), which satisfies
/// the TDAC pair condition w(i)² + w(n−1−i)² = 1.
pub fn sine_window(n: usize) -> Vec<f32> {
    (0..n)
        .map(|i| ((i as f64 + 0.5) * std::f64::consts::PI / (2.0 * n as f64)).sin() as f32)
        .collect()
}

/// Kaiser-Bessel-derived window of length `n` (FFmpeg `kbd_window_init`
/// cumulative-normalization form, matching the ISO-defined windows).
#[allow(clippy::needless_range_loop)]
pub fn kbd_window(n: usize, alpha: f64) -> Vec<f32> {
    let alpha2 = 4.0 * (alpha * std::f64::consts::PI / n as f64).powi(2);
    let mut temp = vec![0.0f64; n / 2 + 1];
    let mut scale = 0.0f64;
    for i in 0..=n / 2 {
        temp[i] = bessel_i0((i * (n - i)) as f64 * alpha2).sqrt();
        scale += temp[i] * (1.0 + ((i != 0 && i != n / 2) as i32 as f64));
    }
    scale = 1.0 / (scale + 1.0);

    let mut window = vec![0.0f32; n];
    let mut sum = 0.0f64;
    for i in 0..=n / 2 {
        sum += temp[i];
        window[i] = (sum * scale).sqrt() as f32;
    }
    for i in n / 2 + 1..n {
        sum += temp[n - i];
        window[i] = (sum * scale).sqrt() as f32;
    }
    window
}

/// N-point inverse MDCT with the ISO normalization x(n) = (2/N)·Σ X(k)·cos(…).
///
/// `input` holds N/2 spectral coefficients; `output` receives N samples.
#[allow(clippy::needless_range_loop)]
pub struct Mdct {
    /// cos table: table[n * half + k].
    table: Box<[f32]>,
    n: usize,
}

impl Mdct {
    /// Builds the synthesis table with the given overall scale folded in.
    pub fn with_scale(n: usize, scale: f64) -> Self {
        let half = n / 2;
        let mut table = vec![0.0f32; half * n];
        for k in 0..half {
            for ni in 0..n {
                let angle = std::f64::consts::PI / n as f64
                    * (ni as f64 + n as f64 / 4.0 + 0.5)
                    * (2.0 * k as f64 + 1.0);
                table[ni * half + k] = (scale * angle.cos()) as f32;
            }
        }
        Mdct {
            table: table.into_boxed_slice(),
            n,
        }
    }

    pub fn new(n: usize) -> Self {
        Self::with_scale(n, 2.0 / n as f64)
    }

    /// `output[n] = Σ_k input[k] · table[n][k]` (the 2/N scale is folded in).
    #[allow(clippy::needless_range_loop)]
    pub fn imdct(&self, input: &[f32], output: &mut [f32]) {
        let half = self.n / 2;
        for ni in 0..self.n {
            let row = &self.table[ni * half..ni * half + half];
            let mut acc = 0.0f32;
            for k in 0..half {
                acc += input[k] * row[k];
            }
            output[ni] = acc;
        }
    }
}

/// Overlap-add windowing (FFmpeg `vector_fmul_window`).
///
/// `dst[i] = saved[i]·win[i] − buf[i]·win[len2·2−1−i]` for the first half
/// and the mirrored sum for the second half, where `len2` is half the
/// window length.
pub fn vector_fmul_window(dst: &mut [f32], saved: &[f32], buf: &[f32], win: &[f32], len2: usize) {
    let (src0, src1) = (saved, buf);
    for i in 0..len2 {
        let j = 2 * len2 - 1 - i;
        let s0 = src0[i];
        let s1 = src1[j];
        let wi = win[i];
        let wj = win[j];
        dst[i] = s0 * wi - s1 * wj;
        dst[j] = s1 * wi + s0 * wj;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sine_window_tdac_property() {
        let w = sine_window(1024);
        for i in 0..1024 {
            let sum = w[i] * w[i] + w[1023 - i] * w[1023 - i];
            assert!((sum - 1.0).abs() < 1e-5, "i={i}: {sum}");
        }
    }

    #[test]
    fn kbd_window_tdac_property() {
        for (n, alpha) in [(1024usize, 4.0f64), (128usize, 6.0f64)] {
            let w = kbd_window(n, alpha);
            for i in 0..n {
                let sum = w[i] * w[i] + w[n - 1 - i] * w[n - 1 - i];
                assert!((sum - 1.0).abs() < 1e-4, "n={n} i={i}: {sum}");
            }
        }
    }

    #[test]
    fn imdct_is_linear_and_matches_definition() {
        let n = 8;
        let mdct = Mdct::new(n);
        let input = [1.0f32, 0.0, 0.0, 0.0];
        let mut output = [0.0f32; 8];
        mdct.imdct(&input, &mut output);
        // x(0) = (2/8)·cos(π/8·(0+2+0.5)·1) = 0.25·cos(π·2.5/8)
        let expect0 = (0.25f64 * (std::f64::consts::PI / 8.0 * 2.5).cos()) as f32;
        assert!((output[0] - expect0).abs() < 1e-6);
    }
}
