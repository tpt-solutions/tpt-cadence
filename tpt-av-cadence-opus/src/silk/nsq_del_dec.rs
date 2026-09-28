//! The reference delayed-decision noise-shaping quantizer — an exact port
//! of `silk/NSQ_del_dec.c`'s `silk_NSQ_del_dec`,
//! `silk_noise_shape_quantizer_del_dec`, and
//! `silk_nsq_del_dec_scale_states`: the multi-state (1–4 quantization
//! paths) rate/distortion tree search the reference runs at encoder
//! complexity >= 2, with `silk_NSQ` (ported in [`crate::silk::nsq_ref`])
//! reserved for complexity < 2.
//!
//! Instead of committing one of two candidates per sample (as the plain
//! reference NSQ does), every sample extends *each* of the
//! `n_states_delayed_decision` paths with its best and second-best
//! candidate and then keeps the tree of paths within `decisionDelay`
//! (<= 40 samples) of the output cursor: the path whose accumulated
//! rate/distortion is worst is replaced by another path's second-best
//! branch whenever that outperforms it, and only the running winner's
//! decisions — delayed by `decisionDelay` samples, so a path can still be
//! pruned after the fact — reach the output and the shared LTP/shaping
//! state the next subframes read. Each path carries its own short-term
//! LPC/shaping state, dither seed and delayed-decision ring buffers; the
//! winner's final state (and its initial seed, which the decoder must
//! know to reproduce the dither) seeds the next frame.
//!
//! Three reference constructs need deliberate Rust shapes, all documented
//! at their sites below: (1) the pruning-point *partial* state copy — C
//! `memcpy`s the struct skipping its first `i` words, which in Rust is a
//! per-field copy that skips the first `i` samples of `s_lpc_q14`; (2) the
//! winner's output writes at `pulses[i - decisionDelay]` — C pointer
//! arithmetic reaching back across subframe boundaries, which becomes
//! frame-relative indexing into whole-frame buffers; (3) the voiced
//! re-whitening at subframe 2, which snaps the delayed-decision tree
//! (penalizing all non-winner states and flushing the winner's pending
//! tail to the output) before filtering with the new coefficients.
//!
//! Like [`crate::silk::nsq_ref`], this module operates on the shared
//! [`crate::silk::nsq_ref::NsqState`] carrier and is validated by the
//! same contract: the `xq` it produces must be bit-identical to what
//! `silk_decode_core` reconstructs from the pulses it emitted, with the
//! seed it transmits — pinned by
//! `tests::del_dec_xq_equals_decode_core` across state counts, dither
//! seeds, interpolation and warping.
//!
//! SOURCE: Xiph.Org libopus 1.5.2, `silk/NSQ_del_dec.c`, `silk/NSQ.h`,
//! `silk/define.h` (BSD-3-Clause).
#![allow(dead_code)]

use crate::silk::decode_indices::{SideInfoIndices, MAX_NB_SUBFR, TYPE_VOICED};
use crate::silk::excitation::QUANT_LEVEL_ADJUST_Q10;
use crate::silk::noise_shape::MAX_SHAPE_LPC_ORDER;
use crate::silk::nsq_ref::{NsqResult, NsqState};
use crate::silk::pitch::LTP_ORDER;
use crate::silk::sigproc::{
    add_sat32, lpc_analysis_filter, rand, rshift_round, sat16, smlawb, smlawt, smulbb, smulwb,
    smulww,
};
use crate::silk::synthesis::{DecoderControl, FrameInfo, MAX_FRAME_LENGTH, MAX_SUB_FRAME_LENGTH};
use crate::silk::tables::QUANTIZATION_OFFSETS_Q10;

/// `DECISION_DELAY` (`silk/define.h`): the delayed-decision ring-buffer
/// depth; the tree may revise a decision up to this many samples back.
const DECISION_DELAY: usize = 40;
/// `MAX_DEL_DEC_STATES` (`silk/define.h`).
pub(crate) const MAX_DEL_DEC_STATES: usize = 4;
/// `HARM_SHAPE_FIR_TAPS` (`silk/define.h`).
const HARM_SHAPE_FIR_TAPS: usize = 3;
/// `NSQ_LPC_BUF_LENGTH` (`silk/define.h`): `MAX_LPC_ORDER`.
const NSQ_LPC_BUF_LENGTH: usize = 16;

/// One delayed-decision quantization path (`NSQ_del_dec_struct`).
#[derive(Clone)]
struct DelDecState {
    /// Short-term prediction state (Q14, scaled domain): the history in
    /// `[..NSQ_LPC_BUF_LENGTH]`, the current subframe's committed `xq`
    /// samples from `[NSQ_LPC_BUF_LENGTH..]`.
    s_lpc_q14: Vec<i32>,
    /// Dither-seed history ring, used to spot paths whose committed
    /// decisions `decisionDelay` back differ from the winner's
    /// ("expired" paths).
    rand_state: [i32; DECISION_DELAY],
    /// Delayed decisions: quantization indices (Q10).
    q_q10: [i32; DECISION_DELAY],
    /// Delayed decisions: unquantized reconstruction (Q14, scaled domain).
    xq_q14: [i32; DECISION_DELAY],
    /// Delayed decisions: LPC+LTP excitation (Q14), stored Q15 for the
    /// long-term prediction state.
    pred_q15: [i32; DECISION_DELAY],
    /// Delayed decisions: shaped-noise samples feeding the shared
    /// long-term shaping state.
    shape_q14: [i32; DECISION_DELAY],
    /// Shaping AR filter state.
    s_ar2_q14: [i32; MAX_SHAPE_LPC_ORDER],
    lf_ar_q14: i32,
    diff_q14: i32,
    seed: i32,
    seed_init: i32,
    /// Accumulated rate/distortion (Q10).
    rd_q10: i32,
}

