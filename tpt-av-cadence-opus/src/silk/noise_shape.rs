//! SILK noise-shaping analysis (`silk/noise_shape_analysis_FLP.c`).
//!
//! The reference derives, per subframe, a short **shaping filter** and its
//! gain from a windowed (frequency-warped) autocorrelation of the input,
//! then smooths a per-subframe spectral tilt and a harmonic shaping gain
//! across frames, and finally computes the rate/distortion factor `Lambda`.
//! Those quantities are *encoder-side only*: the decoder never applies a
//! shaping filter (`silk_decode_core` reconstructs excitation → LTP → LPC
//! straight through, see `synthesis.rs`), so nothing here changes the
//! bitstream contract.
//!
//! **What is wired into the quantizer today:** the per-subframe **gains**,
//! which set the excitation LSB the NSQ works in (and the LPC analysis'
//! input weighting). Measured on the crate's synthetic speech, that alone is
//! worth **+0.8 to +1.2 dB SNR** across 8–48 kbps versus the frame-level
//! proxy gain it replaced, for +5–8% payload.
//!
//! **What is computed but not yet consumed:** the shaping filter itself, the
//! tilt, the harmonic gain and `Lambda` — i.e. closing the *error-feedback
//! loop* in the NSQ (`silk_noise_shape_quantizer`'s `n_AR`/`n_LF`/harmonic
//! terms) and the RD rate term. Both were implemented and measured, and both
//! made quality worse *without* the reference's per-frame rate-control loop
//! (`silk_encode_frame_FLP`'s six-iteration `gainMult` ramp that drives the
//! payload onto the bitrate budget): with this encoder's gain scale the
//! shaped residual walks the excitation level away on non-speech content,
//! and the rate term starves the payload (measured 8–29 dB *below* the
//! current baseline). That work is tracked in `todo.md` as paired with the
//! rate-control loop; see the session log for the numbers.
//!
//! Ports of:
//! - `silk_warped_autocorrelation_FLP.c` (allpass-section warped correlation),
//! - the three static helpers of `noise_shape_analysis_FLP.c`
//!   (`warped_gain`, `warped_true2monic_coefs`, `limit_coefs`),
//! - `silk_noise_shape_analysis_FLP` itself, and the float→fixed conversion
//!   `silk_NSQ_wrapper_FLP` applies to its results (`AR_Q13`, `Tilt_Q14`,
//!   `LF_shp_Q14`, `HarmShapeGain_Q14`, `Lambda_Q10`).
//!
//! **Documented deviations from the reference** (all in the analysis'
//! *inputs*, never in the shaping equations):
//! - `speech_activity_Q8` comes from this crate's frame-RMS VAD stand-in
//!   rather than `silk_VAD_GetSA_Q8` (the 4-band VAD is not ported).
//! - `input_quality` / `input_quality_bands_Q15` are held at the maximum
//!   (1.0 / 32768) because the stand-in VAD has no band-quality estimate; the
//!   reference only produces lower values for noisy input.
//! - `input_tilt_Q15` stays 0, so `process_gains`' voiced quantizer-offset
//!   rule reduces to the reference's tilt-free form.
//! - The shaping LPC order is fixed at 16 (the reference's complexity >= 6
//!   setting) rather than complexity-dependent, which also keeps the shared
//!   `schur` kernel inside its 17-entry workspace.
//! - The `la_shape`-sample look-ahead past each frame is zeroed rather than
//!   holding the next frame's real samples, because this encoder resamples one
//!   frame at a time (libopus' `x_buf` is filled with the whole packet before
//!   frame 0 is coded). This matches the reference exactly past the end of a
//!   packet, where the reference also reads its cleared tail; inside a packet
//!   it only tapers the analysis window's final ~1/6.
//!
//! SOURCE: Xiph.Org libopus 1.5.2, `silk/float/noise_shape_analysis_FLP.c`,
//! `silk/float/warped_autocorrelation_FLP.c`, `silk/float/wrappers_FLP.c`,
//! `silk/tuning_parameters.h`, `silk/control_codec.c` (complexity → shaping
//! order / look-ahead / warping) (BSD-3-Clause).
#![allow(dead_code)]

use crate::silk::decode_indices::{MAX_NB_SUBFR, TYPE_VOICED};
use crate::silk::lpc_analysis::{apply_sine_window, autocorrelation, bwexpander_f32, k2a, schur};
use crate::silk::synthesis::MAX_FS_KHZ;

/// `MAX_SHAPE_LPC_ORDER` (`silk/define.h`).
pub(crate) const MAX_SHAPE_LPC_ORDER: usize = 24;

/// `LA_SHAPE_MAX` = `LA_SHAPE_MS * MAX_FS_KHZ` — the shaping analysis'
/// look-ahead past each subframe.
pub(crate) const LA_SHAPE_MAX: usize = 5 * MAX_FS_KHZ;

/// `SHAPE_LPC_WIN_MAX` = `15 * MAX_FS_KHZ` — the shaping window bound.
pub(crate) const SHAPE_LPC_WIN_MAX: usize = 15 * MAX_FS_KHZ;

/// The shaping filter order this encoder uses. The reference picks it from
/// the encoder complexity (12/14/16/20/24); 16 is its complexity >= 6 value,
/// the smallest *even* order that pairs with warping and 5 ms look-ahead,
/// and the largest this crate's shared `schur` kernel supports.
pub(crate) const SHAPING_LPC_ORDER: usize = 16;

