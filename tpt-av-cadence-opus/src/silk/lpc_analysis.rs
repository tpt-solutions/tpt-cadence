//! SILK encoder-side LPC analysis (float, non-normative) and the exact
//! integer `A → NLSF` conversion.
//!
//! The analysis kernels ([`apply_sine_window`], [`autocorrelation`],
//! [`schur`], [`k2a`], [`bwexpander_f32`], [`lpc_analysis_filter`],
//! [`energy`], [`corr_matrix`], [`corr_vector`]) are float ports of the
//! reference's `silk/float/*` encoder — they only choose the parameters
//! the encoder transmits, so they need not be bit-exact; the codebase
//! keeps them recognizable ports anyway. [`a2nlsf`] by contrast is an
//! exact fixed-point port of `silk/A2NLSF.c`: its outputs (NLSF vectors)
//! feed the quantizer whose indices the decoder reconstructs, and the
//! reference's root-finding/bandwidth-expansion behavior is observable
//! through the quantized result.
//!
//! SOURCE: Xiph.Org libopus 1.5.2, `silk/float/apply_sine_window_FLP.c`,
//! `autocorrelation_FLP.c`, `inner_product_FLP.c`, `schur_FLP.c`,
//! `k2a_FLP.c`, `bwexpander_FLP.c`, `LPC_analysis_filter_FLP.c`,
//! `energy_FLP.c`, `corrMatrix_FLP.c`, and `silk/A2NLSF.c`
//! (BSD-3-Clause).
#![allow(dead_code)]

use crate::silk::nlsf::bwexpander_32;
use crate::silk::sigproc::smlaww;
use crate::silk::tables::LSF_COS_TAB_FIX_Q12;

/// `BIN_DIV_STEPS_A2NLSF_FIX` (`silk/A2NLSF.c`): bisection steps per root.
const BIN_DIV_STEPS_A2NLSF: i32 = 3;
/// `MAX_ITERATIONS_A2NLSF_FIX`: bandwidth-expansion retries before
/// falling back to a white spectrum.
const MAX_ITERATIONS_A2NLSF: i32 = 16;
/// `LSF_COS_TAB_SZ_FIX` (`silk/define.h`).
const LSF_COS_TAB_SZ_FIX: i32 = 128;

/// `silk_inner_product_FLP` (`silk/float/inner_product_FLP.c`): double
/// accumulator, float result.
pub(crate) fn inner_product(a: &[f32], b: &[f32]) -> f32 {
    debug_assert_eq!(a.len(), b.len());
    let mut result = 0.0f64;
    for i in 0..a.len() {
        result += a[i] as f64 * b[i] as f64;
    }
    result as f32
}

/// `silk_apply_sine_window_FLP`: win_type 1 = sine ramp 0..π/2, 2 = the
/// mirrored falling half. Length must be a multiple of 4.
pub(crate) fn apply_sine_window(px_win: &mut [f32], px: &[f32], win_type: i32) {
    let length = px_win.len();
    debug_assert_eq!(px.len(), length);
    debug_assert!(win_type == 1 || win_type == 2);
    debug_assert!(length % 4 == 0);

    let freq = core::f32::consts::PI / (length + 1) as f32;

    /* Approximation of 2 * cos(f) */
    let c = 2.0f32 - freq * freq;

    /* Initialize state */
    let (mut s0, mut s1) = if win_type < 2 {
        (0.0f32, freq) // sin(f) ≈ f
    } else {
        (1.0f32, 0.5 * c) // cos(f) ≈ 1 - f²/2 = c/2
    };

    /* sin(n*f) = 2 * cos(f) * sin((n-1)*f) - sin((n-2)*f), 4 at a time */
    let mut k = 0;
    while k < length {
        px_win[k] = px[k] * 0.5 * (s0 + s1);
        px_win[k + 1] = px[k + 1] * s1;
        s0 = c * s1 - s0;
        px_win[k + 2] = px[k + 2] * 0.5 * (s1 + s0);
        px_win[k + 3] = px[k + 3] * s0;
        s1 = c * s0 - s1;
        k += 4;
    }
}

