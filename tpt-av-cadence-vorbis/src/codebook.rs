//! Vorbis codebooks: packed header decode (spec section 3.2.1), Huffman
//! codeword decode, and the VQ lookup tables.
//!
//! Codewords are assigned canonically in entry order ("lowest valued unused
//! binary Huffman codeword", leftmost bit = MSb). Decode walks an explicit
//! binary tree built once at setup; every structure is preallocated.

use crate::bitreader::BitReader;
use tpt_av_cadence_core::CadenceError;

fn corrupt(what: &str) -> CadenceError {
    CadenceError::CorruptData(format!("vorbis codebook: {what}"))
}

/// Huffman decode tree: internal nodes with two children; leaves carry the
/// entry number. Node 0 is the root.
struct Tree {
    /// Child node indices; `-1` = empty branch; leaf marker uses
    /// `LEAF_MASK | entry` in `left` with `right = -2` sentinel.
    left: Box<[i32]>,
    right: Box<[i32]>,
    used: usize,
}

const LEAF_MASK: i32 = 1 << 30;

impl Tree {
    fn with_capacity(nodes: usize) -> Self {
        Tree {
            left: vec![-1; nodes].into_boxed_slice(),
            right: vec![-1; nodes].into_boxed_slice(),
            used: 1,
        }
    }

    /// Assigns the leftmost unused codeword of length `len` to `entry`
    /// (spec: each entry, in order, takes "the lowest valued unused binary
    /// Huffman codeword possible"). Returns the codeword, MSb-aligned in
    /// `len` bits.
    fn assign_leftmost(&mut self, len: u32, entry: usize) -> Result<u32, CadenceError> {
        // DFS in strict left-first order: empty slots are frames too, so an
        // empty slot under an earlier branch is always reached before any
        // later branch's subtree.
        enum Frame {
            /// Existing internal node: (node, code, depth).
            Node(usize, u32, u32),
            /// Empty slot: (parent, bit, code, depth-of-slot).
            Empty(usize, bool, u32, u32),
        }
        let mut stack: Vec<Frame> = vec![Frame::Node(0, 0, 0)];
        while let Some(frame) = stack.pop() {
            match frame {
                Frame::Node(node, code, depth) => {
                    for branch in (0..2).rev() {
                        let bit = branch != 0;
                        let child_code = (code << 1) | branch as u32;
                        let child = if bit { self.right[node] } else { self.left[node] };
                        if child == -1 {
                            stack.push(Frame::Empty(node, bit, child_code, depth + 1));
                        } else if (child & LEAF_MASK) == 0 && depth + 1 < len {
                            stack.push(Frame::Node(child as usize, child_code, depth + 1));
                        }
                    }
                }
                Frame::Empty(parent, bit, code, depth) => {
                    // The leftmost free slot: codeword = path so far padded
                    // with zeros to `len` bits.
                    let final_code = code << (len - depth);
                    if depth == len {
                        // The empty slot is the leaf itself.
                        if bit {
                            self.right[parent] = LEAF_MASK | entry as i32;
                        } else {
                            self.left[parent] = LEAF_MASK | entry as i32;
                        }
                    } else {
                        // Create the slot node below `parent`, then zero
                        // edges down to the leaf.
                        let nn = self.alloc_node()?;
                        if bit {
                            self.right[parent] = nn as i32;
                        } else {
                            self.left[parent] = nn as i32;
                        }
                        let mut cur = nn;
                        for _ in 0..(len - depth - 1) {
                            let nx = self.alloc_node()?;
                            self.left[cur] = nx as i32;
                            cur = nx;
                        }
                        // Leaf in the parent's edge slot; the leaf node's own
                        // child slots stay empty (they are unreachable).
                        self.left[cur] = LEAF_MASK | entry as i32;
                    }
                    return Ok(final_code);
                }
            }
        }
        Err(corrupt("no free codeword (overspecified huffman tree)"))
    }

