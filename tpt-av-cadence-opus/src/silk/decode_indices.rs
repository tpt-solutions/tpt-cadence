//! SILK per-frame side-information indices (RFC 6716 §4.2.4).
//!
//! Ports `silk/decode_indices.c`, plus the VAD/LBRR flag prologue that
//! the reference performs once per payload in `silk/dec_API.c` (it is
//! kept here because it is part of the same side-info bitstream layout).
//!
//! Bitstream order per frame:
//! 1. frame type + quantization offset (one ICDF symbol, table chosen by
//!    VAD flag / LBRR),
//! 2. gain indices (subframe 0 conditionally delta-coded against the
//!    previous frame when `condCoding == CODE_CONDITIONALLY`, otherwise
//!    MSB stage + 3 uniform LSBs; remaining subframes always delta),
//! 3. NLSF indices (stage-1 vector, then `LPC_order` stage-2 residuals
//!    with escape extension at both ends),
//! 4. NLSF interpolation factor (20 ms frames only),
//! 5. for voiced frames only: pitch lag (absolute, or delta against the
//!    previous frame when allowed), pitch contour, LTP filter index +
//!    per-subframe LTP gain indices, LTP scale (independent coding only),
//! 6. a uniform 2-bit seed.
//!
//! Fields not touched for a frame type (e.g. pitch fields of an
//! unvoiced frame) keep the previous frame's values, matching the
//! reference, where `indices` lives in the persistent channel state.
//!
//! SOURCE: Xiph.Org libopus 1.5.2, `silk/decode_indices.c`,
//! `silk/dec_API.c`, `silk/NLSF_unpack.c`, `silk/decoder_set_fs.c`,
//! `silk/define.h`, `silk/structs.h` (BSD-3-Clause).
#![allow(dead_code)]

use crate::range::RangeDecoder;
use crate::silk::tables::{
    NlsfCbStruct, DELTA_GAIN_ICDF, GAIN_ICDF, LBRR_FLAGS_ICDF_PTR, LTPSCALE_ICDF,
    LTP_GAIN_ICDF_PTRS, LTP_PER_INDEX_ICDF, NLSF_EXT_ICDF, NLSF_INTERPOLATION_FACTOR_ICDF,
    PITCH_CONTOUR_10_MS_ICDF, PITCH_CONTOUR_10_MS_NB_ICDF, PITCH_CONTOUR_ICDF,
    PITCH_CONTOUR_NB_ICDF, PITCH_DELTA_ICDF, PITCH_LAG_ICDF, TYPE_OFFSET_NO_VAD_ICDF,
    TYPE_OFFSET_VAD_ICDF, UNIFORM4_ICDF, UNIFORM6_ICDF, UNIFORM8_ICDF,
};
use crate::{CadenceError, Result};

/// `TYPE_NO_VOICE_ACTIVITY` (`silk/define.h`).
pub(crate) const TYPE_NO_VOICE_ACTIVITY: i8 = 0;
/// `TYPE_UNVOICED` (`silk/define.h`).
pub(crate) const TYPE_UNVOICED: i8 = 1;
/// `TYPE_VOICED` (`silk/define.h`).
pub(crate) const TYPE_VOICED: i8 = 2;

/// `MAX_NB_SUBFR`: 4 × 5 ms subframes in a 20 ms frame.
pub(crate) const MAX_NB_SUBFR: usize = 4;
/// `MAX_LPC_ORDER`.
pub(crate) const MAX_LPC_ORDER: usize = 16;
/// `NLSF_QUANT_MAX_AMPLITUDE`.
pub(crate) const NLSF_QUANT_MAX_AMPLITUDE: i32 = 4;
/// Maximum number of SILK frames per packet (20 ms each, so up to 60 ms).
pub(crate) const MAX_FRAMES_PER_PACKET: usize = 3;

/// The type of conditional coding in effect for a frame — mirrors
/// `CODE_INDEPENDENTLY`, `CODE_INDEPENDENTLY_NO_LTP_SCALING`, and
/// `CODE_CONDITIONALLY` (`silk/define.h`). The distinction matters for
/// voiced frames: only [`CondCoding::Independently`] codes the
/// LTP-scale symbol (`silk/decode_indices.c`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CondCoding {
    /// `CODE_INDEPENDENTLY`: full side info; the LTP-scale symbol is
    /// coded for voiced frames.
    Independently,
    /// `CODE_INDEPENDENTLY_NO_LTP_SCALING`: like `Independently` except
    /// the LTP-scale symbol is not coded (the side channel after a
    /// skipped side frame needs no LTP scaling).
    IndependentlyNoLtpScaling,
    /// `CODE_CONDITIONALLY`: delta-coded side info.
    Conditionally,
}

