//! MPEG-1 Layer III (MP3) encoder.
//!
//! Scope of the current implementation (see `todo.md` for the full
//! rationale and session history):
//!
//! - **MPEG-1 only** (32/44100/48000 Hz), a single fixed CBR bitrate per
//!   stream, **long blocks only** (no block-switching / short blocks), and
//!   no bit-reservoir borrowing across frames (`main_data_begin = 0`,
//!   spec-legal; within a frame, the last granule/channel inherits the
//!   whole frame's unspent bit remainder — the intra-frame equivalent of
//!   reservoir borrowing).
//! - **Full Huffman machinery**: encode tables for all 32 big_values books
//!   mechanically derived from the decoder's own tables (bit-identical to
//!   FFmpeg's canonical code assignment — verified table-by-table), a
//!   three-region exhaustive region/book split, and both count1 quadruple
//!   tables with mid-band `big_values` continuation.
//! - **Two-loop quantizer structure** (ISO/LAME style): an inner
//!   global-gain rate loop (finest gain that fits the slot budget) and an
//!   outer psychoacoustic loop that amplifies the worst band per
//!   scalefactor unit. The outer loop's machinery — ISO model I-style
//!   masking thresholds (spreading function, ATH, tonality), per-band
//!   scalefactor amplification, and `scalefac_compress` selection — is
//!   implemented and unit-tested, but amplification is currently disabled
//!   (`PSY_AMPLIFICATION_ROUNDS = 0`): with it active, some frames
//!   disagreed with FFmpeg's decode of the same bytes at 26–43 dB (our own
//!   decoder round-trips them exactly). Investigation continues; see
//!   `todo.md`.
//! - **Mid/side stereo**, decided per frame (joint stereo + mode_ext bit 2)
//!   when the side channel carries less than half the mid-channel energy:
//!   `M = (L+R)·2^-3/2`, `S = (L−R)·2^-3/2`, which combined with the
//!   decoder's ms requant gain (√2) and `m+s`/`m−s` reconstruction yields
//!   exactly L and R. Verified against FFmpeg at 113.8 dB inter-decoder
//!   agreement on dual-mono noise.
//! - **Subband splitting uses the ISO reference's published 512-tap
//!   polyphase analysis filter** (`tables::ANALYSIS_WINDOW`), verified
//!   against the live `shine` encoder source line by line. The per-band
//!   36-point forward MDCT is the analytic adjoint of this crate's decoder
//!   IMDCT, derived algebraically and checked in this module's tests. The
//!   encoder pre-compensates for the decoder's unconditional
//!   `antialias`/`change_sign` post-processing.
//!
//! Output is spec-compliant Layer III: valid headers, side info,
//! scalefactors, and Huffman-coded data; it decodes cleanly in this
//! crate's own decoder and in FFmpeg (see `tests/encoder_ffmpeg_crosscheck.rs`,
//! whose bitrate ladder requires ≥100 dB agreement between FFmpeg's decode
//! and ours on every standard bitrate in mono and stereo). Known open
//! quality items — tonal-material inter-decoder disagreement at low
//! bitrates and the disabled psychoacoustic amplification — are tracked in
//! `todo.md` and `tests/encoder_ffmpeg_crosscheck.rs`'s ignored regression
//! test.

use std::io::Write;

use tpt_av_cadence_core::{CadenceError, Encoder, Result};

use crate::header;
use crate::imdct;
use crate::scalefac::ldexp_q2;
use crate::sideinfo;
use crate::tables::{HUFF_TABS, LINBITS, TAB_INDEX};

/// Samples per MPEG-1 frame (2 granules of 576).
const FRAME_SAMPLES: usize = 1152;
/// Samples per granule.
const GRANULE_SAMPLES: usize = 576;
/// Long-block scalefactor bands (fixed: MPEG-1 always has 22).
const N_LONG_SFB: usize = 22;

// ---------------------------------------------------------------------------
// Huffman encode tables, mechanically derived from the decoder's own
// `HUFF_TABS` (see `crate::tables`) by walking the exact same two-level
// lookup automaton `crate::huffman::huffman` uses, in the forward direction.
// Reusing the decoder's tables (rather than transcribing a second,
// independent copy of the ISO code tables) guarantees the two stay in sync
// by construction: whatever `HUFF_TABS` says a codeword decodes to, this
// walk finds the same codeword for that same value.
// ---------------------------------------------------------------------------

/// One Huffman table's encode side: `codes[x * 16 + y] = (code, code_len)`.
/// The decoder consumes a leaf's low nibble as `x`, then shifts and consumes
/// the high nibble as `y`; this storage mirrors that order.
type HuffCode = (u16, u8);

/// Walks the flat two-level table starting at `book_off` (see
/// `crate::huffman::huffman`'s identical traversal) to recover every
/// reachable `(x, y) -> (code, len)` mapping.
fn build_huff_table(book_off: i32) -> [HuffCode; 256] {
    let mut out = [(0u16, 0u8); 256];
    walk(book_off, book_off, 5, 0, 0, &mut out);
    out
}

fn walk(
    book_off: i32,
    base: i32,
    w: u32,
    len_so_far: u32,
    prefix_so_far: u32,
    out: &mut [HuffCode; 256],
) {
    for v in 0..(1u32 << w) {
        let idx = (base + v as i32) as usize;
        if idx >= HUFF_TABS.len() {
            continue;
        }
        let e = HUFF_TABS[idx] as i32;
        if e >= 0 {
            let l = (e >> 8) as u32;
            if l == 0 || l > len_so_far + w {
                continue;
            }
            let full_prefix = (prefix_so_far << w) | v;
            let real_len = len_so_far + l;
            let real_code = full_prefix >> (w - l);
            let x = (e & 0xF) as usize;
            let y = ((e >> 4) & 0xF) as usize;
            out[x * 16 + y] = (real_code as u16, real_len as u8);
        } else {
            let w2 = (e & 7) as u32;
            let bias = e >> 3;
            let next_base = book_off - bias;
            walk(
                book_off,
                next_base,
                w2,
                len_so_far + w,
                (prefix_so_far << w) | v,
                out,
            );
        }
    }
}

/// The full encode side of one big_values Huffman book (table_select 0..=31):
/// `codes[x * 16 + y] = (code, code_len)`, the book's escape width, and the
/// largest magnitude any pair in the book can carry.
struct BookTable {
    codes: [HuffCode; 256],
    linbits: u32,
    /// Largest `|value|` a pair encoded through this book can represent:
    /// the walked table's max magnitude, extended by the escape range when
    /// the book has linbits and a `(15, y)`/`(x, 15)` leaf exists.
    max_mag: u32,
    /// `false` for the two ISO-unassigned book numbers (4 and 14), whose
    /// `TAB_INDEX` slots point at an all-zero placeholder with no codewords.
    usable: bool,
}

impl BookTable {
    fn new(book: usize) -> Self {
        let codes = build_huff_table(TAB_INDEX[book] as i32);
        let linbits = LINBITS[book] as u32;
        let mut max_mag = 0u32;
        for x in 0..16u32 {
            for y in 0..16u32 {
                let (_, len) = codes[(x * 16 + y) as usize];
                if len == 0 {
                    continue;
                }
                max_mag = max_mag.max(x.max(y));
            }
        }
        // Escape-capable books: any `(15, y)` leaf extends to
        // `15 + 2^linbits - 1` for that slot's magnitude. (The decoder reads
        // linbits whenever a nibble is 15 and the book has linbits, and books
        // 16..=31 — the only linbits books — all contain 15-leaves; this is
        // asserted by `all_books_round_trip_extreme_magnitudes`.)
        if linbits > 0 && max_mag >= 15 {
            max_mag = 15 + (1u32 << linbits) - 1;
        }
        BookTable {
            codes,
            linbits,
            max_mag,
            usable: max_mag > 0 || codes[0].1 > 0,
        }
    }
}

/// Lazily-built, process-wide encode tables for all 32 big_values books
/// (pure functions of the decoder's own constant tables, so a single shared
/// instance is safe and avoids rebuilding per granule).
fn books() -> &'static [BookTable; 32] {
    static BOOKS: std::sync::OnceLock<[BookTable; 32]> = std::sync::OnceLock::new();
    BOOKS.get_or_init(|| std::array::from_fn(BookTable::new))
}

/// Encode side of one count1 quadruple book (count1table_select 0/1):
/// `codes[v0<<3 | v1<<2 | v2<<1 | v3] = (code, code_len)`, sign bits
/// appended separately per nonzero value. Derived by walking the exact
/// two-level automaton `crate::huffman::huffman` uses for the count1 region
/// (a 4-bit peek, then — when the first-level leaf's bit 3 is clear — a
/// `leaf & 3`-bit suffix indexing the second level; the *consumed* length is
/// always just the final leaf's `& 7`).
struct Count1Book {
    codes: [HuffCode; 16],
}

/// Extracts a count1 leaf's quadruple as the nibble
/// `v0<<3 | v1<<2 | v2<<1 | v3` from bits 7..4 (`crate::huffman::huffman`
/// tests `leaf & (0x80 >> s)` per value slot).
fn nibbles_of(leaf: u32) -> u32 {
    ((leaf >> 7) & 1) << 3 | ((leaf >> 6) & 1) << 2 | ((leaf >> 5) & 1) << 1 | ((leaf >> 4) & 1)
}

impl Count1Book {
    fn new(table: &[u8]) -> Self {
        let mut codes = [(0u16, 0u8); 16];
        for peek in 0..16u32 {
            let leaf = table[peek as usize] as u32;
            if leaf & 8 != 0 {
                let len = leaf & 7;
                debug_assert!(len <= 4);
                // The codeword is the first `len` bits of the peeked window
                // (`len` may be shorter than the 4-bit peek).
                let quad = nibbles_of(leaf);
                codes[quad as usize] = ((peek >> (4 - len)) as u16, len as u8);
            } else {
                let nbits = leaf & 3;
                for suffix in 0..(1u32 << nbits) {
                    let leaf2 = table[((leaf >> 3) + suffix) as usize] as u32;
                    let len = leaf2 & 7;
                    debug_assert!(len <= 4 + nbits);
                    // Codeword = first `len` bits of `peek` + `suffix`; any
                    // peeked bits beyond `len` belong to the next codeword
                    // and must be dropped, or the leading zeros of the peek
                    // would be lost when the value is written `len` wide.
                    let full = (peek << nbits) | suffix;
                    let code = full >> (4 + nbits - len);
                    let quad = nibbles_of(leaf2);
                    let slot = &mut codes[quad as usize];
                    // Two peek paths can reach the same second-level leaf
                    // when the leaf's length is shorter than the peeked
                    // window; both paths then share one codeword.
                    if slot.1 == 0 || slot.1 as u32 > len {
                        *slot = (code as u16, len as u8);
                    }
                }
            }
        }
        Count1Book { codes }
    }
}

fn count1_books() -> &'static [Count1Book; 2] {
    static BOOKS: std::sync::OnceLock<[Count1Book; 2]> = std::sync::OnceLock::new();
    BOOKS.get_or_init(|| {
        [
            Count1Book::new(&crate::tables::COUNT1_TAB_A),
            Count1Book::new(&crate::tables::COUNT1_TAB_B),
        ]
    })
}

// ---------------------------------------------------------------------------
// MSB-first bit writer (same convention as the FLAC/AIFF/WAV encoders).
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

    fn align(&mut self) {
        while self.bit_pos % 8 != 0 {
            self.push(0, 1);
        }
    }
}

// ---------------------------------------------------------------------------
// Subband analysis (windowed-sinc polyphase filter) and the
// per-band forward MDCT (the analytic adjoint of `crate::imdct`'s inverse).
// ---------------------------------------------------------------------------

/// Analysis history ring buffer length: the ISO reference's 512-tap
/// prototype filter length (`HAN_SIZE` in the reference/`shine` source).
const HAN_SIZE: usize = 512;

/// Splits 32 new PCM samples (plus the 480 carried from previous calls, in
/// the circular history buffer `x_hist`/`off`) into 32 subband values via
/// the ISO reference's actual 512-tap analysis prototype filter
/// (`tables::ANALYSIS_WINDOW`), folded into 64 partial sums by the standard
/// `sum_{m=0..7} x[i+64m] * C[i+64m]` identity, then matrixed by *exactly*
/// the ISO reference's analysis/synthesis modulation formula
/// `cos((2k+1)*(i-16)*pi/64)`. This is a mechanical port of the reference
/// algorithm (see e.g. the `shine` encoder's `shine_window_filter_subband`,
/// which follows the same ISO pseudocode: a 512-sample circular buffer
/// advanced by `+480 mod 512` per call so the newest 32 samples always
/// land at `off`, folded, then matrixed) rather than a numerically
/// extracted approximation — `tables::ANALYSIS_WINDOW` is the same
/// published constant table essentially every MP3 encoder embeds (LAME's
/// `enwindow`, the ISO reference's `Ci` table, `shine`'s
/// `shine_enwindow`), so this is provably the correct matched pair for
/// this crate's decoder's synthesis filter (`crate::synth`), which
/// implements the ISO *synthesis* side of the same standard filterbank.
fn analyze_block_polyphase(
    x_hist: &mut [f32; HAN_SIZE],
    off: &mut usize,
    new_samples: &[f32; 32],
    out: &mut [f32; 32],
) {
    let win = &crate::tables::ANALYSIS_WINDOW;

    // `shine_window_filter_subband` receives the 32 PCM samples in forward
    // order, then fills the circular buffer backwards:
    // `x[off + 31] = sample[0]` through `x[off] = sample[31]`. This ordering
    // is essential to the prototype filter's phase, not an implementation
    // detail that can be normalized away later.
    for (i, &sample) in new_samples.iter().enumerate() {
        x_hist[(*off + (31 - i)) % HAN_SIZE] = sample;
    }

    let mut y = [0.0f64; 64];
    for (i, yi) in y.iter_mut().enumerate() {
        let mut acc = 0.0f64;
        for m in 0..8usize {
            let idx = (*off + i + 64 * m) % HAN_SIZE;
            acc += x_hist[idx] as f64 * win[i + 64 * m];
        }
        *yi = acc;
    }

    *off = (*off + 480) % HAN_SIZE;

    for (k, o) in out.iter_mut().enumerate() {
        let mut acc = 0.0f64;
        for (i, &yi) in y.iter().enumerate() {
            let c =
                (std::f64::consts::PI / 64.0 * (2.0 * k as f64 + 1.0) * (i as f64 - 16.0)).cos();
            acc += yi * c;
        }
        *o = acc as f32;
    }
}

