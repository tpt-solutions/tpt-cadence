//! Bounded no-panic checks, not a proof of safety for all malformed streams.
use std::io::Cursor;

use proptest::prelude::*;
use tpt_av_cadence_core::Decoder;
use tpt_av_cadence_mp3::Mp3Decoder;

fn exercise(bytes: Vec<u8>) {
    if let Ok(mut decoder) = Mp3Decoder::from_source(Box::new(Cursor::new(bytes))) {
        let mut pcm = [0.0; 2304];
        for _ in 0..32 {
            match decoder.decode(&mut pcm) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let ch = decoder.info().channels as usize;
                    assert!(n * ch <= pcm.len());
                    assert!(
                        pcm[..n * ch].iter().all(|x| x.is_finite()),
                        "non-finite output from malformed stream"
                    );
                }
            }
        }
        let _ = decoder.seek(0);
        let _ = decoder.decode(&mut pcm);
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn arbitrary_streams_do_not_panic(bytes in prop::collection::vec(any::<u8>(), 0..2048)) {
        exercise(bytes);
    }

    #[test]
    fn mutated_frames_do_not_panic(
        lsf in any::<bool>(),
        changes in prop::collection::vec((0usize..2048, any::<u8>()), 1..16),
        length in 4usize..2049,
    ) {
        let source: &[u8] = if lsf {
            include_bytes!("data/mpeg2_16000_mono_b24k.mp3")
        } else {
            include_bytes!("data/mpeg1_44100_stereo_128k.mp3")
        };
        // Preserve the first two headers so mutations reach decoding, not
        // only the initial sync probe. Both fixtures have a 44-byte ID3 tag.
        let frame_len = if lsf { 108 } else { 417 };
        let mut bytes = source[44..44 + length].to_vec();
        for (offset, value) in changes {
            let offset = offset % bytes.len();
            if offset >= 4 && !(frame_len..frame_len + 4).contains(&offset) {
                bytes[offset] ^= value;
            }
        }
        exercise(bytes);
    }
}

/// A frame whose `main_data_begin` exceeds the reservoir the stream can
/// supply must be dropped (not overread, not panic) and the following
/// frames must still decode: the loss is the starved frame plus bounded
/// classic error propagation through the shared reservoir, never a
/// cascade that eats the stream.
#[test]
fn reservoir_overdraw_drops_one_frame_and_resynchronizes() {
    use std::io::Cursor;

    let source: &[u8] = include_bytes!("data/mpeg1_44100_stereo_128k.mp3");
    // After the 44-byte ID3 tag: set the third frame's main_data_begin
    // (top 9 bits of side info) to the 9-bit maximum, 511 — far more than
    // two preceding frames could have banked. Walk real frame boundaries;
    // padding makes lengths alternate 417/418.
    let total = |off: usize| {
        let h = &source[off..off + 4];
        let kbps = 128;
        1152 * kbps * 125 / 44100 + ((h[2] >> 1) & 1) as usize
    };
    let mut frame3 = 44;
    frame3 += total(frame3);
    frame3 += total(frame3);
    assert_eq!(source[frame3], 0xFF, "expected frame sync");
    let mut stream = source.to_vec();
    stream[frame3 + 4] = 0xFF;
    stream[frame3 + 5] |= 0x80;

    let mut decoder = Mp3Decoder::from_source(Box::new(Cursor::new(stream))).unwrap();
    let ch = decoder.info().channels as usize;
    let mut got = Vec::new();
    let mut buf = vec![0.0f32; 1152 * ch];
    loop {
        let n = decoder.decode(&mut buf).unwrap();
        if n == 0 {
            break;
        }
        got.extend_from_slice(&buf[..n * ch]);
    }
    let plain_len = 133632 * ch; // pinned by the conformance suite
    let lost = plain_len - got.len();
    let frame_samples = 1152 * ch;
    assert!(
        lost >= frame_samples && lost <= 4 * frame_samples,
        "expected bounded loss of 1-4 frames, lost {lost} samples"
    );
    assert!(got.iter().all(|x| x.is_finite()));
}

/// Every single-bit flip inside one real frame's side information must
/// decode without panic, produce finite output, and let the decoder keep
/// producing output afterwards (bounded resync). Unlike the random proptest
/// mutations above this deterministically reaches every side-info field —
/// block type and the mixed-block flag (the short-window/reorder paths),
/// scfsi sharing, Huffman table selects, subblock gains, scalefactor
/// compression, and the `part2_3_length`/`main_data_begin` accounting — on
/// both the MPEG-1 stereo (32-byte) and MPEG-2 LSF mono (9-byte) layouts.
#[test]
fn every_side_info_bit_flip_is_bounded() {
    // (fixture, ID3 length, MPEG-1 flag, bitrate kbps, base Hz, side bytes)
    for (source, id3, mpeg1, kbps, hz, side_len) in [
        (
            include_bytes!("data/mpeg1_44100_stereo_128k.mp3").as_slice(),
            44usize,
            true,
            128u32,
            44100u32,
            32usize,
        ),
        (
            include_bytes!("data/mpeg2_16000_mono_b24k.mp3").as_slice(),
            44,
            false,
            24,
            16000,
            9,
        ),
    ] {
        let base: u32 = if mpeg1 { 1152 } else { 576 };
        let total = |off: usize| {
            let h = &source[off..off + 4];
            base as usize * kbps as usize * 125 / hz as usize + ((h[2] >> 1) & 1) as usize
        };
        // Truncate after frame 4 so each mutated decode is tiny, then walk
        // to frame 3 (mid-stream, real audio data in front of it).
        let mut off = id3;
        for _ in 0..4 {
            off += total(off);
        }
        let cut = off.min(source.len());
        let frame3 = {
            let mut o = id3;
            for _ in 0..3 {
                o += total(o);
            }
            o
        };
        assert!(frame3 + total(frame3) <= cut, "frame 3 inside truncation");

        let window = &source[..cut];
        for bit in 0..side_len * 8 {
            let mut stream = window.to_vec();
            stream[frame3 + 4 + bit / 8] ^= 0x80 >> (bit % 8);
            exercise(stream);
        }
    }
}
