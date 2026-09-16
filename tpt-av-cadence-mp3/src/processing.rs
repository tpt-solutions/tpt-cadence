//! Granule post-processing: short-block reordering and alias reduction.

use crate::tables::AA;

/// Reorders short-block spectral data from scalefactor-band order
/// (sfb, window, line) into time-ordered subband layout, using `scratch`
/// as a staging area (≥ the number of reordered samples).
pub(crate) fn reorder(grbuf: &mut [f32], scratch: &mut [f32], sfb: &[u8]) {
    let mut src = 0usize;
    let mut dst = 0usize;
    for widths in sfb.chunks_exact(3) {
        let len = widths[0] as usize;
        if len == 0 {
            break;
        }
        for i in 0..len {
            scratch[dst] = grbuf[src + i];
            scratch[dst + 1] = grbuf[src + len + i];
            scratch[dst + 2] = grbuf[src + 2 * len + i];
            dst += 3;
        }
        src += 3 * len;
    }
    grbuf[..dst].copy_from_slice(&scratch[..dst]);
}

/// Antialias butterflies across scalefactor-band boundaries.
pub(crate) fn antialias(grbuf: &mut [f32], nbands: usize) {
    for b in 0..nbands {
        let base = b * 18;
        for i in 0..8usize {
            let u = grbuf[base + 18 + i];
            let d = grbuf[base + 17 - i];
            grbuf[base + 18 + i] = u * AA[0][i] - d * AA[1][i];
            grbuf[base + 17 - i] = u * AA[1][i] + d * AA[0][i];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reorder_matches_reference_walk() {
        // Two sfb groups of widths [2,2,2] and [1,1,1]: input order
        // (w0 w0 | w1 w1 | w2 w2) per group must transpose to lines-first.
        let mut g = vec![0.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0];
        let mut scratch = vec![0.0; 9];
        reorder(&mut g, &mut scratch, &[2, 2, 2, 1, 1, 1, 0]);
        assert_eq!(g, vec![0.0, 2.0, 4.0, 1.0, 3.0, 5.0, 6.0, 7.0, 8.0]);
    }

    #[test]
    fn antialias_butterfly_is_unitary() {
        let mut g = vec![0.0; 18 * 2];
        g[17] = 1.0; // d side of the first butterfly (i = 0)
        g[18] = 2.0; // u side
        antialias(&mut g, 1);
        let c = AA[0][0];
        let s = AA[1][0];
        assert!((g[17] - (2.0 * s + 1.0 * c)).abs() < 1e-6);
        assert!((g[18] - (2.0 * c - 1.0 * s)).abs() < 1e-6);
    }
}