/// `WARPING_MULTIPLIER` (`silk/tuning_parameters.h`).
const WARPING_MULTIPLIER: f32 = 0.015;
/// `MIN_QGAIN_DB` (`silk/define.h`).
const MIN_QGAIN_DB: f32 = 2.0;
/// `SHAPE_WHITE_NOISE_FRACTION`.
const SHAPE_WHITE_NOISE_FRACTION: f32 = 3e-5;
/// `FIND_PITCH_WHITE_NOISE_FRACTION`, reused as the shaping bandwidth
/// expansion's strength scaling.
const FIND_PITCH_WHITE_NOISE_FRACTION: f32 = 1e-3;
/// `BANDWIDTH_EXPANSION` — the shaping filter's nominal chirp.
const BANDWIDTH_EXPANSION: f32 = 0.94;
/// `HARMONIC_SHAPING`.
const HARMONIC_SHAPING: f32 = 0.3;
/// `HIGH_RATE_OR_LOW_QUALITY_HARMONIC_SHAPING`.
const HIGH_RATE_OR_LOW_QUALITY_HARMONIC_SHAPING: f32 = 0.2;
/// `HP_NOISE_COEF`.
const HP_NOISE_COEF: f32 = 0.25;
/// `HARM_HP_NOISE_COEF`.
const HARM_HP_NOISE_COEF: f32 = 0.35;
/// `LOW_FREQ_SHAPING`.
const LOW_FREQ_SHAPING: f32 = 4.0;
/// `LOW_QUALITY_LOW_FREQ_SHAPING_DECR`.
const LOW_QUALITY_LOW_FREQ_SHAPING_DECR: f32 = 0.5;
/// `BG_SNR_DECR_dB`.
const BG_SNR_DECR_DB: f32 = 2.0;
/// `HARM_SNR_INCR_dB`.
const HARM_SNR_INCR_DB: f32 = 2.0;
/// `SUBFR_SMTH_COEF`.
const SUBFR_SMTH_COEF: f32 = 0.4;
/// `USE_HARM_SHAPING` (`silk/define.h`).
const USE_HARM_SHAPING: bool = true;
/// `LAMBDA_OFFSET`.
const LAMBDA_OFFSET: f32 = 1.2;
/// `LAMBDA_SPEECH_ACT`.
const LAMBDA_SPEECH_ACT: f32 = -0.2;
/// `LAMBDA_DELAYED_DECISIONS` — zero here (no delayed decisions).
const LAMBDA_DELAYED_DECISIONS: f32 = 0.0;
/// `LAMBDA_INPUT_QUALITY`.
const LAMBDA_INPUT_QUALITY: f32 = -0.1;
/// `LAMBDA_CODING_QUALITY`.
const LAMBDA_CODING_QUALITY: f32 = -0.2;
/// `LAMBDA_QUANT_OFFSET`.
const LAMBDA_QUANT_OFFSET: f32 = 0.8;
/// The shaping filter's coefficient magnitude limit (`3.999f`).
const COEF_LIMIT: f32 = 3.999;
/// `HARM_SHAPE_FIR_TAPS` (`silk/define.h`) — the harmonic shaping FIR.
pub(crate) const HARM_SHAPE_FIR_TAPS: usize = 3;

/// `silk_sigmoid` (`silk/float/SigProc_FLP.h`): `1 / (1 + exp(-x))`.
#[inline]
fn sigmoid(x: f32) -> f32 {
    1.0 / (1.0 + (-x as f64).exp()) as f32
}

/// `silk_float2int` — round-to-nearest conversion of the float shaping
/// parameters into their fixed-point form.
#[inline]
fn float2int(x: f32) -> i32 {
    x.round() as i32
}

/// Per-channel, cross-frame noise-shaping state (`silk_shape_state_FLP`):
/// the two subframe-smoothed parameters. `LastGainIndex` lives with the rest
/// of the gain state in the encoder.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct ShapeState {
    /// `HarmShapeGain_smth`.
    pub harm_shape_gain_smth: f32,
    /// `Tilt_smth`.
    pub tilt_smth: f32,
}

/// The frame geometry the analysis needs (mirrors the channel's frame
/// geometry fields).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ShapeGeometry {
    /// Internal sample rate in kHz (8/12/16).
    pub fs_khz: u32,
    /// Subframes per frame (2 for 10 ms frames, 4 for 20 ms).
    pub nb_subfr: usize,
    /// Samples per subframe.
    pub subfr_length: usize,
    /// Samples per frame.
    pub frame_length: usize,
    /// LTP memory length: the history the analysis window reaches back into.
    pub ltp_mem_length: usize,
}

impl ShapeGeometry {
    /// `la_shape` = `5 * fs_kHz` (the reference's complexity >= 2 value).
    pub fn la_shape(&self) -> usize {
        5 * self.fs_khz as usize
    }

    /// `shapeWinLength` = `subfr_length + 2 * la_shape`.
    pub fn shape_win_length(&self) -> usize {
        self.subfr_length + 2 * self.la_shape()
    }

    /// `warping_Q16` = `fs_kHz * WARPING_MULTIPLIER` in Q16 (the reference's
    /// complexity >= 4 value; zero below that, where warping is disabled).
    pub fn warping_q16(&self) -> i32 {
        warping_q16(self.fs_khz)
    }
}

/// `warping_Q16` = `fs_kHz * WARPING_MULTIPLIER` in Q16 — the warp shared
/// by the shaping analysis (`ShapeGeometry`) and, from complexity 4, the
/// quantizer's noise-shaping feedback loop (`silk_setup_complexity`).
pub(crate) fn warping_q16(fs_khz: u32) -> i32 {
    (fs_khz as f32 * WARPING_MULTIPLIER * 65536.0) as i32
}