// ---------------------------------------------------------------------------
// Forward 36-point MDCT, derived algebraically from the decoder's own
// `crate::imdct::imdct_gr` rather than a hand-guessed closed-form kernel.
//
// `imdct_gr`'s fast DCT-9-based algorithm is an optimized implementation of
// *some* standard windowed MDCT + 50%-overlap-add, but its exact kernel and
// normalization are not obvious from the butterfly code, and (as
// `imdct::tests::imdct36_matches_naive_spec_kernel` shows when actually run
// with assertions — it only prints, it never asserts) neither closed-form
// candidate anyone left in that test module actually matches it. Guessing a
// third candidate risks the same failure mode silently.
//
// Reading `imdct36`'s source directly instead: for one band, it builds two
// 9-vectors `co0`/`si0` by a fixed permutation of the 18 spectral inputs,
// runs each through `dct3_9` (confirmed by direct probing, see
// `imdct::probe_dct3_9`, to be exactly the textbook DCT-III
// `out[n] = sum_k in[k] * cos(pi/9 * k * (n+0.5))`), negates the odd `si`
// entries, then for `i` in `0..9` combines them with `TWID9` into two
// 9-vectors:
//
//   sum[i]   = co[i]*TWID9[9+i] + si[i]*TWID9[i]     (this call's own
//              contribution to *its own* read-out)
//   ownov[i] = co[i]*TWID9[i]   - si[i]*TWID9[9+i]   (this call's
//              contribution to the *next* call's read-out, stashed in the
//              overlap array exactly as `ownov[i]`)
//
// and finally windows/overlap-adds:
//
//   gb[i]    = ovl[i]*w1[i] - sum[i]*w2[i]
//   gb[17-i] = ovl[i]*w2[i] + sum[i]*w1[i]
//
// where `ovl` is the incoming overlap (the previous call's `ownov`) and
// `w1[i] = window[i]`, `w2[i] = window[9+i]` (the sine window has
// `w1[i]^2 + w2[i]^2 == 1`, checked in `mdct_window_is_normalized`).
//
// `X -> (sum, ownov)` (the whole `co0/si0` + `dct3_9` + `TWID9` chain) is an
// invertible 18x18 linear map `L` (DCT-III is invertible and the rest is a
// fixed permutation/sign pattern) — probed directly below rather than
// re-derived symbolically, since it's exactly what the production code
// computes. Given `L`, solving "which `X = A*hist + B*new` makes call N+1's
// read-out reproduce `new` exactly" reduces (per-`i` 2x2 rotation systems;
// worked out in the design notes, not reproduced here) to pure column
// scaling of `L^-1`:
//
//   B[:, i]      = L^-1[:, 9+i] * w1[i]       for i in 0..9
//   B[:, 17-i]   = L^-1[:, 9+i] * w2[i]       for i in 0..9
//   A[:, i]      = L^-1[:, i]   * (-w2[i])    for i in 0..9
//   A[:, 17-i]   = L^-1[:, i]   * w1[i]        for i in 0..9
//
// `mdct_basis_reconstructs_via_decoder_imdct` verifies the result against
// the real production `imdct_gr` path end to end.
// ---------------------------------------------------------------------------

type Mat18 = [[f64; 18]; 18];

fn mat18_zero() -> Mat18 {
    [[0.0; 18]; 18]
}

/// Gauss-Jordan inversion with partial pivoting. Panics if `m` is singular
/// (would indicate `imdct_gr`'s core transform is not invertible, i.e. a
/// bug in this derivation, not a case to handle gracefully).
#[allow(clippy::needless_range_loop)] // clearer as index math for 2D matrix ops
fn mat18_inverse(m: &Mat18) -> Mat18 {
    let mut a = *m;
    let mut inv = mat18_zero();
    for i in 0..18 {
        inv[i][i] = 1.0;
    }
    for col in 0..18 {
        let mut pivot = col;
        let mut best = a[col][col].abs();
        for row in (col + 1)..18 {
            if a[row][col].abs() > best {
                best = a[row][col].abs();
                pivot = row;
            }
        }
        assert!(
            best > 1e-9,
            "imdct_gr core transform L is singular at column {col}"
        );
        a.swap(pivot, col);
        inv.swap(pivot, col);
        let d = a[col][col];
        for j in 0..18 {
            a[col][j] /= d;
            inv[col][j] /= d;
        }
        for row in 0..18 {
            if row == col {
                continue;
            }
            let f = a[row][col];
            if f == 0.0 {
                continue;
            }
            for j in 0..18 {
                a[row][j] -= f * a[col][j];
                inv[row][j] -= f * inv[col][j];
            }
        }
    }
    inv
}

fn mdct_window_halves() -> ([f64; 9], [f64; 9]) {
    let window = &crate::tables::MDCT_WINDOW[0];
    let mut w1 = [0.0f64; 9];
    let mut w2 = [0.0f64; 9];
    for i in 0..9 {
        w1[i] = window[i] as f64;
        w2[i] = window[9 + i] as f64;
    }
    (w1, w2)
}

/// Probes the production `crate::imdct::imdct_gr` path (band 0 of a full
/// 32-band buffer; bands don't interact in this transform stage) to measure
/// `L`, the invertible 18x18 map `spectral -> (sum, ownov)` described in the
/// module comment. `sum[i]` is recovered from a single fresh call's
/// (`ovl == 0`) own read-out via `gb[i] = -sum[i]*w2[i]`; `ownov[i]` is
/// exactly what that same call leaves in the overlap array.
fn probe_l() -> Mat18 {
    let (_w1, w2) = mdct_window_halves();
    let mut l = mat18_zero();
    for k in 0..18 {
        let mut grbuf = [0.0f32; 576];
        let mut overlap = [0.0f32; 288];
        grbuf[k] = 1.0;
        imdct::imdct_gr(&mut grbuf, &mut overlap, 0, 32);
        for i in 0..9 {
            l[i][k] = -(grbuf[i] as f64) / w2[i];
            l[9 + i][k] = overlap[i] as f64;
        }
    }
    l
}

struct Mdct36Basis {
    a: Mat18, // multiplies the 18 history samples
    b: Mat18, // multiplies the 18 new samples
}

impl Mdct36Basis {
    fn new() -> Self {
        let l = probe_l();
        let l_inv = mat18_inverse(&l);
        let (w1, w2) = mdct_window_halves();

        let mut a = mat18_zero();
        let mut b = mat18_zero();
        for i in 0..9 {
            for row in 0..18 {
                let col_i = l_inv[row][i]; // L^-1[:, i]
                let col_9i = l_inv[row][9 + i]; // L^-1[:, 9+i]
                b[row][i] = col_9i * w1[i];
                b[row][17 - i] = col_9i * w2[i];
                a[row][i] = col_i * -w2[i];
                a[row][17 - i] = col_i * w1[i];
            }
        }
        Mdct36Basis { a, b }
    }

    #[allow(clippy::needless_range_loop)] // clearer as index math for 2D matrix ops
    fn forward(&self, x: &[f32; 36]) -> [f32; 18] {
        let mut out = [0.0f32; 18];
        for i in 0..18 {
            let mut acc = 0.0f64;
            for j in 0..18 {
                acc += self.a[i][j] * x[j] as f64;
                acc += self.b[i][j] * x[18 + j] as f64;
            }
            out[i] = acc as f32;
        }
        out
    }
}

fn mdct_basis() -> &'static Mdct36Basis {
    static BASIS: std::sync::OnceLock<Mdct36Basis> = std::sync::OnceLock::new();
    BASIS.get_or_init(Mdct36Basis::new)
}

/// Forward 36-point MDCT for one band: 36 time samples (18 carried from the
/// previous granule + 18 new) in, 18 spectral lines out, using the basis
/// derived by [`Mdct36Basis`].
fn forward_mdct36(x: &[f32; 36]) -> [f32; 18] {
    mdct_basis().forward(x)
}

// ---------------------------------------------------------------------------
// Quantization and bit allocation: per-scalefactor-band gains under
// psychoacoustic noise thresholds, with a global-gain "inner" rate loop and
// a scalefactor-amplification "outer" distortion loop — the classic two-loop
// Layer III structure (ISO/IEC 11172-3 Annex C, LAME's quantize.c), with
// the outer loop's masking thresholds supplied by the simplified Model-I
// style psychoacoustic estimate in this module.
// ---------------------------------------------------------------------------

/// `BITS_DEQUANTIZER_OUT` (−1): the decoder's global gain headroom fold, in
/// quarter-exponent units (mirrors `crate::scalefac`).
const BITS_DEQUANTIZER_OUT: i32 = -1;
/// `MAX_SCFI`: the decoder's rounded-up maximum scalefactor exponent.
const MAX_SCFI: i32 = (255 + BITS_DEQUANTIZER_OUT * 4 - 210 + 3) & !3;

/// Mirrors `crate::scalefac::decode_scalefactors`' base gain (`gain_exp`
/// includes the −2 quarter-unit mid/side shift the decoder applies to both
/// channels of an ms_stereo frame). *Increasing* in `global_gain`: a larger
/// global_gain is a larger dequantization multiplier, i.e. a coarser
/// quantizer, i.e. fewer bits.
fn granule_gain(global_gain: u8, ms_stereo: bool) -> f32 {
    let gain_exp = global_gain as i32 + BITS_DEQUANTIZER_OUT * 4 - 210 - (ms_stereo as i32) * 2;
    ldexp_q2((1 << (MAX_SCFI / 4)) as f32, MAX_SCFI - gain_exp)
}

/// Quantizes one spectral line to `|ix|` via the inverse of the decoder's
/// `X = scf * |ix|^(4/3) * sign(ix)` relation, clamped to the largest
/// magnitude any escape table can represent.
fn quantize_one(x: f32, gain: f32) -> u32 {
    if gain <= 0.0 || !x.is_finite() {
        return 0;
    }
    let ix = (x.abs() / gain).powf(0.75);
    if !ix.is_finite() {
        return 8206;
    }
    (ix.round() as i64).clamp(0, 8206) as u32
}

/// Per-sample-rate long-block scalefactor-band geometry: `widths[b]` lines
/// in band `b` (22 bands, all MPEG-1 long-block tables summing to 576),
/// `band_of_line[l]` so the quantizer can find each line's gain in O(1), and
/// `line_start[b]`/`line_end[b]` for per-band iteration.
struct BandLayout {
    widths: [u8; N_LONG_SFB],
    band_of_line: [u8; GRANULE_SAMPLES],
    line_start: [usize; N_LONG_SFB],
    line_end: [usize; N_LONG_SFB],
}

impl BandLayout {
    fn new(sr_table: usize) -> Self {
        let src = &crate::tables::SCF_LONG[sr_table];
        let mut widths = [0u8; N_LONG_SFB];
        widths.copy_from_slice(&src[..N_LONG_SFB]);
        let mut band_of_line = [0u8; GRANULE_SAMPLES];
        let mut line_start = [0usize; N_LONG_SFB];
        let mut line_end = [0usize; N_LONG_SFB];
        let mut line = 0usize;
        for band in 0..N_LONG_SFB {
            line_start[band] = line;
            for _ in 0..widths[band] {
                band_of_line[line] = band as u8;
                line += 1;
            }
            line_end[band] = line;
        }
        debug_assert_eq!(line, GRANULE_SAMPLES);
        BandLayout {
            widths,
            band_of_line,
            line_start,
            line_end,
        }
    }
}

/// Scalefactor widths `(slen1, slen2)` for a `scalefac_compress` value
/// (mirrors `crate::scalefac`'s MPEG-1 partition decode: slen1 covers
/// bands 0..=10, slen2 bands 11..=20).
fn slens(compress: u8) -> (u32, u32) {
    let part = crate::tables::SCFC_DECODE[compress as usize] as u32;
    (part >> 2, part & 3)
}

/// Total scalefactor bits a granule/channel spends at this compress value:
/// 11 values at slen1 (bands 0..=10) plus 10 at slen2 (bands 11..=20).
fn scalefac_bits(compress: u8) -> u64 {
    let (s1, s2) = slens(compress);
    11 * s1 as u64 + 10 * s2 as u64
}

