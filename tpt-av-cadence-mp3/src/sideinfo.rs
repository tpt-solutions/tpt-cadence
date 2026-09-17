//! Layer III side information (granule parameters) for MPEG-1 and MPEG-2/2.5.

use crate::bitreader::BitReader;
use crate::header::FrameHeader;
use crate::tables::{SCF_LONG, SCF_MIXED, SCF_SHORT};
use tpt_av_cadence_core::{CadenceError, Result};

/// Short-block type (block_type 2).
pub(crate) const SHORT_BLOCK: u8 = 2;
/// Stop-block type (block_type 3); start is 1, long is 0.
pub(crate) const STOP_BLOCK: u8 = 3;

/// Per-granule, per-channel side information.
#[derive(Debug, Clone, Default)]
pub(crate) struct GranuleInfo {
    pub part_23_length: u16,
    pub big_values: u16,
    pub scalefac_compress: u16,
    pub global_gain: u8,
    pub block_type: u8,
    pub mixed_block_flag: bool,
    pub table_select: [u8; 3],
    pub region_count: [u8; 3],
    pub subblock_gain: [u8; 3],
    pub preflag: bool,
    pub scalefac_scale: u8,
    pub count1_table: u8,
    /// scfsi flags (meaningful only for a long-block granule 1; the spec's
    /// sharing model does not extend to short-block scalefactors).
    pub scfsi: u8,
    /// Scalefactor-band width table (trailing 0 terminator included).
    pub sfbtab: &'static [u8],
    pub n_long_sfb: u8,
    pub n_short_sfb: u8,
}

/// Index of the scalefactor-band table triple for this header's sample rate
/// family (0–1 MPEG-2.5, 2–4 MPEG-2, 5–7 MPEG-1), matching the layout of
/// `SCF_LONG`/`SCF_SHORT`/`SCF_MIXED`.
pub(crate) fn sr_table_idx(hdr: &FrameHeader) -> usize {
    let raw = crate::header::sample_rate_index(&hdr.bytes);
    // The second version bit distinguishes MPEG-2 from MPEG-2.5;
    // `!lsf` would duplicate `mpeg1` and select the wrong MPEG-2 rows.
    let not_mpeg25 = hdr.bytes[1] & 0x10 != 0;
    let my = raw + ((hdr.mpeg1 as u32) + (not_mpeg25 as u32)) * 3;
    (if my != 0 { my - 1 } else { 0 }) as usize
}

/// Number of leading long-window subbands for mixed blocks (4 on MPEG-2.5
/// 8 kHz, 2 elsewhere; 0 for non-mixed blocks).
pub(crate) fn mixed_long_bands(hdr: &FrameHeader, mixed: bool) -> usize {
    (mixed as usize) << (sr_table_idx(hdr) == 1) as usize
}

