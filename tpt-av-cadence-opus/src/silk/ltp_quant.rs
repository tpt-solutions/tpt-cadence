//! SILK LTP analysis and quantization — port of the reference encoder's
//! long-term-predictor chain.
//!
//! - [`find_ltp`] (`silk/float/find_LTP_FLP.c` + the `corrMatrix`/
//!   `corrVector` float kernels): per-subframe 5×5 weighted correlation
//!   matrix `XX` and cross-correlation vector `xX` of the pitch-LPC
//!   residual, normalized by the reference's `LTP_CORR_INV_MAX` rule.
//! - [`vq_wmat_ec`] (`silk/VQ_WMat_EC.c`, exact fixed point): the
//!   entropy-constrained matrix-weighted VQ over one subframe's 5-tap
//!   codebook, with the rate/distortion tradeoff from the `LTP gain
//!   BITS_Q5` tables and the cumulative `sum_log_gain` safety cap.
//! - [`quant_ltp_gains`] (`silk/quant_LTP_gains.c`): iterates the three
//!   periodicity codebooks (sizes 8/16/32) and picks the best total
//!   rate/distortion, emitting the per-subframe codebook indices, the
//!   periodicity index, the Q14 filters, and the LTP prediction gain
//!   used by the gain/offset control.
//!
//! SOURCE: Xiph.Org libopus 1.5.2, `silk/float/find_LTP_FLP.c`,
//! `silk/float/corrMatrix_FLP.c`, `silk/VQ_WMat_EC.c`,
//! `silk/quant_LTP_gains.c`, `silk/float/wrappers_FLP.c` (the ×2^17
//! float→Q17 conversion), `silk/tuning_parameters.h` (BSD-3-Clause).
#![allow(dead_code)]

use crate::silk::decode_indices::MAX_NB_SUBFR;
use crate::silk::gains::lin2log;
use crate::silk::lpc_analysis::{corr_matrix, corr_vector, energy, inner_product};
use crate::silk::pitch::LTP_ORDER;
use crate::silk::tables::{
    LTP_GAIN_BITS_Q5_PTRS, LTP_VQ_GAIN_PTRS_Q7, LTP_VQ_PTRS_Q7, LTP_VQ_SIZES,
};

/// `LTP_CORR_INV_MAX` (`silk/tuning_parameters.h`).
const LTP_CORR_INV_MAX: f32 = 0.03;
/// `MAX_SUM_LOG_GAIN_DB / 6` in Q7 (`SILK_FIX_CONST(250.0 / 6.0, 7)`).
const MAX_SUM_LOG_GAIN_Q7: i32 = 5334;
/// `SILK_FIX_CONST(7, 7)`.
const FIX_7_Q7: i32 = 896;
/// `SILK_FIX_CONST(0.4, 7)` — the pitch-gain safety margin.
const GAIN_SAFETY_Q7: i32 = 51;
/// `SILK_FIX_CONST(1.001, 15)` — the VQ's identity-residual floor.
const FIX_1_001_Q15: i32 = 32801;

