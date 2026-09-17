//! Regression tests for MP3 stream probing and end-of-input handling.
use std::io::Cursor;

use tpt_av_cadence_core::Decoder;
use tpt_av_cadence_mp3::Mp3Decoder;

fn silent_frame() -> Vec<u8> {
    // MPEG-1 Layer III, 128 kbps, 44100 Hz, stereo, no CRC.
    // Zero side information encodes empty granules with no reservoir use.
    let mut frame = vec![0; 417];
    frame[..4].copy_from_slice(&[0xff, 0xfb, 0x90, 0x00]);
    frame
}

#[test]
fn short_stream_drops_truncated_final_frame() {
    let frame = silent_frame();
    let mut stream = frame.clone();
    stream.extend_from_slice(&frame[..100]);
    let mut decoder = Mp3Decoder::open(Box::new(Cursor::new(stream))).unwrap();
    assert_eq!(decoder.info().sample_rate, 44100);
    assert_eq!(decoder.info().channels, 2);
    let mut pcm = [1.0; 2304];
    assert_eq!(decoder.decode(&mut pcm).unwrap(), 1152);
    assert!(pcm.iter().all(|&sample| sample == 0.0));
    assert_eq!(decoder.decode(&mut pcm).unwrap(), 0);
    assert_eq!(decoder.decode(&mut pcm).unwrap(), 0);
}

#[test]
fn probe_preserves_frame_crossing_window_boundary() {
    let frame = silent_frame();
    let mut stream = vec![0; 8000];
    stream.extend_from_slice(&frame);
    stream.extend_from_slice(&frame);
    let mut decoder = Mp3Decoder::from_source(Box::new(Cursor::new(stream))).unwrap();
    let mut pcm = [1.0; 4608];
    assert_eq!(decoder.decode(&mut pcm).unwrap(), 2304);
    assert!(pcm.iter().all(|&sample| sample == 0.0));
    assert_eq!(decoder.decode(&mut pcm).unwrap(), 0);
    decoder.seek(0).unwrap();
    assert_eq!(decoder.decode(&mut pcm).unwrap(), 2304);
}

#[test]
fn single_complete_frame_opens_at_eof() {
    let mut decoder = Mp3Decoder::open(Box::new(Cursor::new(silent_frame()))).unwrap();
    let mut pcm = [1.0; 2304];
    assert_eq!(decoder.decode(&mut pcm).unwrap(), 1152);
    assert!(pcm.iter().all(|&sample| sample == 0.0));
    assert_eq!(decoder.decode(&mut pcm).unwrap(), 0);
}

// Independently accumulate the protected bits, excluding the stored CRC.
fn protected_frame(version: u8, mono: bool) -> (Vec<u8>, usize, usize) {
    let mpeg1 = version == 0xfa;
    let samples = if mpeg1 { 1152 } else { 576 };
    let rate = match version {
        0xfa => 44100,
        0xf2 => 22050,
        0xe2 => 11025,
        _ => unreachable!(),
    };
    // Bitrate index 9 = 128 kbps for MPEG-1, 80 kbps for LSF.
    let bitrate = if mpeg1 { 128 } else { 80 };
    let side_len = match (mpeg1, mono) {
        (true, true) | (false, false) => 17,
        (true, false) => 32,
        (false, true) => 9,
    };
    let mut frame = vec![0; samples * bitrate * 125 / rate];
    frame[..4].copy_from_slice(&[0xff, version, 0x90, if mono { 0xc0 } else { 0 }]);
    let mut crc = 0xffffu16;
    for &byte in frame[2..4].iter().chain(&frame[6..6 + side_len]) {
        for bit in (0..8).rev() {
            let feedback = ((crc >> 15) ^ u16::from((byte >> bit) & 1)) != 0;
            crc <<= 1;
            if feedback {
                crc ^= 0x8005;
            }
        }
    }
    frame[4..6].copy_from_slice(&crc.to_be_bytes());
    (frame, samples, side_len)
}

#[test]
fn crc_protected_frames_decode_for_all_version_channel_layouts() {
    for version in [0xfa, 0xf2, 0xe2] {
        for mono in [false, true] {
            let (frame, samples, _) = protected_frame(version, mono);
            let mut decoder = Mp3Decoder::open(Box::new(Cursor::new(frame))).unwrap();
            let channels = if mono { 1 } else { 2 };
            let mut pcm = vec![1.0; samples * channels];
            assert_eq!(
                decoder.decode(&mut pcm).unwrap(),
                samples,
                "version={version:#x}, mono={mono}"
            );
            assert!(pcm.iter().all(|&x| x == 0.0));
            assert_eq!(decoder.decode(&mut pcm).unwrap(), 0);
        }
    }
}

#[test]
fn crc_rejects_protected_bit_changes_and_recovers_at_next_frame() {
    for version in [0xfa, 0xf2, 0xe2] {
        for mono in [false, true] {
            let (frame, samples, side_len) = protected_frame(version, mono);
            // Header private bit, stored CRC, and final side-info bit.
            for offset in [2, 4, 6 + side_len - 1] {
                let mut stream = frame.clone();
                stream[offset] ^= 1;
                stream.extend_from_slice(&frame);
                let mut decoder = Mp3Decoder::open(Box::new(Cursor::new(stream))).unwrap();
                let mut pcm = vec![1.0; samples * 2 * if mono { 1 } else { 2 }];
                assert_eq!(
                    decoder.decode(&mut pcm).unwrap(),
                    samples,
                    "version={version:#x}, mono={mono}, offset={offset}"
                );
                assert_eq!(decoder.decode(&mut pcm).unwrap(), 0);
            }
        }
    }
}

#[test]
fn crc_excludes_ancillary_payload() {
    for version in [0xfa, 0xf2, 0xe2] {
        for mono in [false, true] {
            let (mut frame, samples, _) = protected_frame(version, mono);
            *frame.last_mut().unwrap() ^= 1;
            let mut decoder = Mp3Decoder::open(Box::new(Cursor::new(frame))).unwrap();
            let mut pcm = [0.0; 2304];
            assert_eq!(decoder.decode(&mut pcm).unwrap(), samples);
        }
    }
}
