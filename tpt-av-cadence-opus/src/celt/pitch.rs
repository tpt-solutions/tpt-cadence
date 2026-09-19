//! Pitch postfilter and PLC pitch search (`celt/pitch.c`).
//!
//! `comb_filter` applies the decoder's pitch postfilter in place; the
//! downsample/xcorr/search trio backs the packet-loss-concealment pitch
//! estimate.
//!
//! SOURCE: Xiph.Org libopus 1.5.2, `celt/pitch.c` + `celt.c`
//! (`comb_filter_const_c`) (BSD-3-Clause). Float build: all Q-macros and
//! shifts collapse to plain `f32` operations.

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

use super::celt_lpc::{_celt_autocorr, _celt_lpc};

/// `COMBFILTER_MINPERIOD` (celt.h).
pub(crate) const COMBFILTER_MINPERIOD: usize = 15;
/// `COMBFILTER_MAXPERIOD` (celt.h).
#[allow(dead_code)]
pub(crate) const COMBFILTER_MAXPERIOD: usize = 1024;
/// Maximum pitch lag allowed in the PLC pitch search (66.67 Hz).
pub(crate) const PLC_PITCH_LAG_MAX: usize = 720;
/// Minimum pitch lag allowed in the PLC pitch search (480 Hz).
pub(crate) const PLC_PITCH_LAG_MIN: usize = 100;

/// Tap gains per tapset: {g0, g1, g2}.
static COMB_GAINS: [[f32; 3]; 3] = [
    [0.306_640_625_0, 0.217_041_015_6, 0.129_638_671_9],
    [0.463_867_187_5, 0.268_066_406_2, 0.0],
    [0.799_804_687_5, 0.100_097_656_2, 0.0],
];

/// `comb_filter_const`: the constant-coefficient section. `buf` is indexed
/// absolutely; the source starts at `x_off` so the look-back taps
/// (`x[i-T+2]` etc.) stay inside the channel's history buffer.
fn comb_filter_const(
    buf: &mut [f32],
    x_off: usize,
    y_off: usize,
    t: usize,
    n: usize,
    g10: f32,
    g11: f32,
    g12: f32,
) {
    // The reference dispatches `comb_filter_const_sse` on x86: 4-wide
    // blocks whose per-sample arithmetic groups the terms as
    // `(x + g10*x[-T]) + (g11*(x[-T+1]+x[-T-1]) + g12*(x[-T+2]+x[-T-2]))`
    // (the g11 and g12 contributions are summed *before* being added to
    // the partial result — different rounding from the scalar C). With
    // `CUSTOM_MODES` off the kernel has no tail loop, so the last `n % 4`
    // samples are left unwritten: in-place callers keep their previous
    // content there.
    let n4 = n / 4 * 4;
    for i in 0..n4 {
        let x = buf[x_off + i];
        let x_m1 = buf[x_off + i - t - 1];
        let x_0 = buf[x_off + i - t];
        let x_p1 = buf[x_off + i - t + 1];
        let x_m2 = buf[x_off + i - t - 2];
        let x_p2 = buf[x_off + i - t + 2];
        let t1 = g11 * (x_p1 + x_m1);
        let t2 = g12 * (x_p2 + x_m2);
        buf[y_off + i] = (x + g10 * x_0) + (t1 + t2);
    }
}

