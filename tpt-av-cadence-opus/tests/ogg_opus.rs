//! Tests for the Ogg Opus (RFC 7845) container layer in
//! `tpt_av_cadence_opus::ogg_opus`: header parsing, pre-skip/end-trim
//! granule bookkeeping, `Decoder`/`FormatReader` behavior (including seek
//! replay), and — against the official vectors, when available — bit-exact
//! end-to-end decode of a SILK-only vector muxed into Ogg pages.

use std::io::{Cursor, Write};
use std::sync::{Arc, Mutex};

use tpt_av_cadence_core::{Encoder, FormatReader};
use tpt_av_cadence_opus::{OggOpusEncoder, OggOpusReader, OpusHead};

struct CountingWriter {
    bytes: Arc<Mutex<Vec<u8>>>,
}

impl Write for CountingWriter {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.bytes.lock().unwrap().extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Test-side Ogg muxer (the container crate deliberately exposes only the
// parser; muxing is reconstructed here from `page_crc`)
// ---------------------------------------------------------------------------

struct Muxer {
    out: Vec<u8>,
    serial: u32,
    seq: u32,
    seg_table: Vec<u8>,
    body: Vec<u8>,
    /// Granule of the most recent packet completed on pending pages.
    last_completed: i64,
    /// Granule of a packet completed within the pending page.
    completed_in_page: Option<i64>,
}

impl Muxer {
    fn new(serial: u32) -> Self {
        Muxer {
            out: Vec::new(),
            serial,
            seq: 0,
            seg_table: Vec::new(),
            body: Vec::new(),
            last_completed: 0,
            completed_in_page: None,
        }
    }

    /// Adds one packet, flushing full pages as needed (RFC 3533 lacing).
    fn packet(&mut self, data: &[u8], granule: i64) {
        let mut lacing = Vec::new();
        let mut rem = data.len();
        loop {
            if rem >= 255 {
                lacing.push(255u8);
                rem -= 255;
            } else {
                lacing.push(rem as u8);
                break;
            }
        }
        for l in lacing {
            if self.seg_table.len() == 255 {
                self.flush_page(false, false);
            }
            self.seg_table.push(l);
        }
        self.body.extend_from_slice(data);
        self.completed_in_page = Some(granule);
        self.last_completed = granule;
    }

    fn flush_page(&mut self, eos: bool, bos: bool) {
        let mut page = vec![0u8; 27];
        page[0..4].copy_from_slice(b"OggS");
        page[4] = 0;
        page[5] = (eos as u8) * 4 + (bos as u8) * 2;
        page[6..14].copy_from_slice(
            &self
                .completed_in_page
                .unwrap_or(self.last_completed)
                .to_le_bytes(),
        );
        page[14..18].copy_from_slice(&self.serial.to_le_bytes());
        page[18..22].copy_from_slice(&self.seq.to_le_bytes());
        page[26] = self.seg_table.len() as u8;
        page.extend_from_slice(&self.seg_table);
        page.extend_from_slice(&self.body);
        let crc = tpt_av_cadence_ogg::page_crc(&page);
        page[22..26].copy_from_slice(&crc.to_le_bytes());
        self.out.extend_from_slice(&page);
        self.seq += 1;
        self.seg_table.clear();
        self.body.clear();
        self.completed_in_page = None;
    }

    /// Finishes the stream: flushes any pending segments as the EOS page,
    /// with `granule` as the page's authoritative end position.
    fn finish(mut self, granule: i64) -> Vec<u8> {
        self.completed_in_page = Some(granule);
        self.flush_page(true, false);
        self.out
    }
}

fn opus_head_bytes(
    channels: u16,
    pre_skip: u16,
    input_rate: u32,
    gain_q8: i16,
    family: u8,
) -> Vec<u8> {
    let mut h = Vec::new();
    h.extend_from_slice(b"OpusHead");
    h.push(1); // version
    h.push(channels as u8);
    h.extend_from_slice(&pre_skip.to_le_bytes());
    h.extend_from_slice(&input_rate.to_le_bytes());
    h.extend_from_slice(&gain_q8.to_le_bytes());
    h.push(family);
    if family == 1 {
        h.push(1); // streams
        h.push(channels as u8 - 1); // coupled
        for i in 0..channels {
            h.push(i as u8);
        }
    }
    h
}

/// Muxes a complete Ogg Opus stream: OpusHead (BOS), OpusTags, then
/// `audio_packets` split `per_page` per page. The final audio page carries
/// the EOS flag with `eos_granule` (the shape real muxers emit, and the
/// only shape under which an end trim can be applied per completing
/// packet).
fn mux_stream(
    head: &[u8],
    audio_packets: &[Vec<u8>],
    eos_granule: i64,
    per_page: usize,
) -> Vec<u8> {
    let mut mux = Muxer::new(0x1234_5678);
    mux.packet(head, 0);
    mux.flush_page(false, true); // BOS page with OpusHead
    mux.packet(b"OpusTags\0\0\0\0test", 0);
    mux.flush_page(false, false);
    let n_chunks = audio_packets.chunks(per_page.max(1)).count();
    for (i, chunk) in audio_packets.chunks(per_page.max(1)).enumerate() {
        for p in chunk {
            mux.packet(p, 0);
        }
        // The final audio page stays pending: `finish` flushes it with
        // the EOS flag and the stream's true end granule.
        if i + 1 < n_chunks {
            mux.flush_page(false, false);
        }
    }
    mux.finish(eos_granule)
}

/// Config 31, code 0 (CELT fullband 20 ms, mono): a TOC byte plus a
/// minimal payload that decodes to a valid low-energy frame (same shape as
/// the `decodes_a_celt_only_silence_frame` unit test).
fn silence_packet() -> Vec<u8> {
    vec![31u8 << 3, 0, 0, 0, 0, 0]
}

/// Decodes the whole stream through a fresh reader, returning all PCM.
fn decode_all(data: &[u8], buffer_frames: usize) -> Vec<f32> {
    decode_all_with(
        OggOpusReader::from_source(Box::new(Cursor::new(data.to_vec()))).unwrap(),
        buffer_frames,
    )
    .0
}

/// Decodes the whole stream through `reader`, returning all PCM plus the
/// reader (for post-run metadata assertions).
fn decode_all_with(mut reader: OggOpusReader, buffer_frames: usize) -> (Vec<f32>, OggOpusReader) {
    let dec = reader.decoder();
    let channels = dec.info().channels as usize;
    let mut all = Vec::new();
    let mut buf = vec![0f32; buffer_frames * channels];
    loop {
        let n = dec.decode(&mut buf).unwrap();
        if n == 0 {
            break;
        }
        all.extend_from_slice(&buf[..n * channels]);
    }
    (all, reader)
}

// ---------------------------------------------------------------------------
// OpusHead parsing
// ---------------------------------------------------------------------------

#[test]
fn opus_head_parses_round_trip() {
    let bytes = opus_head_bytes(2, 312, 44_100, -256, 0);
    let head = OpusHead::parse(&bytes).unwrap();
    assert_eq!(head.version, 1);
    assert_eq!(head.channels, 2);
    assert_eq!(head.pre_skip, 312);
    assert_eq!(head.input_sample_rate, 44_100);
    assert_eq!(head.output_gain_q8, -256);
    assert_eq!(head.mapping_family, 0);
}

#[test]
fn opus_head_parses_trivial_family_1() {
    let bytes = opus_head_bytes(1, 0, 48_000, 0, 1);
    let head = OpusHead::parse(&bytes).unwrap();
    assert_eq!(head.channels, 1);
    assert_eq!(head.mapping_family, 1);
}

#[test]
fn opus_head_rejects_garbage_and_unsupported() {
    let valid = opus_head_bytes(2, 0, 48_000, 0, 0);

    let mut bad_magic = valid.clone();
    bad_magic[0] = b'X';
    assert!(OpusHead::parse(&bad_magic).is_err());

    let mut bad_version = valid.clone();
    bad_version[8] = 2;
    assert!(matches!(
        OpusHead::parse(&bad_version),
        Err(tpt_av_cadence_core::CadenceError::UnsupportedFeature(_))
    ));

    let mut bad_channels = valid.clone();
    bad_channels[9] = 0;
    assert!(OpusHead::parse(&bad_channels).is_err());

    let mut family_255 = valid.clone();
    family_255[18] = 255;
    assert!(matches!(
        OpusHead::parse(&family_255),
        Err(tpt_av_cadence_core::CadenceError::UnsupportedFeature(_))
    ));

    // Family 1 with a non-trivial mapping (two uncoupled mono streams).
    let mut swapped = opus_head_bytes(2, 0, 48_000, 0, 1);
    swapped[20] = 0; // coupled = 0
    assert!(matches!(
        OpusHead::parse(&swapped),
        Err(tpt_av_cadence_core::CadenceError::UnsupportedFeature(_))
    ));

    // Truncated mapping table.
    let truncated = &opus_head_bytes(2, 0, 48_000, 0, 1)[..21];
    assert!(OpusHead::parse(truncated).is_err());
}

// ---------------------------------------------------------------------------
// Container decode: pre-skip, end trim, granule, seek
// ---------------------------------------------------------------------------

#[test]
fn decodes_with_preskip_and_end_trim() {
    let channels: usize = 1;
    let n_packets = 7;
    let frames_per_packet = 960; // 20 ms at 48 kHz
    let pre_skip = 312;
    // The EOS granule cuts 47 samples off the final packet's synthesis.
    let end_trim = 47;
    let total_encoded = (n_packets * frames_per_packet) as i64;

    let packets: Vec<Vec<u8>> = (0..n_packets).map(|_| silence_packet()).collect();
    let data = mux_stream(
        &opus_head_bytes(channels as u16, pre_skip, 48_000, 0, 0),
        &packets,
        total_encoded - end_trim,
        2, // two packets per page (exercises multi-packet pages)
    );

    let reader = OggOpusReader::from_source(Box::new(Cursor::new(data.clone()))).unwrap();
    assert_eq!(reader.info().format, tpt_av_cadence_core::Format::Opus);
    assert_eq!(reader.info().sample_rate, 48_000);
    assert_eq!(reader.info().channels, channels as u16);
    assert_eq!(reader.info().total_frames, None);

    let (pcm, mut reader) = decode_all_with(reader, 500);
    let expected = total_encoded as usize - pre_skip as usize - end_trim as usize;
    assert_eq!(
        pcm.len() / channels,
        expected,
        "pre-skip and end-trim must trim exactly"
    );
    // Total frames became known once the EOS page was seen.
    assert_eq!(reader.decoder().info().total_frames, Some(expected as u64));
    assert!(pcm.iter().all(|s| s.is_finite()));
}

#[test]
fn decode_is_deterministic_across_buffer_sizes() {
    let packets: Vec<Vec<u8>> = (0..5).map(|_| silence_packet()).collect();
    let data = mux_stream(&opus_head_bytes(1, 0, 48_000, 0, 0), &packets, 5 * 960, 3);
    let a = decode_all(&data, 1);
    let b = decode_all(&data, 960);
    let c = decode_all(&data, 4096);
    assert_eq!(a, b);
    assert_eq!(b, c);
}

#[test]
fn seek_zero_replays_identically() {
    let packets: Vec<Vec<u8>> = (0..6).map(|_| silence_packet()).collect();
    let data = mux_stream(&opus_head_bytes(1, 120, 48_000, 0, 0), &packets, 6 * 960, 2);
    let first = decode_all(&data, 333);

    let mut reader = OggOpusReader::from_source(Box::new(Cursor::new(data.clone()))).unwrap();
    let dec = reader.decoder();
    dec.seek(0).unwrap();
    let mut second = Vec::new();
    let mut buf = vec![0f32; 333];
    loop {
        let n = dec.decode(&mut buf).unwrap();
        if n == 0 {
            break;
        }
        second.extend_from_slice(&buf[..n]);
    }
    assert_eq!(first, second);
}

#[test]
fn mid_stream_seek_rejoins_the_full_decode() {
    // Decode-and-discard replays the same packets, so once the target is
    // reached the decoder state matches an uninterrupted decode exactly.
    let packets: Vec<Vec<u8>> = (0..10).map(|_| silence_packet()).collect();
    let data = mux_stream(&opus_head_bytes(1, 0, 48_000, 0, 0), &packets, 10 * 960, 2);
    let full = decode_all(&data, 500);

    let seek_frame = 4 * 960 + 137;
    let mut reader = OggOpusReader::from_source(Box::new(Cursor::new(data.clone()))).unwrap();
    let dec = reader.decoder();
    dec.seek(seek_frame as u64).unwrap();
    let mut tail = Vec::new();
    let mut buf = vec![0f32; 500];
    loop {
        let n = dec.decode(&mut buf).unwrap();
        if n == 0 {
            break;
        }
        tail.extend_from_slice(&buf[..n]);
    }
    assert_eq!(
        tail,
        full[seek_frame..],
        "post-seek output must equal the uninterrupted decode's tail"
    );
}

#[test]
fn unseekable_source_rejects_seek() {
    let packets: Vec<Vec<u8>> = (0..3).map(|_| silence_packet()).collect();
    let data = mux_stream(&opus_head_bytes(1, 0, 48_000, 0, 0), &packets, 3 * 960, 1);
    let mut reader = OggOpusReader::open(Box::new(Cursor::new(data))).unwrap();
    let err = reader.decoder().seek(10).unwrap_err();
    assert!(matches!(
        err,
        tpt_av_cadence_core::CadenceError::UnsupportedFeature(_)
    ));
}

#[test]
fn trailing_empty_eos_page_yields_no_end_trim() {
    // An EOS page that carries no packets cannot trim audio already
    // emitted (the trim is per completing packet): output runs to the last
    // packet's natural end, and the stream length stays unknown.
    let channels: usize = 1;
    let n_packets = 5;
    let packets: Vec<Vec<u8>> = (0..n_packets).map(|_| silence_packet()).collect();
    let mut mux = Muxer::new(7);
    mux.packet(&opus_head_bytes(channels as u16, 0, 48_000, 0, 0), 0);
    mux.flush_page(false, true);
    mux.packet(b"OpusTags\0\0\0\0t", 0);
    mux.flush_page(false, false);
    for p in &packets {
        mux.packet(p, 0);
    }
    mux.flush_page(false, false); // all audio complete on a non-EOS page
    let data = mux.finish(n_packets as i64 * 960 - 47); // empty trailing EOS page
    let pcm = decode_all(&data, 500);
    assert_eq!(pcm.len() / channels, n_packets * 960);
}

// ---------------------------------------------------------------------------
// Official RFC 6716 vectors through the container
// ---------------------------------------------------------------------------

/// Muxes the packets of an `opus_demo`-format `.bit` file into an Ogg Opus
/// stream (pre-skip 0, one granule increment per packet's frames) and
/// checks the decoded PCM against the bundled `.dec` file sample-for-sample.
/// Bit-exactness is expected for the SILK-only vectors (02–04), which the
/// packet-level conformance suite already decodes bit-exactly.
#[test]
#[ignore = "needs OPUS_TESTVECTORS_DIR pointing at the extracted opus_testvectors"]
fn vector02_through_ogg_container_is_bit_exact() {
    let dir = std::env::var_os("OPUS_TESTVECTORS_DIR").expect(
        "OPUS_TESTVECTORS_DIR is not set. Download opus_testvectors.tar.gz from \
         https://opus-codec.org/static/testvectors/ and rerun as: \
         OPUS_TESTVECTORS_DIR=<path> cargo test -p tpt-av-cadence-opus --release -- --ignored",
    );
    let bit = std::fs::read(std::path::Path::new(&dir).join("testvector02.bit")).unwrap();
    let dec = std::fs::read(std::path::Path::new(&dir).join("testvector02.dec")).unwrap();

    // `opus_demo` .bit records: 4-byte BE length, 4-byte BE final range,
    // then the packet bytes (see tests/conformance.rs).
    let mut packets = Vec::new();
    let mut pos = 0usize;
    let mut total_frames = 0i64;
    while pos < bit.len() {
        let len = u32::from_be_bytes(bit[pos..pos + 4].try_into().unwrap()) as usize;
        pos += 8;
        packets.push(bit[pos..pos + len].to_vec());
        let packet = tpt_av_cadence_opus::packet::parse_packet(&bit[pos..pos + len]).unwrap();
        total_frames +=
            (packet.frame_count() * tpt_av_cadence_opus::celt_frame_size(&packet)) as i64;
        pos += len;
    }

    let data = mux_stream(
        &opus_head_bytes(2, 0, 48_000, 0, 0),
        &packets,
        total_frames,
        4,
    );
    let pcm = decode_all(&data, 960);

    let reference: Vec<f32> = dec
        .chunks_exact(2)
        .map(|c| i16::from_le_bytes([c[0], c[1]]) as f32 / 32768.0)
        .collect();
    assert_eq!(pcm.len(), reference.len(), "decoded length must match .dec");
    // `opus_demo`'s float→int16 rounding (see conformance.rs): allow the
    // same half-LSB conversion jitter, nothing more. The SILK-only vector
    // decodes bit-exact at the packet level, so any real mismatch here is
    // a container-layer bug (framing, granule bookkeeping, pre-skip).
    let max_diff = pcm
        .iter()
        .zip(&reference)
        .map(|(a, b)| (a - b).abs())
        .fold(0.0f32, f32::max);
    assert!(
        max_diff <= 1.0 / 32768.0 + 1e-6,
        "container decode diverged from .dec by {max_diff}"
    );
}

// ---------------------------------------------------------------------------
// OggOpusEncoder: real encode -> real decode round trip
// ---------------------------------------------------------------------------

/// Mono end-to-end: encode a tone through the real `OggOpusEncoder`, decode
/// through the real `OggOpusReader`/`OpusDecoder`, and check (a) the
/// decoded sample count exactly matches the input (end-trim correctly
/// discards the zero-pad tail needed to complete the final 960-sample
/// frame — this test deliberately uses a sample count that is *not* a
/// multiple of 960, so a wrong granule would either truncate real audio or
/// leak padding silence into the count) and (b) the decoded signal
/// correlates well with the original at the codec's fixed algorithmic
/// delay.
#[test]
fn ogg_opus_encoder_round_trips_a_tone_through_the_real_decoder() {
    let sample_rate = 48_000u32;
    let freq_hz = 440.0f32;
    let n_samples = 960 * 5 + 137; // deliberately not a multiple of 960
    let mut original = Vec::with_capacity(n_samples);
    let mut phase = 0.0f32;
    for _ in 0..n_samples {
        original.push(0.5 * phase.sin());
        phase += 2.0 * std::f32::consts::PI * freq_hz / sample_rate as f32;
    }

    let mut out = Vec::new();
    {
        // 64_000 bps -> 160 bytes/20ms-frame. The broader fixed-size CBR
        // budget matrix is covered by `celt_encoder_cbr_stays_within_requested_budget`.
        let mut enc = OggOpusEncoder::new(&mut out, sample_rate, 1, 64_000).unwrap();
        let mut pos = 0;
        // Feed in irregular chunks to exercise the encoder's own internal
        // frame-boundary buffering, not just whole-frame-at-a-time calls.
        for chunk in [200usize, 960, 333, 960, 960, 1000] {
            let end = (pos + chunk).min(original.len());
            enc.encode(&original[pos..end]).unwrap();
            pos = end;
        }
        if pos < original.len() {
            enc.encode(&original[pos..]).unwrap();
        }
        enc.finish().unwrap();
    }

    // Wire-level RFC 7845 assertions: the first page carries OpusHead with
    // the CELT algorithmic delay, and the final EOS page ends after that
    // delay so pre-skip removal recovers exactly `n_samples` frames.
    assert_eq!(&out[..4], b"OggS");
    let first_segments = out[26] as usize;
    let first_packet = &out[27 + first_segments..27 + first_segments + 19];
    let head = OpusHead::parse(first_packet).unwrap();
    assert_eq!(head.pre_skip, 120);
    let final_page = out
        .windows(4)
        .rposition(|window| window == b"OggS")
        .unwrap();
    let final_granule =
        i64::from_le_bytes(out[final_page + 6..final_page + 14].try_into().unwrap());
    assert_eq!(final_granule, n_samples as i64 + 120);

    let (pcm, mut reader) = decode_all_with(
        OggOpusReader::from_source(Box::new(Cursor::new(out))).unwrap(),
        960,
    );
    assert_eq!(reader.decoder().info().channels, 1, "decoded channel count");
    assert_eq!(
        pcm.len(),
        n_samples,
        "end-trim must recover exactly the encoded sample count"
    );

    // SNR is measured over the well-aligned, full-frame region only
    // (frames 2..5, skipping the first two atypical/warm-up frames and the
    // final partial frame whose zero-padded tail is a separate concern
    // from this test's point — exact-length recovery — already checked
    // above). 440 Hz at 48 kHz has a ~109-sample period, so the search
    // range is capped below that to avoid picking a spurious
    // same-shape-different-cycle peak.
    let region_start = 960 * 2;
    let region_end = 960 * 5;
    let mut best_snr = f64::NEG_INFINITY;
    for delay in 0..100i32 {
        let sig_pow: f64 = original[region_start..region_end]
            .iter()
            .map(|&v| (v as f64) * (v as f64))
            .sum();
        let err_pow: f64 = original[region_start..region_end]
            .iter()
            .enumerate()
            .map(|(i, &a)| {
                let di = region_start as i32 + i as i32 - delay;
                let b = if di >= 0 && (di as usize) < pcm.len() {
                    pcm[di as usize]
                } else {
                    0.0
                };
                let d = a as f64 - b as f64;
                d * d
            })
            .sum();
        let snr = 10.0 * (sig_pow / err_pow.max(1e-12)).log10();
        if snr > best_snr {
            best_snr = snr;
        }
    }
    assert!(
        best_snr > 12.0,
        "best-delay SNR too low for a real encode/decode round trip: {best_snr:.1} dB"
    );
}

/// Stereo: a tone hard-panned to the left channel decodes back with the
/// right channel measurably quieter (not duplicated/collapsed to mono).
#[test]
fn ogg_opus_encoder_stereo_round_trip_keeps_channels_distinguishable() {
    let sample_rate = 48_000u32;
    let freq_hz = 440.0f32;
    let n_frames = 960 * 4;
    let mut original = Vec::with_capacity(n_frames * 2);
    let mut phase = 0.0f32;
    for _ in 0..n_frames {
        let s = 0.5 * phase.sin();
        original.push(s);
        original.push(2e-3 * phase.sin()); // quiet, not exact-zero (see CBR-padding fix note)
        phase += 2.0 * std::f32::consts::PI * freq_hz / sample_rate as f32;
    }

    let mut out = Vec::new();
    {
        // 320 bytes/frame = 128 kbps for a 20 ms stereo CELT frame.
        // A broader budget matrix is covered by
        // `celt_encoder_cbr_stays_within_requested_budget` below.
        let mut enc = OggOpusEncoder::new(&mut out, sample_rate, 2, 128_000).unwrap();
        enc.encode(&original).unwrap();
        enc.finish().unwrap();
    }

    let (pcm, mut reader) = decode_all_with(
        OggOpusReader::from_source(Box::new(Cursor::new(out))).unwrap(),
        960,
    );
    assert_eq!(reader.decoder().info().channels, 2);
    assert_eq!(pcm.len(), n_frames * 2);

    let left_rms: f64 = (pcm
        .iter()
        .step_by(2)
        .map(|&v| (v as f64) * (v as f64))
        .sum::<f64>()
        / n_frames as f64)
        .sqrt();
    let right_rms: f64 = (pcm
        .iter()
        .skip(1)
        .step_by(2)
        .map(|&v| (v as f64) * (v as f64))
        .sum::<f64>()
        / n_frames as f64)
        .sqrt();
    assert!(left_rms > 0.1, "left channel should carry real signal");
    assert!(
        right_rms < left_rms * 0.1,
        "right (quiet) channel leaked too much energy from left: \
         left_rms={left_rms:.6} right_rms={right_rms:.6}"
    );
}

/// A stream with `finish()` called before any `encode()` call still
/// produces a valid, decodable (silent) container — the EOS page must land
/// on a real audio packet, not be skipped entirely.
#[test]
fn ogg_opus_encoder_empty_stream_still_produces_a_valid_container() {
    let mut out = Vec::new();
    {
        let mut enc = OggOpusEncoder::new(&mut out, 48_000, 1, 64_000).unwrap();
        enc.finish().unwrap();
    }
    let pcm = decode_all(&out, 960);
    assert_eq!(pcm.len(), 0, "no samples were ever encoded");
}

#[test]
fn ogg_opus_encoder_finish_is_idempotent() {
    let bytes = Arc::new(Mutex::new(Vec::new()));
    let mut enc = OggOpusEncoder::new(
        CountingWriter {
            bytes: Arc::clone(&bytes),
        },
        48_000,
        1,
        64_000,
    )
    .unwrap();
    enc.encode(&[0.0; 960]).unwrap();
    enc.finish().unwrap();
    let after_first = bytes.lock().unwrap().len();
    enc.finish().unwrap();
    assert_eq!(bytes.lock().unwrap().len(), after_first);
    drop(enc);
    let pcm = decode_all(&bytes.lock().unwrap(), 960);
    assert_eq!(pcm.len(), 960);
}

#[test]
fn celt_encoder_checked_api_rejects_invalid_pcm() {
    let mut enc = tpt_av_cadence_opus::celt::encoder::CeltEncoder::new(1, 3);
    let pcm = vec![0.0; enc.frame_len()];
    assert!(enc.try_encode_frame(&pcm, 160).is_ok());
    let mut invalid = pcm.clone();
    invalid[0] = f32::NAN;
    assert!(enc.try_encode_frame(&invalid, 160).is_err());
    invalid[0] = 1.01;
    assert!(enc.try_encode_frame(&invalid, 160).is_err());
    assert!(enc.try_encode_frame(&pcm[..pcm.len() - 1], 160).is_err());
    assert!(enc.try_encode_frame(&pcm, 160).is_ok());
}

#[test]
fn ogg_opus_encoder_rejects_invalid_pcm_values() {
    let mut out = Vec::new();
    let mut enc = OggOpusEncoder::new(&mut out, 48_000, 1, 64_000).unwrap();
    assert!(enc.encode(&[f32::NAN]).is_err());
    assert!(enc.encode(&[f32::INFINITY]).is_err());
    assert!(enc.encode(&[1.01]).is_err());
    assert!(enc.encode(&[-1.01]).is_err());
    assert!(enc.encode(&[-1.0, 0.0, 1.0]).is_ok());
}

#[test]
fn ogg_opus_encoder_rejects_encode_after_finish() {
    let mut out = Vec::new();
    let mut enc = OggOpusEncoder::new(&mut out, 48_000, 1, 64_000).unwrap();
    enc.finish().unwrap();
    assert!(enc.encode(&[0.0; 4]).is_err());
}

#[test]
fn ogg_opus_encoder_rejects_unsupported_sample_rate() {
    let mut out = Vec::new();
    assert!(OggOpusEncoder::new(&mut out, 44_100, 1, 64_000).is_err());
}

#[test]
fn ogg_opus_encoder_rejects_unsupported_channel_count() {
    let mut out = Vec::new();
    assert!(OggOpusEncoder::new(&mut out, 48_000, 3, 64_000).is_err());
}

/// The encoder's own `OpusHead` output round-trips through `OpusHead::parse`.
#[test]
fn ogg_opus_encoder_head_round_trips_through_parse() {
    let head = OpusHead {
        version: 1,
        channels: 2,
        pre_skip: 0,
        input_sample_rate: 48_000,
        output_gain_q8: 0,
        mapping_family: 0,
    };
    let bytes = head.write();
    let parsed = OpusHead::parse(&bytes).unwrap();
    assert_eq!(parsed, head);
}

/// Regression: CELT CBR output must use libopus-style fixed-size entropy
/// storage. The previous unbounded serializer emitted the final zero carry
/// byte and a separate partial raw byte, adding two physical bytes beyond the
/// allocation-derived packet size. The decoder then derived a different PVQ
/// allocation and could silently return corrupted audio.
#[test]
fn celt_encoder_cbr_stays_within_requested_budget() {
    for &(channels, bytes_per_frame) in &[
        (1usize, 100usize),
        (1, 160),
        (1, 240),
        (2, 160),
        (2, 240),
        (2, 320),
    ] {
        let mut enc = tpt_av_cadence_opus::celt::encoder::CeltEncoder::new(channels, 3);
        let mut dec =
            tpt_av_cadence_opus::celt::decoder::CeltDecoder::new(channels, 48_000).unwrap();
        let mut phase = 0.0f32;
        for _ in 0..8 {
            let mut pcm = vec![0.0f32; 960 * channels];
            for sample in pcm.chunks_mut(channels) {
                for value in sample {
                    *value = 0.5 * phase.sin();
                }
                phase += 2.0 * std::f32::consts::PI * 440.0 / 48_000.0;
            }
            let packet = enc.encode_frame(&pcm, bytes_per_frame);
            assert_eq!(
                packet.len() - 1,
                bytes_per_frame,
                "channels={channels} budget={bytes_per_frame}"
            );
            let mut decoded = vec![0.0f32; 960 * channels];
            dec.decode(Some(&packet[1..]), 960, &mut decoded).unwrap();
        }
    }
}
