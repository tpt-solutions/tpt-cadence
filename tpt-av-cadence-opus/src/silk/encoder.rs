//! SILK top-level encoder — mono and stereo (adaptive mid/side).
//!
//! Assembles the encoder modules into a `silk_encode_frame_FLP`-shaped
//! pipeline (1 or 2 internal channels, 10/20/40/60 ms *packets*, 8/12/16
//! kHz internal rate, VBR payloads, no LBRR/DTX/FEC/hybrid):
//!
//! 1. **Input**: API-rate `i16` samples (8/12/16/24/48 kHz) are resampled
//!    to the internal rate with the shared [`super::resampler::Resampler`]
//!    (encoder direction), and buffered with 20 ms of history — the same
//!    `x_buf` layout the reference uses for pitch/LTP context.
//! 2. **VAD** (foundation stand-in for `silk_VAD_GetSA_Q8`): a frame-RMS
//!    gate picks `TYPE_NO_VOICE_ACTIVITY` vs `TYPE_UNVOICED`; the pitch
//!    analysis may upgrade to `TYPE_VOICED` (as `silk_find_pitch_lags_FLP`
//!    does). The VAD flag mirrors the pre-pitch activity decision.
//! 3. **Pitch analysis** (foundation: full-resolution normalized
//!    cross-correlation over the 2–18 ms range instead of the reference's
//!    decimated two-stage search) → per-subframe lags, the primary lag
//!    index, the contour-codebook selection, and the LTP correlation used
//!    for the voiced decision.
//! 4. **Noise-shaping analysis** ([`super::noise_shape`],
//!    `silk_noise_shape_analysis_FLP`): per-subframe gains from a windowed
//!    *frequency-warped* autocorrelation, plus the shaping filter, spectral
//!    tilt, harmonic shaping gain and rate/distortion factor. The gains
//!    (adjusted by the target SNR from the exact [`control_snr`] port) feed
//!    the LPC analysis, the residual energies and the quantizer.
//! 5. **LTP + LPC** (`silk_find_pred_coefs_FLP` shape): [`find_ltp`] +
//!    [`quant_ltp_gains`] for voiced frames, the weighted LPC analysis on
//!    the (LTP-)residual, [`nlsf_encode`] for the transmitted NLSF vector,
//!    and the decoder-shared `nlsf2a` so the synthesis filters are exactly
//!    the decoder's.
//! 6. **Gains** (`silk_process_gains_FLP` shape): LTP-based reduction, the
//!    soft limit, and [`gains_quant`] into the transmitted indices.
//! 7. **Excitation** ([`nsq::encode_frame_nsq`]): closed-loop quantization
//!    over the decoder's exact arithmetic.
//! 8. **Bitstream**: VAD/LBRR prologue (LBRR always off), then per-frame
//!    side info + excitation, all in one range-coded payload.
//!
//! A payload covers the whole *packet* (RFC 6716 sense): 10/20 ms packets
//! carry one SILK frame; 40/60 ms packets carry two/three 20 ms frames in
//! the reference's intra-packet arrangement — frame 0 coded independently,
//! later frames conditionally (delta gains against the previous frame's
//! last subframe, delta pitch lags when the previous frame was voiced, no
//! LTP-scale symbol, NLSF interpolation factor still transmitted but left
//! at 4 = no interpolation). This is exactly what a decoder expects for
//! TOC configs 0–11 (`payloadSize_ms` → `nFramesPerPacket` of 1/2/3), so
//! each payload drops into a single-frame code-0 Opus packet unchanged.
//!
//! The persistent decoder-mirror state ([`SynthesisState`],
//! `LastGainIndex`, `prevNLSF_Q15`, `ec_prev`, `lagPrev`,
//! `prevSignalType`, the persistent side-info `indices` whose
//! `ltp_scale_index` carries across conditionally coded frames) matches a
//! fresh [`SilkDecoder`]'s initialization, so encoding then decoding from
//! reset reproduces the encoder's simulated output bit-for-bit (pinned by
//! tests).
//!
//! **Stereo** (`new_stereo`): the left/right input is converted to
//! adaptive mid/side per frame ([`stereo::lr_to_ms`]) after resampling —
//! exactly mirroring the decoder's `ms_to_lr` unmixing in reverse, with
//! the mid/side predictor chosen by least squares and quantized to the
//! decoder's table. Each frame carries its own predictor indices and —
//! when the side residual is inactive — a mid-only flag; the side channel
//! is then skipped entirely (the decoder reconstructs side from the
//! prediction alone and resets its state on the next coded side frame,
//! which this encoder mirrors). Per-frame and per-channel conditional
//! coding follow the decoder's rules precisely: the mid channel is
//! independent on frame 0 and conditional afterwards; the side channel's
//! frame index is offset by one (independent for frames 0 and 1), and a
//! side frame following a skipped one drops the LTP-scale symbol
//! (`CODE_INDEPENDENTLY_NO_LTP_SCALING`).
//!
//! SOURCE: Xiph.Org libopus 1.5.2, `silk/float/encode_frame_FLP.c`,
//! `find_pitch_lags_FLP.c`, `find_pred_coefs_FLP.c`,
//! `noise_shape_analysis_FLP.c`, `process_gains_FLP.c`,
//! `silk/control_SNR.c` (tables ported verbatim), `silk/define.h`,
//! `silk/enc_API.c` (per-packet VAD/LBRR prologue) (BSD-3-Clause).
#![allow(dead_code)]

use crate::silk::decode_indices::{
    CondCoding, EcPrevState, FrameParams, SideInfoIndices, MAX_FRAMES_PER_PACKET, MAX_LPC_ORDER,
    MAX_NB_SUBFR, TYPE_NO_VOICE_ACTIVITY, TYPE_UNVOICED, TYPE_VOICED,
};
use crate::silk::encode_indices::{
    encode_indices, encode_lbrr_flags, encode_vad_flags_and_lbrr_flag,
};
use crate::silk::encode_pulses::encode_pulses;
use crate::silk::gains::gains_quant;
use crate::silk::lpc_analysis::{
    a2nlsf, apply_sine_window, autocorrelation, burg_modified_f32, bwexpander_f32, energy,
    interpolate_i16, k2a, lpc_analysis_filter, schur,
};
use crate::silk::ltp_quant::{correlations_to_q17, find_ltp, ltp_correlation, quant_ltp_gains};
use crate::silk::nlsf::nlsf2a;
use crate::silk::nlsf_quant::{nlsf_encode, nlsf_vq_weights_laroia};
use crate::silk::noise_shape::{
    noise_shape_analysis, ShapeGeometry, ShapeParams, ShapeState, LA_SHAPE_MAX,
};
use crate::silk::nsq::encode_frame_nsq;
use crate::silk::resampler::Resampler;
use crate::silk::stereo::{
    encode_mid_only_flag, encode_stereo_pred, lr_to_ms, StereoEncState, StereoPredIx,
};
use crate::silk::synthesis::{DecoderControl, FrameInfo, SynthesisState, MAX_FRAME_LENGTH};
use crate::silk::tables::{
    NlsfCbStruct, CB_LAGS_STAGE2, CB_LAGS_STAGE2_10_MS, CB_LAGS_STAGE3, CB_LAGS_STAGE3_10_MS,
    NLSF_CB_NB_MB, NLSF_CB_WB,
};
use crate::{CadenceError, Result};

/// `FIND_PITCH_WHITE_NOISE_FRACTION` (`silk/tuning_parameters.h`).
const FIND_PITCH_WHITE_NOISE_FRACTION: f32 = 1e-3;
/// `FIND_PITCH_BANDWIDTH_EXPANSION`.
const FIND_PITCH_BANDWIDTH_EXPANSION: f32 = 0.99;
/// `SHAPE_WHITE_NOISE_FRACTION`.
const SHAPE_WHITE_NOISE_FRACTION: f32 = 3e-5;
/// `ENERGY_VARIATION_THRESHOLD_QNT_OFFSET`.
const ENERGY_VARIATION_THRESHOLD_QNT_OFFSET: f32 = 0.6;
/// `PE_MIN_LAG_MS` / `PE_MAX_LAG_MS` (`silk/pitch_est_defines.h`).
const PE_MIN_LAG_MS: i32 = 2;
const PE_MAX_LAG_MS: i32 = 18;
/// Foundation VAD gate: frames whose RMS (i16 units) falls below this are
/// coded as `TYPE_NO_VOICE_ACTIVITY`.
const INACTIVE_RMS_THRESHOLD: f32 = 16.0;
/// `NB_SPEECH_FRAMES_BEFORE_DTX` (`silk/define.h`): inactive frames are
/// still coded until this many have passed.
const NB_SPEECH_FRAMES_BEFORE_DTX: u32 = 10;
/// `MAX_CONSECUTIVE_DTX` (`silk/define.h`): past this many consecutive
/// inactive frames the packet is skipped (DTX), the counter recycling to
/// [`NB_SPEECH_FRAMES_BEFORE_DTX`].
const MAX_CONSECUTIVE_DTX: u32 = 20;
/// `MAX_PREDICTION_POWER_GAIN` (`silk/define.h`): 1e4 linear (~80 dB) —
/// the total-prediction-gain cap behind `minInvGain`.
const MAX_PREDICTION_POWER_GAIN: f32 = 1e4;
/// `MAX_PREDICTION_POWER_GAIN_AFTER_RESET` (`silk/define.h`): 1e2 linear
/// (~40 dB) on the first frame after a reset.
const MAX_PREDICTION_POWER_GAIN_AFTER_RESET: f32 = 1e2;
/// Low-activity override (`silk/control_codec.c`): speech activity below
/// 0.2 in Q8 downgrades the frame to `TYPE_NO_VOICE_ACTIVITY`.
const VAD_INACTIVE_SA_Q8: i32 = 51;

