//! The Opus range coder (RFC 6716 §4.1 decoder, §5.1 encoder).
//!
//! All arithmetic is bit-exact integer arithmetic as the RFC requires. The
//! decoder is the piece every SILK/CELT frame will consume; the encoder is
//! provided so tests (and eventually the future encoder crates) can produce
//! round-trippable streams.

use tpt_av_cadence_core::{CadenceError, Result};

#[inline]
fn ilog(x: u32) -> u32 {
    32 - x.leading_zeros()
}

// ---------------------------------------------------------------------------
// Decoder
// ---------------------------------------------------------------------------

/// Range decoder over one Opus frame's bytes.
///
/// Implements RFC 6716 §4.1: `rng` starts at 128, `val` holds
/// `127 - (b0 >> 1)`, the low bit of the first byte is buffered for
/// renormalization, and reading past the end yields zero bits.
pub struct RangeDecoder<'a> {
    bytes: &'a [u8],
    /// Index of the next byte to read.
    next: usize,
    rng: u32,
    val: u32,
    /// Buffered low bit of the previously read byte (§4.1.1/§4.1.2.1).
    leftover_bit: u32,
    /// Total bits fed, for `tell` (§4.1.6).
    nbits_total: u32,
    /// Bit cursor for raw bits, counted from the end of the frame (§4.1.4).
    end_bit_pos: usize,
}

impl<'a> RangeDecoder<'a> {
    /// Initializes the decoder over `frame` (§4.1.1) and renormalizes.
    pub fn new(frame: &'a [u8]) -> Self {
        let b0 = frame.first().copied().unwrap_or(0) as u32;
        let mut dec = RangeDecoder {
            bytes: frame,
            next: 1,
            rng: 128,
            val: 127 - (b0 >> 1),
            leftover_bit: b0 & 1,
            nbits_total: 9,
            end_bit_pos: 0,
        };
        dec.normalize();
        dec
    }

    /// Reads the next byte, or zero past the end (§4.1.2.1).
    fn read_byte(&mut self) -> u32 {
        let b = self.bytes.get(self.next).copied().unwrap_or(0) as u32;
        self.next += 1;
        b
    }

    /// Renormalization (§4.1.2.1): while `rng <= 2^23`, shift and feed a
    /// 7-bit symbol assembled from the buffered bit and the next byte.
    fn normalize(&mut self) {
        while self.rng <= 1 << 23 {
            self.rng <<= 8;
            let byte = self.read_byte();
            let sym = (self.leftover_bit << 7) | (byte >> 1);
            self.leftover_bit = byte & 1;
            self.val = ((self.val << 8) + (255 - sym)) & 0x7FFF_FFFF;
            self.nbits_total += 8;
        }
    }

    /// `ec_decode`: returns the 16-bit frequency slot `fs` for the next
    /// symbol; the caller finds `k` with `fl[k] <= fs < fh[k]`.
    pub fn decode(&mut self, ft: u32) -> Result<u32> {
        if ft == 0 {
            return Err(CadenceError::CorruptData(
                "range decoder: ft must be non-zero".to_string(),
            ));
        }
        let dm = self.rng / ft;
        if dm == 0 {
            return Err(CadenceError::CorruptData(
                "range decoder: rng < ft (corrupt frame)".to_string(),
            ));
        }
        let fs = ft - (self.val / dm + 1).min(ft);
        Ok(fs)
    }

    /// `ec_dec_update`: narrows the range to the symbol spanning
    /// `[fl, fh)` in `[0, ft)` and renormalizes (§4.1.2).
    pub fn update(&mut self, fl: u32, fh: u32, ft: u32) {
        let dm = self.rng / ft;
        self.val = self.val.wrapping_sub(dm.wrapping_mul(ft - fh));
        if fl > 0 {
            self.rng = dm.wrapping_mul(fh - fl);
        } else {
            self.rng = self.rng.wrapping_sub(dm.wrapping_mul(ft - fh));
        }
        self.normalize();
    }

    /// `ec_decode_bin` (§4.1.3.1): decode over `1 << ftb` equiprobable slots.
    pub fn decode_bin(&mut self, ftb: u32) -> Result<u32> {
        self.decode(1 << ftb)
    }

    /// `ec_dec_bit_logp` (§4.1.3.2): decodes one bit where P(1) = 2^-logp.
    pub fn decode_bit_logp(&mut self, logp: u32) -> Result<bool> {
        let ft = 1u32 << logp;
        let fs = self.decode(ft)?;
        let is_one = fs >= ft - 1;
        if is_one {
            self.update(ft - 1, ft, ft);
        } else {
            self.update(0, ft - 1, ft);
        }
        Ok(is_one)
    }

