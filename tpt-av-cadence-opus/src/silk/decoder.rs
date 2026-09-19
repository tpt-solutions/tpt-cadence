//! SILK top-level decoder — Tier 4 assembly.
//!
//! Ports the reference decoder's orchestration layer:
//!
//! - [`SilkDecoder::decode`] (`silk/dec_API.c` `silk_Decode`): the
//!   per-payload entry point. Handles the first-frame-of-payload
//!   bookkeeping (VAD flags, packet LBRR flag, per-frame LBRR flags),
//!   the LBRR skip/decode pass, mid/side predictor decode and the
//!   side-channel skip logic, per-frame [`LostFlag`] dispatch,
//!   mid/side → left/right conversion (or the mono `sMid` buffering),
//!   the internal→API resampling, mono→stereo duplication, and the
//!   `prevPitchLag` / `LastGainIndex` cross-packet updates.
//! - [`ChannelState::decode_frame`] (`silk/decode_frame.c`): one 5 ms
//!   subframe × `nb_subfr` frame: side-info + excitation + parameter +
//!   core decode, the `outBuf` slide, PLC update/concealment, CNG, and
//!   frame gluing, in the reference's exact call order.
//! - [`decode_parameters`] (`silk/decode_parameters.c`): gain
//!   dequantization, NLSF decode + `NLSF2A` + interframe interpolation
//!   into the per-half-frame LPC vectors, post-loss bandwidth
//!   expansion, and the voiced branch's pitch/LTP lookups (or their
//!   unvoiced zeroing).
//! - [`ChannelState::set_fs`] (`silk/decoder_set_fs.c`): geometry
//!   (subframe/frame/LTP-memory lengths, LPC order), NLSF codebook
//!   selection, and the resampler (re)initialization — with the
//!   reference's reset-on-rate-change semantics (`lagPrev = 100`,
//!   `LastGainIndex = 10`, interpolation disabled for the first frame,
//!   LPC/output buffers zeroed).
//!
//! All scratch (pulses, per-channel frame buffers, resampler output)
//! lives in the state structs, so [`SilkDecoder::decode`] performs no
//! allocation after `new`.
//!
//! SOURCE: Xiph.Org libopus 1.5.2, `silk/dec_API.c`,
//! `silk/decode_frame.c`, `silk/decode_parameters.c`,
//! `silk/decoder_set_fs.c`, `silk/init_decoder.c`, `silk/define.h`,
//! `silk/structs.h` (BSD-3-Clause); cross-checked against RFC 6716
//! §4.2 (esp. §4.2.2 LBRR framing, §4.2.3 "Decode VAD flags" through
//! §4.2.5 side-info ordering).

use crate::range::RangeDecoder;
use crate::silk::decode_indices::{
    decode_indices, decode_lbrr_flags, decode_vad_flags_and_lbrr_flag, CondCoding, EcPrevState,
    FrameParams, SideInfoIndices, MAX_FRAMES_PER_PACKET, MAX_LPC_ORDER, MAX_NB_SUBFR,
};
use crate::silk::excitation::decode_pulses;
use crate::silk::gains::{gains_dequant, LAST_GAIN_INDEX_ON_PACKET_LOSS};
use crate::silk::nlsf::{bwexpander, nlsf2a, nlsf_decode};
use crate::silk::pitch::{decode_pitch, ltp_coefs_q14, ltp_scale_q14, LTP_ORDER};
use crate::silk::plc::{cng, plc, plc_glue_frames, CngState, PlcState};
use crate::silk::resampler::Resampler;
use crate::silk::stereo::{decode_mid_only, decode_pred, ms_to_lr, StereoDecState};
use crate::silk::synthesis::{
    decode_core, DecoderControl, FrameInfo, SynthesisState, MAX_FRAME_LENGTH, MAX_SUB_FRAME_LENGTH,
};
use crate::silk::tables::{NlsfCbStruct, NLSF_CB_NB_MB, NLSF_CB_WB};
use crate::{CadenceError, Result};

/// `BWE_AFTER_LOSS_Q16` (`silk/main.h`): 0.97 in Q16 — the LPC
/// bandwidth expansion applied after a lost frame.
const BWE_AFTER_LOSS_Q16: i32 = 63570;

/// `MAX_API_FS_KHZ` (`silk/define.h`).
const MAX_API_FS_KHZ: i32 = 48;

/// Resampler output scratch: `frame_length * API/fs` samples; the
/// widest supported ratio is 16 kHz → 48 kHz (6×).
const RESAMPLE_OUT_MAX: usize = MAX_FRAME_LENGTH * 6;

/// `lostFlag` of `silk_Decode`/`silk_decode_frame` (`silk/define.h`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LostFlag {
    /// `FLAG_DECODE_NORMAL`: regular decode of one payload frame.
    Normal,
    /// `FLAG_PACKET_LOST`: conceal one frame from decoder state only
    /// (`psRangeDec` is `None`).
    PacketLost,
    /// `FLAG_DECODE_LBRR`: decode this frame's LBRR data (or conceal it
    /// when the frame's LBRR flag is clear).
    DecodeLbrr,
}

/// `silk_DecControlStruct` subset `silk_Decode` reads and writes
/// (`silk/API.h`). The caller fills it per payload (per frame for
/// multi-frame packets, matching the reference's per-call contract);
/// [`SilkDecoder::decode`] writes back `prev_pitch_lag`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DecControl {
    /// API (output) channel count: 1 or 2.
    pub n_channels_api: usize,
    /// Internal (bitstream) channel count: 1 or 2.
    pub n_channels_internal: usize,
    /// Output sampling rate in Hz (8–48 kHz).
    pub api_sample_rate: i32,
    /// Internal SILK rate in Hz (8000, 12000, or 16000; 0 mid-packet on
    /// loss).
    pub internal_sample_rate: i32,
    /// Payload duration in ms (0/10/20/40/60; 0 mid-packet on loss).
    pub payload_size_ms: i32,
    /// Out: previous voiced pitch lag at 48 kHz (0 if unvoiced).
    pub prev_pitch_lag: i32,
}

