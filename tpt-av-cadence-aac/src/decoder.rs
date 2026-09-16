//! AAC-LC raw_data_block parsing and the top-level `Decoder` implementation.

use std::io::Read;

use tpt_av_cadence_core::{
    BufferedSource, ByteSource, CadenceError, Decoder, Format, FormatReader, StreamInfo, Unseekable,
};

use crate::adts::{self, AdtsHeader};
use crate::audio_specific::AudioSpecificConfig;
use crate::bitreader::BitReader;
use crate::huffman::HuffmanTable;
use crate::imdct::{kbd_window, sine_window, vector_fmul_window, Mdct};
use crate::pns::NoiseGenerator;
use crate::stereo;
use crate::tables;
use crate::tns::{self, Tns};

const MAX_CHANNELS: usize = 8;
/// ADTS frames cannot exceed 6144/8 · channels + header bytes; 16 KiB is a
/// generous cap allocated once at open time.
const FRAME_BUF_LEN: usize = 16 * 1024;
/// Coefficients per short window.
const BLOCK_LEN: usize = 128;

// Element ids (id_syn_ele).
// (LONG_START/LONG_STOP referenced by the windowing TODO; CCE/PCE rejected
// at parse time.)

const SCE: u32 = 0;
const CPE: u32 = 1;
const LFE: u32 = 3;
const DSE: u32 = 4;
const FIL: u32 = 6;
const END: u32 = 7;

// Band types (section_sfb_cb).
const ZERO_BT: u8 = 0;
const NOISE_BT: u8 = 13;
const INTENSITY_BT2: u8 = 14;
const INTENSITY_BT: u8 = 15;

const NOISE_PRE: u32 = 256;
const NOISE_PRE_BITS: u32 = 9;
const NOISE_OFFSET: i32 = 90;

// Window sequences.
const ONLY_LONG: u8 = 0;
#[allow(dead_code)]
const LONG_START: u8 = 1;
const EIGHT_SHORT: u8 = 2;
#[allow(dead_code)]
const LONG_STOP: u8 = 3;

/// Per-channel decoding state (allocation confined to open()).
struct ChannelState {
    /// Spectral coefficients of the current transform (1024).
    coeffs: Box<[f32]>,
    /// Dequantized time-domain output of the current transform (1024).
    out: Box<[f32]>,
    /// Overlap history: the reference decoder's `saved` buffer (512).
    saved: Box<[f32]>,
    /// Per-band codebook assignment (groups × max_sfb ≤ 512 entries).
    band_type: Box<[u8]>,
    /// Per-band scalefactor exponents.
    sfo: Box<[i32]>,
    /// Per-band dequantized gains.
    sf: Box<[f32]>,
    tns: Tns,
    /// This frame's window-shape flag (use_kb_window[0]).
    kb_window_cur: bool,
    /// Previous frame's window-shape flag (use_kb_window[1]).
    kb_window_prev: bool,
    /// Previous frame's window sequence.
    window_seq_prev: u8,
}

impl ChannelState {
    fn new() -> Self {
        ChannelState {
            coeffs: vec![0.0; 1024].into_boxed_slice(),
            out: vec![0.0; 1024].into_boxed_slice(),
            saved: vec![0.0; 512].into_boxed_slice(),
            band_type: vec![0u8; 512].into_boxed_slice(),
            sfo: vec![0i32; 512].into_boxed_slice(),
            sf: vec![0.0f32; 512].into_boxed_slice(),
            tns: Tns::default(),
            kb_window_cur: false,
            kb_window_prev: false,
            window_seq_prev: ONLY_LONG,
        }
    }
}

/// Window parameters resolved from ics_info (shared by a common-window CPE).
#[derive(Clone, Copy)]
struct WindowInfo {
    sequence: u8,
    num_windows: usize,
    num_window_groups: usize,
    group_len: [usize; 8],
    max_sfb: usize,
    num_swb: usize,
    sf_index: usize,
}

impl WindowInfo {
    fn swb_offsets(&self) -> &'static [u16] {
        let table = if self.sequence == EIGHT_SHORT {
            tables::SWB_OFFSET_128[self.sf_index]
        } else {
            tables::SWB_OFFSET_1024[self.sf_index]
        };
        table.unwrap_or(&[])
    }

    fn tns_max_bands(&self) -> usize {
        if self.sequence == EIGHT_SHORT {
            tables::TNS_MAX_BANDS_128[self.sf_index] as usize
        } else {
            tables::TNS_MAX_BANDS_1024[self.sf_index] as usize
        }
    }
}

/// Parsed pulse element.
struct Pulse {
    num_pulse: usize,
    pos: [usize; 4],
    amp: [i32; 4],
}

/// AAC-LC decoder.
///
/// Accepts an ADTS stream (auto-detected) or, via
/// [`AacDecoder::from_config`], a raw stream of byte-aligned raw data
/// blocks. All allocation happens at open; `decode()` is allocation-free,
/// lock-free, and panic-free.
#[allow(dead_code)]
pub struct AacDecoder {
    source: BufferedSource,
    info: StreamInfo,
    channels: usize,
    sf_index: usize,
    /// Header of the first ADTS frame, consumed during open.
    pending_header: Option<[u8; 7]>,
    frame_count: u64,
    /// Raw-block framing (no ADTS headers) when opened from a config.
    raw_blocks: bool,

    channels_state: Vec<ChannelState>,
    spectral_books: Vec<HuffmanTable>,
    sf_book: HuffmanTable,
    mdct_long: Mdct,
    mdct_short: Mdct,
    win_long_kb: Box<[f32]>,
    win_long_sine: Box<[f32]>,
    win_short_kb: Box<[f32]>,
    win_short_sine: Box<[f32]>,
    /// Full long-window synthesis (2048), pre-scramble.
    synth: Box<[f32]>,
    /// Half-length MDCT output in the reference decoder's layout (1024).
    buf: Box<[f32]>,
    /// Short-window lap scratch (128).
    temp: Box<[f32]>,
    noise: NoiseGenerator,

    /// M/S decision bits for the current common-window CPE (512 entries).
    ms_mask: Box<[bool]>,
    frame_buf: Box<[u8]>,
    /// Copy of the current frame body (avoids holding borrows across the
    /// block decoder).
    work_buf: Box<[u8]>,
    /// Interleaved PCM awaiting the caller.
    staged: Vec<f32>,
    staged_pos: usize,
    eof: bool,
}