    /// `ec_dec_icdf` (§4.1.3.3): decodes a symbol from an inverse-CDF
    /// table (entries are `(1 << ftb) - fh[k]`, terminated by a 0 entry
    /// that is itself the last symbol's boundary).
    ///
    /// Implemented in libopus's algebraic form: with `r = rng >> ftb`,
    /// symbol `k` satisfies `d >= r*icdf[k]` for the first `k`, giving
    /// `val -= r*icdf[k]` and `rng = r*(icdf[k-1] - icdf[k])`.
    pub fn decode_icdf(&mut self, icdf: &[u8], ftb: u32) -> Result<u32> {
        let r = self.rng >> ftb;
        if r == 0 {
            return Err(CadenceError::CorruptData(
                "range decoder: rng < ft (corrupt frame)".to_string(),
            ));
        }
        let d = self.val;
        let mut t;
        let mut s = self.rng;
        let mut ret = 0usize;
        loop {
            t = s;
            let entry = *icdf.get(ret).ok_or_else(|| {
                CadenceError::CorruptData("icdf table missing its 0 terminator".to_string())
            })? as u32;
            s = r * entry;
            ret += 1;
            if d >= s {
                break;
            }
        }
        self.val = d - s;
        self.rng = t - s;
        self.normalize();
        Ok((ret - 1) as u32)
    }

    /// `ec_dec_bits` (§4.1.4): reads `count` raw bits packed LSB-first from
    /// the end of the frame. Reading past the start yields zero bits.
    pub fn read_raw_bits(&mut self, count: u32) -> u32 {
        let mut value: u32 = 0;
        for i in 0..count {
            let bit_index = self.end_bit_pos;
            self.end_bit_pos += 1;
            let bit = match self.bytes.len().checked_sub(1 + bit_index / 8) {
                Some(bi) => (self.bytes[bi] >> (bit_index % 8)) & 1,
                None => 0,
            };
            value |= (bit as u32) << i;
        }
        // `ec_dec_bits` (entdec.c) counts raw bits towards `nbits_total`
        // too, since `ec_tell`/`ec_tell_frac` must reflect the whole
        // frame's bit budget, not just the range-coded portion.
        self.nbits_total += count;
        value
    }

    /// `ec_dec_uint` (§4.1.5): decodes one of `ft` equiprobable values.
    ///
    /// Out-of-range values on corrupt frames clamp to `ft - 1` and keep
    /// decoding (matching libopus, which sets its error flag).
    pub fn decode_uint(&mut self, ft: u32) -> Result<u32> {
        if ft < 2 {
            return Ok(0);
        }
        let ft_dec = ft - 1;
        let ftb = ilog(ft_dec);
        if ftb <= 8 {
            let t = self.decode(ft)?;
            self.update(t, t + 1, ft);
            Ok(t)
        } else {
            let shift = ftb - 8;
            let top_ft = (ft_dec >> shift) + 1;
            let mut t = self.decode(top_ft)?;
            self.update(t, t + 1, top_ft);
            t = (t << shift) | self.read_raw_bits(shift);
            if t > ft_dec {
                return Ok(ft_dec);
            }
            Ok(t)
        }
    }

    /// Whole-bit usage (§4.1.6): a conservative count of consumed bits.
    pub fn tell(&self) -> u32 {
        self.nbits_total - ilog(self.rng)
    }

    /// `ec_tell_frac`: bit usage in 1/8-bit units, using the same linear
    /// +correction-table shortcut as libopus (`entcode.c`).
    pub fn tell_frac(&self) -> u32 {
        static CORRECTION: [u32; 8] = [35733, 38967, 42495, 46340, 50535, 55109, 60097, 65535];
        let nbits = self.nbits_total << 3;
        let l = ilog(self.rng);
        let r = self.rng >> (l - 16);
        let mut b = (r >> 12) - 8;
        b += u32::from(r > CORRECTION[b as usize]);
        nbits - ((l << 3) + b)
    }

    /// Adjusts `nbits_total` so that [`tell`][Self::tell] reports `target`
    /// (used by the CELT silence flag to pretend all bits were read).
    pub fn force_tell(&mut self, target: i32) {
        let delta = target - self.tell() as i32;
        self.nbits_total = (self.nbits_total as i32 + delta) as u32;
    }

    /// The current range (seed for the CELT spectral LCG).
    pub fn rng(&self) -> u32 {
        self.rng
    }

