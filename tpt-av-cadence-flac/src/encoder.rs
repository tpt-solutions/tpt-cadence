//! FLAC stream encoder (RFC 9639).
//!
//! Scope of this first cut (see `todo.md` for the full rationale):
//!
//! - Fixed blocksize only (a single common block size for every full frame;
//!   the final frame of a stream may be shorter). No block-switching.
//! - Subframe types: CONSTANT, VERBATIM, FIXED (predictor orders 0-4), and
//!   bounded first-order LPC selected by an autocorrelation estimate.
//! - Partitioned Rice residual coding, always using the Rice2 (5-bit
//!   parameter) coding method for simplicity, with a partition-order search
//!   (capped) and a per-partition optimal-parameter search, including the
//!   escaped/raw-partition fallback for low-entropy partitions (e.g. an
//!   all-zero partition costs 0 bits per sample via `raw_bits = 0`).
//! - Channel assignment is always INDEPENDENT (no left/side, right/side, or
//!   mid/side stereo decorrelation). This is spec-valid but leaves stereo
//!   compression on the table; a future session could add mid/side search.
//! - Sample rate and bit depth are always signalled via the STREAMINFO
//!   block (frame header codes 0), matching every frame to the stream's
//!   fixed format.
//! - The STREAMINFO MD5 field is left all-zero (the documented "not
//!   computed" convention many encoders use) rather than duplicating an MD5
//!   implementation into this crate; nothing in this crate's own decoder
//!   validates it, and it does not affect bitstream correctness.
//!
//! Despite the reduced feature set, the output is fully spec-compliant FLAC:
//! every frame carries correct header/footer CRCs, and this crate's own
//! `FlacDecoder` (or any conformant decoder) reconstructs the input
//! bit-exactly.

use std::io::{Seek, SeekFrom, Write};

use tpt_av_cadence_core::{f32_to_int, CadenceError, Encoder, Result};

use crate::lpc;
use crate::stream::{crc16, crc8};

/// Fixed block size used for every full frame (the last frame of a stream
/// may be shorter, if the total sample count isn't a multiple of this).
const BLOCK_SIZE: usize = 4096;

/// Maximum partition order attempted during the Rice partition search
/// (2^6 = 64 partitions per subframe at the default block size).
const MAX_PARTITION_ORDER: u32 = 6;

// ---------------------------------------------------------------------------
// MSB-first bit writer
// ---------------------------------------------------------------------------

struct BitWriter {
    bytes: Vec<u8>,
    bit_pos: u32,
}

impl BitWriter {
    fn new() -> Self {
        BitWriter {
            bytes: Vec::new(),
            bit_pos: 0,
        }
    }

