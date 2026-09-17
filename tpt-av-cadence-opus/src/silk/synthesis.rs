//! SILK inverse noise-shaping quantization, long-term prediction and
//! short-term (LPC) synthesis — port of `silk_decode_core`
//! (`silk/decode_core.c`).
//!
//! Per frame, [`decode_core`]:
//!
//! 1. reconstructs the Q14 excitation from the quantization indices
//!    (delegated to [`excitation::reconstruct_excitation`], which owns
//!    the ±`QUANT_LEVEL_ADJUST_Q10` pull, the signal-type/offset-dependent
//!    `offset_Q10`, and the seed-dithered sign flip),
//! 2. loops over the 5 ms subframes, and per subframe computes the
//!    gain-scaling bookkeeping (`inv_gain_Q31` via `silk_INVERSE32_varQ`,
//!    the `silk_DIV32_varQ` gain-change factor that rescales the LPC and
//!    LTP states so the state stays gain-consistent across subframe
//!    boundaries),
//! 3. for voiced subframes: re-whitens the LTP memory through the LPC
//!    analysis filter (at subframe 0 and — when NLSF interpolation is in
//!    effect — again at subframe 2, where the second half-frame's
//!    coefficients take over), applies the LTP scale, accumulates the
//!    5-tap long-term prediction from the Q15 LTP state, and feeds the
//!    sum of excitation and LTP prediction back into that state,
//! 4. runs the 10th/16th-order LPC synthesis filter over the (possibly
//!    LTP-predicted) residual in Q14, saturating the accumulator, and
//!    quantizes the output with the subframe gain.
//!
//! Also here: the "avoid abrupt transition from voiced PLC to unvoiced
//! normal decoding" fix-up, which for the first half of a frame
//! immediately after losses substitutes a center-tap-only LTP filter and
//! the previous frame's lag; note it *mutates* [`DecoderControl::pitch_l`],
//! which is observable because `silk_decode_frame` feeds
//! `pitchL[nb_subfr - 1]` back into `lagPrev` (and non-voiced frames
//! zero `pitchL`, so the fix-up preserves the lag across them).
//!
//! Persistent state ([`SynthesisState`]) mirrors the subset of
//! `silk_decoder_state` this module reads/writes: the LPC filter state,
//! the rolling output buffer used for re-whitening, and the previous
//! subframe's gain. The Q14 excitation buffer is caller-owned (PLC/CNG
//! read it after `decode_core` in the reference).
//!
//! SOURCE: Xiph.Org libopus 1.5.2, `silk/decode_core.c`,
//! `silk/decode_frame.c`, `silk/Inlines.h` (`silk_INVERSE32_varQ`,
//! `silk_DIV32_varQ`), `silk/LPC_analysis_filter.c`, `silk/define.h`,
//! `silk/structs.h` (BSD-3-Clause); cross-checked against
//! RFC 6716 §4.2.8.
#![allow(dead_code)]

use crate::silk::decode_indices::{SideInfoIndices, MAX_LPC_ORDER, MAX_NB_SUBFR, TYPE_VOICED};
use crate::silk::excitation::reconstruct_excitation;
use crate::silk::nlsf::inverse32_varq;
use crate::silk::pitch::LTP_ORDER;
use crate::silk::sigproc::{
    add_lshift32, add_sat32, div32_varq, lpc_analysis_filter, lshift_sat32, rshift_round, sat16,
    smlawb, smulwb, smulww,
};

/// `SUB_FRAME_LENGTH_MS` (`silk/define.h`).
pub(crate) const SUB_FRAME_LENGTH_MS: usize = 5;
/// `MAX_FRAME_LENGTH_MS = SUB_FRAME_LENGTH_MS * MAX_NB_SUBFR`.
pub(crate) const MAX_FRAME_LENGTH_MS: usize = SUB_FRAME_LENGTH_MS * MAX_NB_SUBFR;
/// `MAX_FS_KHZ` (`silk/define.h`).
pub(crate) const MAX_FS_KHZ: usize = 16;
/// `MAX_FRAME_LENGTH`: longest SILK frame, 20 ms at 16 kHz.
pub(crate) const MAX_FRAME_LENGTH: usize = MAX_FRAME_LENGTH_MS * MAX_FS_KHZ;
/// `MAX_SUB_FRAME_LENGTH`: longest 5 ms subframe, at 16 kHz.
pub(crate) const MAX_SUB_FRAME_LENGTH: usize = SUB_FRAME_LENGTH_MS * MAX_FS_KHZ;
/// `LTP_MEM_LENGTH_MS` (`silk/define.h`): the LTP memory is always
/// 20 ms, i.e. `20 * fs_kHz` samples.
pub(crate) const LTP_MEM_LENGTH_MS: usize = 20;

