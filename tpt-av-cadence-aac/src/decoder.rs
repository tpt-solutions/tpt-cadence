//! AAC-LC raw_data_block parsing and the top-level `Decoder` implementation.

use std::io::Read;

use tpt_av_cadence_core::{
    BufferedSource, ByteSource, CadenceError, Decoder, Format, FormatReader, StreamInfo, Unseekable,
};

use crate::adts::{self, AdtsHeader};
use crate::audio_specific::AudioSpecificConfig;
use crate::bitreader::BitReader;
use crate::huffman::HuffmanTable;
use crate::imdct::{kbd_window, sine_window, Mdct};
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
    /// Overlap-add history (1024 samples; only the first 128 are used
    /// after EIGHT_SHORT frames).
    saved: Box<[f32]>,
    saved_len: usize,
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
            saved: vec![0.0; 1024].into_boxed_slice(),
            saved_len: 0,
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
    /// Long-window synthesis (2048).
    buf_long: Box<[f32]>,
    /// Short-window synthesis (256).
    synth_short: Box<[f32]>,
    /// EIGHT_SHORT overlap accumulator (1152 = 1024 + 128 tail).
    acc: Box<[f32]>,
    /// Assembled frame window (2048).
    win_full: Box<[f32]>,
    noise: NoiseGenerator,

    /// Per-band scratch (groups × max_sfb ≤ 512 entries).
    band_type: Box<[u8]>,
    sfo: Box<[i32]>,
    sf: Box<[f32]>,
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
            buf_long: vec![0.0; 2048].into_boxed_slice(),
            synth_short: vec![0.0; 256].into_boxed_slice(),
            acc: vec![0.0; 1152].into_boxed_slice(),
            win_full: vec![0.0; 2048].into_boxed_slice(),
            noise: NoiseGenerator::new(),
            band_type: vec![0u8; 512].into_boxed_slice(),
            sfo: vec![0i32; 512].into_boxed_slice(),
            sf: vec![0.0f32; 512].into_boxed_slice(),
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
        let mut block_channels: Vec<(usize, WindowInfo)> = Vec::new();

        for _ in 0..num_blocks {
            let mut iter: u64 = 0;
            loop {
                iter += 1;
                if iter > 400 {
                    return Err(CadenceError::CorruptData(
                        "element loop exceeds 400 iterations".to_string(),
                    ));
                }
                eprintln!("[el] iter {iter} pos {}", br.pos());
                if br.overread() {
                    return Err(CadenceError::CorruptData(
                        "bitstream overread while parsing raw_data_block".to_string(),
                    ));
                }
                let id = br.read_bits(3);
                match id {
                    SCE | LFE => {
                        let _tag = br.read_bits(4);
                        let ch = Self::next_free_channel(&block_channels, self.channels)?;
                        let win = self.decode_ics(br, ch, false, None)?;
                        block_channels.push((ch, win));
                    }
                    CPE => {
                        let _tag = br.read_bits(4);
                        let common_window = br.read_bit();
                        let ch_l = Self::next_free_channel(&block_channels, self.channels)?;
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

                        block_channels.push((ch_l, win));
                        block_channels.push((ch_r, win));
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
        let mut decoded: Vec<(usize, WindowInfo)> = Vec::new();
        for (ch, win) in &block_channels {
            if decoded.iter().any(|(c, _)| c == ch) {
                continue;
            }
            decoded.push((*ch, *win));
            let state = &mut self.channels_state[*ch];
            if std::env::var("AAC_DUMP").is_ok() {
                let n = self.frame_count;
                let dir = std::path::Path::new("target/dump");
                let _ = std::fs::create_dir_all(dir);
                let _ = std::fs::write(
                    dir.join(format!("pre_{n}_{ch}.f32")),
                    f32_slice_bytes(&state.coeffs),
                );
                eprintln!(
                    "[frame {n} ch{ch}] seq={} kb_prev={} kb_cur={} max_sfb={} tns_filt={}",
                    win.sequence,
                    state.kb_window_prev,
                    state.kb_window_cur,
                    win.max_sfb,
                    state.tns.n_filt[..win.num_windows].len().min(8),
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
        for (ch, win) in &decoded {
            self.imdct_and_window(*ch, win);
        }

        // Interleave into staging.
        for i in 0..1024usize {
            for (ch, _) in &decoded {
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
        self.decode_band_types(br, &win)?;
        self.decode_scalefactors(br, global_gain, &win)?;
        if std::env::var("AAC_DEBUG").is_ok() {
            let bands: Vec<u8> = self.band_type[..win.max_sfb.min(24)].to_vec();
            let sfs: Vec<i32> = self.sfo[..win.max_sfb.min(24)].to_vec();
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
                    if incr == 0 {
                        return Err(CadenceError::CorruptData(
                            "zero-length section (no progress)".to_string(),
                        ));
                    }
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
                    self.band_type[g * win.max_sfb + sfb] = band_type;
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
        global_gain: u32,
        win: &WindowInfo,
    ) -> Result<(), CadenceError> {
        let mut offset_normal = global_gain as i32;
        let mut offset_noise = global_gain as i32 - NOISE_OFFSET;
        let mut offset_intensity = 0i32;
        let mut noise_flag = true;

        for g in 0..win.num_window_groups {
            for sfb in 0..win.max_sfb {
                let idx = g * win.max_sfb + sfb;
                match self.band_type[idx] {
                    ZERO_BT => self.sfo[idx] = 0,
                    INTENSITY_BT | INTENSITY_BT2 => {
                        offset_intensity += self.sf_book.decode_scalefactor_delta(br)?;
                        let clipped = offset_intensity.clamp(-155, 100);
                        self.sfo[idx] = clipped - 100;
                    }
                    NOISE_BT => {
                        if noise_flag {
                            offset_noise += br.read_bits(NOISE_PRE_BITS) as i32 - NOISE_PRE as i32;
                            noise_flag = false;
                        } else {
                            offset_noise += self.sf_book.decode_scalefactor_delta(br)?;
                        }
                        let clipped = offset_noise.clamp(-100, 155);
                        self.sfo[idx] = clipped;
                    }
                    _ => {
                        offset_normal += self.sf_book.decode_scalefactor_delta(br)?;
                        if offset_normal > 255 {
                            return Err(CadenceError::CorruptData(
                                "scalefactor out of range".to_string(),
                            ));
                        }
                        self.sfo[idx] = offset_normal - 100;
                    }
                }
            }
        }

        // Dequantize: normal/noise gain = 2^(sfo/4); intensity gain =
        // 2^(−ipd/4) where sfo stored ipd − 100.
        for g in 0..win.num_window_groups {
            for sfb in 0..win.max_sfb {
                let idx = g * win.max_sfb + sfb;
                self.sf[idx] = match self.band_type[idx] {
                    ZERO_BT => 0.0,
                    INTENSITY_BT | INTENSITY_BT2 => (-((self.sfo[idx] + 100) as f32) / 4.0).exp2(),
                    _ => (self.sfo[idx] as f32 / 4.0).exp2(),
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
    #[allow(clippy::needless_range_loop)]
    #[allow(clippy::needless_range_loop, clippy::too_many_arguments)]
    fn decode_spectral(
        &mut self,
        br: &mut BitReader,
        ch: usize,
        win: &WindowInfo,
        pulse: Option<&Pulse>,
    ) -> Result<(), CadenceError> {
        let offsets = win.swb_offsets();

        // Zero the whole transform; bands overwrite their ranges.
        let coeffs = &mut self.channels_state[ch].coeffs;
        for slot in coeffs.iter_mut() {
            *slot = 0.0;
        }

        let mut window_base = 0usize;
        for g in 0..win.num_window_groups {
            let group_len = win.group_len[g];
            for sfb in 0..win.max_sfb {
                let idx = g * win.max_sfb + sfb;
                let band_type = self.band_type[idx];
                let start = offsets[sfb] as usize;
                let off_len = (offsets[sfb + 1] - offsets[sfb]) as usize;
                let gain = self.sf[idx];

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
                            self.noise.fill_scaled(out, gain);
                        }
                        cb => {
                            let book = &self.spectral_books[(cb - 1) as usize];
                            let (idx_table, dim, signed): (&[u16], usize, bool) = match cb {
                                1..=2 => (&tables::CODEBOOK_IDX_02, 4, true),
                                3..=4 => (&tables::CODEBOOK_IDX_02, 4, false),
                                5..=6 => (&tables::CODEBOOK_IDX_4, 2, true),
                                7..=8 => (&tables::CODEBOOK_IDX_6, 2, false),
                                9..=10 => (&tables::CODEBOOK_IDX_8, 2, false),
                                _ => (&tables::CODEBOOK_IDX_10, 2, false),
                            };
                            let has_escape = cb == 11;

                            for _ in 0..off_len / dim {
                                let list_index = book.decode(br)?;
                                // VLC symbols map through the codebook idx
                                // table to a packed descriptor (FFmpeg
                                // `cb_idx`).
                                let packed =
                                    idx_table.get(list_index as usize).copied().unwrap_or(0) as u32;

                                if signed {
                                    // VMUL4: quads from vals0 = {−1, 0, 1},
                                    // two bits per dim, low bits = dim 0.
                                    for d in 0..dim {
                                        let vi = (packed >> (2 * d)) & 3;
                                        let v = match vi {
                                            0 => -1.0,
                                            2 => 1.0,
                                            _ => 0.0,
                                        };
                                        out[d] = v * gain;
                                    }
                                } else if has_escape {
                                    // Book 11 (VMUL2S + escape): dims from
                                    // 4-bit nibbles; sign bits read up front
                                    // (MSB first, nnz of them); dims flagged
                                    // in the escape mask read a unary-length
                                    // magnitude.
                                    let nnz = (packed >> 12) & 15;
                                    let esc_mask = (packed >> 8) & 15;
                                    // Sign bits MSB-aligned like SHOW_UBITS.
                                    let mut sign_bits = br.read_bits(nnz) << (32 - nnz);
                                    for d in 0..dim {
                                        let vi = (packed >> (4 * d)) & 15;
                                        let neg = sign_bits & (1 << 31) != 0;
                                        let mut mag = if (esc_mask >> d) & 1 == 1 {
                                            // escape: unary ones + terminator,
                                            // then (ones + 4) magnitude bits
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
                                        out[d] = mag * gain;
                                        sign_bits <<= 1;
                                    }
                                } else {
                                    // VMUL4S / VMUL2S: unsigned magnitudes
                                    // with nnz sign bits applied in dim
                                    // order to the nonzero dims.
                                    let nnz = (packed >> 8) & 15;
                                    // MSB-aligned like the reference cache.
                                    let mut sign_bits = br.read_bits(nnz) << (32 - nnz);
                                    let nz_mask = (packed >> 12) & 15;
                                    for d in 0..dim {
                                        let vi = (packed >> (if dim == 4 { 2 * d } else { 4 * d }))
                                            & (if dim == 4 { 3 } else { 15 });
                                        let mut mag = tables::CODEBOOK_VALS_10_16
                                            .get(vi as usize)
                                            .copied()
                                            .unwrap_or(0.0);
                                        let nonzero = vi != 0;
                                        if nonzero {
                                            let neg = sign_bits & (1 << 31) != 0;
                                            if neg {
                                                mag = -mag;
                                            }
                                            sign_bits <<= 1;
                                        }
                                        out[d] = mag * gain;
                                        let _ = nz_mask;
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
                let band_sf = self.sf[idx];
                if self.band_type[idx] != NOISE_BT && band_sf != 0.0 {
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

    /// IMDCT + windowing + weighted overlap-add for one channel
    /// (ISO/IEC 14496-3 §4.6.4 filter bank; TDAC-verified in imdct.rs).
    #[allow(clippy::needless_range_loop)]
    fn imdct_and_window(&mut self, ch: usize, win: &WindowInfo) {
        let short = win.sequence == EIGHT_SHORT;
        let _m = if short { 128 } else { 1024 }; // coefficients per transform
        let kb_prev = self.channels_state[ch].kb_window_prev;
        let kb_cur = self.channels_state[ch].kb_window_cur;

        // Assemble the frame window: [left half | right half].
        // Left half = previous frame's shape; right half = current shape.
        {
            let cur: &[f32] = if kb_cur {
                &self.win_short_kb
            } else {
                &self.win_short_sine
            };
            let prev: &[f32] = if kb_prev {
                &self.win_short_kb
            } else {
                &self.win_short_sine
            };
            if short {
                let win_full = &mut self.win_full;
                for j in 0..128 {
                    win_full[j] = prev[j];
                    win_full[128 + j] = cur[127 - j];
                    win_full[256 + j] = cur[j];
                    win_full[384 + j] = cur[127 - j];
                    win_full[512 + j] = cur[j];
                    win_full[640 + j] = cur[127 - j];
                    win_full[768 + j] = cur[j];
                    win_full[896 + j] = cur[127 - j];
                    win_full[1024 + j] = cur[j];
                    win_full[1152 + j] = cur[127 - j];
                    win_full[1280 + j] = cur[j];
                    win_full[1408 + j] = cur[127 - j];
                    win_full[1536 + j] = cur[j];
                    win_full[1664 + j] = cur[127 - j];
                    win_full[1792 + j] = cur[j];
                    win_full[1920 + j] = cur[127 - j];
                }
            } else {
                let left: &[f32] = if kb_prev {
                    &self.win_long_kb
                } else {
                    &self.win_long_sine
                };
                let right: &[f32] = if kb_cur {
                    &self.win_long_kb
                } else {
                    &self.win_long_sine
                };
                let win_full = &mut self.win_full;
                win_full[..1024].copy_from_slice(left);
                for n in 0..1024 {
                    win_full[1024 + n] = right[1023 - n];
                }
            }
        }

        let saved_len = self.channels_state[ch].saved_len;
        let state = &mut self.channels_state[ch];
        let ChannelState {
            saved, out, coeffs, ..
        } = state;
        let saved: &mut Box<[f32]> = saved;
        let out: &mut Box<[f32]> = out;
        let coeffs: &[f32] = coeffs;

        if short {
            // 8 short IMDCTs (128 coeffs → 256 samples) at hop 128,
            // overlap-ADDed (consecutive windows overlap by 128).
            let acc = &mut self.acc;
            acc.fill(0.0);
            let mut x = [0.0f32; 256];
            for w in 0..8 {
                self.mdct_short
                    .imdct(&coeffs[w * 128..w * 128 + 128], &mut x);
                let base = w * 128;
                for j in 0..128 {
                    acc[base + j] += x[j] * self.win_full[base + j];
                    acc[base + 128 + j] += x[128 + j] * self.win_full[base + 128 + j];
                }
            }
            for n in 0..saved_len {
                acc[n] += saved[n];
            }
            out.copy_from_slice(&acc[..1024]);
            saved[..128].copy_from_slice(&acc[1024..1152]);
            saved[128..].fill(0.0);
            self.channels_state[ch].saved_len = 128;
        } else {
            self.mdct_long.imdct(coeffs, &mut self.buf_long);
            // window (in place), overlap-add the left half
            for n in 0..2048 {
                self.buf_long[n] *= self.win_full[n];
            }
            for n in 0..saved_len {
                self.buf_long[n] += saved[n];
            }
            out.copy_from_slice(&self.buf_long[..1024]);
            saved.copy_from_slice(&self.buf_long[1024..2048]);
            self.channels_state[ch].saved_len = 1024;
        }
    }

    #[allow(dead_code)]
    fn short_half_cur(&self, kb: bool) -> &[f32] {
        if kb {
            &self.win_short_kb
        } else {
            &self.win_short_sine
        }
    }

    #[allow(dead_code)]
    fn short_half_prev(&self, kb: bool) -> &[f32] {
        if kb {
            &self.win_short_kb
        } else {
            &self.win_short_sine
        }
    }

    /// M/S + intensity stereo for a common-window channel pair.
    fn apply_cpe_stereo(&mut self, ch_l: usize, ch_r: usize, win: &WindowInfo, ms_present: bool) {
        // Snapshot the per-band data to avoid overlapping borrows.
        let band_type: Vec<u8> = self.band_type.to_vec();
        let sf: Vec<f32> = self.sf.to_vec();
        let ms_mask: Vec<bool> = self.ms_mask.to_vec();
        let _ = ms_present;

        let (left, right) = {
            let (a, b) = self.channels_state.split_at_mut(ch_r);
            (&mut a[ch_l].coeffs, &mut b[0].coeffs)
        };
        stereo::apply_mid_side(
            left,
            right,
            &band_type,
            &band_type,
            &ms_mask,
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
                let mut c: i32 = match band_type[idx] {
                    INTENSITY_BT2 => -1,
                    INTENSITY_BT => 1,
                    _ => 0,
                };
                if c != 0 {
                    if ms_present && self.ms_mask_snapshot(idx) {
                        c *= 1 - 2;
                    }
                    gains[idx] = (c as f32) * sf[idx];
                }
            }
        }
        let (left, right) = {
            let (a, b) = self.channels_state.split_at_mut(ch_r);
            (&mut a[ch_l].coeffs, &mut b[0].coeffs)
        };
        stereo::apply_intensity(
            left,
            right,
            &band_type,
            &gains,
            win.num_window_groups,
            &win.group_len,
            win.max_sfb,
            win.swb_offsets(),
        );
    }
}

// Helper referenced above (kept trivial).
impl AacDecoder {
    fn ms_mask_snapshot(&self, idx: usize) -> bool {
        self.ms_mask.get(idx).copied().unwrap_or(false)
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
            state.saved_len = 0;
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
