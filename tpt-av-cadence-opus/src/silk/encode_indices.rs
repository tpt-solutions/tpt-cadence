//! SILK per-frame side-information index encoding — bit-exact mirror of
//! the decoder's `decode_indices` path (RFC 6716 §4.2.4–§4.2.5).
//!
//! Ports the reference encoder half (`silk/encode_indices.c` plus the
//! VAD/LBRR flag prologue of `silk/enc_API.c`):
//!
//! - [`encode_indices`]: writes, in the decoder's exact symbol order, the
//!   frame type + quantization offset, the gain indices (absolute
//!   two-stage for the first subframe of an independently coded frame,
//!   delta otherwise), the NLSF stage-1 vector + stage-2 residuals with
//!   the escape extension at both table ends, the NLSF interpolation
//!   factor (20 ms frames only), and — for voiced frames — the pitch lag
//!   (absolute, or the delta-table symbol when the decoder would take
//!   that path), pitch contour, LTP periodicity + per-subframe filter
//!   indices, and the LTP scale (independent coding only), then the 2-bit
//!   seed.
//! - [`encode_vad_flags_and_lbrr_flag`] / [`encode_lbrr_flags`]: the
//!   once-per-payload prologue.
//!
//! Cross-frame state mirrors the decoder's `ec_prevSignalType` /
//! `ec_prevLagIndex` pair so a delta-coded pitch lag can be selected when
//! the decoder would accept it.
//!
//! SOURCE: Xiph.Org libopus 1.5.2, `silk/encode_indices.c`,
//! `silk/enc_API.c`, `silk/NLSF_unpack.c` (shared with the decode side),
//! `silk/define.h` (BSD-3-Clause).
#![allow(dead_code)]

use crate::range::RangeEncoder;
use crate::silk::decode_indices::{
    nlsf_unpack, CondCoding, EcPrevState, FrameParams, SideInfoIndices, MAX_FRAMES_PER_PACKET,
    MAX_NB_SUBFR, NLSF_QUANT_MAX_AMPLITUDE, TYPE_VOICED,
};
use crate::silk::tables::{
    DELTA_GAIN_ICDF, GAIN_ICDF, LBRR_FLAGS_ICDF_PTR, LTPSCALE_ICDF, LTP_GAIN_ICDF_PTRS,
    LTP_PER_INDEX_ICDF, NLSF_EXT_ICDF, NLSF_INTERPOLATION_FACTOR_ICDF, PITCH_CONTOUR_10_MS_ICDF,
    PITCH_CONTOUR_10_MS_NB_ICDF, PITCH_CONTOUR_ICDF, PITCH_CONTOUR_NB_ICDF, PITCH_DELTA_ICDF,
    PITCH_LAG_ICDF, TYPE_OFFSET_NO_VAD_ICDF, TYPE_OFFSET_VAD_ICDF, UNIFORM4_ICDF, UNIFORM6_ICDF,
    UNIFORM8_ICDF,
};