/// `SILK_FIX_CONST(0.25, 14)`: the center tap substituted by the
/// voiced-PLC transition fix-up.
const LTP_PLCCENTER_Q14: i32 = 4096;

/// Frame geometry — the `silk_decoder_state` fields `silk_decode_core`
/// reads, as set by `silk_decoder_set_fs`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FrameInfo {
    /// Subframes per frame: 4 (20 ms) or 2 (10 ms).
    pub nb_subfr: usize,
    /// Subframe length in samples: 5 ms at the internal rate.
    pub subfr_length: usize,
    /// LPC order: 16 at 16 kHz, 10 at 8/12 kHz.
    pub lpc_order: usize,
    /// LTP memory length in samples: `LTP_MEM_LENGTH_MS * fs_kHz`.
    pub ltp_mem_length: usize,
}

impl FrameInfo {
    /// Derives the geometry for `fs_kHz` and `nb_subfr` — the length
    /// computations of `silk_decoder_set_fs`.
    pub(crate) fn new(fs_khz: u32, nb_subfr: usize) -> Self {
        debug_assert!(
            fs_khz == 8 || fs_khz == 12 || fs_khz == 16,
            "unsupported internal rate {fs_khz}"
        );
        debug_assert!(nb_subfr == MAX_NB_SUBFR || nb_subfr == MAX_NB_SUBFR / 2);
        let subfr_length = SUB_FRAME_LENGTH_MS * fs_khz as usize;
        FrameInfo {
            nb_subfr,
            subfr_length,
            lpc_order: if fs_khz == 16 { 16 } else { 10 },
            ltp_mem_length: LTP_MEM_LENGTH_MS * fs_khz as usize,
        }
    }

    /// `psDec->frame_length`.
    pub(crate) fn frame_length(&self) -> usize {
        self.nb_subfr * self.subfr_length
    }
}

/// Per-frame synthesis control data — mirrors `silk_decoder_control`
/// (`silk/structs.h`), as produced by the Tier 2 modules (pitch lags and
/// LTP filters from [`pitch`], gains from [`gains`], LPC coefficients
/// from [`nlsf`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct DecoderControl {
    /// Per-subframe pitch lag in samples (`psDecCtrl->pitchL`); zeroed
    /// for non-voiced frames by `silk_decode_parameters`.
    pub pitch_l: [i32; MAX_NB_SUBFR],
    /// Per-subframe Q16 gains (`psDecCtrl->Gains_Q16`).
    pub gains_q16: [i32; MAX_NB_SUBFR],
    /// Interpolated LPC coefficients per half-frame in Q12
    /// (`psDecCtrl->PredCoef_Q12`): subframes 0–1 use `[0]`, subframes
    /// 2–3 use `[1]`. Only `lpc_order` entries are meaningful.
    pub pred_coef_q12: [[i16; MAX_LPC_ORDER]; 2],
    /// Per-subframe 5-tap LTP filters in Q14 (`psDecCtrl->LTPCoef_Q14`).
    pub ltp_coef_q14: [i16; LTP_ORDER * MAX_NB_SUBFR],
    /// LTP gain scaling in Q14 (`psDecCtrl->LTP_scale_Q14`).
    pub ltp_scale_q14: i16,
}