impl AacDecoder {
    /// Opens an AAC-LC stream over a byte source. ADTS framing is
    /// auto-detected from the first bytes.
    pub fn from_source(source: Box<dyn ByteSource>) -> Result<Self, CadenceError> {
        let mut source = BufferedSource::new(source, 8192);

        let mut header = [0u8; 7];
        let mut filled = 0usize;
        while filled < header.len() {
            match source.read(&mut header[filled..]) {
                Ok(0) => break,
                Ok(k) => filled += k,
                Err(e) => return Err(CadenceError::from(e)),
            }
        }
        if filled == 0 {
            return Err(CadenceError::EndOfStream);
        }
        if filled < header.len() {
            return Err(CadenceError::CorruptData(
                "stream ends inside the first ADTS header".to_string(),
            ));
        }

        let first = AdtsHeader::parse(&header)?;
        let sample_rate = adts::SAMPLING_FREQUENCIES[first.sampling_frequency_index as usize];
        let mut decoder = Self::open_with_params(
            source,
            sample_rate,
            first.channel_configuration,
            first.sampling_frequency_index as usize,
            false,
        )?;
        decoder.pending_header = Some(header);
        Ok(decoder)
    }

    /// Convenience alias mirroring the other format crates.
    pub fn open(source: Box<dyn ByteSource>) -> Result<Self, CadenceError> {
        Self::from_source(source)
    }

    /// Opens a raw AAC-LC stream: `config` (e.g. from an MP4 `esds`)
    /// followed by byte-aligned raw data blocks in `source`.
    pub fn from_config(
        config: &AudioSpecificConfig,
        source: Box<dyn ByteSource>,
    ) -> Result<Self, CadenceError> {
        let sample_rate = config.sample_rate()?;
        Self::open_with_params(
            BufferedSource::new(source, 8192),
            sample_rate,
            config.channel_configuration,
            config.sampling_frequency_index as usize,
            true,
        )
    }

    fn open_with_params(
        source: BufferedSource,
        sample_rate: u32,
        channels: u8,
        sf_index: usize,
        raw_blocks: bool,
    ) -> Result<Self, CadenceError> {
        let channels = channels as usize;
        if channels == 0 || channels > MAX_CHANNELS {
            return Err(CadenceError::UnsupportedFeature(format!(
                "AAC channel configuration {channels} is out of range (1..={MAX_CHANNELS})"
            )));
        }
        if sf_index >= tables::NUM_SWB_1024.len() {
            return Err(CadenceError::UnsupportedFeature(
                "sampling frequency index is reserved".to_string(),
            ));
        }

        let mut spectral_books = Vec::with_capacity(11);
        for b in 0..11 {
            let (bits, codes): (&[u8], &[u16]) = match b {
                0 => (&tables::SPECTRAL_BITS_1, &tables::SPECTRAL_CODES_1),
                1 => (&tables::SPECTRAL_BITS_2, &tables::SPECTRAL_CODES_2),
                2 => (&tables::SPECTRAL_BITS_3, &tables::SPECTRAL_CODES_3),
                3 => (&tables::SPECTRAL_BITS_4, &tables::SPECTRAL_CODES_4),
                4 => (&tables::SPECTRAL_BITS_5, &tables::SPECTRAL_CODES_5),
                5 => (&tables::SPECTRAL_BITS_6, &tables::SPECTRAL_CODES_6),
                6 => (&tables::SPECTRAL_BITS_7, &tables::SPECTRAL_CODES_7),
                7 => (&tables::SPECTRAL_BITS_8, &tables::SPECTRAL_CODES_8),
                8 => (&tables::SPECTRAL_BITS_9, &tables::SPECTRAL_CODES_9),
                9 => (&tables::SPECTRAL_BITS_10, &tables::SPECTRAL_CODES_10),
                _ => (&tables::SPECTRAL_BITS_11, &tables::SPECTRAL_CODES_11),
            };
            let codes: Vec<u32> = codes.iter().map(|&c| c as u32).collect();
            spectral_books.push(HuffmanTable::new(bits, &codes)?);
        }
        let sf_book: HuffmanTable = {
            let codes: Vec<u32> = tables::SCALEFACTOR_CODE.to_vec();
            HuffmanTable::new(&tables::SCALEFACTOR_BITS, &codes)?
        };

        let mut channels_state = Vec::with_capacity(channels);
        for _ in 0..channels {
            channels_state.push(ChannelState::new());
        }

        let mut info = StreamInfo::new(Format::Aac, sample_rate, channels as u16, 16);
        info.total_frames = None;
        info.validate()?;

        Ok(AacDecoder {
            source,
            info,
            channels,
            sf_index,
            pending_header: None,
            frame_count: 0,
            raw_blocks,
            channels_state,
            spectral_books,
            sf_book,
            mdct_long: Mdct::new(1024),
            mdct_short: Mdct::new(128),
            win_long_kb: kbd_window(1024, 4.0).into_boxed_slice(),
            win_long_sine: sine_window(1024).into_boxed_slice(),
            win_short_kb: kbd_window(128, 6.0).into_boxed_slice(),
            win_short_sine: sine_window(128).into_boxed_slice(),
            synth: vec![0.0; 2048].into_boxed_slice(),
            buf: vec![0.0; 1024].into_boxed_slice(),
            temp: vec![0.0; 128].into_boxed_slice(),
            noise: NoiseGenerator::new(),
            ms_mask: vec![false; 512].into_boxed_slice(),
            frame_buf: vec![0u8; FRAME_BUF_LEN].into_boxed_slice(),
            work_buf: vec![0u8; FRAME_BUF_LEN].into_boxed_slice(),
            staged: Vec::new(),
            staged_pos: 0,
            eof: false,
        })
    }

    /// The parsed stream metadata.
    pub fn streaminfo(&self) -> &StreamInfo {
        &self.info
    }