/// `comb_filter`: pitch postfilter with cross-faded parameters.
///
/// Reads the source at `x_off` (absolute index in `buf`) and writes `n`
/// samples at `y_off`, looking back up to `T0/T1` samples (both callers
/// keep the look-back inside `buf`).
#[allow(clippy::too_many_arguments)]
pub(crate) fn comb_filter(
    buf: &mut [f32],
    x_off: usize,
    y_off: usize,
    t0: usize,
    t1: usize,
    n: usize,
    g0: f32,
    g1: f32,
    tapset0: usize,
    tapset1: usize,
    window: Option<&[f32]>,
    overlap: usize,
) {
    // When both gains are zero, T0/T1 are zero too; the C copy x->y is
    // implicit in our in-place usage.
    if g0 == 0.0 && g1 == 0.0 {
        return;
    }
    let t0 = t0.max(COMBFILTER_MINPERIOD);
    let t1 = t1.max(COMBFILTER_MINPERIOD);
    let g00 = g0 * COMB_GAINS[tapset0][0];
    let g01 = g0 * COMB_GAINS[tapset0][1];
    let g02 = g0 * COMB_GAINS[tapset0][2];
    let g10 = g1 * COMB_GAINS[tapset1][0];
    let g11 = g1 * COMB_GAINS[tapset1][1];
    let g12 = g1 * COMB_GAINS[tapset1][2];
    let mut x1 = buf[x_off - t1 + 1];
    let mut x2 = buf[x_off - t1];
    let mut x3 = buf[x_off - t1 - 1];
    let mut x4 = buf[x_off - t1 - 2];
    // If the filter didn't change, we don't need the overlap.
    let overlap = if g0 == g1 && t0 == t1 && tapset0 == tapset1 {
        0
    } else {
        overlap
    };
    let window = window.unwrap_or(&[]);
    for i in 0..overlap {
        let x0 = buf[x_off + i - t1 + 2];
        let f = window[i] * window[i];
        buf[y_off + i] = buf[x_off + i]
            + (1.0 - f) * g00 * buf[x_off + i - t0]
            + (1.0 - f) * g01 * (buf[x_off + i - t0 + 1] + buf[x_off + i - t0 - 1])
            + (1.0 - f) * g02 * (buf[x_off + i - t0 + 2] + buf[x_off + i - t0 - 2])
            + f * g10 * x2
            + f * g11 * (x1 + x3)
            + f * g12 * (x0 + x4);
        x4 = x3;
        x3 = x2;
        x2 = x1;
        x1 = x0;
    }
    if g1 == 0.0 {
        // The remaining samples pass through unchanged (in-place caller).
        return;
    }
    // Compute the part with the constant filter.
    comb_filter_const(
        buf,
        x_off + overlap,
        y_off + overlap,
        t1,
        n - overlap,
        g10,
        g11,
        g12,
    );
}

/// Two-buffer `comb_filter` (the reference passes `window = NULL,
/// overlap = 0` at the only call site, `prefilter_and_fold`).
///
/// Reads the source at `src_off` in `src` and writes `n` samples to `dst`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn comb_filter_ext(
    src: &[f32],
    src_off: usize,
    dst: &mut [f32],
    dst_off: usize,
    t0: usize,
    t1: usize,
    n: usize,
    g0: f32,
    g1: f32,
    _tapset0: usize,
    tapset1: usize,
) {
    if g0 == 0.0 && g1 == 0.0 {
        // The C caller would OPUS_MOVE x into y; here the destination is
        // scratch the caller initializes, so leave it alone (matches the
        // reference's behavior only in the g!=0 paths below — this branch
        // is unreachable for the prefilter since the gains are negated
        // postfilter values that are zero together with the in-place
        // filter's).
        for i in 0..n {
            dst[dst_off + i] = src[src_off + i];
        }
        return;
    }
    let _t0 = t0.max(COMBFILTER_MINPERIOD);
    let t1 = t1.max(COMBFILTER_MINPERIOD);
    let g10 = g1 * COMB_GAINS[tapset1][0];
    let g11 = g1 * COMB_GAINS[tapset1][1];
    let g12 = g1 * COMB_GAINS[tapset1][2];
    // window == NULL, overlap == 0: the overlap section is skipped.
    if g1 == 0.0 {
        // C: OPUS_MOVE(y+overlap, x+overlap, N-overlap).
        for i in 0..n {
            dst[dst_off + i] = src[src_off + i];
        }
        return;
    }
    // Straight to the constant section (see `comb_filter_const` for the
    // SSE block arithmetic and the unwritten `n % 4` tail).
    for i in 0..n / 4 * 4 {
        let x = src[src_off + i];
        let s11 = g11 * (src[src_off + i - t1 + 1] + src[src_off + i - t1 - 1]);
        let s12 = g12 * (src[src_off + i - t1 + 2] + src[src_off + i - t1 - 2]);
        dst[dst_off + i] = (x + g10 * src[src_off + i - t1]) + (s11 + s12);
    }
}

