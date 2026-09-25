//! QMF-domain Parametric Stereo synthesis, ported from the reference
//! decoder implementation (FFmpeg aacps.c / aacpsdsp_template.c /
//! aacps_tablegen.h). The kernels mirror the C reference closely to
//! preserve its float semantics; `ParametricStereo` (parameter parsing)
//! deliberately stays in [`super::ps`] so its transactional decode can
//! keep cloning a small struct while the synthesis buffers live here.
//!
//! Data layout notes: the reference stores X as two de-interleaved
//! re/im planes (`L[2][38][64]`); this port consumes the SBR crate's
//! interleaved `[[QmfPair; 64]; 38]` slots directly, so every
//! `L[plane][j][i]` becomes `l[j][i].plane`.
#![allow(
    clippy::needless_range_loop,
    clippy::excessive_precision,
    clippy::assign_op_pattern,
    clippy::manual_memcpy
)]

use std::sync::OnceLock;

use super::ps::ParametricStereo;

pub(crate) type QmfPair = (f32, f32);

const PS_MAX_SSB: usize = 91;
const PS_MAX_AP_BANDS: usize = 50;
const PS_AP_LINKS: usize = 3;
const PS_SLOTS: usize = 32;
const PS_MAX_DELAY: usize = 14;
const PS_MAX_AP_DELAY: usize = 5;
const MAX_ENV: usize = 5;
const MAX_PAR: usize = 34;

/// All-pass filter decay slope (reference `DECAY_SLOPE`).
const DECAY_SLOPE: f32 = 0.05;
const NR_PAR_BANDS: [usize; 2] = [20, 34];
const NR_IPDOPD_BANDS: [usize; 2] = [11, 17];
const NR_BANDS: [usize; 2] = [71, 91];
/// Start frequency band for the all-pass filter decay slope.
const DECAY_CUTOFF: [usize; 2] = [10, 32];
const NR_ALLPASS_BANDS: [usize; 2] = [30, 50];
/// First stereo band using the short one-sample delay.
const SHORT_DELAY_BAND: [usize; 2] = [42, 62];

const K_TO_I_20: [i8; 71] = [
    1, 0, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 14, 15, 15, 15, 16, 16, 16, 16, 17, 17,
    17, 17, 17, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 19, 19, 19, 19, 19, 19, 19, 19, 19,
    19, 19, 19, 19, 19, 19, 19, 19, 19, 19, 19, 19, 19, 19, 19, 19, 19, 19, 19, 19,
];

const K_TO_I_34: [i8; 91] = [
    0, 1, 2, 3, 4, 5, 6, 6, 7, 2, 1, 0, 10, 10, 4, 5, 6, 7, 8, 9, 10, 11, 12, 9, 14, 11, 12, 13,
    14, 15, 16, 13, 16, 17, 18, 19, 20, 21, 22, 22, 23, 23, 24, 24, 25, 25, 26, 26, 27, 27, 27, 28,
    28, 28, 29, 29, 29, 30, 30, 30, 31, 31, 31, 31, 32, 32, 32, 32, 33, 33, 33, 33, 33, 33, 33, 33,
    33, 33, 33, 33, 33, 33, 33, 33, 33, 33, 33, 33, 33, 33, 33,
];

fn k_to_i(is34: usize, k: usize) -> usize {
    (if is34 == 1 {
        K_TO_I_34[k]
    } else {
        K_TO_I_20[k]
    }) as usize
}

/// 12-tap symmetric real filter splitting one subband into 2 subsubbands
/// (reference `g1_Q2`); the non-center even coefficients are zero.
const G1_Q2: [f32; 7] = [
    0.0,
    0.01899487526049,
    0.0,
    -0.07293139167538,
    0.0,
    0.30596630545168,
    0.5,
];

/// Generated PS tables (reference `ps_tableinit`), built once on first
/// use with the same float/double arithmetic split as the C table
/// generator so values match its runtime-tablegen build.
pub(crate) struct PsTables {
    pub q_fract_allpass: [[[[f32; 2]; 3]; 50]; 2],
    pub phi_fract: [[[f32; 2]; 50]; 2],
    /// Mono-mixing coefficients (reference `HA`, icc_mode < 3).
    pub ha: [[[f32; 4]; 8]; 46],
    /// Phase-decorrelated mixing coefficients (reference `HB`).
    pub hb: [[[f32; 4]; 8]; 46],
    pub pd_re_smooth: [f32; 512],
    pub pd_im_smooth: [f32; 512],
    pub f20_0_8: [[[f32; 2]; 8]; 8],
    pub f34_0_12: [[[f32; 2]; 8]; 12],
    pub f34_1_8: [[[f32; 2]; 8]; 8],
    pub f34_2_4: [[[f32; 2]; 8]; 4],
}

fn ps_tables() -> &'static PsTables {
    static TABLES: OnceLock<PsTables> = OnceLock::new();
    TABLES.get_or_init(build_tables)
}

fn make_filters_from_proto(bands: usize, proto: &[f32; 7]) -> Vec<[[f32; 2]; 8]> {
    let mut filter = vec![[[0.0f32; 2]; 8]; bands];
    for (q, row) in filter.iter_mut().enumerate() {
        for (n, tap) in row.iter_mut().take(7).enumerate() {
            let theta = 2.0 * std::f64::consts::PI * (q as f64 + 0.5) * (n as i32 - 6) as f64
                / bands as f64;
            *tap = [
                ((proto[n] as f64) * theta.cos()) as f32,
                (-(proto[n] as f64) * theta.sin()) as f32,
            ];
        }
    }
    filter
}

