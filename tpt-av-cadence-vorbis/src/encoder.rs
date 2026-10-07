//! Ogg Vorbis I encoder.
//!
//! A compact, fixed-configuration encoder built on the decoder's own parsed
//! structures:
//!
//! - **Two block sizes with block switching**: long (2048 samples) and short
//!   (256) blocks, two modes and two mappings (one per size). A transient
//!   detector (highpassed energy rise over the region the next long block
//!   would newly cover) starts a run of short blocks that walks up to the
//!   attack and ends after it; the Vorbis window shapes (long slopes next to
//!   long blocks, short slopes next to short ones) follow from each block's
//!   neighbours. Stereo uses square-polar channel coupling.
//! - **Floor 1** (34 posts for long blocks, 16 for short) fitted to a
//!   masking threshold minus a quality-dependent offset. The encoder writes
//!   each floor, then decodes it back through the real [`Floor1::decode`] so
//!   the residue is computed against the *exact* curve the decoder will
//!   apply.
//! - **Residue type 1** with four partition classes (silent / +-1 / +-5 /
//!   coarse+fine cascade) coded through static Huffman-shaped VQ books.
//! - Codebooks, floor and residue configuration are written into the setup
//!   header and immediately parsed back with [`parse_setup`], so an
//!   internally inconsistent header fails at construction, not in a player.
//!
//! Noise shaping follows a psychoacoustic model (`psy`): Bark-band power
//! and spectral flatness, Schroeder spreading, tonality-dependent masking
//! offsets and the absolute threshold of hearing, floored per bin and turned
//! into floor-1 post targets. It is a standard model with unvalidated
//! constants — no listening tests back it — and an objective band-SNR proxy
//! puts it level with the envelope-tracking rule it replaced, so treat the
//! quality ladder as calibrated for bitrate, not proven perceptually.
//! Per-file adaptive Huffman books are available through
//! `VorbisEncoder::new_adaptive` (whole-stream buffering; see there).
//!
//! Stream layout: one Ogg packet per page (`OggPageWriter`), granule
//! positions trimming the final packet to the exact input length.

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::io::Write;

use tpt_av_cadence_core::{CadenceError, Encoder, Result};
use tpt_av_cadence_ogg::OggPageWriter;

use crate::bitreader::BitReader;
use crate::fft::{Fft, C};
use crate::floor::{Floor, Floor1, FLOOR1_INVERSE_DB};
use crate::header::{parse_id, parse_setup, Setup};
use crate::psy::Psy;

const BS_LONG_EXP: u32 = 11;
const BS_SHORT_EXP: u32 = 8;
/// Long block size / coefficient count.
const N: usize = 1 << BS_LONG_EXP;
const M: usize = N / 2;
/// Short block size / coefficient count.
const SN: usize = 1 << BS_SHORT_EXP;
const SM: usize = SN / 2;
const FLOOR_RANGE: i32 = 128;
const FLOOR_RANGE_BITS: u32 = 7;
/// Floor step-to-mask ratio (dB) at quality 0, and its decrease per quality
/// step. Calibrated so the quality ladder spans roughly 60-180 kbps.
const FLOOR_GAIN_DB_Q0: f64 = 12.0;
const FLOOR_GAIN_DB_PER_Q: f64 = 2.0;
const SERIAL: u32 = 0x5450_5456;
/// Residue partition size and class count.
const PARTITION: usize = 16;
const CLASSES: usize = 4;
const CLASSWORDS: usize = 2;

/// Floor post positions (after the fixed 0 and `M` end posts), grouped four
/// per partition.
const POSTS: [u32; 32] = [
    3, 6, 9, 12, 16, 20, 24, 28, 34, 40, 48, 56, 64, 74, 86, 100, 116, 134, 156, 180, 210, 244,
    284, 330, 384, 448, 520, 600, 690, 790, 900, 1010,
];

/// Floor post positions for the short block size (end post is `SM`).
const SHORT_POSTS: [u32; 16] = [2, 4, 6, 8, 10, 13, 16, 20, 25, 31, 38, 47, 58, 72, 90, 110];

// Codebook indices in the setup header.
const BOOK_FLOOR: usize = 0;
const BOOK_CLASS: usize = 1;
const BOOK_B1: usize = 2;
const BOOK_B2: usize = 3;
const BOOK_B3: usize = 4;
const NBOOKS: usize = 5;

/// LSB-first bit packer (Vorbis bit order).
#[derive(Default, Clone)]
struct BitWriter {
    buf: Vec<u8>,
    acc: u64,
    nbits: u32,
}

impl BitWriter {
    fn write(&mut self, value: u32, n: u32) {
        debug_assert!(n <= 32);
        let mask = if n == 32 {
            u64::MAX >> 32
        } else {
            (1u64 << n) - 1
        };
        self.acc |= (u64::from(value) & mask) << self.nbits;
        self.nbits += n;
        while self.nbits >= 8 {
            self.buf.push(self.acc as u8);
            self.acc >>= 8;
            self.nbits -= 8;
        }
    }

    fn finish(mut self) -> Vec<u8> {
        if self.nbits > 0 {
            self.buf.push(self.acc as u8);
        }
        self.buf
    }
}

/// The spec's `float32_pack`.
fn float32_pack(v: f64) -> u32 {
    if v == 0.0 {
        return 0;
    }
    let sign = if v < 0.0 { 0x8000_0000u32 } else { 0 };
    let a = v.abs();
    let mut e = a.log2().floor() as i32;
    let mut mant = (a * 2f64.powi(20 - e)).round() as u32;
    if mant >= 1 << 21 {
        mant >>= 1;
        e += 1;
    }
    sign | (((e - 20 + 788) as u32) << 21) | mant
}

