//! Spectral Band Replication (ISO/IEC 14496-3 subpart 4), ported from the
//! reference decoder implementation (FFmpeg aacsbr_template.c /
//! aacsbr.c / sbrdsp.c). The kernels mirror the C reference closely to
//! preserve its float semantics; a few lints are allowed for fidelity.
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

pub mod dsp;
pub mod freq;
pub mod parse;
pub mod qmf;
pub mod tables;

pub use parse::SbrBitReader;

/// A pair of complex QMF subband samples.
pub type QmfPair = (f32, f32);

pub const ENVELOPE_ADJUSTMENT_OFFSET: usize = 2;
pub const SBR_SYNTHESIS_BUF_SIZE: usize = (1280 - 128) * 2;
const NOISE_FLOOR_OFFSET: i32 = 6;
const FLT_EPSILON: f32 = 1.19209290e-7;
const FLT_MIN: f32 = 1.17549435e-38;

/// Per-channel SBR data (reference `SBRData`).
#[derive(Clone)]
pub struct SbrChannel {
    pub bs_freq_res: [u8; 8],
    pub bs_amp_res: u8,
    pub bs_num_env: usize,
    pub bs_num_noise: usize,
    pub bs_frame_class: usize,
    pub t_env: [u8; 8],
    pub t_env_num_env_old: u8,
    pub e_a: [i8; 2],
    pub t_q: [u8; 3],
    pub f_indexnoise: usize,
    pub f_indexsine: usize,
    pub bs_invf_mode: [[u8; 5]; 2],
    pub bs_df_env: [u8; 8],
    pub bs_df_noise: [u8; 4],
    pub bs_add_harmonic_flag: u8,
    pub bs_add_harmonic: [u8; 64],
    pub bw_array: [f32; 5],
    pub env_facs_q: [[u8; 48]; 6],
    pub env_facs: [[f32; 48]; 6],
    pub noise_facs_q: [[u8; 5]; 3],
    pub noise_facs: [[f32; 5]; 3],
    pub s_indexmapped: [[u8; 48]; 8],
    pub e_origmapped: [[f32; 48]; 7],
    pub q_mapped: [[f32; 48]; 7],
    pub s_mapped: [[u8; 48]; 7],
    pub gain: [[f32; 48]; 7],
    pub q_m: [[f32; 48]; 7],
    pub s_m: [[f32; 48]; 7],
    pub g_temp: [[f32; 48]; 42],
    pub q_temp: [[f32; 48]; 42],
    /// W[buf_idx][time slot][band], double-buffered
    pub w: Box<[[[QmfPair; 32]; 32]; 2]>,
    /// Y[buf][time slot][band]
    pub y: Box<[[[QmfPair; 64]; 38]; 2]>,
    pub ypos: usize,
    pub analysis_filterbank_samples: Box<[f32; 1312]>,
    pub synthesis_filterbank_samples: Box<[f32; 2304]>,
    pub synthesis_filterbank_samples_offset: usize,
}

impl Default for SbrChannel {
    fn default() -> Self {
        SbrChannel {
            bs_freq_res: [0; 8],
            bs_amp_res: 0,
            bs_num_env: 1,
            bs_num_noise: 1,
            bs_frame_class: 0,
            t_env: [0; 8],
            t_env_num_env_old: 0,
            e_a: [0; 2],
            t_q: [0; 3],
            f_indexnoise: 0,
            f_indexsine: 0,
            bs_invf_mode: [[0; 5]; 2],
            bs_df_env: [0; 8],
            bs_df_noise: [0; 4],
            bs_add_harmonic_flag: 0,
            bs_add_harmonic: [0; 64],
            bw_array: [0.0; 5],
            env_facs_q: [[0; 48]; 6],
            env_facs: [[0.0; 48]; 6],
            noise_facs_q: [[0; 5]; 3],
            noise_facs: [[0.0; 5]; 3],
            s_indexmapped: [[0; 48]; 8],
            e_origmapped: [[0.0; 48]; 7],
            q_mapped: [[0.0; 48]; 7],
            s_mapped: [[0; 48]; 7],
            gain: [[0.0; 48]; 7],
            q_m: [[0.0; 48]; 7],
            s_m: [[0.0; 48]; 7],
            g_temp: [[0.0; 48]; 42],
            q_temp: [[0.0; 48]; 42],
            w: Box::new([[[(0.0, 0.0); 32]; 32]; 2]),
            y: Box::new([[[(0.0, 0.0); 64]; 38]; 2]),
            ypos: 0,
            analysis_filterbank_samples: Box::new([0.0; 1312]),
            synthesis_filterbank_samples: Box::new([0.0; 2304]),
            synthesis_filterbank_samples_offset: SBR_SYNTHESIS_BUF_SIZE - (1280 - 128),
        }
    }
}

