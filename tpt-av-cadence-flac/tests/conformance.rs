//! FLAC conformance tests.
//!
//! Two complementary sources of truth:
//!
//! 1. The official IETF FLAC decoder testbench vectors (CC0) bundled under
//!    `tests/data/` — real files produced by independent encoders. Decoded
//!    PCM is MD5-checked against each stream's embedded STREAMINFO digest.
//! 2. Streams produced by the small in-tree reference encoder
//!    (`mod encoder`), covering format axes the bundled files don't:
//!    mono, 4/8 channels, variable block sizes, forced Rice escapes,
//!    Rice2, 8/12/15/24/32-bit depths, wide UTF-8 frame numbers, wasted
//!    bits, and every stereo decorrelation mode.

mod encoder;

use std::io::Cursor;
use std::path::Path;

use proptest::prelude::*;
use tpt_av_cadence_core::{CadenceError, Decoder, FormatReader};
use tpt_av_cadence_flac::stream::StreamInfo as Si;
use tpt_av_cadence_flac::{FlacDecoder, FlacReader};
use tpt_av_cadence_test_utils::md5::Md5;

use encoder::{EncodeConfig, StereoModeEnc, SubframeKind};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn data_path(relative: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("data")
        .join(relative)
}

/// Decodes everything, returning the PCM and the MD5 of the raw integer
/// samples reconstructed from the `f32` output (exact for depths ≤ 24 bits,
/// which covers all bundled vectors).
fn decode_all(dec: &mut FlacDecoder) -> (Vec<f32>, [u8; 16], u64) {
    let bps = dec.streaminfo().bits_per_sample;
    let channels = dec.info().channels as usize;
    let scale = (1u64 << (bps - 1)) as f32;
    let bytes_per = (bps.div_ceil(8)) as usize;

    let mut md5 = Md5::new();
    let mut pcm = Vec::new();
    let mut frames_total = 0u64;
    let mut buf = vec![0.0f32; 8192 * channels];
    loop {
        let frames = dec
            .decode(&mut buf)
            .expect("decode must succeed on valid streams");
        if frames == 0 {
            break;
        }
        frames_total += frames as u64;
        for &s in &buf[..frames * channels] {
            let v = (s * scale) as i64;
            md5.update(&v.to_le_bytes()[..bytes_per]);
        }
        pcm.extend_from_slice(&buf[..frames * channels]);
    }
    (pcm, md5.finalize(), frames_total)
}

/// Full conformance check against an IETF vector: decode, then verify the
/// stream's embedded MD5 and total sample count.
fn verify_ietf_vector(relative: &str) {
    let bytes = std::fs::read(data_path(relative)).expect("bundled vector must exist");
    let mut dec = FlacDecoder::from_source(Box::new(Cursor::new(bytes))).expect("vector must open");
    let si: Si = dec.streaminfo().clone();
    let (pcm, md5, frames) = decode_all(&mut dec);
    eprintln!(
        "[dbg] {relative} first samples: {:?} md5 {:02X?}",
        &pcm[..6],
        md5
    );
    assert_eq!(frames, si.total_samples, "{relative}: frame count mismatch");
    assert_eq!(md5, si.md5, "{relative}: decoded PCM MD5 mismatch");
    assert_eq!(pcm.len() as u64, si.total_samples * si.channels as u64);
    // f32 output must be in range.
    assert!(pcm.iter().all(|&s| (-1.0..=1.0).contains(&s)));
}

/// Round-trip: encode PCM with the reference encoder, decode with the
/// decoder under test, and require bit-exact PCM plus matching MD5.
fn roundtrip(channels: &[Vec<i32>], bps: u16, rate: u32, block: usize, config: &EncodeConfig) {
    let flac = encoder::encode_stream(channels, bps, rate, block, config);
    let mut dec = FlacDecoder::from_source(Box::new(Cursor::new(flac))).expect("encode -> decode");
    assert_eq!(dec.info().channels as usize, channels.len());
    assert_eq!(dec.info().sample_rate, rate);
    let (pcm, md5, frames) = decode_all(&mut dec);

    // Expected f32 and MD5 straight from the source PCM.
    let scale = (1u64 << (bps - 1)) as f32;
    let bytes_per = (bps.div_ceil(8)) as usize;
    let mut expected_md5 = Md5::new();
    let mut expected_pcm = Vec::with_capacity(channels.len() * channels[0].len());
    for f in 0..channels[0].len() {
        for c in channels {
            let v = c[f] as i64;
            expected_md5.update(&v.to_le_bytes()[..bytes_per]);
            expected_pcm.push(v as f32 / scale);
        }
    }
    let expected_digest = expected_md5.finalize();
    assert_eq!(frames, channels[0].len() as u64, "frame count");
    assert_eq!(md5, expected_digest, "MD5 of decoded raw samples");
    assert_eq!(pcm, expected_pcm, "decoded f32 must be bit-exact");
}

