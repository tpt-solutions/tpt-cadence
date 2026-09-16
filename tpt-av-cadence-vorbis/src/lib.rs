//! # tpt-av-cadence-vorbis
//!
//! Ogg Vorbis I decoder for the `tpt-cadence` suite, implemented from the
//! Vorbis I specification: Ogg page/packet layer, codebooks, floor 0/1
//! curves, residue 0/1/2, square-polar channel coupling, and an FFT-based
//! synthesis MDCT with FFmpeg-compatible overlap-add, block-size switching,
//! and granule-based start/end trimming (so PCM output matches FFmpeg's
//! sample-accurately).
//!
//! All allocation happens in [`VorbisDecoder::from_source`] / [`open`];
//! [`Decoder::decode`] is allocation-free, lock-free, and panic-free.
//!
//! Output samples are interleaved `f32` in `[-1, 1)`. Channel order follows
//! `StreamInfo::channel_layout`: Vorbis stream order permuted to WAV order
//! for 3-8 channels (matching FFmpeg). Chained Ogg streams decode only the
//! first logical link.

mod bitreader;
mod codebook;
mod fft;
mod floor;
mod header;
mod mdct;
mod ogg;
mod residue;

use std::io::Read;

use tpt_av_cadence_core::{
    BufferedSource, ByteSource, CadenceError, Decoder, Format, FormatReader, StreamInfo,
    Unseekable,
};

use crate::bitreader::BitReader;
use crate::header::{IdHeader, Setup};
use crate::mdct::{vector_fmul_window, Mdct};
use crate::ogg::PageReader;
use crate::residue::ResidueWorkspace;

/// Memory budget for all codebook VQ value tables combined (elements).
const CODEBOOK_VALUE_BUDGET: usize = 32 << 20;
/// Packet buffer ceiling. Legal audio packets are far below this; oversized
/// setup headers in the wild top out around 64 KiB.
const MAX_PACKET: usize = 1 << 20;

/// Decoder for Ogg Vorbis I streams.
pub struct VorbisDecoder {
    ogg: PageReader,
    info: StreamInfo,
    id: IdHeader,
    setup: Setup,
    /// Long-block sample count.
    blocksize_1: usize,

    /// Synthesis transforms per block size (index 0 = short).
    mdct: [Mdct; 2],

    /// Per-channel spectral residue vectors (`bs1/2`).
    residues: Vec<Box<[f32]>>,
    /// Per-channel floor curves (`bs1/2`).
    floors: Vec<Box<[f32]>>,
    /// Per-channel cached unwindowed right-hand MDCT output (`bs1/4`).
    saved: Vec<Box<[f32]>>,
    /// Overlap-add scratch (`bs1/2`).
    ola_scratch: Box<[f32]>,
    /// VQ/classification scratch.
    workspace: ResidueWorkspace,

    /// Interleaved PCM awaiting delivery (one packet's worth).
    staging: Box<[f32]>,
    staging_frames: usize,
    staging_pos: usize,

    /// Reusable packet buffer.
    packet: Box<[u8]>,

    /// Blockflag of the previous packet (`None` before the first).
    prev_blockflag: Option<bool>,
    first_audio: bool,
    /// Sum of parser durations on the current EOS page (end trim).
    eos_page_duration: i64,
    eos_granule: i64,
    eos_page_index: u64,
    /// Granule of the last non-EOS page.
    last_page_granule: i64,
    /// Leading frames still to drop (positive first-audio-page granule).
    start_trim: u64,
    /// Frames staged after the priming packet.
    decoded_total: u64,
    /// Frames handed to the caller.
    delivered: u64,
    /// Once the EOS page finishes: the exact cap on output frames.
    output_cap: Option<u64>,
    finished: bool,
    /// Page index of the last packet (page-boundary detection).
    last_page_index: u64,
    /// Set when the packet that just completed the EOS page was decoded.
    saw_eos_page: bool,
}

fn corrupt(what: &str) -> CadenceError {
    CadenceError::CorruptData(format!("vorbis: {what}"))
}

