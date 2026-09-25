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

thread_local! {
    static FIL_SPANS: std::cell::RefCell<Vec<(usize, usize)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}
use crate::sbr;
use crate::stereo;
use crate::tables;
use crate::tns::{self, Tns};

const MAX_CHANNELS: usize = 8;
/// Channel count per ISO/IEC 14496-3 channel configuration (Table 1.4).
/// Configuration 0 is PCE-configured (count declared in-band); note
/// configuration 7 ("7.1") carries EIGHT channels.
const CONFIG_CHANNEL_COUNTS: [usize; 8] = [0, 1, 2, 3, 4, 5, 6, 8];
/// ADTS frames cannot exceed 6144/8 · channels + header bytes; 16 KiB is a
/// generous cap allocated once at open time.
const FRAME_BUF_LEN: usize = 16 * 1024;
/// Coefficients per short window.
const BLOCK_LEN: usize = 128;

// Element ids (id_syn_ele). CCE and PCE are both fully decoded (see
// `decode_cce`/`decode_pce`), not rejected.

const SCE: u32 = 0;
const CPE: u32 = 1;
const CCE: u32 = 2;
const LFE: u32 = 3;
const DSE: u32 = 4;
const PCE: u32 = 5;
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

// PCE plan entry types.
const PCE_SCE: u8 = 0;
const PCE_CPE: u8 = 1;
const PCE_LFE: u8 = 2;

