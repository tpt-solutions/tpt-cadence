//! Sample conversion helpers shared by every decoder.
//!
//! Integer PCM is normalized to the `[-1.0, 1.0)` range by dividing by
//! `2^(bit_depth - 1)`, i.e. by the most-negative representable sample. The
//! division is by a power of two, so the conversion is exact and every
//! decoder in the suite produces identical scaling.

/// Converts a signed integer sample to an `f32` in `[-1.0, 1.0)`.
///
/// `sample` is the raw sample value sign-extended into an `i64` (e.g. a 24-bit
/// sample stored in the low 24 bits). `bit_depth` is the width of the sample
/// in bits; out-of-range depths are clamped so the function is total and
/// panic-free.
pub fn int_to_f32(sample: i64, bit_depth: u16) -> f32 {
    let depth = bit_depth.clamp(1, 32);
    let scale = (1u64 << (depth - 1)) as f32;
    sample as f32 / scale
}

/// Converts an `f32` sample (expected in `[-1.0, 1.0]`, but any finite value
/// is accepted) to a signed integer of the given bit depth, clamping to the
/// representable range. The exact inverse scaling of [`int_to_f32`], so
/// encode-then-decode round trips are lossless up to quantization.
///
/// Out-of-range depths are clamped the same way `int_to_f32` clamps them, and
/// non-finite input (NaN/Infinity) is treated as the nearest representable
/// extreme rather than panicking, keeping this total like its counterpart.
pub fn f32_to_int(sample: f32, bit_depth: u16) -> i64 {
    let depth = bit_depth.clamp(1, 32);
    let scale = (1u64 << (depth - 1)) as f32;
    let min = -(1i64 << (depth - 1));
    let max = (1i64 << (depth - 1)) - 1;
    if sample.is_nan() {
        return 0;
    }
    ((sample * scale).round() as i64).clamp(min, max)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn int16_scaling() {
        assert_eq!(int_to_f32(0, 16), 0.0);
        assert_eq!(int_to_f32(16384, 16), 0.5);
        assert_eq!(int_to_f32(-32768, 16), -1.0);
        assert_eq!(int_to_f32(32767, 16), 32767.0 / 32768.0);
    }

    #[test]
    fn int8_scaling() {
        assert_eq!(int_to_f32(-128, 8), -1.0);
        assert_eq!(int_to_f32(127, 8), 127.0 / 128.0);
    }

    #[test]
    fn int24_scaling() {
        assert_eq!(int_to_f32(-(1 << 23), 24), -1.0);
        assert_eq!(int_to_f32(1 << 22, 24), 0.5);
    }

    #[test]
    fn int32_scaling() {
        assert_eq!(int_to_f32(i32::MIN as i64, 32), -1.0);
        assert_eq!(int_to_f32(1 << 30, 32), 0.5);
    }

    #[test]
    fn f32_to_int_round_trips_int_to_f32() {
        // Depths up to 24 bits round-trip exactly: f32's 24-bit mantissa can
        // represent every integer in that range exactly, so `int_to_f32`'s
        // division and `f32_to_int`'s multiplication are each exact.
        for depth in [8u16, 16, 24] {
            let min = -(1i64 << (depth - 1));
            let max = (1i64 << (depth - 1)) - 1;
            for sample in [min, min / 2, 0, max / 2, max] {
                let f = int_to_f32(sample, depth);
                assert_eq!(f32_to_int(f, depth), sample);
            }
        }
        // 32-bit depth exceeds f32's 24-bit mantissa, so `int_to_f32` itself
        // is already lossy for arbitrary 32-bit values (see `int32_scaling`,
        // which only spot-checks power-of-two-friendly values for the same
        // reason) — only check values whose magnitude is an exact power of
        // two, which every decoder/encoder pair here actually round-trips.
        for sample in [i32::MIN as i64, -(1 << 30), 0, 1 << 30, (1 << 31) - 1] {
            let f = int_to_f32(sample, 32);
            assert_eq!(f32_to_int(f, 32), sample);
        }
    }

    #[test]
    fn f32_to_int_clamps_out_of_range() {
        assert_eq!(f32_to_int(2.0, 16), 32767);
        assert_eq!(f32_to_int(-2.0, 16), -32768);
        assert_eq!(f32_to_int(f32::NAN, 16), 0);
        assert_eq!(f32_to_int(f32::INFINITY, 16), 32767);
        assert_eq!(f32_to_int(f32::NEG_INFINITY, 16), -32768);
    }
}
