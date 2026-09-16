//! Floor curves: type 0 (LSP) and type 1 (piecewise line), spec sections 6
//! and 7. Curve synthesis matches FFmpeg's Vorbis decoder (which resolves
//! the spec's bark-map and line-render formulas to reference behavior).

use crate::bitreader::BitReader;
use crate::codebook::Codebook;
use tpt_av_cadence_core::CadenceError;

/// `floor1_inverse_dB_table` (spec section 10.1), 256 entries.
pub(crate) const FLOOR1_INVERSE_DB: [f32; 256] = [
    1.0649863e-07, 1.1341951e-07, 1.2079015e-07, 1.2863978e-07, 1.3699951e-07, 1.4590251e-07,
    1.5538408e-07, 1.6548181e-07, 1.7623575e-07, 1.8768855e-07, 1.9988561e-07, 2.1287530e-07,
    2.2670913e-07, 2.4144197e-07, 2.5713223e-07, 2.7384213e-07, 2.9163793e-07, 3.1059021e-07,
    3.3077411e-07, 3.5226968e-07, 3.7516214e-07, 3.9954229e-07, 4.2550680e-07, 4.5315863e-07,
    4.8260743e-07, 5.1396998e-07, 5.4737065e-07, 5.8294187e-07, 6.2082472e-07, 6.6116941e-07,
    7.0413592e-07, 7.4989464e-07, 7.9862701e-07, 8.5052630e-07, 9.0579828e-07, 9.6466216e-07,
    1.0273513e-06, 1.0941144e-06, 1.1652161e-06, 1.2409384e-06, 1.3215816e-06, 1.4074654e-06,
    1.4989305e-06, 1.5963394e-06, 1.7000785e-06, 1.8105592e-06, 1.9282195e-06, 2.0535261e-06,
    2.1869758e-06, 2.3290978e-06, 2.4804557e-06, 2.6416497e-06, 2.8133190e-06, 2.9961443e-06,
    3.1908506e-06, 3.3982101e-06, 3.6190449e-06, 3.8542308e-06, 4.1047004e-06, 4.3714470e-06,
    4.6555282e-06, 4.9580707e-06, 5.2802740e-06, 5.6234160e-06, 5.9888572e-06, 6.3780469e-06,
    6.7925283e-06, 7.2339451e-06, 7.7040476e-06, 8.2047000e-06, 8.7378876e-06, 9.3057248e-06,
    9.9104632e-06, 1.0554501e-05, 1.1240392e-05, 1.1970856e-05, 1.2748789e-05, 1.3577278e-05,
    1.4459606e-05, 1.5399272e-05, 1.6400004e-05, 1.7465768e-05, 1.8600792e-05, 1.9809576e-05,
    2.1096914e-05, 2.2467911e-05, 2.3928002e-05, 2.5482978e-05, 2.7139006e-05, 2.8902651e-05,
    3.0780908e-05, 3.2781225e-05, 3.4911534e-05, 3.7180282e-05, 3.9596466e-05, 4.2169667e-05,
    4.4910090e-05, 4.7828601e-05, 5.0936773e-05, 5.4246931e-05, 5.7772202e-05, 6.1526565e-05,
    6.5524907e-05, 6.9783085e-05, 7.4317983e-05, 7.9147585e-05, 8.4291040e-05, 8.9768747e-05,
    9.5602426e-05, 0.00010181521, 0.00010843174, 0.00011547824, 0.00012298267, 0.00013097477,
    0.00013948625, 0.00014855085, 0.00015820453, 0.00016848555, 0.00017943469, 0.00019109536,
    0.00020351382, 0.00021673929, 0.00023082423, 0.00024582449, 0.00026179955, 0.00027881276,
    0.00029693158, 0.00031622787, 0.00033677814, 0.00035866388, 0.00038197188, 0.00040679456,
    0.00043323036, 0.00046138411, 0.00049136745, 0.00052329927, 0.00055730621, 0.00059352311,
    0.00063209358, 0.00067317058, 0.00071691700, 0.00076350630, 0.00081312324, 0.00086596457,
    0.00092223983, 0.00098217216, 0.0010459992, 0.0011139742, 0.0011863665, 0.0012634633,
    0.0013455702, 0.0014330129, 0.0015261382, 0.0016253153, 0.0017309374, 0.0018434235,
    0.0019632195, 0.0020908006, 0.0022266726, 0.0023713743, 0.0025254795, 0.0026895994,
    0.0028643847, 0.0030505286, 0.0032487691, 0.0034598925, 0.0036847358, 0.0039241906,
    0.0041792066, 0.0044507950, 0.0047400328, 0.0050480668, 0.0053761186, 0.0057254891,
    0.0060975636, 0.0064938176, 0.0069158225, 0.0073652516, 0.0078438871, 0.0083536271,
    0.0088964928, 0.0094746370, 0.0100903520, 0.0107460800, 0.0114444210, 0.0121881440,
    0.0129801980, 0.0138237250, 0.0147220680, 0.0156787910, 0.0166976870, 0.0177827970,
    0.0189384230, 0.0201691490, 0.0214798540, 0.0228757350, 0.0243623300, 0.0259455310,
    0.0276316180, 0.0294272760, 0.0313396260, 0.0333762520, 0.0355452280, 0.0378551570,
    0.0403151990, 0.0429351080, 0.0457252730, 0.0486967580, 0.0518613480, 0.0552315910,
    0.0588208500, 0.0626433610, 0.0667142790, 0.0710497490, 0.0756669620, 0.0805842270,
    0.0858210440, 0.0913981790, 0.0973377470, 0.1036633000, 0.1103999300, 0.1175743400,
    0.1252149800, 0.1333521500, 0.1420181300, 0.1512472700, 0.1610761700, 0.1715438000,
    0.1826916800, 0.1945640200, 0.2072078800, 0.2206734200, 0.2350140200, 0.2502865600,
    0.2665515900, 0.2838736100, 0.3023213200, 0.3219678600, 0.3428911400, 0.3651741400,
    0.3889052100, 0.4141784700, 0.4410941200, 0.4697589000, 0.5002864800, 0.5327979100,
    0.5674221200, 0.6042964000, 0.6435669900, 0.6853895900, 0.7299300700, 0.7773650400,
    0.8278826000, 0.8816830700, 0.9389798000, 1.0,
];

