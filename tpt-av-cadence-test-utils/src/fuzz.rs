//! Deterministic helpers for never-panic property and mutation fuzz tests.
//!
//! These complement `cargo-fuzz` targets (in the workspace `fuzz/` crate) for
//! use inside ordinary `cargo test` runs: a reproducible PRNG plus simple
//! structure-aware byte mutations. Every parser in the suite must survive
//! arbitrary input without panicking.

/// SplitMix64 PRNG — tiny, deterministic, and good enough for test mutations.
/// The same seed always produces the same stream, so failures reproduce.
pub struct Rng(u64);

impl Rng {
    /// Creates a generator from a seed.
    pub fn new(seed: u64) -> Self {
        Rng(seed)
    }

    /// Creates a generator from a constant; convenient in tests.
    pub fn from_name(name: &str) -> Self {
        let mut h: u64 = 0xcbf29ce484222325;
        for b in name.bytes() {
            h ^= b as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
        Rng(h)
    }

    /// Next raw 64-bit value.
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E3779B97F4A7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
        z ^ (z >> 31)
    }

    /// Uniform value in `0..bound` (`bound` must be non-zero).
    pub fn below(&mut self, bound: usize) -> usize {
        (self.next_u64() % bound as u64) as usize
    }

    /// Fills `out` with pseudo-random bytes.
    pub fn fill_bytes(&mut self, out: &mut [u8]) {
        for chunk in out.chunks_mut(8) {
            let v = self.next_u64().to_le_bytes();
            let n = chunk.len().min(8);
            chunk[..n].copy_from_slice(&v[..n]);
        }
    }
}

/// Applies one random mutation to `data` and returns the result.
///
/// Strategies cover the classic parser-killers: truncation at an arbitrary
/// point, single-bit flips, byte substitutions, insertion, and block
/// duplication/shifts.
pub fn mutate(data: &[u8], rng: &mut Rng) -> Vec<u8> {
    if data.is_empty() {
        return vec![rng.next_u64() as u8];
    }
    match rng.below(5) {
        // Truncate at a random point.
        0 => data[..rng.below(data.len())].to_vec(),
        // Flip one bit.
        1 => {
            let mut out = data.to_vec();
            let i = rng.below(out.len());
            out[i] ^= 1 << rng.below(8);
            out
        }
        // Substitute a run of up to 8 bytes.
        2 => {
            let mut out = data.to_vec();
            let run = (1 + rng.below(8)).min(out.len());
            let start = rng.below(out.len() - run + 1);
            for b in &mut out[start..start + run] {
                *b = rng.next_u64() as u8;
            }
            out
        }
        // Insert up to 16 random bytes at a random point.
        3 => {
            let mut out = data.to_vec();
            let at = rng.below(out.len() + 1);
            let n = 1 + rng.below(16);
            let ins: Vec<u8> = (0..n).map(|_| rng.next_u64() as u8).collect();
            out.splice(at..at, ins);
            out
        }
        // Duplicate/shift a block.
        _ => {
            let mut out = data.to_vec();
            let start = rng.below(out.len());
            let end = (start + 1 + rng.below(64)).min(out.len());
            let block: Vec<u8> = out[start..end].to_vec();
            let at = rng.below(out.len() + 1);
            out.splice(at..at, block);
            out
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rng_is_deterministic() {
        let mut a = Rng::new(42);
        let mut b = Rng::new(42);
        for _ in 0..16 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
    }

    #[test]
    fn rng_from_name_is_stable() {
        let mut a = Rng::from_name("wav-fuzz");
        let mut b = Rng::from_name("wav-fuzz");
        assert_eq!(a.next_u64(), b.next_u64());
    }

    #[test]
    fn mutate_on_empty_input_is_fine() {
        let mut rng = Rng::new(7);
        let out = mutate(&[], &mut rng);
        assert_eq!(out.len(), 1);
    }
}
