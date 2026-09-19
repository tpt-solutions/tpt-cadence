//! Per-band spectrum decoding (`celt/bands.c`) — the decode side.
//!
//! Ports `quant_all_bands` and its recursion (`quant_band`,
//! `quant_partition`, `compute_theta`), plus energy denormalization, the
//! stereo merge, anti-collapse, and the Hadamard/time-frequency
//! recombination helpers.
//!
//! SOURCE: Xiph.Org libopus 1.5.2, `celt/bands.c` + `celt.c` (small
//! pieces) (BSD-3-Clause). Float build: all fixed-point shifts/macros
//! collapse to plain `f32`/`i32` operations exactly as the reference
//! macros do.

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

use super::math::{celt_exp2, celt_sqrt, celt_sudiv, celt_udiv, ec_ilog, isqrt32};
use super::quant_bands::e_means;
use super::rate::{
    bits2pulses, cache_bits_row, get_pulses, pulses2bits, BITRES, NB_EBANDS, QTHETA_OFFSET,
    QTHETA_OFFSET_TWOPHASE,
};
use super::tables::{EBAND5MS, LOGN400};
use super::vq::{alg_unquant, renormalise_vector, SPREAD_AGGRESSIVE};
use crate::range::RangeDecoder;

/// `Q15ONE` (float build).
pub(crate) const Q15ONE: f32 = 1.0;

/// `tf_select_table` (celt.c). Positive values mean better frequency
/// resolution; negative better time resolution. Second index is
/// `4*isTransient + 2*tf_select + per_band_flag`.
pub(crate) static TF_SELECT_TABLE: [[i8; 8]; 4] = [
    // isTransient=0     isTransient=1
    [0, -1, 0, -1, 0, -1, 0, -1], // 2.5 ms
    [0, -1, 0, -2, 1, 0, 1, -1],  // 5 ms
    [0, -2, 0, -3, 2, 0, 1, -1],  // 10 ms
    [0, -2, 0, -3, 3, 0, 1, -1],  // 20 ms
];

/// `celt_lcg_rand`: the spectral folding/noise LCG.
#[inline]
pub(crate) fn celt_lcg_rand(seed: u32) -> u32 {
    1664525u32.wrapping_mul(seed).wrapping_add(1013904223)
}

/// `bitexact_cos`: a cos() approximation designed to be bit-exact on any
/// platform (it has an impact on the bit allocation).
pub(crate) fn bitexact_cos(x: i16) -> i32 {
    let tmp = (4096 + (x as i32) * (x as i32)) >> 13;
    let x2 = tmp as i16;
    let x2 = (32767 - x2 as i32
        + frac_mul16(
            x2 as i32,
            -7651 + frac_mul16(x2 as i32, 8277 + frac_mul16(-626, x2 as i32)),
        )) as i16;
    (1 + x2) as i32
}

/// `bitexact_log2tan(isin, icos)` in Q11.
pub(crate) fn bitexact_log2tan(isin: i32, icos: i32) -> i32 {
    let lc = ec_ilog(icos as u32) as i32;
    let ls = ec_ilog(isin as u32) as i32;
    let icos = icos << (15 - lc);
    let isin = isin << (15 - ls);
    (ls - lc) * (1 << 11) + frac_mul16(isin, frac_mul16(isin, -2597) + 7932)
        - frac_mul16(icos, frac_mul16(icos, -2597) + 7932)
}

/// `FRAC_MUL16`: multiplies two 16-bit fractional values (inputs are
/// truncated to `i16` first — bit-exactness of this macro is important).
#[inline]
pub(crate) fn frac_mul16(a: i32, b: i32) -> i32 {
    (16384 + (a as i16 as i32) * (b as i16 as i32)) >> 15
}

/// This prevents energy collapse for transients with multiple short MDCTs.
#[allow(clippy::too_many_arguments)]
pub(crate) fn anti_collapse(
    x: &mut [f32],
    collapse_masks: &[u8],
    lm: usize,
    c: usize,
    size: usize,
    start: usize,
    end: usize,
    log_e: &[f32],
    prev1_log_e: &[f32],
    prev2_log_e: &[f32],
    pulses: &[i32],
    seed: u32,
) -> u32 {
    let mut seed = seed;
    for i in start..end {
        let n0 = (EBAND5MS[i + 1] - EBAND5MS[i]) as usize;
        // depth in 1/8 bits
        debug_assert!(pulses[i] >= 0);
        let depth =
            celt_udiv(1 + pulses[i] as u32, (EBAND5MS[i + 1] - EBAND5MS[i]) as u32) >> lm as u32;

        let thresh = 0.5 * celt_exp2(-0.125 * depth as f32);
        let sqrt_1 = 1.0 / celt_sqrt((n0 << lm) as f32);

        for ci in 0..c {
            let mut renormalize = false;
            let mut prev1 = prev1_log_e[ci * NB_EBANDS + i];
            let mut prev2 = prev2_log_e[ci * NB_EBANDS + i];
            if c == 1 {
                prev1 = prev1.max(prev1_log_e[NB_EBANDS + i]);
                prev2 = prev2.max(prev2_log_e[NB_EBANDS + i]);
            }
            let mut ediff = log_e[ci * NB_EBANDS + i] - prev1.min(prev2);
            ediff = ediff.max(0.0);

            // r needs to be multiplied by 2 or 2*sqrt(2) depending on LM
            // because short blocks don't have the same energy as long.
            let mut r = 2.0 * celt_exp2(-ediff);
            if lm == 3 {
                r *= 1.414_213_56;
            }
            r = r.min(thresh);
            r *= sqrt_1;

            let x_off = ci * size + ((EBAND5MS[i] as usize) << lm);
            for k in 0..(1usize << lm) {
                // Detect collapse.
                if collapse_masks[i * c + ci] & (1u8 << k) == 0 {
                    // Fill with noise.
                    for j in 0..n0 {
                        seed = celt_lcg_rand(seed);
                        x[x_off + (j << lm) + k] = if seed & 0x8000 != 0 { r } else { -r };
                    }
                    renormalize = true;
                }
            }
            // We just added some energy, so we need to renormalise.
            if renormalize {
                let n = n0 << lm;
                renormalise_vector(&mut x[x_off..x_off + n], Q15ONE);
            }
        }
    }
    seed
}

