//! CRC-16 protection exercised against real encoder output.
//!
//! Each bundled fixture has its first frame rewritten into a CRC-protected
//! variant (protection bit set, a computed CRC-16 after the header, side
//! information shifted by two bytes, two trailing ancillary bytes dropped to
//! preserve the frame length). A fresh stream's first frame always draws a
//! zero bit reservoir, so its main data is self-contained: the protected
//! rewrite must decode bit-identically, and corrupting a protected side-info
//! byte must reject the frame and resynchronize.
//!
//! Only the first frame can be rewritten safely. Real LAME streams run the
//! bit reservoir at exactly the level each subsequent frame's
//! `main_data_begin` requests (essentially zero ancillary slack), so any
//! per-frame loss of two carried bytes starves the chain within a few
//! frames and correctly drops them. Whole-stream protected rewrites are
//! therefore impossible to synthesize without a CRC-capable encoder; the
//! per-layout acceptance/rejection matrix lives in `streaming.rs` on
//! synthetic self-contained frames.
//!
//! The rewrite is intentionally an independent reimplementation of the frame
//! layout (header parse, CRC-16, side-info sizes) so it pins the decoder
//! against the spec rather than against itself.

use std::fs::File;
use std::io::{Cursor, Read};
use tpt_av_cadence_core::Decoder;
use tpt_av_cadence_mp3::Mp3Decoder;

const FIXTURES: &[&str] = &[
    "mpeg1_44100_mono_96k.mp3",
    "mpeg1_44100_joint_160k.mp3",
    "mpeg1_44100_stereo_128k.mp3",
    "mpeg1_44100_stereo_192k_nosecond.mp3",
    "mpeg1_48000_stereo_192k.mp3",
    "mpeg1_32000_mono_b96k.mp3",
    "mpeg2_16000_mono_b24k.mp3",
    "mpeg2_22050_stereo_96k.mp3",
    "mpeg2_24000_stereo_64k.mp3",
    "mpeg25_8000_mono_16k.mp3",
];

const BITRATES_V1: [u32; 15] = [
    0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320,
];
const BITRATES_LSF: [u32; 15] = [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160];

fn frame_total(h: &[u8]) -> Option<usize> {
    // Layer III only; h[1] bits: sync(4) ID(1) ext(1) layer(2).
    let mpeg1 = h[1] & 0x08 != 0;
    let layer = (h[1] >> 1) & 3;
    if h[0] != 0xFF || layer != 1 {
        return None;
    }
    let br_idx = ((h[2] >> 4) & 0xF) as usize;
    let sr_idx = ((h[2] >> 2) & 3) as usize;
    if br_idx == 0 || br_idx == 15 || sr_idx == 3 {
        return None;
    }
    let base_hz = [44100u32, 48000, 32000];
    let kbps = if mpeg1 {
        BITRATES_V1[br_idx]
    } else {
        BITRATES_LSF[br_idx]
    };
    let hz = if mpeg1 {
        base_hz[sr_idx]
    } else if h[1] & 0x10 != 0 {
        base_hz[sr_idx] / 2
    } else {
        base_hz[sr_idx] / 4
    };
    let samples: u32 = if mpeg1 { 1152 } else { 576 };
    let padding = ((h[2] >> 1) & 1) as usize;
    Some(samples as usize * kbps as usize * 125 / hz as usize + padding)
}

fn side_info_len(h: &[u8]) -> usize {
    let mpeg1 = h[1] & 0x08 != 0;
    let mono = (h[3] >> 6) & 3 == 3;
    match (mpeg1, mono) {
        (true, true) => 17,
        (true, false) => 32,
        (false, true) => 9,
        (false, false) => 17,
    }
}

/// MSB-first CRC-16, poly 0x8005, init 0xFFFF (ISO 11172-3 §2.4.3.1).
fn crc16(data: &[u8]) -> u16 {
    let mut crc: u16 = 0xFFFF;
    for &byte in data {
        crc ^= u16::from(byte) << 8;
        for _ in 0..8 {
            let feedback = crc & 0x8000 != 0;
            crc <<= 1;
            if feedback {
                crc ^= 0x8005;
            }
        }
    }
    crc
}

