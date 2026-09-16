//! Synthesis MDCT and Vorbis window.
//!
//! The transform is numerically identical to FFmpeg's `av_tx` inverse MDCT
//! (`AV_TX_FLOAT_MDCT`, inverse, scale −1) as used by its Vorbis decoder —
//! the same half-length output array, so the overlap-add that follows
//! reproduces the reference PCM. The unit tests pin the fast path to a
//! transcription of FFmpeg's `ff_tx_mdct_naive_inv` definition.
//!
//! The window is the Vorbis window
//! `w[n] = sin( (pi/2) * sin^2( pi (n + 1/2) / N ) )`.

use crate::fft::{C, Fft};

/// Per-block-size synthesis transform (allocation confined to [`Mdct::new`]).
pub struct Mdct {
    /// Full block length `N`.
    n: usize,
    fft: Fft,
    gbuf: Box<[C]>,
    obuf: Box<[C]>,
    /// Pre-rotation twiddles: `g[j] = (-1)^j * e^{i pi (2j+1) / N}`.
    gtw: Box<[C]>,
    phase_sin: Box<[f32]>,
    phase_cos: Box<[f32]>,
    /// Vorbis window, `N/2` samples (the half used by the lapping scheme).
    window: Box<[f32]>,
}

impl Mdct {
    /// Plans the transform and window for a full block of `n` samples.
    pub fn new(n: usize) -> Self {
        assert!(n.is_power_of_two() && (64..=8192).contains(&n));
        let nf = n as f64;
        let window = (0..n / 2)
            .map(|m| {
                let s = (std::f64::consts::PI * (m as f64 + 0.5) / nf).sin();
                (0.5 * std::f64::consts::PI * s * s).sin() as f32
            })
            .collect::<Vec<_>>()
            .into_boxed_slice();
        let gtw = (0..n / 2)
            .map(|j| {
                let ang = std::f64::consts::PI * (2 * j + 1) as f64 / (2.0 * nf);
                let sign = if j & 1 == 0 { 1.0 } else { -1.0 };
                C::new((ang.cos() as f32) * sign, (ang.sin() as f32) * sign)
            })
            .collect::<Vec<_>>()
            .into_boxed_slice();
        let phase_sin = (0..n / 2)
            .map(|m| (std::f64::consts::PI * m as f64 / nf).sin() as f32)
            .collect::<Vec<_>>()
            .into_boxed_slice();
        let phase_cos = (0..n / 2)
            .map(|m| (std::f64::consts::PI * m as f64 / nf).cos() as f32)
            .collect::<Vec<_>>()
            .into_boxed_slice();
        Mdct {
            n,
            fft: Fft::new(n, false),
            gbuf: vec![C::default(); n].into_boxed_slice(),
            obuf: vec![C::default(); n].into_boxed_slice(),
            gtw,
            phase_sin,
            phase_cos,
            window,
        }
    }

    /// The `N/2`-sample Vorbis window.
    pub fn window(&self) -> &[f32] {
        &self.window
    }

    /// Inverse MDCT with FFmpeg's half-length convention and scale −1.
    ///
    /// Transforms `N/2` spectral coefficients into the `N/2` (redundant-half)
    /// time samples, in place. Allocation-free.
    pub fn imdct_half(&mut self, spec_out: &mut [f32]) {
        let n = self.n;
        let half = n / 2;
        debug_assert!(spec_out.len() >= half);
        {
            let (g, gtw) = (&mut self.gbuf, &self.gtw);
            for j in 0..half {
                let x = spec_out[j];
                g[j] = C::new(x * gtw[j].re, x * gtw[j].im);
            }
            for g in g[half..].iter_mut() {
                *g = C::default();
            }
        }
        self.fft.run(&self.gbuf, &mut self.obuf);
        // out[m] = -Im{ e^{i phi_m} * G[m] }, phi_m = pi m / N
        for m in 0..half {
            let gr = self.obuf[m].re;
            let gi = self.obuf[m].im;
            spec_out[m] = -(gr * self.phase_sin[m] + gi * self.phase_cos[m]);
        }
    }
}

