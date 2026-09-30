//! Source-fidelity gates for the MP3 encoder: what the decoder hands back is
//! compared against the *source* signal, not against another decoder.
//!
//! Inter-decoder agreement (the FFmpeg cross-checks) only proves that two
//! decoders read the same bitstream; it cannot notice an encoder that codes
//! the wrong signal. These tests pin the encoder's actual accuracy: the
//! gapless-trimmed decode must line up with the source at lag 0, tones must
//! come back at the right amplitude with the right purity, transients must
//! not smear ahead of their onset, and mid/side, intensity and every version
//! family must preserve level.

use std::f32::consts::PI;
use std::io::Cursor;

use tpt_av_cadence_core::{Decoder, Encoder};
use tpt_av_cadence_mp3::{Mp3Decoder, Mp3Encoder};

/// Decodes a tagged stream with this crate's decoder (which applies the
/// Info/Xing gapless trim, so output sample `i` corresponds to source `i`).
fn decode(data: Vec<u8>, channels: usize) -> Vec<f32> {
    let mut dec = Mp3Decoder::open(Box::new(Cursor::new(data))).unwrap();
    let mut out = Vec::new();
    let mut buf = vec![0.0f32; 4096 * channels];
    loop {
        match dec.decode(&mut buf).unwrap() {
            0 => break,
            n => out.extend_from_slice(&buf[..n * channels]),
        }
    }
    out
}

fn encode_cbr(sr: u32, channels: u16, kbps: u32, frames: &[f32], intensity: bool) -> Vec<u8> {
    let mut buf = Cursor::new(Vec::new());
    {
        let mut enc = Mp3Encoder::new_cbr_with_info(&mut buf, sr, channels, kbps).unwrap();
        enc.set_intensity_stereo(intensity);
        enc.encode(frames).unwrap();
        Encoder::finish(&mut enc).unwrap();
    }
    buf.into_inner()
}

fn snr_db(reference: &[f32], decoded: &[f32]) -> f64 {
    let (mut s, mut e) = (0.0f64, 0.0f64);
    for (&r, &d) in reference.iter().zip(decoded) {
        s += f64::from(r).powi(2);
        e += (f64::from(r) - f64::from(d)).powi(2);
    }
    10.0 * (s / e.max(1e-30)).log10()
}

/// Least-squares fit of `a*sin + b*cos` at `freq` over one channel of
/// `x[skip..skip + len]`: returns (amplitude, tone-to-rest ratio in dB).
/// Phase- and delay-insensitive, so it measures purity, not alignment.
fn tone_fit(x: &[f32], channels: usize, ch: usize, freq: f64, sr: f64) -> (f64, f64) {
    let frames = x.len() / channels;
    let skip = (frames / 3).min(8000);
    let len = 24000usize.min(frames - skip);
    let (mut sc, mut ss, mut cc, mut sx, mut cx, mut xx) = (0.0, 0.0, 0.0, 0.0, 0.0, 0.0);
    for i in 0..len {
        let v = f64::from(x[(skip + i) * channels + ch]);
        let a = 2.0 * std::f64::consts::PI * freq * (skip + i) as f64 / sr;
        let (s, c) = (a.sin(), a.cos());
        sc += s * c;
        ss += s * s;
        cc += c * c;
        sx += s * v;
        cx += c * v;
        xx += v * v;
    }
    let det = ss * cc - sc * sc;
    let a = (sx * cc - cx * sc) / det;
    let b = (cx * ss - sx * sc) / det;
    let fit = a * sx + b * cx;
    (
        (a * a + b * b).sqrt(),
        10.0 * (fit / (xx - fit).max(1e-15)).log10(),
    )
}

fn tone(sr: u32, freq: f32, amp: f32, seconds: f32, channels: usize) -> Vec<f32> {
    tone_n(sr, freq, amp, (sr as f32 * seconds) as usize, channels)
}

fn tone_n(sr: u32, freq: f32, amp: f32, n: usize, channels: usize) -> Vec<f32> {
    let mut out = Vec::with_capacity(n * channels);
    for i in 0..n {
        let s = amp * (2.0 * PI * freq * i as f32 / sr as f32).sin();
        for _ in 0..channels {
            out.push(s);
        }
    }
    out
}

