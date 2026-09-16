//! The CELT decoder assembly (`celt/celt_decoder.c`).
//!
//! Ports `celt_decode_with_ec` (normal frames), `celt_decode_lost`
//! (DTX/PLC), `celt_synthesis`, `tf_decode`, the deemphasis filter, and
//! the postfilter driver. All scratch buffers live in the struct so
//! `decode` stays allocation-free after construction.
//!
//! SOURCE: Xiph.Org libopus 1.5.2, `celt/celt_decoder.c` + `celt.c`
//! (BSD-3-Clause). Float build (`opus_val16 = f32`, `celt_sig = f32`);
//! all fixed-point shifts/macros collapse to plain operations exactly as
//! the reference macros do.

// Lints: this file ports libopus verbatim - verbatim float literals and
// reference loop shapes trigger these lints (see tables.rs).
#![allow(
    clippy::excessive_precision,
    clippy::approx_constant,
    clippy::precedence,
    clippy::needless_range_loop,
    clippy::manual_memcpy,
    clippy::too_many_arguments
)]

use super::bands::{anti_collapse, celt_lcg_rand, denormalise_bands, quant_all_bands, tf_decode};
use super::celt_lpc::{_celt_autocorr, _celt_lpc, celt_fir, celt_iir, CELT_LPC_ORDER};
use super::fft::Cpx;
use super::math::VERY_SMALL;
use super::mdct::mdct_backward;
use super::pitch::{
    comb_filter, pitch_downsample, pitch_search, COMBFILTER_MINPERIOD, PLC_PITCH_LAG_MAX,
    PLC_PITCH_LAG_MIN,
};
use super::quant_bands::{unquant_coarse_energy, unquant_energy_finalise, unquant_fine_energy};
use super::rate::{compute_allocation, init_caps, NB_EBANDS};
use super::tables::{EBAND5MS, WINDOW120};
use super::vq::{renormalise_vector, SPREAD_NORMAL};
use crate::range::RangeDecoder;

/// `DECODE_BUFFER_SIZE`.
pub(crate) const DECODE_BUFFER_SIZE: usize = 2048;
/// Mode overlap for the static 48 kHz mode.
pub(crate) const OVERLAP: usize = 120;
/// `shortMdctSize` (120 samples per short block at 48 kHz).
const SHORT_MDCT_SIZE: usize = 120;
/// Channel stride in `_decode_mem`.
const CH_STRIDE: usize = DECODE_BUFFER_SIZE + OVERLAP;
/// Largest block count (LM = 3).
const MAX_LM: usize = 3;
/// `SIG_SAT` saturation is a no-op in the float build, but the reference
/// still bounds the IMDCT output through `SATURATE(.., SIG_SAT)`.
const SIG_SAT: f32 = 3e8;

/// Scratch sizes derived from the mode.
const N_MAX: usize = SHORT_MDCT_SIZE << MAX_LM; // 960
const NORM_LEN_MAX: usize = (1 << MAX_LM) * EBAND5MS[NB_EBANDS - 1] as usize; // 624
const BAND_MAX: usize = (1 << MAX_LM) * (EBAND5MS[NB_EBANDS] - EBAND5MS[NB_EBANDS - 1]) as usize; // 176
const PLC_SEARCH_LEN: usize = DECODE_BUFFER_SIZE - PLC_PITCH_LAG_MAX; // 1328
const PLC_MAX_PITCH: usize = PLC_PITCH_LAG_MAX - PLC_PITCH_LAG_MIN; // 620

/// `TRIM_ICDF` (celt.h).
static TRIM_ICDF: [u8; 11] = [126, 124, 119, 109, 87, 41, 19, 9, 4, 2, 0];
/// `SPREAD_ICDF` (celt.h).
static SPREAD_ICDF_TBL: [u8; 4] = [25, 23, 2, 0];
/// `TAPSET_ICDF` (celt.h).
static TAPSET_ICDF: [u8; 3] = [2, 1, 0];

/// CELT decoder state (mono or stereo output).
pub struct CeltDecoder {
    channels: usize,
    stream_channels: usize,
    downsample: usize,
    start: usize,
    end: usize,
    disable_inv: bool,

    rng: u32,
    error: bool,
    last_pitch_index: i32,
    loss_duration: i32,
    skip_plc: bool,
    postfilter_period: i32,
    postfilter_period_old: i32,
    postfilter_gain: f32,
    postfilter_gain_old: f32,
    postfilter_tapset: usize,
    postfilter_tapset_old: usize,
    prefilter_and_fold: bool,

    preemph_mem_d: [f32; 2],

    // Persistent per-channel history + energies.
    decode_mem: Vec<f32>, // channels * CH_STRIDE
    lpc: [f32; 2 * CELT_LPC_ORDER],
    old_band_e: [f32; 2 * NB_EBANDS],
    old_log_e: [f32; 2 * NB_EBANDS],
    old_log_e2: [f32; 2 * NB_EBANDS],
    background_log_e: [f32; 2 * NB_EBANDS],

    // Scratch (allocation-free decode).
    freq: [f32; N_MAX],
    freq2: [f32; N_MAX],
    x_spec: [f32; 2 * N_MAX],
    norm: [f32; 2 * NORM_LEN_MAX],
    scratch_band: [f32; BAND_MAX],
    htmp: [f32; BAND_MAX],
    iy: [i32; BAND_MAX],
    collapse_masks: [u8; 2 * NB_EBANDS],
    mdct_scratch: [Cpx; 480], // max N4 = 1920>>2
    etmp: [f32; OVERLAP],
    tf_res: [i32; NB_EBANDS],
    cap: [i32; NB_EBANDS],
    offsets: [i32; NB_EBANDS],
    fine_quant: [i32; NB_EBANDS],
    pulses: [i32; NB_EBANDS],
    fine_priority: [i32; NB_EBANDS],