/// Huffman code lengths for the given positive weights (every entry used,
/// complete prefix tree).
fn huffman_lengths(weights: &[f64]) -> Result<Vec<u8>> {
    let n = weights.len();
    if n == 1 {
        return Ok(vec![1]);
    }
    let max = weights.iter().cloned().fold(0.0f64, f64::max);
    let mut heap = BinaryHeap::new();
    let mut parent = vec![usize::MAX; 2 * n - 1];
    for (i, &w) in weights.iter().enumerate() {
        heap.push(Reverse(((((w / max) * 1e7) as u64).max(1), i)));
    }
    let mut next = n;
    while heap.len() > 1 {
        let Reverse((wa, a)) = heap.pop().unwrap();
        let Reverse((wb, b)) = heap.pop().unwrap();
        parent[a] = next;
        parent[b] = next;
        heap.push(Reverse((wa + wb, next)));
        next += 1;
    }
    let mut lengths = vec![0u8; n];
    for (i, len) in lengths.iter_mut().enumerate() {
        let (mut depth, mut node) = (0u32, i);
        while parent[node] != usize::MAX {
            node = parent[node];
            depth += 1;
        }
        if depth > 32 {
            return Err(CadenceError::InvalidFormat(
                "internal Huffman length overflow".into(),
            ));
        }
        *len = depth as u8;
    }
    Ok(lengths)
}

/// Vorbis codeword assignment: each entry, in order, takes the leftmost
/// free node at its length.
fn assign_codes(lengths: &[u8]) -> Vec<u32> {
    #[derive(Default, Clone)]
    struct Node {
        child: [Option<usize>; 2],
        used: bool,
    }
    fn place(nodes: &mut Vec<Node>, at: usize, depth: u8, target: u8, code: u32) -> Option<u32> {
        if nodes[at].used {
            return None;
        }
        if depth == target {
            if nodes[at].child.iter().any(Option::is_some) {
                return None;
            }
            nodes[at].used = true;
            return Some(code);
        }
        for bit in 0..2usize {
            let c = match nodes[at].child[bit] {
                Some(c) => c,
                None => {
                    nodes.push(Node::default());
                    let c = nodes.len() - 1;
                    nodes[at].child[bit] = Some(c);
                    c
                }
            };
            if let Some(found) = place(nodes, c, depth + 1, target, (code << 1) | bit as u32) {
                return Some(found);
            }
        }
        None
    }
    let mut nodes = vec![Node::default()];
    lengths
        .iter()
        .map(|&l| {
            if l == 0 {
                0
            } else {
                place(&mut nodes, 0, 0, l, 0).expect("complete tree has room")
            }
        })
        .collect()
}

/// An encoder-side codebook: code lengths plus the assigned codewords.
struct EncBook {
    lengths: Vec<u8>,
    codes: Vec<u32>,
}

impl EncBook {
    fn new(weights: &[f64]) -> Result<Self> {
        let lengths = huffman_lengths(weights)?;
        let codes = assign_codes(&lengths);
        Ok(EncBook { lengths, codes })
    }

    /// Writes `entry`'s codeword MSB-first (the decoder walks the tree
    /// from the first bit read).
    fn put(&self, bw: &mut BitWriter, entry: usize) {
        let len = u32::from(self.lengths[entry]);
        let code = self.codes[entry];
        for i in (0..len).rev() {
            bw.write((code >> i) & 1, 1);
        }
    }
}

/// A lattice VQ book: `dim` values per entry drawn from
/// `-n*step ..= n*step`.
struct VqBook {
    book: EncBook,
    dim: usize,
    n: i32,
    step: i32,
}

impl VqBook {
    fn new(dim: usize, n: i32, step: i32, theta: f64) -> Result<Self> {
        let vals = (2 * n + 1) as usize;
        let entries = vals.pow(dim as u32);
        let weights: Vec<f64> = (0..entries)
            .map(|e| {
                let mut rest = e;
                let mut w = 1.0;
                for _ in 0..dim {
                    let v = (rest % vals) as i32 - n;
                    rest /= vals;
                    w *= (-(v.abs() as f64) / theta).exp();
                }
                w
            })
            .collect();
        Self::with_weights(dim, n, step, &weights)
    }

    /// A book whose code lengths follow measured entry `weights`.
    fn with_weights(dim: usize, n: i32, step: i32, weights: &[f64]) -> Result<Self> {
        Ok(VqBook {
            book: EncBook::new(weights)?,
            dim,
            n,
            step,
        })
    }

    /// Entry index of one vector of already-step-divided integer digits.
    fn entry(&self, digits: &[i32]) -> usize {
        let vals = (2 * self.n + 1) as usize;
        let mut entry = 0usize;
        let mut mul = 1usize;
        for &d in &digits[..self.dim] {
            entry += (d + self.n) as usize * mul;
            mul *= vals;
        }
        entry
    }
}

/// Which codebook a [`Tok`] indexes (`RAW` = literal bits).
const T_RAW: u8 = 255;

/// One packet element: literal bits, or a codebook entry resolved to a
/// codeword only when the books are final (so per-file books can be trained
/// on the whole stream before any packet is rendered).
#[derive(Clone, Copy)]
struct Tok {
    book: u8,
    n: u8,
    v: u32,
}

impl Tok {
    fn bits(v: u32, n: u32) -> Self {
        Tok {
            book: T_RAW,
            n: n as u8,
            v,
        }
    }

    fn sym(book: usize, entry: usize) -> Self {
        Tok {
            book: book as u8,
            n: 0,
            v: entry as u32,
        }
    }
}

