//! SILK top-level encoder — the mono, single-frame-per-payload foundation.
//!
//! Assembles the encoder modules into a `silk_encode_frame_FLP`-shaped
//! pipeline (mono, 10/20 ms frames, 8/12/16 kHz internal rate, VBR
//! payloads, no LBRR/DTX/FEC/stereo/hybrid):
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
//! 4. **Shaping-proxy gains** (foundation stand-in for
//!    `silk_noise_shape_analysis_FLP`, without the shaping filter/warping):
//!    one frame-level windowed LPC residual energy per frame, adjusted by
//!    the target SNR from the exact [`control_snr`] port.
//! 5. **LTP + LPC** (`silk_find_pred_coefs_FLP` shape): [`find_ltp`] +
//!    [`quant_ltp_gains`] for voiced frames, the weighted LPC analysis on
//!    the (LTP-)residual, [`nlsf_encode`] for the transmitted NLSF vector,
//!    and the decoder-shared `nlsf2a` so the synthesis filters are exactly
//!    the decoder's.
//! 6. **Gains** (`silk_process_gains_FLP` shape): LTP-based reduction, the
//!    soft limit, and [`gains_quant`] into the transmitted indices.
//! 7. **Excitation** ([`nsq::encode_frame_nsq`]): closed-loop quantization
//!    over the decoder's exact arithmetic.
//! 8. **Bitstream**: VAD/LBRR prologue (LBRR always off), side info
//!    (independent coding — the payload carries one frame), excitation.
//!
//! The persistent decoder-mirror state ([`SynthesisState`],
//! `LastGainIndex`, `prevNLSF_Q15`, `ec_prev`, `lagPrev`,
//! `prevSignalType`) matches a fresh [`SilkDecoder`]'s initialization, so
//! encoding then decoding from reset reproduces the encoder's simulated
//! output bit-for-bit (pinned by tests).
//!
//! SOURCE: Xiph.Org libopus 1.5.2, `silk/float/encode_frame_FLP.c`,
//! `find_pitch_lags_FLP.c`, `find_pred_coefs_FLP.c`,
//! `noise_shape_analysis_FLP.c`, `process_gains_FLP.c`,
//! `silk/control_SNR.c` (tables ported verbatim), `silk/define.h`
//! (BSD-3-Clause).
#![allow(dead_code)]

use crate::silk::decode_indices::{
    CondCoding, EcPrevState, FrameParams, SideInfoIndices, MAX_FRAMES_PER_PACKET, MAX_LPC_ORDER,
    MAX_NB_SUBFR, TYPE_NO_VOICE_ACTIVITY, TYPE_UNVOICED, TYPE_VOICED,
};
use crate::silk::encode_indices::{encode_indices, encode_vad_flags_and_lbrr_flag};
use crate::silk::encode_pulses::encode_pulses;
use crate::silk::gains::gains_quant;
use crate::silk::lpc_analysis::{
    a2nlsf, apply_sine_window, autocorrelation, bwexpander_f32, energy, k2a, lpc_analysis_filter,
    schur,
};
use crate::silk::ltp_quant::{correlations_to_q17, find_ltp, ltp_correlation, quant_ltp_gains};
use crate::silk::nlsf::nlsf2a;
use crate::silk::nlsf_quant::{nlsf_encode, nlsf_vq_weights_laroia};
use crate::silk::nsq::encode_frame_nsq;
use crate::silk::resampler::Resampler;
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
/// `MAX_PREDICTION_POWER_GAIN_AFTER_RESET` (`silk/define.h`) — the
/// foundation's LPC analysis does not consume it (A2NLSF bandwidth-expands
/// unstable filters itself); kept for documentation parity.
const _MAX_PREDICTION_POWER_GAIN_AFTER_RESET: f32 = 1e2;

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

/// The SILK frame encoder.
pub struct SilkEncoder {
    /* Geometry */
    fs_khz: u32,
    api_sample_rate: i32,
    nb_subfr: usize,
    frame_length: usize,
    subfr_length: usize,
    ltp_mem_length: usize,
    predict_lpc_order: usize,
    pitch_lpc_order: usize,
    nlsf_cb: &'static NlsfCbStruct,
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

    /* Analysis output retained for the caller/tests */
    last_xq: Vec<i16>,

    /* Input path */
    resampler: Resampler,
    /// `[ltp_mem history | current frame]` at the internal rate.
    x_buf: Vec<f32>,
    vad_flags: [bool; MAX_FRAMES_PER_PACKET],
    frame_counter: u32,
}

