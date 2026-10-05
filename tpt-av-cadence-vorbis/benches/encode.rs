//! Encode throughput benchmark: Ogg Vorbis at a fixed quality scalar, over
//! the same harmonic signal the other encode benches use. Exercises the
//! full frame pipeline (window/MDCT, floor fit, residue quantization,
//! packetization into Ogg pages).

use std::hint::black_box;
use std::io::Cursor;

use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use tpt_av_cadence_core::Encoder;
use tpt_av_cadence_vorbis::VorbisEncoder;

const SAMPLE_RATE: u32 = 48_000;
const CHANNELS: u16 = 2;
const SECONDS: usize = 5;

/// Deterministic harmonic signal (sawtooth-ish stack of a fundamental and
/// two partials) — non-degenerate for the floor fit and residue coder,
/// unlike a pure sine.
fn make_signal() -> Vec<f32> {
    let frames = SAMPLE_RATE as usize * SECONDS;
    let mut samples = vec![0.0f32; frames * CHANNELS as usize];
    for (i, chunk) in samples.chunks_mut(CHANNELS as usize).enumerate() {
        let t = i as f32 / SAMPLE_RATE as f32;
        let s = 0.4 * (t * 440.0 * std::f32::consts::TAU).sin()
            + 0.2 * (t * 1108.73 * std::f32::consts::TAU).sin()
            + 0.1 * (t * 2637.02 * std::f32::consts::TAU).sin();
        let s = s * (0.8 + 0.2 * (t * 2.0 * std::f32::consts::PI).sin());
        for (c, out) in chunk.iter_mut().enumerate() {
            // Slight inter-channel decorrelation so channel coupling has work to do.
            *out = s * if c == 0 { 1.0 } else { 0.9 };
        }
    }
    samples
}

fn bench_encode(c: &mut Criterion) {
    let samples = make_signal();
    let frames = (samples.len() / CHANNELS as usize) as u64;

    let mut group = c.benchmark_group("vorbis_encode");
    group.throughput(Throughput::Elements(frames));
    group.bench_function("q5_48k_stereo_5s", |b| {
        b.iter(|| {
            let mut enc = VorbisEncoder::new(Cursor::new(Vec::new()), SAMPLE_RATE, CHANNELS, 5.0)
                .expect("open");
            enc.encode(black_box(&samples)).expect("encode");
            enc.finish().expect("finish");
        })
    });
    group.finish();
}

criterion_group!(benches, bench_encode);
criterion_main!(benches);
