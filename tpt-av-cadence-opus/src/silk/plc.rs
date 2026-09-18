//! SILK-side packet loss concealment and comfort noise generation
//! (RFC 6716 §4.2.9).
//!
//! Ports the reference decoder's loss-handling layer:
//!
//! - [`plc_reset`] / [`plc`] / [`plc_update`] / [`plc_conceal`]
//!   (`silk/PLC.c`): on a lost frame, [`plc_conceal`] bandwidth-expands
//!   the previous frame's LPC vector, rewhitens the LTP memory through
//!   the expanded filter ([`lpc_analysis_filter`]), re-synthesizes an
//!   LTP excitation from the least-energetic tail of the previous
//!   frame's quantized excitation plus an LCG noise fill, and runs the
//!   regular Q10 short-term synthesis on top. The pitch lag drifts
//!   upward and the harmonic/noise gains attenuate per lost subframe;
//!   a good frame afterwards is faded in against the concealed energy
//!   by [`plc_glue_frames`].
//! - [`cng_reset`] / [`cng`] (`silk/CNG.c`): tracks a smoothed NLSF
//!   vector, gain, and excitation buffer during inactive (DTX) frames
//!   and adds synthetic comfort noise through an [`nlsf2a`]-derived
//!   synthesis filter whenever a packet is lost.
//!
//! Like [`super::synthesis`], the module owns only the state the
//! reference keeps specifically for concealment ([`PlcState`],
//! [`CngState`]); shared decoder state stays with the caller —
//! [`SynthesisState`] for the LPC/output buffers, the caller-owned
//! `exc_Q14` buffer written by `decode_core`, and the cross-frame
//! scalars (`fs_kHz`, `lossCnt`, `prevSignalType`,
//! `first_frame_after_reset`, `prevNLSF_Q15`) as parameters. The
//! call-sequence contract of `silk_decode_frame` (Tier 4 wires this):
//!
//! 1. [`plc`] with `lost == true` in place of the normal
//!    [`super::synthesis::decode_core`] call (then `loss_cnt += 1`),
//!    or with `lost == false` right after a good `decode_core` (which
//!    also implies `prev_signal_type := indices.signal_type`, as both
//!    `silk_PLC_update` and `silk_decode_frame` set it);
//! 2. [`cng`], with `loss_cnt`/`prev_signal_type` as of the call;
//! 3. [`plc_glue_frames`] with `lost` again (the reference tests
//!    `psDec->lossCnt`, which is zero after good frames).
//!
//! SOURCE: Xiph.Org libopus 1.5.2, `silk/PLC.c`, `silk/PLC.h`,
//! `silk/CNG.c`, `silk/structs.h` (`silk_PLC_struct`,
//! `silk_CNG_struct`) (BSD-3-Clause); cross-checked against RFC 6716
//! §4.2.9 ("Packet loss concealment").
#![allow(dead_code)]

use super::decode_indices::{MAX_LPC_ORDER, MAX_NB_SUBFR, TYPE_NO_VOICE_ACTIVITY, TYPE_VOICED};
use super::nlsf::{bwexpander, inverse32_varq, lpc_inverse_pred_gain, nlsf2a};
use super::pitch::LTP_ORDER;
use super::sigproc::{
    add_sat16, add_sat32, div32, div32_16, lpc_analysis_filter, lshift_sat32, rand, rshift_round,
    sat16, smlawb, smulbb, smultt, smulwb, smulww, sqrt_approx, sub_lshift32, sum_sqr_shift,
};
use super::synthesis::{
    DecoderControl, FrameInfo, SynthesisState, MAX_FRAME_LENGTH, MAX_SUB_FRAME_LENGTH,
};

/// Struct for Packet Loss Concealment — `silk_PLC_struct`
/// (`silk/structs.h`). `Default` is the `silk_InitDecoder` all-zero
/// state (the `enable_deep_plc` field of the reference is omitted: deep
/// PLC is compiled out of this port, like a reference build without
/// `ENABLE_DEEP_PLC`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct PlcState {
    /// Pitch lag to use for voiced concealment, Q8.
    pub pitch_l_q8: i32,
    /// LTP coefficients to use for voiced concealment, Q14.
    pub ltp_coef_q14: [i16; LTP_ORDER],
    /// LPC coefficients of the last good frame, Q12 (bandwidth-expanded
    /// in place on each loss).
    pub prev_lpc_q12: [i16; MAX_LPC_ORDER],
    /// Was the previous frame lost.
    pub last_frame_lost: bool,
    /// Seed for unvoiced signal generation.
    pub rand_seed: i32,
    /// Scaling of unvoiced random signal, Q14.
    pub rand_scale_q14: i16,
    /// Energy of the last concealed frame (`silk_PLC_glue_frames`).
    pub conc_energy: i32,
    /// Shift applied to [`PlcState::conc_energy`].
    pub conc_energy_shift: i32,
    /// LTP scaling of the last good frame, Q14.
    pub prev_ltp_scale_q14: i16,
    /// Last two subframe gains of the last good frame, Q16.
    pub prev_gain_q16: [i32; 2],
    /// Rate the PLC state was last (re)initialized for.
    pub fs_khz: u32,
    /// Subframe count the PLC state was last (re)initialized for.
    pub nb_subfr: usize,
    /// Subframe length the PLC state was last (re)initialized for.
    pub subfr_length: usize,
}

/// Struct for Comfort Noise Generation — `silk_CNG_struct`
/// (`silk/structs.h`); `Default` is the `silk_InitDecoder` all-zero
/// state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CngState {
    /// Rolling excitation history of the highest-gain inactive
    /// subframe, Q14 (`silk_CNG` shifts it per good inactive frame).
    pub cng_exc_buf_q14: [i32; MAX_FRAME_LENGTH],
    /// Smoothed NLSF vector used as the CNG synthesis filter, Q15.
    pub cng_smth_nlsf_q15: [i16; MAX_LPC_ORDER],
    /// LPC synthesis filter state of the last CNG frame, Q14.
    pub cng_synth_state: [i32; MAX_LPC_ORDER],
    /// Smoothed CNG gain, Q16.
    pub cng_smth_gain_q16: i32,
    /// Seed for the CNG random excitation selection.
    pub rand_seed: i32,
    /// Rate the CNG state was last (re)initialized for.
    pub fs_khz: u32,
}

impl Default for CngState {
    fn default() -> Self {
        CngState {
            cng_exc_buf_q14: [0; MAX_FRAME_LENGTH],
            cng_smth_nlsf_q15: [0; MAX_LPC_ORDER],
            cng_synth_state: [0; MAX_LPC_ORDER],
            cng_smth_gain_q16: 0,
            rand_seed: 0,
            fs_khz: 0,
        }
    }
}

/// `NB_ATT` (`silk/PLC.c`): number of attenuation stages.
const NB_ATT: usize = 2;
/// `HARM_ATT_Q15` (`silk/PLC.c`): 0.99, 0.95 — per-loss harmonic (LTP)
/// gain attenuation.
const HARM_ATT_Q15: [i32; NB_ATT] = [32440, 31130];
/// `PLC_RAND_ATTENUATE_V_Q15` (`silk/PLC.c`): 0.95, 0.8 — noise
/// attenuation after voiced frames.
const PLC_RAND_ATTENUATE_V_Q15: [i32; NB_ATT] = [31130, 26214];
/// `PLC_RAND_ATTENUATE_UV_Q15` (`silk/PLC.c`): 0.99, 0.9 — noise
/// attenuation after unvoiced frames.
const PLC_RAND_ATTENUATE_UV_Q15: [i32; NB_ATT] = [32440, 29491];

