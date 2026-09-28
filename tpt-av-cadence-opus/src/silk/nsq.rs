//! SILK forward noise-shaping quantization — the closed-loop encoder core.
//!
//! This is the encoder-side counterpart of [`super::synthesis::decode_core`]
//! (the decoder's inverse NSQ). Instead of the reference's multi-state
//! delayed-decision NSQ with noise-shaping feedback (a quality refinement
//! this foundation does not carry yet — see
//! [`crate::silk::noise_shape`] for the measurements), it runs a
//! **closed-loop analysis-by-synthesis over the decoder's own arithmetic**:
//! for each sample, a small set of candidate quantization indices is pushed
//! through the *exact* integer operations the decoder will apply — the
//! seed-dithered excitation reconstruction (offset, `QUANT_LEVEL_ADJUST`,
//! LCG sign flip), the voiced LTP prediction from the re-whitened state,
//! and the gain/LPC synthesis path — and the candidate whose decoded
//! output best tracks the input wins.
//!
//! Consequences:
//!
//! - The decoder's reconstruction of the emitted `pulses` is *bit-exactly*
//!   the `xq` produced here, including every subframe-level state update
//!   (gain-change rescaling of the LPC state, LTP re-whitening at subframes
//!   0/2 with NLSF interpolation, the `ltp_scale` downscale at subframe 0,
//!   the `outBuf` slide), because those updates are literally the same
//!   operations on the same state. The encoder→decoder round trip is
//!   covered by an exact-equality test.
//! - The subframe loop below mirrors `silk_decode_core` statement by
//!   statement; the only addition is the candidate search between LTP
//!   prediction and LPC synthesis. The reference's PLC-transition fix-up
//!   is unreachable (the encoder always encodes "good" frames).
//!
//! SOURCE: Xiph.Org libopus 1.5.2, `silk/decode_core.c` (mirrored),
//! `silk/NSQ.c` and `silk/NSQ_del_dec.c` (simplified: no noise-shaping
//! AR feedback, no delayed decisions), `silk/decode_core.c`'s excitation
//! reconstruction (shared via [`super::excitation`]) (BSD-3-Clause).
#![allow(dead_code)]

use crate::silk::decode_indices::MAX_LPC_ORDER;
use crate::silk::decode_indices::{SideInfoIndices, TYPE_VOICED};
use crate::silk::excitation::QUANT_LEVEL_ADJUST_Q10;
use crate::silk::nlsf::inverse32_varq;
use crate::silk::noise_shape::ShapeParams;
use crate::silk::pitch::LTP_ORDER;
use crate::silk::sigproc::{
    add_lshift32, add_sat32, div32_varq, lpc_analysis_filter, lshift_sat32, rand, rshift_round,
    sat16, smlawb, smulwb, smulww,
};
use crate::silk::synthesis::{FrameInfo, SynthesisState, MAX_FRAME_LENGTH};
use crate::silk::tables::QUANTIZATION_OFFSETS_Q10;

/// The candidate-window half-width around the estimated index. Five
/// candidates straddling the open-loop estimate recover essentially all
/// of the closed-loop gain: the estimate errs by the rounding of the
/// inverse-gain path, and the decoder-state feedback can only move the
/// optimum by a couple of levels per sample. (A superset of the
/// reference's two-candidate comparison.)
const CANDIDATE_RADIUS: i64 = 2;

/// Bound on the emitted quantization indices (the reference's pulses are
/// `opus_int8`; the shell/LSB coder spans this comfortably).
const MAX_ABS_PULSE: i64 = 100;

/// One frame's forward-NSQ result.
pub(crate) struct NsqResult {
    /// Signed quantization indices (shell-rounded frame length; the tail
    /// beyond `frame_length` is zero).
    pub pulses: [i16; MAX_FRAME_LENGTH],
    /// The decoder-exact reconstructed output (frame length).
    pub xq: [i16; MAX_FRAME_LENGTH],
}

