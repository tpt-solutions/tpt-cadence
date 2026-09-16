//! Polyphase synthesis filterbank: 32 subbands → PCM via a 32-point DCT-II
//! over the subband columns followed by a 15-step windowed polyphase
//! accumulation over the 960-float QMF history (`lins`).

use crate::tables::{SEC, SYN_WIN};

const PCM_SCALE: f32 = 1.0 / 32768.0;
/// QMF history lives at `lins[960..2112]` (`zlin` in the reference).
const ZLIN: usize = 15 * 64;

/// 32-point DCT-II across one subband-sample column (all 32 bands), with
/// the decimated output ordering the synthesis window expects.
pub fn dct_ii(grbuf: &mut [f32], n: usize) {
    for k in 0..n {
        let mut t = [[0.0f32; 8]; 4];
        for i in 0..8usize {
            let x0 = grbuf[k + i * 18];
            let x1 = grbuf[k + (15 - i) * 18];
            let x2 = grbuf[k + (16 + i) * 18];
            let x3 = grbuf[k + (31 - i) * 18];
            let u0 = x0 + x3;
            let u1 = x1 + x2;
            let u2 = (x1 - x2) * SEC[3 * i];
            let u3 = (x0 - x3) * SEC[3 * i + 1];
            t[0][i] = u0 + u1;
            t[1][i] = (u0 - u1) * SEC[3 * i + 2];
            t[2][i] = u3 + u2;
            t[3][i] = (u3 - u2) * SEC[3 * i + 2];
        }
        for g in 0..4 {
            let x = &mut t[g];
            let (mut x0, mut x1, mut x2, mut x3) = (x[0], x[1], x[2], x[3]);
            let (mut x4, mut x5, mut x6, mut x7) = (x[4], x[5], x[6], x[7]);
            let mut xt;
            xt = x0 - x7;
            x0 += x7;
            x7 = x1 - x6;
            x1 += x6;
            x6 = x2 - x5;
            x2 += x5;
            x5 = x3 - x4;
            x3 += x4;
            x4 = x0 - x3;
            x0 += x3;
            x3 = x1 - x2;
            x1 += x2;
            x[0] = x0 + x1;
            x[4] = (x0 - x1) * 0.70710677;
            x5 += x6;
            x6 = (x6 + x7) * 0.70710677;
            x7 += xt;
            x3 = (x3 + x4) * 0.70710677;
            x5 -= x7 * 0.198912367; // rotate by PI/8
            x7 += x5 * 0.382683432;
            x5 -= x7 * 0.198912367;
            x0 = xt - x6;
            xt += x6;
            x[1] = (xt + x7) * 0.50979561;
            x[2] = (x4 + x3) * 0.54119611;
            x[3] = (x0 - x5) * 0.60134488;
            x[5] = (x0 + x5) * 0.89997619;
            x[6] = (x4 - x3) * 1.30656302;
            x[7] = (xt - x7) * 2.56291556;
        }
        let mut base = k;
        for i in 0..7usize {
            grbuf[base] = t[0][i];
            grbuf[base + 18] = t[2][i] + t[3][i] + t[3][i + 1];
            grbuf[base + 36] = t[1][i] + t[1][i + 1];
            grbuf[base + 54] = t[2][i + 1] + t[3][i] + t[3][i + 1];
            base += 72;
        }
        grbuf[base] = t[0][7];
        grbuf[base + 18] = t[2][7] + t[3][7];
        grbuf[base + 36] = t[1][7];
        grbuf[base + 54] = t[3][7];
    }
}

/// One polyphase "pair" output (two PCM samples 16*nch apart).
fn synth_pair(pcm: &mut [f32], pcm_off: usize, nch: usize, lins: &[f32], z: usize) {
    let mut a;
    a = (lins[z + 14 * 64] - lins[z]) * 29.0;
    a += (lins[z + 64] + lins[z + 13 * 64]) * 213.0;
    a += (lins[z + 12 * 64] - lins[z + 2 * 64]) * 459.0;
    a += (lins[z + 3 * 64] + lins[z + 11 * 64]) * 2037.0;
    a += (lins[z + 10 * 64] - lins[z + 4 * 64]) * 5153.0;
    a += (lins[z + 5 * 64] + lins[z + 9 * 64]) * 6574.0;
    a += (lins[z + 8 * 64] - lins[z + 6 * 64]) * 37489.0;
    a += lins[z + 7 * 64] * 75038.0;
    pcm[pcm_off] = a * PCM_SCALE;

    let z = z + 2;
    let mut a2;
    a2 = lins[z + 14 * 64] * 104.0;
    a2 += lins[z + 12 * 64] * 1567.0;
    a2 += lins[z + 10 * 64] * 9727.0;
    a2 += lins[z + 8 * 64] * 64019.0;
    a2 += lins[z + 6 * 64] * -9975.0;
    a2 += lins[z + 4 * 64] * -45.0;
    a2 += lins[z + 2 * 64] * 146.0;
    a2 += lins[z] * -5.0;
    pcm[pcm_off + 16 * nch] = a2 * PCM_SCALE;
}

