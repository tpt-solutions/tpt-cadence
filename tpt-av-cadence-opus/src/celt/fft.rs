//! The Opus "kiss FFT" kernel (`celt/kiss_fft.c`) ported to Rust.
//!
//! This is a faithful port of libopus 1.5.2's `kiss_fft.c` float path so the
//! decoded output is bit-exact with the reference decoder. That means:
//!
//! - The same butterfly decomposition (`kf_bfly2/3/4/5`) driven by the same
//!   `factors` tables, applied in the same order.
//! - The same per-operation float arithmetic order. libopus is built without
//!   `-ffast-math`, so each `C_MUL`/`C_SUB`/`C_ADDTO` is a plain `f32`
//!   operation. We match that (Rust does not contract multiply+add into an
//!   FMA unless you call `mul_add`, which we never do).
//
// SOURCE: Xiph.Org libopus 1.5.2, `celt/kiss_fft.c` + `celt/_kiss_fft_guts.h`
// (BSD-3-Clause).

use super::tables::{
    FFT_BITREV120, FFT_BITREV240, FFT_BITREV480, FFT_BITREV60, FFT_TWIDDLES48000_960,
};

/// A complex number with the same layout as the reference `kiss_fft_cpx`.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct Cpx {
    pub r: f32,
    pub i: f32,
}

/// `kiss_fft_state` for the static 48 kHz mode. `factors` is the
/// `{p0, m0, p1, m1, ...}` decomposition table; `bitrev` points at the
/// per-size bit-reversal table; all four sizes (480/240/120/60) share the
/// single `fft_twiddles48000_960` twiddle table.
pub(crate) struct FftState {
    pub nfft: usize,
    pub scale: f32,
    pub shift: i32,
    pub factors: [i16; 16],
    pub bitrev: &'static [i16],
}

// The four substates of the 480-sample MDCT FFT (from `static_modes_float.h`).
static FFT_STATE_0: FftState = FftState {
    nfft: 480,
    scale: 0.002083333,
    shift: -1,
    factors: [5, 96, 3, 32, 4, 8, 2, 4, 4, 1, 0, 0, 0, 0, 0, 0],
    bitrev: &FFT_BITREV480,
};
static FFT_STATE_1: FftState = FftState {
    nfft: 240,
    scale: 0.004166667,
    shift: 1,
    factors: [5, 48, 3, 16, 4, 4, 4, 1, 0, 0, 0, 0, 0, 0, 0, 0],
    bitrev: &FFT_BITREV240,
};
static FFT_STATE_2: FftState = FftState {
    nfft: 120,
    scale: 0.008333333,
    shift: 2,
    factors: [5, 24, 3, 8, 2, 4, 4, 1, 0, 0, 0, 0, 0, 0, 0, 0],
    bitrev: &FFT_BITREV120,
};
static FFT_STATE_3: FftState = FftState {
    nfft: 60,
    scale: 0.016666667,
    shift: 3,
    factors: [5, 12, 3, 4, 4, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    bitrev: &FFT_BITREV60,
};

/// Returns the state for an `nfft`-point FFT: 480, 240, 120 or 60.
pub(crate) fn fft_state(nfft: usize) -> &'static FftState {
    match nfft {
        480 => &FFT_STATE_0,
        240 => &FFT_STATE_1,
        120 => &FFT_STATE_2,
        60 => &FFT_STATE_3,
        _ => unreachable!("CELT only uses 60/120/240/480-point FFTs"),
    }
}

/// Reads the complex twiddle factor at index `k`.
#[inline]
fn tw(k: usize) -> Cpx {
    Cpx {
        r: FFT_TWIDDLES48000_960[2 * k],
        i: FFT_TWIDDLES48000_960[2 * k + 1],
    }
}

// C_MUL(m, a, b): m = a*b, in IEEE f32 order (r*b.r first, etc.).
#[inline]
fn c_mul(a: Cpx, b: Cpx) -> Cpx {
    Cpx {
        r: a.r * b.r - a.i * b.i,
        i: a.r * b.i + a.i * b.r,
    }
}

// ---------------------------------------------------------------------------
// Butterflies
// ---------------------------------------------------------------------------

