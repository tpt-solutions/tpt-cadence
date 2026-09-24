//! MPEG-4 Parametric Stereo parameter parsing (ISO/IEC 14496-3).
//! Stereo synthesis is intentionally separate; malformed PS data disables
//! the PS state without disturbing the surrounding SBR payload.
#![allow(clippy::needless_range_loop, clippy::too_many_arguments, dead_code)]

use super::ps_tables::{HUFF_OFFSET, HUFF_PAIRS};
use super::SbrBitReader;

const MAX_ENV: usize = 5;
const MAX_PAR: usize = 34;
const QMF_SLOTS: usize = 32;
const HUFF_SIZES: [usize; 10] = [61, 61, 29, 29, 15, 15, 8, 8, 8, 8];

#[derive(Clone)]
pub(crate) struct ParametricStereo {
    pub start: bool,
    enable_iid: bool,
    iid_quant: bool,
    nr_iid_par: usize,
    nr_ipdopd_par: usize,
    enable_icc: bool,
    icc_mode: usize,
    nr_icc_par: usize,
    enable_ext: bool,
    enable_ipdopd: bool,
    frame_class: usize,
    num_env_old: usize,
    num_env: usize,
    border_position: [i32; MAX_ENV + 1],
    iid_par: [[i8; MAX_PAR]; MAX_ENV],
    icc_par: [[i8; MAX_PAR]; MAX_ENV],
    ipd_par: [[i8; MAX_PAR]; MAX_ENV],
    opd_par: [[i8; MAX_PAR]; MAX_ENV],
    is34bands: bool,
    is34bands_old: bool,
}

impl ParametricStereo {
    pub fn new() -> Self {
        Self {
            start: false,
            enable_iid: false,
            iid_quant: false,
            nr_iid_par: 0,
            nr_ipdopd_par: 0,
            enable_icc: false,
            icc_mode: 0,
            nr_icc_par: 0,
            enable_ext: false,
            enable_ipdopd: false,
            frame_class: 0,
            num_env_old: 0,
            num_env: 0,
            border_position: [-1; MAX_ENV + 1],
            iid_par: [[0; MAX_PAR]; MAX_ENV],
            icc_par: [[0; MAX_PAR]; MAX_ENV],
            ipd_par: [[0; MAX_PAR]; MAX_ENV],
            opd_par: [[0; MAX_PAR]; MAX_ENV],
            is34bands: false,
            is34bands_old: false,
        }
    }

    pub fn disable(&mut self) {
        self.start = false;
        self.iid_par.fill([0; MAX_PAR]);
        self.icc_par.fill([0; MAX_PAR]);
        self.ipd_par.fill([0; MAX_PAR]);
        self.opd_par.fill([0; MAX_PAR]);
    }

    /// Decode the PS extension payload. `bits_left` excludes the extension id.
    pub fn decode(&mut self, br: &mut SbrBitReader<'_>, bits_left: usize) -> usize {
        let start = br.br.pos();
        if bits_left == 0
            || !self.decode_inner(br)
            || br.br.overread()
            || br.br.pos() - start > bits_left
        {
            self.disable();
            br.br.set_pos(start + bits_left);
            return bits_left;
        }
        br.br.pos() - start
    }