/// Per-band requantization multipliers for a planned granule — an exact
/// mirror of `crate::scalefac::decode_scalefactors`' final loop for long
/// blocks at `scalefac_scale = 0`, including the preflag/pretab add-back and
/// the ms_stereo global-gain shift. `scalefacs` holds the *transmitted*
/// values; the effective band boost is `scalefacs[b]` plus the pretab when
/// `preflag` is set, exactly what the decoder applies.
fn band_gains(global_gain: u8, scalefacs: &[u8; 21], preflag: bool, ms_stereo: bool) -> [f32; 22] {
    let mut iscf = [0u8; 22];
    iscf[..21].copy_from_slice(scalefacs);
    if preflag {
        for (i, pre) in crate::tables::PREAMP.iter().enumerate() {
            iscf[11 + i] = iscf[11 + i].wrapping_add(*pre);
        }
    }
    let base = granule_gain(global_gain, ms_stereo);
    let mut out = [0.0f32; 22];
    for (i, slot) in out.iter_mut().enumerate() {
        *slot = ldexp_q2(base, (iscf[i] as i32) << 1);
    }
    out
}

/// Quantizes all 576 lines against per-band gains.
fn quantize_granule(
    spec: &[f32; GRANULE_SAMPLES],
    gains: &[f32; 22],
    band_of_line: &[u8; GRANULE_SAMPLES],
) -> [u32; GRANULE_SAMPLES] {
    let mut ix = [0u32; GRANULE_SAMPLES];
    for i in 0..GRANULE_SAMPLES {
        ix[i] = quantize_one(spec[i], gains[band_of_line[i] as usize]);
    }
    ix
}

/// Cost of one `(x, y)` pair through a big_values book, in bits including
/// escape bits and sign bits. `None` when the book cannot represent the
/// pair. Book 0 is the degenerate all-zero book: `(0, 0)` costs no bits at
/// all (the decoder's automaton falls straight through it).
fn book_pair_cost(book: &BookTable, x: u32, y: u32) -> Option<u32> {
    let (_, len) = book.codes[(x.min(15) * 16 + y.min(15)) as usize];
    if len == 0 {
        return None;
    }
    let mut bits = len as u32;
    for mag in [x, y] {
        if mag.min(15) == 15 && book.linbits != 0 {
            bits += book.linbits;
        }
        if mag != 0 {
            bits += 1;
        }
    }
    Some(bits)
}

/// Cost of one count1 quadruple (nibble `v0<<3|v1<<2|v2<<1|v3`, values
/// 0/1) including its sign bits. Returns `None` if unrepresentable (both
/// count1 books cover all 16 quads, so this is a formality).
fn count1_quad_cost(book: &Count1Book, quad: u32) -> Option<u32> {
    let (_, len) = book.codes[quad as usize];
    if len == 0 {
        return None;
    }
    Some(len as u32 + quad.count_ones())
}

/// Chosen Huffman structure for the big_values regions of one granule.
#[derive(Debug, Clone, Copy, Default)]
struct RegionPlan {
    table_select: [u8; 3],
    /// Stored side-info values (band count minus one).
    region_count: [u8; 2],
    bits: u64,
}

/// Plans the three big_values Huffman regions for the pairs `[0, big_values)`
/// of a quantized granule.
///
/// For every usable book the pair costs are accumulated into per-band
/// prefix sums; the region boundaries (region 0 spans 1..=16 bands, region
/// 1 1..=8, region 2 the remainder) are then chosen by exhaustive search for
/// the cheapest triple of single-book regions. Within a linbits family
/// (books 16..=23 and 24..=31 share one code table each, differing only in
/// escape width) only the narrowest escape covering the region's maximum
/// magnitude can be cheapest.
fn plan_regions(ix: &[u32; GRANULE_SAMPLES], big_values: usize, layout: &BandLayout) -> RegionPlan {
    let pairs = big_values;
    let mut band_of_pair_end = [0usize; N_LONG_SFB + 1];
    let mut band_max = [0u32; N_LONG_SFB];
    let mut b = 0usize;
    for (band, &w) in layout.widths.iter().enumerate() {
        let mut max = 0u32;
        for _ in 0..w / 2 {
            if b < pairs {
                max = max.max(ix[b * 2]).max(ix[b * 2 + 1]);
            }
            b += 1;
        }
        band_max[band] = max;
        band_of_pair_end[band + 1] = b.min(pairs);
    }

    // Prefix pair costs per book: `prefix[book][band]` = cost of all pairs
    // in bands `< band` under that book (unrepresentable pairs are charged
    // a large-but-finite sentinel so invalid books never win a region).
    const IMPRACTICAL: u64 = 1 << 30;
    let mut prefix = [[0u64; N_LONG_SFB + 1]; 32];
    for (book_n, book) in books().iter().enumerate() {
        let mut acc = [0u64; N_LONG_SFB + 1];
        for band in 0..N_LONG_SFB {
            let mut cost = 0u64;
            let mut ok = true;
            for p in band_of_pair_end[band]..band_of_pair_end[band + 1] {
                match book_pair_cost(book, ix[p * 2], ix[p * 2 + 1]) {
                    Some(c) => cost += c as u64,
                    None => ok = false,
                }
            }
            acc[band + 1] = acc[band] + if ok { cost } else { IMPRACTICAL };
        }
        prefix[book_n] = acc;
    }

    let region_cost = |start: usize, end: usize| -> (u64, u8) {
        let mut best = (IMPRACTICAL * 3, 0u8);
        if end <= start {
            return (0, 0);
        }
        let max = band_max[start..end].iter().copied().max().unwrap_or(0);
        for (book_n, book) in books().iter().enumerate() {
            if !book.usable && book_n != 0 {
                continue;
            }
            if book_n != 0 && book.max_mag < max {
                continue;
            }
            if book_n == 0 && max != 0 {
                continue;
            }
            let cost = prefix[book_n][end] - prefix[book_n][start];
            if cost < best.0 {
                best = (cost, book_n as u8);
            }
        }
        best
    };

    let mut best = RegionPlan {
        bits: u64::MAX,
        ..Default::default()
    };
    let bv_bands = band_of_pair_end
        .iter()
        .position(|&e| e >= pairs)
        .unwrap_or(N_LONG_SFB);
    for r0 in 1..=16usize {
        let a_end = r0.min(bv_bands);
        let (cost_a, book_a) = region_cost(0, a_end);
        for r1 in 1..=8usize {
            let b_start = r0.min(bv_bands);
            let b_end = (r0 + r1).min(bv_bands);
            let (cost_b, book_b) = region_cost(b_start, b_end);
            let (cost_c, book_c) = region_cost(b_end.max(b_start), bv_bands);
            let total = cost_a + cost_b + cost_c;
            if total < best.bits {
                best = RegionPlan {
                    table_select: [book_a, book_b, book_c],
                    region_count: [r0 as u8 - 1, r1 as u8 - 1],
                    bits: total,
                };
            }
        }
    }
    best
}

/// Emitted Huffman structure for one granule/channel.
#[derive(Debug, Clone, Copy)]
struct GranulePlan {
    global_gain: u8,
    scalefac_compress: u8,
    preflag: bool,
    /// Transmitted scalefactors (already pretab-adjusted when `preflag`).
    scalefacs: [u8; 21],
    big_values: u16,
    regions: RegionPlan,
    count1_table: u8,
    /// Quads of count1 data (0 = count1 region unused).
    count1_quads: u16,
    part2_3_length: u16,
}

impl GranulePlan {
    fn flat() -> Self {
        GranulePlan {
            global_gain: 255,
            scalefac_compress: 0,
            preflag: false,
            scalefacs: [0; 21],
            big_values: 0,
            regions: RegionPlan::default(),
            count1_table: 0,
            count1_quads: 0,
            part2_3_length: 0,
        }
    }
}

/// A quantization attempt's full cost structure: the plan it implies plus
/// the per-band quantization noise energies it produced.
struct GranuleCost {
    plan: GranulePlan,
    /// Quantization noise energy per band, `Σ (x − x̂)²`.
    band_noise: [f64; N_LONG_SFB],
    /// Total encoded bits (scalefacs + Huffman).
    bits: u64,
    /// True when even this granule's cheapest structure exceeded the budget.
    over_budget: bool,
}

/// Number of count1 quads (and the plan's `big_values`) covering all lines
/// with `|ix| >= 2` as big_values pairs and everything beyond as count1
/// quads through the last nonzero quad. `big_values` is kept even whenever
/// count1 data follows it, because the decoder reads quads from
/// `big_values * 2` forward and only completes quads that end by line 576
/// (an odd pair count would strand the final two lines).
fn split_big_values(ix: &[u32; GRANULE_SAMPLES]) -> (usize, usize) {
    let mut last_ge2 = None;
    let mut last_nonzero = None;
    for (i, &v) in ix.iter().enumerate() {
        if v >= 2 {
            last_ge2 = Some(i);
        }
        if v != 0 {
            last_nonzero = Some(i);
        }
    }
    let mut bv = last_ge2.map_or(0, |i| i / 2 + 1);
    if last_nonzero.is_some() && last_nonzero.unwrap() >= bv * 2 {
        // count1 quads follow: keep the boundary even-aligned.
        bv += bv & 1;
    }
    let quads = match last_nonzero {
        Some(l) if l >= bv * 2 => (l - bv * 2) / 4 + 1,
        _ => 0,
    };
    (bv, quads)
}

/// Counts the bits and builds the full plan implied by one
/// `(global_gain, scalefacs, compress, preflag)` combination: quantize,
/// split big_values/count1, plan regions, and measure per-band noise.
///
/// This is the shared inner computation of the rate loop; `budget` only
/// decides the returned `over_budget` flag (trimming to a hard budget is a
/// separate emit-time backstop).
#[allow(clippy::too_many_arguments)]
fn evaluate_granule(
    spec: &[f32; GRANULE_SAMPLES],
    layout: &BandLayout,
    global_gain: u8,
    scalefacs: &[u8; 21],
    compress: u8,
    preflag: bool,
    ms_stereo: bool,
) -> GranuleCost {
    let gains = band_gains(global_gain, scalefacs, preflag, ms_stereo);
    let ix = quantize_granule(spec, &gains, &layout.band_of_line);

    // Region plan for the all-pairs variant and the pairs+count1 variant;
    // keep whichever is cheaper.
    let (bv_min, quads) = split_big_values(&ix);
    let all_pairs_end = ix.iter().rposition(|&v| v != 0).map_or(0, |i| i / 2 + 1);

    let regions_pairs = plan_regions(&ix, all_pairs_end, layout);
    let mut best_bits = regions_pairs.bits;
    let mut best_struct = (all_pairs_end, 0u16, regions_pairs, 0u8);
    if quads > 0 {
        let regions_split = plan_regions(&ix, bv_min, layout);
        let book_a = &count1_books()[0];
        let book_b = &count1_books()[1];
        let mut count1_bits = [0u64; 2];
        for q in 0..quads {
            let mut nibble = 0u32;
            for k in 0..4 {
                if ix[bv_min * 2 + q * 4 + k] != 0 {
                    nibble |= 1 << (3 - k);
                }
            }
            for (sel, book) in [(0usize, book_a), (1, book_b)] {
                match count1_quad_cost(book, nibble) {
                    Some(c) => count1_bits[sel] += c as u64,
                    None => count1_bits[sel] = u64::MAX,
                }
            }
        }
        let sel = if count1_bits[1] < count1_bits[0] {
            1
        } else {
            0
        };
        let total = regions_split.bits + count1_bits[sel];
        if total < best_bits {
            best_bits = total;
            best_struct = (bv_min, quads as u16, regions_split, sel as u8);
        }
    }
    let (big_values, count1_quads, regions, count1_table) = best_struct;

    // Per-band quantization noise for the outer loop's distortion metric:
    // the decoder reconstructs `x̂ = scf·|ix|^(4/3)·sign`, so the noise is
    // measured against that exact (table-free powf) reconstruction.
    let mut band_noise = [0.0f64; N_LONG_SFB];
    for (band, noise) in band_noise.iter_mut().enumerate() {
        let gain = gains[band];
        let mut acc = 0.0f64;
        for i in layout.line_start[band]..layout.line_end[band] {
            let s = spec[i];
            let xq = gain * (ix[i] as f32).powf(4.0 / 3.0) * s.signum();
            acc += (s as f64 - xq as f64).powi(2);
        }
        *noise = acc;
    }

    let sfb_bits = scalefac_bits(compress);
    let total_bits = best_bits + sfb_bits;
    GranuleCost {
        plan: GranulePlan {
            global_gain,
            scalefac_compress: compress,
            preflag,
            scalefacs: *scalefacs,
            big_values: big_values as u16,
            regions,
            count1_table,
            count1_quads,
            part2_3_length: total_bits.min(4095) as u16,
        },
        band_noise,
        bits: total_bits,
        // `part2_3_length` is a 12-bit field: a granule costing more than
        // 4095 bits cannot be claimed honestly and must be trimmed back.
        over_budget: total_bits > 4095,
    }
}