    /// Reads and decodes the next frame into staging. Ok(false) at EOS.
    fn decode_next_frame(&mut self) -> Result<bool, CadenceError> {
        if self.eof {
            return Ok(false);
        }

        let (num_blocks, body_len) = if self.raw_blocks {
            let len = self.fill_frame_buf()?;
            if len == 0 {
                return Ok(false);
            }
            (1u32, len)
        } else {
            match self.read_adts_frame() {
                Ok((h, l)) => (u32::from(h.raw_blocks_minus_one) + 1, l),
                Err(CadenceError::EndOfStream) => return Ok(false),
                Err(e) => return Err(e),
            }
        };
        self.work_buf[..body_len].copy_from_slice(&self.frame_buf[..body_len]);
        // Lend the work buffer out so the block decoder can use &mut self.
        let work = std::mem::take(&mut self.work_buf);
        let mut br = BitReader::new(&work[..body_len]);
        let dbg = std::env::var("AAC_DEBUG").is_ok();
        if dbg {
            eprintln!("[block] body_len={body_len} num_blocks={num_blocks}");
        }
        let result = self.decode_raw_block(&mut br, num_blocks);
        if dbg {
            eprintln!("[block] done");
        }
        self.work_buf = work;
        result?;
        self.frame_count += 1;
        Ok(true)
    }

    /// Reads one ADTS frame, returning (header, body length). Handles the
    /// header consumed during open via `pending_header`.
    fn read_adts_frame(&mut self) -> Result<(AdtsHeader, usize), CadenceError> {
        let mut header = [0u8; 7];
        if let Some(pending) = self.pending_header.take() {
            header = pending;
        } else {
            loop {
                if std::env::var("AAC_DEBUG").is_ok() {
                    eprintln!("[adts] scanning, header={:02X?}", &header[..2]);
                }
                match self.source.take_exact(&mut header) {
                    Ok(()) => {}
                    Err(CadenceError::EndOfStream) => {
                        self.eof = true;
                        return Err(CadenceError::EndOfStream);
                    }
                    Err(e) => return Err(e),
                }
                if header[0] == 0xFF && header[1] & 0xF0 == 0xF0 {
                    break;
                }
                // Slide one byte and try again.
                header.rotate_left(1);
                match self.source.take_exact(&mut header[6..7]) {
                    Ok(()) => {}
                    Err(CadenceError::EndOfStream) => {
                        self.eof = true;
                        return Err(CadenceError::EndOfStream);
                    }
                    Err(e) => return Err(e),
                }
            }
        }

        let parsed = AdtsHeader::parse(&header)?;
        if parsed.channel_configuration == 0 {
            return Err(CadenceError::UnsupportedFeature(
                "channel configuration 0 (PCE) is not supported".to_string(),
            ));
        }
        if parsed.channel_configuration as usize != self.channels {
            return Err(CadenceError::UnsupportedFeature(
                "channel configuration changes between ADTS frames".to_string(),
            ));
        }
        let body_len = parsed.frame_length - parsed.header_len;
        if body_len > self.frame_buf.len() {
            return Err(CadenceError::CorruptData(format!(
                "ADTS frame of {} bytes exceeds the {}-byte frame buffer",
                parsed.frame_length,
                self.frame_buf.len()
            )));
        }
        self.source.take_exact(&mut self.frame_buf[..body_len])?;
        Ok((parsed, body_len))
    }

    /// Fills the frame buffer for raw-block framing (reads to EOF once).
    fn fill_frame_buf(&mut self) -> Result<usize, CadenceError> {
        let mut len = 0usize;
        while len < self.frame_buf.len() {
            match self.source.read(&mut self.frame_buf[len..]) {
                Ok(0) => {
                    self.eof = true;
                    break;
                }
                Ok(n) => len += n,
                Err(e) => return Err(CadenceError::from(e)),
            }
        }
        Ok(len)
    }