/// Byte offset just past a leading ID3v2 tag (10-byte header, syncsafe
/// size, optional footer).
fn audio_start(stream: &[u8]) -> usize {
    if stream.len() >= 10 && &stream[..3] == b"ID3" {
        let size = ((stream[6] as usize & 0x7F) << 21)
            | ((stream[7] as usize & 0x7F) << 14)
            | ((stream[8] as usize & 0x7F) << 7)
            | (stream[9] as usize & 0x7F);
        10 + size + if stream[5] & 0x10 != 0 { 10 } else { 0 }
    } else {
        0
    }
}

/// Returns the first frame of `stream` rewritten as CRC-protected, followed
/// by the remaining stream bytes verbatim.
fn protect_first_frame(stream: &[u8]) -> Vec<u8> {
    let off = audio_start(stream);
    let h = [
        stream[off],
        stream[off + 1],
        stream[off + 2],
        stream[off + 3],
    ];
    let total = frame_total(&h).expect("first frame header");
    let si = side_info_len(&h);
    assert!(
        total >= 4 + si + 2,
        "frame too small for a protected rewrite"
    );
    let frame = &stream[off..off + total];
    let mut out = stream[..off].to_vec();
    out.extend_from_slice(&[h[0], h[1] & 0xFE, h[2], h[3]]);
    let mut covered = Vec::with_capacity(2 + si);
    covered.extend_from_slice(&h[2..4]);
    covered.extend_from_slice(&frame[4..4 + si]);
    out.extend_from_slice(&crc16(&covered).to_be_bytes());
    // Side info and main data unchanged; two trailing ancillary bytes
    // dropped so the length stays inside the header formula.
    out.extend_from_slice(&frame[4..total - 2]);
    out.extend_from_slice(&stream[off + total..]);
    out
}

fn decode_all(bytes: Vec<u8>) -> Vec<f32> {
    let mut dec = Mp3Decoder::from_source(Box::new(Cursor::new(bytes))).unwrap();
    let ch = dec.info().channels as usize;
    let mut out = Vec::new();
    let mut buf = vec![0.0f32; 1152 * ch];
    loop {
        let n = dec.decode(&mut buf).unwrap();
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n * ch]);
    }
    out
}

#[test]
fn crc_protected_first_frame_decodes_identically() {
    let base = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/data");
    for file in FIXTURES {
        let mut raw = Vec::new();
        File::open(base.join(file))
            .unwrap()
            .read_to_end(&mut raw)
            .unwrap();
        let off = audio_start(&raw);
        let h = [raw[off], raw[off + 1], raw[off + 2], raw[off + 3]];
        let mpeg1 = h[1] & 0x08 != 0;
        let mono = (h[3] >> 6) & 3 == 3;
        let first_frame_samples = if mpeg1 { 1152 } else { 576 } * if mono { 1 } else { 2 };

        let plain = decode_all(raw.clone());
        let protected = decode_all(protect_first_frame(&raw));
        // A fresh stream's first frame draws no reservoir: its main data is
        // self-contained and the protected rewrite must reproduce it exactly.
        assert_eq!(
            &protected[..first_frame_samples],
            &plain[..first_frame_samples],
            "{file}: protected first frame diverged"
        );
        assert!(protected.iter().all(|x| x.is_finite()));
        assert!(!protected.is_empty());
    }
}

#[test]
fn crc_corruption_of_real_frame_is_caught_and_resyncs() {
    // Flipping one protected side-info bit must reject that frame (the CRC
    // is checked before any decoder state changes) and continue with the
    // rest of the stream. Downstream frames may legitimately wobble: a
    // dropped frame breaks the bit-reservoir chain, so classic MP3 error
    // propagation can cost a couple more frames — but the loss must stay
    // bounded and the stream must end cleanly.
    let base = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/data");
    let mut raw = Vec::new();
    File::open(base.join("mpeg1_44100_stereo_128k.mp3"))
        .unwrap()
        .read_to_end(&mut raw)
        .unwrap();
    let mut protected = protect_first_frame(&raw);
    let off = audio_start(&raw);
    assert_eq!(protected[off], 0xFF, "expected frame sync");
    protected[off + 6 + 4] ^= 0x01; // first side-info byte after header+CRC

    let plain = decode_all(raw);
    let corrupt = decode_all(protected);
    let frame_samples = 1152 * 2; // one frame, interleaved
    let lost = plain.len() - corrupt.len();
    assert!(
        lost >= frame_samples && lost <= 4 * frame_samples,
        "expected bounded loss of 1-4 frames, lost {lost} samples"
    );
    assert!(corrupt.iter().all(|x| x.is_finite()));
}
