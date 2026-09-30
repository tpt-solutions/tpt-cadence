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

/// Highest LPC order the encoder searches (RFC 9639 allows up to 32; 12 is
/// the reference encoder's `-8` setting and the usual sweet spot).
pub const MAX_ENCODE_ORDER: usize = 12;

/// Autocorrelation of `x` for lags `0..out.len()`.
pub fn autocorrelation(x: &[f64], out: &mut [f64]) {
    for (lag, slot) in out.iter_mut().enumerate() {
        *slot = if lag < x.len() {
            x[lag..].iter().zip(x).map(|(a, b)| a * b).sum()
        } else {
            0.0
        };
    }
}

/// Levinson-Durbin recursion. Given autocorrelation `autoc[0..=max_order]`,
/// returns the predictor coefficients for every order `1..=max_order`
/// (`result[o - 1]` holds the `o` coefficients of the order-`o` predictor,
/// nearest sample first, in the `x[i] ~ sum(c[j] * x[i-1-j])` convention).
/// Recursion stops early if the prediction error collapses, so the result
/// may hold fewer than `max_order` entries.
pub fn levinson_durbin(autoc: &[f64], max_order: usize) -> Vec<Vec<f64>> {
    let mut out: Vec<Vec<f64>> = Vec::with_capacity(max_order);
    if autoc.is_empty() || autoc[0] <= 0.0 {
        return out;
    }
    let mut err = autoc[0];
    let mut a: Vec<f64> = Vec::with_capacity(max_order);
    for m in 0..max_order.min(autoc.len() - 1) {
        let mut acc = autoc[m + 1];
        for (j, &aj) in a.iter().enumerate() {
            acc -= aj * autoc[m - j];
        }
        let k = acc / err;
        let prev = a.clone();
        for (j, aj) in a.iter_mut().enumerate() {
            *aj -= k * prev[m - 1 - j];
        }
        a.push(k);
        err *= 1.0 - k * k;
        out.push(a.clone());
        if err <= autoc[0] * 1e-12 {
            break;
        }
    }
    out
}

/// Quantizes real-valued predictor `coefs` to `precision`-bit signed
/// integers with a right-shift in `0..=15` (the range FLAC allows), using
/// running error feedback so rounding errors don't accumulate. Returns
/// `None` when the coefficients can't be represented (all zero, non-finite,
/// or a required shift below zero).
pub fn quantize_coefficients(coefs: &[f64], precision: u32) -> Option<(Vec<i64>, u32)> {
    let cmax = coefs.iter().fold(0.0f64, |m, c| m.max(c.abs()));
    if !cmax.is_finite() || cmax <= 0.0 {
        return None;
    }
    // frexp exponent: cmax = m * 2^e with m in [0.5, 1).
    let e = cmax.log2().floor() as i32 + 1;
    let shift = precision as i32 - e - 1;
    if shift < 0 {
        return None;
    }
    let shift = shift.min(15) as u32;
    let qmax = (1i64 << (precision - 1)) - 1;
    let qmin = -(1i64 << (precision - 1));
    let scale = (1u64 << shift) as f64;
    let mut error = 0.0f64;
    let q = coefs
        .iter()
        .map(|&c| {
            let v = c * scale + error;
            let r = (v.round() as i64).clamp(qmin, qmax);
            error = v - r as f64;
            r
        })
        .collect();
    Some((q, shift))
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
    fn levinson_recovers_ar2_process() {
        // x[n] = 1.5 x[n-1] - 0.7 x[n-2] + tiny drive: order-2 coefficients
        // must come back close to (1.5, -0.7) and the residual must shrink.
        let mut x = vec![1.0f64, 0.5];
        let mut seed = 12345u32;
        for i in 2..4000 {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            let noise = ((seed >> 16) as f64 / 65536.0 - 0.5) * 1.0;
            let v = 1.5 * x[i - 1] - 0.7 * x[i - 2] + noise;
            x.push(v);
        }
        let mut ac = [0.0; 5];
        autocorrelation(&x, &mut ac);
        let orders = levinson_durbin(&ac, 4);
        assert_eq!(orders.len(), 4);
        assert!((orders[1][0] - 1.5).abs() < 0.05, "{:?}", orders[1]);
        assert!((orders[1][1] + 0.7).abs() < 0.05, "{:?}", orders[1]);
    }

    #[test]
    fn quantize_keeps_precision_and_shift_in_range() {
        let (q, shift) = quantize_coefficients(&[1.5, -0.7, 0.2], 12).unwrap();
        assert!(shift <= 15);
        let recon: Vec<f64> = q
            .iter()
            .map(|&v| v as f64 / (1u64 << shift) as f64)
            .collect();
        assert!((recon[0] - 1.5).abs() < 1e-2 && (recon[1] + 0.7).abs() < 1e-2);
        assert!(q.iter().all(|&v| v.abs() < (1 << 11)));
        assert!(quantize_coefficients(&[0.0, 0.0], 12).is_none());
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