/// `denormalise_bands`: de-normalize the energy to produce the synthesis
/// from the unit-energy bands.
pub(crate) fn denormalise_bands(
    x: &[f32],
    freq: &mut [f32],
    band_log_e: &[f32],
    start: usize,
    end: usize,
    m: usize,
    downsample: usize,
    silence: bool,
) {
    let n = m * 120; // M * shortMdctSize
    let mut bound = m * EBAND5MS[end] as usize;
    if downsample != 1 {
        bound = bound.min(n / downsample);
    }
    if silence {
        freq[..n].fill(0.0);
        return;
    }
    let mut f = 0usize; // freq write cursor
    let mut xi = m * EBAND5MS[start] as usize; // x read cursor
    for _ in 0..m * EBAND5MS[start] as usize {
        freq[f] = 0.0;
        f += 1;
    }
    for i in start..end {
        let band_end = m * EBAND5MS[i + 1] as usize;
        let lg = band_log_e[i] + e_means(i);
        let g = celt_exp2(32.0f32.min(lg));
        while f < band_end {
            freq[f] = x[xi] * g;
            f += 1;
            xi += 1;
        }
    }
    debug_assert!(bound >= f);
    freq[f..n.max(f)].fill(0.0);
    let _ = bound;
}

/// `haar1`: one Haar wavelet recombination pass.
pub(crate) fn haar1(x: &mut [f32], n0: usize, stride: usize) {
    let n0 = n0 >> 1;
    for i in 0..stride {
        for j in 0..n0 {
            let tmp1 = 0.707_106_78 * x[stride * 2 * j + i];
            let tmp2 = 0.707_106_78 * x[stride * (2 * j + 1) + i];
            x[stride * 2 * j + i] = tmp1 + tmp2;
            x[stride * (2 * j + 1) + i] = tmp1 - tmp2;
        }
    }
}

/// Indexing table for converting from natural Hadamard to ordered Hadamard
/// (bit-reversed Gray, order-inverted so DC lands at the end).
static ORDERY_TABLE: [usize; 30] = [
    1, 0, //
    3, 0, 2, 1, //
    7, 0, 4, 3, 6, 1, 5, 2, //
    15, 0, 8, 7, 12, 3, 11, 4, 14, 1, 9, 6, 13, 2, 10, 5,
];

fn deinterleave_hadamard(x: &mut [f32], n0: usize, stride: usize, hadamard: bool, tmp: &mut [f32]) {
    let n = n0 * stride;
    debug_assert!(stride > 0);
    if hadamard {
        let ordery = &ORDERY_TABLE[stride - 2..];
        for i in 0..stride {
            for j in 0..n0 {
                tmp[ordery[i] * n0 + j] = x[j * stride + i];
            }
        }
    } else {
        for i in 0..stride {
            for j in 0..n0 {
                tmp[i * n0 + j] = x[j * stride + i];
            }
        }
    }
    x[..n].copy_from_slice(&tmp[..n]);
}

fn interleave_hadamard(x: &mut [f32], n0: usize, stride: usize, hadamard: bool, tmp: &mut [f32]) {
    let n = n0 * stride;
    if hadamard {
        let ordery = &ORDERY_TABLE[stride - 2..];
        for i in 0..stride {
            for j in 0..n0 {
                tmp[j * stride + i] = x[ordery[i] * n0 + j];
            }
        }
    } else {
        for i in 0..stride {
            for j in 0..n0 {
                tmp[j * stride + i] = x[i * n0 + j];
            }
        }
    }
    x[..n].copy_from_slice(&tmp[..n]);
}

fn compute_qn(n: usize, b: i32, offset: i32, pulse_cap: i32, stereo: bool) -> i32 {
    static EXP2_TABLE8: [i32; 8] = [16384, 17866, 19483, 21247, 23170, 25267, 27554, 30048];
    let mut n2 = 2 * n as i32 - 1;
    if stereo && n == 2 {
        n2 -= 1;
    }
    // The upper limit ensures that in a stereo split with itheta==16384,
    // we'll always have enough bits left over to code at least one pulse
    // in the side; otherwise it would collapse, since it doesn't fold.
    let mut qb = celt_sudiv(b + n2 * offset, n2);
    qb = (b - pulse_cap - (4 << BITRES)).min(qb);
    qb = (8 << BITRES).min(qb);

    if qb < (1 << BITRES >> 1) {
        1
    } else {
        let q = EXP2_TABLE8[(qb & 0x7) as usize] >> (14 - (qb >> BITRES));
        (q + 1) >> 1 << 1
    }
}

/// `stereo_merge`: recombines the decoded mid/side bands in place.
pub(crate) fn stereo_merge(x: &mut [f32], y: &mut [f32], mid: f32) {
    let n = x.len();
    // Compute the norm of X+Y and X-Y as |X|^2 + |Y|^2 +/- sum(xy):
    // dual_inner_prod(Y, X, Y, N, &xp, &side).
    let mut xp = 0f32;
    let mut side = 0f32;
    for i in 0..n {
        xp += y[i] * x[i];
        side += y[i] * y[i];
    }
    // Compensating for the mid normalization.
    xp *= mid;
    let mid2 = mid; // float build: SHR16(mid,1) is identity
    let el = mid2 * mid2 + side - 2.0 * xp;
    let er = mid2 * mid2 + side + 2.0 * xp;
    if er < 6e-4 || el < 6e-4 {
        y.copy_from_slice(x);
        return;
    }
    let lgain = 1.0 / celt_sqrt(el);
    let rgain = 1.0 / celt_sqrt(er);

    for j in 0..n {
        // Apply mid scaling (side is already scaled).
        let l = mid * x[j];
        let r = y[j];
        x[j] = lgain * (l - r);
        y[j] = rgain * (l + r);
    }
}

