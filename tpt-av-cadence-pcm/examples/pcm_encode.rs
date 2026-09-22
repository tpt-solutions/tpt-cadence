//! Writes a one-second, 440 Hz sine tone to a headerless 16-bit stereo PCM
//! file: `cargo run -p tpt-av-cadence-pcm --example pcm_encode -- tone.raw`

use std::f32::consts::PI;
use std::fs::File;

use tpt_av_cadence_core::{Encoder, SampleFormat};
use tpt_av_cadence_pcm::{ByteOrder, PcmEncoder, PcmFormat};

const SAMPLE_RATE: u32 = 44_100;
const CHANNELS: u16 = 2;
const FREQ_HZ: f32 = 440.0;

fn main() {
    let path = std::env::args()
        .nth(1)
        .expect("usage: pcm_encode <out.raw>");
    let file = File::create(&path).expect("create output");
    let format = PcmFormat {
        sample_format: SampleFormat::Int16,
        byte_order: ByteOrder::Little,
        channels: CHANNELS,
        sample_rate: SAMPLE_RATE,
    };
    let mut encoder = PcmEncoder::new(file, format).expect("open pcm encoder");

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
    eprintln!(
        "wrote {frames} frames (headerless: sample_rate={SAMPLE_RATE}, channels={CHANNELS}, \
         format=Int16LE) to {path}"
    );
}