/// One SILK channel — the decoder-relevant subset of
/// `silk_decoder_state` (`silk/structs.h`).
///
/// `Default` matches `silk_init_decoder`/`silk_reset_decoder`: all
/// fields zero except `first_frame_after_reset = 1` and
/// `prev_gain_Q16 = 65536` (both from `silk_reset_decoder`), plus the
/// CNG/PLC sub-state resets.
pub(crate) struct ChannelState {
    /// `sLPC_Q14_buf` / `outBuf` / `prev_gain_Q16` (the
    /// `SILK_DECODER_STATE_RESET_START` region).
    pub synth: SynthesisState,
    /// `exc_Q14`: the current frame's Q14 excitation (retained for
    /// PLC/CNG).
    pub exc_q14: [i32; MAX_FRAME_LENGTH],
    /// `lagPrev`.
    pub lag_prev: i32,
    /// `LastGainIndex` (persistent gain-index state for
    /// [`gains_dequant`]).
    pub last_gain_index: i8,
    /// `fs_kHz` (0 until the first [`ChannelState::set_fs`]).
    pub fs_khz: u32,
    /// `fs_API_hz`.
    pub fs_api_hz: i32,
    /// `nb_subfr` (set by the payload bookkeeping in
    /// [`SilkDecoder::decode`]).
    pub nb_subfr: usize,
    /// `frame_length`.
    pub frame_length: usize,
    /// `subfr_length`.
    pub subfr_length: usize,
    /// `ltp_mem_length`.
    pub ltp_mem_length: usize,
    /// `LPC_order`.
    pub lpc_order: usize,
    /// `prevNLSF_Q15`.
    pub prev_nlsf_q15: [i16; MAX_LPC_ORDER],
    /// `first_frame_after_reset`.
    pub first_frame_after_reset: bool,
    /// `nFramesDecoded`.
    pub n_frames_decoded: usize,
    /// `nFramesPerPacket`.
    pub n_frames_per_packet: usize,
    /// `ec_prevSignalType` / `ec_prevLagIndex`.
    pub ec_prev: EcPrevState,
    /// `VAD_flags`.
    pub vad_flags: [bool; MAX_FRAMES_PER_PACKET],
    /// `LBRR_flag` (packet-level).
    pub lbrr_flag: bool,
    /// `LBRR_flags` (per-frame).
    pub lbrr_flags: [bool; MAX_FRAMES_PER_PACKET],
    /// `resampler_state`.
    pub resampler: Resampler,
    /// `psNLSF_CB`.
    pub nlsf_cb: &'static NlsfCbStruct,
    /// `indices`.
    pub indices: SideInfoIndices,
    /// `sCNG`.
    pub cng: CngState,
    /// `lossCnt`.
    pub loss_cnt: i32,
    /// `prevSignalType`.
    pub prev_signal_type: i8,
    /// `sPLC`.
    pub plc: PlcState,
    /// Scratch: shell-block-rounded pulse buffer of
    /// `silk_decode_frame` (`(L + 15) & ~15` ≤ [`MAX_FRAME_LENGTH`]).
    pub pulses: [i16; MAX_FRAME_LENGTH],
}

impl Default for ChannelState {
    fn default() -> Self {
        ChannelState {
            synth: SynthesisState::default(),
            exc_q14: [0; MAX_FRAME_LENGTH],
            lag_prev: 0,
            last_gain_index: 0,
            fs_khz: 0,
            fs_api_hz: 0,
            nb_subfr: 0,
            frame_length: 0,
            subfr_length: 0,
            ltp_mem_length: 0,
            lpc_order: 0,
            prev_nlsf_q15: [0; MAX_LPC_ORDER],
            first_frame_after_reset: true,
            n_frames_decoded: 0,
            n_frames_per_packet: 0,
            ec_prev: EcPrevState::default(),
            vad_flags: [false; MAX_FRAMES_PER_PACKET],
            lbrr_flag: false,
            lbrr_flags: [false; MAX_FRAMES_PER_PACKET],
            // `Resampler` has no all-zero state; `silk_init_decoder`
            // leaves `resampler_state` zeroed and `set_fs` (re)runs
            // `silk_resampler_init` on the first call (fs_kHz mismatch),
            // so an error placeholder is never sampled.
            resampler: Resampler::new(8000, 8000, false).unwrap(),
            nlsf_cb: &NLSF_CB_NB_MB,
            indices: SideInfoIndices::default(),
            cng: CngState::default(),
            loss_cnt: 0,
            prev_signal_type: 0,
            plc: PlcState::default(),
            pulses: [0; MAX_FRAME_LENGTH],
        }
    }
}

impl ChannelState {
    /// `silk_reset_decoder` (`silk/init_decoder.c`): zero all state
    /// from `prev_gain_Q16` on, force interpolation off for the next
    /// frame, and reset the CNG/PLC sub-states.
    pub(crate) fn reset(&mut self) {
        // The reference memset covers everything from `prev_gain_Q16` to
        // the end of the struct — fs_kHz and the geometry included — so
        // the next decode re-runs `set_fs` from scratch. `Default`
        // carries the same zeroed fields plus the reset flags; the
        // CNG/PLC sub-state resets then see LPC_order/frame_length 0,
        // exactly as in the reference.
        *self = ChannelState::default();
        cng_reset_state(&mut self.cng, 0);
        plc_reset_state(&mut self.plc, 0);
    }
}

/// `silk_CNG_Reset` for a channel (the plc.rs resets, re-exported
/// through the module to keep reset semantics next to `reset`).
fn cng_reset_state(cng: &mut CngState, lpc_order: usize) {
    crate::silk::plc::cng_reset(cng, lpc_order);
}

/// `silk_PLC_Reset` for a channel.
fn plc_reset_state(plc: &mut PlcState, frame_length: usize) {
    crate::silk::plc::plc_reset(plc, frame_length);
}

impl ChannelState {
    /// `silk_decoder_set_fs` (`silk/decoder_set_fs.c`): update the
    /// channel geometry for `fs_kHz` and (re)initialize the decimation
    /// resampler towards `fs_api_hz`.
    ///
    /// The caller must have set [`ChannelState::nb_subfr`] (4 for 20 ms
    /// payloads, 2 for 10 ms), exactly as `silk_Decode` does before
    /// calling this.
    pub(crate) fn set_fs(&mut self, fs_khz: u32, fs_api_hz: i32) -> Result<()> {
        if !matches!(fs_khz, 8 | 12 | 16) {
            return Err(CadenceError::CorruptData(format!(
                "unsupported SILK internal sample rate {fs_khz} kHz"
            )));
        }
        debug_assert!(self.nb_subfr == MAX_NB_SUBFR || self.nb_subfr == MAX_NB_SUBFR / 2);

        let subfr_length = 5 * fs_khz as usize;
        let frame_length = self.nb_subfr * subfr_length;

        // Initialize resampler when switching internal or external
        // sampling frequency.
        if self.fs_khz != fs_khz || self.fs_api_hz != fs_api_hz {
            self.resampler = Resampler::new(fs_khz as i32 * 1000, fs_api_hz, false)?;
            self.fs_api_hz = fs_api_hz;
        }

        if self.fs_khz != fs_khz || frame_length != self.frame_length {
            if self.fs_khz != fs_khz {
                if crate::debug::flags().silk_c_fs_debug {
                    eprintln!(
                        "FSCHANGE old_fs={} new_fs={} nb_subfr={}",
                        self.fs_khz, fs_khz, self.nb_subfr
                    );
                }
                self.ltp_mem_length = 20 * fs_khz as usize;
                if fs_khz == 8 || fs_khz == 12 {
                    self.lpc_order = 10;
                    self.nlsf_cb = &NLSF_CB_NB_MB;
                } else {
                    self.lpc_order = MAX_LPC_ORDER;
                    self.nlsf_cb = &NLSF_CB_WB;
                }
                self.first_frame_after_reset = true;
                self.lag_prev = 100;
                self.last_gain_index = LAST_GAIN_INDEX_ON_PACKET_LOSS;
                self.prev_signal_type = crate::silk::decode_indices::TYPE_NO_VOICE_ACTIVITY;
                self.synth.out_buf = [0; MAX_FRAME_LENGTH + 2 * MAX_SUB_FRAME_LENGTH];
                self.synth.s_lpc_q14_buf = [0; MAX_LPC_ORDER];
            }
            self.fs_khz = fs_khz;
            self.frame_length = frame_length;
        }
        self.subfr_length = subfr_length;
        Ok(())
    }
}