/// FFmpeg's `vector_fmul_window` overlap step (`libavutil/float_dsp.c`).
///
/// Combines the previous block's saved (unwindowed) right half `src0` with
/// the current block's half-MDCT output `src1` through the `2*len`-sample
/// window `win`, writing `2*len` samples to `dst`:
///
/// ```text
/// dst[a]        = src0[a] * win[2len-1-a] - src1[len-1-a] * win[a]
/// dst[2len-1-a] = src0[a] * win[a]        + src1[len-1-a] * win[2len-1-a]
/// ```
pub fn vector_fmul_window(dst: &mut [f32], src0: &[f32], src1: &[f32], win: &[f32], len: usize) {
    debug_assert!(dst.len() >= 2 * len && src0.len() >= len && src1.len() >= len && win.len() >= 2 * len);
    for a in 0..len {
        let b = 2 * len - 1 - a;
        let s0 = src0[a];
        let s1 = src1[len - 1 - a];
        let wa = win[a];
        let wb = win[b];
        dst[a] = s0 * wb - s1 * wa;
        dst[b] = s0 * wa + s1 * wb;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Naive reference transcribed from FFmpeg `ff_tx_mdct_naive_inv`
    /// (`libavutil/tx_template.c`) with scale −1, the definition the fast
    /// path must reproduce.
    fn naive_imdct_half(spec: &[f64], n: usize) -> Vec<f64> {
        let len = n / 4;
        let len2 = n / 2;
        let phase = std::f64::consts::PI / (4.0 * len2 as f64);
        let mut dst = vec![0.0f64; 2 * len];
        for i in 0..len {
            let i_d = phase * ((4 * len - 2 * i - 1) as f64);
            let i_u = phase * ((3 * len2 + 2 * i + 1) as f64);
            let (mut sum_d, mut sum_u) = (0.0f64, 0.0f64);
            for (j, &x) in spec.iter().take(len2).enumerate() {
                let a = (2 * j + 1) as f64;
                sum_d += (a * i_d).cos() * x;
                sum_u += (a * i_u).cos() * x;
            }
            dst[i] = sum_d * -1.0;
            dst[i + len] = -sum_u * -1.0;
        }
        dst
    }

    fn lcg(state: &mut u64) -> f32 {
        *state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (((*state >> 33) as i64) as f32) / (1i64 << 31) as f32 - 1.0
    }

    #[test]
    fn imdct_matches_ffmpeg_naive_definition() {
        for &n in &[64usize, 256, 1024, 2048] {
            let mut rng = 0xdead_beef_cafe_f00du64;
            let mut out = (0..n / 2).map(|_| lcg(&mut rng)).collect::<Vec<f32>>();
            let spec = out.clone();
            let mut mdct = Mdct::new(n);
            mdct.imdct_half(&mut out);
            let spec64 = spec.iter().map(|&v| v as f64).collect::<Vec<_>>();
            let want = naive_imdct_half(&spec64, n);
            let max_err = out
                .iter()
                .zip(&want)
                .map(|(&a, &b)| (a as f64 - b).abs())
                .fold(0.0f64, f64::max);
            let scale = want.iter().fold(0.0f64, |m, &v| m.max(v.abs())).max(1e-9);
            assert!(max_err / scale < 1e-4, "n={n} rel err {max_err}");
        }
    }

    #[test]
    fn window_is_the_vorbis_window() {
        for &n in &[256usize, 2048] {
            let mdct = Mdct::new(n);
            let w = mdct.window();
            assert_eq!(w.len(), n / 2);
            assert!(w[0].abs() < 1e-3);
            // The half-window rises from 0 to ~1 at its right edge (the
            // full window's center).
            assert!((w[n / 2 - 1] - 1.0).abs() < 1e-4);
            assert!(w[n / 4] > 0.7 && w[n / 4] < 0.72);
            // Princen-Bradley complementarity: w[m]^2 + w[N/2-1-m]^2 = 1.
            let err = (0..n / 2)
                .map(|m| (w[m] * w[m] + w[n / 2 - 1 - m] * w[n / 2 - 1 - m] - 1.0).abs())
                .fold(0.0f32, |a, b| a.max(b));
            assert!(err < 1e-5, "PB complementarity err {err}");
        }
    }

    /// TDAC consistency: applying the FFmpeg overlap scheme to the transforms
    /// of 50%-overlapped windowed segments must reconstruct the signal up to
    /// one constant (possibly negative) scale, for every segment. The forward
    /// analysis is the classic MLT adjoint
    /// `X[k] = sum_n w[n] x[n] cos(pi/N (n + 1/2 + N/4)(2k+1))`.
    #[test]
    fn overlap_add_reconstructs_windowed_signal() {
        let n = 256usize;
        let half = n / 2;
        let mut mdct = Mdct::new(n);
        let win = mdct.window().to_vec();
        // Full-length analysis window (the synthesis half is its first half
        // mirrored by the PB condition).
        let win_full: Vec<f32> = (0..n)
            .map(|m| {
                let s = (std::f64::consts::PI * (m as f64 + 0.5) / n as f64).sin();
                (0.5 * std::f64::consts::PI * s * s).sin() as f32
            })
            .collect();
        let mut rng = 0x0bad_f00d_u64;
        let segs: Vec<Vec<f32>> = (0..4)
            .map(|_| (0..half).map(|_| lcg(&mut rng) * 0.5).collect())
            .collect();
        let spectrum = |block: &[f32]| -> Vec<f32> {
            let mut spec = vec![0.0f32; half];
            for k in 0..half {
                let mut acc = 0.0f64;
                for (j, &x) in block.iter().enumerate() {
                    let ang = std::f64::consts::PI / n as f64
                        * (j as f64 + 0.5 + 0.25 * n as f64)
                        * (2 * k + 1) as f64;
                    acc += x as f64 * win_full[j] as f64 * ang.cos();
                }
                spec[k] = acc as f32;
            }
            spec
        };
        let mut saved = vec![0.0f32; half / 2];
        let mut ratios: Vec<f64> = Vec::new();
        let mut prev_seg = vec![0.0f32; half];
        for (block_index, seg) in segs.iter().enumerate() {
            let mut block = vec![0.0f32; n];
            block[..half].copy_from_slice(&prev_seg);
            block[half..].copy_from_slice(seg);
            let spec = spectrum(&block);
            prev_seg = seg.clone();

            let mut buf = vec![0.0f32; half];
            buf.copy_from_slice(&spec);
            mdct.imdct_half(&mut buf);
            let mut ret = vec![0.0f32; half];
            vector_fmul_window(&mut ret, &saved, &buf, &win, half / 2);
            saved.copy_from_slice(&buf[half / 2..]);

            // Block 0 has no left neighbor: its output is a partial ramp,
            // not a reconstruction. Block k's OLA spans the *previous*
            // segment's window, so from block 1 on, `ret` must equal
            // `segs[block_index - 1]` times one global constant (the MLT
            // gain, N/4).
            if block_index == 0 {
                continue;
            }
            let recon_seg = &segs[block_index - 1];
            let peak = recon_seg.iter().fold(0.0f32, |m, &v| m.max(v.abs()));
            let mut rs = Vec::new();
            for i in 0..half {
                if recon_seg[i].abs() > 0.1 * peak {
                    rs.push(ret[i] as f64 / recon_seg[i] as f64);
                }
            }
            assert!(rs.len() > 8, "not enough probe points");
            let mean = rs.iter().sum::<f64>() / rs.len() as f64;
            let spread = rs
                .iter()
                .map(|&r| (r - mean).abs())
                .fold(0.0f64, f64::max);
            assert!(
                mean.abs() > 1e-3 && spread / mean.abs() < 1e-3,
                "non-constant ratio mean={mean} spread={spread}"
            );
            ratios.push(mean);
        }
        let base = ratios[1].abs();
        for r in &ratios[1..] {
            assert!(
                (r.abs() - base).abs() / base < 1e-6,
                "ratio not stationary: {ratios:?}"
            );
        }
    }
}