/// `special_hybrid_folding`: duplicate enough of the first band folding
/// data to fold the second band (copies no data for CELT-only mode).
fn special_hybrid_folding(
    norm: &mut [f32],
    norm2: Option<&mut [f32]>,
    start: usize,
    m: usize,
    dual_stereo: bool,
) {
    let n1 = m * (EBAND5MS[start + 1] - EBAND5MS[start]) as usize;
    let n2 = m * (EBAND5MS[start + 2] - EBAND5MS[start + 1]) as usize;
    norm.copy_within(2 * n1 - n2..2 * n1 - n2 + (n2 - n1), n1);
    if let (true, Some(norm2)) = (dual_stereo, norm2) {
        norm2.copy_within(2 * n1 - n2..2 * n1 - n2 + (n2 - n1), n1);
    }
}

/// Per-band split parameters from `compute_theta`.
struct SplitCtx {
    inv: bool,
    imid: i32,
    iside: i32,
    delta: i32,
    itheta: i32,
    qalloc: i32,
}

/// Shared per-call state for the band recursion.
pub(crate) struct BandCtx {
    pub i: usize,
    pub intensity: usize,
    pub spread: i32,
    pub tf_change: i32,
    /// In 1/8-bit units.
    pub remaining_bits: i32,
    pub seed: u32,
    pub disable_inv: bool,
}

/// `compute_theta` (decode side): reads the split angle and derives the
/// mid/side factors. `bs` is the current `B`, `b0` the pre-split `B0`.
#[allow(clippy::too_many_arguments)]
fn compute_theta(
    ctx: &mut BandCtx,
    dec: &mut RangeDecoder,
    n: usize,
    b: &mut i32,
    bs: usize,
    b0: usize,
    lm: i32,
    stereo: bool,
    fill: &mut u32,
) -> crate::Result<SplitCtx> {
    let i = ctx.i;
    let intensity = ctx.intensity;

    // Decide on the resolution to give to the split parameter theta.
    let pulse_cap = LOGN400[i] as i32 + lm * (1 << BITRES);
    let offset = (pulse_cap >> 1)
        - if stereo && n == 2 {
            QTHETA_OFFSET_TWOPHASE
        } else {
            QTHETA_OFFSET
        };
    let mut qn = compute_qn(n, *b, offset, pulse_cap, stereo);
    if stereo && i >= intensity {
        qn = 1;
    }
    let tell = dec.tell_frac();
    let mut itheta: i32 = 0;
    let mut inv = false;
    if qn != 1 {
        // Entropy coding of the angle: uniform for time splits, step for
        // stereo, triangular otherwise.
        if stereo && n > 2 {
            let p0 = 3i64;
            let x0 = (qn / 2) as i64;
            let ft = p0 * (x0 + 1) + x0;
            let fs = dec.decode(ft as u32)?;
            let fs = fs as i64;
            let x = if fs < (x0 + 1) * p0 {
                fs / p0
            } else {
                x0 + 1 + (fs - (x0 + 1) * p0)
            };
            let fl = if x <= x0 {
                p0 * x
            } else {
                (x - 1 - x0) + (x0 + 1) * p0
            };
            let fh = if x <= x0 {
                p0 * (x + 1)
            } else {
                (x - x0) + (x0 + 1) * p0
            };
            dec.update(fl as u32, fh as u32, ft as u32);
            itheta = x as i32;
        } else if b0 > 1 || stereo {
            // Uniform pdf.
            itheta = dec.decode_uint((qn + 1) as u32)? as i32;
        } else {
            // Triangular pdf.
            let ft = ((qn >> 1) + 1) * ((qn >> 1) + 1);
            let fm = dec.decode(ft as u32)? as i32;
            let (fs, fl) = if fm < ((qn >> 1) * ((qn >> 1) + 1) >> 1) {
                itheta = (isqrt32(8 * fm as u32 + 1) as i32 - 1) >> 1;
                (itheta + 1, itheta * (itheta + 1) >> 1)
            } else {
                itheta = (2 * (qn + 1) - isqrt32(8 * (ft - fm - 1) as u32 + 1) as i32) >> 1;
                (
                    qn + 1 - itheta,
                    ft - ((qn + 1 - itheta) * (qn + 2 - itheta) >> 1),
                )
            };
            dec.update(fl as u32, (fl + fs) as u32, ft as u32);
        }
        debug_assert!(itheta >= 0);
        itheta = celt_udiv((itheta * 16384) as u32, qn as u32) as i32;
    } else if stereo {
        if *b > 2 << BITRES && ctx.remaining_bits > 2 << BITRES {
            inv = dec.decode_bit_logp(2)?;
        }
        // inv flag override to avoid problems with downmixing.
        if ctx.disable_inv {
            inv = false;
        }
        itheta = 0;
    }
    let qalloc = dec.tell_frac() as i32 - tell as i32;
    *b -= qalloc;

    let (imid, iside, delta);
    if itheta == 0 {
        imid = 32767;
        iside = 0;
        *fill &= (1u32 << bs).wrapping_sub(1);
        delta = -16384;
    } else if itheta == 16384 {
        imid = 0;
        iside = 32767;
        *fill &= ((1u32 << bs).wrapping_sub(1)) << bs;
        delta = 16384;
    } else {
        imid = bitexact_cos(itheta as i16);
        iside = bitexact_cos((16384 - itheta) as i16);
        // This is the mid vs side allocation that minimizes squared error
        // in that band.
        delta = frac_mul16(((n - 1) << 7) as i32, bitexact_log2tan(iside, imid));
    }
    Ok(SplitCtx {
        inv,
        imid,
        iside,
        delta,
        itheta,
        qalloc,
    })
}

