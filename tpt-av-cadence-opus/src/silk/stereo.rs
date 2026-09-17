//! SILK stereo mid/side prediction and unmixing (RFC 6716 §4.2.7–§4.2.8).
//!
//! Ports `silk/stereo_decode_pred.c` (predictor index entropy decoding +
//! dequantization, and the mid-only flag) and `silk/stereo_MS_to_LR.c`
//! (the MS→LR conversion) from libopus 1.5.2, preserving the exact
//! fixed-point operation order of the reference's generic macros.
//!
//! Bitstream layout per mid-channel SILK frame (decoded once per frame
//! before the per-channel side info, cf. `silk/dec_API.c`):
//! 1. a joint 25-symbol stage-1 index `n`, split as `n/5` for w0 and
//!    `n % 5` for w1,
//! 2. per weight, in order: the low part of the table index (uniform 3),
//!    then the interpolation sub-step (uniform 5) — first for w0's pair,
//!    then for w1's,
//! 3. the mid-only flag (one 2-symbol ICDF; only when the side channel
//!    is not otherwise known to be coded — the caller decides).
//!
//! The decoded weights are interpolated from the previous frame's values
//! over the first 8 ms of each frame while adding the side prediction,
//! then the processed mid/side pair is converted to left/right.
//!
//! SOURCE: Xiph.Org libopus 1.5.2, `silk/stereo_decode_pred.c`,
//! `silk/stereo_MS_to_LR.c`, `silk/define.h`, `silk/structs.h`
//! (BSD-3-Clause); cross-checked against RFC 6716 §4.2.7.1–§4.2.8.
#![allow(dead_code)]

use crate::range::RangeDecoder;
use crate::silk::sigproc::{rshift_round, sat16, smlabb, smlawb, smulbb, smulwb};
use crate::silk::tables::{
    STEREO_ONLY_CODE_MID_ICDF, STEREO_PRED_JOINT_ICDF, STEREO_PRED_QUANT_Q13, UNIFORM3_ICDF,
    UNIFORM5_ICDF,
};
use crate::Result;

/// `STEREO_INTERP_LEN_MS` (`silk/define.h`; "must be even").
const STEREO_INTERP_LEN_MS: i32 = 8;

/// `SILK_FIX_CONST(0.5 / STEREO_QUANT_SUB_STEPS, 16)` with
/// `STEREO_QUANT_SUB_STEPS == 5`: `(0.1 * 2^16 + 0.5)` truncated = 6554.
/// (RFC 6716 §4.2.7.1 states this constant directly.)
const PRED_SUBSTEP_WEIGHT_Q16: i32 = 6554;

/// Mirrors `stereo_dec_state` (`silk/structs.h`): persistent stereo
/// decoder state, all zeros after a reset / mono↔stereo transition.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct StereoDecState {
    /// Q13 predictors applied to the previous frame, stored as the
    /// *combined* `w0 - w1` in slot 0. Like the reference's
    /// `opus_int16` field, out-of-range combined values wrap on store.
    pub(crate) pred_prev_q13: [i16; 2],
    /// Last two mid samples (the one-sample output delay history).
    pub(crate) s_mid: [i16; 2],
    /// Last two side-channel residual samples from the previous frame.
    pub(crate) s_side: [i16; 2],
}