/// Synthesis state carried between frames — the subset of
/// `silk_decoder_state` that `silk_decode_core` touches (plus the
/// `outBuf` update `silk_decode_frame` performs after it). `Default` is
/// the `silk_init_decoder`/`silk_reset_decoder` state: everything
/// zeroed except `prev_gain_Q16 = 65536` (so the first frame's gain
/// change is detected relative to unity).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SynthesisState {
    /// `psDec->sLPC_Q14_buf`: the last `MAX_LPC_ORDER` samples of the
    /// LPC synthesis filter, in Q14, carried across frames.
    pub s_lpc_q14_buf: [i32; MAX_LPC_ORDER],
    /// `psDec->outBuf`: the last `ltp_mem_length` decoded samples
    /// followed by up to two subframes of scratch used by the subframe-2
    /// re-whitening.
    pub out_buf: [i16; MAX_FRAME_LENGTH + 2 * MAX_SUB_FRAME_LENGTH],
    /// `psDec->prev_gain_Q16`: gain of the previous subframe (carried
    /// across frames); drives the gain-change rescaling.
    pub prev_gain_q16: i32,
}

impl Default for SynthesisState {
    fn default() -> Self {
        SynthesisState {
            s_lpc_q14_buf: [0; MAX_LPC_ORDER],
            out_buf: [0; MAX_FRAME_LENGTH + 2 * MAX_SUB_FRAME_LENGTH],
            prev_gain_q16: 65536,
        }
    }
}

impl SynthesisState {
    /// The `silk_reset_decoder` state.
    pub(crate) fn reset(&mut self) {
        *self = SynthesisState::default();
    }

    /// The `outBuf` update at the end of `silk_decode_frame` (both its
    /// normal and lost branches): slide the buffer left by the frame
    /// length and append the freshly decoded frame, keeping the last
    /// `ltp_mem_length` samples.
    pub(crate) fn update_out_buf(&mut self, xq: &[i16], ltp_mem_length: usize) {
        let frame_length = xq.len();
        debug_assert!(ltp_mem_length >= frame_length);
        debug_assert!(self.out_buf.len() >= ltp_mem_length);
        debug_assert!(frame_length > 0);
        self.out_buf.copy_within(frame_length..ltp_mem_length, 0);
        self.out_buf[ltp_mem_length - frame_length..ltp_mem_length].copy_from_slice(xq);
    }
}

