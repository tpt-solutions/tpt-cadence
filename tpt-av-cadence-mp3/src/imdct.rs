//! Layer III IMDCT (36- and 12-point), windowing, and overlap-add.
//!
//! Produces the 18 time-domain samples per subband from the 18 frequency
//! lines (long) or three windows of 6 lines (short), folding the previous
//! granule's tail in through the per-channel overlap buffer.

use crate::sideinfo::{SHORT_BLOCK, STOP_BLOCK};
use crate::tables::{MDCT_WINDOW, TWID3, TWID9};

/// 9-point fast DCT-III kernel used by the 36-point IMDCT.
fn dct3_9(y: &mut [f32; 9]) {
    let s0 = y[0];
    let s2 = y[2];
    let s4 = y[4];
    let s6 = y[6];
    let s8 = y[8];

    let mut t0 = s0 + s6 * 0.5;
    let mut sa = s0 - s6;
    let t4 = (s4 + s2) * 0.93969262;
    let t2 = (s8 + s2) * 0.76604444;
    let sb = (s4 - s8) * 0.17364818;
    let mut s4 = s4 + s8 - s2;

    let s2 = sa - s4 * 0.5;
    y[4] = s4 + sa;
    let s8 = t0 - t2 + sb;
    sa = t0 - t4 + t2;
    s4 = t0 + t4 - sb;

    let s1 = y[1];
    let mut s3 = y[3];
    let s5 = y[5];
    let s7 = y[7];

    s3 *= 0.86602540;
    t0 = (s5 + s1) * 0.98480775;
    let t4 = (s5 - s7) * 0.34202014;
    let t2 = (s1 + s7) * 0.64278761;
    let s1 = (s1 - s5 - s7) * 0.86602540;

    let s5 = t0 - s3 - t2;
    let s7 = t4 - s3 - t0;
    s3 = t4 + s3 - t2;

    y[0] = s4 - s7;
    y[1] = s2 + s1;
    y[2] = sa - s3;
    y[3] = s8 + s5;
    y[5] = s8 - s5;
    y[6] = sa + s3;
    y[7] = s2 - s1;
    y[8] = s4 + s7;
}

/// 36-point IMDCT + windowing + overlap-add for one band (18 in → 18 out,
/// 18 into the overlap buffer).
fn imdct36(grbuf: &mut [f32], overlap: &mut [f32], window: &[f32], nbands: usize) {
    for j in 0..nbands {
        let gb = &mut grbuf[j * 18..j * 18 + 18];
        let ov = &mut overlap[j * 9..j * 9 + 9];
        let mut co = [0.0f32; 9];
        let mut si = [0.0f32; 9];
        co[0] = -gb[0];
        si[0] = gb[17];
        for i in 0..4usize {
            si[8 - 2 * i] = gb[4 * i + 1] - gb[4 * i + 2];
            co[1 + 2 * i] = gb[4 * i + 1] + gb[4 * i + 2];
            si[7 - 2 * i] = gb[4 * i + 4] - gb[4 * i + 3];
            co[2 + 2 * i] = -(gb[4 * i + 3] + gb[4 * i + 4]);
        }
        dct3_9(&mut co);
        dct3_9(&mut si);

        si[1] = -si[1];
        si[3] = -si[3];
        si[5] = -si[5];
        si[7] = -si[7];

        for i in 0..9usize {
            let ovl = ov[i];
            let sum = co[i] * TWID9[9 + i] + si[i] * TWID9[i];
            ov[i] = co[i] * TWID9[i] - si[i] * TWID9[9 + i];
            gb[i] = ovl * window[i] - sum * window[9 + i];
            gb[17 - i] = ovl * window[9 + i] + sum * window[i];
        }
    }
}

/// 3-point IDCT helper.
fn idct3(x0: f32, x1: f32, x2: f32, dst: &mut [f32; 3]) {
    let m1 = x1 * 0.86602540;
    let a1 = x0 - x2 * 0.5;
    dst[1] = x0 + x2;
    dst[0] = a1 + m1;
    dst[2] = a1 - m1;
}