/// `silk_stereo_decode_pred`: entropy-decode and dequantize the mid/side
/// prediction weights for one mid-channel SILK frame.
///
/// Returns `[w0_Q13, w1_Q13]`, where `w0` is already the combined
/// `w0 - w1` value the unmixing step applies (the reference subtracts
/// w1 here "because it helps when actually applying these"). Note that
/// the combined w0 can exceed the ±13732 table range.
pub(crate) fn decode_pred(dec: &mut RangeDecoder) -> Result<[i32; 2]> {
    let joint = dec.decode_icdf(&STEREO_PRED_JOINT_ICDF, 8)?;
    let phase = [joint / 5, joint % 5];
    let mut low_ix = [0usize; 2];
    let mut sub_ix = [0i32; 2];
    for n in 0..2 {
        low_ix[n] = (dec.decode_icdf(&UNIFORM3_ICDF, 8)? + 3 * phase[n]) as usize;
        sub_ix[n] = dec.decode_icdf(&UNIFORM5_ICDF, 8)? as i32;
    }

    // Dequantize: interpolate `2*sub+1` tenths of the way into the
    // table cell starting at `low_ix`.
    let mut pred = [0i32; 2];
    for n in 0..2 {
        let low_q13 = STEREO_PRED_QUANT_Q13[low_ix[n]] as i32;
        let step_q13 = smulwb(
            STEREO_PRED_QUANT_Q13[low_ix[n] + 1] as i32 - low_q13,
            PRED_SUBSTEP_WEIGHT_Q16,
        );
        pred[n] = smlabb(low_q13, step_q13, 2 * sub_ix[n] + 1);
    }

    pred[0] -= pred[1];
    Ok(pred)
}

/// `silk_stereo_decode_mid_only`: decode the flag that says only the mid
/// channel is coded for this interval (the side is then fed zeros).
pub(crate) fn decode_mid_only(dec: &mut RangeDecoder) -> Result<bool> {
    Ok(dec.decode_icdf(&STEREO_ONLY_CODE_MID_ICDF, 8)? != 0)
}

/// `silk_stereo_MS_to_LR`: convert the adaptive Mid/Side representation
/// to Left/Right.
///
/// `mid` and `side` each hold `frame_length + 2` samples: entry `[0..2]`
/// is scratch that receives the history from `state`, and entries
/// `[2..frame_length + 2]` are the frame's decoded mid signal and side
/// residual. On return, positions `[1..frame_length + 1]` hold the
/// frame's left/right output (the stereo layer's one-sample delay,
/// RFC 6716 §4.2.8) and the input tails are stashed in `state` for the
/// next frame.
///
/// The side prediction (a two-tap mid predictor: a 3-sample low-pass in
/// `pred0` plus the raw mid in `pred1`) is added over the whole frame,
/// with `pred0`/`pred1` ramping linearly from `state.pred_prev_q13` to
/// `pred_q13` across the first 8 ms. Requires
/// `frame_length >= 8 * fs_khz` (holds for every SILK frame size).
pub(crate) fn ms_to_lr(
    state: &mut StereoDecState,
    mid: &mut [i16],
    side: &mut [i16],
    pred_q13: &[i32; 2],
    fs_khz: u32,
    frame_length: usize,
) {
    let interp_len = (STEREO_INTERP_LEN_MS * fs_khz as i32) as usize;
    debug_assert!(mid.len() >= frame_length + 2 && side.len() >= frame_length + 2);
    debug_assert!(interp_len <= frame_length);

    // Buffering: pull in the previous frame's history, and save the tail
    // of this frame's input for the next call.
    mid[..2].copy_from_slice(&state.s_mid);
    side[..2].copy_from_slice(&state.s_side);
    state
        .s_mid
        .copy_from_slice(&mid[frame_length..frame_length + 2]);
    state
        .s_side
        .copy_from_slice(&side[frame_length..frame_length + 2]);

    // Interpolate predictors and add prediction to side channel. The
    // per-sample ramp step rounds towards nearest (RSHIFT_ROUND), and
    // SMULBB's i16 truncation of the (possibly out-of-i16) combined
    // predictor difference is part of the reference behavior.
    let denom_q16 = (1i32 << 16) / (STEREO_INTERP_LEN_MS * fs_khz as i32);
    let mut pred0 = state.pred_prev_q13[0] as i32;
    let mut pred1 = state.pred_prev_q13[1] as i32;
    let delta0 = rshift_round(smulbb(pred_q13[0] - pred0, denom_q16), 16);
    let delta1 = rshift_round(smulbb(pred_q13[1] - pred1, denom_q16), 16);
    for n in 0..interp_len {
        pred0 += delta0;
        pred1 += delta1;
        predict_side_sample(mid, side, n, pred0, pred1);
    }
    pred0 = pred_q13[0];
    pred1 = pred_q13[1];
    for n in interp_len..frame_length {
        predict_side_sample(mid, side, n, pred0, pred1);
    }
    state.pred_prev_q13 = [pred_q13[0] as i16, pred_q13[1] as i16];

    // Convert to left/right signals.
    for n in 0..frame_length {
        let m = mid[n + 1] as i32;
        let s = side[n + 1] as i32;
        mid[n + 1] = sat16(m + s);
        side[n + 1] = sat16(m - s);
    }
}

