//! SILK gain dequantization (RFC 6716 §4.2.4).
//!
//! Ports the decoder half of `silk/gain_quant.c`
//! (`silk_gains_dequant`, invoked from `silk/decode_parameters.c`),
//! `silk/log2lin.c`, and the gain-related cross-frame state handling
//! from `silk/dec_API.c`/`silk/init_decoder.c`.
//!
//! The gain index *symbols* are entropy-decoded by
//! [`super::decode_indices`] into `SideInfoIndices::gains_indices`
//! (absolute 0..=63 for the first subframe of an independently coded
//! frame, delta 0..=40 otherwise); this module accumulates them onto the
//! persistent `psDec->LastGainIndex` state and converts the result into
//! the linear Q16 gains the synthesis consumes:
//!
//! - First subframe, independent coding: the index is absolute, but not
//!   allowed to drop more than 16 steps (~21.8 dB) below the previous
//!   frame's last index. The clamp is inert right after a decoder reset
//!   (state 0) and is disabled on packet loss by forcing the state to
//!   [`LAST_GAIN_INDEX_ON_PACKET_LOSS`], so the energy cannot "bounce
//!   back" when loss hits during a fade-down (`silk/dec_API.c`; the RFC
//!   only says the clamp MAY be skipped, libopus does it this way).
//! - Every other subframe (and the first one under conditional coding):
//!   a delta index 0..=40, i.e. an accumulated step of
//!   `[MIN_DELTA_GAIN_QUANT, MAX_DELTA_GAIN_QUANT]` = −4..=36, with the
//!   step size doubled for large increases so the top level stays
//!   reachable (the encoder-side mapping in `silk_gains_quant`).
//! - The accumulated log-domain index (0..=63) maps through
//!   [`log2lin`] to a Q16 gain in `81920..=1686110208`
//!   (RFC 6716 §4.2.4's stated bounds; both endpoints are pinned by
//!   tests).
//!
//! The remaining gain smoothing lives where the reference puts it: the
//! intra-subframe gain ramp keyed on `psDec->prev_gain_Q16`
//! (`silk/decode_core.c`) belongs to `synthesis.rs` (Tier 3), and the
//! comfort-noise gain smoothing (`silk/CNG.c`) to the CNG/PLC work.
//!
//! SOURCE: Xiph.Org libopus 1.5.2, `silk/gain_quant.c`,
//! `silk/log2lin.c`, `silk/dec_API.c`, `silk/init_decoder.c`,
//! `silk/define.h` (BSD-3-Clause).
#![allow(dead_code)]

use crate::silk::decode_indices::MAX_NB_SUBFR;
use crate::silk::sigproc::{smlawb, smulbb, smulwb};

/// `MIN_QGAIN_DB` / `MAX_QGAIN_DB` / `N_LEVELS_QGAIN` (`silk/define.h`):
/// the quantized gain spans 2–88 dB over 64 levels, uniform in the log
/// domain.
const MIN_QGAIN_DB: i32 = 2;
const MAX_QGAIN_DB: i32 = 88;
const N_LEVELS_QGAIN: i32 = 64;
/// `MAX_DELTA_GAIN_QUANT` / `MIN_DELTA_GAIN_QUANT` (`silk/define.h`):
/// bounds of one accumulated delta step; the transmitted symbol is the
/// step shifted up by 4 into `0..=40`.
const MAX_DELTA_GAIN_QUANT: i32 = 36;
const MIN_DELTA_GAIN_QUANT: i32 = -4;

/// `OFFSET` (`gain_quant.c`): log-domain (Q7) gain of index 0 —
/// `2 dB·128/6` (floor) plus a 16 dB offset = 2090.
const OFFSET: i32 = (MIN_QGAIN_DB * 128) / 6 + 16 * 128;
/// `SCALE_Q16` (`gain_quant.c`, encoder side; kept for the round-trip
/// test) = 2251.
const SCALE_Q16: i32 = (65536 * (N_LEVELS_QGAIN - 1)) / (((MAX_QGAIN_DB - MIN_QGAIN_DB) * 128) / 6);
/// `INV_SCALE_Q16` (`gain_quant.c`) = `0x1D1C71` = 1907825, the constant
/// the RFC quotes in §4.2.4's `gain_Q16[k]` formula.
const INV_SCALE_Q16: i32 =
    (65536 * (((MAX_QGAIN_DB - MIN_QGAIN_DB) * 128) / 6)) / (N_LEVELS_QGAIN - 1);