    /// Parses `num_blocks` raw_data_blocks and stages interleaved PCM.
    fn decode_raw_block(
        &mut self,
        br: &mut BitReader,
        num_blocks: u32,
    ) -> Result<(), CadenceError> {
        // (channel, window info) in output order for this frame.
        let mut block_channels = [(
            0usize,
            WindowInfo {
                sequence: ONLY_LONG,
                num_windows: 1,
                num_window_groups: 1,
                group_len: [1; 8],
                max_sfb: 0,
                num_swb: 0,
                sf_index: self.sf_index,
            },
        ); MAX_CHANNELS];
        let mut block_count = 0usize;

        for _ in 0..num_blocks {
            let mut iter: u64 = 0;
            loop {
                iter += 1;
                if iter > 400 {
                    return Err(CadenceError::CorruptData(
                        "element loop exceeds 400 iterations".to_string(),
                    ));
                }
                if br.overread() {
                    return Err(CadenceError::CorruptData(
                        "bitstream overread while parsing raw_data_block".to_string(),
                    ));
                }
                let id = br.read_bits(3);
                match id {
                    SCE | LFE => {
                        let _tag = br.read_bits(4);
                        let ch =
                            Self::next_free_channel(&block_channels[..block_count], self.channels)?;
                        let win = self.decode_ics(br, ch, false, None)?;
                        block_channels[block_count] = (ch, win);
                        block_count += 1;
                    }
                    CPE => {
                        let _tag = br.read_bits(4);
                        let common_window = br.read_bit();
                        let ch_l =
                            Self::next_free_channel(&block_channels[..block_count], self.channels)?;
                        let ch_r = ch_l + 1;
                        if ch_r >= self.channels {
                            return Err(CadenceError::CorruptData(
                                "channel pair exceeds the channel configuration".to_string(),
                            ));
                        }

                        let (win, ms_present) = if common_window {
                            // common_window: ics_info precedes the ms data and
                            // the per-channel ICS bodies (which start with
                            // global_gain).
                            let win = self.decode_ics_info(br, ch_l)?;
                            let ms_present = br.read_bits(2);
                            let max_idx = win.num_window_groups * win.max_sfb;
                            for slot in self.ms_mask.iter_mut() {
                                *slot = false;
                            }
                            match ms_present {
                                0 => {}
                                1 => {
                                    for slot in self.ms_mask.iter_mut().take(max_idx) {
                                        *slot = br.read_bit();
                                    }
                                }
                                2 => {
                                    for slot in self.ms_mask.iter_mut().take(max_idx) {
                                        *slot = true;
                                    }
                                }
                                _ => {
                                    return Err(CadenceError::CorruptData(
                                        "ms_present = 3 is reserved".to_string(),
                                    ))
                                }
                            }
                            // ch1 inherits ch0's window info; its own
                            // previous-shape history is kept.
                            let ch0_cur = self.channels_state[ch_l].kb_window_cur;
                            let ch0_prev_seq = self.channels_state[ch_l].window_seq_prev;
                            let ch1 = &mut self.channels_state[ch_r];
                            ch1.kb_window_prev = ch1.kb_window_cur;
                            ch1.kb_window_cur = ch0_cur;
                            ch1.window_seq_prev = ch0_prev_seq;
                            (win, ms_present)
                        } else {
                            let win = self.decode_ics(br, ch_l, false, None)?;
                            let _win_r = self.decode_ics(br, ch_r, false, None)?;
                            (win, 0u32)
                        };

                        self.decode_ics(br, ch_l, true, Some(win))?;
                        self.decode_ics(br, ch_r, true, Some(win))?;

                        if common_window {
                            self.apply_cpe_stereo(ch_l, ch_r, &win, ms_present != 0);
                        }

                        block_channels[block_count] = (ch_l, win);
                        block_count += 1;
                        block_channels[block_count] = (ch_r, win);
                        block_count += 1;
                    }
                    FIL => {
                        let mut count = br.read_bits(4) as usize;
                        if count == 15 {
                            count = 14 + br.read_bits(8) as usize;
                        }
                        br.skip_bytes(count);
                    }
                    DSE => {
                        let _tag = br.read_bits(4);
                        let align = br.read_bit();
                        let mut count = br.read_bits(4) as usize;
                        if count == 15 {
                            count = br.read_bits(8) as usize;
                        }
                        if align {
                            br.byte_align();
                        }
                        br.skip_bytes(count);
                    }
                    2 | 5 => {
                        return Err(CadenceError::UnsupportedFeature(
                            "channel coupling / program config elements are not supported"
                                .to_string(),
                        ));
                    }
                    END => break,
                    other => {
                        return Err(CadenceError::CorruptData(format!(
                            "invalid element id {other}"
                        )));
                    }
                }
            }
            br.byte_align();
        }

        // TNS, windowing, and staging for every decoded channel.
        let mut decoded = [(
            0usize,
            WindowInfo {
                sequence: ONLY_LONG,
                num_windows: 1,
                num_window_groups: 1,
                group_len: [1; 8],
                max_sfb: 0,
                num_swb: 0,
                sf_index: self.sf_index,
            },
        ); MAX_CHANNELS];
        let mut decoded_count = 0usize;
        for (ch, win) in &block_channels[..block_count] {
            if decoded[..decoded_count].iter().any(|(c, _)| c == ch) {
                continue;
            }
            decoded[decoded_count] = (*ch, *win);
            decoded_count += 1;
            let state = &mut self.channels_state[*ch];
            if std::env::var("AAC_DUMP").is_ok() {
                let n = self.frame_count;
                let dir = std::path::Path::new("target/dump");
                let _ = std::fs::create_dir_all(dir);
                let _ = std::fs::write(
                    dir.join(format!("pre_{n}_{ch}.f32")),
                    f32_slice_bytes(&state.coeffs),
                );
                let bands: Vec<u8> = state.band_type[..win.max_sfb.max(1)].to_vec();
                let sfs: Vec<i32> = state.sfo[..win.max_sfb.max(1)].to_vec();
                let bands = bands.iter().map(|&b| b.to_string()).collect::<Vec<_>>();
                let sfs = sfs.iter().map(|&b| b.to_string()).collect::<Vec<_>>();
                let _ = std::fs::write(
                    dir.join(format!("meta_{n}_{ch}.txt")),
                    format!("bands: {}\nsfo: {}\n", bands.join(","), sfs.join(",")),
                );
                eprintln!(
                    "[frame {n} ch{ch}] seq={} kb_prev={} kb_cur={} max_sfb={} tns_n_filt={:?}",
                    win.sequence,
                    state.kb_window_prev,
                    state.kb_window_cur,
                    win.max_sfb,
                    &state.tns.n_filt[..win.num_windows],
                );
            }
            tns::apply(
                &state.tns,
                &mut state.coeffs,
                win.num_windows,
                win.num_swb,
                win.swb_offsets(),
                win.tns_max_bands(),
                win.max_sfb,
            );
            if std::env::var("AAC_DUMP").is_ok() {
                let n = self.frame_count;
                let dir = std::path::Path::new("target/dump");
                let _ = std::fs::write(
                    dir.join(format!("post_{n}_{ch}.f32")),
                    f32_slice_bytes(&state.coeffs),
                );
            }
        }
        for (ch, win) in &decoded[..decoded_count] {
            self.imdct_and_window(*ch, win);
        }

        // Interleave into staging.
        for i in 0..1024usize {
            for (ch, _) in &decoded[..decoded_count] {
                self.staged.push(self.channels_state[*ch].out[i]);
            }
        }
        Ok(())
    }

    #[allow(clippy::needless_range_loop)]
    fn next_free_channel(
        used: &[(usize, WindowInfo)],
        channels: usize,
    ) -> Result<usize, CadenceError> {
        let mut used_flags = [false; MAX_CHANNELS];
        for (ch, _) in used {
            used_flags[*ch] = true;
        }
        for ch in 0..channels {
            if !used_flags[ch] {
                return Ok(ch);
            }
        }
        Err(CadenceError::CorruptData(
            "more channels in the block than the configuration".to_string(),
        ))
    }

    /// decode_ics_info (ISO/IEC 14496-3 Table 4.5).
    fn decode_ics_info(
        &mut self,
        br: &mut BitReader,
        ch: usize,
    ) -> Result<WindowInfo, CadenceError> {
        // The reference decoder tolerates a set reserved bit here; so do we.
        let _reserved = br.read_bit();
        let sequence = br.read_bits(2) as u8;
        let shape = br.read_bit();

        let mut info = WindowInfo {
            sequence,
            num_windows: 1,
            num_window_groups: 1,
            group_len: [1; 8],
            max_sfb: 0,
            num_swb: 0,
            sf_index: self.sf_index,
        };

        if sequence == EIGHT_SHORT {
            info.max_sfb = br.read_bits(4) as usize;
            for _ in 0..7 {
                if br.read_bit() {
                    info.group_len[info.num_window_groups - 1] += 1;
                } else {
                    info.num_window_groups += 1;
                    info.group_len[info.num_window_groups - 1] = 1;
                }
            }
            info.num_windows = 8;
            info.num_swb = tables::NUM_SWB_128[info.sf_index] as usize;
        } else {
            info.max_sfb = br.read_bits(6) as usize;
            info.num_swb = tables::NUM_SWB_1024[info.sf_index] as usize;
            if br.read_bit() {
                return Err(CadenceError::UnsupportedFeature(
                    "prediction/LTP is not part of AAC-LC".to_string(),
                ));
            }
        }

        if info.max_sfb > info.num_swb {
            return Err(CadenceError::CorruptData(format!(
                "max_sfb {} exceeds num_swb {}",
                info.max_sfb, info.num_swb
            )));
        }

        // Channel windowing state: prev shape = old cur; cur = new bit.
        let state = &mut self.channels_state[ch];
        state.kb_window_prev = state.kb_window_cur;
        state.kb_window_cur = shape;
        Ok(info)
    }

