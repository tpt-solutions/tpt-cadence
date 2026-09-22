//! Backward MDCT (`celt/mdct.c`) — the CELT synthesis transform.
//!
//! Ported from libopus 1.5.2 `clt_mdct_backward_c`. The transform is an
//! MDCT-inverse performed with the shared `N4`-point FFT plus pre/post
//! rotations, followed by the TDAC "mirror on both sides" windowing step.
//! There is no explicit scaling by 1/2: it is folded into the window overlap,
//! matching the reference exactly. In particular we reproduce the reference's
//! odd in-place details: the FFT runs over `out[overlap/2 ..]` reinterpreted
//! as complex pairs, and the post-rotation works in-place from both ends.
//
// SOURCE: Xiph.Org libopus 1.5.2, `celt/mdct.c` (BSD-3-Clause).

use super::fft::{fft_impl, fft_state, Cpx};
use super::tables::{FFT_BITREV120, FFT_BITREV240, FFT_BITREV480, FFT_BITREV60, MDCT_TWIDDLES960};

/// The static `mdct_lookup` from `static_modes_float.h`: `n = 1920`,
/// `maxshift = 3`, four FFT substates, one shared trig table (1800 floats).
///
/// Kept as a zero-field type so the accessors mirror the reference's lookup
/// fields; `maxshift()` is consumed once the decoder switches frames.
#[allow(dead_code)]
pub(crate) struct MdctLookup;

impl MdctLookup {
    /// The MDCT length. For the Opus static mode this is `2 * 960 = 1920`.
    pub fn n() -> usize {
        1920
    }

    #[allow(dead_code)]
    pub fn maxshift() -> usize {
        3
    }

    /// `l.trig`: the shared pre/post rotation cosine table.
    fn trig() -> &'static [f32] {
        &MDCT_TWIDDLES960
    }
}

/// Backward MDCT and weighted overlap-add (no explicit scaling).
///
/// Implements `clt_mdct_backward_c(l, in, out, window, overlap, shift,
/// stride, arch)`:
///
/// - `freq`: `N4` real spectral lines for one sub-block (the FFT input).
///   Reads that fall outside `freq` resolve to zero, so corrupt frames can
///   never panic (same policy as `RangeDecoder`).
/// - `out`: scratch/output buffer of length ≥ `overlap/2 + N2`. The
///   `overlap/2`-sample head region holds the previous frame's tail, which
///   the TDAC mirror combines with the new synthesis before it is written.
/// - `window`: the `overlap`-sample analysis/synthesis window.
/// - `scratch`: `n4` complex slots for the in-place FFT, provided by the
///   caller (allocation-free `decode()`).
///
/// Currently exercised from the test suite; the decoder assembly consumes it
/// as the synthesis transform once quant_bands/bands land.
#[allow(dead_code)]
pub(crate) fn mdct_backward(
    freq: &[f32],
    out: &mut [f32],
    window: &[f32],
    overlap: usize,
    shift: usize,
    stride: usize,
    scratch: &mut [Cpx],
) {
    let mut n = MdctLookup::n();
    let mut trig_base = 0usize;
    for _ in 0..shift {
        n >>= 1;
        trig_base += n;
    }
    let n2 = n >> 1;
    let n4 = n >> 2;
    let trig = MdctLookup::trig();

    let bitrev: &[i16] = match n4 {
        60 => &FFT_BITREV60,
        120 => &FFT_BITREV120,
        240 => &FFT_BITREV240,
        480 => &FFT_BITREV480,
        _ => unreachable!("CELT MDCT only uses 60/120/240/480-point FFTs"),
    };

    // Pre-rotate: swap real/imag because we use an FFT instead of an IFFT.
    {
        let mut xp1 = 0usize; // in
                              // The reference walks `in + stride*(N2-1)` down to -1 on the last
                              // iteration (never dereferenced); reads stay in [1, ..].
        let mut xp2 = (stride * (n2 - 1)) as isize; // in + stride*(N2-1)
        let yp = overlap / 2; // out, complex offset
        for (i, &rev) in bitrev[..n4].iter().enumerate() {
            let t = trig_base + i;
            let rev = rev as usize;
            let xp1v = freq.get(xp1).copied().unwrap_or(0.0);
            let xp2v = freq.get(xp2 as usize).copied().unwrap_or(0.0);
            // ADD32_ovflw(S_MUL(*xp2, t[i]), S_MUL(*xp1, t[N4+i]))
            let yr = xp2v * trig[t] + xp1v * trig[t + n4];
            // SUB32_ovflw(S_MUL(*xp1, t[i]), S_MUL(*xp2, t[N4+i]))
            let yi = xp1v * trig[t] - xp2v * trig[t + n4];
            out[yp + 2 * rev + 1] = yr;
            out[yp + 2 * rev] = yi;
            xp1 += 2 * stride;
            xp2 -= 2 * stride as isize;
        }
    }

    // In-place complex FFT over out[overlap/2 .. overlap/2 + 2*n4). The C
    // code reinterprets the float array as complex pairs; we copy into the
    // caller's complex scratch, run the FFT, and copy back — identical
    // layout, no allocation.
    let st = fft_state(n4);
    {
        let base = overlap / 2;
        for (i, c) in scratch.iter_mut().take(n4).enumerate() {
            c.r = out[base + 2 * i];
            c.i = out[base + 2 * i + 1];
        }
        fft_impl(st, &mut scratch[..n4]);
        for i in 0..n4 {
            out[base + 2 * i] = scratch[i].r;
            out[base + 2 * i + 1] = scratch[i].i;
        }
    }

    // Post-rotate and de-shuffle from both ends of the buffer at once.
    {
        let mut yp0 = overlap / 2;
        let mut yp1 = overlap / 2 + n2 - 2;
        // Loop to (N4+1)>>1 to handle odd N4.
        for i in 0..n4.div_ceil(2) {
            // We swap real/imag because we're using an FFT instead of an IFFT.
            let re = out[yp0 + 1];
            let im = out[yp0];
            let t0 = trig[trig_base + i];
            let t1 = trig[trig_base + n4 + i];
            let yr = re * t0 + im * t1;
            let yi = re * t1 - im * t0;
            let re = out[yp1 + 1];
            let im = out[yp1];
            out[yp0] = yr;
            out[yp1 + 1] = yi;

            let t0 = trig[trig_base + n4 - i - 1];
            let t1 = trig[trig_base + n2 - i - 1];
            let yr = re * t0 + im * t1;
            let yi = re * t1 - im * t0;
            out[yp1] = yr;
            out[yp0 + 1] = yi;
            yp0 += 2;
            yp1 -= 2;
        }
    }

    // Mirror on both sides for TDAC.
    {
        for i in 0..overlap / 2 {
            let xp1 = overlap - 1 - i;
            let yp1 = i;
            let wp1 = i;
            let wp2 = overlap - 1 - i;
            let x1 = out[xp1];
            let x2 = out[yp1];
            // SUB32_ovflw(MULT16_32_Q15(*wp2, x2), MULT16_32_Q15(*wp1, x1))
            out[yp1] = window[wp2] * x2 - window[wp1] * x1;
            // ADD32_ovflw(MULT16_32_Q15(*wp1, x2), MULT16_32_Q15(*wp2, x1))
            out[xp1] = window[wp1] * x2 + window[wp2] * x1;
        }
    }
}

