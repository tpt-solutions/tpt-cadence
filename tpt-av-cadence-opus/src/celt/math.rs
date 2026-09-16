//! Float-build math helpers from `celt/mathops.h` / `celt/entcode.h`.
//!
//! The port targets libopus's default float build (no `FIXED_POINT`, no
//! `FLOAT_APPROX`), where `celt_log2`/`celt_exp2` go through the platform
//! double-precision `log`/`exp` and the result is narrowed to `f32`.

// Lints: this file ports libopus verbatim - verbatim float literals and
// reference loop shapes trigger these lints (see tables.rs).
#![allow(
    clippy::excessive_precision,
    clippy::approx_constant,
    clippy::precedence,
    clippy::needless_range_loop,
    clippy::manual_memcpy,
    clippy::too_many_arguments
)]

/// `EC_ILOG(x)`: index of the highest set bit (undefined for 0).
#[inline]
pub(crate) fn ec_ilog(v: u32) -> u32 {
    32 - v.leading_zeros()
}

/// `celt_ilog2(x) = EC_ILOG(x)-1` (float build takes `f32` promoted to
/// `i32` at the call sites; here for `u32`-valued arguments).
#[inline]
#[allow(dead_code)]
pub(crate) fn celt_ilog2(x: i32) -> i32 {
    ec_ilog(x as u32) as i32 - 1
}

/// `celt_udiv`: tested-exhaustive division; plain `/` without
/// `USE_SMALL_DIV_TABLE`.
#[inline]
pub(crate) fn celt_udiv(n: u32, d: u32) -> u32 {
    debug_assert!(d > 0);
    n / d
}

/// `celt_sudiv`: signed variant (C truncating division, like Rust).
#[inline]
pub(crate) fn celt_sudiv(n: i32, d: i32) -> i32 {
    debug_assert!(d > 0);
    n / d
}

/// `isqrt32`: `floor(sqrt(val))` with exact integer arithmetic.
pub(crate) fn isqrt32(mut val: u32) -> u32 {
    let bshift = (ec_ilog(val).max(1) - 1) >> 1;
    let mut b = 1u32 << bshift;
    let mut g = 0u32;
    let mut s = bshift as i32;
    loop {
        let t = ((g << 1) + b) << s;
        if t <= val {
            g += b;
            val -= t;
        }
        b >>= 1;
        s -= 1;
        if s < 0 {
            break;
        }
    }
    g
}

/// `fast_atan2f` (float build): minimax rational approximation of `atan2`.
#[allow(dead_code)]
pub(crate) fn fast_atan2f(y: f32, x: f32) -> f32 {
    const C_A: f32 = 0.431_579_74;
    const C_B: f32 = 0.678_484_03;
    const C_C: f32 = 0.085_955_42;
    const C_E: f32 = std::f32::consts::PI / 2.0;
    let x2 = x * x;
    let y2 = y * y;
    // For very small values, we don't care about the answer.
    if x2 + y2 < 1e-18 {
        return 0.0;
    }
    if x2 < y2 {
        let den = (y2 + C_B * x2) * (y2 + C_C * x2);
        -x * y * (y2 + C_A * x2) / den + if y < 0.0 { -C_E } else { C_E }
    } else {
        let den = (x2 + C_B * y2) * (x2 + C_C * y2);
        x * y * (x2 + C_A * y2) / den + if y < 0.0 { -C_E } else { C_E }
            - if x * y < 0.0 { -C_E } else { C_E }
    }
}

/// `celt_sqrt` (float build): `(float)sqrt((double)x)`.
#[inline]
pub(crate) fn celt_sqrt(x: f32) -> f32 {
    (x as f64).sqrt() as f32
}

/// `celt_rsqrt` (float build): `1.f / celt_sqrt(x)`.
#[inline]
#[allow(dead_code)]
pub(crate) fn celt_rsqrt(x: f32) -> f32 {
    1.0 / celt_sqrt(x)
}

/// `celt_rcp` (float build).
#[inline]
#[allow(dead_code)]
pub(crate) fn celt_rcp(x: f32) -> f32 {
    1.0 / x
}

/// `celt_div` (float build).
#[inline]
pub(crate) fn celt_div(a: f32, b: f32) -> f32 {
    a / b
}

/// `frac_div32` (float build).
#[inline]
#[allow(dead_code)]
pub(crate) fn frac_div32(a: f32, b: f32) -> f32 {
    a / b
}

/// `celt_log2` (float build, no `FLOAT_APPROX`): double-precision `log`,
/// scaled and narrowed exactly like the reference macro.
#[inline]
#[allow(dead_code)]
pub(crate) fn celt_log2(x: f32) -> f32 {
    (1.442_695_040_888_963_f64 * (x as f64).ln()) as f32
}

/// `celt_exp2` (float build, no `FLOAT_APPROX`).
#[inline]
pub(crate) fn celt_exp2(x: f32) -> f32 {
    (0.693_147_180_559_945_3_f64 * x as f64).exp() as f32
}

/// `celt_cos_norm` (float build): `cos(PI/2 * x)` computed exactly like the
/// reference macro (`(.5f*PI)*(x)` in `f32`, then `cos` in `f64`).
#[inline]
pub(crate) fn celt_cos_norm(x: f32) -> f32 {
    const PI: f32 = 3.141_592_653;
    ((0.5f32 * PI * x) as f64).cos() as f32
}

/// `EPSILON` (arch.h float build).
pub(crate) const EPSILON: f32 = 1e-15;

/// `VERY_SMALL` (arch.h float build), used by the deemphasis filter.
pub(crate) const VERY_SMALL: f32 = 1e-30;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn isqrt_matches_floor_sqrt() {
        for v in [1u32, 2, 3, 4, 15, 16, 17, 255, 256, 1 << 20, u32::MAX] {
            assert_eq!(isqrt32(v), (v as f64).sqrt() as u32, "v={v}");
        }
        for v in 1u32..=100_000 {
            assert_eq!(isqrt32(v), (v as f64).sqrt() as u32, "v={v}");
        }
    }

    #[test]
    fn atan2_close_to_libm() {
        for &(y, x) in &[
            (0.5f32, 0.5f32),
            (-0.3, 0.9),
            (0.9, -0.3),
            (-0.9, -0.3),
            (1e-5, 1.0),
            (0.123, 0.001),
        ] {
            let want = y.atan2(x);
            let got = fast_atan2f(y, x);
            // The reference documents this minimax approximation only as
            // "close"; its observed max error is a few 1e-5.
            assert!((want - got).abs() < 3e-5, "atan2({y},{x}): {got} vs {want}");
        }
    }

    #[test]
    fn log2_exp2_round_trip() {
        for x in [0.001f32, 0.5, 1.0, 2.0, 100.0, 1e10] {
            assert!((celt_log2(x) - x.log2()).abs() < 1e-5);
        }
        for x in [-10.0f32, -1.0, 0.0, 0.5, 7.25] {
            assert!((celt_exp2(x) - x.exp2()).abs() < 1e-5);
        }
    }

    #[test]
    fn cos_norm_matches_cos() {
        for x in [0.0f32, 0.25, 0.5, 1.0, 0.123] {
            let want = ((std::f64::consts::FRAC_PI_2) * x as f64).cos() as f32;
            assert!((celt_cos_norm(x) - want).abs() < 1e-6, "x={x}");
        }
    }
}