/// Spectral Band Replication state (reference `SpectralBandReplication`).
pub struct Sbr {
    pub sample_rate: i32,
    pub start: bool,
    pub ready_for_dequant: bool,
    pub id_aac: usize,
    pub reset: bool,
    /// start_freq, stop_freq, xover_band, freq_scale, alter_scale, noise_bands
    pub spectrum_params: [u8; 6],
    pub bs_amp_res_header: u8,
    pub bs_limiter_bands: u8,
    pub bs_limiter_gains: u8,
    pub bs_interpol_freq: u8,
    pub bs_smoothing_mode: u8,
    pub bs_coupling: u8,
    pub k: [i32; 3],
    pub kx: [usize; 2],
    pub m: [usize; 2],
    pub kx_and_m_pushed: bool,
    pub n_master: usize,
    pub data: [SbrChannel; 2],
    pub n: [usize; 2],
    pub n_q: usize,
    pub n_lim: usize,
    pub f_master: [u16; 49],
    pub f_tablehigh: [u16; 49],
    pub f_tablelow: [u16; 25],
    pub f_tablenoise: [u16; 6],
    pub f_tablelim: [u16; 29],
    pub num_patches: usize,
    pub patch_num_subbands: [usize; 8],
    pub patch_start_subbands: [usize; 8],
    /// Inverse filter coefficients per low band.
    pub alpha0: [[f32; 2]; 64],
    pub alpha1: [[f32; 2]; 64],
    pub e_curr: [[f32; 48]; 7],
    pub x_low: Box<[[[f32; 2]; 40]; 32]>,
    pub x_high: Box<[[[f32; 2]; 40]; 64]>,
    /// X[channel][time slot][band] as (re, im).
    pub x: [Box<[[QmfPair; 64]; 38]>; 2],
    pub mdct: qmf::Mdct64,
    pub mdct_ana: qmf::Mdct64,
    pub qmf_filter_scratch: [[f32; 64]; 2],
    /// `apply()` scratch: analysis QMF windowing buffer. Struct-resident so
    /// the ~1.25 KB array isn't a per-call stack local (see todo.md
    /// "AAC/SBR decode uses enough stack..." entry).
    pub qmf_analysis_z: Box<[f32; 320]>,
    /// `apply()` scratch for `hf_assemble`'s output (`Y[time slot][band]`,
    /// ~19 KB). `Option` lets `apply()` move it out (zero-alloc `take`) to
    /// sidestep the double-`&mut self` borrow of calling `self.hf_assemble`
    /// with a buffer that lives inside `self`, then move it back after.
    pub y1_scratch: Option<Box<[[QmfPair; 64]; 38]>>,
}

/// `ff_exp2fi`: exact power of two.
fn exp2fi(x: i32) -> f32 {
    f32::from_bits(((x + 127) as u32) << 23)
}

impl Sbr {
    /// `ff_aac_sbr_ctx_init` + `sbr_turnoff`.
    pub fn new(id_aac: usize) -> Self {
        let mut sbr = Sbr {
            sample_rate: 0,
            start: false,
            ready_for_dequant: false,
            id_aac,
            reset: false,
            spectrum_params: [0xff; 6],
            bs_amp_res_header: 0,
            bs_limiter_bands: 0,
            bs_limiter_gains: 0,
            bs_interpol_freq: 0,
            bs_smoothing_mode: 0,
            bs_coupling: 0,
            k: [0; 3],
            kx: [0; 2],
            m: [0; 2],
            kx_and_m_pushed: false,
            n_master: 0,
            data: [SbrChannel::default(), SbrChannel::default()],
            n: [0; 2],
            n_q: 0,
            n_lim: 0,
            f_master: [0; 49],
            f_tablehigh: [0; 49],
            f_tablelow: [0; 25],
            f_tablenoise: [0; 6],
            f_tablelim: [0; 29],
            num_patches: 0,
            patch_num_subbands: [0; 8],
            patch_start_subbands: [0; 8],
            alpha0: [[0.0; 2]; 64],
            alpha1: [[0.0; 2]; 64],
            e_curr: [[0.0; 48]; 7],
            x_low: Box::new([[[0.0; 2]; 40]; 32]),
            x_high: Box::new([[[0.0; 2]; 40]; 64]),
            x: [
                Box::new([[(0.0, 0.0); 64]; 38]),
                Box::new([[(0.0, 0.0); 64]; 38]),
            ],
            mdct: qmf::Mdct64::new(),
            mdct_ana: qmf::Mdct64::new(),
            qmf_filter_scratch: [[0.0; 64]; 2],
            qmf_analysis_z: Box::new([0.0; 320]),
            y1_scratch: Some(Box::new([[(0.0, 0.0); 64]; 38])),
        };
        sbr.turnoff();
        sbr.data[0].synthesis_filterbank_samples_offset = SBR_SYNTHESIS_BUF_SIZE - (1280 - 128);
        sbr.data[1].synthesis_filterbank_samples_offset = SBR_SYNTHESIS_BUF_SIZE - (1280 - 128);
        sbr
    }

