//! The reference noise-shaping quantizer — an exact port of
//! `silk/NSQ.c`'s `silk_NSQ`, `silk_noise_shape_quantizer`, and
//! `silk_nsq_scale_states` (the two-candidate rate/distortion variant
//! the reference runs at encoder complexity < 5), replacing the
//! foundation's simplified candidate-window loop when the shaping
//! analysis drives the quantizer.
//!
//! **STATUS: WIRED into the live encode path** via
//! `ChannelState::quantize_and_nsq` behind the per-frame `gainMult`
//! bisection rate control. The remaining reference variant is
//! `silk_NSQ_del_dec` (delayed decision across 2-4 quantization states,
//! `silk/NSQ_del_dec.c`), whose port was drafted and reverted once
//! (2026-09-27): the state-partial-copy at the pruning point and the
//! negative-index deferred writes need a fresh session with the
//! perceptual metrics in `tpt-av-cadence-test-utils::quality` for
//! validation.
//!
//! SOURCE: Xiph.Org libopus 1.5.2, `silk/NSQ.c`, `silk/NSQ.h`
//! (`silk_noise_shape_quantizer_short_prediction_c`,
//! `silk_NSQ_noise_shape_feedback_loop_c`), `silk/structs.h`
//! (`silk_nsq_state`), `silk/define.h` (BSD-3-Clause).
#![allow(dead_code)]

use crate::silk::decode_indices::{SideInfoIndices, MAX_LPC_ORDER, MAX_NB_SUBFR, TYPE_VOICED};
use crate::silk::excitation::QUANT_LEVEL_ADJUST_Q10;
use crate::silk::noise_shape::MAX_SHAPE_LPC_ORDER;
use crate::silk::sigproc::{
    add_sat32, lpc_analysis_filter, rand, rshift_round, sat16, smlawb, smlawt, smulwb, smulww,
};
use crate::silk::synthesis::{DecoderControl, FrameInfo, MAX_FRAME_LENGTH, MAX_SUB_FRAME_LENGTH};
use crate::silk::tables::QUANTIZATION_OFFSETS_Q10;

/// `NSQ_LPC_BUF_LENGTH` (`silk/define.h`): `MAX_LPC_ORDER`.
const NSQ_LPC_BUF_LENGTH: usize = MAX_LPC_ORDER;
/// `HARM_SHAPE_FIR_TAPS` (`silk/define.h`).
const HARM_SHAPE_FIR_TAPS: usize = 3;

/// `silk_nsq_state` (`silk/structs.h`), sized for this crate's 16 kHz cap.
#[derive(Clone)]
pub(crate) struct NsqState {
    /// Buffer for the quantized output signal (history + frame).
    pub xq: [i16; 2 * MAX_FRAME_LENGTH],
    /// Long-term shaped-noise state.
    pub s_ltp_shp_q14: [i32; 2 * MAX_FRAME_LENGTH],
    /// Short-term prediction state (Q14, scaled domain).
    pub s_lpc_q14: [i32; MAX_SUB_FRAME_LENGTH + NSQ_LPC_BUF_LENGTH],
    /// Shaping AR filter state.
    pub s_ar2_q14: [i32; MAX_SHAPE_LPC_ORDER],
    pub s_lf_ar_shp_q14: i32,
    pub s_diff_shp_q14: i32,
    pub lag_prev: i32,
    pub s_ltp_buf_idx: usize,
    pub s_ltp_shp_buf_idx: usize,
    pub rand_seed: i32,
    pub prev_gain_q16: i32,
    pub rewhite_flag: bool,
}