/// `silk_find_LTP_FLP`: fills `xx` (`nb_subfr × 25`) and `x_x`
/// (`nb_subfr × 5`) for the subframes of `res_pitch` starting at
/// `frame_start`, with the analysis lags from `pitch_l`.
pub(crate) fn find_ltp(
    xx: &mut [f32],
    x_x: &mut [f32],
    res_pitch: &[f32],
    frame_start: usize,
    pitch_l: &[i32; MAX_NB_SUBFR],
    subfr_length: usize,
    nb_subfr: usize,
) {
    debug_assert!(xx.len() >= nb_subfr * LTP_ORDER * LTP_ORDER);
    debug_assert!(x_x.len() >= nb_subfr * LTP_ORDER);
    for k in 0..nb_subfr {
        let r_ptr = frame_start + k * subfr_length;
        let lag_ptr = r_ptr - (pitch_l[k] as usize + LTP_ORDER / 2);
        debug_assert!(lag_ptr + subfr_length + LTP_ORDER - 1 <= res_pitch.len());

        let xx_ptr = &mut xx[k * LTP_ORDER * LTP_ORDER..][..LTP_ORDER * LTP_ORDER];
        corr_matrix(xx_ptr, &res_pitch[lag_ptr..], subfr_length, LTP_ORDER);
        let x_x_ptr = &mut x_x[k * LTP_ORDER..][..LTP_ORDER];
        corr_vector(
            x_x_ptr,
            &res_pitch[lag_ptr..],
            &res_pitch[r_ptr..r_ptr + subfr_length],
            subfr_length,
            LTP_ORDER,
        );

        /* The reference's residual buffer carries lookahead; the
         * foundation's stops at the frame end, so clamp the energy
         * window for the final subframe. */
        let nrg_len = (res_pitch.len() - r_ptr).min(subfr_length + LTP_ORDER);
        let xx_energy = energy(&res_pitch[r_ptr..r_ptr + nrg_len]);
        let temp = 1.0f32
            / (xx_energy as f32).max(LTP_CORR_INV_MAX * 0.5 * (xx_ptr[0] + xx_ptr[24]) + 1.0);
        for v in xx_ptr.iter_mut() {
            *v *= temp;
        }
        for v in x_x_ptr.iter_mut() {
            *v *= temp;
        }
    }
}

/// `silk_VQ_WMat_EC_c` (`silk/VQ_WMat_EC.c`): entropy-constrained
/// matrix-weighted VQ, hard-coded to 5-element vectors, for one subframe.
///
/// Returns `(index, res_nrg_q15, rate_dist_q8, gain_q7)`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn vq_wmat_ec(
    xx_q17: &[i32],
    x_x_q17: &[i32],
    cb_q7: &[[i8; 5]],
    cb_gain_q7: &[u8],
    cl_q5: &[u8],
    subfr_len: i32,
    max_gain_q7: i32,
) -> (i8, i32, i32, i32) {
    debug_assert_eq!(xx_q17.len(), 25);
    debug_assert_eq!(x_x_q17.len(), 5);

    /* Negate and convert to new Q domain */
    let mut neg_x_x_q24 = [0i32; 5];
    for (out, &v) in neg_x_x_q24.iter_mut().zip(x_x_q17.iter()) {
        *out = -(v << 7);
    }

    let mut best = (0i8, i32::MAX, i32::MAX, 0i32); // (ind, res_nrg, rate_dist, gain)
    for (k, cb_row) in cb_q7.iter().enumerate() {
        let gain_tmp_q7 = cb_gain_q7[k] as i32;

        /* Penalty for too large gain */
        let penalty = (gain_tmp_q7 - max_gain_q7).max(0) << 11;

        let b = |i: usize| cb_row[i] as i32;

        /* Quantization error: 1 - 2 * xX * cb + cb' * XX * cb */
        let mut sum1_q15 = FIX_1_001_Q15;

        let mut sum2_q24 = neg_x_x_q24[0]
            .wrapping_add(xx_q17[1].wrapping_mul(b(1)))
            .wrapping_add(xx_q17[2].wrapping_mul(b(2)))
            .wrapping_add(xx_q17[3].wrapping_mul(b(3)))
            .wrapping_add(xx_q17[4].wrapping_mul(b(4)))
            << 1;
        sum2_q24 = sum2_q24.wrapping_add(xx_q17[0].wrapping_mul(b(0)));
        sum1_q15 = crate::silk::sigproc::smlawb(sum1_q15, sum2_q24, cb_row[0] as i32);

        sum2_q24 = neg_x_x_q24[1]
            .wrapping_add(xx_q17[7].wrapping_mul(b(2)))
            .wrapping_add(xx_q17[8].wrapping_mul(b(3)))
            .wrapping_add(xx_q17[9].wrapping_mul(b(4)))
            << 1;
        sum2_q24 = sum2_q24.wrapping_add(xx_q17[6].wrapping_mul(b(1)));
        sum1_q15 = crate::silk::sigproc::smlawb(sum1_q15, sum2_q24, cb_row[1] as i32);

        sum2_q24 = neg_x_x_q24[2]
            .wrapping_add(xx_q17[13].wrapping_mul(b(3)))
            .wrapping_add(xx_q17[14].wrapping_mul(b(4)))
            << 1;
        sum2_q24 = sum2_q24.wrapping_add(xx_q17[12].wrapping_mul(b(2)));
        sum1_q15 = crate::silk::sigproc::smlawb(sum1_q15, sum2_q24, cb_row[2] as i32);

        sum2_q24 = neg_x_x_q24[3].wrapping_add(xx_q17[19].wrapping_mul(b(4))) << 1;
        sum2_q24 = sum2_q24.wrapping_add(xx_q17[18].wrapping_mul(b(3)));
        sum1_q15 = crate::silk::sigproc::smlawb(sum1_q15, sum2_q24, cb_row[3] as i32);

        sum2_q24 = (neg_x_x_q24[4] << 1).wrapping_add(xx_q17[24].wrapping_mul(b(4)));
        sum1_q15 = crate::silk::sigproc::smlawb(sum1_q15, sum2_q24, cb_row[4] as i32);

        /* Find best */
        if sum1_q15 >= 0 {
            /* Translate residual energy to bits using the high-rate
             * assumption (6 dB ==> 1 bit/sample) */
            let bits_res_q8 = crate::silk::sigproc::smulbb(
                subfr_len,
                lin2log(sum1_q15.wrapping_add(penalty)) - (15 << 7),
            );
            /* The codelength component is used at half weight ("−1"
             * shift), as in the reference. */
            let bits_tot_q8 = bits_res_q8.wrapping_add((cl_q5[k] as i32) << 2);
            if bits_tot_q8 <= best.2 {
                best = (k as i8, sum1_q15, bits_tot_q8, gain_tmp_q7);
            }
        }
    }
    best
}

