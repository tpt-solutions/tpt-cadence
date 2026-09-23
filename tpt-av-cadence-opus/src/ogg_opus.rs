//! Ogg Opus (RFC 7845) container: `OpusHead`/`OpusTags` header parsing and
//! a [`Decoder`](tpt_av_cadence_core::Decoder)/[`FormatReader`]
//! implementation over a `.opus`/`.ogg` byte source.
//!
//! The container layer ([`tpt_av_cadence_ogg::PageReader`]) reassembles
//! complete packets; this module maps them onto the packet API: the first
//! packet must be an `OpusHead` (on the BOS page), the second an
//! `OpusTags` (validated and skipped), and every subsequent packet is
//! parsed and decoded through [`OpusDecoder`] — SILK-only, CELT-only, and
//! hybrid payloads alike.
//!
//! RFC 7845 bookkeeping implemented here:
//!
//! - **Pre-skip**: the first `OpusHead::pre_skip` decoded samples (at the
//!   always-48 kHz output rate) are discarded before any audio is emitted.
//! - **Granule/end-trim**: a page's granule position is the absolute
//!   48 kHz sample position of the end of its last complete packet; the
//!   final packet's synthesis is trimmed at the EOS page's granule
//!   (applied per completing packet, mirroring opusfile — an EOS page
//!   that carries no packets can therefore not trim audio that was
//!   already emitted, so conforming muxers set the EOS flag on the final
//!   audio page). A `-1` (unset) granule bounds nothing.
//! - **Output gain**: `OpusHead::output_gain_q8` (Q7.8 dB) is applied to
//!   the decoded PCM, mirroring libopusfile's default `OPUS_SET_GAIN`
//!   behavior.
//!
//! Only the first logical link of a chained stream is decoded; the link
//! ends at the next BOS page (matching the Vorbis reader's behavior).
//! Channel mapping families 0 and 1 are accepted for 1–2 channel streams
//! with the trivial mapping; wider layouts are rejected (the underlying
//! [`OpusDecoder`] renders 1 or 2 channels).

use std::io::Read;

use tpt_av_cadence_core::{
    BufferedSource, ByteSource, CadenceError, Decoder, Format, FormatReader, Result, StreamInfo,
    Unseekable,
};
use tpt_av_cadence_ogg::PageReader;

use crate::decoder::OpusDecoder;
use crate::packet::parse_packet;

/// Output sample rate of every Opus stream (RFC 7845 §2: granule positions
/// and pre-skip are always counted at 48 kHz).
const OUTPUT_RATE: u32 = 48_000;
/// Packet buffer ceiling. A legal Opus packet is bounded by 1275 bytes per
/// frame × 48 frames ≈ 61 KiB; this admits every legal packet plus
/// framing slack while still catching gross corruption.
const MAX_PACKET: usize = 1 << 16;
/// Maximum decoded samples per channel from one packet: 120 ms at 48 kHz
/// (the RFC 6716 packet-duration cap).
const MAX_PACKET_FRAMES: usize = 120 * 48;
/// Gain factor constant from libopus's `opus_decode_frame`
/// (`QCONST16(6.48814081e-4f, 25)`): `2^(gain_q8 · c)` realizes the Q7.8
/// dB output gain.
const GAIN_Q8_FACTOR: f32 = 6.488_141e-4;

fn corrupt(what: &str) -> CadenceError {
    CadenceError::CorruptData(format!("ogg opus: {what}"))
}

/// The parsed `OpusHead` identification header (RFC 7845 §5.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpusHead {
    /// Version byte; only 1 is defined.
    pub version: u8,
    /// Output channel count (1 or 2 for this decoder).
    pub channels: u16,
    /// Samples at 48 kHz to discard from the decoder's output start.
    pub pre_skip: u16,
    /// Original input sample rate (informational; not the output rate).
    pub input_sample_rate: u32,
    /// Q7.8 dB gain to apply to the decoded output.
    pub output_gain_q8: i16,
    /// Channel mapping family (0 = mono/stereo, 1 = explicit mapping).
    pub mapping_family: u8,
}