impl DelDecState {
    /// The per-frame initialization from `silk_NSQ_del_dec` (all paths
    /// start from the shared NSQ state; only the seeds differ).
    fn new(nsq: &NsqState, ltp_mem_length: usize, seed: i32) -> Self {
        let mut s = DelDecState {
            s_lpc_q14: vec![0; MAX_SUB_FRAME_LENGTH + NSQ_LPC_BUF_LENGTH],
            rand_state: [0; DECISION_DELAY],
            q_q10: [0; DECISION_DELAY],
            xq_q14: [0; DECISION_DELAY],
            pred_q15: [0; DECISION_DELAY],
            shape_q14: [0; DECISION_DELAY],
            s_ar2_q14: nsq.s_ar2_q14,
            lf_ar_q14: nsq.s_lf_ar_shp_q14,
            diff_q14: nsq.s_diff_shp_q14,
            seed,
            seed_init: seed,
            rd_q10: 0,
        };
        s.shape_q14[0] = nsq.s_ltp_shp_q14[ltp_mem_length - 1];
        s.s_lpc_q14[..NSQ_LPC_BUF_LENGTH].copy_from_slice(&nsq.s_lpc_q14[..NSQ_LPC_BUF_LENGTH]);
        s
    }
}

/// One sample's two candidates, ordered best-first
/// (`NSQ_sample_struct` / `NSQ_sample_pair`).
#[derive(Clone, Copy)]
struct NsqSample {
    q_q10: i32,
    rd_q10: i32,
    xq_q14: i32,
    lf_ar_q14: i32,
    diff_q14: i32,
    s_ltp_shp_q14: i32,
    lpc_exc_q14: i32,
}

impl NsqSample {
    const ZERO: NsqSample = NsqSample {
        q_q10: 0,
        rd_q10: 0,
        xq_q14: 0,
        lf_ar_q14: 0,
        diff_q14: 0,
        s_ltp_shp_q14: 0,
        lpc_exc_q14: 0,
    };
}

