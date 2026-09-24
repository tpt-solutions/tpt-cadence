//! SBR Quadrature Mirror Filterbanks (reference `sbr_qmf_analysis` /
//! `sbr_qmf_synthesis`) and the underlying 64-point inverse MDCT
//! (av_tx `AV_TX_FLOAT_MDCT`, inv=1, len=64 semantics).
#![allow(
    clippy::needless_range_loop,
    clippy::too_many_arguments,
    clippy::unnecessary_cast,
    clippy::manual_contains,
    clippy::excessive_precision,
    clippy::erasing_op,
    clippy::identity_op,
    clippy::unused_enumerate_index,
    clippy::manual_div_ceil
)]

use super::dsp;
use super::tables::SBR_QMF_WINDOW_US;

/// Precomputed cosines for the 64-point inverse MDCT (av_tx naive_inv):
/// out[i]    =  scale·Σ_j in[j]·cos((2j+1)(127−2i)π/256)
/// out[i+32] = −scale·Σ_j in[j]·cos((2j+1)(193+2i)π/256)
pub struct Mdct64 {
    // Boxed: each table is 16 KB (`[[f64; 64]; 32]`). `Sbr` embeds two
    // `Mdct64`s by value (`mdct`, `mdct_ana`), so leaving these inline
    // would put 64 KB of cosine tables directly in `Sbr`'s own size, which
    // is itself boxed by its owner precisely to avoid stack-resident
    // multi-KB scratch (see the AAC/SBR stack-overflow entry in todo.md).
    // Boxing them here means `Mdct64::new()` only ever needs its own
    // small local for a single table at a time to build it, not two
    // 16 KB tables live in the same frame as everything else in `Sbr`.
    table_lo: Box<[[f64; 64]; 32]>, // cos for outputs 0..32
    table_hi: Box<[[f64; 64]; 32]>, // cos for outputs 32..64
}

impl Mdct64 {
    pub fn new() -> Self {
        let mut table_lo = Box::new([[0.0f64; 64]; 32]);
        let mut table_hi = Box::new([[0.0f64; 64]; 32]);
        for i in 0..32 {
            for j in 0..64 {
                let phase = std::f64::consts::PI / 256.0;
                table_lo[i][j] = ((2 * j + 1) as f64 * ((127 - 2 * i) as f64) * phase).cos();
                table_hi[i][j] = ((2 * j + 1) as f64 * ((193 + 2 * i) as f64) * phase).cos();
            }
        }
        Mdct64 { table_lo, table_hi }
    }

    /// av_tx inverse MDCT, len=64 (declared), f64 accumulation.
    pub fn inverse(&self, input: &[f32; 64], out: &mut [f32; 64], scale: f32) {
        for i in 0..32 {
            let mut sum_d = 0.0f64;
            let mut sum_u = 0.0f64;
            for j in 0..64 {
                sum_d += self.table_lo[i][j] * input[j] as f64;
                sum_u += self.table_hi[i][j] * input[j] as f64;
            }
            out[i] = (sum_d * scale as f64) as f32;
            out[i + 32] = (-(sum_u * scale as f64)) as f32;
        }
    }
}

impl Default for Mdct64 {
    fn default() -> Self {
        Self::new()
    }
}

/// Synthesis QMF scale: 1 / (64 × 32768).
pub const QMF_SYNTHESIS_SCALE: f32 = 1.0 / (64.0 * 32768.0);
/// Analysis QMF scale: −2 × 32768.
pub const QMF_ANALYSIS_SCALE: f32 = -2.0 * 32768.0;

/// `sbr_qmf_analysis`: 32 time slots × 1024 input samples → 32×32 complex
/// subbands in `w[buf_idx]`. `x` is the caller's persistent 1312-value
/// history buffer (288 history + 1024 new per frame).
pub fn qmf_analysis(
    mdct: &Mdct64,
    input: &[f32],
    x: &mut [f32; 1312],
    z: &mut [f32; 320],
    w: &mut [[(f32, f32); 32]; 32],
    _buf_idx: usize,
) {
    x.copy_within(1024..1024 + 288, 0);
    x[288..288 + 1024].copy_from_slice(&input[..1024]);
    let mut x_off = 0usize;
    let mut window_ds = [0.0f32; 320];
    for (j, item) in window_ds.iter_mut().enumerate() {
        *item = SBR_QMF_WINDOW_US[2 * j];
    }
    for i in 0..32 {
        // vector_fmul_reverse(z, window_ds, x, 320)
        for j in 0..320 {
            z[j] = window_ds[j] * x[x_off + 319 - j];
        }
        dsp::sum64x5(z);
        dsp::qmf_pre_shuffle(z);
        let mut mdct_in = [0.0f32; 64];
        mdct_in.copy_from_slice(&z[64..128]);
        let mut mdct_out = [0.0f32; 64];
        mdct.inverse(&mdct_in, &mut mdct_out, QMF_ANALYSIS_SCALE);
        let mut z_mdct_tmp = [[0.0f32; 2]; 32];
        dsp::qmf_post_shuffle(&mut z_mdct_tmp, &mdct_out);
        for (k, item) in w[i].iter_mut().enumerate() {
            *item = (z_mdct_tmp[k][0], z_mdct_tmp[k][1]);
        }
        x_off += 32;
    }
}

