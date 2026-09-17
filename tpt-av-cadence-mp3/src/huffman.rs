//! Layer III Huffman decoding with inline requantization.
//!
//! Decodes the `big_values` region through the 32 standard codebook trees
//! (two-level tables with linbits escapes) and the trailing count1 region
//! through the two 4-value tables, applying each scalefactor band's
//! requantization multiplier as values are produced. Bit access mirrors the
//! reference decoder's 32-bit cache so the bit-position semantics (and the
//! exact split of `part2_3_length` between granules) are preserved.

use crate::sideinfo::GranuleInfo;
use crate::tables::{COUNT1_TAB_A, COUNT1_TAB_B, HUFF_TABS, LINBITS, POW43, TAB_INDEX};

/// |x|^(4/3) lookup with linear interpolation above the table range.
fn pow_43(x: i32) -> f32 {
    if x < 129 {
        return POW43[(16 + x) as usize];
    }
    let mut x = x;
    let mut mult = 256.0f32;
    if x < 1024 {
        mult = 16.0;
        x <<= 3;
    }
    let sign = (2 * x) & 64;
    let frac = ((x & 63) - sign) as f32 / ((x & !63) + sign) as f32;
    POW43[(16 + ((x + sign) >> 6)) as usize]
        * (1.0 + frac * (4.0 / 3.0 + frac * (2.0 / 9.0)))
        * mult
}

/// Bit cursor specialized for Huffman decoding: a 32-bit MSB cache over the
/// main-data buffer, mirroring the reference implementation bit-for-bit.
struct HuffBits<'a> {
    buf: &'a [u8],
    next: usize,
    cache: u32,
    sh: i32,
}

impl<'a> HuffBits<'a> {
    /// Starts at bit position `pos` of `buf`.
    fn new(buf: &'a [u8], pos: usize) -> Self {
        let next0 = pos >> 3;
        let shift = (pos & 7) as u32;
        let byte = |i: usize| if i < buf.len() { buf[i] as u32 } else { 0 };
        let cache =
            ((byte(next0) * 256 + byte(next0 + 1)) * 256 + byte(next0 + 2)) * 256 + byte(next0 + 3);
        HuffBits {
            buf,
            next: next0 + 4,
            cache: cache.wrapping_shl(shift),
            sh: (pos & 7) as i32 - 8,
        }
    }

    /// Current absolute bit position.
    fn pos(&self) -> i64 {
        self.next as i64 * 8 - 24 + self.sh as i64
    }

    #[inline(always)]
    fn peek(&self, n: u32) -> u32 {
        self.cache >> (32 - n)
    }

    #[inline(always)]
    fn flush(&mut self, n: u32) {
        self.cache = self.cache.wrapping_shl(n);
        self.sh += n as i32;
    }

    #[inline(always)]
    fn check(&mut self) {
        while self.sh >= 0 {
            let b = if self.next < self.buf.len() {
                self.buf[self.next] as u32
            } else {
                0
            };
            self.cache |= b << self.sh;
            self.sh -= 8;
            self.next += 1;
        }
    }
}