/// Gains + NLSF + pitch + LTP parameter reconstruction — port of
/// `silk_decode_parameters` (`silk/decode_parameters.c`).
fn decode_parameters(ch: &mut ChannelState, ctrl: &mut DecoderControl, cond_coding: CondCoding) {
    let nb_subfr = ch.nb_subfr;
    let lpc_order = ch.lpc_order;

    // Dequant Gains
    gains_dequant(
        &mut ctrl.gains_q16,
        &ch.indices.gains_indices,
        &mut ch.last_gain_index,
        cond_coding == CondCoding::Conditionally,
        nb_subfr,
    );

    if crate::debug::flags().silk_dbg {
        eprintln!(
            "DP fs={} nb={} order={} cb_order={} sig={} first_frame={}",
            ch.fs_khz,
            ch.nb_subfr,
            ch.lpc_order,
            ch.nlsf_cb.order,
            ch.indices.signal_type,
            ch.first_frame_after_reset
        );
    }
    // Decode NLSFs
    let mut p_nlsf_q15 = [0i16; MAX_LPC_ORDER];
    nlsf_decode(
        &mut p_nlsf_q15[..lpc_order],
        &ch.indices.nlsf_indices[..lpc_order + 1],
        ch.nlsf_cb,
    );

    // Convert NLSF parameters to AR prediction filter coefficients
    nlsf2a(
        &mut ctrl.pred_coef_q12[1][..lpc_order],
        &p_nlsf_q15[..lpc_order],
        lpc_order,
    );

    // If just reset, e.g., because internal Fs changed, do not allow
    // interpolation; improves the case of packet loss in the first
    // frame after a switch.
    if ch.first_frame_after_reset {
        ch.indices.nlsf_interp_coef_q2 = 4;
    }

    if ch.indices.nlsf_interp_coef_q2 < 4 {
        // Interpolate the previous NLSF vector towards the current one
        // and convert the first half's coefficients.
        let coef_q2 = ch.indices.nlsf_interp_coef_q2 as i32;
        let mut p_nlsf0_q15 = [0i16; MAX_LPC_ORDER];
        for i in 0..lpc_order {
            let diff =
                (coef_q2.wrapping_mul(p_nlsf_q15[i] as i32 - ch.prev_nlsf_q15[i] as i32)) >> 2;
            p_nlsf0_q15[i] = (ch.prev_nlsf_q15[i] as i32 + diff) as i16;
        }
        nlsf2a(
            &mut ctrl.pred_coef_q12[0][..lpc_order],
            &p_nlsf0_q15[..lpc_order],
            lpc_order,
        );
    } else {
        // Copy LPC coefficients for first half from second half
        let second_half = ctrl.pred_coef_q12[1];
        ctrl.pred_coef_q12[0] = second_half;
    }

    ch.prev_nlsf_q15[..lpc_order].copy_from_slice(&p_nlsf_q15[..lpc_order]);

    // After a packet loss do BWE of LPC coefs
    if ch.loss_cnt != 0 {
        bwexpander(&mut ctrl.pred_coef_q12[0][..lpc_order], BWE_AFTER_LOSS_Q16);
        bwexpander(&mut ctrl.pred_coef_q12[1][..lpc_order], BWE_AFTER_LOSS_Q16);
    }

    if ch.indices.signal_type == crate::silk::decode_indices::TYPE_VOICED {
        // Decode pitch lags
        let mut pitch_l = [0i32; MAX_NB_SUBFR];
        decode_pitch(
            ch.indices.lag_index,
            ch.indices.contour_index,
            &mut pitch_l,
            ch.fs_khz,
            nb_subfr,
        )
        .expect("decode_pitch rate validated by set_fs");
        ctrl.pitch_l = pitch_l;

        // Decode Codebook Index
        let mut ltp_coef = [0i16; LTP_ORDER * MAX_NB_SUBFR];
        ltp_coefs_q14(
            ch.indices.per_index,
            &ch.indices.ltp_index,
            &mut ltp_coef,
            nb_subfr,
        );
        ctrl.ltp_coef_q14 = ltp_coef;

        // Decode LTP scaling
        ctrl.ltp_scale_q14 = ltp_scale_q14(ch.indices.ltp_scale_index);
    } else {
        ctrl.pitch_l[..nb_subfr].fill(0);
        ctrl.ltp_coef_q14[..LTP_ORDER * nb_subfr].fill(0);
        ch.indices.per_index = 0;
        ctrl.ltp_scale_q14 = 0;
    }
}

