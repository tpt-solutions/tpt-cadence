//! SILK NLSF quantization — exact fixed-point port of the reference
//! encoder's quantizer chain (`silk/NLSF_encode.c` and its helpers).
//!
//! The chain, in the reference's order:
//!
//! 1. [`nlsf_stabilize_input`] → the decoder-shared `silk_NLSF_stabilize`
//!    (imported from [`super::nlsf`]) on the unquantized input vector.
//! 2. [`nlsf_vq`] (`silk_NLSF_VQ`): stage-1 weighted absolute
//!    (predictive) quantization error for all 32 codebook vectors, using
//!    the codebook's per-vector weights (`CB1_Wght_Q9`).
//! 3. [`insertion_sort_increasing`] (`silk/sort.c`): the `n_survivors`
//!    best stage-1 candidates.
//! 4. per survivor: stage-2 residual computation, [`nlsf_del_dec_quant`]
//!    (`silk_NLSF_del_dec_quant.c`, a 4-state delayed-decision trellis
//!    over the residual codebook with rate/distortion weighting from the
//!    `ec_rates_Q5` tables), plus the stage-1 symbol's entropy cost.
//! 5. the winner's indices go through the decoder-shared
//!    [`super::nlsf::nlsf_decode`], so the transmitted vector is exactly
//!    what any conforming decoder reconstructs.
//!
//! [`nlsf_vq_weights_laroia`] (`silk/NLSF_VQ_weights_laroia.c`) computes
//! the Laroia input weights that `silk_process_NLSFs` feeds in.
//!
//! SOURCE: Xiph.Org libopus 1.5.2, `silk/NLSF_encode.c`,
//! `silk/NLSF_del_dec_quant.c`, `silk/NLSF_VQ.c`,
//! `silk/NLSF_VQ_weights_laroia.c`, `silk/sort.c`, `silk/define.h`
//! (`NLSF_W_Q`, `NLSF_QUANT_MAX_AMPLITUDE_EXT`,
//! `NLSF_QUANT_DEL_DEC_STATES`) (BSD-3-Clause).
#![allow(dead_code)]

use crate::silk::decode_indices::{nlsf_unpack, MAX_LPC_ORDER, NLSF_QUANT_MAX_AMPLITUDE};
use crate::silk::nlsf::{nlsf_decode, nlsf_stabilize};
use crate::silk::sigproc::{div32_varq, smulbb};
use crate::silk::tables::NlsfCbStruct;

/// `NLSF_W_Q` (`silk/define.h`): the Laroia weights' shift.
const NLSF_W_Q: i32 = 2;
/// `NLSF_QUANT_MAX_AMPLITUDE_EXT` (`silk/define.h`): the escape-extended
/// residual range the trellis quantizer considers.
const NLSF_QUANT_MAX_AMPLITUDE_EXT: i32 = 10;
/// `NLSF_QUANT_DEL_DEC_STATES(_LOG2)` (`silk/define.h`).
const NLSF_QUANT_DEL_DEC_STATES_LOG2: u32 = 2;
const NLSF_QUANT_DEL_DEC_STATES: usize = 1 << NLSF_QUANT_DEL_DEC_STATES_LOG2;
/// `NLSF_QUANT_LEVEL_ADJ` in Q10 (0.1, `SILK_FIX_CONST`); the shared
/// reconstruction pulls nonzero residuals toward zero by this much.
const NLSF_QUANT_LEVEL_ADJ_Q10: i32 = 102;