/// Floor 0 configuration (spec 6.2.1).
pub struct Floor0 {
    pub order: usize,
    pub rate: u32,
    pub bark_map_size: u32,
    pub amplitude_bits: u32,
    pub amplitude_offset: u32,
    pub books: Box<[u8]>,
    /// Bark-scale map per block size (`bs/2` entries plus a `-1` sentinel),
    /// precomputed by `build_maps`.
    pub maps: Vec<Box<[i32]>>,
}

/// Floor 1 configuration (spec 7.2.2).
pub struct Floor1 {
    pub partitions: usize,
    pub partition_class: Box<[u8]>,
    pub class_dimensions: Box<[u8]>,
    pub class_subclasses: Box<[u8]>,
    pub class_masterbooks: Box<[i16]>,
    pub subclass_books: Box<[i16]>, // 8 subclasses per class, flattened
    pub multiplier: u32,
    /// X values and their precomputed low/high neighbor and ascending-sort
    /// order, in list order.
    pub x: Box<[u32]>,
    pub low: Box<[usize]>,
    pub high: Box<[usize]>,
    pub sort: Box<[usize]>,
}

/// A configured floor.
pub enum Floor {
    Zero(Floor0),
    One(Floor1),
}

impl Floor0 {
    /// Precomputes the bark maps for both block sizes (must be called after
    /// the identification header is known, before packet decode).
    pub fn build_maps(&mut self, blocksize_0: usize, blocksize_1: usize) {
        self.maps = [blocksize_0, blocksize_1]
            .iter()
            .map(|&bs| {
                let n = bs / 2;
                let mut map = vec![0i32; n + 1].into_boxed_slice();
                let bark_rate = (self.rate as f64 / 2.0).bark();
                let scale = self.bark_map_size as f64 / bark_rate;
                for (i, m) in map[..n].iter_mut().enumerate() {
                    let v = ((self.rate as f64 * i as f64 / (2.0 * n as f64)).bark() * scale)
                        .floor();
                    *m = (v as i64).min(self.bark_map_size as i64 - 1).max(0) as i32;
                }
                map[n] = -1;
                map
            })
            .collect();
    }