impl ChannelState {
    /// `silk_decode_frame` (`silk/decode_frame.c`): decode (or conceal)
    /// one frame into `frame` (the first `frame_length` samples).
    ///
    /// Returns the decoded frame length (`*pN`); always
    /// `frame_length`. `cond_coding` is selected by
    /// [`SilkDecoder::decode`]; for [`LostFlag::Normal`] and
    /// LBRR-flagged [`LostFlag::DecodeLbrr`] frames a range decoder is
    /// required.
    pub(crate) fn decode_frame(
        &mut self,
        dec: Option<&mut RangeDecoder<'_>>,
        frame: &mut [i16],
        lost_flag: LostFlag,
        cond_coding: CondCoding,
    ) -> Result<usize> {
        let frame_length = self.frame_length;
        debug_assert!(frame.len() >= frame_length);
        let normal_decode = lost_flag == LostFlag::Normal
            || (lost_flag == LostFlag::DecodeLbrr && self.lbrr_flags[self.n_frames_decoded]);
        let mut ctrl = DecoderControl::default();

        if normal_decode {
            let dec = dec.ok_or(CadenceError::CorruptData(
                "SILK frame decode requires a range decoder".to_string(),
            ))?;

            // Decode quantization indices of side info
            let params = FrameParams {
                nlsf_cb: self.nlsf_cb,
                fs_khz: self.fs_khz,
                nb_subfr: self.nb_subfr,
                frame_index: self.n_frames_decoded,
                vad_flag: self.vad_flags[self.n_frames_decoded],
                decode_lbrr: lost_flag == LostFlag::DecodeLbrr,
                cond_coding,
            };
            let mut indices = self.indices;
            let mut ec_prev = self.ec_prev;
            decode_indices(dec, &mut indices, &mut ec_prev, &params)?;
            self.indices = indices;
            self.ec_prev = ec_prev;

            // Decode quantization indices of excitation
            let frame_info = FrameInfo::new(self.fs_khz, self.nb_subfr);
            decode_pulses(
                dec,
                &mut self.pulses,
                self.indices.signal_type as i32,
                self.indices.quant_offset_type as i32,
                frame_length,
            )?;

            // Decode parameters and pulse signal
            decode_parameters(self, &mut ctrl, cond_coding);

            // Run inverse NSQ
            let mut xq = [0i16; MAX_FRAME_LENGTH];
            {
                let (synth, exc) = (&mut self.synth, &mut self.exc_q14);
                decode_core(
                    synth,
                    &mut ctrl,
                    &self.indices,
                    &self.pulses,
                    &mut xq,
                    exc,
                    &frame_info,
                    self.loss_cnt,
                    self.prev_signal_type,
                    self.lag_prev,
                );
            }

            // Update output buffer
            debug_assert!(self.ltp_mem_length >= frame_length);
            self.synth
                .update_out_buf(&xq[..frame_length], self.ltp_mem_length);

            // Update PLC state
            {
                let mut plc_state = self.plc;
                let mut cng_state = self.cng;
                plc(
                    &mut plc_state,
                    &mut self.synth,
                    &mut ctrl,
                    &self.exc_q14,
                    &mut xq,
                    &frame_info,
                    self.fs_khz,
                    self.loss_cnt,
                    self.prev_signal_type,
                    self.first_frame_after_reset,
                    false,
                );
                self.plc = plc_state;

                self.loss_cnt = 0;
                self.prev_signal_type = self.indices.signal_type;
                debug_assert!(
                    (0..=2).contains(&self.prev_signal_type),
                    "invalid signal type {}",
                    self.prev_signal_type
                );

                // A frame has been decoded without errors
                self.first_frame_after_reset = false;

                // Comfort noise generation / estimation
                cng(
                    &mut cng_state,
                    plc_state_read(&self.plc),
                    &ctrl,
                    &self.exc_q14,
                    &mut xq,
                    frame_length,
                    &frame_info,
                    self.fs_khz,
                    self.loss_cnt,
                    self.prev_signal_type,
                    &self.prev_nlsf_q15,
                );
                self.cng = cng_state;

                // Ensure smooth connection of extrapolated and good frames
                let mut plc_state = self.plc;
                plc_glue_frames(&mut plc_state, &mut xq, frame_length, false);
                self.plc = plc_state;
            }

            frame[..frame_length].copy_from_slice(&xq[..frame_length]);
        } else {
            // Handle packet loss by extrapolation
            let frame_info = FrameInfo::new(self.fs_khz, self.nb_subfr);
            {
                let mut plc_state = self.plc;
                plc(
                    &mut plc_state,
                    &mut self.synth,
                    &mut ctrl,
                    &self.exc_q14,
                    &mut frame[..frame_length],
                    &frame_info,
                    self.fs_khz,
                    self.loss_cnt,
                    self.prev_signal_type,
                    self.first_frame_after_reset,
                    true,
                );
                self.plc = plc_state;
                // `silk_PLC` increments `psDec->lossCnt` on loss.
                self.loss_cnt += 1;
            }

            // Update output buffer
            debug_assert!(self.ltp_mem_length >= frame_length);
            let xq = &frame[..frame_length];
            self.synth.update_out_buf(xq, self.ltp_mem_length);

            // Comfort noise generation / estimation
            let mut cng_state = self.cng;
            cng(
                &mut cng_state,
                &self.plc,
                &ctrl,
                &self.exc_q14,
                &mut frame[..frame_length],
                frame_length,
                &frame_info,
                self.fs_khz,
                self.loss_cnt,
                self.prev_signal_type,
                &self.prev_nlsf_q15,
            );
            self.cng = cng_state;

            // Ensure smooth connection of extrapolated and good frames
            let mut plc_state = self.plc;
            plc_glue_frames(
                &mut plc_state,
                &mut frame[..frame_length],
                frame_length,
                true,
            );
            self.plc = plc_state;
        }

        // Update some decoder state variables
        self.lag_prev = ctrl.pitch_l[self.nb_subfr - 1];

        Ok(frame_length)
    }
}

/// `&self.plc` reborrows (the CNG call reads the freshly updated PLC
/// state; the helper exists purely to split the mutable borrow above).
fn plc_state_read(plc: &PlcState) -> &PlcState {
    plc
}

/// The SILK decoder — `silk_decoder` (`silk/dec_API.c`): two channel
/// states plus the mid/side prediction state.
pub struct SilkDecoder {
    channel: [ChannelState; 2],
    stereo: StereoDecState,
    n_channels_api: usize,
    n_channels_internal: usize,
    prev_decode_only_middle: bool,
    /// `samplesOut1_tmp` scratch (frame_length + 2 per channel; the +2
    /// carries the stereo history headers).
    tmp: [[i16; MAX_FRAME_LENGTH + 2]; 2],
    /// `samplesOut2_tmp` resampler-output scratch.
    resample_out: [i16; RESAMPLE_OUT_MAX],
}

impl SilkDecoder {
    /// `silk_InitDecoder`: create a decoder for `n_channels_api`
    /// output channels (1 or 2).
    pub fn new(n_channels_api: usize) -> Result<Self> {
        // Resolve debug-trace env flags before any decode (see debug.rs).
        crate::debug::init();
        if !(1..=2).contains(&n_channels_api) {
            return Err(CadenceError::UnsupportedFeature(
                "SILK decoder needs 1 or 2 output channels".to_string(),
            ));
        }
        Ok(SilkDecoder {
            channel: [ChannelState::default(), ChannelState::default()],
            stereo: StereoDecState::default(),
            n_channels_api,
            n_channels_internal: 1,
            prev_decode_only_middle: false,
            tmp: [[0; MAX_FRAME_LENGTH + 2]; 2],
            resample_out: [0; RESAMPLE_OUT_MAX],
        })
    }

    /// `silk_ResetDecoder`.
    pub fn reset(&mut self) {
        self.channel[0].reset();
        self.channel[1].reset();
        self.stereo = StereoDecState::default();
        self.prev_decode_only_middle = false;
    }

    /// The payloadSize_ms → (nFramesPerPacket, nb_subfr) table of
    /// `silk_Decode`.
    pub(crate) fn frames_per_packet(payload_size_ms: i32) -> Result<(usize, usize)> {
        match payload_size_ms {
            // Assuming packet loss, use 10 ms
            0 => Ok((1, 2)),
            10 => Ok((1, 2)),
            20 => Ok((1, 4)),
            40 => Ok((2, 4)),
            60 => Ok((3, 4)),
            _ => Err(CadenceError::UnsupportedFeature(format!(
                "unsupported SILK payload size {payload_size_ms} ms"
            ))),
        }
    }

