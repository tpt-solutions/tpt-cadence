//! SBR frequency-table derivation (reference `sbr_make_f_master`,
//! `sbr_make_f_derived`, `sbr_hf_calc_npatches`, `sbr_make_f_tablelim`).
#![allow(
    clippy::needless_range_loop,
    clippy::too_many_arguments,
    clippy::unnecessary_cast,
    clippy::manual_contains,
    clippy::excessive_precision,
    clippy::erasing_op,
    clippy::identity_op,
    clippy::unused_enumerate_index,
    clippy::manual_div_ceil
)]

use super::Sbr;

/// `make_bands`: logarithmically spaced band borders.
pub fn make_bands(bands: &mut [i16], start: i32, stop: i32, num_bands: usize) {
    let base = ((stop as f32) / (start as f32)).powf(1.0 / num_bands as f32);
    let mut prod = start as f32;
    let mut previous = start;
    for band in bands[..num_bands - 1].iter_mut() {
        prod *= base;
        let present = lrintf(prod);
        *band = (present - previous) as i16;
        previous = present;
    }
    bands[num_bands - 1] = (stop - previous) as i16;
}

fn qsort_i16(a: &mut [i16]) {
    a.sort();
}

fn qsort_u16(a: &mut [u16]) {
    a.sort();
}

/// `lrintf` with the default round-to-nearest-even mode (MSRV 1.75 has no
/// `round_ties_even`).
fn lrintf(x: f32) -> i32 {
    let f = x.floor();
    let diff = x - f;
    // Ties (diff == 0.5) round to even, matching C's default lrintf.
    if diff > 0.5 || (diff == 0.5 && (f as i32) & 1 == 1) {
        f as i32 + 1
    } else {
        f as i32
    }
}

fn in_table_u16(table: &[u16], last_el: usize, needle: u16) -> bool {
    table[..=last_el].iter().any(|&v| v == needle)
}

fn array_min_i16(a: &[i16], nel: usize) -> i32 {
    a[..nel].iter().copied().min().unwrap_or(0) as i32
}

fn check_n_master(n_master: i32, bs_xover_band: i32) -> bool {
    n_master > 0 && bs_xover_band < n_master
}