/// `silk_NSQ_del_dec` — one frame through the delayed-decision tree.
///
/// `n_states_delayed_decision` is `psEncC->nStatesDelayedDecision` (1–4)
/// and `warping_q16` the allpass warp of the shaping feedback (0 disables
/// it), both per `silk_setup_complexity`. On return `indices.seed` holds
/// the winner's initial dither seed (`psIndices->Seed = psDD->SeedInit`),
/// which is the seed that must be transmitted for the decoder's
/// reconstruction to match.
#[allow(clippy::too_many_arguments)]
pub(crate) fn nsq_del_dec(
    nsq: &mut NsqState,
    indices: &mut SideInfoIndices,
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
    n_states_delayed_decision: usize,
    warping_q16: i32,
) -> NsqResult {
    let frame_length = frame.frame_length();
    let subfr_length = frame.subfr_length;
    let ltp_mem_length = frame.ltp_mem_length;
    let shaping_lpc_order = MAX_SHAPE_LPC_ORDER;
    let predict_lpc_order = frame.lpc_order;
    let nb_subfr = frame.nb_subfr;

    // Set unvoiced lag to the previous one; overwritten for voiced.
    let mut lag = nsq.lag_prev.max(0) as i64;

    // Initialize delayed decision states.
    let mut del_dec: Vec<DelDecState> = (0..n_states_delayed_decision)
        .map(|k| {
            DelDecState::new(
                nsq,
                ltp_mem_length,
                (k as i32 + i32::from(indices.seed)) & 3,
            )
        })
        .collect();

    let offset_q10 = QUANTIZATION_OFFSETS_Q10[(indices.signal_type >> 1) as usize]
        [indices.quant_offset_type as usize] as i32;
    let lsf_interpolation_flag = i32::from(indices.nlsf_interp_coef_q2 < 4);

    // decisionDelay = min(DECISION_DELAY, subfr_length), further limited
    // below the pitch lag for voiced frames: the tree's delayed writes
    // into the shared long-term state must land before the shaping
    // feedback reads those samples back.
    let mut decision_delay = (DECISION_DELAY.min(subfr_length)) as i64;
    if indices.signal_type == TYPE_VOICED {
        for k in 0..nb_subfr {
            decision_delay =
                decision_delay.min(i64::from(ctrl.pitch_l[k]) - LTP_ORDER as i64 / 2 - 1);
        }
    } else if lag > 0 {
        decision_delay = decision_delay.min(lag - LTP_ORDER as i64 / 2 - 1);
    }

    let mut s_ltp_q15 = vec![0i32; ltp_mem_length + frame_length];
    let mut s_ltp = vec![0i16; ltp_mem_length + frame_length];
    let mut x_sc_q10 = vec![0i32; subfr_length];
    let mut delayed_gain_q10 = [0i32; DECISION_DELAY];
    let mut pulses = [0i16; MAX_FRAME_LENGTH];
    let mut smpl_buf_idx = 0i64; // index of oldest samples

    nsq.s_ltp_shp_buf_idx = ltp_mem_length;
    nsq.s_ltp_buf_idx = ltp_mem_length;
    let mut subfr = 0usize;
    for k in 0..nb_subfr {
        let a_q12_sel = (k >> 1) | (1 - lsf_interpolation_flag as usize);
        let a_q12 = &ctrl.pred_coef_q12[a_q12_sel][..predict_lpc_order];
        let b_q14 = &ctrl.ltp_coef_q14[k * 5..k * 5 + 5];
        let ar_shp_q13 = &ar_q13[k * MAX_SHAPE_LPC_ORDER..][..MAX_SHAPE_LPC_ORDER];

        // HarmShapeFIRPacked_Q14 packs (gain>>2, gain>>1) as two 16-bit
        // halves of one i32.
        let harm = harm_shape_gain_q14[k];
        let mut harm_fir_packed_q14 = harm >> 2;
        harm_fir_packed_q14 |= (harm >> 1).wrapping_shl(16);

        nsq.rewhite_flag = false;
        if indices.signal_type == TYPE_VOICED {
            lag = i64::from(ctrl.pitch_l[k]);

            // Re-whiten with new A coefs every other subframe while the
            // NLSFs interpolate (every subframe when they do not).
            if (k & (3 - ((lsf_interpolation_flag as usize) << 1))) == 0 {
                if k == 2 {
                    // RESET DELAYED DECISIONS. Find the winner and make it
                    // permanent: every other path's accumulated
                    // rate/distortion is penalized out of reach.
                    let mut winner = 0usize;
                    let mut rd_min = del_dec[0].rd_q10;
                    for (i, s) in del_dec.iter().enumerate().skip(1) {
                        if s.rd_q10 < rd_min {
                            rd_min = s.rd_q10;
                            winner = i;
                        }
                    }
                    for (i, s) in del_dec.iter_mut().enumerate() {
                        if i != winner {
                            s.rd_q10 = s.rd_q10.saturating_add(i32::MAX >> 4);
                        }
                    }

                    // Copy the winner's pending tail to the output and the
                    // long-term shaping state — the previous subframe's
                    // last `decisionDelay` samples, frame-relative (the
                    // output guard below would otherwise leave them
                    // unwritten).
                    //
                    // **Deviation — reconstruction-form parity.** The
                    // reference materializes this copy as
                    // `SAT16(RSHIFT_ROUND(SMULWW(xq, Gains_Q16[1]), 14))`,
                    // whose intermediate truncation differs by up to 1 LSB
                    // from the decoder's
                    // `SAT16(RSHIFT_ROUND(SMULWW(xq, Gains_Q16[1] >> 6), 8))`
                    // on rounding-boundary samples. libopus tolerates that
                    // (its encoder-side reconstruction is a soft state);
                    // this crate's closed-loop contract
                    // (`encoder_simulation_matches_decoder_bit_exactly`)
                    // does not, so the copy uses the decoder's exact form.
                    let last = &del_dec[winner];
                    let mut last_smple_idx = smpl_buf_idx + decision_delay;
                    let prev_gain_q10 = gains_q16[1] >> 6;
                    for i in 0..decision_delay {
                        last_smple_idx = (last_smple_idx - 1).rem_euclid(DECISION_DELAY as i64);
                        let pos = (2 * subfr_length) as i64 + i - decision_delay;
                        pulses[pos as usize] =
                            rshift_round(last.q_q10[last_smple_idx as usize], 10) as i16;
                        // The previous subframe's gain, in the decoder's
                        // Q10 form.
                        nsq.xq[ltp_mem_length + pos as usize] = sat16(rshift_round(
                            smulww(last.xq_q14[last_smple_idx as usize], prev_gain_q10),
                            8,
                        ));
                        nsq.s_ltp_shp_q14
                            [nsq.s_ltp_shp_buf_idx - decision_delay as usize + i as usize] =
                            last.shape_q14[last_smple_idx as usize];
                    }

                    subfr = 0;
                }

                // Rewhiten with new A coefs.
                let start_idx = ltp_mem_length - lag as usize - predict_lpc_order - LTP_ORDER / 2;
                debug_assert!(start_idx > 0);
                // The reference hands the filter two same-size arrays and
                // filters the whole remaining tail of each (see the plain
                // port's note); the common extent is taken explicitly.
                let in_start = start_idx + k * subfr_length;
                let len = (s_ltp.len() - start_idx).min(nsq.xq.len() - in_start);
                lpc_analysis_filter(
                    &mut s_ltp[start_idx..start_idx + len],
                    &nsq.xq[in_start..in_start + len],
                    a_q12,
                    predict_lpc_order,
                );
                nsq.s_ltp_buf_idx = ltp_mem_length;
                nsq.rewhite_flag = true;
            }
        }

        scale_states_del_dec(
            nsq,
            &mut del_dec,
            &x16[k * subfr_length..],
            &mut x_sc_q10,
            &s_ltp,
            &mut s_ltp_q15,
            k,
            n_states_delayed_decision,
            ltp_scale_q14,
            gains_q16,
            &ctrl.pitch_l,
            indices.signal_type,
            decision_delay as usize,
            ltp_mem_length,
        );

        noise_shape_quantizer_del_dec(
            nsq,
            &mut del_dec,
            indices.signal_type,
            &x_sc_q10,
            &mut pulses,
            k * subfr_length,
            &mut s_ltp_q15,
            &mut delayed_gain_q10,
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
            subfr,
            shaping_lpc_order,
            predict_lpc_order,
            warping_q16,
            n_states_delayed_decision,
            &mut smpl_buf_idx,
            decision_delay,
            ltp_mem_length,
        );

        subfr += 1;
    }

    // Find the frame's winner and copy its pending tail (the last
    // `decisionDelay` samples of the frame) to the output and states.
    let mut winner = 0usize;
    for (i, s) in del_dec.iter().enumerate().skip(1) {
        if s.rd_q10 < del_dec[winner].rd_q10 {
            winner = i;
        }
    }
    let last = &del_dec[winner];
    indices.seed = last.seed_init as i8;
    let mut last_smple_idx = smpl_buf_idx + decision_delay;
    // Gain_Q10 = Gains_Q16[nb_subfr-1] >> 6 — the quantizer's Q10 form.
    let gain_q10 = gains_q16[nb_subfr - 1] >> 6;
    for i in 0..decision_delay {
        last_smple_idx = (last_smple_idx - 1).rem_euclid(DECISION_DELAY as i64);
        let pos = frame_length as i64 + i - decision_delay;
        pulses[pos as usize] = rshift_round(last.q_q10[last_smple_idx as usize], 10) as i16;
        nsq.xq[ltp_mem_length + pos as usize] = sat16(rshift_round(
            smulww(last.xq_q14[last_smple_idx as usize], gain_q10),
            8,
        ));
        nsq.s_ltp_shp_q14[nsq.s_ltp_shp_buf_idx - decision_delay as usize + i as usize] =
            last.shape_q14[last_smple_idx as usize];
    }
    nsq.s_lpc_q14[..NSQ_LPC_BUF_LENGTH]
        .copy_from_slice(&last.s_lpc_q14[subfr_length..subfr_length + NSQ_LPC_BUF_LENGTH]);
    nsq.s_ar2_q14 = last.s_ar2_q14;
    nsq.s_lf_ar_shp_q14 = last.lf_ar_q14;
    nsq.s_diff_shp_q14 = last.diff_q14;
    nsq.lag_prev = ctrl.pitch_l[nb_subfr - 1];

    // Slide the history buffers, keeping `ltp_mem_length` of each.
    nsq.xq
        .copy_within(frame_length..frame_length + ltp_mem_length, 0);
    nsq.s_ltp_shp_q14
        .copy_within(frame_length..frame_length + ltp_mem_length, 0);

    // The frame's quantized output lives at xq[ltp_mem..ltp_mem+frame].
    let mut xq_out = [0i16; MAX_FRAME_LENGTH];
    xq_out[..frame_length].copy_from_slice(&nsq.xq[ltp_mem_length..ltp_mem_length + frame_length]);

    NsqResult { pulses, xq: xq_out }
}

