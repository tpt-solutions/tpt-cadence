//! LSB-first bit reader for Vorbis packets (spec section 2).
//!
//! Vorbis packs bits into packets least-significant-bit first within each
//! octet, and multi-bit integers are stored least-significant-bit first in
//! that bit stream. All reads are bounds-checked; reading past the end of the
//! packet returns [`CadenceError::CorruptData`] (the spec's "end-of-packet"
//! condition) and never panics.

use tpt_av_cadence_core::CadenceError;

/// End-of-packet read error.
fn overread() -> CadenceError {
    CadenceError::CorruptData("vorbis packet overread".to_string())
}

/// LSB-first bit reader over a completed packet.
pub struct BitReader<'a> {
    data: &'a [u8],
    /// Next bit to serve, counted from the start of `data`.
    bit_pos: usize,
}

impl<'a> BitReader<'a> {
    /// Wraps a packet's bytes.
    pub fn new(data: &'a [u8]) -> Self {
        BitReader { data, bit_pos: 0 }
    }

    /// Number of bits still readable.
    pub fn bits_left(&self) -> usize {
        self.data.len() * 8 - self.bit_pos.min(self.data.len() * 8)
    }

    /// Bits consumed so far (spec `get_bits_count`).
    pub fn bits_read(&self) -> usize {
        self.bit_pos
    }

    /// Reads `n` bits (0..=32) as an unsigned integer, LSB-first.
    pub fn read_bits(&mut self, n: u32) -> Result<u32, CadenceError> {
        debug_assert!(n <= 32);
        if n == 0 {
            return Ok(0);
        }
        if self.bits_left() < n as usize {
            self.bit_pos = self.data.len() * 8;
            return Err(overread());
        }
        let mut value: u64 = 0;
        let mut taken = 0u32;
        // Consume whole bytes while available.
        while taken < n {
            let byte = self.data[self.bit_pos / 8];
            let off = (self.bit_pos % 8) as u32;
            let avail = 8 - off;
            let want = (n - taken).min(avail);
            let piece = (byte >> off) as u64 & ((1u16 << want) - 1) as u64;
            value |= piece << taken;
            self.bit_pos += want as usize;
            taken += want;
        }
        Ok(value as u32)
    }

    /// Reads a single bit.
    pub fn read_bit(&mut self) -> Result<bool, CadenceError> {
        Ok(self.read_bits(1)? != 0)
    }

    /// Reads `n` bits (0..=64) as an unsigned integer, LSB-first.
    pub fn read_bits64(&mut self, n: u32) -> Result<u64, CadenceError> {
        debug_assert!(n <= 64);
        if n <= 32 {
            return Ok(self.read_bits(n)? as u64);
        }
        let low = self.read_bits(32)? as u64;
        let high = self.read_bits(n - 32)? as u64;
        Ok(low | (high << 32))
    }

    /// The spec's `ilog(x)`: position of the highest set bit (0 for x <= 0).
    pub fn ilog(x: i64) -> u32 {
        if x <= 0 {
            return 0;
        }
        (64 - (x as u64).leading_zeros()).max(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_lsb_first_across_bytes() {
        // Spec section 2.1.6 coding example: 0b011 formed from bytes below.
        let data = [0b1011_0100, 0b1110_0101];
        let mut br = BitReader::new(&data);
        assert_eq!(br.read_bits(3).unwrap(), 0b100);
        assert_eq!(br.read_bits(5).unwrap(), 0b10110);
        assert_eq!(br.read_bits(8).unwrap(), 0b1110_0101);
        assert!(br.read_bits(1).is_err());
    }

    #[test]
    fn reads_wide_values() {
        let data = [0xff, 0x00, 0xab, 0xcd, 0xef, 0x01, 0x23, 0x45, 0x67];
        let mut br = BitReader::new(&data);
        assert_eq!(br.read_bits(16).unwrap(), 0x00ff);
        let v = br.read_bits64(48).unwrap();
        assert_eq!(v, 0x4523_01ef_cdab);
    }

    #[test]
    fn zero_bit_read_is_a_noop() {
        let mut br = BitReader::new(&[0x5a]);
        assert_eq!(br.read_bits(0).unwrap(), 0);
        assert_eq!(br.bits_left(), 8);
    }

    #[test]
    fn ilog_matches_spec_examples() {
        assert_eq!(BitReader::ilog(0), 0);
        assert_eq!(BitReader::ilog(1), 1);
        assert_eq!(BitReader::ilog(2), 2);
        assert_eq!(BitReader::ilog(3), 2);
        assert_eq!(BitReader::ilog(4), 3);
        assert_eq!(BitReader::ilog(7), 3);
        assert_eq!(BitReader::ilog(-5), 0);
    }
}