impl VorbisDecoder {
    /// Opens a Vorbis stream over a byte source. Seekable sources get
    /// working `seek()`.
    pub fn from_source(source: Box<dyn ByteSource>) -> Result<Self, CadenceError> {
        let buffered = BufferedSource::new(source, 32 * 1024);
        let mut ogg = PageReader::new(buffered, MAX_PACKET);
        let mut raw = vec![0u8; MAX_PACKET].into_boxed_slice();

        let mut id: Option<IdHeader> = None;
        let mut setup: Option<Setup> = None;
        for expected in [1u8, 3, 5] {
            let (len, _) = ogg
                .next_packet(&mut raw)?
                .ok_or_else(|| corrupt("stream ended before the setup headers"))?;
            match expected {
                1 => id = Some(header::parse_id(&raw[..len])?),
                3 => header::skip_comment(&raw[..len])?,
                _ => {
                    setup = Some(header::parse_setup(&raw[..len], id.as_ref().unwrap())?);
                }
            }
        }
        Self::assemble(ogg, &id.unwrap(), setup.unwrap())
    }

    /// Convenience constructor over a plain readable (unseekable) source.
    pub fn open(source: Box<dyn Read + Send>) -> Result<Self, CadenceError> {
        Self::from_source(Box::new(Unseekable(source)))
    }

    fn assemble(mut ogg: PageReader, id: &IdHeader, mut setup: Setup) -> Result<Self, CadenceError> {
        let channels = id.channels as usize;
        let blocksize_0 = 1usize << id.blocksize_0_exp;
        let blocksize_1 = 1usize << id.blocksize_1_exp;

        // Floor 0 bark maps need the block sizes.
        for f in setup.floors.iter_mut() {
            if let crate::floor::Floor::Zero(f0) = f {
                f0.build_maps(blocksize_0, blocksize_1);
            }
        }

        // Codebook memory budget (hostile setups could otherwise request
        // gigabytes of VQ tables).
        let mut values_budget = 0usize;
        for cb in &setup.codebooks {
            values_budget = values_budget.saturating_add(cb.entries * cb.dimensions);
        }
        if values_budget > CODEBOOK_VALUE_BUDGET {
            return Err(CadenceError::InvalidFormat(
                "vorbis: codebook value tables exceed the memory budget".to_string(),
            ));
        }

        let info = StreamInfo::new(Format::Vorbis, id.sample_rate, id.channels, 32);
        info.validate()?;

        let max_classwords = header::max_classbook_dimensions(&setup);
        let workspace = ResidueWorkspace::new(channels, blocksize_1, max_classwords.max(1));

        Ok(VorbisDecoder {
            ogg,
            info,
            id: id.clone(),
            setup,
            blocksize_1,
            mdct: [Mdct::new(blocksize_0), Mdct::new(blocksize_1)],
            residues: (0..channels)
                .map(|_| vec![0.0f32; blocksize_1 / 2].into_boxed_slice())
                .collect(),
            floors: (0..channels)
                .map(|_| vec![0.0f32; blocksize_1 / 2].into_boxed_slice())
                .collect(),
            saved: (0..channels)
                .map(|_| vec![0.0f32; blocksize_1 / 4].into_boxed_slice())
                .collect(),
            ola_scratch: vec![0.0f32; blocksize_1 / 2].into_boxed_slice(),
            workspace,
            staging: vec![0.0f32; channels * blocksize_1].into_boxed_slice(),
            staging_frames: 0,
            staging_pos: 0,
            packet: vec![0u8; MAX_PACKET].into_boxed_slice(),
            prev_blockflag: None,
            first_audio: true,
            eos_page_duration: 0,
            eos_granule: -1,
            eos_page_index: 0,
            last_page_granule: 0,
            start_trim: 0,
            decoded_total: 0,
            delivered: 0,
            output_cap: None,
            finished: false,
            last_page_index: 0,
            saw_eos_page: false,
        })
    }

