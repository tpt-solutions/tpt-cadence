//! Band bit allocation (`celt/rate.h`, `celt/rate.c`).
//!
//! Converts the total bit budget into per-band pulse counts, fine-bit
//! counts, and priorities, decoding the skip / intensity / dual-stereo
//! signals from the range coder along the way.
//!
//! SOURCE: Xiph.Org libopus 1.5.2, `celt/rate.c` + `celt/rate.h`
//! (BSD-3-Clause). The static mode's pulse cache (`cache_index50`,
//! `cache_bits50`, `cache_caps50`) is extracted in [`super::tables`].

// Lints: this file ports libopus verbatim - verbatim float literals and
// reference loop shapes trigger these lints (see tables.rs).
#![allow(
    clippy::excessive_precision,
    clippy::approx_constant,
    clippy::precedence,
    clippy::needless_range_loop,
    clippy::manual_memcpy,
    clippy::too_many_arguments
)]

use super::math::celt_udiv;
use super::tables::{
    BAND_ALLOCATION, CACHE_BITS50, CACHE_CAPS50, CACHE_INDEX50, EBAND5MS, LOGN400,
};
use crate::range::{RangeDecoder, RangeEncoder};

pub(crate) const NB_EBANDS: usize = 21;
#[allow(dead_code)]
pub(crate) const MAX_PSEUDO: usize = 40;
const LOG_MAX_PSEUDO: usize = 6;
#[allow(dead_code)]
pub(crate) const CELT_MAX_PULSES: usize = 128;
pub(crate) const MAX_FINE_BITS: i32 = 8;
pub(crate) const FINE_OFFSET: i32 = 21;
pub(crate) const QTHETA_OFFSET: i32 = 4;
pub(crate) const QTHETA_OFFSET_TWOPHASE: i32 = 16;
pub(crate) const BITRES: i32 = 3;

static LOG2_FRAC_TABLE: [i32; 24] = [
    0, 8, 13, 16, 19, 21, 23, 24, 26, 27, 28, 29, 30, 31, 32, 32, 33, 34, 34, 35, 36, 36, 37, 37,
];

/// `get_pulses(i)`: the number of pulses represented by the quantizer
/// index `i` (0-7 direct, 8+ spread with doubling levels).
#[inline]
pub(crate) fn get_pulses(i: i32) -> i32 {
    if i < 8 {
        i
    } else {
        (8 + (i & 7)) << ((i >> 3) - 1)
    }
}

#[inline]
fn cache_index(lm1: i32, band: usize) -> usize {
    // Static mode: index rows 0..=4 are used (callers pass LM+1, which can
    // be 0 for deep splits). A -1 entry never occurs for reachable
    // (LM+1, band) pairs; the saturating guard keeps it debuggable.
    let idx = CACHE_INDEX50[lm1 as usize * NB_EBANDS + band];
    debug_assert!(
        idx >= 0,
        "pulse cache index is -1 for lm+1={lm1} band={band}"
    );
    idx.max(0) as usize
}

/// The bits-cache row for a band at level `LM+1` (used by the split
/// threshold test in `quant_partition`).
#[inline]
pub(crate) fn cache_bits_row(band: usize, lm1: i32) -> &'static [u8] {
    &CACHE_BITS50[cache_index(lm1, band)..]
}

/// `bits2pulses`: the highest pulse count whose bit cost fits in `bits`.
pub(crate) fn bits2pulses(band: usize, lm: i32, bits: i32) -> i32 {
    let cache = cache_bits_row(band, lm + 1);
    let mut lo = 0i32;
    let mut hi = cache[0] as i32;
    let bits = bits - 1;
    for _ in 0..LOG_MAX_PSEUDO {
        let mid = (lo + hi + 1) >> 1;
        if cache[mid as usize] as i32 >= bits {
            hi = mid;
        } else {
            lo = mid;
        }
    }
    if bits
        - (if lo == 0 {
            -1
        } else {
            cache[lo as usize] as i32
        })
        <= cache[hi as usize] as i32 - bits
    {
        lo
    } else {
        hi
    }
}

/// `pulses2bits`: the bit cost of `pulses` pulses (cost table + 1).
pub(crate) fn pulses2bits(band: usize, lm: i32, pulses: i32) -> i32 {
    let cache = cache_bits_row(band, lm + 1);
    if pulses == 0 {
        0
    } else {
        cache[pulses as usize] as i32 + 1
    }
}