/// `kf_bfly2` for the static mode: the radix-2 stage that always follows a
/// radix-4 stage, so `m == 4` and the four "twiddle-aware" contractions in
/// the reference can be hard-coded.
fn kf_bfly2(fout: &mut [Cpx], m: usize, n: usize) {
    debug_assert_eq!(m, 4);
    // Reference-specced value (QCONST16(0.7071067812f,15)); keep verbatim so
    // the f32 rounding matches libopus bit-for-bit.
    #[allow(clippy::approx_constant, clippy::excessive_precision)]
    let tw = 0.7071067812f32;
    for i in 0..n {
        let f = i * 8;
        let fout2 = f + 4;

        // t = Fout2[0]; C_SUB(Fout2[0], Fout[0], t); C_ADDTO(Fout[0], t);
        let tr = fout[fout2].r;
        let ti = fout[fout2].i;
        fout[fout2].r = fout[f].r - tr;
        fout[fout2].i = fout[f].i - ti;
        fout[f].r += tr;
        fout[f].i += ti;

        // t.r = S_MUL(ADD32_ovflw(Fout2[1].r, Fout2[1].i), tw)
        // t.i = S_MUL(SUB32_ovflw(Fout2[1].i, Fout2[1].r), tw)
        let t_r = (fout[fout2 + 1].r + fout[fout2 + 1].i) * tw;
        let t_i = (fout[fout2 + 1].i - fout[fout2 + 1].r) * tw;
        fout[fout2 + 1].r = fout[f + 1].r - t_r;
        fout[fout2 + 1].i = fout[f + 1].i - t_i;
        fout[f + 1].r += t_r;
        fout[f + 1].i += t_i;

        // t.r = Fout2[2].i;  t.i = -Fout2[2].r
        let t_r = fout[fout2 + 2].i;
        let t_i = -fout[fout2 + 2].r;
        fout[fout2 + 2].r = fout[f + 2].r - t_r;
        fout[fout2 + 2].i = fout[f + 2].i - t_i;
        fout[f + 2].r += t_r;
        fout[f + 2].i += t_i;

        // t.r = S_MUL(SUB32_ovflw(Fout2[3].i, Fout2[3].r), tw)
        // t.i = S_MUL(NEG32_ovflw(ADD32_ovflw(Fout2[3].i, Fout2[3].r)), tw)
        let t_r = (fout[fout2 + 3].i - fout[fout2 + 3].r) * tw;
        let t_i = -(fout[fout2 + 3].i + fout[fout2 + 3].r) * tw;
        fout[fout2 + 3].r = fout[f + 3].r - t_r;
        fout[fout2 + 3].i = fout[f + 3].i - t_i;
        fout[f + 3].r += t_r;
        fout[f + 3].i += t_i;
    }
}