/// The `silk_min_32(..., 3967)` clamp ("31 in Q7") applied to the log
/// gain before conversion.
const LOG_GAIN_MAX_Q7: i32 = 3967;

/// The `psDec->LastGainIndex` value libopus forces on packet loss
/// ("remove the gain clamping to prevent having the energy bounce back
/// if we lose packets when the energy is going down") and when the side
/// channel starts coding again after a mid-only period
/// (`silk/dec_API.c`). A freshly initialized/reset decoder state starts
/// at 0 instead (`silk_init_decoder`), which leaves the 16-step-down
/// clamp inert either way — both values are below the clamp's 16-step
/// reach, matching the RFC's note that the clamp is skipped when no
/// previous gain is meaningful.
pub(crate) const LAST_GAIN_INDEX_ON_PACKET_LOSS: i8 = 10;

/// `silk_log2lin` (`silk/log2lin.c`): approximation of
/// `2^(in_log_q7 / 128.0)` — inverse of the encoder's `silk_lin2log`.
///
/// Bit-exact port of the piece-wise parabolic approximation, including
/// the branch split at `inLog_Q7 == 2048`: the two branches differ only
/// in where the truncating `>> 7` lands (`(out*t) >> 7` vs
/// `(out >> 7)*t`), which is observable for large outputs, so the
/// reference's branch structure is preserved rather than unified.
pub(crate) fn log2lin(in_log_q7: i32) -> i32 {
    if in_log_q7 < 0 {
        return 0;
    } else if in_log_q7 >= LOG_GAIN_MAX_Q7 {
        return i32::MAX;
    }
    let out = 1i32 << (in_log_q7 >> 7);
    let frac_q7 = in_log_q7 & 0x7F;
    /* Q7 parabolic correction term shared by both branches */
    let correction_q7 = smlawb(frac_q7, smulbb(frac_q7, 128 - frac_q7), -174);
    if in_log_q7 < 2048 {
        /* Piece-wise parabolic approximation */
        out + ((out * correction_q7) >> 7)
    } else {
        /* Piece-wise parabolic approximation */
        out + ((out >> 7) * correction_q7)
    }
}

/// `silk_gains_dequant` (`silk/gain_quant.c`): dequantizes the
/// transmitted gain index symbols into linear Q16 gains, threading
/// `prev_ind` — the persistent `psDec->LastGainIndex` — through the
/// delta accumulation exactly as the reference does, once per subframe.
///
/// `conditional` is `condCoding == CODE_CONDITIONALLY`: when false the
/// first subframe's symbol is an absolute index (with the inter-frame
/// 16-step-down clamp), otherwise it — and every later subframe
/// regardless — is a delta in `0..=40`. `nb_subfr` is 4 (20 ms frames)
/// or 2 (10 ms); only that many entries of `gain_q16` are written.
pub(crate) fn gains_dequant(
    gain_q16: &mut [i32; MAX_NB_SUBFR],
    ind: &[i8; MAX_NB_SUBFR],
    prev_ind: &mut i8,
    conditional: bool,
    nb_subfr: usize,
) {
    debug_assert!(nb_subfr == MAX_NB_SUBFR || nb_subfr == MAX_NB_SUBFR / 2);
    let mut prev = i32::from(*prev_ind);
    for k in 0..nb_subfr {
        if k == 0 && !conditional {
            /* Gain index is not allowed to go down more than 16 steps
             * (~21.8 dB). Inert after reset/packet loss: the state is
             * then 0 or LAST_GAIN_INDEX_ON_PACKET_LOSS, both < 16. */
            prev = i32::from(ind[0]).max(prev - 16);
        } else {
            /* Delta index */
            let ind_tmp = i32::from(ind[k]) + MIN_DELTA_GAIN_QUANT;

            /* Accumulate deltas; past `double_step_size_threshold` the
             * effective step size doubles so the max gain level can be
             * reached (mirroring the encoder's `silk_gains_quant`). */
            let double_step_size_threshold = 2 * MAX_DELTA_GAIN_QUANT - N_LEVELS_QGAIN + prev;
            if ind_tmp > double_step_size_threshold {
                prev += ind_tmp * 2 - double_step_size_threshold;
            } else {
                prev += ind_tmp;
            }
        }
        prev = prev.clamp(0, N_LEVELS_QGAIN - 1);

        /* Scale and convert to linear scale */
        let log_gain = (smulwb(INV_SCALE_Q16, prev) + OFFSET).min(LOG_GAIN_MAX_Q7);
        gain_q16[k] = log2lin(log_gain);
    }
    *prev_ind = prev as i8;
}