/// Interpolated allocation outcome for one band set.
pub(crate) struct Allocation {
    pub coded_bands: usize,
    pub balance: i32,
    pub intensity: usize,
    pub dual_stereo: bool,
}

/// Full allocation output: allocation decisions plus the per-band
/// pulses (PVQ bits), fine-energy bits, and fine priorities.
pub(crate) struct AllocationResult {
    pub alloc: Allocation,
    pub pulses: [i32; NB_EBANDS],
    pub ebits: [i32; NB_EBANDS],
    pub fine_priority: [i32; NB_EBANDS],
}

/// `interp_bits2pulses`: bit refinement between two allocation vectors,
/// skip decoding, intensity/dual-stereo coding, and fine-bit assignment.
///
/// Outputs (`pulses`, `ebits`, `fine_priority`) are written into slices of
/// length `NB_EBANDS`.
#[allow(clippy::too_many_arguments)]
fn interp_bits2pulses(
    start: usize,
    end: usize,
    skip_start: i32,
    bits1: &[i32],
    bits2: &[i32],
    thresh: &[i32],
    cap: &[i32],
    mut total: i32,
    skip_rsv: i32,
    intensity_rsv: i32,
    dual_stereo_rsv: i32,
    bits: &mut [i32],
    ebits: &mut [i32],
    fine_priority: &mut [i32],
    c: usize,
    lm: i32,
    dec: &mut RangeDecoder,
) -> crate::Result<Allocation> {
    let c_i = c as i32;
    let alloc_floor = c_i << BITRES;
    let stereo = c > 1;
    let logm = lm << BITRES;

    // Bisection over the interpolation fraction.
    let mut lo = 0i32;
    let mut hi = 1 << 6; // ALLOC_STEPS
    for _ in 0..6 {
        let mid = (lo + hi) >> 1;
        let mut psum = 0i32;
        let mut done = false;
        let mut j = end;
        while j > start {
            j -= 1;
            let tmp = bits1[j] + (mid * bits2[j] >> 6);
            if tmp >= thresh[j] || done {
                done = true;
                psum += tmp.min(cap[j]);
            } else if tmp >= alloc_floor {
                psum += alloc_floor;
            }
        }
        if psum > total {
            hi = mid;
        } else {
            lo = mid;
        }
    }
    let mut psum = 0i32;
    let mut done = false;
    let mut j = end;
    while j > start {
        j -= 1;
        let mut tmp = bits1[j] + (lo * bits2[j] >> 6);
        if tmp < thresh[j] && !done {
            tmp = if tmp >= alloc_floor { alloc_floor } else { 0 };
        } else {
            done = true;
        }
        tmp = tmp.min(cap[j]);
        bits[j] = tmp;
        psum += tmp;
    }

    // Decide which bands to skip, working backwards from the end.
    let mut coded_bands = end;
    let mut intensity_rsv = intensity_rsv;
    let mut dual_stereo_rsv = dual_stereo_rsv;

    let mut dual_stereo;
    loop {
        let j = coded_bands - 1;
        // Never skip the first band, nor a band boosted by dynalloc.
        if (j as i32) <= skip_start {
            // Give the bit we reserved to end skipping back.
            total += skip_rsv;
            break;
        }
        let left = total - psum;
        let percoeff = celt_udiv(
            left as u32,
            (EBAND5MS[coded_bands] - EBAND5MS[start]) as u32,
        ) as i32;
        let left = left - (EBAND5MS[coded_bands] - EBAND5MS[start]) as i32 * percoeff;
        let rem = (left - (EBAND5MS[j] - EBAND5MS[start]) as i32).max(0);
        let band_width = EBAND5MS[coded_bands] - EBAND5MS[j];
        let mut band_bits = bits[j] + percoeff * band_width as i32 + rem;
        // Only code a skip decision above the threshold for this band.
        if band_bits >= thresh[j].max(alloc_floor + (1 << BITRES)) {
            if dec.decode_bit_logp(1)? {
                break;
            }
            // We used a bit to skip this band.
            psum += 1 << BITRES;
            band_bits -= 1 << BITRES;
        }
        // Reclaim the bits originally allocated to this band.
        psum -= bits[j] + intensity_rsv;
        if intensity_rsv > 0 {
            intensity_rsv = LOG2_FRAC_TABLE[j - start];
        }
        psum += intensity_rsv;
        if band_bits >= alloc_floor {
            // Enough for a fine energy bit per channel.
            psum += alloc_floor;
            bits[j] = alloc_floor;
        } else {
            bits[j] = 0;
        }
        coded_bands -= 1;
    }
    debug_assert!(coded_bands > start);

    // Code the intensity and dual stereo parameters.
    let intensity: usize = if intensity_rsv > 0 {
        (start as i64 + dec.decode_uint((coded_bands + 1 - start) as u32)? as i64) as usize
    } else {
        0
    };
    if intensity <= start {
        total += dual_stereo_rsv;
        dual_stereo_rsv = 0;
    }
    dual_stereo = false;
    if dual_stereo_rsv > 0 {
        dual_stereo = dec.decode_bit_logp(1)?;
    }

    // Allocate the remaining bits.
    let mut left = total - psum;
    let percoeff = celt_udiv(
        left as u32,
        (EBAND5MS[coded_bands] - EBAND5MS[start]) as u32,
    ) as i32;
    left -= (EBAND5MS[coded_bands] - EBAND5MS[start]) as i32 * percoeff;
    for j in start..coded_bands {
        bits[j] += percoeff * (EBAND5MS[j + 1] - EBAND5MS[j]) as i32;
    }
    for j in start..coded_bands {
        let tmp = left.min((EBAND5MS[j + 1] - EBAND5MS[j]) as i32);
        bits[j] += tmp;
        left -= tmp;
    }

    // Fine energy assignment with rebalancing.
    let mut balance = 0i32;
    let mut excess;
    for j in start..coded_bands {
        debug_assert!(bits[j] >= 0);
        let n0 = (EBAND5MS[j + 1] - EBAND5MS[j]) as i32;
        let n = n0 << lm;
        let bit = bits[j] + balance;

        if n > 1 {
            excess = (bit - cap[j]).max(0);
            bits[j] = bit - excess;

            // Compensate for the extra DoF in stereo.
            let den = c_i * n + i32::from(c == 2 && n > 2 && !dual_stereo && j < intensity);

            let nclogn = den * (LOGN400[j] as i32 + logm);

            // Offset for the number of fine bits.
            let mut offset = (nclogn >> 1) - den * FINE_OFFSET;

            // N=2 is the only point that doesn't match the curve.
            if n == 2 {
                offset += den << BITRES >> 2;
            }
            // Changed offset for the 2nd/3rd fine energy bit.
            if bits[j] + offset < den * 2 << BITRES {
                offset += nclogn >> 2;
            } else if bits[j] + offset < den * 3 << BITRES {
                offset += nclogn >> 3;
            }

            // Divide with rounding.
            ebits[j] = (bits[j] + offset + (den << (BITRES - 1))).max(0);
            ebits[j] = celt_udiv(ebits[j] as u32, den as u32) as i32 >> BITRES;

            // Make sure not to bust.
            if c_i * ebits[j] > bits[j] >> BITRES {
                ebits[j] = bits[j] >> (u32::from(stereo) + BITRES as u32);
            }

            // More than that is useless.
            ebits[j] = ebits[j].min(MAX_FINE_BITS);

            // Candidate for the final fine energy pass?
            fine_priority[j] = i32::from(ebits[j] * (den << BITRES) >= bits[j] + offset);

            // Remove the allocated fine bits; the rest are assigned to PVQ.
            bits[j] -= c_i * ebits[j] << BITRES;
        } else {
            // For N=1, all bits go to fine energy except a sign bit.
            excess = 0i32.max(bit - (c_i << BITRES));
            bits[j] = bit - excess;
            ebits[j] = 0;
            fine_priority[j] = 1;
        }

        // Fine energy can't take advantage of quant_all_bands()'s
        // re-balancing, so do it here.
        if excess > 0 {
            let extra_fine =
                (excess >> (u32::from(stereo) + BITRES as u32)).min(MAX_FINE_BITS - ebits[j]);
            ebits[j] += extra_fine;
            let extra_bits = extra_fine * c_i << BITRES;
            fine_priority[j] = i32::from(extra_bits >= excess - balance);
            excess -= extra_bits;
        }
        balance = excess;
    }
    // The skipped bands use all their bits for fine energy.
    for j in coded_bands..end {
        ebits[j] = bits[j] >> (u32::from(stereo) + BITRES as u32);
        debug_assert_eq!((c_i * ebits[j]) << BITRES, bits[j]);
        bits[j] = 0;
        fine_priority[j] = i32::from(ebits[j] < 1);
    }

    Ok(Allocation {
        coded_bands,
        balance,
        intensity,
        dual_stereo,
    })
}

