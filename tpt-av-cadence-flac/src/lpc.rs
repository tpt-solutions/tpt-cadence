//! Prediction: FLAC fixed predictors of order 0–4 and general LPC
//! prediction (RFC 9639 §9.2.4–9.2.6).
//!
//! Both functions restore a block in place: `block[..predictor_order]`
//! holds the decoded warm-up samples and `block[predictor_order..]` holds
//! the residuals, which are replaced by the reconstructed samples.

/// Fixed predictor coefficients for orders 0..=4.
pub fn fixed_coefficients(order: usize) -> &'static [i64] {
    match order {
        0 => &[],
        1 => &[1],
        2 => &[2, -1],
        3 => &[3, -3, 1],
        4 => &[4, -6, 4, -1],
        _ => unreachable!("FLAC fixed predictor orders are 0..=4"),
    }
}

/// Applies the fixed predictor of `order`. Panics only on orders > 4,
/// which callers reject during parsing.
pub fn restore_fixed(block: &mut [i32], order: usize) {
    let coefs = fixed_coefficients(order);
    for i in order..block.len() {
        let mut acc: i64 = 0;
        // Coefficient j applies to x[i-1-j] (nearest sample first).
        for (j, &c) in coefs.iter().enumerate() {
            acc = acc.wrapping_add(c.wrapping_mul(block[i - 1 - j] as i64));
        }
        block[i] = block[i].wrapping_add(acc as i32);
    }
}

/// Computes a bounded first-order LPC predictor from integer samples.
///
/// FLAC's general LPC syntax is fully supported by the decoder; this encoder
/// intentionally starts with a stable first-order coefficient derived from
/// normalized autocorrelation. The returned coefficient is already quantized
/// for the requested shift and is suitable for direct bitstream emission.
pub fn analyze_lpc(samples: &[i32], order: usize, shift: u32) -> Vec<i64> {
    if order == 0 || samples.len() <= order {
        return vec![0; order];
    }
    debug_assert_eq!(
        order, 1,
        "the FLAC encoder currently searches first-order LPC only"
    );
    let mut energy = 0i128;
    let mut correlation = 0i128;
    for i in 1..samples.len() {
        energy += samples[i] as i128 * samples[i] as i128;
        correlation += samples[i] as i128 * samples[i - 1] as i128;
    }
    if energy == 0 {
        return vec![0];
    }
    let scale = 1i128 << shift;
    let coefficient = ((correlation * scale) / energy).clamp(-(1i128 << 14), (1i128 << 14) - 1);
    vec![coefficient as i64]
}

/// Applies the general LPC predictor with the given quantized coefficients
/// and right-shift.
pub fn restore_lpc(block: &mut [i32], coefs: &[i64], shift: u32) {
    for i in coefs.len()..block.len() {
        let mut acc: i64 = 0;
        // Coefficient j applies to x[i-1-j] (nearest sample first).
        for (j, &c) in coefs.iter().enumerate() {
            acc = acc.wrapping_add(c.wrapping_mul(block[i - 1 - j] as i64));
        }
        block[i] = block[i].wrapping_add((acc >> shift) as i32);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_order_zero_is_passthrough() {
        // Order 0 predicts nothing; residuals are the samples themselves.
        let mut block = vec![10, 5, 9, -7];
        restore_fixed(&mut block, 0);
        assert_eq!(block, vec![10, 5, 9, -7]);
    }

    #[test]
    fn fixed_order_one_integrates() {
        // Order 1 predicts sample[i] = sample[i-1]; residual is the delta.
        let mut block = vec![10, 2, -3, 1]; // samples 10, 12, 9, 10
        restore_fixed(&mut block, 1);
        assert_eq!(block, vec![10, 12, 9, 10]);
    }

    #[test]
    fn fixed_order_two_second_difference() {
        // Coefs [2, -1]: x[i] = 2x[i-1] - x[i-2] + r[i].
        // Samples 0, 1, 3, 6 (second differences 1, 1, 1).
        let mut block = vec![0, 1, 1, 1];
        restore_fixed(&mut block, 2);
        assert_eq!(block, vec![0, 1, 3, 6]);
    }

    #[test]
    fn fixed_order_four() {
        // Coefs [4, -6, 4, -1] reproduce a cubic sequence with zero residual.
        // Samples of t^3: 0, 1, 8, 27, then pure prediction.
        let mut block = vec![0, 1, 8, 27, 0, 0, 0];
        restore_fixed(&mut block, 4);
        assert_eq!(block, vec![0, 1, 8, 27, 64, 125, 216]);
    }

    #[test]
    fn analyze_first_order_lpc_tracks_correlation() {
        let samples = vec![10, 20, 40, 80, 160, 320];
        let coefficients = analyze_lpc(&samples, 1, 12);
        assert_eq!(coefficients.len(), 1);
        assert!(coefficients[0] > 0);
        assert!(coefficients[0] < (1i64 << 14));
    }

    #[test]
    fn lpc_shift_and_coefs() {
        // Trivial LPC: order 1, coef 8, shift 3: x[i] = (8 * x[i-1]) >> 3 + r.
        let mut block = vec![100, 5, 7, -2];
        restore_lpc(&mut block, &[8], 3);
        assert_eq!(block, vec![100, 105, 112, 110]);
    }

    #[test]
    fn lpc_negative_accumulator_shifts_toward_negative_infinity() {
        // Arithmetic shift: (8 * 3) >> 3 = 3; with coef -8: (-8*3)>>3 = -3.
        let mut block = vec![3, 0];
        restore_lpc(&mut block, &[-8], 3);
        assert_eq!(block[1], (-3));
    }
}
