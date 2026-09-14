//! 80-bit IEEE 754 extended-precision sample-rate field used in AIFF `COMM`
//! chunks (also known as the Motorola 68881 80-bit extended format).
//!
//! Layout (10 bytes, big-endian): 1 sign bit, 15 exponent bits (bias 16383,
//! no implied integer bit — the significand's leading 1 is *explicit*), and
//! a 64-bit significand. The value is `significand × 2^(exponent − 16383 − 63)`.
//!
//! Both directions are provided because writers (and tests) need to encode
//! sample rates back into this format.

/// Decodes the 10-byte extended-precision field to `f64`.
///
/// Returns `None` for infinities and NaNs, which are meaningless sample rates.
pub fn extended_to_f64(bytes: &[u8; 10]) -> Option<f64> {
    let negative = bytes[0] & 0x80 != 0;
    let exponent = (((bytes[0] & 0x7F) as u32) << 8) | bytes[1] as u32;
    let mut significand: u64 = 0;
    for byte in &bytes[2..10] {
        significand = (significand << 8) | *byte as u64;
    }

    if exponent == 0x7FFF {
        // Infinity / NaN (integer bit distinguishes, but both are unusable).
        return None;
    }

    let value = if significand == 0 {
        0.0
    } else if exponent == 0 {
        // Unnormal/denormal: 2^(1 - 16383) scale.
        (significand as f64) * 2f64.powi(1 - 16383 - 63)
    } else {
        (significand as f64) * 2f64.powi(exponent as i32 - 16383 - 63)
    };

    Some(if negative { -value } else { value })
}

/// Encodes an `f64` sample rate as the 10-byte extended-precision field.
///
/// Supports zero and normal values; subnormals beyond `f64` resolution round
/// through `f64` semantics. Integer sample rates encode exactly.
pub fn f64_to_extended(value: f64) -> [u8; 10] {
    let mut out = [0u8; 10];
    if value == 0.0 || !value.is_finite() {
        // Zero encodes as all-zero; non-finite rates have no sane encoding
        // and collapse to zero rather than emitting infinity.
        return out;
    }

    let bits = value.to_bits();
    let negative = (bits >> 63) as u8;
    let biased_exp11 = ((bits >> 52) & 0x7FF) as i32;
    let frac52 = bits & ((1u64 << 52) - 1);

    let (exponent15, significand64) = if biased_exp11 == 0 {
        // f64 subnormal: value = frac × 2^-1074. Normalize so the leading 1
        // sits at bit 63 (the explicit integer bit position).
        let shift = frac52.leading_zeros() as i32; // MSB position of frac
        let significand = frac52 << shift;
        let exp2 = -1074 - shift + 63;
        ((exp2 + 16383) as u32, significand)
    } else {
        // value = 1.f × 2^e; significand64 = 1.f × 2^63 (integer bit explicit).
        let exp2 = biased_exp11 - 1023;
        ((exp2 + 16383) as u32, (frac52 | (1 << 52)) << 11)
    };

    out[0] = (negative << 7) | ((exponent15 >> 8) as u8 & 0x7F);
    out[1] = (exponent15 & 0xFF) as u8;
    out[2..10].copy_from_slice(&significand64.to_be_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_sample_rates() {
        // Well-known encodings for the two most common rates.
        let mut cd = [0u8; 10];
        cd[0] = 0x40;
        cd[1] = 0x0E;
        cd[2] = 0xAC;
        cd[3] = 0x44;
        assert_eq!(extended_to_f64(&cd), Some(44_100.0));
        assert_eq!(f64_to_extended(44_100.0), cd);

        let mut pro = [0u8; 10];
        pro[0] = 0x40;
        pro[1] = 0x0E;
        pro[2] = 0xBB;
        pro[3] = 0x80;
        assert_eq!(extended_to_f64(&pro), Some(48_000.0));
        assert_eq!(f64_to_extended(48_000.0), pro);
    }

    #[test]
    fn roundtrip_common_rates() {
        for rate in [
            8_000.0, 11_025.0, 16_000.0, 22_050.0, 32_000.0, 88_200.0, 96_000.0, 192_000.0,
        ] {
            assert_eq!(extended_to_f64(&f64_to_extended(rate)), Some(rate));
        }
    }

    #[test]
    fn zero_and_rejects() {
        assert_eq!(extended_to_f64(&[0u8; 10]), Some(0.0));
        assert_eq!(f64_to_extended(0.0), [0u8; 10]);
        let mut inf = [0u8; 10];
        inf[0] = 0x7F;
        inf[1] = 0xFF;
        assert_eq!(extended_to_f64(&inf), None);
    }
}
