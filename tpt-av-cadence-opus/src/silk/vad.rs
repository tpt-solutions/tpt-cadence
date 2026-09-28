//! SILK voice-activity detection (`silk/VAD.c`): the 4-band analysis
//! filterbank, per-band noise-level estimation, speech-activity
//! (`speech_activity_Q8`), frequency tilt (`input_tilt_Q15`), and the
//! per-band `input_quality_bands_Q15` — the inputs the noise-shaping
//! analysis previously held at stand-in values.
//!
//! SOURCE: Xiph.Org libopus 1.5.2, `silk/VAD.c`, `silk/ana_filt_bank_1.c`
//! (`silk_ana_filt_bank_1`), `silk/sigm_Q15.c`, `silk/lin2log.c`
//! (`silk_lin2log`), `silk/Inlines.h` (`silk_CLZ_FRAC`,
//! `silk_SQRT_APPROX`), `silk/define.h` (BSD-3-Clause).
#![allow(dead_code)]

use crate::silk::sigproc::{rshift_round, sat16, smlabb, smlawb, smulbb, smulwb, smulww};

/// `VAD_N_BANDS` (`silk/define.h`).
pub(crate) const VAD_N_BANDS: usize = 4;
/// `VAD_INTERNAL_SUBFRAMES_LOG2` / `VAD_INTERNAL_SUBFRAMES`.
const VAD_INTERNAL_SUBFRAMES: usize = 4;
/// `VAD_NOISE_LEVEL_SMOOTH_COEF_Q16` (must be < 4096).
const VAD_NOISE_LEVEL_SMOOTH_COEF_Q16: i32 = 1024;
/// `VAD_NOISE_LEVELS_BIAS`.
const VAD_NOISE_LEVELS_BIAS: i32 = 50;
/// `VAD_NEGATIVE_OFFSET_Q5`.
const VAD_NEGATIVE_OFFSET_Q5: i32 = 128;
/// `VAD_SNR_FACTOR_Q16`.
const VAD_SNR_FACTOR_Q16: i32 = 45000;
/// `VAD_SNR_SMOOTH_COEF_Q18`.
const VAD_SNR_SMOOTH_COEF_Q18: i32 = 4096;

/// `silk_VAD_state` (`silk/structs.h`), all-zero after
/// `silk_VAD_Init`'s memset plus its noise-level initialization.
#[derive(Clone, Debug)]
pub(crate) struct VadState {
    ana_state: [i32; 2],
    ana_state1: [i32; 2],
    ana_state2: [i32; 2],
    xnrg_subfr: [i32; VAD_N_BANDS],
    nrg_ratio_smth_q8: [i32; VAD_N_BANDS],
    hp_state: i16,
    nl: [i32; VAD_N_BANDS],
    inv_nl: [i32; VAD_N_BANDS],
    noise_level_bias: [i32; VAD_N_BANDS],
    counter: i32,
}

impl Default for VadState {
    /// `silk_VAD_Init`.
    fn default() -> Self {
        let mut noise_level_bias = [0i32; VAD_N_BANDS];
        let mut nl = [0i32; VAD_N_BANDS];
        let mut inv_nl = [0i32; VAD_N_BANDS];
        for b in 0..VAD_N_BANDS {
            noise_level_bias[b] = (VAD_NOISE_LEVELS_BIAS / (b as i32 + 1)).max(1);
            nl[b] = 100 * noise_level_bias[b];
            inv_nl[b] = i32::MAX / nl[b];
        }
        let mut nrg_ratio_smth_q8 = [0i32; VAD_N_BANDS];
        for v in nrg_ratio_smth_q8.iter_mut() {
            *v = 100 * 256; // 20 dB SNR
        }
        VadState {
            ana_state: [0; 2],
            ana_state1: [0; 2],
            ana_state2: [0; 2],
            xnrg_subfr: [0; VAD_N_BANDS],
            nrg_ratio_smth_q8,
            hp_state: 0,
            nl,
            inv_nl,
            noise_level_bias,
            counter: 15,
        }
    }
}