    /// Appends the low `n` bits of `value`, most-significant bit first.
    fn push(&mut self, value: u64, n: u32) {
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
    fn push_signed(&mut self, value: i64, n: u32) {
        if n == 0 {
            return;
        }
        let mask = if n >= 64 { u64::MAX } else { (1u64 << n) - 1 };
        self.push((value as u64) & mask, n);
    }

    /// Pads with zero bits up to the next byte boundary.
    fn align(&mut self) {
        while self.bit_pos % 8 != 0 {
            self.push(0, 1);
        }
    }

    /// Writes FLAC's UTF-8-like variable-length frame number (RFC 9639
    /// §9.1.7), the inverse of `stream::read_utf8_number`.
    fn push_utf8(&mut self, value: u64) {
        if value < 0x80 {
            self.push(value, 8);
            return;
        }
        let bits = 64 - value.leading_zeros();
        let (extra, lead_bits): (u32, u32) = if bits <= 11 {
            (1, 5)
        } else if bits <= 16 {
            (2, 4)
        } else if bits <= 21 {
            (3, 3)
        } else if bits <= 26 {
            (4, 2)
        } else if bits <= 31 {
            (5, 1)
        } else {
            (6, 0)
        };
        let leading_prefix: u64 = match extra {
            1 => 0xC0,
            2 => 0xE0,
            3 => 0xF0,
            4 => 0xF8,
            5 => 0xFC,
            _ => 0xFE,
        };
        let lead_value = if lead_bits > 0 {
            (value >> (6 * extra)) & ((1 << lead_bits) - 1)
        } else {
            0
        };
        self.push(leading_prefix | lead_value, 8);
        for i in (0..extra).rev() {
            let cont = 0x80 | ((value >> (6 * i)) & 0x3F);
            self.push(cont, 8);
        }
    }
}

// ---------------------------------------------------------------------------
// Zigzag + Rice cost helpers
// ---------------------------------------------------------------------------

/// FLAC's zigzag mapping from a signed residual to an unsigned code
/// (0 -> 0, -1 -> 1, 1 -> 2, -2 -> 3, ...), the exact inverse of
/// `rice::decode_residual`'s `sval = (uval >> 1) ^ -(uval & 1)`.
fn zigzag(v: i32) -> u32 {
    ((v as u32) << 1) ^ ((v >> 31) as u32)
}

/// Searches Rice parameters 0..=30 for the cheapest encoding of `vals`
/// (already zigzag-mapped), returning `(param, cost_bits)`.
fn best_rice_param(vals: &[u32]) -> (u32, u64) {
    let mut best_k = 0u32;
    let mut best_cost = u64::MAX;
    for k in 0..=30u32 {
        let mut cost = vals.len() as u64 * (k as u64 + 1);
        if cost >= best_cost {
            continue;
        }
        for &v in vals {
            cost += (v >> k) as u64;
            if cost >= best_cost {
                break;
            }
        }
        if cost < best_cost {
            best_cost = cost;
            best_k = k;
        }
    }
    (best_k, best_cost)
}

/// Minimum two's-complement signed bit width that holds every value in
/// `vals`, or `None` if it would need more than 31 bits (the escaped
/// partition's `raw_bits` field is only 5 bits wide).
fn signed_bits_needed(vals: &[i32]) -> Option<u32> {
    let mut min = 0i64;
    let mut max = 0i64;
    for &v in vals {
        let v = v as i64;
        min = min.min(v);
        max = max.max(v);
    }
    if min == 0 && max == 0 {
        return Some(0);
    }
    let mut n = 1u32;
    while n <= 32 {
        let lo = -(1i64 << (n - 1));
        let hi = (1i64 << (n - 1)) - 1;
        if min >= lo && max <= hi {
            return if n <= 31 { Some(n) } else { None };
        }
        n += 1;
    }
    None
}

/// One partition's chosen coding: plain Rice (`raw_bits: None`, `param` is
/// the Rice parameter) or an escaped/raw partition (`param` is always 31,
/// the Rice2 escape code; `raw_bits` is the per-sample signed width).
struct PartitionPlan {
    param: u32,
    raw_bits: Option<u32>,
}

/// Picks the cheaper of a Rice-coded or raw/escaped partition.
fn plan_partition(vals: &[i32]) -> (PartitionPlan, u64) {
    let zz: Vec<u32> = vals.iter().map(|&v| zigzag(v)).collect();
    let (k, rice_cost) = best_rice_param(&zz);
    let rice_total = 5 + rice_cost; // 5-bit parameter field (Rice2) + payload

    if let Some(raw_bits) = signed_bits_needed(vals) {
        let raw_total = 5 + 5 + raw_bits as u64 * vals.len() as u64;
        if raw_total < rice_total {
            return (
                PartitionPlan {
                    param: 0x1F,
                    raw_bits: Some(raw_bits),
                },
                raw_total,
            );
        }
    }
    (
        PartitionPlan {
            param: k,
            raw_bits: None,
        },
        rice_total,
    )
}

/// Searches partition orders 0..=`MAX_PARTITION_ORDER` (bounded further by
/// divisibility and the predictor order) for the cheapest partitioned-Rice
/// plan, returning `(partition_order, per_partition_plans, total_cost_bits)`.
/// `total_cost_bits` includes the 4-bit partition-order field but not the
/// 2-bit coding-method field (constant across every candidate).
fn plan_residual(vals: &[i32], block_size: usize, order: usize) -> (u32, Vec<PartitionPlan>, u64) {
    let mut max_po = 0u32;
    while max_po < MAX_PARTITION_ORDER {
        let next = max_po + 1;
        if block_size % (1usize << next) != 0 {
            break;
        }
        if (block_size >> next) < order {
            break;
        }
        max_po = next;
    }

    let mut best: Option<(u32, Vec<PartitionPlan>, u64)> = None;
    for po in 0..=max_po {
        let part_count = 1usize << po;
        let mut pos = 0usize;
        let mut plans = Vec::with_capacity(part_count);
        let mut total = 4u64; // partition-order field
        for part in 0..part_count {
            let count = if po == 0 {
                block_size - order
            } else if part == 0 {
                (block_size >> po) - order
            } else {
                block_size >> po
            };
            let (plan, cost) = plan_partition(&vals[pos..pos + count]);
            total += cost;
            plans.push(plan);
            pos += count;
        }
        let better = match &best {
            Some((_, _, best_cost)) => total < *best_cost,
            None => true,
        };
        if better {
            best = Some((po, plans, total));
        }
    }
    best.expect("partition order 0 is always a valid candidate")
}

/// Writes a partitioned-Rice residual (always coding method 1 / Rice2) per
/// the plan from `plan_residual`.
fn write_residual(
    bw: &mut BitWriter,
    residual: &[i32],
    block_size: usize,
    order: usize,
    po: u32,
    plans: &[PartitionPlan],
) {
    bw.push(1, 2); // coding method: Rice2 (5-bit parameter)
    bw.push(po as u64, 4);
    let part_count = 1usize << po;
    let mut pos = 0usize;
    for (part, plan) in plans.iter().enumerate().take(part_count) {
        let count = if po == 0 {
            block_size - order
        } else if part == 0 {
            (block_size >> po) - order
        } else {
            block_size >> po
        };
        bw.push(plan.param as u64, 5);
        if let Some(raw_bits) = plan.raw_bits {
            bw.push(raw_bits as u64, 5);
            for &v in &residual[pos..pos + count] {
                bw.push_signed(v as i64, raw_bits);
            }
        } else {
            let k = plan.param;
            for &v in &residual[pos..pos + count] {
                let uval = zigzag(v);
                let q = uval >> k;
                for _ in 0..q {
                    bw.push(0, 1);
                }
                bw.push(1, 1);
                if k > 0 {
                    bw.push((uval & ((1u32 << k) - 1)) as u64, k);
                }
            }
        }
        pos += count;
    }
}

/// Computes the forward-predicted residual for fixed predictor `order`
/// (RFC 9639 §9.2.4), the exact inverse of `lpc::restore_fixed`.
/// `out[..order]` holds the (untouched) warm-up samples.
fn compute_fixed_residual(samples: &[i32], order: usize, out: &mut Vec<i32>) {
    out.clear();
    out.extend_from_slice(samples);
    let coefs = lpc::fixed_coefficients(order);
    for i in order..samples.len() {
        let mut acc: i64 = 0;
        for (j, &c) in coefs.iter().enumerate() {
            acc = acc.wrapping_add(c.wrapping_mul(samples[i - 1 - j] as i64));
        }
        out[i] = samples[i].wrapping_sub(acc as i32);
    }
}

/// Computes the residual for a quantized first-order LPC predictor.
fn compute_lpc_residual(samples: &[i32], coefficient: i64, shift: u32, out: &mut Vec<i32>) {
    out.clear();
    out.extend_from_slice(samples);
    for i in 1..samples.len() {
        let prediction = (coefficient.wrapping_mul(samples[i - 1] as i64) >> shift) as i32;
        out[i] = samples[i].wrapping_sub(prediction);
    }
}

/// Encodes one subframe (one channel's worth of one block).
fn encode_subframe(bw: &mut BitWriter, samples: &[i32], bps: u16, scratch: &mut Vec<i32>) {
    bw.push(0, 1); // padding

    if samples.iter().all(|&v| v == samples[0]) {
        bw.push(0b000000, 6); // CONSTANT
        bw.push(0, 1); // no wasted bits
        bw.push_signed(samples[0] as i64, bps as u32);
        return;
    }

    let max_order = 4usize.min(samples.len());
    let mut best_order = 0usize;
    let mut best_sum = u64::MAX;
    let mut best_residual: Vec<i32> = Vec::new();
    for order in 0..=max_order {
        compute_fixed_residual(samples, order, scratch);
        let sum_abs: u64 = scratch[order..]
            .iter()
            .map(|&r| r.unsigned_abs() as u64)
            .sum();
        if sum_abs < best_sum {
            best_sum = sum_abs;
            best_order = order;
            best_residual.clear();
            best_residual.extend_from_slice(scratch);
        }
    }

    let (po, plans, residual_cost) =
        plan_residual(&best_residual[best_order..], samples.len(), best_order);
    let fixed_cost = best_order as u64 * bps as u64 + 2 /* method */ + residual_cost;
    let verbatim_cost = samples.len() as u64 * bps as u64;

    const LPC_SHIFT: u32 = 12;
    const LPC_PRECISION: u32 = 15;
    let lpc_coefficients = lpc::analyze_lpc(samples, 1, LPC_SHIFT);
    let lpc_coefficient = lpc_coefficients[0];
    compute_lpc_residual(samples, lpc_coefficient, LPC_SHIFT, scratch);
    let lpc_residual: Vec<i32> = scratch[1..].to_vec();
    debug_assert_eq!(lpc_residual.len() + 1, samples.len());
    let (lpc_po, lpc_plans, lpc_residual_cost) = plan_residual(&lpc_residual, samples.len(), 1);
    let lpc_cost = bps as u64 + 4 + 5 + LPC_PRECISION as u64 + 2 + lpc_residual_cost;

    if lpc_cost < fixed_cost && lpc_cost < verbatim_cost {
        bw.push(0b100000, 6); // LPC, order 1
        bw.push(0, 1); // no wasted bits
        bw.push_signed(samples[0] as i64, bps as u32);
        bw.push((LPC_PRECISION - 1) as u64, 4);
        bw.push_signed(LPC_SHIFT as i64, 5);
        bw.push_signed(lpc_coefficient, LPC_PRECISION);
        write_residual(bw, &lpc_residual, samples.len(), 1, lpc_po, &lpc_plans);
    } else if fixed_cost <= verbatim_cost {
        bw.push(0b001000 | best_order as u64, 6); // FIXED, order in low 3 bits
        bw.push(0, 1); // no wasted bits
        for &w in &samples[..best_order] {
            bw.push_signed(w as i64, bps as u32);
        }
        write_residual(
            bw,
            &best_residual[best_order..],
            samples.len(),
            best_order,
            po,
            &plans,
        );
    } else {
        bw.push(0b000001, 6); // VERBATIM
        bw.push(0, 1);
        for &s in samples {
            bw.push_signed(s as i64, bps as u32);
        }
    }
}

// ---------------------------------------------------------------------------
// Top-level encoder
// ---------------------------------------------------------------------------

/// FLAC stream encoder implementing [`Encoder`].
///
/// Writes the `fLaC` marker and STREAMINFO block immediately in [`new`],
/// buffers incoming interleaved samples into fixed-size blocks in
/// [`Encoder::encode`], and patches STREAMINFO's block-size and
/// total-sample-count fields (which can only be known once the whole
/// stream is seen) in [`Encoder::finish`].
///
/// [`new`]: FlacEncoder::new
pub struct FlacEncoder<W: Write + Seek> {
    sink: W,
    channels: u16,
    bits_per_sample: u16,
    sample_rate: u32,

