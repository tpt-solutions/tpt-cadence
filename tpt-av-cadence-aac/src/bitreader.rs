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

    /// Restores an exact bit position (used to undo rounded-up reads: e.g.
    /// the FIL/SBR extension payload capture reads `payload_bits.div_ceil(8)`
    /// whole bytes — deliberately rounding up past the payload's real
    /// bit-exact length — then calls this to walk the logical position back
    /// to `payload_start + payload_bits`). That rounded-up capture can
    /// legitimately touch bits past the buffer's end when the payload sits
    /// within the last byte of the frame with little padding after it
    /// (common — real encoders don't pad extra bytes for this), which sets
    /// `overread` via `read_bits`. Since `set_pos` is exactly the mechanism
    /// that corrects a rounding overshoot back to a valid position, it must
    /// re-derive `overread` from the *restored* position rather than leave
    /// a stale `true` behind — otherwise a perfectly valid raw_data_block
    /// gets rejected as corrupt purely because of how one element's payload
    /// happened to be captured, not because anything was actually wrong.
    pub fn set_pos(&mut self, pos: usize) {
        self.pos = pos;
        self.overread = pos > self.bytes.len() * 8;
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

    /// `set_pos` must clear a stale `overread` flag when it walks the
    /// position back into bounds — this is exactly the FIL/SBR payload
    /// capture pattern (`decoder.rs`'s `br.set_pos(payload_start +
    /// payload_bits)`): it deliberately over-reads by rounding up to whole
    /// bytes, which can legitimately touch bits past the buffer's end when
    /// the payload sits within the buffer's last byte with little padding
    /// after it, then corrects the logical position back to something
    /// valid. Found via a real repro: a genuine libfdk-aac-encoded HE-AAC
    /// stream whose 48th ADTS frame's SBR extension element left br.pos()
    /// exactly 3 bits short of the frame's true end after the rounded-up
    /// byte capture — a perfectly valid frame that this bug rejected as
    /// corrupt.
    #[test]
    fn set_pos_clears_a_stale_overread_from_a_rounded_up_capture() {
        // 16 bits total. Simulate reading 3 whole bytes (24 bits) to
        // capture a value that's really only, say, 13 bits — the
        // rounding-up read touches 8 bits past the 16-bit buffer, setting
        // `overread`.
        let mut br = BitReader::new(&[0xFF, 0xFF]);
        br.read_bits(24);
        assert!(br.overread());
        // Restore the true logical position (13 bits, well within bounds).
        br.set_pos(13);
        assert!(
            !br.overread(),
            "set_pos must clear overread when the restored position is in bounds"
        );
        // set_pos to a position genuinely past the end must still flag it.
        br.set_pos(17);
        assert!(br.overread());
    }
}
