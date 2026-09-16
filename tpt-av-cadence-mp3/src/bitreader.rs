//! MSB-first bit reader over an in-memory frame, with minimp3-compatible
//! overread semantics: a read that would pass `limit` returns zero bits while
//! still advancing the position, so corrupt frames degrade to silence instead
//! of panicking.

/// Bit cursor over a byte slice. `pos` and `limit` are in bits.
pub(crate) struct BitReader<'a> {
    buf: &'a [u8],
    pos: usize,
    limit: usize,
}

impl<'a> BitReader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        BitReader {
            buf,
            pos: 0,
            limit: buf.len() * 8,
        }
    }

    pub fn bit_pos(&self) -> usize {
        self.pos
    }

    pub fn limit_bits(&self) -> usize {
        self.limit
    }

    pub fn set_bit_pos(&mut self, pos: usize) {
        self.pos = pos;
    }

    /// Reads `n` bits MSB-first (n <= 25). Returns 0 on overread, mirroring
    /// the reference decoder: the position advances regardless, so callers
    /// that check `bit_pos()` still make progress.
    pub fn get_bits(&mut self, n: u32) -> u32 {
        debug_assert!(n <= 25);
        let shift = (self.pos & 7) as u32;
        let shl = n + shift;
        let mut p = self.pos >> 3;
        self.pos += n as usize;
        if self.pos > self.limit || p >= self.buf.len() {
            return 0;
        }
        let mut cache: u32 = 0;
        let mut next = (self.buf[p] as u32) & (0xFF >> shift);
        p += 1;
        let mut rem = shl as i32;
        while rem - 8 > 0 {
            rem -= 8;
            cache |= next << rem;
            next = if p < self.buf.len() {
                self.buf[p] as u32
            } else {
                0
            };
            p += 1;
        }
        cache | (next >> (8 - rem as u32))
    }

    /// Reads a single bit.
    pub fn get_bit(&mut self) -> u32 {
        self.get_bits(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_msb_first_fields() {
        let br = &mut BitReader::new(&[0b1011_0111, 0b0100_0001]);
        assert_eq!(br.get_bits(4), 0b1011);
        assert_eq!(br.get_bits(3), 0b011);
        assert_eq!(br.get_bit(), 1);
        assert_eq!(br.get_bits(4), 0b0100);
        assert_eq!(br.get_bits(2), 0b00);
        assert_eq!(br.get_bits(2), 0b01);
    }

    #[test]
    fn unaligned_fields_span_bytes() {
        // 0x12 0x34 -> bits 00010010 00110100
        let br = &mut BitReader::new(&[0x12, 0x34]);
        assert_eq!(br.get_bits(3), 0b000);
        assert_eq!(br.get_bits(9), 0b100100011);
        assert_eq!(br.get_bits(4), 0b0100);
    }

    #[test]
    fn overread_returns_zeros_but_advances() {
        let br = &mut BitReader::new(&[0xFF]);
        assert_eq!(br.get_bits(8), 0xFF);
        assert_eq!(br.get_bits(8), 0);
        assert_eq!(br.bit_pos(), 16);
    }
}
