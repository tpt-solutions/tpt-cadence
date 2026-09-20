//! SBR bitstream parsing (reference: read_sbr_header / read_sbr_grid /
//! read_sbr_dtdf / read_sbr_invf / read_sbr_envelope / read_sbr_noise /
//! read_sbr_data / ff_decode_sbr_extension).
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

use super::tables;
use super::{Sbr, SbrChannel};
use crate::bitreader::BitReader;
use crate::huffman::HuffmanTable;

use std::sync::OnceLock;

/// VLC table indices (reference enum).
const T_HUFFMAN_ENV_1_5DB: usize = 0;
const F_HUFFMAN_ENV_1_5DB: usize = 1;
const T_HUFFMAN_ENV_BAL_1_5DB: usize = 2;
const F_HUFFMAN_ENV_BAL_1_5DB: usize = 3;
const T_HUFFMAN_ENV_3_0DB: usize = 4;
const F_HUFFMAN_ENV_3_0DB: usize = 5;
const T_HUFFMAN_ENV_BAL_3_0DB: usize = 6;
const F_HUFFMAN_ENV_BAL_3_0DB: usize = 7;
const T_HUFFMAN_NOISE_3_0DB: usize = 8;
const T_HUFFMAN_NOISE_BAL_3_0DB: usize = 9;

/// `vlc_sbr_lav`: largest absolute value per table.
const VLC_SBR_LAV: [i32; 10] = [60, 60, 24, 24, 31, 31, 12, 12, 31, 12];

pub struct SbrVlc {
    pub tables: [HuffmanTable; 10],
    pub lav: [i32; 10],
}

impl SbrVlc {
    fn new() -> Self {
        SbrVlc {
            tables: [
                HuffmanTable::new(
                    &tables::T_HUFFMAN_ENV_1_5DB_BITS,
                    &tables::T_HUFFMAN_ENV_1_5DB_CODES,
                )
                .unwrap(),
                HuffmanTable::new(
                    &tables::F_HUFFMAN_ENV_1_5DB_BITS,
                    &tables::F_HUFFMAN_ENV_1_5DB_CODES,
                )
                .unwrap(),
                HuffmanTable::new(
                    &tables::T_HUFFMAN_ENV_BAL_1_5DB_BITS,
                    &tables::T_HUFFMAN_ENV_BAL_1_5DB_CODES,
                )
                .unwrap(),
                HuffmanTable::new(
                    &tables::F_HUFFMAN_ENV_BAL_1_5DB_BITS,
                    &tables::F_HUFFMAN_ENV_BAL_1_5DB_CODES,
                )
                .unwrap(),
                HuffmanTable::new(
                    &tables::T_HUFFMAN_ENV_3_0DB_BITS,
                    &tables::T_HUFFMAN_ENV_3_0DB_CODES,
                )
                .unwrap(),
                HuffmanTable::new(
                    &tables::F_HUFFMAN_ENV_3_0DB_BITS,
                    &tables::F_HUFFMAN_ENV_3_0DB_CODES,
                )
                .unwrap(),
                HuffmanTable::new(
                    &tables::T_HUFFMAN_ENV_BAL_3_0DB_BITS,
                    &tables::T_HUFFMAN_ENV_BAL_3_0DB_CODES,
                )
                .unwrap(),
                HuffmanTable::new(
                    &tables::F_HUFFMAN_ENV_BAL_3_0DB_BITS,
                    &tables::F_HUFFMAN_ENV_BAL_3_0DB_CODES,
                )
                .unwrap(),
                HuffmanTable::new(
                    &tables::T_HUFFMAN_NOISE_3_0DB_BITS,
                    &tables::T_HUFFMAN_NOISE_3_0DB_CODES,
                )
                .unwrap(),
                HuffmanTable::new(
                    &tables::T_HUFFMAN_NOISE_BAL_3_0DB_BITS,
                    &tables::T_HUFFMAN_NOISE_BAL_3_0DB_CODES,
                )
                .unwrap(),
            ],
            lav: VLC_SBR_LAV,
        }
    }
}