/// `quant_band_n1`: the special case for one-sample bands (a raw sign per
/// channel).
fn quant_band_n1(
    ctx: &mut BandCtx,
    dec: &mut RangeDecoder,
    x: &mut [f32],
    y: Option<&mut [f32]>,
    lowband_out: Option<&mut [f32]>,
) -> crate::Result<()> {
    let mut sign = 0i32;
    if ctx.remaining_bits >= 1 << BITRES {
        sign = dec.read_raw_bits(1) as i32;
        ctx.remaining_bits -= 1 << BITRES;
    }
    x[0] = if sign != 0 { -1.0 } else { 1.0 };
    if let Some(y) = y {
        let mut sign = 0i32;
        if ctx.remaining_bits >= 1 << BITRES {
            sign = dec.read_raw_bits(1) as i32;
            ctx.remaining_bits -= 1 << BITRES;
        }
        y[0] = if sign != 0 { -1.0 } else { 1.0 };
    }
    if let Some(lb) = lowband_out {
        lb[0] = x[0];
    }
    Ok(())
}

static BIT_INTERLEAVE: [u32; 16] = [0, 1, 1, 1, 2, 3, 3, 3, 2, 3, 3, 3, 2, 3, 3, 3];
static BIT_DEINTERLEAVE: [u32; 16] = [
    0x00, 0x03, 0x0C, 0x0F, 0x30, 0x33, 0x3C, 0x3F, 0xC0, 0xC3, 0xCC, 0xCF, 0xF0, 0xF3, 0xFC, 0xFF,
];

/// `quant_partition`: responsible for decoding a mono partition; splits
/// the band in two and transmits the energy difference with the two
/// half-bands, recursively.
#[allow(clippy::too_many_arguments, clippy::only_used_in_recursion)]
fn quant_partition(
    ctx: &mut BandCtx,
    dec: &mut RangeDecoder,
    x: &mut [f32],
    n: usize,
    mut b: i32,
    b_blocks: usize,
    mut lowband: Option<&mut [f32]>,
    lm: i32,
    gain: f32,
    mut fill: u32,
    htmp: &mut [f32],
    iy: &mut [i32],
) -> crate::Result<u32> {
    let i = ctx.i;
    let spread = ctx.spread;
    let b0 = b_blocks;

    // If we need 1.5 more bits than we can produce, split the band in two.
    let cache = cache_bits_row(i, lm + 1);
    if lm != -1 && b > cache[cache[0] as usize] as i32 + 12 && n > 2 {
        let n2 = n >> 1;
        let (x0, x1) = x.split_at_mut(n2);
        let lm2 = lm - 1;
        if b_blocks == 1 {
            fill = (fill & 1) | (fill << 1);
        }
        let b2 = (b_blocks + 1) >> 1;

        let sctx = compute_theta(ctx, dec, n2, &mut b, b2, b0, lm2, false, &mut fill)?;
        let imid = (1.0f32 / 32768.0) * sctx.imid as f32;
        let iside = (1.0f32 / 32768.0) * sctx.iside as f32;
        let mut delta = sctx.delta;
        let itheta = sctx.itheta;
        let qalloc = sctx.qalloc;

        // Give more bits to low-energy MDCTs than they would otherwise
        // deserve.
        if b0 > 1 && (itheta & 0x3fff) != 0 {
            if itheta > 8192 {
                // Rough approximation for pre-echo masking.
                delta -= delta >> (4 - lm2);
            } else {
                // Corresponds to a forward-masking slope of 1.5 dB per
                // 10 ms.
                delta = 0.min(delta + ((n2 << BITRES) >> (5 - lm2) as u32) as i32);
            }
        }
        let mut mbits = 0.max(b.min((b - delta) / 2));
        let mut sbits = b - mbits;
        ctx.remaining_bits -= qalloc;

        let (lb1, lb2) = match lowband.take() {
            Some(lb) => {
                let (a, c) = lb.split_at_mut(n2);
                (Some(a), Some(c))
            }
            None => (None, None),
        };

        let mut rebalance = ctx.remaining_bits;
        let cm = if mbits >= sbits {
            let c1 = quant_partition(
                ctx,
                dec,
                x0,
                n2,
                mbits,
                b2,
                lb1,
                lm2,
                gain * imid,
                fill,
                htmp,
                iy,
            )?;
            rebalance = mbits - (rebalance - ctx.remaining_bits);
            if rebalance > 3 << BITRES && itheta != 0 {
                sbits += rebalance - (3 << BITRES);
            }
            c1 | (quant_partition(
                ctx,
                dec,
                x1,
                n2,
                sbits,
                b2,
                lb2,
                lm2,
                gain * iside,
                fill >> b2,
                htmp,
                iy,
            )? << (b0 >> 1))
        } else {
            let c1 = quant_partition(
                ctx,
                dec,
                x1,
                n2,
                sbits,
                b2,
                lb2,
                lm2,
                gain * iside,
                fill >> b2,
                htmp,
                iy,
            )? << (b0 >> 1);
            rebalance = sbits - (rebalance - ctx.remaining_bits);
            if rebalance > 3 << BITRES && itheta != 16384 {
                mbits += rebalance - (3 << BITRES);
            }
            c1 | quant_partition(
                ctx,
                dec,
                x0,
                n2,
                mbits,
                b2,
                lb1,
                lm2,
                gain * imid,
                fill,
                htmp,
                iy,
            )?
        };
        Ok(cm)
    } else {
        // This is the basic no-split case.
        let mut q = bits2pulses(i, lm, b);
        let mut curr_bits = pulses2bits(i, lm, q);
        ctx.remaining_bits -= curr_bits;

        // Ensures we can never bust the budget.
        while ctx.remaining_bits < 0 && q > 0 {
            ctx.remaining_bits += curr_bits;
            q -= 1;
            curr_bits = pulses2bits(i, lm, q);
            ctx.remaining_bits -= curr_bits;
        }

        if crate::debug::flags().celt_band_trace {
            eprintln!(
                "  LEAF i={i} n={n} b={b} q={q} curr_bits={curr_bits} b_blocks={b_blocks} \
                 tell_frac_before={}",
                dec.tell_frac()
            );
        }

        if q != 0 {
            let k = get_pulses(q);
            // Finally do the actual quantization.
            let r = alg_unquant(&mut x[..n], &mut iy[..n], k, spread, b_blocks, dec, gain)?;
            if crate::debug::flags().celt_band_trace {
                eprintln!("  LEAF i={i} k={k} tell_frac_after={}", dec.tell_frac());
            }
            Ok(r)
        } else {
            // If there's no pulse, fill the band anyway (decoder always
            // resynthesizes).
            let cm_mask = (1u32 << b_blocks).wrapping_sub(1);
            fill &= cm_mask;
            if fill == 0 {
                x[..n].fill(0.0);
                Ok(0)
            } else {
                let cm = match lowband {
                    None => {
                        // Noise.
                        for j in 0..n {
                            ctx.seed = celt_lcg_rand(ctx.seed);
                            x[j] = ((ctx.seed as i32) >> 20) as f32;
                        }
                        cm_mask
                    }
                    Some(lb) => {
                        // Folded spectrum (about 48 dB below the "normal"
                        // folding level).
                        for j in 0..n {
                            ctx.seed = celt_lcg_rand(ctx.seed);
                            let tmp = if ctx.seed & 0x8000 != 0 {
                                1.0f32 / 256.0
                            } else {
                                -(1.0f32 / 256.0)
                            };
                            x[j] = lb[j] + tmp;
                        }
                        fill
                    }
                };
                renormalise_vector(&mut x[..n], gain);
                Ok(cm)
            }
        }
    }
}