    fn decode_inner(&mut self, br: &mut SbrBitReader<'_>) -> bool {
        let header = br.bit();
        if header {
            self.enable_iid = br.bit();
            if self.enable_iid {
                let mode = br.bits(3) as usize;
                if mode > 5 {
                    return false;
                }
                self.nr_iid_par = [10, 20, 34, 10, 20, 34][mode];
                self.iid_quant = mode > 2;
                self.nr_ipdopd_par = [5, 11, 17, 5, 11, 17][mode];
            }
            self.enable_icc = br.bit();
            if self.enable_icc {
                self.icc_mode = br.bits(3) as usize;
                if self.icc_mode > 5 {
                    return false;
                }
                self.nr_icc_par = [10, 20, 34, 10, 20, 34][self.icc_mode];
            }
            self.enable_ext = br.bit();
        }
        self.frame_class = usize::from(br.bit());
        self.num_env_old = self.num_env;
        self.num_env = [[0, 1, 2, 4], [1, 2, 3, 4]][self.frame_class][br.bits(2) as usize];
        self.border_position[0] = -1;
        if self.frame_class == 1 {
            for env in 1..=self.num_env {
                self.border_position[env] = br.bits(5) as i32;
                if self.border_position[env] < self.border_position[env - 1] {
                    return false;
                }
            }
        } else {
            let shift = self.num_env.trailing_zeros();
            for env in 1..=self.num_env {
                self.border_position[env] = ((env * QMF_SLOTS) >> shift) as i32 - 1;
            }
        }
        for env in 0..self.num_env {
            let dt = usize::from(br.bit());
            let iid_table = match (dt == 1, self.iid_quant) {
                (false, false) => 2, // df0
                (false, true) => 0,  // df1
                (true, false) => 3,  // dt0
                (true, true) => 1,   // dt1
            };
            if self.enable_iid
                && !read_params(
                    br,
                    &mut self.iid_par,
                    self.nr_iid_par,
                    iid_table,
                    env,
                    self.num_env_old,
                    dt == 1,
                    7 + 8 * i32::from(self.iid_quant),
                    9,
                )
            {
                return false;
            }
        }
        if !self.enable_iid {
            self.iid_par.fill([0; MAX_PAR]);
        }
        for env in 0..self.num_env {
            let dt = usize::from(br.bit());
            if self.enable_icc
                && !read_params(
                    br,
                    &mut self.icc_par,
                    self.nr_icc_par,
                    if dt == 1 { 5 } else { 4 },
                    env,
                    self.num_env_old,
                    dt == 1,
                    7,
                    9,
                )
            {
                return false;
            }
        }
        if !self.enable_icc {
            self.icc_par.fill([0; MAX_PAR]);
        }
        if self.enable_ext {
            let mut count = br.bits(4) as i32;
            if count == 15 {
                count += br.bits(8) as i32;
            }
            count *= 8;
            while count > 7 {
                let extension_id = br.bits(2) as usize;
                if extension_id == 0 {
                    let Some(consumed) = read_extension(br, self) else {
                        return false;
                    };
                    count -= 2 + consumed as i32;
                } else {
                    br.br.skip_bits((count - 2) as usize);
                    count = 0;
                }
            }
            if count < 0 {
                return false;
            }
            if count > 0 {
                br.br.skip_bits(count as usize);
            }
        }
        if header {
            self.start = true;
        }
        true
    }
}

fn read_extension(br: &mut SbrBitReader<'_>, ps: &mut ParametricStereo) -> Option<usize> {
    let start = br.br.pos();
    ps.enable_ipdopd = br.bit();
    if ps.enable_ipdopd {
        for env in 0..ps.num_env {
            let dt = usize::from(br.bit());
            if !read_params(
                br,
                &mut ps.ipd_par,
                ps.nr_ipdopd_par,
                if dt == 1 { 7 } else { 6 },
                env,
                ps.num_env_old,
                dt == 1,
                7,
                5,
            ) || !read_params(
                br,
                &mut ps.opd_par,
                ps.nr_ipdopd_par,
                if dt == 1 { 9 } else { 8 },
                env,
                ps.num_env_old,
                dt == 1,
                7,
                5,
            ) {
                return None;
            }
        }
    }
    let _reserved = br.bit();
    Some(br.br.pos() - start)
}

fn read_params(
    br: &mut SbrBitReader<'_>,
    dst: &mut [[i8; MAX_PAR]; MAX_ENV],
    count: usize,
    table: usize,
    env: usize,
    old_count: usize,
    delta: bool,
    max_value: i32,
    max_bits: u32,
) -> bool {
    if delta {
        let prev = if env > 0 {
            env - 1
        } else {
            old_count.saturating_sub(1).min(MAX_ENV - 1)
        };
        for band in 0..count {
            let Some(step) = huffman(br, table, max_bits) else {
                return false;
            };
            let value = i32::from(dst[prev][band]) + step;
            if !(-128..=127).contains(&value) || value.unsigned_abs() as i32 > max_value {
                return false;
            }
            dst[env][band] = value as i8;
        }
    } else {
        let mut value = 0;
        for band in 0..count {
            let Some(step) = huffman(br, table, max_bits) else {
                return false;
            };
            value += step;
            if !(-128..=127).contains(&value) || value.unsigned_abs() as i32 > max_value {
                return false;
            }
            dst[env][band] = value as i8;
        }
    }
    true
}

fn reverse_bits(mut value: u32, width: u32) -> u32 {
    let mut out = 0;
    for _ in 0..width {
        out = (out << 1) | (value & 1);
        value >>= 1;
    }
    out
}