    /// `dec.storage -= n` (`opus_decoder.c`): marks the last `n` bytes of
    /// the frame as off-limits (they hold CELT redundancy coded with raw
    /// bits at the frame's end, not entropy-coded data). Raw-bit reads are
    /// positioned relative to the end of the storage, so truncating the
    /// slice is exactly the reference's shrink; previously range-coded
    /// state (`rng`/`val`/`nbits_total`) is untouched.
    pub fn shrink_storage(&mut self, bytes_to_remove: usize) {
        let keep = self.bytes.len().saturating_sub(bytes_to_remove);
        self.bytes = &self.bytes[..keep];
    }
}

// ---------------------------------------------------------------------------
// Encoder
// ---------------------------------------------------------------------------

/// Range encoder (RFC 6716 §5.1): state `(val, rng, rem, ext)` initialized
/// to `(0, 2^31, -1, 0)`.
///
/// Range-coded bytes are collected front-to-back; raw bits are collected
/// separately (they fill the frame from its end) and are appended reversed
/// during [`RangeEncoder::done`], producing streams that round-trip through
/// [`RangeDecoder`].
#[derive(Clone)]
pub struct RangeEncoder {
    val: u32,
    rng: u32,
    rem: i32,
    ext: u32,
    out: Vec<u8>,
    /// Raw-bit bytes in production order; the first produced is the frame's
    /// LAST byte.
    end_bytes: Vec<u8>,
    end_window: u32,
    nend_bits: u32,
    nbits_total: u32,
}

impl Default for RangeEncoder {
    fn default() -> Self {
        Self::new()
    }
}

impl RangeEncoder {
    pub fn new() -> Self {
        RangeEncoder {
            val: 0,
            rng: 1 << 31,
            rem: -1,
            ext: 0,
            out: Vec::new(),
            end_bytes: Vec::new(),
            end_window: 0,
            nend_bits: 0,
            nbits_total: 33,
        }
    }

    /// §5.1.1.2: emits a 9-bit value (8 data bits + carry bit).
    fn carry_out(&mut self, c: u32) {
        if c == 255 {
            self.ext = self.ext.wrapping_add(1);
            return;
        }
        let b = (c >> 8) as i32;
        if self.rem != -1 {
            self.out.push((self.rem + b) as u8);
        }
        if self.ext != 0 {
            let filler = if b != 0 { 0u8 } else { 255u8 };
            for _ in 0..self.ext {
                self.out.push(filler);
            }
            self.ext = 0;
        }
        self.rem = (c & 255) as i32;
    }

    /// §5.1.1.1: renormalize while `rng <= 2^23`.
    fn normalize(&mut self) {
        while self.rng <= 1 << 23 {
            self.carry_out(self.val >> 23);
            self.val = (self.val << 8) & 0x7FFF_FFFF;
            self.rng <<= 8;
            self.nbits_total += 8;
        }
    }

    /// §5.1.1: encode symbol `k` spanning `[fl, fh)` in `[0, ft)`.
    pub fn encode(&mut self, fl: u32, fh: u32, ft: u32) {
        if ft == 0 {
            return;
        }
        let dm = self.rng / ft;
        if fl > 0 {
            self.val = self
                .val
                .wrapping_add(self.rng.wrapping_sub(dm.wrapping_mul(ft - fl)));
            self.rng = dm.wrapping_mul(fh - fl);
        } else {
            self.rng = self.rng.wrapping_sub(dm.wrapping_mul(ft - fh));
        }
        self.normalize();
    }

    /// §5.1.2.1: encode over `1 << ftb` equiprobable slots.
    pub fn encode_bin(&mut self, fl: u32, fh: u32, ftb: u32) {
        self.encode(fl, fh, 1 << ftb);
    }

    /// §5.1.2.2: encode one bit with P(1) = 2^-logp.
    pub fn encode_bit_logp(&mut self, bit: bool, logp: u32) {
        let ft = 1u32 << logp;
        if bit {
            self.encode(ft - 1, ft, ft);
        } else {
            self.encode(0, ft - 1, ft);
        }
    }

    /// §5.1.2.3: encode symbol `k` from an icdf table.
    pub fn encode_icdf(&mut self, k: u32, icdf: &[u8], ftb: u32) {
        let ft = 1u32 << ftb;
        let fh = ft - icdf[k as usize] as u32;
        let fl = if k == 0 {
            0
        } else {
            ft - icdf[k as usize - 1] as u32
        };
        self.encode(fl, fh, ft);
    }