/// Decoded per-frame side information — mirrors `SideInfoIndices`
/// (`silk/structs.h`).
///
/// # PROVISIONAL (Tier 1 contract)
///
/// This struct is published early so the Tier 2 modules (`nlsf.rs`,
/// `gains.rs`, `pitch.rs`, `excitation.rs`) can code against it without
/// waiting for this module's bitstream handling to be finalized. Field
/// names map 1:1 onto the reference struct and are not expected to
/// change; field *types* may still be adjusted (e.g. if a Tier 2 module
/// wants wider intermediates), and Tier 4 may add derived accessors.
/// Code against the field set now, but treat the definition as
/// not-yet-frozen.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct SideInfoIndices {
    /// Quantized gain indices, one per subframe (0–63 independently,
    /// 0–39 delta).
    pub gains_indices: [i8; MAX_NB_SUBFR],
    /// LTP gain codebook index per subframe (into
    /// `LTP_GAIN_ICDF_PTRS[per_index]`).
    pub ltp_index: [i8; MAX_NB_SUBFR],
    /// NLSF stage-1 codebook index (`[0]`) followed by `LPC_order`
    /// stage-2 residuals (each in `-10..=10`).
    pub nlsf_indices: [i8; MAX_LPC_ORDER + 1],
    /// Absolute pitch lag index.
    pub lag_index: i16,
    /// Pitch contour codebook index.
    pub contour_index: i8,
    /// `TYPE_NO_VOICE_ACTIVITY` / `TYPE_UNVOICED` / `TYPE_VOICED`.
    pub signal_type: i8,
    /// Quantization offset type (0 or 1) — selects the row of
    /// `QUANTIZATION_OFFSETS_Q10`.
    pub quant_offset_type: i8,
    /// NLSF interframe interpolation factor in Q2 (0–4; forced 4 for
    /// 10 ms frames, which carry no symbol).
    pub nlsf_interp_coef_q2: i8,
    /// Periodicity (LTP codebook) index, 0–2.
    pub per_index: i8,
    /// LTP scaling index, 0–2 (into `LTPSCALES_TABLE_Q14`).
    pub ltp_scale_index: i8,
    /// Uniform random seed for excitation dithering, 0–3.
    pub seed: i8,
}

/// Cross-frame decoder state that the side-info decode mutates — the
/// `ec_prevSignalType` / `ec_prevLagIndex` pair of
/// `silk_decoder_state`. The Tier 4 `SilkDecoder` will hold one per
/// channel and reset it to `Default` on init/reset.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct EcPrevState {
    /// Signal type of the previously decoded frame.
    pub ec_prev_signal_type: i8,
    /// Pitch lag index of the previously decoded voiced frame.
    pub ec_prev_lag_index: i16,
}

/// Everything [`decode_indices`] reads from the decoder context, other
/// than the range decoder and the cross-frame [`EcPrevState`].
pub(crate) struct FrameParams {
    /// NLSF codebook: `&NLSF_CB_NB_MB` at 8/12 kHz, `&NLSF_CB_WB` at
    /// 16 kHz (`silk_decoder_set_fs`).
    pub nlsf_cb: &'static NlsfCbStruct,
    /// Internal sample rate in kHz: 8, 12, or 16 (SILK supports no
    /// other internal rates).
    pub fs_khz: u32,
    /// Subframes per frame: 4 (20 ms) or 2 (10 ms).
    pub nb_subfr: usize,
    /// Frame number within the packet (0-based).
    pub frame_index: usize,
    /// `psDec->VAD_flags[frame_index]`.
    pub vad_flag: bool,
    /// True when decoding an LBRR frame rather than a regular one.
    pub decode_lbrr: bool,
    /// Conditional-coding mode for this frame.
    pub cond_coding: CondCoding,
}