/// One frame's noise-shaping parameters in the fixed-point form the NSQ
/// consumes (`silk_NSQ_wrapper_FLP`'s conversion), plus the float
/// rate/distortion factor and coding quality for diagnostics and tests.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ShapeParams {
    /// `AR_Q13`: per-subframe shaping filter, Q13, `nb_subfr` blocks of
    /// [`MAX_SHAPE_LPC_ORDER`].
    pub ar_q13: [i16; MAX_NB_SUBFR * MAX_SHAPE_LPC_ORDER],
    /// `LF_shp_Q14`: per subframe, `LF_AR_shp` in the upper 16 bits and
    /// `LF_MA_shp` in the lower 16 (both Q14) — the reference's packed pair.
    pub lf_shp_q14: [i32; MAX_NB_SUBFR],
    /// `Tilt_Q14`: per-subframe spectral tilt, Q14.
    pub tilt_q14: [i32; MAX_NB_SUBFR],
    /// `HarmShapeGain_Q14`: per-subframe harmonic shaping gain, Q14.
    pub harm_shape_gain_q14: [i32; MAX_NB_SUBFR],
    /// `Lambda_Q10`: rate/distortion tradeoff, Q10.
    pub lambda_q10: i32,
    /// `Lambda` (float).
    pub lambda: f32,
    /// `coding_quality`, 0..1.
    pub coding_quality: f32,
}

/// `silk_warped_autocorrelation_FLP`: autocorrelation on a warped frequency
/// axis, via a cascade of `order` first-order allpass sections (which is why
/// the reference requires an even order).
fn warped_autocorrelation(corr: &mut [f32], input: &[f32], warping: f32, order: usize) {
    debug_assert_eq!(order & 1, 0, "warped autocorrelation needs an even order");
    let mut state = [0f64; MAX_SHAPE_LPC_ORDER + 1];
    let mut c = [0f64; MAX_SHAPE_LPC_ORDER + 1];
    for &sample in input {
        let mut tmp1 = sample as f64;
        let mut i = 0;
        while i < order {
            let tmp2 = state[i] + warping as f64 * state[i + 1] - warping as f64 * tmp1;
            state[i] = tmp1;
            c[i] += state[0] * tmp1;
            tmp1 = state[i + 1] + warping as f64 * state[i + 2] - warping as f64 * tmp2;
            state[i + 1] = tmp2;
            c[i + 1] += state[0] * tmp2;
            i += 2;
        }
        state[order] = tmp1;
        c[order] += state[0] * tmp1;
    }
    for (dst, &v) in corr.iter_mut().zip(c.iter()).take(order + 1) {
        *dst = v as f32;
    }
}

/// `warped_gain`: the gain that makes the warped filter's log frequency
/// response zero-mean on a non-warped axis (so it stays minimum-phase/monic).
fn warped_gain(coefs: &[f32], lambda: f32, order: usize) -> f32 {
    let lambda = -lambda;
    let mut gain = coefs[order - 1];
    for i in (0..order - 1).rev() {
        gain = lambda * gain + coefs[i];
    }
    1.0 / (1.0 - lambda * gain)
}

/// Largest absolute coefficient and its index (the reference's `maxabs`/`ind`).
fn max_abs_index(coefs: &[f32], order: usize) -> (f32, usize) {
    let mut maxabs = -1.0f32;
    let mut ind = 0;
    for (i, &c) in coefs.iter().take(order).enumerate() {
        let tmp = c.abs();
        if tmp > maxabs {
            maxabs = tmp;
            ind = i;
        }
    }
    (maxabs, ind)
}

/// `warped_true2monic_coefs`: convert to monic pseudo-warped coefficients and
/// limit their magnitude by bandwidth-expanding the true coefficients.
fn warped_true2monic_coefs(coefs: &mut [f32], lambda: f32, limit: f32, order: usize) {
    /* Convert to monic coefficients */
    for i in (1..order).rev() {
        coefs[i - 1] -= lambda * coefs[i];
    }
    let mut gain = (1.0 - lambda * lambda) / (1.0 + lambda * coefs[0]);
    for v in coefs.iter_mut().take(order) {
        *v *= gain;
    }

    /* Limit */
    for iter in 0..10 {
        let (maxabs, ind) = max_abs_index(coefs, order);
        if maxabs <= limit {
            return;
        }

        /* Convert back to true warped coefficients */
        for i in 1..order {
            coefs[i - 1] += lambda * coefs[i];
        }
        gain = 1.0 / gain;
        for v in coefs.iter_mut().take(order) {
            *v *= gain;
        }

        /* Apply bandwidth expansion */
        let chirp =
            0.99 - (0.8 + 0.1 * iter as f32) * (maxabs - limit) / (maxabs * (ind + 1) as f32);
        bwexpander_f32(&mut coefs[..order], chirp);

        /* Convert to monic warped coefficients */
        for i in (1..order).rev() {
            coefs[i - 1] -= lambda * coefs[i];
        }
        gain = (1.0 - lambda * lambda) / (1.0 + lambda * coefs[0]);
        for v in coefs.iter_mut().take(order) {
            *v *= gain;
        }
    }
    debug_assert!(false, "warped_true2monic_coefs failed to converge");
}