/// `silk_NLSF_VQ_weights_laroia` (`silk/NLSF_VQ_weights_laroia.c`):
/// Laroia low-complexity NLSF weights from the inverse neighboring NLSF
/// distances.
pub(crate) fn nlsf_vq_weights_laroia(out_qw: &mut [i16], nlsf_q15: &[i16]) {
    let d = nlsf_q15.len();
    debug_assert!(d > 0 && d % 2 == 0 && out_qw.len() >= d);

    /* First value */
    let mut tmp1_int = (1i32 << (15 + NLSF_W_Q)) / nlsf_q15[0].max(1) as i32;
    let mut tmp2_int = (1i32 << (15 + NLSF_W_Q)) / (nlsf_q15[1] as i32 - nlsf_q15[0] as i32).max(1);
    out_qw[0] = (tmp1_int + tmp2_int).min(i16::MAX as i32) as i16;
    debug_assert!(out_qw[0] > 0);

    /* Main loop */
    let mut k = 1;
    while k + 1 < d {
        tmp1_int = (1i32 << (15 + NLSF_W_Q)) / (nlsf_q15[k + 1] as i32 - nlsf_q15[k] as i32).max(1);
        out_qw[k] = (tmp1_int + tmp2_int).min(i16::MAX as i32) as i16;
        debug_assert!(out_qw[k] > 0);

        tmp2_int =
            (1i32 << (15 + NLSF_W_Q)) / (nlsf_q15[k + 2] as i32 - nlsf_q15[k + 1] as i32).max(1);
        out_qw[k + 1] = (tmp1_int + tmp2_int).min(i16::MAX as i32) as i16;
        debug_assert!(out_qw[k + 1] > 0);
        k += 2;
    }

    /* Last value */
    tmp1_int = (1i32 << (15 + NLSF_W_Q)) / ((1 << 15) - nlsf_q15[d - 1] as i32).max(1);
    out_qw[d - 1] = (tmp1_int + tmp2_int).min(i16::MAX as i32) as i16;
    debug_assert!(out_qw[d - 1] > 0);
}

/// `silk_NLSF_VQ` (`silk/NLSF_VQ.c`): weighted absolute predictive
/// quantization error of `in_q15` against all stage-1 codebook vectors.
pub(crate) fn nlsf_vq(err_q24: &mut [i32], in_q15: &[i16], cb: &NlsfCbStruct) {
    let k_vectors = cb.n_vectors as usize;
    let order = cb.order as usize;
    debug_assert_eq!(err_q24.len(), k_vectors);
    debug_assert!(order % 2 == 0);
    for (i, err) in err_q24.iter_mut().enumerate().take(k_vectors) {
        let cb_q8 = &cb.cb1_nlsf_q8[i * order..(i + 1) * order];
        let w_q9 = &cb.cb1_wght_q9[i * order..(i + 1) * order];
        let mut sum_error_q24: i32 = 0;
        let mut pred_q24: i32 = 0;
        let mut m = order as isize - 2;
        while m >= 0 {
            /* Compute weighted absolute predictive quantization error for
             * index m + 1 (diff_Q15 range: -32767..=32767) */
            let diff_q15 = (in_q15[m as usize + 1] as i32) - ((cb_q8[m as usize + 1] as i32) << 7);
            let diffw_q24 = smulbb(diff_q15, w_q9[m as usize + 1] as i32);
            sum_error_q24 =
                sum_error_q24.wrapping_add((diffw_q24.wrapping_sub(pred_q24 >> 1)).abs());
            pred_q24 = diffw_q24;

            /* ... and for index m */
            let diff_q15 = (in_q15[m as usize] as i32) - ((cb_q8[m as usize] as i32) << 7);
            let diffw_q24 = smulbb(diff_q15, w_q9[m as usize] as i32);
            sum_error_q24 =
                sum_error_q24.wrapping_add((diffw_q24.wrapping_sub(pred_q24 >> 1)).abs());
            pred_q24 = diffw_q24;
            m -= 2;
        }
        *err = sum_error_q24;
    }
}

/// `silk_insertion_sort_increasing` (`silk/sort.c`): insertion-sorts the
/// `k` smallest values of `a` (length `l >= k`) increasingly and records
/// their original indices in `idx[0..k]`.
pub(crate) fn insertion_sort_increasing(a: &mut [i32], idx: &mut [i32], k: usize) {
    let l = a.len();
    debug_assert!(k > 0 && l > 0 && l >= k);

    /* Write start indices in index vector */
    for (i, slot) in idx.iter_mut().enumerate().take(k) {
        *slot = i as i32;
    }

    /* Sort vector elements by value, increasing order */
    for i in 1..k {
        let value = a[i];
        let mut j = i as isize - 1;
        while j >= 0 && value < a[j as usize] {
            a[j as usize + 1] = a[j as usize];
            idx[j as usize + 1] = idx[j as usize];
            j -= 1;
        }
        a[(j + 1) as usize] = value;
        idx[(j + 1) as usize] = i as i32;
    }

    /* If less than L values are asked for, check the remaining values,
     * but only spend CPU to ensure that the K first values are correct */
    for i in k..l {
        let value = a[i];
        if value < a[k - 1] {
            let mut j = k as isize - 2;
            while j >= 0 && value < a[j as usize] {
                a[j as usize + 1] = a[j as usize];
                idx[j as usize + 1] = idx[j as usize];
                j -= 1;
            }
            a[(j + 1) as usize] = value;
            idx[(j + 1) as usize] = i as i32;
        }
    }
}