    /// individual_channel_stream: global_gain, [ics_info when not a
    /// common-window channel], section_data, scale_factor_data,
    /// pulse/tns/gain flags, and spectral_data.
    fn decode_ics(
        &mut self,
        br: &mut BitReader,
        ch: usize,
        _common_window: bool,
        pre_window: Option<WindowInfo>,
    ) -> Result<WindowInfo, CadenceError> {
        let global_gain = br.read_bits(8);
        if std::env::var("AAC_DEBUG").is_ok() {
            eprintln!("[ics ch{ch}] gg={global_gain} at bit {}", br.pos());
        }
        let win = match pre_window {
            Some(w) => w,
            None => self.decode_ics_info(br, ch)?,
        };
        if std::env::var("AAC_DEBUG").is_ok() {
            eprintln!(
                "[ics ch{ch}] after info: seq={} max_sfb={} at bit {}",
                win.sequence,
                win.max_sfb,
                br.pos()
            );
        }
        self.decode_band_types(br, ch, &win)?;
        self.decode_scalefactors(br, ch, global_gain, &win)?;
        if std::env::var("AAC_DEBUG").is_ok() {
            let n = win.max_sfb * win.num_window_groups;
            let state = &self.channels_state[ch];
            let bands: Vec<u8> = state.band_type[..n].to_vec();
            let sfs: Vec<i32> = state.sfo[..n].to_vec();
            eprintln!(
                "[ics ch{ch}] gg={global_gain} seq={} max_sfb={} bands={:?} sfo={:?}",
                win.sequence, win.max_sfb, bands, sfs
            );
        }
        let pulse = self.decode_optional_tools(br, ch, &win)?;
        self.decode_spectral(br, ch, &win, pulse.as_ref())?;
        Ok(win)
    }

    /// section_data: codebook assignment per scalefactor band per group.
    fn decode_band_types(
        &mut self,
        br: &mut BitReader,
        ch: usize,
        win: &WindowInfo,
    ) -> Result<(), CadenceError> {
        let bits = if win.sequence == EIGHT_SHORT { 3 } else { 5 };
        let escape = (1usize << bits) - 1;

        for g in 0..win.num_window_groups {
            let mut k = 0usize;
            let mut sections = 0usize;
            while k < win.max_sfb {
                sections += 1;
                if sections > win.max_sfb {
                    return Err(CadenceError::CorruptData(
                        "too many bitstream sections".to_string(),
                    ));
                }
                let band_type = br.read_bits(4) as u8;
                if band_type == 12 {
                    return Err(CadenceError::CorruptData(
                        "reserved section band type 12".to_string(),
                    ));
                }
                let mut sect_end = k;
                loop {
                    let incr = br.read_bits(bits as u32) as usize;
                    // A zero increment terminates an escape run (the
                    // reference decoder accepts it anywhere); the section
                    // counter above bounds the loop.
                    sect_end += incr;
                    if sect_end > win.max_sfb {
                        return Err(CadenceError::CorruptData(
                            "section extends past max_sfb".to_string(),
                        ));
                    }
                    if incr != escape {
                        break;
                    }
                }
                for sfb in k..sect_end {
                    self.channels_state[ch].band_type[g * win.max_sfb + sfb] = band_type;
                }
                k = sect_end;
            }
        }
        Ok(())
    }

    /// scale_factor_data: per-band scalefactor deltas, noise energies, and
    /// intensity positions, then dequantization into linear gains.
    fn decode_scalefactors(
        &mut self,
        br: &mut BitReader,
        ch: usize,
        global_gain: u32,
        win: &WindowInfo,
    ) -> Result<(), CadenceError> {
        let mut offset_normal = global_gain as i32;
        let mut offset_noise = global_gain as i32 - NOISE_OFFSET;
        let mut offset_intensity = 0i32;
        let mut noise_flag = true;

        let state = &mut self.channels_state[ch];
        for g in 0..win.num_window_groups {
            for sfb in 0..win.max_sfb {
                let idx = g * win.max_sfb + sfb;
                match state.band_type[idx] {
                    ZERO_BT => state.sfo[idx] = 0,
                    INTENSITY_BT | INTENSITY_BT2 => {
                        offset_intensity += self.sf_book.decode_scalefactor_delta(br)?;
                        let clipped = offset_intensity.clamp(-155, 100);
                        state.sfo[idx] = clipped - 100;
                    }
                    NOISE_BT => {
                        if noise_flag {
                            offset_noise += br.read_bits(NOISE_PRE_BITS) as i32 - NOISE_PRE as i32;
                            noise_flag = false;
                        } else {
                            offset_noise += self.sf_book.decode_scalefactor_delta(br)?;
                        }
                        let clipped = offset_noise.clamp(-100, 155);
                        state.sfo[idx] = clipped;
                    }
                    _ => {
                        offset_normal += self.sf_book.decode_scalefactor_delta(br)?;
                        if offset_normal > 255 {
                            return Err(CadenceError::CorruptData(
                                "scalefactor out of range".to_string(),
                            ));
                        }
                        state.sfo[idx] = offset_normal - 100;
                    }
                }
            }
        }

        // Dequantize: normal/noise gain = 2^(sfo/4); intensity gain =
        // 2^(−ipd/4) where sfo stored ipd − 100.
        for g in 0..win.num_window_groups {
            for sfb in 0..win.max_sfb {
                let idx = g * win.max_sfb + sfb;
                state.sf[idx] = match state.band_type[idx] {
                    ZERO_BT => 0.0,
                    INTENSITY_BT | INTENSITY_BT2 => (-((state.sfo[idx] + 100) as f32) / 4.0).exp2(),
                    _ => (state.sfo[idx] as f32 / 4.0).exp2(),
                };
            }
        }
        Ok(())
    }

