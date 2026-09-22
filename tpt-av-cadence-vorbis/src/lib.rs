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
mod residue;

use std::io::Read;

use tpt_av_cadence_core::{
    BufferedSource, ByteSource, CadenceError, Decoder, Format, FormatReader, StreamInfo, Unseekable,
};
use tpt_av_cadence_ogg::PageReader;

use crate::bitreader::BitReader;
use crate::header::{IdHeader, Setup};
use crate::mdct::{vector_fmul_window, Mdct};
use crate::residue::ResidueWorkspace;

/// Memory budget for all codebook VQ value tables combined (elements).
const CODEBOOK_VALUE_BUDGET: usize = 32 << 20;

/// Vorbis stream order → WAV order for 3–8 channels (rows index by
/// `channels - 3`). Vorbis orders multichannel streams L C R … with the
/// LFE near the end; WAV (and every downstream consumer) expects L R C
/// … LFE … — the same tables FFmpeg's Vorbis decoder applies. Mono and
/// stereo need no permutation.
const WAV_ORDER_PERMUTATIONS: [[usize; 8]; 6] = [
    [0, 2, 1, 0, 0, 0, 0, 0], // 3: L C R        -> L R C
    [0, 1, 2, 3, 0, 0, 0, 0], // 4: quad (same order)
    [0, 2, 1, 3, 4, 0, 0, 0], // 5: L C R SL SR  -> L R C SL SR
    [0, 2, 1, 5, 3, 4, 0, 0], // 6: 5.1
    [0, 2, 1, 6, 4, 3, 5, 0], // 7: 6.1
    [0, 2, 1, 7, 5, 6, 3, 4], // 8: 7.1
];
/// Packet buffer ceiling. Legal audio packets are far below this; oversized
/// setup headers in the wild top out around 64 KiB.
const MAX_PACKET: usize = 1 << 20;

/// Decoder for Ogg Vorbis I streams.
pub struct VorbisDecoder {
    ogg: PageReader,
    info: StreamInfo,
    id: IdHeader,
    setup: Setup,

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
    /// [next_window_flag] of the most recent long-block packet (window
    /// synthesis input; spec §4.3.1).
    next_flag: bool,
    /// Granule position carried by the EOS page, once its first packet is
    /// pulled (`-1` before that). Output is capped here (end trim).
    eos_granule: i64,
    /// Frames staged after the priming packet.
    decoded_total: u64,
    /// Frames handed to the caller.
    delivered: u64,
    /// Once the EOS page finishes: the exact cap on output frames.
    output_cap: Option<u64>,
    finished: bool,
}

fn corrupt(what: &str) -> CadenceError {
    CadenceError::CorruptData(format!("vorbis: {what}"))
}

/// Tags a parse error with the header stage it occurred in.
fn stage_err(stage: &str, e: CadenceError) -> CadenceError {
    match e {
        CadenceError::CorruptData(msg) => CadenceError::CorruptData(format!(
            "vorbis {stage}: {}",
            msg.strip_prefix("vorbis: ").unwrap_or(&msg)
        )),
        other => other,
    }
}

impl VorbisDecoder {
    /// Parses the three header packets from the current stream position
    /// (identification, comment, setup).
    fn parse_headers(ogg: &mut PageReader) -> std::result::Result<(IdHeader, Setup), CadenceError> {
        let mut raw = vec![0u8; MAX_PACKET].into_boxed_slice();
        let mut id: Option<IdHeader> = None;
        let mut setup: Option<Setup> = None;
        for expected in [1u8, 3, 5] {
            let (len, _) = ogg
                .next_packet(&mut raw)?
                .ok_or_else(|| corrupt("stream ended before the setup headers"))?;
            match expected {
                1 => {
                    id = Some(
                        header::parse_id(&raw[..len])
                            .map_err(|e| stage_err("identification header", e))?,
                    )
                }
                3 => {
                    header::skip_comment(&raw[..len]).map_err(|e| stage_err("comment header", e))?
                }
                _ => {
                    // `expected` walks the fixed literal array `[1, 3, 5]` in
                    // order, so this `5` arm always runs after the `1` arm
                    // has already set `id` (any failure there returns via
                    // `?` before this point is reached).
                    debug_assert!(id.is_some());
                    setup = Some(
                        header::parse_setup(&raw[..len], id.as_ref().unwrap())
                            .map_err(|e| stage_err("setup header", e))?,
                    );
                }
            }
        }
        // Every arm of the fixed `[1, 3, 5]` loop above either returns early
        // via `?` or, for `1`/`5`, sets `id`/`setup`; reaching here means all
        // three iterations completed, so both are populated.
        debug_assert!(id.is_some() && setup.is_some());
        Ok((id.unwrap(), setup.unwrap()))
    }