    /// Per-channel buffer for the block currently being assembled.
    channel_buf: Vec<Vec<i32>>,
    filled: usize,

    frame_number: u64,
    total_samples: u64,
    min_block_used: usize,
    max_block_used: usize,
    finished: bool,

    // Scratch reused across subframes to avoid per-block allocation for the
    // (order search x residual) intermediate.
    residual_scratch: Vec<i32>,
}

impl<W: Write + Seek> FlacEncoder<W> {
    /// Opens a new FLAC stream for writing. `bits_per_sample` must be in
    /// 4..=32 and `channels` in 1..=8, matching what this crate's own
    /// `FlacDecoder` accepts.
    pub fn new(mut sink: W, sample_rate: u32, channels: u16, bits_per_sample: u16) -> Result<Self> {
        if !(1..=8).contains(&channels) {
            return Err(CadenceError::InvalidFormat(format!(
                "FLAC encoder supports 1..=8 channels, got {channels}"
            )));
        }
        if !(4..=32).contains(&bits_per_sample) {
            return Err(CadenceError::InvalidFormat(format!(
                "FLAC encoder supports 4..=32 bit depths, got {bits_per_sample}"
            )));
        }
        if sample_rate == 0 {
            return Err(CadenceError::InvalidFormat(
                "FLAC stream must have a non-zero sample rate".to_string(),
            ));
        }

        sink.write_all(b"fLaC")?;
        // Metadata block header: last block, type STREAMINFO (0), length 34.
        sink.write_all(&[0x80, 0x00, 0x00, 0x22])?;
        write_streaminfo_body(&mut sink, sample_rate, channels, bits_per_sample, 0, 0)?;

        Ok(FlacEncoder {
            sink,
            channels,
            bits_per_sample,
            sample_rate,
            channel_buf: vec![vec![0i32; BLOCK_SIZE]; channels as usize],
            filled: 0,
            frame_number: 0,
            total_samples: 0,
            min_block_used: usize::MAX,
            max_block_used: 0,
            finished: false,
            residual_scratch: Vec::with_capacity(BLOCK_SIZE),
        })
    }