/// Shared SBR VLC tables.
pub fn sbr_vlc() -> &'static SbrVlc {
    static SBR_VLC: OnceLock<SbrVlc> = OnceLock::new();
    SBR_VLC.get_or_init(SbrVlc::new)
}

/// A bit reader facade over the crate's BitReader with SBR conveniences.
pub struct SbrBitReader<'a> {
    pub br: BitReader<'a>,
}

impl<'a> SbrBitReader<'a> {
    pub fn new(br: BitReader<'a>) -> Self {
        SbrBitReader { br }
    }
    pub fn bits(&mut self, n: u32) -> u32 {
        self.br.read_bits(n)
    }
    pub fn bit(&mut self) -> bool {
        self.br.read_bit()
    }
}

const CEIL_LOG2: [i32; 6] = [0, 1, 2, 2, 3, 3];

fn read_sbr_header(sbr: &mut Sbr, br: &mut SbrBitReader) -> u32 {
    let cnt = br.br.pos();
    let old_bs_limiter_bands = sbr.bs_limiter_bands;
    let old_spectrum_params = sbr.spectrum_params;

    sbr.start = true;
    sbr.ready_for_dequant = false;

    sbr.bs_amp_res_header = br.bit() as u8;
    sbr.spectrum_params[0] = br.bits(4) as u8;
    sbr.spectrum_params[1] = br.bits(4) as u8;
    sbr.spectrum_params[2] = br.bits(3) as u8;
    br.bits(2); // bs_reserved

    let extra1 = br.bit();
    let extra2 = br.bit();

    if extra1 {
        sbr.spectrum_params[3] = br.bits(2) as u8;
        sbr.spectrum_params[4] = br.bit() as u8;
        sbr.spectrum_params[5] = br.bits(2) as u8;
    } else {
        sbr.spectrum_params[3] = 2;
        sbr.spectrum_params[4] = 1;
        sbr.spectrum_params[5] = 2;
    }

    if old_spectrum_params != sbr.spectrum_params {
        sbr.reset = true;
    }

    if extra2 {
        sbr.bs_limiter_bands = br.bits(2) as u8;
        sbr.bs_limiter_gains = br.bits(2) as u8;
        sbr.bs_interpol_freq = br.bit() as u8;
        sbr.bs_smoothing_mode = br.bit() as u8;
    } else {
        sbr.bs_limiter_bands = 2;
        sbr.bs_limiter_gains = 2;
        sbr.bs_interpol_freq = 1;
        sbr.bs_smoothing_mode = 1;
    }

    if sbr.bs_limiter_bands != old_bs_limiter_bands && !sbr.reset {
        freq::make_f_tablelim(sbr);
    }

    (br.br.pos() - cnt) as u32
}