/// Core decoder: inverse NSQ, LTP and LPC synthesis — port of
/// `silk_decode_core`.
///
/// Writes `frame.frame_length()` decoded samples to `xq`, fills
/// `exc_q14[..frame_length]` with the Q14 excitation (retained by the
/// caller for PLC/CNG), and updates [`SynthesisState`]. `pulses` holds
/// the decoded quantization indices padded to a shell-block boundary;
/// only the first `frame_length` are read. `loss_cnt` /
/// `prev_signal_type` / `lag_prev` are the pre-frame `psDec->lossCnt` /
/// `psDec->prevSignalType` / `psDec->lagPrev` values.
///
/// `ctrl` is `&mut` because the voiced-PLC transition fix-up overrides
/// `pitch_l[k]` (observable via the `lagPrev` update in
/// `silk_decode_frame`); `ltp_coef_q14` is overridden only through a
/// local copy, matching the reference's read pattern.
#[allow(clippy::too_many_arguments)]
pub(crate) fn decode_core(
    state: &mut SynthesisState,
    ctrl: &mut DecoderControl,
    indices: &SideInfoIndices,
    pulses: &[i16],
    xq: &mut [i16],
    exc_q14: &mut [i32],
    frame: &FrameInfo,
    loss_cnt: i32,
    prev_signal_type: i8,
    lag_prev: i32,
) {
    debug_assert!(state.prev_gain_q16 != 0);
    let frame_length = frame.frame_length();
    debug_assert!(xq.len() >= frame_length && pulses.len() >= frame_length);
    debug_assert!(exc_q14.len() >= frame_length);

    let mut s_ltp = [0i16; MAX_FRAME_LENGTH];
    let mut s_ltp_q15 = [0i32; MAX_FRAME_LENGTH + MAX_FRAME_LENGTH];
    let mut res_q14 = [0i32; MAX_SUB_FRAME_LENGTH];
    let mut s_lpc_q14 = [0i32; MAX_SUB_FRAME_LENGTH + MAX_LPC_ORDER];

    let nlsf_interpolation_flag = i32::from(indices.nlsf_interp_coef_q2 < 1 << 2);

    /* Decode excitation */
    reconstruct_excitation(
        &mut exc_q14[..frame_length],
        pulses,
        indices.seed,
        indices.signal_type,
        indices.quant_offset_type,
    );

    /* Copy LPC state */
    s_lpc_q14[..MAX_LPC_ORDER].copy_from_slice(&state.s_lpc_q14_buf);

    let mut s_ltp_buf_idx = frame.ltp_mem_length;
    let mut exc_base = 0;
    let mut xq_base = 0;

    /* Loop over subframes */
    for k in 0..frame.nb_subfr {
        /* Preload LPC coefficients to an array on the stack (and do the
         * same for the LTP filter, which the reference addresses in
         * place but only ever reads for the current subframe) */
        let mut a_q12_tmp = [0i16; MAX_LPC_ORDER];
        a_q12_tmp[..frame.lpc_order].copy_from_slice(&ctrl.pred_coef_q12[k >> 1][..frame.lpc_order]);
        let mut b_q14 = [0i16; LTP_ORDER];
        b_q14.copy_from_slice(&ctrl.ltp_coef_q14[k * LTP_ORDER..(k + 1) * LTP_ORDER]);

        let gain_q10 = ctrl.gains_q16[k] >> 6;
        let mut inv_gain_q31 = inverse32_varq(ctrl.gains_q16[k], 47);

        /* Calculate gain adjustment factor */
        let gain_adj_q16 = if ctrl.gains_q16[k] != state.prev_gain_q16 {
            let adj = div32_varq(state.prev_gain_q16, ctrl.gains_q16[k], 16);

            /* Scale short term state */
            for v in s_lpc_q14[..MAX_LPC_ORDER].iter_mut() {
                *v = smulww(adj, *v);
            }
            adj
        } else {
            1 << 16
        };

        /* Save inv_gain */
        debug_assert!(inv_gain_q31 != 0);
        state.prev_gain_q16 = ctrl.gains_q16[k];

        let mut signal_type = indices.signal_type;

        /* Avoid abrupt transition from voiced PLC to unvoiced normal
         * decoding */
        if loss_cnt != 0
            && prev_signal_type == TYPE_VOICED
            && indices.signal_type != TYPE_VOICED
            && k < MAX_NB_SUBFR / 2
        {
            b_q14 = [0, 0, LTP_PLCCENTER_Q14 as i16, 0, 0];

            signal_type = TYPE_VOICED;
            ctrl.pitch_l[k] = lag_prev;
        }

        if signal_type == TYPE_VOICED {
            /* Voiced */
            let lag = ctrl.pitch_l[k] as usize;

            if k == 0 || (k == 2 && nlsf_interpolation_flag != 0) {
                /* Re-whiten with new A coefs */
                let start_idx = frame.ltp_mem_length - lag - frame.lpc_order - LTP_ORDER / 2;
                debug_assert!(start_idx > 0);

                if k == 2 {
                    state.out_buf
                        [frame.ltp_mem_length..frame.ltp_mem_length + 2 * frame.subfr_length]
                        .copy_from_slice(&xq[..2 * frame.subfr_length]);
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
                    /* Do LTP downscaling to reduce inter-packet
                     * dependency */
                    inv_gain_q31 = smulwb(inv_gain_q31, ctrl.ltp_scale_q14 as i32).wrapping_shl(2);
                }
                for i in 0..lag + LTP_ORDER / 2 {
                    s_ltp_q15[s_ltp_buf_idx - i - 1] =
                        smulwb(inv_gain_q31, s_ltp[frame.ltp_mem_length - i - 1] as i32);
                }
            } else {
                /* Update LTP state when the gain changes */
                if gain_adj_q16 != 1 << 16 {
                    for i in 0..lag + LTP_ORDER / 2 {
                        s_ltp_q15[s_ltp_buf_idx - i - 1] =
                            smulww(gain_adj_q16, s_ltp_q15[s_ltp_buf_idx - i - 1]);
                    }
                }
            }

            /* Long-term prediction */
            let mut pred_lag_ptr = s_ltp_buf_idx - lag + LTP_ORDER / 2;
            for i in 0..frame.subfr_length {
                /* Unrolled loop; the +2 bias avoids introducing a bias
                 * because silk_SMLAWB() always rounds to -inf */
                let mut ltp_pred_q13 = 2;
                ltp_pred_q13 = smlawb(ltp_pred_q13, s_ltp_q15[pred_lag_ptr], b_q14[0] as i32);
                ltp_pred_q13 = smlawb(ltp_pred_q13, s_ltp_q15[pred_lag_ptr - 1], b_q14[1] as i32);
                ltp_pred_q13 = smlawb(ltp_pred_q13, s_ltp_q15[pred_lag_ptr - 2], b_q14[2] as i32);
                ltp_pred_q13 = smlawb(ltp_pred_q13, s_ltp_q15[pred_lag_ptr - 3], b_q14[3] as i32);
                ltp_pred_q13 = smlawb(ltp_pred_q13, s_ltp_q15[pred_lag_ptr - 4], b_q14[4] as i32);
                pred_lag_ptr += 1;

                /* Generate LPC excitation */
                res_q14[i] = add_lshift32(exc_q14[exc_base + i], ltp_pred_q13, 1);

                /* Update states */
                s_ltp_q15[s_ltp_buf_idx] = res_q14[i].wrapping_shl(1);
                s_ltp_buf_idx += 1;
            }

            /* Short-term prediction, voiced residual */
            lpc_synthesis_subframe(
                &mut s_lpc_q14,
                &res_q14,
                &a_q12_tmp,
                frame.lpc_order,
                gain_q10,
                &mut xq[xq_base..xq_base + frame.subfr_length],
            );
        } else {
            /* Short-term prediction, excitation passed through */
            lpc_synthesis_subframe(
                &mut s_lpc_q14,
                &exc_q14[exc_base..exc_base + frame.subfr_length],
                &a_q12_tmp,
                frame.lpc_order,
                gain_q10,
                &mut xq[xq_base..xq_base + frame.subfr_length],
            );
        }

        /* Update LPC filter state */
        s_lpc_q14.copy_within(frame.subfr_length..frame.subfr_length + MAX_LPC_ORDER, 0);
        exc_base += frame.subfr_length;
        xq_base += frame.subfr_length;
    }

    /* Save LPC state */
    state.s_lpc_q14_buf.copy_from_slice(&s_lpc_q14[..MAX_LPC_ORDER]);
}

