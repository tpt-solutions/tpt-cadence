//! SILK encode/decode round-trip throughput benchmark. Mirrors
//! `celt_round_trip`: the encoder produces a real SILK payload for a 20 ms
//! frame and the decoder consumes it through the same range-decoder entry
//! point the top-level Opus decoder drives.

use std::hint::black_box;

use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use tpt_av_cadence_opus::range::RangeDecoder;
use tpt_av_cadence_opus::silk::decoder::{DecControl, LostFlag, SilkDecoder};
use tpt_av_cadence_opus::silk::encoder::SilkEncoder;

const INTERNAL_RATE: i32 = 16_000;
const PACKET_MS: i32 = 20;
/// One 20 ms frame at the internal rate.
const FRAME_LEN: usize = (INTERNAL_RATE / 50) as usize;

fn make_tone() -> [i16; FRAME_LEN] {
    let mut pcm = [0i16; FRAME_LEN];
    for (i, s) in pcm.iter_mut().enumerate() {
        let t = i as f32 / INTERNAL_RATE as f32;
        let v = 0.3 * (t * 220.0 * std::f32::consts::TAU).sin()
            + 0.15 * (t * 550.0 * std::f32::consts::TAU).sin();
        *s = (v * 32767.0) as i16;
    }
    pcm
}

fn bench_encode(c: &mut Criterion) {
    let pcm = make_tone();
    let mut group = c.benchmark_group("silk_encode");
    group.throughput(Throughput::Elements(FRAME_LEN as u64));
    group.bench_function("mono_16khz_20ms", |b| {
        b.iter(|| {
            let mut enc = SilkEncoder::new(INTERNAL_RATE, INTERNAL_RATE, PACKET_MS).expect("open");
            enc.set_bitrate(24_000);
            black_box(enc.encode_frame(&pcm).expect("encode"))
        })
    });
    group.finish();
}

fn bench_decode(c: &mut Criterion) {
    let mut enc = SilkEncoder::new(INTERNAL_RATE, INTERNAL_RATE, PACKET_MS).expect("open");
    enc.set_bitrate(24_000);
    let payload = enc.encode_frame(&make_tone()).expect("encode");

    let mut group = c.benchmark_group("silk_decode");
    group.throughput(Throughput::Elements(FRAME_LEN as u64));
    group.bench_function("mono_16khz_20ms", |b| {
        b.iter(|| {
            let mut dec = SilkDecoder::new(1).expect("open decoder");
            let mut out = vec![0i16; FRAME_LEN];
            let mut rng = RangeDecoder::new(&payload);
            let mut ctrl = DecControl {
                n_channels_api: 1,
                n_channels_internal: 1,
                api_sample_rate: INTERNAL_RATE,
                internal_sample_rate: INTERNAL_RATE,
                payload_size_ms: PACKET_MS,
                prev_pitch_lag: 0,
            };
            black_box(
                dec.decode(&mut ctrl, Some(&mut rng), LostFlag::Normal, true, &mut out)
                    .expect("decode"),
            )
        })
    });
    group.finish();
}

criterion_group!(benches, bench_encode, bench_decode);
criterion_main!(benches);
