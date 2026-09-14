//! Temporal Noise Shaping: bitstream parsing and all-pole filtering
//! (ISO/IEC 14496-3 §4.6.9.3).

use crate::bitreader::BitReader;
use tpt_av_cadence_core::CadenceError;
use tpt_av_cadence_core::Result;

pub const TNS_MAX_ORDER_LONG: u32 = 12;
pub const TNS_MAX_ORDER_SHORT: u32 = 7;
const MAX_ORDER: usize = 12;

#[derive(Debug, Clone, Default)]
pub struct Tns {
    pub n_filt: [u8; 8],
    pub length: [[u8; 4]; 8],
    pub order: [[u8; 4]; 8],
    pub direction: [[bool; 4]; 8],
    /// Decoded (signed) reflection coefficients, up to 12 per filter.
    pub coef: [[[f32; MAX_ORDER]; 4]; 8],
    pub present: bool,
}

/// Parses `tns_data` (call only when the present flag was set).
///
/// `num_windows` is 8 for EIGHT_SHORT sequences and 1 otherwise; `is_short`
/// selects the bit-width variants.
pub fn parse(tns: &mut Tns, br: &mut BitReader, num_windows: usize, is_short: bool) -> Result<()> {
    let n_filt_bits = if is_short { 1 } else { 2 };
    let length_bits = if is_short { 4 } else { 6 };
    let order_bits = if is_short { 3 } else { 5 };
    let max_order = if is_short {
        TNS_MAX_ORDER_SHORT
    } else {
        TNS_MAX_ORDER_LONG
    };

    for w in 0..num_windows {
        tns.n_filt[w] = br.read_bits(n_filt_bits) as u8;
        if tns.n_filt[w] == 0 {
            continue;
        }
        let coef_res = br.read_bits(1);
        for filt in 0..tns.n_filt[w] as usize {
            tns.length[w][filt] = br.read_bits(length_bits) as u8;
            tns.order[w][filt] = br.read_bits(order_bits) as u8;
            if tns.order[w][filt] as u32 > max_order {
                return Err(CadenceError::CorruptData(format!(
                    "TNS filter order {} exceeds the maximum {max_order}",
                    tns.order[w][filt]
                )));
            }
            if tns.order[w][filt] == 0 {
                continue;
            }
            tns.direction[w][filt] = br.read_bit();
            let coef_compress = br.read_bit();
            let coef_len = coef_res + 3 - u32::from(coef_compress);
            let tmp2_idx = (2 * u32::from(coef_compress) + coef_res) as usize;
            let map: &[f32] = match tmp2_idx {
                0 => &crate::tables::TNS_TMP2_MAP_0_3[..],
                1 => &crate::tables::TNS_TMP2_MAP_0_4[..],
                2 => &crate::tables::TNS_TMP2_MAP_1_3[..],
                _ => &crate::tables::TNS_TMP2_MAP_1_4[..],
            };
            for i in 0..tns.order[w][filt] as usize {
                let index = br.read_bits(coef_len) as usize;
                tns.coef[w][filt][i] = map.get(index).copied().ok_or_else(|| {
                    CadenceError::CorruptData("TNS coefficient index out of range".to_string())
                })?;
            }
        }
    }
    Ok(())
}

/// Converts reflection coefficients to the LPC ( predictor) coefficients
/// used by the synthesis filter (FFmpeg `compute_lpc_coefs`).
fn compute_lpc(coef: &[f32; MAX_ORDER], order: usize, lpc: &mut [f32; MAX_ORDER]) {
    let mut lpc_prev = [0.0f32; MAX_ORDER];
    for i in 0..order {
        let r = -coef[i];
        lpc[i] = r;
        for j in 0..(i + 1) >> 1 {
            let f = lpc_prev[j];
            let b = lpc_prev[i - 1 - j];
            lpc[j] = f + r * b;
            lpc[i - 1 - j] = b + r * f;
        }
        lpc_prev.copy_from_slice(lpc);
    }
}

/// Applies the TNS all-pole (synthesis) filters to spectral coefficients.
///
/// `coeffs` holds `num_windows × 128` coefficients (short windows are
/// 128-wide even within groups). Reference: ISO/IEC 14496-3 §4.6.9.3.
pub fn apply(
    tns: &Tns,
    coeffs: &mut [f32],
    num_windows: usize,
    num_swb: usize,
    swb_offsets: &[u16],
    tns_max_bands: usize,
    max_sfb: usize,
) {
    let mmm = tns_max_bands.min(max_sfb);
    if mmm == 0 {
        return;
    }
    let mut lpc = [0.0f32; MAX_ORDER];

    for w in 0..num_windows {
        let mut bottom = num_swb;
        for filt in 0..tns.n_filt[w] as usize {
            let top = bottom;
            bottom = top.saturating_sub(tns.length[w][filt] as usize);
            let order = tns.order[w][filt] as usize;
            if order == 0 {
                continue;
            }

            compute_lpc(&tns.coef[w][filt], order, &mut lpc);

            let start = swb_offsets[bottom.min(mmm)] as usize;
            let end = swb_offsets[top.min(mmm)] as usize;
            let size = end.saturating_sub(start);
            if size == 0 {
                continue;
            }
            let (inc, mut start) = if tns.direction[w][filt] {
                (-1isize, end as isize - 1)
            } else {
                (1isize, start as isize)
            };
            start += (w * 128) as isize;

            // all-pole filter
            for m in 0..size {
                let idx = start + m as isize * inc;
                for i in 1..=m.min(order) {
                    coeffs[idx as usize] -=
                        coeffs[(idx - (i as isize * inc)) as usize] * lpc[i - 1];
                }
            }
        }
    }
}
