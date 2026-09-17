//! SILK pitch lag reconstruction and LTP filter-codebook lookup
//! (RFC 6716 §4.2.7.6).
//!
//! Ports `silk/decode_pitch.c` (`silk_decode_pitch`) plus the voiced
//! branch of `silk/decode_parameters.c` that turns the per-subframe LTP
//! codebook indices and the LTP scale index into the Q14 values the
//! synthesis consumes. The *bitstream* side of the pitch parameters —
//! the absolute/relative (delta) primary lag index and the contour,
//! periodicity, and per-subframe LTP index symbols — is entropy-decoded
//! by [`super::decode_indices`] into the `lag_index`, `contour_index`,
//! `per_index`, `ltp_index`, and `ltp_scale_index` fields of
//! `SideInfoIndices`; this module owns the reconstruction:
//!
//! - **Primary lag → per-subframe lags.** The primary lag index counts
//!   samples above `2 ms · fs_kHz` (the encoder's pitch search floor,
//!   `PE_MIN_LAG_MS`); it was assembled by `decode_indices` as
//!   `lag_high * fs_kHz/2 + lag_low` (absolute) or
//!   `prev_lag_index + delta − 9` (relative). Each subframe's lag is
//!   the primary lag plus a small VQ-contour offset
//!   (`CB_LAGS_STAGE*`), clamped to the encoder's 2–18 ms search range
//!   — RFC 6716 §4.2.7.6.1, Tables 33–36.
//! - **LTP filter coefficients.** Each voiced subframe uses a 5-tap
//!   pitch predictor whose taps come from one of three codebooks
//!   selected by the periodicity index (`LTP_VQ_PTRS_Q7`, RFC Tables
//!   39–41). The Q7 taps are widened to Q14 by `<< 7`
//!   (`silk/decode_parameters.c`).
//! - **LTP scaling.** The Q14 scale factor (RFC §4.2.7.6.3, values
//!   15565/12288/8192 ≈ 0.95/0.75/0.5) that dampens the long-term
//!   prediction to trade prediction gain against packet-loss recovery.
//!
//! For unvoiced frames the caller (Tier 4's `decode_parameters` port)
//! zeroes the lag/LTP arrays instead, matching the reference's `else`
//! branch — nothing here needs to run.
//!
//! SOURCE: Xiph.Org libopus 1.5.2, `silk/decode_pitch.c`,
//! `silk/decode_parameters.c`, `silk/pitch_est_defines.h`,
//! `silk/define.h` (BSD-3-Clause).
#![allow(dead_code)]

use crate::silk::decode_indices::MAX_NB_SUBFR;
use crate::silk::sigproc::smulbb;
use crate::silk::tables::{
    CB_LAGS_STAGE2, CB_LAGS_STAGE2_10_MS, CB_LAGS_STAGE3, CB_LAGS_STAGE3_10_MS,
    LTPSCALES_TABLE_Q14, LTP_VQ_PTRS_Q7,
};
use crate::{CadenceError, Result};

/// `LTP_ORDER` (`silk/define.h`): taps per subframe's long-term
/// predictor.
pub(crate) const LTP_ORDER: usize = 5;

/// `PE_MIN_LAG_MS` / `PE_MAX_LAG_MS` (`silk/pitch_est_defines.h`): the
/// encoder's pitch search range in ms — 500 Hz down to 55.6 Hz. The
/// primary lag index counts samples above `PE_MIN_LAG_MS · fs_kHz` and
/// the reconstructed subframe lags are clamped to
/// `[PE_MIN_LAG_MS · fs_kHz, PE_MAX_LAG_MS · fs_kHz]`.
const PE_MIN_LAG_MS: i32 = 2;
const PE_MAX_LAG_MS: i32 = 18;

