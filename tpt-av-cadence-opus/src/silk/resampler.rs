//! SILK resampler — internal-rate (8/12/16 kHz) to output-rate conversion.
//!
//! Ports `silk/resampler*.c` from libopus 1.5.2 verbatim, including the
//! fixed-point arithmetic, so its output is bit-exact with the reference
//! decoder's resampling stage. SILK decodes at 8/12/16 kHz internally;
//! this module converts those samples to the API/output rate (8/12/16/24/48
//! kHz), and — in encoder mode, as in the reference — the reverse direction,
//! with the total group delay equalized across rate pairs by the reference's
//! delay matrices.
//!
//! Matrix of resampling methods used (from `silk/resampler.c`):
//!
//! ```text
//!                                Fs_out (kHz)
//!                       8      12     16     24     48
//!              8        C      UF     U      UF     UF
//! Fs_in       12        AF     C      UF     U      UF
//! (kHz)       16        D      AF     C      UF     UF
//! ```
//!
//! where `C` = copy, `D` = allpass 2x downsample ([`down2`]),
//! `U` = allpass 2x upsample ([`up2_hq`]), `UF` = `U` followed by FIR
//! interpolation (the IIR/FIR path), and `AF` = AR2 filter followed by
//! FIR interpolation (the down-FIR path).
//!
//! Structure (mirroring the reference):
//! * [`Resampler::new`] — `silk_resampler_init`: validates the rate pair,
//!   picks the delay-compensation value and the kernel.
//! * [`Resampler::resample`] — `silk_resampler`: streams through the
//!   per-call delay buffer and the selected kernel in at most 10 ms
//!   internal batches. State persists across calls, so any chunking (of
//!   whole milliseconds) yields bit-identical output to one big call.
//! * [`ar2`], [`up2_hq`], [`down2`], [`down2_3`] — the kernel building
//!   blocks; `down2`/`down2_3` are also used by SILK's encoder-side pitch
//!   analysis.
//!
//! The state is a faithful port of `silk_resampler_state_struct`
//! (`resampler_structs.h`). The reference stores the FIR history in a
//! union (`sFIR.i32` for the downsampler, `sFIR.i16` for the upsampler);
//! here the two arms are separate fields, only one of which is live for a
//! given rate pair.
//!
//! Contract: [`Resampler::resample`] never allocates, locks, or panics.
//! The input must be a whole number of milliseconds (a multiple of
//! `Fs_in/1000` samples, at least 1 ms) and the output slice exactly
//! `input.len() * Fs_out / Fs_in` samples — every rate pair the reference
//! supports divides evenly under this contract.
//!
//! SOURCE: Xiph.Org libopus 1.5.2, `silk/resampler.c`,
//! `silk/resampler_structs.h`, `silk/resampler_private.h`,
//! `silk/resampler_private_IIR_FIR.c`, `silk/resampler_private_down_FIR.c`,
//! `silk/resampler_private_up2_HQ.c`, `silk/resampler_private_AR2.c`,
//! `silk/resampler_down2.c`, `silk/resampler_down2_3.c`,
//! `silk/resampler_rom.c`, `silk/resampler_rom.h` (BSD-3-Clause).
#![allow(dead_code)]

use crate::silk::sigproc::{rshift_round, sat16, smlabb, smlawb, smulbb, smulwb};
use crate::{CadenceError, Result};

// ---- silk/resampler_private.h / resampler_structs.h constants ----

/// `RESAMPLER_MAX_BATCH_SIZE_MS`: input samples are processed in at most
/// 10 ms batches.
const RESAMPLER_MAX_BATCH_SIZE_MS: usize = 10;
/// `RESAMPLER_MAX_FS_KHZ`.
const RESAMPLER_MAX_FS_KHZ: usize = 48;
/// `RESAMPLER_MAX_BATCH_SIZE_IN`.
const RESAMPLER_MAX_BATCH_SIZE_IN: usize = RESAMPLER_MAX_BATCH_SIZE_MS * RESAMPLER_MAX_FS_KHZ;
/// `SILK_RESAMPLER_MAX_FIR_ORDER`.
const MAX_FIR_ORDER: usize = 36;
/// `SILK_RESAMPLER_MAX_IIR_ORDER`.
const MAX_IIR_ORDER: usize = 6;
/// `RESAMPLER_DOWN_ORDER_FIR0` (18 taps, phases of 9 coefficients).
const DOWN_ORDER_FIR0: usize = 18;
/// `RESAMPLER_DOWN_ORDER_FIR1` (24 taps, folded to 12).
const DOWN_ORDER_FIR1: usize = 24;
/// `RESAMPLER_DOWN_ORDER_FIR2` (36 taps, folded to 18).
const DOWN_ORDER_FIR2: usize = 36;
/// `RESAMPLER_ORDER_FIR_12`: the IIR/FIR upsampler's fractional FIR is
/// 8 taps (folded to 4).
const ORDER_FIR_12: usize = 8;

// ---- silk/resampler_rom.h ----

/// `silk_resampler_down2_0` (Q13 allpass coefficient for the odd input).
const RESAMPLER_DOWN2_0: i16 = 9872;
/// `silk_resampler_down2_1` (`39809 - 65536` in the reference).
const RESAMPLER_DOWN2_1: i16 = -25727;
/// `silk_resampler_up2_hq_0`: first allpass bank (even output phase);
/// the last entry is `39083 - 65536` in the reference.
const RESAMPLER_UP2_HQ_0: [i16; 3] = [1746, 14986, -26453];
/// `silk_resampler_up2_hq_1`: second allpass bank (odd output phase);
/// the last entry is `55542 - 65536` in the reference.
const RESAMPLER_UP2_HQ_1: [i16; 3] = [6854, 25769, -9994];

// ---- silk/resampler_rom.c ----

/// `silk_Resampler_3_4_COEFS`: 2 AR (Q14) + 3 phases x 9 FIR coefficients
/// for Fs_out:Fs_in = 3:4 (e.g. 16 -> 12 kHz).
static RESAMPLER_3_4_COEFS: [i16; 2 + 3 * DOWN_ORDER_FIR0 / 2] = [
    -20694, -13867, //
    -49, 64, 17, -157, 353, -496, 163, 11047, 22205, //
    -39, 6, 91, -170, 186, 23, -896, 6336, 19928, //
    -19, -36, 102, -89, -24, 328, -951, 2568, 15909,
];

/// `silk_Resampler_2_3_COEFS` for Fs_out:Fs_in = 2:3 (e.g. 12 -> 8,
/// 24 -> 16 kHz).
static RESAMPLER_2_3_COEFS: [i16; 2 + 2 * DOWN_ORDER_FIR0 / 2] = [
    -14457, -14019, //
    64, 128, -122, 36, 310, -768, 584, 9267, 17733, //
    12, 128, 18, -142, 288, -117, -865, 4123, 14459,
];

/// `silk_Resampler_1_2_COEFS` for Fs_out:Fs_in = 1:2 (e.g. 16 -> 8).
static RESAMPLER_1_2_COEFS: [i16; 2 + DOWN_ORDER_FIR1 / 2] = [
    616, -14323, //
    -10, 39, 58, -46, -84, 120, 184, -315, -541, 1284, 5380, 9024,
];

/// `silk_Resampler_1_3_COEFS` for Fs_out:Fs_in = 1:3 (e.g. 24 -> 8).
static RESAMPLER_1_3_COEFS: [i16; 2 + DOWN_ORDER_FIR2 / 2] = [
    16102, -15162, //
    -13, 0, 20, 26, 5, -31, -43, -4, 65, 90, 7, -157, -248, -44, 593, 1583, 2612, 3271,
];