/// Finds the smallest `global_gain` whose encoding fits `budget_bits` —
/// the finest quantizer that fits, since cost is decreasing in
/// global_gain — via exponential probing plus binary search over the
/// monotone cost.
#[allow(clippy::too_many_arguments)]
fn inner_loop(
    spec: &[f32; GRANULE_SAMPLES],
    layout: &BandLayout,
    scalefacs: &[u8; 21],
    compress: u8,
    preflag: bool,
    ms_stereo: bool,
    budget_bits: u64,
) -> GranuleCost {
    let cost_at =
        |gg: u8| evaluate_granule(spec, layout, gg, scalefacs, compress, preflag, ms_stereo);

    let coarsest = cost_at(255);
    if coarsest.bits > budget_bits {
        // Even the coarsest quantizer overshoots (pathologically small
        // budget): return the coarsest structure; emit-time trimming will
        // cut the tail to restore the byte budget.
        let mut coarsest = coarsest;
        coarsest.over_budget = true;
        return coarsest;
    }
    if coarsest.over_budget {
        return coarsest;
    }

    // Walk down from the coarse end while the gains still fit, then binary
    // search the transition. `hi` is the finest known-fitting gain, `lo`
    // the coarsest known-non-fitting (or -1 before any failure).
    let mut best = coarsest;
    let mut hi = 255u32;
    let mut lo: u32 = u32::MAX; // sentinel: nothing known non-fitting yet
    let mut probe = 128u16;
    while probe > 0 {
        let c = cost_at(probe as u8);
        if c.bits <= budget_bits {
            best = c;
            hi = probe as u32;
            probe /= 2;
        } else {
            lo = probe as u32;
            break;
        }
    }
    if lo == u32::MAX && hi <= 1 {
        // Every probed gain fit; gg = 0 is the only finer candidate left.
        let c = cost_at(0);
        if c.bits <= budget_bits {
            return c;
        }
        lo = 0;
    }
    while lo != u32::MAX && lo + 1 < hi {
        let mid = (lo + hi) / 2;
        let c = cost_at(mid as u8);
        if c.bits <= budget_bits {
            hi = mid;
            best = c;
        } else {
            lo = mid;
        }
    }
    best
}

/// Chooses the smallest bit-width `scalefac_compress` whose slen widths can
/// carry the transmitted scalefactors.
///
/// `preflag` stays `false`: the pretab would add a fixed extra boost to
/// bands 11..20 on the decode side, which in transmitted space only shifts
/// those bands' effective baseline and buys headroom the planner cannot
/// spend per-band (the transmitted cap `2^slen2 − 1` binds regardless).
/// LAME reaches for preflag on pathological spectra; the flat-plan fallback
/// here keeps the same spec-legal ceiling without the extra policy.
fn choose_compress(scalefacs: &[u8; 21]) -> (u8, bool) {
    for compress in 0..16u8 {
        let (s1, s2) = slens(compress);
        let fits = (0..11).all(|b| (scalefacs[b] as u32) < (1 << s1))
            && (11..21).all(|b| (scalefacs[b] as u32) < (1 << s2));
        if fits {
            return (compress, false);
        }
    }
    (15, false) // unreachable: compress 15 carries any legal plan
}

// ---------------------------------------------------------------------------
// Psychoacoustic thresholds: a simplified ISO/IEC 11172-3 psychoacoustic
// model I (Annex D). Per scalefactor band it estimates the *allowed*
// quantization-noise energy: the maximum of the absolute hearing threshold
// and the spread masking contributions of every other band, with tonal
// maskers (poor maskers) contributing 14.5 dB below their energy and noise
// maskers (good maskers) 5.5 dB below — the standard tone-masking-noise /
// noise-masking-tone offsets — attenuated by the model II spreading
// function (10 dB/bark upward spread of masking, 25 dB/bark downward).
// ---------------------------------------------------------------------------

/// Bark scale (Zwicker): `z(f) = 13·atan(0.00076f) + 3.5·atan((f/7500)²)`.
fn bark(f_hz: f64) -> f64 {
    13.0 * (0.00076 * f_hz).atan() + 3.5 * (f_hz / 7500.0).powi(2).atan()
}

/// Absolute hearing threshold in dB SPL (Painter & Spanias' three-term
/// approximation), `f` in Hz.
fn ath_db(f_hz: f64) -> f64 {
    let k = f_hz / 1000.0;
    3.64 * k.powf(-0.8) - 6.8 * (-0.6 * (k - 3.4).powi(2)).exp() + 1e-6 * k.powi(4)
}

/// Model II spreading function in dB at `dz` barks from the masker
/// (`dz > 0`: maskee above masker). Exact ISO model II form; asymptotically
/// −10 dB/bark upward, −25 dB/bark downward.
fn spreading_db(dz: f64) -> f64 {
    let t = dz + 0.474;
    15.810 + 7.5 * t - 17.5 * (1.0 + t * t).sqrt()
}

/// Full-scale calibration: spectral lines live on the decoder's
/// int16-magnitude scale (±32768); a full-scale sine's MDCT line carries
/// roughly half that amplitude squared, which this model treats as 96 dB
/// SPL (16-bit full scale ≈ 96 dB above the 20 µPa reference with ~0 dBFS
/// playback levels). Only the ATH anchor depends on the calibration; the
/// spreading/tonality part is relative and unaffected.
const FULL_SCALE_SINE_LINE_ENERGY: f64 = 32768.0 * 32768.0 / 2.0;
const FULL_SCALE_DB_SPL: f64 = 96.0;

/// Per-band allowed quantization-noise energy.
fn psy_thresholds(
    spec: &[f32; GRANULE_SAMPLES],
    layout: &BandLayout,
    sample_rate: u32,
) -> [f64; N_LONG_SFB] {
    let mut energy = [0.0f64; N_LONG_SFB];
    let mut max_line = [0.0f64; N_LONG_SFB];
    let mut bark_z = [0.0f64; N_LONG_SFB];
    for band in 0..N_LONG_SFB {
        let mut acc = 0.0f64;
        let mut peak = 0.0f64;
        for &s in &spec[layout.line_start[band]..layout.line_end[band]] {
            let p = f64::from(s) * f64::from(s);
            acc += p;
            peak = peak.max(p);
        }
        energy[band] = acc;
        max_line[band] = peak;
        let center = (layout.line_start[band] + layout.line_end[band]) as f64 * 0.5;
        bark_z[band] = bark(center * sample_rate as f64 / GRANULE_SAMPLES as f64 / 2.0);
    }

    let mut thresholds = [0.0f64; N_LONG_SFB];
    let peak_energy = energy.iter().copied().fold(0.0f64, f64::max);
    for band in 0..N_LONG_SFB {
        // Absolute threshold.
        let center_hz =
            (layout.line_start[band] + layout.line_end[band]) as f64 * 0.5 * sample_rate as f64
                / GRANULE_SAMPLES as f64
                / 2.0;
        let mut t = 10.0f64.powf((ath_db(center_hz) - FULL_SCALE_DB_SPL) / 10.0)
            * FULL_SCALE_SINE_LINE_ENERGY
            * layout.widths[band] as f64;
        // Spread masking from every band with significant energy.
        if peak_energy > 0.0 {
            for (masker, &e) in energy.iter().enumerate() {
                if e <= 0.0 || masker == band {
                    continue;
                }
                // Tonality: a band whose peak line dominates its mean by
                // >10 dB behaves tonally (poor masker, 14.5 dB offset);
                // otherwise noise-like (5.5 dB offset).
                let lines = (layout.line_end[masker] - layout.line_start[masker]) as f64;
                let tonal = max_line[masker] > 10.0 * (e / lines);
                let offset_db = if tonal { 14.5 } else { 5.5 };
                let dz = bark_z[band] - bark_z[masker];
                let contrib = e * 10.0f64.powf((spreading_db(dz) - offset_db) / 10.0);
                t = t.max(contrib);
            }
            // Relative floor: never chase noise more than ~60 dB below the
            // frame's loudest band (the budget loop would otherwise spend
            // every spare bit on inaudible residuals).
            t = t.max(peak_energy * 1e-6);
        }
        thresholds[band] = t;
    }
    thresholds
}

/// Number of psychoacoustic amplification rounds performed by
/// [`plan_granule`]'s outer loop. The machinery (thresholds, amplification,
/// best-plan selection) is fully implemented and unit-tested, but rounds
/// are currently **zero**: with amplification active, some encoded frames
/// disagree with FFmpeg's decode of the same bytes at 26–43 dB (our own
/// decoder round-trips them exactly, so the intent is consistent, but the
/// suite's inter-decoder gate requires ≥100 dB). Root-cause hunt and
/// reproduction recipe: see `todo.md`, "MP3 encoder psychoacoustic
/// amplification inter-decoder divergence" (2026-09-27). With zero rounds
/// the encoder still uses the full structural work — per-granule
/// global-gain search, region/table selection across all 32 books, count1
/// coding, and the scalefactor machinery — at the masking-threshold-
/// satisfied criterion of the first (flat) iteration.
const PSY_AMPLIFICATION_ROUNDS: usize = 0;

/// Outer loop: the ISO/LAME two-loop quantizer. Starting from a flat
/// scalefactor vector, repeatedly find the band whose quantization noise
/// most exceeds its psychoacoustic threshold and amplify it one scalefactor
/// unit (≈4.5 dB noise reduction in that band), re-running the inner
/// global-gain loop each time. Keeps the best budget-fitting plan seen
/// (least total relative excess); stops when every band is satisfied, no
/// band can be amplified further, or the iteration/budget limits are hit.
fn plan_granule(
    spec: &[f32; GRANULE_SAMPLES],
    layout: &BandLayout,
    thresholds: &[f64; N_LONG_SFB],
    ms_stereo: bool,
    budget_bits: u64,
) -> GranuleCost {
    let mut scalefacs = [0u8; 21];
    let mut amplified = [false; N_LONG_SFB];
    let mut best: Option<GranuleCost> = None;
    let mut best_excess = f64::INFINITY;
    let mut fallback: Option<GranuleCost> = None;

    for _round in 0..=PSY_AMPLIFICATION_ROUNDS {
        let (compress, preflag) = choose_compress(&scalefacs);
        let cost = inner_loop(
            spec,
            layout,
            &scalefacs,
            compress,
            preflag,
            ms_stereo,
            budget_bits,
        );
        if cost.over_budget {
            fallback = Some(cost);
            break;
        }

        // Per-band relative excess over the masking thresholds (band 21
        // carries no transmissible scalefactor, so it can never be
        // amplified and is excluded from the worst-band search).
        let mut ratios = [0.0f64; N_LONG_SFB];
        for band in 0..N_LONG_SFB {
            let t = thresholds[band];
            ratios[band] = if t > 0.0 {
                cost.band_noise[band] / t
            } else {
                0.0
            };
        }
        let worst_ratio = ratios[..21].iter().copied().fold(0.0f64, f64::max);
        let excess: f64 = ratios.iter().sum();
        if excess < best_excess {
            best_excess = excess;
            best = Some(cost);
        }
        if worst_ratio <= 1.0 || PSY_AMPLIFICATION_ROUNDS == 0 {
            break; // every band at or under threshold / amplification off
        }

        // Amplify the band with the highest noise-to-threshold ratio that
        // still has headroom under the widest compress widths.
        let mut worst = None;
        let mut worst_val = 1.0f64;
        for band in 0..21usize {
            if ratios[band] > worst_val && !amplified[band] && scalefacs[band] < 15 {
                worst_val = ratios[band];
                worst = Some(band);
            }
        }
        match worst {
            Some(band) => {
                scalefacs[band] += 1;
                // When this band's scalefactor can no longer grow within
                // the widest compress widths, stop trying it.
                let (s1, s2) = slens(15);
                let cap = if band < 11 { 1 << s1 } else { 1 << s2 };
                if scalefacs[band] + 1 >= cap as u8 {
                    amplified[band] = true;
                }
            }
            None => break,
        }
    }

    best.or(fallback).unwrap_or_else(|| {
        // Unreachable in practice (the first iteration always yields either
        // a fitting plan or a fallback), but keep a valid plan so callers
        // never see an empty state.
        let mut flat = inner_loop(spec, layout, &[0; 21], 0, false, ms_stereo, u64::MAX);
        flat.over_budget = true;
        flat
    })
}

// ---------------------------------------------------------------------------
// Top-level encoder
// ---------------------------------------------------------------------------

/// MPEG-1 Layer III bitrates in kbps, indexed by the 4-bit header field
/// (`bitrate_index_table[0]` is unused/free-format).
const MPEG1_BITRATES_KBPS: [u32; 15] = [
    0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320,
];

/// Per-channel state carried across granules/frames.
struct ChannelState {
    /// Previous granule's last 18 subband-time samples, per band (for the
    /// 36-point MDCT's 50% overlap). Zero-initialized (silence history).
    history: [[f32; 18]; 32],
    /// Analysis filter's 512-sample circular PCM history (see
    /// [`analyze_block_polyphase`]). Zero-initialized (silence lead-in,
    /// matching the decoder's own zero-initialized QMF/overlap state).
    analysis_hist: [f32; HAN_SIZE],
    /// Current write offset into `analysis_hist`.
    analysis_off: usize,
}

impl ChannelState {
    fn new() -> Self {
        ChannelState {
            history: [[0.0; 18]; 32],
            analysis_hist: [0.0; HAN_SIZE],
            analysis_off: 0,
        }
    }
}

/// MPEG-1 Layer III encoder implementing [`Encoder`].
///
/// Per granule/channel the encoder runs the classic two-loop Layer III
/// quantizer: an inner global-gain rate loop and an outer per-band
/// scalefactor-amplification distortion loop, steered by the module's
/// simplified psychoacoustic model. Stereo frames are analyzed per frame and
/// coded either independent (LR) or mid/side (joint_stereo, mode_ext bit 2)
/// depending on where the energy sits. See the module doc comment for the
/// full scope (fixed CBR, long blocks only, no bit-reservoir borrowing).
pub struct Mp3Encoder<W: Write> {
    sink: W,
    sample_rate: u32,
    channels: u16,
    bitrate_idx: u8,
    sr_idx: u8,

    channel_state: Vec<ChannelState>,
    /// Interleaved PCM samples awaiting a full 1152-sample frame.
    pending: Vec<f32>,
    finished: bool,
    frac_accum: f64,

    // Scratch reused per frame to avoid per-frame allocation.
    spec_scratch: Vec<[f32; GRANULE_SAMPLES]>, // per (channel*2 + granule)
    /// Scalefactor-band geometry for this sample rate.
    layout: BandLayout,
}