/// Encodes the per-frame side-information indices — mirror of
/// [`super::decode_indices::decode_indices`].
///
/// `delta_possible` must be `(cond_coding == Conditionally) &&
/// (ec_state.ec_prev_signal_type == TYPE_VOICED)`; when true the encoder
/// may pick the delta-coded pitch lag (choosing the absolute form costs
/// the extra "delta symbol 0" marker, exactly as the decoder expects).
pub(crate) fn encode_indices(
    enc: &mut RangeEncoder,
    indices: &SideInfoIndices,
    ec_state: &mut EcPrevState,
    params: &FrameParams,
    delta_possible: bool,
) {
    debug_assert!(params.nb_subfr == MAX_NB_SUBFR || params.nb_subfr == MAX_NB_SUBFR / 2);
    let order = params.nlsf_cb.order as usize;

    /*******************************************/
    /* Encode signal type and quantizer offset */
    /*******************************************/
    let ix = ((indices.signal_type as u32) << 1) | indices.quant_offset_type as u32;
    if params.decode_lbrr || params.vad_flag {
        enc.encode_icdf(ix - 2, &TYPE_OFFSET_VAD_ICDF, 8);
    } else {
        enc.encode_icdf(ix, &TYPE_OFFSET_NO_VAD_ICDF, 8);
    }

    /****************/
    /* Encode gains */
    /****************/
    if params.cond_coding == CondCoding::Conditionally {
        enc.encode_icdf(indices.gains_indices[0] as u32, &DELTA_GAIN_ICDF, 8);
    } else {
        enc.encode_icdf(
            (indices.gains_indices[0] >> 3) as u32,
            &GAIN_ICDF[indices.signal_type as usize],
            8,
        );
        enc.encode_icdf((indices.gains_indices[0] & 7) as u32, &UNIFORM8_ICDF, 8);
    }
    for g in indices.gains_indices.iter().take(params.nb_subfr).skip(1) {
        enc.encode_icdf(*g as u32, &DELTA_GAIN_ICDF, 8);
    }

    /**********************/
    /* Encode LSF Indices */
    /**********************/
    let cb1_index = indices.nlsf_indices[0] as usize;
    enc.encode_icdf(
        cb1_index as u32,
        &params.nlsf_cb.cb1_icdf
            [(indices.signal_type as usize >> 1) * params.nlsf_cb.n_vectors as usize..],
        8,
    );
    let (ec_ix, _) = nlsf_unpack(params.nlsf_cb, cb1_index);
    for (i, &residual) in indices.nlsf_indices.iter().skip(1).enumerate().take(order) {
        let s = residual as i32 + NLSF_QUANT_MAX_AMPLITUDE;
        if s <= 0 {
            /* Escape below the quantization table. */
            enc.encode_icdf(0, &params.nlsf_cb.ec_icdf[ec_ix[i]..], 8);
            enc.encode_icdf((-s) as u32, &NLSF_EXT_ICDF, 8);
        } else if s >= 2 * NLSF_QUANT_MAX_AMPLITUDE {
            /* Escape above the quantization table. */
            enc.encode_icdf(
                2 * NLSF_QUANT_MAX_AMPLITUDE as u32,
                &params.nlsf_cb.ec_icdf[ec_ix[i]..],
                8,
            );
            enc.encode_icdf((s - 2 * NLSF_QUANT_MAX_AMPLITUDE) as u32, &NLSF_EXT_ICDF, 8);
        } else {
            enc.encode_icdf(s as u32, &params.nlsf_cb.ec_icdf[ec_ix[i]..], 8);
        }
    }

    /* Encode LSF interpolation factor */
    if params.nb_subfr == MAX_NB_SUBFR {
        enc.encode_icdf(
            indices.nlsf_interp_coef_q2 as u32,
            &NLSF_INTERPOLATION_FACTOR_ICDF,
            8,
        );
    }

    if indices.signal_type == TYPE_VOICED {
        /*********************/
        /* Encode pitch lags */
        /*********************/
        let mult = params.fs_khz >> 1;
        let encode_absolute = |enc: &mut RangeEncoder, lag: i16| {
            enc.encode_icdf((lag as u32) / mult, &PITCH_LAG_ICDF, 8);
            enc.encode_icdf(
                (lag as u32) % mult,
                pitch_lag_low_bits_icdf(params.fs_khz),
                8,
            );
        };
        let mut encode_absolute_lag_index = true;
        if delta_possible {
            /* The decoder reads one delta-table symbol: 0 selects the
             * absolute form, 1..=20 selects `prev + delta - 9`. */
            let delta = indices.lag_index as i32 - ec_state.ec_prev_lag_index as i32 + 9;
            if (1..=20).contains(&delta) {
                enc.encode_icdf(delta as u32, &PITCH_DELTA_ICDF, 8);
                encode_absolute_lag_index = false;
            } else {
                enc.encode_icdf(0, &PITCH_DELTA_ICDF, 8);
            }
        }
        if encode_absolute_lag_index {
            encode_absolute(enc, indices.lag_index);
        }
        ec_state.ec_prev_lag_index = indices.lag_index;

        /* Contour and LTP gains */
        enc.encode_icdf(
            indices.contour_index as u32,
            pitch_contour_icdf(params.fs_khz, params.nb_subfr),
            8,
        );
        enc.encode_icdf(indices.per_index as u32, &LTP_PER_INDEX_ICDF, 8);
        for ltp in indices.ltp_index.iter().take(params.nb_subfr) {
            enc.encode_icdf(
                *ltp as u32,
                LTP_GAIN_ICDF_PTRS[indices.per_index as usize],
                8,
            );
        }

        /**********************/
        /* Encode LTP scaling */
        /**********************/
        if params.cond_coding == CondCoding::Independently {
            enc.encode_icdf(indices.ltp_scale_index as u32, &LTPSCALE_ICDF, 8);
        }
    }
    ec_state.ec_prev_signal_type = indices.signal_type;

    /***************/
    /* Encode seed */
    /***************/
    enc.encode_icdf(indices.seed as u32, &UNIFORM4_ICDF, 8);
}