    fn alloc_node(&mut self) -> Result<usize, CadenceError> {
        let idx = self.used;
        if idx >= self.left.len() {
            return Err(corrupt("huffman tree overflow"));
        }
        self.used += 1;
        Ok(idx)
    }
}

/// A decoded Vorbis codebook.
pub struct Codebook {
    pub dimensions: usize,
    pub entries: usize,
    /// Per-entry codeword lengths (0 = unused/sparse entry).
    lengths: Box<[u8]>,
    lookup_type: u8,
    /// Flattened VQ value vectors, `entry * dimensions` scalars, for lookup
    /// types 1 and 2.
    values: Box<[f32]>,
    tree: Option<Tree>,
    /// Single-entry books decode as one bit + the lone entry (spec erratum
    /// 20150226).
    single_entry: Option<usize>,
    max_depth: u32,
}

/// Hard cap on one codebook's value table (real books are far below this).
const MAX_VALUES: usize = 32 << 20;

impl Codebook {
    /// Parses one codebook from the setup header, starting after the sync
    /// pattern has been verified by the caller.
    pub fn parse(br: &mut BitReader) -> Result<Codebook, CadenceError> {
        let _version = br.read_bits(16)?;
        let dimensions = br.read_bits(16)? as usize;
        let entries = br.read_bits(24)? as usize;
        if entries == 0 || entries > (1 << 24) {
            return Err(corrupt("entry count out of range"));
        }
        let ordered = br.read_bit()?;
        let mut lengths = vec![0u8; entries].into_boxed_slice();

        if ordered {
            let mut current_entry = 0usize;
            let mut current_length = br.read_bits(5)? as usize + 1;
            while current_entry < entries {
                let number = br
                    .read_bits(BitReader::ilog((entries - current_entry) as i64))?
                    as usize;
                if current_entry + number > entries {
                    return Err(corrupt("ordered length overruns entries"));
                }
                if current_length > u8::MAX as usize {
                    return Err(corrupt("codeword length overflow"));
                }
                for len in lengths[current_entry..current_entry + number].iter_mut() {
                    *len = current_length as u8;
                }
                current_entry += number;
                current_length += 1;
            }
        } else {
            let sparse = br.read_bit()?;
            for len in lengths.iter_mut() {
                if sparse {
                    if br.read_bit()? {
                        *len = br.read_bits(5)? as u8 + 1;
                    }
                } else {
                    *len = br.read_bits(5)? as u8 + 1;
                }
            }
        }

        let lookup_type = br.read_bits(4)? as u8;
        let mut values: Box<[f32]> = Box::default();
        match lookup_type {
            0 => {}
            1 | 2 => {
                let minimum = float32_unpack(br.read_bits(32)?);
                let delta = float32_unpack(br.read_bits(32)?);
                let value_bits = br.read_bits(4)? as usize + 1;
                let sequence_p = br.read_bit()?;
                let lookup_values = if lookup_type == 1 {
                    lookup1_values(entries, dimensions)
                } else {
                    entries * dimensions
                };
                if lookup_values.checked_mul(value_bits).unwrap_or(u32::MAX as usize) > (1 << 31) {
                    return Err(corrupt("lookup size overflows the packet"));
                }
                if lookup_values * dimensions > MAX_VALUES {
                    return Err(corrupt("lookup value table exceeds budget"));
                }
                let mut multiplicands = Vec::with_capacity(lookup_values.min(1 << 20));
                for _ in 0..lookup_values {
                    multiplicands.push(br.read_bits(value_bits as u32)? as f32);
                }
                values = vec![0.0f32; entries * dimensions].into_boxed_slice();
                for entry in 0..entries {
                    if lengths[entry] == 0 {
                        continue;
                    }
                    let mut last = 0.0f32;
                    for i in 0..dimensions {
                        let multiplicand = if lookup_type == 1 {
                            // (entry / lookup_values^i) % lookup_values
                            let mut divisor = 1usize;
                            for _ in 0..i {
                                divisor = divisor.saturating_mul(lookup_values.max(1));
                            }
                            let multiplicand_offset = (entry / divisor) % lookup_values.max(1);
                            multiplicands[multiplicand_offset]
                        } else {
                            multiplicands[entry * dimensions + i]
                        };
                        let value = multiplicand * delta + minimum + last;
                        values[entry * dimensions + i] = value;
                        if sequence_p {
                            last = value;
                        }
                    }
                }
            }
            _ => return Err(corrupt("reserved lookup type")),
        }

        // Build the Huffman tree over the used entries.
        let mut single_entry = None;
        let mut used_count = 0usize;
        let mut lone = 0usize;
        for (e, &l) in lengths.iter().enumerate() {
            if l > 0 {
                used_count += 1;
                lone = e;
            }
        }
        if used_count == 1 {
            if lengths[lone] != 1 {
                return Err(corrupt("single-entry codebook length must be 1"));
            }
            single_entry = Some(lone);
        }

        let mut tree = None;
        let mut max_depth = 1u32;
        if used_count > 1 {
            // Kraft check: sum 2^(32-len) must equal 2^32 (complete tree).
            let mut kraft: u64 = 0;
            for &l in lengths.iter() {
                if l > 0 {
                    kraft += 1u64 << (32 - l);
                }
            }
            if kraft != 1 << 32 {
                return Err(corrupt("underspecified or overspecified huffman tree"));
            }
            let mut t = Tree::with_capacity(used_count * 2 + 2);
            for (e, &l) in lengths.iter().enumerate() {
                if l == 0 {
                    continue;
                }
                t.assign_leftmost(l as u32, e)?;
                max_depth = max_depth.max(l as u32);
            }
            tree = Some(t);
        }

        Ok(Codebook {
            dimensions,
            entries,
            lengths,
            lookup_type,
            values,
            tree,
            single_entry,
            max_depth,
        })
    }