/// The five codebooks of the setup header, in header order.
struct Books {
    floor: EncBook,
    class: EncBook,
    b1: VqBook,
    b2: VqBook,
    b3: VqBook,
}

impl Books {
    /// The fixed, hand-shaped books used by streaming encodes.
    fn static_books() -> Result<Self> {
        let floor_weights: Vec<f64> = (0..FLOOR_RANGE)
            .map(|v| 1.0 / (1.0 + f64::from(v)).powf(1.3))
            .collect();
        let class_p = [0.5, 0.25, 0.15, 0.10];
        let class_weights: Vec<f64> = (0..CLASSES * CLASSES)
            .map(|e| class_p[e / CLASSES] * class_p[e % CLASSES])
            .collect();
        Ok(Books {
            floor: EncBook::new(&floor_weights)?,
            class: EncBook::new(&class_weights)?,
            b1: VqBook::new(4, 1, 1, 0.55)?,
            b2: VqBook::new(2, 5, 1, 2.2)?,
            b3: VqBook::new(1, 31, 8, 4.0)?,
        })
    }

    /// Books trained on measured per-entry usage `counts` (indexed by
    /// `BOOK_*`). Every entry keeps a codeword (a Vorbis lattice book must
    /// be able to name every vector); the weight floor bounds the code
    /// length well inside the format's 32-bit limit.
    fn trained(counts: &[Vec<u64>; NBOOKS]) -> Result<Self> {
        fn weights(c: &[u64]) -> Vec<f64> {
            let total: u64 = c.iter().sum();
            let floor = (total as f64 / 65_536.0).max(1.0);
            c.iter().map(|&x| (x as f64).max(floor)).collect()
        }
        let proto = Self::static_books()?;
        Ok(Books {
            floor: EncBook::new(&weights(&counts[BOOK_FLOOR]))?,
            class: EncBook::new(&weights(&counts[BOOK_CLASS]))?,
            b1: VqBook::with_weights(
                proto.b1.dim,
                proto.b1.n,
                proto.b1.step,
                &weights(&counts[BOOK_B1]),
            )?,
            b2: VqBook::with_weights(
                proto.b2.dim,
                proto.b2.n,
                proto.b2.step,
                &weights(&counts[BOOK_B2]),
            )?,
            b3: VqBook::with_weights(
                proto.b3.dim,
                proto.b3.n,
                proto.b3.step,
                &weights(&counts[BOOK_B3]),
            )?,
        })
    }

    fn book(&self, idx: u8) -> &EncBook {
        match usize::from(idx) {
            BOOK_FLOOR => &self.floor,
            BOOK_CLASS => &self.class,
            BOOK_B1 => &self.b1.book,
            BOOK_B2 => &self.b2.book,
            _ => &self.b3.book,
        }
    }

    /// Number of entries of each book, for histogram sizing.
    fn entry_counts(&self) -> [usize; NBOOKS] {
        [
            self.floor.lengths.len(),
            self.class.lengths.len(),
            self.b1.book.lengths.len(),
            self.b2.book.lengths.len(),
            self.b3.book.lengths.len(),
        ]
    }

    /// Packs a token stream into packet bytes.
    fn render(&self, toks: &[Tok]) -> Vec<u8> {
        let mut bw = BitWriter::default();
        for t in toks {
            if t.book == T_RAW {
                bw.write(t.v, u32::from(t.n));
            } else {
                self.book(t.book).put(&mut bw, t.v as usize);
            }
        }
        bw.finish()
    }
}

fn write_codebook_header(bw: &mut BitWriter, dim: usize, lengths: &[u8]) {
    bw.write(0x56_4342, 24);
    bw.write(dim as u32, 16);
    bw.write(lengths.len() as u32, 24);
    bw.write(0, 1); // unordered
    bw.write(0, 1); // not sparse
    for &l in lengths {
        bw.write(u32::from(l) - 1, 5);
    }
}

fn write_scalar_book(bw: &mut BitWriter, dim: usize, b: &EncBook) {
    write_codebook_header(bw, dim, &b.lengths);
    bw.write(0, 4); // no lookup
}

fn write_vq_book(bw: &mut BitWriter, v: &VqBook) {
    write_codebook_header(bw, v.dim, &v.book.lengths);
    bw.write(1, 4); // lattice lookup
    bw.write(float32_pack(f64::from(-v.n * v.step)), 32);
    bw.write(float32_pack(f64::from(v.step)), 32);
    bw.write(5, 4); // 6-bit multiplicands
    bw.write(0, 1);
    for m in 0..(2 * v.n + 1) {
        bw.write(m as u32, 6);
    }
}

fn header_packet(kind: u8, body: &[u8]) -> Vec<u8> {
    let mut p = vec![kind];
    p.extend_from_slice(b"vorbis");
    p.extend_from_slice(body);
    p
}

/// Vorbis window slope (`length` samples rising from 0 to 1).
fn slope(length: usize) -> Vec<f32> {
    (0..length)
        .map(|i| {
            let s = (std::f64::consts::PI * (i as f64 + 0.5) / (2.0 * length as f64)).sin();
            (0.5 * std::f64::consts::PI * s * s).sin() as f32
        })
        .collect()
}

/// Forward MDCT for one block size.
struct Transform {
    n: usize,
    fft: Fft,
    pre: Vec<C>,
    post: Vec<C>,
    /// Reused FFT scratch (input and output sides of `Fft::run`).
    g: Vec<C>,
    o: Vec<C>,
}