/// `silk_NLSF_del_dec_quant` (`silk/NLSF_del_dec_quant.c`): the
/// delayed-decision trellis quantizer for the stage-2 residuals.
///
/// Processes coefficients from `order - 1` down to 0 (matching the
/// decoder's backward-predictive reconstruction), keeping 4 survivor
/// states. Returns the winner's rate/distortion value in Q25.
#[allow(clippy::too_many_arguments)]
pub(crate) fn nlsf_del_dec_quant(
    indices: &mut [i8],
    x_q10: &[i16],
    w_q5: &[i32],
    pred_coef_q8: &[u8],
    ec_ix: &[usize],
    ec_rates_q5: &[u8],
    quant_step_size_q16: i32,
    inv_quant_step_size_q6: i32,
    mu_q20: i32,
    order: usize,
) -> i32 {
    debug_assert!(indices.len() >= order);
    debug_assert!(x_q10.len() >= order);
    debug_assert!(w_q5.len() >= order);

    /* Precompute the reconstruction levels (Q10) of ind_tmp and ind_tmp+1
     * with the toward-zero level adjustment applied. */
    let mut out0_q10_table = [0i32; 2 * NLSF_QUANT_MAX_AMPLITUDE_EXT as usize];
    let mut out1_q10_table = [0i32; 2 * NLSF_QUANT_MAX_AMPLITUDE_EXT as usize];
    for ext in (-NLSF_QUANT_MAX_AMPLITUDE_EXT)..NLSF_QUANT_MAX_AMPLITUDE_EXT {
        let mut out0 = ext << 10;
        let mut out1 = out0 + 1024;
        if ext > 0 {
            out0 -= NLSF_QUANT_LEVEL_ADJ_Q10;
            out1 -= NLSF_QUANT_LEVEL_ADJ_Q10;
        } else if ext == 0 {
            out1 -= NLSF_QUANT_LEVEL_ADJ_Q10;
        } else if ext == -1 {
            out0 += NLSF_QUANT_LEVEL_ADJ_Q10;
        } else {
            out0 += NLSF_QUANT_LEVEL_ADJ_Q10;
            out1 += NLSF_QUANT_LEVEL_ADJ_Q10;
        }
        let slot = (ext + NLSF_QUANT_MAX_AMPLITUDE_EXT) as usize;
        out0_q10_table[slot] = smulbb(out0, quant_step_size_q16) >> 16;
        out1_q10_table[slot] = smulbb(out1, quant_step_size_q16) >> 16;
    }

    let mut ind = [[0i8; MAX_LPC_ORDER]; NLSF_QUANT_DEL_DEC_STATES];
    let mut prev_out_q10 = [0i16; 2 * NLSF_QUANT_DEL_DEC_STATES];
    let mut rd_q25 = [0i32; 2 * NLSF_QUANT_DEL_DEC_STATES];
    let mut rd_min_q25 = [0i32; NLSF_QUANT_DEL_DEC_STATES];
    let mut rd_max_q25 = [0i32; NLSF_QUANT_DEL_DEC_STATES];
    let mut ind_sort = [0i32; NLSF_QUANT_DEL_DEC_STATES];

    let mut n_states = 1usize;
    rd_q25[0] = 0;
    prev_out_q10[0] = 0;
    for i in (0..order).rev() {
        let rates_q5 = &ec_rates_q5[ec_ix[i]..];
        let in_q10 = x_q10[i] as i32;
        for j in 0..n_states {
            let pred_q10 = smulbb(pred_coef_q8[i] as i32, prev_out_q10[j] as i32) >> 8;
            let res_q10 = (in_q10 - pred_q10) as i16 as i32;
            let mut ind_tmp = smulbb(inv_quant_step_size_q6, res_q10 as i16 as i32) >> 16;
            ind_tmp = ind_tmp.clamp(
                -NLSF_QUANT_MAX_AMPLITUDE_EXT,
                NLSF_QUANT_MAX_AMPLITUDE_EXT - 1,
            );
            ind[j][i] = ind_tmp as i8;

            /* Compute outputs for ind_tmp and ind_tmp + 1 */
            let slot = (ind_tmp + NLSF_QUANT_MAX_AMPLITUDE_EXT) as usize;
            let mut out0_q10 = out0_q10_table[slot];
            let mut out1_q10 = out1_q10_table[slot];
            out0_q10 += pred_q10;
            out1_q10 += pred_q10;
            prev_out_q10[j] = out0_q10 as i16;
            prev_out_q10[j + n_states] = out1_q10 as i16;

            /* Compute RD for ind_tmp and ind_tmp + 1 */
            let (rate0_q5, rate1_q5) = if ind_tmp + 1 >= NLSF_QUANT_MAX_AMPLITUDE {
                if ind_tmp + 1 == NLSF_QUANT_MAX_AMPLITUDE {
                    (
                        rates_q5[(ind_tmp + NLSF_QUANT_MAX_AMPLITUDE) as usize] as i32,
                        280i32,
                    )
                } else {
                    let rate0 = 280 - 43 * NLSF_QUANT_MAX_AMPLITUDE + 43 * ind_tmp;
                    (rate0, rate0 + 43)
                }
            } else if ind_tmp <= -NLSF_QUANT_MAX_AMPLITUDE {
                if ind_tmp == -NLSF_QUANT_MAX_AMPLITUDE {
                    (
                        280i32,
                        rates_q5[(ind_tmp + 1 + NLSF_QUANT_MAX_AMPLITUDE) as usize] as i32,
                    )
                } else {
                    let rate0 = 280 - 43 * NLSF_QUANT_MAX_AMPLITUDE - 43 * ind_tmp;
                    (rate0, rate0 - 43)
                }
            } else {
                (
                    rates_q5[(ind_tmp + NLSF_QUANT_MAX_AMPLITUDE) as usize] as i32,
                    rates_q5[(ind_tmp + 1 + NLSF_QUANT_MAX_AMPLITUDE) as usize] as i32,
                )
            };
            let rd_tmp_q25 = rd_q25[j];
            let diff_q10 = in_q10 - out0_q10;
            rd_q25[j] = rd_tmp_q25
                .wrapping_add(smulbb(diff_q10, diff_q10).wrapping_mul(w_q5[i]))
                .wrapping_add(smulbb(mu_q20, rate0_q5));
            let diff_q10 = in_q10 - out1_q10;
            rd_q25[j + n_states] = rd_tmp_q25
                .wrapping_add(smulbb(diff_q10, diff_q10).wrapping_mul(w_q5[i]))
                .wrapping_add(smulbb(mu_q20, rate1_q5));
        }

        if n_states <= NLSF_QUANT_DEL_DEC_STATES / 2 {
            /* Double the number of states and copy */
            for j in 0..n_states {
                ind[j + n_states][i] = ind[j][i] + 1;
            }
            n_states <<= 1;
            for j in n_states..NLSF_QUANT_DEL_DEC_STATES {
                ind[j][i] = ind[j - n_states][i];
            }
        } else {
            /* Sort lower and upper half of RD_Q25, pairwise */
            for j in 0..NLSF_QUANT_DEL_DEC_STATES {
                if rd_q25[j] > rd_q25[j + NLSF_QUANT_DEL_DEC_STATES] {
                    rd_max_q25[j] = rd_q25[j];
                    rd_min_q25[j] = rd_q25[j + NLSF_QUANT_DEL_DEC_STATES];
                    rd_q25[j] = rd_min_q25[j];
                    rd_q25[j + NLSF_QUANT_DEL_DEC_STATES] = rd_max_q25[j];
                    /* Swap prev_out values */
                    prev_out_q10.swap(j, j + NLSF_QUANT_DEL_DEC_STATES);
                    ind_sort[j] = (j + NLSF_QUANT_DEL_DEC_STATES) as i32;
                } else {
                    rd_min_q25[j] = rd_q25[j];
                    rd_max_q25[j] = rd_q25[j + NLSF_QUANT_DEL_DEC_STATES];
                    ind_sort[j] = j as i32;
                }
            }
            /* Compare the highest RD values of the winning half with the
             * lowest one in the losing half, and copy if necessary;
             * afterwards ind_sort[] holds the winning states. */
            loop {
                let mut min_max_q25 = i32::MAX;
                let mut max_min_q25 = 0;
                let mut ind_min_max = 0usize;
                let mut ind_max_min = 0usize;
                for j in 0..NLSF_QUANT_DEL_DEC_STATES {
                    if min_max_q25 > rd_max_q25[j] {
                        min_max_q25 = rd_max_q25[j];
                        ind_min_max = j;
                    }
                    if max_min_q25 < rd_min_q25[j] {
                        max_min_q25 = rd_min_q25[j];
                        ind_max_min = j;
                    }
                }
                if min_max_q25 >= max_min_q25 {
                    break;
                }
                /* Copy ind_min_max to ind_max_min */
                ind_sort[ind_max_min] = ind_sort[ind_min_max] ^ NLSF_QUANT_DEL_DEC_STATES as i32;
                rd_q25[ind_max_min] = rd_q25[ind_min_max + NLSF_QUANT_DEL_DEC_STATES];
                prev_out_q10[ind_max_min] = prev_out_q10[ind_min_max + NLSF_QUANT_DEL_DEC_STATES];
                rd_min_q25[ind_max_min] = 0;
                rd_max_q25[ind_min_max] = i32::MAX;
                ind[ind_max_min] = ind[ind_min_max];
            }
            /* Increment the index if it comes from the upper half */
            for j in 0..NLSF_QUANT_DEL_DEC_STATES {
                ind[j][i] += (ind_sort[j] >> NLSF_QUANT_DEL_DEC_STATES_LOG2) as i8;
            }
        }
    }

    /* Last sample: find winner, copy indices and return RD value */
    let mut ind_tmp = 0usize;
    let mut min_q25 = i32::MAX;
    for (j, &rd) in rd_q25
        .iter()
        .enumerate()
        .take(2 * NLSF_QUANT_DEL_DEC_STATES)
    {
        if min_q25 > rd {
            min_q25 = rd;
            ind_tmp = j;
        }
    }
    for (j, slot) in indices.iter_mut().enumerate().take(order) {
        *slot = ind[ind_tmp & (NLSF_QUANT_DEL_DEC_STATES - 1)][j];
        debug_assert!(*slot >= -NLSF_QUANT_MAX_AMPLITUDE_EXT as i8);
        debug_assert!(*slot <= NLSF_QUANT_MAX_AMPLITUDE_EXT as i8);
    }
    indices[0] += (ind_tmp >> NLSF_QUANT_DEL_DEC_STATES_LOG2) as i8;
    debug_assert!(indices[0] <= NLSF_QUANT_MAX_AMPLITUDE_EXT as i8);
    min_q25
}

