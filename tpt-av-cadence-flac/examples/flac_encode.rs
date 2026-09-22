//! Writes a one-second, 440 Hz sine tone to a 16-bit stereo FLAC file:
//! `cargo run -p tpt-av-cadence-flac --example flac_encode -- tone.flac`

use std::f32::consts::PI;
use std::fs::File;

use tpt_av_cadence_core::Encoder;
use tpt_av_cadence_flac::FlacEncoder;

const SAMPLE_RATE: u32 = 44_100;
const CHANNELS: u16 = 2;
const BITS_PER_SAMPLE: u16 = 16;
const FREQ_HZ: f32 = 440.0;

fn main() {
    let path = std::env::args()
        .nth(1)
        .expect("usage: flac_encode <out.flac>");
    let file = File::create(&path).expect("create output");
    let mut encoder =
        FlacEncoder::new(file, SAMPLE_RATE, CHANNELS, BITS_PER_SAMPLE).expect("open flac encoder");

    let frames = SAMPLE_RATE as usize;
    let mut buf = vec![0.0f32; frames * CHANNELS as usize];
    for i in 0..frames {
        let t = i as f32 / SAMPLE_RATE as f32;
        let s = (2.0 * PI * FREQ_HZ * t).sin() * 0.5;
        buf[i * 2] = s;
        buf[i * 2 + 1] = s;
    }
    encoder.encode(&buf).expect("encode");
    encoder.finish().expect("finish");
    eprintln!("wrote {frames} frames to {path}");
}