/// Encodes the per-frame VAD flags and the packet-level LBRR flag — the
/// once-per-payload prologue of `silk_Decode`'s first decoder call.
pub(crate) fn encode_vad_flags_and_lbrr_flag(
    enc: &mut RangeEncoder,
    vad_flags: &[bool; MAX_FRAMES_PER_PACKET],
    n_frames: usize,
    lbrr_flag: bool,
) {
    debug_assert!((1..=MAX_FRAMES_PER_PACKET).contains(&n_frames));
    for flag in vad_flags.iter().take(n_frames) {
        enc.encode_bit_logp(*flag, 1);
    }
    enc.encode_bit_logp(lbrr_flag, 1);
}

/// Encodes the per-frame LBRR flags of a packet whose packet-level LBRR
/// flag is set. With a single frame the flag is implicitly 1 (no bits are
/// written); otherwise one ICDF symbol carries the whole pattern.
pub(crate) fn encode_lbrr_flags(
    enc: &mut RangeEncoder,
    flags: &[bool; MAX_FRAMES_PER_PACKET],
    n_frames: usize,
) {
    debug_assert!((1..=MAX_FRAMES_PER_PACKET).contains(&n_frames));
    if n_frames == 1 {
        return;
    }
    let table = LBRR_FLAGS_ICDF_PTR[n_frames - 2];
    let mut symbol = 0u32;
    for (i, flag) in flags.iter().take(n_frames).enumerate() {
        if *flag {
            symbol |= 1 << i;
        }
    }
    enc.encode_icdf(symbol - 1, table, 8);
}

/// Decoder's `pitch_lag_low_bits_iCDF` selection (shared with
/// `decode_indices`, which owns the table-choice contract).
fn pitch_lag_low_bits_icdf(fs_khz: u32) -> &'static [u8] {
    match fs_khz {
        8 => &UNIFORM4_ICDF,
        12 => &UNIFORM6_ICDF,
        16 => &UNIFORM8_ICDF,
        _ => unreachable!("fs_khz validated by FrameParams"),
    }
}