    /// §5.1.3: pack `bits` raw bits at the end of the frame (LSB-first).
    pub fn write_raw_bits(&mut self, value: u32, bits: u32) {
        self.end_window |= value << self.nend_bits;
        self.nend_bits += bits;
        while self.nend_bits >= 8 {
            self.end_bytes.push(self.end_window as u8);
            self.end_window >>= 8;
            self.nend_bits -= 8;
        }
        // `ec_enc_bits` (entenc.c) counts raw bits towards `nbits_total`
        // too, mirroring `RangeDecoder::read_raw_bits`.
        self.nbits_total += bits;
    }

    /// §5.1.4: encode one of `ft` equiprobable values.
    pub fn encode_uint(&mut self, t: u32, ft: u32) {
        if ft < 2 {
            return;
        }
        let ftb = ilog(ft - 1);
        if ftb <= 8 {
            self.encode(t, t + 1, ft);
        } else {
            let shift = ftb - 8;
            let top_ft = ((ft - 1) >> shift) + 1;
            let top = t >> shift;
            self.encode(top, top + 1, top_ft);
            self.write_raw_bits(t & ((1 << shift) - 1), shift);
        }
    }

    /// §5.1.5: choose `end` with maximal trailing zeros in `[val, val+rng)`,
    /// flush it through the carry buffer, then assemble the output:
    /// range-coded bytes first, raw bits at the end (reverse production
    /// order), matching the frame layout the decoder expects.
    pub fn done(mut self) -> Vec<u8> {
        let range_end = self.val as u64 + self.rng as u64 - 1;
        let mut b: u32 = 0;
        loop {
            if b >= 31 {
                break;
            }
            let two_b = 1u64 << (b + 1);
            let e = ((self.val as u64) + two_b - 1) & !(two_b - 1);
            if e + two_b - 1 <= range_end {
                b += 1;
            } else {
                break;
            }
        }
        let two_b = 1u64 << b;
        let mut end = ((self.val as u64) + two_b - 1) & !(two_b - 1);

        // `l` = number of significant bits in `end` (31 - b trailing
        // zeros), rounded up to a whole number of bytes below. Reference
        // `ec_enc_done` drives this loop by a bit counter, not by `end`
        // becoming zero: a byte whose bits happen to all be zero (e.g. the
        // low byte of a 9-significant-bit `end`) still must be emitted,
        // since the decoder's `tell()`-derived budget already counted it.
        // Terminating early on `end == 0` silently drops that trailing
        // zero byte and under-produces relative to every other Opus
        // decoder/encoder.
        let mut l = 31i32 - b as i32;
        while l > 0 {
            self.carry_out((end >> 23) as u32);
            end = (end << 8) & 0x7FFF_FFFF;
            l -= 8;
        }
        if (self.rem != 0 && self.rem != -1) || self.ext > 0 {
            self.carry_out(0); // flush: 9 zero bits
        }
        if self.rem != -1 {
            self.out.push(self.rem as u8);
        }

        // Flush any partial raw-bit byte, then append raw bits so that
        // end_bytes[0] lands last in the frame.
        if self.nend_bits > 0 {
            self.end_bytes.push(self.end_window as u8);
            self.end_window = 0;
            self.nend_bits = 0;
        }
        for byte in self.end_bytes.iter().rev() {
            self.out.push(*byte);
        }
        self.out
    }

    /// Whole-bit usage, mirroring the decoder.
    pub fn tell(&self) -> u32 {
        self.nbits_total - ilog(self.rng)
    }