impl Transform {
    fn new(n: usize) -> Self {
        let nf = n as f64;
        let pre = (0..n)
            .map(|k| {
                let a = -std::f64::consts::PI * k as f64 / nf;
                C::new(a.cos() as f32, a.sin() as f32)
            })
            .collect();
        let n0 = 0.5 + nf / 4.0;
        let post = (0..n / 2)
            .map(|k| {
                let a = -2.0 * std::f64::consts::PI * n0 * (k as f64 + 0.5) / nf;
                C::new(a.cos() as f32, a.sin() as f32)
            })
            .collect();
        Transform {
            n,
            fft: Fft::new(n, true),
            pre,
            post,
            g: vec![C::default(); n],
            o: vec![C::default(); n],
        }
    }

    /// MDCT of `n` already-windowed samples into `n / 2` coefficients.
    fn forward(&mut self, x: &[f32], out: &mut [f32]) {
        let n = self.n;
        let scale = 4.0 / n as f32;
        for (g, (&x, pre)) in self.g.iter_mut().zip(x.iter().zip(&self.pre)) {
            *g = C::new(x * pre.re, x * pre.im);
        }
        self.fft.run(&self.g, &mut self.o);
        for (o, (&v, post)) in out.iter_mut().zip(self.o.iter().zip(&self.post)) {
            *o = scale * (v.re * post.re - v.im * post.im);
        }
    }
}

/// Which block is being coded and how its neighbors are sized (the
/// neighbors decide the window shape).
#[derive(Clone, Copy)]
struct BlockSpec {
    long: bool,
    prev_long: bool,
    next_long: bool,
}

/// MDCT coefficient amplitude of a full-scale sine in a long block (the
/// 96 dB SPL reference of the masking model): the maximum over a few tone
/// phases.
fn full_scale_amplitude(tr: &mut Transform, long_slope: &[f32]) -> f64 {
    let n = tr.n;
    let mut best = 0.0f32;
    for phase in 0..8 {
        let ph = phase as f64 * std::f64::consts::PI / 8.0;
        let x: Vec<f32> = (0..n)
            .map(|i| {
                let w = if i < n / 2 {
                    long_slope[i]
                } else {
                    long_slope[n - 1 - i]
                };
                let t = 2.0 * std::f64::consts::PI * 44.5 * (i as f64 + 0.5) / n as f64;
                w * (t + ph).sin() as f32
            })
            .collect();
        let mut out = vec![0.0f32; n / 2];
        tr.forward(&x, &mut out);
        best = best.max(out.iter().fold(0.0f32, |m, v| m.max(v.abs())));
    }
    f64::from(best)
}

/// Deferred-output state of the per-file adaptive mode.
struct Adaptive {
    id_packet: Vec<u8>,
    comment_packet: Vec<u8>,
    packets: Vec<(Vec<Tok>, i64)>,
}

/// Ogg Vorbis encoder.
pub struct VorbisEncoder<W: Write + Send> {
    sink: W,
    channels: usize,
    page_writer: OggPageWriter,
    setup: Setup,
    /// Static books: the stream's books in streaming mode, and the
    /// provisional books used to round-trip floors in adaptive mode.
    books: Books,
    /// Adaptive mode: packets held as tokens until `finish` trains the
    /// books, with the header packets to write ahead of them.
    adaptive: Option<Adaptive>,
    /// `[short, long]` transforms and window slopes.
    transforms: [Transform; 2],
    /// Cached windows per (long, prev_long, next_long) combination — at
    /// most eight distinct shapes, each depending only on the slopes.
    windows: [Option<Vec<f32>>; 8],
    slopes: [Vec<f32>; 2],
    /// Masking model deriving per-bin noise thresholds from each spectrum.
    psy: Psy,
    /// Floor amplitude relative to the masking threshold's amplitude
    /// (quantization-step-to-mask ratio; lower = finer quantization).
    floor_gain: f32,
    /// First bin forced to zero (lowpass) for the long block size.
    cutoff_bin: usize,
    /// Input history; `inbuf[c][0]` sits at absolute (padded) index
    /// `buf_start`. Absolute index 0 is `M` samples before the first real
    /// sample, so the first (long) block's center is the first real sample.
    inbuf: Vec<Vec<f32>>,
    buf_start: usize,
    /// Absolute index of the next block's center.
    center: usize,
    cur_long: bool,
    prev_long: bool,
    /// Frames submitted so far.
    total_in: u64,
    /// Packets emitted so far.
    packets: u64,
    pending: Option<(Vec<u8>, i64)>,
    finished: bool,
    /// Set once the packet whose granule reaches `total_in` was emitted.
    done: bool,
    /// Blocks coded short so far.
    short_blocks: u64,
}

/// Samples of look-ahead past a block's boundary point used by the
/// transient detector.
const LOOKAHEAD: usize = 1088;
/// Transient detector sub-block length.
const DET_BLOCK: usize = 64;

/// Writes one floor-1 configuration (4 posts per partition).
fn write_floor_config(s: &mut BitWriter, posts: &[u32], rangebits: u32) {
    s.write(1, 16); // floor type 1
    s.write(posts.len() as u32 / 4, 5); // partitions
    for _ in 0..posts.len() / 4 {
        s.write(0, 4); // all class 0
    }
    s.write(3, 3); // class dimension 4
    s.write(0, 2); // no subclasses
    s.write(BOOK_FLOOR as u32 + 1, 8);
    s.write(1, 2); // multiplier 2
    s.write(rangebits, 4);
    for &x in posts {
        s.write(x, rangebits);
    }
}

/// Writes one residue-1 configuration covering `end` coefficients.
fn write_residue_config(s: &mut BitWriter, end: usize) {
    s.write(1, 16);
    s.write(0, 24);
    s.write(end as u32, 24);
    s.write(PARTITION as u32 - 1, 24);
    s.write(CLASSES as u32 - 1, 6);
    s.write(BOOK_CLASS as u32, 8);
    for cascade in [0u32, 1, 1, 3] {
        s.write(cascade, 3);
        s.write(0, 1);
    }
    for book in [BOOK_B1, BOOK_B2, BOOK_B3, BOOK_B2] {
        s.write(book as u32, 8);
    }
}