/// Windowed polyphase accumulation for subband pair (i, i+1): produces 64
/// interleaved PCM samples starting at `pcm_off`. `base` is the sliding
/// window offset (64 floats per band-pair, i.e. `i * 64`) applied to the
/// whole QMF work area, exactly as the reference passes `lins + i*64`.
fn synth(
    grbuf: &[f32],
    xl: usize,
    xr: usize,
    pcm: &mut [f32],
    pcm_off: usize,
    nch: usize,
    lins: &mut [f32],
    base: usize,
) {
    let mut wi = 0usize;
    let dstr = pcm_off + nch - 1;
    let dstl = pcm_off;
    let zlin = ZLIN + base;

    lins[zlin + 4 * 15] = grbuf[xl + 18 * 16];
    lins[zlin + 4 * 15 + 1] = grbuf[xr + 18 * 16];
    lins[zlin + 4 * 15 + 2] = grbuf[xl];
    lins[zlin + 4 * 15 + 3] = grbuf[xr];

    lins[zlin + 4 * 31] = grbuf[xl + 1 + 18 * 16];
    lins[zlin + 4 * 31 + 1] = grbuf[xr + 1 + 18 * 16];
    lins[zlin + 4 * 31 + 2] = grbuf[xl + 1];
    lins[zlin + 4 * 31 + 3] = grbuf[xr + 1];

    // Reference order: the right-channel pairs are written first so that
    // mono (where the two bases coincide) keeps the dstl values. The pair
    // taps address the history region at `base + 4*15`, not `zlin`.
    synth_pair(pcm, dstr, nch, lins, base + 4 * 15 + 1);
    synth_pair(pcm, dstr + 32 * nch, nch, lins, base + 4 * 15 + 64 + 1);
    synth_pair(pcm, dstl, nch, lins, base + 4 * 15);
    synth_pair(pcm, dstl + 32 * nch, nch, lins, base + 4 * 15 + 64);

    for i in (0..15usize).rev() {
        let mut a = [0.0f32; 4];
        let mut b = [0.0f32; 4];

        lins[zlin + 4 * i] = grbuf[xl + 18 * (31 - i)];
        lins[zlin + 4 * i + 1] = grbuf[xr + 18 * (31 - i)];
        lins[zlin + 4 * i + 2] = grbuf[xl + 1 + 18 * (31 - i)];
        lins[zlin + 4 * i + 3] = grbuf[xr + 1 + 18 * (31 - i)];
        lins[zlin + 4 * (i + 16)] = grbuf[xl + 1 + 18 * (1 + i)];
        lins[zlin + 4 * (i + 16) + 1] = grbuf[xr + 1 + 18 * (1 + i)];
        lins[zlin + 4 * (i - 16) + 2] = grbuf[xl + 18 * (1 + i)];
        lins[zlin + 4 * (i - 16) + 3] = grbuf[xr + 18 * (1 + i)];

        for step in 0..8usize {
            let w0 = SYN_WIN[wi];
            let w1 = SYN_WIN[wi + 1];
            wi += 2;
            let vz = zlin + 4 * i - step * 64;
            let vy = zlin + 4 * i - (15 - step) * 64;
            for j in 0..4 {
                let sv = lins[vz + j];
                let yv = lins[vy + j];
                match step {
                    0 => {
                        b[j] = sv * w1 + yv * w0;
                        a[j] = sv * w0 - yv * w1;
                    }
                    // S1 steps (2, 4, 6): accumulate with left-hand sign.
                    s if s % 2 == 0 => {
                        b[j] += sv * w1 + yv * w0;
                        a[j] += sv * w0 - yv * w1;
                    }
                    // S2 steps (1, 3, 5, 7): accumulate with flipped sign.
                    _ => {
                        b[j] += sv * w1 + yv * w0;
                        a[j] += yv * w1 - sv * w0;
                    }
                }
            }
        }

        let put = |pcm: &mut [f32], off: usize, v: f32| pcm[off] = v * PCM_SCALE;
        put(pcm, dstr + (15 - i) * nch, a[1]);
        put(pcm, dstr + (17 + i) * nch, b[1]);
        put(pcm, dstl + (15 - i) * nch, a[0]);
        put(pcm, dstl + (17 + i) * nch, b[0]);
        put(pcm, dstr + (47 - i) * nch, a[3]);
        put(pcm, dstr + (49 + i) * nch, b[3]);
        put(pcm, dstl + (47 - i) * nch, a[2]);
        put(pcm, dstl + (49 + i) * nch, b[2]);
    }
}

/// Synthesizes one granule (18 subband samples × 32 bands per channel)
/// into `pcm` (interleaved), carrying the QMF history in `qmf_state`.
pub fn synth_granule(
    qmf_state: &mut [f32; 960],
    grbuf: &mut [f32],
    nch: usize,
    pcm: &mut [f32],
    lins: &mut [f32],
) {
    const NBANDS: usize = 18;
    for ch in 0..nch {
        dct_ii(&mut grbuf[576 * ch..576 * ch + 576], NBANDS);
    }

    lins[..ZLIN].copy_from_slice(qmf_state);

    let xr_off = 576 * (nch - 1);
    let mut i = 0;
    while i < NBANDS {
        synth(grbuf, i, i + xr_off, pcm, 32 * nch * i, nch, lins, i * 64);
        i += 2;
    }

    if nch == 1 {
        for i in (0..ZLIN).step_by(2) {
            qmf_state[i] = lins[NBANDS * 64 + i];
        }
    } else {
        qmf_state.copy_from_slice(&lins[NBANDS * 64..NBANDS * 64 + 960]);
    }
}