/// Decodes the per-frame side-information indices from the bitstream —
/// a direct port of `silk_decode_indices`.
///
/// The frame's VAD flag comes from [`decode_vad_flags_and_lbrr_flag`],
/// which the caller must have run once per payload before the frame
/// loop.
pub(crate) fn decode_indices(
    dec: &mut RangeDecoder<'_>,
    indices: &mut SideInfoIndices,
    ec_state: &mut EcPrevState,
    params: &FrameParams,
) -> Result<()> {
    debug_assert!(params.nb_subfr == MAX_NB_SUBFR || params.nb_subfr == MAX_NB_SUBFR / 2);
    let order = params.nlsf_cb.order as usize;

    /*******************************************/
    /* Decode signal type and quantizer offset */
    /*******************************************/
    let ix = if params.decode_lbrr || params.vad_flag {
        dec.decode_icdf(&TYPE_OFFSET_VAD_ICDF, 8)? as i32 + 2
    } else {
        dec.decode_icdf(&TYPE_OFFSET_NO_VAD_ICDF, 8)? as i32
    };
    indices.signal_type = (ix >> 1) as i8;
    indices.quant_offset_type = (ix & 1) as i8;

    /****************/
    /* Decode gains */
    /****************/
    /* First subframe */
    if params.cond_coding == CondCoding::Conditionally {
        /* Conditional coding */
        indices.gains_indices[0] = dec.decode_icdf(&DELTA_GAIN_ICDF, 8)? as i8;
    } else {
        /* Independent coding, in two stages: MSB bits followed by 3 LSBs */
        let msb = dec.decode_icdf(&GAIN_ICDF[indices.signal_type as usize], 8)? as i32;
        let lsb = dec.decode_icdf(&UNIFORM8_ICDF, 8)? as i32;
        indices.gains_indices[0] = ((msb << 3) + lsb) as i8;
    }

    /* Remaining subframes */
    for g in indices
        .gains_indices
        .iter_mut()
        .take(params.nb_subfr)
        .skip(1)
    {
        *g = dec.decode_icdf(&DELTA_GAIN_ICDF, 8)? as i8;
    }

    /**********************/
    /* Decode LSF Indices */
    /**********************/
    let cb1_index = dec.decode_icdf(
        &params.nlsf_cb.cb1_icdf
            [(indices.signal_type as usize >> 1) * params.nlsf_cb.n_vectors as usize..],
        8,
    )? as usize;
    indices.nlsf_indices[0] = cb1_index as i8;
    let (ec_ix, _pred_q8) = nlsf_unpack(params.nlsf_cb, cb1_index);
    for (i, &table_offset) in ec_ix.iter().enumerate().take(order) {
        let mut ix = dec.decode_icdf(&params.nlsf_cb.ec_icdf[table_offset..], 8)? as i32;
        if ix == 0 {
            ix -= dec.decode_icdf(&NLSF_EXT_ICDF, 8)? as i32;
        } else if ix == 2 * NLSF_QUANT_MAX_AMPLITUDE {
            ix += dec.decode_icdf(&NLSF_EXT_ICDF, 8)? as i32;
        }
        indices.nlsf_indices[i + 1] = (ix - NLSF_QUANT_MAX_AMPLITUDE) as i8;
    }

    /* Decode LSF interpolation factor */
    if params.nb_subfr == MAX_NB_SUBFR {
        indices.nlsf_interp_coef_q2 = dec.decode_icdf(&NLSF_INTERPOLATION_FACTOR_ICDF, 8)? as i8;
    } else {
        indices.nlsf_interp_coef_q2 = 4;
    }

    if indices.signal_type == TYPE_VOICED {
        /*********************/
        /* Decode pitch lags */
        /*********************/
        /* Get lag index */
        let mut decode_absolute_lag_index = true;
        if params.cond_coding == CondCoding::Conditionally
            && ec_state.ec_prev_signal_type == TYPE_VOICED
        {
            /* Decode Delta index */
            let delta_lag_index = dec.decode_icdf(&PITCH_DELTA_ICDF, 8)? as i32;
            if delta_lag_index > 0 {
                indices.lag_index =
                    (ec_state.ec_prev_lag_index as i32 + delta_lag_index - 9) as i16;
                decode_absolute_lag_index = false;
            }
        }
        if decode_absolute_lag_index {
            /* Absolute decoding: coarse stage scaled by fs_kHz/2, then
             * the uniformly-coded low bits. */
            let low_bits_icdf = pitch_lag_low_bits_icdf(params.fs_khz)?;
            indices.lag_index = (dec.decode_icdf(&PITCH_LAG_ICDF, 8)? as i32
                * (params.fs_khz as i32 >> 1)
                + dec.decode_icdf(low_bits_icdf, 8)? as i32) as i16;
        }
        ec_state.ec_prev_lag_index = indices.lag_index;

        /* Get contour index */
        let contour_icdf = pitch_contour_icdf(params.fs_khz, params.nb_subfr)?;
        indices.contour_index = dec.decode_icdf(contour_icdf, 8)? as i8;

        /********************/
        /* Decode LTP gains */
        /********************/
        /* Decode PERIndex value */
        indices.per_index = dec.decode_icdf(&LTP_PER_INDEX_ICDF, 8)? as i8;

        for ltp in indices.ltp_index.iter_mut().take(params.nb_subfr) {
            *ltp = dec.decode_icdf(LTP_GAIN_ICDF_PTRS[indices.per_index as usize], 8)? as i8;
        }

        /**********************/
        /* Decode LTP scaling */
        /**********************/
        if params.cond_coding == CondCoding::Independently {
            indices.ltp_scale_index = dec.decode_icdf(&LTPSCALE_ICDF, 8)? as i8;
        } else {
            indices.ltp_scale_index = 0;
        }
    }
    ec_state.ec_prev_signal_type = indices.signal_type;

    /***************/
    /* Decode seed */
    /***************/
    indices.seed = dec.decode_icdf(&UNIFORM4_ICDF, 8)? as i8;
    Ok(())
}

/// Decodes the per-frame VAD flags and the packet-level LBRR flag —
/// performed once per payload before the frame loop (the "Decode VAD
/// flags and LBRR flag" block of `silk_Decode` in `dec_API.c`). Each is
/// a single bit at probability 1/2. Returns the flags for frames
/// `0..n_frames` plus the packet-level LBRR flag.
pub(crate) fn decode_vad_flags_and_lbrr_flag(
    dec: &mut RangeDecoder<'_>,
    n_frames: usize,
) -> Result<([bool; MAX_FRAMES_PER_PACKET], bool)> {
    debug_assert!((1..=MAX_FRAMES_PER_PACKET).contains(&n_frames));
    let mut vad_flags = [false; MAX_FRAMES_PER_PACKET];
    for flag in vad_flags.iter_mut().take(n_frames) {
        *flag = dec.decode_bit_logp(1)?;
    }
    let lbrr_flag = dec.decode_bit_logp(1)?;
    Ok((vad_flags, lbrr_flag))
}

