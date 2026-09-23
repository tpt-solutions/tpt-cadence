//! CELT encode/decode round-trip throughput benchmark. There's no bundled
//! encoded fixture for Opus (the official RFC 6716 vectors are pulled from
//! an env var, not checked in), so this exercises the real encoder and
//! decoder end to end on a synthetic tone instead.

use std::hint::black_box;

use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use tpt_av_cadence_opus::celt::decoder::CeltDecoder;
use tpt_av_cadence_opus::celt::encoder::CeltEncoder;
use tpt_av_cadence_opus::decoder::{decode_celt_only_packet, OUTPUT_CHANNELS};
use tpt_av_cadence_opus::packet::parse_packet;

const LM: usize = 3; // fullband, 20 ms at 48 kHz
const N2: usize = 960;
const BYTES_PER_FRAME: usize = 160; // ~64 kbps

fn make_tone(frame: usize) -> [f32; N2] {
    let mut pcm = [0.0f32; N2];
    let sample_rate = 48_000.0f32;
    let freq_hz = 440.0f32;
    let mut phase = frame as f32 * N2 as f32 * 2.0 * std::f32::consts::PI * freq_hz / sample_rate;
    for s in pcm.iter_mut() {
        *s = 0.5 * phase.sin();
        phase += 2.0 * std::f32::consts::PI * freq_hz / sample_rate;
    }
    pcm
}

fn bench_encode(c: &mut Criterion) {
    let pcm = make_tone(0);
    let mut group = c.benchmark_group("celt_encode");
    group.throughput(Throughput::Elements(N2 as u64));
    group.bench_function("mono_fullband_20ms", |b| {
        b.iter(|| {
            let mut enc = CeltEncoder::new(1, LM);
            black_box(enc.encode_frame(&pcm, BYTES_PER_FRAME))
        })
    });
    group.finish();
}

fn bench_decode(c: &mut Criterion) {
    let mut enc = CeltEncoder::new(1, LM);
    let pcm = make_tone(0);
    let packet_bytes = enc.encode_frame(&pcm, BYTES_PER_FRAME);
    let packet = parse_packet(&packet_bytes).expect("parse packet");

    let mut group = c.benchmark_group("celt_decode");
    group.throughput(Throughput::Elements(N2 as u64));
    group.bench_function("mono_fullband_20ms", |b| {
        b.iter(|| {
            let mut dec = CeltDecoder::new(2, 48_000).expect("open decoder");
            let mut pcm_out = vec![0.0f32; N2 * OUTPUT_CHANNELS];
            decode_celt_only_packet(&mut dec, &packet, &packet_bytes, &mut pcm_out)
                .expect("decode");
            black_box(pcm_out)
        })
    });
    group.finish();
}

criterion_group!(benches, bench_encode, bench_decode);
criterion_main!(benches);