/// `silk_Resampler_1_4_COEFS` for Fs_out:Fs_in = 1:4 (e.g. 48 -> 12).
static RESAMPLER_1_4_COEFS: [i16; 2 + DOWN_ORDER_FIR2 / 2] = [
    22500, -15099, //
    3, -14, -20, -15, 2, 25, 37, 25, -16, -71, -107, -79, 50, 292, 623, 982, 1288, 1464,
];

/// `silk_Resampler_1_6_COEFS` for Fs_out:Fs_in = 1:6 (e.g. 48 -> 8).
static RESAMPLER_1_6_COEFS: [i16; 2 + DOWN_ORDER_FIR2 / 2] = [
    27540, -15257, //
    17, 12, 8, 1, -10, -22, -30, -32, -22, 3, 44, 100, 168, 243, 317, 381, 429, 455,
];

/// `silk_Resampler_2_3_COEFS_LQ`: the low-quality 2/3 downsampler
/// (2 AR + 2 phases x 2 FIR coefficients), used by [`down2_3`].
static RESAMPLER_2_3_COEFS_LQ: [i16; 2 + 2 * 2] = [-2797, -6507, 4697, 10739, 1567, 8276];

/// `silk_resampler_frac_FIR_12`: interpolation fractions
/// 1/24, 3/24, ..., 23/24 of an 8-tap (folded to 4) FIR, used by the
/// IIR/FIR upsampler.
static RESAMPLER_FRAC_FIR_12: [[i16; ORDER_FIR_12 / 2]; 12] = [
    [189, -600, 617, 30567],
    [117, -159, -1070, 29704],
    [52, 221, -2392, 28276],
    [-4, 529, -3350, 26341],
    [-48, 758, -3956, 23973],
    [-80, 905, -4235, 21254],
    [-99, 972, -4222, 18278],
    [-107, 967, -3957, 15143],
    [-103, 896, -3487, 11950],
    [-91, 773, -2865, 8798],
    [-71, 611, -2143, 5784],
    [-46, 425, -1375, 2996],
];

// ---- silk/resampler.c dispatch tables ----

/// `delay_matrix_enc`: per-call input delay compensation (samples) for the
/// encoder direction, indexed by `[rate_id(Fs_in)][rate_id(Fs_out)]` with
/// rates `[8, 12, 16, 24, 48]` kHz in and `[8, 12, 16]` kHz out.
static DELAY_MATRIX_ENC: [[i8; 3]; 5] = [
    // in \ out   8   12   16
    /*  8 */ [6, 0, 3], //
    /* 12 */ [0, 7, 3], //
    /* 16 */ [0, 1, 10], //
    /* 24 */ [0, 2, 6], //
    /* 48 */ [18, 10, 12],
];

/// `delay_matrix_dec`: delay compensation for the decoder direction,
/// rates `[8, 12, 16]` kHz in and `[8, 12, 16, 24, 48]` kHz out.
static DELAY_MATRIX_DEC: [[i8; 5]; 3] = [
    // in \ out   8   12   16   24   48
    /*  8 */ [4, 0, 2, 0, 0], //
    /* 12 */ [0, 9, 4, 7, 4], //
    /* 16 */ [0, 3, 12, 7, 7],
];

/// `rateID` (`silk/resampler.c`): maps `[8000, 12000, 16000, 24000, 48000]`
/// Hz to `[0, 1, 2, 3, 4]`. Only valid rates reach this (validated by
/// [`Resampler::new`] first), exactly as in the reference where the macro
/// is only used after the input checks.
fn rate_id(fs_hz: i32) -> usize {
    ((((fs_hz >> 12) - i32::from(fs_hz > 16000)) >> i32::from(fs_hz > 24000)) - 1) as usize
}

/// `USE_silk_resampler_*`: the selected kernel for a rate pair.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ResamplerFn {
    /// Input and output rates are equal: copy through the delay buffer.
    Copy,
    /// Output is exactly 2x input: [`up2_hq`] only.
    Up2Hq,
    /// Fractional upscale: allpass 2x upsample then fractional FIR
    /// interpolation.
    IirFir,
    /// Downscale: AR2 prefilter then fractional FIR interpolation.
    DownFir,
}

/// SILK resampler state — mirrors `silk_resampler_state_struct`
/// (`silk/resampler_structs.h`) plus the configuration
/// `silk_resampler_init` computes.
pub(crate) struct Resampler {
    /// `sIIR`: shared IIR/AR state. Used as the 6-element Q10 allpass
    /// state by [`up2_hq`] and as the 2-element AR2 state by [`ar2`].
    s_iir: [i32; MAX_IIR_ORDER],
    /// `sFIR.i32` arm: FIR history of the down-FIR path (first
    /// `fir_order` entries live).
    s_fir_i32: [i32; MAX_FIR_ORDER],
    /// `sFIR.i16` arm: 8-sample upsampled-FIR history of the IIR/FIR path.
    s_fir_i16: [i16; ORDER_FIR_12],
    /// `delayBuf`: per-call delay line of `input_delay` samples.
    delay_buf: [i16; RESAMPLER_MAX_FS_KHZ],
    /// `resampler_function`.
    resampler_function: ResamplerFn,
    /// `batchSize`: input samples per internal batch (10 ms worth).
    batch_size: usize,
    /// `invRatio_Q16`: ceil of (input rate * 2^(16 + up2x)) / output rate.
    inv_ratio_q16: i32,
    /// `FIR_Order` (down-FIR path only).
    fir_order: usize,
    /// `FIR_Fracs` (down-FIR path only): number of FIR phases.
    fir_fracs: i32,
    /// `Fs_in_kHz`.
    fs_in_khz: usize,
    /// `Fs_out_kHz`.
    fs_out_khz: usize,
    /// `inputDelay`: delay-compensation samples carried between calls.
    input_delay: usize,
    /// `Coefs`: the down-FIR path's [2 AR (Q14) | folded FIR phases]
    /// coefficient table.
    coefs: &'static [i16],
}

