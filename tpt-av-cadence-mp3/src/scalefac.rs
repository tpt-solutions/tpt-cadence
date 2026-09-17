//! Scalefactor decoding (MPEG-1 with scfsi sharing and the MPEG-2/2.5 LSF
//! partition tables) and the per-band requantization scale factors.

use crate::bitreader::BitReader;
use crate::header::FrameHeader;
use crate::sideinfo::GranuleInfo;
use crate::tables::{EXPFRAC, PREAMP, SCFC_DECODE, SCF_MOD, SCF_PARTITIONS};

/// `BITS_DEQUANTIZER_OUT` (−1): global headroom fold in quarter units.
const BITS_DEQUANTIZER_OUT: i32 = -1;
/// `MAX_SCF` = 255 + 4·BITS_DEQUANTIZER_OUT − 210; `MAX_SCFI` rounds to 4.
const MAX_SCFI: i32 = (255 + BITS_DEQUANTIZER_OUT * 4 - 210 + 3) & !3;

/// `y * 2^(exp_q2/4)` with the reference decoder's chunked-multiplier
/// evaluation (each step multiplies by `EXPFRAC[e&3] * 2^(30 - e/4)` in
/// quarter-exponent units). The `wrapping_shr` mirrors the reference's
/// behavior for the (corrupt-data) negative-exponent corner.
pub(crate) fn ldexp_q2(mut y: f32, mut exp_q2: i32) -> f32 {
    loop {
        let e = exp_q2.min(30 * 4);
        let mult = (1u32 << 30).wrapping_shr((e >> 2) as u32) as f32;
        y *= EXPFRAC[(e & 3) as usize] * mult;
        exp_q2 -= e;
        if exp_q2 <= 0 {
            break;
        }
    }
    y
}

/// Reads the coded scalefactors of one granule/channel into `iscf`
/// (raw integer factors) and `ist_pos` (intensity stereo positions, with
/// 0xFF marking "not intensity").
fn read_scalefactors(
    scf: &mut [u8; 40],
    ist_pos: &mut [u8; 39],
    scf_size: &[u8; 4],
    scf_count: &[u8],
    bs: &mut BitReader,
    mut scfsi: i32,
) {
    let mut s = 0usize;
    let mut p = 0usize;
    for i in 0..4 {
        let cnt = scf_count[i] as usize;
        if cnt == 0 {
            break;
        }
        if scfsi & 8 != 0 {
            // Shared with the previous granule: its values were stashed in
            // `ist_pos` when that granule was read.
            scf[s..s + cnt].copy_from_slice(&ist_pos[p..p + cnt]);
        } else {
            let bits = scf_size[i];
            if bits == 0 {
                scf[s..s + cnt].fill(0);
                ist_pos[p..p + cnt].fill(0);
            } else {
                let max_scf = if scfsi < 0 { (1i32 << bits) - 1 } else { -1 };
                for k in 0..cnt {
                    let v = bs.get_bits(bits as u32) as i32;
                    ist_pos[p + k] = if v == max_scf { u8::MAX } else { v as u8 };
                    scf[s + k] = v as u8;
                }
            }
        }
        s += cnt;
        p += cnt;
        scfsi *= 2;
    }
    // Bands without transmitted scalefactors (and the LSF tail) read as 0.
    scf[s] = 0;
    scf[s + 1] = 0;
    scf[s + 2] = 0;
}

/// Decodes one granule's scalefactors and fills `scf[0..n_sfb]` with the
/// per-band requantization multipliers (including global gain, scalefac
/// scale, preflag/pretab, and subblock gains).
pub(crate) fn decode_scalefactors(
    hdr: &FrameHeader,
    ist_pos: &mut [u8; 39],
    bs: &mut BitReader,
    gr: &GranuleInfo,
    scf: &mut [f32; 40],
    ch: usize,
) {
    // Scalefactor count partition: one of three rows by block type, then the
    // LSF byte offset (C reference treats the table as a flat array).
    let partition_idx = (gr.n_short_sfb != 0) as usize + (gr.n_long_sfb == 0) as usize;
    let mut scf_size = [0u8; 4];
    let mut iscf = [0u8; 40];
    let scf_shift = gr.scalefac_scale + 1;
    let mut scfsi = gr.scfsi as i32;
    // Byte offset into the flat partition array (nonzero only for LSF).
    let mut k = 0usize;

    if hdr.mpeg1 {
        let part = SCFC_DECODE[gr.scalefac_compress as usize] as usize;
        scf_size[0] = (part >> 2) as u8;
        scf_size[1] = (part >> 2) as u8;
        scf_size[2] = (part & 3) as u8;
        scf_size[3] = (part & 3) as u8;
    } else {
        // LSF: split scalefac_compress over the mixed-radix `SCF_MOD` groups.
        let ist = (hdr.i_stereo && ch == 1) as usize;
        let mut sfc = (gr.scalefac_compress >> ist) as i32;
        k = ist * 12;
        while sfc >= 0 {
            let mut modprod = 1u32;
            for i in (0..4).rev() {
                let m = SCF_MOD[k + i] as u32;
                scf_size[i] = ((sfc as u32 / modprod) % m) as u8;
                modprod *= m;
            }
            sfc -= modprod as i32;
            k += 4;
        }
        scfsi = -16;
    }

    // Flat-array semantics (C reference): the selected row plus the LSF byte
    // offset; the count reader stops at the first zero (terminator). Clamp
    // the offset and zero-fill short tails so corrupt `scalefac_compress`
    // values cannot index past the table (the C reference reads out of
    // bounds here; we substitute zeros instead).
    let part_off = (partition_idx * 28 + k).min(SCF_PARTITIONS.len());
    let mut counts = [0u8; 4];
    for (dst, src) in counts.iter_mut().zip(SCF_PARTITIONS[part_off..].iter()) {
        *dst = *src;
    }
    read_scalefactors(&mut iscf, ist_pos, &scf_size, &counts, bs, scfsi);

    if gr.n_short_sfb != 0 {
        let sh = 3 - scf_shift;
        let base = gr.n_long_sfb as usize;
        for i in (0..gr.n_short_sfb as usize).step_by(3) {
            // u8 wrapping matches the reference's byte-width store.
            iscf[base + i] = iscf[base + i].wrapping_add(gr.subblock_gain[0] << sh);
            iscf[base + i + 1] = iscf[base + i + 1].wrapping_add(gr.subblock_gain[1] << sh);
            iscf[base + i + 2] = iscf[base + i + 2].wrapping_add(gr.subblock_gain[2] << sh);
        }
    } else if gr.preflag {
        for (i, pre) in PREAMP.iter().enumerate() {
            iscf[11 + i] = iscf[11 + i].wrapping_add(*pre);
        }
    }

    let gain_exp =
        gr.global_gain as i32 + BITS_DEQUANTIZER_OUT * 4 - 210 - (hdr.ms_stereo as i32) * 2;
    let gain = ldexp_q2((1 << (MAX_SCFI / 4)) as f32, MAX_SCFI - gain_exp);
    let n_sfb = gr.n_long_sfb as usize + gr.n_short_sfb as usize;
    for (i, slot) in scf.iter_mut().enumerate().take(n_sfb) {
        *slot = ldexp_q2(gain, (iscf[i] as i32) << scf_shift);
    }
}