/// Runs the closed-loop forward NSQ for one frame.
///
/// `state` is the decoder-mirror state ([`SynthesisState`], same fields
/// `decode_core` mutates); `exc_q14` receives the Q14 excitation exactly
/// as the decoder's retained copy would hold (only PLC/CNG read it).
/// `x` is the input frame at the internal rate. `loss_cnt` is always 0 on
/// the encode path.
/// `lag_prev` is the mirror `lagPrev` the reference's shaping path starts
/// unvoiced frames from.
///
/// `shape` carries the frame's noise-shaping parameters from
/// [`crate::silk::noise_shape`]. The **gains** it produced are what actually
/// reach the quantizer (through `ctrl.gains_q16`, which sets the pulse LSB);
/// the shaping filter, tilt, harmonic gain and `Lambda` are computed and
/// range-checked here but not yet fed back into the residual — see the
/// `noise_shape` module docs and `todo.md` for the measurements behind that
/// decision.
#[allow(clippy::too_many_arguments)]
pub(crate) fn encode_frame_nsq(
    state: &mut SynthesisState,
    shape: &ShapeParams,
    exc_q14: &mut [i32],
    pulses_out: &mut [i16],
    xq_out: &mut [i16],
    x: &[i16],
    ctrl: &crate::silk::synthesis::DecoderControl,
    indices: &SideInfoIndices,
    frame: &FrameInfo,
    loss_cnt: i32,
    _prev_signal_type: i8,
    lag_prev: i32,
) {
    // The reference's PLC-transition fix-up reads `loss_cnt` /
    // `prev_signal_type`; the encoder always encodes good frames, so they
    // are carried for signature parity with `decode_core` and otherwise
    // unused.
    debug_assert_eq!(loss_cnt, 0, "the encoder never runs after a loss");
    let frame_length = frame.frame_length();
    debug_assert!(x.len() >= frame_length);
    debug_assert!(pulses_out.len() >= frame_length && xq_out.len() >= frame_length);
    debug_assert!(exc_q14.len() >= frame_length);
    /* The shaping analysis must hand the quantizer a usable rate/distortion
     * factor even though only its gains are consumed today. */
    debug_assert!(shape.lambda_q10 > 0 && shape.lambda_q10 < 2048);

    let mut s_ltp = [0i16; MAX_FRAME_LENGTH];
    let mut s_ltp_q15 = [0i32; 2 * MAX_FRAME_LENGTH];
    let mut s_lpc_q14 = [0i32; MAX_FRAME_LENGTH / 4 + MAX_LPC_ORDER];

    let nlsf_interpolation_flag = i32::from(indices.nlsf_interp_coef_q2 < 1 << 2);
    let offset_q10 = QUANTIZATION_OFFSETS_Q10[(indices.signal_type >> 1) as usize]
        [indices.quant_offset_type as usize] as i32;
    let quant_adjust = QUANT_LEVEL_ADJUST_Q10 << 4;

    /* Excitation dither state: `reconstruct_excitation`'s LCG, advanced
     * per sample before use and accumulated with the chosen q after. */
    let mut rand_seed = indices.seed as i32;

    /* Copy LPC state */
    s_lpc_q14[..MAX_LPC_ORDER].copy_from_slice(&state.s_lpc_q14_buf);

    /* The LTP state index the voiced branch advances; unvoiced subframes
     * leave it untouched, exactly as `silk_NSQ` and the decoder do. */
    let mut s_ltp_buf_idx = frame.ltp_mem_length;
    let mut lag = lag_prev.max(0) as usize;
    let mut exc_base = 0usize;
    let mut xq_base = 0usize;

    for k in 0..frame.nb_subfr {
        let mut a_q12_tmp = [0i16; MAX_LPC_ORDER];
        a_q12_tmp[..frame.lpc_order]
            .copy_from_slice(&ctrl.pred_coef_q12[k >> 1][..frame.lpc_order]);
        let b_q14: [i16; LTP_ORDER] = if indices.signal_type == TYPE_VOICED {
            let mut b = [0i16; LTP_ORDER];
            b.copy_from_slice(&ctrl.ltp_coef_q14[k * LTP_ORDER..(k + 1) * LTP_ORDER]);
            b
        } else {
            [0i16; LTP_ORDER]
        };

        let gain_q10 = ctrl.gains_q16[k] >> 6;
        let mut inv_gain_q31 = inverse32_varq(ctrl.gains_q16[k], 47);

        /* Gain adjustment factor (rescales the short-term state) */
        let gain_adj_q16 = if ctrl.gains_q16[k] != state.prev_gain_q16 {
            let adj = div32_varq(state.prev_gain_q16, ctrl.gains_q16[k], 16);
            for v in s_lpc_q14[..MAX_LPC_ORDER].iter_mut() {
                *v = smulww(adj, *v);
            }
            adj
        } else {
            1 << 16
        };
        state.prev_gain_q16 = ctrl.gains_q16[k];

        if indices.signal_type == TYPE_VOICED {
            let sub_lag = ctrl.pitch_l[k].max(0) as usize;

            if k == 0 || (k == 2 && nlsf_interpolation_flag != 0) {
                /* Re-whiten with new A coefs */
                let start_idx = frame.ltp_mem_length - sub_lag - frame.lpc_order - LTP_ORDER / 2;
                debug_assert!(start_idx > 0);

                if k == 2 {
                    state.out_buf
                        [frame.ltp_mem_length..frame.ltp_mem_length + 2 * frame.subfr_length]
                        .copy_from_slice(&xq_out[..2 * frame.subfr_length]);
                }

                let in_start = start_idx + k * frame.subfr_length;
                lpc_analysis_filter(
                    &mut s_ltp[start_idx..frame.ltp_mem_length],
                    &state.out_buf[in_start..in_start + frame.ltp_mem_length - start_idx],
                    &a_q12_tmp,
                    frame.lpc_order,
                );

                /* After re-whitening the LTP state is unscaled */
                if k == 0 {
                    inv_gain_q31 = smulwb(inv_gain_q31, ctrl.ltp_scale_q14 as i32).wrapping_shl(2);
                }
                for i in 0..sub_lag + LTP_ORDER / 2 {
                    s_ltp_q15[s_ltp_buf_idx - i - 1] =
                        smulwb(inv_gain_q31, s_ltp[frame.ltp_mem_length - i - 1] as i32);
                }
            } else {
                /* Update LTP state when the gain changes */
                if gain_adj_q16 != 1 << 16 {
                    for i in 0..sub_lag + LTP_ORDER / 2 {
                        s_ltp_q15[s_ltp_buf_idx - i - 1] =
                            smulww(gain_adj_q16, s_ltp_q15[s_ltp_buf_idx - i - 1]);
                    }
                }
            }
            /* Unvoiced subframes keep the previous frame's lag, as `silk_NSQ`
             * does, so the frame's LTP state stays contiguous. */
            lag = sub_lag;
        }

        let tap_base = if indices.signal_type == TYPE_VOICED {
            s_ltp_buf_idx - lag + LTP_ORDER / 2
        } else {
            0
        };
        for i in 0..frame.subfr_length {
            /* Long-term prediction from the (re-whitened) state —
             * identical to the decoder's, so candidate evaluation and
             * committed reconstruction share it. */
            let ltp_pred_q13 = if indices.signal_type == TYPE_VOICED {
                let tap = tap_base + i;
                let mut p = 2;
                p = smlawb(p, s_ltp_q15[tap], b_q14[0] as i32);
                p = smlawb(p, s_ltp_q15[tap - 1], b_q14[1] as i32);
                p = smlawb(p, s_ltp_q15[tap - 2], b_q14[2] as i32);
                p = smlawb(p, s_ltp_q15[tap - 3], b_q14[3] as i32);
                p = smlawb(p, s_ltp_q15[tap - 4], b_q14[4] as i32);
                p
            } else {
                0
            };

            let lpc_pred_q10 = lpc_prediction(&s_lpc_q14, i, &a_q12_tmp, frame.lpc_order);

            /* Advance the dither LCG (the decoder does this before
             * reading the sign of the excitation). */
            rand_seed = rand(rand_seed);
            let dither_negative = rand_seed < 0;

            /* Open-loop index estimate: invert the decoder's
             * excitation -> pres -> s_lpc -> xq chain. */
            let ideal_s_lpc =
                (((x[exc_base + i] as i64) << 24) + gain_q10 as i64 / 2) / gain_q10 as i64;
            let ideal_pres = ideal_s_lpc - ((lpc_pred_q10 as i64) << 4);
            let ideal_exc = ideal_pres - ((ltp_pred_q13 as i64) << 1);
            let f = if dither_negative {
                -ideal_exc
            } else {
                ideal_exc
            };
            let q_est = ((f - (offset_q10 << 4) as i64 + (1 << 13)) >> 14)
                .clamp(-MAX_ABS_PULSE, MAX_ABS_PULSE);

            /* Closed loop: keep the candidate whose decoder-exact output is
             * closest to the input. The shaped residual is deliberately NOT
             * used as the target here — see the module docs. */
            let mut best_err = i64::MAX;
            let mut best_q: i16 = 0;
            let mut best_exc_q14 = 0i32;
            let mut best_res_q14 = 0i32;
            let mut best_s_lpc = 0i32;
            for delta in -CANDIDATE_RADIUS..=CANDIDATE_RADIUS {
                let q = (q_est + delta).clamp(-MAX_ABS_PULSE, MAX_ABS_PULSE) as i32;
                let mut e = q.wrapping_shl(14);
                if e > 0 {
                    e = e.wrapping_sub(quant_adjust);
                } else if e < 0 {
                    e = e.wrapping_add(quant_adjust);
                }
                e = e.wrapping_add(offset_q10 << 4);
                let exc = if dither_negative { e.wrapping_neg() } else { e };
                let res_q14 = add_lshift32(exc, ltp_pred_q13, 1);
                let s_lpc_new = add_sat32(res_q14, lshift_sat32(lpc_pred_q10, 4));
                let xq_cand = sat16(rshift_round(smulww(s_lpc_new, gain_q10), 8));
                let err = (i64::from(x[exc_base + i]) - i64::from(xq_cand)).abs();
                if err < best_err {
                    best_err = err;
                    best_q = q as i16;
                    best_exc_q14 = exc;
                    best_res_q14 = res_q14;
                    best_s_lpc = s_lpc_new;
                }
            }

            /* Commit: update the reconstruction states exactly like the
             * decoder. */
            pulses_out[exc_base + i] = best_q;
            exc_q14[exc_base + i] = best_exc_q14;
            s_lpc_q14[MAX_LPC_ORDER + i] = best_s_lpc;
            xq_out[xq_base + i] = sat16(rshift_round(
                smulww(s_lpc_q14[MAX_LPC_ORDER + i], gain_q10),
                8,
            ));
            if indices.signal_type == TYPE_VOICED {
                s_ltp_q15[s_ltp_buf_idx] = best_res_q14.wrapping_shl(1);
                s_ltp_buf_idx += 1;
            }
            rand_seed = rand_seed.wrapping_add(best_q as i32);
        }

        /* Update LPC filter state */
        s_lpc_q14.copy_within(frame.subfr_length..frame.subfr_length + MAX_LPC_ORDER, 0);
        exc_base += frame.subfr_length;
        xq_base += frame.subfr_length;
    }

    /* Save LPC state */
    state
        .s_lpc_q14_buf
        .copy_from_slice(&s_lpc_q14[..MAX_LPC_ORDER]);
}