/// `quant_band`: decodes one band for the mono case (with the time /
/// frequency recombination pre/post-processing around `quant_partition`).
#[allow(clippy::too_many_arguments)]
fn quant_band(
    ctx: &mut BandCtx,
    dec: &mut RangeDecoder,
    x: &mut [f32],
    n: usize,
    b: i32,
    b_blocks: usize,
    lowband: Option<&mut [f32]>,
    lm: i32,
    lowband_out: Option<&mut [f32]>,
    gain: f32,
    lowband_scratch: Option<&mut [f32]>,
    mut fill: u32,
    htmp: &mut [f32],
    iy: &mut [i32],
) -> crate::Result<u32> {
    let n0 = n;
    let mut n_b = n;
    let b0_orig = b_blocks;
    let mut time_divide = 0usize;
    let mut recombine = 0usize;
    let long_blocks = b0_orig == 1;
    let mut tf_change = ctx.tf_change;

    // Special case for one sample.
    if n == 1 {
        quant_band_n1(ctx, dec, x, None, lowband_out)?;
        return Ok(1);
    }

    n_b /= b_blocks;

    if tf_change > 0 {
        recombine = tf_change as usize;
    }

    // Band recombining to increase frequency resolution.
    let mut lowband = lowband;
    let need_copy = lowband.is_some()
        && lowband_scratch.is_some()
        && (recombine > 0 || ((n_b & 1) == 0 && tf_change < 0) || b0_orig > 1);
    if need_copy {
        let lb = lowband.take().unwrap();
        let sc = lowband_scratch.unwrap();
        // `lowband` is an open-ended slice into the shared norm buffer
        // (reference pointer semantics), so the full band width is always
        // available to copy.
        sc[..n].copy_from_slice(&lb[..n]);
        lowband = Some(sc);
    }
    let _ = lowband_scratch;

    for k in 0..recombine {
        if let Some(lb) = lowband.as_deref_mut() {
            haar1(lb, n >> k, 1 << k);
        }
        fill = BIT_INTERLEAVE[(fill & 0xF) as usize] | BIT_INTERLEAVE[(fill >> 4) as usize] << 2;
    }
    let mut b_blocks = b_blocks >> recombine;
    n_b <<= recombine;

    // Increasing the time resolution.
    while (n_b & 1) == 0 && tf_change < 0 {
        if let Some(lb) = lowband.as_deref_mut() {
            haar1(lb, n_b, b_blocks);
        }
        fill |= fill << b_blocks;
        b_blocks <<= 1;
        n_b >>= 1;
        time_divide += 1;
        tf_change += 1;
    }
    let b0 = b_blocks;
    let n_b0 = n_b;

    // Reorganize the samples in time order instead of frequency order.
    if b0 > 1 {
        if let Some(lb) = lowband.as_deref_mut() {
            deinterleave_hadamard(lb, n_b >> recombine, b0 << recombine, long_blocks, htmp);
        }
    }

    let mut cm = quant_partition(
        ctx, dec, x, n, b, b_blocks, lowband, lm, gain, fill, htmp, iy,
    )?;

    // This code is used by the decoder (resynth == true).
    // Undo the sample reorganization going from time order to frequency
    // order.
    if b0 > 1 {
        interleave_hadamard(x, n_b0 >> recombine, b0 << recombine, long_blocks, htmp);
    }

    // Undo time-freq changes that we did earlier.
    n_b = n_b0;
    let mut b_blocks = b0;
    for _ in 0..time_divide {
        b_blocks >>= 1;
        n_b <<= 1;
        cm |= cm >> b_blocks;
        haar1(x, n_b, b_blocks);
    }

    for k in 0..recombine {
        cm = BIT_DEINTERLEAVE[cm as usize];
        haar1(x, n0 >> k, 1 << k);
    }
    let b_final = b0 << recombine;

    // Scale output for later folding.
    if let Some(lb) = lowband_out {
        let nn = celt_sqrt(n0 as f32);
        for j in 0..n0 {
            lb[j] = nn * x[j];
        }
    }
    Ok(cm & ((1u32 << b_final) - 1))
}