/// Parses the side information following the (optionally CRC'd) header.
///
/// Returns `(main_data_begin, granules)` where granules are laid out
/// `[gr0ch0, gr0ch1, gr1ch0, gr1ch1]` (MPEG-1 stereo) and only the first
/// `nch` entries per granule are meaningful.
pub(crate) fn read_side_info(
    bs: &mut BitReader,
    hdr: &FrameHeader,
    granules: &mut [GranuleInfo],
) -> Result<u16> {
    let nch = if hdr.mono { 1 } else { 2 };
    let gr_count = nch * if hdr.mpeg1 { 2 } else { 1 };
    let mut scfsi_stream = [0u8; 2];

    let main_data_begin: u16 = if hdr.mpeg1 {
        let mdb = bs.get_bits(9) as u16;
        // Private bits ride along with the scfsi flags; only the granule-1
        // nibble per channel is meaningful (granule 0 never shares).
        let raw = if hdr.mono {
            bs.get_bits(9)
        } else {
            bs.get_bits(11)
        };
        if hdr.mono {
            scfsi_stream[0] = (raw & 0xF) as u8;
        } else {
            scfsi_stream[0] = ((raw >> 4) & 0xF) as u8;
            scfsi_stream[1] = (raw & 0xF) as u8;
        }
        mdb
    } else {
        let priv_bits = if hdr.mono { 1 } else { 2 };
        (bs.get_bits(8 + priv_bits) >> priv_bits) as u16
    };

    let sr_idx = sr_table_idx(hdr);
    let mut part_23_sum = 0u32;
    for g in 0..gr_count {
        let gr = &mut granules[g];
        gr.part_23_length = bs.get_bits(12) as u16;
        part_23_sum += gr.part_23_length as u32;
        gr.big_values = bs.get_bits(9) as u16;
        if gr.big_values > 288 {
            return Err(CadenceError::CorruptData("big_values out of range".into()));
        }
        gr.global_gain = bs.get_bits(8) as u8;
        gr.scalefac_compress = bs.get_bits(if hdr.mpeg1 { 4 } else { 9 }) as u16;
        gr.sfbtab = &SCF_LONG[sr_idx];
        gr.n_long_sfb = 22;
        gr.n_short_sfb = 0;
        gr.scfsi = 0;
        if bs.get_bit() != 0 {
            gr.block_type = bs.get_bits(2) as u8;
            if gr.block_type == 0 {
                return Err(CadenceError::CorruptData(
                    "reserved block_type 0 with window switching".into(),
                ));
            }
            gr.mixed_block_flag = bs.get_bit() != 0;
            gr.region_count = [7, 255, 255];
            if gr.block_type == SHORT_BLOCK {
                if !gr.mixed_block_flag {
                    gr.region_count[0] = 8;
                    gr.sfbtab = &SCF_SHORT[sr_idx];
                    gr.n_long_sfb = 0;
                    gr.n_short_sfb = 39;
                } else {
                    gr.sfbtab = &SCF_MIXED[sr_idx];
                    gr.n_long_sfb = if hdr.mpeg1 { 8 } else { 6 };
                    gr.n_short_sfb = 30;
                }
            }
            let tables = bs.get_bits(10) << 5;
            gr.subblock_gain = [
                bs.get_bits(3) as u8,
                bs.get_bits(3) as u8,
                bs.get_bits(3) as u8,
            ];
            gr.table_select = [
                ((tables >> 10) & 31) as u8,
                ((tables >> 5) & 31) as u8,
                (tables & 31) as u8,
            ];
        } else {
            gr.block_type = 0;
            gr.mixed_block_flag = false;
            let tables = bs.get_bits(15);
            gr.region_count = [bs.get_bits(4) as u8, bs.get_bits(3) as u8, 255];
            gr.table_select = [
                ((tables >> 10) & 31) as u8,
                ((tables >> 5) & 31) as u8,
                (tables & 31) as u8,
            ];
        }
        gr.preflag = if hdr.mpeg1 {
            bs.get_bit() != 0
        } else {
            gr.scalefac_compress >= 500
        };
        gr.scalefac_scale = bs.get_bits(1) as u8;
        gr.count1_table = bs.get_bits(1) as u8;
        // MPEG-1 granule 1 carries the per-channel scfsi flags. Each
        // granule-channel's own block type gates its own flags: a short
        // block (pure or mixed) cannot share, long/start/stop can.
        if hdr.mpeg1 && g >= nch {
            gr.scfsi = if gr.block_type == SHORT_BLOCK {
                0
            } else {
                scfsi_stream[g % nch]
            };
        }
    }

    // part2_3 lengths may not draw on more bit reservoir than exists.
    if part_23_sum as i64 + bs.bit_pos() as i64
        > bs.limit_bits() as i64 + main_data_begin as i64 * 8
    {
        return Err(CadenceError::CorruptData(
            "side info demands more bit reservoir than available".into(),
        ));
    }
    Ok(main_data_begin)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sample_rate_band_tables_cover_all_version_families() {
        for (version, expected) in [(0xe3, [0, 0, 1]), (0xf3, [2, 3, 4]), (0xfb, [5, 6, 7])] {
            for (rate, index) in expected.into_iter().enumerate() {
                let hdr =
                    crate::header::parse_header(&[0xff, version, 0x30 | (rate as u8) << 2, 0xc0])
                        .unwrap();
                assert_eq!(sr_table_idx(&hdr), index, "{} Hz", hdr.sample_rate_hz);
            }
        }
    }

    #[test]
    fn lsf_side_info_consumes_exact_wire_size() {
        for (mode, bytes) in [(0xc0, 9), (0x00, 17)] {
            let hdr = crate::header::parse_header(&[0xff, 0xf3, 0x38, mode]).unwrap();
            let data = [0u8; 64];
            let mut bs = BitReader::new(&data);
            let mut granules = std::array::from_fn::<_, 4, _>(|_| GranuleInfo::default());
            assert_eq!(read_side_info(&mut bs, &hdr, &mut granules).unwrap(), 0);
            assert_eq!(bs.bit_pos(), bytes * 8, "mode={mode:#x}");
        }
    }

    #[test]
    fn pure_short_block_region_zero_has_nine_scalefactor_bands() {
        // First frame: start windows in granule 0, pure short windows in
        // granule 1. Its ID3v2 tag occupies the first 44 bytes.
        let stream = include_bytes!("../tests/data/mpeg1_44100_stereo_128k.mp3");
        let hdr = crate::header::parse_header(&stream[44..48]).unwrap();
        let mut bs = BitReader::new(&stream[48..44 + hdr.total_bytes()]);
        let mut granules = std::array::from_fn::<_, 4, _>(|_| GranuleInfo::default());
        read_side_info(&mut bs, &hdr, &mut granules).unwrap();
        for gr in &granules[2..] {
            assert_eq!(gr.block_type, SHORT_BLOCK);
            assert!(!gr.mixed_block_flag);
            assert_eq!(gr.region_count[0], 8); // stored as band count minus one
            let bands = gr.region_count[0] as usize + 1;
            assert_eq!(
                gr.sfbtab[..bands]
                    .iter()
                    .map(|&n| n as usize)
                    .sum::<usize>(),
                36
            );
        }
    }

    #[test]
    fn start_block_granule_keeps_scfsi_flags() {
        // Frame 114 (offset 47691): granule 0 is a long block, granule 1 a
        // start block (type 1). The stream's scfsi nibbles are (3, 15) and
        // the reference decoder still applies them (the short-block mask
        // only fires for type 2, i.e. when granule 0 itself is short).
        let stream = include_bytes!("../tests/data/mpeg1_44100_stereo_128k.mp3");
        let hdr = crate::header::parse_header(&stream[47691..47695]).unwrap();
        let mut bs = BitReader::new(&stream[47695..47691 + hdr.total_bytes()]);
        let mut granules = std::array::from_fn::<_, 4, _>(|_| GranuleInfo::default());
        read_side_info(&mut bs, &hdr, &mut granules).unwrap();
        assert_eq!(granules[2].block_type, 1);
        assert_eq!(granules[2].scfsi, 3);
        assert_eq!(granules[3].scfsi, 15);
    }
}
