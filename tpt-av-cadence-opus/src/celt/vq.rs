//! Pyramid vector quantization decode (`celt/vq.c`).
//!
//! Decodes a band's pulse vector (`alg_unquant`), applies the spread
//! rotation (`exp_rotation`), normalizes the residual against the decoded
//! PVQ norm, and provides `renormalise_vector` for folding/collapse.
//!
//! SOURCE: Xiph.Org libopus 1.5.2, `celt/vq.c` (BSD-3-Clause). Float
//! build: all fixed-point Q-macros collapse to plain `f32` math.

// Lints: this file ports libopus verbatim - verbatim float literals and
// reference loop shapes trigger these lints (see tables.rs).
#![allow(
    clippy::excessive_precision,
    clippy::approx_constant,
    clippy::precedence,
    clippy::needless_range_loop,
    clippy::manual_memcpy,
    clippy::too_many_arguments
)]

use super::cwrs::{decode_pulses, encode_pulses};
use super::math::{celt_cos_norm, celt_div, celt_udiv, EPSILON};
use crate::range::{RangeDecoder, RangeEncoder};

/// `SPREAD_NONE` (from bands.h).
pub(crate) const SPREAD_NONE: i32 = 0;
/// `SPREAD_LIGHT`.
#[allow(dead_code)]
pub(crate) const SPREAD_LIGHT: i32 = 1;
/// `SPREAD_NORMAL`.
pub(crate) const SPREAD_NORMAL: i32 = 2;
/// `SPREAD_AGGRESSIVE`.
pub(crate) const SPREAD_AGGRESSIVE: i32 = 3;

static SPREAD_FACTOR: [i32; 3] = [15, 10, 5];

/// `exp_rotation1`: one Givens rotation pass over the band.
fn exp_rotation1(x: &mut [f32], stride: usize, c: f32, s: f32) {
    let len = x.len();
    let ms = -s;
    for i in 0..len - stride {
        let x1 = x[i];
        let x2 = x[i + stride];
        x[i + stride] = c * x2 + s * x1;
        x[i] = c * x1 + ms * x2;
    }
    let mut i = len as isize - 2 * stride as isize - 1;
    while i >= 0 {
        let iu = i as usize;
        let x1 = x[iu];
        let x2 = x[iu + stride];
        x[iu + stride] = c * x2 + s * x1;
        x[iu] = c * x1 + ms * x2;
        i -= 1;
    }
}

/// `exp_rotation`: time-frequency spreading rotation for a band.
pub(crate) fn exp_rotation(x: &mut [f32], dir: i32, stride: usize, k: i32, spread: i32) {
    let len = x.len();
    if 2 * k >= len as i32 || spread == SPREAD_NONE {
        return;
    }
    let factor = SPREAD_FACTOR[(spread - 1) as usize];

    let gain = celt_div(len as f32, (len as i32 + factor * k) as f32);
    let theta = 0.5 * (gain * gain);

    let c = celt_cos_norm(theta);
    let s = celt_cos_norm(1.0 - theta); // sin(theta)

    let mut stride2 = 0usize;
    if len >= 8 * stride {
        stride2 = 1;
        // sqrt(len/stride) with rounding.
        while (stride2 * stride2 + stride2) * stride + (stride >> 2) < len {
            stride2 += 1;
        }
    }
    let blen = celt_udiv(len as u32, stride as u32) as usize;
    for i in 0..stride {
        let sub = &mut x[i * blen..(i + 1) * blen];
        if dir < 0 {
            if stride2 > 0 {
                exp_rotation1(sub, stride2, s, c);
            }
            exp_rotation1(sub, 1, c, s);
        } else {
            exp_rotation1(sub, 1, c, -s);
            if stride2 > 0 {
                exp_rotation1(sub, stride2, s, -c);
            }
        }
    }
}