    // PLC scratch.
    lp_pitch_buf: [f32; DECODE_BUFFER_SIZE >> 1],
    ps_x_lp4: [f32; PLC_SEARCH_LEN >> 2],
    ps_y_lp4: [f32; (PLC_SEARCH_LEN + PLC_MAX_PITCH) >> 2],
    ps_xcorr: [f32; PLC_MAX_PITCH >> 1],
    exc: [f32; PLC_PITCH_LAG_MAX + CELT_LPC_ORDER],
    fir_tmp: [f32; PLC_PITCH_LAG_MAX],
    x_iir: [f32; N_MAX + OVERLAP],
    iir_scratch: [f32; N_MAX + OVERLAP + CELT_LPC_ORDER],
    lpc_mem: [f32; CELT_LPC_ORDER],
}

impl CeltDecoder {
    /// Creates a decoder for `channels` (1 or 2) output channels at 48 kHz
    /// (downsample = 1). Other Opus rates require the caller to trim
    /// `frame_size` and decimate; see `with_downsample`.
    pub fn new(channels: usize, sample_rate: u32) -> crate::Result<Self> {
        let downsample = match sample_rate {
            48_000 => 1usize,
            24_000 => 2,
            16_000 => 3,
            12_000 => 4,
            8_000 => 6,
            _ => {
                return Err(crate::CadenceError::UnsupportedFeature(
                    "CELT: unsupported sample rate (use 8/12/16/24/48 kHz)".to_string(),
                ))
            }
        };
        if channels == 0 || channels > 2 {
            return Err(crate::CadenceError::UnsupportedFeature(
                "CELT: channels must be 1 or 2".to_string(),
            ));
        }
        Ok(Self::with_downsample(channels, downsample))
    }

    /// Creates a decoder with an explicit downsampling factor (48000/factor).
    pub fn with_downsample(channels: usize, downsample: usize) -> Self {
        CeltDecoder {
            channels,
            stream_channels: channels,
            downsample,
            start: 0,
            end: NB_EBANDS,
            disable_inv: channels == 1,
            rng: 1_000_000, // reset state; libopus clears to 0
            error: false,
            last_pitch_index: 0,
            loss_duration: 0,
            skip_plc: false,
            postfilter_period: 0,
            postfilter_period_old: 0,
            postfilter_gain: 0.0,
            postfilter_gain_old: 0.0,
            postfilter_tapset: 0,
            postfilter_tapset_old: 0,
            prefilter_and_fold: false,
            preemph_mem_d: [0.0; 2],
            decode_mem: vec![0.0; channels * CH_STRIDE],
            lpc: [0.0; 2 * CELT_LPC_ORDER],
            old_band_e: [0.0; 2 * NB_EBANDS],
            old_log_e: [0.0; 2 * NB_EBANDS],
            old_log_e2: [0.0; 2 * NB_EBANDS],
            background_log_e: [0.0; 2 * NB_EBANDS],
            freq: [0.0; N_MAX],
            freq2: [0.0; N_MAX],
            x_spec: [0.0; 2 * N_MAX],
            norm: [0.0; 2 * NORM_LEN_MAX],
            scratch_band: [0.0; BAND_MAX],
            htmp: [0.0; BAND_MAX],
            iy: [0; BAND_MAX],
            collapse_masks: [0; 2 * NB_EBANDS],
            mdct_scratch: [Cpx::default(); 480],
            etmp: [0.0; OVERLAP],
            tf_res: [0; NB_EBANDS],
            cap: [0; NB_EBANDS],
            offsets: [0; NB_EBANDS],
            fine_quant: [0; NB_EBANDS],
            pulses: [0; NB_EBANDS],
            fine_priority: [0; NB_EBANDS],
            lp_pitch_buf: [0.0; DECODE_BUFFER_SIZE >> 1],
            ps_x_lp4: [0.0; PLC_SEARCH_LEN >> 2],
            ps_y_lp4: [0.0; (PLC_SEARCH_LEN + PLC_MAX_PITCH) >> 2],
            ps_xcorr: [0.0; PLC_MAX_PITCH >> 1],
            exc: [0.0; PLC_PITCH_LAG_MAX + CELT_LPC_ORDER],
            fir_tmp: [0.0; PLC_PITCH_LAG_MAX],
            x_iir: [0.0; N_MAX + OVERLAP],
            iir_scratch: [0.0; N_MAX + OVERLAP + CELT_LPC_ORDER],
            lpc_mem: [0.0; CELT_LPC_ORDER],
        }
    }

    /// `CELT_SET_CHANNELS`.
    pub fn set_stream_channels(&mut self, channels: usize) {
        self.stream_channels = channels;
    }

    /// `CELT_SET_END_BAND`.
    pub fn set_end_band(&mut self, end: usize) {
        self.end = end;
    }

    /// `CELT_SET_START_BAND`.
    pub fn set_start_band(&mut self, start: usize) {
        self.start = start;
    }

    /// `OPUS_RESET_STATE`: clears the dynamic decoder state.
    pub fn reset(&mut self) {
        self.rng = 0;
        self.error = false;
        self.last_pitch_index = 0;
        self.loss_duration = 0;
        self.skip_plc = false;
        self.postfilter_period = 0;
        self.postfilter_period_old = 0;
        self.postfilter_gain = 0.0;
        self.postfilter_gain_old = 0.0;
        self.postfilter_tapset = 0;
        self.postfilter_tapset_old = 0;
        self.preemph_mem_d = [0.0; 2];
        self.decode_mem.fill(0.0);
        self.lpc = [0.0; 2 * CELT_LPC_ORDER];
        self.old_band_e = [0.0; 2 * NB_EBANDS];
        self.old_log_e = [0.0; 2 * NB_EBANDS];
        self.old_log_e2 = [0.0; 2 * NB_EBANDS];
        self.background_log_e = [0.0; 2 * NB_EBANDS];
    }