    fn emit_frame(&mut self, count: usize) -> Result<()> {
        let mut bw = BitWriter::new();

        // --- Frame header ---
        bw.push(0b11111111111110, 14); // sync
        bw.push(0, 1); // reserved
        bw.push(0, 1); // fixed blocksize (frame number coding)
        bw.push(7, 4); // block-size code: explicit 16-bit (count - 1)
        bw.push(0, 4); // sample-rate code: use STREAMINFO
        bw.push((self.channels - 1) as u64, 4); // independent channels
        bw.push(0, 3); // sample-size code: use STREAMINFO
        bw.push(0, 1); // reserved
        bw.push_utf8(self.frame_number);
        bw.push((count - 1) as u64, 16);

        bw.align();
        let crc = crc8(&bw.bytes);
        bw.bytes.push(crc);
        bw.bit_pos = bw.bytes.len() as u32 * 8;

        // --- Subframes ---
        for c in 0..self.channels as usize {
            let samples = &self.channel_buf[c][..count];
            encode_subframe(
                &mut bw,
                samples,
                self.bits_per_sample,
                &mut self.residual_scratch,
            );
        }
        bw.align();

        // --- Frame footer ---
        let crc = crc16(&bw.bytes);
        bw.bytes.extend_from_slice(&crc.to_be_bytes());

        self.sink.write_all(&bw.bytes)?;

        self.frame_number += 1;
        self.total_samples += count as u64;
        self.min_block_used = self.min_block_used.min(count);
        self.max_block_used = self.max_block_used.max(count);
        Ok(())
    }

