//! Perceptually weighted quality metrics for the codec test suites.
//!
//! [`a_weighted_snr_db`] applies the IEC 61672 A-weighting response to
//! the ERROR spectrum before measuring SNR: quantization noise
//! concentrated where hearing is most sensitive (1-6 kHz) penalizes the
//! score, while noise in less audible regions does not. This is the
//! complement of what the SILK noise shaper deliberately does — making
//! the metric sensitive to shaping *mistakes* without rewarding it — and
//! a suitable regression floor alongside waveform SNR.
//! [`segmental_snr_db`] is the standard speech-codec segmental SNR
//! (per-frame SNR clamped to [-10, +35] dB, averaged), reporting
//! per-frame quality instead of letting loud frames dominate.

/// A-weighting power response at `freq` Hz (IEC 61672:2013 analog
/// prototype, normalized to 1.0 at 1 kHz).
fn a_weight_power(freq: f64) -> f64 {
    let f2 = freq * freq;
    let num = (12194.0f64).powi(2) * f2 * f2;
    let d1 = f2 + 20.6f64.powi(2);
    let d2 = (f2 + 107.7f64.powi(2)).sqrt();
    let d3 = (f2 + 737.9f64.powi(2)).sqrt();
    let d4 = (12194.0f64).powi(2) * f2 * f2;
    let raw = num / (d1 * d2 * d3 * d4);

    // Normalize to 1.0 at 1 kHz.
    let g = 1000.0f64;
    let g2 = g * g;
    let num_g = (12194.0f64).powi(2) * g2 * g2;
    let dg1 = g2 + 20.6f64.powi(2);
    let dg2 = (g2 + 107.7f64.powi(2)).sqrt();
    let dg3 = (g2 + 737.9f64.powi(2)).sqrt();
    let dg4 = (12194.0f64).powi(2) * g2 * g2;
    let raw_g = num_g / (dg1 * dg2 * dg3 * dg4);

    (raw / raw_g) * (raw / raw_g)
}

/// Segmental SNR: split into `frame_len`-sample segments, compute each
/// segment's SNR clamped to [-10, +35] dB (the ITU-T P.561 convention),
/// and average in dB.
pub fn segmental_snr_db(signal: &[f32], test: &[f32], frame_len: usize) -> f64 {
    assert_eq!(signal.len(), test.len(), "length mismatch");
    let mut sum = 0.0f64;
    let mut count = 0usize;
    let mut i = 0usize;
    while i + frame_len <= signal.len() {
        let mut num = 0f64;
        let mut den = 0f64;
        for j in i..i + frame_len {
            let d = signal[j] as f64 - test[j] as f64;
            num += d * d;
            den += signal[j] as f64 * signal[j] as f64;
        }
        let snr = (10.0 * (den.max(1e-10) / num.max(1e-10)).log10()).clamp(-10.0, 35.0);
        sum += snr;
        count += 1;
        i += frame_len;
    }
    if count == 0 {
        0.0
    } else {
        sum / count as f64
    }
}

/// A-weighted SNR: Hann-window the signal and the ERROR into
/// 50%-overlapping 1024-sample segments, weight each error-spectrum bin
/// by the A-weighting power response, and accumulate the weighted
/// signal-vs-error energy ratio across the stream. Penalizes noise in
/// the 1-6 kHz region where hearing is most sensitive.
pub fn a_weighted_snr_db(signal: &[f32], test: &[f32], fs: u32) -> f64 {
    assert_eq!(signal.len(), test.len(), "length mismatch");
    const N: usize = 1024;
    let n = signal.len().min(test.len());

    let window: Vec<f64> = (0..N)
        .map(|i| 0.5 - 0.5 * (2.0 * std::f64::consts::PI * i as f64 / N as f64).cos())
        .collect();

    // Direct DFT of the first N/2 bins (test helper: O(N²) is fine).
    let dft = |re_in: &[f64]| -> (Vec<f64>, Vec<f64>) {
        let mut re = vec![0f64; N / 2];
        let mut im = vec![0f64; N / 2];
        for k in 0..N / 2 {
            let w = -2.0 * std::f64::consts::PI * k as f64 / N as f64;
            for (i, &x) in re_in.iter().enumerate() {
                let ph = w * i as f64;
                re[k] += x * ph.cos();
                im[k] += x * ph.sin();
            }
        }
        (re, im)
    };

    let mut num = 0f64;
    let mut den = 0f64;
    let hop = N / 2;
    let mut seg = 0usize;
    while seg + N <= n {
        let mut sig_w = vec![0f64; N];
        let mut err_w = vec![0f64; N];
        for i in 0..N {
            sig_w[i] = signal[seg + i] as f64 * window[i];
            err_w[i] = (signal[seg + i] - test[seg + i]) as f64 * window[i];
        }
        let (sr, si) = dft(&sig_w);
        let (er, ei) = dft(&err_w);
        for k in 1..N / 2 {
            let freq = fs as f64 * k as f64 / N as f64;
            let w = a_weight_power(freq);
            let sig_p = sr[k] * sr[k] + si[k] * si[k];
            let err_p = er[k] * er[k] + ei[k] * ei[k];
            num += w * sig_p;
            den += w * err_p;
        }
        seg += hop;
    }

    if den <= 0.0 {
        0.0
    } else {
        10.0 * (num / den).log10()
    }
}