    /// Decodes one CELT frame, writing `channels * frame_size` interleaved
    /// samples (±1.0 float scale) into `pcm`. `data == None` or empty
    /// packet conceal the frame (PLC/DTX).
    ///
    /// Returns the number of samples per channel produced.
    pub fn decode(
        &mut self,
        data: Option<&[u8]>,
        frame_size: usize,
        pcm: &mut [f32],
    ) -> crate::Result<usize> {
        let mut dec_owned = None;
        self.decode_with_ec(data, frame_size, dec_owned.as_mut(), pcm)
    }

    /// `celt_decode_with_ec`: as [`decode`][Self::decode], but reuses an
    /// existing range decoder (what the future SILK/hybrid integration
    /// needs). `dec` may be `None` (fresh decoder over `data`).
    pub fn decode_with_ec(
        &mut self,
        data: Option<&[u8]>,
        frame_size: usize,
        dec: Option<&mut RangeDecoder>,
        pcm: &mut [f32],
    ) -> crate::Result<usize> {
        let frame_size = frame_size * self.downsample;

        // Determine LM from the frame size (non-custom path).
        let mut lm = None;
        for cand in 0..=MAX_LM {
            if SHORT_MDCT_SIZE << cand == frame_size {
                lm = Some(cand);
                break;
            }
        }
        let lm = lm.ok_or_else(|| {
            crate::CadenceError::UnsupportedFeature(format!(
                "CELT: invalid frame size {frame_size} (must be 120/240/480/960 per channel)"
            ))
        })?;
        let m = 1 << lm;

        let data_len = data.map(|d| d.len()).unwrap_or(0);
        if data_len > 1275 {
            return Err(crate::CadenceError::CorruptData(
                "CELT: packet longer than 1275 bytes".to_string(),
            ));
        }

        let n = m * SHORT_MDCT_SIZE;
        let cc = self.channels;
        let c = self.stream_channels;
        let start = self.start;
        let end = self.end;
        let mut eff_end = end;
        if eff_end > NB_EBANDS {
            eff_end = NB_EBANDS;
        }

        let empty_or_missing = match data {
            None => true,
            Some(d) => d.len() <= 1,
        };
        if empty_or_missing {
            self.decode_lost(n, lm);
            let mut mem = self.preemph_mem_d;
            {
                let (s0, s1) = split_decode_mem(&mut self.decode_mem, n, cc);
                let s1 = s1.map(|x| &*x);
                deemphasis(&mut mem, s0, s1, pcm, n, cc);
            }
            self.preemph_mem_d = mem;
            return Ok(frame_size / self.downsample);
        }
        let data = data.unwrap();

        // Check if there are at least two packets received consecutively
        // before turning on the pitch-based PLC.
        if self.loss_duration == 0 {
            self.skip_plc = false;
        }

        match dec {
            Some(d) => self.decode_inner(
                data, data_len, frame_size, n, lm, m, c, cc, start, end, eff_end, d, pcm,
            ),
            None => {
                let mut owned_dec = RangeDecoder::new(data);
                self.decode_inner(
                    data,
                    data_len,
                    frame_size,
                    n,
                    lm,
                    m,
                    c,
                    cc,
                    start,
                    end,
                    eff_end,
                    &mut owned_dec,
                    pcm,
                )
            }
        }
    }