/// Body of the `silk_stereo_MS_to_LR` sample loop:
/// `side[n+1] += pred0 * (mid[n] + 2*mid[n+1] + mid[n+2])/4 + pred1 *
/// mid[n+1]`, in the reference's Q formats (Q11 window, Q13 weights,
/// Q8 side accumulator).
#[inline]
fn predict_side_sample(mid: &[i16], side: &mut [i16], n: usize, pred0_q13: i32, pred1_q13: i32) {
    let sum_q11 = (mid[n] as i32 + mid[n + 2] as i32 + ((mid[n + 1] as i32) << 1)) << 9;
    let mut sum_q8 = smlawb((side[n + 1] as i32) << 8, sum_q11, pred0_q13);
    sum_q8 = smlawb(sum_q8, (mid[n + 1] as i32) << 11, pred1_q13);
    side[n + 1] = sat16(rshift_round(sum_q8, 8));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::range::RangeEncoder;

    fn lcg(state: &mut u32) -> u32 {
        *state = state.wrapping_mul(1664525).wrapping_add(1013904223);
        *state
    }

    /// Bounded random `i16` in `[-bound, bound]`.
    fn lcg_i16(state: &mut u32, bound: i32) -> i16 {
        ((lcg(state) % (2 * bound as u32 + 1)) as i32 - bound) as i16
    }

    /// Independent transcription of the RFC 6716 §4.2.7.1 dequantization
    /// (w1 from the second (u3, u5) pair, w0 from the first, minus w1),
    /// used to cross-check [`decode_pred`].
    fn reference_dequant(joint: u32, low: [u32; 2], sub: [u32; 2]) -> [i32; 2] {
        let wi = [
            (low[0] + 3 * (joint / 5)) as usize,
            (low[1] + 3 * (joint % 5)) as usize,
        ];
        let tab = &STEREO_PRED_QUANT_Q13;
        let step = |k: usize| (((tab[k + 1] as i64 - tab[k] as i64) * 6554) >> 16) as i32;
        let w1 = tab[wi[1]] as i32 + step(wi[1]) * (2 * sub[1] as i32 + 1);
        let w0 = tab[wi[0]] as i32 + step(wi[0]) * (2 * sub[0] as i32 + 1) - w1;
        [w0, w1]
    }

    fn encode_pred_indices(joint: u32, low: [u32; 2], sub: [u32; 2]) -> Vec<u8> {
        let mut enc = RangeEncoder::new();
        enc.encode_icdf(joint, &STEREO_PRED_JOINT_ICDF, 8);
        enc.encode_icdf(low[0], &UNIFORM3_ICDF, 8);
        enc.encode_icdf(sub[0], &UNIFORM5_ICDF, 8);
        enc.encode_icdf(low[1], &UNIFORM3_ICDF, 8);
        enc.encode_icdf(sub[1], &UNIFORM5_ICDF, 8);
        enc.done()
    }

    /// Every possible index combination round-trips, and the decoded
    /// weights equal the RFC formula (not just our own dequant code).
    #[test]
    fn decode_pred_round_trips_exhaustively() {
        for joint in 0..25u32 {
            for low0 in 0..3u32 {
                for sub0 in 0..5u32 {
                    for low1 in 0..3u32 {
                        for sub1 in 0..5u32 {
                            let bytes = encode_pred_indices(joint, [low0, low1], [sub0, sub1]);
                            let mut dec = RangeDecoder::new(&bytes);
                            let pred = decode_pred(&mut dec).unwrap();
                            assert_eq!(
                                pred,
                                reference_dequant(joint, [low0, low1], [sub0, sub1]),
                                "joint={joint} low={low0}/{low1} sub={sub0}/{sub1}"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn decode_mid_only_round_trips() {
        for flag in [false, true] {
            let mut enc = RangeEncoder::new();
            enc.encode_icdf(flag as u32, &STEREO_ONLY_CODE_MID_ICDF, 8);
            let bytes = enc.done();
            let mut dec = RangeDecoder::new(&bytes);
            assert_eq!(decode_mid_only(&mut dec).unwrap(), flag);
        }
    }

    /// A truncated frame does not error or panic: the range decoder
    /// reads zero-fill like the reference (`ec_dec` never fails on
    /// truncation; callers detect it via the bit position), so the
    /// decoded weights must still land inside the table range.
    #[test]
    fn decode_empty_frame_reads_zero_fill() {
        let mut dec = RangeDecoder::new(&[]);
        let pred = decode_pred(&mut dec).unwrap();
        assert!(pred[1].abs() <= 13732, "pred1={}", pred[1]);
        assert!(pred[0].abs() <= 2 * 13732, "pred0={}", pred[0]);
        let mut dec = RangeDecoder::new(&[]);
        decode_mid_only(&mut dec).unwrap();
    }

    /// With zero predictors the side prediction vanishes and the whole
    /// operation reduces to L = m + s, R = m - s on the delayed streams.
    #[test]
    fn ms_to_lr_zero_predictors_is_sum_and_difference() {
        const FRAME: usize = 160; // 20 ms at 16 kHz
        let mut rng = 42u32;
        let mut mid_in = [0i16; FRAME + 2];
        let mut side_in = [0i16; FRAME + 2];
        for k in 2..FRAME + 2 {
            mid_in[k] = lcg_i16(&mut rng, 15000);
            side_in[k] = lcg_i16(&mut rng, 15000);
        }
        let mut state = StereoDecState::default();
        let mut mid = mid_in;
        let mut side = side_in;
        ms_to_lr(&mut state, &mut mid, &mut side, &[0, 0], 16, FRAME);

        // First output sample pairs the (zero) history slots.
        assert_eq!(mid[1], 0);
        assert_eq!(side[1], 0);
        for n in 1..FRAME {
            let m = mid_in[n + 1] as i32;
            let s = side_in[n + 1] as i32;
            assert_eq!(mid[n + 1], sat16(m + s), "n={n}");
            assert_eq!(side[n + 1], sat16(m - s), "n={n}");
        }
        // History saved for the next frame is the input tail.
        assert_eq!(state.s_mid, [mid_in[FRAME], mid_in[FRAME + 1]]);
        assert_eq!(state.s_side, [side_in[FRAME], side_in[FRAME + 1]]);
        assert_eq!(state.pred_prev_q13, [0, 0]);
    }

    /// Steady-state (prev == pred) side prediction, checked against the
    /// RFC 6716 §4.2.8 formula written out independently in i64.
    #[test]
    fn ms_to_lr_side_prediction_matches_reference_formula() {
        const FRAME: usize = 160;
        let fs_khz = 16u32;
        let w = [6000i32, -9000i32]; // |w0| exceeds nothing; fits i16 store
        let mut rng = 7u32;
        let mut mid_in = [0i16; FRAME + 2];
        let mut side_in = [0i16; FRAME + 2];
        for k in 2..FRAME + 2 {
            mid_in[k] = lcg_i16(&mut rng, 1500);
            side_in[k] = lcg_i16(&mut rng, 1500);
        }
        let mut state = StereoDecState {
            pred_prev_q13: [w[0] as i16, w[1] as i16],
            ..StereoDecState::default()
        };
        let mut mid = mid_in;
        let mut side = side_in;
        ms_to_lr(&mut state, &mut mid, &mut side, &w, fs_khz, FRAME);

        for n in 1..FRAME {
            let m_sum = (mid_in[n] as i64) + 2 * (mid_in[n + 1] as i64) + (mid_in[n + 2] as i64);
            let side_q8 = (side_in[n + 1] as i64) * 256
                + (((m_sum << 9) * w[0] as i64) >> 16)
                + ((((mid_in[n + 1] as i64) << 11) * w[1] as i64) >> 16);
            let want_side = (((side_q8 >> 7) + 1) >> 1).clamp(-32768, 32767);
            let want_left = (mid_in[n + 1] as i64 + want_side).clamp(-32768, 32767);
            let want_right = (mid_in[n + 1] as i64 - want_side).clamp(-32768, 32767);
            assert_eq!(mid[n + 1] as i64, want_left, "n={n}");
            assert_eq!(side[n + 1] as i64, want_right, "n={n}");
        }
    }

    /// The first 8 ms ramp the predictors from the previous frame's
    /// values (per-sample rounded step); afterwards the exact target
    /// values apply. Uses w0 = 4000 whose step 4000*1024/65536 = 62.5
    /// rounds to 63, so the ramp *overshoots* (63*64 = 4032) and the
    /// phase boundary at n = 64 is observable.
    #[test]
    fn ms_to_lr_ramps_predictors_over_first_8ms() {
        const FRAME: usize = 80; // 10 ms at 8 kHz; ramp = 64 samples
        let fs_khz = 8u32;
        let pred = [4000i32, 8192i32];
        let mut rng = 99u32;
        let mut mid_in = [0i16; FRAME + 2];
        for slot in mid_in[2..].iter_mut() {
            *slot = lcg_i16(&mut rng, 1000);
        }
        let mut state = StereoDecState::default();
        let mut mid = mid_in;
        let mut side = [0i16; FRAME + 2];
        ms_to_lr(&mut state, &mut mid, &mut side, &pred, fs_khz, FRAME);

        let w0_at = |n: usize| {
            if n < 64 {
                63 * (n as i32 + 1)
            } else {
                4000
            }
        };
        let w1_at = |n: usize| {
            if n < 64 {
                128 * (n as i32 + 1)
            } else {
                8192
            }
        };
        for n in 1..FRAME {
            let m_sum = (mid_in[n] as i64) + 2 * (mid_in[n + 1] as i64) + (mid_in[n + 2] as i64);
            let side_q8 = (((m_sum << 9) * w0_at(n) as i64) >> 16)
                + ((((mid_in[n + 1] as i64) << 11) * w1_at(n) as i64) >> 16);
            let want_side = (((side_q8 >> 7) + 1) >> 1).clamp(-32768, 32767);
            let want_left = (mid_in[n + 1] as i64 + want_side).clamp(-32768, 32767);
            let want_right = (mid_in[n + 1] as i64 - want_side).clamp(-32768, 32767);
            assert_eq!(mid[n + 1] as i64, want_left, "n={n}");
            assert_eq!(side[n + 1] as i64, want_right, "n={n}");
        }
        // Sanity: the ramp really does overshoot, i.e. phase boundary n=63
        // (w0=4032) and n=64 (w0=4000) use different weights.
        let m_sum =
            |n: usize| (mid_in[n] as i64) + 2 * (mid_in[n + 1] as i64) + (mid_in[n + 2] as i64);
        assert_ne!(
            ((m_sum(63) << 9) * 4032) >> 16,
            ((m_sum(63) << 9) * 4000) >> 16
        );
    }

    /// Frame k's first output sample must pair frame k-1's last mid
    /// sample with frame k-1's last side residual.
    #[test]
    fn ms_to_lr_history_carries_across_frames() {
        const FRAME: usize = 80;
        let fs_khz = 8u32;
        let mut rng = 1234u32;
        let mut frame1_mid = [0i16; FRAME + 2];
        let mut frame1_side = [0i16; FRAME + 2];
        for k in 2..FRAME + 2 {
            frame1_mid[k] = lcg_i16(&mut rng, 9000);
            frame1_side[k] = lcg_i16(&mut rng, 9000);
        }
        let mut state = StereoDecState::default();
        ms_to_lr(
            &mut state,
            &mut frame1_mid,
            &mut frame1_side,
            &[0, 0],
            fs_khz,
            FRAME,
        );

        // Frame 2 starts with fresh input buffers; its first output must
        // be built from the stashed tail of frame 1.
        let mut frame2_mid = [0i16; FRAME + 2];
        let mut frame2_side = [0i16; FRAME + 2];
        for k in 2..FRAME + 2 {
            frame2_mid[k] = lcg_i16(&mut rng, 9000);
            frame2_side[k] = lcg_i16(&mut rng, 9000);
        }
        let mid2_in = frame2_mid;
        let side2_in = frame2_side;
        ms_to_lr(
            &mut state,
            &mut frame2_mid,
            &mut frame2_side,
            &[0, 0],
            fs_khz,
            FRAME,
        );
        let m1_last = frame1_mid[FRAME + 1] as i32; // == s_mid[1]
        let r1_last = frame1_side[FRAME + 1] as i32; // == s_side[1]
        assert_eq!(frame2_mid[1], sat16(m1_last + r1_last));
        assert_eq!(frame2_side[1], sat16(m1_last - r1_last));
        // And from sample 1 on, frame 2 pairs its own inputs.
        for n in 1..FRAME {
            let m = mid2_in[n + 1] as i32;
            let s = side2_in[n + 1] as i32;
            assert_eq!(frame2_mid[n + 1], sat16(m + s), "n={n}");
            assert_eq!(frame2_side[n + 1], sat16(m - s), "n={n}");
        }
    }

    /// Extreme inputs saturate exactly where the reference would.
    #[test]
    fn ms_to_lr_saturates_extreme_inputs() {
        const FRAME: usize = 80;
        let all_mid = [i16::MAX; FRAME + 2];
        for side_val in [i16::MAX, i16::MIN] {
            let mut state = StereoDecState::default();
            let mut mid = all_mid;
            let mut side = [side_val; FRAME + 2];
            ms_to_lr(&mut state, &mut mid, &mut side, &[0, 0], 8, FRAME);
            for n in 1..FRAME {
                let m = i16::MAX as i32;
                let s = side_val as i32;
                assert_eq!(mid[n + 1], sat16(m + s), "side={side_val} n={n}");
                assert_eq!(side[n + 1], sat16(m - s), "side={side_val} n={n}");
            }
        }
    }

    /// End-to-end recovery: synthesize left/right from an AR(2)-ish mid
    /// plus a shaped side signal, invert the unmixing to build the side
    /// residual a coder would have sent (rounded to the nearest i16 —
    /// the residual's natural granularity), and check the decoder
    /// reproduces L/R to within the resulting ±1 LSB quantization error.
    #[test]
    fn ms_to_lr_recovers_synthetic_stereo() {
        const FRAMES: usize = 3;
        const FRAME: usize = 160; // 20 ms at 8 kHz
        const TOTAL: usize = FRAMES * FRAME;
        let fs_khz = 8u32;
        let w = [6000i32, 9000i32]; // steady state: prev == pred

        // Mid stream (input to the frames), AR(2)-flavored and bounded.
        let mut mid_in = [0i32; TOTAL];
        let (mut p1, mut p2) = (300i32, 100i32);
        let mut rng = 20250917u32;
        for slot in mid_in.iter_mut() {
            let e = lcg_i16(&mut rng, 400) as i32;
            let m = ((3 * p1 + p2) / 4 + e).clamp(-2000, 2000);
            *slot = m;
            p2 = p1;
            p1 = m;
        }
        // Delayed mid: mid_d[k] pairs with output sample k; the initial
        // history is zero.
        let mut mid_d = [0i64; TOTAL + 1];
        for k in 1..=TOTAL {
            mid_d[k] = mid_in[k - 1] as i64;
        }
        // Desired processed side per output sample.
        let side_desired: Vec<i64> = (0..TOTAL)
            .map(|k| ((k * 941) % 6001) as i64 - 3000)
            .collect();
        // Invert: residual[r] feeds output sample r+1 with the window
        // (mid_d[r], mid_d[r+1], mid_d[r+2]); rounded to the nearest
        // representable i16 residual.
        let mut residual = [0i16; TOTAL];
        for r in 0..TOTAL - 1 {
            let k = r + 1;
            let m_sum = mid_d[r] + 2 * mid_d[r + 1] + mid_d[r + 2];
            let t0 = ((m_sum << 9) * w[0] as i64) >> 16;
            let t1 = ((mid_d[r + 1] << 11) * w[1] as i64) >> 16;
            residual[r] = (((side_desired[k] * 256 - t0 - t1 + 128) >> 8) as i32) as i16;
        }
        // residual[TOTAL-1] only ever lands in the saved history; zero.

        let mut state = StereoDecState {
            pred_prev_q13: [w[0] as i16, w[1] as i16],
            ..StereoDecState::default()
        };
        for f in 0..FRAMES {
            let mut mid = [0i16; FRAME + 2];
            let mut side = [0i16; FRAME + 2];
            let mid_frame: Vec<i16> = mid_in[f * FRAME..(f + 1) * FRAME]
                .iter()
                .map(|&m| m as i16)
                .collect();
            mid[2..FRAME + 2].copy_from_slice(&mid_frame);
            side[2..FRAME + 2].copy_from_slice(&residual[f * FRAME..(f + 1) * FRAME]);
            ms_to_lr(&mut state, &mut mid, &mut side, &w, fs_khz, FRAME);
            for n in 0..FRAME {
                let k = f * FRAME + n;
                if k == 0 {
                    // Output 0 consumes the zero-initialized history.
                    continue;
                }
                let want_left = mid_d[k] + side_desired[k];
                let want_right = mid_d[k] - side_desired[k];
                assert!(
                    (mid[n + 1] as i64 - want_left).abs() <= 1,
                    "left k={k}: {} vs {want_left}",
                    mid[n + 1]
                );
                assert!(
                    (side[n + 1] as i64 - want_right).abs() <= 1,
                    "right k={k}: {} vs {want_right}",
                    side[n + 1]
                );
            }
        }
    }

    /// Corrupt/truncated entropy data fails cleanly; successfully
    /// decoded weights always stay inside the quantization table's range.
    #[test]
    fn decode_pred_fuzz_never_panics() {
        let mut rng = 0xC0FFEEu32;
        for _ in 0..2000 {
            let len = (lcg(&mut rng) % 24 + 1) as usize;
            let buf: Vec<u8> = (0..len).map(|_| lcg(&mut rng) as u8).collect();
            let mut dec = RangeDecoder::new(&buf);
            if let Ok(pred) = decode_pred(&mut dec) {
                assert!(pred[1].abs() <= 13732, "pred1={}", pred[1]);
                assert!(pred[0].abs() <= 2 * 13732, "pred0={}", pred[0]);
            }
        }
    }

    /// Arbitrary (even out-of-table-range) predictors and extreme buffers
    /// never panic, including every internal fs/frame-size combination.
    #[test]
    fn ms_to_lr_fuzz_never_panics() {
        let mut rng = 0xDEADBEEFu32;
        for _ in 0..500 {
            let fs_khz = [8u32, 12, 16, 24][lcg(&mut rng) as usize % 4];
            let frame_length = (10 * fs_khz as usize) << (lcg(&mut rng) % 2);
            let mut mid = [0i16; 482];
            let mut side = [0i16; 482];
            for k in 0..frame_length + 2 {
                mid[k] = lcg(&mut rng) as u16 as i16;
                side[k] = lcg(&mut rng) as u16 as i16;
            }
            let w0 = (lcg(&mut rng) as i16 as i32) * 2;
            let w1 = (lcg(&mut rng) as i16 as i32) * 2;
            let mut state = StereoDecState::default();
            ms_to_lr(
                &mut state,
                &mut mid[..frame_length + 2],
                &mut side[..frame_length + 2],
                &[w0, w1],
                fs_khz,
                frame_length,
            );
        }
    }
}
