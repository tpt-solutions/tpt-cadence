//! SILK NLSF decoding and LPC conversion (Tier 2).
//!
//! Ports the normalized line spectral frequency decode path from
//! libopus 1.5.2 with bit-exact integer arithmetic:
//!
//! - [`nlsf_residual_dequant`] + [`nlsf_decode`]: predictive stage-2
//!   residual dequantization and stage-1 codebook add
//!   (`silk/NLSF_decode.c`).
//! - [`nlsf_stabilize`]: enforces minimum LSF spacing and border
//!   distance, converging or falling back to an insertion sort
//!   (`silk/NLSF_stabilize.c`).
//! - [`nlsf_interpolate`]: the inter-frame interpolation formula from
//!   `silk/decode_parameters.c` (NLSF0 for the leading subframes when
//!   `NLSFInterpCoef_Q2 < 4`).
//! - [`nlsf2a`]: NLSF → LPC via the piecewise-linear cos table and
//!   even/odd polynomial convolution, with the Q12 fit and the
//!   bandwidth-expansion stability loop (`silk/NLSF2A.c`,
//!   `silk/LPC_fit.c`, `silk/LPC_inv_pred_gain.c`).
//! - [`bwexpander`] / [`bwexpander_32`]: LPC bandwidth expansion
//!   (`silk/bwexpander.c`, `silk/bwexpander_32.c`).
//!
//! SOURCE: Xiph.Org libopus 1.5.2, `silk/NLSF_decode.c`,
//! `silk/NLSF_stabilize.c`, `silk/NLSF2A.c`, `silk/LPC_fit.c`,
//! `silk/LPC_inv_pred_gain.c`, `silk/bwexpander.c`,
//! `silk/bwexpander_32.c`, `silk/Inlines.h` (BSD-3-Clause).

// Unused until the Tier 3/4 synthesis and decoder assembly consume this
// module — same convention as `tables`/`sigproc`.
#![allow(dead_code)]

use super::decode_indices::{nlsf_unpack, MAX_LPC_ORDER};
use super::sigproc::{
    add_lshift32, add_sat16, div32, div32_16, insertion_sort_increasing_all_values_int16, mul,
    rshift_round, rshift_round64, smlawb, smlaww, smmul, smulbb, smulwb, smulww, sub_sat32,
};
use super::tables::{NlsfCbStruct, LSF_COS_TAB_FIX_Q12};

/// `MAX_LOOPS` (`silk/NLSF_stabilize.c`).
const MAX_LOOPS: u32 = 20;

/// `MAX_LPC_STABILIZE_ITERATIONS` (`silk/define.h`).
const MAX_LPC_STABILIZE_ITERATIONS: u32 = 16;

/// `SILK_FIX_CONST(NLSF_QUANT_LEVEL_ADJ, 10)` with
/// `NLSF_QUANT_LEVEL_ADJ = 0.1` (`silk/define.h`).
const NLSF_QUANT_LEVEL_ADJ_Q10: i32 = 102;

/// `SILK_FIX_CONST(0.999, 16)` (`silk/LPC_fit.c` chirp bias).
const SILK_FIX_0999_Q16: i32 = 65470;

/// `SILK_FIX_CONST(0.99975, 24)` — `A_LIMIT` in `silk/LPC_inv_pred_gain.c`.
const A_LIMIT_QA24: i32 = 16_773_022;

/// `SILK_FIX_CONST(1.0f / MAX_PREDICTION_POWER_GAIN, 30)` with
/// `MAX_PREDICTION_POWER_GAIN = 1e4f` (`silk/define.h`).
const MIN_INVERSE_PRED_GAIN_Q30: i32 = 107_374;

/// `#define QA` inside `silk/LPC_inv_pred_gain.c`.
const INVERSE_PRED_GAIN_QA: u32 = 24;

/// `#define QA` inside `silk/NLSF2A.c`.
const NLSF2A_QA: u32 = 16;

/// Coefficient reordering found to maximize the numerical accuracy of
/// [`nlsf2a_find_poly`] (`silk/NLSF2A.c`).
const NLSF2A_ORDERING16: [u8; 16] = [0, 15, 8, 7, 4, 11, 12, 3, 2, 13, 10, 5, 6, 9, 14, 1];
const NLSF2A_ORDERING10: [u8; 10] = [0, 9, 6, 3, 4, 5, 8, 1, 2, 7];

/// `MUL32_FRAC_Q(a, b, Q)` (`silk/LPC_inv_pred_gain.c`).
fn mul32_frac_q(a: i32, b: i32, q: u32) -> i32 {
    rshift_round64(a as i64 * b as i64, q) as i32
}

/// `silk_INVERSE32_varQ` (`silk/Inlines.h`): approximation of
/// `(1 << Qres) / b32`. Requires `b32 != 0` and `Qres > 0`; all
/// intermediate wrapping mirrors the reference's unchecked/`_ovflw`
/// arithmetic.
pub(crate) fn inverse32_varq(b32: i32, qres: u32) -> i32 {
    let b_headrm = b32.abs().leading_zeros() - 1;
    let b32_nrm = b32.wrapping_shl(b_headrm);
    // Inverse of b32, with 14 bits of precision
    let b32_inv = div32_16(i32::MAX >> 2, b32_nrm >> 16);
    // First approximation
    let mut result = b32_inv.wrapping_shl(16);
    // Compute residual by subtracting product of denominator and first
    // approximation from one
    let err_q32 = (1_i32 << 29)
        .wrapping_sub(smulwb(b32_nrm, b32_inv))
        .wrapping_shl(3);
    // Refinement
    result = smlaww(result, err_q32, b32_inv);
    // Convert to Qres domain
    let lshift = 61 - b_headrm as i32 - qres as i32;
    if lshift <= 0 {
        // silk_LSHIFT_SAT32: the `_ovflw` shift already wraps, so the
        // outer SAT32 can never trigger and is omitted
        result.wrapping_shl(-lshift as u32)
    } else if lshift < 32 {
        result >> lshift
    } else {
        // Avoid undefined result
        0
    }
}

/// `silk_NLSF_residual_dequant` (`silk/NLSF_decode.c`): predictive
/// dequantizer for the stage-2 NLSF residuals. `indices` holds `order`
/// quantization indices; `x_q10` receives the dequantized residuals in
/// Q10.
fn nlsf_residual_dequant(
    x_q10: &mut [i16],
    indices: &[i8],
    pred_coef_q8: &[u8],
    quant_step_size_q16: i32,
) {
    let order = x_q10.len();
    debug_assert_eq!(indices.len(), order);
    debug_assert_eq!(pred_coef_q8.len(), order);
    let mut out_q10: i32 = 0;
    for i in (0..order).rev() {
        let pred_q10 = smulbb(out_q10, pred_coef_q8[i] as i32) >> 8;
        out_q10 = (indices[i] as i32) << 10;
        if out_q10 > 0 {
            out_q10 -= NLSF_QUANT_LEVEL_ADJ_Q10;
        } else if out_q10 < 0 {
            out_q10 += NLSF_QUANT_LEVEL_ADJ_Q10;
        }
        out_q10 = smlawb(pred_q10, out_q10, quant_step_size_q16);
        x_q10[i] = out_q10 as i16;
    }
}