/// Every version family, mono and stereo: a mid-band tone must come back at
/// its own amplitude and far above the coding noise.
#[test]
fn tones_survive_every_sample_rate_and_channel_mode() {
    // (sample rate, bitrate kbps): MPEG-1, MPEG-2 (LSF) and MPEG-2.5.
    let configs = [
        (48_000u32, 128u32),
        (44_100, 128),
        (32_000, 96),
        (24_000, 64),
        (22_050, 64),
        (16_000, 48),
        (12_000, 32),
        (11_025, 32),
        (8_000, 24),
    ];
    for &(sr, kbps) in &configs {
        for channels in [1usize, 2] {
            let freq = 900.0f32;
            let src = tone(sr, freq, 0.5, 1.0, channels);
            let out = decode(encode_cbr(sr, channels as u16, kbps, &src, false), channels);
            for ch in 0..channels {
                let (amp, purity) = tone_fit(&out, channels, ch, f64::from(freq), f64::from(sr));
                eprintln!("{sr} Hz x{channels} ch{ch}: amp {amp:.3}, tone-to-noise {purity:.1} dB");
                assert!(
                    (amp - 0.5).abs() < 0.01,
                    "{sr} Hz x{channels}: amplitude {amp:.3} != 0.5"
                );
                assert!(
                    purity > 35.0,
                    "{sr} Hz x{channels}: tone-to-noise only {purity:.1} dB"
                );
            }
        }
    }
}

/// A range of tone frequencies through the polyphase bands and MDCT lines.
#[test]
fn tone_purity_across_the_spectrum() {
    let sr = 44_100u32;
    for freq in [120.0f32, 440.0, 1000.0, 2500.0, 5000.0, 9000.0, 14_000.0] {
        let src = tone(sr, freq, 0.4, 1.0, 1);
        let out = decode(encode_cbr(sr, 1, 128, &src, false), 1);
        let (amp, purity) = tone_fit(&out, 1, 0, f64::from(freq), f64::from(sr));
        eprintln!("{freq} Hz: amp {amp:.3}, tone-to-noise {purity:.1} dB");
        assert!((amp - 0.4).abs() < 0.008, "{freq} Hz: amplitude {amp:.3}");
        assert!(purity > 40.0, "{freq} Hz: tone-to-noise {purity:.1} dB");
    }
}

/// Mid/side coding (dual-mono and near-mono stereo) must preserve level: the
/// mid/side scaling once left both channels 3 dB low while every
/// inter-decoder check stayed green.
#[test]
fn mid_side_preserves_level_and_phase() {
    let sr = 44_100u32;
    // Dual mono -> pure mid.
    let src = tone(sr, 1000.0, 0.5, 1.0, 2);
    let out = decode(encode_cbr(sr, 2, 128, &src, false), 2);
    let (amp, purity) = tone_fit(&out, 2, 0, 1000.0, f64::from(sr));
    assert!((amp - 0.5).abs() < 0.01, "dual-mono amplitude {amp:.3}");
    assert!(purity > 40.0, "dual-mono purity {purity:.1} dB");

    // Mostly-centered stereo with a small side component.
    let mut src = Vec::new();
    for i in 0..sr as usize {
        let t = i as f32 / sr as f32;
        let m = 0.4 * (2.0 * PI * 700.0 * t).sin();
        let s = 0.05 * (2.0 * PI * 1900.0 * t).sin();
        src.push(m + s);
        src.push(m - s);
    }
    let out = decode(encode_cbr(sr, 2, 192, &src, false), 2);
    let snr = snr_db(&src, &out);
    eprintln!("near-mono stereo SNR {snr:.1} dB");
    assert!(snr > 24.0, "near-mono stereo SNR {snr:.1} dB");
}

/// Independent channels (no mid/side): each channel keeps its own tone.
#[test]
fn independent_channels_stay_independent() {
    let sr = 44_100u32;
    let mut src = Vec::new();
    for i in 0..sr as usize {
        let t = i as f32 / sr as f32;
        src.push(0.5 * (2.0 * PI * 1000.0 * t).sin());
        src.push(0.3 * (2.0 * PI * 1700.0 * t).sin());
    }
    let out = decode(encode_cbr(sr, 2, 192, &src, false), 2);
    let (a0, p0) = tone_fit(&out, 2, 0, 1000.0, f64::from(sr));
    let (a1, p1) = tone_fit(&out, 2, 1, 1700.0, f64::from(sr));
    assert!(
        (a0 - 0.5).abs() < 0.01 && (a1 - 0.3).abs() < 0.01,
        "{a0} {a1}"
    );
    assert!(p0 > 40.0 && p1 > 40.0, "{p0} {p1}");
}

