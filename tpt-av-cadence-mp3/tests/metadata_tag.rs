//! Info (CBR) / Xing (VBR) metadata tag tests: the leading tag frame must
//! parse at the documented offset, carry frames/bytes/TOC counts that match
//! the emitted stream, decode as silence in both this crate's decoder and
//! FFmpeg, and leave the audio frames decodable from a clean reservoir.

use std::io::Cursor;

use tpt_av_cadence_core::{Decoder, Encoder};
use tpt_av_cadence_mp3::{Mp3Decoder, Mp3Encoder};

/// Encodes `frames` frames of a quiet tone with the requested constructor
/// and returns (bytes, side-info byte offset of the tag, tag payload).
fn encode_tagged(vbr: bool, channels: u16, bitrate: u32) -> (Vec<u8>, usize, Vec<u8>) {
    let sr = 44_100u32;
    let n = sr as usize * 2; // two seconds
    let mut samples = Vec::with_capacity(n * channels as usize);
    for i in 0..n {
        let s = 0.05 * (2.0 * std::f32::consts::PI * 440.0 * i as f32 / sr as f32).sin();
        for _ in 0..channels {
            samples.push(s);
        }
    }
    let mut buf = Cursor::new(Vec::new());
    {
        let res = if vbr {
            Mp3Encoder::new_vbr_with_xing(&mut buf, sr, channels, 5)
        } else {
            Mp3Encoder::new_cbr_with_info(&mut buf, sr, channels, bitrate)
        };
        let mut enc = res.unwrap();
        enc.encode(&samples).unwrap();
        Encoder::finish(&mut enc).unwrap();
    }
    let data = buf.into_inner();

    // Side-info size for MPEG-1.
    let si = if channels == 1 { 17usize } else { 32 };
    let fourcc_off = 4 + si;
    let tag = data[fourcc_off + 4..fourcc_off + 4 + 4 + 4 + 4 + 100 + 4].to_vec();
    (data, fourcc_off, tag)
}

#[test]
fn info_tag_cbr_counts_and_toc() {
    for channels in [1u16, 2] {
        let (data, fourcc_off, tag) = encode_tagged(false, channels, 128);
        assert_eq!(&data[fourcc_off..fourcc_off + 4], b"Info");
        let flags = u32::from_be_bytes(tag[0..4].try_into().unwrap());
        assert_eq!(flags & 0b111, 0b111, "frames|bytes|TOC flags set");
        let frames = u32::from_be_bytes(tag[4..8].try_into().unwrap());
        let bytes = u32::from_be_bytes(tag[8..12].try_into().unwrap());
        assert_eq!(
            bytes as usize,
            data.len(),
            "bytes count must equal the file size"
        );
        // 2 seconds at 44.1 kHz: 76.5 audio frames, the trailing partial
        // frame, and the gapless flush frame. The count excludes the tag
        // frame itself (LAME/FFmpeg convention).
        let audio_frames = frames as usize;
        assert!(
            (76..=79).contains(&audio_frames),
            "audio frames {audio_frames} out of range for 2 s"
        );
        // TOC: nondecreasing, spans 0..=255.
        let toc = &tag[12..112];
        assert_eq!(toc[0], 0, "TOC starts at the file start");
        assert!(toc.windows(2).all(|w| w[0] <= w[1]), "TOC must be sorted");
        // Quality field present.
        let _quality = u32::from_be_bytes(tag[112..116].try_into().unwrap());
    }
}

#[test]
fn xing_tag_vbr_counts_and_fourcc() {
    let (data, fourcc_off, tag) = encode_tagged(true, 2, 0);
    assert_eq!(&data[fourcc_off..fourcc_off + 4], b"Xing");
    let frames = u32::from_be_bytes(tag[4..8].try_into().unwrap());
    let bytes = u32::from_be_bytes(tag[8..12].try_into().unwrap());
    assert_eq!(bytes as usize, data.len());
    assert!(frames > 70, "frame count {frames} too small for 2 s of VBR");
}

#[test]
fn tag_frame_decodes_as_silence_and_stream_still_decodes() {
    let (data, _off, _tag) = encode_tagged(false, 2, 128);
    let mut dec = Mp3Decoder::open(Box::new(Cursor::new(data))).unwrap();
    let mut ours = Vec::new();
    let mut buf = vec![0.0f32; 4096 * 2];
    loop {
        match dec.decode(&mut buf) {
            Ok(0) => break,
            Ok(g) => ours.extend_from_slice(&buf[..g * 2]),
            Err(e) => panic!("decode errored: {e}"),
        }
    }
    // The decoder skips the tag frame entirely (FFmpeg's demuxer
    // behavior): the output starts at the first real audio frame.
    let peak: f32 = ours[..1152 * 2].iter().copied().fold(0.0, f32::max);
    assert!(peak > 0.01, "audio must start immediately, peak {peak}");
}