impl<W: Write> Mp3Encoder<W> {
    /// Opens a new MPEG-1 Layer III stream for writing.
    ///
    /// `sample_rate` must be one of 32000/44100/48000 Hz (MPEG-1's family;
    /// this encoder does not support the MPEG-2/2.5 low-sample-rate
    /// extensions), `channels` must be 1 or 2, and `bitrate_kbps` must be
    /// one of the 14 standard MPEG-1 Layer III rates (32..=320).
    pub fn new(sink: W, sample_rate: u32, channels: u16, bitrate_kbps: u32) -> Result<Self> {
        let sr_idx = match sample_rate {
            44100 => 0u8,
            48000 => 1,
            32000 => 2,
            _ => {
                return Err(CadenceError::InvalidFormat(format!(
                    "MP3 encoder supports 32000/44100/48000 Hz (MPEG-1 only), got {sample_rate}"
                )))
            }
        };
        if !(1..=2).contains(&channels) {
            return Err(CadenceError::InvalidFormat(format!(
                "MP3 encoder supports 1 or 2 channels, got {channels}"
            )));
        }
        let bitrate_idx = MPEG1_BITRATES_KBPS
            .iter()
            .position(|&b| b == bitrate_kbps)
            .ok_or_else(|| {
                CadenceError::InvalidFormat(format!(
                    "{bitrate_kbps} kbps is not a standard MPEG-1 Layer III bitrate"
                ))
            })? as u8;
        if bitrate_idx == 0 {
            return Err(CadenceError::InvalidFormat(
                "free-format (0 kbps index) is not supported".to_string(),
            ));
        }

        Ok(Mp3Encoder {
            sink,
            sample_rate,
            channels,
            bitrate_idx,
            sr_idx,
            channel_state: (0..channels).map(|_| ChannelState::new()).collect(),
            pending: Vec::with_capacity(FRAME_SAMPLES * channels as usize),
            finished: false,
            frac_accum: 0.0,
            spec_scratch: vec![[0.0; GRANULE_SAMPLES]; 2 * channels as usize],
            layout: BandLayout::new(sideinfo::sr_table_idx_for_sr(sample_rate)),
        })
    }

    fn header_bytes(&self, padding: bool, ms: bool) -> [u8; 4] {
        let b1 = 0xF0u8 | 0x08 | 0x02 | 0x01; // sync tail + MPEG1 + layer III + no CRC
        let b2 = (self.bitrate_idx << 4) | (self.sr_idx << 2) | (padding as u8) << 1;
        // mode: mono=3, stereo=0, joint stereo=1 (with mode_ext bit 2 = ms)
        let (mode, mode_ext): (u8, u8) = if self.channels == 1 {
            (0b11, 0)
        } else if ms {
            (0b01, 0b10)
        } else {
            (0b00, 0)
        };
        let b3 = (mode << 6) | (mode_ext << 4);
        [0xFF, b1, b2, b3]
    }

    /// Runs the analysis filterbank + forward MDCT for one channel's 1152
    /// PCM samples (already in `self.pending`), filling
    /// `self.spec_scratch[ch*2 + gr]` with each granule's 576 spectral
    /// lines and updating that channel's history.
    #[allow(clippy::needless_range_loop)] // `t` indexes multiple parallel arrays
    fn analyze_channel(&mut self, ch: usize) {
        let channels = self.channels as usize;
        for gr in 0..2 {
            let mut new_subband = [[0.0f32; 18]; 32];
            for t in 0..18 {
                let sample_base = (gr * 18 + t) * 32;
                let mut new_samples = [0.0f32; 32];
                for (n, s) in new_samples.iter_mut().enumerate() {
                    let idx = (sample_base + n) * channels + ch;
                    // The decoder's synthesis filterbank (`crate::synth`)
                    // divides its final output by `PCM_SCALE = 1/32768`, i.e.
                    // it expects subband/spectral values on a roughly
                    // int16-PCM-magnitude scale, not the `Encoder` trait's
                    // normalized `[-1.0, 1.0]` input range. Scale up here so
                    // round-tripped amplitude matches the source instead of
                    // coming out ~32768x too quiet.
                    *s = self.pending[idx] * 32768.0;
                }
                let mut out = [0.0f32; 32];
                {
                    let state = &mut self.channel_state[ch];
                    analyze_block_polyphase(
                        &mut state.analysis_hist,
                        &mut state.analysis_off,
                        &new_samples,
                        &mut out,
                    );
                }
                for band in 0..32 {
                    // Pre-compensate for `crate::imdct::change_sign`, which
                    // the decoder unconditionally applies *after* IMDCT: it
                    // negates odd-indexed time samples of odd-numbered
                    // bands. Negating here (self-inverse: applying the same
                    // flip twice is the identity) makes the decoder's flip
                    // restore the true value instead of corrupting it.
                    let sign = if band % 2 == 1 && t % 2 == 1 {
                        -1.0
                    } else {
                        1.0
                    };
                    new_subband[band][t] = out[band] * sign;
                }
            }

            let spec = &mut self.spec_scratch[ch * 2 + gr];
            let history = &mut self.channel_state[ch].history;
            for band in 0..32 {
                let mut x = [0.0f32; 36];
                x[..18].copy_from_slice(&history[band]);
                x[18..].copy_from_slice(&new_subband[band]);
                let lines = forward_mdct36(&x);
                spec[band * 18..band * 18 + 18].copy_from_slice(&lines);
                history[band] = new_subband[band];
            }

            // Pre-compensate for `crate::processing::antialias`, which the
            // decoder unconditionally applies to the *spectral* data before
            // IMDCT: it rotates each pair of boundary lines between bands
            // `b` and `b+1` (8 lines each side) by a fixed angle. Since
            // `spec` above was constructed to be the exact adjoint target
            // for IMDCT (no antialias involved), apply antialias's inverse
            // rotation here so the decoder's forward rotation restores it.
            // The rotation matrix `[[AA0,-AA1],[AA1,AA0]]` is orthogonal
            // (`AA0^2+AA1^2 == 1`), so its inverse is its transpose.
            for b in 0..31 {
                let base = b * 18;
                for i in 0..8 {
                    let up = spec[base + 18 + i];
                    let dp = spec[base + 17 - i];
                    let aa0 = crate::tables::AA[0][i];
                    let aa1 = crate::tables::AA[1][i];
                    spec[base + 18 + i] = up * aa0 + dp * aa1;
                    spec[base + 17 - i] = -up * aa1 + dp * aa0;
                }
            }
        }
    }

    /// Mid/side preference for this frame (stereo only): true when the side
    /// channel `(L−R)` carries less than half the mid channel `(L+R)`
    /// energy over both granules — i.e. the stereo image is centered enough
    /// that spending separate bits on two near-identical channels wastes
    /// budget. Dual-mono content collapses to side energy 0 (always MS);
    /// hard-panned or independent content has side ≈ mid energy (never MS).
    fn prefer_ms(&self) -> bool {
        let (mut mid_e, mut side_e) = (0.0f64, 0.0f64);
        for gr in 0..2 {
            let (l, r) = (&self.spec_scratch[gr], &self.spec_scratch[2 + gr]);
            for i in 0..GRANULE_SAMPLES {
                let a = l[i] as f64;
                let b = r[i] as f64;
                mid_e += (a + b) * (a + b);
                side_e += (a - b) * (a - b);
            }
        }
        if mid_e <= 0.0 {
            return true; // silence: mode is irrelevant; MS keeps it uniform
        }
        side_e < 0.5 * mid_e
    }

    /// Rewrites both granules' spectra from L/R to mid/side coding values:
    /// `M = (L+R)·2^-3/2`, `S = (L−R)·2^-3/2`. That factor combined with the
    /// decoder's ms_stereo requant gain (×√2 on both channels) and its final
    /// `m+s` / `m−s` reconstruction yields exactly `L` and `R`.
    fn transform_to_ms(&mut self) {
        // 2^-3/2 = √2/4; split as 2^-1/2 / 2 because powf isn't const.
        const K: f32 = std::f32::consts::FRAC_1_SQRT_2 / 2.0;
        let (mid, side) = self.spec_scratch.split_at_mut(2);
        for gr in 0..2 {
            let (l, r) = (&mut mid[gr], &mut side[gr]);
            for i in 0..GRANULE_SAMPLES {
                let a = l[i];
                let b = r[i];
                l[i] = (a + b) * K;
                r[i] = (a - b) * K;
            }
        }
    }