/// Decodes the per-frame LBRR flags of a packet whose packet-level LBRR
/// flag is set — the "Decode LBRR flags" block of `silk_Decode`. With a
/// single frame the flag is implicitly 1 (no bits are read); otherwise
/// one ICDF symbol encodes the whole pattern: bit `i` of `symbol + 1`
/// is frame `i`'s flag.
pub(crate) fn decode_lbrr_flags(
    dec: &mut RangeDecoder<'_>,
    n_frames: usize,
) -> Result<[bool; MAX_FRAMES_PER_PACKET]> {
    debug_assert!((1..=MAX_FRAMES_PER_PACKET).contains(&n_frames));
    let mut flags = [false; MAX_FRAMES_PER_PACKET];
    if n_frames == 1 {
        flags[0] = true;
    } else {
        let lbrr_symbol = dec.decode_icdf(LBRR_FLAGS_ICDF_PTR[n_frames - 2], 8)? + 1;
        for (i, flag) in flags.iter_mut().take(n_frames).enumerate() {
            *flag = (lbrr_symbol >> i) & 1 != 0;
        }
    }
    Ok(flags)
}

/// `psDec->pitch_lag_low_bits_iCDF` (`silk_decoder_set_fs`): the
/// uniformly-coded low bits of the absolute pitch lag index.
fn pitch_lag_low_bits_icdf(fs_khz: u32) -> Result<&'static [u8]> {
    match fs_khz {
        8 => Ok(&UNIFORM4_ICDF),
        12 => Ok(&UNIFORM6_ICDF),
        16 => Ok(&UNIFORM8_ICDF),
        _ => Err(CadenceError::CorruptData(format!(
            "unsupported SILK internal sample rate {fs_khz} kHz"
        ))),
    }
}

/// `psDec->pitch_contour_iCDF` (`silk_decoder_set_fs`): the pitch
/// contour codebook depends on bandwidth and frame size.
fn pitch_contour_icdf(fs_khz: u32, nb_subfr: usize) -> Result<&'static [u8]> {
    match (fs_khz, nb_subfr == MAX_NB_SUBFR) {
        (8, true) => Ok(&PITCH_CONTOUR_NB_ICDF),
        (8, false) => Ok(&PITCH_CONTOUR_10_MS_NB_ICDF),
        (_, true) => Ok(&PITCH_CONTOUR_ICDF),
        (_, false) => Ok(&PITCH_CONTOUR_10_MS_ICDF),
    }
}