impl Resampler {
    /// `silk_resampler_init`: initialize/reset the resampler state for a
    /// given pair of input/output sampling rates (Hz). `for_enc` mirrors
    /// the reference's `forEnc` argument: the decoder direction allows
    /// 8/12/16 kHz input and 8/12/16/24/48 kHz output; the encoder
    /// direction the transpose.
    ///
    /// Returns an error for rate pairs the reference does not support
    /// (the reference asserts and returns -1).
    pub(crate) fn new(fs_hz_in: i32, fs_hz_out: i32, for_enc: bool) -> Result<Self> {
        // Input checking (silk_resampler_init's forEnc branch). The rate
        // must be validated before indexing the delay matrices.
        let valid = |rates: &[i32], fs: i32| rates.contains(&fs);
        const ENC_IN: [i32; 5] = [8000, 12000, 16000, 24000, 48000];
        const ENC_OUT: [i32; 3] = [8000, 12000, 16000];
        const DEC_IN: [i32; 3] = [8000, 12000, 16000];
        const DEC_OUT: [i32; 5] = [8000, 12000, 16000, 24000, 48000];
        let (in_ok, out_ok) = if for_enc {
            (valid(&ENC_IN, fs_hz_in), valid(&ENC_OUT, fs_hz_out))
        } else {
            (valid(&DEC_IN, fs_hz_in), valid(&DEC_OUT, fs_hz_out))
        };
        if !in_ok || !out_ok {
            return Err(CadenceError::UnsupportedFeature(format!(
                "SILK resampler rate pair {fs_hz_in} -> {fs_hz_out} Hz (for_enc: {for_enc}) not supported"
            )));
        }
        let input_delay = if for_enc {
            DELAY_MATRIX_ENC[rate_id(fs_hz_in)][rate_id(fs_hz_out)] as usize
        } else {
            DELAY_MATRIX_DEC[rate_id(fs_hz_in)][rate_id(fs_hz_out)] as usize
        };

        let fs_in_khz = (fs_hz_in / 1000) as usize;
        let fs_out_khz = (fs_hz_out / 1000) as usize;

        // Find resampler with the right sampling ratio.
        let mut resampler_function = ResamplerFn::Copy;
        let mut up2x = 0;
        let mut fir_order = 0;
        let mut fir_fracs = 0;
        let mut coefs: &'static [i16] = &[];
        if fs_hz_out > fs_hz_in {
            if fs_hz_out == fs_hz_in * 2 {
                // Fs_out : Fs_in = 2 : 1: directly use the 2x upsampler.
                resampler_function = ResamplerFn::Up2Hq;
            } else {
                resampler_function = ResamplerFn::IirFir;
                up2x = 1;
            }
        } else if fs_hz_out < fs_hz_in {
            resampler_function = ResamplerFn::DownFir;
            if fs_hz_out * 4 == fs_hz_in * 3 {
                fir_fracs = 3;
                fir_order = DOWN_ORDER_FIR0;
                coefs = &RESAMPLER_3_4_COEFS;
            } else if fs_hz_out * 3 == fs_hz_in * 2 {
                fir_fracs = 2;
                fir_order = DOWN_ORDER_FIR0;
                coefs = &RESAMPLER_2_3_COEFS;
            } else if fs_hz_out * 2 == fs_hz_in {
                fir_fracs = 1;
                fir_order = DOWN_ORDER_FIR1;
                coefs = &RESAMPLER_1_2_COEFS;
            } else if fs_hz_out * 3 == fs_hz_in {
                fir_fracs = 1;
                fir_order = DOWN_ORDER_FIR2;
                coefs = &RESAMPLER_1_3_COEFS;
            } else if fs_hz_out * 4 == fs_hz_in {
                fir_fracs = 1;
                fir_order = DOWN_ORDER_FIR2;
                coefs = &RESAMPLER_1_4_COEFS;
            } else if fs_hz_out * 6 == fs_hz_in {
                fir_fracs = 1;
                fir_order = DOWN_ORDER_FIR2;
                coefs = &RESAMPLER_1_6_COEFS;
            } else {
                // None available (unreachable for the validated pairs).
                return Err(CadenceError::UnsupportedFeature(format!(
                    "SILK resampler rate pair {fs_hz_in} -> {fs_hz_out} Hz has no downsampling filter"
                )));
            }
        }

        // Ratio of input/output samples in Q16, rounded up.
        let mut inv_ratio_q16 = ((fs_hz_in << (14 + up2x)) / fs_hz_out) << 2;
        while smulww(inv_ratio_q16, fs_hz_out) < (fs_hz_in << up2x) {
            inv_ratio_q16 += 1;
        }

        Ok(Resampler {
            s_iir: [0; MAX_IIR_ORDER],
            s_fir_i32: [0; MAX_FIR_ORDER],
            s_fir_i16: [0; ORDER_FIR_12],
            delay_buf: [0; RESAMPLER_MAX_FS_KHZ],
            resampler_function,
            batch_size: fs_in_khz * RESAMPLER_MAX_BATCH_SIZE_MS,
            inv_ratio_q16,
            fir_order,
            fir_fracs,
            fs_in_khz,
            fs_out_khz,
            input_delay,
            coefs,
        })
    }

    /// `silk_resampler_get_delay` equivalent: the per-call delay carried
    /// between calls, in input-rate samples (0..=Fs_in/1000).
    pub(crate) fn input_delay(&self) -> usize {
        self.input_delay
    }

    /// `silk_resampler`: convert `input` from `Fs_in` to `Fs_out`, writing
    /// exactly `input.len() * Fs_out / Fs_in` samples to `out`.
    ///
    /// `input` must be a whole number of milliseconds (a multiple of
    /// `Fs_in/1000` samples) and at least one millisecond. The state
    /// carries across calls: any whole-millisecond chunking produces
    /// bit-identical output to a single monolithic call.
    pub(crate) fn resample(&mut self, out: &mut [i16], input: &[i16]) -> Result<()> {
        let fs_in = self.fs_in_khz;
        let fs_out = self.fs_out_khz;
        if input.len() < fs_in {
            return Err(CadenceError::BufferTooSmall {
                needed: fs_in,
                provided: input.len(),
            });
        }
        if input.len() % fs_in != 0 {
            return Err(CadenceError::UnsupportedFeature(
                "SILK resampler input length must be a whole number of milliseconds".to_string(),
            ));
        }
        let expected_out = input.len() / fs_in * fs_out;
        if out.len() != expected_out {
            return Err(CadenceError::BufferTooSmall {
                needed: expected_out,
                provided: out.len(),
            });
        }

        // Copy the front of the input onto the retained delay tail, so the
        // first `fs_in` samples resampled below start `input_delay` samples
        // earlier in the stream. The first (one-millisecond) batch is
        // resampled from the delay buffer; the kernels then continue from
        // `input[n_samples..]` but stop `input_delay` samples short of the
        // end — those are retained for the next call instead.
        let n_samples = fs_in - self.input_delay;
        self.delay_buf[self.input_delay..fs_in].copy_from_slice(&input[..n_samples]);
        // Local copy so the kernels can take `&mut self` while reading it.
        let delay_head: [i16; RESAMPLER_MAX_FS_KHZ] = self.delay_buf;
        let in_rest = &input[n_samples..input.len() - self.input_delay];

        match self.resampler_function {
            ResamplerFn::Up2Hq => {
                let (out_head, out_rest) = out.split_at_mut(fs_out);
                up2_hq(&mut self.s_iir, out_head, &delay_head[..fs_in]);
                up2_hq(&mut self.s_iir, out_rest, in_rest);
            }
            ResamplerFn::IirFir => {
                let (out_head, out_rest) = out.split_at_mut(fs_out);
                self.iir_fir(out_head, &delay_head[..fs_in]);
                self.iir_fir(out_rest, in_rest);
            }
            ResamplerFn::DownFir => {
                let (out_head, out_rest) = out.split_at_mut(fs_out);
                self.down_fir(out_head, &delay_head[..fs_in]);
                self.down_fir(out_rest, in_rest);
            }
            ResamplerFn::Copy => {
                out[..fs_out].copy_from_slice(&delay_head[..fs_in]);
                out[fs_out..].copy_from_slice(in_rest);
            }
        }

        // Retain the tail of the input for the next call.
        self.delay_buf[..self.input_delay]
            .copy_from_slice(&input[input.len() - self.input_delay..]);

        Ok(())
    }

