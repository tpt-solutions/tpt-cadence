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
    if std::env::var_os("SILK_C_FS_DEBUG").is_some() {
        eprintln!(
            "CORE fs={} nb_subfr={} sigType={} NLSFInterp={} lagPrev={} prevGain={}",
            frame.subfr_length / 5,
            frame.nb_subfr,
            indices.signal_type,
            indices.nlsf_interp_coef_q2,
            lag_prev,
            state.prev_gain_q16
        );
        eprintln!(
            "CORE gains_Q16={},{},{},{}",
            ctrl.gains_q16[0], ctrl.gains_q16[1], ctrl.gains_q16[2], ctrl.gains_q16[3]
        );
        eprintln!(
            "CORE pitchL={},{},{},{} predcoef1[0..4]={},{},{},{}",
            ctrl.pitch_l[0],
            ctrl.pitch_l[1],
            ctrl.pitch_l[2],
            ctrl.pitch_l[3],
            ctrl.pred_coef_q12[1][0],
            ctrl.pred_coef_q12[1][1],
            ctrl.pred_coef_q12[1][2],
            ctrl.pred_coef_q12[1][3]
        );
    }
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
        a_q12_tmp[..frame.lpc_order]
            .copy_from_slice(&ctrl.pred_coef_q12[k >> 1][..frame.lpc_order]);
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
            let tap_base = s_ltp_buf_idx - lag + LTP_ORDER / 2;
            for i in 0..frame.subfr_length {
                /* The reference walks a pred_lag_ptr here; the same
                 * taps relative to the write cursor are tap_base + i.
                 * The +2 bias avoids introducing a bias because
                 * silk_SMLAWB() always rounds to -inf */
                let tap = tap_base + i;
                let mut ltp_pred_q13 = 2;
                ltp_pred_q13 = smlawb(ltp_pred_q13, s_ltp_q15[tap], b_q14[0] as i32);
                ltp_pred_q13 = smlawb(ltp_pred_q13, s_ltp_q15[tap - 1], b_q14[1] as i32);
                ltp_pred_q13 = smlawb(ltp_pred_q13, s_ltp_q15[tap - 2], b_q14[2] as i32);
                ltp_pred_q13 = smlawb(ltp_pred_q13, s_ltp_q15[tap - 3], b_q14[3] as i32);
                ltp_pred_q13 = smlawb(ltp_pred_q13, s_ltp_q15[tap - 4], b_q14[4] as i32);

                /* Generate LPC excitation */
                res_q14[i] = add_lshift32(exc_q14[exc_base + i], ltp_pred_q13, 1);

                /* Update states */
                s_ltp_q15[s_ltp_buf_idx] = res_q14[i].wrapping_shl(1);
                s_ltp_buf_idx += 1;
            }

            /* Short-term prediction, voiced residual */
            lpc_synthesis_subframe(
                &mut s_lpc_q14,
                &res_q14[..frame.subfr_length],
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
    state
        .s_lpc_q14_buf
        .copy_from_slice(&s_lpc_q14[..MAX_LPC_ORDER]);
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

        /* Add prediction to LPC excitation */
        s_lpc_q14[MAX_LPC_ORDER + i] = add_sat32(pres_q14[i], lshift_sat32(lpc_pred_q10, 4));

        /* Scale with gain */
        xq[i] = sat16(rshift_round(
            smulww(s_lpc_q14[MAX_LPC_ORDER + i], gain_q10),
            8,
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::silk::decode_indices::{SideInfoIndices, TYPE_UNVOICED};

    /// Small deterministic PRNG (xorshift32) so the tests need no
    /// external crate (same generator as the excitation tests).
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
    }

    /* ---- golden hashes against an independent Python transcription
     * of decode_core.c (see the module docs) ---- */

    const SEED_BASE: u32 = 0x5EED_C0DE;
    const N_CASES: usize = 48;

    struct FrameCase {
        indices: SideInfoIndices,
        loss_cnt: i32,
        prev_signal_type: i8,
        lag_prev: i32,
        ctrl: DecoderControl,
        pulses: Vec<i16>,
    }

    /// Mirrors the oracle's `gen_case` exactly (same xorshift32 stream).
    fn gen_case(
        rng: &mut XorShift,
        case_idx: usize,
    ) -> (FrameInfo, SynthesisState, Vec<FrameCase>) {
        let fs_khz = [8usize, 12, 16][(rng.next_u32() % 3) as usize];
        let nb_subfr = 2 + 2 * (rng.next_u32() % 2) as usize;
        let subfr_length = 5 * fs_khz;
        let frame_length = nb_subfr * subfr_length;
        let lpc_order = if fs_khz == 16 { 16 } else { 10 };

        let mut state = SynthesisState::default();
        if case_idx % 4 == 3 {
            for v in state.s_lpc_q14_buf.iter_mut() {
                *v = rng.next_u32() as i32;
            }
            for v in state.out_buf.iter_mut() {
                let u = (rng.next_u32() % 65536) as i32;
                *v = if u >= 1 << 15 {
                    (u - (1 << 16)) as i16
                } else {
                    u as i16
                };
            }
            state.prev_gain_q16 = 81920 + (rng.next_u32() % (1 << 24)) as i32;
        }

        let mut frames = Vec::new();
        for _ in 0..3 {
            let signal_type = (rng.next_u32() % 3) as i8;
            let quant_offset_type = (rng.next_u32() % 2) as i8;
            let nlsf_interp_coef_q2 = (if nb_subfr == 4 { rng.next_u32() % 5 } else { 4 }) as i8;
            let seed = (rng.next_u32() % 4) as i8;
            let loss_cnt = (rng.next_u32() % 3) as i32;
            let prev_signal_type = (rng.next_u32() % 3) as i8;
            let lag_prev = 2 * fs_khz as i32 + (rng.next_u32() % (16 * fs_khz as u32 + 1)) as i32;
            let ltp_scale_q14 = [8192i16, 12288, 15565][(rng.next_u32() % 3) as usize];

            let mut gains_q16 = [0i32; MAX_NB_SUBFR];
            let mut prev_g: Option<i32> = None;
            for g in gains_q16.iter_mut().take(nb_subfr) {
                *g = match prev_g {
                    Some(prev) if rng.next_u32() % 2 == 0 => prev,
                    _ => 81920 + (rng.next_u32() % (1 << 26)) as i32,
                };
                prev_g = Some(*g);
            }

            let mut pred_coef_q12 = [[0i16; MAX_LPC_ORDER]; 2];
            // every third case uses the full i16 range (unstable filters
            // -> saturation paths); the rest a realistic +-8k magnitude
            let coef_max = if case_idx % 3 == 0 { 32768 } else { 8192 } as u32;
            for half in pred_coef_q12.iter_mut().take(2) {
                for v in half.iter_mut().take(lpc_order) {
                    *v = ((rng.next_u32() % (2 * coef_max)) as i32 - coef_max as i32) as i16;
                }
            }

            let mut ltp_coef_q14 = [0i16; LTP_ORDER * MAX_NB_SUBFR];
            for v in ltp_coef_q14[..nb_subfr * LTP_ORDER].iter_mut() {
                let u = (rng.next_u32() % 65536) as i32;
                *v = if u >= 1 << 15 {
                    (u - (1 << 16)) as i16
                } else {
                    u as i16
                };
            }

            let mut pitch_l = [0i32; MAX_NB_SUBFR];
            for p in pitch_l.iter_mut().take(nb_subfr) {
                if signal_type == TYPE_VOICED {
                    *p = 2 * fs_khz as i32 + (rng.next_u32() % (16 * fs_khz as u32 + 1)) as i32;
                }
            }

            let mut pulses = Vec::with_capacity(frame_length);
            for _ in 0..frame_length {
                let r = rng.next_u32() % 16;
                let p: i32 = if r < 8 {
                    0
                } else if r < 12 {
                    let v = (rng.next_u32() % 9) as i32;
                    if rng.next_u32() % 2 == 1 {
                        -v
                    } else {
                        v
                    }
                } else if r < 14 {
                    let v = 1 + (rng.next_u32() % 128) as i32;
                    if rng.next_u32() % 2 == 1 {
                        -v
                    } else {
                        v
                    }
                } else {
                    (rng.next_u32() % 65536) as i32 - 32768
                };
                pulses.push(p as i16);
            }

            frames.push(FrameCase {
                indices: SideInfoIndices {
                    signal_type,
                    quant_offset_type,
                    nlsf_interp_coef_q2,
                    seed,
                    ..SideInfoIndices::default()
                },
                loss_cnt,
                prev_signal_type,
                lag_prev,
                ctrl: DecoderControl {
                    pitch_l,
                    gains_q16,
                    pred_coef_q12,
                    ltp_coef_q14,
                    ltp_scale_q14,
                },
                pulses,
            });
        }
        (FrameInfo::new(fs_khz as u32, nb_subfr), state, frames)
    }

    fn fnv1a64(data: &[u8]) -> u64 {
        let mut h = 0xCBF2_9CE4_8422_2325u64;
        for &b in data {
            h ^= b as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01B3);
        }
        h
    }

    /// (seed, FNV-1a64 over xq (i16 LE) + post-frame pitch_l (i32 LE)
    /// for each of 3 frames, then final s_lpc (i32 LE), out_buf (i16
    /// LE), prev_gain (i32 LE)) — values generated by an independent
    /// Python transcription of decode_core.c. The generator above
    /// mirrors the oracle's case generator sample-for-sample, so any
    /// divergence of the port from the reference C shows up here.
    #[test]
    fn decode_core_matches_libopus_oracle() {
        let table: [(u32, u64); N_CASES] = [
            (SEED_BASE, 0x550EEC856B0CD577),
            (SEED_BASE + 1, 0xAFC6057AE3948E80),
            (SEED_BASE + 2, 0x06E878123DF51A24),
            (SEED_BASE + 3, 0x030596A0A4E77D0A),
            (SEED_BASE + 4, 0xF3D6D88CAF57AE4D),
            (SEED_BASE + 5, 0x9C4450A77DC964B9),
            (SEED_BASE + 6, 0x655D1980CFFC42FD),
            (SEED_BASE + 7, 0xF4119164D478853D),
            (SEED_BASE + 8, 0x88898A92EC5A7A03),
            (SEED_BASE + 9, 0xFC77CB484A4B53BB),
            (SEED_BASE + 10, 0xD9E550880BA2A9C4),
            (SEED_BASE + 11, 0xA412543731FE710C),
            (SEED_BASE + 12, 0x2F718AA1C108A803),
            (SEED_BASE + 13, 0xAD98058E9297B543),
            (SEED_BASE + 14, 0xBF4BF9574BEA0C3E),
            (SEED_BASE + 15, 0x6AB42E283FF07287),
            (SEED_BASE + 16, 0x29B1DEA4D046D8A8),
            (SEED_BASE + 17, 0x9CB1A3ADC47B5593),
            (SEED_BASE + 18, 0xA478CDCE6413E6B7),
            (SEED_BASE + 19, 0xB4DCADB2910F8899),
            (SEED_BASE + 20, 0x5093B87F3EC7E912),
            (SEED_BASE + 21, 0x0DF392383E1F1D46),
            (SEED_BASE + 22, 0x09B639E294DBC2B1),
            (SEED_BASE + 23, 0x8C2A6F0F5CC66831),
            (SEED_BASE + 24, 0xC6B9907B221F7EC3),
            (SEED_BASE + 25, 0x048C95E307C8CA67),
            (SEED_BASE + 26, 0xB70368C3000ED44E),
            (SEED_BASE + 27, 0x52D684334E5AC01C),
            (SEED_BASE + 28, 0x5A2EB1D7979F9E8F),
            (SEED_BASE + 29, 0xAAF823733AB4651D),
            (SEED_BASE + 30, 0x46CAD4CD76E388AD),
            (SEED_BASE + 31, 0x3845A5844EA667FD),
            (SEED_BASE + 32, 0x0FDCBB2B8BA0B367),
            (SEED_BASE + 33, 0xB9B10ED092B54A9D),
            (SEED_BASE + 34, 0x703AD900AADC4B2C),
            (SEED_BASE + 35, 0xDB35DEFF26FF7200),
            (SEED_BASE + 36, 0x4D33471C89F83A10),
            (SEED_BASE + 37, 0xB72F38CA3CFEE485),
            (SEED_BASE + 38, 0xA7766E59AC0332F8),
            (SEED_BASE + 39, 0x4F916ADE0701DEBB),
            (SEED_BASE + 40, 0x973D3C6843129DF1),
            (SEED_BASE + 41, 0x147C5FED3662580F),
            (SEED_BASE + 42, 0x51F0F884ACDF0401),
            (SEED_BASE + 43, 0xD3B0BE28817C3E7B),
            (SEED_BASE + 44, 0x769242A9CEC136E8),
            (SEED_BASE + 45, 0x0F813DB264C7DAC2),
            (SEED_BASE + 46, 0x7FEF9F4A9F5DE2B4),
            (SEED_BASE + 47, 0x0EE89754F2F76AB6),
        ];
        for (seed, want) in table {
            let case_idx = (seed - SEED_BASE) as usize;
            let mut rng = XorShift(seed);
            let (frame, mut state, frames) = gen_case(&mut rng, case_idx);
            let mut blob = Vec::new();
            for fc in &frames {
                let mut ctrl = fc.ctrl;
                let mut xq = vec![0i16; frame.frame_length()];
                let mut exc = vec![0i32; frame.frame_length()];
                decode_core(
                    &mut state,
                    &mut ctrl,
                    &fc.indices,
                    &fc.pulses,
                    &mut xq,
                    &mut exc,
                    &frame,
                    fc.loss_cnt,
                    fc.prev_signal_type,
                    fc.lag_prev,
                );
                state.update_out_buf(&xq, frame.ltp_mem_length);
                for &v in &xq {
                    blob.extend_from_slice(&v.to_le_bytes());
                }
                for &v in &ctrl.pitch_l[..frame.nb_subfr] {
                    blob.extend_from_slice(&v.to_le_bytes());
                }
            }
            for &v in &state.s_lpc_q14_buf {
                blob.extend_from_slice(&v.to_le_bytes());
            }
            for &v in &state.out_buf {
                blob.extend_from_slice(&v.to_le_bytes());
            }
            blob.extend_from_slice(&state.prev_gain_q16.to_le_bytes());
            assert_eq!(fnv1a64(&blob), want, "case index {case_idx}");
        }
    }

    /// `(a * b) >> 16` and `((v >> 7) + 1) >> 1` re-implemented with
    /// plain i64/i32 ops for the independent expected-value tests.
    fn mul_shift16(a: i32, b: i32) -> i32 {
        ((a as i64 * b as i64) >> 16) as i32
    }
    fn round_shift8(v: i32) -> i32 {
        ((v >> 7) + 1) >> 1
    }

    /// Unvoiced frame, zero LPC coefficients: the whole voiced/sLTP
    /// machinery must be bypassed, and the output must equal the
    /// independent per-sample formula
    /// `sat16(round((exc + 128) * gain_q10 >> 16, 8))` with `exc` from
    /// the (separately verified) excitation reconstruction.
    #[test]
    fn unvoiced_zero_lpc_matches_closed_form() {
        let frame = FrameInfo::new(16, 2);
        let n = frame.frame_length();
        let mut state = SynthesisState::default();
        let indices = SideInfoIndices {
            signal_type: TYPE_UNVOICED,
            quant_offset_type: 1,
            nlsf_interp_coef_q2: 4,
            seed: 2,
            ..SideInfoIndices::default()
        };
        let mut ctrl = DecoderControl {
            gains_q16: [65536; MAX_NB_SUBFR], // gain_q10 = 1024
            ..DecoderControl::default()       // zero coefficients
        };
        let pulses = vec![0i16; n];
        let mut xq = vec![0i16; n];
        let mut exc = vec![0i32; n];
        decode_core(
            &mut state,
            &mut ctrl,
            &indices,
            &pulses,
            &mut xq,
            &mut exc,
            &frame,
            0,
            TYPE_UNVOICED,
            100,
        );

        let mut exc_exp = vec![0i32; n];
        reconstruct_excitation(
            &mut exc_exp,
            &pulses,
            indices.seed,
            indices.signal_type,
            indices.quant_offset_type,
        );
        for i in 0..n {
            let s_lpc = exc_exp[i].saturating_add(128); // ADD_SAT32 + 8<<4, A = 0
            let want = sat16(round_shift8(mul_shift16(s_lpc, 1024)));
            assert_eq!(xq[i], want, "sample {i}");
        }
        // LPC state carried out is the last 16 filter outputs
        let tail: Vec<i32> = (n - MAX_LPC_ORDER..n)
            .map(|i| exc_exp[i].saturating_add(128))
            .collect();
        assert_eq!(&state.s_lpc_q14_buf[..], &tail[..]);
        // equal gains: no adjustment, prev_gain unchanged (still 65536)
        assert_eq!(state.prev_gain_q16, 65536);
    }

    /// OutBuf pattern used by the voiced hand-trace test.
    fn pattern_state() -> SynthesisState {
        let mut st = SynthesisState::default();
        for (i, v) in st.out_buf.iter_mut().enumerate() {
            *v = ((i * 37) % 251) as i16 - 125;
        }
        st
    }

    /// Voiced frame with a center-tap-only LTP filter and zero LPC
    /// coefficients: verifies the k=0 re-whitening (with A = 0 the
    /// analysis filter passes the outBuf history through), the LTP
    /// downscale, the 5-tap prediction from the Q15 state, and the
    /// residual feedback into that state.
    #[test]
    fn voiced_center_tap_ltp_hand_traced() {
        let frame = FrameInfo::new(16, 4); // ltp_mem 320, subfr 80
        let n = frame.frame_length();
        let lag = 64usize;
        let mut state = pattern_state();
        let indices = SideInfoIndices {
            signal_type: TYPE_VOICED,
            quant_offset_type: 0,
            nlsf_interp_coef_q2: 4, // no interpolation -> no k=2 rewhiten
            seed: 0,
            ..SideInfoIndices::default()
        };
        let mut ctrl = DecoderControl {
            pitch_l: [lag as i32; MAX_NB_SUBFR],
            gains_q16: [65536; MAX_NB_SUBFR],
            pred_coef_q12: [[0; MAX_LPC_ORDER]; 2],
            ltp_coef_q14: [
                0, 0, 4096, 0, 0, // center tap only, per subframe
                0, 0, 4096, 0, 0, //
                0, 0, 4096, 0, 0, //
                0, 0, 4096, 0, 0,
            ],
            ltp_scale_q14: 8192,
        };
        let pulses = vec![0i16; n];
        let mut xq = vec![0i16; n];
        let mut exc = vec![0i32; n];
        decode_core(
            &mut state,
            &mut ctrl,
            &indices,
            &pulses,
            &mut xq,
            &mut exc,
            &frame,
            0,
            TYPE_VOICED,
            100,
        );

        // Independent recomputation (vector level, plain i64 ops):
        let mut exc_exp = vec![0i32; n];
        reconstruct_excitation(
            &mut exc_exp,
            &pulses,
            indices.seed,
            indices.signal_type,
            indices.quant_offset_type,
        );
        let inv = inverse32_varq(65536, 47);
        // k=0 downscale: smulwb(inv, 8192) << 2
        let inv_scaled = mul_shift16(inv, 8192).wrapping_shl(2);
        let mut s_ltp_q15 = vec![0i32; frame.ltp_mem_length + n];
        let mut s_lpc = vec![0i32; MAX_LPC_ORDER + n];
        for i in 0..lag + LTP_ORDER / 2 {
            let idx = frame.ltp_mem_length - i - 1;
            s_ltp_q15[idx] = mul_shift16(inv_scaled, state.out_buf[idx] as i32);
        }
        for k in 0..frame.nb_subfr {
            let base = k * frame.subfr_length;
            for i in 0..frame.subfr_length {
                let cursor = frame.ltp_mem_length + base + i;
                // center tap: pred = 2 + smulwb(s[cursor - lag], 4096)
                let ltp_pred = 2 + mul_shift16(s_ltp_q15[cursor - lag], 4096);
                let res = exc_exp[base + i].wrapping_add(ltp_pred.wrapping_shl(1));
                s_ltp_q15[cursor] = res.wrapping_shl(1);
                let s_val = res.saturating_add(128);
                s_lpc[MAX_LPC_ORDER + base + i] = s_val;
                let want = sat16(round_shift8(mul_shift16(s_val, 1024)));
                assert_eq!(xq[base + i], want, "subframe {k} sample {i}");
            }
        }
        // final LPC state is the tail of the filter outputs
        assert_eq!(
            &state.s_lpc_q14_buf[..],
            &s_lpc[s_lpc.len() - MAX_LPC_ORDER..]
        );

        // A different LTP scale must change the output from the very
        // first sample (the k=0 downscale participates directly).
        let mut state2 = pattern_state();
        let mut ctrl2 = ctrl;
        ctrl2.ltp_scale_q14 = 15565;
        let mut xq2 = vec![0i16; n];
        let mut exc2 = vec![0i32; n];
        decode_core(
            &mut state2,
            &mut ctrl2,
            &indices,
            &pulses,
            &mut xq2,
            &mut exc2,
            &frame,
            0,
            TYPE_VOICED,
            100,
        );
        assert_ne!(xq[0], xq2[0], "LTP scale must affect subframe 0");
    }

    /// The voiced-PLC transition fix-up: with losses immediately after
    /// a voiced frame, subframes 0–1 of an unvoiced frame must get the
    /// previous lag written into `pitch_l` (observable through the
    /// `lagPrev` update), while all other conditions leave the decoded
    /// lags alone.
    #[test]
    fn voiced_plc_transition_overrides_pitch_l() {
        let frame = FrameInfo::new(8, 4);
        let n = frame.frame_length();
        let indices = SideInfoIndices {
            signal_type: TYPE_UNVOICED,
            ..SideInfoIndices::default()
        };
        let pulses = vec![0i16; n];

        let run = |loss_cnt: i32, prev_signal_type: i8, lag_prev: i32| {
            let mut state = SynthesisState::default();
            // unity gains (zero gains are outside the dequantizer's
            // contract and would assert in the reference too)
            let mut ctrl = DecoderControl {
                gains_q16: [65536; MAX_NB_SUBFR],
                ..DecoderControl::default()
            };
            let mut xq = vec![0i16; n];
            let mut exc = vec![0i32; n];
            decode_core(
                &mut state,
                &mut ctrl,
                &indices,
                &pulses,
                &mut xq,
                &mut exc,
                &frame,
                loss_cnt,
                prev_signal_type,
                lag_prev,
            );
            ctrl.pitch_l
        };

        assert_eq!(run(3, TYPE_VOICED, 123), [123, 123, 0, 0]);
        // (lag_prev stays within the 2–18 ms legal range for fs = 8)
        assert_eq!(run(1, TYPE_VOICED, 88), [88, 88, 0, 0]);
        assert_eq!(run(0, TYPE_VOICED, 123), [0, 0, 0, 0]);
        assert_eq!(run(3, TYPE_UNVOICED, 123), [0, 0, 0, 0]);

        // A voiced frame is not touched (its lags were decoded anyway).
        let indices_v = SideInfoIndices {
            signal_type: TYPE_VOICED,
            ..SideInfoIndices::default()
        };
        let mut state = SynthesisState::default();
        let mut ctrl = DecoderControl {
            pitch_l: [31, 32, 33, 34],
            gains_q16: [65536; MAX_NB_SUBFR],
            ..DecoderControl::default()
        };
        let mut xq = vec![0i16; n];
        let mut exc = vec![0i32; n];
        decode_core(
            &mut state,
            &mut ctrl,
            &indices_v,
            &pulses,
            &mut xq,
            &mut exc,
            &frame,
            3,
            TYPE_VOICED,
            99,
        );
        assert_eq!(ctrl.pitch_l, [31, 32, 33, 34]);
    }

    /// The `outBuf` slide-and-append performed by `silk_decode_frame`.
    #[test]
    fn update_out_buf_slides_and_appends() {
        let mut st = SynthesisState::default();
        for (i, v) in st.out_buf.iter_mut().enumerate() {
            *v = (i as i16).wrapping_mul(3) - 1000;
        }
        let before: Vec<i16> = st.out_buf.to_vec();
        let ltp_mem = 240;
        let xq: Vec<i16> = (0..80).map(|i| 5000 + i as i16).collect();
        st.update_out_buf(&xq, ltp_mem);
        assert_eq!(&st.out_buf[..ltp_mem - 80], &before[80..ltp_mem]);
        assert_eq!(&st.out_buf[ltp_mem - 80..ltp_mem], &xq[..]);
        // everything beyond ltp_mem is scratch and must be untouched
        assert_eq!(&st.out_buf[ltp_mem..], &before[ltp_mem..]);

        // frame_length == ltp_mem: plain overwrite, no slide
        let xq_full: Vec<i16> = (0..ltp_mem).map(|i| -7 - i as i16).collect();
        st.update_out_buf(&xq_full, ltp_mem);
        assert_eq!(&st.out_buf[..ltp_mem], &xq_full[..]);
    }

    /// `silk_reset_decoder` state: everything zeroed except
    /// `prev_gain_Q16 = 65536`.
    #[test]
    fn default_state_is_reset_state() {
        let mut st = SynthesisState::default();
        assert_eq!(st.prev_gain_q16, 65536);
        assert!(st.s_lpc_q14_buf == [0; MAX_LPC_ORDER]);
        assert!(st.out_buf == [0; MAX_FRAME_LENGTH + 2 * MAX_SUB_FRAME_LENGTH]);
        st.prev_gain_q16 = 1;
        st.reset();
        assert_eq!(st.prev_gain_q16, 65536);
    }

    /// Frame geometry derivation (`silk_decoder_set_fs` lengths).
    #[test]
    fn frame_info_geometry() {
        let f = FrameInfo::new(8, 4);
        assert_eq!(
            (
                f.subfr_length,
                f.frame_length(),
                f.lpc_order,
                f.ltp_mem_length
            ),
            (40, 160, 10, 160)
        );
        let f = FrameInfo::new(12, 2);
        assert_eq!(
            (
                f.subfr_length,
                f.frame_length(),
                f.lpc_order,
                f.ltp_mem_length
            ),
            (60, 120, 10, 240)
        );
        let f = FrameInfo::new(16, 4);
        assert_eq!(
            (
                f.subfr_length,
                f.frame_length(),
                f.lpc_order,
                f.ltp_mem_length
            ),
            (80, 320, 16, 320)
        );
    }

    /// No-panic smoke over full-range garbage in every geometry,
    /// including heavy loss sequences (guards slice-index regressions;
    /// the arithmetic is total by construction).
    #[test]
    fn never_panics_on_garbage() {
        let mut rng = XorShift(0xAB1E_C0DE);
        for &fs_khz in &[8u32, 12, 16] {
            for nb_subfr in [2usize, 4] {
                let frame = FrameInfo::new(fs_khz, nb_subfr);
                let n = frame.frame_length();
                let mut state = SynthesisState::default();
                for frame_no in 0..4 {
                    let mut ctrl = DecoderControl {
                        ltp_scale_q14: 15565,
                        ..DecoderControl::default()
                    };
                    // keep gains in the legal dequantized range, nonzero
                    for g in ctrl.gains_q16.iter_mut() {
                        *g = 81920 + (rng.next_u32() % (1 << 26)) as i32;
                    }
                    for p in ctrl.pitch_l.iter_mut() {
                        *p = (rng.next_u32() % (18 * fs_khz)) as i32;
                    }
                    for v in ctrl.pred_coef_q12.iter_mut().flatten() {
                        *v = rng.next_u32() as i16;
                    }
                    for v in ctrl.ltp_coef_q14.iter_mut() {
                        *v = rng.next_u32() as i16;
                    }
                    let indices = SideInfoIndices {
                        signal_type: (rng.next_u32() % 3) as i8,
                        quant_offset_type: (rng.next_u32() % 2) as i8,
                        nlsf_interp_coef_q2: (rng.next_u32() % 5) as i8,
                        seed: (rng.next_u32() % 4) as i8,
                        ..SideInfoIndices::default()
                    };
                    let pulses: Vec<i16> = (0..n).map(|_| rng.next_u32() as i16).collect();
                    let mut xq = vec![0i16; n];
                    let mut exc = vec![0i32; n];
                    let loss_cnt = if frame_no % 2 == 0 { 0 } else { 5 };
                    let prev_signal_type = (rng.next_u32() % 3) as i8;
                    let lag_prev = (rng.next_u32() % (18 * fs_khz)) as i32;
                    decode_core(
                        &mut state,
                        &mut ctrl,
                        &indices,
                        &pulses,
                        &mut xq,
                        &mut exc,
                        &frame,
                        loss_cnt,
                        prev_signal_type,
                        lag_prev,
                    );
                    state.update_out_buf(&xq, frame.ltp_mem_length);
                }
            }
        }
    }
}