/// Unpacks the per-coefficient entropy-table offsets and prediction
/// weights for a stage-1 NLSF codebook index — a direct port of
/// `silk_NLSF_unpack`. Only the first `order` entries of the returned
/// arrays are valid; `nlsf.rs` (Tier 2) reuses this for NLSF
/// reconstruction.
///
/// Returns `(ec_ix, pred_Q8)`, where `ec_ix[i]` indexes into
/// `NlsfCbStruct::ec_icdf` and `pred_Q8[i]` is the backward-prediction
/// coefficient for coefficient `i`.
pub(crate) fn nlsf_unpack(
    cb: &NlsfCbStruct,
    cb1_index: usize,
) -> ([usize; MAX_LPC_ORDER], [u8; MAX_LPC_ORDER]) {
    debug_assert!(cb1_index < cb.n_vectors as usize);
    let order = cb.order as usize;
    let mut ec_ix = [0usize; MAX_LPC_ORDER];
    let mut pred_q8 = [0u8; MAX_LPC_ORDER];
    let ec_sel = &cb.ec_sel[cb1_index * order / 2..];
    for i in (0..order).step_by(2) {
        let entry = ec_sel[i / 2];
        ec_ix[i] = ((entry >> 1) & 7) as usize * (2 * NLSF_QUANT_MAX_AMPLITUDE as usize + 1);
        pred_q8[i] = cb.pred_q8[i + (entry & 1) as usize * (order - 1)];
        ec_ix[i + 1] = ((entry >> 5) & 7) as usize * (2 * NLSF_QUANT_MAX_AMPLITUDE as usize + 1);
        pred_q8[i + 1] = cb.pred_q8[i + ((entry >> 4) & 1) as usize * (order - 1) + 1];
    }
    (ec_ix, pred_q8)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::range::RangeEncoder;
    use crate::silk::tables::{NLSF_CB_NB_MB, NLSF_CB_WB};

    /// Small deterministic PRNG (xorshift32) so the property test needs
    /// no external crate.
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

        fn below(&mut self, n: u32) -> u32 {
            self.next_u32() % n
        }
    }

    /// Highest symbol value an icdf table codes (the last entry is the
    /// 0 terminator).
    fn max_symbol(icdf: &[u8]) -> u32 {
        icdf.len() as u32 - 1
    }

    #[test]
    fn nlsf_unpack_matches_reference_spot_checks() {
        // NB/MB codebook, cb1_index 0: entries 16, 0, 0, 0, 0 select
        // prediction row 0 for the first pair's even coefficient and row
        // 1 (offset order-1) for its odd one; every table offset is 0.
        let (ec_ix, pred_q8) = nlsf_unpack(&NLSF_CB_NB_MB, 0);
        assert_eq!(ec_ix, [0; MAX_LPC_ORDER]);
        assert_eq!(
            &pred_q8[..10],
            &[179, 67, 140, 148, 151, 149, 153, 151, 163, 116]
        );

        // WB codebook, cb1_index 1: packed entries 100, 102, 102, 68,
        // 68, 36, 34, 96 (hand-traced through silk_NLSF_unpack).
        let (ec_ix, pred_q8) = nlsf_unpack(&NLSF_CB_WB, 1);
        assert_eq!(
            ec_ix,
            [18, 27, 27, 27, 27, 27, 18, 18, 18, 18, 18, 9, 9, 9, 0, 27]
        );
        assert_eq!(
            &pred_q8[..16],
            &[175, 148, 160, 176, 178, 173, 174, 164, 177, 174, 196, 182, 198, 192, 182, 68]
        );
    }

    #[test]
    fn nlsf_unpack_ec_offsets_stay_in_bounds_for_all_cb1_indices() {
        // ec_iCDF holds 8 rows of 2*NLSF_QUANT_MAX_AMPLITUDE+1 entries;
        // every packed offset must land on a row boundary in range.
        let row = 2 * NLSF_QUANT_MAX_AMPLITUDE as usize + 1;
        for cb in [&NLSF_CB_NB_MB, &NLSF_CB_WB] {
            let order = cb.order as usize;
            for cb1_index in 0..cb.n_vectors as usize {
                let (ec_ix, _) = nlsf_unpack(cb, cb1_index);
                for &off in ec_ix.iter().take(order) {
                    assert!(off % row == 0);
                    assert!(off + row <= cb.ec_icdf.len());
                }
            }
        }
    }

    /// One frame's worth of side info to round-trip.
    #[derive(Clone, Copy, Debug)]
    struct FrameSpec {
        vad_flag: bool,
        cond: CondCoding,
        signal_type: i8,
        quant_offset_type: i8,
        gains: [i8; MAX_NB_SUBFR],
        cb1_index: usize,
        /// Stage-2 NLSF residuals, in `-10..=10`.
        nlsf: [i8; MAX_LPC_ORDER],
        /// NLSF interpolation factor (ignored for 10 ms frames).
        interp: i8,
        /// Absolute lag index, or the delta-table symbol (1–20) when
        /// `lag_is_delta` (symbol 0 would have requested absolute
        /// coding, so it never appears as a delta here).
        lag: i16,
        lag_is_delta: bool,
        contour: i8,
        per_index: i8,
        ltp: [i8; MAX_NB_SUBFR],
        ltp_scale: i8,
        seed: i8,
    }

    /// Applies what `decode_indices` would write for `spec` onto `ix`
    /// (which carries the previous frame's values through, as in the
    /// reference channel state). Returns the new pitch-lag state.
    fn apply_spec(
        spec: &FrameSpec,
        ix: &mut SideInfoIndices,
        order: usize,
        nb_subfr: usize,
        prev_lag: i16,
    ) {
        ix.signal_type = spec.signal_type;
        ix.quant_offset_type = spec.quant_offset_type;
        ix.gains_indices[..nb_subfr].copy_from_slice(&spec.gains[..nb_subfr]);
        ix.nlsf_indices[0] = spec.cb1_index as i8;
        ix.nlsf_indices[1..=order].copy_from_slice(&spec.nlsf[..order]);
        ix.nlsf_interp_coef_q2 = if nb_subfr == MAX_NB_SUBFR {
            spec.interp
        } else {
            4
        };
        if spec.signal_type == TYPE_VOICED {
            ix.lag_index = if spec.lag_is_delta {
                prev_lag + spec.lag - 9
            } else {
                spec.lag
            };
            ix.contour_index = spec.contour;
            ix.per_index = spec.per_index;
            ix.ltp_index[..nb_subfr].copy_from_slice(&spec.ltp[..nb_subfr]);
            ix.ltp_scale_index = if spec.cond == CondCoding::Independently {
                spec.ltp_scale
            } else {
                0
            };
        }
        ix.seed = spec.seed;
    }

    /// Encoder-side mirror of `decode_indices`, written straight from
    /// the RFC order so the round-trip checks table use and slice
    /// offsets rather than sharing decode logic. `delta_possible` is
    /// "conditional coding follows a voiced frame", in which case the
    /// delta symbol is always coded (0 = "absolute lag follows").
    fn encode_frame(
        enc: &mut RangeEncoder,
        cb: &'static NlsfCbStruct,
        fs_khz: u32,
        nb_subfr: usize,
        delta_possible: bool,
        spec: &FrameSpec,
    ) {
        let order = cb.order as usize;

        /* Signal type and quantizer offset */
        let ix = ((spec.signal_type as u32) << 1) | spec.quant_offset_type as u32;
        if spec.vad_flag {
            enc.encode_icdf(ix - 2, &TYPE_OFFSET_VAD_ICDF, 8);
        } else {
            enc.encode_icdf(ix, &TYPE_OFFSET_NO_VAD_ICDF, 8);
        }

        /* Gains */
        if spec.cond == CondCoding::Conditionally {
            enc.encode_icdf(spec.gains[0] as u32, &DELTA_GAIN_ICDF, 8);
        } else {
            enc.encode_icdf(
                (spec.gains[0] >> 3) as u32,
                &GAIN_ICDF[spec.signal_type as usize],
                8,
            );
            enc.encode_icdf((spec.gains[0] & 7) as u32, &UNIFORM8_ICDF, 8);
        }
        for g in spec.gains.iter().take(nb_subfr).skip(1) {
            enc.encode_icdf(*g as u32, &DELTA_GAIN_ICDF, 8);
        }

        /* NLSF indices */
        enc.encode_icdf(
            spec.cb1_index as u32,
            &cb.cb1_icdf[(spec.signal_type as usize >> 1) * cb.n_vectors as usize..],
            8,
        );
        let (ec_ix, _) = nlsf_unpack(cb, spec.cb1_index);
        for (i, &residual) in spec.nlsf.iter().enumerate().take(order) {
            let s = residual as i32 + NLSF_QUANT_MAX_AMPLITUDE;
            if s <= 0 {
                // Escape below the quantization table.
                enc.encode_icdf(0, &cb.ec_icdf[ec_ix[i]..], 8);
                enc.encode_icdf((-s) as u32, &NLSF_EXT_ICDF, 8);
            } else if s >= 2 * NLSF_QUANT_MAX_AMPLITUDE {
                // Escape above the quantization table.
                enc.encode_icdf(
                    2 * NLSF_QUANT_MAX_AMPLITUDE as u32,
                    &cb.ec_icdf[ec_ix[i]..],
                    8,
                );
                enc.encode_icdf((s - 2 * NLSF_QUANT_MAX_AMPLITUDE) as u32, &NLSF_EXT_ICDF, 8);
            } else {
                enc.encode_icdf(s as u32, &cb.ec_icdf[ec_ix[i]..], 8);
            }
        }

        /* NLSF interpolation factor (20 ms frames only) */
        if nb_subfr == MAX_NB_SUBFR {
            enc.encode_icdf(spec.interp as u32, &NLSF_INTERPOLATION_FACTOR_ICDF, 8);
        }

        if spec.signal_type == TYPE_VOICED {
            /* Pitch lag */
            let mult = fs_khz >> 1;
            let encode_absolute = |enc: &mut RangeEncoder, lag: i16| {
                enc.encode_icdf(lag as u32 / mult, &PITCH_LAG_ICDF, 8);
                enc.encode_icdf(
                    lag as u32 % mult,
                    pitch_lag_low_bits_icdf(fs_khz).unwrap(),
                    8,
                );
            };
            if delta_possible {
                if spec.lag_is_delta {
                    enc.encode_icdf(spec.lag as u32, &PITCH_DELTA_ICDF, 8);
                } else {
                    /* Delta symbol 0 requests absolute coding. */
                    enc.encode_icdf(0, &PITCH_DELTA_ICDF, 8);
                    encode_absolute(enc, spec.lag);
                }
            } else {
                encode_absolute(enc, spec.lag);
            }

            /* Contour and LTP gains */
            enc.encode_icdf(
                spec.contour as u32,
                pitch_contour_icdf(fs_khz, nb_subfr).unwrap(),
                8,
            );
            enc.encode_icdf(spec.per_index as u32, &LTP_PER_INDEX_ICDF, 8);
            for ltp in spec.ltp.iter().take(nb_subfr) {
                enc.encode_icdf(*ltp as u32, LTP_GAIN_ICDF_PTRS[spec.per_index as usize], 8);
            }
            if spec.cond == CondCoding::Independently {
                enc.encode_icdf(spec.ltp_scale as u32, &LTPSCALE_ICDF, 8);
            }
        }

        /* Seed */
        enc.encode_icdf(spec.seed as u32, &UNIFORM4_ICDF, 8);
    }

    /// Encodes `specs` into one bitstream, decodes it frame by frame
    /// against a persistent `SideInfoIndices` (as the reference channel
    /// state does), and asserts exact equality every frame.
    fn round_trip_frames(
        specs: &[FrameSpec],
        cb: &'static NlsfCbStruct,
        fs_khz: u32,
        nb_subfr: usize,
    ) {
        let mut enc = RangeEncoder::new();
        let mut enc_prev_signal_type = TYPE_NO_VOICE_ACTIVITY;
        for spec in specs {
            let delta_possible =
                spec.cond == CondCoding::Conditionally && enc_prev_signal_type == TYPE_VOICED;
            encode_frame(&mut enc, cb, fs_khz, nb_subfr, delta_possible, spec);
            enc_prev_signal_type = spec.signal_type;
        }
        let data = enc.done();

        let mut dec = RangeDecoder::new(&data);
        let mut ec_state = EcPrevState::default();
        let mut current = SideInfoIndices::default();
        for (frame_index, spec) in specs.iter().enumerate() {
            let mut expected = current;
            apply_spec(
                spec,
                &mut expected,
                cb.order as usize,
                nb_subfr,
                ec_state.ec_prev_lag_index,
            );
            let params = FrameParams {
                nlsf_cb: cb,
                fs_khz,
                nb_subfr,
                frame_index,
                vad_flag: spec.vad_flag,
                decode_lbrr: false,
                cond_coding: spec.cond,
            };
            decode_indices(&mut dec, &mut current, &mut ec_state, &params)
                .unwrap_or_else(|e| panic!("frame {frame_index}: {e}"));
            assert_eq!(current, expected, "frame {frame_index} mismatch");
        }
    }

    #[test]
    fn round_trip_independent_voiced_wb() {
        let specs = [
            FrameSpec {
                vad_flag: true,
                cond: CondCoding::Independently,
                signal_type: TYPE_VOICED,
                quant_offset_type: 1,
                gains: [63, 39, 0, 17],
                cb1_index: 7,
                // Hit both escape extensions plus the plain range ends.
                nlsf: [-10, -5, -4, -1, 0, 1, 4, 5, 10, 3, -9, 9, 2, -2, 6, -6],
                interp: 4,
                lag: 16 * 8 + 7, // high 16, low 7 at 16 kHz
                lag_is_delta: false,
                contour: 33,
                per_index: 2,
                ltp: [31, 0, 15, 8],
                ltp_scale: 2,
                seed: 3,
            },
            FrameSpec {
                vad_flag: true,
                cond: CondCoding::Independently,
                signal_type: TYPE_UNVOICED,
                quant_offset_type: 0,
                gains: [45, 12, 30, 1],
                cb1_index: 20,
                nlsf: [0; MAX_LPC_ORDER],
                interp: 0,
                lag: 0, // unvoiced: pitch fields uncoded
                lag_is_delta: false,
                contour: 0,
                per_index: 0,
                ltp: [0; MAX_NB_SUBFR],
                ltp_scale: 0,
                seed: 1,
            },
        ];
        round_trip_frames(&specs, &NLSF_CB_WB, 16, MAX_NB_SUBFR);
    }

    #[test]
    fn round_trip_conditional_voiced_nb() {
        let specs = [
            // Frame 0: independent, absolute lag.
            FrameSpec {
                vad_flag: true,
                cond: CondCoding::Independently,
                signal_type: TYPE_VOICED,
                quant_offset_type: 0,
                gains: [20, 5, 5, 5],
                cb1_index: 3,
                nlsf: [1, -1, 2, -2, 3, -3, 0, 4, -4, 0, 2, -2, 1, -1, 3, -3],
                interp: 2,
                lag: 4 * 10 + 3, // high 10, low 3 at 8 kHz
                lag_is_delta: false,
                contour: 10,
                per_index: 1,
                ltp: [7, 3, 9, 14],
                ltp_scale: 1,
                seed: 2,
            },
            // Frame 1: conditionally-coded against a voiced predecessor
            // with a nonzero delta table symbol (12 → lag +3).
            FrameSpec {
                vad_flag: true,
                cond: CondCoding::Conditionally,
                signal_type: TYPE_VOICED,
                quant_offset_type: 1,
                gains: [11, 8, 39, 2],
                cb1_index: 30,
                nlsf: [0; MAX_LPC_ORDER],
                interp: 3,
                lag: 12,
                lag_is_delta: true,
                contour: 5,
                per_index: 0,
                ltp: [5, 6, 7, 2],
                ltp_scale: 0, // not coded conditionally
                seed: 0,
            },
            // Frame 2: unvoiced — first gain still delta-coded, pitch
            // fields keep their previous values.
            FrameSpec {
                vad_flag: false,
                cond: CondCoding::Conditionally,
                signal_type: TYPE_NO_VOICE_ACTIVITY,
                quant_offset_type: 0,
                gains: [7, 7, 7, 7],
                cb1_index: 0,
                nlsf: [1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1],
                interp: 1,
                lag: 0,
                lag_is_delta: false,
                contour: 0,
                per_index: 0,
                ltp: [0; MAX_NB_SUBFR],
                ltp_scale: 0,
                seed: 2,
            },
        ];
        round_trip_frames(&specs, &NLSF_CB_NB_MB, 8, MAX_NB_SUBFR);
    }

    #[test]
    fn round_trip_10ms_mb() {
        // 10 ms frames: two subframes, no interpolation symbol (forced
        // to 4), 10 ms contour table.
        let specs = [FrameSpec {
            vad_flag: true,
            cond: CondCoding::Independently,
            signal_type: TYPE_VOICED,
            quant_offset_type: 1,
            gains: [30, 20, 0, 0],
            cb1_index: 11,
            nlsf: [2, -2, 5, -5, 8, -8, 10, -10, 1, 0, -1, 0, 3, -3, 4, -4],
            interp: 0,      // ignored by the decoder for 10 ms frames
            lag: 6 * 4 + 2, // high 6, low 2 at 12 kHz
            lag_is_delta: false,
            contour: 11,
            per_index: 2,
            ltp: [20, 4, 0, 0],
            ltp_scale: 2,
            seed: 1,
        }];
        round_trip_frames(&specs, &NLSF_CB_NB_MB, 12, MAX_NB_SUBFR / 2);
    }

    /// Randomized round-trip over all bandwidth/frame-size combinations,
    /// sequenced so the cross-frame state (prev signal type, prev lag)
    /// is exercised.
    #[test]
    fn random_round_trip_property() {
        let configs: [(&'static NlsfCbStruct, u32, usize); 3] = [
            (&NLSF_CB_NB_MB, 8, MAX_NB_SUBFR),
            (&NLSF_CB_NB_MB, 12, MAX_NB_SUBFR / 2),
            (&NLSF_CB_WB, 16, MAX_NB_SUBFR),
        ];
        for (cb, fs_khz, nb_subfr) in configs {
            let mut rng = XorShift(0x2545_F491 ^ (fs_khz << 16));
            let mult = fs_khz >> 1;
            for _ in 0..12 {
                let mut specs = Vec::new();
                // Local mirror of the decoder's cross-frame state.
                let mut prev_signal_type = TYPE_NO_VOICE_ACTIVITY;
                for frame in 0..3 {
                    let vad_flag = rng.next_u32() & 1 != 0;
                    let signal_type = if vad_flag {
                        TYPE_UNVOICED + rng.below(2) as i8
                    } else {
                        TYPE_NO_VOICE_ACTIVITY
                    };
                    let cond = if frame > 0 && rng.below(2) == 0 {
                        CondCoding::Conditionally
                    } else {
                        CondCoding::Independently
                    };
                    let mut gains = [0i8; MAX_NB_SUBFR];
                    gains[0] = if cond == CondCoding::Conditionally {
                        rng.below(40) as i8
                    } else {
                        rng.below(64) as i8
                    };
                    for g in gains.iter_mut().skip(1) {
                        *g = rng.below(40) as i8;
                    }
                    let cb1_index = rng.below(cb.n_vectors as u32) as usize;
                    let mut nlsf = [0i8; MAX_LPC_ORDER];
                    for r in nlsf.iter_mut().take(cb.order as usize) {
                        *r = rng.below(21) as i8 - 10;
                    }
                    // Pitch: delta-coded only when the decoder would take
                    // that path; keep absolute lags >= 2*mult so lag-9
                    // deltas stay non-negative.
                    let (lag, lag_is_delta) = if signal_type == TYPE_VOICED {
                        if cond == CondCoding::Conditionally
                            && prev_signal_type == TYPE_VOICED
                            && rng.below(2) == 1
                        {
                            (1 + rng.below(20) as i16, true)
                        } else {
                            let low = rng.below(mult) as i16;
                            ((2 + rng.below(30) as i16) * mult as i16 + low, false)
                        }
                    } else {
                        (0, false)
                    };
                    let per_index = rng.below(3) as usize;
                    let mut ltp = [0i8; MAX_NB_SUBFR];
                    if signal_type == TYPE_VOICED {
                        for l in ltp.iter_mut().take(nb_subfr) {
                            *l = rng.below(max_symbol(LTP_GAIN_ICDF_PTRS[per_index])) as i8;
                        }
                    }
                    let spec = FrameSpec {
                        vad_flag,
                        cond,
                        signal_type,
                        quant_offset_type: rng.below(2) as i8,
                        gains,
                        cb1_index,
                        nlsf,
                        interp: rng.below(5) as i8,
                        lag,
                        lag_is_delta,
                        contour: if signal_type == TYPE_VOICED {
                            rng.below(pitch_contour_icdf(fs_khz, nb_subfr).unwrap().len() as u32)
                                as i8
                        } else {
                            0
                        },
                        per_index: per_index as i8,
                        ltp,
                        ltp_scale: rng.below(3) as i8,
                        seed: rng.below(4) as i8,
                    };
                    prev_signal_type = signal_type;
                    specs.push(spec);
                }
                round_trip_frames(&specs, cb, fs_khz, nb_subfr);
            }
        }
    }

    #[test]
    fn vad_flags_and_lbrr_flag_round_trip() {
        for n_frames in 1..=MAX_FRAMES_PER_PACKET {
            let mut enc = RangeEncoder::new();
            let mut expected_vad = [false; MAX_FRAMES_PER_PACKET];
            for (i, flag) in expected_vad.iter_mut().take(n_frames).enumerate() {
                *flag = (i % 2 == 1) || n_frames == 1;
                enc.encode_bit_logp(*flag, 1);
            }
            let expected_lbrr = n_frames != 2;
            enc.encode_bit_logp(expected_lbrr, 1);
            let data = enc.done();

            let (vad, lbrr) =
                decode_vad_flags_and_lbrr_flag(&mut RangeDecoder::new(&data), n_frames).unwrap();
            assert_eq!(vad, expected_vad);
            assert_eq!(lbrr, expected_lbrr);
        }
    }

    #[test]
    fn lbrr_flags_pattern_decoding() {
        // Single-frame packets imply flag[0] = 1 with no bitstream reads.
        let flags = decode_lbrr_flags(&mut RangeDecoder::new(&[]), 1).unwrap();
        assert_eq!(flags, [true, false, false]);

        // Multi-frame packets: one symbol whose low n_frames bits of
        // (symbol + 1) give the per-frame flags.
        for n_frames in 2..=MAX_FRAMES_PER_PACKET {
            let table = LBRR_FLAGS_ICDF_PTR[n_frames - 2];
            for symbol in 0..table.len() - 1 {
                let mut enc = RangeEncoder::new();
                enc.encode_icdf(symbol as u32, table, 8);
                let data = enc.done();
                let flags = decode_lbrr_flags(&mut RangeDecoder::new(&data), n_frames).unwrap();
                for (i, flag) in flags.iter().take(n_frames).enumerate() {
                    assert_eq!(*flag, (symbol as u32 + 1) >> i & 1 != 0);
                }
                assert!(!flags[n_frames..].iter().any(|f| *f));
            }
        }
    }
}
