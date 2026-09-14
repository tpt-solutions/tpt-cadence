//! FFmpeg cross-check for the WAV decoder.
//!
//! Generates a deterministic 16-bit stereo WAV, decodes it with both the
//! suite's decoder and FFmpeg, and asserts bit-exact equality (tolerance 0).
//! Skips automatically when FFmpeg is not installed; CI runs the comparison
//! against a pinned FFmpeg.

use std::io::{Cursor, Write};

use tpt_av_cadence_core::FormatReader;
use tpt_av_cadence_test_utils::reference::{assert_bit_exact_vs_ffmpeg, ffmpeg_available};
use tpt_av_cadence_wav::WavReader;

/// Deterministic LCG so the signal is pseudo-random but reproducible.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 33
    }
}

#[test]
fn wav_s16_stereo_is_bit_exact_vs_ffmpeg() {
    if !ffmpeg_available() {
        eprintln!("skipping: ffmpeg not installed");
        return;
    }

    const RATE: u32 = 44_100;
    const FRAMES: usize = 16_384;
    let mut rng = Lcg(0x5EED_1234);
    let mut pcm = Vec::with_capacity(FRAMES * 2 * 2);
    for i in 0..FRAMES {
        let t = i as f32 / RATE as f32;
        let l = (t * 440.0 * std::f32::consts::TAU).sin() * 0.6
            + ((rng.next() % 2001) as f32 - 1000.0) / 32768.0;
        let r = (t * 554.37 * std::f32::consts::TAU).sin() * 0.5
            + ((rng.next() % 2001) as f32 - 1000.0) / 32768.0;
        pcm.extend_from_slice(&((l.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes());
        pcm.extend_from_slice(&((r.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes());
    }

    let mut wav = Vec::new();
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&((36 + pcm.len()) as u32).to_le_bytes());
    wav.extend_from_slice(b"WAVE");
    // fmt chunk
    wav.extend_from_slice(b"fmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
    wav.extend_from_slice(&2u16.to_le_bytes()); // stereo
    wav.extend_from_slice(&RATE.to_le_bytes());
    wav.extend_from_slice(&(RATE * 4).to_le_bytes()); // byte rate
    wav.extend_from_slice(&4u16.to_le_bytes()); // block align
    wav.extend_from_slice(&16u16.to_le_bytes()); // bits
                                                 // data chunk
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&(pcm.len() as u32).to_le_bytes());
    wav.extend_from_slice(&pcm);

    let path = std::env::temp_dir().join("tpt-cadence-wav-conformance.wav");
    {
        let mut f = std::fs::File::create(&path).expect("create temp wav");
        f.write_all(&wav).expect("write temp wav");
    }

    let mut reader = WavReader::open(Box::new(Cursor::new(wav)) as Box<dyn std::io::Read + Send>)
        .expect("open wav");
    let result = assert_bit_exact_vs_ffmpeg(&path, reader.decoder(), 0.0);
    let _ = std::fs::remove_file(&path);
    result.expect("WAV decode must be bit-exact vs FFmpeg");
}