    /// `sbr_turnoff`: pure upsampling mode.
    pub fn turnoff(&mut self) {
        self.start = false;
        self.ready_for_dequant = false;
        self.kx[1] = 32;
        self.m[1] = 0;
        self.data[0].e_a[1] = -1;
        self.data[1].e_a[1] = -1;
        self.spectrum_params = [0xff; 6];
    }

    /// `sbr_reset`: re-derive all frequency tables.
    pub fn reset_tables(&mut self) {
        if !freq::make_f_master(self) || !freq::make_f_derived(self) {
            self.turnoff();
        }
    }

    /// `sbr_dequant`: dequantization and stereo decoding of the envelope
    /// and noise scalefactors.
    pub fn dequant(&mut self, id_aac: usize) {
        const EXP2_TAB: [f32; 2] = [1.0, std::f32::consts::SQRT_2];
        if id_aac == 1 && self.bs_coupling != 0 {
            let pan_offset = if self.data[0].bs_amp_res != 0 { 12 } else { 24 };
            for e in 1..=self.data[0].bs_num_env {
                let n = self.n[self.data[0].bs_freq_res[e] as usize];
                for k in 0..n {
                    let (temp1, temp2);
                    if self.data[0].bs_amp_res != 0 {
                        temp1 = exp2fi(self.data[0].env_facs_q[e][k] as i32 + 7);
                        temp2 = exp2fi(pan_offset - self.data[1].env_facs_q[e][k] as i32);
                    } else {
                        let q0 = self.data[0].env_facs_q[e][k];
                        let q1 = self.data[1].env_facs_q[e][k];
                        temp1 = exp2fi((q0 >> 1) as i32 + 7) * EXP2_TAB[(q0 & 1) as usize];
                        temp2 = exp2fi((pan_offset - q1 as i32) >> 1)
                            * EXP2_TAB[((pan_offset - q1 as i32) & 1) as usize];
                    }
                    let temp1 = if temp1 > 1e20 { 1.0 } else { temp1 };
                    let fac = temp1 / (1.0 + temp2);
                    self.data[0].env_facs[e][k] = fac;
                    self.data[1].env_facs[e][k] = fac * temp2;
                }
            }
            for e in 1..=self.data[0].bs_num_noise {
                for k in 0..self.n_q {
                    let temp1 =
                        exp2fi(NOISE_FLOOR_OFFSET - self.data[0].noise_facs_q[e][k] as i32 + 1);
                    let temp2 = exp2fi(12 - self.data[1].noise_facs_q[e][k] as i32);
                    let fac = temp1 / (1.0 + temp2);
                    self.data[0].noise_facs[e][k] = fac;
                    self.data[1].noise_facs[e][k] = fac * temp2;
                }
            }
        } else {
            let nch = usize::from(id_aac == 1) + 1;
            for ch in 0..nch {
                for e in 1..=self.data[ch].bs_num_env {
                    let n = self.n[self.data[ch].bs_freq_res[e] as usize];
                    for k in 0..n {
                        if self.data[ch].bs_amp_res != 0 {
                            self.data[ch].env_facs[e][k] =
                                exp2fi(self.data[ch].env_facs_q[e][k] as i32 + 6);
                        } else {
                            let q = self.data[ch].env_facs_q[e][k];
                            self.data[ch].env_facs[e][k] =
                                exp2fi((q >> 1) as i32 + 6) * EXP2_TAB[(q & 1) as usize];
                        }
                        if self.data[ch].env_facs[e][k] > 1e20 {
                            self.data[ch].env_facs[e][k] = 1.0;
                        }
                    }
                }
                for e in 1..=self.data[ch].bs_num_noise {
                    for k in 0..self.n_q {
                        self.data[ch].noise_facs[e][k] =
                            exp2fi(NOISE_FLOOR_OFFSET - self.data[ch].noise_facs_q[e][k] as i32);
                    }
                }
            }
        }
    }

    /// `sbr_lf_gen`: assemble `x_low` from the current and previous QMF
    /// analysis frames.
    pub fn lf_gen(&mut self, w_cur: &[[(f32, f32); 32]; 32], w_prev: &[[(f32, f32); 32]; 32]) {
        const T_HFGEN: usize = 8;
        const I_F: usize = 32;
        for slot in self.x_low.iter_mut() {
            *slot = [[0.0; 2]; 40];
        }
        for k in 0..self.kx[1] {
            for i in T_HFGEN..I_F + T_HFGEN {
                let src = w_cur[i - T_HFGEN][k];
                self.x_low[k][i] = [src.0, src.1];
            }
        }
        for k in 0..self.kx[0] {
            for i in 0..T_HFGEN {
                let src = w_prev[i + I_F - T_HFGEN][k];
                self.x_low[k][i] = [src.0, src.1];
            }
        }
    }