/// `silk_control_SNR` (`silk/control_SNR.c`): SNR values divided by 21 for
/// target bitrates spaced at 400 bps intervals; the first 10 entries
/// (0–4 kb/s) are omitted because they are all zero.
const TARGET_RATE_NB_21: [u8; 117 - 10] = [
    0, 15, 39, 52, 61, 68, 74, 79, 84, 88, 92, 95, 99, 102, 105, 108, 111, 114, 117, 119, 122, 124,
    126, 129, 131, 133, 135, 137, 139, 142, 143, 145, 147, 149, 151, 153, 155, 157, 158, 160, 162,
    163, 165, 167, 168, 170, 171, 173, 174, 176, 177, 179, 180, 182, 183, 185, 186, 187, 189, 190,
    192, 193, 194, 196, 197, 199, 200, 201, 203, 204, 205, 207, 208, 209, 211, 212, 213, 215, 216,
    217, 219, 220, 221, 223, 224, 225, 227, 228, 230, 231, 232, 234, 235, 236, 238, 239, 241, 242,
    243, 245, 246, 248, 249, 250, 252, 253, 255,
];
const TARGET_RATE_MB_21: [u8; 165 - 10] = [
    0, 0, 28, 43, 52, 59, 65, 70, 74, 78, 81, 85, 87, 90, 93, 95, 98, 100, 102, 105, 107, 109, 111,
    113, 115, 116, 118, 120, 122, 123, 125, 127, 128, 130, 131, 133, 134, 136, 137, 138, 140, 141,
    143, 144, 145, 147, 148, 149, 151, 152, 153, 154, 156, 157, 158, 159, 160, 162, 163, 164, 165,
    166, 167, 168, 169, 171, 172, 173, 174, 175, 176, 177, 178, 179, 180, 181, 182, 183, 184, 185,
    186, 187, 188, 188, 189, 190, 191, 192, 193, 194, 195, 196, 197, 198, 199, 200, 201, 202, 203,
    203, 204, 205, 206, 207, 208, 209, 210, 211, 212, 213, 214, 214, 215, 216, 217, 218, 219, 220,
    221, 222, 223, 224, 224, 225, 226, 227, 228, 229, 230, 231, 232, 233, 234, 235, 236, 236, 237,
    238, 239, 240, 241, 242, 243, 244, 245, 246, 247, 248, 249, 250, 251, 252, 253, 254, 255,
];
const TARGET_RATE_WB_21: [u8; 201 - 10] = [
    0, 0, 0, 8, 29, 41, 49, 56, 62, 66, 70, 74, 77, 80, 83, 86, 88, 91, 93, 95, 97, 99, 101, 103,
    105, 107, 108, 110, 112, 113, 115, 116, 118, 119, 121, 122, 123, 125, 126, 127, 129, 130, 131,
    132, 134, 135, 136, 137, 138, 140, 141, 142, 143, 144, 145, 146, 147, 148, 149, 150, 151, 152,
    153, 154, 156, 157, 158, 159, 159, 160, 161, 162, 163, 164, 165, 166, 167, 168, 169, 170, 171,
    171, 172, 173, 174, 175, 176, 177, 177, 178, 179, 180, 181, 181, 182, 183, 184, 185, 185, 186,
    187, 188, 189, 189, 190, 191, 192, 192, 193, 194, 195, 195, 196, 197, 198, 198, 199, 200, 200,
    201, 202, 203, 203, 204, 205, 206, 206, 207, 208, 209, 209, 210, 211, 211, 212, 213, 214, 214,
    215, 216, 216, 217, 218, 219, 219, 220, 221, 221, 222, 223, 224, 224, 225, 226, 226, 227, 228,
    229, 229, 230, 231, 232, 232, 233, 234, 234, 235, 236, 237, 237, 238, 239, 240, 240, 241, 242,
    243, 243, 244, 245, 246, 246, 247, 248, 249, 249, 250, 251, 252, 253, 255,
];

/// `silk_control_SNR` (`silk/control_SNR.c`), ported exactly (the
/// 10 ms/12 kHz `nb_subfr == 2` rate reduction included): maps the target
/// bitrate to the residual quantizer's SNR target in dB·128 (Q7).
fn control_snr(fs_khz: u32, nb_subfr: usize, mut target_rate_bps: i32) -> i32 {
    if nb_subfr == 2 {
        target_rate_bps -= 2000 + fs_khz as i32 / 16;
    }
    let table: &[u8] = match fs_khz {
        8 => &TARGET_RATE_NB_21,
        12 => &TARGET_RATE_MB_21,
        _ => &TARGET_RATE_WB_21,
    };
    let bound = table.len();
    let id = (target_rate_bps + 200) / 400;
    let id = (id - 10).min(bound as i32 - 1);
    if id <= 0 {
        0
    } else {
        table[id as usize] as i32 * 21
    }
}

/// Per-channel geometry, decoder-mirror state, and analysis chain. The
/// decoder keeps one fully independent `silk_decoder_channel_state` per
/// channel; this mirrors that (including per-channel copies of the frame
/// geometry and SNR target, which are stream-constant here).
struct ChannelState {
    /* Geometry */
    fs_khz: u32,
    /// Subframes per SILK frame: 4 (20 ms frames) or 2 (10 ms frames).
    nb_subfr: usize,
    /// Samples per SILK frame at the internal rate.
    frame_length: usize,
    subfr_length: usize,
    ltp_mem_length: usize,
    predict_lpc_order: usize,
    pitch_lpc_order: usize,
    frame: FrameInfo,

    /* Rate control */
    snr_db_q7: i32,

    /* Decoder-mirror state (initialized like a fresh `SilkDecoder`) */
    synth: SynthesisState,
    exc_q14: [i32; MAX_FRAME_LENGTH],
    last_gain_index: i8,
    prev_nlsf_q15: [i16; MAX_LPC_ORDER],
    ec_prev: EcPrevState,
    lag_prev: i32,
    prev_signal_type: i8,
    sum_log_gain_q7: i32,
    first_frame_after_reset: bool,
    /// The decoder's persistent `psDec->indices`: fields a frame does not
    /// code (notably `ltp_scale_index` of conditionally coded frames)
    /// carry the previous frame's values, exactly as on the decode side.
    indices: SideInfoIndices,

    /* Input path */
    resampler: Resampler,
    /// `[ltp_mem history | current frame | la_shape look-ahead]` at the
    /// internal rate. The trailing look-ahead is zero-filled (see
    /// [`crate::silk::noise_shape`]).
    x_buf: Vec<f32>,
    vad_flags: [bool; MAX_FRAMES_PER_PACKET],
    nsq: crate::silk::nsq_ref::NsqState,

    /* Noise shaping (encoder-only; see `silk::noise_shape`) */
    /// `sShape`: the smoothed tilt / harmonic shaping gain.
    shape: ShapeState,
    /// `input_quality_bands_Q15` from the latest VAD evaluation.
    input_quality_bands_q15: [i32; 4],
    /// `sVAD`: the reference 4-band voice-activity detector.
    vad: crate::silk::vad::VadState,
    /// `speech_activity_Q8` stand-in derived from the frame-RMS VAD gate.
    speech_activity_q8: i32,

    /* Analysis output retained for the caller/tests */
    last_xq: Vec<i16>,
}

impl ChannelState {
    /// Fresh per-channel state from a fresh `SilkDecoder`'s
    /// initialization (zeros everywhere except `first_frame_after_reset`
    /// and the seed/`lagPrev` conventions the decoder uses).
    fn new(
        (
            fs_khz,
            nb_subfr,
            frame_length,
            subfr_length,
            ltp_mem_length,
            predict_lpc_order,
            pitch_lpc_order,
        ): (u32, usize, usize, usize, usize, usize, usize),
        api_sample_rate: i32,
        internal_sample_rate: i32,
    ) -> Result<Self> {
        let resampler = Resampler::new(api_sample_rate, internal_sample_rate, true)?;
        Ok(ChannelState {
            nsq: crate::silk::nsq_ref::NsqState::default(),
            vad: crate::silk::vad::VadState::default(),
            input_quality_bands_q15: [32768; 4],
            fs_khz,
            nb_subfr,
            frame_length,
            subfr_length,
            ltp_mem_length,
            predict_lpc_order,
            pitch_lpc_order,
            frame: FrameInfo::new(fs_khz, nb_subfr),
            snr_db_q7: 0,
            synth: SynthesisState::default(),
            exc_q14: [0; MAX_FRAME_LENGTH],
            last_gain_index: 0,
            prev_nlsf_q15: [0; MAX_LPC_ORDER],
            ec_prev: EcPrevState::default(),
            lag_prev: 0,
            prev_signal_type: TYPE_NO_VOICE_ACTIVITY,
            sum_log_gain_q7: 0,
            first_frame_after_reset: true,
            indices: SideInfoIndices::default(),
            resampler,
            x_buf: vec![0.0; ltp_mem_length + frame_length + LA_SHAPE_MAX],
            vad_flags: [false; MAX_FRAMES_PER_PACKET],
            shape: ShapeState::default(),
            speech_activity_q8: 0,
            last_xq: Vec::new(),
        })
    }

    /// A placeholder second channel for mono streams (never analyzed nor
    /// serialized; mirrors how the decoder builds its second channel
    /// state lazily). The `Resampler` has no all-zero state; the rates
    /// are irrelevant placeholders, exactly like the decoder's own
    /// `ChannelState::default` placeholder.
    fn placeholder() -> Self {
        ChannelState {
            nsq: crate::silk::nsq_ref::NsqState::default(),
            vad: crate::silk::vad::VadState::default(),
            input_quality_bands_q15: [32768; 4],
            fs_khz: 8,
            nb_subfr: MAX_NB_SUBFR,
            frame_length: MAX_FRAME_LENGTH,
            subfr_length: 40,
            ltp_mem_length: 160,
            predict_lpc_order: MAX_LPC_ORDER,
            pitch_lpc_order: 6,
            frame: FrameInfo::new(8, MAX_NB_SUBFR),
            snr_db_q7: 0,
            synth: SynthesisState::default(),
            exc_q14: [0; MAX_FRAME_LENGTH],
            last_gain_index: 0,
            prev_nlsf_q15: [0; MAX_LPC_ORDER],
            ec_prev: EcPrevState::default(),
            lag_prev: 0,
            prev_signal_type: TYPE_NO_VOICE_ACTIVITY,
            sum_log_gain_q7: 0,
            first_frame_after_reset: true,
            indices: SideInfoIndices::default(),
            resampler: Resampler::new(8_000, 8_000, true).unwrap(),
            x_buf: vec![0.0; 160 + MAX_FRAME_LENGTH + LA_SHAPE_MAX],
            vad_flags: [false; MAX_FRAMES_PER_PACKET],
            shape: ShapeState::default(),
            speech_activity_q8: 0,
            last_xq: Vec::new(),
        }
    }

    /// Mirrors the decoder's side-channel reset on the first coded side
    /// frame after a skipped (mid-only) one: synthesis memory zeroed,
    /// pitch/gain/signal-type state re-seeded.
    fn reset_after_mid_only(&mut self) {
        self.synth.out_buf = [0; crate::silk::synthesis::MAX_FRAME_LENGTH
            + 2 * crate::silk::synthesis::MAX_SUB_FRAME_LENGTH];
        self.synth.s_lpc_q14_buf = [0; MAX_LPC_ORDER];
        self.lag_prev = 100;
        self.last_gain_index = crate::silk::gains::LAST_GAIN_INDEX_ON_PACKET_LOSS;
        self.prev_signal_type = TYPE_NO_VOICE_ACTIVITY;
        self.first_frame_after_reset = true;
    }
}

/// The SILK frame encoder.
pub struct SilkEncoder {
    api_sample_rate: i32,
    nlsf_cb: &'static NlsfCbStruct,
    /// SILK frames per payload: 1 (10/20 ms packets), 2 (40 ms), 3 (60 ms).
    packet_frames: usize,
    /// Duration of one SILK frame in ms (10 or 20).
    frame_ms: i32,
    /// Total payload duration in ms (`frame_ms * packet_frames`).
    packet_ms: i32,
    /// Internal channel count: 1 (mono) or 2 (adaptive mid/side).
    channels_internal: usize,
    ch: [ChannelState; 2],
    /// Stereo MS transform state (unused for mono).
    stereo: StereoEncState,
    /// Mirrors the decoder's persistent `prev_decode_only_middle`.
    prev_decode_only_middle: bool,
    /// The caller's rate target (`set_bitrate`); the CBR retry loop
    /// derives reduced working rates from it.
    target_rate_bps: i32,
    /// CBR sizing mode (`None` = plain VBR).
    cbr: Option<CbrMode>,
    /// Per-frame mid-only decisions of the packet being encoded.
    mid_only: [bool; MAX_FRAMES_PER_PACKET],
    /// Per-frame quantized MS predictor indices.
    pred_ix: [StereoPredIx; MAX_FRAMES_PER_PACKET],
    frame_counter: u32,
    /// DTX (`set_dtx`): reference `noSpeechCounter`/`inDTX` pair,
    /// persistent across frames and packets.
    dtx_enabled: bool,
    no_speech_counter: u32,
    in_dtx: bool,
    /// LBRR (`set_packet_loss_perc`): the previous packet's stored
    /// frames, re-serialized into the next payload's LBRR slots.
    lbrr_enabled: bool,
    prev_lbrr: Option<Box<LbrrStored>>,
}

