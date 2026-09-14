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
}