    #[allow(clippy::needless_range_loop)] // slot indices address parallel arrays
    fn emit_frame(&mut self) -> Result<()> {
        let channels = self.channels as usize;
        for ch in 0..channels {
            self.analyze_channel(ch);
        }

        let ms = channels == 2 && self.prefer_ms();
        if ms {
            self.transform_to_ms();
        }

        let bitrate_kbps = MPEG1_BITRATES_KBPS[self.bitrate_idx as usize];
        let ideal_bytes =
            FRAME_SAMPLES as f64 * bitrate_kbps as f64 * 125.0 / self.sample_rate as f64;
        self.frac_accum += ideal_bytes - ideal_bytes.floor();
        let padding = self.frac_accum >= 1.0;
        if padding {
            self.frac_accum -= 1.0;
        }

        let hdr = self.header_bytes(padding, ms);
        let parsed = header::parse_header(&hdr).map_err(|e| {
            CadenceError::InvalidFormat(format!("internal header build error: {e}"))
        })?;
        let total_bytes = parsed.total_bytes();
        let side_info_bytes = if channels == 1 { 17usize } else { 32 };
        let main_data_bits: u64 = ((total_bytes * 8)
            .saturating_sub(32)
            .saturating_sub(side_info_bytes * 8)) as u64;
        let slots = 2 * channels; // 2 granules * channels

        // Per-(granule, channel) two-loop planning. Each slot gets its fair
        // share of the frame's main-data bits except the last, which
        // inherits the whole frame's unspent remainder — the intra-frame
        // equivalent of bit-reservoir borrowing, and a real win whenever one
        // granule (e.g. silence) needs almost nothing.
        let mut plans = [
            GranulePlan::flat(),
            GranulePlan::flat(),
            GranulePlan::flat(),
            GranulePlan::flat(),
        ];
        let fair = main_data_bits / slots as u64;
        let mut remaining = main_data_bits;
        for slot in 0..slots {
            let gr = slot / channels;
            let ch = slot % channels;
            let spec = self.spec_scratch[ch * 2 + gr];
            let budget = if slot == slots - 1 {
                remaining
            } else {
                fair.min(remaining)
            };
            let thresholds = psy_thresholds(&spec, &self.layout, self.sample_rate);
            let cost = plan_granule(&spec, &self.layout, &thresholds, ms, budget);
            remaining -= cost.bits.min(remaining);
            plans[slot] = cost.plan;
        }

        // Hard byte-budget backstop: when even the coarsest structure of a
        // slot overshot (pathologically small CBR budgets), trim trailing
        // count1 quads / big_values pairs until the frame fits again.
        if plans.iter().map(|p| p.part2_3_length as u64).sum::<u64>() > main_data_bits {
            for slot in 0..slots {
                let gr = slot / channels;
                let ch = slot % channels;
                let spec = self.spec_scratch[ch * 2 + gr];
                while plans[slot].big_values > 0 || plans[slot].count1_quads > 0 {
                    if plans.iter().map(|p| p.part2_3_length as u64).sum::<u64>() <= main_data_bits
                    {
                        break;
                    }
                    let plan = &mut plans[slot];
                    if plan.count1_quads > 0 {
                        plan.count1_quads -= 1;
                    } else {
                        plan.big_values -= 1;
                    }
                    plan.part2_3_length =
                        measure_plan(plan, &spec, &self.layout, ms).min(4095) as u16;
                }
            }
        }

        if std::env::var_os("CADENCE_MP3_DUMP_PLANS").is_some() {
            for (slot, p) in plans.iter().enumerate() {
                eprintln!(
                    "PLAN frame-slot {slot}: ms={ms} gg={} sc={} pf={} bv={} q={} regions={:?} books={:?} cnt1tab={} p23={} sfs={:?}",
                    p.global_gain, p.scalefac_compress, p.preflag, p.big_values,
                    p.count1_quads, p.regions.region_count, p.regions.table_select, p.count1_table, p.part2_3_length, p.scalefacs
                );
            }
        }
        let mut bw = BitWriter::new();
        // --- Frame header ---
        for &b in &hdr {
            bw.push(b as u64, 8);
        }

        // --- Side info ---
        bw.push(0, 9); // main_data_begin (always 0: no reservoir borrowing)
        if channels == 1 {
            bw.push(0, 5); // private_bits(5) — mono
        } else {
            bw.push(0, 3); // private_bits(3) — stereo
        }
        for _ in 0..channels {
            bw.push(0, 4); // scfsi: no scalefactor sharing between granules
        }
        for gr in 0..2 {
            for ch in 0..channels {
                let plan = &plans[gr * channels + ch];
                bw.push(plan.part2_3_length as u64, 12);
                bw.push(plan.big_values as u64, 9);
                bw.push(plan.global_gain as u64, 8);
                bw.push(plan.scalefac_compress as u64, 4);
                bw.push(0, 1); // window_switching_flag = 0 (long block, block_type 0)
                bw.push(plan.regions.table_select[0] as u64, 5);
                bw.push(plan.regions.table_select[1] as u64, 5);
                bw.push(plan.regions.table_select[2] as u64, 5);
                bw.push(plan.regions.region_count[0] as u64, 4);
                bw.push(plan.regions.region_count[1] as u64, 3);
                bw.push(plan.preflag as u64, 1);
                bw.push(0, 1); // scalefac_scale = 0 (each scalefac unit is 2^0.5)
                bw.push(plan.count1_table as u64, 1);
            }
        }

        // --- Main data: per granule, per channel: scalefactors, then
        // Huffman pairs, then count1 quads (the decoder's exact read order).
        for gr in 0..2 {
            for ch in 0..channels {
                let plan = &plans[gr * channels + ch];
                let spec = self.spec_scratch[ch * 2 + gr];
                let before = bw.bit_pos;
                emit_granule_data(&mut bw, plan, &spec, &self.layout, ms);
                debug_assert_eq!(
                    (bw.bit_pos - before) as u64,
                    plan.part2_3_length as u64,
                    "emitted main data must match the planned part2_3_length"
                );
            }
        }
        bw.align();

        debug_assert!(
            bw.bytes.len() <= total_bytes,
            "encoded frame ({} bytes) exceeds its CBR budget ({total_bytes} bytes)",
            bw.bytes.len()
        );
        bw.bytes.resize(total_bytes, 0);
        self.sink.write_all(&bw.bytes)?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Main-data emission: the single canonical writer for one granule/channel's
// scalefactors + Huffman data. `emit_frame` calls it for real output; the
// trim backstop calls it (into a scratch writer) to re-measure a trimmed
// plan, and the debug assertion pins the written bit count to the planned
// `part2_3_length` on every frame.
// ---------------------------------------------------------------------------

/// Band → Huffman-region map implied by a plan's stored region counts
/// (region 0 covers `count[0]+1` bands, region 1 `count[1]+1`, region 2 the
/// remainder — mirroring the decoder's region walk).
fn region_map(region_count: [u8; 2]) -> [u8; N_LONG_SFB] {
    let mut map = [2u8; N_LONG_SFB];
    let r0 = (region_count[0] as usize + 1).min(N_LONG_SFB);
    let r1 = (r0 + region_count[1] as usize + 1).min(N_LONG_SFB);
    for band in map.iter_mut().take(r0) {
        *band = 0;
    }
    for band in map.iter_mut().take(r1).skip(r0) {
        *band = 1;
    }
    map
}

/// Transmitted scalefactor value for band `b`. `plan.scalefacs` holds the
/// transmitted values directly; with preflag the decoder adds the fixed
/// pretab on top, which `band_gains` mirrors.
fn transmitted_sfac(plan: &GranulePlan, band: usize) -> u8 {
    plan.scalefacs[band]
}

/// Writes one granule/channel's complete main data (scalefactors, big_values
/// pairs through the three Huffman regions, count1 quads) exactly as the
/// decoder reads it.
fn emit_granule_data(
    bw: &mut BitWriter,
    plan: &GranulePlan,
    spec: &[f32; GRANULE_SAMPLES],
    layout: &BandLayout,
    ms_stereo: bool,
) {
    let gains = band_gains(plan.global_gain, &plan.scalefacs, plan.preflag, ms_stereo);
    let ix = quantize_granule(spec, &gains, &layout.band_of_line);

    // Scalefactors: 11 values at slen1 (bands 0..=10), 10 at slen2
    // (bands 11..=20); band 21 carries no scalefactor.
    let (s1, s2) = slens(plan.scalefac_compress);
    for band in 0..11 {
        bw.push(transmitted_sfac(plan, band) as u64, s1);
    }
    for band in 11..21 {
        bw.push(transmitted_sfac(plan, band) as u64, s2);
    }

    // big_values pairs through the three regions.
    let map = region_map(plan.regions.region_count);
    for p in 0..plan.big_values as usize {
        let (x, y) = (ix[p * 2], ix[p * 2 + 1]);
        let book = &books()
            [plan.regions.table_select[map[layout.band_of_line[p * 2] as usize] as usize] as usize];
        let (code, len) = book.codes[((x.min(15)) * 16 + y.min(15)) as usize];
        bw.push(code as u64, len as u32);
        for (k, &mag) in [x, y].iter().enumerate() {
            if mag.min(15) == 15 && book.linbits != 0 {
                bw.push((mag - 15) as u64, book.linbits);
            }
            if mag != 0 {
                bw.push(u64::from(spec[p * 2 + k] < 0.0), 1);
            }
        }
    }

    // count1 quadruples.
    if plan.count1_quads > 0 {
        let quad_book = &count1_books()[plan.count1_table as usize];
        for q in 0..plan.count1_quads as usize {
            let base = plan.big_values as usize * 2 + q * 4;
            let mut nibble = 0u32;
            for (k, &v) in ix[base..base + 4].iter().enumerate() {
                if v != 0 {
                    nibble |= 1 << (3 - k);
                }
            }
            let (code, len) = quad_book.codes[nibble as usize];
            bw.push(code as u64, len as u32);
            for k in 0..4usize {
                if nibble & (1 << (3 - k)) != 0 {
                    bw.push(u64::from(spec[base + k] < 0.0), 1);
                }
            }
        }
    }
}

/// Re-measures a plan's exact `part2_3_length` by writing it into a scratch
/// writer (used only by the rare trim backstop, where a plan's tail had to
/// be cut after the fact).
fn measure_plan(
    plan: &GranulePlan,
    spec: &[f32; GRANULE_SAMPLES],
    layout: &BandLayout,
    ms_stereo: bool,
) -> u64 {
    let mut scratch = BitWriter::new();
    emit_granule_data(&mut scratch, plan, spec, layout, ms_stereo);
    scratch.bit_pos as u64
}

impl<W: Write + Send> Encoder for Mp3Encoder<W> {
    fn encode(&mut self, samples: &[f32]) -> Result<usize> {
        let channels = self.channels as usize;
        if samples.len() % channels != 0 {
            return Err(CadenceError::InvalidFormat(format!(
                "sample count {} is not a multiple of the channel count {}",
                samples.len(),
                channels
            )));
        }
        self.pending.extend_from_slice(samples);
        while self.pending.len() >= FRAME_SAMPLES * channels {
            self.emit_frame()?;
            self.pending.drain(..FRAME_SAMPLES * channels);
        }
        Ok(samples.len() / channels)
    }

    fn finish(&mut self) -> Result<()> {
        if self.finished {
            return Ok(());
        }
        self.finished = true;
        let channels = self.channels as usize;
        if !self.pending.is_empty() {
            // Pad the final partial frame with silence so it can still be
            // encoded as a full 1152-sample MPEG-1 frame; the extra tail
            // samples are inaudible padding, matching how CBR MP3 streams
            // routinely carry a few silent trailing samples.
            self.pending.resize(FRAME_SAMPLES * channels, 0.0);
            self.emit_frame()?;
            self.pending.clear();
        }
        self.sink.flush()?;
        Ok(())
    }
}

impl<W: Write> Drop for Mp3Encoder<W> {
    fn drop(&mut self) {
        // Best-effort flush; matches the FLAC/WAV/AIFF encoders' Drop
        // convention. `finish()` requires `&mut self` behind `Encoder`,
        // which Drop already gives us directly.
        if !self.finished {
            let _ = self.finish_infallible();
        }
    }
}

impl<W: Write> Mp3Encoder<W> {
    /// `Drop`-safe finish: same as [`Encoder::finish`] but callable without
    /// the trait in scope (`Drop::drop` only has `&mut self`).
    fn finish_infallible(&mut self) -> Result<()> {
        if self.finished {
            return Ok(());
        }
        self.finished = true;
        let channels = self.channels as usize;
        if !self.pending.is_empty() {
            self.pending.resize(FRAME_SAMPLES * channels, 0.0);
            self.emit_frame()?;
            self.pending.clear();
        }
        self.sink.flush()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::imdct;

    #[test]
    fn all_books_round_trip_through_decoder_tables() {
        // For every book and every (x, y) its table reaches, re-decode the
        // emitted codeword through the *exact* automaton
        // `crate::huffman::huffman` uses (replayed here at the bit level)
        // and confirm it reproduces the same (x, y). This is the ground
        // truth check that `build_huff_table`'s forward walk is a correct
        // inverse of the decoder's own tables for all 32 books, not just
        // the escape family.
        for (book_n, table) in books().iter().enumerate() {
            let book_off = TAB_INDEX[book_n] as i32;
            for x in 0..16u32 {
                for y in 0..16u32 {
                    let (code, len) = table.codes[(x * 16 + y) as usize];
                    if len == 0 {
                        continue; // unreachable pair for this table shape
                    }
                    // Feed `code` (len bits, MSB first) into the decode
                    // automaton starting at `book_off`.
                    let len = len as u32;
                    let mut base = book_off;
                    let mut w = 5u32;
                    let mut consumed = 0u32;
                    loop {
                        let take = w.min(len - consumed);
                        // Left-align the remaining code bits into a
                        // `w`-bit peek.
                        let remaining_bits = len - consumed;
                        let peek = if remaining_bits >= w {
                            (code as u32 >> (remaining_bits - w)) & ((1 << w) - 1)
                        } else {
                            (code as u32 & ((1 << remaining_bits) - 1)) << (w - remaining_bits)
                        };
                        let idx = (base + peek as i32) as usize;
                        let e = HUFF_TABS[idx] as i32;
                        if e >= 0 {
                            let l = (e >> 8) as u32;
                            assert_eq!(consumed + l, len, "book {book_n} x={x} y={y}");
                            let gx = (e & 0xF) as u32;
                            let gy = ((e >> 4) & 0xF) as u32;
                            assert_eq!((gx, gy), (x, y), "book {book_n}");
                            break;
                        } else {
                            let w2 = (e & 7) as u32;
                            let bias = e >> 3;
                            base = book_off - bias;
                            w = w2;
                            consumed += take;
                        }
                    }
                }
            }
        }
    }

    /// Encodes all 288 pairs of a granule through a single book (region
    /// counts stretched so region 0 never ends) and decodes it through the
    /// real `crate::huffman::huffman` entry point, checking the requantized
    /// output against the expected `scf·|ix|^(4/3)·sign`.
    fn check_book_round_trip_through_huffman(book_n: usize, magnitudes: &[u32; 576]) {
        let book = &books()[book_n];
        let mut bw = BitWriter::new();
        for i in 0..288usize {
            let (x, y) = (magnitudes[i * 2], magnitudes[i * 2 + 1]);
            let (code, len) = book.codes[((x.min(15)) * 16 + y.min(15)) as usize];
            assert!(
                len > 0 || (book_n == 0 && x == 0 && y == 0),
                "book {book_n} cannot represent ({x},{y})"
            );
            bw.push(code as u64, len as u32);
            for mag in [x, y] {
                if mag.min(15) == 15 && book.linbits != 0 {
                    bw.push((mag - 15) as u64, book.linbits);
                }
                if mag != 0 {
                    bw.push(1, 1); // negative sign
                }
            }
        }
        let mut dst = [0.0f32; 576];
        let info = crate::sideinfo::GranuleInfo {
            part_23_length: bw.bit_pos as u16,
            big_values: 288,
            table_select: [book_n as u8; 3],
            region_count: [255, 255, 255],
            sfbtab: &crate::tables::SCF_LONG[5],
            n_long_sfb: 22,
            ..Default::default()
        };
        // Distinctive per-band scalefactors: band b's gain is 2^(b/2).
        let mut scf = [0.0f32; 40];
        for (b, slot) in scf.iter_mut().take(22).enumerate() {
            *slot = (b as f32 / 2.0).exp2();
        }
        let end = crate::huffman::huffman(&mut dst, &bw.bytes, 0, &info, &scf, bw.bit_pos as i64);
        assert_eq!(end, bw.bit_pos as usize, "book {book_n} bit position");
        for (i, &v) in dst.iter().enumerate() {
            let band = crate::tables::SCF_LONG[5]
                .iter()
                .scan(0usize, |acc, &w| {
                    let start = *acc;
                    *acc += w as usize;
                    Some(start)
                })
                .position(|start| start > i)
                .map_or(21, |p| p - 1);
            let expected = scf[band]
                * (magnitudes[i] as f32).powf(4.0 / 3.0)
                * if magnitudes[i] != 0 { -1.0 } else { 1.0 };
            assert!(
                (v - expected).abs() <= expected.abs() * 2e-3 + 2e-3,
                "book {book_n} line {i} (mag {}): got {v}, want {expected}",
                magnitudes[i]
            );
        }
    }

    #[test]
    fn escape_books_round_trip_extreme_magnitudes_through_huffman() {
        // Every linbits book carries magnitudes at the escape boundary and
        // its advertised maximum, pinning `BookTable::max_mag` and the
        // escape/sign bit order end-to-end.
        for book_n in [15usize, 16, 17, 18, 19, 23, 24, 25, 27, 31] {
            let book = &books()[book_n];
            let mut magnitudes = [0u32; 576];
            let mut probe = |slot: usize, mag: u32| {
                magnitudes[slot] = mag;
            };
            probe(0, 1);
            probe(2, 14);
            probe(4, 15);
            if book.linbits > 0 {
                probe(6, 16);
                probe(8, 15 + (1 << book.linbits) - 1);
            }
            check_book_round_trip_through_huffman(book_n, &magnitudes);
        }
    }

    #[test]
    fn small_books_round_trip_magnitudes_through_huffman() {
        for book_n in [0usize, 1, 2, 3, 5, 9, 13] {
            let mut magnitudes = [0u32; 576];
            let max = books()[book_n].max_mag;
            if book_n != 0 {
                magnitudes[0] = max;
                magnitudes[1] = max.min(1);
                magnitudes[4] = 1;
                magnitudes[6] = max / 2;
            }
            check_book_round_trip_through_huffman(book_n, &magnitudes);
        }
    }

    #[test]
    fn count1_books_round_trip_through_huffman() {
        // All 16 quads (both sign polarities) coded as a pure count1 granule
        // (big_values = 0) through both count1 tables, decoded via the real
        // decoder with per-band scalefactors.
        for sel in 0..2u8 {
            let quad_book = &count1_books()[sel as usize];
            let mut bw = BitWriter::new();
            let mut quads = [[0u32; 4]; 144];
            for (q, quad) in quads.iter_mut().enumerate() {
                let pattern = (q % 16) as u32;
                *quad = [
                    pattern >> 3 & 1,
                    pattern >> 2 & 1,
                    pattern >> 1 & 1,
                    pattern & 1,
                ];
                let nibble = pattern;
                let (code, len) = quad_book.codes[nibble as usize];
                assert!(len > 0, "count1 book {sel} cannot represent {nibble}");
                bw.push(code as u64, len as u32);
                for k in 0..4usize {
                    if nibble & (1 << (3 - k)) != 0 {
                        bw.push(u64::from(q % 2 == 1), 1); // alternate signs
                    }
                }
            }
            let mut dst = [0.0f32; 576];
            let info = crate::sideinfo::GranuleInfo {
                part_23_length: bw.bit_pos as u16,
                big_values: 0,
                count1_table: sel,
                table_select: [0; 3],
                region_count: [255, 255, 255],
                sfbtab: &crate::tables::SCF_LONG[5],
                n_long_sfb: 22,
                ..Default::default()
            };
            let mut scf = [0.0f32; 40];
            for (b, slot) in scf.iter_mut().take(22).enumerate() {
                *slot = (b as f32 / 2.0).exp2();
            }
            let end =
                crate::huffman::huffman(&mut dst, &bw.bytes, 0, &info, &scf, bw.bit_pos as i64);
            assert_eq!(end, bw.bit_pos as usize);
            // Exact per-line check.
            let mut line = 0usize;
            for (b, &w) in crate::tables::SCF_LONG[5].iter().enumerate() {
                for _ in 0..w {
                    let q = line / 4;
                    let k = line % 4;
                    let present = quads[q][k] != 0;
                    let expected = if present {
                        scf[b] * if q % 2 == 0 { 1.0 } else { -1.0 }
                    } else {
                        0.0
                    };
                    assert!(
                        (dst[line] - expected).abs() <= 1e-6,
                        "count1 book {sel} line {line}: got {}, want {expected}",
                        dst[line]
                    );
                    line += 1;
                }
            }
        }
    }

    #[test]
    fn band_gains_match_decode_scalefactors_exactly() {
        // The planner's per-band multipliers must be bit-identical to what
        // the decoder's `decode_scalefactors` produces from the same
        // transmitted side information (global gain, scalefacs, preflag,
        // ms_stereo), for random plans across all compress values.
        use crate::bitreader::BitReader;
        use crate::scalefac;

        let mut seed = 0x5EED_1234u32;
        let mut rnd = move || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed
        };
        for &ms in &[false, true] {
            for compress in 0..16u8 {
                for &preflag in &[false, true] {
                    let mut scalefacs = [0u8; 21];
                    let (s1, s2) = slens(compress);
                    for (b, sf) in scalefacs.iter_mut().enumerate() {
                        let cap = if b < 11 { s1 } else { s2 };
                        *sf = ((rnd() % (1 << cap)) as u8).min(14);
                    }
                    let global_gain = (rnd() & 0xFF) as u8;

                    // Write the scalefactors exactly as the encoder does.
                    let mut bw = BitWriter::new();
                    let plan = GranulePlan {
                        scalefac_compress: compress,
                        preflag,
                        scalefacs,
                        ..GranulePlan::flat()
                    };
                    for band in 0..11 {
                        bw.push(transmitted_sfac(&plan, band) as u64, s1);
                    }
                    for band in 11..21 {
                        bw.push(transmitted_sfac(&plan, band) as u64, s2);
                    }

                    // Decode through the production path.
                    let hdr =
                        header::parse_header(&[0xFF, 0xFB, 0x90, if ms { 0x60 } else { 0x40 }])
                            .unwrap();
                    let mut bs = BitReader::new(&bw.bytes);
                    let mut decoded = [0.0f32; 40];
                    let gr = crate::sideinfo::GranuleInfo {
                        global_gain,
                        scalefac_compress: compress as u16,
                        preflag,
                        sfbtab: &crate::tables::SCF_LONG[5],
                        n_long_sfb: 22,
                        ..Default::default()
                    };
                    let mut ist_pos = [0u8; 39];
                    scalefac::decode_scalefactors(
                        &hdr,
                        &mut ist_pos,
                        &mut bs,
                        &gr,
                        &mut decoded,
                        0,
                    );

                    let gains = band_gains(global_gain, &scalefacs, preflag, ms);
                    for band in 0..22 {
                        assert_eq!(
                            decoded[band], gains[band],
                            "compress {compress} preflag {preflag} ms {ms} band {band}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn forward_mdct36_inverts_decoder_imdct_after_overlap_add() {
        // Three consecutive synthetic "granules" of one band's 18
        // subband-time samples; the middle granule's reconstructed output
        // (after overlap-add with both neighbors) should match its true
        // input closely once decoded through the real production IMDCT
        // path (`crate::imdct::imdct_gr`, block_type 0 / long blocks).
        let mut seed = 0xA5A5_1234u32;
        let mut rnd = move || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            (seed & 0xFFFF) as f32 / 32768.0 - 1.0
        };
        let g0: [f32; 18] = std::array::from_fn(|_| rnd());
        let g1: [f32; 18] = std::array::from_fn(|_| rnd());
        let g2: [f32; 18] = std::array::from_fn(|_| rnd());

        // `imdct_gr` operates on a full 32-band granule buffer; only band 0
        // carries real data; the other 31 stay silent and don't couple back
        // (this transform stage has no cross-band interaction).
        let run = |seq: &[[f32; 18]]| -> Vec<[f32; 18]> {
            let mut history = [0.0f32; 18];
            let mut overlap = [0.0f32; 288];
            let mut outs = Vec::new();
            for &new in seq {
                let mut x = [0.0f32; 36];
                x[..18].copy_from_slice(&history);
                x[18..].copy_from_slice(&new);
                let lines = forward_mdct36(&x);

                let mut grbuf = [0.0f32; 576];
                grbuf[..18].copy_from_slice(&lines);
                imdct::imdct_gr(&mut grbuf, &mut overlap, 0, 32);
                let mut out = [0.0f32; 18];
                out.copy_from_slice(&grbuf[..18]);
                outs.push(out);
                history = new;
            }
            outs
        };

        let outs = run(&[g0, g1, g2]);
        // The derivation (see the module comment) targets a one-block lag:
        // call 2's read-out should reproduce g0 (fed as "new" at call 1),
        // call 3's should reproduce g1. Search rather than assume the exact
        // lag/lead convention, since that's an implementation detail of
        // `imdct_gr`'s internal indexing — but *do* assert a clean match is
        // found (this is the real end-to-end check, through the actual
        // production decode path).
        let err_against = |out: &[f32; 18], target: &[f32; 18]| {
            out.iter()
                .zip(target.iter())
                .map(|(a, b)| (a - b).abs())
                .fold(0.0f32, f32::max)
        };
        let targets = [g0, g1, g2];
        let mut best = (usize::MAX, usize::MAX, f32::MAX);
        for (i, out) in outs.iter().enumerate() {
            for (t, target) in targets.iter().enumerate() {
                let e = err_against(out, target);
                if e < best.2 {
                    best = (i, t, e);
                }
            }
        }
        assert!(
            best.2 < 1e-3,
            "forward MDCT does not invert through the decoder's IMDCT: best match out[{}] vs target[{}] err={}",
            best.0,
            best.1,
            best.2
        );
    }

    #[test]
    fn mdct_window_is_normalized() {
        // The Princen-Bradley condition `w1[i]^2 + w2[i]^2 == 1` the
        // `Mdct36Basis` derivation's per-`i` 2x2 system relies on (see the
        // module comment) — checked directly against the decoder's actual
        // window table rather than assumed.
        let (w1, w2) = mdct_window_halves();
        for i in 0..9 {
            let s = w1[i] * w1[i] + w2[i] * w2[i];
            assert!((s - 1.0).abs() < 1e-6, "i={i} w1^2+w2^2={s}");
        }
    }

    #[test]
    #[allow(clippy::needless_range_loop)] // clearer as index math for 2D matrix ops
    fn mdct_basis_l_round_trips() {
        // `L * L^-1 == I`: a direct sanity check on the probed core
        // transform and its inversion, isolated from the window-scaling
        // step and from any particular granule sequence.
        let l = probe_l();
        let l_inv = mat18_inverse(&l);
        for i in 0..18 {
            for j in 0..18 {
                let mut acc = 0.0f64;
                for k in 0..18 {
                    acc += l[i][k] * l_inv[k][j];
                }
                let expect = if i == j { 1.0 } else { 0.0 };
                assert!(
                    (acc - expect).abs() < 1e-6,
                    "L*L^-1 [{i}][{j}] = {acc}, want {expect}"
                );
            }
        }
    }

    #[test]
    fn encoded_stereo_side_info_orders_tables_before_regions() {
        use crate::{header, sideinfo};
        use std::io::Cursor;

        let sample_rate = 44_100u32;
        let mut samples = vec![0.0f32; 1152 * 2];
        for (i, sample) in samples.iter_mut().step_by(2).enumerate() {
            *sample =
                0.25 * (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / sample_rate as f32).sin();
        }
        let mut output = Cursor::new(Vec::new());
        let mut encoder = Mp3Encoder::new(&mut output, sample_rate, 2, 128).unwrap();
        encoder.encode(&samples).unwrap();
        encoder.finish().unwrap();
        drop(encoder);
        let data = output.into_inner();

        // This content is hard-panned (left-only), so the encoder must keep
        // plain stereo mode (mode bits 00 in byte 3).
        assert_eq!(
            data[3] >> 6,
            0b00,
            "hard-panned content must stay LR stereo"
        );

        let hdr = header::parse_header(&data[..4]).unwrap();
        let mut bits = crate::bitreader::BitReader::new(&data[4..hdr.total_bytes()]);
        let mut granules = std::array::from_fn::<_, 4, _>(|_| sideinfo::GranuleInfo::default());
        sideinfo::read_side_info(&mut bits, &hdr, &mut granules).unwrap();
        assert_eq!(bits.bit_pos(), 32 * 8);
        for granule in &granules {
            // Region tables must be real books (never the unassigned 4/14),
            // and the stored region counts must satisfy the field widths
            // (4-bit and 3-bit count-minus-one) and cover at most 22 bands.
            for ts in granule.table_select {
                assert!(ts <= 31 && ts != 4 && ts != 14, "invalid book {ts}");
            }
            let r0 = granule.region_count[0] as usize + 1;
            let r1 = granule.region_count[1] as usize + 1;
            assert!(r0 <= 16 && r1 <= 8 && r0 + r1 <= N_LONG_SFB);
            // Every granule must claim at least its scalefactor bits.
            let (s1, s2) = slens(granule.scalefac_compress as u8);
            assert!(granule.part_23_length as u64 >= 11 * s1 as u64 + 10 * s2 as u64);
        }
    }

    #[test]
    fn dual_mono_switches_to_mid_side_mode() {
        use std::io::Cursor;

        let sample_rate = 44_100u32;
        let mut samples = vec![0.0f32; 1152 * 2];
        for i in 0..1152usize {
            let s =
                0.25 * (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / sample_rate as f32).sin();
            samples[i * 2] = s;
            samples[i * 2 + 1] = s;
        }
        let mut output = Cursor::new(Vec::new());
        {
            let mut encoder = Mp3Encoder::new(&mut output, sample_rate, 2, 128).unwrap();
            encoder.encode(&samples).unwrap();
            encoder.finish().unwrap();
        }
        let data = output.into_inner();

        // Dual-mono content must engage joint stereo with the mid/side
        // mode extension (mode 01, mode_ext 10 → byte 3 = 0b01_10_0000).
        assert_eq!(data[3] >> 6, 0b01, "dual mono must use joint stereo");
        assert_eq!((data[3] >> 4) & 0b11, 0b10, "mode ext must signal ms only");
    }

    #[test]
    fn planned_granule_decodes_to_planned_reconstruction() {
        // The strongest unit-level parity check: plan a granule with the
        // real planner, emit it with the real writer, decode it with the
        // real decoder, and require the decoded lines to equal the planner's
        // own requantized reconstruction — per-band gains, region mapping,
        // count1 tail, scalefactor write order, and all bit accounting at
        // once.
        use crate::bitreader::BitReader;
        use crate::huffman;
        use crate::scalefac;

        let mut seed = 0xC0FF_EE11u32;
        let mut rnd = move || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            (seed & 0xFFFF) as f32 / 32768.0 * 2.0 - 1.0
        };
        let layout = BandLayout::new(5);
        let mut spec = [0.0f32; GRANULE_SAMPLES];
        // Spectral envelope that exercises every band and a wide dynamic
        // range, plus a sparse 0/1 tail to force count1 usage.
        for (i, s) in spec.iter_mut().enumerate() {
            let band = layout.band_of_line[i] as f32;
            *s = rnd() * 30000.0 / (band + 1.0);
        }
        for (i, s) in spec.iter_mut().enumerate().skip(500).take(20) {
            *s = if i % 3 == 0 {
                rnd().signum() * 1.0
            } else {
                0.0
            };
        }
        let thresholds = psy_thresholds(&spec, &layout, 44_100);
        // A realistic per-slot budget (128 kbps stereo leaves ~990 bits per
        // granule/channel). When even the coarsest plan overshoots, the
        // planner returns its best-effort structure and emit_frame's trim
        // backstop cuts the tail; replicate that here.
        let budget = 990u64;
        let cost = plan_granule(&spec, &layout, &thresholds, false, budget);
        let mut plan = cost.plan;
        while plan.big_values > 0 || plan.count1_quads > 0 {
            if measure_plan(&plan, &spec, &layout, false) <= budget {
                break;
            }
            if plan.count1_quads > 0 {
                plan.count1_quads -= 1;
            } else {
                plan.big_values -= 1;
            }
        }
        plan.part2_3_length = measure_plan(&plan, &spec, &layout, false).min(4095) as u16;
        assert!(plan.part2_3_length as u64 <= budget, "budget exceeded");

        let mut bw = BitWriter::new();
        emit_granule_data(&mut bw, &plan, &spec, &layout, false);
        assert_eq!(bw.bit_pos as u64, plan.part2_3_length as u64);

        let info = crate::sideinfo::GranuleInfo {
            part_23_length: plan.part2_3_length,
            big_values: plan.big_values,
            global_gain: plan.global_gain,
            scalefac_compress: plan.scalefac_compress as u16,
            preflag: plan.preflag,
            table_select: plan.regions.table_select,
            region_count: [
                plan.regions.region_count[0],
                plan.regions.region_count[1],
                255,
            ],
            count1_table: plan.count1_table,
            sfbtab: &crate::tables::SCF_LONG[5],
            n_long_sfb: 22,
            ..Default::default()
        };
        // Scalefactors decoded from the emitted bits (the decoder path).
        let mut bs = BitReader::new(&bw.bytes);
        let mut scf = [0.0f32; 40];
        let mut ist_pos = [0u8; 39];
        let hdr = header::parse_header(&[0xFF, 0xFB, 0x90, 0x40]).unwrap();
        scalefac::decode_scalefactors(&hdr, &mut ist_pos, &mut bs, &info, &mut scf, 0);

        let mut decoded = [0.0f32; GRANULE_SAMPLES];
        let end = huffman::huffman(
            &mut decoded,
            &bw.bytes,
            0,
            &info,
            &scf,
            plan.part2_3_length as i64,
        );
        assert_eq!(end, plan.part2_3_length as usize, "bit accounting");

        // Expected reconstruction: the decoder's dequant of the quantized
        // values (matching `evaluate_granule`'s noise metric); lines past
        // the plan's trimmed coverage decode as exact zeros.
        let gains = band_gains(plan.global_gain, &plan.scalefacs, plan.preflag, false);
        let covered = plan.big_values as usize * 2 + plan.count1_quads as usize * 4;
        let mut expected = [0.0f32; GRANULE_SAMPLES];
        for (i, e) in expected.iter_mut().enumerate() {
            if i >= covered {
                break;
            }
            let gains_ix = quantize_one(spec[i], gains[layout.band_of_line[i] as usize]);
            *e = gains[layout.band_of_line[i] as usize]
                * (gains_ix as f32).powf(4.0 / 3.0)
                * spec[i].signum();
        }
        for i in 0..GRANULE_SAMPLES {
            assert!(
                (decoded[i] - expected[i]).abs() <= expected[i].abs() * 2e-3 + 2e-3,
                "line {i}: decoded {} expected {}",
                decoded[i],
                expected[i]
            );
        }
    }

    #[test]
    fn analysis_history_uses_shine_reverse_fill_order() {
        let mut hist = [0.0f32; HAN_SIZE];
        let mut off = 0usize;
        let new_samples: [f32; 32] = std::array::from_fn(|i| i as f32);
        let mut subbands = [0.0f32; 32];
        analyze_block_polyphase(&mut hist, &mut off, &new_samples, &mut subbands);

        assert_eq!(off, 480);
        for i in 0..32 {
            assert_eq!(hist[(31 - i) as usize], i as f32);
        }
    }

    #[test]
    fn all_band_mdct_precompensation_round_trips() {
        use crate::{imdct, processing};

        let mut seed = 0x6d64_1937u32;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            (seed & 0xFFFF) as f32 / 32768.0 - 1.0
        };
        let granules: [[[f32; 18]; 32]; 5] =
            std::array::from_fn(|_| std::array::from_fn(|_| std::array::from_fn(|_| next())));

        let mut band_history = [[0.0f32; 18]; 32];
        let mut mdct_overlap = [0.0f32; 288];
        let mut reconstructed = Vec::new();
        for granule in granules.iter().take(4) {
            let mut spec = [0.0f32; GRANULE_SAMPLES];
            for band in 0..32usize {
                let mut x = [0.0f32; 36];
                x[..18].copy_from_slice(&band_history[band]);
                for (sample, value) in x[18..].iter_mut().enumerate() {
                    let sign = if band & 1 != 0 && sample & 1 != 0 {
                        -1.0
                    } else {
                        1.0
                    };
                    *value = granule[band][sample] * sign;
                }
                let lines = forward_mdct36(&x);
                spec[band * 18..band * 18 + 18].copy_from_slice(&lines);
                band_history[band].copy_from_slice(&x[18..]);
            }
            for b in 0..31 {
                let base = b * 18;
                for i in 0..8 {
                    let up = spec[base + 18 + i];
                    let dp = spec[base + 17 - i];
                    let aa0 = crate::tables::AA[0][i];
                    let aa1 = crate::tables::AA[1][i];
                    spec[base + 18 + i] = up * aa0 + dp * aa1;
                    spec[base + 17 - i] = -up * aa1 + dp * aa0;
                }
            }
            processing::antialias(&mut spec, 31);
            imdct::imdct_gr(&mut spec, &mut mdct_overlap, 0, 32);
            imdct::change_sign(&mut spec);
            reconstructed.push(spec);
        }

        for g in 1..4usize {
            for band in 0..32usize {
                for i in 0..18usize {
                    let error = (reconstructed[g][band * 18 + i] - granules[g - 1][band][i]).abs();
                    assert!(error < 1e-3, "g={g} band={band} sample={i} error={error}");
                }
            }
        }
    }

    /// Isolates `analyze_block_polyphase` from the MDCT/quantization/Huffman
    /// stages entirely: feeds raw analysis-filter subband output straight
    /// into `crate::synth::dct_ii` + `synth_granule` (skipping
    /// `forward_mdct36`/`imdct_gr` altogether, since both of those are
    /// independently verified elsewhere), and checks the round trip
    /// reconstructs a sine tone with strong correlation. If this fails, the
    /// bug is in `analyze_block_polyphase` (or its mismatch with
    /// `crate::synth`) specifically, not in the MDCT/quant/Huffman chain.
    #[test]
    fn analysis_filter_alone_round_trips_through_synth() {
        use crate::synth;

        let sample_rate = 44_100.0f32;
        let freq = 1000.0f32;
        let n_granules = 40; // 40 * 576 = 23040 samples, plenty to settle.
        let total_samples = n_granules * 576;

        let x: Vec<f32> = (0..total_samples + HAN_SIZE)
            .map(|i| (2.0 * std::f32::consts::PI * freq * i as f32 / sample_rate).sin() * 0.5)
            .collect();

        let mut hist = [0.0f32; HAN_SIZE];
        let mut off = 0usize;
        let mut qmf_state = [0.0f32; 960];
        let mut lins = vec![0.0f32; 33 * 64];
        let mut pcm_out = Vec::with_capacity(total_samples);

        for g in 0..n_granules {
            let mut grbuf = [0.0f32; 576];
            for t in 0..18usize {
                let base = g * 576 + t * 32;
                let mut new_samples = [0.0f32; 32];
                new_samples.copy_from_slice(&x[base..base + 32]);
                let mut subbands = [0.0f32; 32];
                analyze_block_polyphase(&mut hist, &mut off, &new_samples, &mut subbands);
                for band in 0..32usize {
                    grbuf[band * 18 + t] = subbands[band];
                }
            }
            let mut pcm = [0.0f32; 576];
            synth::synth_granule(&mut qmf_state, &mut grbuf, 1, &mut pcm, &mut lins);
            pcm_out.extend_from_slice(&pcm);
        }

        // Cross-correlate against the (delay-compensated) source to find the
        // pipeline's total latency, then check the aligned correlation.
        let skip = 2000; // let history/filter state settle before scoring
        let mut best_corr = -1.0f64;
        let mut best_delay = 0usize;
        for delay in 0..1024usize {
            if delay + skip + 4000 > pcm_out.len() || delay + skip + 4000 > x.len() {
                continue;
            }
            let a = &pcm_out[skip..skip + 4000];
            let b = &x[skip.saturating_sub(delay)..skip.saturating_sub(delay) + 4000];
            let (mut num, mut da, mut db) = (0.0f64, 0.0f64, 0.0f64);
            for (av, bv) in a.iter().zip(b.iter()) {
                num += (*av as f64) * (*bv as f64);
                da += (*av as f64) * (*av as f64);
                db += (*bv as f64) * (*bv as f64);
            }
            let corr = num / (da.sqrt() * db.sqrt() + 1e-12);
            if corr > best_corr {
                best_corr = corr;
                best_delay = delay;
            }
        }

        assert!(
            best_corr > 0.9,
            "analysis-filter-only round trip doesn't correlate with source: \
             best_corr={best_corr} at delay={best_delay}"
        );
    }

    /// Probes `crate::synth`'s true per-band impulse response directly
    /// (bypassing `analyze_block_polyphase` entirely): sets one subband's
    /// one time-sample to 1.0, runs `dct_ii`+`synth_granule`, and reports
    /// where the resulting PCM energy is concentrated. This tells us what
    /// envelope the *decoder* actually expects for band 0, independent of
    /// whatever analysis-side table/formula we transcribe.
    #[test]
    fn synth_single_band_impulse_response_diagnostic() {
        use crate::synth;
        let band = 0usize;
        let n_granules = 4;
        let mut qmf_state = [0.0f32; 960];
        let mut lins = vec![0.0f32; 33 * 64];
        let mut pcm_out = Vec::new();
        for g in 0..n_granules {
            let mut grbuf = [0.0f32; 576];
            if g == 0 {
                grbuf[band * 18] = 1.0; // t=0 of the chosen band
            }
            let mut pcm = [0.0f32; 576];
            synth::synth_granule(&mut qmf_state, &mut grbuf, 1, &mut pcm, &mut lins);
            pcm_out.extend_from_slice(&pcm);
        }
        let nz: Vec<(usize, f32)> = pcm_out
            .iter()
            .enumerate()
            .filter(|(_, v)| v.abs() > 1e-6)
            .map(|(i, &v)| (i, v))
            .collect();
        eprintln!(
            "band {band} t=0 impulse -> {} nonzero PCM samples",
            nz.len()
        );
        if let (Some(&(first, _)), Some(&(last, _))) = (nz.first(), nz.last()) {
            eprintln!("span: [{first}, {last}] (width {})", last - first + 1);
        }
        for (i, v) in nz.iter().take(20) {
            eprintln!("  [{i}] = {v}");
        }
        eprintln!("  ...");
        for (i, v) in nz
            .iter()
            .rev()
            .take(20)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
        {
            eprintln!("  [{i}] = {v}");
        }
    }

    #[test]
    fn analysis_filter_impulse_response_diagnostic() {
        use crate::synth;

        let n_granules = 10;
        let total_samples = n_granules * 576;
        let impulse_pos = 3000usize;
        let mut x = vec![0.0f32; total_samples + HAN_SIZE];
        x[impulse_pos] = 1.0;

        let mut hist = [0.0f32; HAN_SIZE];
        let mut off = 0usize;
        let mut qmf_state = [0.0f32; 960];
        let mut lins = vec![0.0f32; 33 * 64];
        let mut pcm_out = Vec::with_capacity(total_samples);

        for g in 0..n_granules {
            let mut grbuf = [0.0f32; 576];
            for t in 0..18usize {
                let base = g * 576 + t * 32;
                let mut new_samples = [0.0f32; 32];
                new_samples.copy_from_slice(&x[base..base + 32]);
                let mut subbands = [0.0f32; 32];
                analyze_block_polyphase(&mut hist, &mut off, &new_samples, &mut subbands);
                for band in 0..32usize {
                    let sign = if band & 1 != 0 && t & 1 != 0 {
                        -1.0
                    } else {
                        1.0
                    };
                    grbuf[band * 18 + t] = subbands[band] * sign;
                }
            }
            let mut pcm = [0.0f32; 576];
            synth::synth_granule(&mut qmf_state, &mut grbuf, 1, &mut pcm, &mut lins);
            pcm_out.extend_from_slice(&pcm);
        }

        let (mut peak_idx, mut peak_val) = (0usize, 0.0f32);
        for (i, &v) in pcm_out.iter().enumerate() {
            if v.abs() > peak_val.abs() {
                peak_val = v;
                peak_idx = i;
            }
        }
        eprintln!(
            "impulse at {impulse_pos}, peak {peak_val} at {peak_idx} (delay {})",
            peak_idx as i64 - impulse_pos as i64
        );
        let lo = peak_idx.saturating_sub(20);
        let hi = (peak_idx + 20).min(pcm_out.len());
        for (i, v) in pcm_out.iter().enumerate().take(hi).skip(lo) {
            eprintln!("  [{i}] (rel {}) = {v}", i as i64 - impulse_pos as i64);
        }
    }
}
