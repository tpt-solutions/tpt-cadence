//! MSB-first bit reader for AAC bitstream elements.
//!
//! Mirrors the reference decoder's `get_bits` semantics: reads past the end
//! of the input yield zero bits (the frame is corrupt and will be rejected
//! by structure checks), so parsing is panic-free by construction.

pub struct BitReader<'a> {
    bytes: &'a [u8],
    /// Bit position.
    pos: usize,
    overread: bool,
}

impl<'a> BitReader<'a> {
    pub fn new(bytes: &'a [u8]) -> Self {
        BitReader {
            bytes,
            pos: 0,
            overread: false,
        }
    }

    /// Number of bits consumed so far.
    pub fn pos(&self) -> usize {
        self.pos
    }

    /// Whether any read has gone past the end of the input.
    pub fn overread(&self) -> bool {
        self.overread
    }

    /// Reads `n` bits (n <= 32), MSB first.
    pub fn read_bits(&mut self, n: u32) -> u32 {
        debug_assert!(n <= 32);
        let mut value: u32 = 0;
        for _ in 0..n {
            let byte = self.pos >> 3;
            let bit = if byte < self.bytes.len() {
                (self.bytes[byte] >> (7 - (self.pos & 7))) & 1
            } else {
                self.overread = true;
                0
            };
            value = (value << 1) | bit as u32;
            self.pos += 1;
        }
        value
    }

    /// Reads a single bit.
    pub fn read_bit(&mut self) -> bool {
        self.read_bits(1) != 0
    }

    /// Reads `n` bits as a two's-complement signed value.
    pub fn read_signed(&mut self, n: u32) -> i32 {
        let raw = self.read_bits(n);
        if n == 0 {
            return 0;
        }
        if raw & (1 << (n - 1)) != 0 {
            (raw as i64 - (1i64 << n)) as i32
        } else {
            raw as i32
        }
    }

    /// Skips to the next byte boundary.
    pub fn byte_align(&mut self) {
        self.pos = (self.pos + 7) & !7;
    }

    /// Restores an exact bit position (used to undo rounded-up reads).
    pub fn set_pos(&mut self, pos: usize) {
        self.pos = pos;
    }

    /// Skips `n` bits (used by declared-but-unsupported field lists).
    pub fn skip_bits(&mut self, n: usize) {
        self.pos += n;
        if self.pos >> 3 > self.bytes.len() {
            self.overread = true;
        }
    }

    /// Skips `n` bytes (used by fill/data-stream elements).
    pub fn skip_bytes(&mut self, n: usize) {
        self.pos += n * 8;
        if self.pos >> 3 > self.bytes.len() {
            self.overread = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_msb_first() {
        let mut br = BitReader::new(&[0b1011_1000, 0b0100_0001]);
        assert_eq!(br.read_bits(3), 0b101);
        assert_eq!(br.read_bits(4), 0b1100);
        assert!(!br.read_bit());
        assert!(!br.read_bit());
        assert_eq!(br.read_bits(4), 0b1000);
        assert_eq!(br.pos(), 13);
    }

    #[test]
    fn overread_yields_zeros() {
        let mut br = BitReader::new(&[0xFF]);
        assert_eq!(br.read_bits(4), 0x0F);
        // Real bits are served first; only past-the-end bits are zeros.
        assert_eq!(br.read_bits(12), 0b1111_0000_0000);
        assert!(br.overread());
    }

    #[test]
    fn signed_values() {
        let mut br = BitReader::new(&[0xFF, 0x00]);
        assert_eq!(br.read_signed(8), -1);
        assert_eq!(br.read_signed(8), 0);
    }
}