/// `silk_autocorrelation_FLP`: `results[i] = <x, x+i>`; `results.len()`
/// selects the correlation count (clamped to the input length).
pub(crate) fn autocorrelation(results: &mut [f32], input: &[f32]) {
    let mut count = results.len().min(input.len());
    if count == 0 {
        return;
    }
    // The reference clamps `correlationCount` down to `inputDataSize` and
    // computes each lag with the double-accumulator inner product.
    while count > input.len() {
        count -= 1;
    }
    for (i, out) in results.iter_mut().enumerate().take(count) {
        *out = inner_product(&input[..input.len() - i], &input[i..]);
    }
}

/// `silk_schur_FLP`: Levinson-Durbin via the Schur recursion (double
/// intermediate precision). Returns the residual energy.
pub(crate) fn schur(refl_coef: &mut [f32], auto_corr: &[f32], order: usize) -> f32 {
    debug_assert!(order >= 1 && auto_corr.len() > order);
    let mut c = [[0.0f64; 2]; 17]; // SILK_MAX_ORDER_LPC + 1 = 24, but 17 suffices for order <= 16
    for (k, slot) in c.iter_mut().enumerate().take(order + 1) {
        slot[0] = auto_corr[k] as f64;
        slot[1] = auto_corr[k] as f64;
    }

    for k in 0..order {
        /* Get reflection coefficient */
        let rc_tmp = -c[k + 1][0] / c[0][1].max(1e-9);
        refl_coef[k] = rc_tmp as f32;

        /* Update correlations */
        for n in 0..order - k {
            let ctmp1 = c[n + k + 1][0];
            let ctmp2 = c[n][1];
            c[n + k + 1][0] = ctmp1 + ctmp2 * rc_tmp;
            c[n][1] = ctmp2 + ctmp1 * rc_tmp;
        }
    }
    c[0][1] as f32
}

/// `silk_k2a_FLP`: reflection coefficients → prediction coefficients.
pub(crate) fn k2a(a: &mut [f32], rc: &[f32], order: usize) {
    debug_assert!(order >= 1 && a.len() >= order && rc.len() >= order);
    for k in 0..order {
        let rck = rc[k];
        for n in 0..(k + 1) >> 1 {
            let tmp1 = a[n];
            let tmp2 = a[k - n - 1];
            a[n] = tmp1 + tmp2 * rck;
            a[k - n - 1] = tmp2 + tmp1 * rck;
        }
        a[k] = -rck;
    }
}

/// `silk_bwexpander_FLP`: chirp (bandwidth expansion) of an AR vector.
pub(crate) fn bwexpander_f32(ar: &mut [f32], chirp: f32) {
    let d = ar.len();
    debug_assert!(d > 0);
    let mut cfac = chirp;
    for v in ar.iter_mut().take(d - 1) {
        *v *= cfac;
        cfac *= chirp;
    }
    ar[d - 1] *= cfac;
}

/// `silk_energy_FLP`: sum of squares with a double accumulator.
pub(crate) fn energy(data: &[f32]) -> f64 {
    let mut result = 0.0f64;
    for &v in data {
        result += v as f64 * v as f64;
    }
    result
}

/// `silk_LPC_analysis_filter_FLP`: zero-state prediction-error filter;
/// the first `order` outputs are zeroed.
pub(crate) fn lpc_analysis_filter(r_lpc: &mut [f32], pred_coef: &[f32], s: &[f32], order: usize) {
    let length = r_lpc.len();
    debug_assert_eq!(s.len(), length);
    debug_assert!((6..=16).contains(&order) && order <= length);
    for ix in order..length {
        let mut lpc_pred = 0.0f64;
        for j in 0..order {
            lpc_pred += s[ix - 1 - j] as f64 * pred_coef[j] as f64;
        }
        r_lpc[ix] = (s[ix] as f64 - lpc_pred) as f32;
    }
    r_lpc[..order].fill(0.0);
}