/// Encode-side counterpart of [`interp_bits2pulses`]. Everything here is
/// deterministic given the same inputs *except* the three points where the
/// decoder consults the bitstream (per-band skip bit, intensity index,
/// dual-stereo bit) — those aren't normative encoder behavior (RFC 6716
/// only specifies the decoder), so this uses the simplest defensible
/// policy for a first working encoder: never skip a band while there's a
/// real skip decision to make (matches `dec.decode_bit_logp(1)? == true`
/// unconditionally), and never use intensity/dual-stereo coupling
/// (`intensity == start`, `dual_stereo == false`). Skipping bands *can*
/// still happen mechanically when a band's bit budget doesn't clear
/// `thresh[j]` at all (no bit is spent in that case either direction, so
/// encoder and decoder agree automatically). A smarter policy (actually
/// choosing to trade off bands/intensity coupling for quality) is future
/// work — see `todo.md`.
///
/// Verified bit-for-bit against [`interp_bits2pulses`] (same `pulses`,
/// `ebits`, `fine_priority`, and [`Allocation`] fields) in `tests` below.
#[allow(clippy::too_many_arguments, dead_code)]
fn interp_bits2pulses_encode(
    start: usize,
    end: usize,
    skip_start: i32,
    bits1: &[i32],
    bits2: &[i32],
    thresh: &[i32],
    cap: &[i32],
    mut total: i32,
    skip_rsv: i32,
    intensity_rsv: i32,
    dual_stereo_rsv: i32,
    bits: &mut [i32],
    ebits: &mut [i32],
    fine_priority: &mut [i32],
    c: usize,
    lm: i32,
    enc: &mut RangeEncoder,
) -> Allocation {
    let c_i = c as i32;
    let alloc_floor = c_i << BITRES;
    let stereo = c > 1;
    let logm = lm << BITRES;

    // Bisection over the interpolation fraction (identical to the decoder
    // — no bitstream I/O).
    let mut lo = 0i32;
    let mut hi = 1 << 6; // ALLOC_STEPS
    for _ in 0..6 {
        let mid = (lo + hi) >> 1;
        let mut psum = 0i32;
        let mut done = false;
        let mut j = end;
        while j > start {
            j -= 1;
            let tmp = bits1[j] + (mid * bits2[j] >> 6);
            if tmp >= thresh[j] || done {
                done = true;
                psum += tmp.min(cap[j]);
            } else if tmp >= alloc_floor {
                psum += alloc_floor;
            }
        }
        if psum > total {
            hi = mid;
        } else {
            lo = mid;
        }
    }
    let mut psum = 0i32;
    let mut done = false;
    let mut j = end;
    while j > start {
        j -= 1;
        let mut tmp = bits1[j] + (lo * bits2[j] >> 6);
        if tmp < thresh[j] && !done {
            tmp = if tmp >= alloc_floor { alloc_floor } else { 0 };
        } else {
            done = true;
        }
        tmp = tmp.min(cap[j]);
        bits[j] = tmp;
        psum += tmp;
    }

    // Decide which bands to skip, working backwards from the end.
    let mut coded_bands = end;
    let mut intensity_rsv = intensity_rsv;
    let mut dual_stereo_rsv = dual_stereo_rsv;

    loop {
        let j = coded_bands - 1;
        // Never skip the first band, nor a band boosted by dynalloc.
        if (j as i32) <= skip_start {
            // Give the bit we reserved to end skipping back.
            total += skip_rsv;
            break;
        }
        let left = total - psum;
        let percoeff = celt_udiv(
            left as u32,
            (EBAND5MS[coded_bands] - EBAND5MS[start]) as u32,
        ) as i32;
        let left = left - (EBAND5MS[coded_bands] - EBAND5MS[start]) as i32 * percoeff;
        let rem = (left - (EBAND5MS[j] - EBAND5MS[start]) as i32).max(0);
        let band_width = EBAND5MS[coded_bands] - EBAND5MS[j];
        let band_bits = bits[j] + percoeff * band_width as i32 + rem;
        // Only code a skip decision above the threshold for this band.
        if band_bits >= thresh[j].max(alloc_floor + (1 << BITRES)) {
            // Policy: always stop skipping here (see doc comment above).
            enc.encode_bit_logp(true, 1);
            break;
        }
        // Reclaim the bits originally allocated to this band.
        psum -= bits[j] + intensity_rsv;
        if intensity_rsv > 0 {
            intensity_rsv = LOG2_FRAC_TABLE[j - start];
        }
        psum += intensity_rsv;
        if band_bits >= alloc_floor {
            // Enough for a fine energy bit per channel.
            psum += alloc_floor;
            bits[j] = alloc_floor;
        } else {
            bits[j] = 0;
        }
        coded_bands -= 1;
    }
    debug_assert!(coded_bands > start);

    // Code the intensity and dual stereo parameters. Policy: no
    // intensity/dual-stereo coupling (see doc comment above).
    let intensity: usize = if intensity_rsv > 0 {
        enc.encode_uint(0, (coded_bands + 1 - start) as u32);
        start
    } else {
        0
    };
    if intensity <= start {
        total += dual_stereo_rsv;
        dual_stereo_rsv = 0;
    }
    let dual_stereo = false;
    if dual_stereo_rsv > 0 {
        enc.encode_bit_logp(false, 1);
    }

    // Allocate the remaining bits.
    let mut left = total - psum;
    let percoeff = celt_udiv(
        left as u32,
        (EBAND5MS[coded_bands] - EBAND5MS[start]) as u32,
    ) as i32;
    left -= (EBAND5MS[coded_bands] - EBAND5MS[start]) as i32 * percoeff;
    for j in start..coded_bands {
        bits[j] += percoeff * (EBAND5MS[j + 1] - EBAND5MS[j]) as i32;
    }
    for j in start..coded_bands {
        let tmp = left.min((EBAND5MS[j + 1] - EBAND5MS[j]) as i32);
        bits[j] += tmp;
        left -= tmp;
    }

    // Fine energy assignment with rebalancing.
    let mut balance = 0i32;
    let mut excess;
    for j in start..coded_bands {
        debug_assert!(bits[j] >= 0);
        let n0 = (EBAND5MS[j + 1] - EBAND5MS[j]) as i32;
        let n = n0 << lm;
        let bit = bits[j] + balance;

        if n > 1 {
            excess = (bit - cap[j]).max(0);
            bits[j] = bit - excess;

            // Compensate for the extra DoF in stereo.
            let den = c_i * n + i32::from(c == 2 && n > 2 && !dual_stereo && j < intensity);

            let nclogn = den * (LOGN400[j] as i32 + logm);

            // Offset for the number of fine bits.
            let mut offset = (nclogn >> 1) - den * FINE_OFFSET;

            // N=2 is the only point that doesn't match the curve.
            if n == 2 {
                offset += den << BITRES >> 2;
            }
            // Changed offset for the 2nd/3rd fine energy bit.
            if bits[j] + offset < den * 2 << BITRES {
                offset += nclogn >> 2;
            } else if bits[j] + offset < den * 3 << BITRES {
                offset += nclogn >> 3;
            }

            // Divide with rounding.
            ebits[j] = (bits[j] + offset + (den << (BITRES - 1))).max(0);
            ebits[j] = celt_udiv(ebits[j] as u32, den as u32) as i32 >> BITRES;

            // Make sure not to bust.
            if c_i * ebits[j] > bits[j] >> BITRES {
                ebits[j] = bits[j] >> (u32::from(stereo) + BITRES as u32);
            }

            // More than that is useless.
            ebits[j] = ebits[j].min(MAX_FINE_BITS);

            // Candidate for the final fine energy pass?
            fine_priority[j] = i32::from(ebits[j] * (den << BITRES) >= bits[j] + offset);

            // Remove the allocated fine bits; the rest are assigned to PVQ.
            bits[j] -= c_i * ebits[j] << BITRES;
        } else {
            // For N=1, all bits go to fine energy except a sign bit.
            excess = 0i32.max(bit - (c_i << BITRES));
            bits[j] = bit - excess;
            ebits[j] = 0;
            fine_priority[j] = 1;
        }

        // Fine energy can't take advantage of quant_all_bands()'s
        // re-balancing, so do it here.
        if excess > 0 {
            let extra_fine =
                (excess >> (u32::from(stereo) + BITRES as u32)).min(MAX_FINE_BITS - ebits[j]);
            ebits[j] += extra_fine;
            let extra_bits = extra_fine * c_i << BITRES;
            fine_priority[j] = i32::from(extra_bits >= excess - balance);
            excess -= extra_bits;
        }
        balance = excess;
    }
    // The skipped bands use all their bits for fine energy.
    for j in coded_bands..end {
        ebits[j] = bits[j] >> (u32::from(stereo) + BITRES as u32);
        debug_assert_eq!((c_i * ebits[j]) << BITRES, bits[j]);
        bits[j] = 0;
        fine_priority[j] = i32::from(ebits[j] < 1);
    }

    Allocation {
        coded_bands,
        balance,
        intensity,
        dual_stereo,
    }
}

