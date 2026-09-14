//! Mid/Side and intensity stereo processing (ISO/IEC 14496-3 §4.6.8.1.3).

// Index-loop style matches the reference implementations of these numeric
// kernels; the argument counts mirror the ISO tool grouping.
#![allow(clippy::too_many_arguments, clippy::needless_range_loop)]

/// Applies M/S stereo (butterfly) to band `sfb` ranges marked in `ms_mask`.
///
/// Only bands whose channel codebooks are both below the noise/intensity
/// range participate (reference: `apply_mid_side_stereo`).
pub fn apply_mid_side(
    mut ch0: &mut [f32],
    mut ch1: &mut [f32],
    band_type0: &[u8],
    band_type1: &[u8],
    ms_mask: &[bool],
    num_window_groups: usize,
    group_len: &[usize; 8],
    max_sfb: usize,
    swb_offsets: &[u16],
) {
    const NOISE_BT: u8 = 13;
    for g in 0..num_window_groups {
        for sfb in 0..max_sfb {
            let idx = g * max_sfb + sfb;
            if !ms_mask[idx] || band_type0[idx] >= NOISE_BT || band_type1[idx] >= NOISE_BT {
                continue;
            }
            let start = swb_offsets[sfb] as usize;
            let end = swb_offsets[sfb + 1] as usize;
            for group in 0..group_len[g] {
                let o = group * 128 + start;
                for k in start..end {
                    let i = o + k - start;
                    let m = ch0[i];
                    let s = ch1[i];
                    ch0[i] = m + s;
                    ch1[i] = m - s;
                }
            }
        }
        let advance = group_len[g] * 128;
        ch0 = &mut ch0[advance..];
        ch1 = &mut ch1[advance..];
    }
}

/// Applies intensity stereo: the right channel is replaced by a scaled copy
/// of the left channel in every intensity-coded band.
///
/// `sf_right` holds the right channel's (already computed) intensity scale
/// factors; `c` per band is `±1` from the band type and M/S mask.
pub fn apply_intensity(
    mut ch0: &mut [f32],
    mut ch1: &mut [f32],
    band_type1: &[u8],
    gains: &[f32],
    num_window_groups: usize,
    group_len: &[usize; 8],
    max_sfb: usize,
    swb_offsets: &[u16],
) {
    const INTENSITY_BT: u8 = 15;
    const INTENSITY_BT2: u8 = 14;
    for g in 0..num_window_groups {
        for sfb in 0..max_sfb {
            let idx = g * max_sfb + sfb;
            if band_type1[idx] != INTENSITY_BT && band_type1[idx] != INTENSITY_BT2 {
                continue;
            }
            let start = swb_offsets[sfb] as usize;
            let end = swb_offsets[sfb + 1] as usize;
            for group in 0..group_len[g] {
                let o = group * 128 + start;
                for k in start..end {
                    let i = o + k - start;
                    ch1[i] = ch0[i] * gains[idx];
                }
            }
        }
        let advance = group_len[g] * 128;
        ch0 = &mut ch0[advance..];
        ch1 = &mut ch1[advance..];
    }
}