fn build_tables() -> PsTables {
    const IPDOPD_SIN: [f32; 8] = [
        0.0,
        std::f32::consts::FRAC_1_SQRT_2,
        1.0,
        std::f32::consts::FRAC_1_SQRT_2,
        0.0,
        -std::f32::consts::FRAC_1_SQRT_2,
        -1.0,
        -std::f32::consts::FRAC_1_SQRT_2,
    ];
    const IPDOPD_COS: [f32; 8] = [
        1.0,
        std::f32::consts::FRAC_1_SQRT_2,
        0.0,
        -std::f32::consts::FRAC_1_SQRT_2,
        -1.0,
        -std::f32::consts::FRAC_1_SQRT_2,
        0.0,
        std::f32::consts::FRAC_1_SQRT_2,
    ];
    const IID_PAR_DEQUANT: [f32; 46] = [
        // iid_par_dequant_default
        0.05623413251903,
        0.12589254117942,
        0.19952623149689,
        0.31622776601684,
        0.44668359215096,
        0.63095734448019,
        0.79432823472428,
        1.0,
        1.25892541179417,
        1.58489319246111,
        2.23872113856834,
        3.16227766016838,
        5.01187233627272,
        7.94328234724282,
        17.7827941003892,
        // iid_par_dequant_fine
        0.00316227766017,
        0.00562341325190,
        0.01,
        0.01778279410039,
        0.03162277660168,
        0.05623413251903,
        0.07943282347243,
        0.11220184543020,
        0.15848931924611,
        0.22387211385683,
        0.31622776601684,
        0.39810717055350,
        0.50118723362727,
        0.63095734448019,
        0.79432823472428,
        1.0,
        1.25892541179417,
        1.58489319246111,
        1.99526231496888,
        2.51188643150958,
        3.16227766016838,
        4.46683592150963,
        6.30957344480193,
        8.91250938133745,
        12.5892541179417,
        17.7827941003892,
        31.6227766016838,
        56.2341325190349,
        100.0,
        177.827941003892,
        316.227766016837,
    ];
    const ICC_INVQ: [f32; 8] = [1.0, 0.937, 0.84118, 0.60092, 0.36764, 0.0, -0.589, -1.0];
    const ACOS_ICC_INVQ: [f32; 8] = [
        0.0,
        0.35685527,
        0.57133466,
        0.92614472,
        1.1943263,
        std::f32::consts::FRAC_PI_2,
        2.2006171,
        std::f32::consts::PI,
    ];
    const F_CENTER_20: [i8; 10] = [-3, -1, 1, 3, 5, 7, 10, 14, 18, 22];
    const F_CENTER_34: [i8; 32] = [
        2, 6, 10, 14, 18, 22, 26, 30, 34, -10, -6, -2, 51, 57, 15, 21, 27, 33, 39, 45, 54, 66, 78,
        42, 102, 66, 78, 90, 102, 114, 126, 90,
    ];
    const FRACTIONAL_DELAY_LINKS: [f32; 3] = [0.43, 0.75, 0.347];
    const FRACTIONAL_DELAY_GAIN: f32 = 0.39;

    const G0_Q8: [f32; 7] = [
        0.00746082949812,
        0.02270420949825,
        0.04546865930473,
        0.07266113929591,
        0.09885108575264,
        0.11793710567217,
        0.125,
    ];
    const G0_Q12: [f32; 7] = [
        0.04081179924692,
        0.03812810994926,
        0.05144908135699,
        0.06399831151592,
        0.07428313801106,
        0.08100347892914,
        0.08333333333333,
    ];
    const G1_Q8: [f32; 7] = [
        0.01565675600122,
        0.03752716391991,
        0.05417891378782,
        0.08417044116767,
        0.10307344158036,
        0.12222452249753,
        0.125,
    ];
    const G2_Q4: [f32; 7] = [
        -0.05908211155639,
        -0.04871498374946,
        0.0,
        0.07778723915851,
        0.16486303567403,
        0.23279856662996,
        0.25,
    ];

    let mut pd_re_smooth = [0.0f32; 512];
    let mut pd_im_smooth = [0.0f32; 512];
    for pd0 in 0..8 {
        let pd0_re = IPDOPD_COS[pd0];
        let pd0_im = IPDOPD_SIN[pd0];
        for pd1 in 0..8 {
            let pd1_re = IPDOPD_COS[pd1];
            let pd1_im = IPDOPD_SIN[pd1];
            for pd2 in 0..8 {
                let pd2_re = IPDOPD_COS[pd2];
                let pd2_im = IPDOPD_SIN[pd2];
                let re_smooth = 0.25 * pd0_re + 0.5 * pd1_re + pd2_re;
                let im_smooth = 0.25 * pd0_im + 0.5 * pd1_im + pd2_im;
                // reference: pd_mag = 1 / hypot(im, re) computed in double
                let pd_mag = (1.0
                    / (f64::from(im_smooth) * f64::from(im_smooth)
                        + f64::from(re_smooth) * f64::from(re_smooth))
                    .sqrt()) as f32;
                pd_re_smooth[pd0 * 64 + pd1 * 8 + pd2] = re_smooth * pd_mag;
                pd_im_smooth[pd0 * 64 + pd1 * 8 + pd2] = im_smooth * pd_mag;
            }
        }
    }

    let mut ha = [[[0.0f32; 4]; 8]; 46];
    let mut hb = [[[0.0f32; 4]; 8]; 46];
    for (iid, row_ha) in ha.iter_mut().enumerate() {
        let c = IID_PAR_DEQUANT[iid];
        let c1 = std::f32::consts::SQRT_2 / (1.0 + c * c).sqrt();
        let c2 = c * c1;
        for (icc, cell) in row_ha.iter_mut().enumerate() {
            let alpha = 0.5 * ACOS_ICC_INVQ[icc];
            let beta = alpha * (c1 - c2) * std::f32::consts::FRAC_1_SQRT_2;
            *cell = [
                c2 * (beta + alpha).cos(),
                c1 * (beta - alpha).cos(),
                c2 * (beta + alpha).sin(),
                c1 * (beta - alpha).sin(),
            ];
            let rho = ICC_INVQ[icc].max(0.05);
            let mut alpha = 0.5 * (2.0 * c * rho).atan2(c * c - 1.0);
            let mut mu = c + 1.0 / c;
            mu = (1.0 + (4.0 * rho * rho - 4.0) / (mu * mu)).sqrt();
            let gamma = ((1.0 - mu) / (1.0 + mu)).sqrt().atan();
            if alpha < 0.0 {
                alpha += std::f32::consts::FRAC_PI_2;
            }
            let alpha_c = alpha.cos();
            let alpha_s = alpha.sin();
            let gamma_c = gamma.cos();
            let gamma_s = gamma.sin();
            hb[iid][icc] = [
                std::f32::consts::SQRT_2 * alpha_c * gamma_c,
                std::f32::consts::SQRT_2 * alpha_s * gamma_c,
                -std::f32::consts::SQRT_2 * alpha_s * gamma_s,
                std::f32::consts::SQRT_2 * alpha_c * gamma_s,
            ];
        }
    }

    let mut q_fract_allpass = [[[[0.0f32; 2]; 3]; 50]; 2];
    let mut phi_fract = [[[0.0f32; 2]; 50]; 2];
    for k in 0..NR_ALLPASS_BANDS[0] {
        let f_center: f64 = if k < F_CENTER_20.len() {
            f64::from(F_CENTER_20[k]) * 0.125
        } else {
            f64::from(k as i32 - 6) - 0.5
        };
        for (m, link) in q_fract_allpass[0][k].iter_mut().enumerate() {
            let theta = -std::f64::consts::PI * f64::from(FRACTIONAL_DELAY_LINKS[m]) * f_center;
            *link = [theta.cos() as f32, theta.sin() as f32];
        }
        let theta = -std::f64::consts::PI * f64::from(FRACTIONAL_DELAY_GAIN) * f_center;
        phi_fract[0][k] = [theta.cos() as f32, theta.sin() as f32];
    }
    for k in 0..NR_ALLPASS_BANDS[1] {
        let f_center: f64 = if k < F_CENTER_34.len() {
            f64::from(F_CENTER_34[k]) / 24.0
        } else {
            f64::from(k as i32 - 26) - 0.5
        };
        for (m, link) in q_fract_allpass[1][k].iter_mut().enumerate() {
            let theta = -std::f64::consts::PI * f64::from(FRACTIONAL_DELAY_LINKS[m]) * f_center;
            *link = [theta.cos() as f32, theta.sin() as f32];
        }
        let theta = -std::f64::consts::PI * f64::from(FRACTIONAL_DELAY_GAIN) * f_center;
        phi_fract[1][k] = [theta.cos() as f32, theta.sin() as f32];
    }

    let mut f20_0_8 = [[[0.0f32; 2]; 8]; 8];
    for (r, row) in make_filters_from_proto(8, &G0_Q8).into_iter().enumerate() {
        f20_0_8[r] = row;
    }
    let mut f34_0_12 = [[[0.0f32; 2]; 8]; 12];
    for (r, row) in make_filters_from_proto(12, &G0_Q12).into_iter().enumerate() {
        f34_0_12[r] = row;
    }
    let mut f34_1_8 = [[[0.0f32; 2]; 8]; 8];
    for (r, row) in make_filters_from_proto(8, &G1_Q8).into_iter().enumerate() {
        f34_1_8[r] = row;
    }
    let mut f34_2_4 = [[[0.0f32; 2]; 8]; 4];
    for (r, row) in make_filters_from_proto(4, &G2_Q4).into_iter().enumerate() {
        f34_2_4[r] = row;
    }

    PsTables {
        q_fract_allpass,
        phi_fract,
        ha,
        hb,
        pd_re_smooth,
        pd_im_smooth,
        f20_0_8,
        f34_0_12,
        f34_1_8,
        f34_2_4,
    }
}

/// Persistent PS synthesis state (reference `PSContext` minus the parsed
/// parameters, which live in [`ParametricStereo`]). Struct-resident rather
/// than stack-resident: the delay buffers alone are ~130 KB.
pub struct PsSynthesis {
    /// 12-tap hybrid filter history: 6 kept slots per filter bank input.
    in_buf: [[[f32; 2]; 44]; 5],
    /// Decorrelation input delay: `delay[band][slot][re/im]`.
    delay: [[[f32; 2]; PS_SLOTS + PS_MAX_DELAY]; PS_MAX_SSB],
    /// All-pass link delays: `ap_delay[band][link][slot][re/im]`.
    ap_delay: [[[[f32; 2]; PS_SLOTS + PS_MAX_AP_DELAY]; PS_AP_LINKS]; PS_MAX_AP_BANDS],
    peak_decay_nrg: [f32; MAX_PAR],
    power_smooth: [f32; MAX_PAR],
    peak_decay_diff_smooth: [f32; MAX_PAR],
    /// Stereo mixing coefficients `[re/im plane][env][par band]`.
    h11: [[[f32; MAX_PAR]; MAX_ENV + 1]; 2],
    h12: [[[f32; MAX_PAR]; MAX_ENV + 1]; 2],
    h21: [[[f32; MAX_PAR]; MAX_ENV + 1]; 2],
    h22: [[[f32; MAX_PAR]; MAX_ENV + 1]; 2],
    lbuf: [[[f32; 2]; PS_SLOTS]; PS_MAX_SSB],
    rbuf: [[[f32; 2]; PS_SLOTS]; PS_MAX_SSB],
    ipd_hist: [i8; MAX_PAR],
    opd_hist: [i8; MAX_PAR],
}