#[cfg(test)]
mod tests {
    use super::*;

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

    /// RFC 6716 §4.2.4's stated Q16 gain bounds (scale factors 1.25 and
    /// 25728), pinned as literals.
    const RFC_MIN_GAIN_Q16: i32 = 81920;
    const RFC_MAX_GAIN_Q16: i32 = 1686110208;

    /// Independent oracle for the first-subframe absolute path, straight
    /// from RFC 6716 §4.2.4: `log_gain = max(gain_index,
    /// previous_log_gain - 16)`.
    fn rfc_log_gain_absolute(gain_index: i32, previous: i32) -> i32 {
        gain_index.max(previous - 16)
    }

    /// Independent oracle for the delta path, straight from RFC 6716
    /// §4.2.4: `log_gain = clamp(0, max(2*delta_gain_index - 16,
    /// previous_log_gain + delta_gain_index - 4), 63)`. Algebraically
    /// disjoint from the reference's in-place `+=` formulation, which is
    /// what makes the exhaustive comparison meaningful.
    fn rfc_log_gain_delta(delta_gain_index: i32, previous: i32) -> i32 {
        (2 * delta_gain_index - 16)
            .max(previous + delta_gain_index - 4)
            .clamp(0, N_LEVELS_QGAIN - 1)
    }

    /// The RFC's own expression for the Q16 conversion:
    /// `gain_Q16[k] = silk_log2lin((0x1D1C71*log_gain>>16) + 2090)` with
    /// `silk_log2lin` spelled `(1<<i) + ((-174*f*(128-f)>>16)+f)*((1<<i)>>7)`.
    /// (Its uniform `(1<<i)>>7` factor only matches the reference's
    /// branch structure for `inLog >= 896`; the dequant path only ever
    /// feeds `>= OFFSET = 2090`, so it is an exact oracle there.)
    fn rfc_gain_q16(log_gain: i32) -> i32 {
        let in_log = (((0x1D1C71u32 as i64) * log_gain as i64) >> 16) as i32 + OFFSET;
        let i = in_log >> 7;
        let f = in_log & 127;
        let t = ((-174 * f * (128 - f)) >> 16) + f;
        (1 << i) + t * ((1 << i) >> 7)
    }

    /// Runs `gains_dequant` on `ind0` as the only *effective* subframe
    /// and returns the resulting `(prev_ind, gain_Q16)`. The function's
    /// reference assert only accepts 2 or 4 subframes, so the second
    /// subframe carries delta symbol 4 (= a 0-step no-op) and its gain
    /// slot is ignored.
    fn dequant_one(ind0: i8, prev: i8, conditional: bool) -> (i8, i32) {
        let ind = [ind0, 4, 0, 0];
        let mut gain = [0i32; MAX_NB_SUBFR];
        let mut prev = prev;
        gains_dequant(&mut gain, &ind, &mut prev, conditional, 2);
        (prev, gain[0])
    }

