//! Top-level MP3 stream decoder: frame sync, bit reservoir, granule pipeline,
//! and the [`Decoder`]/[`FormatReader`] implementations.

use std::io::Read;

use tpt_av_cadence_core::{
    BufferedSource, ByteSource, CadenceError, Decoder, Format, FormatReader, StreamInfo, Unseekable,
};

use crate::bitreader::BitReader;
use crate::header::{crc16, is_valid_header, parse_header, FrameHeader};
use crate::huffman;
use crate::imdct;
use crate::processing;
use crate::scalefac;
use crate::sideinfo::{self, GranuleInfo};
use crate::stereo;
use crate::synth;

/// Bit reservoir cap (`main_data_begin` is 9 bits).
const MAX_BITRESERVOIR_BYTES: usize = 511;
/// Largest legal (non-free-format) Layer III frame payload plus slack.
const MAX_L3_FRAME_PAYLOAD_BYTES: usize = 1440;
/// Side information byte sizes, `[mono, stereo]` per row: row 0 is the
/// MPEG-2/2.5 (LSF) family, row 1 MPEG-1.
const SIDE_INFO: [[usize; 2]; 2] = [[9, 17], [17, 32]];

/// Decoder for raw MPEG-1/2/2.5 Layer III streams (a leading ID3v2 tag is
/// skipped automatically).
///
/// Codec working buffers are allocated by [`Mp3Decoder::from_source`]/
/// [`Mp3Decoder::open`]. `decode()` itself is allocation-free and
/// panic-free: every granule/synthesis scratch lives in the struct, the
/// bit reservoir is clamped to [`MAX_BITRESERVOIR_BYTES`], and side info
/// demanding more reservoir than exists drops the frame instead of
/// overreading. Error construction can allocate, and source reads may
/// block. Output is interleaved f32 and is not clipped.
pub struct Mp3Decoder {
    source: BufferedSource,
    info: StreamInfo,
    /// Byte offset of the first audio frame (seek reset point).
    audio_start: u64,

    /// Input window over the source stream.
    window: Box<[u8]>,
    window_valid: usize,
    window_pos: usize,
    source_eof: bool,

    /// Persistent codec state.
    mdct_overlap: Box<[[f32; 288]; 2]>,
    qmf_state: Box<[f32; 960]>,
    reserv: usize,
    reserv_buf: Box<[u8; MAX_BITRESERVOIR_BYTES]>,

    /// Per-frame scratch (allocated once).
    maindata: Box<[u8; MAX_BITRESERVOIR_BYTES + MAX_L3_FRAME_PAYLOAD_BYTES]>,
    granules: [GranuleInfo; 4],
    grbuf: Box<[f32; 1152]>,
    scf: Box<[f32; 40]>,
    lins: Box<[f32; 33 * 64]>,
    ist_pos: Box<[[u8; 39]; 2]>,
    reorder_scratch: Box<[f32; 576]>,

    /// Decoded PCM awaiting hand-out (interleaved).
    pcm_staged: Box<[f32; 2304]>,
    staged_frames: usize,
    staged_pos: usize,
    frames_out: u64,

    /// Scratch for decode-and-discard seeking.
    seek_scratch: Box<[f32]>,
}

