//! Encoder-side psychoacoustic analysis for the CELT layer, ported from
//! libopus's `celt_encoder.c` (`tf_analysis`, `spread_decision`,
//! `alloc_trim_analysis`). Each function only *chooses* a value the
//! bitstream already carries; the decoder is unaffected.

use super::bands::{haar1, TF_SELECT_TABLE};
use super::rate::NB_EBANDS;
use super::tables::EBAND5MS;
use super::vq::{SPREAD_AGGRESSIVE, SPREAD_LIGHT, SPREAD_NONE, SPREAD_NORMAL};

/// Per-band importance weight used by the TF search (the reference's default
/// when no dynalloc analysis is available).
const TF_IMPORTANCE: i32 = 13;

fn l1_metric(x: &[f32], lm: i32, bias: f32) -> f32 {
    let l1: f32 = x.iter().map(|v| v.abs()).sum();
    l1 + lm as f32 * bias * l1
}

/// Per-band TF resolution search (`tf_analysis`). `x` holds the normalized
/// spectrum (`n0` bins per channel plane; for stereo the planes are averaged,
/// approximating the mid channel). Fills `tf_res` with the raw per-band 0/1
/// choice for `start..end` and returns the `tf_select` bit.
#[allow(clippy::too_many_arguments)]
pub(crate) fn tf_analysis(
    start: usize,
    end: usize,
    is_transient: bool,
    tf_res: &mut [i32; NB_EBANDS],
    lambda: i32,
    x: &[f32],
    channels: usize,
    n0: usize,
    lm: usize,
    tf_estimate: f32,
) -> usize {
    let bias = 0.04 * (-0.25f32).max(0.5 - tf_estimate);
    let mut metric = [0i32; NB_EBANDS];
    let mut tmp = [0.0f32; 176];
    let mut tmp1 = [0.0f32; 176];
    let transient = is_transient as usize;
    for i in start..end {
        let bw = (EBAND5MS[i + 1] - EBAND5MS[i]) as usize;
        let n = bw << lm;
        let narrow = bw == 1;
        let lo = (EBAND5MS[i] as usize) << lm;
        for j in 0..n {
            tmp[j] = if channels == 2 {
                0.5 * (x[lo + j] + x[n0 + lo + j])
            } else {
                x[lo + j]
            };
        }
        let mut l1 = l1_metric(&tmp[..n], if is_transient { lm as i32 } else { 0 }, bias);
        let mut best_l1 = l1;
        let mut best_level = 0i32;
        if is_transient && !narrow {
            tmp1[..n].copy_from_slice(&tmp[..n]);
            haar1(&mut tmp1[..n], n >> lm, 1 << lm);
            l1 = l1_metric(&tmp1[..n], lm as i32 + 1, bias);
            if l1 < best_l1 {
                best_l1 = l1;
                best_level = -1;
            }
        }
        let steps = lm + usize::from(!(is_transient || narrow));
        for k in 0..steps {
            let b = if is_transient {
                lm as i32 - k as i32 - 1
            } else {
                k as i32 + 1
            };
            haar1(&mut tmp[..n], n >> k, 1 << k);
            l1 = l1_metric(&tmp[..n], b, bias);
            if l1 < best_l1 {
                best_l1 = l1;
                best_level = k as i32 + 1;
            }
        }
        metric[i] = if is_transient {
            2 * best_level
        } else {
            -2 * best_level
        };
        if narrow && (metric[i] == 0 || metric[i] == -2 * lm as i32) {
            metric[i] -= 1;
        }
    }

    let table = |sel: usize, v: usize| 2 * TF_SELECT_TABLE[lm][4 * transient + 2 * sel + v] as i32;
    let trans_pen = if is_transient { 0 } else { lambda };
    let mut sel_cost = [0i32; 2];
    for (sel, slot) in sel_cost.iter_mut().enumerate() {
        let mut cost0 = TF_IMPORTANCE * (metric[start] - table(sel, 0)).abs();
        let mut cost1 = TF_IMPORTANCE * (metric[start] - table(sel, 1)).abs() + trans_pen;
        for &mi in &metric[start + 1..end] {
            let curr0 = cost0.min(cost1 + lambda);
            let curr1 = (cost0 + lambda).min(cost1);
            cost0 = curr0 + TF_IMPORTANCE * (mi - table(sel, 0)).abs();
            cost1 = curr1 + TF_IMPORTANCE * (mi - table(sel, 1)).abs();
        }
        *slot = cost0.min(cost1);
    }
    let tf_select = usize::from(sel_cost[1] < sel_cost[0] && is_transient);

    let mut path0 = [0i32; NB_EBANDS];
    let mut path1 = [0i32; NB_EBANDS];
    let mut cost0 = TF_IMPORTANCE * (metric[start] - table(tf_select, 0)).abs();
    let mut cost1 = TF_IMPORTANCE * (metric[start] - table(tf_select, 1)).abs() + trans_pen;
    for i in start + 1..end {
        let (from0, from1) = (cost0, cost1 + lambda);
        let curr0 = if from0 < from1 {
            path0[i] = 0;
            from0
        } else {
            path0[i] = 1;
            from1
        };
        let (from0, from1) = (cost0 + lambda, cost1);
        let curr1 = if from0 < from1 {
            path1[i] = 0;
            from0
        } else {
            path1[i] = 1;
            from1
        };
        cost0 = curr0 + TF_IMPORTANCE * (metric[i] - table(tf_select, 0)).abs();
        cost1 = curr1 + TF_IMPORTANCE * (metric[i] - table(tf_select, 1)).abs();
    }
    tf_res[end - 1] = i32::from(cost0 >= cost1);
    for i in (start..end - 1).rev() {
        tf_res[i] = if tf_res[i + 1] == 1 {
            path1[i + 1]
        } else {
            path0[i + 1]
        };
    }
    tf_select
}

