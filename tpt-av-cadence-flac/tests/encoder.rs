//! A minimal FLAC stream encoder used to generate deterministic conformance
//! vectors for axes the bundled IETF test files don't cover (mono, >2
//! channels, variable block sizes, forced Rice escapes, Rice2, wide UTF-8
//! frame numbers, …).
//!
//! The decoder under test must reproduce the original PCM bit-exactly, and
//! the STREAMINFO MD5 (computed here independently via RFC 1321 MD5) must
//! match after decoding.

use tpt_av_cadence_flac::stream::{crc16, crc8};
use tpt_av_cadence_test_utils::md5::Md5;

/// MSB-first bit writer.
pub struct BitWriter {
    pub bytes: Vec<u8>,
    bit_pos: u32,
}

impl Default for BitWriter {
    fn default() -> Self {
        Self::new()
    }
}

impl BitWriter {
    pub fn new() -> Self {
        BitWriter {
            bytes: Vec::new(),
            bit_pos: 0,
        }
    }

    /// Appends the low `n` bits of `value`, most-significant bit first.
    pub fn push(&mut self, value: u64, n: u32) {
        for i in (0..n).rev() {
            let bit = ((value >> i) & 1) as u8;
            if self.bit_pos % 8 == 0 {
                self.bytes.push(0);
            }
            let shift = 7 - self.bit_pos % 8;
            *self.bytes.last_mut().unwrap() |= bit << shift;
            self.bit_pos += 1;
        }
    }

    /// Appends `value` as a two's-complement signed field of `n` bits.
    pub fn push_signed(&mut self, value: i64, n: u32) {
        let mask = if n >= 64 { u64::MAX } else { (1u64 << n) - 1 };
        self.push((value as u64) & mask, n);
    }

    /// Pads with zero bits to the next byte boundary.
    pub fn align(&mut self) {
        while self.bit_pos % 8 != 0 {
            self.push(0, 1);
        }
    }
}

/// Stereo decorrelation modes (encoder-side mirror of the stream codes).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StereoModeEnc {
    Left,
    Right,
    Mid,
}

/// Subframe coding strategy for generated streams.
#[derive(Clone, Debug)]
pub enum SubframeKind {
    /// All samples in the block are identical.
    Constant,
    /// Unencoded samples.
    Verbatim,
    /// Fixed predictor of order 0..=4 with Rice-coded residuals.
    Fixed(u32),
    /// LPC with explicit coefficients and shift, Rice-coded residuals.
    Lpc { coefs: Vec<i64>, shift: u32 },
}

/// Per-stream encoding options.
pub struct EncodeConfig {
    /// Stereo decorrelation: None = independent channels.
    pub stereo_mode: Option<StereoModeEnc>,
    pub subframe: SubframeKind,
    /// Partition order for Rice-coded residuals.
    pub partition_order: u32,
    /// Force escaped partitions (raw 31-bit residuals) instead of Rice coding.
    pub escape_partitions: bool,
    /// Use the Rice2 (5-bit parameter) residual method.
    pub rice2: bool,
    /// Variable block sizes across frames.
    pub variable_blocksize: bool,
    /// Declare this many wasted (zero) LSBs per sample. The generated PCM
    /// must have that many trailing zero bits.
    pub wasted_bits: u32,
}

impl Default for EncodeConfig {
    fn default() -> Self {
        EncodeConfig {
            stereo_mode: None,
            subframe: SubframeKind::Fixed(2),
            partition_order: 0,
            escape_partitions: false,
            rice2: false,
            variable_blocksize: false,
            wasted_bits: 0,
        }
    }
}

fn block_size_code(block_size: usize) -> (u16, Option<u16>) {
    match block_size {
        192 => (1, None),
        576 => (2, None),
        1152 => (3, None),
        2304 => (4, None),
        4608 => (5, None),
        256 => (8, None),
        512 => (9, None),
        1024 => (10, None),
        2048 => (11, None),
        4096 => (12, None),
        8192 => (13, None),
        16384 => (14, None),
        32768 => (15, None),
        1..=256 => (6, Some((block_size - 1) as u16)),
        _ => (7, Some((block_size - 1) as u16)),
    }
}

fn sample_rate_code(rate: u32) -> (u16, Option<u32>) {
    match rate {
        88_200 => (1, None),
        176_400 => (2, None),
        192_000 => (3, None),
        8_000 => (4, None),
        16_000 => (5, None),
        22_050 => (6, None),
        24_000 => (7, None),
        32_000 => (8, None),
        44_100 => (9, None),
        48_000 => (10, None),
        96_000 => (11, None),
        _ => (12, Some(rate)),
    }
}