    fn finalize(&mut self) -> Result<()> {
        if self.finished {
            return Ok(());
        }
        self.finished = true;
        if self.filled > 0 {
            let count = self.filled;
            self.emit_frame(count)?;
            self.filled = 0;
        }

        // STREAMINFO's min/max blocksize must be 16..=65535 or 0
        // ("unspecified"); a short final frame (or a stream shorter than 16
        // frames) can legitimately fall below 16, so fall back to 0 rather
        // than emit an out-of-range STREAMINFO field. The frame headers
        // themselves always carry the real, exact block size regardless.
        let min_block = if self.frame_number == 0 || self.min_block_used < 16 {
            0u16
        } else {
            self.min_block_used as u16
        };
        let max_block = if self.frame_number == 0 || self.max_block_used < 16 {
            0u16
        } else {
            self.max_block_used as u16
        };

        self.sink.seek(SeekFrom::Start(8))?;
        self.sink.write_all(&min_block.to_be_bytes())?;
        self.sink.write_all(&max_block.to_be_bytes())?;

        self.sink.seek(SeekFrom::Start(18))?;
        let packed = pack_streaminfo_tail(
            self.sample_rate,
            self.channels,
            self.bits_per_sample,
            self.total_samples,
        );
        self.sink.write_all(&packed.to_be_bytes())?;

        self.sink.seek(SeekFrom::End(0))?;
        self.sink.flush()?;
        Ok(())
    }
}

fn pack_streaminfo_tail(
    sample_rate: u32,
    channels: u16,
    bits_per_sample: u16,
    total_samples: u64,
) -> u64 {
    ((sample_rate as u64) << 44)
        | (((channels - 1) as u64) << 41)
        | (((bits_per_sample - 1) as u64) << 36)
        | (total_samples & ((1u64 << 36) - 1))
}

fn write_streaminfo_body<W: Write>(
    sink: &mut W,
    sample_rate: u32,
    channels: u16,
    bits_per_sample: u16,
    min_block: u16,
    max_block: u16,
) -> Result<()> {
    sink.write_all(&min_block.to_be_bytes())?;
    sink.write_all(&max_block.to_be_bytes())?;
    sink.write_all(&[0u8; 3])?; // min frame size: unknown
    sink.write_all(&[0u8; 3])?; // max frame size: unknown
    let packed = pack_streaminfo_tail(sample_rate, channels, bits_per_sample, 0);
    sink.write_all(&packed.to_be_bytes())?;
    sink.write_all(&[0u8; 16])?; // MD5: not computed
    Ok(())
}

impl<W: Write + Seek + Send> Encoder for FlacEncoder<W> {
    fn encode(&mut self, samples: &[f32]) -> Result<usize> {
        let channels = self.channels as usize;
        if samples.len() % channels != 0 {
            return Err(CadenceError::InvalidFormat(format!(
                "sample count {} is not a multiple of the channel count {}",
                samples.len(),
                channels
            )));
        }

        let frames_in = samples.len() / channels;
        let mut idx = 0usize;
        while idx < samples.len() {
            while self.filled < BLOCK_SIZE && idx < samples.len() {
                for c in 0..channels {
                    let s = samples[idx + c];
                    self.channel_buf[c][self.filled] = f32_to_int(s, self.bits_per_sample) as i32;
                }
                idx += channels;
                self.filled += 1;
            }
            if self.filled == BLOCK_SIZE {
                self.emit_frame(BLOCK_SIZE)?;
                self.filled = 0;
            }
        }
        Ok(frames_in)
    }