/// `quant_band_stereo`: decodes one band for the stereo case.
#[allow(clippy::too_many_arguments)]
fn quant_band_stereo(
    ctx: &mut BandCtx,
    dec: &mut RangeDecoder,
    x: &mut [f32],
    y: &mut [f32],
    n: usize,
    b: i32,
    b_blocks: usize,
    lowband: Option<&mut [f32]>,
    lm: i32,
    lowband_out: Option<&mut [f32]>,
    lowband_scratch: Option<&mut [f32]>,
    fill: u32,
    htmp: &mut [f32],
    iy: &mut [i32],
) -> crate::Result<u32> {
    // Special case for one sample.
    if n == 1 {
        quant_band_n1(ctx, dec, x, Some(y), lowband_out)?;
        return Ok(1);
    }

    let orig_fill = fill;
    let mut fill = fill;
    let mut b = b;

    let sctx = compute_theta(ctx, dec, n, &mut b, b_blocks, b_blocks, lm, true, &mut fill)?;
    let inv = sctx.inv;
    let mid = (1.0f32 / 32768.0) * sctx.imid as f32;
    let side = (1.0f32 / 32768.0) * sctx.iside as f32;
    let delta = sctx.delta;
    let itheta = sctx.itheta;
    let qalloc = sctx.qalloc;

    let mut cm;
    // This is a special case for N=2 that only works for stereo and takes
    // advantage of the fact that mid and side are orthogonal to encode the
    // side with just one bit.
    if n == 2 {
        let mut mbits = b;
        let mut sbits = 0i32;
        // Only need one bit for the side.
        if itheta != 0 && itheta != 16384 {
            sbits = 1 << BITRES;
        }
        mbits -= sbits;
        let swap = itheta > 8192;
        ctx.remaining_bits -= qalloc + sbits;

        let (x2, y2): (&mut [f32], &mut [f32]) = if swap { (y, x) } else { (x, y) };
        let mut sign = 0i32;
        if sbits > 0 {
            // Here we only need to decode a sign for the side.
            sign = dec.read_raw_bits(1) as i32;
        }
        let sign = 1 - 2 * sign;
        // We use orig_fill here because we want to fold the side, but if
        // itheta==16384, we'll have cleared the low bits of fill.
        cm = quant_band(
            ctx,
            dec,
            x2,
            n,
            mbits,
            b_blocks,
            lowband,
            lm,
            lowband_out,
            Q15ONE,
            lowband_scratch,
            orig_fill,
            htmp,
            iy,
        )?;
        // We don't split N=2 bands, so cm is either 1 or 0 (for a
        // fold-collapse), and there's no need to worry about mixing with
        // the other channel.
        y2[0] = if sign == 1 { -x2[1] } else { x2[1] };
        y2[1] = if sign == 1 { x2[0] } else { -x2[0] };
        // Decoder resynthesis.
        x[0] *= mid;
        x[1] *= mid;
        y[0] *= side;
        y[1] *= side;
        let tmp = x[0];
        x[0] = tmp - y[0];
        y[0] += tmp;
        let tmp = x[1];
        x[1] = tmp - y[1];
        y[1] += tmp;
        if inv {
            for v in y.iter_mut() {
                *v = -*v;
            }
        }
    } else {
        // "Normal" split code.
        let mut mbits = 0.max(b.min((b - delta) / 2));
        let mut sbits = b - mbits;
        ctx.remaining_bits -= qalloc;

        let mut rebalance = ctx.remaining_bits;
        if mbits >= sbits {
            // In stereo mode, we do not apply a scaling to the mid because
            // we need the normalized mid for folding later.
            cm = quant_band(
                ctx,
                dec,
                x,
                n,
                mbits,
                b_blocks,
                lowband,
                lm,
                lowband_out,
                Q15ONE,
                lowband_scratch,
                fill,
                htmp,
                iy,
            )?;
            rebalance = mbits - (rebalance - ctx.remaining_bits);
            if rebalance > 3 << BITRES && itheta != 0 {
                sbits += rebalance - (3 << BITRES);
            }

            // For a stereo split, the high bits of fill are always zero,
            // so no folding will be done to the side.
            cm |= quant_band(
                ctx,
                dec,
                y,
                n,
                sbits,
                b_blocks,
                None,
                lm,
                None,
                side,
                None,
                fill >> b_blocks,
                htmp,
                iy,
            )?;
        } else {
            cm = quant_band(
                ctx,
                dec,
                y,
                n,
                sbits,
                b_blocks,
                None,
                lm,
                None,
                side,
                None,
                fill >> b_blocks,
                htmp,
                iy,
            )?;
            rebalance = sbits - (rebalance - ctx.remaining_bits);
            if rebalance > 3 << BITRES && itheta != 16384 {
                mbits += rebalance - (3 << BITRES);
            }
            cm |= quant_band(
                ctx,
                dec,
                x,
                n,
                mbits,
                b_blocks,
                lowband,
                lm,
                lowband_out,
                Q15ONE,
                lowband_scratch,
                fill,
                htmp,
                iy,
            )?;
        }
        // Decoder resynthesis.
        if n != 2 {
            stereo_merge(x, y, mid);
        }
        if inv {
            for v in y.iter_mut() {
                *v = -*v;
            }
        }
    }
    Ok(cm)
}