    /// `silk_resampler_private_IIR_FIR`: allpass 2x upsample then
    /// interpolate with the 12th-order fractional FIR. Processes at most
    /// `batch_size` input samples per internal batch.
    fn iir_fir(&mut self, mut out: &mut [i16], input: &[i16]) {
        let mut buf = [0i16; 2 * RESAMPLER_MAX_BATCH_SIZE_IN + ORDER_FIR_12];

        // Copy buffered samples to start of buffer.
        buf[..ORDER_FIR_12].copy_from_slice(&self.s_fir_i16);

        let index_increment_q16 = self.inv_ratio_q16;
        let batch_size = self.batch_size;
        let mut in_pos = 0;
        let mut in_len = input.len();
        let mut n_samples_in;
        loop {
            n_samples_in = in_len.min(batch_size);

            // Upsample 2x into the rest of the buffer.
            up2_hq(
                &mut self.s_iir,
                &mut buf[ORDER_FIR_12..ORDER_FIR_12 + 2 * n_samples_in],
                &input[in_pos..in_pos + n_samples_in],
            );

            // +1 shift because of the 2x upsampling.
            let max_index_q16 = (n_samples_in as i32) << 17;
            let produced = iir_fir_interpol(out, &buf, max_index_q16, index_increment_q16);
            out = &mut out[produced..];

            in_pos += n_samples_in;
            in_len -= n_samples_in;

            if in_len > 0 {
                // More iterations to do; copy the last part of the
                // filtered signal to the beginning of the buffer.
                let tail = n_samples_in << 1;
                buf.copy_within(tail..tail + ORDER_FIR_12, 0);
            } else {
                break;
            }
        }

        // Copy the last part of the filtered signal to the state for the
        // next call.
        let tail = n_samples_in << 1;
        self.s_fir_i16
            .copy_from_slice(&buf[tail..tail + ORDER_FIR_12]);
    }

    /// `silk_resampler_private_down_FIR`: AR2 prefilter (output in Q8)
    /// then fractional FIR interpolation. Processes at most `batch_size`
    /// input samples per internal batch.
    fn down_fir(&mut self, mut out: &mut [i16], input: &[i16]) {
        let mut buf = [0i32; RESAMPLER_MAX_BATCH_SIZE_IN + MAX_FIR_ORDER];
        let fir_order = self.fir_order;

        // Copy buffered samples to start of buffer.
        buf[..fir_order].copy_from_slice(&self.s_fir_i32[..fir_order]);

        let coefs: &'static [i16] = self.coefs;
        // The first two coefficients are the AR2 (Q14) pair; the rest are
        // the folded FIR phases.
        let fir_coefs = &coefs[2..];

        let index_increment_q16 = self.inv_ratio_q16;
        let batch_size = self.batch_size;
        let fir_fracs = self.fir_fracs;
        let mut in_pos = 0;
        let mut in_len = input.len();
        let mut n_samples_in;
        loop {
            n_samples_in = in_len.min(batch_size);

            // Second-order AR filter (output in Q8).
            ar2(
                &mut self.s_iir,
                &mut buf[fir_order..fir_order + n_samples_in],
                &input[in_pos..in_pos + n_samples_in],
                coefs,
            );

            let max_index_q16 = (n_samples_in as i32) << 16;
            let produced = down_fir_interpol(
                out,
                &buf[..fir_order + n_samples_in],
                fir_coefs,
                fir_order,
                fir_fracs,
                max_index_q16,
                index_increment_q16,
            );
            out = &mut out[produced..];

            in_pos += n_samples_in;
            in_len -= n_samples_in;

            if in_len > 1 {
                // More iterations to do; copy the last part of the
                // filtered signal to the beginning of the buffer.
                buf.copy_within(n_samples_in..n_samples_in + fir_order, 0);
            } else {
                break;
            }
        }

        // Copy the last part of the filtered signal to the state for the
        // next call.
        self.s_fir_i32[..fir_order].copy_from_slice(&buf[n_samples_in..n_samples_in + fir_order]);
    }
}

/// `silk_resampler_private_IIR_FIR_INTERPOL`: interpolate the 2x-upsampled
/// signal in `buf` at Q16 positions stepping by `index_increment_q16`,
/// writing into `out` and returning the number of samples produced.
fn iir_fir_interpol(
    out: &mut [i16],
    buf: &[i16],
    max_index_q16: i32,
    index_increment_q16: i32,
) -> usize {
    let mut produced = 0;
    let mut index_q16 = 0i32;
    while index_q16 < max_index_q16 {
        let table_index = smulwb(index_q16 & 0xFFFF, 12) as usize;
        let p = (index_q16 >> 16) as usize;
        let buf_ptr = &buf[p..p + ORDER_FIR_12];

        let mut res_q15 = smulbb(
            buf_ptr[0] as i32,
            RESAMPLER_FRAC_FIR_12[table_index][0] as i32,
        );
        res_q15 = smlabb(
            res_q15,
            buf_ptr[1] as i32,
            RESAMPLER_FRAC_FIR_12[table_index][1] as i32,
        );
        res_q15 = smlabb(
            res_q15,
            buf_ptr[2] as i32,
            RESAMPLER_FRAC_FIR_12[table_index][2] as i32,
        );
        res_q15 = smlabb(
            res_q15,
            buf_ptr[3] as i32,
            RESAMPLER_FRAC_FIR_12[table_index][3] as i32,
        );
        res_q15 = smlabb(
            res_q15,
            buf_ptr[4] as i32,
            RESAMPLER_FRAC_FIR_12[11 - table_index][3] as i32,
        );
        res_q15 = smlabb(
            res_q15,
            buf_ptr[5] as i32,
            RESAMPLER_FRAC_FIR_12[11 - table_index][2] as i32,
        );
        res_q15 = smlabb(
            res_q15,
            buf_ptr[6] as i32,
            RESAMPLER_FRAC_FIR_12[11 - table_index][1] as i32,
        );
        res_q15 = smlabb(
            res_q15,
            buf_ptr[7] as i32,
            RESAMPLER_FRAC_FIR_12[11 - table_index][0] as i32,
        );
        out[produced] = sat16(rshift_round(res_q15, 15));
        produced += 1;
        index_q16 += index_increment_q16;
    }
    produced
}

/// `silk_resampler_private_down_FIR_INTERPOL`: interpolate the Q8-filtered
/// signal in `buf` at Q16 positions, writing into `out` and returning the
/// number of samples produced. `fir_coefs` points at the folded
/// coefficient phases (the leading AR2 pair is excluded).
fn down_fir_interpol(
    out: &mut [i16],
    buf: &[i32],
    fir_coefs: &[i16],
    fir_order: usize,
    fir_fracs: i32,
    max_index_q16: i32,
    index_increment_q16: i32,
) -> usize {
    let mut produced = 0;
    match fir_order {
        DOWN_ORDER_FIR0 => {
            let mut index_q16 = 0i32;
            while index_q16 < max_index_q16 {
                // Integer part gives pointer to buffered input.
                let p = (index_q16 >> 16) as usize;

                // Fractional part gives interpolation coefficients.
                let interpol_ind = smulwb(index_q16 & 0xFFFF, fir_fracs) as usize;

                // Inner product: lower half from phase `interpol_ind`,
                // upper half (reversed) from the mirrored phase.
                let lo = &fir_coefs[DOWN_ORDER_FIR0 / 2 * interpol_ind..][..DOWN_ORDER_FIR0 / 2];
                let hi = &fir_coefs
                    [DOWN_ORDER_FIR0 / 2 * (fir_fracs as usize - 1 - interpol_ind)..]
                    [..DOWN_ORDER_FIR0 / 2];
                let mut res_q6 = smulwb(buf[p], lo[0] as i32);
                for (j, &coef) in lo.iter().enumerate().skip(1) {
                    res_q6 = smlawb(res_q6, buf[p + j], coef as i32);
                }
                for (j, &coef) in hi.iter().enumerate() {
                    res_q6 = smlawb(res_q6, buf[p + DOWN_ORDER_FIR0 - 1 - j], coef as i32);
                }

                // Scale down, saturate and store in output array.
                out[produced] = sat16(rshift_round(res_q6, 6));
                produced += 1;
                index_q16 += index_increment_q16;
            }
        }
        DOWN_ORDER_FIR1 | DOWN_ORDER_FIR2 => {
            // The 24- and 36-tap filters are linear-phase: fold to half
            // length by summing symmetric taps first (the reference's
            // exact per-tap accumulation order).
            let half = fir_order / 2;
            let mut index_q16 = 0i32;
            while index_q16 < max_index_q16 {
                let p = (index_q16 >> 16) as usize;

                let mut res_q6 = smulwb(
                    buf[p].wrapping_add(buf[p + fir_order - 1]),
                    fir_coefs[0] as i32,
                );
                for i in 1..half {
                    res_q6 = smlawb(
                        res_q6,
                        buf[p + i].wrapping_add(buf[p + fir_order - 1 - i]),
                        fir_coefs[i] as i32,
                    );
                }

                // Scale down, saturate and store in output array.
                out[produced] = sat16(rshift_round(res_q6, 6));
                produced += 1;
                index_q16 += index_increment_q16;
            }
        }
        _ => unreachable!("validated FIR order"),
    }
    produced
}