    fn finish(&mut self) -> Result<()> {
        self.finalize()
    }
}

impl<W: Write + Seek> Drop for FlacEncoder<W> {
    fn drop(&mut self) {
        // Best-effort finalize if the caller forgot; errors are unobservable
        // from `drop`, matching `std::fs::File`'s own drop-flush behavior.
        let _ = self.finalize();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decoder::FlacDecoder;
    use std::io::Cursor;
    use tpt_av_cadence_core::Decoder;

    fn round_trip(
        sample_rate: u32,
        channels: u16,
        bits_per_sample: u16,
        frames: &[f32],
    ) -> (Vec<f32>, tpt_av_cadence_core::StreamInfo) {
        let mut buf = Cursor::new(Vec::new());
        {
            let mut enc =
                FlacEncoder::new(&mut buf, sample_rate, channels, bits_per_sample).unwrap();
            enc.encode(frames).unwrap();
            Encoder::finish(&mut enc).unwrap();
        }
        buf.set_position(0);
        let mut dec = FlacDecoder::from_source(Box::new(buf)).unwrap();
        let info = dec.info().clone();
        let mut out = vec![0.0f32; frames.len() + channels as usize * 8];
        let mut written = 0;
        loop {
            let got = dec.decode(&mut out[written..]).unwrap();
            if got == 0 {
                break;
            }
            written += got * channels as usize;
        }
        out.truncate(written);
        (out, info)
    }

    fn quantize_plain(_sample_rate: u32, frames: &[f32], bits_per_sample: u16) -> Vec<f32> {
        frames
            .iter()
            .map(|&s| {
                tpt_av_cadence_core::int_to_f32(f32_to_int(s, bits_per_sample), bits_per_sample)
            })
            .collect()
    }

    #[test]
    fn silence_round_trips_bit_exact() {
        let frames = vec![0.0f32; 2 * 5000];
        let (got, info) = round_trip(44_100, 2, 16, &frames);
        assert_eq!(info.channels, 2);
        assert_eq!(info.sample_rate, 44_100);
        assert_eq!(got, frames);
    }

    #[test]
    fn sine_tone_round_trips_bit_exact() {
        let sample_rate = 44_100u32;
        let n = sample_rate as usize; // 1 second, mono
        let mut frames = vec![0.0f32; n];
        for (i, s) in frames.iter_mut().enumerate() {
            let t = i as f32 / sample_rate as f32;
            *s = (2.0 * std::f32::consts::PI * 440.0 * t).sin() * 0.5;
        }
        let expect = quantize_plain(sample_rate, &frames, 16);
        let (got, _) = round_trip(sample_rate, 1, 16, &frames);
        assert_eq!(got, expect);
    }

    #[test]
    fn white_noise_round_trips_bit_exact() {
        // Deterministic xorshift PRNG; exercises VERBATIM/high-order-residual
        // paths since white noise has no useful fixed-predictor structure.
        let mut state: u32 = 0x1234_5678;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            (state as f32 / u32::MAX as f32) * 2.0 - 1.0
        };
        let frames: Vec<f32> = (0..2 * 3000).map(|_| next() * 0.9).collect();
        let expect = quantize_plain(44_100, &frames, 16);
        let (got, _) = round_trip(44_100, 2, 16, &frames);
        assert_eq!(got, expect);
    }