/// `quant_all_bands` (decode): decodes all bands' normalized spectra into
/// `x` (layout: channel plane 0 `[0..N)`, channel plane 1 `[N..2N)`).
///
/// Scratch (`norm` = `C * norm_len` floats, `scratch`/`htmp` = 176 floats
/// each, `iy` = 176 i32) must be provided by the caller so `decode()`
/// stays allocation-free.
#[allow(clippy::too_many_arguments)]
pub(crate) fn quant_all_bands(
    dec: &mut RangeDecoder,
    start: usize,
    end: usize,
    x: &mut [f32],
    stereo: bool,
    collapse_masks: &mut [u8],
    pulses: &[i32],
    short_blocks: bool,
    spread: i32,
    mut dual_stereo: bool,
    intensity: usize,
    tf_res: &[i32],
    total_bits: i32,
    mut balance: i32,
    lm: usize,
    coded_bands: usize,
    seed: &mut u32,
    disable_inv: bool,
    norm: &mut [f32],
    scratch: &mut [f32],
    htmp: &mut [f32],
    iy: &mut [i32],
) -> crate::Result<()> {
    let c = if stereo { 2 } else { 1 };
    let m = 1 << lm;
    let b_blocks0 = if short_blocks { m } else { 1 };
    let norm_offset = m * EBAND5MS[start] as usize;
    let norm_len = m * EBAND5MS[NB_EBANDS - 1] as usize - norm_offset;

    let n_total = m * 120;
    let (x0, mut x1) = if stereo {
        let (a, b) = x.split_at_mut(n_total);
        (a, Some(b))
    } else {
        (x, None)
    };
    let (norm0, mut norm1) = if stereo {
        let (a, b) = norm.split_at_mut(norm_len);
        (a, Some(b))
    } else {
        (norm, None)
    };

    let mut lowband_offset = 0usize;
    let mut update_lowband = true;
    let mut ctx = BandCtx {
        i: start,
        intensity,
        spread,
        tf_change: 0,
        remaining_bits: 0,
        seed: *seed,
        disable_inv,
    };
    // The encoder-side `avoid_split_noise` knob only affects encoding.

    for i in start..end {
        ctx.i = i;
        let last = i == end - 1;
        let n = m * (EBAND5MS[i + 1] - EBAND5MS[i]) as usize;
        debug_assert!(n > 0);
        let tell = dec.tell_frac() as i32;

        // Compute how many bits we want to allocate to this band.
        if i != start {
            balance -= tell;
        }
        let remaining_bits = total_bits - tell - 1;
        ctx.remaining_bits = remaining_bits;
        let b = if i < coded_bands {
            let curr_balance = celt_sudiv(balance, 3.min((coded_bands - i) as i32));
            0.max(16383.min((remaining_bits + 1).min(pulses[i] + curr_balance)))
        } else {
            0
        };

        if crate::debug::flags().celt_band_trace {
            eprintln!(
                "BAND i={i} n={n} b_blocks0={b_blocks0} tell={tell} balance_in={balance} b={b} \
                 pulses_i={} remaining_bits={remaining_bits}",
                pulses[i]
            );
        }

        if (m * EBAND5MS[i] as usize >= n + m * EBAND5MS[start] as usize || i == start + 1)
            && (update_lowband || lowband_offset == 0)
        {
            lowband_offset = i;
        }
        if i == start + 1 {
            let n1r = norm1.as_deref_mut();
            special_hybrid_folding(norm0, n1r, start, m, dual_stereo);
        }

        ctx.tf_change = tf_res[i];
        // `i >= effEBands` remapping never triggers for the standard mode
        // (end <= effEBands), so X always stays the output spectrum.

        // Get a conservative estimate of the collapse_masks for the bands
        // we're going to be folding from.
        let effective_lowband = if lowband_offset != 0
            && (spread != SPREAD_AGGRESSIVE || b_blocks0 > 1 || ctx.tf_change < 0)
        {
            // This ensures we never repeat spectral content within one
            // band.
            Some((m * EBAND5MS[lowband_offset] as usize).saturating_sub(norm_offset + n))
        } else {
            None
        };
        let (mut x_cm, mut y_cm);
        if let Some(eff) = effective_lowband {
            let mut fold_start = lowband_offset;
            loop {
                fold_start -= 1;
                if m * EBAND5MS[fold_start] as usize <= eff + norm_offset {
                    break;
                }
            }
            let mut fold_end = lowband_offset - 1;
            loop {
                fold_end += 1;
                if !(fold_end < i && (m * EBAND5MS[fold_end] as usize) < eff + norm_offset + n) {
                    break;
                }
            }
            x_cm = 0;
            y_cm = 0;
            for fold_i in fold_start..fold_end {
                x_cm |= collapse_masks[fold_i * c] as u32;
                y_cm |= collapse_masks[fold_i * c + c - 1] as u32;
            }
        } else {
            // Otherwise, we'll be using the LCG to fold, so all blocks
            // will (almost always) be non-zero.
            x_cm = (1u32 << b_blocks0) - 1;
            y_cm = x_cm;
        }

        if dual_stereo && i == intensity {
            // Switch off dual stereo to do intensity.
            dual_stereo = false;
            let n1 = norm1.as_deref_mut().unwrap();
            for j in 0..m * EBAND5MS[i] as usize - norm_offset {
                norm0[j] = 0.5 * (norm0[j] + n1[j]);
            }
        }

        let out_off = m * EBAND5MS[i] as usize - norm_offset;
        let x_band = &mut x0[m * EBAND5MS[i] as usize..m * EBAND5MS[i] as usize + n];
        let y_band = x1
            .as_deref_mut()
            .map(|x1| &mut x1[m * EBAND5MS[i] as usize..m * EBAND5MS[i] as usize + n]);
        let out = if last { None } else { Some((out_off, n)) };
        let mut scratch_opt = if last { None } else { Some(&mut scratch[..]) };

        if dual_stereo {
            let (lb, out_s, scr) = match (effective_lowband, out, scratch_opt.as_deref_mut()) {
                (Some(eff), Some((o, _)), scratch) if eff + n > o => {
                    // Hybrid folding: the fold source overlaps the output
                    // region. libopus passes two aliased pointers into
                    // `norm_` and relies on the fold reads preceding the
                    // end-of-band lowband_out writes; snapshot the source
                    // so the borrow split preserves that order. The snapshot
                    // then also serves as this band's transform scratch.
                    let sc = scratch.expect("overlap folding needs scratch");
                    sc[..n].copy_from_slice(&norm0[eff..eff + n]);
                    (Some(&mut sc[..]), Some(&mut norm0[o..]), None)
                }
                (Some(eff), Some((o, _)), scratch) => {
                    // Disjoint: the fold source ends before the output
                    // region starts. Both slices are open-ended like the
                    // reference's pointers (the pre/post band transforms
                    // legitimately touch neighbouring bands' norm data).
                    let (l, r) = norm0.split_at_mut(o);
                    (Some(&mut l[eff..]), Some(&mut r[..]), scratch)
                }
                (Some(eff), None, scratch) => (Some(&mut norm0[eff..]), None, scratch),
                (None, Some((o, _)), scratch) => (None, Some(&mut norm0[o..]), scratch),
                (None, None, scratch) => (None, None, scratch),
            };
            x_cm = quant_band(
                &mut ctx,
                dec,
                x_band,
                n,
                b / 2,
                b_blocks0,
                lb,
                lm as i32,
                out_s,
                Q15ONE,
                scr,
                x_cm,
                htmp,
                iy,
            )?;
            let n1 = norm1.as_deref_mut().unwrap();
            let (lb, out_s, scr) = match (effective_lowband, out, scratch_opt.as_deref_mut()) {
                (Some(eff), Some((o, _)), scratch) if eff + n > o => {
                    // Overlap snapshot, second channel (see above).
                    let sc = scratch.expect("overlap folding needs scratch");
                    sc[..n].copy_from_slice(&n1[eff..eff + n]);
                    (Some(&mut sc[..]), Some(&mut n1[o..]), None)
                }
                (Some(eff), Some((o, _)), scratch) => {
                    let (l, r) = n1.split_at_mut(o);
                    (Some(&mut l[eff..]), Some(&mut r[..]), scratch)
                }
                (Some(eff), None, scratch) => (Some(&mut n1[eff..]), None, scratch),
                (None, Some((o, _)), scratch) => (None, Some(&mut n1[o..]), scratch),
                (None, None, scratch) => (None, None, scratch),
            };
            y_cm = quant_band(
                &mut ctx,
                dec,
                y_band.unwrap(),
                n,
                b / 2,
                b_blocks0,
                lb,
                lm as i32,
                out_s,
                Q15ONE,
                scr,
                y_cm,
                htmp,
                iy,
            )?;
        } else if stereo {
            let (lb, out_s, scr) = match (effective_lowband, out, scratch_opt) {
                (Some(eff), Some((o, _)), scratch) if eff + n > o => {
                    // Overlap snapshot (see dual-stereo arm above).
                    let sc = scratch.expect("overlap folding needs scratch");
                    sc[..n].copy_from_slice(&norm0[eff..eff + n]);
                    (Some(&mut sc[..]), Some(&mut norm0[o..]), None)
                }
                (Some(eff), Some((o, _)), scratch) => {
                    let (l, r) = norm0.split_at_mut(o);
                    (Some(&mut l[eff..]), Some(&mut r[..]), scratch)
                }
                (Some(eff), None, scratch) => (Some(&mut norm0[eff..]), None, scratch),
                (None, Some((o, _)), scratch) => (None, Some(&mut norm0[o..]), scratch),
                (None, None, scratch) => (None, None, scratch),
            };
            x_cm = quant_band_stereo(
                &mut ctx,
                dec,
                x_band,
                y_band.unwrap(),
                n,
                b,
                b_blocks0,
                lb,
                lm as i32,
                out_s,
                scr,
                x_cm | y_cm,
                htmp,
                iy,
            )?;
            y_cm = x_cm;
        } else {
            let (lb, out_s, scr) = match (effective_lowband, out, scratch_opt) {
                (Some(eff), Some((o, _)), scratch) if eff + n > o => {
                    // Overlap snapshot (see dual-stereo arm above).
                    let sc = scratch.expect("overlap folding needs scratch");
                    sc[..n].copy_from_slice(&norm0[eff..eff + n]);
                    (Some(&mut sc[..]), Some(&mut norm0[o..]), None)
                }
                (Some(eff), Some((o, _)), scratch) => {
                    let (l, r) = norm0.split_at_mut(o);
                    (Some(&mut l[eff..]), Some(&mut r[..]), scratch)
                }
                (Some(eff), None, scratch) => (Some(&mut norm0[eff..]), None, scratch),
                (None, Some((o, _)), scratch) => (None, Some(&mut norm0[o..]), scratch),
                (None, None, scratch) => (None, None, scratch),
            };
            x_cm = quant_band(
                &mut ctx,
                dec,
                x_band,
                n,
                b,
                b_blocks0,
                lb,
                lm as i32,
                out_s,
                Q15ONE,
                scr,
                x_cm | y_cm,
                htmp,
                iy,
            )?;
            y_cm = x_cm;
        }
        collapse_masks[i * c] = x_cm as u8;
        collapse_masks[i * c + c - 1] = y_cm as u8;
        balance += pulses[i] + tell;

        // Update the folding position only as long as we have 1 bit/sample
        // depth.
        update_lowband = b > (n << BITRES) as i32;
    }
    *seed = ctx.seed;
    Ok(())
}