fn read_sbr_grid(sbr: &mut Sbr, br: &mut SbrBitReader, ch: usize) -> Result<(), ()> {
    let data = &mut sbr.data[ch];
    let mut bs_pointer = 0usize;
    let mut abs_bord_trail = 16usize;
    let bs_num_env_old = data.bs_num_env;
    data.bs_freq_res[0] = data.bs_freq_res[data.bs_num_env];
    data.bs_amp_res = sbr.bs_amp_res_header;
    data.t_env_num_env_old = data.t_env[bs_num_env_old];

    let bs_frame_class = br.bits(2) as usize;
    match bs_frame_class {
        0 => {
            // FIXFIX
            let bs_num_env = 1 << br.bits(2);
            if bs_num_env > 4 {
                return Err(());
            }
            data.bs_num_env = bs_num_env;
            let num_rel_lead = data.bs_num_env - 1;
            if data.bs_num_env == 1 {
                data.bs_amp_res = 0;
            }
            data.t_env[0] = 0;
            data.t_env[data.bs_num_env] = abs_bord_trail as u8;
            abs_bord_trail = (abs_bord_trail + (data.bs_num_env >> 1)) / data.bs_num_env;
            for i in 0..num_rel_lead {
                data.t_env[i + 1] = data.t_env[i] + abs_bord_trail as u8;
            }
            data.bs_freq_res[1] = br.bit() as u8;
            for i in 1..data.bs_num_env {
                data.bs_freq_res[i + 1] = data.bs_freq_res[1];
            }
        }
        1 => {
            // FIXVAR
            abs_bord_trail += br.bits(2) as usize;
            let num_rel_trail = br.bits(2) as usize;
            data.bs_num_env = num_rel_trail + 1;
            data.t_env[0] = 0;
            data.t_env[data.bs_num_env] = abs_bord_trail as u8;
            for i in 0..num_rel_trail {
                data.t_env[data.bs_num_env - 1 - i] =
                    data.t_env[data.bs_num_env - i] - 2 * br.bits(2) as u8 - 2;
            }
            bs_pointer = br.bits(CEIL_LOG2[data.bs_num_env] as u32) as usize;
            for i in 0..data.bs_num_env {
                data.bs_freq_res[data.bs_num_env - i] = br.bit() as u8;
            }
        }
        2 => {
            // VARFIX
            data.t_env[0] = br.bits(2) as u8;
            let num_rel_lead = br.bits(2) as usize;
            data.bs_num_env = num_rel_lead + 1;
            data.t_env[data.bs_num_env] = abs_bord_trail as u8;
            for i in 0..num_rel_lead {
                data.t_env[i + 1] = data.t_env[i] + 2 * br.bits(2) as u8 + 2;
            }
            bs_pointer = br.bits(CEIL_LOG2[data.bs_num_env] as u32) as usize;
            for i in 1..=data.bs_num_env {
                data.bs_freq_res[i] = br.bit() as u8;
            }
        }
        _ => {
            // VARVAR
            data.t_env[0] = br.bits(2) as u8;
            abs_bord_trail += br.bits(2) as usize;
            let num_rel_lead = br.bits(2) as usize;
            let num_rel_trail = br.bits(2) as usize;
            let bs_num_env = num_rel_lead + num_rel_trail + 1;
            if bs_num_env > 5 {
                return Err(());
            }
            data.bs_num_env = bs_num_env;
            data.t_env[data.bs_num_env] = abs_bord_trail as u8;
            for i in 0..num_rel_lead {
                data.t_env[i + 1] = data.t_env[i] + 2 * br.bits(2) as u8 + 2;
            }
            for i in 0..num_rel_trail {
                data.t_env[data.bs_num_env - 1 - i] =
                    data.t_env[data.bs_num_env - i] - 2 * br.bits(2) as u8 - 2;
            }
            bs_pointer = br.bits(CEIL_LOG2[data.bs_num_env] as u32) as usize;
            for i in 1..=data.bs_num_env {
                data.bs_freq_res[i] = br.bit() as u8;
            }
        }
    }
    data.bs_frame_class = bs_frame_class;

    if bs_pointer > data.bs_num_env + 1 {
        return Err(());
    }
    for i in 1..=data.bs_num_env {
        if data.t_env[i - 1] >= data.t_env[i] {
            return Err(());
        }
    }

    data.bs_num_noise = usize::from(data.bs_num_env > 1) + 1;
    data.t_q[0] = data.t_env[0];
    data.t_q[data.bs_num_noise] = data.t_env[data.bs_num_env];
    if data.bs_num_noise > 1 {
        let idx = if data.bs_frame_class == 0 {
            data.bs_num_env >> 1
        } else if data.bs_frame_class & 1 != 0 {
            // FIXVAR or VARVAR
            data.bs_num_env - bs_pointer.max(1)
        } else if bs_pointer == 0 {
            1
        } else if bs_pointer == 1 {
            data.bs_num_env - 1
        } else {
            bs_pointer - 1
        };
        data.t_q[1] = data.t_env[idx];
    }

    data.e_a[0] = -i8::from(data.e_a[1] != bs_num_env_old as i8); // l_APrev
    data.e_a[1] = -1;
    if (data.bs_frame_class & 1) != 0 && bs_pointer != 0 {
        data.e_a[1] = (data.bs_num_env + 1 - bs_pointer) as i8;
    } else if data.bs_frame_class == 2 && bs_pointer > 1 {
        data.e_a[1] = bs_pointer as i8 - 1;
    }

    Ok(())
}