/// Deterministic pseudo-audio with local correlation (so fixed/LPC
/// predictors have something to chew on) and amplitudes bounded to
/// `-(1 << (bits-1)) .. (1 << (bits-1)) - 1`.
fn signal(bits: u32, len: usize, seed: u64) -> Vec<i32> {
    let limit = 1i64 << (bits - 1).min(30);
    let mut state = seed | 1;
    let mut out = Vec::with_capacity(len);
    let mut drift = 0i64;
    for i in 0..len {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        drift += ((state >> 33) % 17) as i64 - 8; // random walk
        let slow = (i as i64 / 64) % 512 - 256; // slow ramp-ish component
        let v = drift + slow;
        out.push(v.clamp(-limit, limit - 1) as i32);
    }
    out
}

/// Like [`signal`] but with `w` guaranteed-zero low bits.
fn signal_with_wasted(bits: u32, wasted: u32, len: usize, seed: u64) -> Vec<i32> {
    signal(bits - wasted, len, seed)
        .iter()
        .map(|s| s << wasted)
        .collect()
}

// ---------------------------------------------------------------------------
// IETF FLAC decoder testbench vectors
// ---------------------------------------------------------------------------

#[test]
fn ietf_subset_01_blocksize_4096() {
    verify_ietf_vector("subset/01-blocksize-4096.flac");
}

#[test]
fn ietf_subset_14_wasted_bits() {
    verify_ietf_vector("subset/14-wasted-bits.flac");
}

#[test]
fn ietf_subset_16_partition_order_8_escaped() {
    verify_ietf_vector("subset/16-partition-order-8-escaped.flac");
}

#[test]
fn ietf_subset_17_all_fixed_orders() {
    verify_ietf_vector("subset/17-all-fixed-orders.flac");
}

#[test]
fn ietf_uncommon_07_15_bit_per_sample() {
    verify_ietf_vector("uncommon/07-15-bit-per-sample.flac");
}

#[test]
fn ietf_uncommon_09_rice_partition_order_15() {
    verify_ietf_vector("uncommon/09-rice-partition-order-15.flac");
}

#[test]
fn faulty_08_blocksize_65536_never_panics() {
    // Declares/uses an illegal 65536-sample block. The decoder must reject
    // the bad frames (and never panic); with no other frames present the
    // result is an empty decode rather than an error.
    let bytes = std::fs::read(data_path("faulty/08-blocksize-65536.flac")).unwrap();
    let mut dec = FlacDecoder::from_source(Box::new(Cursor::new(bytes))).expect("opens");
    let mut buf = vec![0.0f32; 8192 * dec.info().channels as usize];
    loop {
        let frames = dec.decode(&mut buf);
        match frames {
            Ok(0) => break,
            Ok(_) => continue,
            Err(_) => break, // rejected the illegal frame: acceptable
        }
    }
}

// ---------------------------------------------------------------------------
// Round trips through the reference encoder
// ---------------------------------------------------------------------------

#[test]
fn roundtrip_mono_16bit_fixed() {
    let pcm = vec![signal(16, 5000, 1)];
    roundtrip(
        &pcm,
        16,
        44_100,
        192,
        &EncodeConfig {
            subframe: SubframeKind::Fixed(2),
            ..Default::default()
        },
    );
}

#[test]
fn roundtrip_mono_16bit_verbatim() {
    let pcm = vec![signal(16, 1024, 2)];
    roundtrip(
        &pcm,
        16,
        44_100,
        1024,
        &EncodeConfig {
            subframe: SubframeKind::Verbatim,
            ..Default::default()
        },
    );
}

#[test]
fn roundtrip_constant() {
    let pcm = vec![vec![777i32; 512]];
    roundtrip(
        &pcm,
        16,
        48_000,
        512,
        &EncodeConfig {
            subframe: SubframeKind::Constant,
            ..Default::default()
        },
    );
}

#[test]
fn roundtrip_fixed_orders_0_to_4() {
    for order in 0..=4u32 {
        let pcm = vec![signal(16, 2048, 10 + order as u64)];
        roundtrip(
            &pcm,
            16,
            44_100,
            1024,
            &EncodeConfig {
                subframe: SubframeKind::Fixed(order),
                ..Default::default()
            },
        );
    }
}