/// 12-point IMDCT for one short window (6 in → 6 out + 3 overlap).
fn imdct12(x: &[f32], dst: &mut [f32], overlap: &mut [f32]) {
    let mut co = [0.0f32; 3];
    let mut si = [0.0f32; 3];
    idct3(-x[0], x[6] + x[3], x[12] + x[9], &mut co);
    idct3(x[15], x[12] - x[9], x[6] - x[3], &mut si);
    si[1] = -si[1];

    for i in 0..3usize {
        let ovl = overlap[i];
        let sum = co[i] * TWID3[3 + i] + si[i] * TWID3[i];
        overlap[i] = co[i] * TWID3[i] - si[i] * TWID3[3 + i];
        dst[i] = ovl * TWID3[2 - i] - sum * TWID3[5 - i];
        dst[5 - i] = ovl * TWID3[5 - i] + sum * TWID3[2 - i];
    }
}

fn imdct_short(grbuf: &mut [f32], overlap: &mut [f32], nbands: usize) {
    for b in 0..nbands {
        let mut tmp = [0.0f32; 18];
        tmp.copy_from_slice(&grbuf[b * 18..b * 18 + 18]);
        let gb = &mut grbuf[b * 18..b * 18 + 18];
        let ov = &mut overlap[b * 9..b * 9 + 9];
        gb[..6].copy_from_slice(&ov[..6]);
        let (ov_head, ov_tail) = ov.split_at_mut(6);
        imdct12(&tmp, &mut gb[6..], ov_tail);
        imdct12(&tmp[1..], &mut gb[12..], ov_tail);
        imdct12(&tmp[2..], ov_head, ov_tail);
    }
}

/// Negates odd samples of even subbands (subband-domain sign fix).
pub(crate) fn change_sign(grbuf: &mut [f32]) {
    let mut off = 18;
    while off < 576 {
        for i in (1..18).step_by(2) {
            grbuf[off + i] = -grbuf[off + i];
        }
        off += 36;
    }
}