/// Per-channel decoding state (allocation confined to open()).
struct ChannelState {
    /// Spectral coefficients of the current transform (1024).
    coeffs: Box<[f32]>,
    /// Dequantized time-domain output of the current transform (1024;
    /// extended to 2048 when SBR is active).
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

/// One coupling-channel element: its decoded spectrum plus the coupling
/// targets and per-band gains (reference `ChannelCoupling`).
const MAX_CCE_TARGETS: usize = 8;
struct CouplingChannel {
    state: ChannelState,
    /// Window info of the CCE's own ICS (gain index layout).
    window: WindowInfo,
    /// 0 = before TNS, 1 = between TNS and IMDCT, 3 = after IMDCT
    /// (reference `CouplingPoint`).
    coupling_point: u8,
    /// Number of coupled targets minus one.
    num_coupled: usize,
    /// Per target: element type (SCE/CPE), tag, and channel selection.
    ty: [u8; MAX_CCE_TARGETS],
    id_select: [u8; MAX_CCE_TARGETS],
    ch_select: [u8; MAX_CCE_TARGETS],
    /// (sign flag, scale index) read before the ICS.
    sign_and_scale: (u8, usize),
    /// Number of gain sets (targets, +1 for split stereo selections).
    num_gain: usize,
    /// Per target per band gains: gain[target][band_index] (8 × 512).
    gain: Box<[f32]>,
    /// Whether this slot carries a CCE decoded in the current block.
    coupled: bool,
}

impl ChannelState {
    fn new() -> Self {
        ChannelState {
            coeffs: vec![0.0; 1024].into_boxed_slice(),
            out: vec![0.0; 2048].into_boxed_slice(),
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
    fn placeholder(sf_index: usize) -> Self {
        WindowInfo {
            sequence: ONLY_LONG,
            num_windows: 1,
            num_window_groups: 1,
            group_len: [1; 8],
            max_sfb: 0,
            num_swb: 0,
            sf_index,
        }
    }

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

/// One channel element decoded in the current block.
#[derive(Clone, Copy)]
struct BlockElem {
    ch: usize,
    win: WindowInfo,
    is_cpe: bool,
    tag: u8,
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
    /// The stream's fixed channel configuration value (0 = PCE-configured).
    channel_configuration: u8,
    sf_index: usize,
    /// Header of the first ADTS frame, consumed during open.
    pending_header: Option<[u8; 7]>,
    frame_count: u64,
    /// Raw-block framing (no ADTS headers) when opened from a config.
    raw_blocks: bool,
    /// Raw-block stream state: frame_buf[raw_pos..raw_len] holds the not
    /// yet parsed bytes of the current read window.
    raw_pos: usize,
    raw_len: usize,
    /// Channel plan from the most recent Program Config Element (channel
    /// configuration 0), in declaration order: front, side, back, then LFE.
    /// Elements are mapped to channels SOLELY BY TAG (reference:
    /// `ff_aac_get_che`'s `tag_che_map`); `pce_plan_chan` holds each
    /// entry's output channel index.
    pce_plan_type: [u8; MAX_CHANNELS],
    pce_plan_tag: [u8; MAX_CHANNELS],
    pce_plan_chan: [u8; MAX_CHANNELS],
    pce_plan_len: usize,
    /// Element counts per position class: front, side, back, LFE.
    pce_class_counts: [usize; 4],
    /// Output permutation for PCE streams (sniffed WAV order).
    pce_out_order: [u8; MAX_CHANNELS],

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
    /// Coupling channel elements decoded in the current block.
    cces: Vec<CouplingChannel>,
    /// SBR enhancement state (present once an SBR extension is seen).
    ///
    /// Boxed: `Sbr` embeds its own scratch (two `Mdct64` cosine tables plus
    /// two `SbrChannel`s of `[[f32; 48]; N]` envelope/gain tables) and is
    /// well over 100 KB by value. Left inline in `Option<sbr::Sbr>`, that
    /// size becomes part of `AacDecoder` itself, so *every* stack frame
    /// that holds an `AacDecoder` by value (constructors, and — because of
    /// aggressive inlining — even `&mut self` methods whose callee tree
    /// gets folded into one frame) pays for it, whether or not the stream
    /// actually uses SBR. That was the dominant contributor to the
    /// AAC/SBR stack-overflow issue (see todo.md), well above the
    /// SBR-apply() locals it was originally attributed to.
    /// Per-channel-element SBR state, indexed by the element's first
    /// channel. Each SCE/CPE that ever carries an SBR extension gets its
    /// own persistent context: the reference decoder keeps SBR state per
    /// `ChannelElement` (`che[type][tag]`), not in one shared instance,
    /// because each element's envelope/noise/QMF history is independent.
    /// A single shared `Sbr` here used to mean only the last channel
    /// element processed in a frame kept valid SBR state; every other
    /// SBR-carrying element's high band silently used stale data from
    /// whichever element happened to run last.
    sbr_by_channel: [Option<Box<sbr::Sbr>>; MAX_CHANNELS],
    /// Once true, all subsequent frames output 2048 samples/channel and
    /// every decoded element applies SBR (reference: `sbr_apply` runs for
    /// every channel element whenever `m4ac.sbr > 0`, independent of
    /// whether that specific element's FIL carried fresh data this frame
    /// — an element with no fresh data this frame still upsamples via QMF
    /// passthrough using its own carried-over state).
    sbr_output_active: bool,
    /// True once the stream's sample rate has been doubled for SBR.
    /// Doubling must happen exactly once regardless of how many channel
    /// elements carry SBR.
    sbr_rate_doubled: bool,
    /// True once Parametric Stereo synthesis is active for this stream:
    /// either the container signaled HE-AACv2 explicitly (ASC AOT 29) or
    /// the first in-band PS payload was decoded for a mono stream (the
    /// reference decoder's `m4ac.ps` flip in `decode_extension_payload`).
    /// A mono core channel then synthesizes to a stereo output pair.
    ps_signaled: bool,
    /// True when the container explicitly configured PS (ASC AOT 5
    /// disables it, AOT 29 enables it); only streams with unknown
    /// signaling (ADTS, raw ASC AOT 2) may flip to PS output on the first
    /// in-band SBR payload (reference `m4ac.ps == -1`).
    ps_known: bool,

    /// Cached developer diagnostics, initialized during construction and never
    /// queried through the environment on the real-time decode path.
    fate_trace: bool,
    dump_blocks: bool,
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
            None,
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
        let mut decoder = Self::open_with_params(
            BufferedSource::new(source, 8192),
            sample_rate,
            config.channel_configuration,
            config.sampling_frequency_index as usize,
            true,
            config.program_config.as_ref(),
        )?;
        if config.program_config.is_some() {
            decoder.recompute_pce_out_order();
        }
        if config.extension_sampling_frequency_index.is_some() {
            decoder.sbr_output_active = true;
            decoder.sbr_rate_doubled = true;
        }
        decoder.ps_known = true;
        decoder.ps_signaled = config.ps_signaled;
        if config.ps_signaled {
            // HE-AACv2: the mono core is synthesized to a stereo output.
            // The channel configuration is typically 1 (a single SCE); a
            // CPE payload would already be stereo before PS applies, so
            // the output count only grows for the mono case.
            decoder.ps_signaled = true;
            if decoder.channels == 1 {
                decoder.channels = 2;
                decoder.info.channels = 2;
            }
        }
        Ok(decoder)
    }

    fn open_with_params(
        source: BufferedSource,
        sample_rate: u32,
        channel_configuration: u8,
        sf_index: usize,
        raw_blocks: bool,
        asc_pce: Option<&crate::audio_specific::AacPcePlan>,
    ) -> Result<Self, CadenceError> {
        // Channel count and initial PCE plan: an ASC-carried program
        // config element (channel configuration 0) fixes both at open.
        let (channels, pce_plan, pce_class_counts) = match asc_pce {
            Some(plan) => {
                let counts = plan.class_counts;
                let total: usize = plan
                    .entries
                    .iter()
                    .map(|&(ty, _)| 1 + usize::from(ty == PCE_CPE))
                    .sum();
                if plan.entries.len() > MAX_CHANNELS || total == 0 {
                    return Err(CadenceError::CorruptData(
                        "ASC program configuration is empty or exceeds the channel limit"
                            .to_string(),
                    ));
                }
                (total, Some(plan.entries.as_slice()), counts)
            }
            None => (
                match usize::from(channel_configuration) {
                    c if c < CONFIG_CHANNEL_COUNTS.len() => CONFIG_CHANNEL_COUNTS[c],
                    _ => {
                        return Err(CadenceError::UnsupportedFeature(
                            "reserved AAC channel configuration".to_string(),
                        ))
                    }
                },
                None,
                [0; 4],
            ),
        };
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

        // Channel state exists for every possible channel: with channel
        // configuration 0 the count is only known once the first in-band
        // PCE arrives, so `channels` starts at 0 and is upgraded then.
        let mut channels_state = Vec::with_capacity(MAX_CHANNELS);
        for _ in 0..MAX_CHANNELS {
            channels_state.push(ChannelState::new());
        }

        let mut info = StreamInfo::new(Format::Aac, sample_rate, channels as u16, 16);
        info.total_frames = None;
        if channels > 0 {
            info.validate()?;
        }

        Ok(AacDecoder {
            source,
            info,
            channels,
            channel_configuration,
            sf_index,
            pending_header: None,
            frame_count: 0,
            raw_blocks,
            raw_pos: 0,
            raw_len: 0,
            pce_plan_type: {
                let mut t = [0u8; MAX_CHANNELS];
                if let Some(plan) = pce_plan {
                    for (i, &(ty, _)) in plan.iter().enumerate() {
                        t[i] = ty;
                    }
                }
                t
            },
            pce_plan_tag: {
                let mut t = [0u8; MAX_CHANNELS];
                if let Some(plan) = pce_plan {
                    for (i, &(_, tag)) in plan.iter().enumerate() {
                        t[i] = tag;
                    }
                }
                t
            },
            pce_plan_chan: {
                let mut t = [0u8; MAX_CHANNELS];
                if let Some(plan) = pce_plan {
                    let mut ch = 0usize;
                    for (i, &(ty, _)) in plan.iter().enumerate() {
                        t[i] = ch as u8;
                        ch += 1 + usize::from(ty == PCE_CPE);
                    }
                }
                t
            },
            pce_plan_len: pce_plan.map_or(0, |p| p.len()),
            pce_class_counts,
            pce_out_order: [0; MAX_CHANNELS],
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
            sbr_by_channel: std::array::from_fn(|_| None),
            sbr_output_active: false,
            sbr_rate_doubled: false,
            ps_signaled: false,
            ps_known: false,
            fate_trace: std::env::var_os("FATE_TRACE").is_some(),
            dump_blocks: std::env::var_os("AAC_DUMP_BLOCKS").is_some(),
            cces: (0..4)
                .map(|_| CouplingChannel {
                    state: ChannelState::new(),
                    window: WindowInfo::placeholder(sf_index),
                    coupling_point: 0,
                    num_coupled: 0,
                    ty: [0; MAX_CCE_TARGETS],
                    id_select: [0; MAX_CCE_TARGETS],
                    ch_select: [0; MAX_CCE_TARGETS],
                    sign_and_scale: (0, 0),
                    num_gain: 0,
                    gain: vec![0.0; MAX_CCE_TARGETS * 512].into_boxed_slice(),
                    coupled: false,
                })
                .collect(),
            ms_mask: vec![false; 512].into_boxed_slice(),
            frame_buf: vec![0u8; FRAME_BUF_LEN].into_boxed_slice(),
            work_buf: vec![0u8; FRAME_BUF_LEN].into_boxed_slice(),
            // One transform's worth of interleaved PCM for any channel
            // count: `decode()` clears and refills in place, so no
            // allocation happens after open (real-time contract). Sized
            // for MAX_CHANNELS because configuration 0 starts at 0.
            staged: Vec::with_capacity(1024 * MAX_CHANNELS),
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
        if self.raw_blocks {
            return self.decode_next_raw_block();
        }
        if self.eof {
            return Ok(false);
        }

        let (num_blocks, body_len) = match self.read_adts_frame() {
            Ok((h, l)) => (u32::from(h.raw_blocks_minus_one) + 1, l),
            Err(CadenceError::EndOfStream) => return Ok(false),
            Err(e) => return Err(e),
        };
        self.work_buf[..body_len].copy_from_slice(&self.frame_buf[..body_len]);
        // Lend the work buffer out so the block decoder can use &mut self.
        let work = std::mem::take(&mut self.work_buf);
        let mut br = BitReader::new(&work[..body_len]);
        let result = self.decode_raw_block(&mut br, num_blocks);
        self.work_buf = work;
        result?;
        self.frame_count += 1;
        Ok(true)
    }

    /// Parses one raw_data_block from the raw-block stream. Raw blocks carry
    /// no length prefix: blocks are parsed from the buffered bytes, and a
    /// parse that runs out of data at the buffer end is retried after
    /// refilling from the source (a block may straddle a refill boundary).
    /// Ok(false) once the stream is exhausted.
    fn decode_next_raw_block(&mut self) -> Result<bool, CadenceError> {
        loop {
            if self.raw_pos >= self.raw_len && self.refill_raw()? == 0 {
                return Ok(false);
            }
            let avail = self.raw_len - self.raw_pos;
            self.work_buf[..avail].copy_from_slice(&self.frame_buf[self.raw_pos..self.raw_len]);
            // A parse that runs out of data may leave non-idempotent state
            // behind (the PNS LCG advances one-way and the per-channel
            // window-shape flags shift); snapshot those so the retry after
            // a refill decodes from the same state as a first attempt.
            let noise_state = self.noise.state();
            let kb_flags: [(bool, bool); MAX_CHANNELS] = std::array::from_fn(|i| {
                self.channels_state
                    .get(i)
                    .map_or((false, false), |c| (c.kb_window_prev, c.kb_window_cur))
            });
            // Lend the work buffer out so the block decoder can use &mut self.
            let work = std::mem::take(&mut self.work_buf);
            let (result, consumed, overread) = {
                let mut br = BitReader::new(&work[..avail]);
                let r = self.decode_raw_block(&mut br, 1);
                (r, br.pos(), br.overread())
            };
            self.work_buf = work;
            match result {
                Ok(()) => {
                    // Debug-only block dumper: gated behind an explicit
                    // developer-set env var, never triggered by bitstream
                    // content, so the unwraps below are outside the
                    // real-time/no-panic contract for untrusted input.
                    if self.dump_blocks {
                        use std::io::Write;
                        let nbits = consumed;
                        let nbytes = nbits.div_ceil(8);
                        let mut f = std::fs::OpenOptions::new()
                            .create(true)
                            .append(true)
                            .open("blocks.bin")
                            .unwrap();
                        f.write_all(&self.work_buf[..nbytes]).unwrap();
                        let spans = FIL_SPANS.with(|s| std::mem::take(&mut *s.borrow_mut()));
                        let mut f2 = std::fs::OpenOptions::new()
                            .create(true)
                            .append(true)
                            .open("spans.txt")
                            .unwrap();
                        let _ = writeln!(f2, "{} {:?}", nbits, spans);
                    }
                    // A raw_data_block ends byte-aligned, so the next one
                    // starts at the next byte boundary.
                    self.raw_pos += consumed.div_ceil(8);
                    self.frame_count += 1;
                    return Ok(true);
                }
                Err(e) => {
                    if overread && self.refill_raw()? > 0 {
                        self.noise.set_state(noise_state);
                        for (i, (prev, cur)) in kb_flags.iter().enumerate() {
                            if let Some(c) = self.channels_state.get_mut(i) {
                                c.kb_window_prev = *prev;
                                c.kb_window_cur = *cur;
                            }
                        }
                        continue;
                    }
                    return Err(e);
                }
            }
        }
    }

    /// Slides the unconsumed raw-block bytes to the front of the frame
    /// buffer and reads more from the source. Returns how many NEW bytes
    /// were added (0 at EOF, or when the buffer was already full).
    fn refill_raw(&mut self) -> Result<usize, CadenceError> {
        self.frame_buf.copy_within(self.raw_pos..self.raw_len, 0);
        self.raw_len -= self.raw_pos;
        let before = self.raw_len;
        self.raw_pos = 0;
        while self.raw_len < self.frame_buf.len() {
            match self.source.read(&mut self.frame_buf[self.raw_len..]) {
                Ok(0) => {
                    self.eof = true;
                    break;
                }
                Ok(n) => self.raw_len += n,
                Err(e) => return Err(CadenceError::from(e)),
            }
        }
        Ok(self.raw_len - before)
    }

    /// Reads one ADTS frame, returning (header, body length). Handles the
    /// header consumed during open via `pending_header`.
    fn read_adts_frame(&mut self) -> Result<(AdtsHeader, usize), CadenceError> {
        let mut header = [0u8; 7];
        if let Some(pending) = self.pending_header.take() {
            header = pending;
        } else {
            loop {
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
        // Channel configuration 0 means the stream is configured by an
        // in-band PCE; the fixed-configuration consistency check applies
        // only when both sides name a concrete configuration.
        if parsed.channel_configuration != 0
            && self.channel_configuration != 0
            && parsed.channel_configuration != self.channel_configuration
        {
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

    /// Parses `num_blocks` raw_data_blocks and stages interleaved PCM.
    fn decode_raw_block(
        &mut self,
        br: &mut BitReader,
        num_blocks: u32,
    ) -> Result<(), CadenceError> {
        // Channel elements decoded in this frame, in output order: target
        // records for coupling (element type, tag) plus the channel and
        // window info.
        let mut block_channels = [BlockElem {
            ch: 0,
            win: WindowInfo::placeholder(self.sf_index),
            is_cpe: false,
            tag: 0,
        }; MAX_CHANNELS];
        let mut block_count = 0usize;
        // The last channel element seen (first channel, count, element id):
        // an SBR extension payload in a FIL applies to this element.
        let mut che_prev: Option<(usize, usize, u32)> = None;

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
                if self.fate_trace {
                    eprintln!(
                        "frame {} element id={id} bitpos={}",
                        self.frame_count,
                        br.pos()
                    );
                }
                match id {
                    SCE | LFE => {
                        let tag = br.read_bits(4);
                        let ch = if self.pce_plan_len > 0 {
                            self.pce_channel_for(false, id == LFE, tag)?
                        } else {
                            Self::next_free_channel(&block_channels[..block_count], self.channels)?
                        };
                        let win = self.decode_ics(br, ch, false, None)?;
                        block_channels[block_count] = BlockElem {
                            ch,
                            win,
                            is_cpe: false,
                            tag: tag as u8,
                        };
                        block_count += 1;
                        che_prev = Some((ch, 1, id));
                    }
                    CPE => {
                        let tag = br.read_bits(4);
                        let common_window = br.read_bit();
                        let ch_l = if self.pce_plan_len > 0 {
                            self.pce_channel_for(true, false, tag)?
                        } else {
                            Self::next_free_channel(&block_channels[..block_count], self.channels)?
                        };
                        let ch_r = ch_l + 1;
                        if ch_r >= self.channels {
                            return Err(CadenceError::CorruptData(
                                "channel pair exceeds the channel configuration".to_string(),
                            ));
                        }

                        let (win, win_r, ms_present) = if common_window {
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
                            (win, win, ms_present)
                        } else {
                            // Each channel carries its own ics_info and is
                            // parsed exactly once (no ms data).
                            let win_l = self.decode_ics(br, ch_l, false, None)?;
                            let win_r = self.decode_ics(br, ch_r, false, None)?;
                            (win_l, win_r, 0u32)
                        };

                        if common_window {
                            self.decode_ics(br, ch_l, true, Some(win))?;
                            self.decode_ics(br, ch_r, true, Some(win))?;
                            self.apply_cpe_stereo(ch_l, ch_r, &win, ms_present != 0);
                        }

                        block_channels[block_count] = BlockElem {
                            ch: ch_l,
                            win,
                            is_cpe: true,
                            tag: tag as u8,
                        };
                        block_count += 1;
                        block_channels[block_count] = BlockElem {
                            ch: ch_r,
                            win: win_r,
                            is_cpe: true,
                            tag: tag as u8,
                        };
                        block_count += 1;
                        che_prev = Some((ch_l, 2, id));
                    }
                    CCE => self.decode_cce(br)?,
                    FIL => {
                        let fil_start_bit = if self.dump_blocks { br.pos() } else { 0 };
                        let mut count = br.read_bits(4) as usize;
                        if count == 15 {
                            count = 14 + br.read_bits(8) as usize;
                        }
                        if count == 0 {
                            // Empty fill element: nothing to skip.
                            continue;
                        }
                        // extension_payload: extension_type(4) then payload.
                        let ext_type = br.read_bits(4);
                        match ext_type {
                            13 | 14 => {
                                // SBR extension (13 = plain, 14 = with CRC).
                                let crc = ext_type == 14;
                                let payload_bits = count * 8 - 4;
                                let payload_start = br.pos();
                                let nbytes = payload_bits.div_ceil(8);
                                let mut payload = [0u8; 272];
                                for byte in payload[..nbytes.min(272)].iter_mut() {
                                    *byte = br.read_bits(8) as u8;
                                }
                                // The FIL payload is bit-packed: restore the
                                // exact position after the rounded-up capture.
                                br.set_pos(payload_start + payload_bits);
                                let (id_type, nch) = match che_prev {
                                    Some((ch0, n, ty)) => (ty, (ch0, n)),
                                    None => (id, (0usize, 1usize)),
                                };
                                if !self.sbr_rate_doubled {
                                    // First SBR discovery anywhere in the
                                    // stream: the output rate doubles
                                    // (implicit SBR signaling), exactly
                                    // once regardless of how many channel
                                    // elements carry SBR.
                                    self.sbr_rate_doubled = true;
                                    self.info.sample_rate *= 2;
                                }
                                let sbr = self.sbr_by_channel[nch.0].get_or_insert_with(|| {
                                    Box::new(sbr::Sbr::new(id_type as usize))
                                });
                                if sbr.sample_rate == 0 {
                                    sbr.sample_rate = self.info.sample_rate as i32;
                                }
                                sbr::parse::decode_sbr_extension(
                                    sbr,
                                    &payload,
                                    crc,
                                    count,
                                    id_type as usize,
                                );
                                sbr.ps_output = self.ps_signaled;
                                if !self.ps_known
                                    && !self.ps_signaled
                                    && self.channels == 1
                                    && id_type == 0
                                {
                                    // First in-band SBR payload in a mono
                                    // stream whose container did not
                                    // explicitly configure PS: the
                                    // reference decoder treats the stream
                                    // as HE-AACv2 and reconfigures its
                                    // output as stereo ("treating HE-AAC
                                    // mono as stereo"). Until a PS header
                                    // actually arrives, synthesis
                                    // duplicates the mono channel.
                                    self.ps_signaled = true;
                                    sbr.ps_output = true;
                                    self.channels = 2;
                                    self.info.channels = 2;
                                    // The flip frame's own output is staged
                                    // as stereo right after this; drop any
                                    // pre-flip mono samples so the caller's
                                    // channel interpretation stays coherent.
                                    self.staged.clear();
                                    self.staged_pos = 0;
                                }
                                self.sbr_output_active = true;
                                if self.dump_blocks {
                                    FIL_SPANS.with(|s| {
                                        s.borrow_mut()
                                            .push((fil_start_bit, payload_start + payload_bits))
                                    });
                                }
                            }
                            _ => {
                                br.skip_bits(count * 8 - 4);
                            }
                        }
                    }
                    DSE => {
                        let _tag = br.read_bits(4);
                        let align = br.read_bit();
                        // Count is EIGHT bits with a 255 escape byte
                        // (ISO/IEC 14496-3 Table 4.8).
                        let mut count = br.read_bits(8) as usize;
                        if count == 255 {
                            count += br.read_bits(8) as usize;
                        }
                        if align {
                            br.byte_align();
                        }
                        br.skip_bytes(count);
                    }
                    PCE => self.decode_pce(br)?,
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

        // Spectral-domain tool ordering per the reference decoder: each
        // channel element receives BEFORE_TNS dependent coupling, its own
        // TNS, BETWEEN_TNS_AND_IMDCT dependent coupling, then the IMDCT;
        // AFTER_IMDCT independent coupling acts on the time-domain output.
        let mut decoded = [BlockElem {
            ch: 0,
            win: WindowInfo::placeholder(self.sf_index),
            is_cpe: false,
            tag: 0,
        }; MAX_CHANNELS];
        let mut decoded_count = 0usize;
        for el in &block_channels[..block_count] {
            if decoded[..decoded_count].iter().any(|d| d.ch == el.ch) {
                continue;
            }
            decoded[decoded_count] = *el;
            decoded_count += 1;
        }

        // The coupling channels' own TNS runs before any coupling is
        // applied (reference: the CCE elements are processed first in the
        // type loop, where their own TNS is applied).
        for cce in &mut self.cces {
            if cce.coupled {
                let win = cce.window;
                let state = &mut cce.state;
                tns::apply(
                    &state.tns,
                    &mut state.coeffs,
                    win.num_windows,
                    win.num_swb,
                    win.swb_offsets(),
                    win.tns_max_bands(),
                    win.max_sfb,
                );
            }
        }

        // Coupling channels with an after-IMDCT point are transformed
        // first: their time-domain output feeds the targets' independent
        // coupling below (reference processes CCE elements before CPE/SCE).
        for i in 0..self.cces.len() {
            let (point, win) = {
                let cce = &self.cces[i];
                (cce.coupling_point, cce.window)
            };
            if self.cces[i].coupled && point == 3 {
                std::mem::swap(&mut self.channels_state[0], &mut self.cces[i].state);
                self.imdct_and_window(0, &win);
                std::mem::swap(&mut self.channels_state[0], &mut self.cces[i].state);
            }
        }
        for el in &decoded[..decoded_count] {
            let win = el.win;
            self.apply_coupling(el, 0);
            {
                let state = &mut self.channels_state[el.ch];
                tns::apply(
                    &state.tns,
                    &mut state.coeffs,
                    win.num_windows,
                    win.num_swb,
                    win.swb_offsets(),
                    win.tns_max_bands(),
                    win.max_sfb,
                );
            }
            self.apply_coupling(el, 1);
            self.imdct_and_window(el.ch, &win);
            self.apply_coupling(el, 3);
        }

        // SBR enhancement: replaces the 1024-sample core output with 2048
        // samples per channel, for every SCE/CPE element decoded this
        // frame (reference: `sbr_apply` runs for every channel element
        // once SBR is signaled anywhere in the stream, using that
        // element's own carried-over state — even on a frame where its
        // FIL carried no fresh data, it still needs the QMF-passthrough
        // upsample to stay in sync with elements that did).
        if self.sbr_output_active {
            let mut idx = 0;
            while idx < block_count {
                let el = block_channels[idx];
                let count = if el.is_cpe { 2 } else { 1 };
                let first = el.ch;
                if decoded[..decoded_count].iter().any(|d| d.ch == first) {
                    let nch = count.min(2);
                    let nch_out = usize::from(self.ps_signaled && nch == 1) + nch;
                    let mut core = [
                        self.channels_state[first].out.to_vec(),
                        if nch == 2 {
                            self.channels_state[first + 1].out.to_vec()
                        } else if nch_out == 2 {
                            // PS target channel: carries no core input,
                            // only receives the synthesized output.
                            vec![0.0f32; 2048]
                        } else {
                            Vec::new()
                        },
                    ];
                    let id = if nch == 2 { 1 } else { 0 };
                    let sbr = self.sbr_by_channel[first]
                        .get_or_insert_with(|| Box::new(sbr::Sbr::new(id)));
                    sbr.ps_output = self.ps_signaled;
                    sbr.apply(id, &mut core, nch);
                    for (c, dst_ch) in (first..first + nch_out).enumerate() {
                        let dst = &mut self.channels_state[dst_ch].out;
                        dst[..2048].copy_from_slice(&core[c][..2048]);
                    }
                }
                idx += count;
            }
        }

        if self.fate_trace && self.frame_count < 6 {
            for (i, el) in decoded[..decoded_count].iter().enumerate() {
                eprintln!(
                    "frame {} decoded[{i}]: ch={} is_cpe={} tag={}",
                    self.frame_count, el.ch, el.is_cpe, el.tag
                );
            }
            for (ci, cce) in self.cces.iter().enumerate().take(2) {
                if cce.coupled {
                    eprintln!(
                        "frame {} cce{ci}: point={} num_coupled={} targets={:?} gains0..6={:?} coeff_rms={:.2} out_rms={:.4} corr_ch0={:.4}",
                        self.frame_count,
                        cce.coupling_point,
                        cce.num_coupled,
                        &cce.ty[..=cce.num_coupled],
                        &cce.gain[..6],
                        {
                            let s: f64 = cce.state.coeffs.iter().take(1024).map(|&x| (x * x) as f64).sum();
                            (s / 1024.0).sqrt()
                        },
                        {
                            let s: f64 = cce.state.out.iter().take(1024).map(|&x| (x * x) as f64).sum();
                            (s / 1024.0).sqrt()
                        },
                        {
                            let n: f64 = cce.state.out.iter().zip(self.channels_state[0].out.iter()).take(1024)
                                .map(|(&a, &b)| (a as f64) * (b as f64)).sum();
                            let na: f64 = cce.state.out.iter().take(1024).map(|&x| (x * x) as f64).sum();
                            let nb: f64 = self.channels_state[0].out.iter().take(1024).map(|&x| (x * x) as f64).sum();
                            if na > 0.0 && nb > 0.0 { n / (na * nb).sqrt() } else { 0.0 }
                        }
                    );
                }
            }
        }
        if self.fate_trace && self.frame_count < 6 {
            for ch in 0..self.channels {
                let rms: f64 = self.channels_state[ch]
                    .out
                    .iter()
                    .take(1024)
                    .map(|&x| (x * x) as f64)
                    .sum::<f64>()
                    .sqrt();
                eprintln!(
                    "frame {} ch {ch}: out_rms={rms:.4} coeffs_rms={:.4}",
                    self.frame_count,
                    self.channels_state[ch]
                        .coeffs
                        .iter()
                        .take(1024)
                        .map(|&x| (x * x) as f64)
                        .sum::<f64>()
                        .sqrt()
                );
            }
        }
        // Interleave into staging. For the fixed channel configurations the
        // output follows the WAV channel order: the bitstream carries
        // front-center first, while the convention is front pairs first
        // (mappings verified against FFmpeg's decoder; see the round-trip
        // conformance tests). PCE-configured and incomplete blocks keep
        // bitstream element order. SBR-enhanced elements stage 2048
        // samples per channel.
        enum Order<'a> {
            Borrowed(&'static [usize]),
            Pce(&'a [u8]),
            Element,
        }
        let frame_len = if self.sbr_output_active { 2048 } else { 1024 };
        let order: Order = if self.pce_plan_len > 0 && block_count == self.channels {
            Order::Pce(&self.pce_out_order[..self.channels])
        } else if self.pce_plan_len == 0 && block_count == self.channels {
            match self.channels {
                1 => Order::Borrowed(&[0]),
                2 => Order::Borrowed(&[0, 1]),
                3 => Order::Borrowed(&[1, 2, 0]),
                4 => Order::Borrowed(&[1, 2, 0, 3]),
                5 => Order::Borrowed(&[1, 2, 0, 3, 4]),
                6 => Order::Borrowed(&[1, 2, 0, 5, 3, 4]),
                8 => Order::Borrowed(&[1, 2, 0, 7, 5, 6, 3, 4]),
                _ => Order::Element,
            }
        } else if self.ps_signaled && block_count == 1 && decoded_count == 1 && decoded[0].ch == 0 {
            // HE-AACv2: the single mono SCE synthesizes to a stereo pair.
            Order::Borrowed(&[0, 1])
        } else {
            Order::Element
        };
        match order {
            Order::Borrowed(order) => {
                for i in 0..frame_len {
                    for &ch in order.iter().take(self.channels) {
                        self.staged.push(self.channels_state[ch].out[i]);
                    }
                }
            }
            Order::Pce(order) => {
                for i in 0..frame_len {
                    for &ch in order.iter().take(self.channels) {
                        self.staged.push(self.channels_state[ch as usize].out[i]);
                    }
                }
            }
            Order::Element => {
                for i in 0..frame_len {
                    for el in &decoded[..decoded_count] {
                        self.staged.push(self.channels_state[el.ch].out[i]);
                    }
                }
            }
        }
        for cce in &mut self.cces {
            cce.coupled = false;
        }
        Ok(())
    }

    #[allow(clippy::needless_range_loop)]
    fn next_free_channel(used: &[BlockElem], channels: usize) -> Result<usize, CadenceError> {
        let mut used_flags = [false; MAX_CHANNELS];
        for el in used {
            used_flags[el.ch] = true;
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

    /// cc_element (ISO/IEC 14496-3 4.6.8.2.2): decodes the coupling
    /// channel's target list, gains, and its own individual channel stream.
    /// The spectrum is consumed by coupling application, not windowed
    /// (except for after-IMDCT coupling, which consumes time samples).
    fn decode_cce(&mut self, br: &mut BitReader) -> Result<(), CadenceError> {
        let _instance_tag = br.read_bits(4);
        let slot = (0..self.cces.len())
            .find(|&i| !self.cces[i].coupled)
            .ok_or_else(|| {
                CadenceError::UnsupportedFeature(
                    "more coupling channels in one block than supported".to_string(),
                )
            })?;
        {
            let cce = &mut self.cces[slot];
            cce.coupled = true;
            cce.coupling_point = 2 * u8::from(br.read_bit());
            cce.num_coupled = br.read_bits(3) as usize;
            let mut num_gain = 0usize;
            for c in 0..=cce.num_coupled {
                num_gain += 1;
                cce.ty[c] = if br.read_bit() { CPE as u8 } else { SCE as u8 };
                cce.id_select[c] = br.read_bits(4) as u8;
                if cce.ty[c] == CPE as u8 {
                    cce.ch_select[c] = br.read_bits(2) as u8;
                    if cce.ch_select[c] == 3 {
                        num_gain += 1;
                    }
                } else {
                    cce.ch_select[c] = 2;
                }
            }
            cce.coupling_point += u8::from(br.read_bit() || cce.coupling_point >> 1 != 0);
            cce.sign_and_scale = (u8::from(br.read_bit()), br.read_bits(2) as usize);
            cce.num_gain = num_gain;
        }

        // The CCE's own individual channel stream, decoded through a spare
        // channel slot (state swapped in and back out).
        std::mem::swap(&mut self.channels_state[0], &mut self.cces[slot].state);
        let win = self.decode_ics(br, 0, false, None);
        std::mem::swap(&mut self.channels_state[0], &mut self.cces[slot].state);
        let win = win?;
        self.cces[slot].window = win;

        // Coupling gains. Target 0 carries a unit gain; later targets are
        // coded as scalefactor deltas against the running gain.
        const CCE_SCALE: [f32; 4] = [
            1.0905077,                // 2^(1/8)
            1.1892071,                // 2^(1/4)
            std::f32::consts::SQRT_2, // 2^(2/4)
            2.0,                      // 2^(3/4)
        ];
        let (sign, scale_idx) = self.cces[slot].sign_and_scale;
        let scale = CCE_SCALE[scale_idx];
        let num_gain = self.cces[slot].num_gain;
        let (point, max_sfb, num_groups) = {
            let cce = &self.cces[slot];
            (
                cce.coupling_point,
                cce.window.max_sfb,
                cce.window.num_window_groups,
            )
        };
        let mut gain = 0i32;
        for c in 0..num_gain {
            let mut cge = 1i32;
            let mut gain_cache = 1.0f32;
            if c > 0 {
                cge = if point == 3 {
                    1
                } else {
                    i32::from(br.read_bit())
                };
                gain = if cge == 1 {
                    self.sf_book.decode_scalefactor_delta(br)?
                } else {
                    0
                };
                gain_cache = scale.powf(-(gain as f32));
            }
            // Malformed streams can declare more gain sets than target
            // slots; the gain bits are still consumed for alignment, only
            // the storage is skipped.
            if c * 512 >= self.cces[slot].gain.len() {
                continue;
            }
            if point == 3 {
                self.cces[slot].gain[c * 512] = gain_cache;
            } else {
                let mut idx = 0usize;
                // Stack copy (band_type is at most 512 entries): the gain
                // writes below mutate `self.cces[slot]` through the loop.
                let mut band_types = [0u8; 512];
                band_types[..max_sfb * num_groups]
                    .copy_from_slice(&self.cces[slot].state.band_type[..max_sfb * num_groups]);
                for _ in 0..num_groups {
                    for _sfb in 0..max_sfb {
                        let band = band_types[idx];
                        if band != ZERO_BT {
                            if cge == 0 {
                                let delta = self.sf_book.decode_scalefactor_delta(br)?;
                                if delta != 0 {
                                    let mut s = 1.0f32;
                                    gain += delta;
                                    let mut t = gain;
                                    if sign != 0 {
                                        s -= 2.0 * f32::from((t & 1) != 0);
                                        t >>= 1;
                                    }
                                    gain_cache = scale.powf(-(t as f32)) * s;
                                }
                            }
                            self.cces[slot].gain[c * 512 + idx.min(511)] = gain_cache;
                        }
                        idx += 1;
                    }
                }
            }
        }
        Ok(())
    }

    /// Applies every matching coupling channel to the target channel
    /// element at `point` (0/1: dependent, spectral; 3: independent,
    /// time-domain), mirroring the reference dispatcher's gain indexing.
    fn apply_coupling(&mut self, target: &BlockElem, point: u8) {
        let target_ty = if target.is_cpe { CPE as u8 } else { SCE as u8 };
        for ci in 0..self.cces.len() {
            let (coupled, cce_point) = (self.cces[ci].coupled, self.cces[ci].coupling_point);
            if !coupled || cce_point != point {
                continue;
            }
            let mut index = 0usize;
            for c in 0..=self.cces[ci].num_coupled {
                let (ty, id, sel) = (
                    self.cces[ci].ty[c],
                    self.cces[ci].id_select[c],
                    self.cces[ci].ch_select[c],
                );
                if ty == target_ty && id == target.tag {
                    if sel != 1 {
                        self.apply_coupling_method(target.ch, ci, index, point);
                        if sel != 0 {
                            index += 1;
                        }
                    }
                    if sel != 2 && target.is_cpe {
                        self.apply_coupling_method(target.ch + 1, ci, index, point);
                        index += 1;
                    }
                } else {
                    index += 1 + usize::from(sel == 3);
                }
            }
        }
    }

    /// One coupling application to one target channel (reference
    /// `apply_dependent_coupling` / `apply_independent_coupling`).
    fn apply_coupling_method(&mut self, target_ch: usize, cce_idx: usize, index: usize, point: u8) {
        // Malformed streams can declare more targets (via ch_select == 3
        // "both channels" selections) than the fixed per-CCE gain storage
        // holds; matching decode_cce's own overflow handling, extra
        // applications are silently skipped rather than indexed out of
        // bounds.
        let gain_offset = match index.checked_mul(512) {
            Some(off) if off < self.cces[cce_idx].gain.len() => off,
            _ => return,
        };
        if point == 3 {
            // Independent: add the CCE's time output scaled by gain[0].
            let gain = self.cces[cce_idx].gain[gain_offset];
            let src: &[f32] = &self.cces[cce_idx].state.out;
            let dst = &mut self.channels_state[target_ch].out;
            for k in 0..1024 {
                dst[k] += gain * src[k];
            }
            return;
        }
        // Dependent: add gain[band] * cce_coeff[band] over the CCE's
        // non-zero bands, walking the CCE's window grouping.
        let (max_sfb, num_groups) = {
            let cce = &self.cces[cce_idx];
            (cce.window.max_sfb, cce.window.num_window_groups)
        };
        let band_types: &[u8] = &self.cces[cce_idx].state.band_type[..max_sfb * num_groups];
        let group_len: &[usize] = &self.cces[cce_idx].window.group_len;
        let offsets: &[u16] = self.cces[cce_idx].window.swb_offsets();
        let src: &[f32] = &self.cces[cce_idx].state.coeffs;
        let gain_end = gain_offset + 512;
        if gain_end > self.cces[cce_idx].gain.len() {
            return;
        }
        let gains: &[f32] = &self.cces[cce_idx].gain[gain_offset..gain_end];
        let dst = &mut self.channels_state[target_ch].coeffs;
        // The CCE's window layout governs (reference `apply_dependent_
        // coupling` iterates the CCE's groups/sfbs), and the window base
        // ACCUMULATES across window groups: each group's windows live at
        // `sum(group_len[..g]) * 128` in the 1024-sample coefficient
        // array. Forgetting that advance used to pile every group's
        // coupling onto the first window group on EIGHT_SHORT frames.
        let mut idx = 0usize;
        let mut window_base = 0usize;
        for group_len_g in group_len.iter().take(num_groups) {
            for i in 0..max_sfb {
                if band_types[idx] != ZERO_BT {
                    let gain = gains[idx];
                    for group in 0..*group_len_g {
                        let base = (window_base + group) * 128 + offsets[i] as usize;
                        let len = (offsets[i + 1] - offsets[i]) as usize;
                        for k in 0..len {
                            dst[base + k] += gain * src[base + k];
                        }
                    }
                }
                idx += 1;
            }
            window_base += *group_len_g;
        }
    }

    /// Resolves a channel element's tag to its output channel via the PCE
    /// plan. PCE-configured streams map elements SOLELY by tag (reference:
    /// `ff_aac_get_che`'s `tag_che_map`, where later entries overwrite
    /// earlier ones, so the LAST matching entry wins).
    fn pce_channel_for(
        &self,
        want_cpe: bool,
        want_lfe: bool,
        tag: u32,
    ) -> Result<usize, CadenceError> {
        let want_type = match (want_cpe, want_lfe) {
            (true, _) => PCE_CPE,
            (_, true) => PCE_LFE,
            _ => PCE_SCE,
        };
        let mut found = None;
        for i in 0..self.pce_plan_len {
            if self.pce_plan_type[i] == want_type && self.pce_plan_tag[i] as u32 == tag {
                found = Some(self.pce_plan_chan[i] as usize);
            }
        }
        found.ok_or_else(|| {
            CadenceError::CorruptData(format!(
                "channel element tag {tag} is not in the program configuration"
            ))
        })
    }

    /// program_config_element (ISO/IEC 14496-3 4.4.4). Channel
    /// configuration 0 streams are shaped by this element: it declares the
    /// channel elements (front/side/back/LFE, each single or pair) that
    /// the following raw data blocks carry, in decode order. The plan
    /// persists until a new PCE redefines it.
    fn decode_pce(&mut self, br: &mut BitReader) -> Result<(), CadenceError> {
        let _instance_tag = br.read_bits(4);
        let _object_type = br.read_bits(2);
        let sfi = br.read_bits(4) as usize;
        let num_front = br.read_bits(4) as usize;
        let num_side = br.read_bits(4) as usize;
        let num_back = br.read_bits(4) as usize;
        let num_lfe = br.read_bits(2) as usize;
        let num_assoc = br.read_bits(3) as usize;
        let num_cc = br.read_bits(4) as usize;
        let _mono_mixdown = br.read_bit();
        if _mono_mixdown {
            br.read_bits(4); // mono_mixdown_tag
        }
        let _stereo_mixdown = br.read_bit();
        if _stereo_mixdown {
            br.read_bits(4); // stereo_mixdown_tag
        }
        if br.read_bit() {
            // matrix_mixdown_idx + pseudo_surround_enable
            br.read_bits(3);
        }

        let mut plan_len = 0usize;
        let mut total = 0usize;
        for (count, ty) in [
            (num_front, PCE_SCE),
            (num_side, PCE_SCE),
            (num_back, PCE_SCE),
        ] {
            for _ in 0..count {
                if plan_len >= MAX_CHANNELS {
                    return Err(CadenceError::CorruptData(
                        "program configuration exceeds the channel limit".to_string(),
                    ));
                }
                let is_cpe = br.read_bit();
                let tag = br.read_bits(4) as u8;
                self.pce_plan_type[plan_len] = if is_cpe { PCE_CPE } else { ty };
                self.pce_plan_tag[plan_len] = tag;
                self.pce_plan_chan[plan_len] = total as u8;
                plan_len += 1;
                total += 1 + usize::from(is_cpe);
            }
        }
        for _ in 0..num_lfe {
            if plan_len >= MAX_CHANNELS {
                return Err(CadenceError::CorruptData(
                    "program configuration exceeds the channel limit".to_string(),
                ));
            }
            let tag = br.read_bits(4) as u8;
            self.pce_plan_type[plan_len] = PCE_LFE;
            self.pce_plan_tag[plan_len] = tag;
            self.pce_plan_chan[plan_len] = total as u8;
            plan_len += 1;
            total += 1;
        }
        // Associated-data (4-bit tags) and coupling-element (is_ind_sw + 4-bit
        // tag) lists are declarations only; the elements themselves, if any,
        // appear in the raw data block and are rejected there.
        br.skip_bits(4 * num_assoc + 5 * num_cc);
        if sfi != self.sf_index {
            log::warn!(
                "PCE sampling frequency index {sfi} differs from the stream's index {}",
                self.sf_index
            );
        }
        br.byte_align();
        let comment_bytes = br.read_bits(8) as usize;
        br.skip_bytes(comment_bytes);

        if total == 0 || total > MAX_CHANNELS {
            return Err(CadenceError::CorruptData(
                "program configuration channel count is out of range".to_string(),
            ));
        }
        if self.pce_plan_len > 0 && total != self.channels {
            return Err(CadenceError::UnsupportedFeature(
                "PCE changes the channel configuration mid-stream".to_string(),
            ));
        }

        let info = StreamInfo::new(Format::Aac, self.info.sample_rate, total as u16, 16);
        info.validate()?;
        self.info = info;
        self.channels = total;
        self.pce_plan_len = plan_len;
        self.pce_class_counts = [num_front, num_side, num_back, num_lfe];
        self.recompute_pce_out_order();
        Ok(())
    }

    /// Rebuilds `pce_out_order` from the installed plan, mirroring the
    /// reference's `sniff_channel_order`: elements get WAV channel
    /// positions per class (front/side/back/LFE; a leading single takes
    /// the class's center position, pairs take the left/right positions —
    /// "wide" positions once a class holds more than three channels),
    /// then entries stably sort by position.
    fn recompute_pce_out_order(&mut self) {
        if self.pce_plan_len == 0 {
            for (i, slot) in self.pce_out_order.iter_mut().enumerate() {
                *slot = i as u8;
            }
            return;
        }
        // WAV position bits per class row: leading single, wide pair,
        // inner pair. FRONT: FC | FLOC FROC | FL FR; SIDE/BACK:
        // (center unused) | SL SR | BL BR BC; LFE: LFE, LFE2.
        const FRONT: [i64; 6] = [4, 64, 128, 1, 2, 0];
        const SIDE: [i64; 6] = [0, 512, 1024, 16, 32, 256];
        const LFE_ROW: [i64; 6] = [8, 1032, 0, 0, 0, 0];

        let mut positions = [0i64; MAX_CHANNELS];
        let mut entry = 0usize;
        for (ci, count) in self.pce_class_counts.iter().enumerate() {
            let row = match ci {
                0 => FRONT,
                1 | 2 => SIDE,
                _ => LFE_ROW,
            };
            let mut nb = 0usize;
            for k in 0..*count {
                nb += 1 + usize::from(self.pce_plan_type[entry + k] == PCE_CPE);
            }
            if ci == 3 {
                for (k, slot) in positions[entry..entry + *count].iter_mut().enumerate() {
                    *slot = row[k];
                }
                entry += *count;
                continue;
            }
            let mut j = 0usize;
            while nb & 1 == 1 && entry < self.pce_plan_len {
                positions[entry] = row[j];
                entry += 1;
                nb -= 1;
                if row[j] == 0 {
                    break;
                }
                j = if ci != 1 && nb <= 3 { 3 } else { 1 };
            }
            j = j.min(4);
            while nb >= 2 && entry + 1 < self.pce_plan_len.max(MAX_CHANNELS) && j + 1 < row.len() {
                if row[j] < 0 || row[j + 1] < 0 {
                    break; // NONE sentinel: no more positions in this class
                }
                if self.pce_plan_type[entry] == PCE_CPE {
                    positions[entry] = row[j] | row[j + 1];
                    entry += 1;
                } else if entry + 1 < self.pce_plan_len.max(MAX_CHANNELS) {
                    positions[entry] = row[j];
                    positions[entry + 1] = row[j + 1];
                    entry += 2;
                } else {
                    break;
                }
                j += 2;
                nb -= 2;
            }
        }

        // Stable sort of (position, declaration index); a CPE entry sorts
        // by its left position and expands to both channels.
        let mut order: [(i64, u8); MAX_CHANNELS] = [(0, 0); MAX_CHANNELS];
        for (i, o) in order.iter_mut().enumerate().take(self.pce_plan_len) {
            *o = (positions[i], i as u8);
        }
        for i in 1..self.pce_plan_len {
            let mut j = i;
            while j > 0 && order[j - 1].0 > order[j].0 {
                order.swap(j - 1, j);
                j -= 1;
            }
        }
        let mut out_slot = 0usize;
        for &(_pos, entry) in order.iter().take(self.pce_plan_len) {
            let chan = self.pce_plan_chan[entry as usize] as usize;
            if out_slot >= MAX_CHANNELS {
                break;
            }
            self.pce_out_order[out_slot] = chan as u8;
            out_slot += 1;
            if self.pce_plan_type[entry as usize] == PCE_CPE && out_slot < MAX_CHANNELS {
                self.pce_out_order[out_slot] = (chan + 1) as u8;
                out_slot += 1;
            }
        }
        if self.fate_trace {
            eprintln!(
                "PCE computed: len={} out={:?}",
                self.pce_plan_len,
                &self.pce_out_order[..self.channels.max(1)]
            );
        }
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
            // predictor_data_present: for non-Main profiles this signals
            // LTP data. The syntax must be consumed for alignment (lag 11
            // bits, coefficient 3 bits, one used-bit per scalefactor band
            // up to 40), but LTP is not applied outside the LTP profile —
            // matching the reference decoder.
            if br.read_bit() {
                br.read_bits(11);
                br.read_bits(3);
                let used_bands = info.max_sfb.min(40);
                for _ in 0..used_bands {
                    br.read_bit();
                }
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
        if self.fate_trace {
            eprintln!(
                "ics_info ch={ch} seq={sequence} groups={} max_sfb={}",
                info.num_window_groups, info.max_sfb
            );
        }
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
        let win = match pre_window {
            Some(w) => w,
            None => self.decode_ics_info(br, ch)?,
        };
        self.decode_band_types(br, ch, &win)?;
        self.decode_scalefactors(br, ch, global_gain, &win)?;
        if self.fate_trace {
            eprintln!(
                "RUSF ch={ch} gg={global_gain} sfo={:?} bt={:?}",
                &self.channels_state[ch].sfo[..10],
                &self.channels_state[ch].band_type[..6]
            );
            let c: Vec<f32> = self.channels_state[ch].coeffs[..40].to_vec();
            eprintln!("RUCOEF ch={ch}: {:?}", c);
        }
        let pulse = self.decode_optional_tools(br, ch, &win)?;
        self.decode_spectral(br, ch, &win, pulse.as_ref())?;
        if self.fate_trace {
            let c: Vec<f32> = self.channels_state[ch].coeffs[..40].to_vec();
            eprintln!("RUCOEF ch={ch}: {:?}", c);
        }
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
                            // The noise band gain is the same dequantized
                            // 2^(sfo/4) as any other band: this decoder's
                            // MDCT kernel already carries the global −1 that
                            // the reference expresses as a negative sf
                            // (negating here too would invert the noise).
                            self.noise.fill_scaled(out, gain);
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
                                        let mut sign_bits = if nnz == 0 {
                                            0
                                        } else {
                                            br.read_bits(nnz) << (32 - nnz)
                                        };
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
                                } else if cb <= 6 {
                                    // Signed pairs (ISO codebooks 5/6): the
                                    // sign is part of the value table; no
                                    // sign bits in the bitstream (reference
                                    // `VMUL2` with `codebook_vector4_vals`).
                                    for d in 0..2 {
                                        let vi = (packed >> (4 * d)) & 15;
                                        let mag = tables::CODEBOOK_VALS_SIGNED_PAIR
                                            .get(vi as usize)
                                            .copied()
                                            .unwrap_or(0.0);
                                        out[k + d] = mag * gain;
                                    }
                                } else if cb <= 10 {
                                    // Unsigned pairs with nnz sign bits.
                                    let nnz = (packed >> 8) & 15;
                                    let mut sign_bits = if nnz == 0 {
                                        0
                                    } else {
                                        br.read_bits(nnz) << (32 - nnz)
                                    };
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
                                    let mut sign_bits = if nnz == 0 {
                                        0
                                    } else {
                                        br.read_bits(nnz) << (32 - nnz)
                                    };
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
        self.raw_pos = 0;
        self.raw_len = 0;
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
        if self.channels == 0 {
            // Channel configuration 0: the first PCE configures the channel
            // count. Decode frames until it arrives; the configuring
            // frame's own output is kept in staging.
            while self.channels == 0 {
                self.staged.clear();
                self.staged_pos = 0;
                if !self.decode_next_frame()? {
                    return Ok(0);
                }
            }
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
            // Re-read per iteration: an in-band PS flip during the frame
            // just decoded may have changed the channel count (mono HE-AAC
            // reconfigured as stereo output).
            let channels = self.channels;
            if channels == 0 || buffer.len() % channels != 0 {
                return Err(CadenceError::InvalidFormat(format!(
                    "buffer length {} is not a multiple of the channel count {}",
                    buffer.len(),
                    channels
                )));
            }
            let available = self.staged.len() - self.staged_pos;
            let take = (buffer.len() - written).min(available);
            buffer[written..written + take]
                .copy_from_slice(&self.staged[self.staged_pos..self.staged_pos + take]);
            self.staged_pos += take;
            written += take;
        }
        Ok(written / self.channels)
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