/// `BWE_COEF` (`silk/PLC.h`) as `SILK_FIX_CONST(0.99, 16)`: the LPC
/// bandwidth expansion applied to the previous frame's filter on every
/// lost frame.
const BWE_COEF_Q16: i32 = 64881;
/// `V_PITCH_GAIN_START_MIN_Q14` (`silk/PLC.h`): 0.7 in Q14.
const V_PITCH_GAIN_START_MIN_Q14: i32 = 11469;
/// `V_PITCH_GAIN_START_MAX_Q14` (`silk/PLC.h`): 0.95 in Q14.
const V_PITCH_GAIN_START_MAX_Q14: i32 = 15565;
/// `MAX_PITCH_LAG_MS` (`silk/PLC.h`).
const MAX_PITCH_LAG_MS: i32 = 18;
/// `RAND_BUF_SIZE` (`silk/PLC.h`): size of the PLC random-noise window
/// into the previous frame's excitation.
const RAND_BUF_SIZE: usize = 128;
/// `RAND_BUF_MASK` (`silk/PLC.h`).
const RAND_BUF_MASK: i32 = (RAND_BUF_SIZE - 1) as i32;
/// `LOG2_INV_LPC_GAIN_HIGH_THRES` (`silk/PLC.h`): 2^3 = 8 dB LPC gain.
const LOG2_INV_LPC_GAIN_HIGH_THRES: u32 = 3;
/// `LOG2_INV_LPC_GAIN_LOW_THRES` (`silk/PLC.h`): 2^8 = 24 dB LPC gain.
const LOG2_INV_LPC_GAIN_LOW_THRES: u32 = 8;
/// `PITCH_DRIFT_FAC_Q16` (`silk/PLC.h`): 0.01 in Q16 — per-subframe
/// upward drift of the concealed pitch lag.
const PITCH_DRIFT_FAC_Q16: i32 = 655;

/// `CNG_BUF_MASK_MAX` (`silk/define.h`): 2^floor(log2(MAX_FRAME_LENGTH))
/// - 1.
const CNG_BUF_MASK_MAX: i32 = 255;
/// `CNG_GAIN_SMTH_Q16` (`silk/define.h`): 0.25^(1/4).
const CNG_GAIN_SMTH_Q16: i32 = 4634;
/// `CNG_GAIN_SMTH_THRESHOLD_Q16` (`silk/define.h`): -3 dB.
const CNG_GAIN_SMTH_THRESHOLD_Q16: i32 = 46396;
/// `CNG_NLSF_SMTH_Q16` (`silk/define.h`): 0.25.
const CNG_NLSF_SMTH_Q16: i32 = 16348;

/// `silk_PLC_Reset` (`silk/PLC.c`): reset the PLC sub-state. Runs
/// whenever the decoder's rate differs from the rate the PLC state was
/// last initialized for — which includes the first call after
/// `silk_InitDecoder`, where every byte of state is zero.
pub(crate) fn plc_reset(ps_plc: &mut PlcState, frame_length: usize) {
    ps_plc.pitch_l_q8 = (frame_length as i32) << (8 - 1);
    ps_plc.prev_gain_q16[0] = 1 << 16; // SILK_FIX_CONST(1, 16)
    ps_plc.prev_gain_q16[1] = 1 << 16;
    ps_plc.subfr_length = 20;
    ps_plc.nb_subfr = 2;
}

/// `silk_PLC` (`silk/PLC.c`): top-level PLC dispatch. With `lost`, fills
/// `frame` (the first `frame_length` samples) with a concealed
/// continuation of the previous frame — the caller then increments
/// `loss_cnt`, exactly as `silk_PLC` increments `psDec->lossCnt` —
/// and otherwise snapshots the just-decoded frame's parameters for a
/// future loss (also setting `prev_signal_type` to the current frame's
/// signal type, as `silk_PLC_update` does).
#[allow(clippy::too_many_arguments)]
pub(crate) fn plc(
    ps_plc: &mut PlcState,
    state: &mut SynthesisState,
    ctrl: &mut DecoderControl,
    exc_q14: &[i32],
    frame: &mut [i16],
    frame_info: &FrameInfo,
    fs_khz: u32,
    loss_cnt: i32,
    prev_signal_type: i8,
    first_frame_after_reset: bool,
    lost: bool,
) {
    // PLC control function
    if fs_khz != ps_plc.fs_khz {
        plc_reset(ps_plc, frame_info.frame_length());
        ps_plc.fs_khz = fs_khz;
    }

    if lost {
        /****************************/
        /* Generate Signal          */
        /****************************/
        plc_conceal(
            ps_plc,
            state,
            ctrl,
            exc_q14,
            frame,
            frame_info,
            fs_khz,
            loss_cnt,
            prev_signal_type,
            first_frame_after_reset,
        );
    } else {
        /****************************/
        /* Update state             */
        /****************************/
        plc_update(ps_plc, ctrl, frame_info, fs_khz, prev_signal_type);
    }
}

/**************************************************/
/* Update state of PLC                            */
/**************************************************/
fn plc_update(
    ps_plc: &mut PlcState,
    ctrl: &DecoderControl,
    frame_info: &FrameInfo,
    fs_khz: u32,
    signal_type: i8,
) {
    let nb_subfr = frame_info.nb_subfr;
    let subfr_length = frame_info.subfr_length;
    let lpc_order = frame_info.lpc_order;

    // Update parameters used in case of packet loss (the reference also
    // assigns psDec->prevSignalType here; the caller owns that scalar
    // and decode_frame sets it unconditionally afterwards).
    let mut ltp_gain_q14: i32 = 0;
    if signal_type == TYPE_VOICED {
        // Find the parameters for the last subframe which contains a
        // pitch pulse
        let mut j: usize = 0;
        while j * subfr_length < ctrl.pitch_l[nb_subfr - 1] as usize {
            if j == nb_subfr {
                break;
            }
            let mut temp_ltp_gain_q14: i32 = 0;
            for coef in
                &ctrl.ltp_coef_q14[(nb_subfr - 1 - j) * LTP_ORDER..(nb_subfr - j) * LTP_ORDER]
            {
                temp_ltp_gain_q14 += *coef as i32;
            }
            if temp_ltp_gain_q14 > ltp_gain_q14 {
                ltp_gain_q14 = temp_ltp_gain_q14;
                // Copied here as in the reference, although the memset
                // below immediately overwrites everything but the
                // center tap.
                let src = (nb_subfr - 1 - j) * LTP_ORDER;
                ps_plc
                    .ltp_coef_q14
                    .copy_from_slice(&ctrl.ltp_coef_q14[src..src + LTP_ORDER]);

                ps_plc.pitch_l_q8 = ctrl.pitch_l[nb_subfr - 1 - j] << 8;
            }
            j += 1;
        }

        ps_plc.ltp_coef_q14 = [0; LTP_ORDER];
        ps_plc.ltp_coef_q14[LTP_ORDER / 2] = ltp_gain_q14 as i16;

        // Limit LT coefs
        if ltp_gain_q14 < V_PITCH_GAIN_START_MIN_Q14 {
            let tmp = V_PITCH_GAIN_START_MIN_Q14 << 10;
            let scale_q10 = div32(tmp, ltp_gain_q14.max(1));
            for coef in &mut ps_plc.ltp_coef_q14 {
                *coef = (smulbb(*coef as i32, scale_q10) >> 10) as i16;
            }
        } else if ltp_gain_q14 > V_PITCH_GAIN_START_MAX_Q14 {
            let tmp = V_PITCH_GAIN_START_MAX_Q14 << 14;
            let scale_q14 = div32(tmp, ltp_gain_q14.max(1));
            for coef in &mut ps_plc.ltp_coef_q14 {
                *coef = (smulbb(*coef as i32, scale_q14) >> 14) as i16;
            }
        }
    } else {
        ps_plc.pitch_l_q8 = smulbb(fs_khz as i32, 18) << 8;
        ps_plc.ltp_coef_q14 = [0; LTP_ORDER];
    }

    // Save LPC coeficients
    ps_plc.prev_lpc_q12[..lpc_order].copy_from_slice(&ctrl.pred_coef_q12[1][..lpc_order]);
    ps_plc.prev_ltp_scale_q14 = ctrl.ltp_scale_q14;

    // Save last two gains
    ps_plc
        .prev_gain_q16
        .copy_from_slice(&ctrl.gains_q16[nb_subfr - 2..nb_subfr]);

    ps_plc.subfr_length = subfr_length;
    ps_plc.nb_subfr = nb_subfr;
}