impl Mp3Decoder {
    /// Opens an MP3 stream over a byte source. Seekable sources
    /// (`Read + Seek + Send`) get working `seek()`.
    pub fn from_source(source: Box<dyn ByteSource>) -> Result<Self, CadenceError> {
        let mut source = BufferedSource::new(source, 16 * 1024);
        let (mut audio_start, mut probe, mut valid) = skip_id3v2(&mut source)?;

        // Probe for the first frame to learn the stream parameters. Bytes
        // pulled during the probe are carried over into the decoder window
        // so unseekable sources keep working.
        probe.resize(8 * 1024, 0);
        let mut probe_eof = false;
        let mut found: Option<(usize, FrameHeader)> = None;
        loop {
            if !probe_eof && valid < probe.len() {
                match source.read(&mut probe[valid..]) {
                    Ok(0) => probe_eof = true,
                    Ok(n) => valid += n,
                    Err(e) => return Err(CadenceError::from(e)),
                }
                continue;
            }
            // At EOF (`allow_tail`) the buffer may hold only a few frames;
            // still scan it, accepting a tail candidate without a successor.
            match find_first_frame(&probe[..valid], probe_eof) {
                Some(hit) => {
                    found = Some(hit);
                    break;
                }
                None if probe_eof => break,
                None => {
                    // Keep a complete candidate frame plus its successor's
                    // header: retaining only three bytes loses frames whose
                    // successor falls beyond the current probe window.
                    let keep = MAX_L3_FRAME_PAYLOAD_BYTES + 4;
                    let discarded = valid - keep;
                    probe.copy_within(discarded..valid, 0);
                    audio_start += discarded as u64;
                    valid = keep;
                }
            }
        }
        let (off, hdr) = found.ok_or_else(|| {
            CadenceError::InvalidFormat("no MPEG Layer III frame found in stream".into())
        })?;
        // Account for bytes before the first frame within this probe window
        // so seek() resets to the real first frame.
        audio_start += off as u64;

        let channels: usize = if hdr.mono { 1 } else { 2 };
        let info = StreamInfo::new(Format::Mp3, hdr.sample_rate_hz, channels as u16, 16);
        info.validate()?;

        let mut window = vec![0u8; 8 * 1024].into_boxed_slice();
        let carry = valid - off;
        window[..carry].copy_from_slice(&probe[off..valid]);

        Ok(Mp3Decoder {
            source,
            info,
            audio_start,
            window,
            window_valid: carry,
            window_pos: 0,
            source_eof: probe_eof,
            mdct_overlap: Box::new([[0.0; 288]; 2]),
            qmf_state: Box::new([0.0; 960]),
            reserv: 0,
            reserv_buf: Box::new([0; MAX_BITRESERVOIR_BYTES]),
            maindata: Box::new([0; MAX_BITRESERVOIR_BYTES + MAX_L3_FRAME_PAYLOAD_BYTES]),
            granules: Default::default(),
            grbuf: Box::new([0.0; 1152]),
            scf: Box::new([0.0; 40]),
            lins: Box::new([0.0; 33 * 64]),
            ist_pos: Box::new([[0; 39]; 2]),
            reorder_scratch: Box::new([0.0; 576]),
            pcm_staged: Box::new([0.0; 2304]),
            staged_frames: 0,
            staged_pos: 0,
            frames_out: 0,
            seek_scratch: vec![0.0f32; channels * 576].into_boxed_slice(),
        })
    }

    /// Convenience constructor over a plain readable (unseekable) source.
    pub fn open(source: Box<dyn Read + Send>) -> Result<Self, CadenceError> {
        Self::from_source(Box::new(Unseekable(source)))
    }

    /// Resets all codec state (fresh stream position).
    fn reset_stream_state(&mut self) {
        for ch in self.mdct_overlap.iter_mut() {
            ch.fill(0.0);
        }
        self.qmf_state.fill(0.0);
        self.reserv = 0;
        self.staged_frames = 0;
        self.staged_pos = 0;
        self.frames_out = 0;
        self.window_valid = 0;
        self.window_pos = 0;
        self.source_eof = false;
    }

    /// Refills the window, compacting consumed bytes to the front.
    fn fill_window(&mut self) -> Result<(), CadenceError> {
        self.window
            .copy_within(self.window_pos..self.window_valid, 0);
        self.window_valid -= self.window_pos;
        self.window_pos = 0;
        while self.window_valid < self.window.len() && !self.source_eof {
            let n = self
                .source
                .read(&mut self.window[self.window_valid..])
                .map_err(CadenceError::from)?;
            if n == 0 {
                self.source_eof = true;
                break;
            }
            self.window_valid += n;
        }
        Ok(())
    }

    /// Refills once; returns true only when more buffered bytes are available.
    /// EOF without progress yields false instead of looping on a partial frame.
    fn progress_or_eof(&mut self) -> Result<bool, CadenceError> {
        let before = self.window_valid - self.window_pos;
        self.fill_window()?;
        let after = self.window_valid - self.window_pos;
        Ok(after > before)
    }