    /// pulse_data, tns_data, and gain_control flags.
    fn decode_optional_tools(
        &mut self,
        br: &mut BitReader,
        ch: usize,
        win: &WindowInfo,
    ) -> Result<Option<Pulse>, CadenceError> {
        let mut pulse = None;
        if br.read_bit() {
            if win.sequence == EIGHT_SHORT {
                return Err(CadenceError::CorruptData(
                    "pulse tool is not allowed in eight-short sequences".to_string(),
                ));
            }
            let num_pulse = br.read_bits(2) as usize + 1;
            let pulse_swb = br.read_bits(6) as usize;
            if pulse_swb >= win.num_swb {
                return Err(CadenceError::CorruptData(
                    "pulse start band exceeds num_swb".to_string(),
                ));
            }
            let mut pos = [0usize; 4];
            let mut amp = [0i32; 4];
            pos[0] = win.swb_offsets()[pulse_swb] as usize + br.read_bits(5) as usize;
            if pos[0] >= win.swb_offsets()[win.num_swb] as usize {
                return Err(CadenceError::CorruptData(
                    "pulse position out of range".to_string(),
                ));
            }
            amp[0] = br.read_bits(4) as i32;
            for i in 1..num_pulse {
                pos[i] = pos[i - 1] + br.read_bits(5) as usize;
                if pos[i] >= win.swb_offsets()[win.num_swb] as usize {
                    return Err(CadenceError::CorruptData(
                        "pulse position out of range".to_string(),
                    ));
                }
                amp[i] = br.read_bits(4) as i32;
            }
            pulse = Some(Pulse {
                num_pulse,
                pos,
                amp,
            });
        }

        // TNS is parsed per channel; applied later (before windowing).
        self.channels_state[ch].tns = Tns::default();
        if br.read_bit() {
            tns::parse(
                &mut self.channels_state[ch].tns,
                br,
                win.num_windows,
                win.sequence == EIGHT_SHORT,
            )?;
        }

        if br.read_bit() {
            return Err(CadenceError::UnsupportedFeature(
                "gain control is not part of AAC-LC".to_string(),
            ));
        }
        Ok(pulse)
    }

