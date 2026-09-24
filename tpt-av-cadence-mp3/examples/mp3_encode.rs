//! Writes a one-second, 440 Hz sine tone to a 128 kbps stereo MPEG-1 Layer
//! III (MP3) file:
//! `cargo run -p tpt-av-cadence-mp3 --example mp3_encode -- tone.mp3`
//!
//! See `src/encoder.rs`'s module doc comment for this encoder's reduced
//! feature scope and known quality limitations. It produces valid, decodable
//! MP3 streams with active mono and independent-stereo fidelity gates, while
//! psychoacoustics, reservoir borrowing, short blocks, and stereo coupling
//! remain out of scope.

use std::f32::consts::PI;
use std::fs::File;

use tpt_av_cadence_core::Encoder;
use tpt_av_cadence_mp3::Mp3Encoder;

const SAMPLE_RATE: u32 = 44_100;
const CHANNELS: u16 = 2;
const BITRATE_KBPS: u32 = 128;
const FREQ_HZ: f32 = 440.0;

fn main() {
    let path = std::env::args()
        .nth(1)
        .expect("usage: mp3_encode <out.mp3>");
    let file = File::create(&path).expect("create output");
    let mut encoder =
        Mp3Encoder::new(file, SAMPLE_RATE, CHANNELS, BITRATE_KBPS).expect("open mp3 encoder");

    let frames = SAMPLE_RATE as usize;
    let mut buf = vec![0.0f32; frames * CHANNELS as usize];
    for i in 0..frames {
        let t = i as f32 / SAMPLE_RATE as f32;
        let s = (2.0 * PI * FREQ_HZ * t).sin() * 0.5;
        buf[i * 2] = s;
        buf[i * 2 + 1] = s;
    }
    encoder.encode(&buf).expect("encode");
    Encoder::finish(&mut encoder).expect("finish");
    eprintln!("wrote {frames} frames to {path}");
}