fn read_sbr_dtdf(sbr: &mut Sbr, br: &mut SbrBitReader, ch: usize) {
    let (num_env, num_noise) = (sbr.data[ch].bs_num_env, sbr.data[ch].bs_num_noise);
    for i in 1..=num_env {
        sbr.data[ch].bs_df_env[i] = br.bit() as u8;
    }
    for i in 1..=num_noise {
        sbr.data[ch].bs_df_noise[i] = br.bit() as u8;
    }
}

fn read_sbr_invf(sbr: &mut Sbr, br: &mut SbrBitReader, ch: usize) {
    let n_q = sbr.n_q;
    let data = &mut sbr.data[ch];
    data.bs_invf_mode[1] = data.bs_invf_mode[0];
    for i in 0..n_q {
        data.bs_invf_mode[0][i] = br.bits(2) as u8;
    }
}

fn read_sbr_envelope(
    sbr: &mut Sbr,
    br: &mut SbrBitReader,
    vlc: &SbrVlc,
    ch: usize,
) -> Result<(), ()> {
    let delta = i32::from(ch == 1 && sbr.bs_coupling == 1) + 1;
    let odd = sbr.n[1] & 1;

    let (bits, t_huff, f_huff) = if sbr.bs_coupling != 0 && ch != 0 {
        if sbr.data[ch].bs_amp_res != 0 {
            (5, T_HUFFMAN_ENV_BAL_3_0DB, F_HUFFMAN_ENV_BAL_3_0DB)
        } else {
            (6, T_HUFFMAN_ENV_BAL_1_5DB, F_HUFFMAN_ENV_BAL_1_5DB)
        }
    } else if sbr.data[ch].bs_amp_res != 0 {
        (6, T_HUFFMAN_ENV_3_0DB, F_HUFFMAN_ENV_3_0DB)
    } else {
        (7, T_HUFFMAN_ENV_1_5DB, F_HUFFMAN_ENV_1_5DB)
    };
    // convert to the error type of the enclosing Result<(), ()>
    let t_lav = vlc.lav[t_huff];
    let f_lav = vlc.lav[f_huff];

    let n0 = sbr.n[0];
    let n1 = sbr.n[1];
    let num_env = sbr.data[ch].bs_num_env;
    for i in 0..num_env {
        if sbr.data[ch].bs_df_env[i + 1] != 0 {
            if sbr.data[ch].bs_freq_res[i + 1] == sbr.data[ch].bs_freq_res[i] {
                for j in 0..sbr.n[sbr.data[ch].bs_freq_res[i + 1] as usize] {
                    let symbol = vlc.tables[t_huff].decode(&mut br.br).map_err(|_| ())?;
                    let val =
                        sbr.data[ch].env_facs_q[i][j] as i32 + delta * (symbol as i32 - t_lav);
                    if !(0..=127).contains(&val) {
                        return Err(());
                    }
                    sbr.data[ch].env_facs_q[i + 1][j] = val as u8;
                }
            } else if sbr.data[ch].bs_freq_res[i + 1] != 0 {
                for j in 0..sbr.n[sbr.data[ch].bs_freq_res[i + 1] as usize] {
                    let k = (j + odd as usize) >> 1;
                    let symbol = vlc.tables[t_huff].decode(&mut br.br).map_err(|_| ())?;
                    let val =
                        sbr.data[ch].env_facs_q[i][k] as i32 + delta * (symbol as i32 - t_lav);
                    if !(0..=127).contains(&val) {
                        return Err(());
                    }
                    sbr.data[ch].env_facs_q[i + 1][j] = val as u8;
                }
            } else {
                for j in 0..sbr.n[sbr.data[ch].bs_freq_res[i + 1] as usize] {
                    let k = if j != 0 { 2 * j - odd as usize } else { 0 };
                    let symbol = vlc.tables[t_huff].decode(&mut br.br).map_err(|_| ())?;
                    let val =
                        sbr.data[ch].env_facs_q[i][k] as i32 + delta * (symbol as i32 - t_lav);
                    if !(0..=127).contains(&val) {
                        return Err(());
                    }
                    sbr.data[ch].env_facs_q[i + 1][j] = val as u8;
                }
            }
        } else {
            sbr.data[ch].env_facs_q[i + 1][0] = (delta * br.bits(bits as u32) as i32) as u8;
            for j in 1..sbr.n[sbr.data[ch].bs_freq_res[i + 1] as usize] {
                let symbol = vlc.tables[f_huff].decode(&mut br.br).map_err(|_| ())?;
                let val =
                    sbr.data[ch].env_facs_q[i + 1][j - 1] as i32 + delta * (symbol as i32 - f_lav);
                if !(0..=127).contains(&val) {
                    return Err(());
                }
                sbr.data[ch].env_facs_q[i + 1][j] = val as u8;
            }
        }
    }
    let _ = (n0, n1);
    let last = sbr.data[ch].env_facs_q[num_env];
    sbr.data[ch].env_facs_q[0] = last;
    Ok(())
}