    /// `sbr_hf_inverse_filter`.
    pub fn hf_inverse_filter(&mut self, k0: usize) {
        for k in 0..k0 {
            let mut phi = [[[0.0f32; 2]; 2]; 3];
            dsp::autocorrelate(&self.x_low[k], &mut phi);

            let dk = phi[2][1][0] * phi[1][0][0]
                - (phi[1][1][0] * phi[1][1][0] + phi[1][1][1] * phi[1][1][1]) / 1.000001;

            if dk == 0.0 {
                self.alpha1[k] = [0.0, 0.0];
            } else {
                let temp_real = phi[0][0][0] * phi[1][1][0]
                    - phi[0][0][1] * phi[1][1][1]
                    - phi[0][1][0] * phi[1][0][0];
                let temp_im = phi[0][0][0] * phi[1][1][1] + phi[0][0][1] * phi[1][1][0]
                    - phi[0][1][1] * phi[1][0][0];
                self.alpha1[k][0] = temp_real / dk;
                self.alpha1[k][1] = temp_im / dk;
            }

            if phi[1][0][0] == 0.0 {
                self.alpha0[k] = [0.0, 0.0];
            } else {
                let temp_real = phi[0][0][0]
                    + self.alpha1[k][0] * phi[1][1][0]
                    + self.alpha1[k][1] * phi[1][1][1];
                let temp_im = phi[0][0][1] + self.alpha1[k][1] * phi[1][1][0]
                    - self.alpha1[k][0] * phi[1][1][1];
                self.alpha0[k][0] = -temp_real / phi[1][0][0];
                self.alpha0[k][1] = -temp_im / phi[1][0][0];
            }

            if self.alpha1[k][0] * self.alpha1[k][0] + self.alpha1[k][1] * self.alpha1[k][1] >= 16.0
                || self.alpha0[k][0] * self.alpha0[k][0] + self.alpha0[k][1] * self.alpha0[k][1]
                    >= 16.0
            {
                self.alpha1[k] = [0.0, 0.0];
                self.alpha0[k] = [0.0, 0.0];
            }
        }
    }

    /// `sbr_chirp`.
    pub fn chirp(&mut self, ch: usize) {
        const BW_TAB: [f32; 4] = [0.0, 0.75, 0.9, 0.98];
        for i in 0..self.n_q {
            let (invf0, invf1, prev_bw) = {
                let d = &self.data[ch];
                (d.bs_invf_mode[0][i], d.bs_invf_mode[1][i], d.bw_array[i])
            };
            let new_bw = if invf0 + invf1 == 1 {
                0.6
            } else {
                BW_TAB[invf0 as usize]
            };
            let new_bw = if new_bw < prev_bw {
                0.75 * new_bw + 0.25 * prev_bw
            } else {
                0.90625 * new_bw + 0.09375 * prev_bw
            };
            self.data[ch].bw_array[i] = if new_bw < 0.015625 { 0.0 } else { new_bw };
        }
    }

    /// `sbr_hf_gen`: inverse-filtered HF generation for all patch bands.
    pub fn hf_gen(
        &mut self,
        alpha0: &[[f32; 2]],
        alpha1: &[[f32; 2]],
        bw_array: &[f32],
        t_env: &[u8],
        bs_num_env: usize,
    ) -> bool {
        let mut g: i32 = 0;
        let mut k = self.kx[1];
        for j in 0..self.num_patches {
            for x in 0..self.patch_num_subbands[j] {
                let p = self.patch_start_subbands[j] + x;
                while (g as usize) <= self.n_q && k >= self.f_tablenoise[g as usize] as usize {
                    g += 1;
                }
                g -= 1;
                if g < 0 {
                    return false;
                }
                let start = 2 * t_env[0] as usize;
                let end = 2 * t_env[bs_num_env] as usize;
                let mut x_high_slice = self.x_high[k];
                let x_low_slice: [[f32; 2]; 40] = self.x_low[p];
                dsp::hf_gen(
                    &mut x_high_slice,
                    &x_low_slice,
                    &alpha0[p],
                    &alpha1[p],
                    bw_array[g as usize],
                    start + ENVELOPE_ADJUSTMENT_OFFSET,
                    end + ENVELOPE_ADJUSTMENT_OFFSET,
                );
                self.x_high[k] = x_high_slice;
                k += 1;
            }
        }
        if k < self.m[1] + self.kx[1] {
            for slot in self.x_high[k..self.m[1] + self.kx[1]].iter_mut() {
                *slot = [[0.0; 2]; 40];
            }
        }
        true
    }

