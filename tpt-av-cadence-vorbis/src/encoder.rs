//! Ogg Vorbis I encoder.
//!
//! A deliberately compact, fixed-configuration encoder built on the
//! decoder's own parsed structures:
//!
//! - **One block size** (2048 samples, 1024 new samples per packet), one
//!   mode, one mapping; stereo uses square-polar channel coupling.
//! - **Floor 1** with 34 posts (multiplier 2, range 128) fitted to the
//!   smoothed spectral envelope minus a quality-dependent offset. The
//!   encoder writes each floor, then decodes it back through the real
//!   [`Floor1::decode`] so the residue is computed against the *exact*
//!   curve the decoder will apply.
//! - **Residue type 1** with four partition classes (silent / +-1 / +-5 /
//!   coarse+fine cascade) coded through static Huffman-shaped VQ books.
//! - Codebooks, floor and residue configuration are written into the setup
//!   header and immediately parsed back with [`parse_setup`], so an
//!   internally inconsistent header fails at construction, not in a player.
//!
//! Quality is a constant band-SNR model (the floor tracks the spectral
//! envelope), with a lowpass and an absolute floor floor; there is no
//! masking model yet.
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

const BS_EXP: u32 = 11;
const N: usize = 1 << BS_EXP;
const M: usize = N / 2;
const FLOOR_RANGE: i32 = 128;
const FLOOR_RANGE_BITS: u32 = 7;
/// Exponent blending the floor between tracking the local envelope (1.0,
/// constant band SNR) and a flat absolute noise level (0.0, minimum MSE):
/// noise follows the spectrum partially, as masking does.
const FLOOR_ALPHA: f32 = 0.7;
const SERIAL: u32 = 0x5450_5456;
/// Forward MDCT scale: makes the decoder's unnormalized IMDCT + window
/// overlap-add reconstruct unity gain.
const MDCT_SCALE: f32 = 4.0 / N as f32;
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

