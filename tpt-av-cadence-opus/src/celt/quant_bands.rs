//! Coarse/fine band-energy unquantization (`celt/quant_bands.c`).
//!
//! Decodes the band log-energies: Laplace-coded coarse values with
//! prediction across frames, then uniformly-coded fine bits, then the
//! final priority-ordered extra fine bits.
//!
//! SOURCE: Xiph.Org libopus 1.5.2, `celt/quant_bands.c` (BSD-3-Clause).
//! Float build only: the Q-formats below collapse to plain `f32` math.

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

use super::laplace;
use super::rate::{MAX_FINE_BITS, NB_EBANDS};
use crate::range::RangeDecoder;

/// Mean energy in each band (Q4, converted back to float).
static E_MEANS: [f32; 25] = [
    6.437_500, 6.250_000, 5.750_000, 5.312_500, 5.062_500, 4.812_500, 4.500_000, 4.375_000,
    4.875_000, 4.687_500, 4.562_500, 4.437_500, 4.875_000, 4.625_000, 4.312_500, 4.500_000,
    4.375_000, 4.625_000, 4.750_000, 4.437_500, 3.750_000, 3.750_000, 3.750_000, 3.750_000,
    3.750_000,
];

/// Prediction coefficients: 0.9, 0.8, 0.65, 0.5.
static PRED_COEF: [f32; 4] = [
    29440.0 / 32768.0,
    26112.0 / 32768.0,
    21248.0 / 32768.0,
    16384.0 / 32768.0,
];
static BETA_COEF: [f32; 4] = [
    30147.0 / 32768.0,
    22282.0 / 32768.0,
    12124.0 / 32768.0,
    6554.0 / 32768.0,
];
static BETA_INTRA: f32 = 4915.0 / 32768.0;

/// Laplace model parameters (probability of 0, decay) per LM/intra/band.
static E_PROB_MODEL: [[[u8; 42]; 2]; 4] = [
    // 120 sample frames.
    [
        // Inter
        [
            72, 127, 65, 129, 66, 128, 65, 128, 64, 128, 62, 128, 64, 128, 64, 128, 92, 78, 92, 79,
            92, 78, 90, 79, 116, 41, 115, 40, 114, 40, 132, 26, 132, 26, 145, 17, 161, 12, 176, 10,
            177, 11,
        ],
        // Intra
        [
            24, 179, 48, 138, 54, 135, 54, 132, 53, 134, 56, 133, 55, 132, 55, 132, 61, 114, 70,
            96, 74, 88, 75, 88, 87, 74, 89, 66, 91, 67, 100, 59, 108, 50, 120, 40, 122, 37, 97, 43,
            78, 50,
        ],
    ],
    // 240 sample frames.
    [
        // Inter
        [
            83, 78, 84, 81, 88, 75, 86, 74, 87, 71, 90, 73, 93, 74, 93, 74, 109, 40, 114, 36, 117,
            34, 117, 34, 143, 17, 145, 18, 146, 19, 162, 12, 165, 10, 178, 7, 189, 6, 190, 8, 177,
            9,
        ],
        // Intra
        [
            23, 178, 54, 115, 63, 102, 66, 98, 69, 99, 74, 89, 71, 91, 73, 91, 78, 89, 86, 80, 92,
            66, 93, 64, 102, 59, 103, 60, 104, 60, 117, 52, 123, 44, 138, 35, 133, 31, 97, 38, 77,
            45,
        ],
    ],
    // 480 sample frames.
    [
        // Inter
        [
            61, 90, 93, 60, 105, 42, 107, 41, 110, 45, 116, 38, 113, 38, 112, 38, 124, 26, 132, 27,
            136, 19, 140, 20, 155, 14, 159, 16, 158, 18, 170, 13, 177, 10, 187, 8, 192, 6, 175, 9,
            159, 10,
        ],
        // Intra
        [
            21, 178, 59, 110, 71, 86, 75, 85, 84, 83, 91, 66, 88, 73, 87, 72, 92, 75, 98, 72, 105,
            58, 107, 54, 115, 52, 114, 55, 112, 56, 129, 51, 132, 40, 150, 33, 140, 29, 98, 35, 77,
            42,
        ],
    ],
    // 960 sample frames.
    [
        // Inter
        [
            42, 121, 96, 66, 108, 43, 111, 40, 117, 44, 123, 32, 120, 36, 119, 33, 127, 33, 134,
            34, 139, 21, 147, 23, 152, 20, 158, 25, 154, 26, 166, 21, 173, 16, 184, 13, 184, 10,
            150, 13, 139, 15,
        ],
        // Intra
        [
            22, 178, 63, 114, 74, 82, 84, 83, 92, 82, 103, 62, 96, 72, 96, 67, 101, 73, 107, 72,
            113, 55, 118, 52, 125, 52, 118, 52, 117, 55, 135, 49, 137, 39, 157, 32, 145, 29, 97,
            33, 77, 40,
        ],
    ],
];