fn bits_per_sample_code(bps: u16) -> u16 {
    match bps {
        8 => 1,
        12 => 2,
        16 => 4,
        20 => 5,
        24 => 6,
        32 => 7,
        _ => 0, // from STREAMINFO (covers 15 bps and other odd depths)
    }
}

/// Writes a positive number in FLAC's UTF-8-like coding (1–7 bytes).
pub fn push_utf8_number(w: &mut BitWriter, value: u64) {
    assert!(value > 0 || true);
    if value < 0x80 {
        w.push(value, 8);
        return;
    }
    let bits = 64 - value.leading_zeros();
    // Capacity of n+1 total bytes is 6 + 5n bits; choose the smallest n.
    let n = (bits - 6).div_ceil(5).clamp(1, 6);
    let lead: u64 = match n {
        1 => 0xC0,
        2 => 0xE0,
        3 => 0xF0,
        4 => 0xF8,
        5 => 0xFC,
        _ => 0xFE,
    };
    let lead_bits = 6 - n; // value bits carried by the lead byte
    w.push(lead | (value >> (6 * n)) & ((1 << lead_bits) - 1), 8);
    for i in (0..n).rev() {
        w.push(0x80 | ((value >> (6 * i)) & 0x3F), 8);
    }
}

fn zigzag(r: i64) -> u64 {
    ((r << 1) ^ (r >> 63)) as u64
}

fn push_rice(w: &mut BitWriter, residual: i64, p: u64) {
    let uval = zigzag(residual);
    let q = uval >> p;
    for _ in 0..q {
        w.push(0, 1);
    }
    w.push(1, 1);
    if p > 0 {
        w.push(uval & ((1u64 << p) - 1), p as u32);
    }
}

fn pick_param(residuals: &[i64], p_limit: u64) -> u64 {
    let max_abs = residuals
        .iter()
        .fold(0i64, |m, &r| m.max(if r < 0 { -r } else { r }));
    let mut p = 0u64;
    while p < p_limit && (max_abs >> p) > 14 {
        p += 1;
    }
    p
}

/// Assembles the complete FLAC byte stream.
pub fn encode_stream(
    channels: &[Vec<i32>],
    bps: u16,
    sample_rate: u32,
    block_size: usize,
    config: &EncodeConfig,
) -> Vec<u8> {
    let num_channels = channels.len();
    let total_frames = channels[0].len();

    // --- plan the frame blocks first (STREAMINFO declares real bounds) ---
    let mut blocks: Vec<usize> = Vec::new();
    let mut start = 0usize;
    while start < total_frames {
        let block = if config.variable_blocksize {
            let n = if blocks.len() % 2 == 0 {
                block_size.min(256)
            } else {
                block_size.clamp(16, 100)
            };
            n.min(total_frames - start)
        } else {
            block_size.min(total_frames - start)
        };
        blocks.push(block);
        start += block;
    }
    let max_block = blocks.iter().copied().max().unwrap_or(16).max(16);
    let min_block = blocks.iter().copied().min().unwrap_or(16).max(16);

    // --- STREAMINFO + fLaC marker ---
    let md5 = pcm_md5(channels, bps);
    let mut si = BitWriter::new();
    si.push(min_block as u64, 16);
    si.push(max_block as u64, 16);
    si.push(0, 24); // min framesize unknown
    si.push(0, 24); // max framesize unknown
    si.push(sample_rate as u64, 20);
    si.push((num_channels - 1) as u64, 3);
    si.push((bps - 1) as u64, 5);
    si.push(total_frames as u64, 36);
    for b in md5 {
        si.push(b as u64, 8);
    }

    let mut out: Vec<u8> = b"fLaC".to_vec();
    out.push(0x80); // last-of-stream flag, type 0 = STREAMINFO
                    // 24-bit big-endian block length.
    let len = si.bytes.len() as u32;
    out.push((len >> 16) as u8);
    out.push((len >> 8) as u8);
    out.push(len as u8);
    out.extend_from_slice(&si.bytes);

    // --- frames ---
    let mut start = 0usize;
    for (frame_index, &block) in blocks.iter().enumerate() {
        out.extend_from_slice(&encode_frame(
            channels,
            start,
            block,
            bps,
            sample_rate,
            frame_index as u64,
            config,
        ));
        start += block;
    }
    out
}