    /// `sbr_x_gen`.
    pub fn x_gen(&mut self, ch: usize) {
        const I_F: usize = 32;
        let i_temp = (2 * self.data[ch].t_env_num_env_old as usize).saturating_sub(I_F);
        let (y0, y1) = {
            let d = &self.data[ch];
            (&d.y[1 - d.ypos], &d.y[d.ypos])
        };
        let (kx0, kx1, m0, m1) = (self.kx[0], self.kx[1], self.m[0], self.m[1]);
        let x_low = &self.x_low;
        let x = &mut self.x[ch];
        for slot in x.iter_mut() {
            *slot = [(0.0, 0.0); 64];
        }
        for k in 0..kx0 {
            for i in 0..i_temp {
                let src = x_low[k][i + ENVELOPE_ADJUSTMENT_OFFSET];
                x[i][k] = (src[0], src[1]);
            }
        }
        for k in kx0..kx0 + m0 {
            for i in 0..i_temp {
                let src = y0[i + I_F][k];
                x[i][k] = (src.0, src.1);
            }
        }
        for k in 0..kx1 {
            for i in i_temp..38 {
                let src = x_low[k][i + ENVELOPE_ADJUSTMENT_OFFSET];
                x[i][k] = (src[0], src[1]);
            }
        }
        for k in kx1..kx1 + m1 {
            for i in i_temp..I_F {
                let src = y1[i][k];
                x[i][k] = (src.0, src.1);
            }
        }
    }

    /// `sbr_mapping`: envelope/noise/sinusoid mapping (HF adjustment prep).
    pub fn mapping(&mut self, ch: usize, e_a: &[i8; 2]) -> bool {
        let kx = self.kx[1] as u16;
        let data = &mut self.data[ch];
        for slot in data.s_indexmapped[1..8].iter_mut() {
            *slot = [0; 48];
        }
        for e in 0..data.bs_num_env {
            let ilim = self.n[data.bs_freq_res[e + 1] as usize];
            let table: Vec<u16> = if data.bs_freq_res[e + 1] != 0 {
                self.f_tablehigh[..=ilim].to_vec()
            } else {
                self.f_tablelow[..=ilim].to_vec()
            };
            if kx != table[0] {
                self.turnoff();
                return false;
            }
            for i in 0..ilim {
                for m in table[i]..table[i + 1] {
                    data.e_origmapped[e][(m - kx) as usize] = data.env_facs[e + 1][i];
                }
            }
            let k = usize::from(data.bs_num_noise > 1 && data.t_env[e] >= data.t_q[1]);
            for i in 0..self.n_q {
                for m in self.f_tablenoise[i]..self.f_tablenoise[i + 1] {
                    data.q_mapped[e][(m - kx) as usize] = data.noise_facs[k + 1][i];
                }
            }
            for i in 0..self.n[1] {
                if data.bs_add_harmonic_flag != 0 {
                    let m_midpoint =
                        ((self.f_tablehigh[i] + self.f_tablehigh[i + 1]) >> 1) as usize;
                    data.s_indexmapped[e + 1][m_midpoint - self.kx[1]] = data.bs_add_harmonic[i]
                        * u8::from(
                            e as i8 >= e_a[1]
                                || data.s_indexmapped[0][m_midpoint - self.kx[1]] == 1,
                        );
                }
            }
            for i in 0..ilim {
                let mut additional_sinusoid_present = false;
                for m in table[i]..table[i + 1] {
                    if data.s_indexmapped[e + 1][(m - kx) as usize] != 0 {
                        additional_sinusoid_present = true;
                        break;
                    }
                }
                for m in table[i]..table[i + 1] {
                    data.s_mapped[e][(m - kx) as usize] = u8::from(additional_sinusoid_present);
                }
            }
        }
        let last = data.s_indexmapped[data.bs_num_env];
        data.s_indexmapped[0] = last;
        true
    }