/// `silk_NLSF_encode` (`silk/NLSF_encode.c`): full NLSF vector encoding.
///
/// `nlsf_indices` receives the `[order + 1]` codebook path (stage-1 index
/// followed by stage-2 residuals); `p_nlsf_q15` arrives with the
/// unquantized vector and is overwritten with the decoder-reconstructed
/// (stabilized) quantized one. Returns the winning rate/distortion value
/// in Q25. `pw_q2` are the input weights (Q2 shift per `NLSF_W_Q`
/// convention — see [`nlsf_vq_weights_laroia`]), `nlsf_mu_q20` the rate
/// weight, `n_survivors` the stage-1 survivor count.
#[allow(clippy::too_many_arguments)]
pub(crate) fn nlsf_encode(
    nlsf_indices: &mut [i8],
    p_nlsf_q15: &mut [i16],
    cb: &NlsfCbStruct,
    pw_q2: &[i16],
    nlsf_mu_q20: i32,
    n_survivors: usize,
    signal_type: i8,
) -> i32 {
    let order = cb.order as usize;
    let n_vectors = cb.n_vectors as usize;
    /* The slices may be `MAX_LPC_ORDER(+1)`-sized caller buffers; only
     * the first `order(+1)` entries are used. */
    debug_assert!(nlsf_indices.len() > order);
    debug_assert!(p_nlsf_q15.len() >= order);
    debug_assert!(n_survivors >= 1 && n_survivors <= n_vectors);
    debug_assert!((0..=2).contains(&signal_type));

    /* NLSF stabilization */
    nlsf_stabilize(&mut p_nlsf_q15[..order], cb.delta_min_q15);

    /* First stage: VQ */
    let mut err_q24 = [0i32; 32];
    nlsf_vq(&mut err_q24[..n_vectors], p_nlsf_q15, cb);

    /* Sort the quantization errors */
    let mut temp_indices1 = [0i32; 32];
    insertion_sort_increasing(
        &mut err_q24[..n_vectors],
        &mut temp_indices1[..n_survivors],
        n_survivors,
    );

    let mut rd_q25 = [0i32; 32];
    let mut temp_indices2 = [[0i8; MAX_LPC_ORDER]; 32];

    /* Loop over survivors */
    for s in 0..n_survivors {
        let ind1 = temp_indices1[s] as usize;

        /* Residual after first stage */
        let cb_element = &cb.cb1_nlsf_q8[ind1 * order..(ind1 + 1) * order];
        let cb_wght_q9 = &cb.cb1_wght_q9[ind1 * order..(ind1 + 1) * order];
        let mut nlsf_tmp_q15 = [0i16; MAX_LPC_ORDER];
        let mut res_q10 = [0i16; MAX_LPC_ORDER];
        let mut w_adj_q5 = [0i32; MAX_LPC_ORDER];
        for i in 0..order {
            nlsf_tmp_q15[i] = ((cb_element[i] as i32) << 7) as i16;
            let w_tmp_q9 = cb_wght_q9[i] as i32;
            res_q10[i] =
                (smulbb((p_nlsf_q15[i] as i32) - (nlsf_tmp_q15[i] as i32), w_tmp_q9) >> 14) as i16;
            w_adj_q5[i] = div32_varq(pw_q2[i] as i32, smulbb(w_tmp_q9, w_tmp_q9), 21);
        }

        /* Unpack entropy table indices and predictor for current CB1 */
        let (ec_ix, pred_q8) = nlsf_unpack(cb, ind1);

        /* Trellis quantizer */
        rd_q25[s] = nlsf_del_dec_quant(
            &mut temp_indices2[s],
            &res_q10[..order],
            &w_adj_q5[..order],
            &pred_q8[..order],
            &ec_ix[..order],
            cb.ec_rates_q5,
            cb.quant_step_size_q16 as i32,
            cb.inv_quant_step_size_q6 as i32,
            nlsf_mu_q20,
            order,
        );

        /* Add rate for first stage */
        let icdf_row = &cb.cb1_icdf[((signal_type as usize >> 1) * n_vectors)..][..n_vectors];
        let prob_q8 = if ind1 == 0 {
            256 - icdf_row[ind1] as i32
        } else {
            icdf_row[ind1 - 1] as i32 - icdf_row[ind1] as i32
        };
        let bits_q7 = (8 << 7) - crate::silk::gains::lin2log(prob_q8);
        rd_q25[s] = rd_q25[s].wrapping_add(smulbb(bits_q7, nlsf_mu_q20 >> 2));
    }

    /* Find the lowest rate-distortion error */
    let mut best_index = 0usize;
    let mut best = i32::MAX;
    for (s, &rd) in rd_q25.iter().enumerate().take(n_survivors) {
        if rd < best {
            best = rd;
            best_index = s;
        }
    }

    nlsf_indices[0] = temp_indices1[best_index] as i8;
    nlsf_indices[1..=order].copy_from_slice(&temp_indices2[best_index][..order]);

    /* Decode */
    nlsf_decode(&mut p_nlsf_q15[..order], &nlsf_indices[..=order], cb);
    best
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::silk::tables::{NLSF_CB_NB_MB, NLSF_CB_WB};

    /// The quantizer's output must reconstruct exactly through the
    /// decoder's `nlsf_decode` (it *is* `nlsf_decode`'s output by
    /// construction) and the residual indices must stay within the
    /// escape-extended range for any input.
    #[test]
    fn quantized_nlsf_reconstructs_and_stays_in_range() {
        for cb in [&NLSF_CB_NB_MB, &NLSF_CB_WB] {
            let order = cb.order as usize;
            let mu = 3146; // SILK_FIX_CONST(0.003, 20)
            let mut rng_state = 0x2545_F491u32;
            let mut next = move || {
                rng_state ^= rng_state << 13;
                rng_state ^= rng_state >> 17;
                rng_state ^= rng_state << 5;
                rng_state
            };
            for case in 0..40 {
                // Random plausible NLSF input (Q15), increasing-ish.
                let mut nlsf_q15 = [0i16; MAX_LPC_ORDER];
                let mut acc = (next() % 4000) as i32;
                for v in nlsf_q15.iter_mut().take(order) {
                    acc += 500 + (next() % 3000) as i32;
                    *v = acc.min(32767) as i16;
                }

                let mut weights = [0i16; MAX_LPC_ORDER];
                nlsf_vq_weights_laroia(&mut weights, &nlsf_q15);

                let mut indices = [0i8; MAX_LPC_ORDER + 1];
                let mut quantized = nlsf_q15;
                let order1 = order + 1;
                let rd = nlsf_encode(
                    &mut indices[..order1],
                    &mut quantized,
                    cb,
                    &weights,
                    mu,
                    16,
                    (case % 3) as i8,
                );
                assert!(rd >= 0);

                // Stage-1 index in range; residuals within the escape
                // extension.
                assert!((indices[0] as usize) < cb.n_vectors as usize);
                for &r in &indices[1..=order] {
                    assert!(
                        (-NLSF_QUANT_MAX_AMPLITUDE_EXT..=NLSF_QUANT_MAX_AMPLITUDE_EXT)
                            .contains(&(r as i32))
                    );
                }

                // Reconstruction must be stable, monotonic, and equal to
                // what the decoder will compute from the same indices.
                let mut reconstructed = [0i16; MAX_LPC_ORDER];
                nlsf_decode(&mut reconstructed[..order], &indices[..=order], cb);
                assert_eq!(&reconstructed[..order], &quantized[..order]);
                for w in reconstructed[..order].windows(2) {
                    assert!(w[0] <= w[1], "NLSFs must be sorted: {reconstructed:?}");
                }
                assert!(reconstructed[0] >= 0);
            }
        }
    }

    /// A NLSF vector drawn exactly from a codebook entry must quantize to
    /// (near) its own stage-1 index.
    #[test]
    fn codebook_vectors_are_stable_fixed_points() {
        for cb in [&NLSF_CB_NB_MB, &NLSF_CB_WB] {
            let order = cb.order as usize;
            let mu = 3146;
            for v in 0..cb.n_vectors as usize {
                let element: Vec<i16> = cb.cb1_nlsf_q8[v * order..(v + 1) * order]
                    .iter()
                    .map(|&b| ((b as i32) << 7) as i16)
                    .collect();
                let mut nlsf_q15 = [0i16; MAX_LPC_ORDER];
                nlsf_q15[..order].copy_from_slice(&element);

                let mut weights = [0i16; MAX_LPC_ORDER];
                nlsf_vq_weights_laroia(&mut weights, &nlsf_q15);

                let mut indices = [0i8; MAX_LPC_ORDER + 1];
                let mut quantized = nlsf_q15;
                nlsf_encode(
                    &mut indices[..cb.order as usize + 1],
                    &mut quantized,
                    cb,
                    &weights,
                    mu,
                    4,
                    0,
                );
                // Stage-1 index: the same vector or a very close neighbor.
                // (Not guaranteed to be v itself: the trellis may prefer a
                // different cb1 with cheaper residuals — assert the stage-1
                // error was among the smallest instead by checking the
                // reconstruction stays close.)
                let mut dist = 0i64;
                for i in 0..order {
                    dist += ((quantized[i] as i32 - element[i] as i32).pow(2)) as i64;
                }
                let max_coef = cb.cb1_nlsf_q8[v * order..(v + 1) * order]
                    .iter()
                    .map(|&b| b as i32)
                    .max()
                    .unwrap();
                assert!(
                    dist < (order as i64) * (1 << 14) * (16 + max_coef as i64),
                    "cb vector {v} quantized too far: dist {dist}"
                );
            }
        }
    }

    #[test]
    fn laroia_weights_first_and_last() {
        // Hand-traced: d = 4, NLSFs [1000, 5000, 12000, 20000] (Q15);
        // 1<<(15+NLSF_W_Q) = 2^17 = 131072.
        // w0 = 131072/1000 + 131072/4000 = 131 + 32
        // w3 = 131072/(32768-20000) + 131072/8000 = 10 + 16
        let nlsf = [1000i16, 5000, 12000, 20000];
        let mut w = [0i16; 4];
        nlsf_vq_weights_laroia(&mut w, &nlsf);
        let big = 1i32 << (15 + NLSF_W_Q);
        assert_eq!(w[0], (big / 1000 + big / 4000) as i16);
        assert_eq!(w[1], (big / 4000 + big / 7000) as i16);
        assert_eq!(w[2], (big / 7000 + big / 8000) as i16);
        assert_eq!(w[3], (big / 12768 + big / 8000) as i16);
    }
}