/// Writes one mapping-0 configuration using `floor`/`residue`.
fn write_mapping_config(s: &mut BitWriter, ch: usize, floor: u32, residue: u32) {
    s.write(0, 16);
    s.write(0, 1); // one submap
    if ch == 2 {
        s.write(1, 1);
        s.write(0, 8); // one coupling step
        s.write(0, 1); // magnitude = channel 0
        s.write(1, 1); // angle = channel 1
    } else {
        s.write(0, 1);
    }
    s.write(0, 2);
    s.write(0, 8);
    s.write(floor, 8);
    s.write(residue, 8);
}

/// Identification header packet.
fn id_header(sample_rate: u32, channels: u16) -> Vec<u8> {
    let mut id = BitWriter::default();
    id.write(0, 32);
    id.write(u32::from(channels), 8);
    id.write(sample_rate, 32);
    id.write(0, 32);
    id.write(0, 32);
    id.write(0, 32);
    id.write(BS_SHORT_EXP | (BS_LONG_EXP << 4), 8);
    id.write(1, 1);
    header_packet(1, &id.finish())
}

/// Comment header packet.
fn comment_header() -> Vec<u8> {
    // --- Comment header ---
    let mut cm = BitWriter::default();
    let vendor = b"tpt-cadence";
    cm.write(vendor.len() as u32, 32);
    for &b in vendor {
        cm.write(u32::from(b), 8);
    }
    cm.write(0, 32);
    cm.write(1, 1);
    header_packet(3, &cm.finish())
}

/// Setup header packet for the given books.
fn setup_header(ch: usize, books: &Books) -> Vec<u8> {
    // --- Setup header ---
    let mut s = BitWriter::default();
    s.write(4, 8); // 5 codebooks
    write_scalar_book(&mut s, 1, &books.floor);
    write_scalar_book(&mut s, CLASSWORDS, &books.class);
    write_vq_book(&mut s, &books.b1);
    write_vq_book(&mut s, &books.b2);
    write_vq_book(&mut s, &books.b3);
    s.write(0, 6); // one time-domain placeholder
    s.write(0, 16);
    // Floors: 0 = long block, 1 = short block.
    s.write(1, 6);
    write_floor_config(&mut s, &POSTS, 10);
    write_floor_config(&mut s, &SHORT_POSTS, 7);
    // Residues: 0 = long, 1 = short.
    s.write(1, 6);
    write_residue_config(&mut s, M);
    write_residue_config(&mut s, SM);
    // Mappings: 0 = long, 1 = short.
    s.write(1, 6);
    write_mapping_config(&mut s, ch, 0, 0);
    write_mapping_config(&mut s, ch, 1, 1);
    // Modes: 0 = short block (mapping 1), 1 = long block (mapping 0).
    s.write(1, 6);
    for (blockflag, mapping) in [(0u32, 1u32), (1, 0)] {
        s.write(blockflag, 1);
        s.write(0, 16);
        s.write(0, 16);
        s.write(mapping, 8);
    }
    s.write(1, 1);
    header_packet(5, &s.finish())
}

impl<W: Write + Send> VorbisEncoder<W> {
    /// Opens a streaming stream with the fixed codebooks: packets are
    /// written as they are produced. `quality` runs 0.0 (smallest) to 10.0
    /// (best).
    pub fn new(sink: W, sample_rate: u32, channels: u16, quality: f32) -> Result<Self> {
        Self::open(sink, sample_rate, channels, quality, false)
    }

    /// Opens a stream whose Huffman codebooks are trained on the file's own
    /// symbol statistics. The headers precede the audio and need the final
    /// books, so every packet is held (as tokens) until [`Encoder::finish`],
    /// which writes the whole stream: memory grows with the input, and
    /// nothing reaches the sink before `finish`. Same audio, fewer bits.
    pub fn new_adaptive(sink: W, sample_rate: u32, channels: u16, quality: f32) -> Result<Self> {
        Self::open(sink, sample_rate, channels, quality, true)
    }

    fn open(
        mut sink: W,
        sample_rate: u32,
        channels: u16,
        quality: f32,
        adaptive: bool,
    ) -> Result<Self> {
        if !(1..=2).contains(&channels) {
            return Err(CadenceError::InvalidFormat(format!(
                "Vorbis encoder supports 1 or 2 channels, got {channels}"
            )));
        }
        if sample_rate == 0 {
            return Err(CadenceError::InvalidFormat("zero sample rate".into()));
        }
        let quality = quality.clamp(0.0, 10.0);
        let ch = usize::from(channels);

        let books = Books::static_books()?;
        let id_packet = id_header(sample_rate, channels);
        let comment_packet = comment_header();
        let setup_packet = setup_header(ch, &books);

        let id_parsed = parse_id(&id_packet)?;
        let setup = parse_setup(&setup_packet, &id_parsed)?;

        let mut page_writer = OggPageWriter::new(SERIAL);
        let adaptive = if adaptive {
            Some(Adaptive {
                id_packet,
                comment_packet,
                packets: Vec::new(),
            })
        } else {
            sink.write_all(&page_writer.write_page(&id_packet, 0, true, false))?;
            sink.write_all(&page_writer.write_page(&comment_packet, 0, false, false))?;
            sink.write_all(&page_writer.write_page(&setup_packet, 0, false, false))?;
            None
        };

        let mut transforms = [Transform::new(SN), Transform::new(N)];
        let slopes = [slope(SM), slope(M)];
        let amp_ref = full_scale_amplitude(&mut transforms[1], &slopes[1]);
        let psy = Psy::new(sample_rate, amp_ref, SM, M);
        let gain_db = FLOOR_GAIN_DB_Q0 - FLOOR_GAIN_DB_PER_Q * f64::from(quality);
        let cutoff_hz = 12_000.0 + 1_000.0 * f64::from(quality);
        let cutoff_bin = ((cutoff_hz / (f64::from(sample_rate) / 2.0)) * M as f64) as usize;

        Ok(VorbisEncoder {
            sink,
            channels: ch,
            page_writer,
            setup,
            books,
            adaptive,
            transforms,
            windows: [const { None }; 8],
            slopes,
            psy,
            floor_gain: 10f64.powf(gain_db / 20.0) as f32,
            cutoff_bin: cutoff_bin.min(M),
            inbuf: vec![vec![0.0; M]; ch],
            buf_start: 0,
            center: M,
            cur_long: true,
            prev_long: true,
            total_in: 0,
            packets: 0,
            pending: None,
            finished: false,
            done: false,
            short_blocks: 0,
        })
    }