    #[test]
    fn dc_offset_round_trips_bit_exact() {
        // Worst case for fixed predictors of order >= 1 relative to order 0:
        // a nonzero constant is handled by the CONSTANT subframe, so also
        // check a constant-plus-tiny-ripple signal that stays non-constant.
        let mut frames = vec![0.3f32; 4096 * 2];
        frames[4096] += 0.0001;
        let expect = quantize_plain(48_000, &frames, 16);
        let (got, _) = round_trip(48_000, 1, 16, &frames);
        assert_eq!(got, expect);
    }

    #[test]
    fn alternating_extremes_round_trips_bit_exact() {
        // Worst case for low-order fixed predictors: max amplitude flipping
        // sign every sample forces either a high-order predictor or an
        // escaped/verbatim fallback.
        let frames: Vec<f32> = (0..4096)
            .map(|i| if i % 2 == 0 { 1.0 } else { -1.0 })
            .collect();
        let expect = quantize_plain(44_100, &frames, 16);
        let (got, _) = round_trip(44_100, 1, 16, &frames);
        assert_eq!(got, expect);
    }

    #[test]
    fn constant_block_round_trips_bit_exact() {
        let frames = vec![0.25f32; 2 * 4096];
        let (got, _) = round_trip(44_100, 2, 16, &frames);
        assert_eq!(got, frames);
    }

    #[test]
    fn partial_final_block_round_trips_bit_exact() {
        // 2.5 blocks: exercises the finish()-time partial-frame flush.
        let mut state: u32 = 7;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            (state as f32 / u32::MAX as f32) * 2.0 - 1.0
        };
        let frames: Vec<f32> = (0..(BLOCK_SIZE * 2 + 500)).map(|_| next() * 0.5).collect();
        let expect = quantize_plain(44_100, &frames, 16);
        let (got, _) = round_trip(44_100, 1, 16, &frames);
        assert_eq!(got, expect);
    }

    #[test]
    fn multichannel_round_trips_bit_exact() {
        let channels = 6u16;
        let frames_count = 4096 + 100;
        let mut frames = vec![0.0f32; frames_count * channels as usize];
        for (i, s) in frames.iter_mut().enumerate() {
            let ch = i % channels as usize;
            let frame = i / channels as usize;
            let t = frame as f32 / 44_100.0;
            *s = (2.0 * std::f32::consts::PI * (200.0 + 50.0 * ch as f32) * t).sin() * 0.3;
        }
        let expect = quantize_plain(44_100, &frames, 16);
        let (got, _) = round_trip(44_100, channels, 16, &frames);
        assert_eq!(got, expect);
    }

    #[test]
    fn bit_depth_8_round_trips_bit_exact() {
        let frames: Vec<f32> = (0..2000)
            .map(|i| ((i % 200) as f32 / 100.0) - 1.0)
            .collect();
        let expect = quantize_plain(22_050, &frames, 8);
        let (got, _) = round_trip(22_050, 1, 8, &frames);
        assert_eq!(got, expect);
    }