/// `celt_pitch_xcorr_c` (float): forward correlations.
pub(crate) fn celt_pitch_xcorr(
    x: &[f32],
    y: &[f32],
    xcorr: &mut [f32],
    len: usize,
    max_pitch: usize,
) {
    for i in 0..max_pitch {
        let mut sum = 0f32;
        for j in 0..len {
            sum += x[j] * y[i + j];
        }
        xcorr[i] = sum;
    }
}

/// `celt_inner_prod` (float).
pub(crate) fn celt_inner_prod(x: &[f32], y: &[f32]) -> f32 {
    let mut sum = 0f32;
    for (a, b) in x.iter().zip(y.iter()) {
        sum += a * b;
    }
    sum
}

/// `celt_inner_prod_sse`: the runtime-dispatched SSE4.1 kernel the
/// reference uses on x86 — 4 strided accumulators over 4-sample blocks,
/// horizontal-added as `(s0+s2)+(s1+s3)`, with a scalar MAC tail. Used
/// where its rounding order is observable (e.g. `renormalise_vector`).
pub(crate) fn celt_inner_prod_sse_order(x: &[f32], y: &[f32]) -> f32 {
    let n = x.len();
    let mut s = [0f32; 4];
    let mut i = 0;
    while i + 4 <= n {
        for lane in 0..4 {
            s[lane] += x[i + lane] * y[i + lane];
        }
        i += 4;
    }
    let mut xy = (s[0] + s[2]) + (s[1] + s[3]);
    while i < n {
        xy += x[i] * y[i];
        i += 1;
    }
    xy
}

/// `find_best_pitch` (float build: the fixed-point shifts are identity).
fn find_best_pitch(
    xcorr: &[f32],
    y: &[f32],
    len: usize,
    max_pitch: usize,
    best_pitch: &mut [i32; 2],
) {
    let mut syy = 1f32;
    let mut best_num = [-1f32, -1f32];
    let mut best_den = [0f32, 0f32];
    *best_pitch = [0, 1];
    for j in 0..len {
        syy += y[j] * y[j];
    }
    for i in 0..max_pitch {
        if xcorr[i] > 0.0 {
            let mut xcorr16 = xcorr[i];
            // Considering the range of xcorr16, this should avoid both
            // underflows and overflows (inf) when squaring xcorr16.
            xcorr16 *= 1e-12;
            let num = xcorr16 * xcorr16;
            if num * best_den[1] > best_num[1] * syy {
                if num * best_den[0] > best_num[0] * syy {
                    best_num[1] = best_num[0];
                    best_den[1] = best_den[0];
                    best_pitch[1] = best_pitch[0];
                    best_num[0] = num;
                    best_den[0] = syy;
                    best_pitch[0] = i as i32;
                } else {
                    best_num[1] = num;
                    best_den[1] = syy;
                    best_pitch[1] = i as i32;
                }
            }
        }
        syy += y[i + len] * y[i + len] - y[i] * y[i];
        syy = syy.max(1.0);
    }
}

/// `celt_fir5`: 5-tap FIR used by the downsampler's whitening filter.
fn celt_fir5(x: &mut [f32], num: &[f32; 5], n: usize) {
    let (num0, num1, num2, num3, num4) = (num[0], num[1], num[2], num[3], num[4]);
    let (mut mem0, mut mem1, mut mem2, mut mem3, mut mem4) = (0f32, 0f32, 0f32, 0f32, 0f32);
    for xi in x.iter_mut().take(n) {
        let xv = *xi;
        let sum = xv + num0 * mem0 + num1 * mem1 + num2 * mem2 + num3 * mem3 + num4 * mem4;
        mem4 = mem3;
        mem3 = mem2;
        mem2 = mem1;
        mem1 = mem0;
        mem0 = xv;
        *xi = sum;
    }
}