/// Broadband material must line up with the source at lag 0 (the tag's
/// delay field is right) and reach a bitrate-appropriate SNR.
#[test]
fn broadband_signal_is_aligned_and_accurate() {
    let sr = 44_100u32;
    let mut seed = 0x1234_5678u32;
    let mut lp = [0.0f32; 2];
    let mut src = Vec::new();
    for i in 0..sr as usize * 2 {
        let t = i as f32 / sr as f32;
        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        let n = ((seed >> 16) as f32 / 32768.0 - 1.0) * 0.3;
        for (c, l) in lp.iter_mut().enumerate() {
            // One-pole-lowpassed noise plus harmonics: music-like tilt.
            *l = 0.85 * *l + 0.15 * n * (1.0 + c as f32);
        }
        let h = 0.2 * (2.0 * PI * 220.0 * t).sin() + 0.1 * (2.0 * PI * 1320.0 * t).sin();
        src.push(lp[0] + h);
        src.push(lp[1] + 0.8 * h);
    }
    let out = decode(encode_cbr(sr, 2, 192, &src, false), 2);
    assert_eq!(out.len(), src.len(), "gapless length");
    let snr = snr_db(&src, &out);
    // A lag-1 comparison must be clearly worse: proves the alignment is
    // exact rather than merely close.
    let shifted = snr_db(&src[2..], &out[..out.len() - 2]);
    eprintln!("broadband SNR at lag 0: {snr:.1} dB, at lag 1: {shifted:.1} dB");
    assert!(snr > 18.0, "SNR {snr:.1} dB");
    assert!(
        snr > shifted + 6.0,
        "not sample-aligned ({snr:.1} vs {shifted:.1})"
    );
}

/// A click train (window switching): every click must land where it was
/// placed, with almost no energy leaking ahead of it.
#[test]
fn transients_do_not_smear_ahead_of_the_onset() {
    let sr = 44_100u32;
    let period = 5000usize;
    let n = sr as usize * 2;
    let mut src = vec![0.0f32; n * 2];
    let mut seed = 99u32;
    let mut onsets = Vec::new();
    for start in (period..n - 400).step_by(period) {
        onsets.push(start);
        for k in 0..200usize {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let noise = (seed >> 16) as f32 / 32768.0 - 1.0;
            let v = 0.6 * noise * (-(k as f32) / 40.0).exp();
            src[(start + k) * 2] = v;
            src[(start + k) * 2 + 1] = v * 0.9;
        }
    }
    let out = decode(encode_cbr(sr, 2, 192, &src, false), 2);
    assert_eq!(out.len(), src.len());
    let energy = |x: &[f32], from: usize, to: usize| -> f64 {
        (from..to).map(|i| f64::from(x[i * 2]).powi(2)).sum()
    };
    let mut worst_near = f64::NEG_INFINITY;
    let mut worst_far = f64::NEG_INFINITY;
    let mut worst_snr = f64::INFINITY;
    for &o in &onsets {
        let burst = energy(&src, o, o + 200);
        // Within a short block's reach (~256 samples) some spread is inherent;
        // beyond it the click must not have leaked at all.
        let near = energy(&out, o - 256, o - 40);
        let far = energy(&out, o - 600, o - 256);
        worst_near = worst_near.max(10.0 * (near / burst).log10());
        worst_far = worst_far.max(10.0 * (far / burst).log10());
        let err: f64 = (o..o + 400)
            .map(|i| (f64::from(src[i * 2]) - f64::from(out[i * 2])).powi(2))
            .sum();
        worst_snr = worst_snr.min(10.0 * (burst / err.max(1e-30)).log10());
    }
    eprintln!(
        "pre-echo: {worst_near:.1} dB (near), {worst_far:.1} dB (far) below the onset; worst click SNR {worst_snr:.1} dB"
    );
    assert!(
        worst_near < -20.0,
        "near pre-echo {worst_near:.1} dB below the click"
    );
    assert!(
        worst_far < -33.0,
        "far pre-echo {worst_far:.1} dB below the click"
    );
    assert!(worst_snr > 14.0, "click SNR {worst_snr:.1} dB");
}

/// The reservoir/short-block/M-S machinery must also hold in VBR mode.
#[test]
fn vbr_tone_is_accurate() {
    let sr = 44_100u32;
    let src = tone(sr, 1500.0, 0.5, 1.5, 2);
    let mut buf = Cursor::new(Vec::new());
    {
        let mut enc = Mp3Encoder::new_vbr_with_xing(&mut buf, sr, 2, 3).unwrap();
        enc.encode(&src).unwrap();
        Encoder::finish(&mut enc).unwrap();
    }
    let out = decode(buf.into_inner(), 2);
    let (amp, purity) = tone_fit(&out, 2, 0, 1500.0, f64::from(sr));
    eprintln!("VBR: amp {amp:.3}, tone-to-noise {purity:.1} dB");
    assert!((amp - 0.5).abs() < 0.01, "amplitude {amp:.3}");
    assert!(purity > 30.0, "purity {purity:.1} dB");
}