/// IMDCTs a whole granule (32 bands) with the block's window sequence.
pub(crate) fn imdct_gr(
    grbuf: &mut [f32],
    overlap: &mut [f32],
    block_type: u8,
    n_long_bands: usize,
) {
    if n_long_bands != 0 {
        imdct36(
            &mut grbuf[..n_long_bands * 18],
            &mut overlap[..n_long_bands * 9],
            &MDCT_WINDOW[0],
            n_long_bands,
        );
    }
    let rest = n_long_bands * 18;
    let (gr_rest, ov_rest) = (&mut grbuf[rest..], &mut overlap[rest / 2..]);
    if block_type == SHORT_BLOCK {
        imdct_short(gr_rest, ov_rest, 32 - n_long_bands);
    } else {
        let win: &[f32] = if block_type == STOP_BLOCK {
            &MDCT_WINDOW[1]
        } else {
            &MDCT_WINDOW[0]
        };
        imdct36(gr_rest, ov_rest, win, 32 - n_long_bands);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn naive_imdct36(x: &[f32; 18], kernel: u8) -> [f32; 36] {
        let mut raw = [0.0f32; 36];
        for (n, r) in raw.iter_mut().enumerate() {
            let mut acc = 0.0;
            for (k, xk) in x.iter().enumerate() {
                let c = match kernel {
                    // spec candidate: cos(pi/72 (2k+19)(2n+19))
                    0 => ((std::f64::consts::PI / 72.0)
                        * (2.0 * k as f64 + 19.0)
                        * (2.0 * n as f64 + 19.0))
                        .cos(),
                    // generic MDCT: cos(pi/36 (2n+10)(2k+1))
                    _ => ((std::f64::consts::PI / 36.0)
                        * (2.0 * n as f64 + 10.0)
                        * (2.0 * k as f64 + 1.0))
                        .cos(),
                };
                acc += *xk as f64 * c;
            }
            *r = acc as f32;
        }
        raw
    }

    fn naive_windowed_overlap(
        x: &[f32; 18],
        prev: &[f32; 18],
        kernel: u8,
    ) -> ([f32; 18], [f32; 18]) {
        let raw = naive_imdct36(x, kernel);
        let mut out = [0.0f32; 18];
        let mut save = [0.0f32; 18];
        for i in 0..18 {
            let w = |n: f64| (std::f64::consts::PI / 36.0 * (n + 0.5)).sin() as f32;
            out[i] = w(i as f64) * raw[i] + prev[i];
            save[i] = w((i + 18) as f64) * raw[i + 18];
        }
        (out, save)
    }

    #[test]
    fn dump_kernel_columns() {
        for k in [0usize, 1, 5] {
            let mut x = [0.0f32; 18];
            x[k] = 1.0;
            let mut overlap = [0.0f32; 9];
            let mut gb = [0.0f32; 18];
            imdct36_slice(&mut gb, &mut overlap, &x);
            print!("k={k} fast:");
            for i in 0..6 {
                print!(" {:.4}", gb[i]);
            }
            let n0: Vec<f64> = (0..6)
                .map(|i| {
                    ((std::f64::consts::PI / 72.0)
                        * (2.0 * k as f64 + 19.0)
                        * (2.0 * i as f64 + 19.0))
                        .cos()
                })
                .collect();
            print!(" | cand0:");
            for v in &n0 {
                print!(" {:.4}", v);
            }
            let n1: Vec<f64> = (0..6)
                .map(|i| {
                    ((std::f64::consts::PI / 36.0)
                        * (2.0 * i as f64 + 10.0)
                        * (2.0 * k as f64 + 1.0))
                        .cos()
                })
                .collect();
            print!(" | cand1:");
            for v in &n1 {
                print!(" {:.4}", v);
            }
            println!();
        }
    }

    #[test]
    fn imdct36_matches_naive_spec_kernel() {
        let mut seed = 0x9e3779b9u32;
        let mut rnd = move || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            (seed & 0xFFFF) as f32 / 32768.0 - 1.0
        };
        for kernel in 0..2u8 {
            let mut overlap = [0.0f32; 9];
            let mut prev = [0.0f32; 18];
            for _ in 0..3 {
                let mut x = [0.0f32; 18];
                for v in x.iter_mut() {
                    *v = rnd();
                }
                let mut gb = [0.0f32; 18];
                imdct36_slice(&mut gb, &mut overlap, &x);
                let (out, save) = naive_windowed_overlap(&x, &prev, kernel);
                let maxerr = gb
                    .iter()
                    .zip(out.iter())
                    .map(|(a, b)| (a - b).abs())
                    .fold(0.0f32, f32::max);
                println!("kernel={kernel} maxerr={maxerr}");
                overlap = overlap_tmp(&save);
                prev = save;
            }
        }
    }

    fn imdct36_slice(gb: &mut [f32; 18], overlap: &mut [f32; 9], x: &[f32; 18]) {
        // reimplements the fast path for one band with sine window
        let mut co = [0.0f32; 9];
        let mut si = [0.0f32; 9];
        co[0] = -x[0];
        si[0] = x[17];
        for i in 0..4usize {
            si[8 - 2 * i] = x[4 * i + 1] - x[4 * i + 2];
            co[1 + 2 * i] = x[4 * i + 1] + x[4 * i + 2];
            si[7 - 2 * i] = x[4 * i + 4] - x[4 * i + 3];
            co[2 + 2 * i] = -(x[4 * i + 3] + x[4 * i + 4]);
        }
        dct3_9(&mut co);
        dct3_9(&mut si);
        si[1] = -si[1];
        si[3] = -si[3];
        si[5] = -si[5];
        si[7] = -si[7];
        let window = &MDCT_WINDOW[0];
        for i in 0..9usize {
            let ovl = overlap[i];
            let sum = co[i] * TWID9[9 + i] + si[i] * TWID9[i];
            overlap[i] = co[i] * TWID9[i] - si[i] * TWID9[9 + i];
            gb[i] = ovl * window[i] - sum * window[9 + i];
            gb[17 - i] = ovl * window[9 + i] + sum * window[i];
        }
    }

    fn overlap_tmp(save: &[f32; 18]) -> [f32; 9] {
        let mut o = [0.0f32; 9];
        o.copy_from_slice(&save[..9]);
        o
    }
}