/// `silk_corrMatrix_FLP`: X'*X for the data matrix whose column `lag` is
/// `x` delayed by `lag`; row-major `order x order` output.
pub(crate) fn corr_matrix(xx: &mut [f32], x: &[f32], l: usize, order: usize) {
    debug_assert!(x.len() >= l + order - 1);
    debug_assert!(xx.len() >= order * order);
    /* Diagonal: energies of the individual columns. Column j's samples
     * are x[order - 1 - j .. order - 1 - j + L]; start from the energy of
     * the full vector and slide samples in and out like the reference. */
    let mut energy = energy(x);
    for &v in &x[..order - 1] {
        energy -= v as f64 * v as f64;
    }
    xx[0] = energy as f32;
    for j in 1..order {
        energy -= x[order - 1 + l - j] as f64 * x[order - 1 + l - j] as f64;
        energy += x[order - 1 - j] as f64 * x[order - 1 - j] as f64;
        xx[j * order + j] = energy as f32;
    }
    /* Off-diagonals: inner product of column 0 and column `lag`, then the
     * remaining pairs built up from that, mirroring the reference's
     * pointer walk (ptr1 = column 0, ptr2 = column lag). */
    for lag in 1..order {
        let ptr1 = |i: isize| x[(order as isize - 1 + i) as usize];
        let ptr2 = |i: isize| x[(order as isize - 1 - lag as isize + i) as usize];
        let mut e = inner_product(
            &x[order - 1..order - 1 + l],
            &x[order - 1 - lag..order - 1 - lag + l],
        );
        xx[lag * order] = e;
        xx[lag] = e;
        for j in 1..order - lag {
            e -= ptr1(l as isize - j as isize) * ptr2(l as isize - j as isize);
            e += ptr1(-(j as isize)) * ptr2(-(j as isize));
            xx[(lag + j) * order + j] = e;
            xx[j * order + lag + j] = e;
        }
    }
}

/// `silk_corrVector_FLP`: X'*t for each column of the data matrix.
pub(crate) fn corr_vector(xt: &mut [f32], x: &[f32], t: &[f32], l: usize, order: usize) {
    debug_assert!(x.len() >= l + order - 1);
    debug_assert!(xt.len() >= order);
    for (lag, out) in xt.iter_mut().enumerate().take(order) {
        *out = inner_product(&x[order - 1 - lag..order - 1 - lag + l], t);
    }
}