impl OpusHead {
    /// Parses the 19-byte (family 0) or 21+C-byte (family 1) header block
    /// carried by the first packet of the stream.
    pub fn parse(data: &[u8]) -> Result<OpusHead> {
        if data.len() < 19 || &data[0..8] != b"OpusHead" {
            return Err(corrupt("missing OpusHead magic"));
        }
        if data[8] != 1 {
            return Err(CadenceError::UnsupportedFeature(format!(
                "OpusHead version {} is not supported (only 1)",
                data[8]
            )));
        }
        let channels = data[9] as u16;
        if !(1..=8).contains(&channels) {
            return Err(corrupt("OpusHead channel count must be 1..=8"));
        }
        let head = OpusHead {
            version: data[8],
            channels,
            pre_skip: u16::from_le_bytes([data[10], data[11]]),
            input_sample_rate: u32::from_le_bytes([data[12], data[13], data[14], data[15]]),
            output_gain_q8: i16::from_le_bytes([data[16], data[17]]),
            mapping_family: data[18],
        };
        match head.mapping_family {
            0 => {}
            1 => {
                // Family 1: stream count, coupled stream count, then one
                // mapping byte per channel. This decoder only renders the
                // trivial 1–2 channel mappings (mono → 1 uncoupled stream;
                // stereo → 1 coupled stream).
                if data.len() < 21 + head.channels as usize {
                    return Err(corrupt("truncated channel mapping table"));
                }
                let streams = data[19];
                let coupled = data[20];
                if coupled > streams || streams == 0 {
                    return Err(corrupt("invalid stream/coupled counts"));
                }
                let trivial = match head.channels {
                    1 => streams == 1 && coupled == 0 && data[21] == 0,
                    2 => streams == 1 && coupled == 1 && data[21] == 0 && data[22] == 1,
                    _ => false,
                };
                if !trivial {
                    return Err(CadenceError::UnsupportedFeature(
                        "only the trivial 1–2 channel Ogg Opus mappings are supported".to_string(),
                    ));
                }
            }
            other => {
                return Err(CadenceError::UnsupportedFeature(format!(
                    "channel mapping family {other} is not supported"
                )));
            }
        }
        if head.channels > 2 {
            return Err(CadenceError::UnsupportedFeature(format!(
                "{} channel Ogg Opus streams are not supported (1–2 only)",
                head.channels
            )));
        }
        Ok(head)
    }

    /// Serializes the 19-byte family-0 (mono/stereo, trivial mapping)
    /// identification header block — the exact bijective inverse of
    /// [`OpusHead::parse`] for the subset this crate's encoder produces
    /// (`mapping_family == 0`, so no channel mapping table follows).
    pub fn write(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(19);
        out.extend_from_slice(b"OpusHead");
        out.push(self.version);
        out.push(self.channels as u8);
        out.extend_from_slice(&self.pre_skip.to_le_bytes());
        out.extend_from_slice(&self.input_sample_rate.to_le_bytes());
        out.extend_from_slice(&self.output_gain_q8.to_le_bytes());
        out.push(self.mapping_family);
        out
    }
}

/// Ogg Opus decoder: pulls packets from the container, decodes them with
/// one persistent [`OpusDecoder`], and stages the trimmed PCM for
/// allocation-free [`Decoder::decode`] calls.
pub struct OggOpusDecoder {
    opus: OpusDecoder,
    ogg: PageReader,
    info: StreamInfo,
    head: OpusHead,
    /// Packet bytes currently being decoded (filled by `next_packet`).
    packet: Box<[u8]>,
    /// Decoded-but-unemitted PCM, interleaved (`pcm_staged_frames` frames
    /// starting at granule position `decoded_total - pcm_staged_frames`,
    /// pre-skip already removed).
    pcm_staged: Box<[f32]>,
    pcm_staged_frames: usize,
    pcm_staged_pos: usize,
    /// Total frames decoded so far, untrimmed — the granule-space position
    /// of the next packet's output start.
    decoded_total: u64,
    /// Granule position of the EOS page's last complete packet, once seen:
    /// emitted audio stops (and the final partial samples are trimmed)
    /// here.
    stream_end: Option<u64>,
    /// Frames emitted past pre-skip (what `seek` counts in).
    delivered_total: u64,
    /// Scratch for decode-and-discard seeks.
    seek_scratch: Box<[f32]>,
    /// Linear gain factor from `output_gain_q8`, or 1.0 when unset.
    gain: f32,
}