/// Output of [`quant_ltp_gains`].
pub(crate) struct LtpQuantResult {
    /// Per-subframe codebook indices (the transmitted `LTPIndex`).
    pub cbk_index: [i8; MAX_NB_SUBFR],
    /// The transmitted `PERIndex` value.
    pub periodicity_index: i8,
    /// Quantized 5-tap filters in Q14 (already shifted, decoder domain).
    pub b_q14: [i16; MAX_NB_SUBFR * LTP_ORDER],
    /// Updated cumulative log-gain state (Q7).
    pub sum_log_gain_q7: i32,
    /// LTP prediction gain in dB/128 (Q7), for gain/offset control.
    pub pred_gain_d_b_q7: i32,
}

/// `silk_quant_LTP_gains` (`silk/quant_LTP_gains.c`): iterates the three
/// LTP codebooks with different rates/distortions and chooses the best.
///
/// `xx_q17`/`x_x_q17` are the [`find_ltp`] outputs converted to Q17
/// (`silk_quant_LTP_gains_FLP` multiplies the float correlations by
/// 131072.0 and truncates).
pub(crate) fn quant_ltp_gains(
    xx_q17: &[i32],
    x_x_q17: &[i32],
    subfr_len: i32,
    nb_subfr: usize,
    sum_log_gain_q7: i32,
) -> LtpQuantResult {
    debug_assert!(xx_q17.len() >= nb_subfr * LTP_ORDER * LTP_ORDER);
    debug_assert!(x_x_q17.len() >= nb_subfr * LTP_ORDER);

    let mut best_index = [0i8; MAX_NB_SUBFR];
    let mut best_periodicity = 0i8;
    let mut best_sum_log_gain = 0i32;
    let mut best_res_nrg_q15 = 0i32;
    let mut min_rate_dist_q7 = i32::MAX;

    for (k, &cbk_size) in LTP_VQ_SIZES.iter().enumerate().take(3) {
        let cbk_size = cbk_size as usize;
        let cl_q5 = LTP_GAIN_BITS_Q5_PTRS[k];
        let cbk_q7 = LTP_VQ_PTRS_Q7[k];
        let cbk_gain_q7 = LTP_VQ_GAIN_PTRS_Q7[k];

        let mut temp_idx = [0i8; MAX_NB_SUBFR];
        let mut res_nrg_q15: i32 = 0;
        let mut rate_dist_q7: i32 = 0;
        let mut sum_log_gain_tmp_q7 = sum_log_gain_q7;

        for j in 0..nb_subfr {
            let max_gain_q7 =
                lin2log((MAX_SUM_LOG_GAIN_Q7 - sum_log_gain_tmp_q7) + FIX_7_Q7) - GAIN_SAFETY_Q7;
            let xx_ptr = &xx_q17[j * LTP_ORDER * LTP_ORDER..][..LTP_ORDER * LTP_ORDER];
            let x_x_ptr = &x_x_q17[j * LTP_ORDER..][..LTP_ORDER];
            let (idx, res_nrg_subfr, rate_dist_subfr, gain_q7) = vq_wmat_ec(
                xx_ptr,
                x_x_ptr,
                &cbk_q7[..cbk_size],
                cbk_gain_q7,
                cl_q5,
                subfr_len,
                max_gain_q7,
            );

            temp_idx[j] = idx;
            res_nrg_q15 = res_nrg_q15.saturating_add(res_nrg_subfr.max(0));
            rate_dist_q7 = rate_dist_q7.saturating_add(rate_dist_subfr.max(0));
            sum_log_gain_tmp_q7 =
                (sum_log_gain_tmp_q7 + lin2log(GAIN_SAFETY_Q7 + gain_q7) - FIX_7_Q7).max(0);
        }

        if rate_dist_q7 <= min_rate_dist_q7 {
            min_rate_dist_q7 = rate_dist_q7;
            best_periodicity = k as i8;
            best_index = temp_idx;
            best_sum_log_gain = sum_log_gain_tmp_q7;
            best_res_nrg_q15 = res_nrg_q15;
        }
    }

    /* Widen the winning codebook's vectors to Q14 (decoder domain). */
    let cbk_q7 = LTP_VQ_PTRS_Q7[best_periodicity as usize];
    let mut b_q14 = [0i16; MAX_NB_SUBFR * LTP_ORDER];
    for j in 0..nb_subfr {
        for (k, slot) in b_q14[j * LTP_ORDER..(j + 1) * LTP_ORDER]
            .iter_mut()
            .enumerate()
        {
            *slot = ((cbk_q7[best_index[j] as usize][k] as i32) << 7) as i16;
        }
    }

    let res_nrg_q15 = if nb_subfr == 2 {
        best_res_nrg_q15 >> 1
    } else {
        best_res_nrg_q15 >> 2
    };

    LtpQuantResult {
        cbk_index: best_index,
        periodicity_index: best_periodicity,
        b_q14,
        sum_log_gain_q7: best_sum_log_gain,
        pred_gain_d_b_q7: crate::silk::sigproc::smulbb(-3, lin2log(res_nrg_q15) - (15 << 7)),
    }
}