    /// Exhaustively checks the delta path (all 64 starting states × all
    /// 41 transmitted deltas) against the RFC's absolute formulation.
    #[test]
    fn delta_path_matches_rfc_formula_exhaustively() {
        for prev in 0..N_LEVELS_QGAIN {
            for delta_symbol in 0..=40i32 {
                let (prev_out, gain) = dequant_one(delta_symbol as i8, prev as i8, true);
                let expected_log = rfc_log_gain_delta(delta_symbol, prev);
                assert_eq!(
                    i32::from(prev_out),
                    expected_log,
                    "prev {prev}, delta {delta_symbol}"
                );
                assert_eq!(
                    gain,
                    rfc_gain_q16(expected_log),
                    "prev {prev}, delta {delta_symbol}"
                );
            }
        }
    }

    /// Exhaustively checks the absolute path (all 64 indices × all 64
    /// starting states) against the RFC formula, and confirms the clamp
    /// is inert after a reset (previous state 0) and after packet loss
    /// (state forced to 10): both lie within 16 steps of everything.
    #[test]
    fn absolute_path_matches_rfc_formula_exhaustively() {
        for prev in 0..N_LEVELS_QGAIN {
            for index in 0..N_LEVELS_QGAIN {
                let (prev_out, gain) = dequant_one(index as i8, prev as i8, false);
                let expected_log = rfc_log_gain_absolute(index, prev);
                assert_eq!(
                    i32::from(prev_out),
                    expected_log,
                    "prev {prev}, index {index}"
                );
                assert_eq!(
                    gain,
                    rfc_gain_q16(expected_log),
                    "prev {prev}, index {index}"
                );
            }
        }
    }

    #[test]
    fn log2lin_endpoints_and_sentinels() {
        // RFC 6716 §4.2.4 bounds, reached at the dequant path extremes:
        // index 0 → log 2090, index 63 → log 3923 (hand-traced).
        assert_eq!(log2lin(2090), RFC_MIN_GAIN_Q16);
        assert_eq!(log2lin(3923), RFC_MAX_GAIN_Q16);
        // Input clamps.
        assert_eq!(log2lin(-1), 0);
        assert_eq!(log2lin(LOG_GAIN_MAX_Q7), i32::MAX);
        assert_eq!(log2lin(i32::MAX), i32::MAX);
        // Just below the clamp: 2^30 scaled by the Q7 parabola at
        // f = 126 (correction 125) = 2^30·253/128, hand-traced.
        assert_eq!(log2lin(LOG_GAIN_MAX_Q7 - 1), 2122317824);
        // Small inputs hit the other branch: exact powers of two have a
        // zero fractional part, and zero maps to 1.
        assert_eq!(log2lin(0), 1);
        assert_eq!(log2lin(128), 2);
        assert_eq!(log2lin(15 * 128), 1 << 15);
        assert_eq!(log2lin(2048), 1 << 16);
    }

    #[test]
    fn log2lin_monotonic_and_close_to_exact_exponential() {
        let mut prev = 0;
        for x in 0..LOG_GAIN_MAX_Q7 {
            let out = log2lin(x);
            assert!(out >= prev, "not monotonic at {x}");
            prev = out;
            // The Q7 parabola's relative error is coarse below ~2^3 by
            // design (same formula in C); check accuracy where the
            // dequant path actually operates.
            if x >= 896 {
                let exact = 2.0f64.powf(x as f64 / 128.0);
                let rel_err = (out as f64 / exact - 1.0).abs();
                assert!(
                    rel_err < 0.01,
                    "{x}: {out} vs exact {exact} (rel err {rel_err})"
                );
            }
        }
    }