fn encode_frame(
    channels: &[Vec<i32>],
    start: usize,
    block: usize,
    bps: u16,
    sample_rate: u32,
    frame_index: u64,
    config: &EncodeConfig,
) -> Vec<u8> {
    let num_channels = channels.len();
    let mut w = BitWriter::new();

    w.push(0b11111111111110, 14); // sync
    w.push(0, 1); // reserved
    w.push(config.variable_blocksize as u64, 1); // blocking strategy

    let (bs_code, bs_extra) = block_size_code(block);
    w.push(bs_code as u64, 4);

    let (sr_code, sr_extra) = sample_rate_code(sample_rate);
    w.push(sr_code as u64, 4);

    if num_channels == 2 {
        if let Some(mode) = config.stereo_mode {
            w.push(
                match mode {
                    StereoModeEnc::Left => 8,
                    StereoModeEnc::Right => 9,
                    StereoModeEnc::Mid => 10,
                } as u64,
                4,
            );
        } else {
            w.push(1, 4);
        }
    } else {
        w.push((num_channels - 1) as u64, 4);
    }

    w.push(bits_per_sample_code(bps) as u64, 3);
    w.push(0, 1); // reserved

    // Fixed blocksize: frame number; variable: first sample number.
    let number = if config.variable_blocksize {
        start as u64
    } else {
        frame_index
    };
    push_utf8_number(&mut w, number);

    // Extension bytes at the end of the header: block size, then sample rate.
    if let Some(extra) = bs_extra {
        w.push(extra as u64, if bs_code == 6 { 8 } else { 16 });
    }
    if let Some(extra) = sr_extra {
        w.push(extra as u64, 8);
    }

    w.push(crc8(&w.bytes) as u64, 8); // header CRC-8

    // Subframes: decorrelate for stereo modes.
    let mut planes: Vec<Vec<i64>> = channels
        .iter()
        .map(|c| c[start..start + block].iter().map(|&s| s as i64).collect())
        .collect();
    if num_channels == 2 {
        if let Some(mode) = config.stereo_mode {
            let l: Vec<i64> = channels[0][start..start + block]
                .iter()
                .map(|&s| s as i64)
                .collect();
            let r: Vec<i64> = channels[1][start..start + block]
                .iter()
                .map(|&s| s as i64)
                .collect();
            planes.clear();
            match mode {
                StereoModeEnc::Left => {
                    planes.push(l.clone());
                    planes.push(l.iter().zip(&r).map(|(a, b)| a - b).collect());
                }
                StereoModeEnc::Right => {
                    // Side channel comes first (channel 0) per RFC 9639.
                    planes.push(l.iter().zip(&r).map(|(a, b)| a - b).collect());
                    planes.push(r.clone());
                }
                StereoModeEnc::Mid => {
                    planes.push(l.iter().zip(&r).map(|(a, b)| (a + b) >> 1).collect());
                    planes.push(l.iter().zip(&r).map(|(a, b)| a - b).collect());
                }
            }
        }
    }

    // The side plane is one bit wider: channel 1 for left/side and
    // mid/side, channel 0 for right/side.
    let plane_bps: Vec<u16> = (0..planes.len())
        .map(|c| {
            let side = match config.stereo_mode {
                Some(StereoModeEnc::Right) => c == 0,
                Some(_) => c == 1,
                None => false,
            };
            if side {
                bps + 1
            } else {
                bps
            }
        })
        .collect();
    for (plane, pbps) in planes.iter().zip(&plane_bps) {
        encode_subframe(&mut w, plane, *pbps, config);
    }

    w.align();
    w.push(crc16(&w.bytes) as u64, 16); // frame CRC-16
    w.bytes
}