/// `silk_resampler_private_AR2`: second-order AR filter with single delay
/// elements, Q14 coefficients, output in Q8. Only `s[0..2]` are touched.
fn ar2(s: &mut [i32], out_q8: &mut [i32], input: &[i16], a_q14: &[i16]) {
    for k in 0..input.len() {
        let mut out32 = s[0].wrapping_add((input[k] as i32).wrapping_shl(8));
        out_q8[k] = out32;
        out32 = out32.wrapping_shl(2);
        s[0] = smlawb(s[1], out32, a_q14[0] as i32);
        s[1] = smulwb(out32, a_q14[1] as i32);
    }
}

/// `silk_resampler_private_up2_HQ`: upsample by a factor 2, high quality —
/// two banks of three second-order allpass sections (Q10 internal), one
/// per output phase. `s` is the 6-element Q10 state; `out` receives
/// `2 * input.len()` samples.
fn up2_hq(s: &mut [i32; MAX_IIR_ORDER], out: &mut [i16], input: &[i16]) {
    for k in 0..input.len() {
        // Convert to Q10.
        let in32 = (input[k] as i32).wrapping_shl(10);

        // First allpass section for even output sample.
        let mut y = in32.wrapping_sub(s[0]);
        let mut x = smulwb(y, RESAMPLER_UP2_HQ_0[0] as i32);
        let mut out32_1 = s[0].wrapping_add(x);
        s[0] = in32.wrapping_add(x);

        // Second allpass section for even output sample.
        y = out32_1.wrapping_sub(s[1]);
        x = smulwb(y, RESAMPLER_UP2_HQ_0[1] as i32);
        let out32_2 = s[1].wrapping_add(x);
        s[1] = out32_1.wrapping_add(x);

        // Third allpass section for even output sample.
        y = out32_2.wrapping_sub(s[2]);
        x = smlawb(y, y, RESAMPLER_UP2_HQ_0[2] as i32);
        out32_1 = s[2].wrapping_add(x);
        s[2] = out32_2.wrapping_add(x);

        // Convert back to int16 and store to output.
        out[2 * k] = sat16(rshift_round(out32_1, 10));

        // First allpass section for odd output sample.
        y = in32.wrapping_sub(s[3]);
        x = smulwb(y, RESAMPLER_UP2_HQ_1[0] as i32);
        out32_1 = s[3].wrapping_add(x);
        s[3] = in32.wrapping_add(x);

        // Second allpass section for odd output sample.
        y = out32_1.wrapping_sub(s[4]);
        x = smulwb(y, RESAMPLER_UP2_HQ_1[1] as i32);
        let out32_2 = s[4].wrapping_add(x);
        s[4] = out32_1.wrapping_add(x);

        // Third allpass section for odd output sample.
        y = out32_2.wrapping_sub(s[5]);
        x = smlawb(y, y, RESAMPLER_UP2_HQ_1[2] as i32);
        out32_1 = s[5].wrapping_add(x);
        s[5] = out32_2.wrapping_add(x);

        // Convert back to int16 and store to output.
        out[2 * k + 1] = sat16(rshift_round(out32_1, 10));
    }
}

/// `silk_resampler_down2`: downsample by a factor 2 — two allpass
/// sections (Q10 internal). `s` is the 2-element state; `out` receives
/// `input.len() / 2` samples.
pub(crate) fn down2(s: &mut [i32; 2], out: &mut [i16], input: &[i16]) {
    let len2 = input.len() >> 1;
    for k in 0..len2 {
        // Convert to Q10.
        let mut in32 = (input[2 * k] as i32).wrapping_shl(10);

        // All-pass section for even input sample.
        let mut y = in32.wrapping_sub(s[0]);
        let x = smlawb(y, y, RESAMPLER_DOWN2_1 as i32);
        let mut out32 = s[0].wrapping_add(x);
        s[0] = in32.wrapping_add(x);

        // Convert to Q10.
        in32 = (input[2 * k + 1] as i32).wrapping_shl(10);

        // All-pass section for odd input sample, and add to output of the
        // previous section.
        y = in32.wrapping_sub(s[1]);
        let x = smulwb(y, RESAMPLER_DOWN2_0 as i32);
        out32 = out32.wrapping_add(s[1]);
        out32 = out32.wrapping_add(x);
        s[1] = in32.wrapping_add(x);

        // Add, convert back to int16 and store to output.
        out[k] = sat16(rshift_round(out32, 11));
    }
}

