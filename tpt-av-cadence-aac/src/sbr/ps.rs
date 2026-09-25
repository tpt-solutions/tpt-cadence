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

/// Which parameter family a `read_params` call decodes. This controls the
/// delta-accumulation semantics: IID/ICC are signed values with hard range
/// errors (reference `ERR_CONDITION`), while IPD/OPD phase indices wrap
/// modulo 8 (reference `MASK` 0x07) and never fail.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ParKind {
    Iid,
    Icc,
    IpdOpd,
}

#[derive(Clone)]
pub struct ParametricStereo {
    pub start: bool,
    pub enable_iid: bool,
    pub iid_quant: bool,
    pub nr_iid_par: usize,
    pub nr_ipdopd_par: usize,
    pub enable_icc: bool,
    pub icc_mode: usize,
    pub nr_icc_par: usize,
    pub enable_ext: bool,
    pub enable_ipdopd: bool,
    pub frame_class: usize,
    pub num_env_old: usize,
    pub num_env: usize,
    pub border_position: [i32; MAX_ENV + 1],
    pub iid_par: [[i8; MAX_PAR]; MAX_ENV],
    pub icc_par: [[i8; MAX_PAR]; MAX_ENV],
    pub ipd_par: [[i8; MAX_PAR]; MAX_ENV],
    pub opd_par: [[i8; MAX_PAR]; MAX_ENV],
    pub is34bands: bool,
    pub is34bands_old: bool,
}

impl Default for ParametricStereo {
    fn default() -> Self {
        Self::new()
    }
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
        *self = Self::new();
    }

    /// Decode the PS extension payload. `bits_left` excludes the extension id.
    pub fn decode(&mut self, br: &mut SbrBitReader<'_>, bits_left: usize) -> usize {
        let start = br.br.pos();
        let mut candidate = self.clone();
        let valid = bits_left != 0
            && candidate.decode_inner(br)
            && !br.br.overread()
            && br.br.pos() - start <= bits_left;
        if valid {
            *self = candidate;
            br.br.pos() - start
        } else {
            self.disable();
            br.br.set_pos(start + bits_left);
            bits_left
        }
    }

    fn decode_inner(&mut self, br: &mut SbrBitReader<'_>) -> bool {
        let header = br.bit();
        if header {
            self.enable_iid = br.bit();
            if self.enable_iid {
                let mode = br.bits(3) as usize;
                if mode > 5 {
                    {
                        eprintln!("FAIL at line {}", line!());
                        return false;
                    }
                }
                self.nr_iid_par = [10, 20, 34, 10, 20, 34][mode];
                self.iid_quant = mode > 2;
                self.nr_ipdopd_par = [5, 11, 17, 5, 11, 17][mode];
            }
            self.enable_icc = br.bit();
            if self.enable_icc {
                self.icc_mode = br.bits(3) as usize;
                if self.icc_mode > 5 {
                    {
                        eprintln!("FAIL at line {}", line!());
                        return false;
                    }
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
                    {
                        eprintln!("FAIL at line {}", line!());
                        return false;
                    }
                }
            }
        } else {
            let shift = self.num_env.trailing_zeros();
            for env in 1..=self.num_env {
                self.border_position[env] = ((env * QMF_SLOTS) >> shift) as i32 - 1;
            }
        }
        // The reference reads each per-envelope dt flag inside the
        // enabled branch, so a disabled parameter family consumes no
        // dt bits at all.
        if self.enable_iid {
            for env in 0..self.num_env {
                let dt = usize::from(br.bit());
                let iid_table = match (dt == 1, self.iid_quant) {
                    (false, false) => 2, // df0
                    (false, true) => 0,  // df1
                    (true, false) => 3,  // dt0
                    (true, true) => 1,   // dt1
                };
                if !read_params(
                    br,
                    &mut self.iid_par,
                    self.nr_iid_par,
                    ParKind::Iid,
                    iid_table,
                    env,
                    self.num_env_old,
                    dt == 1,
                    7 + 8 * i32::from(self.iid_quant),
                ) {
                    {
                        eprintln!("FAIL at line {}", line!());
                        return false;
                    }
                }
            }
        } else {
            self.iid_par.fill([0; MAX_PAR]);
        }
        if self.enable_icc {
            for env in 0..self.num_env {
                let dt = usize::from(br.bit());
                if !read_params(
                    br,
                    &mut self.icc_par,
                    self.nr_icc_par,
                    ParKind::Icc,
                    if dt == 1 { 5 } else { 4 },
                    env,
                    self.num_env_old,
                    dt == 1,
                    7,
                ) {
                    {
                        eprintln!("FAIL at line {}", line!());
                        return false;
                    }
                }
            }
        } else {
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
                        {
                            eprintln!("FAIL at line {}", line!());
                            return false;
                        }
                    };
                    if std::env::var_os("PS_TEST_TRACE").is_some() {
                        eprintln!("ext consumed={consumed} count_before={count}");
                    }
                    count -= 2 + consumed as i32;
                } else {
                    br.br.skip_bits((count - 2) as usize);
                    count = 0;
                }
            }
            if count < 0 {
                {
                    eprintln!("FAIL at line {}", line!());
                    return false;
                }
            }
            if count > 0 {
                br.br.skip_bits(count as usize);
            }
        }
        if !self.enable_ipdopd {
            self.ipd_par.fill([0; MAX_PAR]);
            self.opd_par.fill([0; MAX_PAR]);
        }
        // End-of-frame fix-ups (reference ff_ps_read_data): a final "fake"
        // envelope extends the parameter run to the last QMF slot so the
        // synthesis-time interpolation covers the whole frame, and the
        // 20/34-band mode history advances afterwards (both stereo
        // processing and decorrelation compare `is34bands` against
        // `is34bands_old`).
        if self.num_env == 0 || self.border_position[self.num_env] < (QMF_SLOTS - 1) as i32 {
            let source = if self.num_env > 0 {
                self.num_env - 1
            } else {
                self.num_env_old.saturating_sub(1)
            };
            if source != self.num_env {
                self.iid_par[self.num_env] = self.iid_par[source];
                self.icc_par[self.num_env] = self.icc_par[source];
                if self.enable_ipdopd {
                    self.ipd_par[self.num_env] = self.ipd_par[source];
                    self.opd_par[self.num_env] = self.opd_par[source];
                }
            }
            // The copied row was range-checked when it was read, so the
            // reference's re-validation here is vacuous.
            self.num_env += 1;
            self.border_position[self.num_env] = (QMF_SLOTS - 1) as i32;
        }
        self.is34bands_old = self.is34bands;
        self.is34bands = (self.enable_iid && self.nr_iid_par == 34)
            || (self.enable_icc && self.nr_icc_par == 34);
        if header {
            self.start = true;
        }
        true
    }
}