    /// Reads one codeword and returns the entry number (scalar context).
    pub fn read_scalar(&self, br: &mut BitReader) -> Result<usize, CadenceError> {
        if let Some(entry) = self.single_entry {
            let _ = br.read_bit()?; // sink one bit; value tolerated as 0 or 1
            return Ok(entry);
        }
        let tree = self.tree.as_ref().ok_or_else(|| corrupt("empty codebook read"))?;
        let mut node = 0usize;
        for _ in 0..=self.max_depth {
            let bit = br.read_bit()?;
            let next = if bit { tree.right[node] } else { tree.left[node] };
            if next < 0 {
                return Err(corrupt("invalid codeword"));
            }
            if (next & LEAF_MASK) != 0 {
                return Ok((next & !LEAF_MASK) as usize);
            }
            node = next as usize;
        }
        Err(corrupt("codeword exceeds maximum depth"))
    }

    /// Reads one codeword and returns its VQ value vector (VQ context).
    pub fn read_vector<'v>(&self, br: &mut BitReader, out: &'v mut [f32]) -> Result<(), CadenceError> {
        let entry = self.read_scalar(br)?;
        if self.lookup_type == 0 {
            return Err(corrupt("VQ context on a book without a lookup"));
        }
        let src = &self.values[entry * self.dimensions..(entry + 1) * self.dimensions];
        out[..self.dimensions].copy_from_slice(src);
        Ok(())
    }

    pub fn has_lookup(&self) -> bool {
        self.lookup_type != 0
    }

    /// Codeword length of an entry (0 = unused); test support.
    #[cfg(test)]
    pub fn length_of(&self, entry: usize) -> u8 {
        self.lengths[entry]
    }
}

/// The spec's `float32_unpack`: sign-mantissa-exponent packing with an
/// exponent bias of 788 (mantissa holds the implied leading bit).
fn float32_unpack(bits: u32) -> f32 {
    let mantissa = (bits & 0x1f_ffff) as i64;
    let sign = bits & 0x8000_0000;
    let exponent = ((bits & 0x7fe0_0000) >> 21) as i64;
    let mantissa = if sign != 0 { -mantissa } else { mantissa };
    ((mantissa as f64) * (2.0f64).powi((exponent - 788) as i32)) as f32
}