/// `silk_resampler_down2_3`: downsample by a factor 2/3, low quality —
/// AR2 prefilter then a 2-phase FIR. `s` holds the 4-element FIR history
/// followed by the 2-element AR2 state (6 entries total, as in the
/// reference); `out` receives `2 * input.len() / 3` samples for input
/// lengths that are multiples of 3. Chunk boundaries must also be
/// multiples of 3 for the output to be chunking-invariant (the reference
/// only ever calls this with 3-aligned pitch-analysis frame lengths).
pub(crate) fn down2_3(s: &mut [i32; 6], out: &mut [i16], input: &[i16]) {
    const ORDER_FIR: usize = 4;
    let mut buf = [0i32; RESAMPLER_MAX_BATCH_SIZE_IN + ORDER_FIR];

    // Copy buffered samples to start of buffer.
    buf[..ORDER_FIR].copy_from_slice(&s[..ORDER_FIR]);

    let mut in_pos = 0;
    let mut in_len = input.len();
    let mut out_pos = 0;
    let mut n_samples_in;
    loop {
        n_samples_in = in_len.min(RESAMPLER_MAX_BATCH_SIZE_IN);

        // Second-order AR filter (output in Q8).
        ar2(
            &mut s[ORDER_FIR..],
            &mut buf[ORDER_FIR..ORDER_FIR + n_samples_in],
            &input[in_pos..in_pos + n_samples_in],
            &RESAMPLER_2_3_COEFS_LQ,
        );

        // Interpolate filtered signal.
        let mut buf_ptr = 0usize;
        let mut counter = n_samples_in;
        while counter > 2 {
            // Inner product (first output phase).
            let mut res_q6 = smulwb(buf[buf_ptr], RESAMPLER_2_3_COEFS_LQ[2] as i32);
            res_q6 = smlawb(res_q6, buf[buf_ptr + 1], RESAMPLER_2_3_COEFS_LQ[3] as i32);
            res_q6 = smlawb(res_q6, buf[buf_ptr + 2], RESAMPLER_2_3_COEFS_LQ[5] as i32);
            res_q6 = smlawb(res_q6, buf[buf_ptr + 3], RESAMPLER_2_3_COEFS_LQ[4] as i32);

            // Scale down, saturate and store in output array.
            out[out_pos] = sat16(rshift_round(res_q6, 6));
            out_pos += 1;

            // Inner product (second output phase).
            res_q6 = smulwb(buf[buf_ptr + 1], RESAMPLER_2_3_COEFS_LQ[4] as i32);
            res_q6 = smlawb(res_q6, buf[buf_ptr + 2], RESAMPLER_2_3_COEFS_LQ[5] as i32);
            res_q6 = smlawb(res_q6, buf[buf_ptr + 3], RESAMPLER_2_3_COEFS_LQ[3] as i32);
            res_q6 = smlawb(res_q6, buf[buf_ptr + 4], RESAMPLER_2_3_COEFS_LQ[2] as i32);

            // Scale down, saturate and store in output array.
            out[out_pos] = sat16(rshift_round(res_q6, 6));
            out_pos += 1;

            buf_ptr += 3;
            counter -= 3;
        }

        in_pos += n_samples_in;
        in_len -= n_samples_in;

        if in_len > 0 {
            // More iterations to do; copy the last part of the filtered
            // signal to the beginning of the buffer.
            buf.copy_within(n_samples_in..n_samples_in + ORDER_FIR, 0);
        } else {
            break;
        }
    }

    // Copy the last part of the filtered signal to the state for the next
    // call.
    s[..ORDER_FIR].copy_from_slice(&buf[n_samples_in..n_samples_in + ORDER_FIR]);
}

// ---- silk/macros.h, SigProc_FIX.h (portable C semantics) ----
//
// The common helpers (`silk_SMULWB`, `silk_SMLAWB`, `silk_SMULBB`,
// `silk_SMLABB`, `silk_RSHIFT_ROUND`, `silk_SAT16`) live in
// [`crate::silk::sigproc`]; only `SMULWW`, which the ratio computation
// needs and nothing else in the crate does yet, is kept here.