/// `limit_coefs`: bandwidth-expand until every coefficient is within `limit`.
fn limit_coefs(coefs: &mut [f32], limit: f32, order: usize) {
    for iter in 0..10 {
        let (maxabs, ind) = max_abs_index(coefs, order);
        if maxabs <= limit {
            return;
        }
        let chirp =
            0.99 - (0.8 + 0.1 * iter as f32) * (maxabs - limit) / (maxabs * (ind + 1) as f32);
        bwexpander_f32(&mut coefs[..order], chirp);
    }
    debug_assert!(false, "limit_coefs failed to converge");
}

impl ShapeParams {
    /// The `LF_AR_shp` half of [`Self::lf_shp_q14`], Q14.
    #[inline]
    pub fn lf_ar_shp_q14(&self, k: usize) -> i32 {
        self.lf_shp_q14[k] >> 16
    }

    /// The `LF_MA_shp` half of [`Self::lf_shp_q14`], Q14.
    #[inline]
    pub fn lf_ma_shp_q14(&self, k: usize) -> i32 {
        (self.lf_shp_q14[k] as u16 as i16) as i32
    }
}

/// `silk_noise_shape_analysis_FLP`.
///
/// `x_buf` is the channel's analysis buffer laid out as
/// `[ltp_mem history | frame | la_shape look-ahead]` (the reference's
/// `x_buf`/`x_frame` arrangement), so `x_buf[ltp_mem_length]` is the first
/// sample of the frame being encoded; the look-ahead region past it is
/// zero-filled by the caller (the reference reads whatever the next frame
/// will write there, which is zeros until the stream ends). Writes the
/// per-subframe gains into `gains` and returns the shaping parameters the
/// NSQ consumes.
#[allow(clippy::too_many_arguments)]
pub(crate) fn noise_shape_analysis(
    shape: &mut ShapeState,
    geo: &ShapeGeometry,
    x_buf: &[f32],
    snr_db_q7: i32,
    signal_type: i8,
    quant_offset_type: i8,
    pitch_l: &[i32; MAX_NB_SUBFR],
    ltp_corr: f32,
    pred_gain: f32,
    speech_activity_q8: i32,
    input_quality_bands_q15: &[i32; 4],
    use_cbr: bool,
    gains: &mut [f32; MAX_NB_SUBFR],
) -> ShapeParams {
    let la_shape = geo.la_shape();
    let shape_win_length = geo.shape_win_length();
    let warping_q16 = geo.warping_q16();
    debug_assert!(la_shape <= geo.ltp_mem_length);
    debug_assert!(shape_win_length <= SHAPE_LPC_WIN_MAX);
    debug_assert!(x_buf.len() >= geo.ltp_mem_length + geo.frame_length + la_shape);

    /*----------------*/
    /* GAIN CONTROL   */
    /*----------------*/
    let mut snr_adj_db = snr_db_q7 as f32 * (1.0 / 128.0);
    /* Input quality from the VAD's band SNRs (`silk_control_encoder`
     * maps band 0's quality to the scalar input quality). */
    let input_quality_band0 = input_quality_bands_q15[0] as f32 * (1.0 / 32768.0);
    let input_quality = input_quality_band0;
    let coding_quality = sigmoid(0.25 * (snr_adj_db - 20.0));

    if !use_cbr {
        /* Reduce coding SNR during low speech activity */
        let b = 1.0 - speech_activity_q8 as f32 * (1.0 / 256.0);
        snr_adj_db -= BG_SNR_DECR_DB * coding_quality * (0.5 + 0.5 * input_quality) * b * b;
    }
    if signal_type == TYPE_VOICED {
        /* Reduce gains for periodic signals */
        snr_adj_db += HARM_SNR_INCR_DB * ltp_corr;
    } else {
        /* For unvoiced signals and low-quality input, adjust the quality
         * slower than the SNR_dB setting */
        snr_adj_db += (-0.4 * snr_db_q7 as f32 * (1.0 / 128.0) + 6.0) * (1.0 - input_quality);
    }

    /*---------------------------------*/
    /* Control bandwidth expansion      */
    /*---------------------------------*/
    /* More BWE for signals with high prediction gain */
    let strength = FIND_PITCH_WHITE_NOISE_FRACTION * pred_gain;
    let bw_exp = BANDWIDTH_EXPANSION / (1.0 + strength * strength);

    /* Slightly more warping in analysis moves quantization noise up in
     * frequency, where it is better masked */
    let warping = warping_q16 as f32 / 65536.0 + 0.01 * coding_quality;

    /*------------------------------------------*/
    /* Compute noise shaping AR coefs and gains  */
    /*------------------------------------------*/
    let mut ar = [0f32; MAX_NB_SUBFR * MAX_SHAPE_LPC_ORDER];
    let mut auto_corr = [0f32; MAX_SHAPE_LPC_ORDER + 1];
    let mut rc = [0f32; MAX_SHAPE_LPC_ORDER + 1];
    let mut x_windowed = vec![0f32; shape_win_length];
    for k in 0..geo.nb_subfr {
        /* Apply window: sine slope, flat part, cosine slope. The reference
         * uses `flat_part = 3 * fs_kHz` and splits the rest evenly; the
         * sine-window kernel needs multiple-of-4 lengths. */
        let flat_part = 3 * geo.fs_khz as usize;
        let mut slope_part = (shape_win_length - flat_part) / 2;
        slope_part -= slope_part % 4;
        let start = geo.ltp_mem_length + k * geo.subfr_length - la_shape;
        let src = &x_buf[start..start + shape_win_length];
        apply_sine_window(&mut x_windowed[..slope_part], &src[..slope_part], 1);
        x_windowed[slope_part..slope_part + flat_part]
            .copy_from_slice(&src[slope_part..slope_part + flat_part]);
        apply_sine_window(
            &mut x_windowed[slope_part + flat_part..],
            &src[slope_part + flat_part..],
            2,
        );

        if warping_q16 > 0 {
            warped_autocorrelation(&mut auto_corr, &x_windowed, warping, SHAPING_LPC_ORDER);
        } else {
            autocorrelation(&mut auto_corr[..SHAPING_LPC_ORDER + 1], &x_windowed);
        }

        /* Add white noise, as a fraction of energy */
        auto_corr[0] += auto_corr[0] * SHAPE_WHITE_NOISE_FRACTION + 1.0;

        /* Correlations to prediction coefficients, plus residual energy */
        let nrg = schur(&mut rc[..SHAPING_LPC_ORDER], &auto_corr, SHAPING_LPC_ORDER);
        k2a(&mut ar[k * MAX_SHAPE_LPC_ORDER..], &rc, SHAPING_LPC_ORDER);
        gains[k] = nrg.sqrt();

        if warping_q16 > 0 {
            /* Adjust the gain for warping */
            gains[k] *= warped_gain(&ar[k * MAX_SHAPE_LPC_ORDER..], warping, SHAPING_LPC_ORDER);
        }

        /* Bandwidth expansion for the synthesis filter shaping */
        bwexpander_f32(
            &mut ar[k * MAX_SHAPE_LPC_ORDER..k * MAX_SHAPE_LPC_ORDER + SHAPING_LPC_ORDER],
            bw_exp,
        );

        if warping_q16 > 0 {
            /* Monic warped prediction coefficients, magnitude-limited */
            warped_true2monic_coefs(
                &mut ar[k * MAX_SHAPE_LPC_ORDER..],
                warping,
                COEF_LIMIT,
                SHAPING_LPC_ORDER,
            );
        } else {
            limit_coefs(
                &mut ar[k * MAX_SHAPE_LPC_ORDER..],
                COEF_LIMIT,
                SHAPING_LPC_ORDER,
            );
        }
    }

    /*----------------*/
    /* Gain tweaking  */
    /*----------------*/
    let gain_mult = 2.0f32.powf(-0.16 * snr_adj_db);
    let gain_add = 2.0f32.powf(0.16 * MIN_QGAIN_DB);
    for g in gains.iter_mut().take(geo.nb_subfr) {
        *g = *g * gain_mult + gain_add;
    }

    /*------------------------------------------*/
    /* Low-frequency shaping and noise tilt     */
    /*------------------------------------------*/
    /* Less low frequency shaping for noisy inputs */
    let mut lf_strength =
        LOW_FREQ_SHAPING * (1.0 + LOW_QUALITY_LOW_FREQ_SHAPING_DECR * (input_quality_band0 - 1.0));
    lf_strength *= speech_activity_q8 as f32 * (1.0 / 256.0);
    let mut lf_ma_shp = [0f32; MAX_NB_SUBFR];
    let mut lf_ar_shp = [0f32; MAX_NB_SUBFR];
    let tilt = if signal_type == TYPE_VOICED {
        /* Reduce low frequency quantization noise for periodic signals,
         * depending on the pitch lag */
        for k in 0..geo.nb_subfr {
            let b = 0.2 / geo.fs_khz as f32 + 3.0 / pitch_l[k].max(1) as f32;
            lf_ma_shp[k] = -1.0 + b;
            lf_ar_shp[k] = 1.0 - b - b * lf_strength;
        }
        -HP_NOISE_COEF
            - (1.0 - HP_NOISE_COEF) * HARM_HP_NOISE_COEF * speech_activity_q8 as f32 * (1.0 / 256.0)
    } else {
        let b = 1.3 / geo.fs_khz as f32;
        lf_ma_shp[0] = -1.0 + b;
        lf_ar_shp[0] = 1.0 - b - b * lf_strength * 0.6;
        for k in 1..geo.nb_subfr {
            lf_ma_shp[k] = lf_ma_shp[0];
            lf_ar_shp[k] = lf_ar_shp[0];
        }
        -HP_NOISE_COEF
    };

    /*--------------------------*/
    /* Harmonic shaping control */
    /*--------------------------*/
    let mut harm_shape_gain = 0f32;
    if USE_HARM_SHAPING && signal_type == TYPE_VOICED {
        /* More harmonic noise shaping for high bitrates or noisy input */
        harm_shape_gain = HARMONIC_SHAPING
            + HIGH_RATE_OR_LOW_QUALITY_HARMONIC_SHAPING
                * (1.0 - (1.0 - coding_quality) * input_quality);
        /* Less harmonic noise shaping for less periodic signals */
        harm_shape_gain *= ltp_corr.max(0.0).sqrt();
    }

    /*------------------------*/
    /* Smooth over subframes   */
    /*------------------------*/
    let mut params = ShapeParams {
        ar_q13: [0i16; MAX_NB_SUBFR * MAX_SHAPE_LPC_ORDER],
        lf_shp_q14: [0i32; MAX_NB_SUBFR],
        tilt_q14: [0i32; MAX_NB_SUBFR],
        harm_shape_gain_q14: [0i32; MAX_NB_SUBFR],
        lambda_q10: 0,
        lambda: 0.0,
        coding_quality,
    };
    for k in 0..geo.nb_subfr {
        shape.harm_shape_gain_smth +=
            SUBFR_SMTH_COEF * (harm_shape_gain - shape.harm_shape_gain_smth);
        shape.tilt_smth += SUBFR_SMTH_COEF * (tilt - shape.tilt_smth);

        /* Float to fixed (`silk_NSQ_wrapper_FLP`) */
        for j in 0..SHAPING_LPC_ORDER {
            params.ar_q13[k * MAX_SHAPE_LPC_ORDER + j] =
                float2int(ar[k * MAX_SHAPE_LPC_ORDER + j] * 8192.0) as i16;
        }
        params.lf_shp_q14[k] = (float2int(lf_ar_shp[k] * 16384.0) << 16)
            | (float2int(lf_ma_shp[k] * 16384.0) as u16 as i32);
        params.tilt_q14[k] = float2int(shape.tilt_smth * 16384.0);
        params.harm_shape_gain_q14[k] = float2int(shape.harm_shape_gain_smth * 16384.0);
        debug_assert!(params.harm_shape_gain_q14[k] >= 0);
    }

    /*---------------------------------------*/
    /* Quantizer boundary adjustment (Lambda) */
    /*---------------------------------------*/
    /* The offset type is the one `noise_shape_analysis` fixed for this frame
     * (the reference recomputes `Lambda` in `process_gains` once the offset
     * type is final; this encoder keeps the offset type fixed after the
     * frame's analysis, so the two agree). */
    let quant_offset = crate::silk::tables::QUANTIZATION_OFFSETS_Q10[(signal_type >> 1) as usize]
        [quant_offset_type.clamp(0, 1) as usize] as f32
        / 1024.0;
    let lambda = LAMBDA_OFFSET
        + LAMBDA_DELAYED_DECISIONS
        + LAMBDA_SPEECH_ACT * speech_activity_q8 as f32 * (1.0 / 256.0)
        + LAMBDA_INPUT_QUALITY * input_quality
        + LAMBDA_CODING_QUALITY * coding_quality
        + LAMBDA_QUANT_OFFSET * quant_offset;
    debug_assert!(lambda > 0.0 && lambda < 2.0);
    params.lambda = lambda;
    params.lambda_q10 = float2int(lambda * 1024.0);
    params
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::silk::synthesis::SUB_FRAME_LENGTH_MS;

    /// Geometry for a `nb_subfr`-subframe frame at `fs_kHz` internal.
    fn geo(fs_khz: u32, nb_subfr: usize) -> ShapeGeometry {
        let subfr_length = SUB_FRAME_LENGTH_MS * fs_khz as usize;
        ShapeGeometry {
            fs_khz,
            nb_subfr,
            subfr_length,
            frame_length: subfr_length * nb_subfr,
            ltp_mem_length: 20 * fs_khz as usize,
        }
    }

    /// A 130 Hz-plus-second-harmonic signal over the whole analysis buffer.
    fn speechy(g: &ShapeGeometry) -> Vec<f32> {
        let total = g.ltp_mem_length + g.frame_length + g.la_shape();
        let fs = g.fs_khz as f32;
        (0..total)
            .map(|i| {
                let t = i as f32 / fs;
                6000.0 * (2.0 * core::f32::consts::PI * 130.0 * t).sin()
                    + 2000.0 * (2.0 * core::f32::consts::PI * 260.0 * t).sin()
            })
            .collect()
    }

    /// The SNR target (Q7 dB) the analysis tests run at: 24 kbps, 20 ms.
    const SNR_DB_Q7: i32 = 24_000 / 400 * 21;

    fn analyze(
        g: &ShapeGeometry,
        x: &[f32],
        signal_type: i8,
        pitch_l: &[i32; MAX_NB_SUBFR],
        state: &mut ShapeState,
    ) -> (ShapeParams, [f32; MAX_NB_SUBFR]) {
        let mut gains = [0f32; MAX_NB_SUBFR];
        let params = noise_shape_analysis(
            state,
            g,
            x,
            SNR_DB_Q7,
            signal_type,
            0,
            pitch_l,
            0.8,
            20.0,
            200,
            &[32768; 4],
            false,
            &mut gains,
        );
        (params, gains)
    }

    #[test]
    fn geometry_matches_the_reference_definitions() {
        let g = geo(16, MAX_NB_SUBFR);
        assert_eq!(g.la_shape(), 5 * 16);
        assert_eq!(g.shape_win_length(), 80 + 160);
        assert!(g.shape_win_length() <= SHAPE_LPC_WIN_MAX);
        assert_eq!(
            g.warping_q16(),
            (16.0 * WARPING_MULTIPLIER * 65536.0) as i32
        );
        assert!(g.warping_q16() > 0, "warping must be enabled");
        // 8 kHz / 10 ms frames are the smallest geometry the analysis runs on.
        let g = geo(8, 2);
        assert_eq!(g.la_shape(), 40);
        assert_eq!(g.shape_win_length(), 40 + 80);
        assert!(g.la_shape() <= g.ltp_mem_length);
        assert!(g.shape_win_length() <= SHAPE_LPC_WIN_MAX);
    }

    #[test]
    fn gains_are_positive_and_bounded() {
        for &(fs, nb) in &[(16u32, 4usize), (12, 4), (8, 2), (16, 2)] {
            let g = geo(fs, nb);
            let x = speechy(&g);
            let mut state = ShapeState::default();
            let (_, gains) = analyze(&g, &x, TYPE_VOICED, &[124; MAX_NB_SUBFR], &mut state);
            for (k, &gain) in gains.iter().enumerate().take(g.nb_subfr) {
                assert!(
                    gain.is_finite() && gain > 0.0 && gain < 32767.0,
                    "{fs} kHz / {nb} subfr gain {k} = {gain}"
                );
            }
        }
    }

    #[test]
    fn shaping_coefficients_are_bounded_and_non_degenerate() {
        let g = geo(16, MAX_NB_SUBFR);
        let x = speechy(&g);
        let mut state = ShapeState::default();
        let (params, _) = analyze(&g, &x, TYPE_VOICED, &[124; MAX_NB_SUBFR], &mut state);
        let mut energy = 0f32;
        for k in 0..g.nb_subfr {
            for j in 0..SHAPING_LPC_ORDER {
                let a = params.ar_q13[k * MAX_SHAPE_LPC_ORDER + j] as f32 / 8192.0;
                assert!(
                    a.abs() <= COEF_LIMIT + 1e-3,
                    "AR[{k}][{j}] = {a} exceeds the limit"
                );
                energy += a * a;
            }
        }
        // A tonal input must shape with real, sub-unit coefficients (an
        // all-zero shaping filter would mean the analysis did nothing).
        assert!(energy > 1e-3, "shaping filter is degenerate ({energy})");
    }

    #[test]
    fn voiced_and_unvoiced_take_different_lf_and_harmonic_paths() {
        let g = geo(16, MAX_NB_SUBFR);
        let x = speechy(&g);
        let mut voiced_state = ShapeState::default();
        let (voiced, _) = analyze(&g, &x, TYPE_VOICED, &[124; MAX_NB_SUBFR], &mut voiced_state);
        let mut unvoiced_state = ShapeState::default();
        let (unvoiced, _) = analyze(&g, &x, 1, &[0; MAX_NB_SUBFR], &mut unvoiced_state);
        // Voiced frames get harmonic shaping and a pitch-dependent LF pair;
        // unvoiced frames get neither and one flat LF pair.
        assert!(voiced.harm_shape_gain_q14[0] > 0);
        assert_eq!(unvoiced.harm_shape_gain_q14[0], 0);
        assert_ne!(voiced.lf_ma_shp_q14(0), unvoiced.lf_ma_shp_q14(0));
        assert!(unvoiced.tilt_q14[0] < 0);
        for k in 1..g.nb_subfr {
            assert_eq!(unvoiced.lf_ma_shp_q14(k), unvoiced.lf_ma_shp_q14(0));
            assert_eq!(unvoiced.lf_ar_shp_q14(k), unvoiced.lf_ar_shp_q14(0));
        }
    }

    #[test]
    fn voiced_lf_shaping_tracks_the_pitch_lag() {
        let g = geo(16, MAX_NB_SUBFR);
        let x = speechy(&g);
        let mut state = ShapeState::default();
        let (long_lag, _) = analyze(&g, &x, TYPE_VOICED, &[288; MAX_NB_SUBFR], &mut state);
        let mut state = ShapeState::default();
        let (short_lag, _) = analyze(&g, &x, TYPE_VOICED, &[32; MAX_NB_SUBFR], &mut state);
        // `b = 0.2/fs + 3/lag` grows as the lag shortens, so the LF moving
        // average coefficient ($-1 + b$) must too.
        assert!(short_lag.lf_ma_shp_q14(0) > long_lag.lf_ma_shp_q14(0));
    }

    #[test]
    fn tilt_and_harmonic_gain_are_smoothed_across_calls() {
        let g = geo(16, MAX_NB_SUBFR);
        let x = speechy(&g);
        let mut state = ShapeState::default();
        let mut prev_tilt = 0.0f32;
        let mut prev_harm = 0.0f32;
        for _ in 0..20 {
            let (params, _) = analyze(&g, &x, TYPE_VOICED, &[124; MAX_NB_SUBFR], &mut state);
            // SUBFR_SMTH_COEF < 1 (applied once per subframe, so four times
            // per 20 ms frame): the smoothed values converge towards their
            // target gradually instead of jumping to it on the first frame.
            assert!(state.tilt_smth.abs() >= prev_tilt - f32::EPSILON);
            assert!(state.tilt_smth.abs() <= 0.5, "tilt overshot its target");
            assert!(state.harm_shape_gain_smth >= prev_harm);
            prev_tilt = state.tilt_smth.abs();
            prev_harm = state.harm_shape_gain_smth;
            assert!(params.lambda_q10 > 0 && params.lambda_q10 < 2048);
            /* The smoothing runs once per subframe, so the frame's tilt
             * values step monotonically (tilt is negative and grows in
             * magnitude) towards the final smoothed state; the Q14
             * conversion can repeat a value while the steps are tiny. */
            for k in 1..g.nb_subfr {
                assert!(
                    params.tilt_q14[k] <= params.tilt_q14[k - 1],
                    "tilt not smoothed at subframe {k}"
                );
            }
            assert_eq!(
                params.tilt_q14[g.nb_subfr - 1],
                float2int(state.tilt_smth * 16384.0),
                "the last subframe must carry the final smoothed tilt"
            );
        }
        assert!(state.tilt_smth < 0.0, "voiced tilt must be negative");
        assert!(state.harm_shape_gain_smth > 0.0);
    }

    #[test]
    fn silence_yields_a_flat_bounded_shaping_filter() {
        let g = geo(16, MAX_NB_SUBFR);
        let x = vec![0f32; g.ltp_mem_length + g.frame_length + g.la_shape()];
        let mut state = ShapeState::default();
        let (params, gains) = analyze(&g, &x, TYPE_VOICED, &[124; MAX_NB_SUBFR], &mut state);
        for (k, &gain) in gains.iter().enumerate().take(g.nb_subfr) {
            for j in 0..SHAPING_LPC_ORDER {
                let a = params.ar_q13[k * MAX_SHAPE_LPC_ORDER + j] as f32 / 8192.0;
                assert!(a.abs() <= COEF_LIMIT + 1e-3);
            }
            assert!(gain > 0.0 && gain < 32767.0);
        }
        assert!(state.tilt_smth < 0.0);
    }

    #[test]
    fn lambda_falls_with_speech_activity_and_quality() {
        let g = geo(16, MAX_NB_SUBFR);
        let x = speechy(&g);
        let run = |snr: i32, activity: i32, cbr: bool| {
            let mut state = ShapeState::default();
            let mut gains = [0f32; MAX_NB_SUBFR];
            noise_shape_analysis(
                &mut state,
                &g,
                &x,
                snr,
                TYPE_VOICED,
                0,
                &[124; MAX_NB_SUBFR],
                0.8,
                20.0,
                activity,
                &[32768; 4],
                cbr,
                &mut gains,
            )
            .lambda
        };
        // `LAMBDA_SPEECH_ACT`/`LAMBDA_CODING_QUALITY` are both negative, so
        // Lambda decreases as the activity and the coding quality rise.
        assert!(run(SNR_DB_Q7, 0, false) > run(SNR_DB_Q7, 256, false));
        assert!(run(SNR_DB_Q7, 200, false) > run(SNR_DB_Q7 * 3, 200, false));
        // CBR only suppresses the background-SNR *gain* reduction, so it
        // changes the gains while leaving Lambda (a function of activity,
        // quality and the quantizer offset alone) untouched.
        let gains_of = |cbr: bool| {
            let mut state = ShapeState::default();
            let mut gains = [0f32; MAX_NB_SUBFR];
            noise_shape_analysis(
                &mut state,
                &g,
                &x,
                SNR_DB_Q7,
                TYPE_VOICED,
                0,
                &[124; MAX_NB_SUBFR],
                0.8,
                20.0,
                0,
                &[32768; 4],
                cbr,
                &mut gains,
            );
            gains
        };
        assert_ne!(gains_of(false), gains_of(true));
    }

    #[test]
    fn warped_autocorrelation_reduces_to_plain_correlation_at_zero_warp() {
        let input: Vec<f32> = (0..64).map(|i| (i as f32 * 0.37).sin() * 1000.0).collect();
        let order = 8;
        let mut warped = [0f32; MAX_SHAPE_LPC_ORDER + 1];
        warped_autocorrelation(&mut warped, &input, 0.0, order);
        let mut plain = [0f32; MAX_SHAPE_LPC_ORDER + 1];
        autocorrelation(&mut plain[..order + 1], &input);
        for i in 0..=order {
            assert!(
                (warped[i] - plain[i]).abs() <= 1e-3 * plain[i].abs().max(1.0),
                "lag {i}: {} vs {}",
                warped[i],
                plain[i]
            );
        }
    }

    #[test]
    fn warping_flattens_a_low_pass_signal() {
        // The allpass cascade rotates the spectrum up towards flat, so the
        // sum of the non-zero-lag correlations (a flatness measure) of a
        // strongly low-passed signal must fall.
        let fs = 16_000.0f32;
        let a = (-2.0 * core::f32::consts::PI * 300.0 / fs).exp();
        let mut seed = 0x1234_5678u32;
        let mut state_lp = 0f32;
        let input: Vec<f32> = (0..1024)
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                let white = (seed % 2000) as f32 - 1000.0;
                state_lp = a * state_lp + (1.0 - a) * white;
                state_lp
            })
            .collect();
        let order = 16;
        let mut plain = [0f32; MAX_SHAPE_LPC_ORDER + 1];
        autocorrelation(&mut plain[..order + 1], &input);
        let mut warped = [0f32; MAX_SHAPE_LPC_ORDER + 1];
        warped_autocorrelation(&mut warped, &input, 0.02, order);
        let plain_sum: f32 = plain[1..=order].iter().map(|v| v.abs()).sum();
        let warped_sum: f32 = warped[1..=order].iter().map(|v| v.abs()).sum();
        assert!(
            warped_sum < plain_sum,
            "warping should flatten: {warped_sum} vs {plain_sum}"
        );
    }

    #[test]
    fn limit_coefs_and_warped_true2monic_bound_the_magnitude() {
        let order = SHAPING_LPC_ORDER;
        let mut coefs = [0f32; MAX_SHAPE_LPC_ORDER];
        for (i, c) in coefs.iter_mut().take(order).enumerate() {
            *c = 10.0 - i as f32;
        }
        limit_coefs(&mut coefs, COEF_LIMIT, order);
        let (maxabs, _) = max_abs_index(&coefs, order);
        assert!(maxabs <= COEF_LIMIT + 1e-3, "maxabs {maxabs}");

        let mut coefs = [0f32; MAX_SHAPE_LPC_ORDER];
        for (i, c) in coefs.iter_mut().take(order).enumerate() {
            *c = 6.0 - i as f32 * 0.3;
        }
        warped_true2monic_coefs(&mut coefs, 0.01, COEF_LIMIT, order);
        let (maxabs, _) = max_abs_index(&coefs, order);
        assert!(maxabs <= COEF_LIMIT + 1e-3, "maxabs {maxabs}");
    }
}