    /// `sbr_env_estimate`.
    pub fn env_estimate(&mut self, ch: usize) {
        let kx1 = self.kx[1];
        let bs_interpol = self.bs_interpol_freq != 0;
        let (num_env, t_env, freq_res) = {
            let d = &self.data[ch];
            (d.bs_num_env, d.t_env, d.bs_freq_res)
        };
        if bs_interpol {
            for e in 0..num_env {
                let recip_env_size = 0.5 / (t_env[e + 1] - t_env[e]) as f32;
                let ilb = t_env[e] as usize * 2 + ENVELOPE_ADJUSTMENT_OFFSET;
                let iub = t_env[e + 1] as usize * 2 + ENVELOPE_ADJUSTMENT_OFFSET;
                for m in 0..self.m[1] {
                    let sum = dsp::sum_square(&self.x_high[m + kx1][ilb..iub], iub - ilb);
                    self.e_curr[e][m] = sum * recip_env_size;
                }
            }
        } else {
            for e in 0..num_env {
                let env_size = 2 * (t_env[e + 1] - t_env[e]) as usize;
                let ilb = t_env[e] as usize * 2 + ENVELOPE_ADJUSTMENT_OFFSET;
                let iub = t_env[e + 1] as usize * 2 + ENVELOPE_ADJUSTMENT_OFFSET;
                let table: Vec<u16> = if freq_res[e + 1] != 0 {
                    self.f_tablehigh[..=self.n[freq_res[e + 1] as usize]].to_vec()
                } else {
                    self.f_tablelow[..=self.n[freq_res[e + 1] as usize]].to_vec()
                };
                for p in 0..self.n[freq_res[e + 1] as usize] {
                    let mut sum = 0.0f32;
                    let den = env_size * (table[p + 1] - table[p]) as usize;
                    for k in table[p]..table[p + 1] {
                        sum += dsp::sum_square(&self.x_high[k as usize][ilb..iub], iub - ilb);
                    }
                    let sum = sum / den as f32;
                    for k in table[p]..table[p + 1] {
                        self.e_curr[e][k as usize - kx1] = sum;
                    }
                }
            }
        }
    }

    /// `sbr_gain_calc`.
    pub fn gain_calc(&mut self, ch: usize, e_a: &[i8; 2]) {
        const LIMGAIN: [f32; 4] = [0.70795, 1.0, 1.41254, 10000000000.0];
        let data = &mut self.data[ch];
        for e in 0..data.bs_num_env {
            let delta = i32::from(!((e as i8 == e_a[1]) || (e as i8 == e_a[0])));
            for k in 0..self.n_lim {
                let lo = self.f_tablelim[k] as usize - self.kx[1];
                let hi = self.f_tablelim[k + 1] as usize - self.kx[1];
                for m in lo..hi {
                    let q = data.q_mapped[e][m];
                    let temp = data.e_origmapped[e][m] / (1.0 + q);
                    data.q_m[e][m] = (temp * q).sqrt();
                    data.s_m[e][m] = (temp * data.s_indexmapped[e + 1][m] as f32).sqrt();
                    data.gain[e][m] = if data.s_mapped[e][m] == 0 {
                        (data.e_origmapped[e][m]
                            / ((1.0 + self.e_curr[e][m]) * (1.0 + q * delta as f32)))
                            .sqrt()
                    } else {
                        (data.e_origmapped[e][m] * q / ((1.0 + self.e_curr[e][m]) * (1.0 + q)))
                            .sqrt()
                    };
                    data.gain[e][m] += FLT_MIN;
                }
                let mut sum = [0.0f32; 2];
                for m in lo..hi {
                    sum[0] += data.e_origmapped[e][m];
                    sum[1] += self.e_curr[e][m];
                }
                let mut gain_max = LIMGAIN[self.bs_limiter_gains as usize]
                    * ((FLT_EPSILON + sum[0]) / (FLT_EPSILON + sum[1])).sqrt();
                gain_max = gain_max.min(100000.0);
                for m in lo..hi {
                    let q_m_max = data.q_m[e][m] * gain_max / data.gain[e][m];
                    data.q_m[e][m] = data.q_m[e][m].min(q_m_max);
                    data.gain[e][m] = data.gain[e][m].min(gain_max);
                }
                sum[0] = 0.0;
                sum[1] = 0.0;
                for m in lo..hi {
                    sum[0] += data.e_origmapped[e][m];
                    sum[1] += self.e_curr[e][m] * data.gain[e][m] * data.gain[e][m]
                        + data.s_m[e][m] * data.s_m[e][m]
                        + f32::from(delta != 0 && data.s_m[e][m] == 0.0)
                            * data.q_m[e][m]
                            * data.q_m[e][m];
                }
                let gain_boost = ((FLT_EPSILON + sum[0]) / (FLT_EPSILON + sum[1])).sqrt();
                let gain_boost = gain_boost.min(1.584893192);
                for m in lo..hi {
                    data.gain[e][m] *= gain_boost;
                    data.q_m[e][m] *= gain_boost;
                    data.s_m[e][m] *= gain_boost;
                }
            }
        }
    }
}