    /// Number of blocks coded with the short block size so far.
    pub fn short_block_count(&self) -> u64 {
        self.short_blocks
    }

    /// Cache slot index for a block spec.
    fn window_slot(spec: BlockSpec) -> usize {
        (usize::from(spec.long) << 2)
            | (usize::from(spec.prev_long) << 1)
            | usize::from(spec.next_long)
    }

    /// The Vorbis window for a block of the given size and neighbors.
    fn build_window(&self, spec: BlockSpec) -> Vec<f32> {
        let n = if spec.long { N } else { SN };
        let half = n / 2;
        let mut w = vec![0.0f32; n];
        // A long block next to a short one uses the short slope on that
        // side (centered at a quarter of the block); every other side uses
        // the block's own full-width slope.
        let fill = |out: &mut [f32], neighbor_long: bool| {
            let sl = if spec.long && neighbor_long {
                &self.slopes[1]
            } else {
                &self.slopes[0]
            };
            let start = n / 4 - sl.len() / 2;
            for (i, slot) in out.iter_mut().enumerate() {
                *slot = if i < start {
                    0.0
                } else if i >= start + sl.len() {
                    1.0
                } else {
                    sl[i - start]
                };
            }
        };
        fill(&mut w[..half], spec.prev_long);
        let mut right = vec![0.0f32; half];
        fill(&mut right, spec.next_long);
        for (j, &v) in right.iter().enumerate() {
            w[n - 1 - j] = v;
        }
        w
    }

    /// True when a sharp energy rise sits in the region the *next* block
    /// would newly cover, starting at boundary point `b`.
    fn attack_ahead(&self, b: usize) -> bool {
        let from = b - 256;
        let count = (b + LOOKAHEAD - from) / DET_BLOCK;
        let mut energy = vec![0.0f32; count];
        for (k, e) in energy.iter_mut().enumerate() {
            let s0 = from + k * DET_BLOCK - self.buf_start;
            let mut acc = 0.0f32;
            for i in s0..s0 + DET_BLOCK {
                let mut hp = 0.0f32;
                for c in 0..self.channels {
                    hp += self.inbuf[c][i] - self.inbuf[c][i - 1];
                }
                acc += hp * hp;
            }
            *e = acc;
        }
        let abs_floor = DET_BLOCK as f32 * 1e-4;
        // Sub-blocks 3.. cover [b - 64, ...): where a short block's
        // support could begin.
        (3..count).any(|k| {
            let prev = (energy[k - 3] + energy[k - 2] + energy[k - 1]) / 3.0;
            energy[k] > 10.0 * prev + abs_floor
        })
    }

    /// Floor 1 post values (`Y` per x-list entry) for a spectrum of `m`
    /// coefficients.
    fn fit_floor(&self, thr: &[f32], floor_idx: usize) -> Vec<i32> {
        let m = thr.len();
        let f1 = self.floor1(floor_idx);
        let min_hw = if m == M { 3 } else { 1 };
        f1.x.iter()
            .map(|&x| {
                let pos = x as usize;
                let hw = (pos / 5).max(min_hw);
                let lo = pos.saturating_sub(hw).min(m - 1);
                let hi = (pos + hw).min(m - 1);
                // Log-mean masking power over the post's neighbourhood.
                let log_mean = thr[lo..=hi]
                    .iter()
                    .map(|v| f64::from(v.max(1e-20)).ln())
                    .sum::<f64>()
                    / (hi - lo + 1) as f64;
                let target = (log_mean.exp().sqrt() as f32 * self.floor_gain).max(1e-5);
                let idx = FLOOR1_INVERSE_DB.partition_point(|&t| t < target);
                let idx = if idx > 0
                    && idx < 256
                    && (target / FLOOR1_INVERSE_DB[idx - 1]) < (FLOOR1_INVERSE_DB[idx] / target)
                {
                    idx - 1
                } else {
                    idx.min(255)
                };
                ((idx as i32 + 1) / 2).min(FLOOR_RANGE - 1)
            })
            .collect()
    }

    fn floor1(&self, idx: usize) -> &Floor1 {
        match &self.setup.floors[idx] {
            Floor::One(f) => f,
            Floor::Zero(_) => unreachable!("setup declares floor 1"),
        }
    }