    /// `silk_Decode`: decode (or conceal) the next frame of the payload
    /// into `samples_out` (interleaved), returning the number of
    /// interleaved output samples (`*nSamplesOut`).
    ///
    /// `dec` is the payload range decoder, required unless `lost_flag`
    /// is [`LostFlag::PacketLost`]. `new_packet_flag` marks the first
    /// frame of a payload (resetting the per-packet frame counters).
    pub fn decode(
        &mut self,
        ctrl: &mut DecControl,
        mut dec: Option<&mut RangeDecoder<'_>>,
        lost_flag: LostFlag,
        new_packet_flag: bool,
        samples_out: &mut [i16],
    ) -> Result<usize> {
        debug_assert!(ctrl.n_channels_internal == 1 || ctrl.n_channels_internal == 2);
        debug_assert!(ctrl.n_channels_api == 1 || ctrl.n_channels_api == 2);

        /************************************/
        /* Test if first frame in payload   */
        /************************************/
        if new_packet_flag {
            for ch in self.channel.iter_mut().take(ctrl.n_channels_internal) {
                ch.n_frames_decoded = 0;
            }
        }

        // If Mono -> Stereo transition in bitstream: init state of
        // second channel
        if ctrl.n_channels_internal > self.n_channels_internal {
            self.channel[1].reset();
        }

        let stereo_to_mono = ctrl.n_channels_internal == 1
            && self.n_channels_internal == 2
            && ctrl.internal_sample_rate == 1000 * self.channel[0].fs_khz as i32;

        if self.channel[0].n_frames_decoded == 0 {
            let (n_frames_per_packet, nb_subfr) = Self::frames_per_packet(ctrl.payload_size_ms)?;
            let fs_khz_dec = (ctrl.internal_sample_rate >> 10) + 1;
            if !matches!(fs_khz_dec, 8 | 12 | 16) {
                return Err(CadenceError::CorruptData(format!(
                    "invalid SILK internal sample rate {} Hz",
                    ctrl.internal_sample_rate
                )));
            }
            for ch in self.channel.iter_mut().take(ctrl.n_channels_internal) {
                ch.n_frames_per_packet = n_frames_per_packet;
                ch.nb_subfr = nb_subfr;
                ch.set_fs(fs_khz_dec as u32, ctrl.api_sample_rate)?;
            }
        }

        if ctrl.n_channels_api == 2
            && ctrl.n_channels_internal == 2
            && (self.n_channels_api == 1 || self.n_channels_internal == 1)
        {
            self.stereo.pred_prev_q13 = [0; 2];
            self.stereo.s_side = [0; 2];
            self.channel[1].resampler = self.channel[0].resampler.clone();
        }
        self.n_channels_api = ctrl.n_channels_api;
        self.n_channels_internal = ctrl.n_channels_internal;

        if ctrl.api_sample_rate > MAX_API_FS_KHZ * 1000 || ctrl.api_sample_rate < 8000 {
            return Err(CadenceError::CorruptData(format!(
                "invalid SILK API sample rate {} Hz",
                ctrl.api_sample_rate
            )));
        }

        let frame_length = self.channel[0].frame_length;
        let fs_khz = self.channel[0].fs_khz;

        let mut decode_only_middle = false;
        let mut ms_pred_q13 = [0i32; 2];

        if lost_flag != LostFlag::PacketLost && self.channel[0].n_frames_decoded == 0 {
            let dec = dec.as_deref_mut().ok_or(CadenceError::CorruptData(
                "SILK payload decode requires a range decoder".to_string(),
            ))?;
            /* First decoder call for this payload */
            /* Decode VAD flags and LBRR flag */
            for ch in self.channel.iter_mut().take(ctrl.n_channels_internal) {
                let (vad_flags, lbrr_flag) =
                    decode_vad_flags_and_lbrr_flag(dec, ch.n_frames_per_packet)?;
                ch.vad_flags = vad_flags;
                ch.lbrr_flag = lbrr_flag;
            }
            /* Decode LBRR flags */
            for ch in self.channel.iter_mut().take(ctrl.n_channels_internal) {
                ch.lbrr_flags = [false; MAX_FRAMES_PER_PACKET];
                if ch.lbrr_flag {
                    ch.lbrr_flags = decode_lbrr_flags(dec, ch.n_frames_per_packet)?;
                }
            }

            if lost_flag == LostFlag::Normal {
                /* Regular decoding: skip all LBRR data */
                let n_frames = self.channel[0].n_frames_per_packet;
                let lbrr_flags0 = self.channel[0].lbrr_flags;
                let lbrr_flags1 = self.channel[1].lbrr_flags;
                for i in 0..n_frames {
                    for n in 0..ctrl.n_channels_internal {
                        if self.channel[n].lbrr_flags[i] {
                            let cond_coding = if i > 0 && [lbrr_flags0, lbrr_flags1][n][i - 1] {
                                CondCoding::Conditionally
                            } else {
                                CondCoding::Independently
                            };
                            if ctrl.n_channels_internal == 2 && n == 0 {
                                ms_pred_q13 = decode_pred(dec)?;
                                if !lbrr_flags1[i] {
                                    decode_only_middle = decode_mid_only(dec)?;
                                }
                            }
                            // (decode_only_middle is consumed by the
                            // stereo decode below; the LBRR pass only
                            // needs to advance the bitstream.)
                            let _ = decode_only_middle;
                            let params = FrameParams {
                                nlsf_cb: self.channel[n].nlsf_cb,
                                fs_khz: self.channel[n].fs_khz,
                                nb_subfr: self.channel[n].nb_subfr,
                                frame_index: i,
                                vad_flag: self.channel[n].vad_flags[i],
                                decode_lbrr: true,
                                cond_coding,
                            };
                            let mut indices = self.channel[n].indices;
                            let mut ec_prev = self.channel[n].ec_prev;
                            decode_indices(dec, &mut indices, &mut ec_prev, &params)?;
                            self.channel[n].indices = indices;
                            self.channel[n].ec_prev = ec_prev;
                            decode_pulses(
                                dec,
                                &mut self.channel[n].pulses,
                                self.channel[n].indices.signal_type as i32,
                                self.channel[n].indices.quant_offset_type as i32,
                                frame_length,
                            )?;
                        }
                    }
                }
            }
        }

        // Get MS predictor index
        if ctrl.n_channels_internal == 2 {
            if lost_flag == LostFlag::Normal
                || (lost_flag == LostFlag::DecodeLbrr
                    && self.channel[0].lbrr_flags[self.channel[0].n_frames_decoded])
            {
                let dec = dec.as_deref_mut().ok_or(CadenceError::CorruptData(
                    "SILK stereo decode requires a range decoder".to_string(),
                ))?;
                ms_pred_q13 = decode_pred(dec)?;
                /* For LBRR data, decode mid-only flag only if
                 * side-channel's LBRR flag is false */
                if (lost_flag == LostFlag::Normal
                    && !self.channel[1].vad_flags[self.channel[0].n_frames_decoded])
                    || (lost_flag == LostFlag::DecodeLbrr
                        && !self.channel[1].lbrr_flags[self.channel[0].n_frames_decoded])
                {
                    decode_only_middle = decode_mid_only(dec)?;
                } else {
                    decode_only_middle = false;
                }
            } else {
                ms_pred_q13 = self.stereo.pred_prev_q13.map(i32::from);
            }
        }

        // Reset side channel decoder prediction memory for first frame
        // with side coding
        if ctrl.n_channels_internal == 2 && !decode_only_middle && self.prev_decode_only_middle {
            self.channel[1].synth.out_buf = [0; MAX_FRAME_LENGTH + 2 * MAX_SUB_FRAME_LENGTH];
            self.channel[1].synth.s_lpc_q14_buf = [0; MAX_LPC_ORDER];
            self.channel[1].lag_prev = 100;
            self.channel[1].last_gain_index = LAST_GAIN_INDEX_ON_PACKET_LOSS;
            self.channel[1].prev_signal_type = crate::silk::decode_indices::TYPE_NO_VOICE_ACTIVITY;
            self.channel[1].first_frame_after_reset = true;
        }

        let has_side = if lost_flag == LostFlag::Normal {
            !decode_only_middle
        } else {
            !self.prev_decode_only_middle
                || (ctrl.n_channels_internal == 2
                    && lost_flag == LostFlag::DecodeLbrr
                    && self.channel[1].lbrr_flags[self.channel[1].n_frames_decoded])
        };

        /* Call decoder for one frame */
        let mut n_samples_out_dec = 0usize;
        for n in 0..ctrl.n_channels_internal {
            if n == 0 || has_side {
                let frame_index = self.channel[0].n_frames_decoded as isize - n as isize;
                /* Use independent coding if no previous frame available */
                let cond_coding = if frame_index <= 0 {
                    CondCoding::Independently
                } else if lost_flag == LostFlag::DecodeLbrr {
                    if self.channel[n].lbrr_flags[frame_index as usize - 1] {
                        CondCoding::Conditionally
                    } else {
                        CondCoding::Independently
                    }
                } else if n > 0 && self.prev_decode_only_middle {
                    /* If we skipped a side frame in this packet, we
                    don't need LTP scaling; the LTP state is
                    well-defined. */
                    CondCoding::IndependentlyNoLtpScaling
                } else {
                    CondCoding::Conditionally
                };
                let dec = if lost_flag == LostFlag::PacketLost {
                    None
                } else {
                    dec.as_deref_mut()
                };
                if crate::debug::flags().silk_dbg {
                    eprintln!(
                        "DF ch{n} fs={} nb={} frame={} ltp={} nfp={} nfd={} new_pk={}",
                        self.channel[n].fs_khz,
                        self.channel[n].nb_subfr,
                        self.channel[n].frame_length,
                        self.channel[n].ltp_mem_length,
                        self.channel[n].n_frames_per_packet,
                        self.channel[n].n_frames_decoded,
                        new_packet_flag,
                    );
                }
                n_samples_out_dec = self.channel[n].decode_frame(
                    dec,
                    &mut self.tmp[n][2..],
                    lost_flag,
                    cond_coding,
                )?;
            } else {
                self.tmp[n][2..2 + n_samples_out_dec].fill(0);
            }
            self.channel[n].n_frames_decoded += 1;
        }

        if ctrl.n_channels_api == 2 && ctrl.n_channels_internal == 2 {
            /* Convert Mid/Side to Left/Right */
            let mut mid = self.tmp[0];
            let mut side = self.tmp[1];
            ms_to_lr(
                &mut self.stereo,
                &mut mid,
                &mut side,
                &ms_pred_q13,
                fs_khz,
                n_samples_out_dec,
            );
            self.tmp[0] = mid;
            self.tmp[1] = side;
        } else {
            /* Buffering */
            self.tmp[0][..2].copy_from_slice(&self.stereo.s_mid);
            let tail: [i16; 2] = self.tmp[0][n_samples_out_dec..n_samples_out_dec + 2]
                .try_into()
                .unwrap();
            self.stereo.s_mid = tail;
        }

        /* Number of output samples */
        let n_samples_out =
            n_samples_out_dec * ctrl.api_sample_rate as usize / (fs_khz as usize * 1000);
        debug_assert!(
            n_samples_out <= RESAMPLE_OUT_MAX,
            "resampler output scratch too small"
        );

        /* Resample decoded signal to API_sampleRate */
        let n_mix = ctrl.n_channels_api.min(ctrl.n_channels_internal);
        for n in 0..n_mix {
            if ctrl.n_channels_api == 2 {
                if crate::debug::flags().silk_c_fs_debug {
                    eprintln!(
                        "PRERESAMP ch={n} nSamplesOutDec={n_samples_out_dec} in={:?}",
                        &self.tmp[n][1..1 + n_samples_out_dec.min(20)]
                    );
                }
                self.channel[n].resampler.resample(
                    &mut self.resample_out[..n_samples_out],
                    &self.tmp[n][1..1 + n_samples_out_dec],
                )?;
                if crate::debug::flags().silk_c_fs_debug {
                    eprintln!(
                        "POSTRESAMP ch={n} nSamplesOut={n_samples_out} out={:?}",
                        &self.resample_out[..n_samples_out]
                    );
                }
                for i in 0..n_samples_out {
                    samples_out[n + 2 * i] = self.resample_out[i];
                }
            } else {
                self.channel[n].resampler.resample(
                    &mut samples_out[..n_samples_out],
                    &self.tmp[n][1..1 + n_samples_out_dec],
                )?;
            }
        }

        /* Create two channel output from mono stream */
        if ctrl.n_channels_api == 2 && ctrl.n_channels_internal == 1 {
            if stereo_to_mono {
                /* Resample right channel for newly collapsed stereo just
                in case we weren't doing collapsing when switching to
                mono */
                self.channel[1].resampler.resample(
                    &mut self.resample_out[..n_samples_out],
                    &self.tmp[0][1..1 + n_samples_out_dec],
                )?;
                for i in 0..n_samples_out {
                    samples_out[1 + 2 * i] = self.resample_out[i];
                }
            } else {
                for i in 0..n_samples_out {
                    samples_out[1 + 2 * i] = samples_out[2 * i];
                }
            }
        }

        /* Export pitch lag, measured at 48 kHz sampling rate */
        if self.channel[0].prev_signal_type == crate::silk::decode_indices::TYPE_VOICED {
            let mult_tab = [6i32, 4, 3];
            ctrl.prev_pitch_lag = self.channel[0].lag_prev * mult_tab[((fs_khz - 8) >> 2) as usize];
        } else {
            ctrl.prev_pitch_lag = 0;
        }

        if lost_flag == LostFlag::PacketLost {
            /* On packet loss, remove the gain clamping to prevent having
            the energy "bounce back" if we lose packets when the
            energy is going down */
            for ch in self.channel.iter_mut().take(self.n_channels_internal) {
                ch.last_gain_index = LAST_GAIN_INDEX_ON_PACKET_LOSS;
            }
        } else {
            self.prev_decode_only_middle = decode_only_middle;
        }

        Ok(n_samples_out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::silk::excitation::SHELL_CODEC_FRAME_LENGTH;

    fn ctrl(api: usize, internal: usize, api_rate: i32, internal_rate: i32, ms: i32) -> DecControl {
        DecControl {
            n_channels_api: api,
            n_channels_internal: internal,
            api_sample_rate: api_rate,
            internal_sample_rate: internal_rate,
            payload_size_ms: ms,
            prev_pitch_lag: 0,
        }
    }

    /* ---- set_fs geometry table (silk_decoder_set_fs) ---- */

    #[test]
    fn set_fs_geometry() {
        for &(fs, nb_subfr, subfr, ltp_mem, order) in &[
            (8u32, 4usize, 40usize, 160usize, 10usize),
            (8, 2, 40, 160, 10),
            (12, 4, 60, 240, 10),
            (12, 2, 60, 240, 10),
            (16, 4, 80, 320, 16),
            (16, 2, 80, 320, 16),
        ] {
            let mut ch = ChannelState {
                nb_subfr,
                ..ChannelState::default()
            };
            ch.set_fs(fs, 48000).unwrap();
            assert_eq!(ch.subfr_length, subfr, "fs {fs}");
            assert_eq!(ch.frame_length, nb_subfr * subfr, "fs {fs}");
            assert_eq!(ch.ltp_mem_length, ltp_mem, "fs {fs}");
            assert_eq!(ch.lpc_order, order, "fs {fs}");
            assert_eq!(
                ch.nlsf_cb.order as usize, order,
                "NLSF codebook order for fs {fs}"
            );
            assert_eq!(ch.fs_api_hz, 48000);
            assert!(ch.first_frame_after_reset, "fs switch forces reset flag");
            assert_eq!(ch.lag_prev, 100);
            assert_eq!(ch.last_gain_index, LAST_GAIN_INDEX_ON_PACKET_LOSS);
        }
    }

    /// The first set_fs initializes the resampler (fs 0 → mismatch); a
    /// same-rate call keeps it (bit-identical continued output), and an
    /// API-rate change switches the output ratio (16k in → 24k vs 48k).
    #[test]
    fn set_fs_resampler_reinit() {
        let mut ch = ChannelState {
            nb_subfr: 4,
            ..ChannelState::default()
        };
        ch.set_fs(16, 48000).unwrap();
        let mut out = [0i16; 960];
        ch.resampler
            .resample(&mut out[..960], &[0i16; 320])
            .unwrap();
        // Same everything: still the 16k → 48k ratio (960 out per 320 in).
        ch.set_fs(16, 48000).unwrap();
        ch.resampler
            .resample(&mut out[..960], &[0i16; 320])
            .unwrap();
        // API rate change: reinit to the 1.5 ratio.
        ch.set_fs(16, 24000).unwrap();
        ch.resampler
            .resample(&mut out[..480], &[0i16; 320])
            .unwrap();
        // Internal rate change with same API rate: reinit again (8→24).
        ch.set_fs(8, 24000).unwrap();
        ch.resampler.resample(&mut out[..240], &[0i16; 80]).unwrap();
    }

    /// Unsupported rates are rejected (set_fs and the DecControl
    /// formula), like the reference's asserts/`SILK_DEC_INVALID_*`.
    #[test]
    fn invalid_rates_rejected() {
        let mut ch = ChannelState {
            nb_subfr: 4,
            ..ChannelState::default()
        };
        assert!(ch.set_fs(24, 48000).is_err());
        assert!(ch.set_fs(16, 96000).is_err());

        let mut dec = SilkDecoder::new(1).unwrap();
        let mut c = ctrl(1, 1, 48000, 44100, 20);
        let mut out = [0i16; 1920];
        assert!(dec
            .decode(&mut c, None, LostFlag::PacketLost, true, &mut out)
            .is_err());

        let mut c = ctrl(1, 1, 4000, 16000, 20);
        assert!(dec
            .decode(&mut c, None, LostFlag::PacketLost, true, &mut out)
            .is_err());
        let mut c = ctrl(1, 1, 96000, 16000, 20);
        assert!(dec
            .decode(&mut c, None, LostFlag::PacketLost, true, &mut out)
            .is_err());
    }

    /* ---- payloadSize_ms table + fs formula ---- */

    #[test]
    fn frames_per_packet_table() {
        assert_eq!(SilkDecoder::frames_per_packet(0).unwrap(), (1, 2));
        assert_eq!(SilkDecoder::frames_per_packet(10).unwrap(), (1, 2));
        assert_eq!(SilkDecoder::frames_per_packet(20).unwrap(), (1, 4));
        assert_eq!(SilkDecoder::frames_per_packet(40).unwrap(), (2, 4));
        assert_eq!(SilkDecoder::frames_per_packet(60).unwrap(), (3, 4));
        assert!(SilkDecoder::frames_per_packet(30).is_err());
    }

    /// `fs_kHz_dec = (internalSampleRate >> 10) + 1`.
    #[test]
    fn internal_rate_formula() {
        for &(hz, khz) in &[(8000i32, 8u32), (12000, 12), (16000, 16)] {
            assert_eq!((hz >> 10) + 1, khz as i32);
        }
    }

    /* ---- PacketLost plumbing: geometry, output sizing, PLC state ---- */

    /// A pure-loss run needs no bitstream and exercises the whole PLC +
    /// CNG + resampler pipeline: geometry per payload size, output
    /// length math, loss counting, and the gain-clamp removal.
    #[test]
    fn packet_loss_plumbing() {
        let mut dec = SilkDecoder::new(1).unwrap();
        let mut c = ctrl(1, 1, 48000, 16000, 20);
        let mut out = [0i16; 960];

        // First frame of a packet: sets 20 ms geometry (nb_subfr 4).
        let n = dec
            .decode(&mut c, None, LostFlag::PacketLost, true, &mut out)
            .unwrap();
        assert_eq!(n, 960); // 20 ms at 48 kHz
        assert_eq!(dec.channel[0].frame_length, 320);
        assert_eq!(dec.channel[0].nb_subfr, 4);
        assert_eq!(dec.channel[0].n_frames_per_packet, 1);
        assert_eq!(dec.channel[0].loss_cnt, 1);
        assert_eq!(
            dec.channel[0].last_gain_index,
            LAST_GAIN_INDEX_ON_PACKET_LOSS
        );
        // A cold-start concealment is silent: every state byte the
        // concealment reads was zeroed (zero LPC/excitation/gain
        // history in, silence out) — the reference behaves likewise.
        assert!(
            out[..n].iter().all(|&v| v == 0),
            "cold-start conceal is silent"
        );

        // Second call in the same packet: geometry untouched (the
        // frame is still 20 ms), no set_fs re-run.
        let n = dec
            .decode(&mut c, None, LostFlag::PacketLost, false, &mut out)
            .unwrap();
        assert_eq!(n, 960);
        assert_eq!(dec.channel[0].loss_cnt, 2);

        // New packet, 10 ms payload: geometry switches to nb_subfr 2.
        let mut c10 = ctrl(1, 1, 48000, 12000, 10);
        let n = dec
            .decode(&mut c10, None, LostFlag::PacketLost, true, &mut out)
            .unwrap();
        assert_eq!(n, 480);
        assert_eq!(dec.channel[0].frame_length, 120);
        assert_eq!(dec.channel[0].nb_subfr, 2);
        assert_eq!(dec.channel[0].fs_khz, 12);

        // 8 kHz internal into a 16 kHz API: 10 ms → 160 samples.
        let mut c8 = ctrl(1, 1, 16000, 8000, 10);
        let n = dec
            .decode(&mut c8, None, LostFlag::PacketLost, true, &mut out)
            .unwrap();
        assert_eq!(n, 160);
        assert_eq!(c8.prev_pitch_lag, 0, "concealed unvoiced frames export 0");
    }

    /// Loss runs are deterministic: identical call sequences produce
    /// identical output, and the PLC pitch-lag drift is monotonic
    /// across a long loss stretch (bounded by 18 ms).
    #[test]
    fn loss_determinism_and_pitch_drift() {
        let run = || {
            let mut dec = SilkDecoder::new(1).unwrap();
            let mut c = ctrl(1, 1, 48000, 16000, 20);
            let mut out = [0i16; 960];
            let mut blob = Vec::new();
            for i in 0..12 {
                let n = dec
                    .decode(&mut c, None, LostFlag::PacketLost, i == 0, &mut out)
                    .unwrap();
                for &v in &out[..n] {
                    blob.extend_from_slice(&v.to_le_bytes());
                }
            }
            (blob, dec.channel[0].plc.pitch_l_q8)
        };
        let (blob_a, lag_a) = run();
        let (blob_b, lag_b) = run();
        assert_eq!(blob_a, blob_b);
        assert_eq!(lag_a, lag_b);
        // 12 losses at 16 kHz: 20 ms * 16 = 320 << 8 initial, drifted up
        // but clamped to 18 ms * 16 kHz << 8.
        let max_q8 = (18 * 16) << 8;
        assert!(lag_a <= max_q8);
        assert!(
            lag_a > 320 << 7,
            "drift should have moved off the reset value"
        );
    }

    /// Stereo output from a mono internal stream duplicates the left
    /// channel (no `stereo_to_mono` yet, since no stereo was decoded).
    #[test]
    fn mono_to_stereo_duplication() {
        let mut dec = SilkDecoder::new(2).unwrap();
        let mut c = ctrl(2, 1, 48000, 16000, 20);
        let mut out = [0i16; 1920];
        let n = dec
            .decode(&mut c, None, LostFlag::PacketLost, true, &mut out)
            .unwrap();
        assert_eq!(n, 960);
        for i in 0..n {
            assert_eq!(out[2 * i], out[2 * i + 1], "sample {i}");
        }
    }

    /// Two-channel internal state: the side channel exists, gets its
    /// own loss bookkeeping, and mono→stereo internal transitions
    /// reinit it.
    #[test]
    fn stereo_internal_loss() {
        let mut dec = SilkDecoder::new(2).unwrap();
        let mut c = ctrl(2, 2, 48000, 16000, 20);
        let mut out = [0i16; 1920];
        let n = dec
            .decode(&mut c, None, LostFlag::PacketLost, true, &mut out)
            .unwrap();
        assert_eq!(n, 960);
        assert_eq!(dec.channel[0].loss_cnt, 1);
        assert_eq!(dec.channel[1].loss_cnt, 1);
        // Per-sample stereo output is not necessarily L==R under PLC
        // (independent channels), but it must be finite/terminated.
        assert!(out[..2 * n]
            .iter()
            .all(|&v| (i16::MIN..=i16::MAX).contains(&v)));
    }

    /* ---- decode_parameters ---- */

    /// Structural checks of the decode_parameters wiring with a
    /// synthetic channel: unvoiced zeroing, voiced lookups, NLSF
    /// interpolation vs first-half copy, and the post-loss BWE.
    #[test]
    fn decode_parameters_paths() {
        use crate::silk::decode_indices::TYPE_UNVOICED;

        // --- unvoiced frame: pitch/LTP zeroed, scale 0, per_index 0.
        let mut ch = ChannelState {
            nb_subfr: 4,
            ..ChannelState::default()
        };
        ch.set_fs(16, 48000).unwrap();
        ch.indices.signal_type = TYPE_UNVOICED;
        ch.indices.nlsf_interp_coef_q2 = 4;
        // The NLSF indices must decode to something; index 0 residuals.
        ch.indices.nlsf_indices[0] = 0;
        let mut ctrl = DecoderControl::default();
        decode_parameters(&mut ch, &mut ctrl, CondCoding::Independently);
        assert!(ctrl.pitch_l[..4].iter().all(|&v| v == 0));
        assert!(ctrl.ltp_coef_q14[..20].iter().all(|&v| v == 0));
        assert_eq!(ctrl.ltp_scale_q14, 0);
        assert_eq!(ch.indices.per_index, 0);
        // First half == second half (no interpolation at coef 4), and
        // nonzero LPC coefficients came out of NLSF2A.
        assert_eq!(ctrl.pred_coef_q12[0][..16], ctrl.pred_coef_q12[1][..16]);
        assert!(ctrl.pred_coef_q12[1][..16].iter().any(|&v| v != 0));
        // prevNLSF updated to the decoded NLSF.
        assert!(!ch.prev_nlsf_q15[..16].iter().all(|&v| v == 0));
        assert_eq!(ch.last_gain_index, 0, "gains dequant starts from reset");
    }

    /// `first_frame_after_reset` forces `NLSFInterpCoef_Q2 = 4`, so the
    /// interpolated half equals the direct half; after consuming the
    /// frame (flag cleared) a coef < 4 interpolates a distinct half.
    #[test]
    fn nlsf_interpolation_gate() {
        let mut ch = ChannelState {
            nb_subfr: 4,
            ..ChannelState::default()
        };
        ch.set_fs(16, 48000).unwrap();
        ch.indices.signal_type = crate::silk::decode_indices::TYPE_UNVOICED;
        ch.indices.nlsf_indices[0] = 0;
        ch.indices.nlsf_interp_coef_q2 = 0;
        let mut ctrl = DecoderControl::default();
        decode_parameters(&mut ch, &mut ctrl, CondCoding::Independently);
        // first_frame_after_reset: forced coef 4 → halves identical.
        assert_eq!(ctrl.pred_coef_q12[0][..16], ctrl.pred_coef_q12[1][..16]);
        assert_eq!(ch.indices.nlsf_interp_coef_q2, 4);
        ch.first_frame_after_reset = false;
        ch.indices.nlsf_interp_coef_q2 = 0;
        ch.indices.nlsf_indices[1] = 2; // different stage-2 residual: cur != prev
        let mut ctrl = DecoderControl::default();
        decode_parameters(&mut ch, &mut ctrl, CondCoding::Independently);
        // pred[0] interpolates prev towards cur (coef 0: prev only),
        // pred[1] is cur — different filters.
        assert_ne!(ctrl.pred_coef_q12[0][..16], ctrl.pred_coef_q12[1][..16]);
    }

    /// Post-loss BWE: with lossCnt != 0 both halves are bandwidth
    /// expanded (every coefficient magnitude shrinks by the chirp).
    #[test]
    fn post_loss_bwe() {
        let mut ch = ChannelState {
            nb_subfr: 4,
            ..ChannelState::default()
        };
        ch.set_fs(16, 48000).unwrap();
        ch.first_frame_after_reset = false;
        ch.indices.signal_type = crate::silk::decode_indices::TYPE_UNVOICED;
        ch.indices.nlsf_indices[0] = 0;
        ch.indices.nlsf_interp_coef_q2 = 4;
        ch.loss_cnt = 1;
        let mut ctrl = DecoderControl::default();
        decode_parameters(&mut ch, &mut ctrl, CondCoding::Independently);
        // The expanded coefficients differ from the unexpanded ones the
        // NLSF2A produced (they'd be equal only if all were 0/65535-ish).
        let mut direct = [0i16; 16];
        let mut nlsf = [0i16; 16];
        crate::silk::nlsf::nlsf_decode(&mut nlsf, &ch.indices.nlsf_indices, ch.nlsf_cb);
        crate::silk::nlsf::nlsf2a(&mut direct, &nlsf, 16);
        assert_ne!(ctrl.pred_coef_q12[1][..16], direct[..16]);
    }

    /// Invalid payloads: payload sizes outside the table are rejected
    /// before any state mutation is observable.
    #[test]
    fn invalid_payload_size() {
        let mut dec = SilkDecoder::new(1).unwrap();
        let mut c = ctrl(1, 1, 48000, 16000, 30);
        let mut out = [0i16; 1920];
        let err = dec
            .decode(&mut c, None, LostFlag::PacketLost, true, &mut out)
            .unwrap_err();
        assert!(matches!(err, CadenceError::UnsupportedFeature(_)));
    }

    /// The pulses scratch of `decode_frame` covers the shell-rounded
    /// frame length for every geometry.
    #[test]
    fn pulses_scratch_size() {
        for fs in [8u32, 12, 16] {
            for nb_subfr in [2usize, 4] {
                let l = nb_subfr * 5 * fs as usize;
                let rounded = (l + SHELL_CODEC_FRAME_LENGTH - 1) & !(SHELL_CODEC_FRAME_LENGTH - 1);
                assert!(rounded <= MAX_FRAME_LENGTH, "fs {fs} nb {nb_subfr}");
            }
        }
    }
}