    /// Resets all packet-decode state (used by `seek`).
    fn reset_decode_state(&mut self) {
        for r in self.residues.iter_mut() {
            r.fill(0.0);
        }
        for f in self.floors.iter_mut() {
            f.fill(0.0);
        }
        for s in self.saved.iter_mut() {
            s.fill(0.0);
        }
        self.staging_frames = 0;
        self.staging_pos = 0;
        self.prev_blockflag = None;
        self.first_audio = true;
        self.eos_page_duration = 0;
        self.eos_granule = -1;
        self.eos_page_index = 0;
        self.last_page_granule = 0;
        self.start_trim = 0;
        self.decoded_total = 0;
        self.delivered = 0;
        self.output_cap = None;
        self.finished = false;
        self.last_page_index = 0;
        self.saw_eos_page = false;
    }

    /// Decodes one audio packet, writing its `retlen` frames interleaved
    /// into `self.staging`. Returns `(frames, blockflag, was_first)`.
    fn decode_packet(&mut self, packet: &[u8]) -> Result<(usize, bool, bool), CadenceError> {
        let channels = self.id.channels as usize;
        let was_first = self.first_audio;
        let mut br = BitReader::new(packet);
        if br.read_bit()? {
            // A header packet mid-stream: chained-link boundary. The first
            // logical stream is the whole decode.
            self.finished = true;
            return Ok((0, false, was_first));
        }
        let mode_count = self.setup.modes.len();
        let mode_number = if mode_count == 1 {
            0
        } else {
            br.read_bits(BitReader::ilog(mode_count as i64 - 1))? as usize
        };
        if mode_number >= mode_count {
            return Err(corrupt("mode number out of range"));
        }
        let blockflag = self.setup.modes[mode_number].blockflag;
        let mapping_idx = self.setup.modes[mode_number].mapping;
        let prev_flag = if blockflag {
            let _window = br.read_bit()?;
            let prev = br.read_bit()?;
            let _next = br.read_bit()?;
            prev
        } else {
            false
        };
        let n = if blockflag {
            1usize << self.id.blocksize_1_exp
        } else {
            1usize << self.id.blocksize_0_exp
        };
        let vlen = n / 2;

        // Floor decode (channel order).
        let mut no_residue = [false; 256];
        for ch in 0..channels {
            let (floor_idx, submap) = {
                let m = &self.setup.mappings[mapping_idx];
                let submap = if m.submaps > 1 { m.mux[ch] as usize } else { 0 };
                (m.submap_floor[submap] as usize, submap)
            };
            let _ = submap;
            let mut curve = &mut self.floors[ch][..vlen];
            let nonzero = crate::floor::floor_decode(
                &self.setup.floors[floor_idx],
                &self.setup.codebooks,
                &mut br,
                blockflag as usize,
                &mut curve,
                vlen,
            )?;
            no_residue[ch] = !nonzero;
            if !nonzero {
                self.floors[ch][..vlen].fill(0.0);
            }
        }

        // Nonzero propagate through coupling pairs.
        {
            let m = &self.setup.mappings[mapping_idx];
            for i in (0..m.coupling_steps).rev() {
                let (a, b) = (m.magnitude[i], m.angle[i]);
                if !(no_residue[a] && no_residue[b]) {
                    no_residue[a] = false;
                    no_residue[b] = false;
                }
            }
        }

        // Residue decode (submap order).
        for submap in 0..self.setup.mappings[mapping_idx].submaps {
            let mut do_not_decode = [false; 256];
            let mut members = [usize::MAX; 256];
            let mut ch_count = 0usize;
            let m = &self.setup.mappings[mapping_idx];
            for j in 0..channels {
                if m.submaps == 1 || m.mux[j] as usize == submap {
                    members[ch_count] = j;
                    do_not_decode[ch_count] = no_residue[j];
                    ch_count += 1;
                }
            }
            if ch_count == 0 {
                continue;
            }
            let res_idx = m.submap_residue[submap] as usize;
            let res = &self.setup.residues[res_idx];
            let mut vecs: Vec<&mut [f32]> = Vec::with_capacity(ch_count);
            for (idx, r) in self.residues.iter_mut().enumerate() {
                if members[..ch_count].contains(&idx) {
                    vecs.push(&mut r[..vlen]);
                }
            }
            res.decode(
                &self.setup.codebooks,
                &mut br,
                &mut vecs,
                &do_not_decode[..ch_count],
                &mut self.workspace,
            )?;
        }
        for ch in 0..channels {
            self.residues[ch][vlen..].fill(0.0);
        }

        // Inverse coupling (reverse order; square polar, spec 4.3.5).
        for i in (0..self.setup.mappings[mapping_idx].coupling_steps).rev() {
            let (m_idx, a_idx) = {
                let m = &self.setup.mappings[mapping_idx];
                (m.magnitude[i], m.angle[i])
            };
            // Indices are distinct (validated at setup).
            let (lo, hi) = if m_idx < a_idx { (m_idx, a_idx) } else { (a_idx, m_idx) };
            let (first, second) = self.residues.split_at_mut(hi);
            let (mag, ang) = if m_idx < a_idx {
                (&mut first[lo], &mut second[0])
            } else {
                (&mut second[0], &mut first[lo])
            };
            inverse_coupling(mag, ang, vlen);
        }

        // Dot product with the floor curve + inverse MDCT per channel.
        for ch in 0..channels {
            if no_residue[ch] {
                self.residues[ch][..vlen].fill(0.0);
                continue;
            }
            for k in 0..vlen {
                self.residues[ch][k] *= self.floors[ch][k];
            }
            let spectra = &mut self.residues[ch][..vlen];
            if blockflag {
                self.mdct[1].imdct_half(spectra);
            } else {
                self.mdct[0].imdct_half(spectra);
            }
        }

        // Overlap-add (the FFmpeg lapping scheme; the short window applies
        // whenever either neighbor block is short).
        let prev = if was_first {
            if blockflag {
                prev_flag
            } else {
                false
            }
        } else {
            self.prev_blockflag.unwrap_or(blockflag)
        };
        let bs0 = 1usize << self.id.blocksize_0_exp;
        let bs1 = 1usize << self.id.blocksize_1_exp;
        let retlen = (n + if prev { bs1 } else { bs0 }) / 4;
        let win_idx = (blockflag as usize) & (prev as usize);
        for ch in 0..channels {
            let win: &[f32] = if win_idx == 1 {
                self.mdct[1].window()
            } else {
                self.mdct[0].window()
            };
            overlap_add_channel(
                &mut self.staging[..],
                ch,
                channels,
                &self.residues[ch][..],
                &mut self.saved[ch][..],
                win,
                blockflag,
                prev,
                bs0,
                bs1,
                retlen,
                &mut self.ola_scratch[..],
            );
        }
        self.prev_blockflag = Some(blockflag);
        Ok((retlen, blockflag, was_first))
    }