/// The previous packet's coded frames, stored for LBRR re-serialization.
struct LbrrStored {
    /// Per channel, per frame: side-info indices and excitation pulses.
    indices: [[SideInfoIndices; MAX_FRAMES_PER_PACKET]; 2],
    pulses: [[[i16; MAX_FRAME_LENGTH]; MAX_FRAMES_PER_PACKET]; 2],
    /// Per frame: the quantized MS predictor indices (stereo only).
    pred_ix: [StereoPredIx; MAX_FRAMES_PER_PACKET],
    /// Per frame: whether the side channel was coded (false = mid-only).
    side_coded: [bool; MAX_FRAMES_PER_PACKET],
    /// Per frame: whether the mid frame was stored at all — the
    /// reference's LBRR only covers ACTIVE frames (an inactive frame's
    /// signal-type symbol has no LBRR encoding).
    mid_stored: [bool; MAX_FRAMES_PER_PACKET],
}

fn lbrr_flags_at(
    lbrr0: &[bool; MAX_FRAMES_PER_PACKET],
    lbrr1: &[bool; MAX_FRAMES_PER_PACKET],
    ch: usize,
    i: usize,
) -> bool {
    if ch == 0 {
        lbrr0[i]
    } else {
        lbrr1[i]
    }
}

/// The NLSF codebook for an internal rate (`silk_decoder_set_fs`):
/// wideband at 16 kHz, the shared NB/MB book otherwise.
fn nlsf_cb_for(fs_khz: u32) -> &'static NlsfCbStruct {
    if fs_khz == 16 {
        &NLSF_CB_WB
    } else {
        &NLSF_CB_NB_MB
    }
}

/// CBR sizing mode for [`SilkEncoder::encode_frame_into`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CbrMode {
    /// Standalone payload must be exactly this many bytes: oversized
    /// payloads are re-encoded at a reduced rate, undersized ones are
    /// padded with zero bytes (which the SILK decoder never reads — its
    /// range coding has no end-relative raw bits).
    ExactBytes(usize),
    /// Written symbols must fit this many bytes (plus a small headroom
    /// for the hybrid redundancy-bit lookahead): oversized payloads are
    /// re-encoded at a reduced rate, nothing is padded. Used by the
    /// hybrid packet assembler, whose CELT layer continues on the same
    /// range coder.
    MaxBytes(usize),
}

/// Maximum CBR sizing re-encodes per payload (each attempt quarters the
/// previous working rate; eight attempts reach ~10% of the original,
/// below which the payload is near-silence and always fits).
const CBR_MAX_ATTEMPTS: usize = 12;

/// Full mutable-state snapshot of one channel, taken before a CBR
/// sizing attempt and restored when the attempt must be retried at a
/// lower rate (so every attempt starts from identical state and the
/// final attempt's end state is exactly what the decoder will have).
struct ChannelSnapshot {
    synth: SynthesisState,
    exc_q14: [i32; MAX_FRAME_LENGTH],
    last_gain_index: i8,
    prev_nlsf_q15: [i16; MAX_LPC_ORDER],
    ec_prev: EcPrevState,
    lag_prev: i32,
    prev_signal_type: i8,
    sum_log_gain_q7: i32,
    first_frame_after_reset: bool,
    indices: SideInfoIndices,
    x_buf: Vec<f32>,
    vad_flags: [bool; MAX_FRAMES_PER_PACKET],
    last_xq: Vec<i16>,
    /// The resampler retains the previous call's tail between calls, so
    /// re-sampling the same chunk a second time (a CBR retry) starting
    /// from the post-call state would corrupt the internal-rate signal.
    resampler: Resampler,
    /// Noise-shaping analysis state (tilt / harmonic smoothing) and the
    /// NSQ's shaping history — both advance per frame, so a retry must
    /// rewind them exactly like the decoder-mirror state.
    shape: ShapeState,

    speech_activity_q8: i32,
}

impl ChannelState {
    fn snapshot(&self) -> ChannelSnapshot {
        ChannelSnapshot {
            synth: self.synth,
            exc_q14: self.exc_q14,
            last_gain_index: self.last_gain_index,
            prev_nlsf_q15: self.prev_nlsf_q15,
            ec_prev: self.ec_prev,
            lag_prev: self.lag_prev,
            prev_signal_type: self.prev_signal_type,
            sum_log_gain_q7: self.sum_log_gain_q7,
            first_frame_after_reset: self.first_frame_after_reset,
            indices: self.indices,
            x_buf: self.x_buf.clone(),
            vad_flags: self.vad_flags,
            last_xq: self.last_xq.clone(),
            resampler: self.resampler.clone(),
            shape: self.shape,

            speech_activity_q8: self.speech_activity_q8,
        }
    }

    fn restore(&mut self, s: &ChannelSnapshot) {
        self.synth = s.synth;
        self.exc_q14 = s.exc_q14;
        self.last_gain_index = s.last_gain_index;
        self.prev_nlsf_q15 = s.prev_nlsf_q15;
        self.ec_prev = s.ec_prev;
        self.lag_prev = s.lag_prev;
        self.prev_signal_type = s.prev_signal_type;
        self.sum_log_gain_q7 = s.sum_log_gain_q7;
        self.first_frame_after_reset = s.first_frame_after_reset;
        self.indices = s.indices;
        self.x_buf = s.x_buf.clone();
        self.vad_flags = s.vad_flags;
        self.last_xq = s.last_xq.clone();
        self.resampler = s.resampler.clone();
        self.shape = s.shape;
        self.speech_activity_q8 = s.speech_activity_q8;
    }
}

/// Full mutable-state snapshot of the encoder (see [`ChannelSnapshot`]).
struct EncoderSnapshot {
    ch: [ChannelSnapshot; 2],
    stereo: StereoEncState,
    prev_decode_only_middle: bool,
    frame_counter: u32,
}

impl SilkEncoder {
    /// Creates an encoder for mono input at `api_sample_rate` Hz (one of
    /// 8/12/16/24/48 kHz) coded at `internal_sample_rate` Hz (8/12/16 kHz)
    /// with 10/20/40/60 ms packets. A 10 ms packet carries one 10 ms SILK
    /// frame; a 20/40/60 ms packet carries one/two/three 20 ms SILK frames
    /// (later frames of multi-frame packets conditionally coded).
    pub fn new(
        api_sample_rate: i32,
        internal_sample_rate: i32,
        packet_size_ms: i32,
    ) -> Result<Self> {
        Self::new_impl(api_sample_rate, internal_sample_rate, packet_size_ms, 1)
    }

    /// Stereo counterpart to [`Self::new`]: adaptive mid/side coding with
    /// the per-frame quantized MS predictor and mid-only side skipping of
    /// the reference. `input` to [`Self::encode_frame_into`] is
    /// interleaved left/right; the payload carries the stereo TOC bit.
    pub fn new_stereo(
        api_sample_rate: i32,
        internal_sample_rate: i32,
        packet_size_ms: i32,
    ) -> Result<Self> {
        Self::new_impl(api_sample_rate, internal_sample_rate, packet_size_ms, 2)
    }

    fn new_impl(
        api_sample_rate: i32,
        internal_sample_rate: i32,
        packet_size_ms: i32,
        channels_internal: usize,
    ) -> Result<Self> {
        crate::debug::init();
        let fs_khz = match internal_sample_rate {
            8000 => 8u32,
            12000 => 12,
            16000 => 16,
            _ => {
                return Err(CadenceError::UnsupportedFeature(format!(
                    "unsupported SILK internal sample rate {internal_sample_rate} Hz"
                )))
            }
        };
        let (frame_ms, packet_frames) = match packet_size_ms {
            10 => (10i32, 1usize),
            20 => (20, 1),
            40 => (20, 2),
            60 => (20, 3),
            _ => {
                return Err(CadenceError::UnsupportedFeature(format!(
                    "unsupported SILK packet size {packet_size_ms} ms"
                )))
            }
        };
        let nb_subfr = if frame_ms == 10 {
            MAX_NB_SUBFR / 2
        } else {
            MAX_NB_SUBFR
        };
        if !(8000..=48000).contains(&api_sample_rate) || api_sample_rate % 1000 != 0 {
            return Err(CadenceError::UnsupportedFeature(format!(
                "unsupported SILK API sample rate {api_sample_rate} Hz"
            )));
        }

        let subfr_length = 5 * fs_khz as usize;
        let frame_length = nb_subfr * subfr_length;
        let ltp_mem_length = 20 * fs_khz as usize;
        let predict_lpc_order = if fs_khz == 16 { 16 } else { 10 };
        let pitch_lpc_order = 6.min(predict_lpc_order);

        let geom = (
            fs_khz,
            nb_subfr,
            frame_length,
            subfr_length,
            ltp_mem_length,
            predict_lpc_order,
            pitch_lpc_order,
        );
        let ch0 = ChannelState::new(geom, api_sample_rate, internal_sample_rate)?;
        let ch1 = if channels_internal == 2 {
            ChannelState::new(geom, api_sample_rate, internal_sample_rate)?
        } else {
            ChannelState::placeholder()
        };

        Ok(SilkEncoder {
            api_sample_rate,
            nlsf_cb: if fs_khz == 16 {
                &NLSF_CB_WB
            } else {
                &NLSF_CB_NB_MB
            },
            packet_frames,
            frame_ms,
            packet_ms: frame_ms * packet_frames as i32,
            channels_internal,
            ch: [ch0, ch1],
            stereo: StereoEncState::default(),
            prev_decode_only_middle: false,
            target_rate_bps: 0,
            cbr: None,
            mid_only: [false; MAX_FRAMES_PER_PACKET],
            pred_ix: [StereoPredIx::default(); MAX_FRAMES_PER_PACKET],
            frame_counter: 0,
            dtx_enabled: false,
            no_speech_counter: 0,
            in_dtx: false,
            lbrr_enabled: false,
            prev_lbrr: None,
        })
    }

    /// Snapshot of all mutable encoder state (CBR sizing retries).
    fn snapshot_state(&self) -> EncoderSnapshot {
        EncoderSnapshot {
            ch: [self.ch[0].snapshot(), self.ch[1].snapshot()],
            stereo: self.stereo,
            prev_decode_only_middle: self.prev_decode_only_middle,
            frame_counter: self.frame_counter,
        }
    }

    fn restore_state(&mut self, s: &EncoderSnapshot) {
        self.ch[0].restore(&s.ch[0]);
        self.ch[1].restore(&s.ch[1]);
        self.stereo = s.stereo;
        self.prev_decode_only_middle = s.prev_decode_only_middle;
        self.frame_counter = s.frame_counter;
    }