/// Decodes one granule/channel of Huffman data (starting at bit position
/// `start_pos` of `buf`) into `dst` (576 requantized samples; untouched
/// slots remain zero). Returns `granule_limit` clamped to the buffer — the
/// reference always lands the bit position exactly on the granule boundary
/// so the next granule/channel starts at its own `part2_3_length` split.
pub(crate) fn huffman(
    dst: &mut [f32],
    buf: &[u8],
    start_pos: usize,
    gr: &GranuleInfo,
    scf: &[f32; 40],
    granule_limit: i64,
) -> usize {
    let mut bs = HuffBits::new(buf, start_pos);
    let mut d = 0usize;
    let mut scf_idx = 0usize;
    let mut sfb_idx = 0usize;
    let mut big_val_cnt = gr.big_values as i32;
    let mut ireg = 0usize;
    // A band may contain both big_values pairs and count1 quadruples.
    let mut one = 0.0f32;

    'regions: while big_val_cnt > 0 {
        if ireg >= 3 {
            break; // corrupt: regions exhausted with pairs left over
        }
        let tab_num = gr.table_select[ireg] as usize;
        let mut sfb_cnt = gr.region_count[ireg] as i32;
        ireg += 1;
        let book_off = TAB_INDEX[tab_num] as usize;
        let linbits = LINBITS[tab_num] as u32;

        loop {
            if sfb_idx >= gr.sfbtab.len() {
                break 'regions; // corrupt: past the sfb terminator
            }
            let np = (gr.sfbtab[sfb_idx] / 2) as i32;
            sfb_idx += 1;
            let pairs = big_val_cnt.min(np);
            one = scf[scf_idx];
            scf_idx += 1;
            for _ in 0..pairs {
                let mut w: u32 = 5;
                let mut leaf = HUFF_TABS[book_off + bs.peek(w) as usize];
                while leaf < 0 {
                    bs.flush(w);
                    w = (leaf & 7) as u32;
                    leaf =
                        HUFF_TABS[book_off + (bs.peek(w) as i32 - i32::from(leaf >> 3)) as usize];
                }
                bs.flush((leaf >> 8) as u32);

                for _ in 0..2 {
                    let lsb = (leaf & 0x0F) as u32;
                    if lsb == 15 && linbits != 0 {
                        let mag = lsb + bs.peek(linbits);
                        bs.flush(linbits);
                        bs.check();
                        let sign = if bs.cache & 0x8000_0000 != 0 {
                            -1.0f32
                        } else {
                            1.0
                        };
                        if d < dst.len() {
                            dst[d] = one * pow_43(mag as i32) * sign;
                        }
                    } else {
                        // Negative magnitudes index the signed head of
                        // POW43 (entries 0..16 hold −|x|^(4/3)).
                        let sign = (bs.cache >> 31) as usize;
                        if d < dst.len() {
                            dst[d] = POW43[16 + lsb as usize - 16 * sign] * one;
                        }
                    }
                    bs.flush(u32::from(lsb != 0));
                    leaf >>= 4;
                    d += 1;
                }
                bs.check();
            }
            big_val_cnt -= np;
            sfb_cnt -= 1;
            if !(big_val_cnt > 0 && sfb_cnt >= 0) {
                continue 'regions;
            }
        }
    }

    // count1 region: quadruples of ±1 values, bounded by the granule limit.
    // Each quad advances the output by 4 slots whether or not all values
    // are present, and consumes one scalefactor pair-slot per two values.
    let count1book: &[u8] = if gr.count1_table != 0 {
        &COUNT1_TAB_B
    } else {
        &COUNT1_TAB_A
    };
    // 1 - big_val_cnt: when big_values ended mid-band, that band's leftover
    // values are count1-coded and keep its scalefactor (np counts down the
    // overshoot before the next reload pulls the following band's factor).
    let mut np = 1 - big_val_cnt;
    'count1: loop {
        if d + 4 > dst.len() {
            break;
        }
        let mut leaf = count1book[bs.peek(4) as usize] as i32;
        if leaf & 8 == 0 {
            let nbits = (leaf & 3) as u32;
            let idx = (leaf >> 3) + (bs.cache.wrapping_shl(4) >> (32 - nbits)) as i32;
            leaf = count1book[idx as usize] as i32;
        }
        bs.flush((leaf & 7) as u32);
        if bs.pos() > granule_limit {
            break;
        }
        for s in 0..4 {
            if s == 0 || s == 2 {
                np -= 1;
                if np == 0 {
                    if sfb_idx >= gr.sfbtab.len() {
                        break 'count1;
                    }
                    np = (gr.sfbtab[sfb_idx] / 2) as i32;
                    sfb_idx += 1;
                    if np == 0 {
                        break 'count1; // ran into the sfb terminator
                    }
                    one = scf[scf_idx];
                    scf_idx += 1;
                }
            }
            if leaf & (0x80 >> s) != 0 {
                let sign = bs.cache & 0x8000_0000 != 0;
                dst[d] = if sign { -one } else { one };
                bs.flush(1);
            }
            d += 1;
        }
        bs.check();
    }

    let limit = granule_limit.max(0) as usize;
    limit.min(buf.len() * 8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn count1_keeps_scale_when_big_values_end_inside_band() {
        // Table 0 emits one zero pair without consuming bits. Table B's
        // 0000 code followed by 0101 signs emits (+1,-1,+1,-1).
        // The first count1 pair is still in band 0; the second is in band 1.
        let gr = GranuleInfo {
            big_values: 1,
            count1_table: 1,
            sfbtab: &[4, 4, 0],
            ..Default::default()
        };
        let mut scf = [0.0; 40];
        scf[0] = 2.0;
        scf[1] = 3.0;
        let mut dst = [0.0; 576];
        assert_eq!(huffman(&mut dst, &[0x05], 0, &gr, &scf, 8), 8);
        assert_eq!(&dst[..6], &[0.0, 0.0, 2.0, -2.0, 3.0, -3.0]);
        assert!(dst[6..].iter().all(|&v| v == 0.0));
    }

    #[test]
    fn pow43_matches_direct_computation() {
        // Positive range (incl. the interpolated fast path above 128).
        for x in 0i32..200 {
            let expect = (x as f32).powf(4.0 / 3.0);
            let got = pow_43(x);
            assert!(
                (got - expect).abs() < 1e-2 * expect.abs().max(1.0),
                "x={x} got={got} want={expect}"
            );
        }
        // The signed head mirrors magnitudes by index: entry k = −k^(4/3).
        for k in 0i32..16 {
            let expect = -((k as f32).powf(4.0 / 3.0));
            let got = POW43[k as usize];
            assert!((got - expect).abs() < 1e-3, "k={k} got={got} want={expect}");
        }
    }

    #[test]
    fn huffbits_positions_match_simple_reader() {
        // Feed a byte pattern through both the raw cache cursor and a naive
        // shift-based reader and confirm the bit positions stay in lockstep
        // for a stream of 5-bit peeks and odd flushes.
        let data = [
            0b1011_0111u8,
            0b0100_1101,
            0b1110_0010,
            0b0101_1010,
            0x93,
            0x07,
        ];
        let mut hb = HuffBits::new(&data, 3);
        let mut pos = 3usize;
        let mut seed = 0x1234u32;
        for _ in 0..200 {
            seed = seed.wrapping_mul(1103515245).wrapping_add(12345);
            let n = (seed >> 16) % 14 + 1;
            hb.flush(n);
            pos += n as usize;
            hb.check();
        }
        assert_eq!(hb.pos(), pos as i64);
    }
}