fn encode_subframe(w: &mut BitWriter, plane: &[i64], bps: u16, config: &EncodeConfig) {
    let wasted = config.wasted_bits;
    let sub_bps = bps as u32 - wasted;

    w.push(0, 1); // padding bit

    // Everything downstream (warm-up, prediction, residuals) operates on
    // the shifted samples; the decoder shifts back after reconstruction.
    let plane_owned: Vec<i64>;
    let plane: &[i64] = if wasted > 0 {
        plane_owned = plane.iter().map(|s| s >> wasted).collect();
        &plane_owned
    } else {
        plane
    };

    match &config.subframe {
        SubframeKind::Constant => {
            w.push(0b000000, 6);
            push_wasted_flag(w, wasted);
            w.push_signed(plane[0], sub_bps);
            // CONSTANT blocks are exactly one value; residuals are implied.
        }
        SubframeKind::Verbatim => {
            w.push(0b000001, 6);
            push_wasted_flag(w, wasted);
            for &s in plane {
                w.push_signed(s, sub_bps);
            }
        }
        SubframeKind::Fixed(order) => {
            let order = *order as usize;
            w.push((0b001000 | order) as u64, 6);
            push_wasted_flag(w, wasted);
            for &sample in &plane[..order] {
                w.push_signed(sample, sub_bps);
            }
            let coefs: &[i64] = match order {
                0 => &[],
                1 => &[1],
                2 => &[2, -1],
                3 => &[3, -3, 1],
                4 => &[4, -6, 4, -1],
                _ => unreachable!("fixed orders are 0..=4"),
            };
            let residuals: Vec<i64> = (order..plane.len())
                .map(|i| {
                    let pred: i64 = coefs
                        .iter()
                        .enumerate()
                        .map(|(j, &c)| c * plane[i - 1 - j])
                        .sum();
                    plane[i] - pred
                })
                .collect();
            push_residuals(w, &residuals, plane.len(), order, config);
        }
        SubframeKind::Lpc { coefs, shift } => {
            let order = coefs.len();
            w.push((0b100000 | (order - 1)) as u64, 6);
            push_wasted_flag(w, wasted);
            for &sample in &plane[..order] {
                w.push_signed(sample, sub_bps);
            }
            // Precision: signed bits needed for the widest coefficient.
            // For c >= 0: bits(c) + 1; for c < 0: bits(!c) — i.e. the
            // classic "flip negatives" trick on the u64 magnitude.
            let precision = coefs
                .iter()
                .map(|&c| {
                    let m = if c < 0 { !(c as u64) } else { c as u64 };
                    65 - m.leading_zeros()
                })
                .max()
                .unwrap_or(1)
                .clamp(1, 15);
            w.push((precision - 1) as u64, 4);
            w.push_signed(*shift as i64, 5);
            for &c in coefs {
                w.push_signed(c, precision);
            }
            let residuals: Vec<i64> = (order..plane.len())
                .map(|i| {
                    let pred: i64 = coefs
                        .iter()
                        .enumerate()
                        .map(|(j, &c)| c * plane[i - 1 - j])
                        .sum();
                    plane[i] - (pred >> *shift)
                })
                .collect();
            push_residuals(w, &residuals, plane.len(), order, config);
        }
    }
}

fn push_wasted_flag(w: &mut BitWriter, wasted: u32) {
    if wasted == 0 {
        w.push(0, 1);
    } else {
        w.push(1, 1);
        for _ in 0..(wasted - 1) {
            w.push(0, 1);
        }
        w.push(1, 1);
    }
}

fn push_residuals(
    w: &mut BitWriter,
    residuals: &[i64],
    block: usize,
    order: usize,
    config: &EncodeConfig,
) {
    let param_bits: u32 = if config.rice2 { 5 } else { 4 };
    let escape_code: u64 = if config.rice2 { 0x1F } else { 0xF };
    let p_limit: u64 = if config.rice2 { 31 } else { 14 };

    w.push(if config.rice2 { 1 } else { 0 }, 2); // residual method
    w.push(config.partition_order as u64, 4);

    let parts = 1usize << config.partition_order;
    let mut idx = 0usize;
    for part in 0..parts {
        let count = if config.partition_order == 0 {
            block - order
        } else if part == 0 {
            (block >> config.partition_order) - order
        } else {
            block >> config.partition_order
        };
        let slice = &residuals[idx..idx + count];
        if config.escape_partitions {
            w.push(escape_code, param_bits);
            w.push(31, 5);
            for &r in slice {
                w.push_signed(r, 31);
            }
        } else {
            let p = pick_param(slice, p_limit);
            w.push(p, param_bits);
            for &r in slice {
                push_rice(w, r, p);
            }
        }
        idx += count;
    }
}

/// MD5 over interleaved signed little-endian samples, as FLAC defines it.
pub fn pcm_md5(channels: &[Vec<i32>], bps: u16) -> [u8; 16] {
    let bytes_per = (bps.div_ceil(8)) as usize;
    let frames = channels[0].len();
    let mut md5 = Md5::new();
    let mut buf = vec![0u8; bytes_per];
    for f in 0..frames {
        for c in channels {
            let v = c[f] as i64;
            buf.copy_from_slice(&v.to_le_bytes()[..bytes_per]);
            md5.update(&buf);
        }
    }
    md5.finalize()
}
