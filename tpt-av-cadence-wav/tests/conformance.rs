//! Bit-exact conformance tests for the WAV decoder.
//!
//! WAV is an uncompressed container, so reference vectors are generated
//! programmatically: each test builds a canonical WAVE byte stream with
//! known sample data and asserts the decoder reproduces the expected `f32`
//! values exactly (integer scaling by powers of two is exact in IEEE-754).

use std::io::Cursor;

use proptest::prelude::*;
use tpt_av_cadence_core::{CadenceError, Decoder, FormatReader};
use tpt_av_cadence_wav::{WavDecoder, WavReader};

// ---------------------------------------------------------------------------
// WAVE byte-stream construction helpers
// ---------------------------------------------------------------------------

/// Serializes one RIFF chunk, including the pad byte after odd payloads.
fn chunk(id: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + payload.len() + 1);
    out.extend_from_slice(id);
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(payload);
    if payload.len() % 2 == 1 {
        out.push(0);
    }
    out
}

/// Builds a classic 16-byte `fmt ` payload.
fn fmt_payload(tag: u16, channels: u16, rate: u32, bits: u16) -> Vec<u8> {
    let block_align = channels * bits / 8;
    let mut p = Vec::new();
    p.extend_from_slice(&tag.to_le_bytes());
    p.extend_from_slice(&channels.to_le_bytes());
    p.extend_from_slice(&rate.to_le_bytes());
    p.extend_from_slice(&(rate * block_align as u32).to_le_bytes());
    p.extend_from_slice(&block_align.to_le_bytes());
    p.extend_from_slice(&bits.to_le_bytes());
    p
}

/// Builds a 40-byte `WAVE_FORMAT_EXTENSIBLE` fmt payload.
fn fmt_extensible_payload(
    sub_tag: u16,
    channels: u16,
    rate: u32,
    bits: u16,
    channel_mask: u32,
) -> Vec<u8> {
    let mut p = fmt_payload(0xFFFE, channels, rate, bits);
    p.extend_from_slice(&22u16.to_le_bytes()); // cbSize
    p.extend_from_slice(&bits.to_le_bytes()); // valid bits per sample
    p.extend_from_slice(&channel_mask.to_le_bytes());
    let mut guid = [0u8; 16];
    guid[0] = (sub_tag & 0xFF) as u8;
    guid[1] = (sub_tag >> 8) as u8;
    guid[2..].copy_from_slice(&[
        0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B, 0x71,
    ]);
    p.extend_from_slice(&guid);
    p
}