/// Reconstructs the per-subframe pitch lags (in samples at the internal
/// rate) from the primary lag index and the contour codebook index — a
/// direct port of `silk_decode_pitch`.
///
/// Writes exactly `nb_subfr` entries of `pitch_lags` (2 for 10 ms
/// frames, 4 for 20 ms); the remaining entries are left untouched,
/// matching the reference's caller-owned array.
///
/// `fs_kHz` selects NB's stage-2 contour codebooks (11 offsets for 20 ms
/// frames, 3 for 10 ms) versus MB/WB's stage-3 ones (34/12); the same
/// selection drove the contour ICDF `decode_indices` decoded
/// `contour_index` from, so a structurally consistent call always has
/// `contour_index` in range (asserted in debug builds).
pub(crate) fn decode_pitch(
    lag_index: i16,
    contour_index: i8,
    pitch_lags: &mut [i32; MAX_NB_SUBFR],
    fs_khz: u32,
    nb_subfr: usize,
) -> Result<()> {
    debug_assert!(nb_subfr == MAX_NB_SUBFR || nb_subfr == MAX_NB_SUBFR / 2);
    if !matches!(fs_khz, 8 | 12 | 16) {
        return Err(CadenceError::CorruptData(format!(
            "unsupported SILK internal sample rate {fs_khz} kHz"
        )));
    }
    let cbk_size = if fs_khz == 8 {
        if nb_subfr == MAX_NB_SUBFR {
            CB_LAGS_STAGE2[0].len() // PE_NB_CBKS_STAGE2_EXT
        } else {
            CB_LAGS_STAGE2_10_MS[0].len() // PE_NB_CBKS_STAGE2_10MS
        }
    } else if nb_subfr == MAX_NB_SUBFR {
        CB_LAGS_STAGE3[0].len() // PE_NB_CBKS_STAGE3_MAX
    } else {
        CB_LAGS_STAGE3_10_MS[0].len() // PE_NB_CBKS_STAGE3_10MS
    };
    let ci = contour_index as usize;
    debug_assert!(ci < cbk_size, "contour index {ci} out of codebook range");

    let min_lag = smulbb(PE_MIN_LAG_MS, fs_khz as i32);
    let max_lag = smulbb(PE_MAX_LAG_MS, fs_khz as i32);
    let lag = min_lag + i32::from(lag_index);

    for (k, out) in pitch_lags.iter_mut().enumerate().take(nb_subfr) {
        /* matrix_ptr(Lag_CB_ptr, k, contourIndex, cbk_size): row k,
         * column contourIndex of the reference's row-major [nb_subfr]
         * x [cbk_size] codebook. */
        let offset = if fs_khz == 8 {
            if nb_subfr == MAX_NB_SUBFR {
                CB_LAGS_STAGE2[k][ci]
            } else {
                CB_LAGS_STAGE2_10_MS[k][ci]
            }
        } else if nb_subfr == MAX_NB_SUBFR {
            CB_LAGS_STAGE3[k][ci]
        } else {
            CB_LAGS_STAGE3_10_MS[k][ci]
        };
        *out = (lag + i32::from(offset)).clamp(min_lag, max_lag);
    }
    Ok(())
}

/// Looks up the voiced subframes' 5-tap LTP predictor coefficients in
/// the Q7 codebook selected by the periodicity index and widens them to
/// Q14 — the "Decode Codebook Index" block of the voiced branch of
/// `silk_decode_parameters` (`silk/decode_parameters.c`).
///
/// Writes exactly `nb_subfr * [`LTP_ORDER`]` entries of `ltp_coefs_q14`
/// and leaves the tail untouched, mirroring the reference's in-place
/// `psDecCtrl->LTPCoef_Q14` (the unvoiced branch zeroes the same span).
/// `ltp_index[k]` must be within codebook `per_index` (guaranteed by
/// `decode_indices`' ICDF table choice; asserted in debug builds).
pub(crate) fn ltp_coefs_q14(
    per_index: i8,
    ltp_index: &[i8; MAX_NB_SUBFR],
    ltp_coefs_q14: &mut [i16; MAX_NB_SUBFR * LTP_ORDER],
    nb_subfr: usize,
) {
    debug_assert!(nb_subfr == MAX_NB_SUBFR || nb_subfr == MAX_NB_SUBFR / 2);
    debug_assert!((0..LTP_VQ_PTRS_Q7.len() as i8).contains(&per_index));
    let cbk_q7 = &LTP_VQ_PTRS_Q7[per_index as usize];
    for k in 0..nb_subfr {
        let ix = ltp_index[k] as usize;
        debug_assert!(ix < cbk_q7.len(), "LTP index {ix} out of codebook range");
        for (i, coef) in ltp_coefs_q14[k * LTP_ORDER..(k + 1) * LTP_ORDER]
            .iter_mut()
            .enumerate()
        {
            /* silk_LSHIFT(cbk_ptr_Q7[...], 7): Q7 taps to Q14. The tap
             * range (-128..=127) maps into i16 without saturation. */
            *coef = ((cbk_q7[ix][i] as i32) << 7) as i16;
        }
    }
}

