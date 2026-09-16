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
/// cumulative form, matching the ISO-defined windows). The Bessel argument
/// is sqrt(i·(n−i)·alpha²) — the square root goes inside I₀.
#[allow(clippy::needless_range_loop)]
pub fn kbd_window(n: usize, alpha: f64) -> Vec<f32> {
    let alpha2 = 4.0 * (alpha * std::f64::consts::PI / n as f64).powi(2);
    let mut temp = vec![0.0f64; n / 2 + 1];
    let mut scale = 0.0f64;
    for i in 0..=n / 2 {
        temp[i] = bessel_i0(((i * (n - i)) as f64 * alpha2).sqrt());
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
    /// cos table: table[n * m + k], n over the full 2M synthesis.
    table: Box<[f32]>,
    m: usize,
}

impl Mdct {
    /// M = number of spectral coefficients; synthesis is 2M samples:
    /// x(n) = −1/(M·2^15)·Σ X(k)·cos(π/(2M)·(n + M/2 + ½)·(2k+1)).
    /// (The −1/M is the ISO normalization; the reference decoder's float
    /// MDCT additionally scales by 1/2^15 — its scalefactor and quantizer
    /// tables are built to match — so the factor is repeated here.)
    pub fn new(m: usize) -> Self {
        let mut table = vec![0.0f32; 2 * m * m];
        let scale = -1.0 / (m as f64 * 32768.0);
        for n in 0..2 * m {
            for k in 0..m {
                let angle = std::f64::consts::PI / (2.0 * m as f64)
                    * (n as f64 + m as f64 / 2.0 + 0.5)
                    * (2.0 * k as f64 + 1.0);
                table[n * m + k] = (scale * angle.cos()) as f32;
            }
        }
        Mdct {
            table: table.into_boxed_slice(),
            m,
        }
    }

    /// `input` holds M spectral coefficients; `output` receives 2M samples.
    #[allow(clippy::needless_range_loop)]
    pub fn imdct(&self, input: &[f32], output: &mut [f32]) {
        let m = self.m;
        for n in 0..2 * m {
            let row = &self.table[n * m..n * m + m];
            let mut acc = 0.0f32;
            for k in 0..m {
                acc += input[k] * row[k];
            }
            output[n] = acc;
        }
    }
}

/// Windowed overlap lap (FFmpeg `vector_fmul_window`).
///
/// `dst` receives 2·len samples from `src0` (len), `src1` (len), and the
/// full 2·len window:
/// `dst[i] = src0[i]·win[2len−1−i] − src1[len−1−i]·win[i]`,
/// `dst[len+i] = src0[len−1−i]·win[len−1−i] + src1[i]·win[len+i]`.
pub fn vector_fmul_window(dst: &mut [f32], src0: &[f32], src1: &[f32], win: &[f32], len: usize) {
    for i in 0..len {
        let j = len - 1 - i;
        let s0 = src0[i];
        let s1 = src1[j];
        let wi = win[i];
        let wj = win[2 * len - 1 - i];
        dst[i] = s0 * wj - s1 * wi;
        dst[len + i] = src0[j] * win[j] + src1[i] * win[len + i];
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
    fn imdct_matches_definition() {
        // M = 4 coefficients → 8 synthesis samples.
        let m = 4;
        let mdct = Mdct::new(m);
        let input = [1.0f32, 0.0, 0.0, 0.0];
        let mut output = [0.0f32; 8];
        mdct.imdct(&input, &mut output);
        // x(0) = −1/(4·2^15)·cos(π/8·(0 + 2 + 0.5)·1) = −0.25/2^15·cos(π·2.5/8)
        let expect0 = (-0.25f64 / 32768.0 * (std::f64::consts::PI / 8.0 * 2.5).cos()) as f32;
        assert!((output[0] - expect0).abs() < 1e-6);
    }

    #[test]
    fn mdct_tdac_perfect_reconstruction() {
        // Analysis (same kernel, windowed) + synthesis + overlap-add must
        // reconstruct a random signal exactly when the window pair satisfies
        // w_first(n)² + w_second(n)² = 1.
        let m = 128usize;
        let half = sine_half(m);
        let win: Vec<f32> = half.iter().chain(half.iter().rev()).copied().collect();

        let mut state = 123456789u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let hops = 8;
        let signal: Vec<f32> = (0..(hops + 2) * m)
            .map(|_| (next() % 2000) as f32 / 16384.0 - 0.06)
            .collect();

        {
            let mdct = Mdct::new(m);
            let mut out = vec![0.0f32; (hops + 2) * m];
            for hop in 0..hops {
                let block = &signal[hop * m..hop * m + 2 * m];
                let mut coeffs = vec![0.0f32; m];
                #[allow(clippy::needless_range_loop)]
                for k in 0..m {
                    let mut acc = 0.0f64;
                    for n in 0..2 * m {
                        let angle = std::f64::consts::PI / (2.0 * m as f64)
                            * (n as f64 + m as f64 / 2.0 + 0.5)
                            * (2.0 * k as f64 + 1.0);
                        acc += (block[n] * win[n]) as f64 * angle.cos();
                    }
                    // ISO analysis carries a leading −2 (·2^15 to cancel the
                    // synthesis normalization's 1/2^15).
                    coeffs[k] = (-2.0 * 32768.0 * acc) as f32;
                }
                let mut y = vec![0.0f32; 2 * m];
                mdct.imdct(&coeffs, &mut y);
                for n in 0..2 * m {
                    y[n] *= win[n];
                }
                for n in 0..2 * m {
                    out[hop * m + n] += y[n];
                }
            }
            let mut max_err = 0.0f32;
            for t in m..hops * m {
                max_err = max_err.max((out[t] - signal[t]).abs());
            }
            assert!(max_err < 1e-3, "TDAC PR failed: max_err = {max_err}");
        }
    }

    /// Half-window: sin(π(n+½)/(2·M)) ascending — pair condition holds with
    /// its own reversal.
    pub fn sine_half(m: usize) -> Vec<f32> {
        (0..m)
            .map(|i| ((i as f64 + 0.5) * std::f64::consts::PI / (2.0 * m as f64)).sin() as f32)
            .collect()
    }
}