/// `kf_bfly4`: radix-4 butterfly.
fn kf_bfly4(fout: &mut [Cpx], fstride: usize, m: usize, n: usize, mm: usize) {
    if m == 1 {
        // Degenerate case where all the twiddles are 1.
        for i in 0..n {
            let f = i * mm;
            // C_SUB(scratch0, *Fout, Fout[2]); C_ADDTO(*Fout, Fout[2])
            let s0r = fout[f].r - fout[f + 2].r;
            let s0i = fout[f].i - fout[f + 2].i;
            fout[f].r += fout[f + 2].r;
            fout[f].i += fout[f + 2].i;
            // C_ADD(scratch1, Fout[1], Fout[3])
            let s1r = fout[f + 1].r + fout[f + 3].r;
            let s1i = fout[f + 1].i + fout[f + 3].i;
            // C_SUB(Fout[2], *Fout, scratch1); C_ADDTO(*Fout, scratch1)
            fout[f + 2].r = fout[f].r - s1r;
            fout[f + 2].i = fout[f].i - s1i;
            fout[f].r += s1r;
            fout[f].i += s1i;
            // C_SUB(scratch1, Fout[1], Fout[3])
            let s1r = fout[f + 1].r - fout[f + 3].r;
            let s1i = fout[f + 1].i - fout[f + 3].i;
            fout[f + 1].r = s0r + s1i;
            fout[f + 1].i = s0i - s1r;
            fout[f + 3].r = s0r - s1i;
            fout[f + 3].i = s0i + s1r;
        }
    } else {
        let m2 = 2 * m;
        let m3 = 3 * m;
        for i in 0..n {
            let base = i * mm;
            let mut tw1 = 0usize;
            let mut tw2 = 0usize;
            let mut tw3 = 0usize;
            for j in 0..m {
                let f = base + j;
                // C_MUL(scratch[0], Fout[m], *tw1)
                let s0 = c_mul(fout[f + m], tw(tw1));
                // C_MUL(scratch[1], Fout[m2], *tw2)
                let s1 = c_mul(fout[f + m2], tw(tw2));
                // C_MUL(scratch[2], Fout[m3], *tw3)
                let s2 = c_mul(fout[f + m3], tw(tw3));

                // C_SUB(scratch[5], *Fout, scratch[1]); C_ADDTO(*Fout, scratch[1])
                let s5 = Cpx {
                    r: fout[f].r - s1.r,
                    i: fout[f].i - s1.i,
                };
                fout[f].r += s1.r;
                fout[f].i += s1.i;
                // C_ADD(scratch[3], scratch[0], scratch[2])
                let s3 = Cpx {
                    r: s0.r + s2.r,
                    i: s0.i + s2.i,
                };
                // C_SUB(scratch[4], scratch[0], scratch[2])
                let s4 = Cpx {
                    r: s0.r - s2.r,
                    i: s0.i - s2.i,
                };
                // C_SUB(Fout[m2], *Fout, scratch[3])
                fout[f + m2].r = fout[f].r - s3.r;
                fout[f + m2].i = fout[f].i - s3.i;
                tw1 += fstride;
                tw2 += fstride * 2;
                tw3 += fstride * 3;
                // C_ADDTO(*Fout, scratch[3])
                fout[f].r += s3.r;
                fout[f].i += s3.i;

                fout[f + m].r = s5.r + s4.i;
                fout[f + m].i = s5.i - s4.r;
                fout[f + m3].r = s5.r - s4.i;
                fout[f + m3].i = s5.i + s4.r;
            }
        }
    }
}

/// `kf_bfly3`: radix-3 butterfly.
fn kf_bfly3(fout: &mut [Cpx], fstride: usize, m: usize, n: usize, mm: usize) {
    let m2 = 2 * m;
    let epi3 = tw(fstride * m);
    for i in 0..n {
        let base = i * mm;
        let mut tw1 = 0usize;
        let mut tw2 = 0usize;
        for k in 0..m {
            let f = base + k;
            // C_MUL(scratch[1], Fout[m], *tw1); C_MUL(scratch[2], Fout[m2], *tw2)
            let s1 = c_mul(fout[f + m], tw(tw1));
            let s2 = c_mul(fout[f + m2], tw(tw2));
            // C_ADD(scratch[3], scratch[1], scratch[2])
            let s3r = s1.r + s2.r;
            let s3i = s1.i + s2.i;
            // C_SUB(scratch[0], scratch[1], scratch[2])
            let s0r = s1.r - s2.r;
            let s0i = s1.i - s2.i;
            tw1 += fstride;
            tw2 += fstride * 2;

            // Fout[m].r = SUB32_ovflw(Fout->r, HALF_OF(scratch[3].r))
            fout[f + m].r = fout[f].r - s3r * 0.5f32;
            fout[f + m].i = fout[f].i - s3i * 0.5f32;

            // C_MULBYSCALAR(scratch[0], epi3.i): both parts multiplied in turn.
            let s0r = s0r * epi3.i;
            let s0i = s0i * epi3.i;

            // C_ADDTO(*Fout, scratch[3])
            fout[f].r += s3r;
            fout[f].i += s3i;

            fout[f + m2].r = fout[f + m].r + s0i;
            fout[f + m2].i = fout[f + m].i - s0r;

            fout[f + m].r -= s0i;
            fout[f + m].i += s0r;
        }
    }
}