fn read_sbr_noise(sbr: &mut Sbr, br: &mut SbrBitReader, vlc: &SbrVlc, ch: usize) -> Result<(), ()> {
    let delta = i32::from(ch == 1 && sbr.bs_coupling == 1) + 1;

    let (t_huff, f_huff) = if sbr.bs_coupling != 0 && ch != 0 {
        (T_HUFFMAN_NOISE_BAL_3_0DB, F_HUFFMAN_ENV_BAL_3_0DB)
    } else {
        (T_HUFFMAN_NOISE_3_0DB, F_HUFFMAN_ENV_3_0DB)
    };
    let t_lav = vlc.lav[t_huff];
    let f_lav = vlc.lav[f_huff];

    let n_q = sbr.n_q;
    let num_noise = sbr.data[ch].bs_num_noise;
    for i in 0..num_noise {
        if sbr.data[ch].bs_df_noise[i + 1] != 0 {
            for j in 0..n_q {
                let symbol = vlc.tables[t_huff].decode(&mut br.br).map_err(|_| ())?;
                let val = sbr.data[ch].noise_facs_q[i][j] as i32 + delta * (symbol as i32 - t_lav);
                if !(0..=30).contains(&val) {
                    return Err(());
                }
                sbr.data[ch].noise_facs_q[i + 1][j] = val as u8;
            }
        } else {
            sbr.data[ch].noise_facs_q[i + 1][0] = (delta * br.bits(5) as i32) as u8;
            for j in 1..n_q {
                let symbol = vlc.tables[f_huff].decode(&mut br.br).map_err(|_| ())?;
                let val = sbr.data[ch].noise_facs_q[i + 1][j - 1] as i32
                    + delta * (symbol as i32 - f_lav);
                if !(0..=30).contains(&val) {
                    return Err(());
                }
                sbr.data[ch].noise_facs_q[i + 1][j] = val as u8;
            }
        }
    }
    let last = sbr.data[ch].noise_facs_q[num_noise];
    sbr.data[ch].noise_facs_q[0] = last;
    Ok(())
}

