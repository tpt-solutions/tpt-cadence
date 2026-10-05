//! Encode throughput benchmark: MP3 at a fixed CBR rate and on the VBR
//! quality ladder, over the same harmonic signal the other encode benches
//! use. Each iteration covers the whole frame pipeline (polyphase filter
//! bank, MDCT, psychoacoustic shot, bit allocation, reservoir).

use std::hint::black_box;
use std::io::Cursor;

use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use tpt_av_cadence_core::Encoder;
use tpt_av_cadence_mp3::Mp3Encoder;

const SAMPLE_RATE: u32 = 48_000;
const CHANNELS: u16 = 2;
const SECONDS: usize = 5;

/// Deterministic harmonic signal (sawtooth-ish stack of a fundamental and
/// two partials) — non-degenerate for the bit reservoir and allocation
/// search, unlike a pure sine.
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
            // Slight inter-channel decorrelation so joint stereo has work to do.
            *out = s * if c == 0 { 1.0 } else { 0.9 };
        }
    }
    samples
}

fn bench_encode(c: &mut Criterion) {
    let samples = make_signal();
    let frames = (samples.len() / CHANNELS as usize) as u64;

    let mut group = c.benchmark_group("mp3_encode");
    group.throughput(Throughput::Elements(frames));
    group.bench_function("cbr_128k_48k_stereo_5s", |b| {
        b.iter(|| {
            let mut enc =
                Mp3Encoder::new(Cursor::new(Vec::new()), SAMPLE_RATE, CHANNELS, 128).expect("open");
            enc.encode(black_box(&samples)).expect("encode");
            enc.finish().expect("finish");
        })
    });
    group.bench_function("vbr_q4_48k_stereo_5s", |b| {
        b.iter(|| {
            let mut enc = Mp3Encoder::new_vbr(Cursor::new(Vec::new()), SAMPLE_RATE, CHANNELS, 4)
                .expect("open");
            enc.encode(black_box(&samples)).expect("encode");
            enc.finish().expect("finish");
        })
    });
    group.finish();
}

criterion_group!(benches, bench_encode);
criterion_main!(benches);
