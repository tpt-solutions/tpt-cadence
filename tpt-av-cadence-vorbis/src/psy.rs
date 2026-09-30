//! Psychoacoustic masking model for the Vorbis encoder.
//!
//! For each transform block the model computes, per critical-band group, the
//! level below which quantization noise is masked:
//!
//! 1. bins are grouped into half-Bark bands (a Bark-scale frequency map);
//! 2. per-band mean power and spectral flatness are measured (flatness is the
//!    tonality estimate);
//! 3. every band's power is spread across neighbouring bands with the
//!    Schroeder spreading function (simultaneous masking);
//! 4. the spread energy is lowered by a tonality-dependent offset — tonal
//!    maskers mask less than noise-like ones (`14.5 + z` dB versus `5.5` dB,
//!    the ISO 11172-3 model 1 constants);
//! 5. the result is floored by the absolute threshold of hearing
//!    (Terhardt's approximation), referenced so that a full-scale sine is
//!    96 dB SPL.
//!
//! The output is a per-bin masking threshold in *coefficient power* units
//! (the units of the encoder's MDCT coefficients), which the encoder turns
//! into floor-1 post targets. This is a model, not a listening test: it
//! shapes noise the way the standard masking models do, but the constants
//! are unvalidated against human listeners.

/// Bark value of frequency `f` in Hz (Zwicker).
fn bark(f: f64) -> f64 {
    13.0 * (0.00076 * f).atan() + 3.5 * ((f / 7500.0) * (f / 7500.0)).atan()
}

/// Absolute threshold of hearing in dB SPL at `f_hz` (Terhardt).
fn ath_db(f_hz: f64) -> f64 {
    let f = (f_hz / 1000.0).max(0.02);
    3.64 * f.powf(-0.8) - 6.5 * (-0.6 * (f - 3.3) * (f - 3.3)).exp() + 1e-3 * f.powi(4)
}

/// Schroeder spreading function in dB for a masked band `dz` Bark above
/// the masker (negative `dz`: masked band below the masker).
fn spread_db(dz: f64) -> f64 {
    let x = dz + 0.474;
    15.81 + 7.5 * x - 17.5 * (1.0 + x * x).sqrt()
}

/// Precomputed band layout for one transform size.
struct BandTable {
    /// Number of transform coefficients.
    m: usize,
    /// Band index of each bin.
    band_of_bin: Vec<usize>,
    /// First bin and bin count of each band.
    ranges: Vec<(usize, usize)>,
    /// Bark center of each band.
    bark: Vec<f64>,
    /// Absolute-threshold power per band, in coefficient units.
    ath_power: Vec<f32>,
    /// Row-major `bands x bands` power weights: masker `j` -> masked `b`.
    spread: Vec<f32>,
}

impl BandTable {
    fn new(m: usize, sample_rate: f64, amp_ref: f64, size_scale: f64) -> Self {
        let bin_hz = sample_rate / 2.0 / m as f64;
        let mut band_of_bin = vec![0usize; m];
        let mut ranges: Vec<(usize, usize)> = Vec::new();
        let mut last_key = i64::MIN;
        for (k, slot) in band_of_bin.iter_mut().enumerate() {
            let z = bark((k as f64 + 0.5) * bin_hz);
            let key = (z * 2.0).floor() as i64;
            if key != last_key {
                ranges.push((k, 0));
                last_key = key;
            }
            let b = ranges.len() - 1;
            ranges[b].1 += 1;
            *slot = b;
        }
        let bands = ranges.len();
        let mut bark_c = Vec::with_capacity(bands);
        let mut ath_power = Vec::with_capacity(bands);
        for &(start, len) in &ranges {
            let mid = start as f64 + len as f64 / 2.0;
            let f = mid * bin_hz;
            bark_c.push(bark(f));
            // 96 dB SPL == full-scale sine == `amp_ref` coefficient amplitude.
            // Short blocks see noise-like content with larger coefficients
            // (1/sqrt(n) scaling), so the threshold is scaled to match.
            let amp = amp_ref * 10f64.powf((ath_db(f) - 96.0) / 20.0) * size_scale;
            ath_power.push((amp * amp) as f32);
        }
        let mut spread = vec![0.0f32; bands * bands];
        for b in 0..bands {
            for j in 0..bands {
                let db = spread_db(bark_c[b] - bark_c[j]);
                spread[b * bands + j] = 10f64.powf(db / 10.0) as f32;
            }
        }
        BandTable {
            m,
            band_of_bin,
            ranges,
            bark: bark_c,
            ath_power,
            spread,
        }
    }
}