#[test]
fn roundtrip_stereo_decorrelation_modes() {
    let l = signal(16, 4096, 21);
    let r = signal(16, 4096, 22);
    for mode in [
        None,
        Some(StereoModeEnc::Left),
        Some(StereoModeEnc::Right),
        Some(StereoModeEnc::Mid),
    ] {
        roundtrip(
            &[l.clone(), r.clone()],
            16,
            44_100,
            4096,
            &EncodeConfig {
                stereo_mode: mode,
                subframe: SubframeKind::Fixed(2),
                ..Default::default()
            },
        );
    }
}

#[test]
fn roundtrip_multi_channel() {
    let ch: Vec<Vec<i32>> = (0..4).map(|c| signal(16, 1024, 30 + c)).collect();
    roundtrip(&ch, 16, 48_000, 512, &EncodeConfig::default());
    let ch: Vec<Vec<i32>> = (0..8).map(|c| signal(16, 1024, 40 + c)).collect();
    roundtrip(&ch, 16, 48_000, 512, &EncodeConfig::default());
}

#[test]
fn roundtrip_bit_depths() {
    for bps in [8u16, 12, 16, 20, 24, 32] {
        let pcm = vec![signal(bps as u32, 1500, 50 + bps as u64)];
        roundtrip(
            &pcm,
            bps,
            44_100,
            750,
            &EncodeConfig {
                subframe: SubframeKind::Fixed(1),
                ..Default::default()
            },
        );
    }
    // 15-bit takes the "bit depth from STREAMINFO" header path.
    let pcm = vec![signal(15, 1500, 60)];
    roundtrip(
        &pcm,
        15,
        44_100,
        750,
        &EncodeConfig {
            subframe: SubframeKind::Fixed(1),
            ..Default::default()
        },
    );
}

#[test]
fn roundtrip_escape_partitions() {
    let pcm = vec![signal(16, 512, 70)];
    roundtrip(
        &pcm,
        16,
        44_100,
        64,
        &EncodeConfig {
            subframe: SubframeKind::Fixed(0),
            partition_order: 2,
            escape_partitions: true,
            ..Default::default()
        },
    );
}

#[test]
fn roundtrip_rice2_with_partitions() {
    let pcm = vec![signal(16, 4096, 80)];
    roundtrip(
        &pcm,
        16,
        44_100,
        1024,
        &EncodeConfig {
            subframe: SubframeKind::Fixed(2),
            partition_order: 3,
            rice2: true,
            ..Default::default()
        },
    );
}

#[test]
fn roundtrip_partition_order_8() {
    let pcm = vec![signal(16, 4096, 81)];
    roundtrip(
        &pcm,
        16,
        44_100,
        4096,
        &EncodeConfig {
            subframe: SubframeKind::Fixed(2),
            partition_order: 8,
            ..Default::default()
        },
    );
}

#[test]
fn roundtrip_variable_blocksize() {
    let pcm = vec![signal(16, 2000, 90), signal(16, 2000, 91)];
    roundtrip(
        &pcm,
        16,
        44_100,
        256,
        &EncodeConfig {
            subframe: SubframeKind::Fixed(2),
            variable_blocksize: true,
            ..Default::default()
        },
    );
}

#[test]
fn roundtrip_wasted_bits() {
    let pcm = vec![signal_with_wasted(20, 4, 1500, 100)];
    roundtrip(
        &pcm,
        20,
        44_100,
        750,
        &EncodeConfig {
            subframe: SubframeKind::Fixed(1),
            wasted_bits: 4,
            ..Default::default()
        },
    );
}

#[test]
fn roundtrip_lpc_orders() {
    for order in [2usize, 4, 8, 16, 32] {
        let coefs: Vec<i64> = (0..order)
            .map(|j| (((j as i64) % 7) - 3) * 64) // small mixed-sign coefficients
            .collect();
        let pcm = vec![signal(16, 3000, 110 + order as u64)];
        roundtrip(
            &pcm,
            16,
            44_100,
            1024,
            &EncodeConfig {
                subframe: SubframeKind::Lpc { coefs, shift: 12 },
                ..Default::default()
            },
        );
    }
}

#[test]
fn roundtrip_wide_utf8_frame_numbers() {
    // 48_000 frames of 16 samples: frame numbers reach 2999, exercising
    // 2-byte UTF-8-like coding; sample numbers (variable strategy) would
    // reach 767_999. Keep it mono/verbatim for speed.
    let pcm = vec![signal(16, 48_000, 120)];
    roundtrip(
        &pcm,
        16,
        44_100,
        16,
        &EncodeConfig {
            subframe: SubframeKind::Fixed(0),
            variable_blocksize: true,
            ..Default::default()
        },
    );
}