/// Intensity stereo must keep the panned source's level in both channels.
#[test]
fn intensity_stereo_preserves_levels() {
    let sr = 44_100u32;
    let mut src = Vec::new();
    for i in 0..sr as usize * 2 {
        let t = i as f32 / sr as f32;
        let low = 0.3 * (2.0 * PI * 500.0 * t).sin();
        let high = 0.2 * (2.0 * PI * 9000.0 * t).sin();
        src.push(low + high);
        src.push(0.5 * low + 0.5 * high);
    }
    let plain = decode(encode_cbr(sr, 2, 128, &src, false), 2);
    let is = decode(encode_cbr(sr, 2, 128, &src, true), 2);
    // Intensity positions are quantized (15 degree steps on MPEG-1), so a
    // single channel's level may move ~15 % while the band energy stays put.
    let (mut e_plain, mut e_is) = (0.0f64, 0.0f64);
    for ch in 0..2 {
        let (a_plain, _) = tone_fit(&plain, 2, ch, 9000.0, f64::from(sr));
        let (a_is, _) = tone_fit(&is, 2, ch, 9000.0, f64::from(sr));
        eprintln!("ch{ch}: 9 kHz level plain {a_plain:.3} vs intensity {a_is:.3}");
        assert!(
            (a_is / a_plain - 1.0).abs() < 0.15,
            "ch{ch}: intensity level {a_is:.3} vs plain {a_plain:.3}"
        );
        e_plain += a_plain * a_plain;
        e_is += a_is * a_is;
    }
    assert!(
        (e_is / e_plain - 1.0).abs() < 0.05,
        "intensity coding must preserve the band energy ({e_is:.4} vs {e_plain:.4})"
    );
}

/// The last samples of the input must not be lost to the encoder's
/// look-ahead buffering (up to a granule of PCM is pending at `finish`):
/// the gapless-trimmed decode is exactly as long as the source and its final
/// stretch is still accurate.
#[test]
fn stream_tail_is_not_truncated() {
    let sr = 44_100u32;
    for extra in [1usize, 200, 575, 576, 577, 1000, 1151] {
        let n = 1152 * 6 + extra;
        let src = tone_n(sr, 700.0, 0.5, n, 2);
        let src = &src[..];
        let out = decode(encode_cbr(sr, 2, 192, src, false), 2);
        assert_eq!(out.len(), src.len(), "length for extra={extra}");
        let tail = 2 * extra.min(400);
        let snr = snr_db(&src[src.len() - tail..], &out[out.len() - tail..]);
        assert!(
            snr > 25.0,
            "extra={extra}: last {tail} samples SNR {snr:.1} dB"
        );
    }
}

/// FFmpeg must recover the exact source length from the Info tag too (the
/// tag's frame count is audio frames only, per LAME).
#[test]
fn ffmpeg_gapless_length_matches_source() {
    use tpt_av_cadence_test_utils::reference::{decode_with_ffmpeg, ffmpeg_available};
    if !ffmpeg_available() {
        if std::env::var_os("CADENCE_REQUIRE_FFMPEG").is_some() {
            panic!("FFmpeg is required but unavailable on PATH");
        }
        eprintln!("skipping: FFmpeg not on PATH");
        return;
    }
    let sr = 44_100u32;
    for extra in [1usize, 577, 1151] {
        let n = 1152 * 6 + extra;
        let src = tone_n(sr, 700.0, 0.5, n, 2);
        let src = &src[..];
        let data = encode_cbr(sr, 2, 192, src, false);
        let path = std::env::temp_dir().join(format!(
            "cadence_mp3_gapless_{}_{extra}.mp3",
            std::process::id()
        ));
        std::fs::write(&path, &data).unwrap();
        let oracle = decode_with_ffmpeg(&path, sr, 2).expect("ffmpeg decode");
        let _ = std::fs::remove_file(&path);
        assert_eq!(oracle.len(), src.len(), "FFmpeg length for extra={extra}");
        let snr = snr_db(src, &oracle);
        assert!(snr > 30.0, "extra={extra}: FFmpeg-decoded SNR {snr:.1} dB");
    }
}