/// `sbr_make_f_master`. Returns false on bitstream failure.
pub fn make_f_master(sbr: &mut Sbr) -> bool {
    let sample_rate = sbr.sample_rate as i32;
    let sbr_offset_ptr: [i32; 16] = match sample_rate {
        16000 => super::tables::SBR_OFFSET[0],
        22050 => super::tables::SBR_OFFSET[1],
        24000 => super::tables::SBR_OFFSET[2],
        32000 => super::tables::SBR_OFFSET[3],
        44100 | 48000 | 64000 => super::tables::SBR_OFFSET[4],
        88200 | 96000 | 128000 | 176400 | 192000 => super::tables::SBR_OFFSET[5],
        _ => return false,
    }
    .map(|v| v as i32);

    let temp: i32 = if sample_rate < 32000 {
        3000
    } else if sample_rate < 64000 {
        4000
    } else {
        5000
    };

    let start_min = ((temp << 7) + (sample_rate >> 1)) / sample_rate;
    let stop_min = ((temp << 8) + (sample_rate >> 1)) / sample_rate;

    let bs_start_freq = sbr.spectrum_params[0] as usize;
    let bs_stop_freq = sbr.spectrum_params[1] as usize;
    let bs_xover_band = sbr.spectrum_params[2] as i32;
    let bs_freq_scale = sbr.spectrum_params[3] as usize;
    let bs_alter_scale = sbr.spectrum_params[4] as i32;

    sbr.k[0] = (start_min + sbr_offset_ptr[bs_start_freq]) as i32;

    if bs_stop_freq < 14 {
        sbr.k[2] = stop_min;
        let mut stop_dk = [0i16; 13];
        make_bands(&mut stop_dk, stop_min, 64, 13);
        qsort_i16(&mut stop_dk);
        for k in 0..bs_stop_freq {
            sbr.k[2] += stop_dk[k] as i32;
        }
    } else if bs_stop_freq == 14 {
        sbr.k[2] = 2 * sbr.k[0];
    } else if bs_stop_freq == 15 {
        sbr.k[2] = 3 * sbr.k[0];
    } else {
        return false;
    }
    sbr.k[2] = sbr.k[2].min(64);

    let max_qmf_subbands: i32 = if sample_rate <= 32000 {
        48
    } else if sample_rate == 44100 {
        35
    } else {
        32
    };

    if sbr.k[2] - sbr.k[0] > max_qmf_subbands {
        return false;
    }

    if bs_freq_scale == 0 {
        let dk = bs_alter_scale + 1;
        sbr.n_master = (((sbr.k[2] - sbr.k[0] + (dk & 2)) >> dk) << 1) as usize;
        if !check_n_master(sbr.n_master as i32, bs_xover_band) {
            return false;
        }
        for k in 1..=sbr.n_master {
            sbr.f_master[k] = dk as u16;
        }
        let k2diff = sbr.k[2] - sbr.k[0] - sbr.n_master as i32 * dk;
        if k2diff < 0 {
            sbr.f_master[1] -= 1;
            if k2diff < -1 {
                sbr.f_master[2] -= 1;
            }
        } else if k2diff != 0 {
            sbr.f_master[sbr.n_master as usize] += 1;
        }
        sbr.f_master[0] = sbr.k[0] as u16;
        for k in 1..=sbr.n_master {
            sbr.f_master[k as usize] += sbr.f_master[k as usize - 1];
        }
    } else {
        let half_bands: i32 = 7 - bs_freq_scale as i32; // {1,2,3}
        let two_regions = 49 * sbr.k[2] > 110 * sbr.k[0];
        sbr.k[1] = if two_regions { 2 * sbr.k[0] } else { sbr.k[2] };

        let num_bands_0 =
            lrintf((half_bands as f32) * ((sbr.k[1] as f32) / (sbr.k[0] as f32)).log2()) * 2;
        if num_bands_0 <= 0 {
            return false;
        }

        let mut vk0 = [0i16; 49];
        make_bands(&mut vk0[1..], sbr.k[0], sbr.k[1], num_bands_0 as usize);
        qsort_i16(&mut vk0[1..=num_bands_0 as usize]);
        let vdk0_max = vk0[num_bands_0 as usize] as i32;

        vk0[0] = sbr.k[0] as i16;
        for k in 1..=num_bands_0 as usize {
            if vk0[k] <= 0 {
                return false;
            }
            vk0[k] += vk0[k - 1];
        }

        if two_regions {
            let mut vk1 = [0i16; 49];
            let invwarp = if bs_alter_scale != 0 {
                0.76923076923076923077f32
            } else {
                1.0
            };
            let num_bands_1 = lrintf(
                (half_bands as f32 * invwarp) * ((sbr.k[2] as f32) / (sbr.k[1] as f32)).log2(),
            ) * 2;
            make_bands(&mut vk1[1..], sbr.k[1], sbr.k[2], num_bands_1 as usize);

            let vdk1_min = array_min_i16(&vk1, num_bands_1 as usize);

            if vdk1_min < vdk0_max {
                qsort_i16(&mut vk1[1..=num_bands_1 as usize]);
                let change = (vdk0_max - vk1[1] as i32)
                    .min((vk1[num_bands_1 as usize] as i32 - vk1[1] as i32) >> 1);
                vk1[1] += change as i16;
                vk1[num_bands_1 as usize] -= change as i16;
            }
            qsort_i16(&mut vk1[1..=num_bands_1 as usize]);

            vk1[0] = sbr.k[1] as i16;
            for k in 1..=num_bands_1 as usize {
                if vk1[k] <= 0 {
                    return false;
                }
                vk1[k] += vk1[k - 1];
            }

            sbr.n_master = (num_bands_0 + num_bands_1) as usize;
            if !check_n_master(sbr.n_master as i32, bs_xover_band) {
                return false;
            }
            for (dst, src) in sbr.f_master[..=(num_bands_0) as usize]
                .iter_mut()
                .zip(&vk0[..=(num_bands_0) as usize])
            {
                *dst = *src as u16;
            }
            for (dst, src) in sbr.f_master[num_bands_0 as usize + 1..=(sbr.n_master) as usize]
                .iter_mut()
                .zip(&vk1[1..=num_bands_1 as usize])
            {
                *dst = *src as u16;
            }
        } else {
            sbr.n_master = num_bands_0 as usize;
            if !check_n_master(sbr.n_master as i32, bs_xover_band) {
                return false;
            }
            for (dst, src) in sbr.f_master[..=(num_bands_0) as usize]
                .iter_mut()
                .zip(&vk0[..=(num_bands_0) as usize])
            {
                *dst = *src as u16;
            }
        }
    }

    true
}