/// `kf_bfly5`: radix-5 butterfly.
fn kf_bfly5(fout: &mut [Cpx], fstride: usize, m: usize, n: usize, mm: usize) {
    let ya = tw(fstride * m);
    let yb = tw(fstride * 2 * m);
    for i in 0..n {
        let base = i * mm;
        for u in 0..m {
            let f0 = base + u;
            // scratch[0] = *Fout0
            let s0 = fout[f0];

            let s1 = c_mul(fout[f0 + m], tw(u * fstride));
            let s2 = c_mul(fout[f0 + 2 * m], tw(2 * u * fstride));
            let s3 = c_mul(fout[f0 + 3 * m], tw(3 * u * fstride));
            let s4 = c_mul(fout[f0 + 4 * m], tw(4 * u * fstride));

            // C_ADD(scratch[7], scratch[1], scratch[4]); C_SUB(scratch[10], ...)
            let s7 = Cpx {
                r: s1.r + s4.r,
                i: s1.i + s4.i,
            };
            let s10 = Cpx {
                r: s1.r - s4.r,
                i: s1.i - s4.i,
            };
            // C_ADD(scratch[8], scratch[2], scratch[3]); C_SUB(scratch[9], ...)
            let s8 = Cpx {
                r: s2.r + s3.r,
                i: s2.i + s3.i,
            };
            let s9 = Cpx {
                r: s2.r - s3.r,
                i: s2.i - s3.i,
            };

            // Fout0 = Fout0 + s7 + s8  (inner add evaluated first)
            fout[f0].r += s7.r + s8.r;
            fout[f0].i += s7.i + s8.i;

            // scratch[5] = s0 + (s7*ya.r + s8*yb.r)
            let s5 = Cpx {
                r: s0.r + (s7.r * ya.r + s8.r * yb.r),
                i: s0.i + (s7.i * ya.r + s8.i * yb.r),
            };
            // scratch[6]
            let s6 = Cpx {
                r: s10.i * ya.i + s9.i * yb.i,
                i: -(s10.r * ya.i + s9.r * yb.i),
            };

            // C_SUB(*Fout1, s5, s6); C_ADD(*Fout4, s5, s6)
            fout[f0 + m].r = s5.r - s6.r;
            fout[f0 + m].i = s5.i - s6.i;
            fout[f0 + 4 * m].r = s5.r + s6.r;
            fout[f0 + 4 * m].i = s5.i + s6.i;

            let s11 = Cpx {
                r: s0.r + (s7.r * yb.r + s8.r * ya.r),
                i: s0.i + (s7.i * yb.r + s8.i * ya.r),
            };
            let s12 = Cpx {
                r: s9.i * ya.i - s10.i * yb.i,
                i: s10.r * yb.i - s9.r * ya.i,
            };

            // C_ADD(*Fout2, s11, s12); C_SUB(*Fout3, s11, s12)
            fout[f0 + 2 * m].r = s11.r + s12.r;
            fout[f0 + 2 * m].i = s11.i + s12.i;
            fout[f0 + 3 * m].r = s11.r - s12.r;
            fout[f0 + 3 * m].i = s11.i - s12.i;
        }
    }
}

// ---------------------------------------------------------------------------
// Compute kernels
// ---------------------------------------------------------------------------

/// `opus_fft_impl`: the in-place FFT, no bit-reversal input stage, no scaling.
pub(crate) fn fft_impl(st: &FftState, fout: &mut [Cpx]) {
    let shift = if st.shift > 0 { st.shift as usize } else { 0 };
    let mut fstride = [1usize; 8];
    let mut l = 0usize;
    loop {
        let p = st.factors[2 * l] as usize;
        let m = st.factors[2 * l + 1] as usize;
        fstride[l + 1] = fstride[l] * p;
        l += 1;
        if m == 1 {
            break;
        }
    }
    let mut m = st.factors[2 * l - 1] as usize;
    for i in (0..l).rev() {
        let m2 = if i != 0 {
            st.factors[2 * i - 1] as usize
        } else {
            1
        };
        match st.factors[2 * i] {
            2 => kf_bfly2(fout, m, fstride[i]),
            4 => kf_bfly4(fout, fstride[i] << shift, m, fstride[i], m2),
            3 => kf_bfly3(fout, fstride[i] << shift, m, fstride[i], m2),
            5 => kf_bfly5(fout, fstride[i] << shift, m, fstride[i], m2),
            _ => unreachable!("CELT FFT factors are 2/3/4/5 only"),
        }
        m = m2;
    }
}

/// `opus_fft_c`: bit-reverse + scale the input, then `fft_impl`.
#[allow(dead_code)]
pub(crate) fn fft(st: &FftState, fin: &[Cpx], fout: &mut [Cpx]) {
    let scale = st.scale;
    for (i, &rev) in (0..st.nfft).zip(st.bitrev.iter()) {
        let rev = rev as usize;
        fout[rev] = Cpx {
            r: scale * fin[i].r,
            i: scale * fin[i].i,
        };
    }
    fft_impl(st, fout);
}