impl OggOpusDecoder {
    /// Parses the two header packets from the current stream position into
    /// `buf` (which must be at least [`MAX_PACKET`]): the first must be an
    /// `OpusHead` on a BOS page, the second an `OpusTags`.
    fn read_headers(ogg: &mut PageReader, buf: &mut [u8]) -> Result<OpusHead> {
        let (len, meta) = ogg
            .next_packet(buf)?
            .ok_or_else(|| corrupt("stream ended before OpusHead"))?;
        if !meta.bos_page {
            return Err(corrupt("first packet is not on a BOS page"));
        }
        let head = OpusHead::parse(&buf[..len])?;

        let (len, _) = ogg
            .next_packet(buf)?
            .ok_or_else(|| corrupt("stream ended before OpusTags"))?;
        if len < 8 || &buf[..8] != b"OpusTags" {
            return Err(corrupt("second packet is not OpusTags"));
        }
        Ok(head)
    }

    /// Re-establishes the header state after a seek rewound the source.
    ///
    /// A page reader reads ahead whole pages, so the byte offset after the
    /// tags packet cannot be recorded exactly; instead a seek restarts from
    /// byte 0 and re-parses the (tiny) header pages.
    fn rewind_and_reopen(&mut self) -> Result<()> {
        self.ogg.restart()?;
        self.head = Self::read_headers(&mut self.ogg, &mut self.packet)?;
        self.opus = OpusDecoder::new(self.head.channels as usize)?;
        self.pcm_staged_frames = 0;
        self.pcm_staged_pos = 0;
        self.decoded_total = 0;
        self.stream_end = None;
        self.delivered_total = 0;
        Ok(())
    }

    /// Records the stream's end at an EOS page's granule (which also fixes
    /// `total_frames`). A `-1` granule (no packet ends on the page) bounds
    /// nothing.
    fn note_eos_page(&mut self, granule: i64) {
        if granule < 0 {
            return;
        }
        let end = granule as u64;
        if self.stream_end.is_none() {
            self.stream_end = Some(end);
            self.info.total_frames = Some(end.saturating_sub(self.head.pre_skip as u64));
        }
    }

    /// Pulls and decodes the next audio packet into the staging buffer
    /// (pre-skip removed, output gain applied). Returns `false` at end of
    /// stream; when it returns `true` the staging buffer is non-empty.
    fn stage_next_packet(&mut self) -> Result<bool> {
        let channels = self.head.channels as usize;
        loop {
            let Some((len, meta)) = self.ogg.next_packet(&mut self.packet)? else {
                return Ok(false);
            };
            if meta.eos_page && meta.granule >= 0 {
                self.note_eos_page(meta.granule);
            }
            if len == 0 {
                // RFC 7845 forbids empty packets; skip them (libvorbis-style
                // tolerance) rather than fail the stream.
                continue;
            }
            let payload = &self.packet[..len];
            let packet = parse_packet(payload)?;
            let n = self.opus.decode_packet(
                &packet,
                payload,
                &mut self.pcm_staged[..MAX_PACKET_FRAMES * channels],
            )?;
            let start = self.decoded_total;
            self.decoded_total += n as u64;

            // Window this packet's output: skip pre-skip at the stream
            // start, and stop at the EOS page's granule — the end trim for
            // the final packet's synthesis running past the encoder's last
            // full sample. (The trim is applied per completing packet,
            // mirroring opusfile: an EOS page that carries no packets
            // cannot trim audio that has already been emitted.)
            let out_start = start.max(self.head.pre_skip as u64);
            let skip = (out_start - start) as usize;
            let mut out_frames = n - skip;
            if let Some(end) = self.stream_end {
                out_frames = out_frames.min(end.saturating_sub(out_start) as usize);
            }
            if out_frames > 0 {
                if skip > 0 {
                    self.pcm_staged
                        .copy_within(skip * channels..n * channels, 0);
                }
                if self.gain != 1.0 {
                    for s in &mut self.pcm_staged[..out_frames * channels] {
                        *s *= self.gain;
                    }
                }
                self.pcm_staged_frames = out_frames;
                self.pcm_staged_pos = 0;
                return Ok(true);
            }
            // Whole packet fell inside the pre-skip/end-trim regions: keep
            // pulling until audio or EOF.
        }
    }
}

impl Decoder for OggOpusDecoder {
    fn info(&self) -> &StreamInfo {
        &self.info
    }