    /// Serializes floor 1 for post values `y` (audible-channel flag set).
    fn write_floor(&self, bw: &mut Vec<Tok>, y: &[i32], floor_idx: usize) {
        let f1 = self.floor1(floor_idx);
        bw.push(Tok::bits(1, 1));
        bw.push(Tok::bits(y[0] as u32, FLOOR_RANGE_BITS));
        bw.push(Tok::bits(y[1] as u32, FLOOR_RANGE_BITS));
        for i in 2..f1.x.len() {
            let (low, high) = (f1.low[i], f1.high[i]);
            let dy = y[high] - y[low];
            let adx = (f1.x[high] - f1.x[low]) as i32;
            let off = dy.abs() * (f1.x[i] as i32 - f1.x[low] as i32) / adx;
            let predicted =
                (if dy < 0 { y[low] - off } else { y[low] + off }).clamp(0, FLOOR_RANGE - 1);
            let d = y[i] - predicted;
            let highroom = FLOOR_RANGE - predicted;
            let lowroom = predicted;
            let m = highroom.min(lowroom);
            let val = if d == 0 {
                0
            } else if d > 0 && d < m {
                2 * d
            } else if d < 0 && -d <= m {
                -2 * d - 1
            } else if d > 0 {
                d + lowroom
            } else {
                highroom - 1 - d
            };
            bw.push(Tok::sym(BOOK_FLOOR, val as usize));
        }
    }

    /// Encodes the block centered at `self.center`.
    fn encode_block(&mut self, spec: BlockSpec) -> Vec<Tok> {
        let ch = self.channels;
        let (n, m, fidx) = if spec.long { (N, M, 0) } else { (SN, SM, 1) };
        let cutoff = if spec.long {
            self.cutoff_bin
        } else {
            (self.cutoff_bin * SM).div_ceil(M)
        };
        let mut pkt: Vec<Tok> = Vec::new();
        pkt.push(Tok::bits(0, 1)); // audio packet
        pkt.push(Tok::bits(u32::from(spec.long), 1)); // mode: 0 short, 1 long
        if spec.long {
            pkt.push(Tok::bits(u32::from(spec.prev_long), 1));
            pkt.push(Tok::bits(u32::from(spec.next_long), 1));
        }

        let wslot = Self::window_slot(spec);
        if self.windows[wslot].is_none() {
            let w = self.build_window(spec);
            self.windows[wslot] = Some(w);
        }
        let window = self.windows[wslot]
            .as_deref()
            .expect("window cache filled above");
        let start = self.center - n / 2 - self.buf_start;

        // Analysis + floors.
        let mut res: Vec<Vec<f32>> = Vec::with_capacity(ch);
        for c in 0..ch {
            let windowed: Vec<f32> = self.inbuf[c][start..start + n]
                .iter()
                .zip(window.iter())
                .map(|(x, w)| x * w)
                .collect();
            let mut coefs = vec![0.0f32; m];
            self.transforms[usize::from(spec.long)].forward(&windowed, &mut coefs);
            let mut thr = vec![0.0f32; m];
            self.psy.thresholds(spec.long, &coefs, &mut thr);
            let y = self.fit_floor(&thr, fidx);
            self.write_floor(&mut pkt, &y, fidx);
            // Decode the floor back for the exact curve.
            let mut scratch = Vec::new();
            self.write_floor(&mut scratch, &y, fidx);
            let bytes = self.books.render(&scratch);
            let mut br = BitReader::new(&bytes);
            let mut curve = vec![0.0f32; m];
            self.floor1(fidx)
                .decode(&self.setup.codebooks, &mut br, &mut curve, m)
                .expect("self-written floor decodes");
            res.push(
                coefs
                    .iter()
                    .zip(&curve)
                    .enumerate()
                    .map(|(k, (&s, &f))| if k < cutoff { s / f } else { 0.0 })
                    .collect(),
            );
        }

        // Square-polar coupling (channel 0 = magnitude, 1 = angle).
        if ch == 2 {
            let (a, b) = res.split_at_mut(1);
            for (l, r) in a[0].iter_mut().zip(b[0].iter_mut()) {
                let (lq, rq) = (
                    l.round().clamp(-250.0, 250.0),
                    r.round().clamp(-250.0, 250.0),
                );
                let (mag, ang) = if lq.abs() > rq.abs() {
                    (lq, if lq > 0.0 { lq - rq } else { rq - lq })
                } else {
                    (rq, if rq > 0.0 { lq - rq } else { rq - lq })
                };
                *l = mag;
                *r = ang;
            }
        }
        let q: Vec<Vec<i32>> = res
            .iter()
            .map(|v| {
                v.iter()
                    .map(|x| x.round().clamp(-250.0, 250.0) as i32)
                    .collect()
            })
            .collect();

        // Classification.
        let parts = m / PARTITION;
        let classes: Vec<Vec<usize>> = q
            .iter()
            .map(|v| {
                (0..parts)
                    .map(|p| {
                        let mx = v[p * PARTITION..(p + 1) * PARTITION]
                            .iter()
                            .map(|x| x.abs())
                            .max()
                            .unwrap();
                        match mx {
                            0 => 0,
                            1 => 1,
                            2..=5 => 2,
                            _ => 3,
                        }
                    })
                    .collect()
            })
            .collect();

        // Residue: two passes; classwords first in pass 0.
        for pass in 0..2 {
            let mut pc = 0;
            while pc < parts {
                if pass == 0 {
                    for cls in &classes {
                        let mut entry = 0;
                        for i in 0..CLASSWORDS {
                            entry = entry * CLASSES + cls.get(pc + i).copied().unwrap_or(0);
                        }
                        pkt.push(Tok::sym(BOOK_CLASS, entry));
                    }
                }
                let mut i = 0;
                while i < CLASSWORDS && pc < parts {
                    for (c, cls) in classes.iter().enumerate() {
                        let part = &q[c][pc * PARTITION..(pc + 1) * PARTITION];
                        match (cls[pc], pass) {
                            (1, 0) => {
                                for v in part.chunks(4) {
                                    pkt.push(Tok::sym(BOOK_B1, self.books.b1.entry(v)));
                                }
                            }
                            (2, 0) => {
                                for v in part.chunks(2) {
                                    pkt.push(Tok::sym(BOOK_B2, self.books.b2.entry(v)));
                                }
                            }
                            (3, 0) => {
                                for &x in part {
                                    let c8 = (((x as f32) / 8.0).round() as i32).clamp(-31, 31);
                                    pkt.push(Tok::sym(BOOK_B3, self.books.b3.entry(&[c8])));
                                }
                            }
                            (3, 1) => {
                                let fine: Vec<i32> = part
                                    .iter()
                                    .map(|&x| {
                                        x - (((x as f32) / 8.0).round() as i32).clamp(-31, 31) * 8
                                    })
                                    .collect();
                                for v in fine.chunks(2) {
                                    pkt.push(Tok::sym(BOOK_B2, self.books.b2.entry(v)));
                                }
                            }
                            _ => {}
                        }
                    }
                    pc += 1;
                    i += 1;
                }
            }
        }
        pkt
    }

