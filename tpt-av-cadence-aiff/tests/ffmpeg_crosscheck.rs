//! Cross-checks the AIFF decoder against FFmpeg on synthetic big-endian
//! integer PCM. AIFF stores integer samples, so both decoders produce the
//! same normalized floats and the comparison is bit-exact (tolerance 0).
//!
//! FFmpeg is a subprocess only, found on PATH. Set CADENCE_REQUIRE_FFMPEG=1
//! to fail instead of skipping when the executable is unavailable.

use std::io::{Cursor, Write};

use tpt_av_cadence_aiff::ext_float::f64_to_extended;
use tpt_av_cadence_aiff::AiffDecoder;
use tpt_av_cadence_test_utils::reference::{assert_bit_exact_vs_ffmpeg, ConformanceError};

fn chunk(id: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + payload.len() + 1);
    out.extend_from_slice(id);
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(payload);
    if payload.len() % 2 == 1 {
        out.push(0);
    }
    out
}

fn comm_payload(channels: u16, frames: u32, bits: u16, rate: f64) -> Vec<u8> {
    let mut p = Vec::new();
    p.extend_from_slice(&channels.to_be_bytes());
    p.extend_from_slice(&frames.to_be_bytes());
    p.extend_from_slice(&bits.to_be_bytes());
    p.extend_from_slice(&f64_to_extended(rate));
    p
}

fn build_aiff(bits: u16, channels: u16, data: &[u8], rate: f64) -> Vec<u8> {
    let frames = (data.len() / (channels as usize * bits as usize / 8)) as u32;
    let mut body = Vec::new();
    let comm = chunk(b"COMM", &comm_payload(channels, frames, bits, rate));
    body.extend_from_slice(&comm);
    let mut ssnd = 0u32.to_be_bytes().to_vec();
    ssnd.extend_from_slice(&0u32.to_be_bytes());
    ssnd.extend_from_slice(data);
    body.extend_from_slice(&chunk(b"SSND", &ssnd));
    let payload_len = (4 + body.len()) as u32;
    let mut out = b"FORM".to_vec();
    out.extend_from_slice(&payload_len.to_be_bytes());
    out.extend_from_slice(b"AIFF");
    out.extend_from_slice(&body);
    out
}

/// Sine sweep as big-endian PCM of the given width.
fn sweep_samples(bits: u16, channels: u16, frames: usize, rate: f64) -> Vec<u8> {
    let max = (1i64 << (bits - 1)) - 1;
    let mut out = Vec::new();
    for i in 0..frames {
        let t = i as f64 / rate;
        let f = 200.0 + 600.0 * t;
        let v = (0.8 * (2.0 * std::f64::consts::PI * f * t).sin() * max as f64) as i64;
        for c in 0..channels {
            let sample = if c == 0 { v } else { -v };
            match bits {
                8 => out.push((sample + 128) as u8), // AIFF 8-bit is unsigned
                16 => out.extend_from_slice(&(sample as i16).to_be_bytes()),
                24 => {
                    let b = (sample as i32).to_be_bytes();
                    out.extend_from_slice(&b[1..4]);
                }
                32 => out.extend_from_slice(&(sample as i32).to_be_bytes()),
                _ => unreachable!("test covers 8/16/24/32 bit"),
            }
        }
    }
    out
}

fn crosscheck(bits: u16, channels: u16, rate: f64) -> Result<(), ConformanceError> {
    let frames = 2000;
    let data = sweep_samples(bits, channels, frames, rate);
    let aiff = build_aiff(bits, channels, &data, rate);

    let temp = std::env::temp_dir().join(format!(
        "aiff_xcheck_{}ch_{}bit_{}.aiff",
        channels,
        bits,
        std::process::id()
    ));
    {
        let mut f = std::fs::File::create(&temp)?;
        f.write_all(&aiff)?;
    }

    let result = {
        let file = std::fs::File::open(&temp)?;
        let mut decoder = AiffDecoder::from_source(Box::new(Cursor::new(std::fs::read(&temp)?)))?;
        let _ = file;
        assert_bit_exact_vs_ffmpeg(&temp, &mut decoder, 0.0)
    };
    let _ = std::fs::remove_file(&temp);
    result
}

#[test]
fn aiff_matches_ffmpeg_bit_exact() {
    let mut skipped = 0usize;
    let mut checked = 0usize;
    for &(bits, channels, rate) in &[
        (16u16, 1u16, 44_100.0f64),
        (16, 2, 44_100.0),
        (8, 1, 44_100.0),
        (24, 2, 44_100.0),
        (32, 2, 48_000.0),
    ] {
        match crosscheck(bits, channels, rate) {
            Ok(()) => checked += 1,
            Err(ConformanceError::ReferenceUnavailable) => skipped += 1,
            Err(e) => panic!("{bits}-bit {channels}ch {rate} Hz: {e}"),
        }
    }
    assert!(checked + skipped > 0, "no AIFF variants found to check");
    if skipped > 0 {
        assert!(
            std::env::var_os("CADENCE_REQUIRE_FFMPEG").is_none(),
            "FFmpeg is required but unavailable on PATH"
        );
        eprintln!("skipped {skipped} variants: FFmpeg not on PATH");
    }
}