fn read_sbr_single_channel_element(
    sbr: &mut Sbr,
    br: &mut SbrBitReader,
    vlc: &SbrVlc,
) -> Result<(), ()> {
    if br.bit() {
        br.bits(4); // bs_reserved
    }
    read_sbr_grid(sbr, br, 0)?;
    read_sbr_dtdf(sbr, br, 0);
    read_sbr_invf(sbr, br, 0);
    read_sbr_envelope(sbr, br, vlc, 0)?;
    read_sbr_noise(sbr, br, vlc, 0)?;
    sbr.data[0].bs_add_harmonic_flag = br.bit() as u8;
    if sbr.data[0].bs_add_harmonic_flag != 0 {
        let n1 = sbr.n[1];
        for i in 0..n1 {
            sbr.data[0].bs_add_harmonic[i] = br.bit() as u8;
        }
    }
    Ok(())
}

fn read_sbr_channel_pair_element(
    sbr: &mut Sbr,
    br: &mut SbrBitReader,
    vlc: &SbrVlc,
) -> Result<(), ()> {
    if br.bit() {
        br.bits(8); // bs_reserved
    }
    sbr.bs_coupling = br.bit() as u8;
    if sbr.bs_coupling != 0 {
        read_sbr_grid(sbr, br, 0)?;
        // copy_sbr_grid(&data[1], &data[0])
        {
            let (a, b) = sbr.data.split_at_mut(1);
            let dst = &mut b[0];
            let src = &a[0];
            dst.bs_freq_res[1..].copy_from_slice(&src.bs_freq_res[1..]);
            dst.t_env = src.t_env;
            dst.t_q = src.t_q;
            dst.bs_num_env = src.bs_num_env;
            dst.bs_amp_res = src.bs_amp_res;
            dst.bs_num_noise = src.bs_num_noise;
            dst.bs_frame_class = src.bs_frame_class;
            dst.e_a[1] = src.e_a[1];
        }
        read_sbr_dtdf(sbr, br, 0);
        read_sbr_dtdf(sbr, br, 1);
        read_sbr_invf(sbr, br, 0);
        let invf0 = sbr.data[0].bs_invf_mode[0];
        sbr.data[1].bs_invf_mode[1] = sbr.data[1].bs_invf_mode[0];
        sbr.data[1].bs_invf_mode[0] = invf0;
        read_sbr_envelope(sbr, br, vlc, 0)?;
        read_sbr_noise(sbr, br, vlc, 0)?;
        read_sbr_envelope(sbr, br, vlc, 1)?;
        read_sbr_noise(sbr, br, vlc, 1)?;
    } else {
        read_sbr_grid(sbr, br, 0)?;
        read_sbr_grid(sbr, br, 1)?;
        read_sbr_dtdf(sbr, br, 0);
        read_sbr_dtdf(sbr, br, 1);
        read_sbr_invf(sbr, br, 0);
        read_sbr_invf(sbr, br, 1);
        read_sbr_envelope(sbr, br, vlc, 0)?;
        read_sbr_envelope(sbr, br, vlc, 1)?;
        read_sbr_noise(sbr, br, vlc, 0)?;
        read_sbr_noise(sbr, br, vlc, 1)?;
    }

    sbr.data[0].bs_add_harmonic_flag = br.bit() as u8;
    if sbr.data[0].bs_add_harmonic_flag != 0 {
        let n1 = sbr.n[1];
        for i in 0..n1 {
            sbr.data[0].bs_add_harmonic[i] = br.bit() as u8;
        }
    }
    sbr.data[1].bs_add_harmonic_flag = br.bit() as u8;
    if sbr.data[1].bs_add_harmonic_flag != 0 {
        let n1 = sbr.n[1];
        for i in 0..n1 {
            sbr.data[1].bs_add_harmonic[i] = br.bit() as u8;
        }
    }
    Ok(())
}