    #[test]
    fn gains_stay_within_rfc_bounds_for_all_states() {
        // Every reachable state (prev 0..=63) with maximal deltas still
        // lands inside the RFC bounds, and the minimum-input frame sits
        // exactly on the lower bound.
        let mut gain = [0i32; MAX_NB_SUBFR];
        for prev in 0..N_LEVELS_QGAIN {
            let mut prev_ind = prev as i8;
            let ind = [40, 40, 40, 40];
            gains_dequant(&mut gain, &ind, &mut prev_ind, false, MAX_NB_SUBFR);
            for g in gain.iter() {
                assert!((RFC_MIN_GAIN_Q16..=RFC_MAX_GAIN_Q16).contains(g));
            }
        }
        let mut prev_ind = 0i8;
        let ind = [0; MAX_NB_SUBFR];
        gains_dequant(&mut gain, &ind, &mut prev_ind, false, MAX_NB_SUBFR);
        assert_eq!(gain[..MAX_NB_SUBFR], [RFC_MIN_GAIN_Q16; MAX_NB_SUBFR]);
    }

    /// Hand-traced reference vectors exercising the double-step delta
    /// mapping, the inter-frame clamp, and per-subframe chaining.
    #[test]
    fn hand_traced_reference_vectors() {
        let mut gain = [0i32; MAX_NB_SUBFR];
        let mut prev_ind = 63i8;

        // Index 63 held: clamp pulls 0 up to 63-16; then two deltas of
        // +32/+20 clamp against the ceiling; final +16 clamps too.
        let ind = [63, 0, 40, 20];
        gains_dequant(&mut gain, &ind, &mut prev_ind, false, MAX_NB_SUBFR);
        assert_eq!(prev_ind, 63);
        // k=0: prev = max(63, 63-16) = 63 → RFC max gain (literal).
        assert_eq!(gain[0], RFC_MAX_GAIN_Q16);
        // k=1: delta -4 → prev index 59 (its Q16 gain goes through the
        // SMULWB floor 1907825*59 >> 16 = 1717 → log 3807, which
        // rfc_gain_q16 applies from the index itself).
        assert_eq!(gain[1], rfc_gain_q16(59));
        // k=2: ind_tmp 36 ≤ thr 8+59=67 → prev 59+36=95 → clamp 63.
        assert_eq!(gain[2], RFC_MAX_GAIN_Q16);
        // k=3: ind_tmp 16 ≤ thr 71 → 79 → clamp 63.
        assert_eq!(gain[3], RFC_MAX_GAIN_Q16);

        // Double-step: from state 0, delta symbol 40 (=+36) maps over
        // the threshold 8 to 2*40-16 = 64 → clamped to 63. (Second
        // subframe is the 0-step no-op from `dequant_one`.)
        let mut prev_ind = 0i8;
        let ind = [40, 4, 0, 0];
        gains_dequant(&mut gain, &ind, &mut prev_ind, true, 2);
        assert_eq!(prev_ind, 63);
        assert_eq!(gain[0], RFC_MAX_GAIN_Q16);

        // Boundary just below the threshold (prev 20: thr 28; +28 stays
        // linear → 48, +29 doubles → 2*33-16 = 50).
        let (prev_out, _) = dequant_one(32, 20, true);
        assert_eq!(prev_out, 48);
        let (prev_out, _) = dequant_one(33, 20, true);
        assert_eq!(prev_out, 50);
    }

    /// The 16-step-down clamp only applies to the absolute (first,
    /// independent) path, and delta chains ignore it: with prev 63, an
    /// absolute index of 0 lands at 47, while a conditional delta
    /// symbol 0 (−4) lands at 59.
    #[test]
    fn inter_frame_clamp_path_dependence() {
        let (prev_out, _) = dequant_one(0, 63, false);
        assert_eq!(prev_out, 47);
        let (prev_out, _) = dequant_one(0, 63, true);
        assert_eq!(prev_out, 59);

        // After packet loss the state is 10, so the clamp stays inert
        // (10 < 16): an absolute index of 0 lands at 0, not at max(0,
        // 10-16).
        let (prev_out, gain) = dequant_one(0, LAST_GAIN_INDEX_ON_PACKET_LOSS, false);
        assert_eq!(prev_out, 0);
        assert_eq!(gain, RFC_MIN_GAIN_Q16);
    }