/// Decoder's `pitch_contour_iCDF` selection.
fn pitch_contour_icdf(fs_khz: u32, nb_subfr: usize) -> &'static [u8] {
    match (fs_khz, nb_subfr == MAX_NB_SUBFR) {
        (8, true) => &PITCH_CONTOUR_NB_ICDF,
        (8, false) => &PITCH_CONTOUR_10_MS_NB_ICDF,
        (_, true) => &PITCH_CONTOUR_ICDF,
        (_, false) => &PITCH_CONTOUR_10_MS_ICDF,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::range::RangeDecoder;
    use crate::silk::decode_indices::{decode_indices, MAX_LPC_ORDER, TYPE_NO_VOICE_ACTIVITY};
    use crate::silk::tables::{NlsfCbStruct, NLSF_CB_NB_MB, NLSF_CB_WB};

    /// Encodes a frame's side info through [`encode_indices`], decodes it
    /// back through the real decoder, and requires exact equality of every
    /// decoded field (including fields carried over from the previous
    /// frame, which the encoder intentionally does not touch).
    #[test]
    fn round_trip_matches_decode_indices() {
        let configs: [(&'static NlsfCbStruct, u32, usize); 3] = [
            (&NLSF_CB_WB, 16, MAX_NB_SUBFR),
            (&NLSF_CB_NB_MB, 8, MAX_NB_SUBFR),
            (&NLSF_CB_NB_MB, 12, MAX_NB_SUBFR / 2),
        ];
        for (cb, fs_khz, nb_subfr) in configs {
            for voiced_delta in [false, true] {
                let mut enc = RangeEncoder::new();
                let mut enc_ec = EcPrevState {
                    ec_prev_signal_type: if voiced_delta {
                        TYPE_VOICED
                    } else {
                        TYPE_NO_VOICE_ACTIVITY
                    },
                    ec_prev_lag_index: 100,
                };
                let order = cb.order as usize;

                // Frame 1: voiced, independently coded (or conditionally
                // with a voiced predecessor when `voiced_delta`).
                let cond = if voiced_delta {
                    CondCoding::Conditionally
                } else {
                    CondCoding::Independently
                };
                let ix1 = SideInfoIndices {
                    signal_type: TYPE_VOICED,
                    quant_offset_type: 1,
                    // Delta coding (conditional) caps the first symbol at
                    // 40; the absolute form spans 0..=63.
                    gains_indices: [if voiced_delta { 35 } else { 63 }, 40, 0, 39],
                    nlsf_indices: {
                        let mut n = [0i8; MAX_LPC_ORDER + 1];
                        n[0] = 7;
                        // Both escapes plus interior values.
                        n[1..=order].copy_from_slice(
                            &[-10, -5, 0, 5, 10, 3, -3, 1, -1, 4, 0, -8, 8, 2, -2, 6][..order],
                        );
                        n
                    },
                    nlsf_interp_coef_q2: 2,
                    // 93 is encodable at every bandwidth (high part
                    // 93/(fs/2) < 32) and 93 - 100 + 9 = 2 lands in the
                    // delta table's 1..=20 range for the conditional case.
                    lag_index: 93,
                    contour_index: 1,
                    per_index: 2,
                    ltp_index: [31, 0, 15, 8],
                    ltp_scale_index: 1,
                    seed: 2,
                };
                let params1 = FrameParams {
                    nlsf_cb: cb,
                    fs_khz,
                    nb_subfr,
                    frame_index: 0,
                    vad_flag: true,
                    decode_lbrr: false,
                    cond_coding: cond,
                };
                let delta_possible = voiced_delta;
                encode_indices(&mut enc, &ix1, &mut enc_ec, &params1, delta_possible);

                // Frame 2: unvoiced (pitch fields keep previous values on
                // decode; gains delta-coded).
                let ix2 = SideInfoIndices {
                    signal_type: 0,
                    quant_offset_type: 0,
                    gains_indices: [12, 7, 7, 7],
                    nlsf_indices: {
                        let mut n = [0i8; MAX_LPC_ORDER + 1];
                        n[0] = 0;
                        n
                    },
                    nlsf_interp_coef_q2: 4,
                    seed: 3,
                    ..ix1
                };
                let params2 = FrameParams {
                    frame_index: 1,
                    vad_flag: false,
                    cond_coding: CondCoding::Conditionally,
                    ..params1
                };
                encode_indices(&mut enc, &ix2, &mut enc_ec, &params2, false);

                let enc_tell = enc.tell();
                let data = enc.done();
                let mut dec = RangeDecoder::new(&data);
                let mut dec_ec = EcPrevState {
                    ec_prev_signal_type: if voiced_delta {
                        TYPE_VOICED
                    } else {
                        TYPE_NO_VOICE_ACTIVITY
                    },
                    ec_prev_lag_index: 100,
                };
                let mut current = SideInfoIndices::default();

                let mut got1 = current;
                decode_indices(&mut dec, &mut got1, &mut dec_ec, &params1).unwrap();
                // Both coding paths reconstruct lag 93: absolute, or the
                // delta symbol 2 against the previous lag 100.
                let want1 = ix1;
                assert_eq!(got1.signal_type, want1.signal_type);
                assert_eq!(got1.quant_offset_type, want1.quant_offset_type);
                // Only the coded prefix is transmitted; the tail carries over.
                assert_eq!(
                    &got1.gains_indices[..nb_subfr],
                    &want1.gains_indices[..nb_subfr]
                );
                assert_eq!(&got1.nlsf_indices[..=order], &want1.nlsf_indices[..=order]);
                if nb_subfr == MAX_NB_SUBFR {
                    assert_eq!(got1.nlsf_interp_coef_q2, want1.nlsf_interp_coef_q2);
                } else {
                    assert_eq!(got1.nlsf_interp_coef_q2, 4);
                }
                assert_eq!(got1.lag_index, want1.lag_index);
                assert_eq!(got1.contour_index, want1.contour_index);
                assert_eq!(got1.per_index, want1.per_index);
                assert_eq!(&got1.ltp_index[..nb_subfr], &want1.ltp_index[..nb_subfr]);
                if cond == CondCoding::Independently {
                    assert_eq!(got1.ltp_scale_index, want1.ltp_scale_index);
                } else {
                    assert_eq!(got1.ltp_scale_index, 0);
                }
                assert_eq!(got1.seed, want1.seed);
                current = got1;

                let mut got2 = current;
                decode_indices(&mut dec, &mut got2, &mut dec_ec, &params2).unwrap();
                assert_eq!(got2.signal_type, ix2.signal_type);
                assert_eq!(got2.quant_offset_type, ix2.quant_offset_type);
                // Unvoiced: pitch fields keep the previous frame's values
                // (decode_indices only reads them when voiced; the unvoiced
                // zeroing happens later in decode_parameters).
                assert_eq!(got2.lag_index, current.lag_index);
                assert_eq!(got2.contour_index, current.contour_index);
                assert_eq!(got2.per_index, current.per_index);
                assert_eq!(got2.seed, ix2.seed);

                // The decoder must have consumed exactly the written bits.
                assert_eq!(dec.tell(), enc_tell);
            }
        }
    }

    #[test]
    fn vad_and_lbrr_flags_round_trip() {
        for n_frames in 1..=MAX_FRAMES_PER_PACKET {
            let vad = [true, false, true];
            let lbrr = n_frames == 3;
            let mut enc = RangeEncoder::new();
            encode_vad_flags_and_lbrr_flag(&mut enc, &vad, n_frames, lbrr);
            let flags = [lbrr, false, true];
            if lbrr {
                encode_lbrr_flags(&mut enc, &flags, n_frames);
            }
            let data = enc.done();

            let mut dec = RangeDecoder::new(&data);
            let (got_vad, got_lbrr) =
                crate::silk::decode_indices::decode_vad_flags_and_lbrr_flag(&mut dec, n_frames)
                    .unwrap();
            assert_eq!(&got_vad[..n_frames], &vad[..n_frames]);
            assert_eq!(got_lbrr, lbrr);
            if lbrr {
                let got_flags =
                    crate::silk::decode_indices::decode_lbrr_flags(&mut dec, n_frames).unwrap();
                assert_eq!(&got_flags[..n_frames], &flags[..n_frames]);
            }
        }
    }
}