#[test]
fn roundtrip_nonstandard_sample_rate_header_path() {
    // 37123 Hz has no table code: takes the 8-bit rate field path.
    let pcm = vec![signal(16, 1000, 130)];
    roundtrip(&pcm, 16, 37_123, 500, &EncodeConfig::default());
}

// ---------------------------------------------------------------------------
// Seeking
// ---------------------------------------------------------------------------

#[test]
fn seek_roundtrip_and_midstream() {
    let pcm = vec![signal(16, 6000, 140)];
    let flac = encoder::encode_stream(
        &pcm,
        16,
        44_100,
        256,
        &EncodeConfig {
            subframe: SubframeKind::Fixed(2),
            ..Default::default()
        },
    );

    let mut dec = FlacDecoder::from_source(Box::new(Cursor::new(flac.clone()))).unwrap();
    let (full, _, _) = decode_all(&mut dec);
    assert_eq!(full.len(), 6000);

    // Restart produces identical output.
    dec.seek(0).unwrap();
    let (again, _, _) = decode_all(&mut dec);
    assert_eq!(again, full);

    // Mid-stream seek lands on the exact frame.
    dec.seek(4321).unwrap();
    let mut buf = vec![0.0f32; 100];
    assert_eq!(dec.decode(&mut buf).unwrap(), 100);
    assert_eq!(&buf[..], &full[4321..4421]);

    // Seek to the end yields EOF; past the end errors.
    dec.seek(6000).unwrap();
    assert_eq!(dec.decode(&mut buf).unwrap(), 0);
    assert!(matches!(
        dec.seek(6001),
        Err(CadenceError::SeekOutOfRange {
            requested: 6001,
            total: 6000
        })
    ));
}

#[test]
fn unseekable_source_decode_works_and_seek_is_unsupported() {
    let pcm = vec![signal(16, 2000, 150)];
    let flac = encoder::encode_stream(&pcm, 16, 44_100, 256, &EncodeConfig::default());
    let mut reader =
        FlacReader::open(Box::new(Cursor::new(flac)) as Box<dyn std::io::Read + Send>).unwrap();
    assert_eq!(reader.info().total_frames, Some(2000));
    let mut buf = vec![0.0f32; 256];
    assert_eq!(reader.decoder().decode(&mut buf).unwrap(), 256);
    assert!(matches!(
        reader.decoder().seek(10),
        Err(CadenceError::UnsupportedFeature(_))
    ));
}

// ---------------------------------------------------------------------------
// Never-panic property tests
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn arbitrary_bytes_never_panic(data in proptest::collection::vec(any::<u8>(), 0..2048)) {
        if let Ok(mut dec) = FlacDecoder::from_source(Box::new(Cursor::new(data))) {
            let mut buf = vec![0.0f32; 995];
            for _ in 0..16 {
                if dec.decode(&mut buf).unwrap_or(0) == 0 { break; }
            }
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn mutated_valid_stream_never_panic(seed in any::<u64>()) {
        let pcm = vec![signal(16, 512, seed & 0xFF)];
        let base = encoder::encode_stream(&pcm, 16, 44_100, 128, &EncodeConfig {
            subframe: SubframeKind::Fixed(2),
            partition_order: 2,
            ..Default::default()
        });
        let mut rng = tpt_av_cadence_test_utils::fuzz::Rng::new(seed ^ 0xBEEF);
        let mutated = tpt_av_cadence_test_utils::fuzz::mutate(&base, &mut rng);
        if let Ok(mut dec) = FlacDecoder::from_source(Box::new(Cursor::new(mutated))) {
            let mut buf = vec![0.0f32; 997];
            for _ in 0..64 {
                if dec.decode(&mut buf).unwrap_or(0) == 0 { break; }
            }
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn corrupted_real_vector_never_panic(seed in any::<u64>()) {
        // Mutate bytes deep inside a real frame body (past the metadata).
        let mut bytes = std::fs::read(data_path("subset/01-blocksize-4096.flac")).unwrap();
        let mut rng = tpt_av_cadence_test_utils::fuzz::Rng::new(seed);
        if bytes.len() > 8192 {
            let at = 8192 + rng.below(bytes.len() - 8192 - 1);
            bytes[at] ^= 1 << rng.below(8);
        }
        if let Ok(mut dec) = FlacDecoder::from_source(Box::new(Cursor::new(bytes))) {
            let mut buf = vec![0.0f32; 4096 * 2];
            for _ in 0..64 {
                if dec.decode(&mut buf).unwrap_or(0) == 0 { break; }
            }
        }
    }
}
