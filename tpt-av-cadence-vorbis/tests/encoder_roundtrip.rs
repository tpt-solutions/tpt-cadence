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

#[test]
#[ignore]
fn bitrate_sweep() {
    let input: Vec<f32> = match std::env::var("VORBIS_SWEEP_RAW") {
        Ok(p) => std::fs::read(p)
            .unwrap()
            .chunks_exact(4)
            .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            .collect(),
        Err(_) => signal(44100 * 4, 2, 44100.0),
    };
    let secs = input.len() as f64 / 2.0 / 44100.0;
    for q in [0.0f32, 2.0, 4.0, 6.0, 8.0, 10.0] {
        let data = encode(&input, 2, 44100, q);
        let out = decode(data.clone(), 2);
        eprintln!(
            "q{q}: {:.0} kbps, snr {:.1} dB",
            data.len() as f64 * 8.0 / secs / 1000.0,
            snr_db(&input, &out)
        );
    }
}

#[test]
fn short_and_odd_lengths_keep_exact_length() {
    for channels in [1usize, 2] {
        for n in [1usize, 100, 1023, 1024, 1025, 5000] {
            let input = signal(n, channels, 44100.0);
            let out = decode(encode(&input, channels as u16, 44100, 5.0), channels);
            assert_eq!(out.len(), input.len(), "{channels}ch n={n}");
        }
    }
}

#[test]
fn rejects_bad_configuration() {
    assert!(VorbisEncoder::new(Vec::new(), 44100, 3, 5.0).is_err());
    assert!(VorbisEncoder::new(Vec::new(), 0, 2, 5.0).is_err());
}

/// Quiet tone with a loud noise burst every `period` samples.
fn bursty(n: usize, period: usize, burst: usize) -> Vec<f32> {
    (0..n)
        .map(|i| {
            let t = i as f32 / 44100.0;
            let tone = 0.02 * (2.0 * std::f32::consts::PI * 300.0 * t).sin();
            let pos = i % period;
            if i >= period && pos < burst {
                // Decaying inharmonic partials: a struck-drum onset.
                let env = 1.0 - pos as f32 / burst as f32;
                let hit = (2.0 * std::f32::consts::PI * 1500.0 * t).sin()
                    + 0.8 * (2.0 * std::f32::consts::PI * 3170.0 * t).sin()
                    + 0.6 * (2.0 * std::f32::consts::PI * 5230.0 * t).sin();
                tone + 0.25 * env * hit
            } else {
                tone
            }
        })
        .collect()
}

fn encode_counting(input: &[f32], q: f32) -> (Vec<u8>, u64) {
    let mut buf = Cursor::new(Vec::new());
    let shorts;
    {
        let mut enc = VorbisEncoder::new(&mut buf, 44100, 1, q).unwrap();
        for chunk in input.chunks(999) {
            enc.encode(chunk).unwrap();
        }
        shorts = enc.short_block_count();
        enc.finish().unwrap();
    }
    (buf.into_inner(), shorts)
}

#[test]
fn transients_switch_to_short_blocks_and_round_trip() {
    let input = bursty(44100 * 2, 11_000, 1500);
    let (data, shorts) = encode_counting(&input, 6.0);
    assert!(shorts > 0, "no short blocks chosen for a burst train");
    let out = decode(data, 1);
    assert_eq!(out.len(), input.len());
    let snr = snr_db(&input, &out);
    assert!(snr > 15.0, "snr {snr}");
}

#[test]
fn stationary_signal_stays_long() {
    let input = signal(44100, 1, 44100.0);
    let (_, shorts) = encode_counting(&input, 6.0);
    assert_eq!(shorts, 0);
}

#[test]
fn short_blocks_limit_pre_echo() {
    // Silence, then a sudden loud burst: error energy in the 1024 samples
    // before the onset must stay far below the burst's energy.
    let mut input = vec![0.0f32; 44100];
    let onset = 20_000;
    let mut seed = 5u32;
    for s in input[onset..onset + 800].iter_mut() {
        seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
        *s = 0.8 * ((seed >> 16) as f32 / 32768.0 - 1.0);
    }
    let (data, shorts) = encode_counting(&input, 6.0);
    assert!(shorts > 0);
    let out = decode(data, 1);
    let e =
        |r: std::ops::Range<usize>| -> f64 { out[r].iter().map(|v| f64::from(*v).powi(2)).sum() };
    let burst: f64 = input[onset..onset + 800]
        .iter()
        .map(|v| f64::from(*v).powi(2))
        .sum();
    // Error energy just before the onset, and in the 512 samples that end
    // 400 before it (where a long block's spread noise would sit).
    let near = 10.0 * (e(onset - 400..onset) / burst).log10();
    let far = 10.0 * (e(onset - 912..onset - 400) / burst).log10();
    eprintln!("pre-echo: {near:.1} dB (near), {far:.1} dB (far) below burst");
    assert!(near < -15.0, "near pre-echo only {near:.1} dB below burst");
    assert!(far < -25.0, "far pre-echo only {far:.1} dB below burst");
}

#[test]
fn block_switching_stream_agrees_with_ffmpeg() {
    use tpt_av_cadence_test_utils::reference::{decode_with_ffmpeg, ffmpeg_available};
    if !ffmpeg_available() {
        if std::env::var_os("CADENCE_REQUIRE_FFMPEG").is_some() {
            panic!("FFmpeg is required but unavailable on PATH");
        }
        return;
    }
    let input = bursty(44100 * 3, 9_000, 1200);
    let (data, shorts) = encode_counting(&input, 6.0);
    assert!(shorts > 0);
    let ours = decode(data.clone(), 1);
    let path = std::env::temp_dir().join(format!("cadence_vorbis_bs_{}.ogg", std::process::id()));
    std::fs::write(&path, &data).unwrap();
    let oracle = decode_with_ffmpeg(&path, 44100, 1).expect("ffmpeg decode");
    let _ = std::fs::remove_file(&path);
    assert_eq!(ours.len(), oracle.len());
    let snr = snr_db(&oracle, &ours);
    eprintln!("block-switching inter-decoder {snr:.1} dB, {shorts} short blocks");
    assert!(snr > 90.0, "inter-decoder SNR {snr}");
}