impl SilkEncoder {
    /// Creates an encoder for mono input at `api_sample_rate` Hz (one of
    /// 8/12/16/24/48 kHz) coded at `internal_sample_rate` Hz (8/12/16 kHz)
    /// with 10 or 20 ms frames.
    pub fn new(
        api_sample_rate: i32,
        internal_sample_rate: i32,
        frame_size_ms: i32,
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
        let nb_subfr = match frame_size_ms {
            10 => MAX_NB_SUBFR / 2,
            20 => MAX_NB_SUBFR,
            _ => {
                return Err(CadenceError::UnsupportedFeature(format!(
                    "unsupported SILK frame size {frame_size_ms} ms"
                )))
            }
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

        let resampler = Resampler::new(api_sample_rate, internal_sample_rate, true)?;

        Ok(SilkEncoder {
            fs_khz,
            api_sample_rate,
            nb_subfr,
            frame_length,
            subfr_length,
            ltp_mem_length,
            predict_lpc_order,
            pitch_lpc_order,
            nlsf_cb: if fs_khz == 16 {
                &NLSF_CB_WB
            } else {
                &NLSF_CB_NB_MB
            },
            frame: FrameInfo::new(fs_khz, nb_subfr),
            snr_db_q7: 0,
            synth: SynthesisState::default(),
            exc_q14: [0; MAX_FRAME_LENGTH],
            last_xq: Vec::new(),
            last_gain_index: 0,
            prev_nlsf_q15: [0; MAX_LPC_ORDER],
            ec_prev: EcPrevState::default(),
            lag_prev: 0,
            prev_signal_type: TYPE_NO_VOICE_ACTIVITY,
            sum_log_gain_q7: 0,
            first_frame_after_reset: true,
            resampler,
            x_buf: vec![0.0; ltp_mem_length + frame_length],
            vad_flags: [false; MAX_FRAMES_PER_PACKET],
            frame_counter: 0,
        })
    }

    /// `silk_control_SNR`: sets the rate target. `target_rate_bps` is the
    /// total mono bitrate including the ~25 kbps... (the caller passes the
    /// SILK-mode target directly; packet overhead is not modeled).
    pub fn set_bitrate(&mut self, target_rate_bps: i32) {
        self.snr_db_q7 = control_snr(self.fs_khz, self.nb_subfr, target_rate_bps.max(0));
    }

    /// Current SNR target in dB·128 (exposed for tests).
    pub fn snr_db_q7(&self) -> i32 {
        self.snr_db_q7
    }

    /// The number of API-rate samples one frame consumes.
    pub fn frame_length_api(&self) -> usize {
        self.frame_length * self.api_sample_rate as usize / (self.fs_khz as usize * 1000)
    }

    /// The decoder-exact reconstruction of the most recently encoded
    /// frame (the output a conforming decoder produces from the last
    /// payload, at the internal sample rate). Because the closed-loop NSQ
    /// runs on the decoder's own arithmetic, this is bit-identical to the
    /// decoder's output — the property the round-trip tests pin.
    pub fn last_reconstructed_frame(&self) -> &[i16] {
        &self.last_xq
    }

    /// Encodes one frame; `input` must be [`Self::frame_length_api`]
    /// interleaved... (mono) API-rate samples. Returns the SILK payload
    /// bytes (one frame, VBR).
    pub fn encode_frame(&mut self, input: &[i16]) -> Result<Vec<u8>> {
        let api_len = self.frame_length_api();
        if input.len() != api_len {
            return Err(CadenceError::CorruptData(format!(
                "expected {api_len} input samples, got {}",
                input.len()
            )));
        }

        /* Resample to the internal rate */
        let mut x_int = vec![0i16; self.frame_length];
        self.resampler.resample(&mut x_int, input)?;

        /* Slide the history window and append the frame */
        let total = self.ltp_mem_length + self.frame_length;
        self.x_buf.copy_within(self.frame_length..total, 0);
        {
            let tail = &mut self.x_buf[self.ltp_mem_length..total];
            for (dst, &src) in tail.iter_mut().zip(x_int.iter()) {
                *dst = src as f32;
            }
        }

        let seed = (self.frame_counter & 3) as i8;
        self.frame_counter += 1;

        /*--------------------------------------------------------*/
        /* Side-info skeleton (type, VAD flag, seed)              */
        /*--------------------------------------------------------*/
        let frame_rms = (energy(&self.x_buf[self.ltp_mem_length..total]) / self.frame_length as f64)
            .sqrt() as f32;
        let mut indices = SideInfoIndices {
            seed,
            ..SideInfoIndices::default()
        };
        indices.signal_type = if frame_rms < INACTIVE_RMS_THRESHOLD {
            TYPE_NO_VOICE_ACTIVITY
        } else {
            TYPE_UNVOICED
        };
        let vad_flag = indices.signal_type != TYPE_NO_VOICE_ACTIVITY;
        self.vad_flags = [false; MAX_FRAMES_PER_PACKET];
        self.vad_flags[0] = vad_flag;

        /*--------------------------------------------------------*/
        /* Pitch analysis (find_pitch_lags shape)                 */
        /*--------------------------------------------------------*/
        let res_pitch = self.pitch_residual();
        let mut pitch_l = [0i32; MAX_NB_SUBFR];

        if indices.signal_type != TYPE_NO_VOICE_ACTIVITY {
            let (lags, ltp_corr, lag_index, contour_index) = self.pitch_search(&res_pitch);
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
        /* Shaping-proxy gains (noise_shape_analysis shape)       */
        /*--------------------------------------------------------*/
        let snr_adj_db = self.snr_db_q7 as f32 * (1.0 / 128.0);
        let mut gains = self.proxy_gains(snr_adj_db);

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
             * reference's comparisons are both false -> index 0. */
            indices.ltp_scale_index = 0;
            ltpred_cod_gain_db = ltp.pred_gain_d_b_q7 as f32 * (1.0 / 128.0);
        } else {
            ctrl.ltp_coef_q14[..5 * self.nb_subfr].fill(0);
            self.sum_log_gain_q7 = 0;
        }
        ctrl.ltp_scale_q14 =
            crate::silk::tables::LTPSCALES_TABLE_Q14[indices.ltp_scale_index as usize];

        /* Weighted LPC analysis on the (LTP-)residual */
        let (nlsf_q15, lpc_in_pre) =
            self.lpc_analysis_to_nlsf(&pitch_l, &ctrl.ltp_coef_q14, &gains, indices.signal_type);

        /* NLSF quantization + conversion to the decoder's Q12 filters */
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
            self.nlsf_cb,
            &weights,
            nlsf_mu_q20,
            16,
            indices.signal_type,
        );
        indices.nlsf_indices[0] = nlsf_indices[0];
        indices.nlsf_indices[1..=self.predict_lpc_order]
            .copy_from_slice(&nlsf_indices[1..=self.predict_lpc_order]);
        indices.nlsf_interp_coef_q2 = 4;
        nlsf2a(
            &mut ctrl.pred_coef_q12[1][..self.predict_lpc_order],
            &quantized[..self.predict_lpc_order],
            self.predict_lpc_order,
        );
        ctrl.pred_coef_q12[0] = ctrl.pred_coef_q12[1];
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
        gains_quant(
            &mut indices.gains_indices,
            &mut gains_q16,
            &mut self.last_gain_index,
            false,
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
            &mut self.exc_q14,
            &mut pulses,
            &mut xq,
            &x_int,
            &ctrl,
            &indices,
            &self.frame,
            0,
            self.prev_signal_type,
            self.lag_prev,
        );
        self.synth
            .update_out_buf(&xq[..self.frame_length], self.ltp_mem_length);
        self.last_xq = xq[..self.frame_length].to_vec();
        self.lag_prev = ctrl.pitch_l[self.nb_subfr - 1];
        self.prev_signal_type = indices.signal_type;
        self.first_frame_after_reset = false;

        /*--------------------------------------------------------*/
        /* Bitstream                                              */
        /*--------------------------------------------------------*/
        let mut enc = crate::range::RangeEncoder::new();
        encode_vad_flags_and_lbrr_flag(&mut enc, &self.vad_flags, 1, false);
        let params = FrameParams {
            nlsf_cb: self.nlsf_cb,
            fs_khz: self.fs_khz,
            nb_subfr: self.nb_subfr,
            frame_index: 0,
            vad_flag,
            decode_lbrr: false,
            cond_coding: CondCoding::Independently,
        };
        encode_indices(&mut enc, &indices, &mut self.ec_prev, &params, false);
        encode_pulses(
            &mut enc,
            indices.signal_type as i32,
            indices.quant_offset_type as i32,
            &pulses,
            self.frame_length,
        );
        Ok(enc.done())
    }
}