    /// `silk_control_SNR`: sets the rate target (per internal channel).
    /// `target_rate_bps` is the per-mono-channel SILK-mode target;
    /// packet overhead is not modeled. With CBR sizing enabled this is
    /// also the *starting* rate the sizing loop reduces from.
    pub fn set_bitrate(&mut self, target_rate_bps: i32) {
        self.target_rate_bps = target_rate_bps;
        self.set_working_rate(target_rate_bps);
    }

    /// Applies a working rate without touching the caller's target (the
    /// CBR retry loop reduces this until the payload fits).
    fn set_working_rate(&mut self, rate_bps: i32) {
        for ch in self.ch.iter_mut().take(self.channels_internal) {
            ch.snr_db_q7 = control_snr(ch.fs_khz, ch.nb_subfr, rate_bps.max(0));
        }
    }

    /// Enables CBR sizing: [`Self::encode_frame`] returns payloads of
    /// exactly `bytes` bytes. Oversized payloads are re-encoded at a
    /// progressively reduced rate (up to [`CBR_MAX_ATTEMPTS`] attempts;
    /// an error is returned if even the quietest encoding does not fit);
    /// undersized payloads are padded with zero bytes, which the SILK
    /// decoder never reads (its range coding has no end-relative raw
    /// bits), so padding is lossless.
    pub fn set_cbr_bytes(&mut self, bytes: usize) -> Result<()> {
        if bytes < 8 {
            return Err(CadenceError::InvalidFormat(format!(
                "SILK CBR payload size {bytes} bytes is below the 8-byte minimum"
            )));
        }
        self.cbr = Some(CbrMode::ExactBytes(bytes));
        Ok(())
    }

    /// Enables discontinuous transmission (`useDTX`): packets whose
    /// every SILK frame is voice-inactive (per the VAD stand-in's RMS
    /// gate, using the reference's `noSpeechCounter`/`inDTX` schedule —
    /// the first 10 inactive frames still coded, all after the 20th
    /// skipped) are emitted as 1-byte packets (TOC byte only), which the
    /// decoder decodes as comfort-noise generation. Only meaningful for
    /// standalone SILK streams; the hybrid always codes its CELT layer.
    pub fn set_dtx(&mut self, enabled: bool) {
        self.dtx_enabled = enabled;
        if !enabled {
            self.no_speech_counter = 0;
            self.in_dtx = false;
        }
    }

    /// Enables low-bitrate redundancy (`useLBRR`): each payload carries
    /// re-serialized copies of the previous packet's coded frames (side
    /// info + excitation, in the decoder's exact LBRR skip order), so a
    /// lost packet can be recovered from its successor's LBRR data. The
    /// foundation's policy codes LBRR for every frame whenever `pct > 0`
    /// (the reference's per-frame LBRR rate decision scales this by the
    /// actual loss rate and activity); `pct <= 0` disables LBRR.
    pub fn set_packet_loss_perc(&mut self, pct: i32) {
        self.lbrr_enabled = pct > 0;
    }

    /// Hybrid-mode variant: the written symbols must fit `bytes` bytes
    /// of the shared range coder (with headroom for the hybrid
    /// redundancy-bit lookahead); oversized payloads are re-encoded at a
    /// progressively reduced rate, and nothing is padded (the CELT layer
    /// continues on the same coder).
    pub fn set_max_payload_bytes(&mut self, bytes: usize) {
        self.cbr = Some(CbrMode::MaxBytes(bytes));
    }

    /// Current SNR target in dB·128 (exposed for tests).
    pub fn snr_db_q7(&self) -> i32 {
        self.ch[0].snr_db_q7
    }

    /// The number of API-rate samples one *SILK frame* consumes per
    /// channel.
    pub fn frame_length_api(&self) -> usize {
        self.ch[0].frame_length * self.api_sample_rate as usize
            / (self.ch[0].fs_khz as usize * 1000)
    }

    /// The number of API-rate samples one payload (packet) consumes per
    /// channel — [`Self::frame_length_api`] × [`Self::frames_per_packet`].
    /// [`Self::encode_frame_into`] takes interleaved samples across all
    /// internal channels (× 1 mono, × 2 stereo).
    pub fn packet_length_api(&self) -> usize {
        self.frame_length_api() * self.packet_frames
    }

    /// SILK frames per payload (1, 2, or 3).
    pub fn frames_per_packet(&self) -> usize {
        self.packet_frames
    }

    /// Payload duration in ms (10/20/40/60).
    pub fn packet_ms(&self) -> i32 {
        self.packet_ms
    }

    /// Internal channel count (1 mono, 2 mid/side).
    pub fn channels_internal(&self) -> usize {
        self.channels_internal
    }

    /// The decoder-exact reconstruction of the most recently encoded
    /// payload (the output a conforming decoder produces from the last
    /// payload, at the internal sample rate, concatenated across the
    /// packet's SILK frames — and, for stereo, the mid channel followed
    /// by the coded side frames). Because the closed-loop NSQ runs on the
    /// decoder's own arithmetic, this is bit-identical to the decoder's
    /// output — the property the round-trip tests pin.
    pub fn last_reconstructed_frame(&self) -> &[i16] {
        &self.ch[0].last_xq
    }

    /// The side channel's reconstruction (stereo only; empty when the
    /// last packet's side frames were all skipped).
    pub fn last_reconstructed_side(&self) -> &[i16] {
        &self.ch[1].last_xq
    }

    /// Encodes one payload; `input` must be [`Self::packet_length_api`]
    /// interleaved... (mono) API-rate samples. Returns the SILK payload
    /// bytes (one range-coded packet covering [`Self::frames_per_packet`]
    /// SILK frames, VBR), ready to drop into a single-frame code-0 Opus
    /// packet for the matching TOC config.
    pub fn encode_frame(&mut self, input: &[i16]) -> Result<Vec<u8>> {
        let mut enc = crate::range::RangeEncoder::new();
        self.encode_frame_into(input, &mut enc)?;
        let mut payload = enc.done();
        if let Some(CbrMode::ExactBytes(n)) = self.cbr {
            payload.resize(n, 0);
        }
        Ok(payload)
    }