/// `tf_decode` (celt_decoder.c).
pub(crate) fn tf_decode(
    start: usize,
    end: usize,
    is_transient: bool,
    tf_res: &mut [i32],
    lm: usize,
    dec: &mut RangeDecoder,
    len: usize,
) -> crate::Result<()> {
    let mut budget = (len * 8) as i32;
    let mut tell = dec.tell() as i32;
    let mut logp = if is_transient { 2 } else { 4 };
    let tf_select_rsv = lm > 0 && tell + logp < budget;
    if tf_select_rsv {
        budget -= 1;
    }
    let mut tf_changed = false;
    let mut curr = 0i32;
    for i in start..end {
        if tell + logp <= budget {
            curr ^= i32::from(dec.decode_bit_logp(logp as u32)?);
            tell = dec.tell() as i32;
            tf_changed |= curr != 0;
        }
        tf_res[i] = curr;
        logp = if is_transient { 4 } else { 5 };
    }
    let mut tf_select = 0i32;
    if tf_select_rsv
        && TF_SELECT_TABLE[lm][(if is_transient { 4 } else { 0 }) + tf_changed as i32 as usize]
            != TF_SELECT_TABLE[lm]
                [(if is_transient { 4 } else { 0 }) + 2 + tf_changed as i32 as usize]
    {
        tf_select = i32::from(dec.decode_bit_logp(1)?);
    }
    for i in start..end {
        tf_res[i] = TF_SELECT_TABLE[lm]
            [(4 * i32::from(is_transient) + 2 * tf_select + tf_res[i]) as usize]
            as i32;
    }
    Ok(())
}