/// `silk_NLSF_decode` (`silk/NLSF_decode.c`): reconstructs the
/// quantized NLSF vector (Q15) from the codebook path `nlsf_indices`
/// (`[LPC_order + 1]`: stage-1 index followed by the stage-2
/// residuals) and stabilizes it.
pub(crate) fn nlsf_decode(p_nlsf_q15: &mut [i16], nlsf_indices: &[i8], cb: &NlsfCbStruct) {
    let order = cb.order as usize;
    debug_assert_eq!(p_nlsf_q15.len(), order);
    debug_assert_eq!(nlsf_indices.len(), order + 1);
    debug_assert!((0..cb.n_vectors as i32).contains(&(nlsf_indices[0] as i32)));

    // Unpack entropy table indices and predictor for current CB1 index
    let (_, pred_q8) = nlsf_unpack(cb, nlsf_indices[0] as usize);

    // Predictive residual dequantizer
    let mut res_q10 = [0i16; MAX_LPC_ORDER];
    nlsf_residual_dequant(
        &mut res_q10[..order],
        &nlsf_indices[1..=order],
        &pred_q8[..order],
        cb.quant_step_size_q16 as i32,
    );

    // Apply inverse square-rooted weights to first stage and add to output
    let cb1_index = nlsf_indices[0] as usize;
    let cb1_element = &cb.cb1_nlsf_q8[cb1_index * order..][..order];
    let cb1_wght_q9 = &cb.cb1_wght_q9[cb1_index * order..][..order];
    for i in 0..order {
        let nlsf_q15_tmp = add_lshift32(
            div32_16((res_q10[i] as i32) << 14, cb1_wght_q9[i] as i32),
            cb1_element[i] as i32,
            7,
        );
        p_nlsf_q15[i] = nlsf_q15_tmp.clamp(0, 32767) as i16;
    }

    // NLSF stabilization
    nlsf_stabilize(p_nlsf_q15, cb.delta_min_q15);
}

/// `silk_NLSF_stabilize` (`silk/NLSF_stabilize.c`): moves NLSFs apart
/// that are too close to each other or to the 0/1 borders, with
/// minimum Euclidean distance. `n_delta_min_q15` has `len + 1` entries.
pub(crate) fn nlsf_stabilize(nlsf_q15: &mut [i16], n_delta_min_q15: &[i16]) {
    let l = nlsf_q15.len();
    debug_assert_eq!(n_delta_min_q15.len(), l + 1);
    // This is necessary to ensure an output within range of a opus_int16
    debug_assert!(n_delta_min_q15[l] >= 1);

    for _loops in 0..MAX_LOOPS {
        // Find smallest distance
        // First element
        let mut min_diff_q15 = nlsf_q15[0] as i32 - n_delta_min_q15[0] as i32;
        let mut i_min: usize = 0;
        // Middle elements
        for i in 1..l {
            let diff_q15 =
                nlsf_q15[i] as i32 - (nlsf_q15[i - 1] as i32 + n_delta_min_q15[i] as i32);
            if diff_q15 < min_diff_q15 {
                min_diff_q15 = diff_q15;
                i_min = i;
            }
        }
        // Last element
        let diff_q15 = (1 << 15) - (nlsf_q15[l - 1] as i32 + n_delta_min_q15[l] as i32);
        if diff_q15 < min_diff_q15 {
            min_diff_q15 = diff_q15;
            i_min = l;
        }

        // Now check if the smallest distance is non-negative
        if min_diff_q15 >= 0 {
            return;
        }

        if i_min == 0 {
            // Move away from lower limit
            nlsf_q15[0] = n_delta_min_q15[0];
        } else if i_min == l {
            // Move away from higher limit
            nlsf_q15[l - 1] = ((1 << 15) - n_delta_min_q15[l] as i32) as i16;
        } else {
            // Find the lower extreme for the location of the current
            // center frequency
            let mut min_center_q15: i32 = 0;
            for &delta in &n_delta_min_q15[..i_min] {
                min_center_q15 += delta as i32;
            }
            min_center_q15 += (n_delta_min_q15[i_min] >> 1) as i32;

            // Find the upper extreme for the location of the current
            // center frequency
            let mut max_center_q15: i32 = 1 << 15;
            for &delta in n_delta_min_q15[i_min + 1..=l].iter().rev() {
                max_center_q15 -= delta as i32;
            }
            max_center_q15 -= (n_delta_min_q15[i_min] >> 1) as i32;

            // Move apart, sorted by value, keeping the same center frequency
            let center_freq_q15 =
                rshift_round(nlsf_q15[i_min - 1] as i32 + nlsf_q15[i_min] as i32, 1)
                    .clamp(min_center_q15, max_center_q15) as i16;
            nlsf_q15[i_min - 1] =
                (center_freq_q15 as i32 - (n_delta_min_q15[i_min] >> 1) as i32) as i16;
            nlsf_q15[i_min] = (nlsf_q15[i_min - 1] as i32 + n_delta_min_q15[i_min] as i32) as i16;
        }
    }

    // Safe and simple fall back method, which is less ideal than the
    // above (runs when the loop above completed MAX_LOOPS iterations)
    // Insertion sort (fast for already almost sorted arrays)
    insertion_sort_increasing_all_values_int16(nlsf_q15);

    // First NLSF should be no less than NDeltaMin[0]
    nlsf_q15[0] = nlsf_q15[0].max(n_delta_min_q15[0]);

    // Keep delta_min distance between the NLSFs
    for i in 1..l {
        nlsf_q15[i] = nlsf_q15[i].max(add_sat16(nlsf_q15[i - 1] as i32, n_delta_min_q15[i] as i32));
    }

    // Last NLSF should be no higher than 1 - NDeltaMin[L]
    nlsf_q15[l - 1] = (nlsf_q15[l - 1] as i32).min((1 << 15) - n_delta_min_q15[l] as i32) as i16;

    // Keep NDeltaMin distance between the NLSFs
    for i in (0..l - 1).rev() {
        nlsf_q15[i] =
            (nlsf_q15[i] as i32).min(nlsf_q15[i + 1] as i32 - n_delta_min_q15[i + 1] as i32) as i16;
    }
}

/// NLSF interframe interpolation (the `NLSFInterpCoef_Q2 < 4` branch of
/// `silk_decode_parameters` in `silk/decode_parameters.c`):
/// `dst[i] = prev[i] + (coef * (cur[i] - prev[i])) >> 2`.
pub(crate) fn nlsf_interpolate(
    dst: &mut [i16],
    prev_nlsf_q15: &[i16],
    cur_nlsf_q15: &[i16],
    coef_q2: i32,
) {
    debug_assert!((0..=4).contains(&coef_q2));
    debug_assert_eq!(dst.len(), prev_nlsf_q15.len());
    debug_assert_eq!(dst.len(), cur_nlsf_q15.len());
    for i in 0..dst.len() {
        dst[i] = (prev_nlsf_q15[i] as i32
            + (mul(coef_q2, cur_nlsf_q15[i] as i32 - prev_nlsf_q15[i] as i32) >> 2))
            as i16;
    }
}

/// `silk_NLSF2A_find_poly` (`silk/NLSF2A.c`): builds one intermediate
/// polynomial (QA coefficients, `dd + 1` entries) from the interleaved
/// `2*cos(LSF)` vector starting at `clsf_off`.
fn nlsf2a_find_poly(
    out: &mut [i32],
    cos_lsf_qa: &[i32; MAX_LPC_ORDER],
    clsf_off: usize,
    dd: usize,
) {
    out[0] = 1 << NLSF2A_QA;
    out[1] = -cos_lsf_qa[clsf_off];
    for k in 1..dd {
        let ftmp = cos_lsf_qa[clsf_off + 2 * k]; /* QA */
        out[k + 1] = (out[k - 1] << 1)
            .wrapping_sub(rshift_round64(ftmp as i64 * out[k] as i64, NLSF2A_QA) as i32);
        for n in (2..=k).rev() {
            out[n] = out[n]
                .wrapping_add(out[n - 2])
                .wrapping_sub(rshift_round64(ftmp as i64 * out[n - 1] as i64, NLSF2A_QA) as i32);
        }
        out[1] = out[1].wrapping_sub(ftmp);
    }
}

