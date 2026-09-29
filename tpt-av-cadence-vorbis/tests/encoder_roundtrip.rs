use std::io::Cursor;

use tpt_av_cadence_core::{Decoder, Encoder};
use tpt_av_cadence_vorbis::{VorbisDecoder, VorbisEncoder};

fn signal(n: usize, channels: usize, rate: f32) -> Vec<f32> {
    let mut out = Vec::with_capacity(n * channels);
    let mut seed = 12345u32;
    for i in 0..n {
        let t = i as f32 / rate;
        seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
        let noise = ((seed >> 16) as f32 / 32768.0 - 1.0) * 0.01;
        let tone = 0.3 * (2.0 * std::f32::consts::PI * 440.0 * t).sin()
            + 0.15 * (2.0 * std::f32::consts::PI * 2500.0 * t).sin();
        for c in 0..channels {
            out.push(if c == 0 {
                tone + noise
            } else {
                0.8 * tone - noise
            });
        }
    }
    out
}

fn encode(input: &[f32], channels: u16, rate: u32, q: f32) -> Vec<u8> {
    let mut buf = Cursor::new(Vec::new());
    {
        let mut enc = VorbisEncoder::new(&mut buf, rate, channels, q).unwrap();
        for chunk in input.chunks(1000 * channels as usize) {
            enc.encode(chunk).unwrap();
        }
        enc.finish().unwrap();
    }
    buf.into_inner()
}

fn decode(data: Vec<u8>, channels: usize) -> Vec<f32> {
    let mut dec = VorbisDecoder::open(Box::new(Cursor::new(data))).unwrap();
    let mut out = Vec::new();
    let mut tmp = vec![0.0f32; 4096 * channels];
    loop {
        match dec.decode(&mut tmp).unwrap() {
            0 => break,
            n => out.extend_from_slice(&tmp[..n * channels]),
        }
    }
    out
}

fn snr_db(a: &[f32], b: &[f32]) -> f64 {
    let (mut s, mut e) = (0.0f64, 0.0f64);
    for (&x, &y) in a.iter().zip(b) {
        s += f64::from(x).powi(2);
        e += (f64::from(x) - f64::from(y)).powi(2);
    }
    10.0 * (s / e.max(1e-30)).log10()
}

#[test]
fn round_trip_recovers_signal() {
    for channels in [1usize, 2] {
        let n = 44100;
        let input = signal(n, channels, 44100.0);
        let data = encode(&input, channels as u16, 44100, 10.0);
        let out = decode(data.clone(), channels);
        eprintln!(
            "ch={channels}: {} bytes, {} samples out of {}",
            data.len(),
            out.len() / channels,
            n
        );
        assert_eq!(out.len(), input.len(), "length");
        let rms = |x: &[f32]| {
            (x.iter().map(|v| f64::from(*v).powi(2)).sum::<f64>() / x.len() as f64).sqrt()
        };
        eprintln!("rms in {:.4} out {:.4}", rms(&input), rms(&out));
        let snr = snr_db(&input, &out);
        eprintln!("snr {snr:.2} dB");
        assert!(snr > 15.0, "snr {snr}");
    }
}

#[test]
fn ffmpeg_decodes_our_stream_like_we_do() {
    use tpt_av_cadence_test_utils::reference::{decode_with_ffmpeg, ffmpeg_available};
    if !ffmpeg_available() {
        if std::env::var_os("CADENCE_REQUIRE_FFMPEG").is_some() {
            panic!("FFmpeg is required but unavailable on PATH");
        }
        eprintln!("skipping: FFmpeg not on PATH");
        return;
    }
    for (channels, rate) in [(1usize, 44100u32), (2, 44100), (2, 48000)] {
        let input = signal(rate as usize, channels, rate as f32);
        let data = encode(&input, channels as u16, rate, 6.0);
        let ours = decode(data.clone(), channels);
        let path = std::env::temp_dir().join(format!(
            "cadence_vorbis_{}_{channels}_{rate}.ogg",
            std::process::id()
        ));
        std::fs::write(&path, &data).unwrap();
        let oracle = decode_with_ffmpeg(&path, rate, channels as u16).expect("ffmpeg decode");
        let _ = std::fs::remove_file(&path);
        assert_eq!(
            ours.len(),
            oracle.len(),
            "length vs ffmpeg ({channels}ch {rate})"
        );
        let snr = snr_db(&oracle, &ours);
        eprintln!(
            "{channels}ch {rate}: inter-decoder {snr:.1} dB, vs source {:.1} dB",
            snr_db(&input, &oracle)
        );
        assert!(snr > 90.0, "inter-decoder SNR {snr}");
    }
}