    /// The main-frame decode path once the packet is known to be present
    /// (`data_len >= 2`).
    #[allow(clippy::too_many_arguments)]
    fn decode_inner(
        &mut self,
        _data: &[u8],
        data_len: usize,
        frame_size: usize,
        n: usize,
        lm: usize,
        m: usize,
        c: usize,
        cc: usize,
        start: usize,
        end: usize,
        eff_end: usize,
        dec: &mut RangeDecoder,
        pcm: &mut [f32],
    ) -> crate::Result<usize> {
        if c == 1 {
            for i in 0..NB_EBANDS {
                self.old_band_e[i] = self.old_band_e[i].max(self.old_band_e[NB_EBANDS + i]);
            }
        }

        let mut total_bits = (data_len * 8) as i32;
        let mut tell = dec.tell() as i32;

        let silence = if tell >= total_bits {
            true
        } else if tell == 1 {
            dec.decode_bit_logp(15)?
        } else {
            false
        };
        if silence {
            // Pretend we've read all the remaining bits.
            let tell_target = (data_len * 8) as i32;
            dec.force_tell(tell_target);
            tell = dec.tell() as i32;
        }

        let mut postfilter_gain = 0f32;
        let mut postfilter_pitch = 0i32;
        let mut postfilter_tapset = 0usize;
        if start == 0 && tell + 16 <= total_bits {
            if dec.decode_bit_logp(1)? {
                let octave = dec.decode_uint(6)?;
                postfilter_pitch = ((16 << octave) + dec.read_raw_bits(4 + octave) - 1) as i32;
                let qg = dec.read_raw_bits(3);
                if dec.tell() as i32 + 2 <= total_bits {
                    postfilter_tapset = dec.decode_icdf(&TAPSET_ICDF, 2)? as usize;
                }
                postfilter_gain = 0.093_75 * (qg as f32 + 1.0);
            }
            tell = dec.tell() as i32;
        }

        let is_transient = if lm > 0 && tell + 3 <= total_bits {
            let t = dec.decode_bit_logp(3)?;
            tell = dec.tell() as i32;
            t
        } else {
            false
        };
        let short_blocks = is_transient;

        // Decode the global flags (first symbols in the stream).
        let intra_ener = if tell + 3 <= total_bits {
            dec.decode_bit_logp(3)?
        } else {
            false
        };
        // If recovering from packet loss, make sure we make the energy
        // prediction safe to reduce the risk of getting loud artifacts.
        if !intra_ener && self.loss_duration != 0 {
            for ci in 0..2 {
                let mut safety = 0f32;
                let missing = 10.min(self.loss_duration >> lm);
                if lm == 0 {
                    safety = 1.5;
                } else if lm == 1 {
                    safety = 0.5;
                }
                for i in start..end {
                    let idx = ci * NB_EBANDS + i;
                    if self.old_band_e[idx] < self.old_log_e[idx].max(self.old_log_e2[idx]) {
                        // If energy is going down already, continue the
                        // trend.
                        let e0 = self.old_band_e[idx];
                        let e1 = self.old_log_e[idx];
                        let e2 = self.old_log_e2[idx];
                        let slope = (e1 - e0).max(0.5 * (e2 - e0));
                        let e0 = e0 - 0f32.max((1 + missing) as f32 * slope);
                        self.old_band_e[idx] = e0.max(-20.0);
                    } else {
                        // Otherwise take the min of the last frames.
                        self.old_band_e[idx] = self.old_band_e[idx]
                            .min(self.old_log_e[idx])
                            .min(self.old_log_e2[idx]);
                    }
                    // Shorter frames have more natural fluctuations.
                    self.old_band_e[idx] -= safety;
                }
            }
        }

        // Get band energies.
        unquant_coarse_energy(
            start,
            end,
            &mut self.old_band_e,
            intra_ener,
            data_len,
            dec,
            c,
            lm,
        )?;

        tf_decode(
            start,
            end,
            is_transient,
            &mut self.tf_res,
            lm,
            dec,
            data_len,
        )?;

        let tell = dec.tell() as i32;
        let mut spread_decision = SPREAD_NORMAL;
        if tell + 4 <= total_bits {
            spread_decision = dec.decode_icdf(&SPREAD_ICDF_TBL, 5)? as i32;
        }

        self.cap = init_caps(lm, c);

        let mut dynalloc_logp = 6i32;
        total_bits <<= 3; // BITRES
        let mut tell = dec.tell_frac() as i32;
        for i in start..end {
            let width = (c * (EBAND5MS[i + 1] - EBAND5MS[i]) as usize) << lm;
            // quanta is 6 bits, but no more than 1 bit/sample and no less
            // than 1/8 bit/sample.
            let quanta = ((width << 3) as i32).min((6 << 3).max(width as i32));
            let mut dynalloc_loop_logp = dynalloc_logp;
            let mut boost = 0i32;
            while tell + (dynalloc_loop_logp << 3) < total_bits && boost < self.cap[i] {
                let flag = dec.decode_bit_logp(dynalloc_loop_logp as u32)?;
                tell = dec.tell_frac() as i32;
                if !flag {
                    break;
                }
                boost += quanta;
                total_bits -= quanta;
                dynalloc_loop_logp = 1;
            }
            self.offsets[i] = boost;
            // Making dynalloc more likely.
            if boost > 0 {
                dynalloc_logp = 2.max(dynalloc_logp - 1);
            }
        }

        let alloc_trim = if tell + (6 << 3) <= total_bits {
            dec.decode_icdf(&TRIM_ICDF, 7)? as i32
        } else {
            5
        };

        let mut bits = ((data_len as i32 * 8) << 3) - dec.tell_frac() as i32 - 1;
        let anti_collapse_rsv = if is_transient && lm >= 2 && bits >= ((lm + 2) << 3) as i32 {
            1 << 3
        } else {
            0
        };
        bits -= anti_collapse_rsv;

        let result = compute_allocation(
            start,
            end,
            &self.offsets,
            &self.cap,
            alloc_trim,
            bits,
            lm as i32,
            c,
            dec,
        )?;
        self.pulses.copy_from_slice(&result.pulses);
        self.fine_quant.copy_from_slice(&result.ebits);
        self.fine_priority.copy_from_slice(&result.fine_priority);
        let intensity = result.alloc.intensity;
        let dual_stereo = result.alloc.dual_stereo;
        let coded_bands = result.alloc.coded_bands;
        let balance = result.alloc.balance;

        unquant_fine_energy(start, end, &mut self.old_band_e, &self.fine_quant, dec, c)?;

        // Shift the per-channel decode buffers left by N.
        for ci in 0..cc {
            let base = ci * CH_STRIDE;
            self.decode_mem
                .copy_within(base + n..base + CH_STRIDE, base);
        }

        // Decode fixed codebook.
        let total_bits_q = (data_len as i32 * 8) * 8 - anti_collapse_rsv; // len*(8<<BITRES)-rsv
        {
            let mut seed = self.rng;
            let res = quant_all_bands(
                dec,
                start,
                end,
                &mut self.x_spec,
                c == 2,
                &mut self.collapse_masks,
                &self.pulses,
                short_blocks,
                spread_decision,
                dual_stereo,
                intensity,
                &self.tf_res,
                total_bits_q,
                balance,
                lm,
                coded_bands,
                &mut seed,
                self.disable_inv,
                &mut self.norm,
                &mut self.scratch_band,
                &mut self.htmp,
                &mut self.iy,
            );
            self.rng = seed;
            res?;
        }

        let mut anti_collapse_on = false;
        if anti_collapse_rsv > 0 {
            anti_collapse_on = dec.read_raw_bits(1) != 0;
        }

        unquant_energy_finalise(
            start,
            end,
            &mut self.old_band_e,
            &self.fine_quant,
            &self.fine_priority,
            (data_len * 8) as i32 - dec.tell() as i32,
            dec,
            c,
        )?;

        if anti_collapse_on {
            anti_collapse(
                &mut self.x_spec,
                &self.collapse_masks,
                lm,
                c,
                n,
                start,
                end,
                &self.old_band_e,
                &self.old_log_e,
                &self.old_log_e2,
                &self.pulses,
                self.rng,
            );
        }

        if silence {
            for v in self.old_band_e.iter_mut() {
                *v = -28.0;
            }
        }
        if self.prefilter_and_fold {
            self.prefilter_and_fold(n);
        }

        self.celt_synthesis(eff_end, c, cc, is_transient, lm, silence);

        for ci in 0..cc {
            let base = ci * CH_STRIDE;
            self.postfilter_period = self.postfilter_period.max(COMBFILTER_MINPERIOD as i32);
            self.postfilter_period_old =
                self.postfilter_period_old.max(COMBFILTER_MINPERIOD as i32);
            {
                let buf = &mut self.decode_mem[base..base + CH_STRIDE];
                comb_filter(
                    buf,
                    DECODE_BUFFER_SIZE - n,
                    DECODE_BUFFER_SIZE - n,
                    self.postfilter_period_old as usize,
                    self.postfilter_period as usize,
                    SHORT_MDCT_SIZE,
                    self.postfilter_gain_old,
                    self.postfilter_gain,
                    self.postfilter_tapset_old,
                    self.postfilter_tapset,
                    Some(&WINDOW120),
                    OVERLAP,
                );
            }
            if lm != 0 {
                let buf = &mut self.decode_mem[base..base + CH_STRIDE];
                comb_filter(
                    buf,
                    DECODE_BUFFER_SIZE - n + SHORT_MDCT_SIZE,
                    DECODE_BUFFER_SIZE - n + SHORT_MDCT_SIZE,
                    self.postfilter_period as usize,
                    postfilter_pitch as usize,
                    n - SHORT_MDCT_SIZE,
                    self.postfilter_gain,
                    postfilter_gain,
                    self.postfilter_tapset,
                    postfilter_tapset,
                    Some(&WINDOW120),
                    OVERLAP,
                );
            }
        }
        self.postfilter_period_old = self.postfilter_period;
        self.postfilter_gain_old = self.postfilter_gain;
        self.postfilter_tapset_old = self.postfilter_tapset;
        self.postfilter_period = postfilter_pitch;
        self.postfilter_gain = postfilter_gain;
        self.postfilter_tapset = postfilter_tapset;
        if lm != 0 {
            self.postfilter_period_old = self.postfilter_period;
            self.postfilter_gain_old = self.postfilter_gain;
            self.postfilter_tapset_old = self.postfilter_tapset;
        }

        if c == 1 {
            self.old_band_e.copy_within(..NB_EBANDS, NB_EBANDS);
        }

        if !is_transient {
            self.old_log_e2 = self.old_log_e;
            self.old_log_e.copy_from_slice(&self.old_band_e);
        } else {
            for i in 0..2 * NB_EBANDS {
                self.old_log_e[i] = self.old_log_e[i].min(self.old_band_e[i]);
            }
        }
        // In normal circumstances, we only allow the noise floor to
        // increase by up to 2.4 dB/second.
        let max_background_increase = 160.min(self.loss_duration + m as i32) as f32 * 0.001;
        for i in 0..2 * NB_EBANDS {
            self.background_log_e[i] =
                (self.background_log_e[i] + max_background_increase).min(self.old_band_e[i]);
        }
        // In case start or end were to change.
        for ci in 0..2 {
            for i in 0..start {
                let idx = ci * NB_EBANDS + i;
                self.old_band_e[idx] = 0.0;
                self.old_log_e[idx] = -28.0;
                self.old_log_e2[idx] = -28.0;
            }
            for i in end..NB_EBANDS {
                let idx = ci * NB_EBANDS + i;
                self.old_band_e[idx] = 0.0;
                self.old_log_e[idx] = -28.0;
                self.old_log_e2[idx] = -28.0;
            }
        }
        self.rng = dec.rng();

        let mut mem = self.preemph_mem_d;
        {
            let (s0, s1) = split_decode_mem(&mut self.decode_mem, n, cc);
            let s1 = s1.map(|x| &*x);
            deemphasis(&mut mem, s0, s1, pcm, n, cc);
        }
        self.preemph_mem_d = mem;
        self.loss_duration = 0;
        self.prefilter_and_fold = false;
        if dec.tell() > (8 * data_len) as u32 {
            return Err(crate::CadenceError::CorruptData(
                "CELT: range decoder overread".to_string(),
            ));
        }
        Ok(frame_size / self.downsample)
    }