/// Masking model for the long and short block sizes of one stream.
pub(crate) struct Psy {
    /// `[short, long]`.
    tables: [BandTable; 2],
}

impl Psy {
    /// `amp_ref` is the MDCT coefficient amplitude produced by a full-scale
    /// sine in a long block; `n_long`/`n_short` are the block sizes.
    pub(crate) fn new(sample_rate: u32, amp_ref: f64, m_short: usize, m_long: usize) -> Self {
        let sr = f64::from(sample_rate);
        let size_scale = (m_long as f64 / m_short as f64).sqrt();
        Psy {
            tables: [
                BandTable::new(m_short, sr, amp_ref, size_scale),
                BandTable::new(m_long, sr, amp_ref, 1.0),
            ],
        }
    }

    /// Per-bin masking threshold (coefficient power) for one channel's
    /// spectrum. `long` selects the band table.
    pub(crate) fn thresholds(&self, long: bool, coefs: &[f32], out: &mut [f32]) {
        let t = &self.tables[usize::from(long)];
        debug_assert_eq!(coefs.len(), t.m);
        let bands = t.ranges.len();

        // Per-band mean power and tonality.
        let mut energy = vec![0.0f32; bands];
        let mut tonality = vec![0.0f32; bands];
        for (b, &(start, len)) in t.ranges.iter().enumerate() {
            let bins = &coefs[start..start + len];
            let arith = bins.iter().map(|v| v * v).sum::<f32>() / len as f32;
            energy[b] = arith;
            tonality[b] = if len >= 3 && arith > 1e-20 {
                let log_mean = bins
                    .iter()
                    .map(|v| f64::from((v * v).max(1e-20)).ln())
                    .sum::<f64>()
                    / len as f64;
                let sfm_db = 10.0 * (log_mean.exp() / f64::from(arith)).log10();
                (-sfm_db / 30.0).clamp(0.0, 1.0) as f32
            } else {
                0.4
            };
        }

        // Spread and offset.
        let mut thr = vec![0.0f32; bands];
        for b in 0..bands {
            let row = &t.spread[b * bands..(b + 1) * bands];
            let spread_energy: f32 = row.iter().zip(&energy).map(|(w, e)| w * e).sum();
            let tone = tonality[b];
            let z = t.bark[b] as f32;
            let offset_db = tone * (14.5 + z) + (1.0 - tone) * 5.5;
            let masked = spread_energy * 10f32.powf(-offset_db / 10.0);
            thr[b] = masked.max(t.ath_power[b]);
        }
        for (k, o) in out.iter_mut().enumerate() {
            *o = thr[t.band_of_bin[k]];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bark_scale_is_monotonic_and_reasonable() {
        assert!(bark(100.0) < bark(1000.0) && bark(1000.0) < bark(10_000.0));
        assert!((bark(1000.0) - 8.5).abs() < 0.5);
        assert!((bark(15_500.0) - 24.0).abs() < 1.0);
    }

    #[test]
    fn ath_is_lowest_in_the_speech_band() {
        assert!(ath_db(3500.0) < ath_db(100.0));
        assert!(ath_db(3500.0) < ath_db(16_000.0));
    }

    #[test]
    fn spreading_peaks_at_zero_and_falls_off_faster_downward() {
        assert!(spread_db(0.0).abs() < 0.5);
        assert!(spread_db(2.0) > spread_db(-2.0)); // upward spread is wider
        assert!(spread_db(-4.0) < spread_db(-1.0));
    }

    #[test]
    fn masker_raises_the_threshold_of_neighbouring_bands() {
        let psy = Psy::new(44_100, 1.0, 128, 1024);
        let quiet = vec![1e-6f32; 1024];
        let mut loud_at_1k = quiet.clone();
        for v in &mut loud_at_1k[44..50] {
            *v = 0.5;
        }
        let (mut a, mut b) = (vec![0.0; 1024], vec![0.0; 1024]);
        psy.thresholds(true, &quiet, &mut a);
        psy.thresholds(true, &loud_at_1k, &mut b);
        assert!(b[60] > a[60] * 10.0, "masking must spread upward");
        assert!(b[600] <= a[600] * 2.0, "distant bands stay unmasked");
    }
}