fn canonical_code(table: &[(u8, u8)], target: (u8, u8)) -> u32 {
    let rank = table
        .iter()
        .filter(|&&(symbol, len)| len < target.1 || (len == target.1 && symbol < target.0))
        .count() as u32;
    reverse_bits(rank, target.1 as u32)
}

fn huffman(br: &mut SbrBitReader<'_>, table: usize, max_bits: u32) -> Option<i32> {
    let start = HUFF_SIZES[..table].iter().sum::<usize>();
    let entries = &HUFF_PAIRS[start..start + HUFF_SIZES[table]];
    let mut code = 0u32;
    for depth in 1..=max_bits {
        code = (code << 1) | u32::from(br.bit());
        for &entry in entries {
            let (symbol, bits) = entry;
            if bits as u32 == depth && canonical_code(entries, entry) == code {
                return Some(i32::from(symbol) + i32::from(HUFF_OFFSET[table]));
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bitreader::BitReader;

    fn packed(fields: &[(u32, u32)]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut acc = 0u32;
        let mut count = 0u32;
        for &(value, width) in fields {
            acc = (acc << width) | value;
            count += width;
            while count >= 8 {
                count -= 8;
                out.push((acc >> count) as u8);
            }
        }
        if count > 0 {
            out.push((acc << (8 - count)) as u8);
        }
        out
    }

    #[test]
    fn zero_iid_and_icc_codewords_decode() {
        let bytes = [0b0000_0000u8];
        let mut br = SbrBitReader::new(BitReader::new(&bytes));
        assert_eq!(huffman(&mut br, 2, 9), Some(0));
        let mut br = SbrBitReader::new(BitReader::new(&bytes));
        assert_eq!(huffman(&mut br, 4, 9), Some(0));
    }

    #[test]
    fn parses_zero_iid_icc_payload() {
        let bytes = packed(&[
            (1, 1),
            (1, 1),
            (0, 3),
            (1, 1),
            (0, 3),
            (0, 1),
            (0, 1),
            (1, 2),
            (0, 1),
            (0, 1),
            (0, 1),
            (0, 1),
            (0, 1),
            (0, 1),
            (0, 1),
            (0, 1),
            (0, 1),
            (0, 1),
            (0, 1),
            (0, 1),
            (0, 1),
            (0, 1),
            (0, 1),
            (0, 1),
            (0, 1),
            (0, 1),
            (0, 1),
            (0, 1),
            (0, 1),
            (0, 1),
        ]);
        let mut ps = ParametricStereo::new();
        let mut br = SbrBitReader::new(BitReader::new(&bytes));
        let used = ps.decode(&mut br, bytes.len() * 8);
        assert!(ps.start);
        assert_eq!(used, 35);
        assert_eq!(ps.num_env, 1);
        assert_eq!(ps.nr_iid_par, 10);
        assert_eq!(ps.nr_icc_par, 10);
    }

    #[test]
    fn nested_extension_payload_is_consumed_exactly() {
        let bytes = packed(&[
            (1, 1),
            (1, 1),
            (0, 3),
            (1, 1),
            (0, 3),
            (1, 1),
            (0, 1),
            (1, 2),
            (0, 1),
            (0, 1),
            (0, 1),
            (0, 1),
            (0, 1),
            (0, 1),
            (0, 1),
            (0, 1),
            (0, 1),
            (0, 1),
            (0, 1),
            (0, 1),
            (0, 1),
            (0, 1),
            (0, 1),
            (0, 1),
            (0, 1),
            (0, 1),
            (0, 1),
            (0, 1),
            (0, 1),
            (0, 1),
            (1, 4),
            (1, 2),
            (0, 1),
            (0, 1),
            (0, 1),
            (0, 1),
        ]);
        let mut ps = ParametricStereo::new();
        let mut br = SbrBitReader::new(BitReader::new(&bytes));
        let used = ps.decode(&mut br, bytes.len() * 8);
        assert!(ps.start);
        assert_eq!(used, 45);
        assert_eq!(br.br.pos(), 45);
    }

    #[test]
    fn malformed_payload_disables_ps() {
        let bytes = [0b1010_0000u8];
        let mut ps = ParametricStereo::new();
        let mut br = SbrBitReader::new(BitReader::new(&bytes));
        assert_eq!(ps.decode(&mut br, 8), 8);
        assert!(!ps.start);
        assert_eq!(br.br.pos(), 8);
    }
}