    #[test]
    fn multi_subframe_chaining_and_nb_subfr_scoping() {
        // 20 ms frame: subframes chain through the shared state; only
        // nb_subfr entries are touched (sentinel check for 10 ms).
        let ind = [30, 5, 40, 10];
        let mut prev_ind = 10i8;
        let mut gain = [-1i32; MAX_NB_SUBFR];
        gains_dequant(&mut gain, &ind, &mut prev_ind, true, 4);

        let mut expected_prev = 10i32;
        let mut expected = [0i32; MAX_NB_SUBFR];
        for k in 0..4 {
            expected_prev = rfc_log_gain_delta(ind[k] as i32, expected_prev);
            expected[k] = rfc_gain_q16(expected_prev);
        }
        assert_eq!(prev_ind as i32, expected_prev);
        assert_eq!(gain, expected);

        // 10 ms frame: entries 2..4 keep their sentinel values.
        let mut prev_ind = 10i8;
        let mut gain = [-1i32; MAX_NB_SUBFR];
        let ind = [20, 20, 99, 99];
        gains_dequant(&mut gain, &ind, &mut prev_ind, false, 2);
        assert_eq!(gain[2], -1);
        assert_eq!(gain[3], -1);
        // Subframes 1.. stay delta-coded even under independent coding:
        // 10 → max(20, -6) = 20 → 20 + (20-4) = 36.
        assert_eq!(prev_ind, 36);
        assert_eq!(gain[0], rfc_gain_q16(20));
        assert_eq!(gain[1], rfc_gain_q16(36));
    }

    /// Total over every bit-pattern the symbol arrays can carry,
    /// including corrupt ones: no panic, and the state never leaves
    /// `0..=63`.
    #[test]
    fn total_over_adversarial_symbols() {
        let mut gain = [0i32; MAX_NB_SUBFR];
        for &conditional in &[false, true] {
            for symbol in i8::MIN..=i8::MAX {
                for prev in [0i8, 10, 63] {
                    let ind = [symbol, symbol.wrapping_add(7), 40, -100];
                    let mut prev_ind = prev;
                    gains_dequant(&mut gain, &ind, &mut prev_ind, conditional, 4);
                    assert!((0..=63).contains(&prev_ind));
                    for g in gain.iter() {
                        assert!((RFC_MIN_GAIN_Q16..=RFC_MAX_GAIN_Q16).contains(g));
                    }
                }
            }
        }
    }

    /// `silk_lin2log` (`silk/lin2log.c`), test-only so the encoder-side
    /// round trip can run; includes the `silk_CLZ_FRAC` inline
    /// (`frac_Q7` = the 7 bits after the leading one). The reference
    /// computes it via `ROR32(in, 24 - lz) & 0x7f`, which equals the
    /// shift form below for every positive input.
    fn lin2log(in_lin: i32) -> i32 {
        debug_assert!(in_lin > 0);
        let lz = in_lin.leading_zeros() as i32;
        let frac_q7 = ((in_lin << lz) >> 24) & 0x7f;
        smlawb(frac_q7, frac_q7 * (128 - frac_q7), 179) + ((31 - lz) << 7)
    }