    /// Finalizes the end trim when the EOS page has completed.
    fn finish_eos_page(&mut self) {
        if self.saw_eos_page && self.output_cap.is_none() {
            if self.eos_granule >= 0 {
                // FFmpeg Ogg demuxer semantics: trailing samples beyond the
                // final granule are dropped.
                let delta = self.eos_granule - self.last_page_granule;
                let skip = self.eos_page_duration - delta;
                if skip > 0 {
                    self.output_cap = Some(self.decoded_total.saturating_sub(skip as u64));
                } else {
                    self.output_cap = Some(self.decoded_total);
                }
                self.info.total_frames = Some(self.eos_granule as u64);
            } else {
                self.output_cap = Some(self.decoded_total);
            }
        }
        self.saw_eos_page = false;
        self.eos_page_duration = 0;
    }
}

/// Square-polar inverse coupling over `len` samples (spec 4.3.5).
fn inverse_coupling(mag: &mut [f32], ang: &mut [f32], len: usize) {
    for k in 0..len {
        let m = mag[k];
        let a = ang[k];
        match (m > 0.0, a > 0.0) {
            (true, true) => {
                ang[k] = m - a;
            }
            (true, false) => {
                ang[k] = m;
                mag[k] = m + a;
            }
            (false, true) => {
                ang[k] = m + a;
            }
            (false, false) => {
                ang[k] = m;
                mag[k] = m - a;
            }
        }
    }
}