impl Default for NsqState {
    /// `silk_memset(&psNSQ->sNSQ, 0, ...)` from `silk_InitEncoder`: all
    /// zeros except `prev_gain_Q16 = 65536`... the reference zeroes the
    /// whole struct (`silk_InitEncoder` memsets `sNSQ`), and the scale
    /// states' `prev_gain_Q16 != 0` assert is satisfied because the
    /// first frame's gains are applied via the `Gains_Q16 != prev_gain`
    /// branch with prev 0 — matching the reference, which memsets and
    /// relies on the gain-adjust branch scaling everything from 0.
    fn default() -> Self {
        NsqState {
            xq: [0; 2 * MAX_FRAME_LENGTH],
            s_ltp_shp_q14: [0; 2 * MAX_FRAME_LENGTH],
            s_lpc_q14: [0; MAX_SUB_FRAME_LENGTH + NSQ_LPC_BUF_LENGTH],
            s_ar2_q14: [0; MAX_SHAPE_LPC_ORDER],
            s_lf_ar_shp_q14: 0,
            s_diff_shp_q14: 0,
            lag_prev: 0,
            s_ltp_buf_idx: 0,
            s_ltp_shp_buf_idx: 0,
            rand_seed: 0,
            prev_gain_q16: 0,
            rewhite_flag: false,
        }
    }
}

