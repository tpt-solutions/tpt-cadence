//! Joint-stereo processing: mid/side and intensity stereo, applied to the
//! requantized (but still scalefactor-band ordered) granule data.

use crate::header::FrameHeader;
use crate::scalefac::ldexp_q2;
use crate::sideinfo::GranuleInfo;
use crate::tables::PAN;

/// Left/right layout of the granule scratch: ch0 samples `[0..576]`, ch1
/// `[576..1152]` — the stereo helpers address both through `left`.
#[inline(always)]
fn midside_stereo(left: &mut [f32], n: usize) {
    for i in 0..n {
        let a = left[i];
        let b = left[576 + i];
        left[i] = a + b;
        left[576 + i] = a - b;
    }
}

#[inline(always)]
fn intensity_band(left: &mut [f32], n: usize, kl: f32, kr: f32) {
    for i in 0..n {
        let l = left[i];
        left[576 + i] = l * kr;
        left[i] = l * kl;
    }
}

/// Finds the highest scalefactor band per short-window block that carries
/// signal in the right channel (coded order), used to decide where
/// intensity stereo stops applying.
fn stereo_top_band(right: &[f32], sfb: &[u8], nbands: usize, max_band: &mut [i32; 3]) {
    *max_band = [-1, -1, -1];
    let mut off = 0usize;
    for (i, &w) in sfb.iter().enumerate().take(nbands) {
        let mut k = 0;
        while k < w as usize {
            if right[off + k] != 0.0 || right[off + k + 1] != 0.0 {
                max_band[i % 3] = i as i32;
                break;
            }
            k += 2;
        }
        off += w as usize;
    }
}

/// Applies intensity/mid-side per scalefactor band (coded order).
fn stereo_process(
    left: &mut [f32],
    ist_pos: &[u8; 39],
    sfb: &[u8],
    hdr: &FrameHeader,
    max_band: &[i32; 3],
    mpeg2_sh: u32,
) {
    let max_pos = if hdr.mpeg1 { 7 } else { 64 };
    let mut off = 0usize;
    for (i, &w) in sfb.iter().enumerate() {
        if w == 0 {
            break;
        }
        let ipos = ist_pos[i] as u32;
        if i as i32 > max_band[i % 3] && ipos < max_pos {
            // Intensity band; mid/side gain (√2) folds in when both are on.
            let s = if hdr.ms_stereo {
                std::f32::consts::SQRT_2
            } else {
                1.0
            };
            let (kl, kr) = if hdr.mpeg1 {
                (PAN[2 * ipos as usize], PAN[2 * ipos as usize + 1])
            } else {
                let mut kl = 1.0f32;
                let mut kr = ldexp_q2(1.0, (((ipos + 1) >> 1) << mpeg2_sh) as i32);
                if ipos & 1 != 0 {
                    std::mem::swap(&mut kl, &mut kr);
                }
                (kl, kr)
            };
            intensity_band(&mut left[off..], w as usize, kl * s, kr * s);
        } else if hdr.ms_stereo {
            midside_stereo(&mut left[off..], w as usize);
        }
        off += w as usize;
    }
}

/// Runs joint-stereo processing for one granule. `gr` is this granule's
/// channel-info slice; `ist_pos` is the right channel's position array
/// (carrying the -1 sentinels from scalefactor reading).
pub(crate) fn intensity_stereo(
    granule: &mut [f32], // 1152 floats: left | right
    ist_pos: &[u8; 39],
    gr: &[GranuleInfo],
    hdr: &FrameHeader,
) {
    let n_sfb = gr[0].n_long_sfb as usize + gr[0].n_short_sfb as usize;
    let max_blocks = if gr[0].n_short_sfb != 0 { 3 } else { 1 };
    let mut max_band = [0i32; 3];
    stereo_top_band(&granule[576..], gr[0].sfbtab, n_sfb, &mut max_band);
    if gr[0].n_long_sfb != 0 {
        // Long blocks share one boundary across all three "blocks".
        let m = max_band[0].max(max_band[1]).max(max_band[2]);
        max_band = [m, m, m];
    }
    let default_pos = if hdr.mpeg1 { 3 } else { 0 };
    let mut pos = *ist_pos;
    for (i, &band) in max_band.iter().enumerate().take(max_blocks) {
        let itop = n_sfb - max_blocks + i;
        let prev = itop - max_blocks;
        pos[itop] = if band >= prev as i32 {
            default_pos
        } else {
            ist_pos[prev]
        };
    }
    stereo_process(
        granule,
        &pos,
        gr[0].sfbtab,
        hdr,
        &max_band,
        gr[1].scalefac_compress as u32 & 1,
    );
}

/// Plain mid/side for fully mid/side-coded frames (no intensity).
pub(crate) fn midside(granule: &mut [f32]) {
    midside_stereo(granule, 576);
}