impl Default for PsSynthesis {
    fn default() -> Self {
        Self::new()
    }
}

impl PsSynthesis {
    pub fn new() -> Self {
        PsSynthesis {
            in_buf: [[[0.0; 2]; 44]; 5],
            delay: [[[0.0; 2]; PS_SLOTS + PS_MAX_DELAY]; PS_MAX_SSB],
            ap_delay: [[[[0.0; 2]; PS_SLOTS + PS_MAX_AP_DELAY]; PS_AP_LINKS]; PS_MAX_AP_BANDS],
            peak_decay_nrg: [0.0; MAX_PAR],
            power_smooth: [0.0; MAX_PAR],
            peak_decay_diff_smooth: [0.0; MAX_PAR],
            h11: [[[0.0; MAX_PAR]; MAX_ENV + 1]; 2],
            h12: [[[0.0; MAX_PAR]; MAX_ENV + 1]; 2],
            h21: [[[0.0; MAX_PAR]; MAX_ENV + 1]; 2],
            h22: [[[0.0; MAX_PAR]; MAX_ENV + 1]; 2],
            lbuf: [[[0.0; 2]; PS_SLOTS]; PS_MAX_SSB],
            rbuf: [[[0.0; 2]; PS_SLOTS]; PS_MAX_SSB],
            ipd_hist: [0; MAX_PAR],
            opd_hist: [0; MAX_PAR],
        }
    }

    /// Zero every buffer without reallocating (`sbr_turnoff` analogue).
    pub fn reset(&mut self) {
        *self = PsSynthesis::new();
    }

    /// `ff_ps_apply`: transform the mono QMF frame `l` into a stereo pair
    /// `(l, r)` in the QMF domain. `top` is `kx[1] + m[1]` — the highest
    /// QMF band carrying data.
    pub fn apply(
        &mut self,
        ps: &ParametricStereo,
        l: &mut [[QmfPair; 64]],
        r: &mut [[QmfPair; 64]],
        top: usize,
    ) {
        let is34 = usize::from(ps.is34bands);

        let top = top + NR_BANDS[is34] - 64;
        for band in self.delay[top..NR_BANDS[is34]].iter_mut() {
            *band = [[0.0; 2]; PS_SLOTS + PS_MAX_DELAY];
        }
        if top < NR_ALLPASS_BANDS[is34] {
            for band in self.ap_delay[top..NR_ALLPASS_BANDS[is34]].iter_mut() {
                *band = [[[0.0; 2]; PS_SLOTS + PS_MAX_AP_DELAY]; PS_AP_LINKS];
            }
        }

        self.hybrid_analysis(l, is34);
        self.decorrelation(ps, is34);
        self.stereo_processing(ps, is34);
        self.hybrid_synthesis(l, &self.lbuf, is34);
        self.hybrid_synthesis(r, &self.rbuf, is34);
    }

    /// `hybrid_analysis`: split the lowest QMF bands into subsubbands.
    fn hybrid_analysis(&mut self, l: &[[QmfPair; 64]], is34: usize) {
        for i in 0..5 {
            for j in 0..38 {
                let src = l[j][i];
                self.in_buf[i][j + 6] = [src.0, src.1];
            }
        }
        let t = ps_tables();
        if is34 == 1 {
            hybrid_4_8_12(&mut self.lbuf[0..12], &self.in_buf[0], &t.f34_0_12, 12);
            hybrid_4_8_12(&mut self.lbuf[12..20], &self.in_buf[1], &t.f34_1_8, 8);
            hybrid_4_8_12(&mut self.lbuf[20..24], &self.in_buf[2], &t.f34_2_4, 4);
            hybrid_4_8_12(&mut self.lbuf[24..28], &self.in_buf[3], &t.f34_2_4, 4);
            hybrid_4_8_12(&mut self.lbuf[28..32], &self.in_buf[4], &t.f34_2_4, 4);
            // Direct copy of the unsplit QMF bands (ileave): lbuf[27 + i]
            // takes QMF band i for i in 5..64.
            for i in 5..64 {
                for j in 0..PS_SLOTS {
                    let src = l[j][i];
                    self.lbuf[27 + i][j] = [src.0, src.1];
                }
            }
        } else {
            hybrid_6_cx(&mut self.lbuf[0..6], &self.in_buf[0], &t.f20_0_8);
            hybrid_2_re(&mut self.lbuf[6..8], &self.in_buf[1], true);
            hybrid_2_re(&mut self.lbuf[8..10], &self.in_buf[2], false);
            for i in 3..64 {
                for j in 0..PS_SLOTS {
                    let src = l[j][i];
                    self.lbuf[7 + i][j] = [src.0, src.1];
                }
            }
        }
        // Keep the 6 most recent slots as the convolution history.
        for i in 0..5 {
            for j in 0..6 {
                self.in_buf[i][j] = self.in_buf[i][j + 32];
            }
        }
    }

    /// `decorrelation`: transient-aware all-pass/linear-phase delays
    /// producing the surround-only component of the right channel.
    fn decorrelation(&mut self, ps: &ParametricStereo, is34: usize) {
        let mut power = [[0.0f32; PS_SLOTS]; MAX_PAR];
        let mut transient_gain = [[0.0f32; PS_SLOTS]; MAX_PAR];
        const PEAK_DECAY_FACTOR: f32 = 0.76592833836465;
        let transient_impact = 1.5f32;
        let a_smooth = 0.25f32;

        if (is34 == 1) != ps.is34bands_old {
            self.peak_decay_nrg = [0.0; MAX_PAR];
            self.power_smooth = [0.0; MAX_PAR];
            self.peak_decay_diff_smooth = [0.0; MAX_PAR];
            self.delay = [[[0.0; 2]; PS_SLOTS + PS_MAX_DELAY]; PS_MAX_SSB];
            self.ap_delay =
                [[[[0.0; 2]; PS_SLOTS + PS_MAX_AP_DELAY]; PS_AP_LINKS]; PS_MAX_AP_BANDS];
        }

        for k in 0..NR_BANDS[is34] {
            let i = k_to_i(is34, k);
            for n in 0..PS_SLOTS {
                power[i][n] += self.lbuf[k][n][0] * self.lbuf[k][n][0]
                    + self.lbuf[k][n][1] * self.lbuf[k][n][1];
            }
        }

        // Transient detection
        for (i, row) in transient_gain
            .iter_mut()
            .enumerate()
            .take(NR_PAR_BANDS[is34])
        {
            for n in 0..PS_SLOTS {
                let decayed_peak = PEAK_DECAY_FACTOR * self.peak_decay_nrg[i];
                self.peak_decay_nrg[i] = decayed_peak.max(power[i][n]);
                self.power_smooth[i] += a_smooth * (power[i][n] - self.power_smooth[i]);
                self.peak_decay_diff_smooth[i] += a_smooth
                    * (self.peak_decay_nrg[i] - power[i][n] - self.peak_decay_diff_smooth[i]);
                let denom = transient_impact * self.peak_decay_diff_smooth[i];
                row[n] = if denom > self.power_smooth[i] {
                    self.power_smooth[i] / denom
                } else {
                    1.0
                };
            }
        }

        // Decorrelation and transient reduction
        let t = ps_tables();
        for k in 0..NR_ALLPASS_BANDS[is34] {
            let b = k_to_i(is34, k);
            let g_decay_slope =
                (1.0 - DECAY_SLOPE * (k as i32 - DECAY_CUTOFF[is34] as i32) as f32).clamp(0.0, 1.0);
            self.delay[k].copy_within(PS_SLOTS..PS_SLOTS + PS_MAX_DELAY, 0);
            for n in 0..PS_SLOTS {
                self.delay[k][PS_MAX_DELAY + n] = self.lbuf[k][n];
            }
            for m in 0..PS_AP_LINKS {
                self.ap_delay[k][m].copy_within(PS_SLOTS..PS_SLOTS + 5, 0);
            }
            ps_decorrelate(
                &mut self.rbuf[k],
                &self.delay[k][PS_MAX_DELAY - 2..],
                &mut self.ap_delay[k],
                &t.phi_fract[is34][k],
                &t.q_fract_allpass[is34][k],
                &transient_gain[b],
                g_decay_slope,
                PS_SLOTS,
            );
        }
        for k in NR_ALLPASS_BANDS[is34]..SHORT_DELAY_BAND[is34] {
            let i = k_to_i(is34, k);
            self.delay[k].copy_within(PS_SLOTS..PS_SLOTS + PS_MAX_DELAY, 0);
            for n in 0..PS_SLOTS {
                self.delay[k][PS_MAX_DELAY + n] = self.lbuf[k][n];
            }
            // H = delay 14
            for n in 0..PS_SLOTS {
                let src = self.delay[k][n];
                self.rbuf[k][n] = [src[0] * transient_gain[i][n], src[1] * transient_gain[i][n]];
            }
        }
        for k in SHORT_DELAY_BAND[is34]..NR_BANDS[is34] {
            let i = k_to_i(is34, k);
            self.delay[k].copy_within(PS_SLOTS..PS_SLOTS + PS_MAX_DELAY, 0);
            for n in 0..PS_SLOTS {
                self.delay[k][PS_MAX_DELAY + n] = self.lbuf[k][n];
            }
            // H = delay 1
            for n in 0..PS_SLOTS {
                let src = self.delay[k][PS_MAX_DELAY - 1 + n];
                self.rbuf[k][n] = [src[0] * transient_gain[i][n], src[1] * transient_gain[i][n]];
            }
        }
    }