    /// spectral_data: Huffman decode + requantization + PNS + zero bands,
    /// then pulse application.
    #[allow(clippy::needless_range_loop, clippy::too_many_arguments)]
    fn decode_spectral(
        &mut self,
        br: &mut BitReader,
        ch: usize,
        win: &WindowInfo,
        pulse: Option<&Pulse>,
    ) -> Result<(), CadenceError> {
        let offsets = win.swb_offsets();
        let ChannelState {
            coeffs,
            band_type,
            sf,
            ..
        } = &mut self.channels_state[ch];
        let band_type: &[u8] = band_type;
        let sf: &[f32] = sf;

        // Zero the whole transform; bands overwrite their ranges.
        for slot in coeffs.iter_mut() {
            *slot = 0.0;
        }

        let mut window_base = 0usize;
        for g in 0..win.num_window_groups {
            let group_len = win.group_len[g];
            for sfb in 0..win.max_sfb {
                let idx = g * win.max_sfb + sfb;
                let band_type = band_type[idx];
                let start = offsets[sfb] as usize;
                let off_len = (offsets[sfb + 1] - offsets[sfb]) as usize;
                let gain = sf[idx];

                for group in 0..group_len {
                    let base = (window_base + group) * BLOCK_LEN + start;
                    let out = &mut coeffs[base..base + off_len];
                    match band_type {
                        ZERO_BT | INTENSITY_BT2 | INTENSITY_BT => {
                            for slot in out.iter_mut() {
                                *slot = 0.0;
                            }
                        }
                        NOISE_BT => {
                            // Reference scalefactors are negative; the noise
                            // band gain inherits that sign.
                            self.noise.fill_scaled(out, -gain);
                        }
                        cb => {
                            let book = &self.spectral_books[(cb - 1) as usize];
                            // VLC symbol → packed descriptor (reference
                            // `cb_idx`): dims/vals/signs per book class.
                            let idx_table: &[u16] = match cb {
                                1..=4 => &tables::CODEBOOK_IDX_02,
                                5..=6 => &tables::CODEBOOK_IDX_4,
                                7..=8 => &tables::CODEBOOK_IDX_6,
                                9..=10 => &tables::CODEBOOK_IDX_8,
                                _ => &tables::CODEBOOK_IDX_10,
                            };
                            let dim: usize = if cb >= 5 { 2 } else { 4 };

                            for k in (0..off_len).step_by(dim) {
                                let list_index = book.decode(br)?;
                                let packed =
                                    idx_table.get(list_index as usize).copied().unwrap_or(0) as u32;
                                if std::env::var("AAC_TRACE").is_ok()
                                    && window_base == 0
                                    && (band_type == 2 || band_type == 13)
                                {
                                    eprintln!("[tr] p={} wb={window_base} sfb={sfb} sym={list_index} packed={packed:#06x}", br.pos());
                                }

                                if cb <= 4 {
                                    // Quads: 2-bit value fields, low bits =
                                    // dim 0.
                                    if cb <= 2 {
                                        // Signed values from {−1, 0, +1}.
                                        for d in 0..4 {
                                            let vi = (packed >> (2 * d)) & 3;
                                            let v = match vi {
                                                0 => -1.0,
                                                2 => 1.0,
                                                _ => 0.0,
                                            };
                                            out[k + d] = v * gain;
                                        }
                                    } else {
                                        // Unsigned quads with nnz sign bits,
                                        // consumed in dim order over the
                                        // nonzero dims.
                                        let nnz = (packed >> 8) & 15;
                                        let mut sign_bits = br.read_bits(nnz) << (32 - nnz);
                                        for d in 0..4 {
                                            let vi = (packed >> (2 * d)) & 3;
                                            let mut mag = tables::CODEBOOK_VALS_10_16
                                                .get(vi as usize)
                                                .copied()
                                                .unwrap_or(0.0);
                                            if vi != 0 {
                                                if sign_bits & (1 << 31) != 0 {
                                                    mag = -mag;
                                                }
                                                sign_bits <<= 1;
                                            }
                                            out[k + d] = mag * gain;
                                        }
                                    }
                                } else if cb <= 10 {
                                    // Unsigned pairs with nnz sign bits.
                                    let nnz = (packed >> 8) & 15;
                                    let mut sign_bits = br.read_bits(nnz) << (32 - nnz);
                                    for d in 0..2 {
                                        let vi = (packed >> (4 * d)) & 15;
                                        let mut mag = tables::CODEBOOK_VALS_10_16
                                            .get(vi as usize)
                                            .copied()
                                            .unwrap_or(0.0);
                                        if vi != 0 {
                                            if sign_bits & (1 << 31) != 0 {
                                                mag = -mag;
                                            }
                                            sign_bits <<= 1;
                                        }
                                        out[k + d] = mag * gain;
                                    }
                                } else if packed == 0 {
                                    // All-zero pair: no sign bits.
                                    out[k] = 0.0;
                                    out[k + 1] = 0.0;
                                } else {
                                    // Book 11: nnz sign bits up front, then
                                    // per dim: escape dims read a unary
                                    // ones-count + (ones+4) magnitude bits;
                                    // a sign bit is consumed only for
                                    // nonzero (escaped or table-valued)
                                    // dims.
                                    let nnz = (packed >> 12) & 15;
                                    let esc_mask = (packed >> 8) & 15;
                                    let mut sign_bits = br.read_bits(nnz) << (32 - nnz);
                                    for d in 0..2 {
                                        let vi = (packed >> (4 * d)) & 15;
                                        let escaped = (esc_mask >> d) & 1 == 1;
                                        let neg = sign_bits & (1 << 31) != 0;
                                        let mut mag = if escaped {
                                            let mut ones = 0u32;
                                            loop {
                                                if !br.read_bit() {
                                                    break;
                                                }
                                                ones += 1;
                                                if ones > 8 {
                                                    return Err(CadenceError::CorruptData(
                                                        "spectral escape sequence overflow"
                                                            .to_string(),
                                                    ));
                                                }
                                            }
                                            let q = (1u32 << (ones + 4)) | br.read_bits(ones + 4);
                                            (q as f32).powf(4.0 / 3.0)
                                        } else {
                                            tables::CODEBOOK_VALS_10_16
                                                .get(vi as usize)
                                                .copied()
                                                .unwrap_or(0.0)
                                        };
                                        if neg {
                                            mag = -mag;
                                        }
                                        out[k + d] = mag * gain;
                                        if escaped || vi != 0 {
                                            sign_bits <<= 1;
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            window_base += group_len;
        }

        if std::env::var("AAC_DEBUG").is_ok() {
            eprintln!("[spec ch{ch}] first 16: {:?}", &coeffs[..16]);
        }
        // Pulses modify specific quantized coefficients; applied in the
        // dequantized domain like the reference decoder.
        if let Some(pulse) = pulse {
            let mut idx = 0usize;
            for i in 0..pulse.num_pulse {
                let mut co = coeffs[pulse.pos[i]];
                while (offsets[idx + 1] as usize) <= pulse.pos[i] {
                    idx += 1;
                }
                let band_sf = sf[idx];
                if band_type[idx] != NOISE_BT && band_sf != 0.0 {
                    let mut ico = -(pulse.amp[i] as f32);
                    if co != 0.0 {
                        co /= band_sf;
                        ico = co / (co.abs().sqrt().sqrt()) + if co > 0.0 { -ico } else { ico };
                    }
                    coeffs[pulse.pos[i]] = ico.abs().cbrt() * ico * band_sf;
                }
            }
        }
        Ok(())
    }

    /// IMDCT + windowing + weighted overlap-add for one channel.
    ///
    /// Exact port of the reference decoder's `imdct_and_windowing`: the
    /// half-length MDCT output is kept in the reference `buf` layout, and
    /// the window sequences are realized through the same lap/saved-update
    /// structure (all "meaningless" short↔long transitions are treated as
    /// short↔short, with special handling inside EIGHT_SHORT).
    #[allow(clippy::needless_range_loop)]
    fn imdct_and_window(&mut self, ch: usize, win: &WindowInfo) {
        let seq_cur = win.sequence;
        let Self {
            channels_state,
            mdct_long,
            mdct_short,
            win_long_kb,
            win_long_sine,
            win_short_kb,
            win_short_sine,
            synth,
            buf,
            temp,
            ..
        } = self;
        let state = &mut channels_state[ch];
        let ChannelState {
            coeffs,
            out,
            saved,
            window_seq_prev,
            kb_window_cur,
            kb_window_prev,
            ..
        } = &mut *state;
        let coeffs: &[f32] = coeffs;

        let seq_prev = *window_seq_prev;
        *window_seq_prev = seq_cur;
        let kb_cur = *kb_window_cur;
        let kb_prev = *kb_window_prev;

        let lwindow_prev: &[f32] = if kb_prev { win_long_kb } else { win_long_sine };
        let swindow: &[f32] = if kb_cur { win_short_kb } else { win_short_sine };
        let swindow_prev: &[f32] = if kb_prev {
            win_short_kb
        } else {
            win_short_sine
        };

        // IMDCT into the half-length buf layout: with y the natural
        // 2M-point synthesis, buf[i] = y[M/2−1−i] and buf[M/2+i] = −y[M+i].
        if seq_cur != EIGHT_SHORT {
            mdct_long.imdct(coeffs, synth);
            for i in 0..512 {
                buf[i] = synth[511 - i];
                buf[512 + i] = -synth[1024 + i];
            }
        } else {
            let mut y = [0.0f32; 256];
            for w in 0..8 {
                mdct_short.imdct(&coeffs[w * 128..w * 128 + 128], &mut y);
                let b = w * 128;
                for i in 0..64 {
                    buf[b + i] = y[63 - i];
                    buf[b + 64 + i] = -y[128 + i];
                }
            }
        }

        let long_lap = (seq_prev == ONLY_LONG || seq_prev == LONG_STOP)
            && (seq_cur == ONLY_LONG || seq_cur == LONG_START);
        if long_lap {
            vector_fmul_window(out, saved, buf, lwindow_prev, 512);
        } else {
            out[..448].copy_from_slice(&saved[..448]);

            if seq_cur == EIGHT_SHORT {
                vector_fmul_window(&mut out[448..], &saved[448..], buf, swindow_prev, 64);
                vector_fmul_window(&mut out[448 + 128..], &buf[64..], &buf[128..], swindow, 64);
                vector_fmul_window(&mut out[448 + 256..], &buf[192..], &buf[256..], swindow, 64);
                vector_fmul_window(&mut out[448 + 384..], &buf[320..], &buf[384..], swindow, 64);
                vector_fmul_window(temp, &buf[448..], &buf[512..], swindow, 64);
                out[448 + 512..448 + 576].copy_from_slice(&temp[..64]);
            } else {
                vector_fmul_window(&mut out[448..], &saved[448..], buf, swindow_prev, 64);
                out[576..1024].copy_from_slice(&buf[64..512]);
            }
        }

        // Buffer update.
        if seq_cur == EIGHT_SHORT {
            saved[..64].copy_from_slice(&temp[64..128]);
            vector_fmul_window(&mut saved[64..], &buf[576..], &buf[640..], swindow, 64);
            vector_fmul_window(&mut saved[192..], &buf[704..], &buf[768..], swindow, 64);
            vector_fmul_window(&mut saved[320..], &buf[832..], &buf[896..], swindow, 64);
            saved[448..512].copy_from_slice(&buf[960..1024]);
        } else if seq_cur == LONG_START {
            saved[..448].copy_from_slice(&buf[512..960]);
            saved[448..512].copy_from_slice(&buf[960..1024]);
        } else {
            saved.copy_from_slice(&buf[512..1024]);
        }
    }

    /// M/S + intensity stereo for a common-window channel pair.
    ///
    /// Mirrors the reference decoder: the M/S butterfly only touches bands
    /// whose codebook is below the noise/intensity range in BOTH channels,
    /// and the intensity scale combines the right channel's band type and
    /// (when MS data is present) the M/S decision.
    fn apply_cpe_stereo(&mut self, ch_l: usize, ch_r: usize, win: &WindowInfo, ms_present: bool) {
        let (left_state, right_state) = {
            let (a, b) = self.channels_state.split_at_mut(ch_r);
            (&mut a[ch_l], &mut b[0])
        };
        stereo::apply_mid_side(
            &mut left_state.coeffs,
            &mut right_state.coeffs,
            &left_state.band_type,
            &right_state.band_type,
            &self.ms_mask,
            win.num_window_groups,
            &win.group_len,
            win.max_sfb,
            win.swb_offsets(),
        );

        // Intensity: rebuild the per-band gains for the right channel.
        let mut gains = [0.0f32; 512];
        for g in 0..win.num_window_groups {
            for sfb in 0..win.max_sfb {
                let idx = g * win.max_sfb + sfb;
                let mut c: i32 = match right_state.band_type[idx] {
                    INTENSITY_BT2 => -1,
                    INTENSITY_BT => 1,
                    _ => 0,
                };
                if c != 0 {
                    if ms_present && self.ms_mask.get(idx).copied().unwrap_or(false) {
                        c *= 1 - 2;
                    }
                    gains[idx] = (c as f32) * right_state.sf[idx];
                }
            }
        }
        stereo::apply_intensity(
            &mut left_state.coeffs,
            &mut right_state.coeffs,
            &right_state.band_type,
            &gains,
            win.num_window_groups,
            &win.group_len,
            win.max_sfb,
            win.swb_offsets(),
        );
    }
}

/// Little-endian bytes of an f32 slice (AAC_DUMP debug-dump helper).
fn f32_slice_bytes(slice: &[f32]) -> Vec<u8> {
    slice.iter().flat_map(|v| v.to_le_bytes()).collect()
}

impl Decoder for AacDecoder {
    fn info(&self) -> &StreamInfo {
        &self.info
    }

    fn seek(&mut self, frame: u64) -> Result<(), CadenceError> {
        // Reset to the start of the stream, then decode-and-discard.
        // (AAC has no frame-accurate index; this MAY block and is not
        // real-time safe.)
        self.source.seek_to(0)?;
        self.pending_header = None;
        self.staged.clear();
        self.staged_pos = 0;
        self.eof = false;
        for state in &mut self.channels_state {
            state.saved.fill(0.0);
            state.window_seq_prev = ONLY_LONG;
            state.kb_window_prev = false;
            state.kb_window_cur = false;
        }
        self.noise = NoiseGenerator::new();

        let channels = self.channels;
        let mut discarded = 0u64;
        let mut scratch = vec![0.0f32; 1024 * channels].into_boxed_slice();
        while discarded < frame {
            let got = self.decode(&mut scratch)?;
            if got == 0 {
                return Err(CadenceError::CorruptData(
                    "stream ended while seeking".to_string(),
                ));
            }
            discarded += got as u64;
        }
        Ok(())
    }

    fn decode(&mut self, buffer: &mut [f32]) -> Result<usize, CadenceError> {
        let channels = self.channels;
        if buffer.len() % channels != 0 {
            return Err(CadenceError::InvalidFormat(format!(
                "buffer length {} is not a multiple of the channel count {}",
                buffer.len(),
                channels
            )));
        }
        let mut written = 0usize;
        while written < buffer.len() {
            if self.staged_pos >= self.staged.len() {
                self.staged.clear();
                self.staged_pos = 0;
                match self.decode_next_frame() {
                    Ok(true) => {}
                    Ok(false) => break,
                    Err(e) => return Err(e),
                }
            }
            let available = self.staged.len() - self.staged_pos;
            let take = (buffer.len() - written).min(available);
            buffer[written..written + take]
                .copy_from_slice(&self.staged[self.staged_pos..self.staged_pos + take]);
            self.staged_pos += take;
            written += take;
        }
        Ok(written / channels)
    }
}

/// Reader wrapper implementing [`FormatReader`] for ADTS streams.
pub struct AacReader {
    decoder: AacDecoder,
}

impl FormatReader for AacReader {
    fn open(source: Box<dyn Read + Send>) -> Result<Self, CadenceError> {
        Ok(AacReader {
            decoder: AacDecoder::open(Box::new(Unseekable(source)))?,
        })
    }

    fn decoder(&mut self) -> &mut dyn Decoder {
        &mut self.decoder
    }

    fn info(&self) -> &StreamInfo {
        self.decoder.info()
    }
}