/// `silk_NLSF2A` (`silk/NLSF2A.c`): computes whitening filter
/// coefficients (Q12, monic without the leading 1) from normalized line
/// spectral frequencies (Q15). `d` must be 10 or 16.
pub(crate) fn nlsf2a(a_q12: &mut [i16], nlsf: &[i16], d: usize) {
    debug_assert!(d == 10 || d == 16);
    debug_assert_eq!(a_q12.len(), d);
    debug_assert_eq!(nlsf.len(), d);

    // convert LSFs to 2*cos(LSF), using piecewise linear curve from table
    let ordering: &[u8] = if d == 16 {
        &NLSF2A_ORDERING16
    } else {
        &NLSF2A_ORDERING10
    };
    let mut cos_lsf_qa = [0i32; MAX_LPC_ORDER];
    for k in 0..d {
        debug_assert!(nlsf[k] >= 0);

        // f_int on a scale 0-127 (rounded down)
        let f_int = (nlsf[k] >> (15 - 7)) as usize;

        // f_frac, range: 0..255
        let f_frac = nlsf[k] as i32 - ((f_int as i32) << (15 - 7));

        debug_assert!(f_int < LSF_COS_TAB_FIX_Q12.len() - 1);

        // Read start and end value from table
        let cos_val = LSF_COS_TAB_FIX_Q12[f_int] as i32; /* Q12 */
        let delta = LSF_COS_TAB_FIX_Q12[f_int + 1] as i32 - cos_val; /* Q12, range 0..200 */

        // Linear interpolation
        cos_lsf_qa[ordering[k] as usize] = rshift_round(
            (cos_val << 8).wrapping_add(mul(delta, f_frac)),
            20 - NLSF2A_QA,
        ); /* QA */
    }

    let dd = d >> 1;

    // generate even and odd polynomials using convolution
    let mut p_poly = [0i32; MAX_LPC_ORDER / 2 + 1];
    let mut q_poly = [0i32; MAX_LPC_ORDER / 2 + 1];
    nlsf2a_find_poly(&mut p_poly, &cos_lsf_qa, 0, dd);
    nlsf2a_find_poly(&mut q_poly, &cos_lsf_qa, 1, dd);

    // convert even and odd polynomials to opus_int32 Q12 filter coefs
    let mut a32_qa1 = [0i32; MAX_LPC_ORDER];
    for k in 0..dd {
        let ptmp = p_poly[k + 1].wrapping_add(p_poly[k]);
        let qtmp = q_poly[k + 1].wrapping_sub(q_poly[k]);

        // the Ptmp and Qtmp values at this stage need to fit in int32
        a32_qa1[k] = qtmp.wrapping_neg().wrapping_sub(ptmp); /* QA+1 */
        a32_qa1[d - k - 1] = qtmp.wrapping_sub(ptmp); /* QA+1 */
    }

    // Convert int32 coefficients to Q12 int16 coefs
    lpc_fit(a_q12, &mut a32_qa1[..d], 12, NLSF2A_QA + 1, d);

    let mut i = 0;
    while lpc_inverse_pred_gain(a_q12, d) == 0 && i < MAX_LPC_STABILIZE_ITERATIONS {
        // Prediction coefficients are (too close to) unstable; apply
        // bandwidth expansion on the unscaled coefficients, convert to
        // Q12 and measure again
        bwexpander_32(&mut a32_qa1[..d], 65536 - (2 << i));
        for (a, &a32) in a_q12[..d].iter_mut().zip(&a32_qa1[..d]) {
            *a = rshift_round(a32, NLSF2A_QA + 1 - 12) as i16; /* QA+1 -> Q12 */
        }
        i += 1;
    }
}

/// `silk_LPC_fit` (`silk/LPC_fit.c`): converts `a_qin` (Q`qin`) to
/// `a_qout` (Q`qout`), bandwidth-expanding (and finally clipping) so
/// the Q`qout` coefficients never wrap around.
pub(crate) fn lpc_fit(a_qout: &mut [i16], a_qin: &mut [i32], qout: u32, qin: u32, d: usize) {
    debug_assert_eq!(a_qout.len(), d);
    debug_assert!(a_qin.len() >= d);
    let qshift = qin - qout;
    let mut idx: usize = 0;
    let mut iterations = 0usize;

    // Limit the maximum absolute value of the prediction coefficients,
    // so that they'll fit in int16
    while iterations < 10 {
        // Find maximum absolute value and its index
        let mut maxabs: i32 = 0;
        for (k, &a) in a_qin[..d].iter().enumerate() {
            let absval = a.abs();
            if absval > maxabs {
                maxabs = absval;
                idx = k;
            }
        }
        maxabs = rshift_round(maxabs, qshift);

        if maxabs > i16::MAX as i32 {
            // Reduce magnitude of prediction coefficients
            // ( silk_int32_MAX >> 14 ) + silk_int16_MAX = 163838
            maxabs = maxabs.min(163838);
            let chirp_q16 = SILK_FIX_0999_Q16.wrapping_sub(div32(
                (maxabs - i16::MAX as i32) << 14,
                mul(maxabs, idx as i32 + 1) >> 2,
            ));
            bwexpander_32(&mut a_qin[..d], chirp_q16);
        } else {
            break;
        }
        iterations += 1;
    }

    if iterations == 10 {
        // Reached the last iteration, clip the coefficients
        for k in 0..d {
            a_qout[k] =
                rshift_round(a_qin[k], qshift).clamp(i16::MIN as i32, i16::MAX as i32) as i16;
            a_qin[k] = (a_qout[k] as i32) << qshift;
        }
    } else {
        for (a, &a_in) in a_qout[..d].iter_mut().zip(&a_qin[..d]) {
            *a = rshift_round(a_in, qshift) as i16;
        }
    }
}

/// `silk_LPC_inverse_pred_gain_QA_c` (`silk/LPC_inv_pred_gain.c`):
/// inverse prediction gain in the energy domain, Q30 — 0 signals an
/// unstable (or too-close-to-unstable) filter. Operates on QA
/// coefficients, consuming `a_qa` as scratch.
fn lpc_inverse_pred_gain_qa(a_qa: &mut [i32], order: usize) -> i32 {
    let mut inv_gain_q30: i32 = 1 << 30;
    for k in (1..order).rev() {
        if a_qa[k] > A_LIMIT_QA24 || a_qa[k] < -A_LIMIT_QA24 {
            return 0;
        }

        let rc_q31 = -(a_qa[k] << (31 - INVERSE_PRED_GAIN_QA));

        let rc_mult1_q30 = (1_i32 << 30).wrapping_sub(smmul(rc_q31, rc_q31));

        inv_gain_q30 = smmul(inv_gain_q30, rc_mult1_q30).wrapping_shl(2);
        if inv_gain_q30 < MIN_INVERSE_PRED_GAIN_Q30 {
            return 0;
        }

        let mult2q = 32 - rc_mult1_q30.abs().leading_zeros();
        let rc_mult2 = inverse32_varq(rc_mult1_q30, mult2q + 30);

        for n in 0..((k + 1) >> 1) {
            let tmp1 = a_qa[n];
            let tmp2 = a_qa[k - n - 1];
            let tmp64 = rshift_round64(
                sub_sat32(tmp1, mul32_frac_q(tmp2, rc_q31, 31)) as i64 * rc_mult2 as i64,
                mult2q,
            );
            if tmp64 > i32::MAX as i64 || tmp64 < i32::MIN as i64 {
                return 0;
            }
            a_qa[n] = tmp64 as i32;
            let tmp64 = rshift_round64(
                sub_sat32(tmp2, mul32_frac_q(tmp1, rc_q31, 31)) as i64 * rc_mult2 as i64,
                mult2q,
            );
            if tmp64 > i32::MAX as i64 || tmp64 < i32::MIN as i64 {
                return 0;
            }
            a_qa[k - n - 1] = tmp64 as i32;
        }
    }

    if a_qa[0] > A_LIMIT_QA24 || a_qa[0] < -A_LIMIT_QA24 {
        return 0;
    }

    let rc_q31 = -(a_qa[0] << (31 - INVERSE_PRED_GAIN_QA));

    let rc_mult1_q30 = (1_i32 << 30).wrapping_sub(smmul(rc_q31, rc_q31));

    inv_gain_q30 = smmul(inv_gain_q30, rc_mult1_q30).wrapping_shl(2);
    if inv_gain_q30 < MIN_INVERSE_PRED_GAIN_Q30 {
        return 0;
    }

    inv_gain_q30
}

/// `silk_LPC_inverse_pred_gain_c` (`silk/LPC_inv_pred_gain.c`):
/// computes the inverse prediction gain of the Q12 filter `a_q12`,
/// returning 0 for filters that are (nearly) unstable.
pub(crate) fn lpc_inverse_pred_gain(a_q12: &[i16], order: usize) -> i32 {
    debug_assert!(order <= MAX_LPC_ORDER);
    let mut dc_resp: i32 = 0;
    let mut atmp_qa = [0i32; MAX_LPC_ORDER];
    for (&a, atmp) in a_q12[..order].iter().zip(&mut atmp_qa) {
        dc_resp = dc_resp.wrapping_add(a as i32);
        *atmp = (a as i32) << (INVERSE_PRED_GAIN_QA - 12);
    }
    if dc_resp >= 4096 {
        return 0;
    }
    lpc_inverse_pred_gain_qa(&mut atmp_qa[..order], order)
}