    /// `stereo_processing`: map IID/ICC/IPD/OPD parameters onto QMF bands
    /// and mix L/R with per-slot interpolated coefficients.
    fn stereo_processing(&mut self, ps: &ParametricStereo, is34: usize) {
        let t = ps_tables();

        // Carry the previous frame's last-envelope coefficients forward.
        if ps.num_env_old != 0 {
            for plane in 0..2 {
                self.h11[plane][0] = self.h11[plane][ps.num_env_old];
                self.h12[plane][0] = self.h12[plane][ps.num_env_old];
                self.h21[plane][0] = self.h21[plane][ps.num_env_old];
                self.h22[plane][0] = self.h22[plane][ps.num_env_old];
            }
        }

        let mut iid_mapped = [[0i8; MAX_PAR]; MAX_ENV];
        let mut icc_mapped = [[0i8; MAX_PAR]; MAX_ENV];
        let mut ipd_mapped = [[0i8; MAX_PAR]; MAX_ENV];
        let mut opd_mapped = [[0i8; MAX_PAR]; MAX_ENV];
        if is34 == 1 {
            remap_34(
                &mut iid_mapped,
                &ps.iid_par,
                ps.nr_iid_par,
                ps.num_env,
                true,
            );
            remap_34(
                &mut icc_mapped,
                &ps.icc_par,
                ps.nr_icc_par,
                ps.num_env,
                true,
            );
            if ps.enable_ipdopd {
                remap_34(
                    &mut ipd_mapped,
                    &ps.ipd_par,
                    ps.nr_ipdopd_par,
                    ps.num_env,
                    false,
                );
                remap_34(
                    &mut opd_mapped,
                    &ps.opd_par,
                    ps.nr_ipdopd_par,
                    ps.num_env,
                    false,
                );
            }
            if !ps.is34bands_old {
                for plane in 0..2 {
                    map_val_20_to_34(&mut self.h11[plane][0]);
                    map_val_20_to_34(&mut self.h12[plane][0]);
                    map_val_20_to_34(&mut self.h21[plane][0]);
                    map_val_20_to_34(&mut self.h22[plane][0]);
                }
                self.ipd_hist = [0; MAX_PAR];
                self.opd_hist = [0; MAX_PAR];
            }
        } else {
            remap_20(
                &mut iid_mapped,
                &ps.iid_par,
                ps.nr_iid_par,
                ps.num_env,
                true,
            );
            remap_20(
                &mut icc_mapped,
                &ps.icc_par,
                ps.nr_icc_par,
                ps.num_env,
                true,
            );
            if ps.enable_ipdopd {
                remap_20(
                    &mut ipd_mapped,
                    &ps.ipd_par,
                    ps.nr_ipdopd_par,
                    ps.num_env,
                    false,
                );
                remap_20(
                    &mut opd_mapped,
                    &ps.opd_par,
                    ps.nr_ipdopd_par,
                    ps.num_env,
                    false,
                );
            }
            if ps.is34bands_old {
                for plane in 0..2 {
                    map_val_34_to_20(&mut self.h11[plane][0]);
                    map_val_34_to_20(&mut self.h12[plane][0]);
                    map_val_34_to_20(&mut self.h21[plane][0]);
                    map_val_34_to_20(&mut self.h22[plane][0]);
                }
                self.ipd_hist = [0; MAX_PAR];
                self.opd_hist = [0; MAX_PAR];
            }
        }

        let h_lut: &[[[f32; 4]; 8]; 46] = if ps.icc_mode < 3 { &t.ha } else { &t.hb };

        // Mixing
        for e in 0..ps.num_env {
            for b in 0..NR_PAR_BANDS[is34] {
                let lut_row = &h_lut
                    [(i32::from(iid_mapped[e][b]) + 7 + 23 * i32::from(ps.iid_quant)) as usize];
                let lut_cell = &lut_row[icc_mapped[e][b] as usize];
                let mut h11 = lut_cell[0];
                let mut h12 = lut_cell[1];
                let mut h21 = lut_cell[2];
                let mut h22 = lut_cell[3];

                if ps.enable_ipdopd && b < NR_IPDOPD_BANDS[is34] {
                    // The spec says to only run this smoother when
                    // enable_ipdopd is set, but the reference decoder
                    // appears to run it constantly; either way the state
                    // update below only matters in this branch.
                    let opd_idx = i32::from(self.opd_hist[b]) * 8 + i32::from(opd_mapped[e][b]);
                    let ipd_idx = i32::from(self.ipd_hist[b]) * 8 + i32::from(ipd_mapped[e][b]);
                    let opd_re = t.pd_re_smooth[opd_idx as usize];
                    let opd_im = t.pd_im_smooth[opd_idx as usize];
                    let ipd_re = t.pd_re_smooth[ipd_idx as usize];
                    let ipd_im = t.pd_im_smooth[ipd_idx as usize];
                    self.opd_hist[b] = (opd_idx & 0x3F) as i8;
                    self.ipd_hist[b] = (ipd_idx & 0x3F) as i8;

                    let ipd_adj_re = opd_re * ipd_re + opd_im * ipd_im;
                    let ipd_adj_im = opd_im * ipd_re - opd_re * ipd_im;
                    let h11i = h11 * opd_im;
                    h11 = h11 * opd_re;
                    let h12i = h12 * ipd_adj_im;
                    h12 = h12 * ipd_adj_re;
                    let h21i = h21 * opd_im;
                    h21 = h21 * opd_re;
                    let h22i = h22 * ipd_adj_im;
                    h22 = h22 * ipd_adj_re;
                    self.h11[1][e + 1][b] = h11i;
                    self.h12[1][e + 1][b] = h12i;
                    self.h21[1][e + 1][b] = h21i;
                    self.h22[1][e + 1][b] = h22i;
                }
                self.h11[0][e + 1][b] = h11;
                self.h12[0][e + 1][b] = h12;
                self.h21[0][e + 1][b] = h21;
                self.h22[0][e + 1][b] = h22;
            }
            for k in 0..NR_BANDS[is34] {
                let mut h = [[0.0f32; 4]; 2];
                let mut h_step = [[0.0f32; 4]; 2];
                let start = ps.border_position[e];
                let stop = ps.border_position[e + 1];
                let width = 1.0f32
                    / (if stop != start {
                        (stop - start) as f32
                    } else {
                        1.0
                    });
                let b = k_to_i(is34, k);
                h[0] = [
                    self.h11[0][e][b],
                    self.h12[0][e][b],
                    self.h21[0][e][b],
                    self.h22[0][e][b],
                ];
                if ps.enable_ipdopd {
                    // Is this necessary? ps_04_new seems unchanged
                    let sign = if (is34 == 1 && (9..=13).contains(&k)) || (is34 == 0 && k <= 1) {
                        -1.0f32
                    } else {
                        1.0
                    };
                    h[1] = [
                        sign * self.h11[1][e][b],
                        sign * self.h12[1][e][b],
                        sign * self.h21[1][e][b],
                        sign * self.h22[1][e][b],
                    ];
                }
                // Interpolation
                for (j, step) in h_step[0].iter_mut().enumerate() {
                    *step = (self.h_by_index(j)[0][e + 1][b] - h[0][j]) * width;
                }
                if ps.enable_ipdopd {
                    for (j, step) in h_step[1].iter_mut().enumerate() {
                        *step = (self.h_by_index(j)[1][e + 1][b] - h[1][j]) * width;
                    }
                }
                if stop - start != 0 {
                    let s = (1 + start) as usize;
                    let len = (stop - start) as usize;
                    let (l_slot, r_slot) = (&mut self.lbuf[k], &mut self.rbuf[k]);
                    if ps.enable_ipdopd {
                        stereo_interpolate_ipdopd(
                            &mut l_slot[s..s + len],
                            &mut r_slot[s..s + len],
                            &h,
                            &h_step,
                        );
                    } else {
                        stereo_interpolate(
                            &mut l_slot[s..s + len],
                            &mut r_slot[s..s + len],
                            &h,
                            &h_step,
                        );
                    }
                }
            }
        }
    }

