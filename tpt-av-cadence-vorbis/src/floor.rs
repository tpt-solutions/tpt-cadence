//! Floor curves: type 0 (LSP) and type 1 (piecewise line), spec sections 6
//! and 7. Curve synthesis matches FFmpeg's Vorbis decoder (which resolves
//! the spec's bark-map and line-render formulas to reference behavior).

use crate::bitreader::BitReader;
use crate::codebook::Codebook;
use tpt_av_cadence_core::CadenceError;

/// `floor1_inverse_dB_table` (spec section 10.1), 256 entries.
pub(crate) const FLOOR1_INVERSE_DB: [f32; 256] = [
    1.0649863e-07,
    1.1341951e-07,
    1.2079015e-07,
    1.2863978e-07,
    1.369_995e-7,
    1.459_025e-7,
    1.5538408e-07,
    1.6548181e-07,
    1.7623575e-07,
    1.8768855e-07,
    1.998_856e-7,
    2.128_753e-7,
    2.2670913e-07,
    2.4144197e-07,
    2.5713223e-07,
    2.7384213e-07,
    2.9163793e-07,
    3.1059021e-07,
    3.307_741e-7,
    3.5226968e-07,
    3.7516214e-07,
    3.995_423e-7,
    4.255_068e-7,
    4.5315863e-07,
    4.8260743e-07,
    5.139_7e-7,
    5.4737065e-07,
    5.829_419e-7,
    6.208_247e-7,
    6.611_694e-7,
    7.041_359e-7,
    7.4989464e-07,
    7.986_27e-7,
    8.505_263e-7,
    9.057_983e-7,
    9.646_621e-7,
    1.0273513e-06,
    1.0941144e-06,
    1.1652161e-06,
    1.2409384e-06,
    1.3215816e-06,
    1.4074654e-06,
    1.4989305e-06,
    1.5963394e-06,
    1.7000785e-06,
    1.8105592e-06,
    1.9282195e-06,
    2.053_526e-6,
    2.1869758e-06,
    2.3290978e-06,
    2.4804557e-06,
    2.6416497e-06,
    2.813_319e-6,
    2.9961443e-06,
    3.1908506e-06,
    3.398_21e-6,
    3.619_045e-6,
    3.8542308e-06,
    4.1047004e-06,
    4.371_447e-6,
    4.6555282e-06,
    4.958_071e-6,
    5.280_274e-6,
    5.623_416e-6,
    5.988_857e-6,
    6.3780469e-06,
    6.7925283e-06,
    7.2339451e-06,
    7.704_048e-6,
    8.204_7e-6,
    8.737_888e-6,
    9.305_725e-6,
    9.910_464e-6,
    1.0554501e-05,
    1.1240392e-05,
    1.1970856e-05,
    1.2748789e-05,
    1.3577278e-05,
    1.4459606e-05,
    1.5399272e-05,
    1.6400004e-05,
    1.7465768e-05,
    1.8600792e-05,
    1.9809576e-05,
    2.1096914e-05,
    2.2467911e-05,
    2.3928002e-05,
    2.5482978e-05,
    2.7139006e-05,
    2.890_265e-5,
    3.078_091e-5,
    3.2781225e-05,
    3.4911534e-05,
    3.718_028e-5,
    3.9596466e-05,
    4.2169667e-05,
    4.491_009e-5,
    4.7828601e-05,
    5.0936773e-05,
    5.424_693e-5,
    5.7772202e-05,
    6.152_657e-5,
    6.552_491e-5,
    6.9783085e-05,
    7.4317983e-05,
    7.914_758e-5,
    8.429_104e-5,
    8.976_875e-5,
    9.560_242e-5,
    0.00010181521,
    0.00010843174,
    0.00011547824,
    0.00012298267,
    0.00013097477,
    0.00013948625,
    0.00014855085,
    0.00015820453,
    0.00016848555,
    0.00017943469,
    0.00019109536,
    0.00020351382,
    0.000_216_739_3,
    0.00023082423,
    0.00024582449,
    0.00026179955,
    0.00027881276,
    0.00029693158,
    0.00031622787,
    0.00033677814,
    0.00035866388,
    0.00038197188,
    0.00040679456,
    0.00043323036,
    0.000_461_384_1,
    0.000_491_367_5,
    0.00052329927,
    0.000_557_306_2,
    0.000_593_523_1,
    0.000_632_093_6,
    0.000_673_170_6,
    0.000_716_917,
    0.000_763_506_3,
    0.00081312324,
    0.00086596457,
    0.00092223983,
    0.000_982_172_2,
    0.0010459992,
    0.0011139742,
    0.0011863665,
    0.0012634633,
    0.0013455702,
    0.0014330129,
    0.0015261382,
    0.0016253153,
    0.0017309374,
    0.0018434235,
    0.0019632195,
    0.0020908006,
    0.0022266726,
    0.0023713743,
    0.0025254795,
    0.0026895994,
    0.0028643847,
    0.0030505286,
    0.003_248_769,
    0.0034598925,
    0.0036847358,
    0.0039241906,
    0.0041792066,
    0.004_450_795,
    0.004_740_033,
    0.005_048_067,
    0.0053761186,
    0.005_725_489,
    0.0060975636,
    0.0064938176,
    0.0069158225,
    0.0073652516,
    0.007_843_887,
    0.008_353_627,
    0.008_896_492,
    0.009_474_637,
    0.010_090_352,
    0.010_746_08,
    0.011_444_421,
    0.012_188_144,
    0.012_980_198,
    0.013_823_725,
    0.014_722_068,
    0.015_678_791,
    0.016_697_686,
    0.017_782_796,
    0.018_938_422,
    0.020_169_148,
    0.021_479_854,
    0.022_875_736,
    0.024_362_33,
    0.025_945_531,
    0.027_631_618,
    0.029_427_277,
    0.031_339_627,
    0.033_376_25,
    0.035_545_226,
    0.037_855_156,
    0.040_315_2,
    0.042_935_107,
    0.045_725_275,
    0.048_696_756,
    0.051_861_35,
    0.055_231_59,
    0.058_820_85,
    0.062_643_364,
    0.066_714_28,
    0.071_049_75,
    0.075_666_964,
    0.080_584_23,
    0.085_821_05,
    0.091_398_18,
    0.097_337_745,
    0.103_663_3,
    0.110_399_93,
    0.117_574_34,
    0.125_214_98,
    0.133_352_15,
    0.142_018_12,
    0.151_247_26,
    0.161_076_17,
    0.171_543_8,
    0.182_691_68,
    0.194_564_01,
    0.207_207_87,
    0.220_673_43,
    0.235_014_02,
    0.250_286_55,
    0.266_551_58,
    0.283_873_62,
    0.302_321_3,
    0.321_967_87,
    0.342_891_13,
    0.365_174_14,
    0.388_905_2,
    0.414_178_46,
    0.441_094_13,
    0.469_758_9,
    0.500_286_46,
    0.532_797_93,
    0.567_422_1,
    0.604_296_4,
    0.643_566_97,
    0.685_389_6,
    0.729_930_04,
    0.777_365,
    0.827_882_6,
    0.881_683_05,
    0.938_979_8,
    1.0,
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
                    let v =
                        ((self.rate as f64 * i as f64 / (2.0 * n as f64)).bark() * scale).floor();
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
                p = p * p * (2.0 - two_cos_w);
                q = q * q * (2.0 + two_cos_w);
            } else {
                q *= two_cos_w - lsp[j];
                p *= 4.0 - two_cos_w * two_cos_w;
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
                // `offset` walks the same `partition_class`/`class_dimensions`
                // sequence used to build `self.x` in `header::parse_setup`,
                // which caps `x.len()` (== 2 + total dimensions read here) at
                // 65; `y`/`y_final`/`flag` are sized 258, well above that
                // bound, so `offset + j` never runs off the end.
                debug_assert!(offset + j < y.len());
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
            // `self.low`/`self.high` are precomputed once at setup (see
            // `header::parse_setup`'s floor1 neighbor search) from an `x`
            // list already validated to hold distinct values; the search
            // there always picks `low` as the nearest lesser and `high` as
            // the nearest greater neighbor of `x[i]`, so `x[high] > x[low]`
            // always holds and this subtraction cannot underflow.
            debug_assert!(self.x[high] > self.x[low]);
            let adx = (self.x[high] - self.x[low]) as i32;
            let ady = dy.abs();
            let err = ady * (self.x[i] as i32 - self.x[low] as i32);
            let off = err / adx;
            let predicted = if dy < 0 {
                y_final[low] as i32 - off
            } else {
                y_final[low] as i32 + off
            };
            // Spec 7.2.3 `render_point`: the prediction is clamped to the
            // valid Y range *before* deriving `lowroom`/`highroom` below.
            // `off` is derived from packet-controlled amplitude codewords
            // and is otherwise unbounded, so skipping this clamp lets
            // `predicted` land far outside `[0, range)`; `highroom` would
            // then wrap through the `as u32` cast and the `* 2` below would
            // overflow (a debug-build panic) on adversarial input.
            let predicted = predicted.clamp(0, range as i32 - 1);

            let val = y[i] as u32;
            let highroom = (range as i32 - predicted) as u32;
            let lowroom = predicted as u32;
            let room = if highroom < lowroom {
                highroom * 2
            } else {
                lowroom * 2
            };
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
                    y_final[i] =
                        (predicted - val.div_ceil(2) as i32).clamp(0, u16::MAX as i32) as u16;
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
    // Callers only ever advance `lx`/pass `n` forward along the ascending
    // `self.sort` order (or `n` itself, the tail segment), so `x1 >= x0`
    // always holds here and this subtraction cannot underflow.
    debug_assert!(x1 >= x0);
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