/// Applies the FFmpeg overlap-add for one channel, writing `retlen`
/// interleaved frames into `staging` at channel offset `ch`.
#[allow(clippy::too_many_arguments)]
fn overlap_add_channel(
    staging: &mut [f32],
    ch: usize,
    channels: usize,
    buf: &[f32],
    saved: &mut [f32],
    win: &[f32],
    blockflag: bool,
    prev: bool,
    bs0: usize,
    bs1: usize,
    retlen: usize,
    scratch: &mut [f32],
) {
    let n = if blockflag { bs1 } else { bs0 };
    if blockflag == prev {
        vector_fmul_window(&mut scratch[..retlen], saved, buf, win, n / 4);
    } else if blockflag && !prev {
        // Short prev -> long cur.
        let head_len = bs0 / 2;
        vector_fmul_window(&mut scratch[..head_len], saved, buf, win, bs0 / 4);
        let copy = (bs1 - bs0) / 4;
        scratch[head_len..head_len + copy].copy_from_slice(&buf[bs0 / 4..bs0 / 4 + copy]);
    } else {
        // Long prev -> short cur.
        let lead = (bs1 - bs0) / 4;
        scratch[..lead].copy_from_slice(&saved[..lead]);
        vector_fmul_window(
            &mut scratch[lead..lead + bs0 / 2],
            &mut saved[lead..],
            buf,
            win,
            bs0 / 4,
        );
    }
    saved[..n / 4].copy_from_slice(&buf[n / 4..n / 2]);
    for (i, &s) in scratch[..retlen].iter().enumerate() {
        staging[ch + i * channels] = s;
    }
}

/// Reads format-level information.
pub struct VorbisFormatReader {
    decoder: VorbisDecoder,
}

impl FormatReader for VorbisFormatReader {
    fn open(source: Box<dyn Read + Send>) -> Result<Self, CadenceError> {
        Ok(VorbisFormatReader {
            decoder: VorbisDecoder::open(source)?,
        })
    }

    fn decoder(&mut self) -> &mut dyn Decoder {
        &mut self.decoder
    }

    fn info(&self) -> &StreamInfo {
        &self.decoder.info
    }
}

impl Decoder for VorbisDecoder {
    fn info(&self) -> &StreamInfo {
        &self.info
    }

    fn seek(&mut self, frame: u64) -> Result<(), CadenceError> {
        // Decode-and-discard seek: scan page headers to the last page whose
        // granule is <= `frame`, then decode forward discarding PCM until
        // the internal decoded position reaches `frame` exactly.
        let resume_pos = {
            let source = self.ogg.source_mut();
            source.seek_to(0)?;
            let mut resume = 0u64;
            let mut pos = 0u64;
            loop {
                let mut header = [0u8; 27];
                if !read_exact_or_eof(source, &mut header)? {
                    break;
                }
                if &header[0..4] != b"OggS" {
                    return Err(corrupt("seek scan: bad capture pattern"));
                }
                let nsegs = header[26] as usize;
                let mut seg_table = [0u8; 255];
                source.take_exact(&mut seg_table[..nsegs])?;
                let body: u64 = seg_table[..nsegs].iter().map(|&s| s as u64).sum();
                let granule = i64::from_le_bytes(header[6..14].try_into().unwrap());
                let is_bos = header[5] & 0x02 != 0;
                let is_eos = header[5] & 0x04 != 0;
                if !is_bos && granule >= 0 && (granule as u64) <= frame {
                    resume = pos;
                }
                pos += 27 + nsegs as u64 + body;
                if is_eos {
                    break;
                }
                source.seek_to(pos)?;
            }
            resume
        };

        self.ogg.reset(resume_pos)?;
        self.reset_decode_state();

        // Decode-and-discard up to `frame`.
        let channels = self.id.channels as usize;
        let mut discard = vec![0.0f32; channels * self.blocksize_1];
        while self.decoded_total < frame {
            let frames = self.decode(&mut discard)?;
            if frames == 0 {
                break;
            }
        }
        Ok(())
    }