/// `silk_PLC_energy` (`silk/PLC.c`): scales the last two subframes of
/// the previous excitation back to signal level and measures their
/// energies; the quieter one becomes the PLC random-noise source.
fn plc_energy(
    exc_q14: &[i32],
    prev_gain_q10: &[i32; 2],
    subfr_length: usize,
    nb_subfr: usize,
) -> (i32, u32, i32, u32) {
    debug_assert!(subfr_length <= MAX_SUB_FRAME_LENGTH);
    let mut exc_buf = [0i16; 2 * MAX_SUB_FRAME_LENGTH];
    // Find random noise component
    // Scale previous excitation signal
    for k in 0..2 {
        for i in 0..subfr_length {
            exc_buf[k * subfr_length + i] = sat16(
                smulww(
                    exc_q14[i + (k + nb_subfr - 2) * subfr_length],
                    prev_gain_q10[k],
                ) >> 8,
            );
        }
    }
    // Find the subframe with lowest energy of the last two and use that
    // as random noise generator
    let (energy1, shift1) = sum_sqr_shift(&exc_buf[..subfr_length]);
    let (energy2, shift2) = sum_sqr_shift(&exc_buf[subfr_length..2 * subfr_length]);
    (energy1, shift1, energy2, shift2)
}

/// `silk_PLC_conceal` (`silk/PLC.c`): generate one concealed frame.
#[allow(clippy::too_many_arguments)]
fn plc_conceal(
    ps_plc: &mut PlcState,
    state: &mut SynthesisState,
    ctrl: &mut DecoderControl,
    exc_q14: &[i32],
    frame: &mut [i16],
    frame_info: &FrameInfo,
    fs_khz: u32,
    loss_cnt: i32,
    prev_signal_type: i8,
    first_frame_after_reset: bool,
) {
    let lpc_order = frame_info.lpc_order;
    let ltp_mem_length = frame_info.ltp_mem_length;
    let frame_length = frame_info.frame_length();
    debug_assert!(frame.len() >= frame_length);

    // Working buffers: the combined LTP memory + frame excitation
    // (`sLTP_Q14`) and the whitened LTP memory (`sLTP`). The entries
    // below `idx + LPC_order` are never read (the reference leaves them
    // uninitialized).
    let mut sltp_q14 = [0i32; MAX_FRAME_LENGTH + MAX_FRAME_LENGTH];
    let mut sltp = [0i16; MAX_FRAME_LENGTH];

    let prev_gain_q10: [i32; 2] = [ps_plc.prev_gain_q16[0] >> 6, ps_plc.prev_gain_q16[1] >> 6];

    if first_frame_after_reset {
        ps_plc.prev_lpc_q12 = [0; MAX_LPC_ORDER];
    }

    let (energy1, shift1, energy2, shift2) = plc_energy(
        exc_q14,
        &prev_gain_q10,
        frame_info.subfr_length,
        frame_info.nb_subfr,
    );

    let rand_ptr_base = if (energy1 >> shift2) < (energy2 >> shift1) {
        // First sub-frame has lowest energy
        ((ps_plc.nb_subfr - 1) * ps_plc.subfr_length).saturating_sub(RAND_BUF_SIZE)
    } else {
        // Second sub-frame has lowest energy
        (ps_plc.nb_subfr * ps_plc.subfr_length).saturating_sub(RAND_BUF_SIZE)
    };

    // Set up Gain to random noise component
    let mut b_q14: [i16; LTP_ORDER] = ps_plc.ltp_coef_q14;
    let mut rand_scale_q14: i16 = ps_plc.rand_scale_q14;

    // Set up attenuation gains
    let harm_gain_q15 = HARM_ATT_Q15[(NB_ATT - 1).min(loss_cnt.max(0) as usize)];
    let mut rand_gain_q15 = if prev_signal_type == TYPE_VOICED {
        PLC_RAND_ATTENUATE_V_Q15[(NB_ATT - 1).min(loss_cnt.max(0) as usize)]
    } else {
        PLC_RAND_ATTENUATE_UV_Q15[(NB_ATT - 1).min(loss_cnt.max(0) as usize)]
    };

    // LPC concealment. Apply BWE to previous LPC
    bwexpander(&mut ps_plc.prev_lpc_q12[..lpc_order], BWE_COEF_Q16);

    // Preload LPC coeficients to array on stack. Gives small performance gain
    let mut a_q12 = [0i16; MAX_LPC_ORDER];
    a_q12[..lpc_order].copy_from_slice(&ps_plc.prev_lpc_q12[..lpc_order]);

    // First Lost frame
    if loss_cnt == 0 {
        rand_scale_q14 = 1 << 14;

        // Reduce random noise Gain for voiced frames
        if prev_signal_type == TYPE_VOICED {
            for coef in &b_q14 {
                rand_scale_q14 = rand_scale_q14.wrapping_sub(*coef);
            }
            rand_scale_q14 = rand_scale_q14.max(3277); /* 0.2 */
            rand_scale_q14 =
                (smulbb(rand_scale_q14 as i32, ps_plc.prev_ltp_scale_q14 as i32) >> 14) as i16;
        } else {
            // Reduce random noise for unvoiced frames with high LPC gain
            let inv_gain_q30 = lpc_inverse_pred_gain(&ps_plc.prev_lpc_q12, lpc_order);

            let mut down_scale_q30 = (1i32 << 30) >> LOG2_INV_LPC_GAIN_HIGH_THRES;
            down_scale_q30 = down_scale_q30.min(inv_gain_q30);
            down_scale_q30 = ((1i32 << 30) >> LOG2_INV_LPC_GAIN_LOW_THRES).max(down_scale_q30);
            down_scale_q30 <<= LOG2_INV_LPC_GAIN_HIGH_THRES;

            rand_gain_q15 = smulwb(down_scale_q30, rand_gain_q15) >> 14;
        }
    }

    let mut rand_seed: i32 = ps_plc.rand_seed;
    let mut lag: i32 = rshift_round(ps_plc.pitch_l_q8, 8);
    let mut sltp_buf_idx: usize = ltp_mem_length;

    // Rewhiten LTP state
    let idx = ltp_mem_length - lag as usize - lpc_order - LTP_ORDER / 2;
    debug_assert!(idx > 0);
    lpc_analysis_filter(
        &mut sltp[idx..ltp_mem_length],
        &state.out_buf[idx..ltp_mem_length],
        &a_q12[..lpc_order],
        lpc_order,
    );
    // Scale LTP state
    let mut inv_gain_q30 = inverse32_varq(ps_plc.prev_gain_q16[1], 46);
    inv_gain_q30 = inv_gain_q30.min(i32::MAX >> 1);
    for i in idx + lpc_order..ltp_mem_length {
        sltp_q14[i] = smulwb(inv_gain_q30, sltp[i] as i32);
    }

    /***************************/
    /* LTP synthesis filtering */
    /***************************/
    for _k in 0..frame_info.nb_subfr {
        // Set up pointer
        let pred_lag = sltp_buf_idx - lag as usize + LTP_ORDER / 2;
        for p in 0..frame_info.subfr_length {
            // Unrolled loop
            // Avoids introducing a bias because silk_SMLAWB() always
            // rounds to -inf
            let mut ltp_pred_q12: i32 = 2;
            ltp_pred_q12 = smlawb(ltp_pred_q12, sltp_q14[pred_lag + p], b_q14[0] as i32);
            ltp_pred_q12 = smlawb(ltp_pred_q12, sltp_q14[pred_lag + p - 1], b_q14[1] as i32);
            ltp_pred_q12 = smlawb(ltp_pred_q12, sltp_q14[pred_lag + p - 2], b_q14[2] as i32);
            ltp_pred_q12 = smlawb(ltp_pred_q12, sltp_q14[pred_lag + p - 3], b_q14[3] as i32);
            ltp_pred_q12 = smlawb(ltp_pred_q12, sltp_q14[pred_lag + p - 4], b_q14[4] as i32);

            // Generate LPC excitation
            rand_seed = rand(rand_seed);
            let noise_idx = ((rand_seed >> 25) & RAND_BUF_MASK) as usize;
            sltp_q14[sltp_buf_idx] = smlawb(
                ltp_pred_q12,
                exc_q14[rand_ptr_base + noise_idx],
                rand_scale_q14 as i32,
            )
            .wrapping_shl(2);
            sltp_buf_idx += 1;
        }

        // Gradually reduce LTP gain
        for coef in &mut b_q14 {
            *coef = (smulbb(harm_gain_q15, *coef as i32) >> 15) as i16;
        }
        // Gradually reduce excitation gain
        rand_scale_q14 = (smulbb(rand_scale_q14 as i32, rand_gain_q15) >> 15) as i16;

        // Slowly increase pitch lag
        ps_plc.pitch_l_q8 = smlawb(ps_plc.pitch_l_q8, ps_plc.pitch_l_q8, PITCH_DRIFT_FAC_Q16);
        ps_plc.pitch_l_q8 = ps_plc
            .pitch_l_q8
            .min(smulbb(MAX_PITCH_LAG_MS, fs_khz as i32) << 8);
        lag = rshift_round(ps_plc.pitch_l_q8, 8);
    }

    /***************************/
    /* LPC synthesis filtering */
    /***************************/
    let slpc_base = ltp_mem_length - MAX_LPC_ORDER;

    // Copy LPC state
    sltp_q14[slpc_base..ltp_mem_length].copy_from_slice(&state.s_lpc_q14_buf);

    debug_assert!(lpc_order >= 10); /* check that unrolling works */
    for i in 0..frame_length {
        // partly unrolled
        // Avoids introducing a bias because silk_SMLAWB() always rounds
        // to -inf
        let mut lpc_pred_q10: i32 = (lpc_order >> 1) as i32;
        for (j, a) in a_q12[..lpc_order].iter().enumerate() {
            lpc_pred_q10 = smlawb(
                lpc_pred_q10,
                sltp_q14[ltp_mem_length + i - j - 1],
                *a as i32,
            );
        }

        // Add prediction to LPC excitation
        sltp_q14[ltp_mem_length + i] =
            add_sat32(sltp_q14[ltp_mem_length + i], lshift_sat32(lpc_pred_q10, 4));

        // Scale with Gain
        frame[i] = sat16(sat16(rshift_round(
            smulww(sltp_q14[ltp_mem_length + i], prev_gain_q10[1]),
            8,
        )) as i32);
    }

    // Save LPC state
    state.s_lpc_q14_buf.copy_from_slice(
        &sltp_q14[slpc_base + frame_length..slpc_base + frame_length + MAX_LPC_ORDER],
    );

    /**************************************/
    /* Update states                      */
    /**************************************/
    ps_plc.rand_seed = rand_seed;
    ps_plc.rand_scale_q14 = rand_scale_q14;
    ps_plc.ltp_coef_q14 = b_q14;
    ctrl.pitch_l = [lag; MAX_NB_SUBFR];
}