impl Sbr {
    /// `sbr_hf_assemble`.
    pub fn hf_assemble(&mut self, ch: usize, y1: &mut [[(f32, f32); 64]], e_a: &[i8; 2]) {
        let h_sl = 4 * usize::from(self.bs_smoothing_mode == 0);
        let kx = self.kx[1];
        let m_max = self.m[1];
        const H_SMOOTH: [f32; 5] = [
            0.33333333333333,
            0.30150283239582,
            0.21816949906249,
            0.11516383427084,
            0.03183050093751,
        ];
        let mut indexnoise = self.data[ch].f_indexnoise;
        let mut indexsine = self.data[ch].f_indexsine;

        if self.reset {
            for i in 0..h_sl {
                let base = i + 2 * self.data[ch].t_env[0] as usize;
                let g0 = self.data[ch].gain[0];
                let q0 = self.data[ch].q_m[0];
                self.data[ch].g_temp[base][..m_max].copy_from_slice(&g0[..m_max]);
                self.data[ch].q_temp[base][..m_max].copy_from_slice(&q0[..m_max]);
            }
        } else if h_sl != 0 {
            for i in 0..4 {
                let dst = i + 2 * self.data[ch].t_env[0] as usize;
                let src = i + 2 * self.data[ch].t_env_num_env_old as usize;
                self.data[ch].g_temp[dst] = self.data[ch].g_temp[src];
                self.data[ch].q_temp[dst] = self.data[ch].q_temp[src];
            }
        }

        for e in 0..self.data[ch].bs_num_env {
            for i in 2 * self.data[ch].t_env[e] as usize..2 * self.data[ch].t_env[e + 1] as usize {
                let gain = self.data[ch].gain[e];
                let q_m = self.data[ch].q_m[e];
                self.data[ch].g_temp[h_sl + i][..m_max].copy_from_slice(&gain[..m_max]);
                self.data[ch].q_temp[h_sl + i][..m_max].copy_from_slice(&q_m[..m_max]);
            }
        }

        for e in 0..self.data[ch].bs_num_env {
            for i in 2 * self.data[ch].t_env[e] as usize..2 * self.data[ch].t_env[e + 1] as usize {
                let (g_filt, q_filt) = if h_sl != 0 && e as i8 != e_a[0] && e as i8 != e_a[1] {
                    let mut g_filt_tab = [0.0f32; 48];
                    let mut q_filt_tab = [0.0f32; 48];
                    let idx1 = i + h_sl;
                    for m in 0..m_max {
                        let mut g = 0.0f32;
                        let mut q = 0.0f32;
                        for j in 0..=h_sl {
                            g += self.data[ch].g_temp[idx1 - j][m] * H_SMOOTH[j];
                            q += self.data[ch].q_temp[idx1 - j][m] * H_SMOOTH[j];
                        }
                        g_filt_tab[m] = g;
                        q_filt_tab[m] = q;
                    }
                    (g_filt_tab, q_filt_tab)
                } else {
                    (self.data[ch].g_temp[i + h_sl], self.data[ch].q_temp[i])
                };

                for m in 0..m_max {
                    let src = self.x_high[kx + m][i + ENVELOPE_ADJUSTMENT_OFFSET];
                    y1[i][kx + m].0 = src[0] * g_filt[m];
                    y1[i][kx + m].1 = src[1] * g_filt[m];
                }

                if e as i8 != e_a[0] && e as i8 != e_a[1] {
                    let (phi0, mut phi1) = match indexsine {
                        0 => (1.0f32, 0.0f32),
                        1 => (0.0, (1i32 - 2 * (kx as i32 & 1)) as f32),
                        2 => (-1.0, 0.0),
                        _ => (0.0, -((1i32 - 2 * (kx as i32 & 1)) as f32)),
                    };
                    let mut noise = indexnoise;
                    for m in 0..m_max {
                        let mut y0 = y1[i][kx + m].0;
                        let mut yy = y1[i][kx + m].1;
                        noise = (noise + 1) & 0x1ff;
                        let s_m = self.data[ch].s_m[e][m];
                        if s_m != 0.0 {
                            y0 += s_m * phi0;
                            yy += s_m * phi1;
                        } else {
                            let q = q_filt[m];
                            y0 += q * tables::SBR_NOISE_TABLE[noise][0];
                            yy += q * tables::SBR_NOISE_TABLE[noise][1];
                        }
                        y1[i][kx + m].0 = y0;
                        y1[i][kx + m].1 = yy;
                        phi1 = -phi1;
                    }
                } else {
                    // Sinusoid addition with the alternating A/B signs; the
                    // flat-index form out[2m] = Y[i][kx+m][idx].
                    let idx = indexsine & 1;
                    let a: i32 = 1 - ((indexsine + (kx & 1)) & 2) as i32;
                    let b: i32 = if idx == 1 { -a } else { a };
                    let s_m = self.data[ch].s_m[e];
                    let mut m = 0usize;
                    while m + 1 < m_max {
                        if idx == 0 {
                            y1[i][kx + m].0 += s_m[m] * a as f32;
                            y1[i][kx + m + 1].0 += s_m[m + 1] * b as f32;
                        } else {
                            y1[i][kx + m].1 += s_m[m] * a as f32;
                            y1[i][kx + m + 1].1 += s_m[m + 1] * b as f32;
                        }
                        m += 2;
                    }
                    if m_max & 1 != 0 {
                        if idx == 0 {
                            y1[i][kx + m].0 += s_m[m] * a as f32;
                        } else {
                            y1[i][kx + m].1 += s_m[m] * a as f32;
                        }
                    }
                }
                indexnoise = (indexnoise + m_max) & 0x1ff;
                indexsine = (indexsine + 1) & 3;
            }
        }
        self.data[ch].f_indexnoise = indexnoise;
        self.data[ch].f_indexsine = indexsine;
    }