    fn decode(&mut self, buffer: &mut [f32]) -> Result<usize, CadenceError> {
        let channels = self.id.channels as usize;
        if buffer.len() < channels {
            return Err(CadenceError::BufferTooSmall {
                needed: channels,
                provided: buffer.len(),
            });
        }
        let mut written = 0usize;
        let cap = buffer.len() - buffer.len() % channels;

        loop {
            // Deliver from staging, honoring the output cap and start trim.
            if self.staging_pos < self.staging_frames * channels {
                let avail = self.staging_frames * channels - self.staging_pos;
                let want = ((cap - written).min(avail) / channels) * channels;
                let mut frames_want = want / channels;
                let mut skip_now = 0usize;
                if self.start_trim > 0 {
                    skip_now = (self.start_trim as usize).min(frames_want);
                    frames_want -= skip_now;
                }
                if let Some(cap_frames) = self.output_cap {
                    let room = cap_frames.saturating_sub(self.delivered) as usize;
                    frames_want = frames_want.min(room);
                }
                let src = self.staging_pos + skip_now * channels;
                let copy_samples = frames_want * channels;
                buffer[written..written + copy_samples]
                    .copy_from_slice(&self.staging[src..src + copy_samples]);
                written += copy_samples;
                self.staging_pos = src + copy_samples;
                self.delivered += frames_want as u64;
                self.start_trim -= skip_now as u64;
                if written == cap {
                    return Ok(written / channels);
                }
                continue;
            }

            if self.finished {
                return Ok(written / channels);
            }

            // Pull the next packet.
            let (len, meta) = match self.ogg.next_packet(&mut self.packet)? {
                Some(x) => x,
                None => {
                    self.finish_eos_page();
                    self.finished = true;
                    return Ok(written / channels);
                }
            };

            // Page boundary: the previous page is now complete.
            if meta.page_index != self.last_page_index {
                let eos_done = self.saw_eos_page;
                self.finish_eos_page();
                self.last_page_index = meta.page_index;
                if eos_done {
                    self.finished = true;
                    continue;
                }
            }

            // Decode it (take the packet buffer to appease the borrow
            // checker; it is put back below).
            let mut pkt = std::mem::take(&mut self.packet);
            let decoded = self.decode_packet(&pkt[..len]);
            self.packet = pkt;
            let (retlen, blockflag, was_first) = decoded?;

            if was_first {
                // The priming packet is parsed (its `saved` cache is used)
                // but its PCM is not delivered (spec 4.3.8).
                self.first_audio = false;
                // A positive first-audio-page granule marks a mid-program
                // start; drop that many leading samples.
                if meta.granule > 0 {
                    self.start_trim = meta.granule as u64;
                }
            } else {
                self.decoded_total += retlen as u64;
                self.staging_frames = retlen;
                self.staging_pos = 0;
            }

            // Per-page duration sums use the demuxer's parser durations
            // (first audio packet: current blocksize/4, no previous block).
            let parser_duration = if was_first {
                (if blockflag {
                    1i64 << self.id.blocksize_1_exp
                } else {
                    1i64 << self.id.blocksize_0_exp
                }) / 4
            } else {
                retlen as i64
            };
            if meta.eos_page {
                if self.eos_page_index != meta.page_index {
                    self.eos_page_index = meta.page_index;
                    self.eos_page_duration = 0;
                }
                self.eos_page_duration += parser_duration;
                if meta.granule >= 0 {
                    self.eos_granule = meta.granule;
                    self.saw_eos_page = true;
                }
            } else {
                self.last_page_granule = meta.granule;
            }
        }
    }
}

fn read_exact_or_eof(
    source: &mut BufferedSource,
    buf: &mut [u8],
) -> Result<bool, CadenceError> {
    let mut got = 0usize;
    while got < buf.len() {
        let n = source.read(&mut buf[got..]).map_err(CadenceError::from)?;
        if n == 0 {
            return Ok(got == 0);
        }
        got += n;
    }
    Ok(true)
}