    /// Decodes the next frame into `pcm_staged`. Returns Ok(false) at end of
    /// stream or when no decodable frame remains.
    fn decode_next_frame(&mut self) -> Result<bool, CadenceError> {
        loop {
            if self.window_valid - self.window_pos < 4 && !self.source_eof {
                self.fill_window()?;
            }
            if self.window_pos >= self.window_valid {
                return Ok(false); // clean EOF
            }

            // Scan for the next frame sync.
            let scan_end = self.window_valid.saturating_sub(3);
            let mut i = self.window_pos;
            while i < scan_end && !(self.window[i] == 0xFF && is_valid_header(&self.window[i..])) {
                i += 1;
            }
            if i >= scan_end {
                if self.source_eof {
                    return Ok(false);
                }
                // Retain the final 3 bytes (a partial candidate).
                self.window_pos = self.window_valid.saturating_sub(3);
                if !self.progress_or_eof()? {
                    return Ok(false);
                }
                continue;
            }
            let sync = i;
            self.window_pos = sync;

            let hdr = match parse_header(&self.window[sync..]) {
                Ok(h) => h,
                Err(_) => {
                    self.window_pos = sync + 1;
                    continue;
                }
            };
            let total = hdr.total_bytes();
            if sync + total > self.window_valid {
                if !self.progress_or_eof()? {
                    // Truncated trailing frame (or window exhausted): drop.
                    self.window_pos = self.window_valid;
                    continue;
                }
                continue;
            }

            // Frames whose geometry disagrees with the stream we opened are
            // treated as garbage (mid-stream parameter changes unsupported).
            if hdr.sample_rate_hz != self.info.sample_rate || hdr.mono != (self.info.channels == 1)
            {
                self.window_pos = sync + total;
                continue;
            }

            match self.decode_frame(&hdr, sync) {
                Ok(frames) => {
                    self.window_pos = sync + total;
                    if frames == 0 {
                        continue;
                    }
                    self.staged_frames = frames;
                    self.staged_pos = 0;
                    return Ok(true);
                }
                Err(_) => {
                    // Corrupt frame: resynchronize after it.
                    self.window_pos = sync + total;
                    continue;
                }
            }
        }
    }

    /// Runs the full Layer III pipeline for the frame at `sync`, staging
    /// interleaved PCM. Returns the number of frames staged (0 = dropped,
    /// e.g. missing bit reservoir).
    fn decode_frame(&mut self, hdr: &FrameHeader, sync: usize) -> Result<usize, CadenceError> {
        let nch: usize = if hdr.mono { 1 } else { 2 };
        let si_len = SIDE_INFO[hdr.mpeg1 as usize][nch - 1];
        let hdr_len = 4 + (hdr.crc as usize) * 2;
        if hdr.total_bytes() < hdr_len + si_len {
            return Err(CadenceError::CorruptData(
                "truncated side information".into(),
            ));
        }

        if hdr.crc {
            let stored = u16::from_be_bytes([self.window[sync + 4], self.window[sync + 5]]);
            // Only the final two header bytes and side information are
            // protected, not the intervening stored CRC or main data.
            let mut covered = [0u8; 2 + 32];
            covered[..2].copy_from_slice(&self.window[sync + 2..sync + 4]);
            covered[2..2 + si_len]
                .copy_from_slice(&self.window[sync + hdr_len..sync + hdr_len + si_len]);
            if crc16(&covered[..2 + si_len]) != stored {
                return Err(CadenceError::CorruptData("header CRC-16 mismatch".into()));
            }
        }

        // Side information.
        let frame_end = sync + hdr.total_bytes();
        let payload_off = sync + hdr_len;
        let mut bs = BitReader::new(&self.window[payload_off..frame_end]);
        let main_data_begin = sideinfo::read_side_info(&mut bs, hdr, &mut self.granules)?;

        // Bit reservoir: prepend up to main_data_begin bytes from the
        // previous frame's leftovers, then this frame's remaining payload.
        let frame_bytes = (bs.limit_bits() - bs.bit_pos()) / 8;
        let bytes_have = self.reserv.min(main_data_begin as usize);
        let src_off = self.reserv.saturating_sub(main_data_begin as usize);
        let md_len = bytes_have + frame_bytes;
        let success = self.reserv >= main_data_begin as usize;
        {
            let reserv: &[u8] = &self.reserv_buf[..];
            let maindata: &mut [u8] = &mut self.maindata[..];
            maindata[..bytes_have].copy_from_slice(&reserv[src_off..src_off + bytes_have]);
            let payload = (bs.bit_pos()) / 8;
            maindata[bytes_have..md_len].copy_from_slice(
                &self.window[payload_off + payload..payload_off + payload + frame_bytes],
            );
        }

        let n_granules = if hdr.mpeg1 { 2 } else { 1 };
        let mut frames = 0usize;
        let md_end_bits;
        {
            let granules: &[GranuleInfo] = &self.granules;
            let mut md_bits = BitReader::new(&self.maindata[..md_len]);
            let mut pcm_off = 0usize;
            for igr in 0..n_granules {
                if success {
                    self.grbuf.fill(0.0);
                    decode_granule(
                        hdr,
                        &granules[igr * nch..igr * nch + nch],
                        nch,
                        &mut md_bits,
                        &self.maindata[..md_len],
                        &mut self.grbuf[..],
                        &mut self.scf,
                        &mut self.ist_pos,
                        &mut self.reorder_scratch[..],
                        &mut self.mdct_overlap,
                    );
                    synth::synth_granule(
                        &mut self.qmf_state,
                        &mut self.grbuf[..],
                        nch,
                        &mut self.pcm_staged[pcm_off..pcm_off + 576 * nch],
                        &mut self.lins[..],
                    );
                    pcm_off += 576 * nch;
                    frames += 576;
                }
            }
            md_end_bits = md_bits.bit_pos();
        }

        // Save the unconsumed main data for the next frame's reservoir.
        let mut pos_byte = if success { md_end_bits.div_ceil(8) } else { 0 };
        let mut remains = md_len.saturating_sub(pos_byte);
        if remains > MAX_BITRESERVOIR_BYTES {
            pos_byte += remains - MAX_BITRESERVOIR_BYTES;
            remains = MAX_BITRESERVOIR_BYTES;
        }
        self.reserv_buf[..remains].copy_from_slice(&self.maindata[pos_byte..pos_byte + remains]);
        self.reserv = remains;
        let _ = frame_end;

        Ok(frames)
    }
}