/// `silk_A2NLSF` (`silk/A2NLSF.c`): converts monic Q16 whitening-filter
/// coefficients to NLSFs (Q15). If not all roots are found, `a_q16` is
/// bandwidth-expanded (in place, like the reference) and the search
/// reruns; after too many retries the white-spectrum NLSF vector is
/// emitted.
pub(crate) fn a2nlsf(nlsf_q15: &mut [i16], a_q16: &mut [i32]) {
    let d = nlsf_q15.len();
    debug_assert!(d % 2 == 0 && (2..=16).contains(&d));
    let dd = d >> 1;

    let mut p = [0i32; 9]; // SILK_MAX_ORDER_LPC / 2 + 1
    let mut q = [0i32; 9];
    a2nlsf_init(a_q16, &mut p, &mut q, dd);

    let mut root_ix;
    let mut p_sel = 0usize; // 0 = P, 1 = Q
    let mut xlo = LSF_COS_TAB_FIX_Q12[0] as i32;
    let mut ylo = eval_poly(&p, xlo, dd);

    if ylo < 0 {
        /* Set the first NLSF to zero and move on to the next */
        nlsf_q15[0] = 0;
        p_sel = 1;
        ylo = eval_poly(&q, xlo, dd);
        root_ix = 1;
    } else {
        root_ix = 0;
    }
    let mut k = 1i32;
    let mut bw_expansions = 0i32;
    let mut thr = 0i32;
    loop {
        /* Evaluate polynomial */
        let mut xhi = LSF_COS_TAB_FIX_Q12[k as usize] as i32;
        let mut yhi = if p_sel == 0 {
            eval_poly(&p, xhi, dd)
        } else {
            eval_poly(&q, xhi, dd)
        };

        /* Detect zero crossing */
        if (ylo <= 0 && yhi >= thr) || (ylo >= 0 && yhi <= -thr) {
            if yhi == 0 {
                /* If the root lies exactly at the end of the current
                 * interval, look for the next root in the next interval */
                thr = 1;
            } else {
                thr = 0;
            }
            /* Binary division */
            let mut ffrac = -256i32;
            for m in 0..BIN_DIV_STEPS_A2NLSF {
                /* Evaluate polynomial; xmid = RSHIFT_ROUND(xlo + xhi, 1) */
                let sum = xlo.wrapping_add(xhi);
                let xmid = (sum >> 1) + (sum & 1);
                let ymid = if p_sel == 0 {
                    eval_poly(&p, xmid, dd)
                } else {
                    eval_poly(&q, xmid, dd)
                };

                /* Detect zero crossing */
                if (ylo <= 0 && ymid >= 0) || (ylo >= 0 && ymid <= 0) {
                    /* Reduce frequency */
                    xhi = xmid;
                    yhi = ymid;
                } else {
                    /* Increase frequency */
                    xlo = xmid;
                    ylo = ymid;
                    /* ffrac = silk_ADD_RSHIFT(ffrac, 128, m) */
                    ffrac = ffrac.wrapping_add(128 >> m);
                }
            }

            /* Interpolate */
            if ylo.abs() < 65536 {
                /* Avoid dividing by zero */
                let den = ylo - yhi;
                let nom = (ylo << (8 - BIN_DIV_STEPS_A2NLSF)) + (den >> 1);
                if den != 0 {
                    ffrac += nom / den;
                }
            } else {
                ffrac += ylo / ((ylo - yhi) >> (8 - BIN_DIV_STEPS_A2NLSF));
            }
            nlsf_q15[root_ix] = k.wrapping_shl(8).wrapping_add(ffrac).min(i16::MAX as i32) as i16;
            debug_assert!(nlsf_q15[root_ix] >= 0);

            root_ix += 1; /* Next root */
            if root_ix >= d {
                /* Found all roots */
                break;
            }
            /* Alternate pointer to polynomial */
            p_sel = root_ix & 1;

            /* Evaluate polynomial */
            xlo = LSF_COS_TAB_FIX_Q12[(k - 1) as usize] as i32;
            ylo = (1 - (root_ix & 2) as i32) << 12;
        } else {
            /* Increment loop counter */
            k += 1;
            xlo = xhi;
            ylo = yhi;
            thr = 0;

            if k > LSF_COS_TAB_SZ_FIX {
                bw_expansions += 1;
                if bw_expansions > MAX_ITERATIONS_A2NLSF {
                    /* Set NLSFs to white spectrum and exit */
                    nlsf_q15[0] = ((1 << 15) / (d as i32 + 1)) as i16;
                    for kk in 1..d {
                        nlsf_q15[kk] = nlsf_q15[kk - 1] + nlsf_q15[0];
                    }
                    return;
                }

                /* Error: Apply progressively more bandwidth expansion and
                 * run again */
                bwexpander_32(a_q16, 65536 - (1 << bw_expansions));

                a2nlsf_init(a_q16, &mut p, &mut q, dd);
                p_sel = 0;
                xlo = LSF_COS_TAB_FIX_Q12[0] as i32;
                ylo = eval_poly(&p, xlo, dd);
                if ylo < 0 {
                    nlsf_q15[0] = 0;
                    p_sel = 1;
                    ylo = eval_poly(&q, xlo, dd);
                    root_ix = 1;
                } else {
                    root_ix = 0;
                }
                k = 1;
            }
        }
    }
}

/// `silk_A2NLSF_trans_poly`: cos(n·f) → cos(f)^n coefficient transform.
fn a2nlsf_trans_poly(p: &mut [i32], dd: usize) {
    for k in 2..=dd {
        let mut n = dd;
        while n > k {
            p[n - 2] -= p[n];
            n -= 1;
        }
        p[k - 2] -= p[k].wrapping_shl(1);
    }
}

/// `silk_A2NLSF_eval_poly`: polynomial evaluation in Q16 at a Q12 point.
fn eval_poly(p: &[i32], x: i32, dd: usize) -> i32 {
    let mut y32 = p[dd];
    let x_q16 = (x as u32).wrapping_shl(4) as i32;
    for n in (0..dd).rev() {
        y32 = smlaww(p[n], y32, x_q16);
    }
    y32
}