    fn h_by_index(&self, j: usize) -> &[[[f32; MAX_PAR]; MAX_ENV + 1]; 2] {
        match j {
            0 => &self.h11,
            1 => &self.h12,
            2 => &self.h21,
            _ => &self.h22,
        }
    }

    /// `hybrid_synthesis`: fold subsubbands back into 64 QMF bands.
    fn hybrid_synthesis(
        &self,
        out: &mut [[QmfPair; 64]],
        in_: &[[[f32; 2]; PS_SLOTS]],
        is34: usize,
    ) {
        if is34 == 1 {
            for n in 0..PS_SLOTS {
                for k in 0..5 {
                    out[n][k] = (0.0, 0.0);
                }
                for i in 0..12 {
                    out[n][0].0 += in_[i][n][0];
                    out[n][0].1 += in_[i][n][1];
                }
                for i in 0..8 {
                    out[n][1].0 += in_[12 + i][n][0];
                    out[n][1].1 += in_[12 + i][n][1];
                }
                for i in 0..4 {
                    out[n][2].0 += in_[20 + i][n][0];
                    out[n][2].1 += in_[20 + i][n][1];
                    out[n][3].0 += in_[24 + i][n][0];
                    out[n][3].1 += in_[24 + i][n][1];
                    out[n][4].0 += in_[28 + i][n][0];
                    out[n][4].1 += in_[28 + i][n][1];
                }
            }
            for i in 5..64 {
                for n in 0..PS_SLOTS {
                    out[n][i].0 = in_[27 + i][n][0];
                    out[n][i].1 = in_[27 + i][n][1];
                }
            }
        } else {
            for n in 0..PS_SLOTS {
                out[n][0] = (
                    in_[0][n][0]
                        + in_[1][n][0]
                        + in_[2][n][0]
                        + in_[3][n][0]
                        + in_[4][n][0]
                        + in_[5][n][0],
                    in_[0][n][1]
                        + in_[1][n][1]
                        + in_[2][n][1]
                        + in_[3][n][1]
                        + in_[4][n][1]
                        + in_[5][n][1],
                );
                out[n][1] = (in_[6][n][0] + in_[7][n][0], in_[6][n][1] + in_[7][n][1]);
                out[n][2] = (in_[8][n][0] + in_[9][n][0], in_[8][n][1] + in_[9][n][1]);
            }
            for i in 3..64 {
                for n in 0..PS_SLOTS {
                    out[n][i].0 = in_[7 + i][n][0];
                    out[n][i].1 = in_[7 + i][n][1];
                }
            }
        }
    }
}

/// `ps_hybrid_analysis` DSP kernel: n-tap complex convolution of the
/// symmetric window around the 12-sample frame history. The reference
/// writes `out[i * stride]` over a flat pair pointer; here the caller
/// receives the n band results in order.
fn hybrid_analysis_kernel(
    out: &mut [[f32; 2]],
    in_: &[[f32; 2]],
    filter: &[[[f32; 2]; 8]],
    n: usize,
) {
    let mut inre0 = [0.0f32; 6];
    let mut inre1 = [0.0f32; 6];
    let mut inim0 = [0.0f32; 6];
    let mut inim1 = [0.0f32; 6];

    for (j, cell) in inre0.iter_mut().enumerate() {
        *cell = in_[j][0] + in_[12 - j][0];
        inre1[j] = in_[j][1] - in_[12 - j][1];
        inim0[j] = in_[j][1] + in_[12 - j][1];
        inim1[j] = in_[j][0] - in_[12 - j][0];
    }

    for i in 0..n {
        let mut sum_re = filter[i][6][0] * in_[6][0];
        let mut sum_im = filter[i][6][0] * in_[6][1];
        for j in 0..6 {
            sum_re += filter[i][j][0] * inre0[j] - filter[i][j][1] * inre1[j];
            sum_im += filter[i][j][0] * inim0[j] + filter[i][j][1] * inim1[j];
        }
        out[i] = [sum_re, sum_im];
    }
}

/// `hybrid6_cx`: split one subband into 6 subsubbands with the complex
/// type-A filter, reordering the prototype outputs.
fn hybrid_6_cx(
    out: &mut [[[f32; 2]; PS_SLOTS]],
    in_: &[[f32; 2]; 44],
    filter: &[[[f32; 2]; 8]; 8],
) {
    let mut temp = [[0.0f32; 2]; 8];
    for i in 0..PS_SLOTS {
        hybrid_analysis_kernel(&mut temp, &in_[i..], filter, 8);
        out[0][i] = temp[6];
        out[1][i] = temp[7];
        out[2][i] = temp[0];
        out[3][i] = temp[1];
        out[4][i] = [temp[2][0] + temp[5][0], temp[2][1] + temp[5][1]];
        out[5][i] = [temp[3][0] + temp[4][0], temp[3][1] + temp[4][1]];
    }
}

/// `hybrid2_re`: split one subband into 2 subsubbands with a symmetric
/// real filter.
fn hybrid_2_re(out: &mut [[[f32; 2]; PS_SLOTS]], in_: &[[f32; 2]; 44], reverse: bool) {
    for i in 0..PS_SLOTS {
        let in_ = &in_[i..];
        let re_in = G1_Q2[6] * in_[6][0];
        let mut re_op = 0.0f32;
        let im_in = G1_Q2[6] * in_[6][1];
        let mut im_op = 0.0f32;
        for j in (0..6).step_by(2) {
            re_op += G1_Q2[j + 1] * (in_[j + 1][0] + in_[12 - j - 1][0]);
            im_op += G1_Q2[j + 1] * (in_[j + 1][1] + in_[12 - j - 1][1]);
        }
        out[usize::from(reverse)][i] = [re_in + re_op, im_in + im_op];
        out[usize::from(!reverse)][i] = [re_in - re_op, im_in - im_op];
    }
}

/// `hybrid4_8_12_cx`: split one subband into N subsubbands.
fn hybrid_4_8_12(
    out: &mut [[[f32; 2]; PS_SLOTS]],
    in_: &[[f32; 2]; 44],
    filter: &[[[f32; 2]; 8]],
    n: usize,
) {
    let mut temp = [[0.0f32; 2]; 12];
    for i in 0..PS_SLOTS {
        hybrid_analysis_kernel(&mut temp, &in_[i..], filter, n);
        for (j, cell) in temp.iter_mut().enumerate().take(n) {
            out[j][i] = *cell;
        }
    }
}

/// `ps_decorrelate` DSP kernel: 3-link all-pass cascade with fractional
/// delays and decay-slope damping.
#[allow(clippy::too_many_arguments)]
fn ps_decorrelate(
    out: &mut [[f32; 2]; PS_SLOTS],
    delay: &[[f32; 2]],
    ap_delay: &mut [[[f32; 2]; PS_SLOTS + PS_MAX_AP_DELAY]; PS_AP_LINKS],
    phi_fract: &[f32; 2],
    q_fract: &[[f32; 2]; 3],
    transient_gain: &[f32],
    g_decay_slope: f32,
    len: usize,
) {
    const A: [f32; 3] = [0.65143905753106, 0.56471812200776, 0.48954165955695];
    let ag = [
        A[0] * g_decay_slope,
        A[1] * g_decay_slope,
        A[2] * g_decay_slope,
    ];

    for n in 0..len {
        let mut in_re = delay[n][0] * phi_fract[0] - delay[n][1] * phi_fract[1];
        let mut in_im = delay[n][0] * phi_fract[1] + delay[n][1] * phi_fract[0];
        for m in 0..PS_AP_LINKS {
            let a_re = ag[m] * in_re;
            let a_im = ag[m] * in_im;
            let link_delay_re = ap_delay[m][n + 2 - m][0];
            let link_delay_im = ap_delay[m][n + 2 - m][1];
            let fractional_delay_re = q_fract[m][0];
            let fractional_delay_im = q_fract[m][1];
            let apd_re = in_re;
            let apd_im = in_im;
            in_re = link_delay_re * fractional_delay_re - link_delay_im * fractional_delay_im;
            in_re -= a_re;
            in_im = link_delay_re * fractional_delay_im + link_delay_im * fractional_delay_re;
            in_im -= a_im;
            ap_delay[m][n + 5] = [apd_re + ag[m] * in_re, apd_im + ag[m] * in_im];
        }
        out[n] = [transient_gain[n] * in_re, transient_gain[n] * in_im];
    }
}