/// `sbr_make_f_derived`.
pub fn make_f_derived(sbr: &mut Sbr) -> bool {
    let bs_xover_band = sbr.spectrum_params[2] as usize;
    let bs_noise_bands = sbr.spectrum_params[5] as i32;

    sbr.n[1] = sbr.n_master - bs_xover_band;
    sbr.n[0] = (sbr.n[1] + 1) >> 1;

    sbr.f_tablehigh[..=sbr.n[1]]
        .copy_from_slice(&sbr.f_master[bs_xover_band..=bs_xover_band + sbr.n[1]]);
    sbr.m[1] = (sbr.f_tablehigh[sbr.n[1]] - sbr.f_tablehigh[0]) as usize;
    sbr.kx[1] = sbr.f_tablehigh[0] as usize;

    if sbr.kx[1] + sbr.m[1] > 64 || sbr.kx[1] > 32 {
        return false;
    }

    sbr.f_tablelow[0] = sbr.f_tablehigh[0];
    let temp = sbr.n[1] & 1;
    for k in 1..=sbr.n[0] {
        sbr.f_tablelow[k] = sbr.f_tablehigh[2 * k - temp];
    }

    sbr.n_q = 1.max(
        lrintf(bs_noise_bands as f32 * ((sbr.k[2] as f32) / (sbr.kx[1] as f32)).log2()) as usize,
    );
    if sbr.n_q > 5 {
        return false;
    }

    sbr.f_tablenoise[0] = sbr.f_tablelow[0];
    let mut temp = 0usize;
    for k in 1..=sbr.n_q {
        temp += (sbr.n[0] - temp) / (sbr.n_q + 1 - k);
        sbr.f_tablenoise[k] = sbr.f_tablelow[temp];
    }

    if !hf_calc_npatches(sbr) {
        return false;
    }

    make_f_tablelim(sbr);

    sbr.data[0].f_indexnoise = 0;
    sbr.data[1].f_indexnoise = 0;

    true
}

/// `sbr_hf_calc_npatches`.
pub fn hf_calc_npatches(sbr: &mut Sbr) -> bool {
    let mut last_k: i32 = -1;
    let mut last_msb: i32 = -1;
    let mut msb = sbr.k[0];
    let mut usb = sbr.kx[1];
    let goal_sb = ((1000 << 11) + (sbr.sample_rate >> 1)) / sbr.sample_rate;

    sbr.num_patches = 0;

    let mut k: i32 = if goal_sb < (sbr.kx[1] + sbr.m[1]) as i32 {
        let mut k = 0usize;
        while (sbr.f_master[k] as i32) < goal_sb {
            k += 1;
        }
        k as i32
    } else {
        sbr.n_master as i32
    };

    loop {
        let mut odd = 0i32;
        if k == last_k && msb == last_msb {
            return false;
        }
        last_k = k;
        last_msb = msb;
        // Verbatim C for-loop semantics: the condition tests the PREVIOUS
        // sb/odd before each body execution, so the loop may exit with the
        // value read for i == k.
        let mut sb: i32 = 0;
        let mut i = k;
        loop {
            if !(i == k || sb > (sbr.k[0] - 1 + msb - odd)) {
                break;
            }
            sb = sbr.f_master[i as usize] as i32;
            odd = (sb + sbr.k[0]) & 1;
            i -= 1;
        }

        if sbr.num_patches > 5 {
            return false;
        }

        sbr.patch_num_subbands[sbr.num_patches] = 0.max(sb - usb as i32) as usize;
        sbr.patch_start_subbands[sbr.num_patches] =
            (sbr.k[0] - odd - sbr.patch_num_subbands[sbr.num_patches] as i32) as usize;

        if sbr.patch_num_subbands[sbr.num_patches] > 0 {
            usb = sb as usize;
            msb = sb;
            sbr.num_patches += 1;
        } else {
            msb = sbr.kx[1] as i32;
        }

        if (sbr.f_master[k as usize] as i32) - sb < 3 {
            k = sbr.n_master as i32;
        }

        if sb == (sbr.kx[1] + sbr.m[1]) as i32 {
            break;
        }
    }

    if sbr.num_patches > 1 && sbr.patch_num_subbands[sbr.num_patches - 1] < 3 {
        sbr.num_patches -= 1;
    }

    true
}