/// One VAD evaluation's outputs (the `psEncC` fields the reference
/// writes).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct VadOutput {
    /// `speech_activity_Q8`.
    pub speech_activity_q8: i32,
    /// `input_tilt_Q15`.
    pub input_tilt_q15: i32,
    /// `input_quality_bands_Q15`.
    pub input_quality_bands_q15: [i32; VAD_N_BANDS],
}

/// `silk_sigm_Q15` (`silk/sigm_Q15.c`): sigmoid via LUT interpolation,
/// input Q5, output Q15.
fn sigm_q15(mut in_q5: i32) -> i32 {
    // fprintf(1, '%d, ', round(1024 * ([1 ./ (1 + exp(-(1:5))), 1] - 1 ./ (1 + exp(-(0:5))))));
    const SIGM_LUT_SLOPE_Q10: [i32; 6] = [237, 153, 73, 30, 12, 7];
    // fprintf(1, '%d, ', round(32767 * 1 ./ (1 + exp(-(0:5)))));
    const SIGM_LUT_POS_Q15: [i32; 6] = [16384, 23955, 28861, 31213, 32178, 32548];
    // fprintf(1, '%d, ', round(32767 * 1 ./ (1 + exp((0:5)))));
    const SIGM_LUT_NEG_Q15: [i32; 6] = [16384, 8812, 3906, 1554, 589, 219];

    if in_q5 < 0 {
        in_q5 = -in_q5;
        if in_q5 >= 6 * 32 {
            return 0;
        }
        let ind = in_q5 >> 5;
        return SIGM_LUT_NEG_Q15[ind as usize]
            - smulbb(SIGM_LUT_SLOPE_Q10[ind as usize], in_q5 & 0x1F);
    }
    if in_q5 >= 6 * 32 {
        return 32767;
    }
    let ind = in_q5 >> 5;
    SIGM_LUT_POS_Q15[ind as usize] + smulbb(SIGM_LUT_SLOPE_Q10[ind as usize], in_q5 & 0x1F)
}

/// `silk_CLZ_FRAC` (`silk/Inlines.h`): leading-zero count and the 7 bits
/// after the leading one.
fn clz_frac(input: i32) -> (i32, i32) {
    let lzeros = input.leading_zeros() as i32;
    // silk_ROR32(in, 24 - lzeros) & 0x7f
    let frac_q7 = input.rotate_right((24 - lzeros) as u32) & 0x7f;
    (lzeros, frac_q7)
}

/// `silk_SQRT_APPROX` (`silk/Inlines.h`): approximate square root.
fn sqrt_approx(x: i32) -> i32 {
    if x <= 0 {
        return 0;
    }
    let (lz, frac_q7) = clz_frac(x);
    let mut y: i32 = if lz & 1 != 0 { 32768 } else { 46214 }; // sqrt(2)*32768

    // Get scaling right.
    y >>= (lz as u32) >> 1;

    // Increment using the fractional part of the input.
    y = smlawb(y, y, smulbb(213, frac_q7));

    y
}

/// `silk_lin2log` (`silk/lin2log.c`): approximation of 128·log2().
fn lin2log(in_lin: i32) -> i32 {
    let (lz, frac_q7) = clz_frac(in_lin);
    // Piece-wise parabolic approximation.
    (smlawb(frac_q7, frac_q7 * 128 - frac_q7, 179)).wrapping_add((31 - lz) << 7)
}