/// `opus_ifft_c`: bit-reverse, negate-imag, `fft_impl`, negate-imag.
#[allow(dead_code)]
pub(crate) fn ifft(st: &FftState, fin: &[Cpx], fout: &mut [Cpx]) {
    for i in 0..st.nfft {
        fout[st.bitrev[i] as usize] = fin[i];
    }
    for x in fout.iter_mut().take(st.nfft) {
        x.i = -x.i;
    }
    fft_impl(st, fout);
    for x in fout.iter_mut().take(st.nfft) {
        x.i = -x.i;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Naive O(N^2) DFT for cross-checking the butterfly network. The
    /// reference FFT scales its input by `1/nfft` (kiss `st->scale`), so the
    /// oracle is normalized to match.
    #[allow(clippy::needless_range_loop)]
    fn naive_dft(x: &[Cpx]) -> Vec<Cpx> {
        let n = x.len();
        let nf = n as f32;
        let mut out = vec![Cpx::default(); n];
        for k in 0..n {
            for j in 0..n {
                let ang = -2.0 * std::f32::consts::PI * (k * j) as f32 / nf;
                let (s, c) = ang.sin_cos();
                out[k].r += x[j].r * c - x[j].i * s;
                out[k].i += x[j].r * s + x[j].i * c;
            }
            out[k].r /= nf;
            out[k].i /= nf;
        }
        out
    }

    fn close(a: Cpx, b: Cpx) -> bool {
        (a.r - b.r).abs() < 1e-4 && (a.i - b.i).abs() < 1e-4
    }

    #[test]
    fn fft_matches_dft_480() {
        let st = fft_state(480);
        let src: Vec<Cpx> = (0..480)
            .map(|i| Cpx {
                r: (i as f32 * 0.17).sin() * 0.5,
                i: (i as f32 * 0.29).cos() * 0.3,
            })
            .collect();
        let mut got = vec![Cpx::default(); 480];
        fft(st, &src, &mut got);
        let want = naive_dft(&src);
        for (g, w) in got.iter().zip(want.iter()) {
            assert!(close(*g, *w), "mismatch at 480: {g:?} vs {w:?}");
        }
    }

    #[test]
    fn fft_matches_dft_small_sizes() {
        for nfft in [240usize, 120, 60] {
            let st = fft_state(nfft);
            let src: Vec<Cpx> = (0..nfft)
                .map(|i| Cpx {
                    r: (i as f32 * 0.11).sin(),
                    i: (i as f32 * 0.23).cos(),
                })
                .collect();
            let mut got = vec![Cpx::default(); nfft];
            fft(st, &src, &mut got);
            let want = naive_dft(&src);
            for (g, w) in got.iter().zip(want.iter()) {
                assert!(close(*g, *w), "mismatch at {nfft}: {g:?} vs {w:?}");
            }
        }
    }

    #[test]
    fn ifft_inverts_fft() {
        for nfft in [480usize, 240, 120, 60] {
            let st = fft_state(nfft);
            let src: Vec<Cpx> = (0..nfft)
                .map(|i| Cpx {
                    r: (i as f32 * 0.37).cos() * 0.1,
                    i: (i as f32 * 0.29).sin() * 0.1,
                })
                .collect();
            let mut fwd = vec![Cpx::default(); nfft];
            fft(st, &src, &mut fwd);
            let mut back = vec![Cpx::default(); nfft];
            ifft(st, &fwd, &mut back);
            for (b, s) in back.iter().zip(src.iter()) {
                assert!(
                    close(*b, *s),
                    "round-trip mismatch at {nfft}: {b:?} vs {s:?}"
                );
            }
        }
    }

    #[test]
    fn substate_twiddle_indexing_is_in_bounds() {
        // Any out-of-bounds twiddle indexing would panic here, proving the
        // shared twiddle table covers every state's fstride*sized walks.
        for nfft in [480usize, 240, 120, 60] {
            let st = fft_state(nfft);
            let mut buf: Vec<Cpx> = (0..nfft)
                .map(|i| Cpx {
                    r: (i as f32).sin(),
                    i: (i as f32).cos(),
                })
                .collect();
            fft_impl(st, &mut buf);
        }
    }
}