/// Greatest integer `v` with `v^dim <= entries` (spec 9.2.3).
fn lookup1_values(entries: usize, dim: usize) -> usize {
    if dim == 0 {
        return 0;
    }
    let mut r = (entries as f64).powf(1.0 / dim as f64).floor() as usize;
    while r.checked_pow(dim as u32).map_or(false, |p| p > entries) {
        r -= 1;
    }
    while (r + 1)
        .checked_pow(dim as u32)
        .map_or(false, |p| p <= entries)
    {
        r += 1;
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Packs a bit list LSB-first into packet bytes (spec section 2).
    fn pack(bits: &[u32]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut acc = 0u32;
        let mut n = 0u32;
        for &b in bits {
            acc |= b << n;
            n += 1;
            if n == 8 {
                out.push(acc as u8);
                acc = 0;
                n = 0;
            }
        }
        if n > 0 {
            out.push(acc as u8);
        }
        out
    }

    fn bits_of(value: u32, count: u32) -> Vec<u32> {
        (0..count).map(|i| (value >> i) & 1).collect()
    }

    #[test]
    fn parses_handcrafted_sparse_book_and_decodes() {
        // dimensions=2, entries=4, sparse, lengths [2, 0(unused), 2, 2];
        // lookup 0 (none). Canonical codewords: e0=00, e2=01, e3=1? — per
        // assignment: e0 len2 -> 00, e2 len2 -> 01, e3 len1 -> 1.
        let mut bits = Vec::new();
        bits.extend(bits_of(0, 16)); // version
        bits.extend(bits_of(2, 16)); // dimensions
        bits.extend(bits_of(4, 24)); // entries
        bits.push(0); // ordered = 0
        bits.push(1); // sparse
        bits.push(1); // entry0 used
        bits.extend(bits_of(1, 5)); // length 2
        bits.push(0); // entry1 unused
        bits.push(1); // entry2 used
        bits.extend(bits_of(1, 5)); // length 2
        bits.push(1); // entry3 used
        bits.extend(bits_of(0, 5)); // length 1
        bits.extend(bits_of(0, 4)); // lookup type 0
        let data = pack(&bits);
        let mut br = BitReader::new(&data);
        let cb = Codebook::parse(&mut br).unwrap();
        assert_eq!(cb.dimensions, 2);
        assert_eq!(cb.length_of(0), 2);
        assert_eq!(cb.length_of(1), 0);
        assert_eq!(cb.length_of(3), 1);

        // Decode: bits "00" -> 0, "1" -> 3, "01" -> 2.
        let dec = |bits: &[u32]| {
            let data = pack(bits);
            let mut br = BitReader::new(&data);
            cb.read_scalar(&mut br)
        };
        assert_eq!(dec(&[0, 0]).unwrap(), 0);
        assert_eq!(dec(&[1]).unwrap(), 3);
        assert_eq!(dec(&[0, 1]).unwrap(), 2);
    }

    #[test]
    fn lookup_type1_values_follow_the_spec_formula() {
        // dimensions=2, entries=4, ordered lengths [1,2,3,3]... keep sparse
        // simple: lengths [2,2,2,2]; lookup 1: min=-1.0, delta=0.5, value
        // bits=3, sequence_p=0; lookup1_values(4,2) = 2; multiplicands 0..3.
        let mut bits = Vec::new();
        bits.extend(bits_of(0, 16));
        bits.extend(bits_of(2, 16));
        bits.extend(bits_of(4, 24));
        bits.push(0); // not ordered
        bits.push(0); // not sparse
        for _ in 0..4 {
            bits.extend(bits_of(1, 5)); // length 2
        }
        bits.extend(bits_of(1, 4)); // lookup type 1
        // float32_unpack: value = mant * 2^(exp-788), mant is a 21-bit
        // integer. -1.0: mant = 1<<20, exp = 768, sign set.
        let f = |sign: u32, mant: u32, exp: u32| sign | (exp << 21) | mant;
        bits.extend(bits_of(f(0x8000_0000, 1 << 20, 768), 32));
        // delta 0.5: mant = 1<<19, exp = 768.
        bits.extend(bits_of(f(0, 1 << 19, 768), 32));
        bits.extend(bits_of(2, 4)); // value bits = 3
        bits.push(0); // sequence_p = 0
        for m in 0..2u32 {
            // lookup_values = 2
            bits.extend(bits_of(m, 3));
        }
        let data = pack(&bits);
        let mut br = BitReader::new(&data);
        let cb = Codebook::parse(&mut br).unwrap();
        // entry 0 (codeword 00): offsets (0/1)%2=0 -> m0, (0/2)%2=0 -> m0
        let cw = pack(&[0, 0]);
        let mut br = BitReader::new(&cw);
        let mut vec = vec![0.0f32; 2];
        cb.read_vector(&mut br, &mut vec).unwrap();
        assert_eq!(vec, [0.0 * 0.5 - 1.0, 0.0 * 0.5 - 1.0]);
        // entry 2 (codeword 10): (2/1)%2=0 -> m0, (2/2)%2=1 -> m1
        let cw = pack(&[1, 0]);
        let mut br = BitReader::new(&cw);
        let mut vec = vec![0.0f32; 2];
        cb.read_vector(&mut br, &mut vec).unwrap();
        assert_eq!(vec, [-1.0, 1.0 * 0.5 - 1.0]);
    }

    #[test]
    fn single_entry_book_sinks_one_bit() {
        let mut bits = Vec::new();
        bits.extend(bits_of(0, 16));
        bits.extend(bits_of(3, 16));
        bits.extend(bits_of(1, 24));
        bits.push(0); // not ordered
        bits.push(0); // not sparse
        bits.extend(bits_of(0, 5)); // length 1
        bits.extend(bits_of(0, 4));
        let data = pack(&bits);
        let mut br = BitReader::new(&data);
        let cb = Codebook::parse(&mut br).unwrap();
        let codeword = pack(&[1]);
        let mut br = BitReader::new(&codeword);
        assert_eq!(cb.read_scalar(&mut br).unwrap(), 0);
    }

    #[test]
    fn spec_example_non_monotonic_lengths_assign_leftmost() {
        // The spec's example: lengths [2,4,4,4,4,2,3,3] produce codewords
        // e0=00 e1=0100 e2=0101 e3=0110 e4=0111 e5=10 e6=110 e7=111 (read
        // MSb first).
        let lengths = [2u32, 4, 4, 4, 4, 2, 3, 3];
        let mut bits = Vec::new();
        bits.extend(bits_of(0, 16));
        bits.extend(bits_of(1, 16));
        bits.extend(bits_of(8, 24));
        bits.push(0); // not ordered
        bits.push(0); // not sparse
        for &l in &lengths {
            bits.extend(bits_of(l - 1, 5));
        }
        bits.extend(bits_of(0, 4));
        let data = pack(&bits);
        let mut br = BitReader::new(&data);
        let cb = Codebook::parse(&mut br).unwrap();

        let decode = |code_bits: &[u32]| -> usize {
            let data = pack(code_bits);
            let mut br = BitReader::new(&data);
            cb.read_scalar(&mut br).unwrap()
        };
        assert_eq!(decode(&[0, 0]), 0);
        assert_eq!(decode(&[0, 1, 0, 0]), 1);
        assert_eq!(decode(&[0, 1, 0, 1]), 2);
        assert_eq!(decode(&[0, 1, 1, 0]), 3);
        assert_eq!(decode(&[0, 1, 1, 1]), 4);
        assert_eq!(decode(&[1, 0]), 5);
        assert_eq!(decode(&[1, 1, 0]), 6);
        assert_eq!(decode(&[1, 1, 1]), 7);
    }
}