/// `pitch_downsample`: 2x decimation with a whitening pre-filter.
///
/// `x_lp` must hold `len >> 1` samples; `len` is the input length per
/// channel (`x0`, and optionally `x1` for stereo).
pub(crate) fn pitch_downsample(x0: &[f32], x1: Option<&[f32]>, x_lp: &mut [f32], len: usize) {
    let mut ac = [0f32; 5];
    let mut lpc = [0f32; 4];
    let mut lpc2 = [0f32; 5];
    let c1 = 0.8f32;

    for i in 1..len >> 1 {
        x_lp[i] = 0.25 * x0[2 * i - 1] + 0.25 * x0[2 * i + 1] + 0.5 * x0[2 * i];
    }
    x_lp[0] = 0.25 * x0[1] + 0.5 * x0[0];
    if let Some(x1) = x1 {
        for i in 1..len >> 1 {
            x_lp[i] += 0.25 * x1[2 * i - 1] + 0.25 * x1[2 * i + 1] + 0.5 * x1[2 * i];
        }
        x_lp[0] += 0.25 * x1[1] + 0.5 * x1[0];
    }

    _celt_autocorr(&x_lp[..len >> 1], &mut ac, None, 0, 4, len >> 1);

    // Noise floor -40 dB.
    ac[0] *= 1.0001;
    // Lag windowing.
    for i in 1..=4usize {
        ac[i] -= ac[i] * (0.008 * i as f32) * (0.008 * i as f32);
    }

    _celt_lpc(&mut lpc, &ac, 4);
    let mut tmp = 1f32;
    for i in 0..4 {
        tmp *= 0.9;
        lpc[i] *= tmp;
    }
    // Add a zero.
    lpc2[0] = lpc[0] + 0.8;
    lpc2[1] = lpc[1] + c1 * lpc[0];
    lpc2[2] = lpc[2] + c1 * lpc[1];
    lpc2[3] = lpc[3] + c1 * lpc[2];
    lpc2[4] = c1 * lpc[3];
    celt_fir5(x_lp, &lpc2, len >> 1);
}

/// `pitch_search`: coarse 4x-decimated search refined at 2x.
///
/// Scratch: `x_lp4` holds `len >> 2` samples, `y_lp4` holds
/// `(len + max_pitch) >> 2`, `xcorr` holds `max_pitch >> 1`.
pub(crate) fn pitch_search(
    x_lp: &[f32],
    y: &[f32],
    len: usize,
    max_pitch: usize,
    x_lp4: &mut [f32],
    y_lp4: &mut [f32],
    xcorr: &mut [f32],
) -> i32 {
    let lag = len + max_pitch;

    // Downsample by 2 again.
    for j in 0..len >> 2 {
        x_lp4[j] = x_lp[2 * j];
    }
    for j in 0..lag >> 2 {
        y_lp4[j] = y[2 * j];
    }

    // Coarse search with 4x decimation.
    celt_pitch_xcorr(x_lp4, y_lp4, xcorr, len >> 2, max_pitch >> 2);

    let mut best_pitch = [0i32; 2];
    find_best_pitch(xcorr, y_lp4, len >> 2, max_pitch >> 2, &mut best_pitch);

    // Finer search with 2x decimation.
    for i in 0..max_pitch >> 1 {
        xcorr[i] = 0.0;
        if (i as i32 - 2 * best_pitch[0]).abs() > 2 && (i as i32 - 2 * best_pitch[1]).abs() > 2 {
            continue;
        }
        let sum = celt_inner_prod(&x_lp[..len >> 1], &y[i..i + (len >> 1)]);
        xcorr[i] = sum.max(-1.0);
    }
    find_best_pitch(xcorr, y, len >> 1, max_pitch >> 1, &mut best_pitch);

    // Refine by pseudo-interpolation.
    let offset = if best_pitch[0] > 0 && (best_pitch[0] as usize) < (max_pitch >> 1) - 1 {
        let bp = best_pitch[0] as usize;
        let a = xcorr[bp - 1];
        let b = xcorr[bp];
        let c = xcorr[bp + 1];
        if c - a > 0.7 * (b - a) {
            1
        } else if a - c > 0.7 * (b - c) {
            -1
        } else {
            0
        }
    } else {
        0
    };
    2 * best_pitch[0] - offset
}