fn read_extension(br: &mut SbrBitReader<'_>, ps: &mut ParametricStereo) -> Option<usize> {
    let start = br.br.pos();
    ps.enable_ipdopd = br.bit();
    if std::env::var_os("PS_TEST_TRACE").is_some() {
        eprintln!(
            "read_extension: enable_ipdopd={} num_env={}",
            ps.enable_ipdopd, ps.num_env
        );
    }
    if ps.enable_ipdopd {
        for env in 0..ps.num_env {
            let dt = usize::from(br.bit());
            if !read_params(
                br,
                &mut ps.ipd_par,
                ps.nr_ipdopd_par,
                ParKind::IpdOpd,
                if dt == 1 { 7 } else { 6 },
                env,
                ps.num_env_old,
                dt == 1,
                7,
            ) || !read_params(
                br,
                &mut ps.opd_par,
                ps.nr_ipdopd_par,
                ParKind::IpdOpd,
                if dt == 1 { 9 } else { 8 },
                env,
                ps.num_env_old,
                dt == 1,
                7,
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
    kind: ParKind,
    table: usize,
    env: usize,
    old_count: usize,
    delta: bool,
    max_value: i32,
) -> bool {
    let mut value = 0;
    for band in 0..count {
        let Some(step) = huffman(br, table) else {
            return false;
        };
        match kind {
            // Phase indices wrap through 3 bits and are never an error
            // (reference `MASK` 0x07 with no `ERR_CONDITION`).
            ParKind::IpdOpd => {
                value = if delta {
                    let prev = if env > 0 {
                        env - 1
                    } else {
                        old_count.saturating_sub(1).min(MAX_ENV - 1)
                    };
                    i32::from(dst[prev][band]) + step
                } else {
                    value + step
                } & 0x07;
                dst[env][band] = value as i8;
            }
            ParKind::Iid => {
                value = if delta {
                    let prev = if env > 0 {
                        env - 1
                    } else {
                        old_count.saturating_sub(1).min(MAX_ENV - 1)
                    };
                    i32::from(dst[prev][band]) + step
                } else {
                    value + step
                };
                if !(-128..=127).contains(&value) || value.abs() > max_value {
                    return false;
                }
                dst[env][band] = value as i8;
            }
            // ICC is an unsigned 3-bit index; negative accumulated values
            // are illegal (reference `icc_par[e][b] > 7U`).
            ParKind::Icc => {
                value = if delta {
                    let prev = if env > 0 {
                        env - 1
                    } else {
                        old_count.saturating_sub(1).min(MAX_ENV - 1)
                    };
                    i32::from(dst[prev][band]) + step
                } else {
                    value + step
                };
                if !(0..=max_value).contains(&value) {
                    return false;
                }
                dst[env][band] = value as i8;
            }
        }
    }
    true
}

/// Canonical code assignment mirroring `ff_vlc_init_from_lengths`: the
/// normative tables are listed in code order, and codes are assigned by
/// incrementing a left-aligned 32-bit counter per entry — NOT by symbol
/// value within a code length (several PS codebooks list same-length
/// symbols out of numeric order, e.g. `(9,17), (51,17), (11,17), ...`).
/// Each entry is `(len-bit code, len, symbol + table offset)`.
fn huffman_codes() -> &'static [Vec<(u32, u32, i32)>] {
    static CODES: std::sync::OnceLock<Vec<Vec<(u32, u32, i32)>>> = std::sync::OnceLock::new();
    CODES.get_or_init(|| {
        HUFF_SIZES
            .iter()
            .scan(0usize, |offset, size| {
                let start = *offset;
                *offset += size;
                Some((start, *size))
            })
            .map(|(start, size)| {
                let entries = &HUFF_PAIRS[start..start + size];
                let mut running = 0u32;
                let mut codes = Vec::with_capacity(entries.len());
                for &(symbol, len) in entries {
                    if len == 0 {
                        continue;
                    }
                    let len = u32::from(len);
                    codes.push((
                        running >> (32 - len),
                        len,
                        i32::from(symbol) + i32::from(HUFF_OFFSET[table_offset(start)]),
                    ));
                    running = running.wrapping_add(1u32 << (32 - len));
                }
                codes
            })
            .collect()
    })
}

fn table_offset(start: usize) -> usize {
    HUFF_SIZES
        .iter()
        .scan(0usize, |offset, size| {
            let s = *offset;
            *offset += size;
            Some(s)
        })
        .position(|s| s == start)
        .unwrap()
}

fn huffman(br: &mut SbrBitReader<'_>, table: usize) -> Option<i32> {
    let codes = &huffman_codes()[table];
    let max_len = codes.iter().map(|&(_, len, _)| len).max().unwrap_or(0);
    let mut acc = 0u32;
    for depth in 1..=max_len {
        acc = (acc << 1) | u32::from(br.bit());
        for &(code, len, symbol) in codes {
            if len == depth && code == acc {
                return Some(symbol);
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
        assert_eq!(huffman(&mut br, 2), Some(0));
        let mut br = SbrBitReader::new(BitReader::new(&bytes));
        assert_eq!(huffman(&mut br, 4), Some(0));
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
            (2, 2),
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
        assert_eq!(used, 47);
        assert_eq!(br.br.pos(), 47);
    }

    #[test]
    fn malformed_payload_does_not_commit_mode_history() {
        let mut ps = ParametricStereo::new();
        let bytes = [0b1010_0000u8];
        let mut br = SbrBitReader::new(BitReader::new(&bytes));
        assert_eq!(ps.decode(&mut br, bytes.len() * 8), bytes.len() * 8);
        assert!(!ps.start);
        assert!(!ps.is34bands);
        assert!(!ps.is34bands_old);
    }

    #[test]
    fn disable_clears_ps_feature_flags() {
        let mut ps = ParametricStereo::new();
        ps.start = true;
        ps.enable_iid = true;
        ps.enable_icc = true;
        ps.enable_ext = true;
        ps.enable_ipdopd = true;
        ps.icc_mode = 5;
        ps.frame_class = 1;
        ps.num_env_old = 4;
        ps.num_env = 3;
        ps.disable();
        assert!(!ps.start);
        assert!(!ps.enable_iid);
        assert!(!ps.enable_icc);
        assert!(!ps.enable_ext);
        assert!(!ps.enable_ipdopd);
        assert_eq!(ps.icc_mode, 0);
        assert_eq!(ps.frame_class, 0);
        assert_eq!(ps.num_env_old, 0);
        assert_eq!(ps.num_env, 0);
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

    #[test]
    fn fake_final_envelope_extends_to_last_qmf_slot() {
        // frame_class 1 with a last border below slot 31: the reference
        // appends a copy of the last envelope so synthesis covers all 32
        // slots.
        fn code_bits(table: usize, symbol: i32) -> Vec<(u32, u32)> {
            let start = HUFF_SIZES[..table].iter().sum::<usize>();
            let entries = &HUFF_PAIRS[start..start + HUFF_SIZES[table]];
            let mut running = 0u32;
            for &(sym, len) in entries {
                if i32::from(sym) == symbol {
                    let code = running >> (32 - u32::from(len));
                    return (0..u32::from(len))
                        .map(|i| ((code >> (u32::from(len) - 1 - i)) & 1, 1))
                        .collect();
                }
                if len > 0 {
                    running += 1u32 << (32 - u32::from(len));
                }
            }
            panic!("symbol {symbol} not in table {table}");
        }

        let mut fields: Vec<(u32, u32)> = vec![
            (1, 1),  // header
            (1, 1),  // enable_iid
            (1, 3),  // iid mode 1: 20 coarse bands
            (0, 1),  // enable_icc
            (0, 1),  // enable_ext
            (1, 1),  // frame_class variable
            (1, 2),  // num_env 2
            (5, 5),  // border 1 = 5
            (10, 5), // border 2 = 10
            (0, 1),  // env 0: dt = 0
        ];
        for _ in 0..20 {
            fields.extend(code_bits(2, 14));
        }
        fields.push((0, 1)); // env 1: dt = 0
        for _ in 0..20 {
            fields.extend(code_bits(2, 14));
        }
        let bytes = packed(&fields);
        let mut ps = ParametricStereo::new();
        let mut br = SbrBitReader::new(BitReader::new(&bytes));
        ps.decode(&mut br, bytes.len() * 8);
        assert!(ps.start);
        assert_eq!(ps.num_env, 3, "fake envelope appended");
        assert_eq!(ps.border_position[1], 5);
        assert_eq!(ps.border_position[2], 10);
        assert_eq!(ps.border_position[3], 31);
        assert_eq!(ps.iid_par[2], ps.iid_par[1], "last envelope copied");
    }

    fn code_bits(table: usize, symbol: i32) -> Vec<(u32, u32)> {
        let start = HUFF_SIZES[..table].iter().sum::<usize>();
        let entries = &HUFF_PAIRS[start..start + HUFF_SIZES[table]];
        let mut running = 0u32;
        for &(sym, len) in entries {
            if i32::from(sym) == symbol {
                let code = running >> (32 - u32::from(len));
                return (0..u32::from(len))
                    .map(|i| ((code >> (u32::from(len) - 1 - i)) & 1, 1))
                    .collect();
            }
            if len > 0 {
                running += 1u32 << (32 - u32::from(len));
            }
        }
        panic!("symbol {symbol} not in table {table}");
    }

    #[test]
    fn ipd_delta_wraps_modulo_eight_instead_of_erroring() {
        // ipd df steps of +6 then +6 accumulate to 12, which wraps to 4
        // rather than erroring (reference MASK 0x07).
        let bits: Vec<u32> = code_bits(6, 6)
            .into_iter()
            .chain(code_bits(6, 6))
            .chain(code_bits(6, 1))
            .map(|(b, _)| b)
            .collect();
        let mut byte = 0u32;
        let mut n = 0u32;
        let mut bytes = Vec::new();
        for b in bits {
            byte = (byte << 1) | b;
            n += 1;
            if n == 8 {
                bytes.push(byte as u8);
                byte = 0;
                n = 0;
            }
        }
        if n > 0 {
            bytes.push((byte << (8 - n)) as u8);
        }
        let mut dst = [[0i8; MAX_PAR]; MAX_ENV];
        let mut br = SbrBitReader::new(BitReader::new(&bytes));
        assert!(read_params(
            &mut br,
            &mut dst,
            3,
            ParKind::IpdOpd,
            6,
            0,
            0,
            false,
            7,
        ));
        assert_eq!(dst[0][0], 6);
        assert_eq!(dst[0][1], 4, "(6 + 6) & 7 == 4");
        assert_eq!(dst[0][2], 4 + 1);
    }

    #[test]
    fn negative_icc_accumulation_is_rejected() {
        // header, iid off, icc on mode 1, ext off, fixed class, 1 env,
        // icc df first step -1 -> negative -> invalid (reference
        // `icc_par[e][b] > 7U`).
        let mut fields: Vec<(u32, u32)> = vec![
            (1, 1),
            (0, 1),
            (1, 1),
            (1, 3),
            (0, 1),
            (0, 1),
            (1, 2),
            (0, 1), // icc env 0: dt = 0
        ];
        fields.extend(code_bits(4, 6)); // step -1
        let bytes = packed(&fields);
        let mut ps = ParametricStereo::new();
        let mut br = SbrBitReader::new(BitReader::new(&bytes));
        let used = ps.decode(&mut br, bytes.len() * 8);
        assert_eq!(used, bytes.len() * 8);
        assert!(!ps.start, "negative ICC must invalidate the payload");
    }

    #[test]
    fn canonical_codes_follow_table_order_not_symbol_order() {
        // Regression: the PS codebooks list same-length symbols out of
        // numeric order (e.g. iid-fine df has ... (9,17), (51,17),
        // (11,17) ...), so codes are assigned in table order per
        // ff_vlc_init_from_lengths, not by symbol value.
        let codes = huffman_codes();
        let fine_df = &codes[0];
        // First entries: (-2, 4 bits), (+2, 4), (-1, 3), (+1, 3) with
        // offset -30. The two 4-bit codes precede the 3-bit ones.
        assert_eq!(fine_df[0], (0b0000, 4, 28 - 30));
        assert_eq!(fine_df[1], (0b0001, 4, 32 - 30));
        assert_eq!(fine_df[2], (0b001, 3, 29 - 30));
        assert_eq!(fine_df[3], (0b010, 3, 31 - 30));
        // Same-length symbols appear in table (not numeric) order:
        // ... (9,17), (51,17), (11,17), (49,17) ... with offset -30.
        let by_sym: std::collections::HashMap<i32, (u32, u32)> =
            fine_df.iter().map(|&(c, l, s)| (s, (c, l))).collect();
        let (c9, l9) = by_sym[&(9 - 30)];
        let (c51, l51) = by_sym[&(51 - 30)];
        let (c11, l11) = by_sym[&(11 - 30)];
        let (c49, l49) = by_sym[&(49 - 30)];
        assert_eq!((l9, l51, l11, l49), (17, 17, 17, 17));
        assert!(
            c9 < c51 && c51 < c11 && c11 < c49,
            "table order, not symbol order"
        );
        // Decoding long codewords round-trips through huffman().
        let mut bits = Vec::new();
        for (code, len) in [(c9, l9), (c51, l51), (c11, l11), (c49, l49)] {
            for i in (0..len).rev() {
                bits.push((code >> i) & 1);
            }
        }
        let mut byte = 0u32;
        let mut n = 0u32;
        let mut bytes = Vec::new();
        for b in bits {
            byte = (byte << 1) | b;
            n += 1;
            if n == 8 {
                bytes.push(byte as u8);
                byte = 0;
                n = 0;
            }
        }
        if n > 0 {
            bytes.push((byte << (8 - n)) as u8);
        }
        let mut br = SbrBitReader::new(BitReader::new(&bytes));
        assert_eq!(huffman(&mut br, 0), Some(9 - 30));
        assert_eq!(huffman(&mut br, 0), Some(51 - 30));
        assert_eq!(huffman(&mut br, 0), Some(11 - 30));
        assert_eq!(huffman(&mut br, 0), Some(49 - 30));
    }
}