    #[test]
    fn bit_depth_24_round_trips_bit_exact() {
        let mut state: u32 = 99;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            (state as f32 / u32::MAX as f32) * 2.0 - 1.0
        };
        let frames: Vec<f32> = (0..2 * 4200).map(|_| next() * 0.7).collect();
        let expect = quantize_plain(96_000, &frames, 24);
        let (got, _) = round_trip(96_000, 2, 24, &frames);
        assert_eq!(got, expect);
    }

    #[test]
    fn tiny_block_round_trips_bit_exact() {
        let frames = [0.1f32, -0.2, 0.3, -0.4, 0.5];
        let expect = quantize_plain(44_100, &frames, 16);
        let (got, _) = round_trip(44_100, 1, 16, &frames);
        assert_eq!(got, expect);
    }

    #[test]
    fn single_frame_round_trips_bit_exact() {
        let frames = [0.5f32];
        let expect = quantize_plain(44_100, &frames, 16);
        let (got, _) = round_trip(44_100, 1, 16, &frames);
        assert_eq!(got, expect);
    }

    #[test]
    fn zero_channels_rejected() {
        let buf = Cursor::new(Vec::new());
        assert!(FlacEncoder::new(buf, 44_100, 0, 16).is_err());
    }

    #[test]
    fn too_many_channels_rejected() {
        let buf = Cursor::new(Vec::new());
        assert!(FlacEncoder::new(buf, 44_100, 9, 16).is_err());
    }

    #[test]
    fn bad_bit_depth_rejected() {
        let buf = Cursor::new(Vec::new());
        assert!(FlacEncoder::new(buf, 44_100, 1, 3).is_err());
        let buf2 = Cursor::new(Vec::new());
        assert!(FlacEncoder::new(buf2, 44_100, 1, 33).is_err());
    }

    #[test]
    fn zero_sample_rate_rejected() {
        let buf = Cursor::new(Vec::new());
        assert!(FlacEncoder::new(buf, 0, 1, 16).is_err());
    }

    #[test]
    fn non_multiple_of_channels_is_error() {
        let mut buf = Cursor::new(Vec::new());
        let mut enc = FlacEncoder::new(&mut buf, 44_100, 2, 16).unwrap();
        assert!(enc.encode(&[0.0, 0.1, 0.2]).is_err());
    }

    #[test]
    fn finish_is_idempotent() {
        let mut buf = Cursor::new(Vec::new());
        let mut enc = FlacEncoder::new(&mut buf, 44_100, 1, 16).unwrap();
        enc.encode(&[0.0, 0.5]).unwrap();
        Encoder::finish(&mut enc).unwrap();
        Encoder::finish(&mut enc).unwrap();
    }

    #[test]
    fn empty_stream_finishes_cleanly() {
        let mut buf = Cursor::new(Vec::new());
        let mut enc = FlacEncoder::new(&mut buf, 44_100, 1, 16).unwrap();
        Encoder::finish(&mut enc).unwrap();
        drop(enc);
        buf.set_position(0);
        let mut dec = FlacDecoder::from_source(Box::new(buf)).unwrap();
        let mut out = [0.0f32; 4];
        assert_eq!(dec.decode(&mut out).unwrap(), 0);
    }

    #[test]
    fn total_samples_reported_correctly() {
        let mut buf = Cursor::new(Vec::new());
        let frames = vec![0.1f32; 4096 + 37];
        {
            let mut enc = FlacEncoder::new(&mut buf, 44_100, 1, 16).unwrap();
            enc.encode(&frames).unwrap();
            Encoder::finish(&mut enc).unwrap();
        }
        buf.set_position(0);
        let dec = FlacDecoder::from_source(Box::new(buf)).unwrap();
        assert_eq!(dec.streaminfo().total_samples, frames.len() as u64);
    }

    #[test]
    fn bitwriter_utf8_round_trips_via_frame_header() {
        // Indirect check: a stream long enough to reach a multi-byte UTF-8
        // frame number (>127 frames) still round-trips bit-exactly.
        let frames_needed = BLOCK_SIZE * 130;
        let mut state: u32 = 42;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            (state as f32 / u32::MAX as f32) * 2.0 - 1.0
        };
        let frames: Vec<f32> = (0..frames_needed).map(|_| next() * 0.4).collect();
        let expect = quantize_plain(44_100, &frames, 16);
        let (got, _) = round_trip(44_100, 1, 16, &frames);
        assert_eq!(got, expect);
    }
}