/// Shared, deterministic setup for [`compute_allocation`] and
/// [`compute_allocation_encode`]: everything up to (but not including)
/// [`interp_bits2pulses`]'s skip/intensity/dual-stereo bitstream I/O. Reads
/// no bits and writes none, so encoder and decoder call it identically.
#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn compute_bits1_bits2(
    start: usize,
    end: usize,
    offsets: &[i32],
    cap: &[i32],
    alloc_trim: i32,
    total_bits: i32,
    lm: i32,
    c: usize,
) -> (
    i32,              // total (after skip/intensity/dual-stereo reservations)
    i32,              // skip_start
    i32,              // skip_rsv
    i32,              // intensity_rsv
    i32,              // dual_stereo_rsv
    [i32; NB_EBANDS], // bits1
    [i32; NB_EBANDS], // bits2
    [i32; NB_EBANDS], // thresh
) {
    let mut total = total_bits.max(0);
    let skip_start = start as i32;
    // Reserve a bit to signal the end of manually skipped bands.
    let skip_rsv = if total >= 1 << BITRES { 1 << BITRES } else { 0 };
    total -= skip_rsv;
    // Reserve bits for the intensity and dual stereo parameters.
    let mut intensity_rsv = 0i32;
    let mut dual_stereo_rsv = 0i32;
    if c == 2 {
        intensity_rsv = LOG2_FRAC_TABLE[end - start];
        if intensity_rsv > total {
            intensity_rsv = 0;
        } else {
            total -= intensity_rsv;
            dual_stereo_rsv = if total >= 1 << BITRES { 1 << BITRES } else { 0 };
            total -= dual_stereo_rsv;
        }
    }

    let mut bits1 = [0i32; NB_EBANDS];
    let mut bits2 = [0i32; NB_EBANDS];
    let mut thresh = [0i32; NB_EBANDS];
    let mut trim_offset = [0i32; NB_EBANDS];

    for j in start..end {
        // Below this threshold, we're sure not to allocate any PVQ bits.
        thresh[j] = ((c as i32) << BITRES)
            .max((3 * ((EBAND5MS[j + 1] - EBAND5MS[j]) as i32) << lm << BITRES) >> 4);
        // Tilt of the allocation curve.
        trim_offset[j] = (c as i32)
            * (EBAND5MS[j + 1] - EBAND5MS[j]) as i32
            * (alloc_trim - 5 - lm)
            * (end as i32 - j as i32 - 1)
            * (1 << (lm as u32 + BITRES as u32))
            >> 6;
        // Less resolution for single-coefficient bands.
        if ((EBAND5MS[j + 1] - EBAND5MS[j]) as i32) << lm == 1 {
            trim_offset[j] -= (c as i32) << BITRES;
        }
    }

    // Search for the two surrounding allocation vectors.
    let mut lo = 1i32;
    let mut hi = BAND_ALLOCATION.len() as i32 / NB_EBANDS as i32 - 1;
    loop {
        let mut done = false;
        let mut psum = 0i32;
        let mid = (lo + hi) >> 1;
        let mut j = end;
        while j > start {
            j -= 1;
            let n = EBAND5MS[j + 1] - EBAND5MS[j];
            let mut bitsj = ((c as i32)
                * (n as i32)
                * (BAND_ALLOCATION[(mid * NB_EBANDS as i32 + j as i32) as usize] as i32))
                << lm
                >> 2;
            if bitsj > 0 {
                bitsj = 0.max(bitsj + trim_offset[j]);
            }
            bitsj += offsets[j];
            if bitsj >= thresh[j] || done {
                done = true;
                psum += bitsj.min(cap[j]);
            } else if bitsj >= (c as i32) << BITRES {
                psum += (c as i32) << BITRES;
            }
        }
        if psum > total {
            hi = mid - 1;
        } else {
            lo = mid + 1;
        }
        if lo > hi {
            break;
        }
    }
    hi = lo;
    lo -= 1;
    let mut skip_start = skip_start;
    for j in start..end {
        let n = EBAND5MS[j + 1] - EBAND5MS[j];
        let mut bits1j = ((c as i32)
            * (n as i32)
            * (BAND_ALLOCATION[(lo * NB_EBANDS as i32 + j as i32) as usize] as i32))
            << lm
            >> 2;
        let nb_alloc_vectors = BAND_ALLOCATION.len() as i32 / NB_EBANDS as i32;
        let mut bits2j = if hi >= nb_alloc_vectors {
            cap[j]
        } else {
            ((c as i32)
                * (n as i32)
                * (BAND_ALLOCATION[(hi * NB_EBANDS as i32 + j as i32) as usize] as i32))
                << lm
                >> 2
        };
        if bits1j > 0 {
            bits1j = 0.max(bits1j + trim_offset[j]);
        }
        if bits2j > 0 {
            bits2j = 0.max(bits2j + trim_offset[j]);
        }
        if lo > 0 {
            bits1j += offsets[j];
        }
        bits2j += offsets[j];
        if offsets[j] > 0 {
            skip_start = j as i32;
        }
        bits2j = 0.max(bits2j - bits1j);
        bits1[j] = bits1j;
        bits2[j] = bits2j;
    }

    (
        total,
        skip_start,
        skip_rsv,
        intensity_rsv,
        dual_stereo_rsv,
        bits1,
        bits2,
        thresh,
    )
}