    fn seek(&mut self, frame: u64) -> Result<()> {
        // Linear decode-and-discard from the headers (correct but not
        // real-time safe; MAY block on the source).
        self.rewind_and_reopen()?;
        let channels = self.head.channels as usize;
        while self.delivered_total < frame {
            let want = ((frame - self.delivered_total) as usize)
                .min(self.seek_scratch.len() / channels)
                * channels;
            let mut scratch = std::mem::take(&mut self.seek_scratch);
            let got = self.decode(&mut scratch[..want]);
            self.seek_scratch = scratch;
            if got? == 0 {
                return Err(CadenceError::CorruptData(
                    "stream ended while seeking".into(),
                ));
            }
        }
        Ok(())
    }

    fn decode(&mut self, buffer: &mut [f32]) -> Result<usize> {
        let channels = self.head.channels as usize;
        if buffer.len() % channels != 0 {
            return Err(CadenceError::InvalidFormat(format!(
                "buffer length {} is not a multiple of the channel count {}",
                buffer.len(),
                channels
            )));
        }
        let want = buffer.len() / channels;
        let mut written = 0;
        while written < want {
            if self.pcm_staged_pos >= self.pcm_staged_frames && !self.stage_next_packet()? {
                break;
            }
            // The staged window may have been capped by the end trim;
            // never emit past the EOS granule (in pre-skip-free frames).
            let remaining_to_end = match self.stream_end {
                Some(end) => {
                    let limit = (end.saturating_sub(self.head.pre_skip as u64)) as usize;
                    limit.saturating_sub(self.delivered_total as usize)
                }
                None => usize::MAX,
            };
            if remaining_to_end == 0 {
                break;
            }
            let available = self.pcm_staged_frames - self.pcm_staged_pos;
            let n = (want - written).min(available).min(remaining_to_end);
            let src = self.pcm_staged_pos * channels;
            buffer[written * channels..(written + n) * channels]
                .copy_from_slice(&self.pcm_staged[src..src + n * channels]);
            self.pcm_staged_pos += n;
            self.delivered_total += n as u64;
            written += n;
        }
        Ok(written)
    }
}

impl std::fmt::Debug for OggOpusDecoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OggOpusDecoder")
            .field("info", &self.info)
            .field("head", &self.head)
            .field("delivered_total", &self.delivered_total)
            .finish()
    }
}

/// Reader wrapper implementing [`FormatReader`] for Ogg Opus streams.
pub struct OggOpusReader {
    decoder: OggOpusDecoder,
}

impl OggOpusReader {
    /// Opens a seekable source directly (in-memory cursors, files).
    pub fn from_source(source: Box<dyn ByteSource>) -> Result<Self> {
        let mut buf = vec![0u8; MAX_PACKET].into_boxed_slice();
        let mut ogg = PageReader::new(BufferedSource::new(source, 32 * 1024), MAX_PACKET);
        let head = OggOpusDecoder::read_headers(&mut ogg, &mut buf)?;
        Self::from_parts(ogg, head)
    }

    fn from_parts(ogg: PageReader, head: OpusHead) -> Result<Self> {
        let channels = head.channels as usize;
        let gain = if head.output_gain_q8 == 0 {
            1.0
        } else {
            (head.output_gain_q8 as f32 * GAIN_Q8_FACTOR).exp2()
        };
        let info = StreamInfo::new(Format::Opus, OUTPUT_RATE, head.channels, 16);
        info.validate()?;
        // The EOS granule (when it exists) fixes the stream length.
        let decoder = OggOpusDecoder {
            opus: OpusDecoder::new(channels)?,
            ogg,
            info,
            head,
            packet: vec![0u8; MAX_PACKET].into_boxed_slice(),
            pcm_staged: vec![0.0; MAX_PACKET_FRAMES * channels].into_boxed_slice(),
            pcm_staged_frames: 0,
            pcm_staged_pos: 0,
            decoded_total: 0,
            stream_end: None,
            delivered_total: 0,
            seek_scratch: vec![0.0; MAX_PACKET_FRAMES * channels].into_boxed_slice(),
            gain,
        };
        Ok(OggOpusReader { decoder })
    }
}

impl FormatReader for OggOpusReader {
    fn open(source: Box<dyn Read + Send>) -> Result<Self> {
        Self::from_source(Box::new(Unseekable(source)))
    }

    fn decoder(&mut self) -> &mut dyn Decoder {
        &mut self.decoder
    }

    fn info(&self) -> &StreamInfo {
        self.decoder.info()
    }
}