/// Assembles a complete RIFF/WAVE file.
fn build_wav(fmt: Vec<u8>, data: &[u8]) -> Vec<u8> {
    let riff_size = 4 + (8 + fmt.len()) + (8 + data.len());
    let mut out = b"RIFF".to_vec();
    out.extend_from_slice(&(riff_size as u32).to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(&chunk(b"fmt ", &fmt));
    out.extend_from_slice(&chunk(b"data", data));
    out
}

fn open(data: Vec<u8>) -> Result<WavDecoder, CadenceError> {
    WavDecoder::from_source(Box::new(Cursor::new(data)))
}

fn decode_all(dec: &mut WavDecoder) -> Vec<f32> {
    let channels = dec.info().channels as usize;
    let mut out = Vec::new();
    let mut buf = vec![0.0f32; 1024 * channels];
    loop {
        let frames = dec.decode(&mut buf).expect("decode must succeed");
        if frames == 0 {
            break;
        }
        out.extend_from_slice(&buf[..frames * channels]);
    }
    out
}

// ---------------------------------------------------------------------------
// Integer PCM decode
// ---------------------------------------------------------------------------

#[test]
fn decode_i8_mono() {
    // WAV 8-bit is unsigned: 0 -> -1.0, 128 -> 0.0, 255 -> 127/128.
    let samples: [u8; 5] = [0, 64, 128, 200, 255];
    let wav = build_wav(fmt_payload(1, 1, 8_000, 8), &samples);
    let mut dec = open(wav).unwrap();
    assert_eq!(dec.info().bit_depth, 8);
    assert_eq!(dec.info().total_frames, Some(5));
    let got = decode_all(&mut dec);
    let want = [-1.0, -0.5, 0.0, (200 - 128) as f32 / 128.0, 127.0 / 128.0];
    assert_eq!(got.len(), 5);
    assert_eq!(got, want);
}

#[test]
fn decode_i16_stereo() {
    // Frame 0: L=1000 R=-1000; frame 1: L=-32768 R=32767.
    let mut data = Vec::new();
    for v in [1000i16, -1000, -32768, 32767] {
        data.extend_from_slice(&v.to_le_bytes());
    }
    let wav = build_wav(fmt_payload(1, 2, 44_100, 16), &data);
    let mut dec = open(wav).unwrap();
    assert_eq!(dec.info().sample_rate, 44_100);
    assert_eq!(dec.info().channels, 2);
    assert_eq!(dec.info().total_frames, Some(2));
    let got = decode_all(&mut dec);
    let want = [1000.0 / 32768.0, -1000.0 / 32768.0, -1.0, 32767.0 / 32768.0];
    assert_eq!(got, want);
}

#[test]
fn decode_i24_stereo() {
    // L: 0x000000 (0.0), 0x800000 (-1.0); R: 0x7FFFFF (max), 0xFFFFFF (min LSB).
    let data = [
        0x00, 0x00, 0x00, 0xFF, 0xFF, 0xFF, 0x00, 0x00, 0x80, 0xFF, 0xFF, 0x7F,
    ];
    let wav = build_wav(fmt_payload(1, 2, 48_000, 24), &data);
    let mut dec = open(wav).unwrap();
    assert_eq!(dec.info().bit_depth, 24);
    let got = decode_all(&mut dec);
    let want = [0.0, -1.0 / 8388608.0, -1.0, (8388607f32) / 8388608.0];
    assert_eq!(got, want);
}

#[test]
fn decode_i32_mono() {
    let mut data = Vec::new();
    for v in [0i32, 1 << 30, -(1 << 30), i32::MIN, i32::MAX] {
        data.extend_from_slice(&v.to_le_bytes());
    }
    let wav = build_wav(fmt_payload(1, 1, 48_000, 32), &data);
    let mut dec = open(wav).unwrap();
    let got = decode_all(&mut dec);
    let want = [
        0.0,
        0.5,
        -0.5,
        -1.0,
        (i32::MAX as f64 / 2147483648.0) as f32,
    ];
    assert_eq!(got, want);
}

#[test]
fn decode_quad_channels() {
    let mut data = Vec::new();
    for i in 0..7i16 {
        for ch in 0..4 {
            data.extend_from_slice(&(i * 10 + ch).to_le_bytes());
        }
    }
    let wav = build_wav(fmt_payload(1, 4, 48_000, 16), &data);
    let mut dec = open(wav).unwrap();
    assert_eq!(dec.info().channel_layout.channels(), 4);
    let got = decode_all(&mut dec);
    assert_eq!(got.len(), 28);
    assert_eq!(got[0], 0.0);
    assert_eq!(got[3], 3.0 / 32768.0);
    assert_eq!(got[27], (6 * 10 + 3) as f32 / 32768.0);
}

// ---------------------------------------------------------------------------
// IEEE float decode
// ---------------------------------------------------------------------------

#[test]
fn decode_f32_mono() {
    let mut data = Vec::new();
    for v in [-1.0f32, -0.25, 0.0, 0.5, 1.0] {
        data.extend_from_slice(&v.to_le_bytes());
    }
    let wav = build_wav(fmt_payload(3, 1, 48_000, 32), &data);
    let mut dec = open(wav).unwrap();
    let got = decode_all(&mut dec);
    assert_eq!(got, vec![-1.0, -0.25, 0.0, 0.5, 1.0]);
}

#[test]
fn decode_f64_stereo() {
    let mut data = Vec::new();
    for v in [-1.0f64, 0.5, 0.25, -0.125] {
        data.extend_from_slice(&v.to_le_bytes());
    }
    let wav = build_wav(fmt_payload(3, 2, 96_000, 64), &data);
    let mut dec = open(wav).unwrap();
    assert_eq!(dec.info().bit_depth, 64);
    let got = decode_all(&mut dec);
    let want = [-1.0f32, 0.5, 0.25, -0.125];
    assert_eq!(got, want);
}

// ---------------------------------------------------------------------------
// WAVE_FORMAT_EXTENSIBLE
// ---------------------------------------------------------------------------

#[test]
fn extensible_pcm_16bit_stereo() {
    let mut data = Vec::new();
    for v in [1000i16, -2000, 3000, -4000] {
        data.extend_from_slice(&v.to_le_bytes());
    }
    let wav = build_wav(fmt_extensible_payload(1, 2, 48_000, 16, 0x3), &data);
    let mut dec = open(wav).unwrap();
    let got = decode_all(&mut dec);
    let want = [
        1000.0 / 32768.0,
        -2000.0 / 32768.0,
        3000.0 / 32768.0,
        -4000.0 / 32768.0,
    ];
    assert_eq!(got, want);
}

#[test]
fn extensible_float_32bit_mono() {
    let mut data = Vec::new();
    for v in [0.5f32, -0.5] {
        data.extend_from_slice(&v.to_le_bytes());
    }
    let wav = build_wav(fmt_extensible_payload(3, 1, 44_100, 32, 0x4), &data);
    let mut dec = open(wav).unwrap();
    assert_eq!(decode_all(&mut dec), vec![0.5, -0.5]);
}

#[test]
fn extensible_with_nonstandard_guid_is_rejected() {
    let mut p = fmt_payload(0xFFFE, 1, 48_000, 16);
    p.extend_from_slice(&22u16.to_le_bytes());
    p.extend_from_slice(&16u16.to_le_bytes());
    p.extend_from_slice(&0u32.to_le_bytes());
    p.extend_from_slice(&[
        0xDE, 0xAD, 0xBE, 0xEF, 0, 0, 0x10, 0, 0x80, 0, 0, 0xAA, 0, 0x38, 0x9B, 0x71,
    ]);
    let wav = build_wav(p, &[0u8; 4]);
    assert!(open(wav).is_err());
}

#[test]
fn fmt_chunk_with_extension_bytes_is_tolerated() {
    // Classic fmt + 2 extra bytes (cbSize = 0): size 18.
    let mut p = fmt_payload(1, 1, 8_000, 8);
    p.extend_from_slice(&0u16.to_le_bytes());
    let wav = build_wav(p, &[128u8]);
    let mut dec = open(wav).unwrap();
    assert_eq!(decode_all(&mut dec), vec![0.0]);
}

// ---------------------------------------------------------------------------
// Chunk-level robustness
// ---------------------------------------------------------------------------

#[test]
fn odd_sized_chunks_and_trailing_chunks_are_skipped() {
    let mut data = Vec::new();
    for v in [100i16, -100] {
        data.extend_from_slice(&v.to_le_bytes());
    }
    let mut out = b"RIFF".to_vec();

    let list_payload: &[u8] = b"INFOISFTtpt-cadence"; // 19 bytes -> odd, needs pad
    let junk_after: &[u8] = b"JUNX";
    let riff_size =
        4 + (8 + 16) + (8 + list_payload.len() + 1) + (8 + data.len()) + (8 + junk_after.len());
    out.extend_from_slice(&(riff_size as u32).to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(&chunk(b"fmt ", &fmt_payload(1, 1, 8_000, 16)));
    out.extend_from_slice(&chunk(b"LIST", list_payload));
    out.extend_from_slice(&chunk(b"data", &data));
    out.extend_from_slice(&chunk(b"JUNK", junk_after));

    let mut dec = WavDecoder::from_source(Box::new(Cursor::new(out))).unwrap();
    assert_eq!(
        decode_all(&mut dec),
        vec![100.0 / 32768.0, -100.0 / 32768.0]
    );
}

#[test]
fn fact_chunk_before_data_is_skipped() {
    let mut data = Vec::new();
    data.extend_from_slice(&0.25f32.to_le_bytes());
    let mut out = b"RIFF".to_vec();
    let fact: &[u8] = &[4, 0, 0, 0];
    let riff_size = 4 + (8 + 16) + (8 + fact.len()) + (8 + data.len());
    out.extend_from_slice(&(riff_size as u32).to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(&chunk(b"fmt ", &fmt_payload(3, 1, 48_000, 32)));
    out.extend_from_slice(&chunk(b"fact", fact));
    out.extend_from_slice(&chunk(b"data", &data));
    let mut dec = WavDecoder::from_source(Box::new(Cursor::new(out))).unwrap();
    assert_eq!(decode_all(&mut dec), vec![0.25]);
}

#[test]
fn streaming_data_length_sentinel_decodes_to_eof() {
    let mut data = Vec::new();
    for v in [5i16, 6, 7] {
        data.extend_from_slice(&v.to_le_bytes());
    }
    // Hand-build with data size 0xFFFFFFFF (unknown/streaming).
    let mut out = b"RIFF".to_vec();
    out.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(&chunk(b"fmt ", &fmt_payload(1, 1, 8_000, 16)));
    out.extend_from_slice(b"data");
    out.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
    out.extend_from_slice(&data);

    let mut dec = WavDecoder::from_source(Box::new(Cursor::new(out))).unwrap();
    assert_eq!(dec.info().total_frames, None);
    assert_eq!(
        decode_all(&mut dec),
        vec![5.0 / 32768.0, 6.0 / 32768.0, 7.0 / 32768.0]
    );
}

#[test]
fn truncated_data_drops_partial_frame() {
    // 16-bit stereo needs 4 bytes per frame; only 7 available -> 1 whole frame.
    let data = [0x01u8, 0x00, 0x02, 0x00, 0x03, 0x00, 0x04];
    let wav = build_wav(fmt_payload(1, 2, 8_000, 16), &data);
    let mut dec = open(wav).unwrap();
    assert_eq!(dec.info().total_frames, Some(1));
    let got = decode_all(&mut dec);
    assert_eq!(got, vec![1.0 / 32768.0, 2.0 / 32768.0]);
}

#[test]
fn empty_data_chunk_decodes_zero_frames() {
    let wav = build_wav(fmt_payload(1, 1, 8_000, 16), &[]);
    let mut dec = open(wav).unwrap();
    let mut buf = [0.0f32; 16];
    assert_eq!(dec.decode(&mut buf).unwrap(), 0);
    assert_eq!(dec.decode(&mut buf).unwrap(), 0);
}

#[test]
fn wrong_riff_size_is_tolerated() {
    let mut data = Vec::new();
    data.extend_from_slice(&64i16.to_le_bytes());
    let mut out = b"RIFF".to_vec();
    out.extend_from_slice(&0u32.to_le_bytes()); // bogus size
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(&chunk(b"fmt ", &fmt_payload(1, 1, 8_000, 16)));
    out.extend_from_slice(&chunk(b"data", &data));
    let mut dec = WavDecoder::from_source(Box::new(Cursor::new(out))).unwrap();
    assert_eq!(decode_all(&mut dec), vec![64.0 / 32768.0]);
}

// ---------------------------------------------------------------------------
// Rejections
// ---------------------------------------------------------------------------

#[test]
fn data_before_fmt_is_rejected() {
    let mut out = b"RIFF".to_vec();
    let payload = [1u8, 0, 2, 0];
    out.extend_from_slice(&(4 + 8 + payload.len() as u32).to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(&chunk(b"data", &payload));
    out.extend_from_slice(&chunk(b"fmt ", &fmt_payload(1, 1, 8_000, 16)));
    assert!(open(out).is_err());
}

#[test]
fn rifx_is_rejected() {
    let wav = build_wav(fmt_payload(1, 1, 8_000, 16), &[1, 0]);
    let mut rifx = wav;
    rifx[0..4].copy_from_slice(b"RIFX");
    let err = open(rifx).unwrap_err();
    assert!(matches!(err, CadenceError::UnsupportedFeature(_)));
}

#[test]
fn non_riff_input_is_rejected() {
    let err = open(b"this is not a wav file at all".to_vec()).unwrap_err();
    assert!(matches!(err, CadenceError::InvalidFormat(_)));
}

#[test]
fn zero_channels_are_rejected() {
    // Regression: a zero-channel fmt must be rejected before the block-align
    // division (found by the mutation property test in the AIFF crate).
    let wav = build_wav(fmt_payload(1, 0, 8_000, 16), &[0u8; 4]);
    let err = open(wav).unwrap_err();
    assert!(matches!(err, CadenceError::CorruptData(_)));
}

#[test]
fn unsupported_bit_depth_is_rejected() {
    let wav = build_wav(fmt_payload(1, 1, 8_000, 12), &[0, 0, 0]);
    let err = open(wav).unwrap_err();
    assert!(matches!(err, CadenceError::UnsupportedFeature(_)));
}

#[test]
fn unsupported_codec_tag_is_rejected() {
    // 0x0011 = IMA ADPCM.
    let wav = build_wav(fmt_payload(0x0011, 1, 8_000, 4), &[0, 0]);
    let err = open(wav).unwrap_err();
    assert!(matches!(err, CadenceError::UnsupportedFeature(_)));
}

// ---------------------------------------------------------------------------
// Stream info, buffering, and seeking
// ---------------------------------------------------------------------------

fn sample_file_100_frames() -> Vec<u8> {
    let mut data = Vec::new();
    for i in 0..100i16 {
        data.extend_from_slice(&i.to_le_bytes());
    }
    build_wav(fmt_payload(1, 1, 8_000, 16), &data)
}

#[test]
fn buffer_not_multiple_of_channels_is_rejected() {
    let mut dec = open(build_wav(fmt_payload(1, 2, 8_000, 16), &[0u8; 8])).unwrap();
    let mut buf = [0.0f32; 3];
    assert!(dec.decode(&mut buf).is_err());
}

#[test]
fn seek_roundtrip_and_midstream() {
    let mut dec = open(sample_file_100_frames()).unwrap();
    let all = decode_all(&mut dec);
    assert_eq!(all.len(), 100);

    // Back to start reproduces identical output.
    dec.seek(0).unwrap();
    assert_eq!(decode_all(&mut dec), all);

    // Mid-stream seek lands on the exact frame.
    dec.seek(50).unwrap();
    let mut buf = [0.0f32; 5];
    assert_eq!(dec.decode(&mut buf).unwrap(), 5);
    assert_eq!(buf[0], 50.0 / 32768.0);
    assert_eq!(buf[4], 54.0 / 32768.0);

    // Seeking to exactly the end is legal and yields silence/EOF.
    dec.seek(100).unwrap();
    let mut buf = [0.0f32; 4];
    assert_eq!(dec.decode(&mut buf).unwrap(), 0);

    // Past the end is an error.
    assert!(matches!(
        dec.seek(101),
        Err(CadenceError::SeekOutOfRange {
            requested: 101,
            total: 100
        })
    ));
}

#[test]
fn unseekable_source_decode_works_and_seek_is_unsupported() {
    let data = sample_file_100_frames();
    let mut reader =
        WavReader::open(Box::new(Cursor::new(data)) as Box<dyn std::io::Read + Send>).unwrap();
    assert_eq!(reader.info().total_frames, Some(100));
    let mut buf = [0.0f32; 4];
    assert_eq!(reader.decoder().decode(&mut buf).unwrap(), 4);
    assert_eq!(buf[0], 0.0);
    let err = reader.decoder().seek(10).unwrap_err();
    assert!(matches!(err, CadenceError::UnsupportedFeature(_)));
}

#[test]
fn eof_decode_is_stable_across_calls() {
    let mut dec = open(sample_file_100_frames()).unwrap();
    let mut buf = [0.0f32; 128];
    let mut total = 0;
    loop {
        let frames = dec.decode(&mut buf).unwrap();
        if frames == 0 {
            break;
        }
        total += frames;
    }
    assert_eq!(total, 100);
    // Repeated EOF calls stay at zero and do not error.
    for _ in 0..3 {
        assert_eq!(dec.decode(&mut buf).unwrap(), 0);
    }
}

// ---------------------------------------------------------------------------
// Never-panic property tests (proptest + deterministic mutations)
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn arbitrary_bytes_never_panic(data in proptest::collection::vec(any::<u8>(), 0..4096)) {
        if let Ok(mut dec) = WavDecoder::from_source(Box::new(Cursor::new(data))) {
            let mut buf = vec![0.0f32; 997]; // odd size exercises channel checks
            for _ in 0..16 {
                if dec.decode(&mut buf).unwrap_or(0) == 0 { break; }
            }
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn mutated_valid_file_never_panic(seed in any::<u64>()) {
        // Mutate a *valid* file so the parser gets deep into chunk iteration.
        let base = sample_file_100_frames();
        let mut rng = tpt_av_cadence_test_utils::fuzz::Rng::new(seed);
        let mutated = tpt_av_cadence_test_utils::fuzz::mutate(&base, &mut rng);
        if let Ok(mut dec) = WavDecoder::from_source(Box::new(Cursor::new(mutated))) {
            let mut buf = [0.0f32; 512];
            for _ in 0..64 {
                if dec.decode(&mut buf).unwrap_or(0) == 0 { break; }
            }
        }
    }
}