/// The voiced frame's LTP scale factor in Q14 — the "Decode LTP
/// scaling" block of `silk_decode_parameters`
/// (`silk_LTPScales_table_Q14`); RFC 6716 §4.2.7.6.3's 15565, 12288,
/// 8192. `decode_indices` stores index 0 (≈ 0.95) when the parameter is
/// not coded.
#[inline]
pub(crate) fn ltp_scale_q14(ltp_scale_index: i8) -> i16 {
    LTPSCALES_TABLE_Q14[ltp_scale_index as usize]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Subframe pitch contour offsets transcribed directly from RFC 6716
    /// Tables 33–36 (independent of `tables.rs`), one row per contour
    /// index, entries per subframe.
    const RFC_CONTOUR_NB_20_MS: &[&[i8]] = &[
        &[0, 0, 0, 0],
        &[2, 1, 0, -1],
        &[-1, 0, 1, 2],
        &[-1, 0, 0, 1],
        &[-1, 0, 0, 0],
        &[0, 0, 0, 1],
        &[0, 0, 1, 1],
        &[1, 1, 0, 0],
        &[1, 0, 0, 0],
        &[0, 0, 0, -1],
        &[1, 0, 0, -1],
    ];
    const RFC_CONTOUR_NB_10_MS: &[&[i8]] = &[&[0, 0], &[1, 0], &[0, 1]];
    const RFC_CONTOUR_MB_WB_20_MS: &[&[i8]] = &[
        &[0, 0, 0, 0],
        &[0, 0, 1, 1],
        &[1, 1, 0, 0],
        &[-1, 0, 0, 0],
        &[0, 0, 0, 1],
        &[1, 0, 0, 0],
        &[-1, 0, 0, 1],
        &[0, 0, 0, -1],
        &[-1, 0, 1, 2],
        &[1, 0, 0, -1],
        &[-2, -1, 1, 2],
        &[2, 1, 0, -1],
        &[-2, 0, 0, 2],
        &[-2, 0, 1, 3],
        &[2, 1, -1, -2],
        &[-3, -1, 1, 3],
        &[2, 0, 0, -2],
        &[3, 1, 0, -2],
        &[-3, -1, 2, 4],
        &[-4, -1, 1, 4],
        &[3, 1, -1, -3],
        &[-4, -1, 2, 5],
        &[4, 2, -1, -3],
        &[4, 1, -1, -4],
        &[-5, -1, 2, 6],
        &[5, 2, -1, -4],
        &[-6, -2, 2, 6],
        &[-5, -2, 2, 5],
        &[6, 2, -1, -5],
        &[-7, -2, 3, 8],
        &[6, 2, -2, -6],
        &[5, 2, -2, -5],
        &[8, 3, -2, -7],
        &[-9, -3, 3, 9],
    ];
    const RFC_CONTOUR_MB_WB_10_MS: &[&[i8]] = &[
        &[0, 0],
        &[0, 1],
        &[1, 0],
        &[-1, 1],
        &[1, -1],
        &[-1, 2],
        &[2, -1],
        &[-2, 2],
        &[2, -2],
        &[-2, 3],
        &[3, -2],
        &[-3, 3],
    ];

    /// Independent oracle straight from RFC 6716 §4.2.7.6.1:
    /// `pitch_lags[k] = clamp(lag + lag_cb[contour_index][k],
    /// lag_min, lag_max)` where `lag = lag_min + lag_index` and
    /// `lag_min`/`lag_max` are Table 30's Minimum/Maximum Lag columns.
    /// (The RFC's absolute-lag formula
    /// `lag = lag_high·lag_scale + lag_low + lag_min` means `decode_indices`'
    /// `lag_index` already carries `lag_high·lag_scale + lag_low`.)
    fn rfc_decode_pitch(
        lag_index: i16,
        contour_index: usize,
        fs_khz: u32,
        nb_subfr: usize,
    ) -> [i32; MAX_NB_SUBFR] {
        let table: &[&[i8]] = match (fs_khz, nb_subfr) {
            (8, 4) => RFC_CONTOUR_NB_20_MS,
            (8, 2) => RFC_CONTOUR_NB_10_MS,
            (_, 4) => RFC_CONTOUR_MB_WB_20_MS,
            (_, 2) => RFC_CONTOUR_MB_WB_10_MS,
            _ => unreachable!(),
        };
        let lag_min = 2 * fs_khz as i32;
        let lag_max = 18 * fs_khz as i32;
        let lag = lag_min + i32::from(lag_index);
        let mut lags = [0; MAX_NB_SUBFR];
        for (k, out) in lags.iter_mut().enumerate().take(nb_subfr) {
            *out = (lag + i32::from(table[contour_index][k])).clamp(lag_min, lag_max);
        }
        lags
    }

    /// Every codebook entry of every bandwidth/frame-size combination,
    /// over the full reachable primary-lag range, agrees with the RFC
    /// oracle.
    #[test]
    fn decode_pitch_matches_rfc_oracle_exhaustively() {
        let configs: [(u32, usize, usize); 4] = [
            (8, MAX_NB_SUBFR, RFC_CONTOUR_NB_20_MS.len()),
            (8, MAX_NB_SUBFR / 2, RFC_CONTOUR_NB_10_MS.len()),
            (12, MAX_NB_SUBFR, RFC_CONTOUR_MB_WB_20_MS.len()),
            (16, MAX_NB_SUBFR / 2, RFC_CONTOUR_MB_WB_10_MS.len()),
        ];
        for (fs_khz, nb_subfr, n_contours) in configs {
            // All primary lag indices the entropy coder can produce for
            // this bandwidth (high part 0..=31 scaled by fs_khz/2 plus
            // the fs_khz/2 uniform low bits), plus out-of-range values
            // relative coding can legally reach (RFC: the primary lag is
            // unclamped under relative coding; only the per-subframe
            // lags clamp).
            let mult = fs_khz >> 1;
            let max_reachable = 31 * mult + mult - 1;
            let lag_sweep = (-300..=max_reachable as i32 + 300).map(|l| l as i16);
            for lag_index in lag_sweep {
                for contour in 0..n_contours {
                    let mut lags = [i32::MIN; MAX_NB_SUBFR];
                    decode_pitch(lag_index, contour as i8, &mut lags, fs_khz, nb_subfr).unwrap();
                    let expected = rfc_decode_pitch(lag_index, contour, fs_khz, nb_subfr);
                    // Only the first nb_subfr entries are written.
                    assert_eq!(
                        &lags[..nb_subfr],
                        &expected[..nb_subfr],
                        "fs {fs_khz}, nb_subfr {nb_subfr}, lag_index {lag_index}, contour {contour}"
                    );
                }
            }
        }
    }

    /// The search-range clamp engages exactly at the Table 30 bounds:
    /// every subframe lag stays within `2·fs ..= 18·fs`, and saturates
    /// there for far out-of-range primary lags (reachable via relative
    /// coding, which the RFC leaves unclamped).
    #[test]
    fn decode_pitch_clamps_to_search_range() {
        for fs_khz in [8u32, 12, 16] {
            let (min_lag, max_lag) = (2 * fs_khz as i32, 18 * fs_khz as i32);
            for lag_index in [i16::MIN, -1000, i16::MAX / 2, i16::MAX - 1] {
                let mut lags = [0; MAX_NB_SUBFR];
                decode_pitch(lag_index, 0, &mut lags, fs_khz, MAX_NB_SUBFR).unwrap();
                assert!(lags[..MAX_NB_SUBFR]
                    .iter()
                    .all(|&l| (min_lag..=max_lag).contains(&l)));
                assert!(lags[..MAX_NB_SUBFR]
                    .iter()
                    .all(|&l| l == min_lag || l == max_lag));
            }
            // Exactly at the floor with contour 0: no clamping needed.
            let mut lags = [0; MAX_NB_SUBFR];
            decode_pitch(0, 0, &mut lags, fs_khz, MAX_NB_SUBFR).unwrap();
            assert_eq!(lags, [min_lag; MAX_NB_SUBFR]);
        }
    }

    /// Hand-traced NB example tying the whole chain together: RFC
    /// §4.2.7.6.1's `lag = lag_high·4 + lag_low + 16` with
    /// (high, low) = (10, 3), then contour index 1 = [2, 1, 0, -1].
    #[test]
    fn decode_pitch_hand_traced_nb_20ms() {
        let mut lags = [0; MAX_NB_SUBFR];
        let lag_index = 10 * 4 + 3; // decode_indices: high * fs/2 + low
        decode_pitch(lag_index, 1, &mut lags, 8, MAX_NB_SUBFR).unwrap();
        assert_eq!(lags, [16 + 43 + 2, 16 + 43 + 1, 16 + 43, 16 + 43 - 1]);
    }

    /// The 10 ms variants write only the first two subframe lags and
    /// leave the rest of the caller's array untouched, like the
    /// reference's caller-owned `psDecCtrl->pitchL`.
    #[test]
    fn decode_pitch_10ms_writes_only_two_subframes() {
        let mut lags = [-1; MAX_NB_SUBFR];
        decode_pitch(50, 1, &mut lags, 16, MAX_NB_SUBFR / 2).unwrap();
        // WB: min lag 32, lag = 32 + 50 = 82; 10 ms MB/WB contour
        // index 1 = [0, 1].
        assert_eq!(&lags[..2], &[82, 83]);
        assert_eq!(&lags[2..], &[-1; 2]);
    }

    /// All 56 LTP filter vectors (RFC 6716 Tables 39–41, transcribed
    /// from the RFC) come back widened Q7 → Q14 (`<< 7`) with the right
    /// codebook selection and per-subframe indexing.
    #[test]
    fn ltp_coefs_q14_matches_rfc_codebooks_exhaustively() {
        const TAPS_0: [[i8; 5]; 8] = [
            [4, 6, 24, 7, 5],
            [0, 0, 2, 0, 0],
            [12, 28, 41, 13, -4],
            [-9, 15, 42, 25, 14],
            [1, -2, 62, 41, -9],
            [-10, 37, 65, -4, 3],
            [-6, 4, 66, 7, -8],
            [16, 14, 38, -3, 33],
        ];
        const TAPS_1: [[i8; 5]; 16] = [
            [13, 22, 39, 23, 12],
            [-1, 36, 64, 27, -6],
            [-7, 10, 55, 43, 17],
            [1, 1, 8, 1, 1],
            [6, -11, 74, 53, -9],
            [-12, 55, 76, -12, 8],
            [-3, 3, 93, 27, -4],
            [26, 39, 59, 3, -8],
            [2, 0, 77, 11, 9],
            [-8, 22, 44, -6, 7],
            [40, 9, 26, 3, 9],
            [-7, 20, 101, -7, 4],
            [3, -8, 42, 26, 0],
            [-15, 33, 68, 2, 23],
            [-2, 55, 46, -2, 15],
            [3, -1, 21, 16, 41],
        ];
        const TAPS_2: [[i8; 5]; 32] = [
            [-6, 27, 61, 39, 5],
            [-11, 42, 88, 4, 1],
            [-2, 60, 65, 6, -4],
            [-1, -5, 73, 56, 1],
            [-9, 19, 94, 29, -9],
            [0, 12, 99, 6, 4],
            [8, -19, 102, 46, -13],
            [3, 2, 13, 3, 2],
            [9, -21, 84, 72, -18],
            [-11, 46, 104, -22, 8],
            [18, 38, 48, 23, 0],
            [-16, 70, 83, -21, 11],
            [5, -11, 117, 22, -8],
            [-6, 23, 117, -12, 3],
            [3, -8, 95, 28, 4],
            [-10, 15, 77, 60, -15],
            [-1, 4, 124, 2, -4],
            [3, 38, 84, 24, -25],
            [2, 13, 42, 13, 31],
            [21, -4, 56, 46, -1],
            [-1, 35, 79, -13, 19],
            [-7, 65, 88, -9, -14],
            [20, 4, 81, 49, -29],
            [20, 0, 75, 3, -17],
            [5, -9, 44, 92, -8],
            [1, -3, 22, 69, 31],
            [-6, 95, 41, -12, 5],
            [39, 67, 16, -4, 1],
            [0, -6, 120, 55, -36],
            [-13, 44, 122, 4, -24],
            [81, 5, 11, 3, 7],
            [2, 0, 9, 10, 88],
        ];
        let codebooks: [(i8, &[[i8; 5]]); 3] = [(0, &TAPS_0), (1, &TAPS_1), (2, &TAPS_2)];
        for (per_index, taps) in codebooks {
            for (ix, taps) in taps.iter().enumerate() {
                let mut coefs = [0i16; MAX_NB_SUBFR * LTP_ORDER];
                let ltp_index = [ix as i8; MAX_NB_SUBFR];
                ltp_coefs_q14(per_index, &ltp_index, &mut coefs, MAX_NB_SUBFR);
                for k in 0..MAX_NB_SUBFR {
                    for i in 0..LTP_ORDER {
                        let expected = (taps[i] as i32) << 7;
                        assert_eq!(
                            coefs[k * LTP_ORDER + i] as i32,
                            expected,
                            "per {per_index}, ix {ix}, subframe {k}, tap {i}"
                        );
                    }
                }
            }
        }
    }

    /// Only `nb_subfr * LTP_ORDER` entries are written; the tail keeps
    /// the caller's previous contents (reference in-place semantics).
    #[test]
    fn ltp_coefs_q14_10ms_leaves_tail_untouched() {
        let mut coefs = [7777i16; MAX_NB_SUBFR * LTP_ORDER];
        let ltp_index = [3, 7, 0, 0];
        ltp_coefs_q14(1, &ltp_index, &mut coefs, MAX_NB_SUBFR / 2);
        // Subframe 0 = Table 40 index 3 << 7, subframe 1 = index 7 << 7.
        assert_eq!(&coefs[..LTP_ORDER], &[128, 128, 1024, 128, 128]);
        assert_eq!(
            &coefs[LTP_ORDER..2 * LTP_ORDER],
            &[3328, 4992, 7552, 384, -1024]
        );
        assert_eq!(&coefs[2 * LTP_ORDER..], &[7777; 2 * LTP_ORDER]);
    }

    /// RFC 6716 §4.2.7.6.3's three Q14 scale factors (≈ 0.95, 0.75,
    /// 0.5); index 0 is also the value used when the parameter is not
    /// coded.
    #[test]
    fn ltp_scale_q14_rfc_values() {
        assert_eq!(ltp_scale_q14(0), 15565);
        assert_eq!(ltp_scale_q14(1), 12288);
        assert_eq!(ltp_scale_q14(2), 8192);
    }

    /// Unsupported internal rates are rejected like `decode_indices`'s
    /// rate-dependent table selectors, not silently misdecoded.
    #[test]
    fn decode_pitch_rejects_unsupported_fs_khz() {
        let mut lags = [0; MAX_NB_SUBFR];
        let err = decode_pitch(0, 0, &mut lags, 24, MAX_NB_SUBFR).unwrap_err();
        assert!(err.to_string().contains("sample rate"), "{err}");
    }
}