    /// `ec_tell_frac`: bit usage in 1/8-bit units, mirroring
    /// [`RangeDecoder::tell_frac`].
    pub fn tell_frac(&self) -> u32 {
        static CORRECTION: [u32; 8] = [35733, 38967, 42495, 46340, 50535, 55109, 60097, 65535];
        let nbits = self.nbits_total << 3;
        let l = ilog(self.rng);
        let r = self.rng >> (l - 16);
        let mut b = (r >> 12) - 8;
        b += u32::from(r > CORRECTION[b as usize]);
        nbits - ((l << 3) + b)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn xorshift(seed: &mut u64) -> u64 {
        *seed ^= *seed << 13;
        *seed ^= *seed >> 7;
        *seed ^= *seed << 17;
        *seed
    }

    #[test]
    fn roundtrip_uniform_symbols() {
        let mut rng_state = 1u64;
        let mut enc = RangeEncoder::new();
        let mut symbols = Vec::new();
        for _ in 0..500 {
            let sym = (xorshift(&mut rng_state) % 4) as u32;
            enc.encode(sym, sym + 1, 4);
            symbols.push(sym);
        }
        let frame = enc.done();

        let mut dec = RangeDecoder::new(&frame);
        for &sym in &symbols {
            let fs = dec.decode(4).unwrap();
            assert!(fs < 4);
            dec.update(fs, fs + 1, 4);
            assert_eq!(fs, sym);
        }
    }

    /// `RangeEncoder::tell_frac` must track `RangeDecoder::tell_frac`
    /// step-by-step on the same bitstream (needed by any encode-side code
    /// mirroring the decoder's bit-budget accounting, e.g. `compute_theta`).
    #[test]
    fn tell_frac_matches_decoder_at_every_step() {
        let mut rng_state = 42u64;
        let mut enc = RangeEncoder::new();
        let mut symbols = Vec::new();
        let mut enc_tells = Vec::new();
        for _ in 0..100 {
            let sym = (xorshift(&mut rng_state) % 4) as u32;
            enc.encode(sym, sym + 1, 4);
            symbols.push(sym);
            enc_tells.push(enc.tell_frac());
        }
        let frame = enc.done();

        let mut dec = RangeDecoder::new(&frame);
        for (i, &sym) in symbols.iter().enumerate() {
            let fs = dec.decode(4).unwrap();
            dec.update(fs, fs + 1, 4);
            assert_eq!(fs, sym);
            assert_eq!(dec.tell_frac(), enc_tells[i], "step {i}");
        }
    }

    #[test]
    fn roundtrip_bit_logp() {
        for logp in [1u32, 2, 3, 4, 7, 11, 15] {
            let mut rng_state = logp as u64 + 9;
            let mut enc = RangeEncoder::new();
            let mut bits = Vec::new();
            for _ in 0..200 {
                let bit = xorshift(&mut rng_state) & 1 == 1;
                enc.encode_bit_logp(bit, logp);
                bits.push(bit);
            }
            let frame = enc.done();
            let mut dec = RangeDecoder::new(&frame);
            for &bit in &bits {
                assert_eq!(dec.decode_bit_logp(logp).unwrap(), bit);
            }
        }
    }

    #[test]
    fn roundtrip_icdf() {
        // Zero-terminated icdf table over ftb = 8: frequencies
        // {64, 32, 16, 16, 32, 64, 32}/256.
        let icdf: [u8; 8] = [192, 160, 144, 128, 96, 32, 0, 0];
        let mut rng_state = 42u64;
        let mut enc = RangeEncoder::new();
        let mut symbols = Vec::new();
        for _ in 0..500 {
            let k = (xorshift(&mut rng_state) % 6) as u32;
            enc.encode_icdf(k, &icdf, 8);
            symbols.push(k);
        }
        let frame = enc.done();
        let mut dec = RangeDecoder::new(&frame);
        for &k in &symbols {
            assert_eq!(dec.decode_icdf(&icdf, 8).unwrap(), k);
        }
    }

    #[test]
    fn roundtrip_uint_then_raw_bits() {
        let mut rng_state = 7u64;
        let mut enc = RangeEncoder::new();
        let mut values = Vec::new();
        for _ in 0..200 {
            let ft = 1 + (xorshift(&mut rng_state) % 100_000) as u32;
            let t = (xorshift(&mut rng_state) % ft as u64) as u32;
            enc.encode_uint(t, ft);
            values.push((t, ft));
        }
        let mut raw = Vec::new();
        for bits in [1u32, 3, 8, 13, 24] {
            let v = xorshift(&mut rng_state) as u32 & ((1u32 << bits) - 1);
            enc.write_raw_bits(v, bits);
            raw.push((v, bits));
        }
        let frame = enc.done();

        let mut dec = RangeDecoder::new(&frame);
        for &(t, ft) in &values {
            assert_eq!(dec.decode_uint(ft).unwrap(), t);
        }
        for &(v, bits) in &raw {
            assert_eq!(dec.read_raw_bits(bits), v);
        }
    }

    #[test]
    fn tell_matches_between_coder_halves() {
        let mut enc = RangeEncoder::new();
        for _ in 0..50 {
            enc.encode_bit_logp(true, 3);
        }
        let tell_enc = enc.tell();
        let frame = enc.done();
        let mut dec = RangeDecoder::new(&frame);
        for _ in 0..50 {
            let _ = dec.decode_bit_logp(3).unwrap();
        }
        assert_eq!(dec.tell(), tell_enc);
    }
}