/// `ps_stereo_interpolate` DSP kernel (no ipd/opd).
fn stereo_interpolate(
    l: &mut [[f32; 2]],
    r: &mut [[f32; 2]],
    h: &[[f32; 4]; 2],
    h_step: &[[f32; 4]; 2],
) {
    let mut h0 = h[0][0];
    let mut h1 = h[0][1];
    let mut h2 = h[0][2];
    let mut h3 = h[0][3];
    let hs0 = h_step[0][0];
    let hs1 = h_step[0][1];
    let hs2 = h_step[0][2];
    let hs3 = h_step[0][3];

    for n in 0..l.len() {
        // l is s, r is d
        let l_re = l[n][0];
        let l_im = l[n][1];
        let r_re = r[n][0];
        let r_im = r[n][1];
        h0 += hs0;
        h1 += hs1;
        h2 += hs2;
        h3 += hs3;
        l[n] = [h0 * l_re + h2 * r_re, h0 * l_im + h2 * r_im];
        r[n] = [h1 * l_re + h3 * r_re, h1 * l_im + h3 * r_im];
    }
}

/// `ps_stereo_interpolate_ipdopd` DSP kernel.
fn stereo_interpolate_ipdopd(
    l: &mut [[f32; 2]],
    r: &mut [[f32; 2]],
    h: &[[f32; 4]; 2],
    h_step: &[[f32; 4]; 2],
) {
    let (mut h00, mut h01, mut h02, mut h03) = (h[0][0], h[0][1], h[0][2], h[0][3]);
    let (mut h10, mut h11, mut h12, mut h13) = (h[1][0], h[1][1], h[1][2], h[1][3]);
    let (hs00, hs01, hs02, hs03) = (h_step[0][0], h_step[0][1], h_step[0][2], h_step[0][3]);
    let (hs10, hs11, hs12, hs13) = (h_step[1][0], h_step[1][1], h_step[1][2], h_step[1][3]);

    for n in 0..l.len() {
        // l is s, r is d
        let l_re = l[n][0];
        let l_im = l[n][1];
        let r_re = r[n][0];
        let r_im = r[n][1];
        h00 += hs00;
        h01 += hs01;
        h02 += hs02;
        h03 += hs03;
        h10 += hs10;
        h11 += hs11;
        h12 += hs12;
        h13 += hs13;

        l[n] = [
            h00 * l_re + h02 * r_re - h10 * l_im - h12 * r_im,
            h00 * l_im + h02 * r_im + h10 * l_re + h12 * r_re,
        ];
        r[n] = [
            h01 * l_re + h03 * r_re - h11 * l_im - h13 * r_im,
            h01 * l_im + h03 * r_im + h11 * l_re + h13 * r_re,
        ];
    }
}

// ---------------------------------------------------------------------------
// Parameter band remapping (reference map_idx_* / map_val_* / remap*_).
// ---------------------------------------------------------------------------

/// `remap34`: place 20/34-band parameters on 34 par-band indices.
fn remap_34(
    par_mapped: &mut [[i8; MAX_PAR]; MAX_ENV],
    par: &[[i8; MAX_PAR]; MAX_ENV],
    num_par: usize,
    num_env: usize,
    full: bool,
) {
    if num_par == 20 || num_par == 11 {
        for e in 0..num_env {
            map_idx_20_to_34(&mut par_mapped[e], &par[e], full);
        }
    } else if num_par == 10 || num_par == 5 {
        for e in 0..num_env {
            map_idx_10_to_34(&mut par_mapped[e], &par[e], full);
        }
    } else {
        for e in 0..num_env {
            par_mapped[e] = par[e];
        }
    }
}

/// `remap20`: place 34/20/10-band parameters on 20 par-band indices.
fn remap_20(
    par_mapped: &mut [[i8; MAX_PAR]; MAX_ENV],
    par: &[[i8; MAX_PAR]; MAX_ENV],
    num_par: usize,
    num_env: usize,
    full: bool,
) {
    if num_par == 34 || num_par == 17 {
        for e in 0..num_env {
            map_idx_34_to_20(&mut par_mapped[e], &par[e], full);
        }
    } else if num_par == 10 || num_par == 5 {
        for e in 0..num_env {
            map_idx_10_to_20(&mut par_mapped[e], &par[e], full);
        }
    } else {
        for e in 0..num_env {
            par_mapped[e] = par[e];
        }
    }
}

/// Table 8.46.
fn map_idx_10_to_20(par_mapped: &mut [i8; MAX_PAR], par: &[i8; MAX_PAR], full: bool) {
    if !full {
        par_mapped[10] = 0;
    }
    for b in (0..=if full { 9 } else { 4 }).rev() {
        par_mapped[2 * b + 1] = par[b];
        par_mapped[2 * b] = par[b];
    }
}

fn map_idx_34_to_20(par_mapped: &mut [i8; MAX_PAR], par: &[i8; MAX_PAR], full: bool) {
    par_mapped[0] = ((2 * i16::from(par[0]) + i16::from(par[1])) / 3) as i8;
    par_mapped[1] = ((i16::from(par[1]) + 2 * i16::from(par[2])) / 3) as i8;
    par_mapped[2] = ((2 * i16::from(par[3]) + i16::from(par[4])) / 3) as i8;
    par_mapped[3] = ((i16::from(par[4]) + 2 * i16::from(par[5])) / 3) as i8;
    par_mapped[4] = ((i16::from(par[6]) + i16::from(par[7])) / 2) as i8;
    par_mapped[5] = ((i16::from(par[8]) + i16::from(par[9])) / 2) as i8;
    par_mapped[6] = par[10];
    par_mapped[7] = par[11];
    par_mapped[8] = ((i16::from(par[12]) + i16::from(par[13])) / 2) as i8;
    par_mapped[9] = ((i16::from(par[14]) + i16::from(par[15])) / 2) as i8;
    par_mapped[10] = par[16];
    if full {
        par_mapped[11] = par[17];
        par_mapped[12] = par[18];
        par_mapped[13] = par[19];
        par_mapped[14] = ((i16::from(par[20]) + i16::from(par[21])) / 2) as i8;
        par_mapped[15] = ((i16::from(par[22]) + i16::from(par[23])) / 2) as i8;
        par_mapped[16] = ((i16::from(par[24]) + i16::from(par[25])) / 2) as i8;
        par_mapped[17] = ((i16::from(par[26]) + i16::from(par[27])) / 2) as i8;
        par_mapped[18] =
            ((i16::from(par[28]) + i16::from(par[29]) + i16::from(par[30]) + i16::from(par[31]))
                / 4) as i8;
        par_mapped[19] = ((i16::from(par[32]) + i16::from(par[33])) / 2) as i8;
    }
}

fn half_sum(a: f32, b: f32) -> f32 {
    (a + b) * 0.5
}

fn map_val_34_to_20(par: &mut [f32; MAX_PAR]) {
    par[0] = (2.0 * par[0] + par[1]) * 0.333_333_33;
    par[1] = (par[1] + 2.0 * par[2]) * 0.333_333_33;
    par[2] = (2.0 * par[3] + par[4]) * 0.333_333_33;
    par[3] = (par[4] + 2.0 * par[5]) * 0.333_333_33;
    par[4] = half_sum(par[6], par[7]);
    par[5] = half_sum(par[8], par[9]);
    par[6] = par[10];
    par[7] = par[11];
    par[8] = half_sum(par[12], par[13]);
    par[9] = half_sum(par[14], par[15]);
    par[10] = par[16];
    par[11] = par[17];
    par[12] = par[18];
    par[13] = par[19];
    par[14] = half_sum(par[20], par[21]);
    par[15] = half_sum(par[22], par[23]);
    par[16] = half_sum(par[24], par[25]);
    par[17] = half_sum(par[26], par[27]);
    par[18] = (par[28] + par[29] + par[30] + par[31]) * 0.25;
    par[19] = half_sum(par[32], par[33]);
}