/// The short-term (LPC) synthesis inner loop of `silk_decode_core` for
/// one subframe: `pres_q14` is either the LTP-predicted residual or the
/// raw excitation (the reference aliases the pointer instead).
#[inline]
fn lpc_synthesis_subframe(
    s_lpc_q14: &mut [i32; MAX_SUB_FRAME_LENGTH + MAX_LPC_ORDER],
    pres_q14: &[i32],
    a_q12: &[i16; MAX_LPC_ORDER],
    lpc_order: usize,
    gain_q10: i32,
    xq: &mut [i16],
) {
    debug_assert!(lpc_order == 10 || lpc_order == 16);
    debug_assert_eq!(pres_q14.len(), xq.len());
    for i in 0..xq.len() {
        /* Short-term prediction; the order/2 bias avoids introducing a
         * bias because silk_SMLAWB() always rounds to -inf */
        let mut lpc_pred_q10 = (lpc_order >> 1) as i32;
        for (j, a) in a_q12[..10].iter().enumerate() {
            lpc_pred_q10 = smlawb(lpc_pred_q10, s_lpc_q14[MAX_LPC_ORDER + i - 1 - j], *a as i32);
        }
        if lpc_order == 16 {
            for (j, a) in a_q12[10..16].iter().enumerate() {
                lpc_pred_q10 = smlawb(lpc_pred_q10, s_lpc_q14[MAX_LPC_ORDER + i - 11 - j], *a as i32);
            }
        }

        /* Add prediction to LPC excitation */
        s_lpc_q14[MAX_LPC_ORDER + i] = add_sat32(pres_q14[i], lshift_sat32(lpc_pred_q10, 4));

        /* Scale with gain */
        xq[i] = sat16(rshift_round(smulww(s_lpc_q14[MAX_LPC_ORDER + i], gain_q10), 8));
    }
}
