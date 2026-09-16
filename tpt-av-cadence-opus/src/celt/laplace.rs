//! Laplace-distributed integer coding (`celt/laplace.c`).
//!
//! Used for the coarse band energies (and TF change in the encoder). The
//! decoder consumes 15-bit binary range-coder symbols.
//!
//! SOURCE: Xiph.Org libopus 1.5.2, `celt/laplace.c` (BSD-3-Clause).

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

use crate::range::{RangeDecoder, RangeEncoder};

const LAPLACE_MINP: u32 = 1; // 1 << LAPLACE_LOG_MINP
const LAPLACE_NMIN: i32 = 16;

/// The minimum probability of an energy delta (out of 32768).
const LAPLACE_LOG_MINP: u32 = 0;

#[inline]
fn laplace_get_freq1(fs0: u32, decay: i32) -> u32 {
    let ft = 32768 - LAPLACE_MINP * (2 * LAPLACE_NMIN as u32) - fs0;
    ((ft as i64 * (16384 - decay) as i64) >> 15) as u32
}

/// `ec_laplace_encode`: encodes `value` with the Laplace model.
///
/// On extreme values the reference clamps `*value` to what the model can
/// represent; the clamped value is written back through `value` (the
/// encoder's caller compares `qi0`/`qi`, so the Rust caller needs the same
/// behavior via the return value).
#[allow(dead_code)]
pub(crate) fn laplace_encode(enc: &mut RangeEncoder, value: i32, fs: u32, decay: i32) -> i32 {
    let mut val = value;
    let mut fs = fs;
    let mut fl = 0u32;
    let mut clamped = val;
    if val != 0 {
        let s = val < 0;
        val = val.abs();
        fl = fs;
        fs = laplace_get_freq1(fs, decay);
        // Search the decaying part of the PDF.
        let mut i = 1i32;
        while fs > 0 && i < val {
            fs *= 2;
            fl += fs + 2 * LAPLACE_MINP;
            fs = ((fs as i64 * decay as i64) >> 15) as u32;
            i += 1;
        }
        // Everything beyond that has probability LAPLACE_MINP.
        if fs == 0 {
            // C's `s` is -(val<0) i.e. -1 or 0.
            let s_i = if s { -1i32 } else { 0 };
            let ndi_max = (32768u32.wrapping_sub(fl).wrapping_add(LAPLACE_MINP - 1)
                >> LAPLACE_LOG_MINP) as i32;
            let ndi_max = (ndi_max - s_i) >> 1;
            let di = (val - i).min(ndi_max - 1);
            fl += (2 * di + 1 + s_i) as u32 * LAPLACE_MINP;
            fs = LAPLACE_MINP.min(32768u32.wrapping_sub(fl));
            // C: *value = (i+di+s)^s with s = -1 or 0.
            clamped = if s { -(i + di) } else { i + di };
        } else {
            fs += LAPLACE_MINP;
            if !s {
                fl += fs;
            }
        }
    }
    enc.encode_bin(fl, fl + fs, 15);
    clamped
}

/// `ec_laplace_decode`: decodes one Laplace-distributed integer.
pub(crate) fn laplace_decode(dec: &mut RangeDecoder, fs: u32, decay: i32) -> crate::Result<i32> {
    let mut val = 0i32;
    let mut fs = fs;
    let mut fl;
    let fm = dec.decode_bin(15)?;
    fl = 0;
    if fm >= fs {
        val += 1;
        fl = fs;
        fs = laplace_get_freq1(fs, decay) + LAPLACE_MINP;
        // Search the decaying part of the PDF.
        while fs > LAPLACE_MINP && fm >= fl + 2 * fs {
            fs *= 2;
            fl += fs;
            fs = (((fs - 2 * LAPLACE_MINP) as i64 * decay as i64) >> 15) as u32 + LAPLACE_MINP;
            val += 1;
        }
        // Everything beyond that has probability LAPLACE_MINP.
        if fs <= LAPLACE_MINP {
            let di = ((fm - fl) >> (LAPLACE_LOG_MINP + 1)) as i32;
            val += di;
            fl += 2 * di as u32 * LAPLACE_MINP;
        }
        if fm < fl + fs {
            val = -val;
        } else {
            fl += fs;
        }
    }
    dec.update(fl, (fl + fs).min(32768), 32768);
    Ok(val)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Round trip through the range coder over a spread of values/decays.
    #[test]
    fn laplace_round_trip() {
        for &(fs0, decay) in &[
            (9000u32, 9000i32),
            (16384, 12000),
            (4000, 16000),
            (20000, 4000),
            (100, 16000),
        ] {
            for v in [-30i32, -7, -1, 0, 1, 2, 5, 13, 40, 200] {
                let mut enc = RangeEncoder::new();
                let clamped = laplace_encode(&mut enc, v, fs0, decay);
                let frame = enc.done();
                let mut dec = RangeDecoder::new(&frame);
                let got = laplace_decode(&mut dec, fs0, decay).unwrap();
                assert_eq!(got, clamped, "fs={fs0} decay={decay} v={v}");
            }
        }
    }
}