/// Per-granule pipeline: scalefactors + Huffman per channel, joint stereo,
/// then reorder / antialias / IMDCT / sign fix per channel.
#[allow(clippy::too_many_arguments)]
fn decode_granule(
    hdr: &FrameHeader,
    gr_info: &[GranuleInfo],
    nch: usize,
    bs: &mut BitReader,
    maindata: &[u8],
    grbuf: &mut [f32],
    scf: &mut [f32; 40],
    ist_pos: &mut [[u8; 39]; 2],
    reorder_scratch: &mut [f32],
    mdct_overlap: &mut [[f32; 288]; 2],
) {
    for ch in 0..nch {
        let granule_limit = bs.bit_pos() as i64 + gr_info[ch].part_23_length as i64;
        scalefac::decode_scalefactors(hdr, &mut ist_pos[ch], bs, &gr_info[ch], scf, ch);
        let pos = bs.bit_pos();
        let end = huffman::huffman(
            &mut grbuf[ch * 576..ch * 576 + 576],
            maindata,
            pos,
            &gr_info[ch],
            scf,
            granule_limit,
        );
        bs.set_bit_pos(end);
    }

    if hdr.i_stereo {
        stereo::intensity_stereo(grbuf, &ist_pos[1], gr_info, hdr);
    } else if hdr.ms_stereo {
        stereo::midside(grbuf);
    }

    for ch in 0..nch {
        let gr = &gr_info[ch];
        let mut aa_bands: i32 = 31;
        let n_long_bands = sideinfo::mixed_long_bands(hdr, gr.mixed_block_flag);
        if gr.n_short_sfb != 0 {
            aa_bands = n_long_bands as i32 - 1;
            processing::reorder(
                &mut grbuf[ch * 576 + n_long_bands * 18..ch * 576 + 576],
                reorder_scratch,
                &gr.sfbtab[gr.n_long_sfb as usize..],
            );
        }
        if aa_bands > 0 {
            processing::antialias(&mut grbuf[ch * 576..ch * 576 + 576], aa_bands as usize);
        }
        imdct::imdct_gr(
            &mut grbuf[ch * 576..ch * 576 + 576],
            &mut mdct_overlap[ch],
            gr.block_type,
            n_long_bands,
        );
        imdct::change_sign(&mut grbuf[ch * 576..ch * 576 + 576]);
    }
}