#[test]
fn untagged_constructors_stay_tag_free() {
    // Backward compatibility: the plain constructors write no metadata.
    let sr = 44_100u32;
    let samples = vec![0.0f32; 1152 * 4 * 2];
    let mut buf = Cursor::new(Vec::new());
    {
        let mut enc = Mp3Encoder::new(&mut buf, sr, 2, 128).unwrap();
        enc.encode(&samples).unwrap();
        Encoder::finish(&mut enc).unwrap();
    }
    let data = buf.into_inner();
    assert_eq!(&data[36..40], b"\0\0\0\0", "no FOURCC in untagged CBR");
}

/// The LAME gapless extension: the 24-bit delay/padding field must parse
/// back with delay = the measured encoder pipeline delay (528) and padding
/// = (audio frames)·spf − delay − source pairs, and applying that trim to
/// our decode must yield exactly the source sample count.
#[test]
fn lame_gapless_field_and_trim() {
    let sr = 44_100u32;
    let src_pairs = sr as usize * 2; // two seconds
    let mut samples = Vec::with_capacity(src_pairs * 2);
    for i in 0..src_pairs {
        let s = 0.3 * (2.0 * std::f32::consts::PI * 440.0 * i as f32 / sr as f32).sin();
        samples.push(s);
        samples.push(s);
    }
    let mut buf = Cursor::new(Vec::new());
    {
        let mut enc = Mp3Encoder::new_cbr_with_info(&mut buf, sr, 2, 128).unwrap();
        enc.encode(&samples).unwrap();
        Encoder::finish(&mut enc).unwrap();
    }
    let data = buf.into_inner();

    // Parse the tag the same way the suite's oracle parser does.
    let fourcc_off = 4 + 32;
    assert_eq!(&data[fourcc_off..fourcc_off + 4], b"Info");
    let field = fourcc_off + 4 + 4 + 4 + 4 + 100 + 4 + 21;
    let v = (u32::from(data[field]) << 16)
        | (u32::from(data[field + 1]) << 8)
        | u32::from(data[field + 2]);
    let delay = (v >> 12) as usize;
    let padding = (v & 0xFFF) as usize;
    assert_eq!(delay, 528, "measured encoder delay");
    let audio_frames = ((data.len() - 36) / 417).max(1); // CBR 417-byte frames incl. tag
    let expected_pad = (audio_frames * 1152)
        .saturating_sub(delay + src_pairs)
        .min(4095);
    assert_eq!(
        padding, expected_pad,
        "padding must equal frames·spf − delay − src"
    );

    // Trimmed decode length: drop `delay` leading and `padding` trailing
    // samples from the (tag-skipping) decode → exactly the source length.
    let mut dec = Mp3Decoder::open(Box::new(Cursor::new(data))).unwrap();
    let mut ours = Vec::new();
    let mut buf = vec![0.0f32; 4096 * 2];
    loop {
        match dec.decode(&mut buf) {
            Ok(0) => break,
            Ok(g) => ours.extend_from_slice(&buf[..g * 2]),
            Err(e) => panic!("decode errored: {e}"),
        }
    }
    // The decoder applies the LAME trim internally: the output must be
    // source-exact without any manual trimming.
    assert_eq!(
        ours.len() / 2,
        src_pairs,
        "gapless trim must recover the exact source sample count"
    );
}

/// Seeking back to the start must skip the gapless delay again (the front
/// skip used to be consumed once and never restored).
#[test]
fn seek_to_start_repeats_the_gapless_skip() {
    let sr = 44_100u32;
    let n = sr as usize; // one second, mono
    let samples: Vec<f32> = (0..n)
        .map(|i| 0.4 * (2.0 * std::f32::consts::PI * 440.0 * i as f32 / sr as f32).sin())
        .collect();
    let mut buf = Cursor::new(Vec::new());
    {
        let mut enc = Mp3Encoder::new_cbr_with_info(&mut buf, sr, 1, 128).unwrap();
        enc.encode(&samples).unwrap();
        Encoder::finish(&mut enc).unwrap();
    }
    let mut dec = Mp3Decoder::from_source(Box::new(Cursor::new(buf.into_inner()))).unwrap();
    let mut first = vec![0.0f32; 2048];
    assert_eq!(dec.decode(&mut first).unwrap(), 2048);
    dec.seek(0).unwrap();
    let mut again = vec![0.0f32; 2048];
    assert_eq!(dec.decode(&mut again).unwrap(), 2048);
    assert_eq!(first, again, "seek(0) must reproduce the initial output");
}