/// `silk_A2NLSF_init`: split the filter into even/odd polynomials, divide
/// out the forced roots, and transform to cos(f)^n form.
fn a2nlsf_init(a_q16: &[i32], p: &mut [i32], q: &mut [i32], dd: usize) {
    p[dd] = 1 << 16;
    q[dd] = 1 << 16;
    for k in 0..dd {
        p[k] = -a_q16[dd - k - 1] - a_q16[dd + k];
        q[k] = -a_q16[dd - k - 1] + a_q16[dd + k];
    }
    /* Divide out z = 1 (Q) and z = -1 (P) zeros */
    for k in (1..=dd).rev() {
        p[k - 1] = p[k - 1].wrapping_sub(p[k]);
        q[k - 1] = q[k - 1].wrapping_add(q[k]);
    }
    a2nlsf_trans_poly(p, dd);
    a2nlsf_trans_poly(q, dd);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sine_window_endpoints_and_recurrence() {
        let n = 16;
        // Unit-amplitude input so the windowed peak is the window peak.
        let x = vec![1.0f32; n];
        let mut w = vec![0f32; n];
        apply_sine_window(&mut w, &x, 1);
        // Direct sine evaluation: sin((k+0.5)·π/(n+1))-shaped recurrence.
        // The recurrence approximates sin((k+1)·f); endpoint behavior:
        // first sample ~0, energy concentrated mid-window.
        // First coefficient: 0.5·(S0+S1) with S0 = 0, S1 = f — i.e. the
        // sin((k+0.5)·f) window evaluated at k = 0, ≈ f/2.
        let f = core::f32::consts::PI / (n + 1) as f32;
        assert!((w[0] - x[0] * f * 0.5).abs() < 1e-5, "w[0] = {}", w[0]);
        let peak = w.iter().cloned().fold(0.0f32, f32::max);
        assert!(peak > 0.9 && peak <= 1.0 + 1e-3, "peak {peak}");

        let mut w2 = vec![0f32; n];
        apply_sine_window(&mut w2, &x, 2);
        // Type 2 starts at the peak: 0.5·(S0+S1) with S0 = 1,
        // S1 = c/2 ≈ cos(f), i.e. cos²(f/2).
        let expect = 0.5 * (1.0 + 0.5 * (2.0 - f * f));
        assert!((w2[0] - expect).abs() < 1e-5, "w2[0] = {}", w2[0]);
    }

    #[test]
    fn schur_k2a_round_trip_on_known_vector() {
        // A stable 2nd-order filter: reflection coefficients give a
        // prediction filter whose residual energy decreases.
        let auto = [100.0f32, 50.0, 25.0, 12.5];
        let mut rc = [0f32; 3];
        let nrg = schur(&mut rc, &auto, 3);
        assert!(nrg > 0.0 && nrg < auto[0], "residual {nrg} vs input energy");
        let mut a = [0f32; 3];
        k2a(&mut a, &rc, 3);
        assert!(a.iter().all(|v| v.is_finite()));
    }

    #[test]
    fn autocorrelation_lags_match_inner_products() {
        let x: Vec<f32> = (0..64).map(|i| ((i as f32) * 0.37).sin()).collect();
        let mut r = [0f32; 5];
        autocorrelation(&mut r, &x);
        for lag in 0..5 {
            let expect = inner_product(&x[..x.len() - lag], &x[lag..]);
            assert!((r[lag] - expect).abs() < 1e-3);
        }
    }

    /// NLSFs of a white-spectrum-ish filter must be strictly increasing,
    /// inside (0, 32768), and round the trip through the decoder's
    /// `nlsf2a` to a stable filter — this pins the A2NLSF root walk.
    #[test]
    fn a2nlsf_produces_monotonic_nlsfs() {
        use crate::silk::nlsf::{lpc_inverse_pred_gain, nlsf2a};
        // A mild low-pass prediction filter (dominant a1).
        let a_q12: [i16; 10] = [-4000, -2000, -1000, -500, -250, -125, -60, -30, -15, -8];
        // NLSF2A takes Q12 coefficients; A2NLSF wants Q16 "monic" ones.
        let mut a_q16: Vec<i32> = a_q12.iter().map(|&v| (v as i32) << 4).collect();
        let mut nlsf = [0i16; 10];
        a2nlsf(&mut nlsf, &mut a_q16);
        for w in nlsf.windows(2) {
            assert!(w[0] < w[1], "NLSFs must increase: {:?}", nlsf);
        }
        assert!(nlsf[0] > 0 && (nlsf[9] as i32) < 32768);
        // Round trip through the decoder's converter yields a stable
        // filter (positive inverse prediction gain).
        let mut a_back = [0i16; 10];
        nlsf2a(&mut a_back, &nlsf, 10);
        assert!(lpc_inverse_pred_gain(&a_back, 10) > 0);
    }
}