    /// Trains the books on the held packets, then writes headers and audio.
    /// The last packet stays in `pending` so `finish` can flag end-of-stream.
    fn write_adaptive(&mut self, a: Adaptive) -> Result<()> {
        let mut counts: [Vec<u64>; NBOOKS] = self.books.entry_counts().map(|n| vec![0u64; n]);
        for t in a.packets.iter().flat_map(|(toks, _)| toks) {
            if t.book != T_RAW {
                counts[usize::from(t.book)][t.v as usize] += 1;
            }
        }
        let books = Books::trained(&counts)?;
        let setup_packet = setup_header(self.channels, &books);
        // Same check the streaming path gets: the header must parse back.
        parse_setup(&setup_packet, &parse_id(&a.id_packet)?)?;
        let w = &mut self.page_writer;
        self.sink
            .write_all(&w.write_page(&a.id_packet, 0, true, false))?;
        self.sink
            .write_all(&w.write_page(&a.comment_packet, 0, false, false))?;
        self.sink
            .write_all(&w.write_page(&setup_packet, 0, false, false))?;
        for (toks, granule) in a.packets {
            self.emit_bytes(books.render(&toks), granule)?;
        }
        Ok(())
    }

    fn emit(&mut self, toks: Vec<Tok>, granule: i64) -> Result<()> {
        if let Some(a) = &mut self.adaptive {
            a.packets.push((toks, granule));
            return Ok(());
        }
        let packet = self.books.render(&toks);
        self.emit_bytes(packet, granule)
    }

    fn emit_bytes(&mut self, packet: Vec<u8>, granule: i64) -> Result<()> {
        if let Some((p, g)) = self.pending.replace((packet, granule)) {
            self.sink
                .write_all(&self.page_writer.write_page(&p, g, false, false))?;
        }
        Ok(())
    }

    /// Encodes every block whose input (plus detector look-ahead) is buffered.
    fn drain(&mut self) -> Result<()> {
        while !self.done {
            let n = if self.cur_long { N } else { SN };
            let boundary = self.center + n / 4;
            if self.buf_start + self.inbuf[0].len() < boundary + LOOKAHEAD {
                break;
            }
            let next_long = !self.attack_ahead(boundary);
            let spec = BlockSpec {
                long: self.cur_long,
                prev_long: self.prev_long,
                next_long,
            };
            let packet = self.encode_block(spec);
            if !spec.long {
                self.short_blocks += 1;
            }
            let produced = (self.center - M) as u64;
            let granule = produced.min(self.total_in) as i64;
            self.packets += 1;
            self.emit(packet, granule)?;
            if produced >= self.total_in {
                self.done = true;
            }
            let next_n = if next_long { N } else { SN };
            self.center += n / 4 + next_n / 4;
            self.prev_long = self.cur_long;
            self.cur_long = next_long;
            // Keep enough history for the next block and the detector.
            let keep_from = self.center.saturating_sub(N / 2 + 256);
            if keep_from > self.buf_start {
                let drop = keep_from - self.buf_start;
                for b in &mut self.inbuf {
                    b.drain(..drop);
                }
                self.buf_start = keep_from;
            }
        }
        Ok(())
    }
}

impl<W: Write + Send> Encoder for VorbisEncoder<W> {
    fn encode(&mut self, samples: &[f32]) -> Result<usize> {
        if self.finished {
            return Err(CadenceError::InvalidFormat(
                "encoder already finished".into(),
            ));
        }
        let ch = self.channels;
        if samples.len() % ch != 0 {
            return Err(CadenceError::InvalidFormat(
                "sample count is not a multiple of the channel count".into(),
            ));
        }
        let frames = samples.len() / ch;
        for f in samples.chunks_exact(ch) {
            for (c, &s) in f.iter().enumerate() {
                self.inbuf[c].push(s);
            }
        }
        self.total_in += frames as u64;
        self.drain()?;
        Ok(frames)
    }

    fn finish(&mut self) -> Result<()> {
        if self.finished {
            return Ok(());
        }
        self.finished = true;
        while !self.done {
            for b in &mut self.inbuf {
                b.resize(b.len() + M, 0.0);
            }
            self.drain()?;
        }
        if let Some(a) = self.adaptive.take() {
            self.write_adaptive(a)?;
        }
        if let Some((p, g)) = self.pending.take() {
            self.sink
                .write_all(&self.page_writer.write_page(&p, g, false, true))?;
        }
        self.sink.flush()?;
        Ok(())
    }
}