#[cfg(test)]
mod tests {
    use super::*;

    /// comb_filter with zero gains is a no-op.
    #[test]
    fn comb_filter_zero_gains_noop() {
        let mut buf = vec![0f32; 64];
        for (i, v) in buf.iter_mut().enumerate() {
            *v = (i as f32 * 0.1).sin();
        }
        let orig = buf.clone();
        comb_filter(&mut buf, 32, 32, 0, 0, 16, 0.0, 0.0, 0, 0, None, 0);
        assert_eq!(buf, orig);
    }

    /// With constant gain/period/tapset, the filter reduces to
    /// y[i] = x[i] + g10*x[i-T] + g11*(x[i-T+1]+x[i-T-1]) + g12*(...).
    #[test]
    fn comb_filter_constant_section_matches_formula() {
        let mut buf = vec![0f32; 256];
        for (i, v) in buf.iter_mut().enumerate() {
            *v = ((i as f32) * 0.37).sin();
        }
        let t = 40usize;
        let g = 0.3f32;
        let n = 32usize;
        // tapset 1 has gains {0.4638671875, 0.2680664062, 0}: taps 0 and ±1.
        comb_filter(&mut buf, 128, 128, t, t, n, g, g, 1, 1, None, 0);
        // Rebuild the expectation from the original signal.
        for i in 0..n {
            let at = |off: usize| ((off as f32) * 0.37).sin();
            let x_i = at(128 + i);
            let want = x_i
                + g * 0.463_867_187_5 * at(128 + i - t)
                + g * 0.268_066_406_2 * (at(128 + i - t + 1) + at(128 + i - t - 1));
            assert!(
                (buf[128 + i] - want).abs() < 1e-6,
                "i={i} got {} want {want}",
                buf[128 + i]
            );
        }
    }

    /// The PLC pitch search finds the period of a periodic test signal.
    #[test]
    fn pitch_search_finds_period() {
        // 2048-sample sawtooth-like buffer, exactly periodic with period
        // 212 (even, so the 2x-decimated signal is periodic too). Rich
        // harmonics keep the whitening pre-filter from notching the
        // excitation away.
        let period = 212usize;
        let len = 2048usize;
        let mut buf = vec![0f32; len];
        for (i, v) in buf.iter_mut().enumerate() {
            let t = i as f32;
            let mut s = 0f32;
            for h in 1..=12usize {
                s += (std::f32::consts::TAU * h as f32 * t / period as f32).sin() / h as f32;
            }
            *v = s;
        }
        let mut lp = vec![0f32; len >> 1];
        pitch_downsample(&buf, None, &mut lp, len);
        let plc_max = PLC_PITCH_LAG_MAX;
        let plc_min = PLC_PITCH_LAG_MIN;
        let search_len = len - plc_max;
        let max_pitch = plc_max - plc_min;
        let mut x_lp4 = vec![0f32; search_len >> 2];
        let mut y_lp4 = vec![0f32; (search_len + max_pitch) >> 2];
        let mut xcorr = vec![0f32; max_pitch >> 1];
        let pitch = pitch_search(
            &lp[plc_max >> 1..],
            &lp,
            search_len,
            max_pitch,
            &mut x_lp4,
            &mut y_lp4,
            &mut xcorr,
        );
        let found = (plc_max as i32 - pitch) as usize;
        // The search may lock onto any exact period multiple (the metric
        // normalizes per-candidate energy, so stationary signals tie);
        // any of 1x..3x validates the pipeline.
        assert!(
            found % period == 0 && found / period <= 3,
            "found pitch {found}, expected a multiple of {period}"
        );
    }
}