    /// Writes the same symbols onto a caller-provided range encoder (so a
    /// CELT layer can continue on the same coder, as hybrid packets
    /// require) without finalizing — the caller serializes the combined
    /// payload. `input` holds interleaved API-rate samples covering
    /// [`Self::packet_length_api`] samples per internal channel (× 2 for
    /// stereo). The payload is one range-coded packet covering
    /// [`Self::frames_per_packet`] SILK frames, ready to drop into a
    /// single-frame code-0 Opus packet for the matching TOC config.
    pub fn encode_frame_into(
        &mut self,
        input: &[i16],
        enc: &mut crate::range::RangeEncoder,
    ) -> Result<()> {
        let channels = self.channels_internal;
        let api_len = self.packet_length_api() * channels;
        if input.len() != api_len {
            return Err(CadenceError::CorruptData(format!(
                "expected {api_len} interleaved input samples, got {}",
                input.len()
            )));
        }
        let frame_len_api = self.frame_length_api();
        let frame_length = self.ch[0].frame_length;
        let stereo = channels == 2;
        /* The reference's CBR flag: under CBR the gain path skips the
         * low-activity SNR reduction (the sizing loop below pulls the
         * payload back with its own rate control instead). */
        let use_cbr = self.cbr.is_some();

        /* CBR sizing loop: encode, then (only when a sizing mode is
         * active) check the fit — an oversized payload restores the
         * pre-attempt snapshot, resets the encoder, and re-encodes at a
         * reduced working rate. Without a sizing mode the single pass
         * below runs exactly once, as before. */
        let mut working_rate = self.target_rate_bps;
        let mut attempts = 0;
        loop {
            let snap = self.snapshot_state();

            /*--------------------------------------------------------*/
            /* Pass A1: resample, run the stereo MS transform, and    */
            /* decide DTX per frame (reference `noSpeechCounter` /    */
            /* `inDTX` schedule, on the mid channel's activity). The  */
            /* input pipeline keeps running through DTX frames —     */
            /* only their coding is skipped.                          */
            /*--------------------------------------------------------*/
            for ch in self.ch.iter_mut().take(channels) {
                ch.vad_flags = [false; MAX_FRAMES_PER_PACKET];
                ch.last_xq.clear();
            }
            self.mid_only = [false; MAX_FRAMES_PER_PACKET];
            let mut mid_buf = [0i16; MAX_FRAME_LENGTH + 2];
            let mut side_buf = [0i16; MAX_FRAME_LENGTH + 2];
            let mut mid_frames: [Vec<i16>; MAX_FRAMES_PER_PACKET] =
                std::array::from_fn(|_| vec![0i16; frame_length]);
            let mut side_frames: [Vec<i16>; MAX_FRAMES_PER_PACKET] =
                std::array::from_fn(|_| vec![0i16; frame_length]);
            let mut frame_in_dtx = [false; MAX_FRAMES_PER_PACKET];
            let mut frame_sa = [0i32; MAX_FRAMES_PER_PACKET];
            let mut frame_bands = [[32768i32; 4]; MAX_FRAMES_PER_PACKET];
            let mut prev_mid_only = self.prev_decode_only_middle;
            let prev_mid_only_at_start = self.prev_decode_only_middle;
            for i in 0..self.packet_frames {
                let frame =
                    &input[i * frame_len_api * channels..(i + 1) * frame_len_api * channels];

                /* Resample each channel to the internal rate */
                let mut l_int = vec![0i16; frame_length];
                let deinterleave = |ch: usize| -> Vec<i16> {
                    frame[ch..].iter().step_by(channels).copied().collect()
                };
                self.ch[0]
                    .resampler
                    .resample(&mut l_int, &deinterleave(0))?;
                let mut r_int = Vec::new();
                if stereo {
                    r_int = vec![0i16; frame_length];
                    self.ch[1]
                        .resampler
                        .resample(&mut r_int, &deinterleave(1))?;
                }

                /* Stereo: convert Left/Right to adaptive Mid/Side with the
                 * quantized predictor, and decide the mid-only flag for this
                 * frame (the side is skipped when its residual is inactive —
                 * which is also exactly when the decoder reads the flag). */
                if stereo {
                    mid_buf[..2].copy_from_slice(&self.stereo.s_mid);
                    side_buf[..2].copy_from_slice(&self.stereo.s_side);
                    let (ix, _pred_q13, mid_e, side_e) = lr_to_ms(
                        &mut self.stereo,
                        &l_int,
                        &r_int,
                        &mut mid_buf,
                        &mut side_buf,
                        self.ch[0].fs_khz,
                        frame_length,
                    );
                    self.pred_ix[i] = ix;
                    self.mid_only[i] = (side_e.sqrt() as f32) < INACTIVE_RMS_THRESHOLD;
                    let _ = mid_e;
                    mid_frames[i].copy_from_slice(&mid_buf[2..frame_length + 2]);
                    side_frames[i].copy_from_slice(&side_buf[2..frame_length + 2]);
                } else {
                    mid_frames[i].copy_from_slice(&l_int);
                }

                /* DTX schedule (`silk_Encode`): the mid channel's activity
                 * drives the counter; the first `NB_SPEECH_FRAMES_BEFORE_DTX`
                 * inactive frames are still coded, and past
                 * `MAX_CONSECUTIVE_DTX` the packet is skipped (the counter
                 * recycling to the before-DTX mark). */
                let vad_out =
                    self.ch[0]
                        .vad
                        .vad_get_sa_q8(&mid_frames[i], frame_length, self.ch[0].fs_khz);
                frame_sa[i] = vad_out.speech_activity_q8;
                frame_bands[i] = vad_out.input_quality_bands_q15;
                let mid_rms = {
                    let sum: f64 = mid_frames[i].iter().map(|&v| (v as f64) * v as f64).sum();
                    (sum / frame_length as f64).sqrt() as f32
                };
                if self.dtx_enabled && mid_rms < INACTIVE_RMS_THRESHOLD {
                    self.no_speech_counter = self.no_speech_counter.saturating_add(1);
                    if self.no_speech_counter <= NB_SPEECH_FRAMES_BEFORE_DTX {
                        self.in_dtx = false;
                    }
                    if self.no_speech_counter > MAX_CONSECUTIVE_DTX {
                        self.no_speech_counter = NB_SPEECH_FRAMES_BEFORE_DTX;
                        self.in_dtx = true;
                    }
                } else {
                    self.no_speech_counter = 0;
                    self.in_dtx = false;
                }
                frame_in_dtx[i] = self.in_dtx;
                let mid_only_i = stereo && self.mid_only[i];
                self.ch[1].vad_flags[i] = !mid_only_i;
            }

            /* A packet is skipped only when EVERY frame is in DTX (the
             * decoder reads per-frame side info for any packet that
             * carries a payload, so partial-DTX packets are coded in
             * full). The caller emits a 1-byte packet (TOC only), which
             * the decoder decodes as comfort-noise generation. */
            if self.dtx_enabled && frame_in_dtx[..self.packet_frames].iter().all(|&d| d) {
                // A DTX-skipped packet carries no LBRR either, and its
                // predecessor's redundant frames are stale after the gap.
                self.prev_lbrr = None;
                return Ok(());
            }

            /*--------------------------------------------------------*/
            /* Pass A2: per-frame analysis, quantization, closed-loop */
            /* NSQ (in order — the simulation state carries across    */
            /* frames and channels), buffering each frame's indices   */
            /* and pulses.                                            */
            /*--------------------------------------------------------*/
            let mut plans = [Vec::new(), Vec::new()];
            for i in 0..self.packet_frames {
                let l_int = &mid_frames[i];
                let mid_only_i = stereo && self.mid_only[i];

                /* Mid channel: slide the history window, append the frame. */
                {
                    let ch = &mut self.ch[0];
                    let total = ch.ltp_mem_length + ch.frame_length;
                    ch.x_buf.copy_within(ch.frame_length..total, 0);
                    let tail = &mut ch.x_buf[ch.ltp_mem_length..total];
                    for (dst, &src) in tail.iter_mut().zip(l_int.iter()) {
                        *dst = src as f32;
                    }
                    /* The shaping analysis reads `la_shape` samples of
                     * look-ahead past the frame (zero-filled; see the
                     * noise_shape module). */
                    for v in ch.x_buf[total..].iter_mut() {
                        *v = 0.0;
                    }
                    let seed = (self.frame_counter & 3) as i8;
                    self.frame_counter += 1;
                    let plan = ch.analyze_and_quantize_frame(
                        l_int,
                        i,
                        frame_sa[i],
                        &frame_bands[i],
                        i > 0,
                        seed,
                        use_cbr,
                    );
                    plans[0].push(plan);
                }

                /* Side channel (skipped entirely on mid-only frames — the
                 * decoder mirrors this by zeroing the side and leaving its
                 * state untouched) */
                if stereo && !mid_only_i {
                    if prev_mid_only {
                        self.ch[1].reset_after_mid_only();
                    }
                    let conditional_side = !(i == 0 || i == 1 || prev_mid_only);
                    {
                        let ch = &mut self.ch[1];
                        let total = ch.ltp_mem_length + ch.frame_length;
                        ch.x_buf.copy_within(ch.frame_length..total, 0);
                        let tail = &mut ch.x_buf[ch.ltp_mem_length..total];
                        for (dst, &src) in
                            tail.iter_mut().zip(side_frames[i][..frame_length].iter())
                        {
                            *dst = src as f32;
                        }
                        for v in ch.x_buf[total..].iter_mut() {
                            *v = 0.0;
                        }
                        let seed = (self.frame_counter & 3) as i8;
                        self.frame_counter += 1;
                        let plan = ch.analyze_and_quantize_frame(
                            &side_frames[i],
                            i,
                            frame_sa[i],
                            &frame_bands[i],
                            conditional_side,
                            seed,
                            use_cbr,
                        );
                        plans[1].push(plan);
                    }
                }
                prev_mid_only = mid_only_i;
            }
            self.prev_decode_only_middle = prev_mid_only;

            /*--------------------------------------------------------*/
            /* Pass B: serialize the whole payload — per-channel      */
            /* VAD/LBRR prologues once, then per frame the MS         */
            /* predictor indices, the mid-only flag (present exactly  */
            /* when the side VAD flag makes the decoder read it), and */
            /* each coded channel's side info + excitation — exactly  */
            /* as the decoder reads it.                               */
            /*--------------------------------------------------------*/
            // LBRR: when enabled and the previous packet stored frames,
            // the per-channel packet-level LBRR flags are set, the
            // per-frame LBRR flags follow (mid always coded; the side's
            // flag is its coded/mid-only decision), and the previous
            // frames are re-serialized frame-major / channel-minor — the
            // decoder's normal-decode path reads (and discards) exactly
            // this data, so a serialization mismatch desyncs loudly.
            let have_lbrr = self.lbrr_enabled && self.prev_lbrr.is_some();
            encode_vad_flags_and_lbrr_flag(
                enc,
                &self.ch[0].vad_flags,
                self.packet_frames,
                have_lbrr,
            );
            if stereo {
                encode_vad_flags_and_lbrr_flag(
                    enc,
                    &self.ch[1].vad_flags,
                    self.packet_frames,
                    have_lbrr,
                );
            }
            if have_lbrr {
                let prev = self.prev_lbrr.as_ref().unwrap();
                let lbrr0 = prev.mid_stored;
                let lbrr1: [bool; MAX_FRAMES_PER_PACKET] =
                    std::array::from_fn(|i| prev.mid_stored[i] && !prev.side_coded[i]);
                encode_lbrr_flags(enc, &lbrr0, self.packet_frames);
                if stereo {
                    encode_lbrr_flags(enc, &lbrr1, self.packet_frames);
                }
                for i in 0..self.packet_frames {
                    for n in 0..channels {
                        let coded = if n == 0 { true } else { !prev.side_coded[i] };
                        if !coded {
                            continue;
                        }
                        if stereo && n == 0 {
                            encode_stereo_pred(enc, &prev.pred_ix[i]);
                            if !prev.side_coded[i] {
                                encode_mid_only_flag(enc, true);
                            }
                        }
                        let cond_coding = if i > 0 && lbrr_flags_at(&lbrr0, &lbrr1, n, i - 1) {
                            CondCoding::Conditionally
                        } else {
                            CondCoding::Independently
                        };
                        let delta_possible = cond_coding == CondCoding::Conditionally
                            && self.ch[n].ec_prev.ec_prev_signal_type == TYPE_VOICED;
                        let params = FrameParams {
                            nlsf_cb: self.nlsf_cb,
                            fs_khz: self.ch[n].fs_khz,
                            nb_subfr: self.ch[n].nb_subfr,
                            frame_index: i,
                            vad_flag: true,
                            decode_lbrr: true,
                            cond_coding,
                        };
                        encode_indices(
                            enc,
                            &prev.indices[n][i],
                            &mut self.ch[n].ec_prev,
                            &params,
                            delta_possible,
                        );
                        encode_pulses(
                            enc,
                            prev.indices[n][i].signal_type as i32,
                            prev.indices[n][i].quant_offset_type as i32,
                            &prev.pulses[n][i],
                            self.ch[n].frame_length,
                        );
                    }
                }
            }
            let mut side_plan_iter = plans[1].iter();
            for (i, plan0) in plans[0].iter().enumerate() {
                if stereo {
                    encode_stereo_pred(enc, &self.pred_ix[i]);
                    if self.mid_only[i] {
                        encode_mid_only_flag(enc, true);
                    }
                }

                /* Mid channel: decoder cond rule with frame_index = i. */
                {
                    let cond_coding = if i == 0 {
                        CondCoding::Independently
                    } else {
                        CondCoding::Conditionally
                    };
                    let delta_possible = cond_coding == CondCoding::Conditionally
                        && self.ch[0].ec_prev.ec_prev_signal_type == TYPE_VOICED;
                    let params = FrameParams {
                        nlsf_cb: self.nlsf_cb,
                        fs_khz: self.ch[0].fs_khz,
                        nb_subfr: self.ch[0].nb_subfr,
                        frame_index: i,
                        vad_flag: plan0.vad_flag,
                        decode_lbrr: false,
                        cond_coding,
                    };
                    encode_indices(
                        enc,
                        &plan0.indices,
                        &mut self.ch[0].ec_prev,
                        &params,
                        delta_possible,
                    );
                    encode_pulses(
                        enc,
                        plan0.indices.signal_type as i32,
                        plan0.indices.quant_offset_type as i32,
                        &plan0.pulses,
                        self.ch[0].frame_length,
                    );
                }

                /* Side channel: the decoder's frame index for channel 1 is
                 * offset by one (n_frames_decoded - n with n == 1), so frames
                 * 0 and 1 are independent; a frame following a skipped one
                 * drops the LTP scale (NO_LTP_SCALING). */
                if stereo && !self.mid_only[i] {
                    let plan1 = side_plan_iter.next().expect("side plan for coded frame");
                    let prev_skipped = if i > 0 {
                        self.mid_only[i - 1]
                    } else {
                        prev_mid_only_at_start
                    };
                    let cond_coding = if i == 0 || i == 1 {
                        CondCoding::Independently
                    } else if prev_skipped {
                        CondCoding::IndependentlyNoLtpScaling
                    } else {
                        CondCoding::Conditionally
                    };
                    let delta_possible = cond_coding == CondCoding::Conditionally
                        && self.ch[1].ec_prev.ec_prev_signal_type == TYPE_VOICED;
                    let params = FrameParams {
                        nlsf_cb: self.nlsf_cb,
                        fs_khz: self.ch[1].fs_khz,
                        nb_subfr: self.ch[1].nb_subfr,
                        frame_index: i,
                        vad_flag: plan1.vad_flag,
                        decode_lbrr: false,
                        cond_coding,
                    };
                    encode_indices(
                        enc,
                        &plan1.indices,
                        &mut self.ch[1].ec_prev,
                        &params,
                        delta_possible,
                    );
                    encode_pulses(
                        enc,
                        plan1.indices.signal_type as i32,
                        plan1.indices.quant_offset_type as i32,
                        &plan1.pulses,
                        self.ch[1].frame_length,
                    );
                }
            }
            /* Store this packet's ACTIVE coded frames as the next
             * packet's LBRR payload (the reference's LBRR covers active
             * frames only). */
            let mut stored = Box::new(LbrrStored {
                indices: [[SideInfoIndices::default(); MAX_FRAMES_PER_PACKET]; 2],
                pulses: [[[0i16; MAX_FRAME_LENGTH]; MAX_FRAMES_PER_PACKET]; 2],
                pred_ix: self.pred_ix,
                side_coded: self.mid_only,
                mid_stored: [false; MAX_FRAMES_PER_PACKET],
            });
            for (n, _ch) in self.ch.iter().take(channels).enumerate() {
                for (i, plan) in plans[n].iter().enumerate() {
                    if plan.indices.signal_type == TYPE_NO_VOICE_ACTIVITY {
                        continue;
                    }
                    stored.indices[n][i] = plan.indices;
                    stored.pulses[n][i] = plan.pulses;
                    if n == 0 {
                        stored.mid_stored[i] = true;
                    }
                }
            }
            if stored.mid_stored[..self.packet_frames].iter().any(|&s| s) {
                self.prev_lbrr = Some(stored);
            } else {
                self.prev_lbrr = None;
            }

            let fits = match self.cbr {
                None => true,
                Some(CbrMode::ExactBytes(n)) => enc.clone().done().len() <= n,
                Some(CbrMode::MaxBytes(n)) => i64::from(enc.tell()) + 37 <= (8 * n) as i64,
            };
            if fits {
                return Ok(());
            }
            attempts += 1;
            if attempts > CBR_MAX_ATTEMPTS {
                match self.cbr {
                    // Exact-size payloads have no fallback: the caller
                    // asked for a constant size and the minimum-rate
                    // encoding still does not fit, which is a hard
                    // (content-dependent) failure.
                    Some(CbrMode::ExactBytes(n)) => {
                        return Err(CadenceError::InvalidFormat(format!(
                            "SILK CBR sizing: payload does not fit {n} bytes even at the minimum rate"
                        )));
                    }
                    // Best-effort mode: keep the last attempt (the rate
                    // was reduced as far as the attempts allow) and let
                    // the caller's own hard backstop — for the hybrid,
                    // the redundancy-lookahead guard — have the final
                    // word. The foundation's quantizer has a
                    // content-dependent minimum payload (measured ~45 B
                    // for active speech at 16 kHz internal) that no rate
                    // reduction can go below.
                    Some(CbrMode::MaxBytes(_)) | None => return Ok(()),
                }
            }
            // Restore the pre-attempt state so the retry encodes from
            // identical conditions, start the shared/payload coder over,
            // and step the working rate proportionally to the measured
            // overshoot (a fixed multiplicative cut degrades quality far
            // more than needed when the payload is only slightly over).
            let actual_bytes = match self.cbr {
                Some(CbrMode::ExactBytes(_)) => enc.clone().done().len().max(1) as f64,
                // MaxBytes works in bits with a 37-bit headroom; convert
                // to the equivalent byte count.
                Some(CbrMode::MaxBytes(n)) => (((i64::from(enc.tell()) + 37) as f64) / 8.0)
                    .max(1.0)
                    .min(n as f64),
                None => 1.0,
            };
            let target_bytes = match self.cbr {
                Some(CbrMode::ExactBytes(n)) | Some(CbrMode::MaxBytes(n)) => n as f64,
                None => 1.0,
            };
            self.restore_state(&snap);
            *enc = crate::range::RangeEncoder::new();
            let ratio = (target_bytes / actual_bytes).clamp(0.25, 1.0);
            let reduced = ((working_rate as f64 * ratio) as i32).max(1);
            working_rate = reduced.min(working_rate.saturating_sub(1)).max(1);
            self.set_working_rate(working_rate);
        }
    }
}

