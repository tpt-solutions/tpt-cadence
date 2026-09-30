//! Writes a two-second stereo test signal (a 440 Hz / 660 Hz chord with a
//! slow tremolo) to an Ogg Opus file:
//! `cargo run -p tpt-av-cadence-opus --example opus_encode -- tone.opus [mode]`
//!
//! `mode` selects the Opus coding mode (default `celt`):
//! - `celt`   — CELT, 96 kbps CBR
//! - `vbr`    — CELT, 96 kbps average VBR
//! - `silk`   — SILK wideband (16 kHz internal), 32 kbps, 20 ms packets
//! - `hybrid` — hybrid SILK+CELT fullband, 20 + 60 kbps, 20 ms packets
//!
//! Opus input must be 48 kHz; the encoder handles 1 or 2 channels.

use std::f32::consts::PI;
use std::fs::File;

use tpt_av_cadence_core::Encoder;
use tpt_av_cadence_opus::OggOpusEncoder;

const SAMPLE_RATE: u32 = 48_000;
const CHANNELS: u16 = 2;

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args
        .next()
        .expect("usage: opus_encode <out.opus> [celt|vbr|silk|hybrid]");
    let mode = args.next().unwrap_or_else(|| "celt".to_string());

    let file = File::create(&path).expect("create output");
    let mut encoder = match mode.as_str() {
        "celt" => OggOpusEncoder::new(file, SAMPLE_RATE, CHANNELS, 96_000),
        "vbr" => OggOpusEncoder::new_vbr(file, SAMPLE_RATE, CHANNELS, 96_000),
        "silk" => OggOpusEncoder::new_silk(file, SAMPLE_RATE, CHANNELS, 32_000, 16_000, 20),
        "hybrid" => {
            OggOpusEncoder::new_hybrid(file, SAMPLE_RATE, CHANNELS, 20_000, 60_000, 20, true)
        }
        other => panic!("unknown mode {other:?}; expected celt, vbr, silk, or hybrid"),
    }
    .expect("open opus encoder");

    let frames = SAMPLE_RATE as usize * 2;
    let mut buf = vec![0.0f32; frames * CHANNELS as usize];
    for i in 0..frames {
        let t = i as f32 / SAMPLE_RATE as f32;
        let tremolo = 0.75 + 0.25 * (2.0 * PI * 3.0 * t).sin();
        buf[i * 2] = (2.0 * PI * 440.0 * t).sin() * 0.4 * tremolo;
        buf[i * 2 + 1] = (2.0 * PI * 660.0 * t).sin() * 0.4 * tremolo;
    }
    encoder.encode(&buf).expect("encode");
    Encoder::finish(&mut encoder).expect("finish");
    eprintln!("wrote {frames} frames to {path} ({mode})");
}