fn map_idx_10_to_34(par_mapped: &mut [i8; MAX_PAR], par: &[i8; MAX_PAR], full: bool) {
    if full {
        par_mapped[33] = par[9];
        par_mapped[32] = par[9];
        par_mapped[31] = par[9];
        par_mapped[30] = par[9];
        par_mapped[29] = par[9];
        par_mapped[28] = par[9];
        par_mapped[27] = par[8];
        par_mapped[26] = par[8];
        par_mapped[25] = par[8];
        par_mapped[24] = par[8];
        par_mapped[23] = par[7];
        par_mapped[22] = par[7];
        par_mapped[21] = par[7];
        par_mapped[20] = par[7];
        par_mapped[19] = par[6];
        par_mapped[18] = par[6];
        par_mapped[17] = par[5];
        par_mapped[16] = par[5];
    } else {
        par_mapped[16] = 0;
    }
    par_mapped[15] = par[4];
    par_mapped[14] = par[4];
    par_mapped[13] = par[4];
    par_mapped[12] = par[4];
    par_mapped[11] = par[3];
    par_mapped[10] = par[3];
    par_mapped[9] = par[2];
    par_mapped[8] = par[2];
    par_mapped[7] = par[2];
    par_mapped[6] = par[2];
    par_mapped[5] = par[1];
    par_mapped[4] = par[1];
    par_mapped[3] = par[1];
    par_mapped[2] = par[0];
    par_mapped[1] = par[0];
    par_mapped[0] = par[0];
}

fn map_idx_20_to_34(par_mapped: &mut [i8; MAX_PAR], par: &[i8; MAX_PAR], full: bool) {
    if full {
        par_mapped[33] = par[19];
        par_mapped[32] = par[19];
        par_mapped[31] = par[18];
        par_mapped[30] = par[18];
        par_mapped[29] = par[18];
        par_mapped[28] = par[18];
        par_mapped[27] = par[17];
        par_mapped[26] = par[17];
        par_mapped[25] = par[16];
        par_mapped[24] = par[16];
        par_mapped[23] = par[15];
        par_mapped[22] = par[15];
        par_mapped[21] = par[14];
        par_mapped[20] = par[14];
        par_mapped[19] = par[13];
        par_mapped[18] = par[12];
        par_mapped[17] = par[11];
    }
    par_mapped[16] = par[10];
    par_mapped[15] = par[9];
    par_mapped[14] = par[9];
    par_mapped[13] = par[8];
    par_mapped[12] = par[8];
    par_mapped[11] = par[7];
    par_mapped[10] = par[6];
    par_mapped[9] = par[5];
    par_mapped[8] = par[5];
    par_mapped[7] = par[4];
    par_mapped[6] = par[4];
    par_mapped[5] = par[3];
    par_mapped[4] = ((i16::from(par[2]) + i16::from(par[3])) / 2) as i8;
    par_mapped[3] = par[2];
    par_mapped[2] = par[1];
    par_mapped[1] = ((i16::from(par[0]) + i16::from(par[1])) / 2) as i8;
    par_mapped[0] = par[0];
}