    /// Decodes one channel's floor 0 curve into `curve[0..n)`. Returns
    /// `false` when the channel is unused.
    pub fn decode(
        &self,
        books: &[Codebook],
        br: &mut BitReader,
        blockflag: usize,
        curve: &mut [f32],
        n: usize,
    ) -> Result<bool, CadenceError> {
        if self.amplitude_bits == 0 {
            return Ok(false);
        }
        let amplitude = br.read_bits64(self.amplitude_bits)?;
        if amplitude == 0 {
            return Ok(false);
        }
        let num_books = self.books.len();
        let book_idx = br.read_bits(BitReader::ilog(num_books as i64))? as usize;
        if book_idx >= num_books {
            return Err(CadenceError::CorruptData(
                "vorbis floor0: book number out of range".to_string(),
            ));
        }
        let codebook = &books[self.books[book_idx] as usize];

        // Read LSP coefficients (spec 6.2.2); oversampled VQ vectors may
        // produce more than `order` scalars — extras update the running
        // `last` but are otherwise dropped.
        let order = self.order;
        let mut lsp = vec![0.0f32; order].into_boxed_slice();
        let mut lsp_len = 0usize;
        let mut last = 0.0f32;
        let mut vec_buf = vec![0.0f32; codebook.dimensions.max(1)].into_boxed_slice();
        while lsp_len < order {
            codebook.read_vector(br, &mut vec_buf)?;
            let dims = codebook.dimensions;
            for v in vec_buf[..dims].iter_mut() {
                *v += last;
                last = *v;
            }
            let take = dims.min(order - lsp_len);
            lsp[lsp_len..lsp_len + take].copy_from_slice(&vec_buf[..take]);
            lsp_len += dims;
        }

        // Curve synthesis (per the reference): the LSP curve on a bark-scale
        // frequency axis, mapped to linear amplitude.
        for c in lsp.iter_mut() {
            *c = 2.0 * c.cos();
        }
        let map = &self.maps[blockflag];
        let wstep = std::f64::consts::PI / self.bark_map_size as f64;
        let amp_denom = ((1u64 << self.amplitude_bits) - 1) as f64;
        let mut i = 0usize;
        while i < n {
            let iter_cond = map[i];
            let mut p = 0.5f32;
            let mut q = 0.5f32;
            let two_cos_w = (2.0 * (wstep * iter_cond as f64).cos()) as f32;
            let mut j = 0usize;
            while j + 1 < order {
                q *= lsp[j] - two_cos_w;
                p *= lsp[j + 1] - two_cos_w;
                j += 2;
            }
            if j == order {
                p *= p * (2.0 - two_cos_w);
                q *= q * (2.0 + two_cos_w);
            } else {
                q *= two_cos_w - lsp[j];
                p *= p * (4.0 - two_cos_w * two_cos_w);
                q *= q;
            }
            if p + q == 0.0 {
                return Err(CadenceError::CorruptData(
                    "vorbis floor0: degenerate curve".to_string(),
                ));
            }
            let value = ((((amplitude as f64 * self.amplitude_offset as f64)
                / (amp_denom * (p + q) as f64).sqrt())
                - self.amplitude_offset as f64)
                * 0.11512925)
                .exp() as f32;
            loop {
                curve[i] = value;
                i += 1;
                if i >= n || map[i] != iter_cond {
                    break;
                }
            }
        }
        Ok(true)
    }
}