/// `silk_bwexpander` (`silk/bwexpander.c`): chirp (bandwidth expand) an
/// LP AR filter in place. Deliberately uses rounded products instead of
/// `silk_SMULWB`, whose bias can lead to unstable filters.
pub(crate) fn bwexpander(ar: &mut [i16], chirp_q16: i32) {
    let d = ar.len();
    debug_assert!(d >= 1);
    let mut chirp_q16 = chirp_q16;
    let chirp_minus_one_q16 = chirp_q16 - 65536;

    for a in ar.iter_mut().take(d - 1) {
        *a = rshift_round(mul(chirp_q16, *a as i32), 16) as i16;
        chirp_q16 = chirp_q16.wrapping_add(rshift_round(mul(chirp_q16, chirp_minus_one_q16), 16));
    }
    ar[d - 1] = rshift_round(mul(chirp_q16, ar[d - 1] as i32), 16) as i16;
}

/// `silk_bwexpander_32` (`silk/bwexpander_32.c`): 32-input variant of
/// [`bwexpander`].
pub(crate) fn bwexpander_32(ar: &mut [i32], chirp_q16: i32) {
    let d = ar.len();
    debug_assert!(d >= 1);
    let mut chirp_q16 = chirp_q16;
    let chirp_minus_one_q16 = chirp_q16 - 65536;

    for a in ar.iter_mut().take(d - 1) {
        *a = smulww(chirp_q16, *a);
        chirp_q16 = chirp_q16.wrapping_add(rshift_round(mul(chirp_q16, chirp_minus_one_q16), 16));
    }
    ar[d - 1] = smulww(chirp_q16, ar[d - 1]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::silk::tables::{NLSF_CB_NB_MB, NLSF_CB_WB};

    /// Golden vectors produced by an independent Python transcription of
    /// the libopus 1.5.2 C sources (tables parsed from the C files, not
    /// from this crate's Rust tables). Each case runs
    /// `nlsf_decode` -> `nlsf2a` -> `bwexpander(63570)`
    /// (`BWE_AFTER_LOSS_Q16`).
    struct NlsfOracleCase {
        wb: bool,
        cb1: i8,
        residuals: [i8; 16],
        order: usize,
        nlsf_q15: [i16; 16],
        a_q12: [i16; 16],
        a_bwe: [i16; 16],
    }

    static NLSF_ORACLE_CASES: [NlsfOracleCase; 10] = [
        NlsfOracleCase {
            wb: false,
            cb1: 0,
            order: 10,
            residuals: [-2, 0, -3, 2, 2, 2, 0, -1, -1, -1, 0, 0, 0, 0, 0, 0],
            nlsf_q15: [
                250, 4126, 6335, 15098, 17320, 18579, 18709, 20711, 24453, 27995, 0, 0, 0, 0, 0, 0,
            ],
            a_q12: [
                1427, -2058, 5354, 678, 350, -2601, -1268, 69, 1005, 1128, 0, 0, 0, 0, 0, 0,
            ],
            a_bwe: [
                1384, -1936, 4886, 600, 301, -2167, -1025, 54, 764, 832, 0, 0, 0, 0, 0, 0,
            ],
        },
        NlsfOracleCase {
            wb: false,
            cb1: 1,
            order: 10,
            residuals: [3, -3, 2, 2, 3, -1, -3, -3, 3, 2, 0, 0, 0, 0, 0, 0],
            nlsf_q15: [
                3607, 3610, 11242, 13206, 13454, 13457, 15085, 21752, 31174, 31413, 0, 0, 0, 0, 0,
                0,
            ],
            a_q12: [
                3527, -1788, -4123, 7574, -3923, -850, 2539, -3472, 324, -845, 0, 0, 0, 0, 0, 0,
            ],
            a_bwe: [
                3421, -1682, -3763, 6705, -3369, -708, 2052, -2721, 246, -623, 0, 0, 0, 0, 0, 0,
            ],
        },
        NlsfOracleCase {
            wb: false,
            cb1: 7,
            order: 10,
            residuals: [-1, 2, -3, -2, 0, 3, -1, 0, 3, -4, 0, 0, 0, 0, 0, 0],
            nlsf_q15: [
                2308, 3724, 3730, 9346, 14613, 18105, 18109, 21805, 23279, 23282, 0, 0, 0, 0, 0, 0,
            ],
            a_q12: [
                6511, -6481, 10452, -10972, 8400, -8931, 6658, -3221, 1300, 66, 0, 0, 0, 0, 0, 0,
            ],
            a_bwe: [
                6316, -6098, 9539, -9714, 7213, -7439, 5380, -2524, 988, 49, 0, 0, 0, 0, 0, 0,
            ],
        },
        NlsfOracleCase {
            wb: false,
            cb1: 15,
            order: 10,
            residuals: [3, 3, -3, 1, 4, 2, 0, -2, 4, 3, 0, 0, 0, 0, 0, 0],
            nlsf_q15: [
                5477, 5480, 5486, 10855, 17600, 17620, 18497, 21622, 32304, 32307, 0, 0, 0, 0, 0, 0,
            ],
            a_q12: [
                761, 878, -741, 2661, -5210, -811, 570, -451, 1504, -1468, 0, 0, 0, 0, 0, 0,
            ],
            a_bwe: [
                738, 826, -676, 2356, -4474, -676, 461, -353, 1143, -1083, 0, 0, 0, 0, 0, 0,
            ],
        },
        NlsfOracleCase {
            wb: false,
            cb1: 31,
            order: 10,
            residuals: [-2, -3, 2, 4, -1, -3, 1, 3, 0, 3, 0, 0, 0, 0, 0, 0],
            nlsf_q15: [
                2038, 4900, 11491, 12671, 12674, 12677, 22965, 26762, 27136, 32307, 0, 0, 0, 0, 0,
                0,
            ],
            a_q12: [
                344, 2131, -4162, 809, 5903, -1511, -2007, 3013, -1362, -1705, 0, 0, 0, 0, 0, 0,
            ],
            a_bwe: [
                334, 2005, -3799, 716, 5069, -1259, -1622, 2361, -1035, -1257, 0, 0, 0, 0, 0, 0,
            ],
        },
        NlsfOracleCase {
            wb: true,
            cb1: 0,
            order: 16,
            residuals: [0, -1, 0, 0, -3, 1, -3, 1, 0, -2, 4, 2, -2, -2, -2, -2],
            nlsf_q15: [
                100, 1432, 3604, 4902, 5914, 10281, 10773, 15548, 16662, 18665, 21504, 21507,
                21515, 23063, 25917, 28911,
            ],
            a_q12: [
                7498, -6912, 10484, -12889, 10173, -8112, 6500, -4191, 1857, -1358, 1903, 505,
                -840, -1142, -358, 977,
            ],
            a_bwe: [
                7273, -6504, 9568, -11411, 8736, -6757, 5252, -3285, 1412, -1001, 1361, 350, -565,
                -746, -227, 600,
            ],
        },
        NlsfOracleCase {
            wb: true,
            cb1: 1,
            order: 16,
            residuals: [-2, 0, 2, -4, 4, -1, -1, 4, -3, 0, 1, 0, 0, -4, -2, -1],
            nlsf_q15: [
                447, 3614, 5853, 5856, 10768, 10771, 12674, 14397, 14411, 17275, 18796, 19176,
                20011, 20838, 25910, 29373,
            ],
            a_q12: [
                9467, -17304, 23823, -25765, 22720, -14461, 5498, 913, -2357, 714, 3199, -4823,
                5804, -4282, 1834, -1001,
            ],
            a_bwe: [
                9183, -16281, 21743, -22810, 19511, -12046, 4442, 716, -1792, 527, 2288, -3346,
                3906, -2795, 1161, -615,
            ],
        },
        NlsfOracleCase {
            wb: true,
            cb1: 7,
            order: 16,
            residuals: [-2, 4, 2, 3, 2, -2, -3, -1, -1, 3, -1, -2, -3, 3, -4, -4],
            nlsf_q15: [
                2097, 4174, 6790, 7660, 7663, 7666, 8528, 12829, 14741, 16503, 16514, 16839, 19900,
                23416, 23423, 26531,
            ],
            a_q12: [
                12382, -22252, 29569, -32697, 30970, -26290, 20355, -13953, 7961, -3376, 613, 522,
                -622, 375, -153, 35,
            ],
            a_bwe: [
                12011, -20937, 26987, -28947, 26595, -21899, 16447, -10936, 6052, -2490, 438, 362,
                -419, 245, -97, 21,
            ],
        },
        NlsfOracleCase {
            wb: true,
            cb1: 15,
            order: 16,
            residuals: [-1, 0, 2, -3, 1, -2, 4, 2, -3, 0, 1, 1, -1, -1, 1, -2],
            nlsf_q15: [
                893, 2519, 3867, 3870, 8439, 9819, 13684, 13698, 13712, 17966, 20333, 21643, 22720,
                25216, 28019, 28632,
            ],
            a_q12: [
                7085, -5653, 5488, -5056, 4879, -4576, 2769, -3729, 5585, -5633, 4398, -3275, 3261,
                -1455, 440, -619,
            ],
            a_bwe: [
                6872, -5319, 5009, -4476, 4190, -3812, 2237, -2923, 4246, -4154, 3146, -2272, 2195,
                -950, 279, -380,
            ],
        },
        NlsfOracleCase {
            wb: true,
            cb1: 31,
            order: 16,
            residuals: [-3, -4, 2, 1, 0, 2, 1, -1, 0, -2, 4, 0, 0, 2, -1, 4],
            nlsf_q15: [
                100, 1086, 6483, 7937, 8959, 11076, 11545, 11978, 14772, 16174, 19789, 19792,
                25006, 28334, 29720, 32421,
            ],
            a_q12: [
                5974, -3691, -2826, 7843, -6791, 3714, 2703, -6007, 6722, -1679, -3940, 6015,
                -3895, -806, 2186, -1438,
            ],
            a_bwe: [
                5795, -3473, -2579, 6943, -5832, 3094, 2184, -4708, 5110, -1238, -2818, 4173,
                -2621, -526, 1384, -883,
            ],
        },
    ];

    #[test]
    fn nlsf_decode_matches_reference_oracle() {
        for case in &NLSF_ORACLE_CASES {
            let cb = if case.wb { &NLSF_CB_WB } else { &NLSF_CB_NB_MB };
            let mut indices = [0i8; MAX_LPC_ORDER + 1];
            indices[0] = case.cb1;
            indices[1..=case.order].copy_from_slice(&case.residuals[..case.order]);
            let mut nlsf_q15 = [0i16; MAX_LPC_ORDER];
            nlsf_decode(&mut nlsf_q15[..case.order], &indices[..=case.order], cb);
            assert_eq!(
                &nlsf_q15[..case.order],
                &case.nlsf_q15[..case.order],
                "nlsf_decode mismatch (wb={}, cb1={})",
                case.wb,
                case.cb1
            );
        }
    }

    #[test]
    fn nlsf2a_matches_reference_oracle() {
        for case in &NLSF_ORACLE_CASES {
            let mut a_q12 = [0i16; MAX_LPC_ORDER];
            nlsf2a(
                &mut a_q12[..case.order],
                &case.nlsf_q15[..case.order],
                case.order,
            );
            assert_eq!(
                &a_q12[..case.order],
                &case.a_q12[..case.order],
                "nlsf2a mismatch (wb={}, cb1={})",
                case.wb,
                case.cb1
            );
            // All decoded filters end up stable (the stability loop in
            // nlsf2a succeeded within MAX_LPC_STABILIZE_ITERATIONS).
            assert_ne!(lpc_inverse_pred_gain(&a_q12[..case.order], case.order), 0);
        }
    }

    #[test]
    fn bwexpander_matches_reference_oracle() {
        for case in &NLSF_ORACLE_CASES {
            let mut a_bwe = case.a_q12;
            bwexpander(&mut a_bwe[..case.order], 63570);
            assert_eq!(
                &a_bwe[..case.order],
                &case.a_bwe[..case.order],
                "bwexpander mismatch (wb={}, cb1={})",
                case.wb,
                case.cb1
            );
        }
    }

    /// (codebook tag, unstable input, stabilized output, order) — the
    /// full-range cases exhaust all 20 repair loops and finish through
    /// the insertion-sort fallback; the "-light" cases converge inside
    /// the move-apart repair loop.
    static STABILIZE_ORACLE_CASES: [(&str, [i16; 16], [i16; 16], usize); 38] = [
        (
            "NB_MB",
            [
                -12804, -21371, 10970, -9461, -12824, -29695, 6822, 7399, -8044, -6339, 0, 0, 0, 0,
                0, 0,
            ],
            [
                250, 253, 390, 393, 631, 634, 1718, 1917, 1920, 2118, 0, 0, 0, 0, 0, 0,
            ],
            10,
        ),
        (
            "NB_MB",
            [
                15154, -24445, 29184, -17863, -10114, -15905, -20948, -16139, -374, -25349, 0, 0,
                0, 0, 0, 0,
            ],
            [
                250, 253, 278, 281, 530, 861, 865, 1405, 1408, 1949, 0, 0, 0, 0, 0, 0,
            ],
            10,
        ),
        (
            "NB_MB",
            [
                -32488, 30577, 31062, 3543, -26940, 6573, -11038, -20365, 23344, -23639, 0, 0, 0,
                0, 0, 0,
            ],
            [
                250, 275, 281, 284, 3628, 7205, 10535, 10538, 13867, 16950, 0, 0, 0, 0, 0, 0,
            ],
            10,
        ),
        (
            "NB_MB",
            [
                23854, -7985, -1444, -11931, 7738, -17685, 24648, 7905, -7162, 6855, 0, 0, 0, 0, 0,
                0,
            ],
            [
                1107, 1110, 2903, 2906, 3857, 5054, 6855, 8223, 8226, 8952, 0, 0, 0, 0, 0, 0,
            ],
            10,
        ),
        (
            "WB",
            [
                14580, -14307, 14994, -25501, -4000, -14055, 28126, -25665, -19828, -8747, -30230,
                13531, 27512, -32175, 23222, 17335,
            ],
            [
                136, 139, 179, 182, 185, 188, 206, 329, 439, 453, 558, 4599, 4686, 4695, 20278,
                20281,
            ],
            16,
        ),
        (
            "WB",
            [
                -14556, -3443, 27714, 25683, 12688, 10889, 2702, -3409, -10564, -6587, 16794, 2763,
                -23896, -24127, -8858, -30553,
            ],
            [
                100, 103, 185, 195, 226, 233, 238, 4277, 4291, 4357, 4368, 8501, 11227, 15236,
                19340, 19343,
            ],
            16,
        ),
        (
            "WB",
            [
                16724, -26371, 27122, 2115, 7872, -14343, 13118, -6753, 26348, -5451, 17738, 15547,
                1496, 32049, 27670, -5737,
            ],
            [
                100, 103, 3176, 3190, 5582, 7386, 7391, 9192, 10444, 10454, 10824, 10832, 13127,
                17559, 17566, 18870,
            ],
            16,
        ),
        (
            "WB",
            [
                -22652, -17555, 13218, -30669, -18448, -24215, -6162, 29839, 25372, -5339, -27398,
                -13653, -27384, 5281, 1734, 26247,
            ],
            [
                100, 103, 143, 146, 149, 152, 157, 217, 1734, 4528, 4539, 5281, 9443, 9453, 12514,
                26247,
            ],
            16,
        ),
        (
            "NB_MB-light",
            [
                4126, 4126, 6335, 15098, 17320, 18579, 18709, 20711, 24453, 32767, 0, 0, 0, 0, 0, 0,
            ],
            [
                4125, 4128, 6335, 15098, 17320, 18579, 18709, 20711, 24453, 32307, 0, 0, 0, 0, 0, 0,
            ],
            10,
        ),
        (
            "NB_MB-light",
            [
                250, 6335, 6335, 15098, 17320, 18579, 18709, 20711, 32727, 27995, 0, 0, 0, 0, 0, 0,
            ],
            [
                250, 6332, 6338, 15098, 17320, 18579, 18709, 20711, 30360, 30363, 0, 0, 0, 0, 0, 0,
            ],
            10,
        ),
        (
            "NB_MB-light",
            [
                250, 4126, 15098, 15098, 17320, 18579, 18709, 32687, 24453, 27995, 0, 0, 0, 0, 0, 0,
            ],
            [
                250, 4126, 15097, 15100, 17320, 18579, 18709, 28381, 28384, 28387, 0, 0, 0, 0, 0, 0,
            ],
            10,
        ),
        (
            "NB_MB-light",
            [
                3610, 3610, 11242, 13206, 13454, 13457, 15085, 21752, 31174, 32767, 0, 0, 0, 0, 0,
                0,
            ],
            [
                3609, 3612, 11242, 13206, 13454, 13457, 15085, 21752, 31174, 32307, 0, 0, 0, 0, 0,
                0,
            ],
            10,
        ),
        (
            "NB_MB-light",
            [
                3607, 11242, 11242, 13206, 13454, 13457, 15085, 21752, 32727, 31413, 0, 0, 0, 0, 0,
                0,
            ],
            [
                3607, 11239, 11245, 13206, 13454, 13457, 15085, 21752, 32069, 32072, 0, 0, 0, 0, 0,
                0,
            ],
            10,
        ),
        (
            "NB_MB-light",
            [
                3607, 3610, 13206, 13206, 13454, 13457, 15085, 32687, 31174, 31413, 0, 0, 0, 0, 0,
                0,
            ],
            [
                3607, 3610, 13205, 13208, 13454, 13457, 15085, 31760, 31763, 31766, 0, 0, 0, 0, 0,
                0,
            ],
            10,
        ),
        (
            "NB_MB-light",
            [
                3724, 3724, 3730, 9346, 14613, 18105, 18109, 21805, 23279, 32767, 0, 0, 0, 0, 0, 0,
            ],
            [
                3723, 3726, 3732, 9346, 14613, 18105, 18109, 21805, 23279, 32307, 0, 0, 0, 0, 0, 0,
            ],
            10,
        ),
        (
            "NB_MB-light",
            [
                2308, 3730, 3730, 9346, 14613, 18105, 18109, 21805, 32727, 23282, 0, 0, 0, 0, 0, 0,
            ],
            [
                2308, 3727, 3733, 9346, 14613, 18105, 18109, 21805, 28004, 28007, 0, 0, 0, 0, 0, 0,
            ],
            10,
        ),
        (
            "NB_MB-light",
            [
                2308, 3724, 9346, 9346, 14613, 18105, 18109, 32687, 23279, 23282, 0, 0, 0, 0, 0, 0,
            ],
            [
                2308, 3724, 9345, 9348, 14613, 18105, 18109, 26420, 26423, 26426, 0, 0, 0, 0, 0, 0,
            ],
            10,
        ),
        (
            "NB_MB-light",
            [
                5480, 5480, 5486, 10855, 17600, 17620, 18497, 21622, 32304, 32767, 0, 0, 0, 0, 0, 0,
            ],
            [
                5479, 5482, 5488, 10855, 17600, 17620, 18497, 21622, 32304, 32307, 0, 0, 0, 0, 0, 0,
            ],
            10,
        ),
        (
            "NB_MB-light",
            [
                5477, 5486, 5486, 10855, 17600, 17620, 18497, 21622, 32727, 32307, 0, 0, 0, 0, 0, 0,
            ],
            [
                5477, 5483, 5489, 10855, 17600, 17620, 18497, 21622, 32304, 32307, 0, 0, 0, 0, 0, 0,
            ],
            10,
        ),
        (
            "NB_MB-light",
            [
                5477, 5480, 10855, 10855, 17600, 17620, 18497, 32687, 32304, 32307, 0, 0, 0, 0, 0,
                0,
            ],
            [
                5477, 5480, 10854, 10857, 17600, 17620, 18497, 32301, 32304, 32307, 0, 0, 0, 0, 0,
                0,
            ],
            10,
        ),
        (
            "NB_MB-light",
            [
                4900, 4900, 11491, 12671, 12674, 12677, 22965, 26762, 27136, 32767, 0, 0, 0, 0, 0,
                0,
            ],
            [
                4899, 4902, 11491, 12671, 12674, 12677, 22965, 26762, 27136, 32307, 0, 0, 0, 0, 0,
                0,
            ],
            10,
        ),
        (
            "NB_MB-light",
            [
                2038, 11491, 11491, 12671, 12674, 12677, 22965, 26762, 32727, 32307, 0, 0, 0, 0, 0,
                0,
            ],
            [
                2038, 11488, 11494, 12671, 12674, 12677, 22965, 26762, 32304, 32307, 0, 0, 0, 0, 0,
                0,
            ],
            10,
        ),
        (
            "NB_MB-light",
            [
                2038, 4900, 12671, 12671, 12674, 12677, 22965, 32687, 27136, 32307, 0, 0, 0, 0, 0,
                0,
            ],
            [
                2038, 4900, 12670, 12673, 12676, 12679, 22965, 29911, 29914, 32307, 0, 0, 0, 0, 0,
                0,
            ],
            10,
        ),
        (
            "WB-light",
            [
                1432, 1432, 3604, 4902, 5914, 10281, 10773, 15548, 16662, 18665, 21504, 21507,
                21515, 23063, 25917, 32767,
            ],
            [
                1431, 1434, 3604, 4902, 5914, 10281, 10773, 15548, 16662, 18665, 21504, 21507,
                21515, 23063, 25917, 32421,
            ],
            16,
        ),
        (
            "WB-light",
            [
                100, 3604, 3604, 4902, 5914, 10281, 10773, 15548, 16662, 18665, 21504, 21507,
                21515, 23063, 32727, 28911,
            ],
            [
                100, 3584, 3624, 4902, 5914, 10281, 10773, 15548, 16662, 18665, 21504, 21507,
                21515, 23063, 30818, 30821,
            ],
            16,
        ),
        (
            "WB-light",
            [
                100, 1432, 4902, 4902, 5914, 10281, 10773, 15548, 16662, 18665, 21504, 21507,
                21515, 32687, 25917, 28911,
            ],
            [
                100, 1432, 4901, 4904, 5914, 10281, 10773, 15548, 16662, 18665, 21504, 21507,
                21515, 29171, 29178, 29181,
            ],
            16,
        ),
        (
            "WB-light",
            [
                3614, 3614, 5853, 5856, 10768, 10771, 12674, 14397, 14411, 17275, 18796, 19176,
                20011, 20838, 25910, 32767,
            ],
            [
                3613, 3616, 5853, 5856, 10768, 10771, 12674, 14397, 14411, 17275, 18796, 19176,
                20011, 20838, 25910, 32421,
            ],
            16,
        ),
        (
            "WB-light",
            [
                447, 5853, 5853, 5856, 10768, 10771, 12674, 14397, 14411, 17275, 18796, 19176,
                20011, 20838, 32727, 29373,
            ],
            [
                447, 5828, 5868, 5871, 10768, 10771, 12674, 14397, 14411, 17275, 18796, 19176,
                20011, 20838, 31049, 31052,
            ],
            16,
        ),
        (
            "WB-light",
            [
                447, 3614, 5856, 5856, 10768, 10771, 12674, 14397, 14411, 17275, 18796, 19176,
                20011, 32687, 25910, 29373,
            ],
            [
                447, 3614, 5855, 5858, 10768, 10771, 12674, 14397, 14411, 17275, 18796, 19176,
                20011, 29296, 29303, 29373,
            ],
            16,
        ),
        (
            "WB-light",
            [
                4174, 4174, 6790, 7660, 7663, 7666, 8528, 12829, 14741, 16503, 16514, 16839, 19900,
                23416, 23423, 32767,
            ],
            [
                4173, 4176, 6790, 7660, 7663, 7666, 8528, 12829, 14741, 16503, 16514, 16839, 19900,
                23416, 23423, 32421,
            ],
            16,
        ),
        (
            "WB-light",
            [
                2097, 6790, 6790, 7660, 7663, 7666, 8528, 12829, 14741, 16503, 16514, 16839, 19900,
                23416, 32727, 26531,
            ],
            [
                2097, 6770, 6810, 7660, 7663, 7666, 8528, 12829, 14741, 16503, 16514, 16839, 19900,
                23416, 29628, 29631,
            ],
            16,
        ),
        (
            "WB-light",
            [
                2097, 4174, 7660, 7660, 7663, 7666, 8528, 12829, 14741, 16503, 16514, 16839, 19900,
                32687, 23423, 26531,
            ],
            [
                2097, 4174, 7659, 7662, 7665, 7668, 8528, 12829, 14741, 16503, 16514, 16839, 19900,
                27547, 27554, 27557,
            ],
            16,
        ),
        (
            "WB-light",
            [
                2519, 2519, 3867, 3870, 8439, 9819, 13684, 13698, 13712, 17966, 20333, 21643,
                22720, 25216, 28019, 32767,
            ],
            [
                2518, 2521, 3867, 3870, 8439, 9819, 13684, 13698, 13712, 17966, 20333, 21643,
                22720, 25216, 28019, 32421,
            ],
            16,
        ),
        (
            "WB-light",
            [
                893, 3867, 3867, 3870, 8439, 9819, 13684, 13698, 13712, 17966, 20333, 21643, 22720,
                25216, 32727, 28632,
            ],
            [
                893, 3842, 3882, 3885, 8439, 9819, 13684, 13698, 13712, 17966, 20333, 21643, 22720,
                25216, 30679, 30682,
            ],
            16,
        ),
        (
            "WB-light",
            [
                893, 2519, 3870, 3870, 8439, 9819, 13684, 13698, 13712, 17966, 20333, 21643, 22720,
                32687, 28019, 28632,
            ],
            [
                893, 2519, 3869, 3872, 8439, 9819, 13684, 13698, 13712, 17966, 20333, 21643, 22720,
                29779, 29786, 29789,
            ],
            16,
        ),
        (
            "WB-light",
            [
                1086, 1086, 6483, 7937, 8959, 11076, 11545, 11978, 14772, 16174, 19789, 19792,
                25006, 28334, 29720, 32767,
            ],
            [
                1085, 1088, 6483, 7937, 8959, 11076, 11545, 11978, 14772, 16174, 19789, 19792,
                25006, 28334, 29720, 32421,
            ],
            16,
        ),
        (
            "WB-light",
            [
                100, 6483, 6483, 7937, 8959, 11076, 11545, 11978, 14772, 16174, 19789, 19792,
                25006, 28334, 32727, 32421,
            ],
            [
                100, 6463, 6503, 7937, 8959, 11076, 11545, 11978, 14772, 16174, 19789, 19792,
                25006, 28334, 32418, 32421,
            ],
            16,
        ),
        (
            "WB-light",
            [
                100, 1086, 7937, 7937, 8959, 11076, 11545, 11978, 14772, 16174, 19789, 19792,
                25006, 32687, 29720, 32421,
            ],
            [
                100, 1086, 7936, 7939, 8959, 11076, 11545, 11978, 14772, 16174, 19789, 19792,
                25006, 31201, 31208, 32421,
            ],
            16,
        ),
    ];

    #[test]
    fn stabilize_matches_reference_oracle() {
        for &(tag, input, expected, order) in &STABILIZE_ORACLE_CASES {
            let cb = if tag.starts_with("WB") {
                &NLSF_CB_WB
            } else {
                &NLSF_CB_NB_MB
            };
            let mut nlsf_q15 = input;
            nlsf_stabilize(&mut nlsf_q15[..order], cb.delta_min_q15);
            assert_eq!(
                &nlsf_q15[..order],
                &expected[..order],
                "stabilize mismatch ({tag})"
            );
        }
    }

    /// (Q12 filter, order, inverse prediction gain is nonzero).
    static IPG_ORACLE_CASES: [([i16; 16], usize, bool); 14] = [
        (
            [
                1427, -2058, 5354, 678, 350, -2601, -1268, 69, 1005, 1128, 0, 0, 0, 0, 0, 0,
            ],
            10,
            true,
        ),
        (
            [
                3527, -1788, -4123, 7574, -3923, -850, 2539, -3472, 324, -845, 0, 0, 0, 0, 0, 0,
            ],
            10,
            true,
        ),
        (
            [
                6511, -6481, 10452, -10972, 8400, -8931, 6658, -3221, 1300, 66, 0, 0, 0, 0, 0, 0,
            ],
            10,
            true,
        ),
        (
            [
                761, 878, -741, 2661, -5210, -811, 570, -451, 1504, -1468, 0, 0, 0, 0, 0, 0,
            ],
            10,
            true,
        ),
        (
            [
                344, 2131, -4162, 809, 5903, -1511, -2007, 3013, -1362, -1705, 0, 0, 0, 0, 0, 0,
            ],
            10,
            true,
        ),
        (
            [
                7498, -6912, 10484, -12889, 10173, -8112, 6500, -4191, 1857, -1358, 1903, 505,
                -840, -1142, -358, 977,
            ],
            16,
            true,
        ),
        (
            [
                9467, -17304, 23823, -25765, 22720, -14461, 5498, 913, -2357, 714, 3199, -4823,
                5804, -4282, 1834, -1001,
            ],
            16,
            true,
        ),
        (
            [
                12382, -22252, 29569, -32697, 30970, -26290, 20355, -13953, 7961, -3376, 613, 522,
                -622, 375, -153, 35,
            ],
            16,
            true,
        ),
        (
            [
                7085, -5653, 5488, -5056, 4879, -4576, 2769, -3729, 5585, -5633, 4398, -3275, 3261,
                -1455, 440, -619,
            ],
            16,
            true,
        ),
        (
            [
                5974, -3691, -2826, 7843, -6791, 3714, 2703, -6007, 6722, -1679, -3940, 6015,
                -3895, -806, 2186, -1438,
            ],
            16,
            true,
        ),
        (
            [
                2000, 2000, 2000, 2000, 2000, 2000, 2000, 2000, 2000, 2000, 0, 0, 0, 0, 0, 0,
            ],
            10,
            false,
        ),
        (
            [
                3000, -3000, 3000, -3000, 3000, -3000, 3000, -3000, 3000, -3000, 0, 0, 0, 0, 0, 0,
            ],
            10,
            true,
        ),
        (
            [
                32767, 32767, 32767, 32767, 32767, 32767, 32767, 32767, 32767, 32767, 32767, 32767,
                32767, 32767, 32767, 32767,
            ],
            16,
            false,
        ),
        (
            [
                20770, 6067, -11920, 31977, 23918, 31759, -27492, -347, -7558, -13781, 0, 0, 0, 0,
                0, 0,
            ],
            10,
            false,
        ),
    ];

    #[test]
    fn lpc_inverse_pred_gain_matches_reference_oracle() {
        for &(filter, order, stable) in &IPG_ORACLE_CASES {
            let gain = lpc_inverse_pred_gain(&filter[..order], order);
            assert_eq!(
                gain != 0,
                stable,
                "inverse pred gain mismatch (gain={gain}, order={order})"
            );
        }
    }

    /// `inverse32_varq` on power-of-two denominators: the reference's
    /// 14-bit first approximation plus one refinement step lands one
    /// unit below the exact reciprocal here (both cases equal 16383,
    /// verified against the C algorithm's exact arithmetic).
    #[test]
    fn inverse32_varq_matches_reference_arithmetic() {
        assert_eq!(inverse32_varq(1 << 16, 30), 16383);
        assert_eq!(inverse32_varq(1 << 20, 34), 16383);
    }

    #[test]
    fn nlsf_interpolate_endpoints_and_formula() {
        let cur = [100i16, 400, 9000, 32000, 100, 0, 7, 32767];
        let prev = [200i16, -300, 8000, 32000, 50, 0, -2, 0];
        for &coef in &[0i32, 1, 2, 3, 4] {
            let mut dst = [0i16; 8];
            nlsf_interpolate(&mut dst, &prev, &cur, coef);
            for i in 0..8 {
                let expected = prev[i] as i32 + ((coef * (cur[i] as i32 - prev[i] as i32)) >> 2);
                assert_eq!(dst[i] as i32, expected, "coef={coef}, i={i}");
            }
        }
        // coef 0 -> exactly prev, coef 4 -> exactly cur
        let mut dst = [0i16; 8];
        nlsf_interpolate(&mut dst, &prev, &cur, 0);
        assert_eq!(dst, prev);
        nlsf_interpolate(&mut dst, &prev, &cur, 4);
        assert_eq!(dst, cur);
    }

    /// Small deterministic PRNG (xorshift32) for the property tests, so
    /// no external crate is needed.
    struct XorShift(u32);

    impl XorShift {
        fn next_u32(&mut self) -> u32 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            self.0 = x;
            x
        }

        fn next_range(&mut self, lo: i32, hi: i32) -> i32 {
            lo + (self.next_u32() % (hi - lo + 1) as u32) as i32
        }
    }

    /// Both stabilization paths must output a sorted vector inside the
    /// border limits with at least the minimum spacing.
    #[test]
    fn stabilize_output_satisfies_spacing_invariants() {
        let mut rng = XorShift(0x1234_5678);
        for &cb in &[&NLSF_CB_NB_MB, &NLSF_CB_WB] {
            let order = cb.order as usize;
            for _ in 0..1000 {
                let mut nlsf_q15 = [0i16; MAX_LPC_ORDER];
                for v in nlsf_q15[..order].iter_mut() {
                    *v = rng.next_range(i16::MIN as i32, i16::MAX as i32) as i16;
                }
                let nlsf_in = nlsf_q15;
                nlsf_stabilize(&mut nlsf_q15[..order], cb.delta_min_q15);
                let ndm = cb.delta_min_q15;
                assert!(nlsf_q15[0] >= ndm[0], "lower border violated");
                for i in 1..order {
                    assert!(
                        nlsf_q15[i] - nlsf_q15[i - 1] >= ndm[i],
                        "min spacing violated at {i}"
                    );
                }
                assert!(
                    nlsf_q15[order - 1] as i32 <= 32768 - ndm[order] as i32,
                    "upper border violated: input {:?} output {:?}",
                    &nlsf_in[..order],
                    &nlsf_q15[..order]
                );
            }
        }
    }

    /// Decoding arbitrary (but bitstream-valid) index vectors never
    /// panics and always produces a stabilized in-range NLSF vector.
    #[test]
    fn nlsf_decode_never_panics_and_is_stabilized() {
        let mut rng = XorShift(0x9E37_79B9);
        for &cb in &[&NLSF_CB_NB_MB, &NLSF_CB_WB] {
            let order = cb.order as usize;
            for _ in 0..1000 {
                let mut indices = [0i8; MAX_LPC_ORDER + 1];
                indices[0] = rng.next_range(0, cb.n_vectors as i32 - 1) as i8;
                for r in &mut indices[1..=order] {
                    *r = rng.next_range(-4, 4) as i8;
                }
                let mut nlsf_q15 = [0i16; MAX_LPC_ORDER];
                nlsf_decode(&mut nlsf_q15[..order], &indices[..=order], cb);
                for &v in &nlsf_q15[..order] {
                    assert!((0..=32767).contains(&v));
                }
                let ndm = cb.delta_min_q15;
                assert!(nlsf_q15[0] >= ndm[0]);
                for i in 1..order {
                    assert!(nlsf_q15[i] - nlsf_q15[i - 1] >= ndm[i]);
                }
                assert!(nlsf_q15[order - 1] as i32 <= 32768 - ndm[order] as i32);
            }
        }
    }

    #[test]
    fn lpc_fit_passthrough_and_clipping() {
        // Small coefficients pass through with plain rounding (Q17 -> Q12).
        let mut a_qin = [
            -40_000i32,
            33_000,
            -1,
            0,
            65536,
            -65_536,
            1 << 16,
            123,
            -321,
            999,
        ];
        let mut a_qout = [0i16; 10];
        lpc_fit(&mut a_qout, &mut a_qin, 12, 17, 10);
        assert_eq!(a_qout[4], 2048);
        assert_eq!(a_qout[5], -2048);
        for k in 0..10 {
            assert_eq!(a_qout[k] as i32, rshift_round(a_qin[k], 5));
        }

        // Coefficients too large for Q12, maximum held by the LAST
        // coefficient (idx + 1 == d gives the gentlest expansion chirp):
        // all 10 bandwidth expansions still leave the maximum above the
        // Q12 limit, so the clip branch runs (oracle-verified: the
        // expansions wrap the chirp arithmetic and land at 32460).
        let mut a_qin = [0i32; 10];
        a_qin[9] = 1 << 30;
        let mut a_qout = [0i16; 10];
        lpc_fit(&mut a_qout, &mut a_qin, 12, 17, 10);
        assert_eq!(a_qout[9], 32460);
        assert_eq!(a_qin[9], 32460 << 5);
        for k in 0..9 {
            assert_eq!(a_qout[k], 0);
            assert_eq!(a_qin[k], 0);
        }

        // A vector shrunk below the limit after ONE expansion exits
        // through the non-clipping branch: outputs are the rounded
        // expanded values, unclipped.
        let mut a_qin = [1 << 20; 10];
        let mut a_qout = [0i16; 10];
        lpc_fit(&mut a_qout, &mut a_qin, 12, 17, 10);
        for k in 0..10 {
            assert_eq!(a_qout[k] as i32, rshift_round(a_qin[k], 5));
        }
        assert!(a_qout[0] < 32767);
    }

    /// FNV-1a 64 hash of a byte stream.
    fn fnv1a64(data: &[u8]) -> u64 {
        let mut h: u64 = 0xCBF2_9CE4_8422_2325;
        for &b in data {
            h ^= b as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01B3);
        }
        h
    }

    /// Same LCG the Python oracle uses (glibc-style, 31-bit output).
    struct Lcg(u32);

    impl Lcg {
        fn next(&mut self) -> u32 {
            self.0 = (1103515245u32.wrapping_mul(self.0).wrapping_add(12345)) & 0x7FFF_FFFF;
            self.0
        }
    }

    /// Differential test against the Python transcription of the C code:
    /// 500 random (codebook, CB1 index, residual vector) cases run
    /// through the full decode -> NLSF2A -> bwexpander pipeline, and the
    /// FNV-1a hash of every output value must match the hash the oracle
    /// computed from the reference semantics.
    #[test]
    fn differential_hash_matches_python_oracle() {
        let mut lc = Lcg(0xA1_1CE);
        let mut stream: Vec<u8> = Vec::with_capacity(39000);
        for it in 0..500 {
            let cb = if it % 2 == 1 {
                &NLSF_CB_WB
            } else {
                &NLSF_CB_NB_MB
            };
            let order = cb.order as usize;
            let cb1 = (lc.next() % 32) as i8;
            let mut indices = [0i8; MAX_LPC_ORDER + 1];
            indices[0] = cb1;
            for r in &mut indices[1..=order] {
                *r = (lc.next() % 9) as i8 - 4;
            }
            let mut nlsf_q15 = [0i16; MAX_LPC_ORDER];
            nlsf_decode(&mut nlsf_q15[..order], &indices[..=order], cb);
            let mut a_q12 = [0i16; MAX_LPC_ORDER];
            nlsf2a(&mut a_q12[..order], &nlsf_q15[..order], order);
            let mut a_bwe = a_q12;
            bwexpander(&mut a_bwe[..order], 63570);
            for v in nlsf_q15[..order]
                .iter()
                .chain(&a_q12[..order])
                .chain(&a_bwe[..order])
            {
                stream.extend_from_slice(&(*v as u16).to_le_bytes());
            }
        }
        assert_eq!(stream.len(), 39000);
        assert_eq!(fnv1a64(&stream), 0x1DB2_6DF7_52BE_F2E7);
    }
}