/// The per-frame analysis chain: every step below reads only the
/// channel's own geometry, buffers, and decoder-mirror state.
impl ChannelState {
    /// The per-frame analysis chain: runs the full
    /// `encode_frame_FLP`-shaped pipeline on the current `x_buf`
    /// contents, advances every piece of decoder-mirror state, and
    /// returns the frame's serializable parameters. `frame_index` is the
    /// frame's position within its packet (0-based); `conditional_gains`
    /// selects delta coding for subframe 0's gain (the caller derives it
    /// from the decoder's per-channel conditional-coding rule); `seed`
    /// is the frame's deterministic excitation dither seed, and `use_cbr`
    /// mirrors the reference's CBR flag (see [`Self::noise_shape`]).
    #[allow(clippy::too_many_arguments)]
    fn analyze_and_quantize_frame(
        &mut self,
        x_int: &[i16],
        frame_index: usize,
        sa_q8: i32,
        input_quality_bands_q15: &[i32; 4],
        conditional_gains: bool,
        seed: i8,
        use_cbr: bool,
    ) -> FramePlan {
        /*--------------------------------------------------------*/
        /* Side-info skeleton (type, VAD flag, seed). `indices`   */
        /* starts from the persistent mirror so uncoded fields    */
        /* carry over as they do on the decode side.              */
        /*--------------------------------------------------------*/
        let mut indices = self.indices;
        indices.seed = seed;
        /* signalType gate: a documented foundation deviation — the
         * reference downgrades to TYPE_NO_VOICE_ACTIVITY when its VAD's
         * `speech_activity_Q8` falls below 0.2, but that classifies
         * synthetic test signals (harmonic stacks) as silence. The RMS
         * gate keeps every frame coded while remaining inert for genuine
         * silence; the VAD's activity/quality outputs still drive the
         * shaping analysis. */
        let frame_rms = {
            let sum: f64 = x_int.iter().map(|&v| (v as f64) * v as f64).sum();
            (sum / self.frame_length as f64).sqrt() as f32
        };
        indices.signal_type = if frame_rms < INACTIVE_RMS_THRESHOLD {
            TYPE_NO_VOICE_ACTIVITY
        } else {
            TYPE_UNVOICED
        };
        let vad_flag = indices.signal_type != TYPE_NO_VOICE_ACTIVITY;
        self.vad_flags[frame_index] = vad_flag;
        let _ = sa_q8;
        self.speech_activity_q8 = sa_q8;
        self.input_quality_bands_q15 = *input_quality_bands_q15;

        /* `speech_activity_Q8` from the reference 4-band VAD
         * (`silk_VAD_GetSA_Q8`), evaluated on the frame input. */
        let vad_out = self
            .vad
            .vad_get_sa_q8(x_int, self.frame_length, self.fs_khz);
        self.speech_activity_q8 = vad_out.speech_activity_q8;
        self.input_quality_bands_q15 = vad_out.input_quality_bands_q15;

        /*--------------------------------------------------------*/
        /* Pitch analysis (find_pitch_lags shape)                 */
        /*--------------------------------------------------------*/
        let (res_pitch, pred_gain) = self.pitch_residual();
        let mut pitch_l = [0i32; MAX_NB_SUBFR];
        let mut ltp_corr = 0.0f32;

        if indices.signal_type != TYPE_NO_VOICE_ACTIVITY {
            let (lags, corr, lag_index, contour_index) = self.pitch_search(&res_pitch);
            ltp_corr = corr;
            /* Voiced threshold (find_pitch_lags_FLP with the foundation's
             * neutral speech-activity/tilt terms) */
            let thrhld = 0.6f32
                - 0.004 * self.pitch_lpc_order as f32
                - 0.15 * i32::from(self.prev_signal_type >> 1) as f32;
            if ltp_corr >= thrhld {
                indices.signal_type = TYPE_VOICED;
                pitch_l[..self.nb_subfr].copy_from_slice(&lags[..self.nb_subfr]);
                indices.lag_index = lag_index;
                indices.contour_index = contour_index;
            }
        }

        /* Quantizer offset: voiced starts at 0 (process_gains may keep
         * it); otherwise the sparseness rule of
         * noise_shape_analysis_FLP decides 0 vs 1. */
        if indices.signal_type == TYPE_VOICED {
            indices.quant_offset_type = 0;
        } else {
            indices.quant_offset_type = self.sparseness_quant_offset_type(&res_pitch);
        }

        /*--------------------------------------------------------*/
        /* Noise shaping analysis + per-subframe gains             */
        /*--------------------------------------------------------*/
        let mut gains = [0f32; MAX_NB_SUBFR];
        let input_quality_bands_q15 = self.input_quality_bands_q15;
        let shape = self.noise_shape(
            &mut gains,
            indices.signal_type,
            indices.quant_offset_type,
            &pitch_l,
            ltp_corr,
            pred_gain,
            &input_quality_bands_q15,
            use_cbr,
        );

        /*--------------------------------------------------------*/
        /* LPC + LTP (find_pred_coefs shape)                      */
        /*--------------------------------------------------------*/
        let mut ctrl = DecoderControl {
            pitch_l,
            ..DecoderControl::default()
        };
        let mut ltpred_cod_gain_db = 0.0f32;

        if indices.signal_type == TYPE_VOICED {
            /* LTP analysis on the pitch residual */
            let mut xx = vec![0f32; self.nb_subfr * 25];
            let mut x_x = vec![0f32; self.nb_subfr * 5];
            find_ltp(
                &mut xx,
                &mut x_x,
                &res_pitch,
                self.ltp_mem_length,
                &pitch_l,
                self.subfr_length,
                self.nb_subfr,
            );
            let (xx_q17, x_x_q17) = correlations_to_q17(&xx, &x_x);
            let ltp = quant_ltp_gains(
                &xx_q17,
                &x_x_q17,
                self.subfr_length as i32,
                self.nb_subfr,
                self.sum_log_gain_q7,
            );
            self.sum_log_gain_q7 = ltp.sum_log_gain_q7;
            ctrl.ltp_coef_q14 = ltp.b_q14;
            indices.ltp_index = ltp.cbk_index;
            indices.per_index = ltp.periodicity_index;
            /* LTP scale control with 0% packet loss and LBRR off: the
             * reference's comparisons are both false -> index 0. The
             * symbol is only *coded* for independently coded frames;
             * conditionally coded frames keep the previous frame's index
             * (persistent `indices`), exactly like the decoder. */
            indices.ltp_scale_index = 0;
            ltpred_cod_gain_db = ltp.pred_gain_d_b_q7 as f32 * (1.0 / 128.0);
        } else {
            ctrl.ltp_coef_q14[..5 * self.nb_subfr].fill(0);
            ctrl.ltp_scale_q14 = 0;
            self.sum_log_gain_q7 = 0;
        }
        if indices.signal_type == TYPE_VOICED {
            ctrl.ltp_scale_q14 =
                crate::silk::tables::LTPSCALES_TABLE_Q14[indices.ltp_scale_index as usize];
        }

        /* Weighted LPC analysis on the (LTP-)residual */
        /* `silk_find_pred_coefs_FLP` computes minInvGain from the LTP
         * prediction gain and the coding quality (both frame-local, so
         * they are passed in); the first frame after a reset uses the
         * post-reset cap. */
        let ltpred_cod_gain = ltpred_cod_gain_db;
        let coding_quality = shape.coding_quality;
        let first_frame_after_reset = self.first_frame_after_reset;
        let (nlsf_q15, nlsf_interp_coef, _nlsf0_q15, lpc_in_pre) = self.lpc_analysis_to_nlsf(
            &pitch_l,
            &ctrl.ltp_coef_q14,
            &gains,
            indices.signal_type,
            first_frame_after_reset,
            ltpred_cod_gain,
            coding_quality,
        );

        /* NLSF quantization + conversion to the decoder's Q12 filters.
         * When the interpolation search picked a coefficient, the
         * first-half filter comes from the interpolated NLSF (previous
         * quantized → current quantized), exactly as the decoder
         * reconstructs it. */
        let mut weights = [0i16; MAX_LPC_ORDER];
        nlsf_vq_weights_laroia(&mut weights, &nlsf_q15);
        let mut nlsf_mu_q20: i32 = 3146; // SILK_FIX_CONST(0.003, 20), speech activity 0
        if self.nb_subfr == 2 {
            nlsf_mu_q20 = nlsf_mu_q20.wrapping_add(nlsf_mu_q20 >> 1);
        }
        let mut nlsf_indices = [0i8; MAX_LPC_ORDER + 1];
        let mut quantized = nlsf_q15;
        nlsf_encode(
            &mut nlsf_indices,
            &mut quantized,
            crate::silk::encoder::nlsf_cb_for(self.fs_khz),
            &weights,
            nlsf_mu_q20,
            16,
            indices.signal_type,
        );
        indices.nlsf_indices[0] = nlsf_indices[0];
        indices.nlsf_indices[1..=self.predict_lpc_order]
            .copy_from_slice(&nlsf_indices[1..=self.predict_lpc_order]);
        indices.nlsf_interp_coef_q2 = nlsf_interp_coef;
        nlsf2a(
            &mut ctrl.pred_coef_q12[1][..self.predict_lpc_order],
            &quantized[..self.predict_lpc_order],
            self.predict_lpc_order,
        );
        if nlsf_interp_coef < 4 {
            let mut nlsf0_q15 = [0i16; MAX_LPC_ORDER];
            interpolate_i16(
                &mut nlsf0_q15[..self.predict_lpc_order],
                &self.prev_nlsf_q15,
                &quantized[..self.predict_lpc_order],
                i32::from(nlsf_interp_coef),
                self.predict_lpc_order,
            );
            nlsf2a(
                &mut ctrl.pred_coef_q12[0][..self.predict_lpc_order],
                &nlsf0_q15[..self.predict_lpc_order],
                self.predict_lpc_order,
            );
        } else {
            ctrl.pred_coef_q12[0] = ctrl.pred_coef_q12[1];
        }
        self.prev_nlsf_q15 = quantized;

        /* Residual energies with the quantized filters
         * (residual_energy_FLP shape, gains included) */
        let res_nrg = self.residual_energies(&lpc_in_pre, &ctrl.pred_coef_q12, &gains);

        /*--------------------------------------------------------*/
        /* process_gains shape                                    */
        /*--------------------------------------------------------*/
        if indices.signal_type == TYPE_VOICED {
            let s = 1.0 - 0.5 * sigmoid(0.25 * (ltpred_cod_gain_db - 12.0));
            for g in gains.iter_mut().take(self.nb_subfr) {
                *g *= s;
            }
        }
        let inv_max_sqr_val = (2.0f32.powf(0.33 * (21.0 - self.snr_db_q7 as f32 * (1.0 / 128.0))))
            / self.subfr_length as f32;
        for k in 0..self.nb_subfr {
            let g = gains[k];
            let limited = (g * g + res_nrg[k] * inv_max_sqr_val).sqrt();
            gains[k] = limited.min(32767.0);
        }
        let mut gains_q16 = [0i32; MAX_NB_SUBFR];
        for k in 0..self.nb_subfr {
            gains_q16[k] = (gains[k] * 65536.0) as i32;
        }
        /* Frames 1.. of a multi-frame packet are conditionally coded:
         * subframe 0's gain is quantized as a delta against the persistent
         * `LastGainIndex`, mirroring the decoder's `gains_dequant`. */
        gains_quant(
            &mut indices.gains_indices,
            &mut gains_q16,
            &mut self.last_gain_index,
            conditional_gains,
            self.nb_subfr,
        );
        ctrl.gains_q16 = gains_q16;
        if indices.signal_type == TYPE_VOICED {
            indices.quant_offset_type = if ltpred_cod_gain_db > 1.0 { 0 } else { 1 };
        }

        /*--------------------------------------------------------*/
        /* Excitation (closed-loop NSQ) + state updates           */
        /*--------------------------------------------------------*/
        let mut pulses = [0i16; MAX_FRAME_LENGTH];
        let mut xq = [0i16; MAX_FRAME_LENGTH];
        encode_frame_nsq(
            &mut self.synth,
            &shape,
            &mut self.exc_q14,
            &mut pulses,
            &mut xq,
            x_int,
            &ctrl,
            &indices,
            &self.frame,
            0,
            self.prev_signal_type,
            self.lag_prev,
        );
        self.synth
            .update_out_buf(&xq[..self.frame_length], self.ltp_mem_length);
        self.last_xq.extend_from_slice(&xq[..self.frame_length]);
        self.lag_prev = ctrl.pitch_l[self.nb_subfr - 1];
        self.prev_signal_type = indices.signal_type;
        self.first_frame_after_reset = false;
        self.indices = indices;

        FramePlan {
            indices,
            pulses,
            xq: xq[..self.frame_length].to_vec(),
            vad_flag,
        }
    }