impl Floor1 {
    /// Decodes one channel's floor 1 curve into `curve[0..n)`. Returns
    /// `false` when the channel is unused.
    pub fn decode(
        &self,
        books: &[Codebook],
        br: &mut BitReader,
        curve: &mut [f32],
        n: usize,
    ) -> Result<bool, CadenceError> {
        if !br.read_bit()? {
            return Ok(false); // silence flag
        }
        let range = [256u32, 128, 86, 64][self.multiplier as usize - 1];
        let range_bits = BitReader::ilog(range as i64 - 1);

        let mut y = [0u16; 258];
        y[0] = br.read_bits(range_bits)? as u16;
        y[1] = br.read_bits(range_bits)? as u16;
        let mut offset = 2usize;
        for i in 0..self.partitions {
            let class = self.partition_class[i] as usize;
            let cdim = self.class_dimensions[class] as usize;
            let cbits = self.class_subclasses[class] as u32;
            let csub = (1usize << cbits) - 1;
            let mut cval = 0usize;
            if cbits > 0 {
                let master = self.class_masterbooks[class];
                cval = books[master as usize].read_scalar(br)?;
            }
            for j in 0..cdim {
                let book = self.subclass_books[class * 8 + (cval & csub)];
                cval >>= cbits;
                if book >= 0 {
                    y[offset + j] = books[book as usize].read_scalar(br)? as u16;
                } else {
                    y[offset + j] = 0;
                }
            }
            offset += cdim;
        }

        // Step 1: amplitude synthesis from the wrapped differences.
        let mut y_final = [0u16; 258];
        let mut flag = [false; 258];
        let values = self.x.len();
        flag[0] = true;
        flag[1] = true;
        y_final[0] = y[0];
        y_final[1] = y[1];
        for i in 2..values {
            let low = self.low[i];
            let high = self.high[i];
            let dy = y_final[high] as i32 - y_final[low] as i32;
            let adx = (self.x[high] - self.x[low]) as i32;
            let ady = dy.abs();
            let err = ady * (self.x[i] as i32 - self.x[low] as i32);
            let off = err / adx;
            let predicted = if dy < 0 {
                y_final[low] as i32 - off
            } else {
                y_final[low] as i32 + off
            };

            let val = y[i] as u32;
            let highroom = (range as i32 - predicted) as u32;
            let lowroom = predicted as u32;
            let room = if highroom < lowroom { highroom * 2 } else { lowroom * 2 };
            if val != 0 {
                flag[low] = true;
                flag[high] = true;
                flag[i] = true;
                if val >= room {
                    y_final[i] = if highroom > lowroom {
                        (val as i32 - lowroom as i32 + predicted).clamp(0, u16::MAX as i32) as u16
                    } else {
                        (predicted - val as i32 + highroom as i32 - 1).clamp(0, u16::MAX as i32)
                            as u16
                    };
                } else if val & 1 != 0 {
                    y_final[i] = (predicted - ((val + 1) / 2) as i32).clamp(0, u16::MAX as i32) as u16;
                } else {
                    y_final[i] = (predicted + (val / 2) as i32).clamp(0, u16::MAX as i32) as u16;
                }
            } else {
                flag[i] = false;
                y_final[i] = predicted.clamp(0, u16::MAX as i32) as u16;
            }
        }

        // Step 2: curve synthesis over the sorted X list.
        let mut lx = 0usize;
        let mut ly = y_final[0] as usize * self.multiplier as usize;
        for i in 1..values {
            let pos = self.sort[i];
            if flag[pos] {
                let hx = (self.x[pos] as usize).min(n);
                let hy = y_final[pos] as usize * self.multiplier as usize;
                if lx < n {
                    render_line(lx, ly, hx, hy, curve);
                }
                lx = self.x[pos] as usize;
                ly = hy;
            }
            if lx >= n {
                break;
            }
        }
        if lx < n {
            render_line(lx, ly, n, ly, curve);
        }
        Ok(true)
    }
}

/// Bresenham line in the dB-index domain, substituting
/// `floor1_inverse_dB_table` values (spec 9.2.7 + 7.2.4 step 15).
fn render_line(x0: usize, y0: usize, x1: usize, y1: usize, buf: &mut [f32]) {
    let dy = y1 as i64 - y0 as i64;
    let adx = (x1 - x0) as i64;
    if adx == 0 {
        return;
    }
    let ady = dy.abs();
    let sy = if dy < 0 { -1i64 } else { 1 };
    buf[x0] = FLOOR1_INVERSE_DB[(y0 as i64).clamp(0, 255) as usize];
    let base = dy / adx;
    let mut x = x0 as i64;
    let mut y = y0 as i64;
    let mut err = -adx;
    let ady = ady - base.abs() * adx;
    while x + 1 < x1 as i64 {
        x += 1;
        y += base;
        err += ady;
        if err >= 0 {
            err -= adx;
            y += sy;
        }
        buf[x as usize] = FLOOR1_INVERSE_DB[y.clamp(0, 255) as usize];
    }
}

/// The spec's bark scale helper.
trait Bark {
    fn bark(self) -> f64;
}
impl Bark for f64 {
    fn bark(self) -> f64 {
        13.1 * (0.00074 * self).atan() + 2.24 * (1.85e-8 * self * self).atan() + 1e-4 * self
    }
}

/// Decodes a floor of either type; `false` = channel unused.
pub fn floor_decode(
    floor: &Floor,
    books: &[Codebook],
    br: &mut BitReader,
    blockflag: usize,
    curve: &mut [f32],
    n: usize,
) -> Result<bool, CadenceError> {
    match floor {
        Floor::Zero(f0) => f0.decode(books, br, blockflag, curve, n),
        Floor::One(f1) => f1.decode(books, br, curve, n),
    }
}