/// `clt_compute_allocation`: computes the per-band pulse allocation.
///
/// Returns the per-band pulses (PVQ bits), fine-energy bits, and fine
/// priorities plus the interpolation results.
#[allow(clippy::too_many_arguments)]
pub(crate) fn compute_allocation(
    start: usize,
    end: usize,
    offsets: &[i32],
    cap: &[i32],
    alloc_trim: i32,
    total_bits: i32,
    lm: i32,
    c: usize,
    dec: &mut RangeDecoder,
) -> crate::Result<AllocationResult> {
    let (total, skip_start, skip_rsv, intensity_rsv, dual_stereo_rsv, bits1, bits2, thresh) =
        compute_bits1_bits2(start, end, offsets, cap, alloc_trim, total_bits, lm, c);

    let mut pulses = [0i32; NB_EBANDS];
    let mut ebits = [0i32; NB_EBANDS];
    let mut fine_priority = [0i32; NB_EBANDS];
    let alloc = interp_bits2pulses(
        start,
        end,
        skip_start,
        &bits1,
        &bits2,
        &thresh,
        cap,
        total,
        skip_rsv,
        intensity_rsv,
        dual_stereo_rsv,
        &mut pulses,
        &mut ebits,
        &mut fine_priority,
        c,
        lm,
        dec,
    )?;
    Ok(AllocationResult {
        alloc,
        pulses,
        ebits,
        fine_priority,
    })
}