    /// The pitch-LPC residual over the whole buffered window (history +
    /// frame), foundation shape of `silk_find_pitch_lags_FLP`'s
    /// sine-windowed order-6 analysis, together with the analysis'
    /// `predGain` (`auto_corr[0] / max(res_nrg, 1)`) — the noise-shaping
    /// analysis scales its bandwidth expansion by it.
    fn pitch_residual(&self) -> (Vec<f32>, f32) {
        let total = self.ltp_mem_length + self.frame_length;
        /* Only the history + frame region; the trailing noise-shaping
         * look-ahead is not part of the pitch window. */
        let buf = &self.x_buf[..total];
        let mut wsig = vec![0f32; total];
        let slope = (self.fs_khz as usize).min(total / 4);
        apply_sine_window(&mut wsig[..slope], &buf[..slope], 1);
        let flat = total - 2 * slope;
        wsig[slope..slope + flat].copy_from_slice(&buf[slope..slope + flat]);
        apply_sine_window(&mut wsig[total - slope..], &buf[total - slope..], 2);

        let mut auto_corr = [0f32; 7];
        autocorrelation(&mut auto_corr, &wsig);
        auto_corr[0] += auto_corr[0] * FIND_PITCH_WHITE_NOISE_FRACTION + 1.0;
        let mut rc = [0f32; 6];
        let res_nrg = schur(&mut rc, &auto_corr, self.pitch_lpc_order);
        let pred_gain = auto_corr[0] / res_nrg.max(1.0);
        let mut a = [0f32; 6];
        k2a(&mut a, &rc, self.pitch_lpc_order);
        bwexpander_f32(
            &mut a[..self.pitch_lpc_order],
            FIND_PITCH_BANDWIDTH_EXPANSION,
        );

        let mut res = vec![0f32; total];
        lpc_analysis_filter(&mut res, &a, buf, self.pitch_lpc_order);
        (res, pred_gain)
    }

    /// Full-resolution normalized cross-correlation pitch search over the
    /// 2–18 ms range, per subframe, followed by the contour-codebook
    /// quantization. Returns the per-subframe lags and the LTP
    /// correlation at the chosen lags.
    #[allow(clippy::type_complexity)]
    fn pitch_search(&self, res_pitch: &[f32]) -> ([i32; MAX_NB_SUBFR], f32, i16, i8) {
        let fs = self.fs_khz as i32;
        let min_lag = (PE_MIN_LAG_MS * fs) as usize;
        let max_lag = (PE_MAX_LAG_MS * fs) as usize;

        let mut lags = [0i32; MAX_NB_SUBFR];
        for (k, lag_out) in lags.iter_mut().enumerate().take(self.nb_subfr) {
            let start = self.ltp_mem_length + k * self.subfr_length;
            let seg = &res_pitch[start..start + self.subfr_length];
            let seg_energy = energy(seg);
            let mut best_lag = min_lag;
            let mut best_corr = -1.0f64;
            for lag in min_lag..=max_lag {
                let delayed = &res_pitch[start - lag..start - lag + self.subfr_length];
                let den = (seg_energy * energy(delayed)).sqrt();
                if den < 1e-9 {
                    continue;
                }
                let corr = res_pitch[start..start + self.subfr_length]
                    .iter()
                    .zip(delayed.iter())
                    .map(|(&a, &b)| (a * b) as f64)
                    .sum::<f64>()
                    / den;
                if corr > best_corr {
                    best_corr = corr;
                    best_lag = lag;
                }
            }
            *lag_out = best_lag as i32;
        }

        /* Primary lag from subframe 0, clamped into the encodable range:
         * `lag_index` spans 0..=16·fs−1 above the 2·fs floor. */
        let primary = lags[0].clamp(PE_MIN_LAG_MS * fs, PE_MIN_LAG_MS * fs + 16 * fs - 1) as usize;
        let lag_index = (primary as i32 - PE_MIN_LAG_MS * fs) as i16;

        /* Contour: minimize the total distance to the per-subframe lags */
        let n_contours = if fs == 8 {
            if self.nb_subfr == MAX_NB_SUBFR {
                CB_LAGS_STAGE2[0].len()
            } else {
                CB_LAGS_STAGE2_10_MS[0].len()
            }
        } else if self.nb_subfr == MAX_NB_SUBFR {
            CB_LAGS_STAGE3[0].len()
        } else {
            CB_LAGS_STAGE3_10_MS[0].len()
        };
        let mut best_contour = 0usize;
        let mut best_err = f64::MAX;
        let mut best_lags = [min_lag as i32; MAX_NB_SUBFR];
        for c in 0..n_contours {
            let mut err = 0.0f64;
            let mut cand = [0i32; MAX_NB_SUBFR];
            for k in 0..self.nb_subfr {
                let offset = if fs == 8 {
                    if self.nb_subfr == MAX_NB_SUBFR {
                        CB_LAGS_STAGE2[k][c]
                    } else {
                        CB_LAGS_STAGE2_10_MS[k][c]
                    }
                } else if self.nb_subfr == MAX_NB_SUBFR {
                    CB_LAGS_STAGE3[k][c]
                } else {
                    CB_LAGS_STAGE3_10_MS[k][c]
                };
                let l =
                    (primary as i32 + offset as i32).clamp(PE_MIN_LAG_MS * fs, PE_MAX_LAG_MS * fs);
                cand[k] = l;
                err += (l - lags[k]) as f64 * (l - lags[k]) as f64;
            }
            if err < best_err {
                best_err = err;
                best_contour = c;
                best_lags = cand;
            }
        }
        let ltp_corr = ltp_correlation(
            res_pitch,
            self.ltp_mem_length,
            &best_lags,
            self.subfr_length,
            self.nb_subfr,
        );
        (best_lags, ltp_corr, lag_index, best_contour as i8)
    }

    /// `noise_shape_analysis_FLP`'s sparseness rule for the quantization
    /// offset of non-voiced frames, on the pitch residual.
    fn sparseness_quant_offset_type(&self, res_pitch: &[f32]) -> i8 {
        let n_samples = 2 * self.fs_khz as usize;
        let n_segs = (5 * self.nb_subfr) / 2;
        let mut energy_variation = 0.0f64;
        let mut log_energy_prev = 0.0f64;
        for k in 0..n_segs {
            let start = self.ltp_mem_length + k * n_samples;
            let nrg = n_samples as f64 + energy(&res_pitch[start..start + n_samples]);
            let log_energy = nrg.log2();
            if k > 0 {
                energy_variation += (log_energy - log_energy_prev).abs();
            }
            log_energy_prev = log_energy;
        }
        if energy_variation > ENERGY_VARIATION_THRESHOLD_QNT_OFFSET as f64 * (n_segs - 1) as f64 {
            0
        } else {
            1
        }
    }