    /// `celt_synthesis`: denormalize + inverse MDCT + overlap-add.
    fn celt_synthesis(
        &mut self,
        eff_end: usize,
        c: usize,
        cc: usize,
        is_transient: bool,
        lm: usize,
        silence: bool,
    ) {
        let n = SHORT_MDCT_SIZE << lm;
        let m = 1 << lm;
        let (b_blocks, nb, shift) = if is_transient {
            (m, SHORT_MDCT_SIZE, MAX_LM)
        } else {
            (1, n, MAX_LM - lm)
        };
        let downsample = self.downsample;

        if cc == 2 && c == 1 {
            // Copying a mono stream to two channels.
            denormalise_bands(
                &self.x_spec,
                &mut self.freq,
                &self.old_band_e,
                self.start,
                eff_end,
                m,
                downsample,
                silence,
            );
            // Store a temporary copy because the IMDCT destroys its input.
            self.freq2[..n].copy_from_slice(&self.freq[..n]);
            for b in 0..b_blocks {
                let (ch0, ch1) = self.decode_mem.split_at_mut(CH_STRIDE);
                let out0 = &mut ch0[DECODE_BUFFER_SIZE - n + nb * b..];
                mdct_backward(
                    &self.freq2[b..],
                    out0,
                    &WINDOW120,
                    OVERLAP,
                    shift,
                    b_blocks,
                    &mut self.mdct_scratch,
                );
                let out1 = &mut ch1[DECODE_BUFFER_SIZE - n + nb * b..];
                mdct_backward(
                    &self.freq[b..],
                    out1,
                    &WINDOW120,
                    OVERLAP,
                    shift,
                    b_blocks,
                    &mut self.mdct_scratch,
                );
            }
        } else if cc == 1 && c == 2 {
            // Downmixing a stereo stream to mono.
            denormalise_bands(
                &self.x_spec,
                &mut self.freq,
                &self.old_band_e,
                self.start,
                eff_end,
                m,
                downsample,
                silence,
            );
            {
                let (ch0, _ch1) = self.decode_mem.split_at_mut(CH_STRIDE);
                // Use the output buffer as temp array before downmixing,
                // exactly like the reference: `freq2 = out_syn[0]+overlap/2`
                // (the head must stay intact for the TDAC read).
                let out0 = &mut ch0[DECODE_BUFFER_SIZE - n + OVERLAP / 2
                    ..DECODE_BUFFER_SIZE - n + OVERLAP / 2 + n];
                denormalise_bands(
                    &self.x_spec[n..],
                    out0,
                    &self.old_band_e[NB_EBANDS..],
                    self.start,
                    eff_end,
                    m,
                    downsample,
                    silence,
                );
                let head = DECODE_BUFFER_SIZE - n + OVERLAP / 2;
                for i in 0..n {
                    self.freq[i] = 0.5 * self.freq[i] + 0.5 * ch0[head + i];
                }
            }
            for b in 0..b_blocks {
                let (ch0, _) = self.decode_mem.split_at_mut(CH_STRIDE);
                let out0 = &mut ch0[DECODE_BUFFER_SIZE - n + nb * b..];
                mdct_backward(
                    &self.freq[b..],
                    out0,
                    &WINDOW120,
                    OVERLAP,
                    shift,
                    b_blocks,
                    &mut self.mdct_scratch,
                );
            }
        } else {
            // Normal case (mono or stereo).
            for ci in 0..cc {
                denormalise_bands(
                    &self.x_spec[ci * n..],
                    &mut self.freq,
                    &self.old_band_e[ci * NB_EBANDS..],
                    self.start,
                    eff_end,
                    m,
                    downsample,
                    silence,
                );
                for b in 0..b_blocks {
                    let (ch0, ch1) = self.decode_mem.split_at_mut(CH_STRIDE);
                    let ch = if ci == 0 { ch0 } else { ch1 };
                    let out = &mut ch[DECODE_BUFFER_SIZE - n + nb * b..];
                    mdct_backward(
                        &self.freq[b..],
                        out,
                        &WINDOW120,
                        OVERLAP,
                        shift,
                        b_blocks,
                        &mut self.mdct_scratch,
                    );
                }
            }
        }
        // Saturate IMDCT output (no-op bound in float, kept for parity).
        for ci in 0..cc {
            let base = ci * CH_STRIDE;
            for v in
                self.decode_mem[base + DECODE_BUFFER_SIZE - n..base + DECODE_BUFFER_SIZE].iter_mut()
            {
                *v = v.clamp(-SIG_SAT, SIG_SAT);
            }
        }
    }