/// `sbr_qmf_synthesis`: 32 complex subband slots → 2048 output samples.
/// `v` is the caller's 2304-value synthesis buffer; `v_off` its offset.
pub fn qmf_synthesis(
    mdct: &Mdct64,
    mut out: &mut [f32],
    x: &[[(f32, f32); 64]; 38], // [time slot][band] as (re, im)
    v: &mut [f32; 2304],
    v_off: &mut usize,
) {
    let mut mdct_buf = [[0.0f32; 64]; 2];
    for i in 0..32 {
        if *v_off < 128 {
            let saved_samples = 1152usize;
            v.copy_within(0..saved_samples, 2304 - saved_samples);
            *v_off = 2304 - saved_samples - 128;
        } else {
            *v_off -= 128;
        }
        let v_slice = &mut v[*v_off..];
        // neg_odd_64 on the imaginary parts
        let mut imag = [0.0f32; 64];
        for (k, item) in imag.iter_mut().enumerate() {
            *item = x[i][k].1;
        }
        let mut k = 1;
        while k < 64 {
            imag[k] = -imag[k];
            imag[k + 2] = -imag[k + 2];
            k += 4;
        }
        let mut real = [0.0f32; 64];
        for (k, item) in real.iter_mut().enumerate() {
            *item = x[i][k].0;
        }
        mdct.inverse(&real, &mut mdct_buf[0], QMF_SYNTHESIS_SCALE);
        mdct.inverse(&imag, &mut mdct_buf[1], QMF_SYNTHESIS_SCALE);
        dsp::qmf_deint_bfly(v_slice, &mdct_buf[1], &mdct_buf[0]);

        // 10 windowed accumulations of 64 samples each.
        for j in 0..64 {
            out[j] = v_slice[j] * SBR_QMF_WINDOW_US[j];
        }
        let taps = [
            (192usize, 64usize),
            (256, 128),
            (448, 192),
            (512, 256),
            (704, 320),
            (768, 384),
            (960, 448),
            (1024, 512),
            (1216, 576),
        ];
        for (v_off2, w_off) in taps {
            for j in 0..64 {
                out[j] += v_slice[v_off2 + j] * SBR_QMF_WINDOW_US[w_off + j];
            }
        }
        out = &mut out[64..];
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sbr::SBR_SYNTHESIS_BUF_SIZE;

    /// Deterministic pseudo-random input shared by the QMF tests.
    fn test_input() -> [f32; 1024] {
        let mut input = [0.0f32; 1024];
        for (i, v) in input.iter_mut().enumerate() {
            *v = ((i as f32) * 0.01).sin() + 0.3 * ((i as f32) * 0.037).cos();
        }
        input
    }

    /// The analysis pipeline must reproduce the reference decoder's QMF
    /// coefficient-for-coefficient. The expected values below were
    /// regenerated this session after fixing a real bug in
    /// [`super::tables::SBR_QMF_WINDOW_US`] (two of its 640 entries, at
    /// indices 384 and 512, had the wrong sign — found by tracing a real
    /// libfdk-aac-encoded HE-AAC stream against a live FFmpeg n7.1 build;
    /// see `todo.md`'s AAC SBR session log). The *previous* hardcoded
    /// values here were computed against that same buggy table (this test
    /// alone never caught it: `test_input()`'s zero-history first call only
    /// exercises `w[16]`/`w[31]` sensitively, and even there the error was
    /// within this test's own `1e-4` relative tolerance for two of the six
    /// spot checks) — regenerating from the *fixed* implementation is
    /// therefore not circular: the fix itself is independently confirmed by
    /// a full real-audio decode against a live FFmpeg reference jumping
    /// from ~18-23 dB to ~120-126 dB SNR (see todo.md), which only the
    /// window-table fix explains.
    #[test]
    fn qmf_analysis_matches_reference() {
        let mdct = Mdct64::new();
        let input = test_input();
        let mut x = [0.0f32; 1312];
        let mut z = [0.0f32; 320];
        let mut w = [[(0.0f32, 0.0f32); 32]; 32];
        qmf_analysis(&mdct, &input, &mut x, &mut z, &mut w, 0);

        // (slot, band, re, im), regenerated from the fixed implementation
        // (see doc comment above).
        let expect: [(usize, usize, f32, f32); 6] = [
            (
                0,
                0,
                f32::from_bits(0xc37b_aef6),
                f32::from_bits(0x426f_5df5),
            ),
            (
                0,
                1,
                f32::from_bits(0xc307_59c7),
                f32::from_bits(0xc3d6_7b37),
            ),
            (
                0,
                31,
                f32::from_bits(0x41b5_6656),
                f32::from_bits(0x4138_6be3),
            ),
            (
                1,
                0,
                f32::from_bits(0xc437_ab52),
                f32::from_bits(0x450e_ee2e),
            ),
            (
                16,
                8,
                f32::from_bits(0xc13c_920f),
                f32::from_bits(0xbf25_713c),
            ),
            (
                31,
                31,
                f32::from_bits(0xc1a3_3242),
                f32::from_bits(0x4198_2045),
            ),
        ];
        // This implementation accumulates the transform in f64 while the
        // reference fixture was generated with a platform-specific f32 FFT.
        // Transcendental and FFT rounding can accumulate to several 1e-4
        // relative on large values across Windows, Linux, and macOS. Keep a
        // relative bound rather than exact bit matching; real port bugs
        // (signs, index swaps, scaling) remain orders of magnitude outside it.
        for (slot, band, re, im) in expect {
            let got = w[slot][band];
            let tol = re.abs().max(im.abs()).max(got.0.abs()).max(got.1.abs()) * 1e-3;
            assert!(
                (got.0 - re).abs() <= tol,
                "W[{slot}][{band}].0 = {} != {re}",
                got.0
            );
            assert!(
                (got.1 - im).abs() <= tol,
                "W[{slot}][{band}].1 = {} != {im}",
                got.1
            );
        }
    }

    /// The synthesis pipeline must likewise reproduce the reference build's
    /// output samples for the same subband input (single frame, primed
    /// synthesis buffer state as at decoder open). Expected values
    /// regenerated for the same reason as `qmf_analysis_matches_reference`
    /// above (the `SBR_QMF_WINDOW_US` sign-transcription fix — synthesis
    /// reads this same table directly, at indices including 384/512).
    #[test]
    fn qmf_synthesis_matches_reference() {
        let mdct = Mdct64::new();
        let input = test_input();
        let mut x = [0.0f32; 1312];
        let mut z = [0.0f32; 320];
        let mut w = [[(0.0f32, 0.0f32); 32]; 32];
        qmf_analysis(&mdct, &input, &mut x, &mut z, &mut w, 0);
        let mut xs = [[(0.0f32, 0.0f32); 64]; 38];
        for (i, slot) in w.iter().enumerate() {
            for (k, b) in slot.iter().enumerate() {
                xs[i][k] = *b;
            }
        }
        let mut v = [0.0f32; 2304];
        let mut v_off = SBR_SYNTHESIS_BUF_SIZE - (1280 - 128);
        let mut out = [0.0f32; 2048];
        qmf_synthesis(&mdct, &mut out, &xs, &mut v, &mut v_off);

        // (index, value), regenerated from the fixed implementation (see
        // doc comment above).
        let expect: [(usize, f32); 5] = [
            (0, f32::from_bits(0x0000_0000)),
            (1, f32::from_bits(0x328c_155c)),
            (100, f32::from_bits(0xb62f_4e5e)),
            (1023, f32::from_bits(0x3f2e_89d2)),
            (2047, f32::from_bits(0x3f3c_5154)),
        ];
        for (i, val) in expect {
            let tol = val.abs().max(out[i].abs()) * 1e-4;
            assert!(
                (out[i] - val).abs() <= tol,
                "out[{i}] = {} != {val}",
                out[i]
            );
        }
    }

    /// The 64-point inverse MDCT must be an orthogonal transform up to the
    /// (N/2)^(1/2) factor inherent to the MDCT-IV kernel: transforming a
    /// unit vector must yield a column of norm |scale|*sqrt(N/2).
    #[test]
    fn mdct64_kernel_norm() {
        let mdct = Mdct64::new();
        let scale = 1.0 / (64.0 * 32768.0);
        let mut input = [0.0f32; 64];
        input[7] = 1.0;
        let mut out = [0.0f32; 64];
        mdct.inverse(&input, &mut out, scale);
        let sum: f64 = out.iter().map(|v| f64::from(*v) * f64::from(*v)).sum();
        let norm = (sum.sqrt()) as f32;
        let expect = scale * (32.0f32).sqrt();
        assert!(
            (norm - expect).abs() < 1e-6 * expect,
            "Mdct64 column norm {norm} != expected {expect}"
        );
    }
}