/// Spread (PVQ rotation) decision from the normalized spectrum's sparsity
/// (`spread_decision`). `average`/`last` carry the recursive-averaging and
/// hysteresis state across frames.
pub(crate) fn spread_decision(
    x: &[f32],
    channels: usize,
    n0: usize,
    end: usize,
    lm: usize,
    average: &mut i32,
    last: &mut i32,
) -> i32 {
    let mut sum = 0i32;
    let mut bands = 0i32;
    for c in 0..channels {
        for i in 0..end {
            let n = ((EBAND5MS[i + 1] - EBAND5MS[i]) as usize) << lm;
            if n <= 8 {
                continue;
            }
            let lo = c * n0 + ((EBAND5MS[i] as usize) << lm);
            let mut tcount = [0i32; 3];
            for &v in &x[lo..lo + n] {
                let x2n = v * v * n as f32;
                tcount[0] += i32::from(x2n < 0.25);
                tcount[1] += i32::from(x2n < 0.0625);
                tcount[2] += i32::from(x2n < 0.015625);
            }
            let n = n as i32;
            let tmp = i32::from(2 * tcount[2] >= n)
                + i32::from(2 * tcount[1] >= n)
                + i32::from(2 * tcount[0] >= n);
            sum += tmp * 256;
            bands += 1;
        }
    }
    if bands == 0 {
        return SPREAD_NORMAL;
    }
    sum /= bands;
    sum = (sum + *average) >> 1;
    *average = sum;
    sum = (3 * sum + (((3 - *last) << 7) + 64) + 2) >> 2;
    let decision = if sum < 80 {
        SPREAD_AGGRESSIVE
    } else if sum < 256 {
        SPREAD_NORMAL
    } else if sum < 384 {
        SPREAD_LIGHT
    } else {
        SPREAD_NONE
    };
    *last = decision;
    decision
}

/// Allocation trim (`alloc_trim_analysis`, 0..=10): biases bits toward low
/// or high bands from the bitrate and the spectral tilt of the band
/// energies. The reference also lowers the trim for strongly correlated
/// stereo; measured here (A/B in `psychoacoustic_ab_report`) that cost up to
/// 3 dB on dual-mono-like content because this encoder has no matching
/// stereo-saving bit reallocation, so the term is omitted. `means` is the per-band log2
/// energy with the e-means tilt removed (as in the encoder);
/// `bits_per_sec` is the equivalent bitrate.
pub(crate) fn alloc_trim(means: &[f32], end: usize, channels: usize, bits_per_sec: i32) -> i32 {
    let mut trim = 5.0f32;
    if bits_per_sec < 64_000 {
        trim = 4.0;
    } else if bits_per_sec < 80_000 {
        trim = 4.0 + ((bits_per_sec - 64_000) >> 10) as f32 / 16.0;
    }
    let mut diff = 0.0f32;
    for c in 0..channels {
        for i in 0..end - 1 {
            diff += means[c * NB_EBANDS + i] * (2 + 2 * i as i32 - end as i32) as f32;
        }
    }
    diff /= (channels * (end - 1)) as f32;
    trim -= (-2.0f32).max(2.0f32.min((diff + 1.0) / 6.0));
    ((0.5 + trim).floor() as i32).clamp(0, 10)
}