/// `normalise_residual`: scales the integer pulse vector to unit norm and
/// writes it into `x` (the caller then un-rotates).
fn normalise_residual(iy: &[i32], x: &mut [f32], ryy: f32, gain: f32) {
    // g = celt_rsqrt_norm(t) * gain, float build: 1/sqrt(Ryy) * gain.
    let g = (1.0 / (ryy as f64).sqrt() as f32) * gain;
    for (xi, &iyi) in x.iter_mut().zip(iy.iter()) {
        *xi = g * iyi as f32;
    }
}

/// `extract_collapse_mask`: which time blocks of the band got energy.
fn extract_collapse_mask(iy: &[i32], b: usize) -> u32 {
    if b <= 1 {
        return 1;
    }
    let n0 = celt_udiv(iy.len() as u32, b as u32) as usize;
    let mut collapse_mask = 0u32;
    for i in 0..b {
        let mut tmp: i32 = 0;
        for j in 0..n0 {
            tmp |= iy[i * n0 + j];
        }
        collapse_mask |= u32::from(tmp != 0) << i;
    }
    collapse_mask
}

/// `alg_unquant`: decode a pulse vector and combine the result with the
/// rotated input to produce the final normalized signal for the band.
///
/// `x` holds the (possibly folded) input and receives the output; `iy` is
/// caller-provided scratch of length `n == x.len()`. Returns the collapse
/// mask.
pub(crate) fn alg_unquant(
    x: &mut [f32],
    iy: &mut [i32],
    k: i32,
    spread: i32,
    b: usize,
    dec: &mut RangeDecoder,
    gain: f32,
) -> crate::Result<u32> {
    debug_assert!(k > 0 && x.len() > 1);
    let n = x.len();
    let ryy = decode_pulses(iy, n, k as usize, dec)?;
    normalise_residual(&iy[..n], x, ryy, gain);
    exp_rotation(x, -1, b, k, spread);
    Ok(extract_collapse_mask(&iy[..n], b))
}

/// `alg_quant`: finds an integer pulse vector with exactly `k` pulses that
/// approximates the normalized band `x`, encodes it, and (when `resynth`)
/// overwrites `x` with the quantized reconstruction so downstream encoder
/// stages (e.g. folding into the next band) see what the decoder will.
///
/// The search is a greedy correlation maximizer (project onto the pyramid
/// `sum|y|==k` first when `k` is large relative to `n`, then place any
/// remaining pulses one at a time on whichever dimension best increases
/// `(x·y)^2 / |y|^2`). This is *not* a port of libopus's `alg_quant` — RFC
/// 6716 only specifies the decoder, so an encoder's pulse-search heuristic
/// has no normative reference to match bit-for-bit. Correctness here means
/// "produces a vector [`decode_pulses`] reconstructs exactly", which
/// [`encode_pulses`] guarantees by construction (see its own tests); the
/// search quality only affects coding efficiency, not correctness.
///
/// `x` holds the rotated input and (when `resynth`) receives the quantized
/// output; `iy` is caller-provided scratch of length `n == x.len()`.
/// Returns the collapse mask.
#[allow(dead_code, clippy::too_many_arguments)]
pub(crate) fn alg_quant(
    x: &mut [f32],
    iy: &mut [i32],
    k: i32,
    spread: i32,
    b: usize,
    enc: &mut RangeEncoder,
    gain: f32,
    resynth: bool,
) -> u32 {
    debug_assert!(k > 0 && x.len() > 1);
    let n = x.len();
    exp_rotation(x, 1, b, k, spread);

    // Strip the sign (PVQ search works on magnitudes; the sign is folded
    // back in once the pulse counts are chosen).
    let mut sign = vec![false; n];
    for (j, xj) in x.iter_mut().enumerate() {
        if *xj < 0.0 {
            sign[j] = true;
            *xj = -*xj;
        }
        iy[j] = 0;
    }

    let mut y = vec![0.0f32; n];
    let mut xy = 0.0f32;
    let mut yy = 0.0f32;
    let mut pulses_left = k;

    // Pre-search: when there are more pulses than dimensions, project x
    // onto the K-pulse pyramid directly instead of placing pulses one at a
    // time (an O(n) shortcut for the common "most dimensions get >=1
    // pulse" case).
    if k > (n as i32) >> 1 {
        let mut sum: f32 = x[..n].iter().sum();
        if !(sum > EPSILON && sum < 64.0) {
            // Degenerate (near-silent or non-finite) band: put everything
            // on the first coefficient rather than divide by a tiny/huge
            // sum.
            x[0] = 1.0;
            for v in x[1..n].iter_mut() {
                *v = 0.0;
            }
            sum = 1.0;
        }
        let rcp = (k as f32 + 0.8) / sum;
        for j in 0..n {
            let iyj = (rcp * x[j]).floor() as i32;
            iy[j] = iyj;
            y[j] = iyj as f32;
            yy += y[j] * y[j];
            xy += x[j] * y[j];
            y[j] *= 2.0;
            pulses_left -= iyj;
        }
    }
    debug_assert!(pulses_left >= 0);

    // Greedy refinement: place each remaining pulse on the dimension that
    // most increases the normalized correlation (x.y)^2 / yy.
    for _ in 0..pulses_left {
        let mut best_id = 0usize;
        let mut best_num = f32::NEG_INFINITY;
        let mut best_den = 0.0f32;
        yy += 1.0;
        for j in 0..n {
            let rxy = xy + x[j];
            let ryy = yy + y[j];
            let score = rxy * rxy;
            if best_den * score > ryy * best_num {
                best_den = ryy;
                best_num = score;
                best_id = j;
            }
        }
        xy += x[best_id];
        yy += y[best_id];
        y[best_id] += 2.0;
        iy[best_id] += 1;
    }

    // Restore signs.
    for j in 0..n {
        if sign[j] {
            x[j] = -x[j];
            iy[j] = -iy[j];
        }
    }

    let ryy = encode_pulses(&iy[..n], n, k as usize, enc);

    if resynth {
        normalise_residual(&iy[..n], x, ryy, gain);
        exp_rotation(x, -1, b, k, spread);
    }
    extract_collapse_mask(&iy[..n], b)
}