/// Skips a leading ID3v2 tag. Returns the byte offset where audio starts
/// plus any bytes already pulled from the source (they must seed the
/// first-frame probe, since the source cannot rewind them for unseekable
/// streams).
fn skip_id3v2(source: &mut BufferedSource) -> Result<(u64, Vec<u8>, usize), CadenceError> {
    let mut probe = [0u8; 10];
    let n = read_up_to(source, &mut probe)?;
    if n >= 10 && &probe[..3] == b"ID3" {
        let size = ((probe[6] as u64 & 0x7F) << 21)
            | ((probe[7] as u64 & 0x7F) << 14)
            | ((probe[8] as u64 & 0x7F) << 7)
            | (probe[9] as u64 & 0x7F);
        let footer = u64::from(probe[5] & 0x10 != 0);
        source.skip(size + footer * 10)?;
        return Ok((10 + size + footer * 10, Vec::new(), 0));
    }
    Ok((0, probe.to_vec(), n))
}

fn read_up_to(source: &mut BufferedSource, out: &mut [u8]) -> Result<usize, CadenceError> {
    let mut done = 0;
    while done < out.len() {
        match source.read(&mut out[done..]) {
            Ok(0) => break,
            Ok(n) => done += n,
            Err(e) => return Err(CadenceError::from(e)),
        }
    }
    Ok(done)
}

/// Finds the offset of the first frame whose header is followed by a
/// plausibly matching successor (minimp3's frame-matching heuristic).
/// With `allow_tail`, a candidate at the buffer tail is accepted without a
/// successor check (tiny streams at end of input).
fn find_first_frame(buf: &[u8], allow_tail: bool) -> Option<(usize, FrameHeader)> {
    for i in 0..buf.len().saturating_sub(4) {
        if buf[i] != 0xFF || !is_valid_header(&buf[i..]) {
            continue;
        }
        let hdr = match parse_header(&buf[i..]) {
            Ok(h) => h,
            Err(_) => continue,
        };
        let total = hdr.total_bytes();
        if i + total + 4 > buf.len() {
            if allow_tail {
                return Some((i, hdr));
            }
            continue;
        }
        if is_valid_header(&buf[i + total..]) {
            if let Ok(next) = parse_header(&buf[i + total..]) {
                let same_family = ((hdr.bytes[1] ^ next.bytes[1]) & 0xFE) == 0
                    && ((hdr.bytes[2] ^ next.bytes[2]) & 0x0C) == 0;
                if same_family {
                    return Some((i, hdr));
                }
            }
        }
    }
    None
}

impl std::fmt::Debug for Mp3Decoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Mp3Decoder")
            .field("info", &self.info)
            .field("frames_out", &self.frames_out)
            .finish()
    }
}

impl Decoder for Mp3Decoder {
    fn info(&self) -> &StreamInfo {
        &self.info
    }

    fn seek(&mut self, frame: u64) -> Result<(), CadenceError> {
        // Linear decode-and-discard from the first frame (correct but not
        // real-time safe; MAY block on the source).
        self.source.seek_to(self.audio_start)?;
        self.reset_stream_state();

        let channels = self.info.channels as usize;
        let scratch_frames = self.seek_scratch.len() / channels;
        while self.frames_out < frame {
            let want = ((frame - self.frames_out) as usize).min(scratch_frames) * channels;
            let mut scratch = std::mem::take(&mut self.seek_scratch);
            let result = self.decode(&mut scratch[..want]);
            self.seek_scratch = scratch;
            if result? == 0 {
                return Err(CadenceError::CorruptData(
                    "stream ended while seeking".into(),
                ));
            }
        }
        Ok(())
    }

    fn decode(&mut self, buffer: &mut [f32]) -> Result<usize, CadenceError> {
        let channels = self.info.channels as usize;
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
            if self.staged_pos >= self.staged_frames && !self.decode_next_frame()? {
                break;
            }
            let available = self.staged_frames - self.staged_pos;
            let n = (want - written).min(available);
            let src = self.staged_pos * channels;
            buffer[written * channels..(written + n) * channels]
                .copy_from_slice(&self.pcm_staged[src..src + n * channels]);
            self.staged_pos += n;
            self.frames_out += n as u64;
            written += n;
        }
        Ok(written)
    }
}

/// Reader wrapper implementing [`FormatReader`] for MP3 streams.
pub struct Mp3Reader {
    decoder: Mp3Decoder,
}

impl FormatReader for Mp3Reader {
    fn open(source: Box<dyn Read + Send>) -> Result<Self, CadenceError> {
        Ok(Mp3Reader {
            decoder: Mp3Decoder::open(source)?,
        })
    }

    fn decoder(&mut self) -> &mut dyn Decoder {
        &mut self.decoder
    }

    fn info(&self) -> &StreamInfo {
        self.decoder.info()
    }
}