    /// Opens a Vorbis stream over a byte source. Seekable sources get
    /// working `seek()`.
    pub fn from_source(source: Box<dyn ByteSource>) -> Result<Self, CadenceError> {
        let buffered = BufferedSource::new(source, 32 * 1024);
        let mut ogg = PageReader::new(buffered, MAX_PACKET);
        let (id, setup) = Self::parse_headers(&mut ogg)?;
        Self::assemble(ogg, &id, setup)
    }

    /// Convenience constructor over a plain readable (unseekable) source.
    pub fn open(source: Box<dyn Read + Send>) -> Result<Self, CadenceError> {
        Self::from_source(Box::new(Unseekable(source)))
    }

    fn assemble(ogg: PageReader, id: &IdHeader, mut setup: Setup) -> Result<Self, CadenceError> {
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
            next_flag: false,
            eos_granule: -1,
            decoded_total: 0,
            delivered: 0,
            output_cap: None,
            finished: false,
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
        self.next_flag = false;
        self.eos_granule = -1;
        self.decoded_total = 0;
        self.delivered = 0;
        self.output_cap = None;
        self.finished = false;
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
        // Spec §4.3.1: a long-window packet carries exactly TWO flag bits —
        // [previous_window_flag] then [next_window_flag]. The current window
        // comes from the mode's blockflag; there is no separate "window" bit.
        let mut prev_flag = false;
        if blockflag {
            prev_flag = br.read_bit()?;
            self.next_flag = br.read_bit()?;
        } else {
            self.next_flag = false;
        }
        let n = if blockflag {
            1usize << self.id.blocksize_1_exp
        } else {
            1usize << self.id.blocksize_0_exp
        };
        let vlen = n / 2;

        // Floor decode (channel order).
        let mut no_residue = [false; 256];
        for (ch, is_empty) in no_residue.iter_mut().enumerate().take(channels) {
            let (floor_idx, submap) = {
                let m = &self.setup.mappings[mapping_idx];
                let submap = if m.submaps > 1 { m.mux[ch] as usize } else { 0 };
                (m.submap_floor[submap] as usize, submap)
            };
            let _ = submap;
            let curve = &mut self.floors[ch][..vlen];
            let nonzero = crate::floor::floor_decode(
                &self.setup.floors[floor_idx],
                &self.setup.codebooks,
                &mut br,
                blockflag as usize,
                curve,
                vlen,
            )?;
            *is_empty = !nonzero;
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

        // Residue decode (submap order). The spec allocates and zeroes the
        // return vectors per packet (§8: "allocate and zero all vectors")
        // because residue decoding ADDS into them — without this, stale
        // values from the previous packet accumulate into the output
        // (libvorbis memsets pcm[i] to n/2 in mapping0_inverse).
        for ch in 0..channels {
            self.residues[ch][..vlen].fill(0.0);
        }
        for submap in 0..self.setup.mappings[mapping_idx].submaps {
            let mut do_not_decode = [false; 256];
            let mut members = [usize::MAX; 256];
            let mut ch_count = 0usize;
            let m = &self.setup.mappings[mapping_idx];
            for (j, empty) in no_residue.iter().enumerate().take(channels) {
                if m.submaps == 1 || m.mux[j] as usize == submap {
                    members[ch_count] = j;
                    do_not_decode[ch_count] = *empty;
                    ch_count += 1;
                }
            }
            if ch_count == 0 {
                continue;
            }
            let res_idx = m.submap_residue[submap] as usize;
            let res = &self.setup.residues[res_idx];
            res.decode(
                &self.setup.codebooks,
                &mut br,
                &mut self.residues[..],
                &members[..ch_count],
                &do_not_decode[..ch_count],
                &mut self.workspace,
                vlen,
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
            let (lo, hi) = if m_idx < a_idx {
                (m_idx, a_idx)
            } else {
                (a_idx, m_idx)
            };
            let (first, second) = self.residues.split_at_mut(hi);
            let (mag, ang) = if m_idx < a_idx {
                (&mut first[lo], &mut second[0])
            } else {
                (&mut second[0], &mut first[lo])
            };
            inverse_coupling(mag, ang, vlen);
        }

        // Dot product with the floor curve + inverse MDCT per channel.
        for (ch, empty) in no_residue.iter().enumerate().take(channels) {
            if *empty {
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
        // `dst` indexes both the permutation table and the destination
        // stride; iterator style obscures the dual indexing.
        #[allow(clippy::needless_range_loop)]
        for dst in 0..channels {
            let win: &[f32] = if win_idx == 1 {
                self.mdct[1].window()
            } else {
                self.mdct[0].window()
            };
            // Vorbis orders multichannel output in its own stream order
            // (e.g. 6 ch: L C R SL SR LFE); the decoded PCM handed to the
            // engine follows WAV order (L R C LFE SL SR), matching
            // FFmpeg's ff_vorbis_channel_layout_offsets: WAV channel
            // `dst` carries the content of source channel perm[dst].
            // Channel state (residues/saved) stays indexed by source
            // channel; only the staging destination is permuted.
            let src = if channels <= 2 {
                dst
            } else {
                WAV_ORDER_PERMUTATIONS[(channels - 3).min(5)][dst]
            };
            overlap_add_channel(
                &mut self.staging[..],
                dst,
                channels,
                &self.residues[src][..],
                &mut self.saved[src][..],
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
            &saved[lead..],
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
        // Decode-and-discard seek: scan the page headers for the page
        // boundary at-or-before `frame`, then decode forward from there
        // discarding PCM until the delivered position reaches `frame`.
        //
        // A resume boundary is the granule of some audio page (pages whose
        // first packet follows the three header packets): decoding from the
        // NEXT page's start replays every packet, so the post-seek stream
        // is bit-identical to an uninterrupted decode. The page boundary's
        // stream position is that page's own granule (samples are numbered
        // from the end of the priming discard).
        #[derive(Clone)]
        struct Candidate {
            page_pos: u64,
            /// Stream position at the page's first packet.
            start: u64,
            is_first_audio_page: bool,
        }
        let mut candidate: Option<Candidate> = None;
        {
            use crate::bitreader::BitReader;
            let source = self.ogg.source_mut();
            source.seek_to(0)?;
            let mut pos = 0u64;
            let mut packets_seen = 0u64;
            // Stream position of the next audio packet's first sample.
            let mut stream_pos = 0u64;
            let mut prev_n: Option<u64> = None;
            let mut first_audio_page_pos: Option<u64> = None;
            let mut last: Option<Candidate> = None;
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
                let page_pos = pos;
                let page_start_stream_pos = stream_pos;
                let is_first_audio_page = first_audio_page_pos.is_none() && packets_seen >= 3;

                // Walk the page's packets: count them and sum their PCM
                // durations (from each packet's mode bits) so the page's
                // start position is known for the NEXT page's accounting.
                let seg_pos = 27usize + nsegs;
                let mut packet: Vec<u8> = Vec::new();
                for &seg in &seg_table[..nsegs] {
                    let mut buf = vec![0u8; seg as usize];
                    source.take_exact(&mut buf)?;
                    packet.extend_from_slice(&buf);
                    if seg == 255 {
                        continue;
                    }
                    // Packet completed.
                    let headered =
                        packets_seen < 3 || is_bos && packet.first().is_some_and(|b| b & 1 == 1);
                    if !headered && granule >= 0 {
                        let mut br = BitReader::new(&packet);
                        let type_bit = br.read_bit().unwrap_or(true);
                        let mut n = 0u64;
                        if !type_bit {
                            let mode_bits = BitReader::ilog(self.setup.modes.len() as i64 - 1);
                            let mode = if mode_bits > 0 {
                                br.read_bits(mode_bits).unwrap_or(0) as usize
                            } else {
                                0
                            };
                            let blockflag = self.setup.modes.get(mode).is_some_and(|m| m.blockflag);
                            n = 1u64
                                << if blockflag {
                                    self.id.blocksize_1_exp
                                } else {
                                    self.id.blocksize_0_exp
                                };
                            let _ = br.read_bits(if blockflag { 2 } else { 0 });
                        }
                        let first = packets_seen == 3;
                        let duration = if first {
                            n / 4
                        } else {
                            (n + prev_n.unwrap_or(n)) / 4
                        };
                        stream_pos += duration;
                        prev_n = Some(n);
                    }
                    packets_seen += 1;
                    packet.clear();
                }
                let _ = seg_pos;

                if is_first_audio_page {
                    first_audio_page_pos = Some(page_pos);
                }
                if is_first_audio_page || packets_seen > 3 {
                    // Track the latest audio page as a decode restart point.
                    let start = if is_first_audio_page {
                        0
                    } else {
                        page_start_stream_pos
                    };
                    last = Some(Candidate {
                        page_pos,
                        start,
                        is_first_audio_page,
                    });
                    // The candidate for `frame` is the latest page whose
                    // START position is <= frame (start <= granule).
                    if start <= frame {
                        candidate = last.clone();
                    }
                }

                pos += 27 + nsegs as u64 + body;
                if is_eos {
                    break;
                }
                source.seek_to(pos)?;
            }
            // If no page qualified (frame beyond the stream), decode to the end.
            if candidate.is_none() {
                candidate = last;
            }
        }
        let Some(c) = candidate else {
            // No audio at all: nothing to seek within.
            return Ok(());
        };

        // Re-parse the headers (the decoder state is rebuilt from them).
        self.ogg.restart()?;
        let (id, mut setup) = Self::parse_headers(&mut self.ogg)?;
        // Floor 0 bark maps need the block sizes (same stream, so the
        // block sizes are unchanged, but the setup was re-parsed).
        for f in setup.floors.iter_mut() {
            if let crate::floor::Floor::Zero(f0) = f {
                f0.build_maps(1 << id.blocksize_0_exp, 1 << id.blocksize_1_exp);
            }
        }
        self.id = id;
        self.setup = setup;
        self.reset_decode_state();
        if c.page_pos > 0 {
            self.ogg.reset(c.page_pos)?;
        }
        if !c.is_first_audio_page {
            // Mid-stream resume: there is no priming packet to discard.
            self.first_audio = false;
        }

        // Decode-and-discard exactly to `frame` (one frame per call so the
        // landing is sample-exact).
        let channels = self.id.channels as usize;
        let mut discard = vec![0.0f32; channels];
        let mut to_skip = frame.saturating_sub(c.start);
        while to_skip > 0 {
            match self.decode(&mut discard) {
                Ok(0) | Err(_) => break,
                Ok(_) => to_skip -= 1,
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
            // Deliver from staging, honoring the end-trim output cap.
            if self.staging_pos < self.staging_frames * channels {
                let avail = self.staging_frames * channels - self.staging_pos;
                let want = ((cap - written).min(avail) / channels) * channels;
                let mut frames_want = want / channels;
                if let Some(cap_frames) = self.output_cap {
                    let room = cap_frames.saturating_sub(self.delivered) as usize;
                    frames_want = frames_want.min(room);
                }
                if frames_want == 0 {
                    // The end-trim cap is exhausted: the stream is over.
                    self.finished = true;
                    return Ok(written / channels);
                }
                let src = self.staging_pos;
                let copy_samples = frames_want * channels;
                buffer[written..written + copy_samples]
                    .copy_from_slice(&self.staging[src..src + copy_samples]);
                written += copy_samples;
                self.staging_pos = src + copy_samples;
                self.delivered += frames_want as u64;
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
                    self.finished = true;
                    return Ok(written / channels);
                }
            };

            // The EOS page's granule is the exact end of the program: cap
            // delivery there (end trim) from the moment it is known.
            if meta.eos_page && meta.granule >= 0 && self.eos_granule < 0 {
                self.eos_granule = meta.granule;
                self.output_cap = Some(meta.granule as u64);
                self.info.total_frames = Some(meta.granule as u64);
            }

            // Decode it (take the packet buffer to appease the borrow
            // checker; it is put back below).
            let pkt = std::mem::take(&mut self.packet);
            let decoded = self.decode_packet(&pkt[..len]);
            self.packet = pkt;
            let (retlen, _blockflag, was_first) = decoded?;

            if was_first {
                // The priming packet is parsed (its `saved` cache is used)
                // but its PCM is not delivered (spec 4.3.8). A stream opened
                // from byte 0 has no leading trim: the priming discard is
                // the entire pre-roll (a page's granule counts samples at
                // its END and is nonzero even for the first page, so it
                // must not be used as a start offset).
                self.first_audio = false;
            } else {
                self.decoded_total += retlen as u64;
                self.staging_frames = retlen;
                self.staging_pos = 0;
            }
        }
    }
}

fn read_exact_or_eof(source: &mut BufferedSource, buf: &mut [u8]) -> Result<bool, CadenceError> {
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