/// `renormalise_vector`: rescales `x` to norm `gain`.
pub(crate) fn renormalise_vector(x: &mut [f32], gain: f32) {
    // The reference computes the energy with `celt_inner_prod`, whose
    // runtime-dispatched SSE kernel accumulates in 4 strided lanes with a
    // horizontal `(s0+s2)+(s1+s3)` fold — a different rounding order than
    // a scalar loop.
    let e = EPSILON + super::pitch::celt_inner_prod_sse_order(x, x);
    let g = 1.0 / (e as f64).sqrt() as f32 * gain;
    for v in x.iter_mut() {
        *v *= g;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::range::RangeEncoder;

    /// A vector of ones renormalizes to unit norm (gain 1).
    #[test]
    fn renormalise_scales_to_gain() {
        let mut x = [2.0f32; 16];
        renormalise_vector(&mut x, 1.0);
        let norm: f32 = x.iter().map(|v| v * v).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-6, "norm={norm}");
    }

    /// exp_rotation with 2K >= len is a no-op; with spread NONE too.
    #[test]
    fn exp_rotation_noop_cases() {
        let mut x = vec![0.5f32; 16];
        let orig = x.clone();
        exp_rotation(&mut x, -1, 1, 8, SPREAD_NORMAL); // 2K >= len
        assert_eq!(x, orig);
        exp_rotation(&mut x, -1, 1, 3, SPREAD_NONE);
        assert_eq!(x, orig);
    }

    /// alg_unquant decodes a pulse vector through the range coder and
    /// produces a unit-norm band (gain=1, no spread).
    #[test]
    fn alg_unquant_produces_unit_norm() {
        let mut seed = 0xABCDu64;
        for &(n, k) in &[(4usize, 3usize), (8, 12), (16, 5)] {
            let v = super::super::cwrs::pvq_v(n, k);
            // A handful of random codewords, each a valid unit vector.
            for _ in 0..4 {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                let i = (seed % v as u64) as u32;
                let mut enc = RangeEncoder::new();
                enc.encode_uint(i, v);
                let frame = enc.done();
                let mut dec = RangeDecoder::new(&frame);
                let mut x = vec![0f32; n];
                let mut iy = vec![0i32; n];
                let cm =
                    alg_unquant(&mut x, &mut iy, k as i32, SPREAD_NONE, 1, &mut dec, 1.0).unwrap();
                assert_eq!(cm, 1);
                let norm: f32 = x.iter().map(|v| v * v).sum::<f32>().sqrt();
                assert!((norm - 1.0).abs() < 1e-4, "n={n} k={k} norm={norm}");
            }
        }
    }

    fn lcg_next(state: &mut u64) -> f32 {
        *state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ((*state >> 32) & 0x3fff_ffff) as f32 / 0x3fff_ffff as f32 - 0.5
    }

    /// `alg_quant` then `alg_unquant` round-trips through the real range
    /// coder: the decoded vector must be unit-norm (gain=1) and correlate
    /// strongly with the original target (the PVQ search should find a
    /// vector close to it, not just any valid `k`-pulse codeword).
    #[test]
    fn alg_quant_round_trips_and_correlates_with_target() {
        let mut seed = 0x1234_5678_9abc_def0u64;
        // (n, k) pairs reused from cwrs.rs's own round-trip tests, which
        // are the ones actually verified to fit the PVQ_U table's shape
        // (not every (n, k) with min(n, k) <= 14 fits — the per-row column
        // budget shrinks as the row index grows).
        for &(n, k) in &[(4usize, 3i32), (176, 3), (8, 12), (16, 5), (2, 1), (12, 15)] {
            for _ in 0..8 {
                let mut target: Vec<f32> = (0..n).map(|_| lcg_next(&mut seed)).collect();
                renormalise_vector(&mut target, 1.0);
                let original = target.clone();

                let mut x = target.clone();
                let mut iy = vec![0i32; n];
                let mut enc = RangeEncoder::new();
                let cm_enc = alg_quant(&mut x, &mut iy, k, SPREAD_NONE, 1, &mut enc, 1.0, true);
                let frame = enc.done();

                // The encoder's own resynth output must already be unit norm.
                let enc_norm: f32 = x.iter().map(|v| v * v).sum::<f32>().sqrt();
                assert!(
                    (enc_norm - 1.0).abs() < 1e-3,
                    "n={n} k={k} enc_norm={enc_norm}"
                );

                let mut dec = RangeDecoder::new(&frame);
                let mut x2 = original.clone();
                let mut iy2 = vec![0i32; n];
                let cm_dec =
                    alg_unquant(&mut x2, &mut iy2, k, SPREAD_NONE, 1, &mut dec, 1.0).unwrap();

                assert_eq!(cm_enc, cm_dec, "n={n} k={k} collapse mask mismatch");
                assert_eq!(
                    iy, iy2,
                    "n={n} k={k}: decoder didn't reproduce the encoded pulses"
                );
                // What the encoder resynthesized and what the decoder
                // independently reconstructs from the bitstream must match
                // bit-for-bit (both are `normalise_residual` over the same
                // `iy`).
                assert_eq!(x, x2, "n={n} k={k}: encoder resynth != decoder output");

                // The quantized vector should correlate positively with
                // the original target (sanity check that the search moves
                // toward it at all, not just that the bitstream is valid).
                // With very few pulses spread over many dimensions (e.g.
                // n=176, k=3) most of the target's energy is necessarily
                // uncaptured, so the bound scales down with k/n instead of
                // being a fixed threshold.
                let dot: f32 = original.iter().zip(x2.iter()).map(|(a, b)| a * b).sum();
                let min_dot = 0.15 * (k as f32 / n as f32).sqrt().min(1.0);
                assert!(
                    dot > min_dot,
                    "n={n} k={k} dot={dot} min_dot={min_dot} target={original:?} got={x2:?}"
                );
            }
        }
    }
}