/// `silk_PLC_glue_frames` (`silk/PLC.c`): glues concealed frames with
/// new good received frames by fading in the good frame's energy
/// relative to the concealed frame's. `lost` mirrors the reference's
/// `psDec->lossCnt != 0` test at the call site (lossCnt is zero on
/// every good frame).
pub(crate) fn plc_glue_frames(ps_plc: &mut PlcState, frame: &mut [i16], length: usize, lost: bool) {
    debug_assert!(frame.len() >= length);
    if lost {
        // Calculate energy in concealed residual
        let (energy, shift) = sum_sqr_shift(&frame[..length]);
        ps_plc.conc_energy = energy;
        ps_plc.conc_energy_shift = shift as i32;

        ps_plc.last_frame_lost = true;
    } else {
        if ps_plc.last_frame_lost {
            // Calculate residual in decoded signal if last frame was lost
            let (mut energy, energy_shift) = sum_sqr_shift(&frame[..length]);

            // Normalize energies
            let conc_energy_shift = ps_plc.conc_energy_shift;
            if (energy_shift as i32) > conc_energy_shift {
                ps_plc.conc_energy >>= (energy_shift as i32) - conc_energy_shift;
            } else if (energy_shift as i32) < conc_energy_shift {
                energy >>= conc_energy_shift - (energy_shift as i32);
            }

            // Fade in the energy difference
            if energy > ps_plc.conc_energy {
                let mut lz = ps_plc.conc_energy.leading_zeros() as i32;
                lz -= 1;
                ps_plc.conc_energy = ps_plc.conc_energy.wrapping_shl(lz as u32);
                energy >>= 0.max(24 - lz);

                let frac_q24 = div32(ps_plc.conc_energy, energy.max(1));

                let mut gain_q16 = sqrt_approx(frac_q24).wrapping_shl(4);
                let mut slope_q16 = div32_16((1 << 16) - gain_q16, length as i32);
                // Make slope 4x steeper to avoid missing onsets after DTX
                slope_q16 <<= 2;

                for v in &mut frame[..length] {
                    *v = smulwb(gain_q16, *v as i32) as i16;
                    gain_q16 = gain_q16.wrapping_add(slope_q16);
                    if gain_q16 > 1 << 16 {
                        break;
                    }
                }
            }
        }
        ps_plc.last_frame_lost = false;
    }
}

/// `silk_CNG_Reset` (`silk/CNG.c`): initialize the comfort-noise state
/// with a flat-spectrum NLSF ramp and zero gain.
pub(crate) fn cng_reset(ps_cng: &mut CngState, lpc_order: usize) {
    let nlsf_step_q15 = div32_16(i16::MAX as i32, lpc_order as i32 + 1);
    let mut nlsf_acc_q15: i32 = 0;
    for i in 0..lpc_order {
        nlsf_acc_q15 += nlsf_step_q15;
        ps_cng.cng_smth_nlsf_q15[i] = nlsf_acc_q15 as i16;
    }
    ps_cng.cng_smth_gain_q16 = 0;
    ps_cng.rand_seed = 3176576;
}

/// `silk_CNG_exc` (`silk/CNG.c`): random-selection resampler over the
/// CNG excitation buffer, driven by the LCG.
fn cng_exc(exc_q14: &mut [i32], exc_buf_q14: &[i32], rand_seed: &mut i32) {
    let length = exc_q14.len();
    let mut exc_mask = CNG_BUF_MASK_MAX;
    while exc_mask > length as i32 {
        exc_mask >>= 1;
    }

    let mut seed = *rand_seed;
    for slot in exc_q14.iter_mut() {
        seed = rand(seed);
        let idx = ((seed >> 24) & exc_mask) as usize;
        *slot = exc_buf_q14[idx];
    }
    *rand_seed = seed;
}