impl NsqState {
    /// Reset to the all-zero state (bandwidth switch / reset paths).
    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

/// `silk_NSQ_noise_shape_feedback_loop_c` (`silk/NSQ.h`): the even-order
/// allpass-style shaping feedback, Q11 in → Q12 out.
fn nsq_noise_shape_feedback_loop(
    data0: &[i32],
    data1: &mut [i32],
    coef: &[i16],
    order: usize,
) -> i32 {
    let mut tmp2 = data0[0];
    let mut tmp1 = data1[0];
    data1[0] = tmp2;

    let mut out = (order >> 1) as i32;
    out = smlawb(out, tmp2, coef[0] as i32);

    let mut j = 2;
    while j < order {
        tmp2 = data1[j - 1];
        data1[j - 1] = tmp1;
        out = smlawb(out, tmp1, coef[j - 1] as i32);
        tmp1 = data1[j];
        data1[j] = tmp2;
        out = smlawb(out, tmp2, coef[j] as i32);
        j += 2;
    }
    data1[order - 1] = tmp1;
    out = smlawb(out, tmp1, coef[order - 1] as i32);
    // Q11 -> Q12.
    out.wrapping_shl(1)
}

/// `silk_noise_shape_quantizer_short_prediction_c` (`silk/NSQ.h`): the
/// LPC prediction from the Q14 state, result Q10.
fn short_prediction(buf32: &[i32], pos: usize, coef16: &[i16], order: usize) -> i32 {
    let mut out = (order >> 1) as i32;
    out = smlawb(out, buf32[pos], coef16[0] as i32);
    for j in 1..order {
        out = smlawb(out, buf32[pos - j], coef16[j] as i32);
    }
    out
}

/// One frame's forward-NSQ result.
pub(crate) struct NsqResult {
    pub pulses: [i16; MAX_FRAME_LENGTH],
    pub xq: [i16; MAX_FRAME_LENGTH],
}

/// `silk_NSQ` — the exact subframe loop (rewhitening, scale states,
/// quantizer) driving [`noise_shape_quantizer`].
#[allow(clippy::too_many_arguments)]
pub(crate) fn nsq(
    nsq: &mut NsqState,
    indices: &SideInfoIndices,
    x16: &[i16],
    ctrl: &DecoderControl,
    frame: &FrameInfo,
    ar_q13: &[i16; MAX_NB_SUBFR * MAX_SHAPE_LPC_ORDER],
    lf_shp_q14: &[i32; MAX_NB_SUBFR],
    tilt_q14: &[i32; MAX_NB_SUBFR],
    harm_shape_gain_q14: &[i32; MAX_NB_SUBFR],
    lambda_q10: i32,
    ltp_scale_q14: i32,
    gains_q16: &[i32; MAX_NB_SUBFR],
) -> NsqResult {
    let frame_length = frame.frame_length();
    let subfr_length = frame.subfr_length;
    let ltp_mem_length = frame.ltp_mem_length;

    nsq.rand_seed = i32::from(indices.seed);

    // Set unvoiced lag to the previous one; overwritten for voiced.
    let mut lag = nsq.lag_prev.max(0) as usize;

    let offset_q10 = QUANTIZATION_OFFSETS_Q10[(indices.signal_type >> 1) as usize]
        [indices.quant_offset_type as usize] as i32;
    let lsf_interpolation_flag = i32::from(indices.nlsf_interp_coef_q2 < 4);

    let mut s_ltp = vec![0i16; ltp_mem_length + frame_length];
    let mut s_ltp_q15 = vec![0i32; ltp_mem_length + frame_length];
    let mut x_sc_q10 = vec![0i32; subfr_length];
    let mut pulses = [0i16; MAX_FRAME_LENGTH];

    nsq.s_ltp_shp_buf_idx = ltp_mem_length;
    nsq.s_ltp_buf_idx = ltp_mem_length;

    for k in 0..frame.nb_subfr {
        let a_q12_sel = (k >> 1) | (1 - lsf_interpolation_flag as usize);
        let a_q12 = &ctrl.pred_coef_q12[a_q12_sel][..frame.lpc_order];
        let b_q14 = &ctrl.ltp_coef_q14[k * 5..k * 5 + 5];
        let ar_shp_q13 = &ar_q13[k * MAX_SHAPE_LPC_ORDER..][..MAX_SHAPE_LPC_ORDER];

        // HarmShapeFIRPacked_Q14 packs (gain>>2, gain>>1) as two 16-bit
        // halves of one i32.
        let harm = harm_shape_gain_q14[k];
        let mut harm_fir_packed_q14 = harm >> 2;
        harm_fir_packed_q14 |= (harm >> 1).wrapping_shl(16);

        nsq.rewhite_flag = false;
        if indices.signal_type == TYPE_VOICED {
            lag = ctrl.pitch_l[k] as usize;

            // Re-whiten with new A coefs every other subframe while the
            // NLSFs interpolate (every subframe when they do not).
            if (k & (3 - ((lsf_interpolation_flag as usize) << 1))) == 0 {
                let start_idx = ltp_mem_length - lag - frame.lpc_order - 5 / 2;
                debug_assert!(start_idx > 0);
                lpc_analysis_filter(
                    &mut s_ltp[start_idx..],
                    &nsq.xq[start_idx + k * subfr_length..],
                    a_q12,
                    frame.lpc_order,
                );
                nsq.rewhite_flag = true;
                nsq.s_ltp_buf_idx = ltp_mem_length;
            }
        }

        scale_states(
            nsq,
            &x16[k * subfr_length..],
            &mut x_sc_q10,
            &s_ltp,
            &mut s_ltp_q15,
            k,
            ltp_scale_q14,
            gains_q16,
            &ctrl.pitch_l,
            indices.signal_type,
            subfr_length,
            ltp_mem_length,
        );

        let xq_pos = ltp_mem_length + k * subfr_length;
        // The quantizer only WRITES xq (never reads it), so a local
        // buffer is equivalent to the reference's pointer into NSQ->xq.
        let mut xq_slice = vec![0i16; subfr_length];
        noise_shape_quantizer(
            nsq,
            indices.signal_type,
            &x_sc_q10,
            &mut pulses[k * subfr_length..(k + 1) * subfr_length],
            &mut xq_slice,
            &mut s_ltp_q15,
            a_q12,
            b_q14,
            ar_shp_q13,
            lag,
            harm_fir_packed_q14,
            tilt_q14[k],
            lf_shp_q14[k],
            gains_q16[k],
            lambda_q10,
            offset_q10,
            subfr_length,
            frame.lpc_order,
        );
        nsq.xq[xq_pos..xq_pos + subfr_length].copy_from_slice(&xq_slice);
    }

    // The frame's quantized output lives at xq[ltp_mem..ltp_mem+frame].
    let mut xq_out = [0i16; MAX_FRAME_LENGTH];
    xq_out[..frame_length].copy_from_slice(&nsq.xq[ltp_mem_length..ltp_mem_length + frame_length]);

    // Update lagPrev for the next frame, and shift the history buffers.
    nsq.lag_prev = ctrl.pitch_l[frame.nb_subfr - 1];
    nsq.xq.copy_within(frame_length.., 0);
    nsq.s_ltp_shp_q14.copy_within(frame_length.., 0);

    NsqResult { pulses, xq: xq_out }
}

/// `silk_noise_shape_quantizer` — the exact two-candidate
/// rate/distortion quantizer with the shaping error-feedback loop.
#[allow(clippy::too_many_arguments)]
fn noise_shape_quantizer(
    nsq: &mut NsqState,
    signal_type: i8,
    x_sc_q10: &[i32],
    pulses: &mut [i16],
    xq: &mut [i16],
    s_ltp_q15: &mut [i32],
    a_q12: &[i16],
    b_q14: &[i16],
    ar_shp_q13: &[i16],
    lag: usize,
    harm_shape_fir_packed_q14: i32,
    tilt_q14: i32,
    lf_shp_q14: i32,
    gain_q16: i32,
    lambda_q10: i32,
    offset_q10: i32,
    length: usize,
    predict_lpc_order: usize,
) {
    let shp_lag_base = nsq.s_ltp_shp_buf_idx as i64 - lag as i64 + HARM_SHAPE_FIR_TAPS as i64 / 2;
    let pred_lag_base = nsq.s_ltp_buf_idx as i64 - lag as i64 + 5 / 2;
    let gain_q10 = gain_q16 >> 6;

    let mut lpc_pos = NSQ_LPC_BUF_LENGTH - 1;
    let mut shp_lag_ptr = shp_lag_base;
    let mut pred_lag_ptr = pred_lag_base;

    for i in 0..length {
        // Generate dither.
        nsq.rand_seed = rand(nsq.rand_seed);

        // Short-term prediction (Q10).
        let lpc_pred_q10 = short_prediction(&nsq.s_lpc_q14, lpc_pos, a_q12, predict_lpc_order);

        // Long-term prediction (Q13).
        let ltp_pred_q13 = if signal_type == TYPE_VOICED {
            // +2 avoids the bias from SMLAWB's round-to-negative-inf.
            let mut p = 2i32;
            p = smlawb(p, s_ltp_q15[pred_lag_ptr as usize], b_q14[0] as i32);
            p = smlawb(p, s_ltp_q15[pred_lag_ptr as usize - 1], b_q14[1] as i32);
            p = smlawb(p, s_ltp_q15[pred_lag_ptr as usize - 2], b_q14[2] as i32);
            p = smlawb(p, s_ltp_q15[pred_lag_ptr as usize - 3], b_q14[3] as i32);
            p = smlawb(p, s_ltp_q15[pred_lag_ptr as usize - 4], b_q14[4] as i32);
            pred_lag_ptr += 1;
            p
        } else {
            0
        };

        // Noise shape feedback (Q12).
        let mut n_ar_q12 = {
            // The reference walks a moving window over sDiff_shp_Q14
            // (data0 = &sDiff_shp_Q14[lpc_pos-equivalent current]) —
            // data0[0] is the latest diff sample; sAR2 holds the rest.
            let data0 = [nsq.s_diff_shp_q14];
            nsq_noise_shape_feedback_loop(&data0, &mut nsq.s_ar2_q14, ar_shp_q13, ar_shp_q13.len())
        };
        n_ar_q12 = smlawb(n_ar_q12, nsq.s_lf_ar_shp_q14, tilt_q14);

        let mut n_lf_q12 = smulwb(nsq.s_ltp_shp_q14[nsq.s_ltp_shp_buf_idx - 1], lf_shp_q14);
        n_lf_q12 = smlawt(n_lf_q12, nsq.s_lf_ar_shp_q14, lf_shp_q14);

        debug_assert!(lag > 0 || signal_type != TYPE_VOICED);

        // Combine prediction and shaping signals.
        let mut tmp1 = (lpc_pred_q10 << 2).wrapping_sub(n_ar_q12); // Q12
        tmp1 = tmp1.wrapping_sub(n_lf_q12); // Q12
        if lag > 0 {
            let base = shp_lag_ptr as usize;
            let n_ltp_q13_a = smulwb(
                add_sat32(nsq.s_ltp_shp_q14[base], nsq.s_ltp_shp_q14[base - 2]),
                harm_shape_fir_packed_q14,
            );
            let mut n_ltp_q13 = smlawt(
                n_ltp_q13_a,
                nsq.s_ltp_shp_q14[base - 1],
                harm_shape_fir_packed_q14,
            );
            n_ltp_q13 = n_ltp_q13.wrapping_shl(1);
            shp_lag_ptr += 1;

            let tmp2 = ltp_pred_q13 - n_ltp_q13; // Q13
            tmp1 = tmp2.wrapping_add(tmp1.wrapping_shl(1)); // Q13
            tmp1 = rshift_round(tmp1, 3); // Q10
        } else {
            tmp1 = rshift_round(tmp1, 2); // Q10
        }

        let mut r_q10 = x_sc_q10[i] - tmp1; // residual error Q10

        // Flip sign depending on dither.
        if nsq.rand_seed < 0 {
            r_q10 = -r_q10;
        }
        r_q10 = r_q10.clamp(-(31 << 10), 30 << 10);

        // Two quantization candidates, rate-distortion weighted.
        let q1_q10_raw = r_q10 - offset_q10;
        let mut q1_q0 = q1_q10_raw >> 10;
        if lambda_q10 > 2048 {
            // For aggressive RDO the bias becomes more than one pulse.
            let rdo_offset = lambda_q10 / 2 - 512;
            if q1_q10_raw > rdo_offset {
                q1_q0 = (q1_q10_raw - rdo_offset) >> 10;
            } else if q1_q10_raw < -rdo_offset {
                q1_q0 = (q1_q10_raw + rdo_offset) >> 10;
            } else if q1_q10_raw < 0 {
                q1_q0 = -1;
            } else {
                q1_q0 = 0;
            }
        }
        let mut q1_q10;
        let q2_q10;
        let mut rd1_q20;
        let mut rd2_q20;
        if q1_q0 > 0 {
            q1_q10 = (q1_q0 << 10) - QUANT_LEVEL_ADJUST_Q10 + offset_q10;
            q2_q10 = q1_q10 + 1024;
            rd1_q20 = q1_q10 * lambda_q10;
            rd2_q20 = q2_q10 * lambda_q10;
        } else if q1_q0 == 0 {
            q1_q10 = offset_q10;
            q2_q10 = q1_q10 + 1024 - QUANT_LEVEL_ADJUST_Q10;
            rd1_q20 = q1_q10 * lambda_q10;
            rd2_q20 = q2_q10 * lambda_q10;
        } else if q1_q0 == -1 {
            q2_q10 = offset_q10;
            q1_q10 = q2_q10 - (1024 - QUANT_LEVEL_ADJUST_Q10);
            rd1_q20 = (-q1_q10) * lambda_q10;
            rd2_q20 = q2_q10 * lambda_q10;
        } else {
            q1_q10 = ((q1_q0 << 10) + QUANT_LEVEL_ADJUST_Q10) + offset_q10;
            q2_q10 = q1_q10 + 1024;
            rd1_q20 = (-q1_q10) * lambda_q10;
            rd2_q20 = (-q2_q10) * lambda_q10;
        }
        let mut rr_q10 = r_q10 - q1_q10;
        rd1_q20 += rr_q10 * rr_q10;
        rr_q10 = r_q10 - q2_q10;
        rd2_q20 += rr_q10 * rr_q10;

        if rd2_q20 < rd1_q20 {
            q1_q10 = q2_q10;
        }

        pulses[i] = rshift_round(q1_q10, 10) as i16;

        // Excitation (Q10 → Q14).
        let mut exc_q14 = q1_q10 << 4;
        if nsq.rand_seed < 0 {
            exc_q14 = -exc_q14;
        }

        // Add predictions.
        let lpc_exc_q14 = exc_q14 + (ltp_pred_q13 << 1); // Q14
        let xq_q14 = lpc_exc_q14.wrapping_add(lpc_pred_q10 << 4); // Q14

        // Scale back to the normal level before saving.
        xq[i] = sat16(rshift_round(smulww(xq_q14, gain_q10), 8));

        // Update states.
        lpc_pos += 1;
        nsq.s_lpc_q14[lpc_pos] = xq_q14;
        nsq.s_diff_shp_q14 = xq_q14.wrapping_sub(x_sc_q10[i].wrapping_shl(4));
        let s_lf_ar_shp_q14 = nsq.s_diff_shp_q14.wrapping_sub(n_ar_q12 << 2);
        nsq.s_lf_ar_shp_q14 = s_lf_ar_shp_q14;

        nsq.s_ltp_shp_q14[nsq.s_ltp_shp_buf_idx] = s_lf_ar_shp_q14.wrapping_sub(n_lf_q12 << 2);
        s_ltp_q15[nsq.s_ltp_buf_idx] = lpc_exc_q14 << 1;
        nsq.s_ltp_shp_buf_idx += 1;
        nsq.s_ltp_buf_idx += 1;

        // Make the dither depend on the quantized signal.
        nsq.rand_seed = nsq.rand_seed.wrapping_add(i32::from(pulses[i]));
    }

    // Update the LPC synth buffer (shift by the frame length).
    nsq.s_lpc_q14
        .copy_within(length..length + NSQ_LPC_BUF_LENGTH, 0);
}

/// `silk_nsq_scale_states` — per-subframe input scaling with 1/Gain,
/// rewhitened-LTP scaling, and gain-change adjustments across all
/// shaping states.
#[allow(clippy::too_many_arguments)]
fn scale_states(
    nsq: &mut NsqState,
    x16: &[i16],
    x_sc_q10: &mut [i32],
    s_ltp: &[i16],
    s_ltp_q15: &mut [i32],
    subfr: usize,
    ltp_scale_q14: i32,
    gains_q16: &[i32; MAX_NB_SUBFR],
    pitch_l: &[i32; MAX_NB_SUBFR],
    signal_type: i8,
    subfr_length: usize,
    ltp_mem_length: usize,
) {
    let lag = pitch_l[subfr] as usize;
    let gain_q16 = gains_q16[subfr];
    let inv_gain_q31 = crate::silk::nlsf::inverse32_varq(gain_q16.max(1), 47);

    // Scale input.
    let inv_gain_q26 = rshift_round(inv_gain_q31, 5);
    for i in 0..subfr_length {
        x_sc_q10[i] = smulww(i32::from(x16[i]), inv_gain_q26);
    }

    // After rewhitening the LTP state is un-scaled: scale with the full
    // inverse gain (with LTP downscaling on subframe 0).
    if nsq.rewhite_flag {
        let mut inv = inv_gain_q31;
        if subfr == 0 {
            inv = smulwb(inv, ltp_scale_q14) << 2;
        }
        for i in nsq.s_ltp_buf_idx - lag - 5 / 2..nsq.s_ltp_buf_idx {
            s_ltp_q15[i] = smulwb(inv, i32::from(s_ltp[i]));
        }
    }

    // Adjust for changing gain.
    if gain_q16 != nsq.prev_gain_q16 {
        let gain_adj_q16 = crate::silk::sigproc::div32_varq(nsq.prev_gain_q16, gain_q16, 16);

        // Scale the long-term shaping state.
        for i in nsq.s_ltp_shp_buf_idx - ltp_mem_length..nsq.s_ltp_shp_buf_idx {
            nsq.s_ltp_shp_q14[i] = smulww(gain_adj_q16, nsq.s_ltp_shp_q14[i]);
        }

        // Scale the long-term prediction state.
        if signal_type == TYPE_VOICED && !nsq.rewhite_flag {
            for st in &mut s_ltp_q15[nsq.s_ltp_buf_idx - lag - 2..nsq.s_ltp_buf_idx] {
                *st = smulww(gain_adj_q16, *st);
            }
        }

        nsq.s_lf_ar_shp_q14 = smulww(gain_adj_q16, nsq.s_lf_ar_shp_q14);
        nsq.s_diff_shp_q14 = smulww(gain_adj_q16, nsq.s_diff_shp_q14);

        for i in 0..NSQ_LPC_BUF_LENGTH {
            nsq.s_lpc_q14[i] = smulww(gain_adj_q16, nsq.s_lpc_q14[i]);
        }
        for i in 0..MAX_SHAPE_LPC_ORDER {
            nsq.s_ar2_q14[i] = smulww(gain_adj_q16, nsq.s_ar2_q14[i]);
        }

        nsq.prev_gain_q16 = gain_q16;
    }
}