/// `silk_ana_filt_bank_1` (`silk/ana_filt_bank_1.c`): split a signal into
/// two decimated bands using first-order allpass filters. `state` is the
/// 2-entry filter state (Q10 internally).
fn ana_filt_bank_1(input: &[i16], state: &mut [i32; 2], out_l: &mut [i16], out_h: &mut [i16]) {
    // A_fb1_20 = 5394 << 1; A_fb1_21 = -24290 (20623 << 1 as i16).
    const A_FB1_20: i32 = 5394 << 1;
    const A_FB1_21: i32 = -24290;

    let n2 = input.len() / 2;
    for k in 0..n2 {
        // Even input sample: all-pass section.
        let in32 = (input[2 * k] as i32) << 10;
        let y = in32.wrapping_sub(state[0]);
        let x = smlawb(y, y, A_FB1_21);
        let out_1 = state[0].wrapping_add(x);
        state[0] = in32.wrapping_add(x);

        // Odd input sample, added to the previous section's output.
        let in32 = (input[2 * k + 1] as i32) << 10;
        let y = in32.wrapping_sub(state[1]);
        let x = smulwb(y, A_FB1_20);
        let out_2 = state[1].wrapping_add(x);
        state[1] = in32.wrapping_add(x);

        out_l[k] = sat16(rshift_round(out_2.wrapping_add(out_1), 11));
        out_h[k] = sat16(rshift_round(out_2.wrapping_sub(out_1), 11));
    }
}

/// `silk_VAD_GetNoiseLevels` (`silk/VAD.c`): smooth the per-band noise
/// levels toward the current subband energies.
fn vad_get_noise_levels(xnrg: &[i32; VAD_N_BANDS], state: &mut VadState) {
    // Initially faster smoothing (1000 frames = 20 s).
    let min_coef = if state.counter < 1000 {
        let c = i16::MAX as i32 / ((state.counter >> 4) + 1);
        state.counter += 1;
        c
    } else {
        0
    };

    #[allow(clippy::needless_range_loop)]
    for k in 0..VAD_N_BANDS {
        let (nl, bias) = (state.nl[k], state.noise_level_bias[k]);
        let nrg = (xnrg[k] + bias).max(0).max(1);
        let inv_nrg = i32::MAX / nrg;

        // Less update when the subband energy is high.
        let coef = if nrg > (nl << 3) {
            VAD_NOISE_LEVEL_SMOOTH_COEF_Q16 >> 3
        } else if nrg < nl {
            VAD_NOISE_LEVEL_SMOOTH_COEF_Q16
        } else {
            smulwb(smulww(inv_nrg, nl), VAD_NOISE_LEVEL_SMOOTH_COEF_Q16 << 1)
        };
        let coef = coef.max(min_coef);

        // Smooth inverse energies.
        state.inv_nl[k] = smlawb(state.inv_nl[k], inv_nrg.wrapping_sub(state.inv_nl[k]), coef);

        // Compute the noise level by inverting again.
        let nl_new = i32::MAX / state.inv_nl[k];
        // Limit noise levels (guarantee 7 bits of head room).
        state.nl[k] = nl_new.min(0x00FF_FFFF);
    }
}