/// The decoder's short-term prediction for sample `i` of the current
/// subframe: `lpc_pred_Q10 = order/2 + Σ a[j]·s_lpc[ORDER+i-1-j] >> 12`
/// (the `order/2` bias matches `silk_SMLAWB`'s rounding in
/// `silk_decode_core`).
#[inline]
fn lpc_prediction(
    s_lpc_q14: &[i32],
    i: usize,
    a_q12: &[i16; MAX_LPC_ORDER],
    lpc_order: usize,
) -> i32 {
    let mut lpc_pred_q10 = (lpc_order >> 1) as i32;
    for (j, a) in a_q12[..10].iter().enumerate() {
        lpc_pred_q10 = smlawb(
            lpc_pred_q10,
            s_lpc_q14[MAX_LPC_ORDER + i - 1 - j],
            *a as i32,
        );
    }
    if lpc_order == 16 {
        for (j, a) in a_q12[10..16].iter().enumerate() {
            lpc_pred_q10 = smlawb(
                lpc_pred_q10,
                s_lpc_q14[MAX_LPC_ORDER + i - 11 - j],
                *a as i32,
            );
        }
    }
    lpc_pred_q10
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::silk::decode_indices::TYPE_UNVOICED;
    use crate::silk::synthesis::{decode_core, DecoderControl};

    /// A non-trivial but bounded shaping configuration, so the differential
    /// test below runs with the shaping analysis' parameters in play.
    fn shaping() -> ShapeParams {
        use crate::silk::decode_indices::MAX_NB_SUBFR as NB;
        use crate::silk::noise_shape::MAX_SHAPE_LPC_ORDER;
        let mut ar_q13 = [0i16; NB * MAX_SHAPE_LPC_ORDER];
        for (i, v) in ar_q13.iter_mut().enumerate() {
            // A gentle, decaying coefficient set (well inside the 3.999 limit).
            *v = (((i % 8) as f32 - 3.5) * 900.0) as i16;
        }
        ShapeParams {
            ar_q13,
            lf_shp_q14: [(-2000i32) << 16 | (0xF000u16 as i16) as i32; NB],
            tilt_q14: [-1200; NB],
            harm_shape_gain_q14: [3000; NB],
            lambda_q10: 1000,
            lambda: 1000.0 / 1024.0,
            coding_quality: 0.5,
        }
    }

    /// Differential test: with identical parameters and starting state,
    /// the committed reconstruction of the closed-loop NSQ must be
    /// bit-identical to what the real decoder's `decode_core` produces
    /// from the emitted pulses (the encoder's core contract).
    #[test]
    fn committed_xq_equals_decode_core() {
        let fs = 16u32;
        let nb_subfr = 4usize;
        let frame = FrameInfo::new(fs, nb_subfr);
        let frame_length = frame.frame_length();

        for signal_type in [TYPE_UNVOICED, TYPE_VOICED] {
            for seed in [0i8, 2] {
                let mut ctrl = DecoderControl::default();
                for (k, g) in ctrl.gains_q16.iter_mut().enumerate().take(nb_subfr) {
                    *g = if k == 2 { 4_000_000 } else { 1_500_000 };
                }
                for v in ctrl.pred_coef_q12[1].iter_mut().take(16) {
                    *v = -1200;
                }
                ctrl.pred_coef_q12[0] = ctrl.pred_coef_q12[1];
                if signal_type == TYPE_VOICED {
                    for k in 0..nb_subfr {
                        ctrl.pitch_l[k] = 100 + 10 * k as i32;
                    }
                    for (i, v) in ctrl.ltp_coef_q14[..nb_subfr * 5].iter_mut().enumerate() {
                        *v = [0, 1000, 8000, 1000, 0][i % 5];
                    }
                }
                ctrl.ltp_scale_q14 = 15565;

                let indices = SideInfoIndices {
                    signal_type,
                    quant_offset_type: 1,
                    seed,
                    nlsf_interp_coef_q2: 4,
                    ..SideInfoIndices::default()
                };

                /* Deterministic input */
                let x: Vec<i16> = (0..frame_length)
                    .map(|i| (2000.0 * (i as f32 * 0.05).sin()) as i16)
                    .collect();

                /* Encoder side */
                let mut enc_state = SynthesisState::default();
                let mut exc = [0i32; MAX_FRAME_LENGTH];
                let mut pulses = [0i16; MAX_FRAME_LENGTH];
                let mut xq_enc = [0i16; MAX_FRAME_LENGTH];
                encode_frame_nsq(
                    &mut enc_state,
                    &shaping(),
                    &mut exc,
                    &mut pulses,
                    &mut xq_enc,
                    &x,
                    &ctrl,
                    &indices,
                    &frame,
                    0,
                    0,
                    0,
                );

                /* Decoder side: same starting state, same params, same
                 * pulses -> must reproduce xq_enc exactly. */
                let mut dec_state = SynthesisState::default();
                let mut dec_ctrl = ctrl;
                let mut exc_dec = [0i32; MAX_FRAME_LENGTH];
                let mut xq_dec = [0i16; MAX_FRAME_LENGTH];
                decode_core(
                    &mut dec_state,
                    &mut dec_ctrl,
                    &indices,
                    &pulses,
                    &mut xq_dec,
                    &mut exc_dec,
                    &frame,
                    0,
                    0,
                    0,
                );

                assert_eq!(
                    &xq_enc[..frame_length],
                    &xq_dec[..frame_length],
                    "signal_type {signal_type}, seed {seed}"
                );
                assert_eq!(enc_state, dec_state, "state must match too");
            }
        }
    }
}