    /// `prefilter_and_fold`: applies the inverse postfilter to the MDCT
    /// overlap for the next frame and simulates TDAC on the concealed
    /// audio.
    fn prefilter_and_fold(&mut self, n: usize) {
        for ci in 0..self.channels {
            let base = ci * CH_STRIDE;
            // comb_filter into etmp with the source in decode_mem.
            super::pitch::comb_filter_ext(
                &self.decode_mem[base..base + CH_STRIDE],
                DECODE_BUFFER_SIZE - n,
                &mut self.etmp,
                0,
                self.postfilter_period_old as usize,
                self.postfilter_period as usize,
                OVERLAP,
                -self.postfilter_gain_old,
                -self.postfilter_gain,
                self.postfilter_tapset_old,
                self.postfilter_tapset,
            );
            // Simulate TDAC on the concealed audio so that it blends with
            // the MDCT of the next frame.
            for i in 0..OVERLAP / 2 {
                let v = WINDOW120[i] * self.etmp[OVERLAP - 1 - i]
                    + WINDOW120[OVERLAP - i - 1] * self.etmp[i];
                self.decode_mem[base + DECODE_BUFFER_SIZE - n + i] = v;
            }
        }
    }

    /// `celt_decode_lost`: concealment for missing/DTX packets.
    fn decode_lost(&mut self, n: usize, lm: usize) {
        let c = self.channels;
        let noise_based = self.loss_duration >= 40 || self.start != 0 || self.skip_plc;

        if noise_based {
            // Noise-based PLC/CNG.
            let eff_end = self.start.max(self.end.min(NB_EBANDS));
            for ci in 0..c {
                let base = ci * CH_STRIDE;
                self.decode_mem
                    .copy_within(base + n..base + CH_STRIDE, base);
            }
            if self.prefilter_and_fold {
                self.prefilter_and_fold(n);
            }
            // Energy decay.
            let decay = if self.loss_duration == 0 { 1.5 } else { 0.5 };
            for ci in 0..c {
                for i in self.start..self.end {
                    let idx = ci * NB_EBANDS + i;
                    self.old_band_e[idx] =
                        self.background_log_e[idx].max(self.old_band_e[idx] - decay);
                }
            }
            let mut seed = self.rng;
            for ci in 0..c {
                for i in self.start..eff_end {
                    let j0 = n * ci + ((EBAND5MS[i] as usize) << lm);
                    let blen = ((EBAND5MS[i + 1] - EBAND5MS[i]) as usize) << lm;
                    for j in 0..blen {
                        seed = celt_lcg_rand(seed);
                        self.x_spec[j0 + j] = ((seed as i32) >> 20) as f32;
                    }
                    renormalise_vector(&mut self.x_spec[j0..j0 + blen], 1.0);
                }
            }
            self.rng = seed;
            self.celt_synthesis(eff_end, c, c, false, lm, false);
            self.prefilter_and_fold = false;
            // Skip regular PLC until we get two consecutive packets.
            self.skip_plc = true;
        } else {
            // Pitch-based PLC.
            let mut fade = 1f32;
            let pitch_index;
            if self.loss_duration == 0 {
                let p = self.plc_pitch_search();
                self.last_pitch_index = p;
                pitch_index = p;
            } else {
                pitch_index = self.last_pitch_index;
                fade = 0.8;
            }

            // We want the excitation for 2 pitch periods in order to look
            // for a decaying signal, but we can't get more than
            // MAX_PERIOD.
            let exc_length = (2 * pitch_index as usize).min(PLC_PITCH_LAG_MAX);

            for ci in 0..c {
                let base = ci * CH_STRIDE;
                // exc_full[-24..1024): exc[i-24] = exc_full[i].
                for i in 0..PLC_PITCH_LAG_MAX + CELT_LPC_ORDER {
                    self.exc[i] = self.decode_mem
                        [base + DECODE_BUFFER_SIZE - PLC_PITCH_LAG_MAX - CELT_LPC_ORDER + i];
                }

                if self.loss_duration == 0 {
                    let mut ac = [0f32; CELT_LPC_ORDER + 1];
                    // Compute LPC coefficients for the last MAX_PERIOD
                    // samples before the first loss.
                    _celt_autocorr(
                        &self.exc[CELT_LPC_ORDER..],
                        &mut ac,
                        Some(&WINDOW120),
                        OVERLAP,
                        CELT_LPC_ORDER,
                        PLC_PITCH_LAG_MAX,
                    );
                    // Add a noise floor of -40 dB.
                    ac[0] *= 1.0001;
                    // Use lag windowing to stabilize Levinson-Durbin.
                    for i in 1..=CELT_LPC_ORDER {
                        ac[i] -= ac[i] * (0.008 * 0.008) * (i * i) as f32;
                    }
                    let mut lpc_ch = [0f32; CELT_LPC_ORDER];
                    _celt_lpc(&mut lpc_ch, &ac, CELT_LPC_ORDER);
                    self.lpc[ci * CELT_LPC_ORDER..(ci + 1) * CELT_LPC_ORDER]
                        .copy_from_slice(&lpc_ch);
                }

                // Initialize the LPC history with the samples just before
                // the start of the region for which we're computing the
                // excitation.
                {
                    let fir_src = PLC_PITCH_LAG_MAX - exc_length;
                    let x_slice = &self.exc[..];
                    let out_fir = &mut self.fir_tmp[..exc_length];
                    celt_fir(
                        x_slice,
                        fir_src + CELT_LPC_ORDER,
                        &self.lpc[ci * CELT_LPC_ORDER..(ci + 1) * CELT_LPC_ORDER],
                        out_fir,
                        exc_length,
                        CELT_LPC_ORDER,
                    );
                    self.exc[fir_src + CELT_LPC_ORDER..fir_src + CELT_LPC_ORDER + exc_length]
                        .copy_from_slice(&self.fir_tmp[..exc_length]);
                }

                // Check if the waveform is decaying, and if so how fast.
                let decay;
                {
                    let mut e1 = 1f32;
                    let mut e2 = 1f32;
                    let decay_length = exc_length >> 1;
                    for i in 0..decay_length {
                        let e = self.exc[CELT_LPC_ORDER + PLC_PITCH_LAG_MAX - decay_length + i];
                        e1 += e * e;
                        let e = self.exc[CELT_LPC_ORDER + PLC_PITCH_LAG_MAX - 2 * decay_length + i];
                        e2 += e * e;
                    }
                    e1 = e1.min(e2);
                    decay = (e1 / e2).sqrt();
                }

                // Move the decoder memory one frame to the left.
                self.decode_mem
                    .copy_within(base + n..base + DECODE_BUFFER_SIZE, base);

                // Extrapolate from the end of the excitation with a period
                // of "pitch_index", scaling down each period by an
                // additional factor of "decay".
                let extrapolation_offset = PLC_PITCH_LAG_MAX - pitch_index as usize;
                let extrapolation_len = n + OVERLAP;
                let mut attenuation = fade * decay;
                let mut j = 0usize;
                let mut s1 = 0f32;
                for i in 0..extrapolation_len {
                    if j >= pitch_index as usize {
                        j -= pitch_index as usize;
                        attenuation *= decay;
                    }
                    self.decode_mem[base + DECODE_BUFFER_SIZE - n + i] =
                        attenuation * self.exc[CELT_LPC_ORDER + extrapolation_offset + j];
                    // Compute the energy of the previously decoded signal
                    // whose excitation we're copying.
                    let tmp = self.decode_mem[base + DECODE_BUFFER_SIZE - PLC_PITCH_LAG_MAX - n
                        + extrapolation_offset
                        + j];
                    s1 += tmp * tmp;
                    j += 1;
                }
                {
                    // Copy the last decoded samples (prior to the overlap
                    // region) to synthesis filter memory.
                    for i in 0..CELT_LPC_ORDER {
                        self.lpc_mem[i] = self.decode_mem[base + DECODE_BUFFER_SIZE - n - 1 - i];
                    }
                    // celt_iir with aliased x/y in C: copy the input first
                    // (bit-identical, see celt_lpc.rs notes).
                    self.x_iir[..extrapolation_len].copy_from_slice(
                        &self.decode_mem[base + DECODE_BUFFER_SIZE - n
                            ..base + DECODE_BUFFER_SIZE - n + extrapolation_len],
                    );
                    let (lpc_ch, dst) = {
                        let lpc_ch: [f32; CELT_LPC_ORDER] = self.lpc
                            [ci * CELT_LPC_ORDER..(ci + 1) * CELT_LPC_ORDER]
                            .try_into()
                            .unwrap();
                        let dst =
                            &mut self.decode_mem[base + DECODE_BUFFER_SIZE - n..base + CH_STRIDE];
                        (lpc_ch, dst)
                    };
                    let mut mem = self.lpc_mem;
                    celt_iir(
                        &self.x_iir[..extrapolation_len],
                        &lpc_ch,
                        dst,
                        extrapolation_len,
                        CELT_LPC_ORDER,
                        &mut mem,
                        &mut self.iir_scratch,
                    );
                    self.lpc_mem = mem;
                }

                // Check if the synthesis energy is higher than expected,
                // which can happen with signal changes during our window.
                {
                    let mut s2 = 0f32;
                    for i in 0..extrapolation_len {
                        let tmp = self.decode_mem[base + DECODE_BUFFER_SIZE - n + i];
                        s2 += tmp * tmp;
                    }
                    // This checks for an "explosion" in the synthesis.
                    if s1 <= 0.2 * s2 {
                        for v in self.decode_mem[base + DECODE_BUFFER_SIZE - n
                            ..base + DECODE_BUFFER_SIZE - n + extrapolation_len]
                            .iter_mut()
                        {
                            *v = 0.0;
                        }
                    } else if s1 < s2 {
                        let ratio = ((s1 + 1.0) / (s2 + 1.0)).sqrt();
                        for i in 0..OVERLAP {
                            let tmp_g = 1.0 - WINDOW120[i] * (1.0 - ratio);
                            self.decode_mem[base + DECODE_BUFFER_SIZE - n + i] *= tmp_g;
                        }
                        for i in OVERLAP..extrapolation_len {
                            self.decode_mem[base + DECODE_BUFFER_SIZE - n + i] *= ratio;
                        }
                    }
                }
            }
            self.prefilter_and_fold = true;
        }

        // Saturate to something large to avoid wrap-around.
        self.loss_duration = 10000.min(self.loss_duration + (1 << lm));
    }