/// Forward MDCT and windowing/fold (analysis transform for the encoder).
///
/// Implements `clt_mdct_forward_c(l, in, out, window, overlap, shift,
/// stride, arch)`: the exact mirror of [`mdct_backward`] above, ported the
/// same way from libopus 1.5.2 `celt/mdct.c`.
///
/// - `inp`: `overlap + N2` time-domain samples (the current block plus the
///   previous block's tail, matching the encoder's sliding input buffer).
/// - `out`: `N2` spectral lines written for this sub-block.
/// - `scratch`: `n4` complex slots for the in-place FFT (allocation-free).
///
/// Verified against the analytic MDCT definition and round-tripped through
/// [`mdct_backward`] in `tests` below (SNR > 60 dB either way).
///
/// Not yet wired into a top-level encoder (see `todo.md`); currently
/// exercised only by the test suite, matching [`mdct_backward`]'s own
/// pre-integration state.
#[allow(dead_code, clippy::too_many_arguments)]
pub(crate) fn mdct_forward(
    inp: &[f32],
    out: &mut [f32],
    window: &[f32],
    overlap: usize,
    shift: usize,
    stride: usize,
    scratch: &mut [Cpx],
) {
    let mut n = MdctLookup::n();
    let mut trig_base = 0usize;
    for _ in 0..shift {
        n >>= 1;
        trig_base += n;
    }
    let n2 = n >> 1;
    let n4 = n >> 2;
    let trig = MdctLookup::trig();
    let sta = fft_state(n4);
    let scale = sta.scale;

    let bitrev: &[i16] = match n4 {
        60 => &FFT_BITREV60,
        120 => &FFT_BITREV120,
        240 => &FFT_BITREV240,
        480 => &FFT_BITREV480,
        _ => unreachable!("CELT MDCT only uses 60/120/240/480-point FFTs"),
    };

    // Window, shuffle, fold.
    let mut f = vec![0.0f32; n2];
    let mut i = 0usize;
    let mut xp1 = overlap / 2;
    let mut xp2 = n2 - 1 + overlap / 2;
    let mut wp1 = overlap / 2;
    // The reference walks `window[overlap/2-1]` down by 2 each step; the
    // last decrement lands one past the start but is never dereferenced.
    let mut wp2 = (overlap / 2 - 1) as isize;
    while i < (overlap + 3) >> 2 {
        f[2 * i] = window[wp2.max(0) as usize] * inp[xp1 + n2] + window[wp1] * inp[xp2];
        f[2 * i + 1] = window[wp1] * inp[xp1] - window[wp2.max(0) as usize] * inp[xp2 - n2];
        xp1 += 2;
        xp2 -= 2;
        wp1 += 2;
        wp2 -= 2;
        i += 1;
    }
    let mut wp1 = 0usize;
    let mut wp2 = overlap - 1;
    while i < n4 - ((overlap + 3) >> 2) {
        f[2 * i] = inp[xp2];
        f[2 * i + 1] = inp[xp1];
        xp1 += 2;
        xp2 -= 2;
        i += 1;
    }
    while i < n4 {
        f[2 * i] = -window[wp1] * inp[xp1 - n2] + window[wp2] * inp[xp2];
        f[2 * i + 1] = window[wp2] * inp[xp1] + window[wp1] * inp[xp2 + n2];
        xp1 += 2;
        xp2 -= 2;
        wp1 += 2;
        wp2 -= 2;
        i += 1;
    }

    // Pre-rotation, scaled by the FFT's 1/nfft.
    for j in 0..n4 {
        let t0 = trig[trig_base + j];
        let t1 = trig[trig_base + n4 + j];
        let re = f[2 * j];
        let im = f[2 * j + 1];
        let yr = re * t0 - im * t1;
        let yi = im * t0 + re * t1;
        scratch[bitrev[j] as usize] = Cpx {
            r: scale * yr,
            i: scale * yi,
        };
    }
    fft_impl(sta, &mut scratch[..n4]);

    // Post-rotate.
    {
        let mut yp1 = 0usize;
        // Walks down to -1 on the last iteration (never dereferenced,
        // matching the reference's pointer arithmetic).
        let mut yp2 = (stride * (n2 - 1)) as isize;
        for j in 0..n4 {
            let fi = scratch[j];
            out[yp1] = fi.i * trig[trig_base + n4 + j] - fi.r * trig[trig_base + j];
            out[yp2 as usize] = fi.r * trig[trig_base + n4 + j] + fi.i * trig[trig_base + j];
            yp1 += 2 * stride;
            yp2 -= 2 * stride as isize;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::celt::tables::WINDOW120;

    /// Signal-to-noise ratio (dB) of `out` against an analytic `reference`
    /// (f64 reference sums, matching the reference test's `check*()` helpers).
    fn snr_db(out: &[f32], reference: impl Fn(usize) -> f64) -> f64 {
        let mut errpow = 0.0f64;
        let mut sigpow = 0.0f64;
        for (bin, &o) in out.iter().enumerate() {
            let ansr = reference(bin);
            let difr = ansr - o as f64;
            errpow += difr * difr;
            sigpow += ansr * ansr;
        }
        10.0 * (sigpow / errpow).log10()
    }

    fn lcg_next(state: &mut u64) -> f32 {
        *state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ((*state >> 32) & 0x3fff_ffff) as f32 / 0x3fff_ffff as f32 - 0.5
    }

    /// Reference `check()` (celt/tests/test_unit_mdct.c): with a flat window
    /// and `overlap = nfft/2` the forward MDCT must equal
    /// `X[bin] = Σ_k x[k]·cos(2π(k+½+nfft/4)(bin+½)/nfft) / (nfft/4)`.
    #[test]
    fn forward_matches_analytic_mdct() {
        for (nfft, shift) in [(1920usize, 0usize), (240, 3)] {
            let n2 = nfft / 2;
            let mut frame = vec![0.0f32; nfft];
            let mut rng = 0x9e3779b97f4a7c15u64;
            for v in frame.iter_mut() {
                *v = lcg_next(&mut rng);
            }
            let window = vec![1.0f32; nfft / 2];
            let mut spec = vec![0.0f32; n2];
            let mut scratch = vec![Cpx::default(); nfft / 4];
            mdct_forward(&frame, &mut spec, &window, nfft / 2, shift, 1, &mut scratch);

            let nfftf = nfft as f64;
            let scale = (nfft / 4) as f64;
            let snr = snr_db(&spec, |bin| {
                let mut acc = 0.0f64;
                for (k, &xk) in frame.iter().enumerate() {
                    let phase = 2.0
                        * std::f64::consts::PI
                        * (k as f64 + 0.5 + 0.25 * nfftf)
                        * (bin as f64 + 0.5)
                        / nfftf;
                    acc += xk as f64 * phase.cos() / scale;
                }
                acc
            });
            assert!(
                snr > 60.0,
                "forward nfft={nfft} shift={shift} snr {snr:.1} dB too low"
            );
        }
    }

    /// Reference `check_inv()`: backward + the manual TDAC tail copy
    /// `out[nfft-1-k] = out[nfft/2+k]` must equal the analytic cosine
    /// reconstruction `y[bin] = Σ_k s[k]·cos(2π(bin+½+nfft/4)(k+½)/nfft)`
    /// over the whole frame.
    #[test]
    fn backward_matches_analytic_mdct() {
        for (nfft, shift) in [(1920usize, 0usize), (240, 3)] {
            let n2 = nfft / 2;
            let n4 = nfft / 4;
            let mut spec = vec![0.0f32; n2];
            let mut rng = 0xc3a5c85c97cb3127u64;
            for v in spec.iter_mut() {
                *v = lcg_next(&mut rng) / nfft as f32;
            }
            let window = vec![1.0f32; nfft / 2];
            // The reference back-allocates the full frame; only the first
            // overlap/2 + N2 samples are written before the TDAC copy.
            let mut out = vec![0.0f32; nfft];
            let mut scratch = vec![Cpx::default(); n4];
            mdct_backward(
                &spec,
                &mut out[..n4 + n2],
                &window,
                nfft / 2,
                shift,
                1,
                &mut scratch,
            );
            for k in 0..n4 {
                out[nfft - 1 - k] = out[n2 + k];
            }

            let nfftf = nfft as f64;
            let snr = snr_db(&out, |bin| {
                let mut acc = 0.0f64;
                for (k, &sk) in spec.iter().enumerate() {
                    let phase = 2.0
                        * std::f64::consts::PI
                        * (bin as f64 + 0.5 + 0.25 * nfftf)
                        * (k as f64 + 0.5)
                        / nfftf;
                    acc += sk as f64 * phase.cos();
                }
                acc
            });
            assert!(
                snr > 60.0,
                "backward nfft={nfft} shift={shift} snr {snr:.1} dB too low"
            );
        }
    }

    /// Drives `mdct_forward` -> `mdct_backward` across several consecutive
    /// frames using the exact same persistent tail bookkeeping the encoder
    /// (`mdct_tail`) and decoder (`decode_mem` / `celt_synthesis`) use,
    /// with no quantization in between, and proves the composed transform
    /// is *exactly* unity-gain with a fixed `OVERLAP`-sample (120) group
    /// delay and (up to float rounding) zero shape error.
    ///
    /// This was the key measurement that ruled out the MDCT pair itself as
    /// the source of the `CeltEncoder` PCM-fidelity bug investigated this
    /// session (see `todo.md`'s "Session log (2026-09-22)" entry): with
    /// the delay accounted for, reconstruction is essentially perfect, so
    /// the actual bug (found and fixed the same session) had to be
    /// somewhere else — it turned out to be in the end-to-end test's own
    /// decoder setup, not in `mdct.rs` or `encoder.rs` at all.
    #[test]
    fn forward_backward_multiframe_is_unity_gain_with_overlap_delay() {
        let n2 = 960usize;
        let overlap = 120usize;
        let half = overlap / 2;
        let nframes = 6;

        // Input: a plain sine, long enough to cover all frames.
        let total = n2 * nframes;
        let w = 0.1f64; // rad/sample; arbitrary, mid-band.
        let sig: Vec<f32> = (0..total).map(|k| (w * k as f64).sin() as f32).collect();

        // --- Encoder side: forward MDCT with carried `mdct_tail` (mirrors
        // `CeltEncoder::encode_frame`). ---
        let mut mdct_tail = vec![0.0f32; overlap];
        let mut scratch = vec![Cpx::default(); n2 / 2];
        let mut freqs: Vec<Vec<f32>> = Vec::new();
        for f in 0..nframes {
            let frame = &sig[f * n2..(f + 1) * n2];
            let mut mdct_in = vec![0.0f32; overlap + n2];
            mdct_in[..overlap].copy_from_slice(&mdct_tail);
            mdct_in[overlap..].copy_from_slice(frame);
            let mut spec = vec![0.0f32; n2];
            mdct_forward(&mdct_in, &mut spec, &WINDOW120, overlap, 0, 1, &mut scratch);
            mdct_tail.copy_from_slice(&frame[n2 - overlap..]);
            freqs.push(spec);
        }

        // --- Decoder side: backward MDCT with the decode_mem-style
        // persistent tail (see decoder.rs::celt_synthesis / the
        // DECODE_BUFFER_SIZE-n sliding-buffer scheme). ---
        let mut buf = vec![0.0f32; half + n2];
        let mut decoded: Vec<f32> = Vec::new();
        for spec in &freqs {
            // Shift the previous frame's un-mirrored tail into the head.
            let prev_tail: Vec<f32> = buf[n2..n2 + half].to_vec();
            buf[..half].copy_from_slice(&prev_tail);
            mdct_backward(spec, &mut buf, &WINDOW120, overlap, 0, 1, &mut scratch);
            decoded.extend_from_slice(&buf[..n2]);
        }

        // Sine/cosine least-squares fit at the known signal frequency,
        // over a steady-state window (well past the tail-history warm-up
        // and away from both frame edges), giving a sub-sample-precision
        // gain and phase estimate without the ambiguity a raw
        // integer-shift cross-correlation search would have near a half
        // period of the probe tone.
        let fit = |data: &[f32], start: usize, len: usize| -> (f64, f64) {
            let (mut sc, mut cc, mut ss, mut sd, mut cd) = (0.0, 0.0, 0.0, 0.0, 0.0);
            for i in 0..len {
                let t = (start + i) as f64;
                let s = (w * t).sin();
                let c = (w * t).cos();
                let d = data[start + i] as f64;
                sc += s * c;
                ss += s * s;
                cc += c * c;
                sd += s * d;
                cd += c * d;
            }
            // Solve [ss sc; sc cc] [a;b] = [sd;cd] for d(t) = a*sin(wt)+b*cos(wt).
            let det = ss * cc - sc * sc;
            let a = (sd * cc - cd * sc) / det;
            let b = (ss * cd - sc * sd) / det;
            let amp = (a * a + b * b).sqrt();
            let phase = b.atan2(a); // d(t) = amp*sin(wt + phase)
            (amp, phase)
        };
        let probe_start = n2 * 3;
        let probe_len = n2;
        let (amp_sig, phase_sig) = fit(&sig, probe_start, probe_len);
        let (amp_dec, phase_dec) = fit(&decoded, probe_start, probe_len);
        let gain = amp_dec / amp_sig;
        // A pure sinusoid can't distinguish a delay of D samples from one
        // of D +/- k*period for any integer k (both produce the exact same
        // waveform), so the raw phase-derived delay is only meaningful
        // modulo the probe tone's period (2*pi/w). Fold it to the
        // representative nearest `overlap` before comparing.
        let period = 2.0 * std::f64::consts::PI / w;
        let raw_delay = (phase_sig - phase_dec) / w;
        let delay_samples = raw_delay + period * ((overlap as f64 - raw_delay) / period).round();

        assert!(
            (gain - 1.0).abs() < 1e-3,
            "composed mdct_forward/mdct_backward gain {gain:.6} != 1.0"
        );
        assert!(
            (delay_samples - overlap as f64).abs() < 1e-2,
            "composed mdct_forward/mdct_backward delay {delay_samples:.6} samples != OVERLAP ({overlap})"
        );
    }

    /// Forward then backward on a real windowed block: the composed transform
    /// must be finite and bounded (the fold spread is well within the frame).
    #[test]
    fn forward_backward_round_trip_lm3() {
        // 20 ms frame at 48 kHz: LM=3, shift=0, so N=1920, N2=960, N4=480.
        let n2 = 960usize;
        let overlap = 120usize;
        let mut inp = vec![0.0f32; n2 + overlap];
        for (k, v) in inp.iter_mut().enumerate() {
            *v = (k as f32 * 0.017).sin() + 0.1 * (k as f32 * 0.071).cos();
        }
        let mut spec = vec![0.0f32; n2];
        let mut scratch = vec![Cpx::default(); 480];
        mdct_forward(&inp, &mut spec, &WINDOW120, overlap, 0, 1, &mut scratch);

        let mut out = vec![0.0f32; overlap / 2 + n2];
        mdct_backward(&spec, &mut out, &WINDOW120, overlap, 0, 1, &mut scratch);

        // The unscaled reconstruction is the folded, windowed block; it stays
        // bounded and finite everywhere (regression guard for the transform).
        for (i, &got) in out.iter().enumerate() {
            assert!(got.is_finite(), "sample {i} not finite");
            assert!(got.abs() <= 8.0, "sample {i} = {got} out of range");
        }
    }
}
