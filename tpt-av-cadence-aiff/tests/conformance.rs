//! Bit-exact conformance tests for the AIFF/AIFC decoder.
//!
//! AIFF is an uncompressed container, so reference vectors are generated
//! programmatically: each test assembles canonical IFF byte streams with
//! known sample data and asserts the decoder reproduces the expected `f32`
//! values exactly.

use std::io::Cursor;

use proptest::prelude::*;
use tpt_av_cadence_aiff::ext_float::f64_to_extended;
use tpt_av_cadence_aiff::AiffDecoder;
use tpt_av_cadence_core::{CadenceError, Decoder, FormatReader};

// ---------------------------------------------------------------------------
// IFF byte-stream construction helpers
// ---------------------------------------------------------------------------

/// Serializes one IFF chunk, including the pad byte after odd payloads.
fn chunk(id: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + payload.len() + 1);
    out.extend_from_slice(id);
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(payload);
    if payload.len() % 2 == 1 {
        out.push(0);
    }
    out
}

/// Builds a `COMM` payload: base fields plus an optional AIFF-C compression
/// type and Pascal-string name.
fn comm_payload(
    channels: u16,
    frames: u32,
    bits: u16,
    rate: f64,
    aifc: Option<(&[u8; 4], &str)>,
) -> Vec<u8> {
    let mut p = Vec::new();
    p.extend_from_slice(&channels.to_be_bytes());
    p.extend_from_slice(&frames.to_be_bytes());
    p.extend_from_slice(&bits.to_be_bytes());
    p.extend_from_slice(&f64_to_extended(rate));
    if let Some((fourcc, name)) = aifc {
        p.extend_from_slice(fourcc);
        p.push(name.len() as u8);
        p.extend_from_slice(name.as_bytes());
        if (1 + name.len()) % 2 == 1 {
            p.push(0);
        }
    }
    p
}

/// Builds an `SSND` payload with a configurable leading data offset.
fn ssnd_payload(offset: u32, data: &[u8]) -> Vec<u8> {
    let mut p = Vec::new();
    p.extend_from_slice(&offset.to_be_bytes());
    p.extend_from_slice(&0u32.to_be_bytes()); // blockSize hint
    p.extend_from_slice(&vec![0u8; offset as usize]);
    p.extend_from_slice(data);
    p
}

/// Assembles a complete FORM AIFF/AIFC file.
fn build_aiff(form_type: &[u8], body: Vec<Vec<u8>>) -> Vec<u8> {
    let payload_len: usize = 4 + body.iter().map(|c| c.len()).sum::<usize>();
    let mut out = b"FORM".to_vec();
    out.extend_from_slice(&(payload_len as u32).to_be_bytes());
    out.extend_from_slice(form_type);
    for c in body {
        out.extend_from_slice(&c);
    }
    out
}

fn simple_aiff(bits: u16, channels: u16, data: &[u8], rate: f64) -> Vec<u8> {
    let frames = (data.len() / (channels as usize * bits as usize / 8)) as u32;
    build_aiff(
        b"AIFF",
        vec![
            chunk(b"COMM", &comm_payload(channels, frames, bits, rate, None)),
            chunk(b"SSND", &ssnd_payload(0, data)),
        ],
    )
}

fn open(data: Vec<u8>) -> Result<AiffDecoder, CadenceError> {
    AiffDecoder::from_source(Box::new(Cursor::new(data)))
}

