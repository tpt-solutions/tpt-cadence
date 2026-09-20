//! SBR DSP kernels (reference `sbrdsp.c`).
#![allow(
    clippy::needless_range_loop,
    clippy::too_many_arguments,
    clippy::unnecessary_cast,
    clippy::manual_contains,
    clippy::excessive_precision,
    clippy::erasing_op,
    clippy::identity_op,
    clippy::unused_enumerate_index,
    clippy::manual_div_ceil
)]

/// `sbr_sum_square_c`: pairwise-folded sum of squares.
pub fn sum_square(x: &[[f32; 2]], n: usize) -> f32 {
    let mut sum0 = 0.0f32;
    let mut sum1 = 0.0f32;
    let mut i = 0;
    while i < n {
        sum0 += x[i][0] * x[i][0];
        sum1 += x[i][1] * x[i][1];
        sum0 += x[i + 1][0] * x[i + 1][0];
        sum1 += x[i + 1][1] * x[i + 1][1];
        i += 2;
    }
    sum0 + sum1
}

/// `sbr_sum64x5_c`: folds 320 values into 64.
pub fn sum64x5(z: &mut [f32; 320]) {
    for k in 0..64 {
        let f = z[k] + z[k + 64] + z[k + 128] + z[k + 192] + z[k + 256];
        z[k] = f;
    }
}

/// `sbr_neg_odd_64_c`: negate every fourth sample starting at index 1.
pub fn neg_odd_64(x: &mut [f32; 64]) {
    let mut i = 1;
    while i < 64 {
        x[i] = -x[i];
        x[i + 2] = -x[i + 2];
        i += 4;
    }
}

/// `sbr_qmf_pre_shuffle_c`: folds the 64 analysis values into 128.
pub fn qmf_pre_shuffle(z: &mut [f32; 320]) {
    let (left, right) = z.split_at_mut(64);
    right[0] = left[0];
    right[1] = left[1];
    let mut k = 1;
    while k < 31 {
        right[2 * k] = -left[64 - k];
        right[2 * k + 1] = left[k + 1];
        right[2 * k + 2] = -left[63 - k];
        right[2 * k + 3] = left[k + 2];
        k += 2;
    }
    right[2 * 31] = -left[64 - 31];
    right[2 * 31 + 1] = left[31 + 1];
}

/// `sbr_qmf_post_shuffle_c`: reshuffles the 64 MDCT outputs into complex
/// subband pairs, negating the first of each pair.
pub fn qmf_post_shuffle(w: &mut [[f32; 2]; 32], z: &[f32; 64]) {
    for k in (0..32).step_by(2) {
        w[k][0] = -z[63 - k];
        w[k][1] = z[k];
        w[k + 1][0] = -z[62 - k];
        w[k + 1][1] = z[k + 1];
    }
}

/// `sbr_qmf_deint_bfly_c`: writes 128 values into the synthesis window
/// buffer at `v` (the caller's slice spans into the larger buffer).
pub fn qmf_deint_bfly(v: &mut [f32], src0: &[f32; 64], src1: &[f32; 64]) {
    for i in 0..64 {
        v[i] = src0[i] - src1[63 - i];
        v[127 - i] = src0[i] + src1[63 - i];
    }
}

/// `sbr_autocorrelate_c` (C reference variant).
pub fn autocorrelate(x: &[[f32; 2]; 40], phi: &mut [[[f32; 2]; 2]; 3]) {
    let mut real_sum2 = x[0][0] * x[2][0] + x[0][1] * x[2][1];
    let mut imag_sum2 = x[0][0] * x[2][1] - x[0][1] * x[2][0];
    let mut real_sum1 = 0.0f32;
    let mut imag_sum1 = 0.0f32;
    let mut real_sum0 = 0.0f32;
    for i in 1..38 {
        real_sum0 += x[i][0] * x[i][0] + x[i][1] * x[i][1];
        real_sum1 += x[i][0] * x[i + 1][0] + x[i][1] * x[i + 1][1];
        imag_sum1 += x[i][0] * x[i + 1][1] - x[i][1] * x[i + 1][0];
        real_sum2 += x[i][0] * x[i + 2][0] + x[i][1] * x[i + 2][1];
        imag_sum2 += x[i][0] * x[i + 2][1] - x[i][1] * x[i + 2][0];
    }
    phi[0][1][0] = real_sum2;
    phi[0][1][1] = imag_sum2;
    phi[2][1][0] = real_sum0 + x[0][0] * x[0][0] + x[0][1] * x[0][1];
    phi[1][0][0] = real_sum0 + x[38][0] * x[38][0] + x[38][1] * x[38][1];
    phi[1][1][0] = real_sum1 + x[0][0] * x[1][0] + x[0][1] * x[1][1];
    phi[1][1][1] = imag_sum1 + x[0][0] * x[1][1] - x[0][1] * x[1][0];
    phi[0][0][0] = real_sum1 + x[38][0] * x[39][0] + x[38][1] * x[39][1];
    phi[0][0][1] = imag_sum1 + x[38][0] * x[39][1] - x[38][1] * x[39][0];
}

/// `sbr_hf_gen_c`: the inverse-filtered HF generation for one band.
pub fn hf_gen(
    x_high: &mut [[f32; 2]],
    x_low: &[[f32; 2]],
    alpha0: &[f32; 2],
    alpha1: &[f32; 2],
    bw: f32,
    start: usize,
    end: usize,
) {
    let alpha = [
        alpha1[0] * bw * bw,
        alpha1[1] * bw * bw,
        alpha0[0] * bw,
        alpha0[1] * bw,
    ];
    for i in start..end {
        x_high[i][0] = x_low[i - 2][0] * alpha[0] - x_low[i - 2][1] * alpha[1]
            + x_low[i - 1][0] * alpha[2]
            - x_low[i - 1][1] * alpha[3]
            + x_low[i][0];
        x_high[i][1] = x_low[i - 2][1] * alpha[0]
            + x_low[i - 2][0] * alpha[1]
            + x_low[i - 1][1] * alpha[2]
            + x_low[i - 1][0] * alpha[3]
            + x_low[i][1];
    }
}

/// `sbr_hf_g_filt_c`: gain application over the patch bands.
pub fn hf_g_filt(
    y: &mut [[f32; 2]],
    x_high: &[[[f32; 2]; 40]],
    g_filt: &[f32],
    m_max: usize,
    ixh: usize,
) {
    for m in 0..m_max {
        y[m][0] = x_high[m][ixh][0] * g_filt[m];
        y[m][1] = x_high[m][ixh][1] * g_filt[m];
    }
}

/// `sbr_hf_apply_noise`: adds noise/sinusoidal levels with the alternating
/// sign; `phi_sign0`/`phi_sign1` follow the reference's four variants.
pub fn hf_apply_noise(
    y: &mut [[f32; 2]],
    s_m: &[f32],
    q_filt: &[f32],
    noise: &mut usize,
    phi_sign0: f32,
    mut phi_sign1: f32,
    m_max: usize,
    noise_table: &[[f32; 2]; 512],
) {
    let mut noise = *noise;
    for m in 0..m_max {
        let mut y0 = y[m][0];
        let mut y1 = y[m][1];
        noise = (noise + 1) & 0x1ff;
        if s_m[m] != 0.0 {
            y0 += s_m[m] * phi_sign0;
            y1 += s_m[m] * phi_sign1;
        } else {
            y0 += q_filt[m] * noise_table[noise][0];
            y1 += q_filt[m] * noise_table[noise][1];
        }
        y[m][0] = y0;
        y[m][1] = y1;
        phi_sign1 = -phi_sign1;
    }
}