fn map_val_20_to_34(par: &mut [f32; MAX_PAR]) {
    par[33] = par[19];
    par[32] = par[19];
    par[31] = par[18];
    par[30] = par[18];
    par[29] = par[18];
    par[28] = par[18];
    par[27] = par[17];
    par[26] = par[17];
    par[25] = par[16];
    par[24] = par[16];
    par[23] = par[15];
    par[22] = par[15];
    par[21] = par[14];
    par[20] = par[14];
    par[19] = par[13];
    par[18] = par[12];
    par[17] = par[11];
    par[16] = par[10];
    par[15] = par[9];
    par[14] = par[9];
    par[13] = par[8];
    par[12] = par[8];
    par[11] = par[7];
    par[10] = par[6];
    par[9] = par[5];
    par[8] = par[5];
    par[7] = par[4];
    par[6] = par[4];
    par[5] = par[3];
    par[4] = half_sum(par[2], par[3]);
    par[3] = par[2];
    par[2] = par[1];
    par[1] = half_sum(par[0], par[1]);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> Vec<f32> {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/data/ps_oracle")
            .join(name);
        std::fs::read(&path)
            .unwrap_or_else(|e| panic!("missing fixture {}: {e}", path.display()))
            .chunks_exact(4)
            .map(|b| f32::from_le_bytes(b.try_into().unwrap()))
            .collect()
    }

    #[test]
    fn generated_tables_match_ffmpeg_reference_values() {
        let t = ps_tables();
        let cases: Vec<(&str, Vec<f32>)> = vec![
            (
                "table_q_fract_allpass.f32",
                t.q_fract_allpass
                    .iter()
                    .flat_map(|a| {
                        a.iter()
                            .flat_map(|b| b.iter().flat_map(|c| c.iter().copied()))
                    })
                    .collect(),
            ),
            (
                "table_phi_fract.f32",
                t.phi_fract
                    .iter()
                    .flat_map(|a| a.iter().flat_map(|b| b.iter().copied()))
                    .collect(),
            ),
            (
                "table_ha.f32",
                t.ha.iter()
                    .flat_map(|r| r.iter().flat_map(|c| c.iter().copied()))
                    .collect(),
            ),
            (
                "table_hb.f32",
                t.hb.iter()
                    .flat_map(|r| r.iter().flat_map(|c| c.iter().copied()))
                    .collect(),
            ),
            ("table_pd_re_smooth.f32", t.pd_re_smooth.to_vec()),
            ("table_pd_im_smooth.f32", t.pd_im_smooth.to_vec()),
            (
                "table_f20_0_8.f32",
                t.f20_0_8
                    .iter()
                    .flat_map(|r| r.iter().flat_map(|c| c.iter().copied()))
                    .collect(),
            ),
            (
                "table_f34_0_12.f32",
                t.f34_0_12
                    .iter()
                    .flat_map(|r| r.iter().flat_map(|c| c.iter().copied()))
                    .collect(),
            ),
            (
                "table_f34_1_8.f32",
                t.f34_1_8
                    .iter()
                    .flat_map(|r| r.iter().flat_map(|c| c.iter().copied()))
                    .collect(),
            ),
            (
                "table_f34_2_4.f32",
                t.f34_2_4
                    .iter()
                    .flat_map(|r| r.iter().flat_map(|c| c.iter().copied()))
                    .collect(),
            ),
        ];
        for (name, values) in &cases {
            let expected = fixture(name);
            assert_eq!(values.len(), expected.len(), "{name}: length mismatch");
            let mut max_diff = 0.0f32;
            let mut mismatched = 0usize;
            for (a, b) in values.iter().zip(&expected) {
                let d = (a - b).abs();
                if d > 0.0 {
                    mismatched += 1;
                    max_diff = max_diff.max(d);
                }
            }
            // HB trigonometry differs by up to ~1 f32 ulp between libms.
            assert!(
                max_diff <= 2.0e-6,
                "{name}: {mismatched} values differ, max |delta| {max_diff:e}"
            );
        }
    }

    struct OracleCase {
        name: &'static str,
        iid_quant: bool,
        nr_iid: usize,
        icc_mode: usize,
        nr_icc: usize,
        nr_ipdopd: usize,
        enable_ipdopd: bool,
        num_env: usize,
        borders: [i32; 6],
        top: usize,
        seed: u32,
        switch34: Option<usize>,
        is34: bool,
    }

    fn oracle_cases() -> [OracleCase; 4] {
        [
            OracleCase {
                name: "a_20band_ipd_last",
                iid_quant: true,
                nr_iid: 20,
                icc_mode: 3,
                nr_icc: 20,
                nr_ipdopd: 11,
                enable_ipdopd: true,
                num_env: 2,
                borders: [-1, 16, 31, 0, 0, 0],
                top: 64,
                seed: 12345,
                switch34: None,
                is34: false,
            },
            OracleCase {
                name: "b_34band_ipd_last",
                iid_quant: true,
                nr_iid: 34,
                icc_mode: 5,
                nr_icc: 34,
                nr_ipdopd: 17,
                enable_ipdopd: true,
                num_env: 1,
                borders: [-1, 31, 0, 0, 0, 0],
                top: 52,
                seed: 98765,
                switch34: None,
                is34: true,
            },
            OracleCase {
                name: "c_10band_baseline_last",
                iid_quant: false,
                nr_iid: 10,
                icc_mode: 1,
                nr_icc: 10,
                nr_ipdopd: 5,
                enable_ipdopd: false,
                num_env: 4,
                borders: [-1, 7, 15, 23, 31, 0],
                top: 64,
                seed: 55555,
                switch34: None,
                is34: false,
            },
            OracleCase {
                name: "d_modeswitch_last",
                iid_quant: true,
                nr_iid: 20,
                icc_mode: 3,
                nr_icc: 20,
                nr_ipdopd: 11,
                enable_ipdopd: true,
                num_env: 2,
                borders: [-1, 16, 31, 0, 0, 0],
                top: 48,
                seed: 424242,
                switch34: Some(3),
                is34: false,
            },
        ]
    }

    fn lcg(state: &mut u32) -> f32 {
        *state = state.wrapping_mul(1664525).wrapping_add(1013904223);
        (((*state >> 8) & 0xFFFFFF) as f32 / 16777215.0) * 2.0 - 1.0
    }

    fn fill_frame(seed: u32) -> Box<[[(f32, f32); 64]; 38]> {
        let mut st = seed;
        let mut x = Box::new([[(0.0f32, 0.0f32); 64]; 38]);
        for slot in x.iter_mut() {
            for band in slot.iter_mut() {
                *band = (lcg(&mut st), lcg(&mut st));
            }
        }
        x
    }

    fn setup_ps(ps: &mut ParametricStereo, c: &OracleCase) {
        ps.start = true;
        ps.enable_iid = true;
        ps.iid_quant = c.iid_quant;
        ps.nr_iid_par = c.nr_iid;
        ps.enable_icc = true;
        ps.icc_mode = c.icc_mode;
        ps.nr_icc_par = c.nr_icc;
        ps.enable_ext = c.enable_ipdopd;
        ps.enable_ipdopd = c.enable_ipdopd;
        ps.nr_ipdopd_par = c.nr_ipdopd;
        ps.num_env = c.num_env;
        ps.border_position = c.borders;
        ps.is34bands = c.is34;
        // The reference harness keeps is34bands_old at its pre-run value;
        // the mode-switch case therefore re-triggers the band-change
        // reset on every post-switch frame exactly like the oracle.
        ps.is34bands_old = false;
        if c.switch34.is_none() {
            ps.is34bands_old = c.is34;
        }
        for e in 0..c.num_env {
            for b in 0..c.nr_iid {
                ps.iid_par[e][b] = ((7 * b + 5 * e) % 15) as i8 - 7;
            }
            for b in 0..c.nr_icc {
                ps.icc_par[e][b] = ((b + e) % 8) as i8;
            }
            for b in 0..c.nr_ipdopd {
                ps.ipd_par[e][b] = ((3 * b + e) % 8) as i8;
                ps.opd_par[e][b] = ((5 * b + 2 * e) % 8) as i8;
            }
        }
    }

    /// Full-pipeline comparison against the reference oracle: each case
    /// runs 8 frames of deterministic synthetic QMF input through
    /// `apply()` and checks the final frame's L/R (the synthesized 32
    /// slots) against the dumped reference output.
    #[test]
    fn ps_apply_matches_ffmpeg_reference_on_all_oracle_cases() {
        let cases = oracle_cases();
        for c in &cases {
            let mut ps = ParametricStereo::new();
            setup_ps(&mut ps, c);
            let mut synth = PsSynthesis::new();
            let mut l = fill_frame(c.seed);
            let mut r = fill_frame(0);
            for i in 0..8u32 {
                l = fill_frame(c.seed + i * 7919);
                if let Some(f) = c.switch34 {
                    ps.is34bands = i >= f as u32;
                }
                synth.apply(&ps, &mut l[..], &mut r[..], c.top);
            }
            let dump = fixture(&format!("{}.f32", c.name));
            let mut worst = (0usize, 0.0f64, 0.0f64);
            for plane in 0..2 {
                for (s, slot) in l.iter().enumerate().take(32) {
                    for (k, b) in slot.iter().enumerate() {
                        let v = if plane == 0 { b.0 } else { b.1 };
                        let e = dump[plane * 38 * 64 + s * 64 + k];
                        if ((v - e) as f64).abs() > worst.1 {
                            worst = (plane * 38 * 64 + s * 64 + k, (v - e) as f64, e as f64);
                        }
                    }
                }
            }
            for plane in 0..2 {
                for (s, slot) in r.iter().enumerate().take(32) {
                    for (k, b) in slot.iter().enumerate() {
                        let v = if plane == 0 { b.0 } else { b.1 };
                        let e = dump[2 * 38 * 64 + plane * 38 * 64 + s * 64 + k];
                        if ((v - e) as f64).abs() > worst.1 {
                            worst = (
                                2 * 38 * 64 + plane * 38 * 64 + s * 64 + k,
                                (v - e) as f64,
                                e as f64,
                            );
                        }
                    }
                }
            }
            assert!(
                worst.1.abs() <= 2e-4,
                "{}: worst |diff| {:.4e} at flat index {} (want {:.5})",
                c.name,
                worst.1.abs(),
                worst.0,
                worst.2
            );
        }
    }

    /// Stage-by-stage trace comparison against the reference (case A dump
    /// from the instrumented ff_ps_apply run: after hybrid analysis, after
    /// decorrelation, and after stereo processing, per frame).
    #[test]
    fn stages_match_reference_frame_by_frame() {
        let bin = std::fs::read(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/data/ps_oracle/stages_case_a.bin"),
        )
        .unwrap();
        let mut records: Vec<(i32, &[u8])> = Vec::new();
        let mut off = 0usize;
        while off < bin.len() {
            let tag = i32::from_le_bytes(bin[off..off + 4].try_into().unwrap());
            let len = 91 * 32 * 2 * 4;
            records.push((tag, &bin[off + 4..off + 4 + len]));
            off += 4 + len;
        }
        assert!(records.len() >= 32);

        let cases = oracle_cases();
        // The stage dump interleaves all four cases; case A records come
        // first (8 frames x 4 stages).
        let c = &cases[0];
        let mut ps = ParametricStereo::new();
        setup_ps(&mut ps, c);
        let mut synth = PsSynthesis::new();
        let mut rec = 0usize;
        let mut l;
        let mut r = fill_frame(0);
        for i in 0..8u32 {
            l = fill_frame(c.seed + i * 7919);
            let top = c.top + NR_BANDS[0] - 64;
            for band in synth.delay[top..NR_BANDS[0]].iter_mut() {
                *band = [[0.0; 2]; PS_SLOTS + PS_MAX_DELAY];
            }
            synth.hybrid_analysis(&l[..], 0);
            let exp = f32s(records[rec].1);
            assert_eq!(records[rec].0, 1);
            compare_stage("analysis", i, &synth.lbuf, &exp);
            rec += 1;
            synth.decorrelation(&ps, 0);
            let exp = f32s(records[rec].1);
            assert_eq!(records[rec].0, 2);
            compare_stage("decorrelation", i, &synth.rbuf, &exp);
            rec += 1;
            synth.stereo_processing(&ps, 0);
            let exp = f32s(records[rec].1);
            assert_eq!(records[rec].0, 3);
            compare_stage("stereo-l", i, &synth.lbuf, &exp);
            rec += 1;
            let exp = f32s(records[rec].1);
            assert_eq!(records[rec].0, 4);
            compare_stage("stereo-r", i, &synth.rbuf, &exp);
            rec += 1;
            synth.hybrid_synthesis(&mut l[..], &synth.lbuf, 0);
            synth.hybrid_synthesis(&mut r[..], &synth.rbuf, 0);
        }

        fn f32s(bytes: &[u8]) -> Vec<f32> {
            bytes
                .chunks_exact(4)
                .map(|b| f32::from_le_bytes(b.try_into().unwrap()))
                .collect()
        }
        fn compare_stage(stage: &str, frame: u32, buf: &[[[f32; 2]; 32]; 91], exp: &[f32]) {
            let mut first_bad = None;
            let mut bad = 0usize;
            for (k, row) in buf.iter().enumerate() {
                for (n, cell) in row.iter().enumerate() {
                    for (c, v) in cell.iter().enumerate() {
                        let e = exp[(k * 32 + n) * 2 + c];
                        if (*v - e).abs() > 1e-5 {
                            bad += 1;
                            if first_bad.is_none() {
                                first_bad = Some((k, n, c, *v, e));
                            }
                        }
                    }
                }
            }
            if bad > 0 {
                let (k, n, c, v, e) = first_bad.unwrap();
                panic!(
                    "stage {stage} frame {frame}: {bad} mismatches, first at band {k} slot {n} ch {c}: got {v} want {e}"
                );
            }
        }
    }
}