static SMALL_ENERGY_ICDF: [u8; 3] = [2, 1, 0];

/// `unquant_coarse_energy`: decodes the coarse band energies in place into
/// `old_ebands` (length `2*NB_EBANDS` for up to two channels).
///
/// `len` is the packet size in bytes (the reference reads `dec->storage`).
pub(crate) fn unquant_coarse_energy(
    start: usize,
    end: usize,
    old_ebands: &mut [f32],
    intra: bool,
    len: usize,
    dec: &mut RangeDecoder,
    c: usize,
    lm: usize,
) -> crate::Result<()> {
    let prob_model = &E_PROB_MODEL[lm][usize::from(intra)];
    let (coef, beta) = if intra {
        (0.0f32, BETA_INTRA)
    } else {
        (PRED_COEF[lm], BETA_COEF[lm])
    };

    let budget = (len * 8) as i32;
    let mut prev = [0f32; 2];

    // Decode at a fixed coarse resolution.
    for i in start..end {
        for ci in 0..c {
            let tell = dec.tell() as i32;
            let qi: i32;
            if budget - tell >= 15 {
                let pi = 2 * i.min(20);
                qi = laplace::laplace_decode(
                    dec,
                    (prob_model[pi] as u32) << 7,
                    (prob_model[pi + 1] as i32) << 6,
                )?;
            } else if budget - tell >= 2 {
                let q = dec.decode_icdf(&SMALL_ENERGY_ICDF, 2)?;
                qi = ((q >> 1) as i32) ^ -((q & 1) as i32);
            } else if budget - tell >= 1 {
                qi = -i32::from(dec.decode_bit_logp(1)?);
            } else {
                qi = -1;
            }
            let q = qi as f32;

            let idx = ci * NB_EBANDS + i;
            old_ebands[idx] = old_ebands[idx].max(-9.0);
            let tmp = coef * old_ebands[idx] + prev[ci] + q;
            old_ebands[idx] = tmp;
            prev[ci] += q - beta * q;
        }
    }
    Ok(())
}

/// `unquant_fine_energy`: refines the energies with the per-band fine bits.
pub(crate) fn unquant_fine_energy(
    start: usize,
    end: usize,
    old_ebands: &mut [f32],
    fine_quant: &[i32],
    dec: &mut RangeDecoder,
    c: usize,
) -> crate::Result<()> {
    for i in start..end {
        if fine_quant[i] <= 0 {
            continue;
        }
        for ci in 0..c {
            let q2 = dec.read_raw_bits(fine_quant[i] as u32) as i32;
            let offset =
                (q2 as f32 + 0.5) * ((1 << (14 - fine_quant[i])) as f32) * (1.0 / 16384.0) - 0.5;
            old_ebands[ci * NB_EBANDS + i] += offset;
        }
    }
    Ok(())
}

/// `unquant_energy_finalise`: spends the leftover bits on extra fine
/// resolution, priority-ordered.
pub(crate) fn unquant_energy_finalise(
    start: usize,
    end: usize,
    old_ebands: &mut [f32],
    fine_quant: &[i32],
    fine_priority: &[i32],
    mut bits_left: i32,
    dec: &mut RangeDecoder,
    c: usize,
) -> crate::Result<()> {
    for prio in 0..2 {
        let mut i = start;
        while i < end && bits_left >= c as i32 {
            if fine_quant[i] >= MAX_FINE_BITS || fine_priority[i] != prio {
                i += 1;
                continue;
            }
            for ci in 0..c {
                let q2 = dec.read_raw_bits(1) as i32;
                let offset =
                    (q2 as f32 - 0.5) * ((1 << (14 - fine_quant[i] - 1)) as f32) * (1.0 / 16384.0);
                old_ebands[ci * NB_EBANDS + i] += offset;
                bits_left -= 1;
            }
            i += 1;
        }
    }
    Ok(())
}

/// `eMeans` accessor used by `denormalise_bands`.
#[inline]
pub(crate) fn e_means(i: usize) -> f32 {
    E_MEANS[i]
}