    /// `ff_sbr_apply` (without PS): transform the 1024-sample core output
    /// in `core_out[ch]` into 2048 SBR-enhanced samples written back.
    pub fn apply(&mut self, id_aac: usize, core_out: &mut [Vec<f32>; 2], nch: usize) {
        if id_aac != self.id_aac {
            self.turnoff();
        }
        if self.start && !self.ready_for_dequant {
            self.turnoff();
        }
        if !self.kx_and_m_pushed {
            self.kx[0] = self.kx[1];
            self.m[0] = self.m[1];
        } else {
            self.kx_and_m_pushed = false;
        }
        if self.start {
            self.dequant(id_aac);
            self.ready_for_dequant = false;
        }
        for ch in 0..nch {
            // `input`, `w` and `z` are no longer per-call stack locals: `w`
            // is written straight into its eventual home
            // (`self.data[ch].w[ypos]`, already struct-resident) and `z` is
            // a struct-resident scratch buffer (`qmf_analysis_z`) — both
            // qmf_analysis and qmf_synthesis are free functions, so there is
            // no self-borrow conflict in passing disjoint self fields
            // straight through. `input` needs no local copy at all: it was
            // only ever read from `core_out[ch]`, which qmf_analysis can
            // borrow directly.
            let ypos = self.data[ch].ypos;
            {
                let mdct_ana = &self.mdct_ana;
                let z = &mut *self.qmf_analysis_z;
                let d = &mut self.data[ch];
                qmf::qmf_analysis(
                    mdct_ana,
                    &core_out[ch][..1024],
                    &mut d.analysis_filterbank_samples,
                    z,
                    &mut d.w[ypos],
                    ypos,
                );
            }

            {
                let (w_cur, w_prev) = {
                    let d = &self.data[ch];
                    (d.w[ypos], d.w[1 - ypos])
                };
                self.lf_gen(&w_cur, &w_prev);
            }
            self.data[ch].ypos ^= 1;

            if self.start {
                self.hf_inverse_filter(self.k[0] as usize);
                self.chirp(ch);
                let (alpha0, alpha1) = (self.alpha0, self.alpha1);
                let bw_array = self.data[ch].bw_array;
                let (t_env, bs_num_env) = (self.data[ch].t_env, self.data[ch].bs_num_env);
                if self.hf_gen(&alpha0, &alpha1, &bw_array, &t_env, bs_num_env) {
                    let e_a_copy = self.data[ch].e_a;
                    if self.mapping(ch, &e_a_copy) {
                        self.env_estimate(ch);
                        let e_a2 = self.data[ch].e_a;
                        self.gain_calc(ch, &e_a2);
                        // `y1` (~19 KB) lives in `y1_scratch`, not on the
                        // stack. It's moved out (a cheap pointer move, not a
                        // copy of the buffer) so it's a plain local the
                        // `self.hf_assemble(&mut self, ...)` call can borrow
                        // without a double-borrow-of-self conflict, then
                        // moved back so the allocation is never repeated.
                        let mut y1 = self
                            .y1_scratch
                            .take()
                            .expect("y1_scratch missing (apply() re-entrant?)");
                        let e_a3 = self.data[ch].e_a;
                        self.hf_assemble(ch, &mut y1[..], &e_a3);
                        self.data[ch].y[self.data[ch].ypos] = *y1;
                        self.y1_scratch = Some(y1);
                    }
                }
            }

            self.x_gen(ch);
        }

        for ch in 0..nch {
            // `out` likewise no longer needs a stack local: qmf_synthesis
            // can write its 2048 samples straight into `core_out[ch]`,
            // which is already sized for them.
            let mdct = &self.mdct;
            let d = &mut self.data[ch];
            qmf::qmf_synthesis(
                mdct,
                &mut core_out[ch][..2048],
                &self.x[ch],
                &mut d.synthesis_filterbank_samples,
                &mut d.synthesis_filterbank_samples_offset,
            );
        }
    }
}