fn read_sbr_data(sbr: &mut Sbr, br: &mut SbrBitReader, vlc: &SbrVlc, id_aac: usize) -> u32 {
    let cnt = br.br.pos();
    sbr.id_aac = id_aac;
    sbr.ready_for_dequant = true;

    let res = if id_aac == 0 || id_aac == 4 {
        // TYPE_SCE (0) or TYPE_CCE (4)... FFmpeg handles SCE|CCE via SCE path
        read_sbr_single_channel_element(sbr, br, vlc)
    } else if id_aac == 1 {
        read_sbr_channel_pair_element(sbr, br, vlc)
    } else {
        Err(())
    };
    if res.is_err() {
        sbr.turnoff();
        return (br.br.pos() - cnt) as u32;
    }

    if br.bit() {
        // bs_extended_data
        let mut num_bits_left = br.bits(4) as i32;
        if num_bits_left == 15 {
            num_bits_left += br.bits(8) as i32;
        }
        num_bits_left <<= 3;
        while num_bits_left > 7 {
            num_bits_left -= 2;
            let ext_id = br.bits(2);
            match ext_id {
                1 => {
                    // EXTENSION_ID_PS: parametric stereo — unsupported;
                    // skip the declared payload.
                    br.bits(num_bits_left as u32);
                    num_bits_left = 0;
                }
                _ => {
                    br.bits(num_bits_left as u32);
                    num_bits_left = 0;
                }
            }
        }
    }

    (br.br.pos() - cnt) as u32
}

/// `ff_decode_sbr_extension`. `payload` holds the cnt bytes of the FIL
/// extension payload INCLUDING the 4-bit extension type. Returns the
/// number of FIL bytes consumed (always `cnt` on success paths).
pub fn decode_sbr_extension(
    sbr: &mut Sbr,
    payload: &[u8],
    crc: bool,
    cnt: usize,
    id_aac: usize,
) -> usize {
    // The payload starts after the 4-bit extension type: build a reader
    // over the remaining bytes plus slack for overreads.
    let mut storage = [0u8; 1024];
    let avail = payload.len().min(storage.len() - 8);
    storage[..avail].copy_from_slice(&payload[..avail]);
    let mut br = SbrBitReader::new(BitReader::new(&storage));

    let num_sbr_bits_start = 0usize;
    let mut num_sbr_bits = 0usize;

    sbr.reset = false;

    if crc {
        br.bits(10);
        num_sbr_bits += 10;
    }

    // Save some state from the previous frame.
    sbr.kx[0] = sbr.kx[1];
    sbr.m[0] = sbr.m[1];
    sbr.kx_and_m_pushed = true;

    num_sbr_bits += 1;
    if br.bit() {
        num_sbr_bits += read_sbr_header(sbr, &mut br) as usize;
    }

    if sbr.reset {
        sbr.reset_tables();
    }

    if sbr.start {
        num_sbr_bits += read_sbr_data(sbr, &mut br, sbr_vlc(), id_aac) as usize;
    }

    let _ = num_sbr_bits_start;
    let _ = num_sbr_bits;
    let _ = avail;
    cnt
}

use super::freq;

// SBRChannel gains an extra field for bs_df_env/bs_df_noise arrays.
impl SbrChannel {
    pub fn zero_df(&mut self) {}
}

// Provide bs_df_env/bs_df_noise through the existing struct: declared in
// SbrChannel below via parse-only storage arrays.
impl Sbr {
    pub fn df_arrays_mut(&mut self, ch: usize) -> (&mut [u8; 8], &mut [u8; 4]) {
        let d = &mut self.data[ch];
        (&mut d.bs_df_env, &mut d.bs_df_noise)
    }
}

// Silence unused warnings for tables only used by PS (not ported).
const _: () = ();

// The SBRChannel struct needs the df arrays; they are part of the struct
// definition in mod.rs (bs_df_env, bs_df_noise).
const _: () = ();