/// `silk_CNG` (`silk/CNG.c`): updates the CNG estimate after a good
/// inactive frame, and adds comfort noise to `frame` after lost
/// packets. `loss_cnt` and `prev_signal_type` are the decoder scalars
/// as of the `silk_CNG` call site (i.e. after a good frame: zero and
/// the current frame's type; after a loss: nonzero and the last good
/// frame's type). The noise level derives from the PLC state's saved
/// random scale and gain (`psDec->sPLC` in the reference).
#[allow(clippy::too_many_arguments)]
pub(crate) fn cng(
    ps_cng: &mut CngState,
    ps_plc: &PlcState,
    ctrl: &DecoderControl,
    exc_q14: &[i32],
    frame: &mut [i16],
    length: usize,
    frame_info: &FrameInfo,
    fs_khz: u32,
    loss_cnt: i32,
    prev_signal_type: i8,
    prev_nlsf_q15: &[i16],
) {
    debug_assert!(frame.len() >= length);
    debug_assert!(prev_nlsf_q15.len() >= frame_info.lpc_order);
    if fs_khz != ps_cng.fs_khz {
        // Reset state
        cng_reset(ps_cng, frame_info.lpc_order);

        ps_cng.fs_khz = fs_khz;
    }
    if loss_cnt == 0 && prev_signal_type == TYPE_NO_VOICE_ACTIVITY {
        let lpc_order = frame_info.lpc_order;
        let nb_subfr = frame_info.nb_subfr;
        let subfr_length = frame_info.subfr_length;

        // Update CNG parameters

        // Smoothing of LSF's
        for (smth, &prev) in ps_cng.cng_smth_nlsf_q15[..lpc_order]
            .iter_mut()
            .zip(&prev_nlsf_q15[..lpc_order])
        {
            *smth = (*smth as i32 + smulwb(prev as i32 - *smth as i32, CNG_NLSF_SMTH_Q16)) as i16;
        }
        // Find the subframe with the highest gain
        let mut max_gain_q16: i32 = 0;
        let mut subfr: usize = 0;
        for (i, gain) in ctrl.gains_q16.iter().take(nb_subfr).enumerate() {
            if *gain > max_gain_q16 {
                max_gain_q16 = *gain;
                subfr = i;
            }
        }
        // Update CNG excitation buffer with excitation from this subframe
        ps_cng
            .cng_exc_buf_q14
            .copy_within(0..(nb_subfr - 1) * subfr_length, subfr_length);
        ps_cng.cng_exc_buf_q14[..subfr_length]
            .copy_from_slice(&exc_q14[subfr * subfr_length..subfr * subfr_length + subfr_length]);

        // Smooth gains
        for gain in ctrl.gains_q16.iter().take(nb_subfr) {
            ps_cng.cng_smth_gain_q16 += smulwb(gain - ps_cng.cng_smth_gain_q16, CNG_GAIN_SMTH_Q16);
            // If the smoothed gain is 3 dB greater than this subframe's
            // gain, use this subframe's gain to adapt faster.
            if smulww(ps_cng.cng_smth_gain_q16, CNG_GAIN_SMTH_THRESHOLD_Q16) > *gain {
                ps_cng.cng_smth_gain_q16 = *gain;
            }
        }
    }

    // Add CNG when packet is lost or during DTX
    if loss_cnt != 0 {
        let lpc_order = frame_info.lpc_order;

        // Generate CNG excitation
        let mut gain_q16 = smulww(ps_plc.rand_scale_q14 as i32, ps_plc.prev_gain_q16[1]);
        if gain_q16 >= (1 << 21) || ps_cng.cng_smth_gain_q16 > (1 << 23) {
            gain_q16 = smultt(gain_q16, gain_q16);
            gain_q16 = sub_lshift32(
                smultt(ps_cng.cng_smth_gain_q16, ps_cng.cng_smth_gain_q16),
                gain_q16,
                5,
            );
            gain_q16 = sqrt_approx(gain_q16).wrapping_shl(16);
        } else {
            gain_q16 = smulww(gain_q16, gain_q16);
            gain_q16 = sub_lshift32(
                smulww(ps_cng.cng_smth_gain_q16, ps_cng.cng_smth_gain_q16),
                gain_q16,
                5,
            );
            gain_q16 = sqrt_approx(gain_q16).wrapping_shl(8);
        }
        let gain_q10 = gain_q16 >> 6;

        let mut cng_sig_q14 = [0i32; MAX_FRAME_LENGTH + MAX_LPC_ORDER];

        // Generate CNG excitation
        cng_exc(
            &mut cng_sig_q14[MAX_LPC_ORDER..MAX_LPC_ORDER + length],
            &ps_cng.cng_exc_buf_q14,
            &mut ps_cng.rand_seed,
        );

        // Convert CNG NLSF to filter representation
        let mut a_q12 = [0i16; MAX_LPC_ORDER];
        nlsf2a(
            &mut a_q12[..lpc_order],
            &ps_cng.cng_smth_nlsf_q15[..lpc_order],
            lpc_order,
        );

        // Generate CNG signal, by synthesis filtering
        cng_sig_q14[..MAX_LPC_ORDER].copy_from_slice(&ps_cng.cng_synth_state);
        debug_assert!(lpc_order == 10 || lpc_order == 16);
        for i in 0..length {
            // Avoids introducing a bias because silk_SMLAWB() always
            // rounds to -inf
            let mut lpc_pred_q10: i32 = (lpc_order >> 1) as i32;
            for (j, a) in a_q12[..10].iter().enumerate() {
                lpc_pred_q10 = smlawb(
                    lpc_pred_q10,
                    cng_sig_q14[MAX_LPC_ORDER + i - 1 - j],
                    *a as i32,
                );
            }
            if lpc_order == 16 {
                for (j, a) in a_q12[10..16].iter().enumerate() {
                    lpc_pred_q10 = smlawb(
                        lpc_pred_q10,
                        cng_sig_q14[MAX_LPC_ORDER + i - 11 - j],
                        *a as i32,
                    );
                }
            }

            // Update states
            cng_sig_q14[MAX_LPC_ORDER + i] = add_sat32(
                cng_sig_q14[MAX_LPC_ORDER + i],
                lshift_sat32(lpc_pred_q10, 4),
            );

            // Scale with Gain and add to input signal
            frame[i] = add_sat16(
                frame[i] as i32,
                sat16(rshift_round(
                    smulww(cng_sig_q14[MAX_LPC_ORDER + i], gain_q10),
                    8,
                )) as i32,
            );
        }
        ps_cng
            .cng_synth_state
            .copy_from_slice(&cng_sig_q14[length..length + MAX_LPC_ORDER]);
    } else {
        let lpc_order = frame_info.lpc_order;
        ps_cng.cng_synth_state[..lpc_order].fill(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::silk::synthesis::{DecoderControl as Ctrl, FrameInfo};

    /// Small deterministic PRNG (xorshift32) so the tests need no
    /// external crate (same generator as the other silk tests).
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

    const SEED_BASE: u32 = 0x5EED_FA11;
    const N_CASES: usize = 32;
    const FRAMES_PER_CASE: usize = 6;
    const MAX_SUBFRS: usize = MAX_NB_SUBFR;

    struct FrameCase {
        lost: bool,
        signal_type: i8,
        ctrl: Ctrl,
        xq: Vec<i16>,
        exc: Vec<i32>,
        prev_nlsf: Vec<i16>,
    }

    struct Case {
        info: FrameInfo,
        plc: PlcState,
        cng: CngState,
        state: SynthesisState,
        prev_signal_type: i8,
        first_frame_after_reset: bool,
        frames: Vec<FrameCase>,
    }

    /// The Q14 excitation draw of the oracle: mostly realistic Q14
    /// magnitudes (|x| < 2^24), every tenth sample full-range i32.
    fn exc_draw(rng: &mut XorShift) -> i32 {
        let r = rng.next_u32() % 10;
        if r < 8 {
            (rng.next_u32() % (1 << 25)) as i32 - (1 << 24)
        } else {
            rng.next_u32() as i32
        }
    }

    /// Mirrors the oracle's `gen_case` exactly (same xorshift32 stream,
    /// same draw order, same value ranges).
    fn gen_case(rng: &mut XorShift, case_idx: usize) -> Case {
        let fs = [8u32, 12, 16][(rng.next_u32() % 3) as usize];
        let nb_subfr = 2 + 2 * (rng.next_u32() % 2) as usize;
        let subfr_length = 5 * fs as usize;
        let frame_length = nb_subfr * subfr_length;
        let lpc_order = if fs == 16 { 16 } else { 10 };
        let coef_max: u32 = if case_idx % 3 == 0 { 32768 } else { 8192 };

        let i16_of = |u: u32| -> i16 { (u as u16) as i16 };

        // --- PlcState
        let plc_fs_khz = if case_idx % 2 == 0 { fs } else { 0 };
        let pitch_l_q8 = ((2 * fs as i32) + (rng.next_u32() % (16 * fs + 1)) as i32) << 8;
        let ltp_coef: Vec<i16> = (0..5).map(|_| i16_of(rng.next_u32() % 65536)).collect();
        let prev_lpc: Vec<i16> = (0..16)
            .map(|_| (rng.next_u32() % (2 * coef_max)) as i32 - coef_max as i32)
            .map(|v| v as i16)
            .collect();
        let last_frame_lost = (rng.next_u32() % 2) == 1;
        let rand_seed = rng.next_u32() as i32;
        let rand_scale_q14 = i16_of(rng.next_u32() % 65536);
        let conc_energy = rng.next_u32() as i32;
        let conc_energy_shift = (rng.next_u32() % 8) as i32;
        let prev_ltp_scale_q14 = [8192i16, 12288, 15565][(rng.next_u32() % 3) as usize];
        let pg0 = 81920 + (rng.next_u32() % (1 << 26)) as i32;
        let pg1 = 81920 + (rng.next_u32() % (1 << 26)) as i32;
        let plc_subfr_length = [20usize, 40, 60, 80][(rng.next_u32() % 4) as usize];
        let plc_nb_subfr = 2 + 2 * (rng.next_u32() % 2) as usize;

        // --- CngState
        let cng_fs_khz = if case_idx % 2 == 0 { fs } else { 0 };
        let cng_exc_vec: Vec<i32> = (0..MAX_FRAME_LENGTH).map(|_| exc_draw(rng)).collect();
        let mut cng_nlsf: Vec<i16> = (0..16).map(|_| (rng.next_u32() % 32768) as i16).collect();
        cng_nlsf.sort_unstable();
        let cng_synth: Vec<i32> = (0..16).map(|_| rng.next_u32() as i32).collect();
        let cng_smth_gain = (rng.next_u32() % (1 << 26)) as i32;
        let cng_rand_seed = rng.next_u32() as i32;

        // --- synthesis state
        let s_lpc: Vec<i32> = (0..16).map(|_| rng.next_u32() as i32).collect();
        let out_buf: Vec<i16> = (0..MAX_FRAME_LENGTH + 2 * MAX_SUB_FRAME_LENGTH)
            .map(|_| i16_of(rng.next_u32() % 65536))
            .collect();

        // --- scalars
        let prev_signal_type = (rng.next_u32() % 3) as i8;
        let _lag_prev = 2 * fs as i32 + (rng.next_u32() % (16 * fs + 1)) as i32;
        let first_frame_after_reset = (rng.next_u32() % 2) == 0;

        let mut frames = Vec::with_capacity(FRAMES_PER_CASE);
        for f in 0..FRAMES_PER_CASE {
            let mut lost = (rng.next_u32() % 5) < 2;
            if f == 1 {
                lost = true;
            }
            if f == 2 {
                lost = false;
            }
            let signal_type = (rng.next_u32() % 3) as i8;
            let _qot = rng.next_u32() % 2;

            let mut pitch_l = [0i32; MAX_SUBFRS];
            for p in pitch_l.iter_mut().take(nb_subfr) {
                if signal_type == TYPE_VOICED {
                    *p = 2 * fs as i32 + (rng.next_u32() % (16 * fs + 1)) as i32;
                }
            }
            let mut gains_q16 = [0i32; MAX_SUBFRS];
            for g in gains_q16.iter_mut().take(nb_subfr) {
                *g = 81920 + (rng.next_u32() % (1 << 26)) as i32;
            }
            let draw_coef = |rng: &mut XorShift| -> [i16; MAX_LPC_ORDER] {
                let mut a = [0i16; MAX_LPC_ORDER];
                for v in a.iter_mut() {
                    *v = ((rng.next_u32() % (2 * coef_max)) as i32 - coef_max as i32) as i16;
                }
                a
            };
            let pred0 = draw_coef(rng);
            let pred1 = draw_coef(rng);
            let mut ltp_coef_q14 = [0i16; LTP_ORDER * MAX_SUBFRS];
            for v in ltp_coef_q14[..nb_subfr * LTP_ORDER].iter_mut() {
                *v = i16_of(rng.next_u32() % 65536);
            }
            let ltp_scale_q14 = [8192i16, 12288, 15565][(rng.next_u32() % 3) as usize];
            let xq: Vec<i16> = (0..frame_length)
                .map(|_| i16_of(rng.next_u32() % 65536))
                .collect();
            let exc: Vec<i32> = (0..MAX_FRAME_LENGTH + 2 * MAX_SUB_FRAME_LENGTH)
                .map(|_| exc_draw(rng))
                .collect();
            let mut prev_nlsf: Vec<i16> = (0..lpc_order)
                .map(|_| (rng.next_u32() % 32768) as i16)
                .collect();
            prev_nlsf.sort_unstable();

            frames.push(FrameCase {
                lost,
                signal_type,
                ctrl: Ctrl {
                    pitch_l,
                    gains_q16,
                    pred_coef_q12: [pred0, pred1],
                    ltp_coef_q14,
                    ltp_scale_q14,
                },
                xq,
                exc,
                prev_nlsf,
            });
        }

        let to_arr16 = |v: &Vec<i16>| -> [i16; 16] {
            let mut a = [0i16; 16];
            a.copy_from_slice(v);
            a
        };
        let to_arr16_i32 = |v: &Vec<i32>| -> [i32; 16] {
            let mut a = [0i32; 16];
            a.copy_from_slice(v);
            a
        };
        let mut plc_arr = [0i16; LTP_ORDER];
        plc_arr.copy_from_slice(&ltp_coef);
        let mut out_buf_arr = [0i16; MAX_FRAME_LENGTH + 2 * MAX_SUB_FRAME_LENGTH];
        out_buf_arr.copy_from_slice(&out_buf);
        let mut cng_exc_buf = [0i32; MAX_FRAME_LENGTH];
        cng_exc_buf.copy_from_slice(&cng_exc_vec);

        Case {
            info: FrameInfo::new(fs, nb_subfr),
            plc: PlcState {
                pitch_l_q8,
                ltp_coef_q14: plc_arr,
                prev_lpc_q12: to_arr16(&prev_lpc),
                last_frame_lost,
                rand_seed,
                rand_scale_q14,
                conc_energy,
                conc_energy_shift,
                prev_ltp_scale_q14,
                prev_gain_q16: [pg0, pg1],
                fs_khz: plc_fs_khz,
                nb_subfr: plc_nb_subfr,
                subfr_length: plc_subfr_length,
            },
            cng: CngState {
                cng_exc_buf_q14: cng_exc_buf,
                cng_smth_nlsf_q15: to_arr16(&cng_nlsf),
                cng_synth_state: to_arr16_i32(&cng_synth),
                cng_smth_gain_q16: cng_smth_gain,
                rand_seed: cng_rand_seed,
                fs_khz: cng_fs_khz,
            },
            state: SynthesisState {
                s_lpc_q14_buf: to_arr16_i32(&s_lpc),
                out_buf: out_buf_arr,
                prev_gain_q16: 65536,
            },
            prev_signal_type,
            first_frame_after_reset,
            frames,
        }
    }

    fn fnv1a64(data: &[u8]) -> u64 {
        let mut h = 0xCBF2_9CE4_8422_2325u64;
        for &b in data {
            h ^= b as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01B3);
        }
        h
    }

    fn run_case(case: &mut Case) -> Vec<u8> {
        let fs = case.info.subfr_length / 5;
        let ltp_mem_length = case.info.ltp_mem_length;
        let frame_length = case.info.frame_length();
        let nb_subfr = case.info.nb_subfr;

        let mut loss_cnt: i32 = 0;
        let mut prev_signal_type = case.prev_signal_type;
        let mut first_frame_after_reset = case.first_frame_after_reset;
        let mut lag_prev: i32 = 0;

        let mut blob = Vec::new();
        for fc in &mut case.frames {
            let mut ctrl = fc.ctrl;
            let mut frame = vec![0i16; frame_length];
            if !fc.lost {
                frame.copy_from_slice(&fc.xq);
                case.state.update_out_buf(&frame, ltp_mem_length);
                plc(
                    &mut case.plc,
                    &mut case.state,
                    &mut ctrl,
                    &fc.exc,
                    &mut frame,
                    &case.info,
                    fs as u32,
                    loss_cnt,
                    prev_signal_type,
                    first_frame_after_reset,
                    false,
                );
                loss_cnt = 0;
                prev_signal_type = fc.signal_type;
                first_frame_after_reset = false;
            } else {
                plc(
                    &mut case.plc,
                    &mut case.state,
                    &mut ctrl,
                    &fc.exc,
                    &mut frame,
                    &case.info,
                    fs as u32,
                    loss_cnt,
                    prev_signal_type,
                    first_frame_after_reset,
                    true,
                );
                loss_cnt += 1;
                case.state.update_out_buf(&frame, ltp_mem_length);
            }

            cng(
                &mut case.cng,
                &case.plc,
                &ctrl,
                &fc.exc,
                &mut frame,
                frame_length,
                &case.info,
                fs as u32,
                loss_cnt,
                prev_signal_type,
                &fc.prev_nlsf,
            );
            plc_glue_frames(&mut case.plc, &mut frame, frame_length, loss_cnt != 0);
            lag_prev = ctrl.pitch_l[nb_subfr - 1];

            for &v in &frame {
                blob.extend_from_slice(&v.to_le_bytes());
            }
            for &v in &ctrl.pitch_l {
                blob.extend_from_slice(&v.to_le_bytes());
            }
        }

        // final state
        blob.extend_from_slice(&loss_cnt.to_le_bytes());
        blob.extend_from_slice(&(prev_signal_type as i32).to_le_bytes());
        blob.extend_from_slice(&lag_prev.to_le_bytes());
        let mut first = [0u8; 1];
        first[0] = u8::from(first_frame_after_reset);
        blob.extend_from_slice(&first);
        let p = &case.plc;
        blob.extend_from_slice(&p.pitch_l_q8.to_le_bytes());
        for v in &p.ltp_coef_q14 {
            blob.extend_from_slice(&v.to_le_bytes());
        }
        for v in &p.prev_lpc_q12 {
            blob.extend_from_slice(&v.to_le_bytes());
        }
        blob.push(u8::from(p.last_frame_lost));
        blob.extend_from_slice(&p.rand_seed.to_le_bytes());
        blob.extend_from_slice(&p.rand_scale_q14.to_le_bytes());
        blob.extend_from_slice(&p.conc_energy.to_le_bytes());
        blob.extend_from_slice(&p.conc_energy_shift.to_le_bytes());
        blob.extend_from_slice(&p.prev_ltp_scale_q14.to_le_bytes());
        for v in &p.prev_gain_q16 {
            blob.extend_from_slice(&v.to_le_bytes());
        }
        blob.extend_from_slice(&(p.subfr_length as i32).to_le_bytes());
        blob.extend_from_slice(&(p.nb_subfr as i32).to_le_bytes());
        blob.extend_from_slice(&(p.fs_khz as i32).to_le_bytes());
        let c = &case.cng;
        for v in &c.cng_exc_buf_q14 {
            blob.extend_from_slice(&v.to_le_bytes());
        }
        for v in &c.cng_smth_nlsf_q15 {
            blob.extend_from_slice(&v.to_le_bytes());
        }
        for v in &c.cng_synth_state {
            blob.extend_from_slice(&v.to_le_bytes());
        }
        blob.extend_from_slice(&c.cng_smth_gain_q16.to_le_bytes());
        blob.extend_from_slice(&c.rand_seed.to_le_bytes());
        blob.extend_from_slice(&(c.fs_khz as i32).to_le_bytes());
        for v in &case.state.s_lpc_q14_buf {
            blob.extend_from_slice(&v.to_le_bytes());
        }
        for v in &case.state.out_buf {
            blob.extend_from_slice(&v.to_le_bytes());
        }
        blob
    }

    /// (case index, FNV-1a64 over the per-frame outputs (concealed /
    /// decoded frames + pitch_l) and every PLC/CNG/synthesis state
    /// field at the end) — values generated by an independent Python
    /// transcription of PLC.c + CNG.c driven through silk_decode_frame's
    /// exact call sequence. The generator above mirrors the oracle's
    /// case generator sample-for-sample.
    #[test]
    fn plc_cng_match_libopus_oracle() {
        const HASHES: [u64; N_CASES] = [
            0xA4A754DAD33FA42C,
            0xB02DC6B0D8AF1F9B,
            0x7C00119F8B8789F7,
            0x8A34D367D14D7B6A,
            0x23552B86BBB3C521,
            0xEF388B15BE042B80,
            0xC8036C5333704236,
            0x3A62AF57B3D300F7,
            0xCABAC58F54E53FA8,
            0xF4DAB467D03E8BB9,
            0x8855255F64C7256C,
            0x00956F217D641B65,
            0x6D50D6535AD1FE07,
            0xA78B12C78368B7E1,
            0x8CC59258CD0A65D5,
            0xCCAB2B09712A84DC,
            0xA7A4580B753D3027,
            0xC6D26E7A04DE01CE,
            0x1549991FCD356ED6,
            0x0BB7B05977CD86C2,
            0x737651EC78CDC0FE,
            0xB3754964B14E2963,
            0xCF7702C31E08EDA5,
            0xE17B8CBE6BCD587F,
            0x9DA8C9604B36B401,
            0x10D4D04590A7FBD2,
            0x885F9EC5E8AF872A,
            0xAF5A7030868256D8,
            0x1D375E3B32286560,
            0x694AF5A64FBB31B9,
            0xA657D57998C361EB,
            0xF2CBB56829667831,
        ];
        for (c, &want) in HASHES.iter().enumerate() {
            let mut rng = XorShift(SEED_BASE + c as u32);
            let mut case = gen_case(&mut rng, c);
            let got = fnv1a64(&run_case(&mut case));
            assert_eq!(got, want, "case {c}: PLC/CNG diverged from the oracle");
        }
    }

    /// Reset-value contracts: `plc_reset` (unity gains, midpoint pitch,
    /// 20 ms geometry) and `cng_reset` (flat NLSF ramp, zero gain, the
    /// 3176576 seed).
    #[test]
    fn reset_values() {
        let mut plc_s = PlcState::default();
        plc_reset(&mut plc_s, 320);
        assert_eq!(plc_s.pitch_l_q8, 320 << 7);
        assert_eq!(plc_s.prev_gain_q16, [1 << 16, 1 << 16]);
        assert_eq!(plc_s.subfr_length, 20);
        assert_eq!(plc_s.nb_subfr, 2);

        let mut cng_s = CngState::default();
        cng_reset(&mut cng_s, 10);
        // 32767 / 11, accumulated
        let step = 32767 / 11;
        let want: Vec<i16> = (1..=10).map(|i| (step * i) as i16).collect();
        assert_eq!(&cng_s.cng_smth_nlsf_q15[..10], &want[..]);
        assert_eq!(cng_s.cng_smth_nlsf_q15[10..16], [0; 6]);
        assert_eq!(cng_s.cng_smth_gain_q16, 0);
        assert_eq!(cng_s.rand_seed, 3176576);

        let mut cng_s16 = CngState::default();
        cng_reset(&mut cng_s16, 16);
        let step16 = 32767 / 17;
        let want16: Vec<i16> = (1..=16).map(|i| (step16 * i) as i16).collect();
        assert_eq!(&cng_s16.cng_smth_nlsf_q15[..16], &want16[..]);
    }

    /// Rate changes reset both sub-states: feeding a different fs_kHz
    /// reinitializes the PLC/CNG state on the next call.
    #[test]
    fn fs_change_resets() {
        let mut plc_s = PlcState {
            fs_khz: 8,
            ..PlcState::default()
        };
        let mut state = SynthesisState::default();
        let mut ctrl = Ctrl::default();
        let mut rng = XorShift(42);
        let mut exc = vec![0i32; MAX_FRAME_LENGTH + 2 * MAX_SUB_FRAME_LENGTH];
        for v in exc.iter_mut() {
            *v = (rng.next_u32() % (1 << 25)) as i32 - (1 << 24);
        }
        let mut frame = vec![0i16; 320];
        let info = FrameInfo::new(16, 4);
        // The rate change runs the reset path (plc_conceal with loss_cnt
        // == 0 and a first_frame_after_reset state): must not panic, and
        // every subframe's pitch lag must be the (single) drifted lag.
        plc(
            &mut plc_s,
            &mut state,
            &mut ctrl,
            &exc,
            &mut frame,
            &info,
            16,
            0,
            TYPE_NO_VOICE_ACTIVITY,
            true,
            true,
        );
        assert_eq!(plc_s.prev_gain_q16, [1 << 16, 1 << 16]);
        assert_eq!(plc_s.fs_khz, 16);
        assert!(ctrl.pitch_l.windows(2).all(|w| w[0] == w[1]));
        assert!(ctrl.pitch_l[0] >= 2 * 16 && ctrl.pitch_l[0] <= 18 * 16);
    }

    /// Never-panic sweep over hostile (but lag-legal) states: any i16
    /// coefficients/gains/scales, any seeds/energies, both signal types,
    /// 0..=3 prior losses, both geometries, DTX and loss call orders.
    #[test]
    fn conceal_cng_glue_never_panic() {
        let mut rng = XorShift(0xA5F00D);
        for case in 0..64 {
            let fs = [8u32, 12, 16][case % 3];
            let nb_subfr = 2 + 2 * ((case / 3) % 2);
            let info = FrameInfo::new(fs, nb_subfr);
            let u16v = |rng: &mut XorShift| -> i16 { (rng.next_u32() as u16) as i16 };
            let mut plc_s = PlcState {
                pitch_l_q8: ((2 * fs as i32) + (rng.next_u32() % (16 * fs + 1)) as i32) << 8,
                ltp_coef_q14: [u16v(&mut rng); LTP_ORDER],
                prev_lpc_q12: [u16v(&mut rng); MAX_LPC_ORDER],
                last_frame_lost: rng.next_u32() % 2 == 1,
                rand_seed: rng.next_u32() as i32,
                rand_scale_q14: u16v(&mut rng),
                conc_energy: rng.next_u32() as i32,
                conc_energy_shift: (rng.next_u32() % 40) as i32,
                prev_ltp_scale_q14: u16v(&mut rng),
                // gains stay in the legal dequantized range (positive):
                // negative gains would leave the reference's own domain
                // (silk_INVERSE32_varQ's headroom math assumes b32 > 0).
                prev_gain_q16: [
                    81920 + (rng.next_u32() % (1 << 26)) as i32,
                    81920 + (rng.next_u32() % (1 << 26)) as i32,
                ],
                fs_khz: 0,
                nb_subfr,
                subfr_length: info.subfr_length,
            };
            let mut cng_s = CngState {
                cng_exc_buf_q14: [0; MAX_FRAME_LENGTH],
                cng_smth_nlsf_q15: [(rng.next_u32() % 32768) as i16; MAX_LPC_ORDER],
                cng_synth_state: [rng.next_u32() as i32; MAX_LPC_ORDER],
                cng_smth_gain_q16: rng.next_u32() as i32,
                rand_seed: rng.next_u32() as i32,
                fs_khz: 0,
            };
            let mut state = SynthesisState::default();
            for v in state.out_buf.iter_mut() {
                *v = (rng.next_u32() as u16) as i16;
            }
            let mut ctrl = Ctrl {
                pitch_l: [0; MAX_SUBFRS],
                gains_q16: [rng.next_u32() as i32; MAX_SUBFRS],
                pred_coef_q12: [[u16v(&mut rng); MAX_LPC_ORDER]; 2],
                ltp_coef_q14: [u16v(&mut rng); LTP_ORDER * MAX_SUBFRS],
                ltp_scale_q14: u16v(&mut rng),
            };
            let mut exc = vec![0i32; MAX_FRAME_LENGTH + 2 * MAX_SUB_FRAME_LENGTH];
            for v in exc.iter_mut() {
                *v = rng.next_u32() as i32;
            }
            let mut prev_nlsf = vec![0i16; info.lpc_order];
            for v in prev_nlsf.iter_mut() {
                *v = (rng.next_u32() % 32768) as i16;
            }
            let loss_cnt = (case % 4) as i32;
            let prev_st = (case % 3) as i8;

            let mut frame = vec![0i16; info.frame_length()];
            plc(
                &mut plc_s,
                &mut state,
                &mut ctrl,
                &exc,
                &mut frame,
                &info,
                fs,
                loss_cnt,
                prev_st,
                case % 2 == 0,
                true,
            );
            cng(
                &mut cng_s,
                &plc_s,
                &ctrl,
                &exc,
                &mut frame,
                info.frame_length(),
                &info,
                fs,
                loss_cnt + 1,
                prev_st,
                &prev_nlsf,
            );
            plc_glue_frames(&mut plc_s, &mut frame, info.frame_length(), true);
            // And the good-frame update path with the hostile ctrl.
            plc(
                &mut plc_s, &mut state, &mut ctrl, &exc, &mut frame, &info, fs, 0, prev_st, false,
                false,
            );
            cng(
                &mut cng_s,
                &plc_s,
                &ctrl,
                &exc,
                &mut frame,
                info.frame_length(),
                &info,
                fs,
                0,
                prev_st,
                &prev_nlsf,
            );
            plc_glue_frames(&mut plc_s, &mut frame, info.frame_length(), false);
        }
    }
}