    /// `celt_plc_pitch_search` over the decode buffers.
    fn plc_pitch_search(&mut self) -> i32 {
        let stereo = self.channels == 2;
        let (x0, x1): (&[f32], Option<&[f32]>) = {
            let (a, b) = self.decode_mem.split_at(CH_STRIDE);
            (
                &a[..DECODE_BUFFER_SIZE],
                if stereo {
                    Some(&b[..DECODE_BUFFER_SIZE])
                } else {
                    None
                },
            )
        };
        let ps = &mut self.ps_x_lp4;
        let y_lp4 = &mut self.ps_y_lp4;
        let xcorr = &mut self.ps_xcorr;
        let lp = &mut self.lp_pitch_buf;
        pitch_downsample(x0, x1, lp, DECODE_BUFFER_SIZE);
        let pitch = pitch_search(
            &lp[PLC_PITCH_LAG_MAX >> 1..],
            lp,
            PLC_SEARCH_LEN,
            PLC_MAX_PITCH,
            ps,
            y_lp4,
            xcorr,
        );
        PLC_PITCH_LAG_MAX as i32 - pitch
    }
}

/// Splits the per-channel output regions `decode_mem[c]+2048-N ..` used as
/// `out_syn[]` in the reference.
fn split_decode_mem(
    decode_mem: &mut [f32],
    n: usize,
    cc: usize,
) -> (&mut [f32], Option<&mut [f32]>) {
    match cc {
        1 => {
            let (a, _) = decode_mem.split_at_mut(CH_STRIDE);
            (&mut a[DECODE_BUFFER_SIZE - n..], None)
        }
        _ => {
            let (a, b) = decode_mem.split_at_mut(CH_STRIDE);
            (
                &mut a[DECODE_BUFFER_SIZE - n..],
                Some(&mut b[DECODE_BUFFER_SIZE - n..]),
            )
        }
    }
}

/// `deemphasis` + output scaling (float build: ±1.0 output, i.e.
/// `SCALEOUT(SIG2WORD16(x)) = x * (1/32768)`).
fn deemphasis(
    preemph_mem: &mut [f32; 2],
    out_syn0: &[f32],
    out_syn1: Option<&[f32]>,
    pcm: &mut [f32],
    n: usize,
    cc: usize,
) {
    // mode->preemph[0] for the static 48 kHz mode.
    const COEF0: f32 = 0.850_006_10;
    let scale = 1.0 / 32768.0;
    let mut mem0 = preemph_mem[0];
    for j in 0..n {
        let tmp = out_syn0[j] + VERY_SMALL + mem0;
        mem0 = COEF0 * tmp;
        pcm[j * cc] = tmp * scale;
    }
    preemph_mem[0] = mem0;
    if let (2, Some(out_syn1)) = (cc, out_syn1) {
        let mut mem1 = preemph_mem[1];
        for j in 0..n {
            let tmp = out_syn1[j] + VERY_SMALL + mem1;
            mem1 = COEF0 * tmp;
            pcm[j * cc + 1] = tmp * scale;
        }
        preemph_mem[1] = mem1;
    }
}