/// `silk_noise_shape_quantizer_del_dec` — one subframe through the tree.
///
/// `frame_pos` is the subframe's offset within the frame (the reference's
/// `pulses`/`pxq` cursor); the winner's delayed writes land at
/// `frame_pos + i - decisionDelay`, reaching back into previous subframes
/// exactly as the reference's negative pointer offsets do.
#[allow(clippy::too_many_arguments)]
fn noise_shape_quantizer_del_dec(
    nsq: &mut NsqState,
    del_dec: &mut [DelDecState],
    signal_type: i8,
    x_q10: &[i32],
    pulses: &mut [i16; MAX_FRAME_LENGTH],
    frame_pos: usize,
    s_ltp_q15: &mut [i32],
    delayed_gain_q10: &mut [i32; DECISION_DELAY],
    a_q12: &[i16],
    b_q14: &[i16],
    ar_shp_q13: &[i16],
    lag: i64,
    harm_shape_fir_packed_q14: i32,
    tilt_q14: i32,
    lf_shp_q14: i32,
    gain_q16: i32,
    lambda_q10: i32,
    offset_q10: i32,
    length: usize,
    subfr: usize,
    shaping_lpc_order: usize,
    predict_lpc_order: usize,
    warping_q16: i32,
    n_states_delayed_decision: usize,
    smpl_buf_idx: &mut i64,
    decision_delay: i64,
    ltp_mem_length: usize,
) {
    let mut shp_lag_ptr = nsq.s_ltp_shp_buf_idx as i64 - lag + HARM_SHAPE_FIR_TAPS as i64 / 2;
    let mut pred_lag_ptr = nsq.s_ltp_buf_idx as i64 - lag + LTP_ORDER as i64 / 2;
    let gain_q10 = gain_q16 >> 6;

    let mut ps_sample_state = vec![[NsqSample::ZERO; 2]; n_states_delayed_decision];

    for (i, &x) in x_q10.iter().enumerate().take(length) {
        /* Perform common calculations used in all states */

        // Long-term prediction (Q13 → Q14).
        let ltp_pred_q14 = if signal_type == TYPE_VOICED {
            // +2 avoids the bias from SMLAWB's round-to-negative-inf.
            let mut p = 2i32;
            p = smlawb(p, s_ltp_q15[pred_lag_ptr as usize], i32::from(b_q14[0]));
            p = smlawb(p, s_ltp_q15[pred_lag_ptr as usize - 1], i32::from(b_q14[1]));
            p = smlawb(p, s_ltp_q15[pred_lag_ptr as usize - 2], i32::from(b_q14[2]));
            p = smlawb(p, s_ltp_q15[pred_lag_ptr as usize - 3], i32::from(b_q14[3]));
            p = smlawb(p, s_ltp_q15[pred_lag_ptr as usize - 4], i32::from(b_q14[4]));
            let p = p.wrapping_shl(1);
            pred_lag_ptr += 1;
            p
        } else {
            0
        };

        // Long-term shaping (Q12 → Q14).
        let n_ltp_q14 = if lag > 0 {
            // Symmetric, packed FIR coefficients.
            let base = shp_lag_ptr as usize;
            let mut n = smulwb(
                add_sat32(nsq.s_ltp_shp_q14[base], nsq.s_ltp_shp_q14[base - 2]),
                harm_shape_fir_packed_q14,
            );
            n = smlawt(n, nsq.s_ltp_shp_q14[base - 1], harm_shape_fir_packed_q14);
            shp_lag_ptr += 1;
            // silk_SUB_LSHIFT32(LTP_pred, n, 2): LTP_pred - (n << 2).
            ltp_pred_q14.wrapping_sub(n.wrapping_shl(2))
        } else {
            0
        };

        for k in 0..n_states_delayed_decision {
            let ps_dd = &mut del_dec[k];

            // Generate dither.
            ps_dd.seed = rand(ps_dd.seed);

            // Short-term prediction from this path's LPC state (Q10 → Q14).
            let ps_lpc_pos = NSQ_LPC_BUF_LENGTH - 1 + i;
            let mut lpc_pred_q14 = (predict_lpc_order >> 1) as i32;
            lpc_pred_q14 = smlawb(
                lpc_pred_q14,
                ps_dd.s_lpc_q14[ps_lpc_pos],
                i32::from(a_q12[0]),
            );
            for (j, a) in a_q12[1..predict_lpc_order].iter().enumerate() {
                lpc_pred_q14 = smlawb(
                    lpc_pred_q14,
                    ps_dd.s_lpc_q14[ps_lpc_pos - (j + 1)],
                    i32::from(*a),
                );
            }
            let lpc_pred_q14 = lpc_pred_q14.wrapping_shl(4);

            // Noise shape feedback (per path; Q11 → Q12 → Q14). With
            // warping_q16 == 0 this reduces exactly to the plain NSQ's
            // `silk_NSQ_noise_shape_feedback_loop_c`.
            let mut tmp2 = smlawb(ps_dd.diff_q14, ps_dd.s_ar2_q14[0], warping_q16);
            let mut tmp1 = smlawb(
                ps_dd.s_ar2_q14[0],
                ps_dd.s_ar2_q14[1].wrapping_sub(tmp2),
                warping_q16,
            );
            ps_dd.s_ar2_q14[0] = tmp2;
            let mut n_ar_q14 = (shaping_lpc_order >> 1) as i32;
            n_ar_q14 = smlawb(n_ar_q14, tmp2, i32::from(ar_shp_q13[0]));
            let mut j = 2;
            while j < shaping_lpc_order {
                tmp2 = smlawb(
                    ps_dd.s_ar2_q14[j - 1],
                    ps_dd.s_ar2_q14[j].wrapping_sub(tmp1),
                    warping_q16,
                );
                ps_dd.s_ar2_q14[j - 1] = tmp1;
                n_ar_q14 = smlawb(n_ar_q14, tmp1, i32::from(ar_shp_q13[j - 1]));
                tmp1 = smlawb(
                    ps_dd.s_ar2_q14[j],
                    ps_dd.s_ar2_q14[j + 1].wrapping_sub(tmp2),
                    warping_q16,
                );
                ps_dd.s_ar2_q14[j] = tmp2;
                n_ar_q14 = smlawb(n_ar_q14, tmp2, i32::from(ar_shp_q13[j]));
                j += 2;
            }
            ps_dd.s_ar2_q14[shaping_lpc_order - 1] = tmp1;
            n_ar_q14 = smlawb(n_ar_q14, tmp1, i32::from(ar_shp_q13[shaping_lpc_order - 1]));

            n_ar_q14 = n_ar_q14.wrapping_shl(1); // Q11 -> Q12
            n_ar_q14 = smlawb(n_ar_q14, ps_dd.lf_ar_q14, tilt_q14); // Q12
            n_ar_q14 = n_ar_q14.wrapping_shl(2); // Q12 -> Q14

            let mut n_lf_q14 = smulwb(ps_dd.shape_q14[*smpl_buf_idx as usize], lf_shp_q14); // Q12
            n_lf_q14 = smlawt(n_lf_q14, ps_dd.lf_ar_q14, lf_shp_q14); // Q12
            n_lf_q14 = n_lf_q14.wrapping_shl(2); // Q12 -> Q14

            // r = x[i] - LTP_pred - LPC_pred + n_AR + n_Tilt + n_LF + n_LTP
            let tmp1 = add_sat32(n_ar_q14, n_lf_q14); // Q14
            let tmp2 = n_ltp_q14.wrapping_add(lpc_pred_q14); // Q13
            let tmp1 = tmp2.saturating_sub(tmp1); // Q13
            let tmp1 = rshift_round(tmp1, 4); // Q10

            let mut r_q10 = x - tmp1; // residual error Q10

            // Flip sign depending on dither.
            if ps_dd.seed < 0 {
                r_q10 = -r_q10;
            }
            r_q10 = r_q10.clamp(-(31 << 10), 30 << 10);

            // Find two quantization level candidates and measure their
            // rate-distortion.
            let q1_q10_raw = r_q10 - offset_q10;
            let mut q1_q0 = q1_q10_raw >> 10;
            if lambda_q10 > 2048 {
                // For aggressive RDO, the bias becomes more than one pulse.
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
            let mut rd1_q10;
            let mut rd2_q10;
            if q1_q0 > 0 {
                q1_q10 = (q1_q0 << 10) - QUANT_LEVEL_ADJUST_Q10;
                q1_q10 += offset_q10;
                q2_q10 = q1_q10 + 1024;
                rd1_q10 = smulbb(q1_q10, lambda_q10);
                rd2_q10 = smulbb(q2_q10, lambda_q10);
            } else if q1_q0 == 0 {
                q1_q10 = offset_q10;
                q2_q10 = q1_q10 + (1024 - QUANT_LEVEL_ADJUST_Q10);
                rd1_q10 = smulbb(q1_q10, lambda_q10);
                rd2_q10 = smulbb(q2_q10, lambda_q10);
            } else if q1_q0 == -1 {
                q2_q10 = offset_q10;
                q1_q10 = q2_q10 - (1024 - QUANT_LEVEL_ADJUST_Q10);
                rd1_q10 = smulbb(-q1_q10, lambda_q10);
                rd2_q10 = smulbb(q2_q10, lambda_q10);
            } else {
                q1_q10 = (q1_q0 << 10) + QUANT_LEVEL_ADJUST_Q10;
                q1_q10 += offset_q10;
                q2_q10 = q1_q10 + 1024;
                rd1_q10 = smulbb(-q1_q10, lambda_q10);
                rd2_q10 = smulbb(-q2_q10, lambda_q10);
            }
            let mut rr_q10 = r_q10 - q1_q10;
            rd1_q10 = (rd1_q10 + rr_q10 * rr_q10) >> 10;
            rr_q10 = r_q10 - q2_q10;
            rd2_q10 = (rd2_q10 + rr_q10 * rr_q10) >> 10;

            // Order the candidates best-first; each extends the path's
            // accumulated rate/distortion.
            let ordered = if rd1_q10 < rd2_q10 {
                [(q1_q10, rd1_q10), (q2_q10, rd2_q10)]
            } else {
                [(q2_q10, rd2_q10), (q1_q10, rd1_q10)]
            };
            let mut ss = [NsqSample::ZERO; 2];
            for (cand, (q, rd)) in ordered.iter().enumerate() {
                // Update states for this quantization.
                let mut exc_q14 = q.wrapping_shl(4);
                if ps_dd.seed < 0 {
                    exc_q14 = -exc_q14;
                }

                // Add predictions.
                let lpc_exc_q14 = exc_q14 + ltp_pred_q14;
                let xq_q14 = lpc_exc_q14.wrapping_add(lpc_pred_q14);

                ss[cand].q_q10 = *q;
                ss[cand].rd_q10 = ps_dd.rd_q10.wrapping_add(*rd);
                ss[cand].diff_q14 = xq_q14.wrapping_sub(x.wrapping_shl(4));
                let s_lf_ar_shp_q14 = ss[cand].diff_q14.wrapping_sub(n_ar_q14);
                ss[cand].s_ltp_shp_q14 = s_lf_ar_shp_q14.saturating_sub(n_lf_q14);
                ss[cand].lf_ar_q14 = s_lf_ar_shp_q14;
                ss[cand].lpc_exc_q14 = lpc_exc_q14;
                ss[cand].xq_q14 = xq_q14;
            }
            ps_sample_state[k] = ss;
        }

        *smpl_buf_idx = (*smpl_buf_idx - 1).rem_euclid(DECISION_DELAY as i64);
        let last_smple_idx = (*smpl_buf_idx + decision_delay) % DECISION_DELAY as i64;

        // Find the winner among the paths' best candidates.
        let mut winner_ind = 0usize;
        for k in 1..n_states_delayed_decision {
            if ps_sample_state[k][0].rd_q10 < ps_sample_state[winner_ind][0].rd_q10 {
                winner_ind = k;
            }
        }

        // Increase RD values of expired states: any path whose committed
        // decision `decisionDelay` back disagrees with the winner's is put
        // out of contention (both its candidates).
        let winner_rand_state = del_dec[winner_ind].rand_state[last_smple_idx as usize];
        for k in 0..n_states_delayed_decision {
            if del_dec[k].rand_state[last_smple_idx as usize] != winner_rand_state {
                ps_sample_state[k][0].rd_q10 =
                    ps_sample_state[k][0].rd_q10.saturating_add(i32::MAX >> 4);
                ps_sample_state[k][1].rd_q10 =
                    ps_sample_state[k][1].rd_q10.saturating_add(i32::MAX >> 4);
            }
        }

        // Find worst in the first set and best in the second set.
        let mut rd_max_ind = 0usize;
        let mut rd_min_ind = 0usize;
        for k in 1..n_states_delayed_decision {
            if ps_sample_state[k][0].rd_q10 > ps_sample_state[rd_max_ind][0].rd_q10 {
                rd_max_ind = k;
            }
            if ps_sample_state[k][1].rd_q10 < ps_sample_state[rd_min_ind][1].rd_q10 {
                rd_min_ind = k;
            }
        }

        // Replace a state if best from second set outperforms worst in
        // first set. The reference `memcpy`s the struct skipping its first
        // `i` words — the first `i` samples of `s_lpc_q14` (older history
        // both paths share); every other field is a whole-field copy. (The
        // two indices are always distinct when the branch fires: a path's
        // second-best candidate can never beat its own best.)
        if ps_sample_state[rd_min_ind][1].rd_q10 < ps_sample_state[rd_max_ind][0].rd_q10 {
            let (dst, src) = {
                let split = rd_max_ind.max(rd_min_ind);
                let (left, right) = del_dec.split_at_mut(split);
                if rd_max_ind > rd_min_ind {
                    (&mut right[0], &mut left[rd_min_ind])
                } else {
                    (&mut left[rd_max_ind], &mut right[0])
                }
            };
            dst.s_lpc_q14[i..].copy_from_slice(&src.s_lpc_q14[i..]);
            dst.rand_state = src.rand_state;
            dst.q_q10 = src.q_q10;
            dst.xq_q14 = src.xq_q14;
            dst.pred_q15 = src.pred_q15;
            dst.shape_q14 = src.shape_q14;
            dst.s_ar2_q14 = src.s_ar2_q14;
            dst.lf_ar_q14 = src.lf_ar_q14;
            dst.diff_q14 = src.diff_q14;
            dst.seed = src.seed;
            dst.seed_init = src.seed_init;
            dst.rd_q10 = src.rd_q10;
            ps_sample_state[rd_max_ind][0] = ps_sample_state[rd_min_ind][1];
        }

        // Write samples from the winner to the output and the shared
        // long-term filter states, `decisionDelay` samples delayed. The
        // first `decisionDelay` samples of the first subframe (positive
        // `subfr` counter reset by the subframe-2 rewhite included) are
        // emitted by a later subframe or the frame-final copy.
        if subfr > 0 || i >= decision_delay as usize {
            let pos = (frame_pos as i64 + i as i64 - decision_delay) as usize;
            let ps_dd = &del_dec[winner_ind];
            pulses[pos] = rshift_round(ps_dd.q_q10[last_smple_idx as usize], 10) as i16;
            nsq.xq[ltp_mem_length + pos] = sat16(rshift_round(
                smulww(
                    ps_dd.xq_q14[last_smple_idx as usize],
                    delayed_gain_q10[last_smple_idx as usize],
                ),
                8,
            ));
            nsq.s_ltp_shp_q14[nsq.s_ltp_shp_buf_idx - decision_delay as usize] =
                ps_dd.shape_q14[last_smple_idx as usize];
            s_ltp_q15[nsq.s_ltp_buf_idx - decision_delay as usize] =
                ps_dd.pred_q15[last_smple_idx as usize];
        }
        nsq.s_ltp_shp_buf_idx += 1;
        nsq.s_ltp_buf_idx += 1;

        // Update every path's state with its own best candidate.
        for k in 0..n_states_delayed_decision {
            let ps_ss = ps_sample_state[k][0];
            let ps_dd = &mut del_dec[k];
            ps_dd.lf_ar_q14 = ps_ss.lf_ar_q14;
            ps_dd.diff_q14 = ps_ss.diff_q14;
            ps_dd.s_lpc_q14[NSQ_LPC_BUF_LENGTH + i] = ps_ss.xq_q14;
            ps_dd.xq_q14[*smpl_buf_idx as usize] = ps_ss.xq_q14;
            ps_dd.q_q10[*smpl_buf_idx as usize] = ps_ss.q_q10;
            ps_dd.pred_q15[*smpl_buf_idx as usize] = ps_ss.lpc_exc_q14.wrapping_shl(1);
            ps_dd.shape_q14[*smpl_buf_idx as usize] = ps_ss.s_ltp_shp_q14;
            ps_dd.seed = ps_dd.seed.wrapping_add(rshift_round(ps_ss.q_q10, 10));
            ps_dd.rand_state[*smpl_buf_idx as usize] = ps_dd.seed;
            ps_dd.rd_q10 = ps_ss.rd_q10;
        }
        delayed_gain_q10[*smpl_buf_idx as usize] = gain_q10;
    }

    // Update the LPC states of every path (slide the last
    // NSQ_LPC_BUF_LENGTH committed samples into the history).
    for ps_dd in del_dec.iter_mut() {
        ps_dd
            .s_lpc_q14
            .copy_within(length..length + NSQ_LPC_BUF_LENGTH, 0);
    }
}

/// `silk_nsq_del_dec_scale_states` — per-subframe input scaling with
/// 1/Gain, rewhitened-LTP scaling, and gain-change adjustments across the
/// shared long-term state and every delayed-decision path.
///
/// `subfr` is the raw subframe index `k` — the reference passes `k` here,
/// *not* the quantizer's output-guard counter (which the subframe-2 rewhite
/// resets to 0), so the LTP downscale happens only at subframe 0, exactly
/// as on the decode side.
#[allow(clippy::too_many_arguments)]
fn scale_states_del_dec(
    nsq: &mut NsqState,
    del_dec: &mut [DelDecState],
    x16: &[i16],
    x_sc_q10: &mut [i32],
    s_ltp: &[i16],
    s_ltp_q15: &mut [i32],
    subfr: usize,
    n_states_delayed_decision: usize,
    ltp_scale_q14: i32,
    gains_q16: &[i32; MAX_NB_SUBFR],
    pitch_l: &[i32; MAX_NB_SUBFR],
    signal_type: i8,
    decision_delay: usize,
    ltp_mem_length: usize,
) {
    let lag = pitch_l[subfr] as usize;
    let gain_q16 = gains_q16[subfr];
    let mut inv_gain_q31 = crate::silk::nlsf::inverse32_varq(gain_q16.max(1), 47);

    // Scale input.
    let inv_gain_q26 = rshift_round(inv_gain_q31, 5);
    for (dst, &src) in x_sc_q10.iter_mut().zip(x16.iter()) {
        *dst = smulww(i32::from(src), inv_gain_q26);
    }

    // After rewhitening the LTP state is un-scaled: scale with the full
    // inverse gain (with LTP downscaling on subframe 0).
    if nsq.rewhite_flag {
        if subfr == 0 {
            inv_gain_q31 = smulwb(inv_gain_q31, ltp_scale_q14).wrapping_shl(2);
        }
        for i in nsq.s_ltp_buf_idx - lag - LTP_ORDER / 2..nsq.s_ltp_buf_idx {
            s_ltp_q15[i] = smulwb(inv_gain_q31, i32::from(s_ltp[i]));
        }
    }

    // Adjust for changing gain.
    if gain_q16 != nsq.prev_gain_q16 {
        let gain_adj_q16 = crate::silk::sigproc::div32_varq(nsq.prev_gain_q16, gain_q16, 16);

        // Scale the long-term shaping state.
        for i in nsq.s_ltp_shp_buf_idx - ltp_mem_length..nsq.s_ltp_shp_buf_idx {
            nsq.s_ltp_shp_q14[i] = smulww(gain_adj_q16, nsq.s_ltp_shp_q14[i]);
        }

        // Scale the long-term prediction state — up to the delayed region
        // only: the last `decisionDelay` entries are written by the tree's
        // delayed output and must not be rescaled twice.
        if signal_type == TYPE_VOICED && !nsq.rewhite_flag {
            for st in &mut s_ltp_q15
                [nsq.s_ltp_buf_idx - lag - LTP_ORDER / 2..nsq.s_ltp_buf_idx - decision_delay]
            {
                *st = smulww(gain_adj_q16, *st);
            }
        }

        for ps_dd in del_dec.iter_mut().take(n_states_delayed_decision) {
            // Scale scalar states.
            ps_dd.lf_ar_q14 = smulww(gain_adj_q16, ps_dd.lf_ar_q14);
            ps_dd.diff_q14 = smulww(gain_adj_q16, ps_dd.diff_q14);

            // Scale short-term prediction and shaping states.
            for v in ps_dd.s_lpc_q14[..NSQ_LPC_BUF_LENGTH].iter_mut() {
                *v = smulww(gain_adj_q16, *v);
            }
            for v in ps_dd.s_ar2_q14.iter_mut() {
                *v = smulww(gain_adj_q16, *v);
            }
            for i in 0..DECISION_DELAY {
                ps_dd.pred_q15[i] = smulww(gain_adj_q16, ps_dd.pred_q15[i]);
                ps_dd.shape_q14[i] = smulww(gain_adj_q16, ps_dd.shape_q14[i]);
            }
        }

        nsq.prev_gain_q16 = gain_q16;
    }
}

/// The `nStatesDelayedDecision` and `warping_Q16` columns of
/// `silk_setup_complexity` (`silk/control_codec.c`): 1 state below
/// complexity 2, 2 below 6, 3 below 8, `MAX_DEL_DEC_STATES` at 8..=10;
/// the warped shaping feedback engages at complexity 4. Returns
/// `(n_states_delayed_decision, warping_q16)`.
pub(crate) fn complexity_del_dec_config(complexity: u8, fs_khz: u32) -> (usize, i32) {
    let n_states = match complexity {
        0..=1 => 1,
        2..=5 => 2,
        6..=7 => 3,
        _ => MAX_DEL_DEC_STATES,
    };
    let warping_q16 = if complexity >= 4 {
        crate::silk::noise_shape::warping_q16(fs_khz)
    } else {
        0
    };
    (n_states, warping_q16)
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::silk::decode_indices::TYPE_UNVOICED;
    use crate::silk::noise_shape::{warping_q16, ShapeParams};
    use crate::silk::synthesis::{decode_core, SynthesisState};

    /// A bounded but non-trivial shaping configuration (the plain port's
    /// differential fixture), so the test exercises the AR/LF/harmonic
    /// feedback paths rather than a degenerate all-zero filter.
    fn shape_params() -> ShapeParams {
        let mut ar_q13 = [0i16; MAX_NB_SUBFR * MAX_SHAPE_LPC_ORDER];
        for (i, v) in ar_q13.iter_mut().enumerate() {
            // A gentle, decaying coefficient set (well inside the 3.999 limit).
            *v = (((i % 8) as f32 - 3.5) * 900.0) as i16;
        }
        ShapeParams {
            ar_q13,
            lf_shp_q14: [(-2000i32) << 16 | (0xF000u16 as i16) as i32; MAX_NB_SUBFR],
            tilt_q14: [-1200; MAX_NB_SUBFR],
            harm_shape_gain_q14: [3000; MAX_NB_SUBFR],
            lambda_q10: 1000,
            lambda: 1000.0 / 1024.0,
            coding_quality: 0.5,
        }
    }

    /// The encoder's core contract for the delayed-decision port: given
    /// the same parameters, gains and starting state, the `xq` this port
    /// produces is bit-identical to what `silk_decode_core` reconstructs
    /// from the pulses it emitted **with the seed it transmits** (the
    /// winner's initial dither seed — a wrong seed desyncs the dither and
    /// every sample after it). Sweeps signal type, seed, NLSF
    /// interpolation, state count and warping (the warp only steers which
    /// pulses are chosen, never the reconstruction, so the equality gate
    /// must hold at non-zero warping too).
    #[test]
    fn del_dec_xq_equals_decode_core() {
        let frame = FrameInfo::new(16, 4);
        let frame_length = frame.frame_length();
        let history = frame.ltp_mem_length + frame_length;
        let shape = shape_params();

        for signal_type in [TYPE_UNVOICED, TYPE_VOICED] {
            for seed in [0i8, 2] {
                for nlsf_interp in [4i8, 2] {
                    for n_states in 1..=MAX_DEL_DEC_STATES {
                        for warping in [0, warping_q16(16)] {
                            let mut ctrl = DecoderControl::default();
                            for (k, g) in ctrl.gains_q16.iter_mut().enumerate().take(4) {
                                *g = if k == 2 { 4_000_000 } else { 1_500_000 };
                            }
                            for v in ctrl.pred_coef_q12[1].iter_mut().take(16) {
                                *v = -1200;
                            }
                            // `decode_parameters` only builds a distinct
                            // `PredCoef_Q12[0]` when the frame interpolates
                            // the NLSFs; otherwise it copies `[1]`, and the
                            // encoder's `(k >> 1) | (1 - NLSFInterp)`
                            // selection must agree.
                            ctrl.pred_coef_q12[0] = ctrl.pred_coef_q12[1];
                            if nlsf_interp < 4 {
                                for v in ctrl.pred_coef_q12[0].iter_mut().take(16) {
                                    *v += 300;
                                }
                            }
                            if signal_type == TYPE_VOICED {
                                for k in 0..4 {
                                    ctrl.pitch_l[k] = 100 + 10 * k as i32;
                                }
                                for (i, v) in ctrl.ltp_coef_q14[..4 * 5].iter_mut().enumerate() {
                                    *v = [0, 1000, 8000, 1000, 0][i % 5];
                                }
                            }
                            ctrl.ltp_scale_q14 = 15565;

                            let mut indices = SideInfoIndices {
                                signal_type,
                                quant_offset_type: 1,
                                seed,
                                nlsf_interp_coef_q2: nlsf_interp,
                                ..SideInfoIndices::default()
                            };

                            // Warm history so the LPC state, the shaped-LTP
                            // feedback and the gain-change rescaling are live.
                            let mut state = NsqState::default();
                            state.s_lpc_q14[..NSQ_LPC_BUF_LENGTH]
                                .copy_from_slice(&[1234; NSQ_LPC_BUF_LENGTH]);
                            state.prev_gain_q16 = 900_000;
                            for (i, v) in state.xq[..history].iter_mut().enumerate() {
                                *v = ((i as f32) * 7.0).sin() as i16 * 40;
                            }
                            for (i, v) in state.s_ltp_shp_q14[..history].iter_mut().enumerate() {
                                *v = ((i as f32) * 3.0).cos() as i32 * 500;
                            }
                            // The frame slides `xq` at its end, so the
                            // decoder's mirror of the starting history is
                            // captured before the call.
                            let xq_history = state.xq;

                            let x: Vec<i16> = (0..frame_length)
                                .map(|i| (2000.0 * (i as f32 * 0.05).sin()) as i16)
                                .collect();

                            let res = nsq_del_dec(
                                &mut state,
                                &mut indices,
                                &x,
                                &ctrl,
                                &frame,
                                &shape.ar_q13,
                                &shape.lf_shp_q14,
                                &shape.tilt_q14,
                                &shape.harm_shape_gain_q14,
                                shape.lambda_q10,
                                i32::from(ctrl.ltp_scale_q14),
                                &ctrl.gains_q16,
                                n_states,
                                warping,
                            );

                            // Decoder side: same parameters, same starting
                            // state, fed the pulses and the TRANSMITTED seed.
                            let mut dec = SynthesisState::default();
                            dec.s_lpc_q14_buf[..NSQ_LPC_BUF_LENGTH]
                                .copy_from_slice(&[1234; NSQ_LPC_BUF_LENGTH]);
                            dec.prev_gain_q16 = 900_000;
                            // Only the `ltp_mem_length` samples of
                            // reconstruction history are shared.
                            dec.out_buf[..frame.ltp_mem_length]
                                .copy_from_slice(&xq_history[..frame.ltp_mem_length]);
                            let mut dec_ctrl = ctrl;
                            let mut xq_dec = [0i16; MAX_FRAME_LENGTH];
                            let mut exc_dec = [0i32; MAX_FRAME_LENGTH];
                            decode_core(
                                &mut dec,
                                &mut dec_ctrl,
                                &indices,
                                &res.pulses,
                                &mut xq_dec,
                                &mut exc_dec,
                                &frame,
                                0,
                                0,
                                0,
                            );

                            assert_eq!(
                                &res.xq[..frame_length],
                                &xq_dec[..frame_length],
                                "signal_type {signal_type}, seed {seed}, interp {nlsf_interp}, \
                                 n_states {n_states}, warping {warping}"
                            );
                            // The transmitted seed is the winner's initial
                            // seed: one of the (k + seed) & 3 path seeds.
                            let path = (indices.seed - seed) & 3;
                            assert!(
                                (0..n_states as i8).contains(&path),
                                "transmitted seed must be one of the paths' initial seeds"
                            );
                        }
                    }
                }
            }
        }
    }
}