    /// `silk_noise_shape_analysis_FLP`: the per-subframe shaping filters,
    /// tilt, harmonic shaping gain and rate/distortion factor, plus the
    /// per-subframe gains written into `gains` (the reference computes both
    /// in one pass).
    ///
    /// `use_cbr` mirrors the reference's flag: a CBR-targeted encode keeps
    /// its gains (and so its noise floor) up during low speech activity
    /// instead of reducing the coding SNR.
    #[allow(clippy::too_many_arguments)]
    fn noise_shape(
        &mut self,
        gains: &mut [f32; MAX_NB_SUBFR],
        signal_type: i8,
        quant_offset_type: i8,
        pitch_l: &[i32; MAX_NB_SUBFR],
        ltp_corr: f32,
        pred_gain: f32,
        input_quality_bands_q15: &[i32; 4],
        use_cbr: bool,
    ) -> ShapeParams {
        let geo = ShapeGeometry {
            fs_khz: self.fs_khz,
            nb_subfr: self.nb_subfr,
            subfr_length: self.subfr_length,
            frame_length: self.frame_length,
            ltp_mem_length: self.ltp_mem_length,
        };
        noise_shape_analysis(
            &mut self.shape,
            &geo,
            &self.x_buf,
            self.snr_db_q7,
            signal_type,
            quant_offset_type,
            pitch_l,
            ltp_corr,
            pred_gain,
            self.speech_activity_q8,
            input_quality_bands_q15,
            use_cbr,
            gains,
        )
    }

    /// Weighted LPC analysis of the (LTP-)filtered input, converted to
    /// NLSFs — `find_pred_coefs_FLP` + `silk_find_LPC_FLP`'s analysis
    /// chain (autocorrelation/schur/k2a in place of Burg; A2NLSF is the
    /// exact reference port and bandwidth-expands unstable filters
    /// itself).
    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::type_complexity)]
    #[allow(clippy::too_many_arguments)]
    fn lpc_analysis_to_nlsf(
        &self,
        pitch_l: &[i32; MAX_NB_SUBFR],
        ltp_coef_q14: &[i16; 20],
        gains: &[f32; MAX_NB_SUBFR],
        signal_type: i8,
        first_frame_after_reset: bool,
        ltpred_cod_gain: f32,
        coding_quality: f32,
    ) -> ([i16; MAX_LPC_ORDER], i8, [i16; MAX_LPC_ORDER], Vec<f32>) {
        let order = self.predict_lpc_order;
        let frame_start = self.ltp_mem_length;

        /* Build `LPC_in_pre`: per subframe, `order` history samples plus
         * the subframe, weighted by the inverse gains. */
        let mut lpc_in_pre = vec![0f32; self.nb_subfr * (self.subfr_length + order)];
        let mut out_ptr = 0usize;
        for k in 0..self.nb_subfr {
            let inv_gain = 1.0 / gains[k];
            let x_ptr = frame_start + k * self.subfr_length - order;
            if signal_type == TYPE_VOICED {
                let lag = pitch_l[k] as usize;
                /* LTP_analysis_filter_FLP: subtract the 5-tap long-term
                 * prediction, then scale. The reference's `x` pointer
                 * starts at frame_start - order, with the lag taps
                 * reaching back into the history. */
                for i in 0..self.subfr_length + order {
                    let mut v = self.x_buf[x_ptr + i];
                    for j in 0..5 {
                        v -= (ltp_coef_q14[k * 5 + j] as f32 / 16384.0)
                            * self.x_buf[x_ptr + i - lag + 2 - j];
                    }
                    lpc_in_pre[out_ptr + i] = v * inv_gain;
                }
            } else {
                for i in 0..self.subfr_length + order {
                    lpc_in_pre[out_ptr + i] = self.x_buf[x_ptr + i] * inv_gain;
                }
            }
            out_ptr += self.subfr_length + order;
        }

        /* minInvGain (`silk_find_pred_coefs_FLP`): cap the TOTAL
         * prediction gain (LTP + LPC) at MAX_PREDICTION_POWER_GAIN
         * (1e4, ~80 dB), raised when the LTP already predicts well and
         * relaxed by the coding quality; the first frame after a reset
         * uses the post-reset cap. */
        let min_inv_gain = if first_frame_after_reset {
            1.0 / MAX_PREDICTION_POWER_GAIN_AFTER_RESET
        } else {
            (2.0f32.powf(ltpred_cod_gain / 3.0) / MAX_PREDICTION_POWER_GAIN)
                / (0.25 + 0.75 * coding_quality)
        };

        /* Modified Burg LPC analysis over the gain-weighted subframes
         * (`silk_find_LPC_FLP` + `silk_burg_modified_FLP`). */
        let mut a = [0f32; 16];
        let res_full = burg_modified_f32(
            &mut a[..order],
            &lpc_in_pre,
            min_inv_gain,
            order + self.subfr_length,
            self.nb_subfr,
            order,
        );

        /* A (float) → Q16 monic → NLSF (exact A2NLSF port) */
        let mut a_q16 = [0i32; 16];
        for (dst, &v) in a_q16.iter_mut().zip(a.iter()).take(order) {
            *dst = (v * 65536.0) as i32;
        }
        let mut nlsf = [0i16; MAX_LPC_ORDER];
        a2nlsf(&mut nlsf[..order], &mut a_q16[..order]);

        /* NLSF interpolation search (`silk_find_LPC_FLP`'s second half):
         * for 20 ms frames with established prediction state, test
         * interpolating the previous quantized NLSF toward the last
         * half's NLSF and keep the coefficient whose interpolated filter
         * gives the lowest first-half residual energy. */
        let mut nlsf_interp_coef_q2: i8 = 4;
        let mut nlsf0_q15 = [0i16; MAX_LPC_ORDER];
        if self.nb_subfr == MAX_NB_SUBFR && !first_frame_after_reset {
            let block = order + self.subfr_length;
            let mut a_half = [0f32; 16];
            let res_half = burg_modified_f32(
                &mut a_half[..order],
                &lpc_in_pre[2 * block..],
                1e-4,
                block,
                2,
                order,
            );
            let mut a_half_q16 = [0i32; 16];
            for (dst, &v) in a_half_q16.iter_mut().zip(a_half.iter()).take(order) {
                *dst = (v * 65536.0) as i32;
            }
            let mut nlsf_half = [0i16; MAX_LPC_ORDER];
            a2nlsf(&mut nlsf_half[..order], &mut a_half_q16[..order]);

            /* First-half residual energy given the last-half solution. */
            let mut res_nrg = res_full - res_half;
            let mut res_nrg_2nd = f32::MAX;
            let mut lpc_res = vec![0f32; 2 * block];
            for k in (0..4).rev() {
                let mut nlsf0 = [0i16; MAX_LPC_ORDER];
                interpolate_i16(
                    &mut nlsf0[..order],
                    &self.prev_nlsf_q15,
                    &nlsf_half,
                    k,
                    order,
                );
                let mut a_q12 = [0i16; MAX_LPC_ORDER];
                nlsf2a(&mut a_q12[..order], &nlsf0[..order], order);
                let a_f32: Vec<f32> = a_q12[..order].iter().map(|&c| c as f32 / 4096.0).collect();
                lpc_analysis_filter(&mut lpc_res, &a_f32, &lpc_in_pre[..2 * block], order);
                let res_nrg_interp = (energy(&lpc_res[order..order + self.subfr_length])
                    + energy(&lpc_res[order + block..order + block + self.subfr_length]))
                    as f32;

                if res_nrg_interp < res_nrg {
                    /* Interpolation has lower residual energy. */
                    res_nrg = res_nrg_interp;
                    nlsf_interp_coef_q2 = k as i8;
                } else if res_nrg_interp > res_nrg_2nd {
                    /* Residual energies would continue to climb. */
                    break;
                }
                res_nrg_2nd = res_nrg_interp;
            }
        }
        if nlsf_interp_coef_q2 < 4 {
            /* The NSQ and decoder interpolate the PREVIOUS QUANTIZED NLSF
             * toward the transmitted one — mirror that vector exactly. */
            interpolate_i16(
                &mut nlsf0_q15[..order],
                &self.prev_nlsf_q15,
                &nlsf,
                i32::from(nlsf_interp_coef_q2),
                order,
            );
        }
        (nlsf, nlsf_interp_coef_q2, nlsf0_q15, lpc_in_pre)
    }

    /// Per-subframe LPC residual energies with the quantized filters
    /// (`silk_residual_energy_FLP`), *without* the gains weighting (the
    /// gains are applied by the soft limit in `process_gains`).
    fn residual_energies(
        &self,
        lpc_in_pre: &[f32],
        pred_coef_q12: &[[i16; MAX_LPC_ORDER]; 2],
        gains: &[f32; MAX_NB_SUBFR],
    ) -> [f32; MAX_NB_SUBFR] {
        /* The reference filters the *weighted* signal (`LPC_in_pre`, whose
         * layout is [order history + subframe] per subframe) in
         * two-subframe halves and weights the energies with the current
         * shaping gains. */
        let mut nrgs = [0f32; MAX_NB_SUBFR];
        let shift = self.predict_lpc_order + self.subfr_length;
        let mut buf = vec![0f32; 2 * shift];
        for half in 0..(self.nb_subfr / 2) {
            let mut a = [0f32; 16];
            for (dst, &q) in a
                .iter_mut()
                .zip(pred_coef_q12[half].iter())
                .take(self.predict_lpc_order)
            {
                *dst = q as f32 * (1.0 / 4096.0);
            }
            let start = half * 2 * shift;
            lpc_analysis_filter(
                &mut buf,
                &a,
                &lpc_in_pre[start..start + 2 * shift],
                self.predict_lpc_order,
            );
            for k in 0..2 {
                /* Residual positions: after the first subframe's `order`
                 * history samples, then after the next full
                 * [order + subframe] block (reference `shift`). */
                let s = self.predict_lpc_order + k * shift;
                let e = energy(&buf[s..s + self.subfr_length]);
                nrgs[half * 2 + k] = gains[half * 2 + k] * gains[half * 2 + k] * e as f32;
            }
        }
        nrgs
    }
}

/// The serializable result of one analyzed SILK frame — the side-info
/// indices plus the quantized excitation pulses, buffered by pass A of
/// [`SilkEncoder::encode_frame`] so the whole payload can be serialized
/// in order in pass B.
struct FramePlan {
    indices: SideInfoIndices,
    pulses: [i16; MAX_FRAME_LENGTH],
    /// The decoder-exact reconstruction (the reference NSQ's output for
    /// this frame).
    xq: Vec<i16>,
    vad_flag: bool,
}

/// `silk_sigmoid` (`silk/SigProc_FIX.h` approximation is fixed-point; the
/// FLP encoder uses the libm `1/(1+exp(-x))` via `silk_sigmoid`).
fn sigmoid(x: f32) -> f32 {
    1.0 / (1.0 + (-x as f64).exp()) as f32
}