    /// Test-only port of the encoder-side `silk_gains_quant`
    /// (`silk/gain_quant.c`): scalar quantization with hysteresis,
    /// delta coding with the double-step mapping, and the same final
    /// Q16 conversion the decoder applies. The round trip asserts that
    /// `gains_dequant` inverts it bit-exactly.
    fn gains_quant(
        ind: &mut [i8; MAX_NB_SUBFR],
        gain_q16: &mut [i32; MAX_NB_SUBFR],
        prev_ind: &mut i8,
        conditional: bool,
        nb_subfr: usize,
    ) {
        let mut prev = i32::from(*prev_ind);
        for k in 0..nb_subfr {
            /* Convert to log scale, scale, floor() */
            let mut ix = smulwb(SCALE_Q16, lin2log(gain_q16[k]) - OFFSET);

            /* Round towards previous quantized gain (hysteresis) */
            if ix < prev {
                ix += 1;
            }
            ix = ix.clamp(0, N_LEVELS_QGAIN - 1);

            /* Compute delta indices and limit */
            if k == 0 && !conditional {
                /* Full index */
                ix = ix.clamp(prev + MIN_DELTA_GAIN_QUANT, N_LEVELS_QGAIN - 1);
                prev = ix;
            } else {
                /* Delta index */
                ix -= prev;

                /* Double the quantization step size for large gain
                 * increases, so that the max gain level can be reached */
                let double_step_size_threshold = 2 * MAX_DELTA_GAIN_QUANT - N_LEVELS_QGAIN + prev;
                if ix > double_step_size_threshold {
                    ix = double_step_size_threshold + ((ix - double_step_size_threshold + 1) >> 1);
                }
                ix = ix.clamp(MIN_DELTA_GAIN_QUANT, MAX_DELTA_GAIN_QUANT);

                /* Accumulate deltas */
                if ix > double_step_size_threshold {
                    prev = (prev + ix * 2 - double_step_size_threshold).min(N_LEVELS_QGAIN - 1);
                } else {
                    prev += ix;
                }

                /* Shift to make non-negative */
                ix -= MIN_DELTA_GAIN_QUANT;
            }

            ind[k] = ix as i8;
            /* Scale and convert to linear scale */
            gain_q16[k] = log2lin((smulwb(INV_SCALE_Q16, prev) + OFFSET).min(LOG_GAIN_MAX_Q7));
        }
        *prev_ind = prev as i8;
    }

    /// `dequant(quant(gains)) == gains` bit-exactly, across random
    /// gain vectors, both coding modes, both frame sizes, and
    /// sequenced frames so the cross-frame state is exercised.
    #[test]
    fn encoder_decoder_round_trip() {
        // Sanity literals for the test-only lin2log port (exact powers
        // of two have a zero fractional part).
        assert_eq!(lin2log(65536), 2048);
        assert_eq!(lin2log(1 << 17), 2176);
        // 1.25·2^16: frac_Q7 = 32, parabolic correction +8 → 2048 + 40.
        assert_eq!(lin2log(81920), 2088);

        let mut rng = XorShift(0x5EED_1A17);
        for _ in 0..200 {
            let nb_subfr = if rng.below(2) == 0 {
                MAX_NB_SUBFR
            } else {
                MAX_NB_SUBFR / 2
            };
            let conditional = rng.below(2) == 1;
            let mut prev_ind = rng.below(N_LEVELS_QGAIN as u32) as i8;
            let mut gain = [0i32; MAX_NB_SUBFR];
            for g in gain.iter_mut().take(nb_subfr) {
                // Sample the dequant output range — the gains an
                // encoder actually feeds in come from the same domain.
                *g = RFC_MIN_GAIN_Q16
                    + (rng.next_u32() as i64 % ((RFC_MAX_GAIN_Q16 - RFC_MIN_GAIN_Q16) as i64))
                        as i32;
            }

            let initial_prev = prev_ind;
            let mut ind = [0i8; MAX_NB_SUBFR];
            gains_quant(&mut ind, &mut gain, &mut prev_ind, conditional, nb_subfr);
            let (quant_inds, quant_gains, quant_prev) = (ind, gain, prev_ind);

            // Decode from the *pre-quant* state.
            let mut decoded_gain = [0i32; MAX_NB_SUBFR];
            let mut decoded_prev = initial_prev;
            gains_dequant(
                &mut decoded_gain,
                &quant_inds,
                &mut decoded_prev,
                conditional,
                nb_subfr,
            );

            assert_eq!(
                decoded_gain[..nb_subfr],
                quant_gains[..nb_subfr],
                "gains diverged (conditional={conditional})"
            );
            assert_eq!(decoded_prev, quant_prev);
            for symbol in quant_inds.iter().take(nb_subfr) {
                if conditional {
                    assert!((0..=40).contains(symbol));
                }
            }
        }
    }
}