/* ---- analysis helpers (impl block split for readability) ---- */

/// Fields set by [`SilkEncoder::pitch_search`].
impl SilkEncoder {
    /// The pitch-LPC residual over the whole buffered window (history +
    /// frame), foundation shape of `silk_find_pitch_lags_FLP`'s
    /// sine-windowed order-6 analysis.
    fn pitch_residual(&self) -> Vec<f32> {
        let total = self.ltp_mem_length + self.frame_length;
        let mut wsig = vec![0f32; total];
        let slope = (self.fs_khz as usize).min(total / 4);
        apply_sine_window(&mut wsig[..slope], &self.x_buf[..slope], 1);
        let flat = total - 2 * slope;
        wsig[slope..slope + flat].copy_from_slice(&self.x_buf[slope..slope + flat]);
        apply_sine_window(&mut wsig[total - slope..], &self.x_buf[total - slope..], 2);

        let mut auto_corr = [0f32; 7];
        autocorrelation(&mut auto_corr, &wsig);
        auto_corr[0] += auto_corr[0] * FIND_PITCH_WHITE_NOISE_FRACTION + 1.0;
        let mut rc = [0f32; 6];
        schur(&mut rc, &auto_corr, self.pitch_lpc_order);
        let mut a = [0f32; 6];
        k2a(&mut a, &rc, self.pitch_lpc_order);
        bwexpander_f32(
            &mut a[..self.pitch_lpc_order],
            FIND_PITCH_BANDWIDTH_EXPANSION,
        );

        let mut res = vec![0f32; total];
        lpc_analysis_filter(&mut res, &a, &self.x_buf, self.pitch_lpc_order);
        res
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

    /// One windowed autocorrelation over the frame; the schur residual
    /// energy feeds the proxy shaping gains (the SNR adjustment is the
    /// reference's `gain_mult`/`gain_add` pair).
    fn proxy_gains(&self, snr_adj_db: f32) -> [f32; MAX_NB_SUBFR] {
        let total = self.ltp_mem_length + self.frame_length;
        let mut wsig = vec![0f32; self.frame_length];
        let flat = (3 * self.fs_khz as usize).min(self.frame_length / 4);
        /* The sine-window kernel requires multiple-of-4 lengths. */
        let mut slope = (self.frame_length - flat) / 2;
        slope -= slope % 4;
        let x_start = total - self.frame_length;
        apply_sine_window(&mut wsig[..slope], &self.x_buf[x_start..x_start + slope], 1);
        let flat_len = self.frame_length - 2 * slope;
        wsig[slope..slope + flat_len]
            .copy_from_slice(&self.x_buf[x_start + slope..x_start + slope + flat_len]);
        apply_sine_window(
            &mut wsig[slope + flat_len..],
            &self.x_buf[total - slope..total],
            2,
        );

        let mut auto_corr = [0f32; 17];
        autocorrelation(&mut auto_corr, &wsig);
        auto_corr[0] += auto_corr[0] * SHAPE_WHITE_NOISE_FRACTION + 1.0;
        let mut rc = [0f32; 16];
        let nrg = schur(&mut rc, &auto_corr, self.predict_lpc_order);

        let gain_mult = 2.0f32.powf(-0.16 * snr_adj_db);
        let gain_add = 2.0f32.powf(0.16 * 2.0); // 2^(-0.16·MIN_QGAIN_DB form, MIN_QGAIN_DB = 2)
        let mut gains = [0f32; MAX_NB_SUBFR];
        for g in gains.iter_mut().take(self.nb_subfr) {
            *g = nrg.sqrt() * gain_mult + gain_add;
        }
        gains
    }

    /// Weighted LPC analysis of the (LTP-)filtered input, converted to
    /// NLSFs — `find_pred_coefs_FLP` + `silk_find_LPC_FLP`'s analysis
    /// chain (autocorrelation/schur/k2a in place of Burg; A2NLSF is the
    /// exact reference port and bandwidth-expands unstable filters
    /// itself).
    #[allow(clippy::too_many_arguments)]
    fn lpc_analysis_to_nlsf(
        &self,
        pitch_l: &[i32; MAX_NB_SUBFR],
        ltp_coef_q14: &[i16; 20],
        gains: &[f32; MAX_NB_SUBFR],
        signal_type: i8,
    ) -> ([i16; MAX_LPC_ORDER], Vec<f32>) {
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

        /* Autocorrelation + schur + k2a (Burg stand-in) */
        let mut auto_corr = [0f32; 17];
        autocorrelation(&mut auto_corr, &lpc_in_pre);
        auto_corr[0] += auto_corr[0] * SHAPE_WHITE_NOISE_FRACTION + 1.0;
        let mut rc = [0f32; 16];
        schur(&mut rc, &auto_corr, order);
        let mut a = [0f32; 16];
        k2a(&mut a, &rc, order);

        /* A (float) → Q16 monic → NLSF (exact A2NLSF port) */
        let mut a_q16 = [0i32; 16];
        for (dst, &v) in a_q16.iter_mut().zip(a.iter()).take(order) {
            *dst = (v * 65536.0) as i32;
        }
        let mut nlsf = [0i16; MAX_LPC_ORDER];
        a2nlsf(&mut nlsf[..order], &mut a_q16[..order]);
        (nlsf, lpc_in_pre)
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

/// `silk_sigmoid` (`silk/SigProc_FIX.h` approximation is fixed-point; the
/// FLP encoder uses the libm `1/(1+exp(-x))` via `silk_sigmoid`).
fn sigmoid(x: f32) -> f32 {
    1.0 / (1.0 + (-x as f64).exp()) as f32
}