/// `silk_VAD_GetSA_Q8` (`silk/VAD.c`): evaluate one frame's speech
/// activity and band qualities. `frame` must be the internal-rate frame
/// (length = frame_length; the reference asserts ≤ 512 and a multiple of
/// 8 — both hold at 8/12/16 kHz for 10/20 ms frames).
impl VadState {
    pub(crate) fn vad_get_sa_q8(
        &mut self,
        frame: &[i16],
        frame_length: usize,
        fs_khz: u32,
    ) -> VadOutput {
        debug_assert_eq!(frame_length, 8 * (frame_length >> 3));
        debug_assert!(frame_length <= 512);

        /***********************/
        /* Filter and decimate */
        /***********************/
        let decimated_framelength1 = frame_length >> 1;
        let decimated_framelength2 = frame_length >> 2;
        let decimated_framelength = frame_length >> 3;
        let x_offset = [
            0usize,
            decimated_framelength + decimated_framelength2,
            decimated_framelength + decimated_framelength2 + decimated_framelength,
            decimated_framelength
                + decimated_framelength2
                + decimated_framelength
                + decimated_framelength2,
        ];
        let total = x_offset[3] + decimated_framelength1;
        let mut x = vec![0i16; total];

        // 0-8 kHz → 0-4 kHz and 4-8 kHz.
        {
            let (low, high) = x.split_at_mut(x_offset[3]);
            ana_filt_bank_1(frame, &mut self.ana_state, low, high);
        }
        // 0-4 kHz → 0-2 kHz and 2-4 kHz. The reference filters in place:
        // the output regions precede the input regions in x, so stage the
        // inputs into locals to satisfy the borrow checker without changing
        // the values read.
        let src1: Vec<i16> = x[..decimated_framelength1].to_vec();
        {
            let (low, high) = x.split_at_mut(x_offset[2]);
            ana_filt_bank_1(&src1, &mut self.ana_state1, low, high);
        }
        let src2: Vec<i16> = x[..decimated_framelength2].to_vec();
        {
            let (low, high) = x.split_at_mut(x_offset[1]);
            ana_filt_bank_1(&src2, &mut self.ana_state2, low, high);
        }

        /*********************************************/
        /* HP filter on lowest band (differentiator) */
        /*********************************************/
        x[decimated_framelength - 1] >>= 1;
        let hp_state_tmp = x[decimated_framelength - 1];
        for i in (1..decimated_framelength).rev() {
            x[i - 1] >>= 1;
            x[i] -= x[i - 1];
        }
        x[0] -= self.hp_state;
        self.hp_state = hp_state_tmp;

        /*************************************/
        /* Calculate the energy in each band */
        /*************************************/
        let mut xnrg = [0i32; VAD_N_BANDS];
        for b in 0..VAD_N_BANDS {
            let dec_len = frame_length >> (VAD_N_BANDS - b).min(VAD_N_BANDS - 1);
            let dec_subframe_length = dec_len >> 2; // VAD_INTERNAL_SUBFRAMES_LOG2
            let mut dec_subframe_offset = 0usize;

            // Initialize with the last subframe's summed energy.
            xnrg[b] = self.xnrg_subfr[b];
            let mut sum_squared = 0i32;
            for s in 0..VAD_INTERNAL_SUBFRAMES {
                sum_squared = 0;
                for i in 0..dec_subframe_length {
                    let x_tmp = (x[x_offset[b] + i + dec_subframe_offset] >> 3) as i32;
                    sum_squared = smlabb(sum_squared, x_tmp, x_tmp);
                }

                // Add/saturate; the last subframe is look-ahead (half weight).
                if s < VAD_INTERNAL_SUBFRAMES - 1 {
                    xnrg[b] = add_pos_sat32(xnrg[b], sum_squared);
                } else {
                    xnrg[b] = add_pos_sat32(xnrg[b], sum_squared >> 1);
                }

                dec_subframe_offset += dec_subframe_length;
            }
            self.xnrg_subfr[b] = sum_squared;
        }

        /********************/
        /* Noise estimation */
        /********************/
        vad_get_noise_levels(&xnrg, self);

        /***********************************************/
        /* Signal-plus-noise to noise ratio estimation */
        /***********************************************/
        let mut sum_squared = 0i32;
        let mut input_tilt = 0i32;
        let mut nrg_to_noise_ratio_q8 = [256i32; VAD_N_BANDS];
        for b in 0..VAD_N_BANDS {
            let speech_nrg = xnrg[b] - self.nl[b];
            if speech_nrg > 0 {
                if (xnrg[b] as u32) & 0xFF80_0000 == 0 {
                    nrg_to_noise_ratio_q8[b] = (xnrg[b] << 8) / (self.nl[b] + 1);
                } else {
                    nrg_to_noise_ratio_q8[b] = xnrg[b] / ((self.nl[b] >> 8) + 1);
                }

                // Convert to the log domain.
                let mut snr_q7 = lin2log(nrg_to_noise_ratio_q8[b]) - 8 * 128;

                // Sum of squares (Q14).
                sum_squared = smlabb(sum_squared, snr_q7, snr_q7);

                // Tilt measure; scale down for small subband speech energies.
                if speech_nrg < (1 << 20) {
                    snr_q7 = smulwb(sqrt_approx(speech_nrg) << 6, snr_q7);
                }
                input_tilt = smlawb(input_tilt, TILT_WEIGHTS[b], snr_q7);
            } else {
                nrg_to_noise_ratio_q8[b] = 256;
            }
        }

        // Mean of squares (Q14), RMS to dBs.
        let sum_squared = sum_squared / VAD_N_BANDS as i32;
        let p_snr_db_q7 = 3 * sqrt_approx(sum_squared);

        /*********************************/
        /* Speech probability estimation */
        /*********************************/
        let mut sa_q15 = sigm_q15(smulwb(VAD_SNR_FACTOR_Q16, p_snr_db_q7) - VAD_NEGATIVE_OFFSET_Q5);

        /**************************/
        /* Frequency tilt measure */
        /**************************/
        let input_tilt_q15 = (sigm_q15(input_tilt) - 16384) << 1;

        /**************************************************/
        /* Scale the sigmoid output based on power levels */
        /**************************************************/
        let mut speech_nrg: i32 = 0;
        for (b, x) in xnrg.iter().enumerate() {
            // Higher frequency bands have more weight.
            speech_nrg += ((b + 1) as i32) * ((x - self.nl[b]) >> 4);
        }

        if frame_length == 20 * fs_khz as usize {
            speech_nrg = speech_nrg.wrapping_shr(1);
        }
        // Power scaling.
        if speech_nrg <= 0 {
            sa_q15 >>= 1;
        } else if speech_nrg < 16384 {
            let speech_nrg = sqrt_approx(speech_nrg << 16);
            sa_q15 = smulwb(32768 + speech_nrg, sa_q15);
        }

        let speech_activity_q8 = (sa_q15 >> 7).min(255);

        /***********************************/
        /* Energy level and SNR estimation */
        /***********************************/
        let mut smooth_coef_q16 = smulwb(VAD_SNR_SMOOTH_COEF_Q18, smulwb(sa_q15, sa_q15));
        if frame_length == 10 * fs_khz as usize {
            smooth_coef_q16 >>= 1;
        }

        let mut input_quality_bands_q15 = [0i32; VAD_N_BANDS];
        for b in 0..VAD_N_BANDS {
            // Smoothed energy-to-noise ratio per band.
            self.nrg_ratio_smth_q8[b] = smlawb(
                self.nrg_ratio_smth_q8[b],
                nrg_to_noise_ratio_q8[b] - self.nrg_ratio_smth_q8[b],
                smooth_coef_q16,
            );

            // Signal-to-noise ratio in dB per band.
            let snr_q7 = 3 * (lin2log(self.nrg_ratio_smth_q8[b]) - 8 * 128);
            // quality = sigmoid(0.25 * (SNR_dB - 16)).
            input_quality_bands_q15[b] = sigm_q15((snr_q7 - 16 * 128) >> 4);
        }

        VadOutput {
            speech_activity_q8,
            input_tilt_q15,
            input_quality_bands_q15,
        }
    }
}

/// `silk_ADD_POS_SAT32` (`SigProc_FIX.h`).
fn add_pos_sat32(a: i32, b: i32) -> i32 {
    match (a as u32).checked_add(b as u32) {
        Some(v) if v & 0x8000_0000 == 0 => v as i32,
        _ => i32::MAX,
    }
}

/// Per-band tilt weights (`silk/VAD.c`).
const TILT_WEIGHTS: [i32; VAD_N_BANDS] = [30000, 6000, -12000, -12000];