/// `silk_SMULWW(a32, b32)`: `(a * b) >> 16` with a 64-bit product.
fn smulww(a: i32, b: i32) -> i32 {
    (((a as i64) * (b as i64)) >> 16) as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    /// All rate pairs (Hz) reachable in decoder mode.
    const DEC_PAIRS: [(i32, i32); 15] = [
        (8000, 8000),
        (8000, 12000),
        (8000, 16000),
        (8000, 24000),
        (8000, 48000),
        (12000, 8000),
        (12000, 12000),
        (12000, 16000),
        (12000, 24000),
        (12000, 48000),
        (16000, 8000),
        (16000, 12000),
        (16000, 16000),
        (16000, 24000),
        (16000, 48000),
    ];

    /// The encoder-only rate pairs (input rates 24/48 kHz).
    const ENC_PAIRS: [(i32, i32); 6] = [
        (24000, 8000),
        (24000, 12000),
        (24000, 16000),
        (48000, 8000),
        (48000, 12000),
        (48000, 16000),
    ];

    /// Deterministic 16-bit noise with a nonzero low bit (never 0), kept
    /// below full scale so filtering cannot clip.
    fn lcg_noise(n: usize, seed: u32) -> Vec<i16> {
        let mut x = seed;
        (0..n)
            .map(|_| {
                x = x.wrapping_mul(1664525).wrapping_add(1013904223);
                let v = (((x >> 16) as u16 as i32 & 0x3FFF) | 1) - 8192;
                v as i16
            })
            .collect()
    }

    fn sine(freq: f64, fs_hz: i32, len: usize, amp: f64) -> Vec<i16> {
        (0..len)
            .map(|n| {
                (amp * (2.0 * std::f64::consts::PI * freq * n as f64 / fs_hz as f64).sin()).round()
                    as i16
            })
            .collect()
    }

    /// Goertzel amplitude estimate of `freq` in `x` sampled at `fs_hz`.
    fn goertzel_amplitude(x: &[i16], freq: f64, fs_hz: f64) -> f64 {
        let w = 2.0 * std::f64::consts::PI * freq / fs_hz;
        let coeff = 2.0 * w.cos();
        let (mut s1, mut s2) = (0.0f64, 0.0f64);
        for &sample in x {
            let s0 = sample as f64 + coeff * s1 - s2;
            s2 = s1;
            s1 = s0;
        }
        let power = s1 * s1 + s2 * s2 - coeff * s1 * s2;
        power.max(0.0).sqrt() * 2.0 / x.len() as f64
    }

    fn resample_all(fs_in: i32, fs_out: i32, input: &[i16], for_enc: bool) -> Vec<i16> {
        let mut r = Resampler::new(fs_in, fs_out, for_enc).unwrap();
        let out_len = input.len() / (fs_in as usize / 1000) * (fs_out as usize / 1000);
        let mut out = vec![0i16; out_len];
        r.resample(&mut out, input).unwrap();
        out
    }

    // ---- tables & configuration ----

    #[test]
    fn coefficient_table_spot_checks() {
        // Selected values against resampler_rom.c / resampler_rom.h.
        assert_eq!(RESAMPLER_DOWN2_0, 9872);
        assert_eq!(RESAMPLER_DOWN2_1, -25727);
        assert_eq!(RESAMPLER_UP2_HQ_0, [1746, 14986, -26453]);
        assert_eq!(RESAMPLER_UP2_HQ_1, [6854, 25769, -9994]);
        assert_eq!(RESAMPLER_3_4_COEFS.len(), 29);
        assert_eq!(RESAMPLER_2_3_COEFS.len(), 20);
        assert_eq!(RESAMPLER_1_2_COEFS.len(), 14);
        assert_eq!(RESAMPLER_1_3_COEFS.len(), 20);
        assert_eq!(RESAMPLER_1_4_COEFS.len(), 20);
        assert_eq!(RESAMPLER_1_6_COEFS.len(), 20);
        assert_eq!(&RESAMPLER_3_4_COEFS[..2], &[-20694, -13867]);
        assert_eq!(
            RESAMPLER_3_4_COEFS[2..9],
            [-49, 64, 17, -157, 353, -496, 163]
        );
        assert_eq!(RESAMPLER_3_4_COEFS[27..], [2568, 15909]);
        assert_eq!(&RESAMPLER_1_2_COEFS[..2], &[616, -14323]);
        assert_eq!(RESAMPLER_1_2_COEFS[12..], [5380, 9024]);
        assert_eq!(&RESAMPLER_1_6_COEFS[2..8], &[17, 12, 8, 1, -10, -22]);
        assert_eq!(RESAMPLER_1_6_COEFS[19], 455);
        assert_eq!(
            RESAMPLER_2_3_COEFS_LQ,
            [-2797, -6507, 4697, 10739, 1567, 8276]
        );
        assert_eq!(RESAMPLER_FRAC_FIR_12.len(), 12);
        assert_eq!(RESAMPLER_FRAC_FIR_12[0], [189, -600, 617, 30567]);
        assert_eq!(RESAMPLER_FRAC_FIR_12[5], [-80, 905, -4235, 21254]);
        assert_eq!(RESAMPLER_FRAC_FIR_12[11], [-46, 425, -1375, 2996]);
    }

    #[test]
    fn rate_id_mapping() {
        assert_eq!(rate_id(8000), 0);
        assert_eq!(rate_id(12000), 1);
        assert_eq!(rate_id(16000), 2);
        assert_eq!(rate_id(24000), 3);
        assert_eq!(rate_id(48000), 4);
    }

    #[test]
    fn init_delay_matches_reference_matrices() {
        // delay_matrix_dec spot checks.
        let cases = [
            ((8000, 8000), 4),
            ((8000, 48000), 0),
            ((12000, 12000), 9),
            ((12000, 48000), 4),
            ((16000, 12000), 3),
            ((16000, 48000), 7),
        ];
        for &((fs_in, fs_out), delay) in &cases {
            let r = Resampler::new(fs_in, fs_out, false).unwrap();
            assert_eq!(r.input_delay(), delay, "{fs_in} -> {fs_out}");
        }
        // delay_matrix_enc spot checks.
        assert_eq!(Resampler::new(48000, 8000, true).unwrap().input_delay(), 18);
        assert_eq!(
            Resampler::new(16000, 16000, true).unwrap().input_delay(),
            10
        );
    }

    #[test]
    fn init_rejects_unsupported_rate_pairs() {
        // Decoder mode: input limited to 8/12/16 kHz.
        assert!(Resampler::new(24000, 48000, false).is_err());
        assert!(Resampler::new(11000, 48000, false).is_err());
        // Decoder mode: output limited to the 5 supported rates.
        assert!(Resampler::new(16000, 44100, false).is_err());
        assert!(Resampler::new(16000, 32000, false).is_err());
        // Encoder mode: transpose limits.
        assert!(Resampler::new(48000, 48000, true).is_err());
        assert!(Resampler::new(44100, 16000, true).is_err());
    }

    #[test]
    fn inv_ratio_q16_values() {
        // Recomputed independently: ceil(in << (16+up2x) / out) exactly.
        for &(fs_in, fs_out, up2x, want) in &[
            (8000, 48000, 1, 21846),
            (16000, 48000, 1, 43691),
            (8000, 12000, 1, 87382),
            (16000, 12000, 0, 87382),
            (16000, 8000, 0, 131072),
            (8000, 8000, 0, 65536),
            (24000, 16000, 0, 98304),
        ] {
            let r = Resampler::new(fs_in, fs_out, fs_out <= fs_in).unwrap();
            // Recompute via the reference formula for cross-checking.
            let mut q16 = ((fs_in << (14 + up2x)) / fs_out) << 2;
            while smulww(q16, fs_out) < (fs_in << up2x) {
                q16 += 1;
            }
            assert_eq!(r.inv_ratio_q16, q16, "{fs_in} -> {fs_out}");
            assert_eq!(r.inv_ratio_q16, want, "{fs_in} -> {fs_out}");
        }
    }

    // ---- output lengths for every supported pair ----

    #[test]
    fn output_length_is_exact_for_all_supported_pairs() {
        const MS: usize = 137; // deliberately not a multiple of 10 ms
        for &(fs_in, fs_out) in DEC_PAIRS.iter().chain(ENC_PAIRS.iter()) {
            let for_enc = ENC_PAIRS.contains(&(fs_in, fs_out));
            let input = lcg_noise(MS * (fs_in as usize) / 1000, 1);
            let mut r = Resampler::new(fs_in, fs_out, for_enc).unwrap();
            // Fill with a sentinel that the LCG input can never produce.
            let mut out = vec![i16::MIN; MS * (fs_out as usize) / 1000];
            r.resample(&mut out, &input).unwrap();
            assert!(
                out.iter().all(|&s| s != i16::MIN),
                "{fs_in} -> {fs_out}: underproduced output samples"
            );
        }
    }

    // ---- state continuity: chunking must not change the output ----

    #[test]
    fn chunked_resampling_is_bit_exact_vs_monolithic() {
        const MS: usize = 137;
        for &(fs_in, fs_out) in DEC_PAIRS.iter().chain(ENC_PAIRS.iter()) {
            let for_enc = ENC_PAIRS.contains(&(fs_in, fs_out));
            let input = lcg_noise(MS * (fs_in as usize) / 1000, 42);

            let want = resample_all(fs_in, fs_out, &input, for_enc);

            // 1 ms chunks.
            let ms_in = (fs_in as usize) / 1000;
            let ms_out = (fs_out as usize) / 1000;
            let mut r = Resampler::new(fs_in, fs_out, for_enc).unwrap();
            let mut got1 = Vec::with_capacity(want.len());
            for chunk in input.chunks(ms_in) {
                let mut out = vec![0i16; ms_out];
                r.resample(&mut out, chunk).unwrap();
                got1.extend_from_slice(&out);
            }
            assert_eq!(got1, want, "1 ms chunks, {fs_in} -> {fs_out}");

            // Irregular whole-millisecond chunk pattern.
            let mut r = Resampler::new(fs_in, fs_out, for_enc).unwrap();
            let mut got2 = Vec::with_capacity(want.len());
            let pattern = [7usize, 3, 5, 2, 11, 1];
            let mut pos = 0;
            let mut pat = 0;
            while pos < input.len() {
                let ms = pattern[pat % pattern.len()];
                pat += 1;
                let end = (pos + ms * ms_in).min(input.len());
                let chunk = &input[pos..end];
                let mut out = vec![0i16; chunk.len() / ms_in * ms_out];
                r.resample(&mut out, chunk).unwrap();
                got2.extend_from_slice(&out);
                pos = end;
            }
            assert_eq!(got2, want, "irregular chunks, {fs_in} -> {fs_out}");
        }
    }

    // ---- kernels: DC and step behaviour ----

    #[test]
    fn up2_hq_dc_settles_exactly() {
        let mut s = [0i32; MAX_IIR_ORDER];
        let input = vec![1000i16; 200];
        let mut out = vec![0i16; 400];
        up2_hq(&mut s, &mut out, &input);
        // Both output phases must settle to exactly the DC value.
        assert!(
            out[300..].iter().all(|&o| o == 1000),
            "out = {:?}",
            &out[300..]
        );
    }

    #[test]
    fn down2_dc_settles_exactly() {
        let mut s = [0i32; 2];
        let input = vec![2000i16; 200];
        let mut out = vec![0i16; 100];
        down2(&mut s, &mut out, &input);
        assert!(
            out[40..].iter().all(|&o| o == 2000),
            "out = {:?}",
            &out[40..]
        );
    }

    #[test]
    fn down2_3_length_and_dc() {
        let mut s = [0i32; 6];
        let input = vec![8000i16; 300];
        let mut out = vec![0i16; 200];
        down2_3(&mut s, &mut out, &input);
        // The low-quality 2/3 downsampler (pitch-analysis only) has a
        // slightly non-unity DC gain (~0.984); it must at least settle to
        // a constant near the input level.
        let steady = &out[100..];
        assert!(steady.iter().all(|&o| o == steady[0]), "out = {:?}", steady);
        assert!((steady[0] - 8000).abs() < 130, "steady = {}", steady[0]);

        // Chunked calls must match the monolithic one. (The FIR history
        // carry keeps the 2-outputs-per-3-inputs phase aligned only across
        // chunk boundaries that are multiples of 3 samples — the reference
        // only ever calls this with 3-aligned lengths too.)
        let mut s2 = [0i32; 6];
        let mut out2 = Vec::with_capacity(200);
        for chunk in input.chunks(96) {
            let mut o = vec![0i16; chunk.len() * 2 / 3];
            down2_3(&mut s2, &mut o, chunk);
            out2.extend_from_slice(&o);
        }
        assert_eq!(out2, out);
    }

    #[test]
    fn ar2_identity_and_full_scale_smoke() {
        // Zero coefficients: out_Q8 == in << 8 exactly.
        let mut s = [0i32; 2];
        let input = lcg_noise(100, 7);
        let mut out_q8 = vec![0i32; 100];
        ar2(&mut s, &mut out_q8, &input, &[0, 0]);
        assert_eq!(
            &out_q8[..],
            &input[..]
                .iter()
                .map(|&x| (x as i32) << 8)
                .collect::<Vec<_>>()
        );

        // Full-scale alternating input with real coefficients must not
        // panic or wrap into the wrong octave for these stable filters.
        let mut s = [0i32; 2];
        let input = [32767i16, -32768].repeat(500);
        let mut out_q8 = vec![0i32; 1000];
        ar2(&mut s, &mut out_q8, &input, &RESAMPLER_1_2_COEFS[..2]);
    }

    #[test]
    fn step_response_settles_at_input_level() {
        for &(fs_in, fs_out) in &[
            (8000, 48000),
            (16000, 48000),
            (16000, 8000),
            (8000, 12000),
            (12000, 8000),
            (16000, 12000),
        ] {
            let for_enc = ENC_PAIRS.contains(&(fs_in, fs_out));
            // 10 ms of silence, then 50 ms of constant 12000.
            let mut input = vec![0i16; (fs_in as usize) / 100];
            input.extend(vec![12000i16; 50 * (fs_in as usize) / 1000]);
            let out = resample_all(fs_in, fs_out, &input, for_enc);

            // The last 10 ms must have settled at (or very near) the input
            // level. Two tolerance sources, both inherent to the reference
            // filters: the fractional-FIR phases have slightly different
            // coefficient sums (a ±few-LSB periodic ripple on a constant),
            // and the DC gain is not exactly 1 on every pair (worst is the
            // 12 -> 8 pair at ~+0.6%). Require the ripple to be small and
            // the mean within 1%.
            let tail = &out[out.len() - (fs_out as usize) / 100..];
            let lo = tail.iter().min().unwrap();
            let hi = tail.iter().max().unwrap();
            assert!(hi - lo <= 4, "{fs_in} -> {fs_out}: tail ripple {lo}..{hi}");
            let mean = tail.iter().map(|&o| o as i64).sum::<i64>() as f64 / tail.len() as f64;
            assert!(
                (mean - 12000.0).abs() <= 120.0,
                "{fs_in} -> {fs_out}: step tail mean {mean}"
            );
        }
    }

    // ---- sine sweeps ----

    #[test]
    fn sine_passband_gain_across_pairs() {
        for &(fs_in, fs_out) in DEC_PAIRS.iter().chain(ENC_PAIRS.iter()) {
            let for_enc = ENC_PAIRS.contains(&(fs_in, fs_out));
            let amp = 8000.0;
            let mut freqs = [100.0, 500.0, 1000.0, 2000.0, 3000.0].to_vec();
            freqs.push(0.4 * (fs_in.min(fs_out)) as f64);
            for &freq in &freqs {
                let input = sine(freq, fs_in, 40 * (fs_in as usize) / 1000, amp);
                let out = resample_all(fs_in, fs_out, &input, for_enc);
                // Skip the transient; measure the steady-state amplitude.
                let skip = 10 * (fs_out as usize) / 1000;
                let gain = goertzel_amplitude(&out[skip..], freq, fs_out as f64) / amp;
                assert!(
                    (gain - 1.0).abs() < 0.15,
                    "{fs_in} -> {fs_out} @ {freq} Hz: gain {gain}"
                );
            }
        }
    }

    #[test]
    fn downsample_rejects_out_of_band_tones() {
        // (fs_in, fs_out, tone above fs_out's Nyquist): must be attenuated.
        let cases = [
            (12000, 8000, 5500.0),
            (16000, 8000, 6500.0),
            (16000, 12000, 7300.0),
            (24000, 8000, 9000.0),
            (24000, 16000, 11000.0),
            (48000, 8000, 20000.0),
            (48000, 16000, 20000.0),
        ];
        for &(fs_in, fs_out, freq) in &cases {
            let for_enc = ENC_PAIRS.contains(&(fs_in, fs_out));
            let amp = 12000.0;
            let input = sine(freq, fs_in, 60 * (fs_in as usize) / 1000, amp);
            let out = resample_all(fs_in, fs_out, &input, for_enc);
            let skip = 15 * (fs_out as usize) / 1000;
            let gain = goertzel_amplitude(&out[skip..], freq, fs_out as f64) / amp;
            assert!(gain < 0.1, "{fs_in} -> {fs_out} @ {freq} Hz: gain {gain}");
        }
    }

    #[test]
    fn upsample_suppresses_images() {
        // A 1 kHz tone upsampled must not leak images near the internal
        // rate's spectral repeats.
        let cases = [
            ((8000, 48000), 17000.0),
            ((12000, 48000), 13000.0),
            ((16000, 48000), 17000.0),
            ((8000, 16000), 9000.0),
            ((8000, 24000), 9000.0),
        ];
        for &((fs_in, fs_out), image) in &cases {
            let amp = 12000.0;
            let input = sine(1000.0, fs_in, 60 * (fs_in as usize) / 1000, amp);
            let out = resample_all(fs_in, fs_out, &input, false);
            let skip = 15 * (fs_out as usize) / 1000;
            let main = goertzel_amplitude(&out[skip..], 1000.0, fs_out as f64);
            let image_gain = goertzel_amplitude(&out[skip..], image, fs_out as f64);
            assert!(
                image_gain < 0.12 * main,
                "{fs_in} -> {fs_out}: image @ {image} Hz {image_gain} vs main {main}"
            );
        }
    }

    #[test]
    fn copy_path_reproduces_input_with_reference_delay() {
        // Fs_in == Fs_out: exact pass-through delayed by `input_delay`.
        let input = lcg_noise(400, 9);
        let out = resample_all(8000, 8000, &input, false);
        let delay = Resampler::new(8000, 8000, false).unwrap().input_delay();
        assert_eq!(delay, 4);
        assert_eq!(&out[..delay], &[0i16; 4]);
        assert_eq!(&out[delay..], &input[..input.len() - delay]);

        let out = resample_all(16000, 16000, &input, false);
        let delay = Resampler::new(16000, 16000, false).unwrap().input_delay();
        assert_eq!(delay, 12);
        assert!(out[..delay].iter().all(|&s| s == 0));
        assert_eq!(&out[delay..], &input[..input.len() - delay]);
    }

    #[test]
    fn resample_enforces_input_contract() {
        let mut r = Resampler::new(16000, 48000, false).unwrap();
        let mut out = vec![0i16; 48];
        // Shorter than 1 ms.
        assert!(r.resample(&mut out, &[1, 2, 3]).is_err());
        // Not a whole number of milliseconds.
        assert!(r.resample(&mut out, &[0i16; 17]).is_err());
        // Wrong output size.
        let mut bad = vec![0i16; 47];
        assert!(r.resample(&mut bad, &[0i16; 16]).is_err());
        assert_eq!(out.len(), 48);
        r.resample(&mut out, &[0i16; 16]).unwrap();
    }
}