/// `sbr_make_f_tablelim`.
pub fn make_f_tablelim(sbr: &mut Sbr) {
    const BANDS_WARPED: [f64; 3] = [
        1.32715174233856803909, // 2^(0.49/1.2)
        1.18509277094158210129, // 2^(0.49/2)
        1.11987160404675912501, // 2^(0.49/3)
    ];
    if sbr.bs_limiter_bands > 0 {
        let lim_bands_per_octave_warped = BANDS_WARPED[(sbr.bs_limiter_bands - 1) as usize];
        let mut patch_borders = [0u16; 7];
        patch_borders[0] = sbr.kx[1] as u16;
        for k in 1..=sbr.num_patches {
            patch_borders[k] = patch_borders[k - 1] + sbr.patch_num_subbands[k - 1] as u16;
        }

        let n_copy = sbr.n[0] + 1;
        sbr.f_tablelim[..n_copy].copy_from_slice(&sbr.f_tablelow[..n_copy]);
        if sbr.num_patches > 1 {
            sbr.f_tablelim[n_copy..n_copy + sbr.num_patches - 1]
                .copy_from_slice(&patch_borders[1..sbr.num_patches]);
        }

        let total = sbr.num_patches + sbr.n[0];
        qsort_u16(&mut sbr.f_tablelim[..total]);

        sbr.n_lim = sbr.n[0] + sbr.num_patches - 1;
        let mut out = 0usize;
        let mut input = 1usize;
        while out + 1 < sbr.n_lim + 1 && input < total + 1 {
            if input >= MAX_LIM {
                break;
            }
            if sbr.f_tablelim[input] as f64
                >= sbr.f_tablelim[out] as f64 * lim_bands_per_octave_warped
            {
                out += 1;
                sbr.f_tablelim[out] = sbr.f_tablelim[input];
                input += 1;
            } else if sbr.f_tablelim[input] == sbr.f_tablelim[out]
                || !in_table_u16(&patch_borders, sbr.num_patches, sbr.f_tablelim[input])
            {
                input += 1;
                sbr.n_lim -= 1;
            } else if !in_table_u16(&patch_borders, sbr.num_patches, sbr.f_tablelim[out]) {
                sbr.f_tablelim[out] = sbr.f_tablelim[input];
                input += 1;
                sbr.n_lim -= 1;
            } else {
                out += 1;
                sbr.f_tablelim[out] = sbr.f_tablelim[input];
                input += 1;
            }
        }
    } else {
        sbr.f_tablelim[0] = sbr.f_tablelow[0];
        sbr.f_tablelim[1] = sbr.f_tablelow[sbr.n[0]];
        sbr.n_lim = 1;
    }
}

const MAX_LIM: usize = 29;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sbr::Sbr;

    /// The full frequency-table derivation for the FATE al_sbr_cm_48_2
    /// stream (spectrum parameters read from its first SBR header, 48 kHz
    /// SBR rate) must match the reference C implementation's output:
    /// f_master, kx/m, noise floor table, and the two-patch configuration
    /// (verified against an independent build of the reference code).
    #[test]
    fn tables_match_reference_for_al_sbr_cm_48_2() {
        let mut sbr = Sbr::new(1);
        sbr.sample_rate = 48_000;
        // bs_start_freq=12, bs_stop_freq=9, bs_xover_band=0,
        // bs_freq_scale=2, bs_alter_scale=1, bs_noise_bands=3.
        sbr.spectrum_params = [12, 9, 0, 2, 1, 3];

        assert!(make_f_master(&mut sbr), "make_f_master failed");
        assert_eq!(sbr.k, [22, 45, 45]);
        assert_eq!(sbr.n_master, 10);
        assert_eq!(
            &sbr.f_master[..=sbr.n_master],
            &[22, 23, 25, 27, 29, 31, 33, 36, 39, 42, 45]
        );

        assert!(make_f_derived(&mut sbr), "make_f_derived failed");
        assert_eq!(sbr.kx[1], 22);
        assert_eq!(sbr.m[1], 23);
        assert_eq!(sbr.n, [5, 10]);
        assert_eq!(sbr.n_q, 3);
        assert_eq!(&sbr.f_tablenoise[..=sbr.n_q], &[22, 25, 33, 45]);
        assert_eq!(sbr.num_patches, 2);
        assert_eq!(sbr.patch_num_subbands[0], 20);
        assert_eq!(sbr.patch_num_subbands[1], 3);
        assert_eq!(sbr.patch_start_subbands[0], 2);
        assert_eq!(sbr.patch_start_subbands[1], 18);
    }
}