// Codebook indices in the setup header.
const BOOK_FLOOR: usize = 0;
const BOOK_CLASS: usize = 1;
const BOOK_B1: usize = 2;
const BOOK_B2: usize = 3;
const BOOK_B3: usize = 4;

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
        Ok(VqBook {
            book: EncBook::new(&weights)?,
            dim,
            n,
            step,
        })
    }

    /// Writes one vector of already-step-divided integer digits.
    fn put(&self, bw: &mut BitWriter, digits: &[i32]) {
        let vals = (2 * self.n + 1) as usize;
        let mut entry = 0usize;
        let mut mul = 1usize;
        for &d in &digits[..self.dim] {
            entry += (d + self.n) as usize * mul;
            mul *= vals;
        }
        self.book.put(bw, entry);
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

/// Ogg Vorbis encoder.
pub struct VorbisEncoder<W: Write + Send> {
    sink: W,
    channels: usize,
    page_writer: OggPageWriter,
    setup: Setup,
    floor_book: EncBook,
    class_book: EncBook,
    b1: VqBook,
    b2: VqBook,
    b3: VqBook,
    fft: Fft,
    pre: Vec<C>,
    post: Vec<C>,
    window: Vec<f32>,
    /// Floor offset below the envelope, as an amplitude ratio.
    floor_gain: f32,
    alpha: f32,
    /// Absolute noise level the floor is pulled toward (amplitude).
    floor_d: f32,
    /// First bin forced to zero (lowpass).
    cutoff_bin: usize,
    inbuf: Vec<Vec<f32>>,
    /// Frames submitted so far.
    total_in: u64,
    /// Packets emitted so far.
    packets: u64,
    pending: Option<(Vec<u8>, i64)>,
    finished: bool,
}

impl<W: Write + Send> VorbisEncoder<W> {
    /// Opens a stream. `quality` runs 0.0 (smallest) to 10.0 (best).
    pub fn new(mut sink: W, sample_rate: u32, channels: u16, quality: f32) -> Result<Self> {
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

        // --- Books ---
        let floor_weights: Vec<f64> = (0..FLOOR_RANGE)
            .map(|v| 1.0 / (1.0 + f64::from(v)).powf(1.3))
            .collect();
        let floor_book = EncBook::new(&floor_weights)?;
        let class_p = [0.5, 0.25, 0.15, 0.10];
        let class_weights: Vec<f64> = (0..CLASSES * CLASSES)
            .map(|e| class_p[e / CLASSES] * class_p[e % CLASSES])
            .collect();
        let class_book = EncBook::new(&class_weights)?;
        let b1 = VqBook::new(4, 1, 1, 0.55)?;
        let b2 = VqBook::new(2, 5, 1, 2.2)?;
        let b3 = VqBook::new(1, 31, 8, 4.0)?;

        // --- Identification header ---
        let mut id = BitWriter::default();
        id.write(0, 32);
        id.write(u32::from(channels), 8);
        id.write(sample_rate, 32);
        id.write(0, 32);
        id.write(0, 32);
        id.write(0, 32);
        id.write(BS_EXP | (BS_EXP << 4), 8);
        id.write(1, 1);
        let id_packet = header_packet(1, &id.finish());

        // --- Comment header ---
        let mut cm = BitWriter::default();
        let vendor = b"tpt-cadence";
        cm.write(vendor.len() as u32, 32);
        for &b in vendor {
            cm.write(u32::from(b), 8);
        }
        cm.write(0, 32);
        cm.write(1, 1);
        let comment_packet = header_packet(3, &cm.finish());

        // --- Setup header ---
        let mut s = BitWriter::default();
        s.write(4, 8); // 5 codebooks
        write_scalar_book(&mut s, 1, &floor_book);
        write_scalar_book(&mut s, CLASSWORDS, &class_book);
        write_vq_book(&mut s, &b1);
        write_vq_book(&mut s, &b2);
        write_vq_book(&mut s, &b3);
        s.write(0, 6); // one time-domain placeholder
        s.write(0, 16);
        // Floor 1.
        s.write(0, 6);
        s.write(1, 16);
        s.write(8, 5); // partitions
        for _ in 0..8 {
            s.write(0, 4); // all class 0
        }
        s.write(3, 3); // class dimension 4
        s.write(0, 2); // no subclasses
        s.write(BOOK_FLOOR as u32 + 1, 8);
        s.write(1, 2); // multiplier 2
        s.write(10, 4); // rangebits: posts up to 1023, end at 1024
        for &x in &POSTS {
            s.write(x, 10);
        }
        // Residue 1.
        s.write(0, 6);
        s.write(1, 16);
        s.write(0, 24);
        s.write(M as u32, 24);
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
        // Mapping 0.
        s.write(0, 6);
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
        s.write(0, 8);
        s.write(0, 8);
        // Mode 0: short-flag block, mapping 0.
        s.write(0, 6);
        s.write(0, 1);
        s.write(0, 16);
        s.write(0, 16);
        s.write(0, 8);
        s.write(1, 1);
        let setup_packet = header_packet(5, &s.finish());

        let id_parsed = parse_id(&id_packet)?;
        let setup = parse_setup(&setup_packet, &id_parsed)?;

        let mut page_writer = OggPageWriter::new(SERIAL);
        sink.write_all(&page_writer.write_page(&id_packet, 0, true, false))?;
        sink.write_all(&page_writer.write_page(&comment_packet, 0, false, false))?;
        sink.write_all(&page_writer.write_page(&setup_packet, 0, false, false))?;

        // --- Transform tables ---
        let nf = N as f64;
        let pre = (0..N)
            .map(|n| {
                let a = -std::f64::consts::PI * n as f64 / nf;
                C::new(a.cos() as f32, a.sin() as f32)
            })
            .collect();
        let n0 = 0.5 + nf / 4.0;
        let post = (0..M)
            .map(|k| {
                let a = -2.0 * std::f64::consts::PI * n0 * (k as f64 + 0.5) / nf;
                C::new(a.cos() as f32, a.sin() as f32)
            })
            .collect();
        let window = (0..N)
            .map(|n| {
                let s = (std::f64::consts::PI * (n as f64 + 0.5) / nf).sin();
                (0.5 * std::f64::consts::PI * s * s).sin() as f32
            })
            .collect();

        let snr_db = -8.0 + 1.6 * f64::from(quality);
        let cutoff_hz = 12_000.0 + 1_000.0 * f64::from(quality);
        let cutoff_bin = ((cutoff_hz / (f64::from(sample_rate) / 2.0)) * M as f64) as usize;

        Ok(VorbisEncoder {
            sink,
            channels: ch,
            page_writer,
            setup,
            floor_book,
            class_book,
            b1,
            b2,
            b3,
            fft: Fft::new(N, true),
            pre,
            post,
            window,
            floor_gain: 10f64.powf(-snr_db / 20.0) as f32,
            alpha: FLOOR_ALPHA,
            floor_d: 10f32.powf(-(50.0 + 1.5 * quality) / 20.0),
            cutoff_bin: cutoff_bin.min(M),
            inbuf: vec![vec![0.0; M]; ch],
            total_in: 0,
            packets: 0,
            pending: None,
            finished: false,
        })
    }

    /// Windowed forward MDCT of `N` samples into `M` coefficients.
    fn mdct(&mut self, x: &[f32], out: &mut [f32]) {
        let mut g = vec![C::default(); N];
        for n in 0..N {
            let z = x[n] * self.window[n];
            g[n] = C::new(z * self.pre[n].re, z * self.pre[n].im);
        }
        let mut o = vec![C::default(); N];
        self.fft.run(&g, &mut o);
        for k in 0..M {
            out[k] = MDCT_SCALE * (o[k].re * self.post[k].re - o[k].im * self.post[k].im);
        }
    }

    /// Floor 1 post values (`Y` per x-list entry) for a spectrum.
    fn fit_floor(&self, spec: &[f32]) -> Vec<i32> {
        let f1 = self.floor1();
        f1.x.iter()
            .map(|&x| {
                let pos = x as usize;
                let hw = (pos / 5).max(3);
                let lo = pos.saturating_sub(hw).min(M - 1);
                let hi = (pos + hw).min(M - 1);
                let e: f32 =
                    spec[lo..=hi].iter().map(|v| v * v).sum::<f32>() / (hi - lo + 1) as f32;
                let target = ((e.sqrt().max(1e-9) * self.floor_gain).powf(self.alpha)
                    * self.floor_d.powf(1.0 - self.alpha))
                .max(1e-5);
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

    fn floor1(&self) -> &Floor1 {
        match &self.setup.floors[0] {
            Floor::One(f) => f,
            Floor::Zero(_) => unreachable!("setup declares floor 1"),
        }
    }

    /// Serializes floor 1 for post values `y` (audible-channel flag set).
    fn write_floor(&self, bw: &mut BitWriter, y: &[i32]) {
        let f1 = self.floor1();
        bw.write(1, 1);
        bw.write(y[0] as u32, FLOOR_RANGE_BITS);
        bw.write(y[1] as u32, FLOOR_RANGE_BITS);
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
            self.floor_book.put(bw, val as usize);
        }
    }

    /// Encodes one block from the first `N` samples of each channel buffer.
    fn encode_block(&mut self) -> Vec<u8> {
        let ch = self.channels;
        let mut pkt = BitWriter::default();
        pkt.write(0, 1); // audio packet

        // Analysis + floors.
        let mut res: Vec<Vec<f32>> = Vec::with_capacity(ch);
        for c in 0..ch {
            let block: Vec<f32> = self.inbuf[c][..N].to_vec();
            let mut spec = vec![0.0f32; M];
            self.mdct(&block, &mut spec);
            let y = self.fit_floor(&spec);
            self.write_floor(&mut pkt, &y);
            // Decode the floor back for the exact curve.
            let mut scratch = BitWriter::default();
            self.write_floor(&mut scratch, &y);
            let bytes = scratch.finish();
            let mut br = BitReader::new(&bytes);
            let mut curve = vec![0.0f32; M];
            self.floor1()
                .decode(&self.setup.codebooks, &mut br, &mut curve, M)
                .expect("self-written floor decodes");
            res.push(
                spec.iter()
                    .zip(&curve)
                    .enumerate()
                    .map(|(k, (&s, &f))| if k < self.cutoff_bin { s / f } else { 0.0 })
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
                let (m, ang) = if lq.abs() > rq.abs() {
                    (lq, if lq > 0.0 { lq - rq } else { rq - lq })
                } else {
                    (rq, if rq > 0.0 { lq - rq } else { rq - lq })
                };
                *l = m;
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
        let parts = M / PARTITION;
        let classes: Vec<Vec<usize>> = q
            .iter()
            .map(|v| {
                (0..parts)
                    .map(|p| {
                        let m = v[p * PARTITION..(p + 1) * PARTITION]
                            .iter()
                            .map(|x| x.abs())
                            .max()
                            .unwrap();
                        match m {
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
                        self.class_book.put(&mut pkt, entry);
                    }
                }
                let mut i = 0;
                while i < CLASSWORDS && pc < parts {
                    for (c, cls) in classes.iter().enumerate() {
                        let part = &q[c][pc * PARTITION..(pc + 1) * PARTITION];
                        match (cls[pc], pass) {
                            (1, 0) => {
                                for v in part.chunks(4) {
                                    self.b1.put(&mut pkt, v);
                                }
                            }
                            (2, 0) => {
                                for v in part.chunks(2) {
                                    self.b2.put(&mut pkt, v);
                                }
                            }
                            (3, 0) => {
                                for &x in part {
                                    let c8 = (((x as f32) / 8.0).round() as i32).clamp(-31, 31);
                                    self.b3.put(&mut pkt, &[c8]);
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
                                    self.b2.put(&mut pkt, v);
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
        pkt.finish()
    }

    fn emit(&mut self, packet: Vec<u8>, granule: i64) -> Result<()> {
        if let Some((p, g)) = self.pending.replace((packet, granule)) {
            self.sink
                .write_all(&self.page_writer.write_page(&p, g, false, false))?;
        }
        Ok(())
    }

    /// Encodes every block for which `2 * M` samples are buffered.
    fn drain(&mut self) -> Result<()> {
        while self.inbuf[0].len() >= N {
            let packet = self.encode_block();
            let j = self.packets;
            self.packets += 1;
            let granule = if j == 0 {
                0
            } else {
                (j * M as u64).min(self.total_in) as i64
            };
            self.emit(packet, granule)?;
            for b in &mut self.inbuf {
                b.drain(..M);
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
        let needed = self.total_in.div_ceil(M as u64) + 1;
        while self.packets < needed {
            for b in &mut self.inbuf {
                b.resize(b.len() + M, 0.0);
            }
            self.drain()?;
        }
        if let Some((p, g)) = self.pending.take() {
            self.sink
                .write_all(&self.page_writer.write_page(&p, g, false, true))?;
        }
        self.sink.flush()?;
        Ok(())
    }
}