/// Encode-side counterpart of [`compute_allocation`]. See
/// [`interp_bits2pulses_encode`] for the encoder-policy notes (skip/
/// intensity/dual-stereo bits aren't normative — RFC 6716 only specifies
/// the decoder).
#[allow(clippy::too_many_arguments, dead_code)]
pub(crate) fn compute_allocation_encode(
    start: usize,
    end: usize,
    offsets: &[i32],
    cap: &[i32],
    alloc_trim: i32,
    total_bits: i32,
    lm: i32,
    c: usize,
    enc: &mut RangeEncoder,
) -> AllocationResult {
    let (total, skip_start, skip_rsv, intensity_rsv, dual_stereo_rsv, bits1, bits2, thresh) =
        compute_bits1_bits2(start, end, offsets, cap, alloc_trim, total_bits, lm, c);

    let mut pulses = [0i32; NB_EBANDS];
    let mut ebits = [0i32; NB_EBANDS];
    let mut fine_priority = [0i32; NB_EBANDS];
    let alloc = interp_bits2pulses_encode(
        start,
        end,
        skip_start,
        &bits1,
        &bits2,
        &thresh,
        cap,
        total,
        skip_rsv,
        intensity_rsv,
        dual_stereo_rsv,
        &mut pulses,
        &mut ebits,
        &mut fine_priority,
        c,
        lm,
        enc,
    );
    AllocationResult {
        alloc,
        pulses,
        ebits,
        fine_priority,
    }
}

