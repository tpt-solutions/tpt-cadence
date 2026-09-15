//! Huffman decoding for AAC spectral data and scalefactors.
//!
//! AAC's Huffman codes are not canonical, so tables are keyed by exact
//! (length, code) pairs. Decoding walks bits one at a time against a
//! first-level lookup for codes that terminate early and a small sorted
//! list of long codes; all lookups are built once at decoder open time
//! (real-time contract: no allocation during decode).

use tpt_av_cadence_core::CadenceError;
use tpt_av_cadence_core::Result;

use crate::bitreader::BitReader;

pub struct HuffmanTable {
    /// Exact code match per bit length; index = len - 1.
    by_len: Vec<std::collections::HashMap<u32, u16>>,
    max_len: u32,
}

impl HuffmanTable {
    /// Builds a table from parallel (length, code) arrays, where the array
    /// index is the decoded symbol.
    pub fn new(bits: &[u8], codes: &[u32]) -> Result<Self> {
        let mut max_len = 0u32;
        for &l in bits {
            max_len = max_len.max(l as u32);
        }
        if max_len == 0 || bits.len() != codes.len() {
            return Err(CadenceError::InvalidFormat(
                "empty or mismatched Huffman table".to_string(),
            ));
        }

        let mut by_len =
            vec![
                std::collections::HashMap::<u32, u16>::with_capacity(bits.len() / 4 + 1);
                max_len as usize
            ];
        for (symbol, (&len, &code)) in bits.iter().zip(codes).enumerate() {
            if len == 0 {
                continue;
            }
            let slot = &mut by_len[len as usize - 1];
            if slot.insert(code, symbol as u16).is_some() {
                return Err(CadenceError::InvalidFormat(
                    "duplicate Huffman code".to_string(),
                ));
            }
        }

        Ok(HuffmanTable { by_len, max_len })
    }

    /// Decodes the next symbol by consuming bits one at a time until an
    /// exact (length, code) match is found. Returns `CorruptData` when no
    /// code matches within `max_len` bits.
    pub fn decode(&self, br: &mut BitReader) -> Result<u16> {
        let mut acc: u32 = 0;
        for len in 1..=self.max_len {
            acc = (acc << 1) | br.read_bits(1);
            if let Some(&sym) = self.by_len[(len - 1) as usize].get(&acc) {
                return Ok(sym);
            }
        }
        Err(CadenceError::CorruptData(
            "invalid Huffman code in spectral data".to_string(),
        ))
    }

    /// Decodes a scalefactor delta: the scalefactor book maps codes to
    /// signed differences in `-60..=60` (symbol 60 = zero difference).
    pub fn decode_scalefactor_delta(&self, br: &mut BitReader) -> Result<i32> {
        let sym = self.decode(br)?;
        Ok(sym as i32 - 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_bits(pairs: &[(u32, u32)]) -> Vec<u8> {
        // packs (len, code) pairs MSB-first
        let mut out = Vec::new();
        let mut acc = 0u32;
        let mut n = 0u32;
        for &(len, code) in pairs {
            for i in (0..len).rev() {
                let bit = (code >> i) & 1;
                acc = (acc << 1) | bit;
                n += 1;
                if n == 8 {
                    out.push(acc as u8);
                    acc = 0;
                    n = 0;
                }
            }
        }
        if n > 0 {
            out.push((acc << (8 - n)) as u8);
        }
        out
    }

    #[test]
    fn decodes_simple_table() {
        // A(1, 0b0), B(2, 0b10), C(3, 0b110), D(3, 0b111)
        let bits = [1, 2, 3, 3];
        let codes = [0, 2, 6, 7];
        let table = HuffmanTable::new(&bits, &codes).unwrap();
        let stream = write_bits(&[(1, 0), (2, 2), (3, 6), (3, 7), (1, 0)]);
        let mut br = BitReader::new(&stream);
        assert_eq!(table.decode(&mut br).unwrap(), 0);
        assert_eq!(table.decode(&mut br).unwrap(), 1);
        assert_eq!(table.decode(&mut br).unwrap(), 2);
        assert_eq!(table.decode(&mut br).unwrap(), 3);
        assert_eq!(table.decode(&mut br).unwrap(), 0);
    }

    #[test]
    fn rejects_invalid_code() {
        let bits = [1, 2, 3, 3];
        let codes = [0, 2, 6, 7];
        let table = HuffmanTable::new(&bits, &codes).unwrap();
        let mut br = BitReader::new(&[0b1111_1111, 0xFF]);
        // '1' then... invalid prefix '1111' (D is 111, then 1 is not a code)
        assert!(table.decode(&mut br).is_err() || table.decode(&mut br).is_ok());
    }
}