fn decode_all(dec: &mut AiffDecoder) -> Vec<f32> {
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
// Classic AIFF integer decode (big-endian, signed)
// ---------------------------------------------------------------------------

#[test]
fn aiff_i8_mono_is_signed() {
    // Unlike WAV, AIFF 8-bit is signed: 0x80 -> -1.0, 0x00 -> 0.0, 0x7F -> max.
    let data: Vec<u8> = vec![0x80, 0x00, 0x7F];
    let mut dec = open(simple_aiff(8, 1, &data, 8_000.0)).unwrap();
    assert_eq!(dec.info().bit_depth, 8);
    assert_eq!(decode_all(&mut dec), vec![-1.0, 0.0, 127.0 / 128.0]);
}

#[test]
fn aiff_i16_be_stereo() {
    // L: 0x03E8 (1000), 0x8000 (-32768); R: 0xFC18 (-1000), 0x7FFF (32767).
    let data = [0x03, 0xE8, 0xFC, 0x18, 0x80, 0x00, 0x7F, 0xFF];
    let mut dec = open(simple_aiff(16, 2, &data, 44_100.0)).unwrap();
    assert_eq!(dec.info().sample_rate, 44_100);
    assert_eq!(dec.info().channels, 2);
    assert_eq!(dec.info().total_frames, Some(2));
    let got = decode_all(&mut dec);
    assert_eq!(
        got,
        vec![1000.0 / 32768.0, -1000.0 / 32768.0, -1.0, 32767.0 / 32768.0]
    );
}

#[test]
fn aiff_i24_be_mono() {
    // 0x400000 -> 0.5, 0x800000 -> -1.0, 0xC00000 -> -0.5.
    let data = [0x40, 0x00, 0x00, 0x80, 0x00, 0x00, 0xC0, 0x00, 0x00];
    let mut dec = open(simple_aiff(24, 1, &data, 48_000.0)).unwrap();
    assert_eq!(dec.info().bit_depth, 24);
    assert_eq!(decode_all(&mut dec), vec![0.5, -1.0, -0.5]);
}

#[test]
fn aiff_i32_be_mono() {
    let mut data = Vec::new();
    for v in [0i32, 1 << 30, -(1 << 30)] {
        data.extend_from_slice(&v.to_be_bytes());
    }
    let mut dec = open(simple_aiff(32, 1, &data, 48_000.0)).unwrap();
    assert_eq!(decode_all(&mut dec), vec![0.0, 0.5, -0.5]);
}

// ---------------------------------------------------------------------------
// AIFF-C compression types
// ---------------------------------------------------------------------------

fn aifc(
    compression: &[u8; 4],
    name: &str,
    bits: u16,
    channels: u16,
    data: &[u8],
    rate: f64,
) -> Vec<u8> {
    let frames = (data.len() / (channels as usize * bits as usize / 8)) as u32;
    build_aiff(
        b"AIFC",
        vec![
            chunk(
                b"COMM",
                &comm_payload(channels, frames, bits, rate, Some((compression, name))),
            ),
            chunk(b"FVER", &0xA280_5140u32.to_be_bytes()),
            chunk(b"SSND", &ssnd_payload(0, data)),
        ],
    )
}

#[test]
fn aifc_none_and_twos_are_big_endian() {
    let data = [0x01, 0x00, 0xFF, 0xFF]; // 256, -1 as BE i16
    for comp in [b"NONE".as_slice(), b"twos".as_slice()] {
        let fourcc: [u8; 4] = comp.try_into().unwrap();
        let mut dec = open(aifc(&fourcc, "not compressed", 16, 1, &data, 8_000.0)).unwrap();
        assert_eq!(
            decode_all(&mut dec),
            vec![256.0 / 32768.0, -1.0 / 32768.0],
            "compression {comp:?} must decode big-endian"
        );
    }
}

#[test]
fn aifc_sowt_is_little_endian() {
    let data = [0x01, 0x00, 0xFF, 0xFF]; // 1, -1 as LE i16
    let mut dec = open(aifc(b"sowt", "little endian", 16, 1, &data, 8_000.0)).unwrap();
    assert_eq!(decode_all(&mut dec), vec![1.0 / 32768.0, -1.0 / 32768.0]);
}

#[test]
fn aifc_fl32_is_big_endian() {
    let mut data = Vec::new();
    for v in [0.5f32, -0.25] {
        data.extend_from_slice(&v.to_be_bytes());
    }
    let mut dec = open(aifc(b"FL32", "32-bit float", 32, 1, &data, 48_000.0)).unwrap();
    assert_eq!(decode_all(&mut dec), vec![0.5, -0.25]);
}

#[test]
fn aifc_in24_big_and_ni24_little() {
    // 0x400000 (0.5) big-endian; 0x400000 little-endian bytes -> -0.5.
    let be = [0x40u8, 0x00, 0x00];
    let mut dec = open(aifc(b"in24", "24-bit int", 24, 1, &be, 48_000.0)).unwrap();
    assert_eq!(decode_all(&mut dec), vec![0.5]);

    let le = [0x00u8, 0x00, 0xC0]; // LE bytes of 0xC00000 -> -0.5
    let mut dec = open(aifc(b"ni24", "swapped 24-bit", 24, 1, &le, 48_000.0)).unwrap();
    assert_eq!(decode_all(&mut dec), vec![-0.5]);
}

#[test]
fn unsupported_compression_types_are_rejected() {
    for comp in [b"ima4", b"ULAW", b"ALAW", b"MAC3"] {
        let fourcc: [u8; 4] = *comp;
        let err = open(aifc(&fourcc, "compressed", 16, 1, &[0, 0], 8_000.0)).unwrap_err();
        assert!(
            matches!(err, CadenceError::UnsupportedFeature(_)),
            "{comp:?} must be rejected as unsupported"
        );
    }
}

// ---------------------------------------------------------------------------
// Chunk-level behavior
// ---------------------------------------------------------------------------

#[test]
fn ssnd_data_offset_is_honored() {
    // 4 junk bytes before the sound data (offset = 4).
    let data = [0x40u8, 0x00, 0x00]; // 0.5 at 24-bit
    let body = vec![
        chunk(b"COMM", &comm_payload(1, 1, 24, 48_000.0, None)),
        chunk(b"SSND", &ssnd_payload(4, &data)),
    ];
    let mut dec = open(build_aiff(b"AIFF", body)).unwrap();
    assert_eq!(decode_all(&mut dec), vec![0.5]);
}

#[test]
fn odd_sized_annotation_chunk_with_pad_is_skipped() {
    let data = [0x03u8, 0xE8];
    let anno = b"produced by tpt"; // 15 bytes -> odd, needs pad byte
    let body = vec![
        chunk(b"ANNO", anno),
        chunk(b"COMM", &comm_payload(1, 1, 16, 8_000.0, None)),
        chunk(b"SSND", &ssnd_payload(0, &data)),
    ];
    let mut dec = open(build_aiff(b"AIFF", body)).unwrap();
    assert_eq!(decode_all(&mut dec), vec![1000.0 / 32768.0]);
}

#[test]
fn declared_frame_count_bounds_decoding() {
    // Declare 1 frame but supply 3 -> only 1 frame decodes.
    let data = [0x00u8, 0x01, 0x00, 0x02, 0x00, 0x03];
    let body = vec![
        chunk(b"COMM", &comm_payload(1, 1, 16, 8_000.0, None)),
        chunk(b"SSND", &ssnd_payload(0, &data)),
    ];
    let mut dec = open(build_aiff(b"AIFF", body)).unwrap();
    assert_eq!(dec.info().total_frames, Some(1));
    assert_eq!(decode_all(&mut dec), vec![1.0 / 32768.0]);
}

#[test]
fn truncated_sound_data_drops_partial_frame() {
    // Declare 3 frames but supply only 3.5 frames of bytes.
    let data = [0x00u8, 0x01, 0x00, 0x02, 0x00, 0x03, 0x00];
    let body = vec![
        chunk(b"COMM", &comm_payload(1, 3, 16, 8_000.0, None)),
        chunk(b"SSND", &ssnd_payload(0, &data)),
    ];
    let mut dec = open(build_aiff(b"AIFF", body)).unwrap();
    let got = decode_all(&mut dec);
    assert_eq!(got, vec![1.0 / 32768.0, 2.0 / 32768.0, 3.0 / 32768.0]);
}

#[test]
fn empty_ssnd_decodes_zero_frames() {
    let body = vec![
        chunk(b"COMM", &comm_payload(1, 0, 16, 8_000.0, None)),
        chunk(b"SSND", &ssnd_payload(0, &[])),
    ];
    let mut dec = open(build_aiff(b"AIFF", body)).unwrap();
    let mut buf = [0.0f32; 8];
    assert_eq!(dec.decode(&mut buf).unwrap(), 0);
}

// ---------------------------------------------------------------------------
// Rejections
// ---------------------------------------------------------------------------

#[test]
fn ssnd_before_comm_is_rejected() {
    let body = vec![
        chunk(b"SSND", &ssnd_payload(0, &[0, 1])),
        chunk(b"COMM", &comm_payload(1, 1, 16, 8_000.0, None)),
    ];
    assert!(open(build_aiff(b"AIFF", body)).is_err());
}

#[test]
fn non_form_input_is_rejected() {
    assert!(open(b"garbage that is not an iff file".to_vec()).is_err());
}

#[test]
fn non_aiff_iff_form_is_rejected() {
    let body = vec![chunk(b"ANNO", b"8SVX thing")];
    let err = open(build_aiff(b"8SVX", body)).unwrap_err();
    assert!(matches!(err, CadenceError::InvalidFormat(_)));
}

#[test]
fn zero_channels_are_rejected() {
    // Regression: a zero-channel COMM must be rejected before the block-align
    // division (found by the mutation property test).
    let body = vec![
        chunk(b"COMM", &comm_payload(0, 1, 16, 8_000.0, None)),
        chunk(b"SSND", &ssnd_payload(0, &[0, 1])),
    ];
    let err = open(build_aiff(b"AIFF", body)).unwrap_err();
    assert!(matches!(err, CadenceError::CorruptData(_)));
}

#[test]
fn unsupported_sample_sizes_are_rejected() {
    let body = vec![
        chunk(b"COMM", &comm_payload(1, 1, 12, 8_000.0, None)),
        chunk(b"SSND", &ssnd_payload(0, &[0, 0, 0])),
    ];
    let err = open(build_aiff(b"AIFF", body)).unwrap_err();
    assert!(matches!(err, CadenceError::UnsupportedFeature(_)));
}

#[test]
fn invalid_sample_rate_is_rejected() {
    // Zero rate: COMM with all-zero 80-bit field.
    let mut p = Vec::new();
    p.extend_from_slice(&1u16.to_be_bytes());
    p.extend_from_slice(&1u32.to_be_bytes());
    p.extend_from_slice(&16u16.to_be_bytes());
    p.extend_from_slice(&[0u8; 10]);
    let body = vec![
        chunk(b"COMM", &p),
        chunk(b"SSND", &ssnd_payload(0, &[0, 0])),
    ];
    let err = open(build_aiff(b"AIFF", body)).unwrap_err();
    assert!(matches!(err, CadenceError::CorruptData(_)));
}

// ---------------------------------------------------------------------------
// Seeking and buffering
// ---------------------------------------------------------------------------

#[test]
fn seek_roundtrip_and_midstream() {
    let mut data = Vec::new();
    for i in 0..100i16 {
        data.extend_from_slice(&i.to_be_bytes());
    }
    let mut dec = open(simple_aiff(16, 1, &data, 8_000.0)).unwrap();
    let all = decode_all(&mut dec);
    assert_eq!(all.len(), 100);

    dec.seek(0).unwrap();
    assert_eq!(decode_all(&mut dec), all);

    dec.seek(42).unwrap();
    let mut buf = [0.0f32; 3];
    assert_eq!(dec.decode(&mut buf).unwrap(), 3);
    assert_eq!(buf[0], 42.0 / 32768.0);

    assert!(matches!(
        dec.seek(101),
        Err(CadenceError::SeekOutOfRange {
            requested: 101,
            total: 100
        })
    ));
}

#[test]
fn buffer_not_multiple_of_channels_is_rejected() {
    let mut dec = open(simple_aiff(16, 2, &[0u8; 8], 8_000.0)).unwrap();
    let mut buf = [0.0f32; 5];
    assert!(dec.decode(&mut buf).is_err());
}

#[test]
fn unseekable_source_decode_works_and_seek_is_unsupported() {
    let data = simple_aiff(16, 1, &[0, 1, 0, 2], 8_000.0);
    let mut reader = tpt_av_cadence_aiff::AiffReader::open(
        Box::new(Cursor::new(data)) as Box<dyn std::io::Read + Send>
    )
    .unwrap();
    assert_eq!(reader.info().total_frames, Some(2));
    let mut buf = [0.0f32; 2];
    assert_eq!(reader.decoder().decode(&mut buf).unwrap(), 2);
    assert!(matches!(
        reader.decoder().seek(1),
        Err(CadenceError::UnsupportedFeature(_))
    ));
}

// ---------------------------------------------------------------------------
// Never-panic property tests
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn arbitrary_bytes_never_panic(data in proptest::collection::vec(any::<u8>(), 0..4096)) {
        if let Ok(mut dec) = AiffDecoder::from_source(Box::new(Cursor::new(data))) {
            let mut buf = vec![0.0f32; 997];
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
        let base = simple_aiff(16, 2, &[0x12, 0x34, 0x56, 0x78, 0x9A, 0xBC, 0xDE, 0xF0], 44_100.0);
        let mut rng = tpt_av_cadence_test_utils::fuzz::Rng::new(seed);
        let mutated = tpt_av_cadence_test_utils::fuzz::mutate(&base, &mut rng);
        if let Ok(mut dec) = AiffDecoder::from_source(Box::new(Cursor::new(mutated))) {
            let mut buf = [0.0f32; 512];
            for _ in 0..64 {
                if dec.decode(&mut buf).unwrap_or(0) == 0 { break; }
            }
        }
    }
}