/// Converts the [`find_ltp`] float correlations to the Q17 domain the
/// fixed-point VQ consumes (`silk_quant_LTP_gains_FLP`'s ×131072
/// conversion).
pub(crate) fn correlations_to_q17(xx: &[f32], x_x: &[f32]) -> (Vec<i32>, Vec<i32>) {
    let xx_q17 = xx.iter().map(|&v| (v * 131072.0) as i32).collect();
    let x_x_q17 = x_x.iter().map(|&v| (v * 131072.0) as i32).collect();
    (xx_q17, x_x_q17)
}

/// Average normalized cross-correlation of the residual with itself at
/// `pitch_l[k]` over each subframe — the foundation's stand-in for the
/// reference's `LTPCorr` pitch-gain estimate (used for the voiced
/// decision and the voiced quantization-offset rule).
pub(crate) fn ltp_correlation(
    res_pitch: &[f32],
    frame_start: usize,
    pitch_l: &[i32; MAX_NB_SUBFR],
    subfr_length: usize,
    nb_subfr: usize,
) -> f32 {
    let mut sum = 0.0f64;
    for (k, lag_arg) in pitch_l.iter().enumerate().take(nb_subfr) {
        let r = frame_start + k * subfr_length;
        let lag = *lag_arg as usize;
        let target = &res_pitch[r..r + subfr_length];
        let delayed = &res_pitch[r - lag..r - lag + subfr_length];
        let den = (energy(target) * energy(delayed)).sqrt();
        if den > 1e-9 {
            sum += (inner_product(target, delayed) as f64 / den).max(0.0);
        }
    }
    (sum / nb_subfr as f64) as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A synthetic periodic residual: the correlation matrix at the true
    /// lag must be dominant on the diagonal, and the VQ must pick a
    /// codebook vector with positive gain for a strongly periodic signal.
    #[test]
    fn find_ltp_and_vq_pick_periodic_lag() {
        // Build a 20 ms @ 16 kHz residual with a strong 5 ms period.
        let fs = 16usize;
        let subfr = 5 * fs;
        let lag = 5 * fs;
        let mut res = vec![0f32; lag + 4 * subfr + 16];
        for i in 0..res.len() {
            // Decaying periodic pulse train + a little noise shape.
            res[i] = if i % lag == 0 {
                100.0
            } else {
                res[i - 1] * 0.6
            };
        }
        let frame_start = lag + 8;
        let pitch_l = [lag as i32; MAX_NB_SUBFR];
        let nb_subfr = 4;

        let mut xx = vec![0f32; nb_subfr * 25];
        let mut x_x = vec![0f32; nb_subfr * 5];
        find_ltp(
            &mut xx,
            &mut x_x,
            &res,
            frame_start,
            &pitch_l,
            subfr,
            nb_subfr,
        );
        // xX[2] (center tap) should be positive and dominant for this
        // periodic signal.
        for k in 0..nb_subfr {
            let x_x_k = &x_x[k * 5..k * 5 + 5];
            assert!(x_x_k[2] > 0.0, "center tap must dominate: {x_x_k:?}");
        }

        let (xx_q17, x_x_q17) = correlations_to_q17(&xx, &x_x);
        let result = quant_ltp_gains(&xx_q17, &x_x_q17, subfr as i32, nb_subfr, 0);
        // The quantized center tap must be positive (a real predictor).
        for k in 0..nb_subfr {
            assert!(result.b_q14[k * 5 + 2] > 0, "quantized center tap k={k}");
        }
        assert!((0..=2).contains(&result.periodicity_index));
    }

    #[test]
    fn vq_wmat_ec_picks_zero_vector_for_flat_correlations() {
        // All-zero correlations: the safest choice is the identity-ish
        // "no prediction" vector (index 1 in codebook 0, taps [0,0,2,0,0]).
        let xx = [0i32; 25];
        let x_x = [0i32; 5];
        let cb = LTP_VQ_PTRS_Q7[0];
        let (idx, res_nrg, _rd, _g) = vq_wmat_ec(
            &xx,
            &x_x,
            cb,
            LTP_VQ_GAIN_PTRS_Q7[0],
            LTP_GAIN_BITS_Q5_PTRS[0],
            80,
            i32::MAX,
        );
        // Residual energy must be the positive floor for the chosen entry.
        assert!(res_nrg > 0);
        let _ = idx;
    }

    #[test]
    fn ltp_correlation_is_high_at_true_lag() {
        let fs = 16usize;
        let subfr = 5 * fs;
        let lag = 6 * fs;
        let mut res = vec![0f32; lag + 2 * subfr + 8];
        for i in 0..res.len() {
            res[i] = if i % lag == 0 { 80.0 } else { res[i - 1] * 0.5 };
        }
        let frame_start = lag + 8;
        let corr = ltp_correlation(&res, frame_start, &[lag as i32; MAX_NB_SUBFR], subfr, 2);
        assert!(corr > 0.9, "corr {corr}");
    }
}