/// `init_caps` (celt.c): the maximum useful bits per band at this LM/C.
pub(crate) fn init_caps(lm: usize, c: usize) -> [i32; NB_EBANDS] {
    let mut cap = [0i32; NB_EBANDS];
    for i in 0..NB_EBANDS {
        let n = (EBAND5MS[i + 1] - EBAND5MS[i]) << lm;
        cap[i] =
            ((CACHE_CAPS50[NB_EBANDS * (2 * lm + c - 1) + i] as i32 + 64) * c as i32 * n as i32)
                >> 2;
    }
    cap
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_pulses_matches_reference() {
        assert_eq!(get_pulses(0), 0);
        assert_eq!(get_pulses(7), 7);
        assert_eq!(get_pulses(8), 8);
        assert_eq!(get_pulses(9), 9);
        assert_eq!(get_pulses(15), 15);
        // i=8..15: (8 + (i&7)) << 0
        assert_eq!(get_pulses(16), 16);
        assert_eq!(get_pulses(17), 18);
        assert_eq!(get_pulses(23), 30);
        assert_eq!(get_pulses(24), 32);
        assert_eq!(get_pulses(31), 60);
        assert_eq!(get_pulses(32), 64);
    }

    #[test]
    fn bits2pulses_is_monotone_and_consistent() {
        // Every band's pulse choice must be monotone in bits.
        for lm in 0..4i32 {
            for band in 0..NB_EBANDS {
                let mut prev = 0;
                for bits in 0..400i32 {
                    let q = bits2pulses(band, lm, bits);
                    assert!(q >= prev, "band={band} lm={lm}");
                    assert!(q <= CELT_MAX_PULSES as i32);
                    prev = q;
                }
            }
        }
    }

    #[test]
    fn caps_are_sane() {
        for lm in 0..4usize {
            for c in 1..=2usize {
                let caps = init_caps(lm, c);
                for (band, &cv) in caps.iter().enumerate() {
                    assert!(cv >= 0, "band={band} lm={lm} c={c}");
                }
            }
        }
    }

    /// `compute_allocation_encode` must produce bits that
    /// `compute_allocation` (the trusted, RFC-conformance-tested decoder
    /// path) decodes back to the exact same allocation the encoder
    /// computed — same pulses/ebits/fine_priority per band, same
    /// coded_bands/balance/intensity/dual_stereo.
    #[test]
    fn compute_allocation_encode_round_trips_through_decoder() {
        use crate::range::RangeEncoder;

        for &c in &[1usize, 2usize] {
            for lm in 0..4i32 {
                for &total_bits in &[400i32, 1600, 6400, 16000] {
                    let offsets = [0i32; NB_EBANDS];
                    let cap = init_caps(lm as usize, c);
                    let alloc_trim = 5i32;

                    let mut enc = RangeEncoder::new();
                    let enc_result = compute_allocation_encode(
                        0, NB_EBANDS, &offsets, &cap, alloc_trim, total_bits, lm, c, &mut enc,
                    );
                    let frame = enc.done();

                    let mut dec = RangeDecoder::new(&frame);
                    let dec_result = compute_allocation(
                        0, NB_EBANDS, &offsets, &cap, alloc_trim, total_bits, lm, c, &mut dec,
                    )
                    .unwrap();

                    assert_eq!(
                        enc_result.pulses, dec_result.pulses,
                        "c={c} lm={lm} total_bits={total_bits}"
                    );
                    assert_eq!(
                        enc_result.ebits, dec_result.ebits,
                        "c={c} lm={lm} total_bits={total_bits}"
                    );
                    assert_eq!(
                        enc_result.fine_priority, dec_result.fine_priority,
                        "c={c} lm={lm} total_bits={total_bits}"
                    );
                    assert_eq!(
                        enc_result.alloc.coded_bands, dec_result.alloc.coded_bands,
                        "c={c} lm={lm} total_bits={total_bits}"
                    );
                    assert_eq!(
                        enc_result.alloc.balance, dec_result.alloc.balance,
                        "c={c} lm={lm} total_bits={total_bits}"
                    );
                    assert_eq!(
                        enc_result.alloc.intensity, dec_result.alloc.intensity,
                        "c={c} lm={lm} total_bits={total_bits}"
                    );
                    assert_eq!(
                        enc_result.alloc.dual_stereo, dec_result.alloc.dual_stereo,
                        "c={c} lm={lm} total_bits={total_bits}"
                    );
                }
            }
        }
    }
}
