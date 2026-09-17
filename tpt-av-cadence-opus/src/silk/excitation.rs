//! SILK excitation decoding (RFC 6716 §4.2.7.8.2–§4.2.7.8.5).
//!
//! Ports the quantized-excitation bitstream layer of the reference
//! decoder:
//!
//! - [`decode_pulses`] (`silk/decode_pulses.c`): the per-block pulse
//!   counts (rate level + per-block count with the `SILK_MAX_PULSES + 1`
//!   LSB-shift escape), the shell-coded pulse positions per block of
//!   [`SHELL_CODEC_FRAME_LENGTH`] samples ([`shell_decoder`],
//!   `silk/shell_coder.c`), the LSB refinement bits, and the sign bits
//!   ([`decode_signs`], `silk/code_signs.c`).
//! - [`reconstruct_excitation`]: the "Decode excitation" block at the
//!   top of `silk/decode_core.c` — the ±`QUANT_LEVEL_ADJUST_Q10` pull
//!   toward zero, the quantization offset, and the per-sample sign
//!   dither from the linear congruential generator seeded by the frame
//!   seed (RFC 6716 §4.2.7.7/§4.2.7.8.5).
//!
//! The output of [`decode_pulses`] is the signed quantization index
//! `q[n]` ("pulses"); [`reconstruct_excitation`] turns that into the
//! Q14 excitation `exc_Q14[n]` the LPC/LTP synthesis consumes.
//!
//! SOURCE: Xiph.Org libopus 1.5.2, `silk/decode_pulses.c`,
//! `silk/shell_coder.c`, `silk/code_signs.c`, `silk/decode_core.c`,
//! `silk/encode_pulses.c` (round-trip reference), `silk/define.h`,
//! `silk/SigProc_FIX.h` (BSD-3-Clause); cross-checked against
//! RFC 6716 §4.2.7.4–§4.2.7.8.
#![allow(dead_code)]

use crate::range::RangeDecoder;
use crate::silk::tables::{
    LSB_ICDF, PULSES_PER_BLOCK_ICDF, QUANTIZATION_OFFSETS_Q10, RATE_LEVELS_ICDF, SHELL_CODE_TABLE0,
    SHELL_CODE_TABLE1, SHELL_CODE_TABLE2, SHELL_CODE_TABLE3, SHELL_CODE_TABLE_OFFSETS, SIGN_ICDF,
};
use crate::Result;

/// `SHELL_CODEC_FRAME_LENGTH`: samples per shell-coded block
/// (`silk/define.h`).
pub(crate) const SHELL_CODEC_FRAME_LENGTH: usize = 16;
/// `LOG2_SHELL_CODEC_FRAME_LENGTH`.
pub(crate) const LOG2_SHELL_CODEC_FRAME_LENGTH: u32 = 4;
/// `MAX_NB_SHELL_BLOCKS = MAX_FRAME_LENGTH / SHELL_CODEC_FRAME_LENGTH`
/// (320 samples / 16).
pub(crate) const MAX_NB_SHELL_BLOCKS: usize = 20;
/// `N_RATE_LEVELS` (`silk/define.h`).
pub(crate) const N_RATE_LEVELS: usize = 10;
/// `SILK_MAX_PULSES` (`silk/define.h`): largest per-block pulse count
/// the shell code carries directly; the count `SILK_MAX_PULSES + 1` is
/// the LSB-shift escape.
pub(crate) const SILK_MAX_PULSES: i32 = 16;
/// `QUANT_LEVEL_ADJUST_Q10` (`silk/define.h`): the decoder-side pull of
/// nonzero excitation values toward zero, in Q10.
pub(crate) const QUANT_LEVEL_ADJUST_Q10: i32 = 80;
/// `RAND_MULTIPLIER` (`silk/SigProc_FIX.h`).
const RAND_MULTIPLIER: i32 = 196314165;
/// `RAND_INCREMENT` (`silk/SigProc_FIX.h`).
const RAND_INCREMENT: i32 = 907633515;

/// Shell decoder for one block of 16 pulses — port of
/// `silk_shell_decoder` (`silk/shell_coder.c`).
///
/// Reconstructs the 16 nonnegative per-sample pulse amplitudes from a
/// binary pulse-count tree: `pulses4` is the block's total pulse count
/// (1..=16; the caller skips zero-count blocks), and each
/// [`decode_split`] divides a node's count between its two children
/// with one ICDF symbol whose table row is selected by the node's
/// count. `pulses0` must be at least [`SHELL_CODEC_FRAME_LENGTH`] long
/// and is fully overwritten.
pub(crate) fn shell_decoder(
    dec: &mut RangeDecoder<'_>,
    pulses0: &mut [i16],
    pulses4: i32,
) -> Result<()> {
    debug_assert!(pulses0.len() >= SHELL_CODEC_FRAME_LENGTH);
    debug_assert!((1..=SILK_MAX_PULSES).contains(&pulses4));

    /* Split order matches the reference exactly; it fixes the order in
     * which ICDF symbols are consumed. */
    let (p3_0, p3_1) = decode_split(dec, &SHELL_CODE_TABLE3, pulses4)?;
    let (p2_0, p2_1) = decode_split(dec, &SHELL_CODE_TABLE2, p3_0 as i32)?;
    let (p1_0, p1_1) = decode_split(dec, &SHELL_CODE_TABLE1, p2_0 as i32)?;
    let (p0_0, p0_1) = decode_split(dec, &SHELL_CODE_TABLE0, p1_0 as i32)?;
    pulses0[0] = p0_0;
    pulses0[1] = p0_1;
    let (p0_2, p0_3) = decode_split(dec, &SHELL_CODE_TABLE0, p1_1 as i32)?;
    pulses0[2] = p0_2;
    pulses0[3] = p0_3;

    let (p1_2, p1_3) = decode_split(dec, &SHELL_CODE_TABLE1, p2_1 as i32)?;
    let (p0_4, p0_5) = decode_split(dec, &SHELL_CODE_TABLE0, p1_2 as i32)?;
    pulses0[4] = p0_4;
    pulses0[5] = p0_5;
    let (p0_6, p0_7) = decode_split(dec, &SHELL_CODE_TABLE0, p1_3 as i32)?;
    pulses0[6] = p0_6;
    pulses0[7] = p0_7;

    let (p2_2, p2_3) = decode_split(dec, &SHELL_CODE_TABLE2, p3_1 as i32)?;

    let (p1_4, p1_5) = decode_split(dec, &SHELL_CODE_TABLE1, p2_2 as i32)?;
    let (p0_8, p0_9) = decode_split(dec, &SHELL_CODE_TABLE0, p1_4 as i32)?;
    pulses0[8] = p0_8;
    pulses0[9] = p0_9;
    let (p0_10, p0_11) = decode_split(dec, &SHELL_CODE_TABLE0, p1_5 as i32)?;
    pulses0[10] = p0_10;
    pulses0[11] = p0_11;

    let (p1_6, p1_7) = decode_split(dec, &SHELL_CODE_TABLE1, p2_3 as i32)?;
    let (p0_12, p0_13) = decode_split(dec, &SHELL_CODE_TABLE0, p1_6 as i32)?;
    pulses0[12] = p0_12;
    pulses0[13] = p0_13;
    let (p0_14, p0_15) = decode_split(dec, &SHELL_CODE_TABLE0, p1_7 as i32)?;
    pulses0[14] = p0_14;
    pulses0[15] = p0_15;
    Ok(())
}

/// `decode_split` (`silk/shell_coder.c`): divides `p` pulses between
/// two children by decoding the first child's amplitude from the table
/// row for count `p`; a zero-count node splits trivially.
fn decode_split(dec: &mut RangeDecoder<'_>, shell_table: &[u8], p: i32) -> Result<(i16, i16)> {
    if p > 0 {
        let offset = SHELL_CODE_TABLE_OFFSETS[p as usize] as usize;
        let child1 = dec.decode_icdf(&shell_table[offset..], 8)? as i16;
        Ok((child1, (p - child1 as i32) as i16))
    } else {
        Ok((0, 0))
    }
}

/// Decodes the quantization indices of the excitation signal — port of
/// `silk_decode_pulses` (`silk/decode_pulses.c`).
///
/// Writes the signed quantization indices into `pulses`, which must be
/// at least `ceil(frame_length / 16) * 16` long (the reference operates
/// on `MAX_FRAME_LENGTH` buffers; a 10 ms @ 12 kHz frame's final
/// partial block still fills a whole 16-sample block, and every entry
/// in the padded region is always written).
///
/// Bitstream order: one rate-level symbol, then per block one
/// pulse-count symbol (with the `SILK_MAX_PULSES + 1` escape repeated
/// per LSB shift, reading the last rate level's table — offset by one
/// entry from the 10th shift on, which removes the escape from that
/// table), then the per-block shell codes, then the LSB bits, then the
/// sign bits.
pub(crate) fn decode_pulses(
    dec: &mut RangeDecoder<'_>,
    pulses: &mut [i16],
    signal_type: i32,
    quant_offset_type: i32,
    frame_length: usize,
) -> Result<()> {
    /*********************/
    /* Decode rate level */
    /*********************/
    let rate_level_index =
        dec.decode_icdf(&RATE_LEVELS_ICDF[(signal_type >> 1) as usize], 8)? as usize;
    debug_assert!(rate_level_index < N_RATE_LEVELS);

    /* Calculate number of shell blocks */
    let mut iter = frame_length >> LOG2_SHELL_CODEC_FRAME_LENGTH;
    if iter << LOG2_SHELL_CODEC_FRAME_LENGTH < frame_length {
        /* Make sure only happens for 10 ms @ 12 kHz */
        debug_assert!(
            frame_length == 12 * 10,
            "unexpected partial shell block for frame_length {frame_length}"
        );
        iter += 1;
    }
    debug_assert!(iter <= MAX_NB_SHELL_BLOCKS);
    debug_assert!(pulses.len() >= iter * SHELL_CODEC_FRAME_LENGTH);

    /***************************************************/
    /* Sum-Weighted-Pulses Decoding                    */
    /***************************************************/
    let mut sum_pulses = [0i32; MAX_NB_SHELL_BLOCKS];
    let mut n_lshifts = [0i32; MAX_NB_SHELL_BLOCKS];
    let cdf_row = &PULSES_PER_BLOCK_ICDF[rate_level_index];
    for i in 0..iter {
        n_lshifts[i] = 0;
        let mut s = dec.decode_icdf(cdf_row, 8)? as i32;

        /* LSB indication */
        while s == SILK_MAX_PULSES + 1 {
            n_lshifts[i] += 1;
            /* When we've already got 10 LSBs, we shift the table to
             * not allow (SILK_MAX_PULSES + 1) */
            let row = &PULSES_PER_BLOCK_ICDF[N_RATE_LEVELS - 1][(n_lshifts[i] == 10) as usize..];
            s = dec.decode_icdf(row, 8)? as i32;
        }
        sum_pulses[i] = s;
    }

    /***************************************************/
    /* Shell decoding                                  */
    /***************************************************/
    for i in 0..iter {
        let block = &mut pulses[i * SHELL_CODEC_FRAME_LENGTH..(i + 1) * SHELL_CODEC_FRAME_LENGTH];
        if sum_pulses[i] > 0 {
            shell_decoder(dec, block, sum_pulses[i])?;
        } else {
            block.fill(0);
        }
    }

    /***************************************************/
    /* LSB Decoding                                    */
    /***************************************************/
    for i in 0..iter {
        if n_lshifts[i] > 0 {
            let n_ls = n_lshifts[i];
            for k in 0..SHELL_CODEC_FRAME_LENGTH {
                let mut abs_q = pulses[i * SHELL_CODEC_FRAME_LENGTH + k] as i32;
                for _ in 0..n_ls {
                    abs_q <<= 1;
                    abs_q += dec.decode_icdf(&LSB_ICDF, 8)? as i32;
                }
                pulses[i * SHELL_CODEC_FRAME_LENGTH + k] = abs_q as i16;
            }
            /* Mark the number of pulses non-zero for sign decoding. */
            sum_pulses[i] |= n_ls << 5;
        }
    }

    /****************************************/
    /* Decode and add signs to pulse signal */
    /****************************************/
    decode_signs(
        dec,
        pulses,
        frame_length,
        signal_type,
        quant_offset_type,
        &sum_pulses,
    )
}

/// Decodes and applies the sign bits — port of `silk_decode_signs`
/// (`silk/code_signs.c`).
///
/// One sign symbol is consumed per *nonzero* magnitude in each block
/// whose marked pulse count is positive; the sign table row is selected
/// by the signal/quantization-offset types and the (masked) pulse count
/// determines the sign probability. The count is marked `pulses4 |
/// nLS << 5` by [`decode_pulses`], so the low 5 bits carry the shell
/// count and blocks with only LSB-refined magnitudes still get signs.
fn decode_signs(
    dec: &mut RangeDecoder<'_>,
    pulses: &mut [i16],
    length: usize,
    signal_type: i32,
    quant_offset_type: i32,
    sum_pulses: &[i32; MAX_NB_SHELL_BLOCKS],
) -> Result<()> {
    let mut icdf = [0u8; 2]; /* icdf[1] = 0 */
    let row = 7 * (quant_offset_type + (signal_type << 1)) as usize;
    debug_assert!(row + 6 < SIGN_ICDF.len());
    let icdf_ptr = &SIGN_ICDF[row..];
    let n_blocks = (length + SHELL_CODEC_FRAME_LENGTH / 2) >> LOG2_SHELL_CODEC_FRAME_LENGTH;
    debug_assert!(n_blocks <= sum_pulses.len());
    for i in 0..n_blocks {
        let p = sum_pulses[i];
        if p > 0 {
            icdf[0] = icdf_ptr[(p & 0x1F).min(6) as usize];
            for q in &mut pulses[i * SHELL_CODEC_FRAME_LENGTH..(i + 1) * SHELL_CODEC_FRAME_LENGTH] {
                if *q > 0 {
                    /* attach sign: symbol 0 -> -1, symbol 1 -> +1 */
                    let s = dec.decode_icdf(&icdf, 8)? as i16;
                    *q *= (s << 1) - 1;
                }
            }
        }
    }
    Ok(())
}

/// Reconstructs the Q14 excitation from the signed quantization
/// indices — the "Decode excitation" block at the top of
/// `silk_decode_core` (`silk/decode_core.c`).
///
/// For each sample: `exc_Q14 = (q << 14)`, pulled toward zero by
/// `QUANT_LEVEL_ADJUST_Q10` for nonzero `q`, shifted by the
/// signal-type/quantization-offset-dependent `offset_Q10`, then
/// conditionally negated by the frame's linear congruential generator
/// dither (`seed` is advanced first, the pulse value is accumulated
/// after the sign test — both with wraparound, as in the reference's
/// `silk_RAND`/`silk_ADD32_ovflw`). `exc_q14.len()` is the frame
/// length; `pulses` must be at least as long (its padded tail is
/// ignored, as in the reference).
pub(crate) fn reconstruct_excitation(
    exc_q14: &mut [i32],
    pulses: &[i16],
    seed: i8,
    signal_type: i8,
    quant_offset_type: i8,
) {
    debug_assert!(pulses.len() >= exc_q14.len());
    let offset_q10 =
        QUANTIZATION_OFFSETS_Q10[(signal_type >> 1) as usize][quant_offset_type as usize] as i32;

    let mut rand_seed = seed as i32;
    for (i, exc) in exc_q14.iter_mut().enumerate() {
        rand_seed = rand_seed
            .wrapping_mul(RAND_MULTIPLIER)
            .wrapping_add(RAND_INCREMENT);
        let mut e = (pulses[i] as i32).wrapping_shl(14);
        if e > 0 {
            e = e.wrapping_sub(QUANT_LEVEL_ADJUST_Q10 << 4);
        } else if e < 0 {
            e = e.wrapping_add(QUANT_LEVEL_ADJUST_Q10 << 4);
        }
        e = e.wrapping_add(offset_q10 << 4);
        if rand_seed < 0 {
            e = e.wrapping_neg();
        }
        *exc = e;

        rand_seed = rand_seed.wrapping_add(pulses[i] as i32);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::range::RangeEncoder;
    use crate::silk::tables::MAX_PULSES_TABLE;

    /// Small deterministic PRNG (xorshift32) so the tests need no
    /// external crate.
    struct XorShift(u32);

    impl XorShift {
        fn next_u32(&mut self) -> u32 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            self.0 = x;
            x
        }
        fn next_range(&mut self, lo: i32, hi: i32) -> i32 {
            lo + (self.next_u32() % (hi - lo + 1) as u32) as i32
        }
    }

    /* ---- test-side ports of the reference ENCODERS, for bit-exact
     * round trips (mirrors how gains.rs validates its dequant) ---- */

    /// `encode_split` (`silk/shell_coder.c`).
    fn encode_split(enc: &mut RangeEncoder, p_child1: i32, p: i32, shell_table: &[u8]) {
        if p > 0 {
            let offset = SHELL_CODE_TABLE_OFFSETS[p as usize] as usize;
            enc.encode_icdf(p_child1 as u32, &shell_table[offset..], 8);
        }
    }

    /// `silk_shell_encoder`: encodes one 16-pulse block.
    fn shell_encoder(enc: &mut RangeEncoder, pulses0: &[i32]) {
        assert_eq!(pulses0.len(), SHELL_CODEC_FRAME_LENGTH);
        let pulses1: Vec<i32> = (0..8)
            .map(|k| pulses0[2 * k] + pulses0[2 * k + 1])
            .collect();
        let pulses2: Vec<i32> = (0..4)
            .map(|k| pulses1[2 * k] + pulses1[2 * k + 1])
            .collect();
        let pulses3: Vec<i32> = (0..2)
            .map(|k| pulses2[2 * k] + pulses2[2 * k + 1])
            .collect();
        let pulses4 = pulses3[0] + pulses3[1];

        encode_split(enc, pulses3[0], pulses4, &SHELL_CODE_TABLE3);

        encode_split(enc, pulses2[0], pulses3[0], &SHELL_CODE_TABLE2);

        encode_split(enc, pulses1[0], pulses2[0], &SHELL_CODE_TABLE1);
        encode_split(enc, pulses0[0], pulses1[0], &SHELL_CODE_TABLE0);
        encode_split(enc, pulses0[2], pulses1[1], &SHELL_CODE_TABLE0);

        encode_split(enc, pulses1[2], pulses2[1], &SHELL_CODE_TABLE1);
        encode_split(enc, pulses0[4], pulses1[2], &SHELL_CODE_TABLE0);
        encode_split(enc, pulses0[6], pulses1[3], &SHELL_CODE_TABLE0);

        encode_split(enc, pulses2[2], pulses3[1], &SHELL_CODE_TABLE2);

        encode_split(enc, pulses1[4], pulses2[2], &SHELL_CODE_TABLE1);
        encode_split(enc, pulses0[8], pulses1[4], &SHELL_CODE_TABLE0);
        encode_split(enc, pulses0[10], pulses1[5], &SHELL_CODE_TABLE0);

        encode_split(enc, pulses1[6], pulses2[3], &SHELL_CODE_TABLE1);
        encode_split(enc, pulses0[12], pulses1[6], &SHELL_CODE_TABLE0);
        encode_split(enc, pulses0[14], pulses1[7], &SHELL_CODE_TABLE0);
    }

    /// `combine_and_check` for one level; `None` = "scale down"
    /// (a partial sum exceeds `max_pulses`).
    fn combine_level(input: &[i32], max_pulses: i32) -> Option<Vec<i32>> {
        let mut out = vec![0i32; input.len() / 2];
        for (k, o) in out.iter_mut().enumerate() {
            let sum = input[2 * k] + input[2 * k + 1];
            if sum > max_pulses {
                return None;
            }
            *o = sum;
        }
        Some(out)
    }

    /// `silk_encode_signs`.
    fn encode_signs(
        enc: &mut RangeEncoder,
        pulses: &[i16],
        length: usize,
        signal_type: i32,
        quant_offset_type: i32,
        sum_pulses: &[i32],
    ) {
        let mut icdf = [0u8; 2];
        let row = 7 * (quant_offset_type + (signal_type << 1)) as usize;
        let icdf_ptr = &SIGN_ICDF[row..];
        let n_blocks = (length + SHELL_CODEC_FRAME_LENGTH / 2) >> LOG2_SHELL_CODEC_FRAME_LENGTH;
        for i in 0..n_blocks {
            let p = sum_pulses[i];
            if p > 0 {
                icdf[0] = icdf_ptr[(p & 0x1F).min(6) as usize];
                for &q in &pulses[i * SHELL_CODEC_FRAME_LENGTH..(i + 1) * SHELL_CODEC_FRAME_LENGTH]
                {
                    if q != 0 {
                        /* silk_enc_map: positive -> 1, negative -> 0 */
                        let sym = if q > 0 { 1u32 } else { 0u32 };
                        enc.encode_icdf(sym, &icdf, 8);
                    }
                }
            }
        }
    }

    /// `silk_encode_pulses` with the rate-level search replaced by an
    /// explicit `rate_level` parameter (any valid level must round
    /// trip; the search itself only picks among them).
    ///
    /// Returns the encoder's `tell()` just before finalizing, for the
    /// symbol-count assertions.
    fn encode_pulses(
        enc: &mut RangeEncoder,
        signal_type: i32,
        quant_offset_type: i32,
        rate_level: usize,
        pulses: &mut Vec<i16>,
        frame_length: usize,
    ) -> u32 {
        let mut iter = frame_length >> LOG2_SHELL_CODEC_FRAME_LENGTH;
        if iter << LOG2_SHELL_CODEC_FRAME_LENGTH < frame_length {
            iter += 1;
            pulses.resize(iter * SHELL_CODEC_FRAME_LENGTH, 0);
        }

        /* Absolute values; per-block sums with halving retries. */
        let mut abs_pulses: Vec<i32> = pulses.iter().map(|&q| q.unsigned_abs() as i32).collect();
        let mut sum_pulses = vec![0i32; iter];
        let mut n_rshifts = vec![0i32; iter];
        for i in 0..iter {
            let block =
                &mut abs_pulses[i * SHELL_CODEC_FRAME_LENGTH..(i + 1) * SHELL_CODEC_FRAME_LENGTH];
            loop {
                let mut scale_down = false;
                let stage1 = combine_level(block, MAX_PULSES_TABLE[0] as i32);
                let stage2 = stage1
                    .as_ref()
                    .and_then(|v| combine_level(v, MAX_PULSES_TABLE[1] as i32));
                let stage3 = stage2
                    .as_ref()
                    .and_then(|v| combine_level(v, MAX_PULSES_TABLE[2] as i32));
                let stage4 = stage3
                    .as_ref()
                    .and_then(|v| combine_level(v, MAX_PULSES_TABLE[3] as i32));
                scale_down |=
                    stage1.is_none() || stage2.is_none() || stage3.is_none() || stage4.is_none();
                if scale_down {
                    n_rshifts[i] += 1;
                    for v in block.iter_mut() {
                        *v >>= 1;
                    }
                } else {
                    sum_pulses[i] = stage4.unwrap()[0];
                    break;
                }
            }
        }

        /* Rate level */
        enc.encode_icdf(
            rate_level as u32,
            &RATE_LEVELS_ICDF[(signal_type >> 1) as usize],
            8,
        );

        /* Sum-Weighted-Pulses encoding */
        for i in 0..iter {
            if n_rshifts[i] == 0 {
                enc.encode_icdf(sum_pulses[i] as u32, &PULSES_PER_BLOCK_ICDF[rate_level], 8);
            } else {
                enc.encode_icdf(
                    (SILK_MAX_PULSES + 1) as u32,
                    &PULSES_PER_BLOCK_ICDF[rate_level],
                    8,
                );
                for _ in 0..n_rshifts[i] - 1 {
                    enc.encode_icdf(
                        (SILK_MAX_PULSES + 1) as u32,
                        &PULSES_PER_BLOCK_ICDF[N_RATE_LEVELS - 1],
                        8,
                    );
                }
                enc.encode_icdf(
                    sum_pulses[i] as u32,
                    &PULSES_PER_BLOCK_ICDF[N_RATE_LEVELS - 1],
                    8,
                );
            }
        }

        /* Shell encoding */
        for i in 0..iter {
            if sum_pulses[i] > 0 {
                shell_encoder(
                    enc,
                    &abs_pulses[i * SHELL_CODEC_FRAME_LENGTH..(i + 1) * SHELL_CODEC_FRAME_LENGTH],
                );
            }
        }

        /* LSB encoding */
        for i in 0..iter {
            if n_rshifts[i] > 0 {
                let n_ls = n_rshifts[i] - 1;
                for k in 0..SHELL_CODEC_FRAME_LENGTH {
                    let abs_q = pulses[i * SHELL_CODEC_FRAME_LENGTH + k].unsigned_abs() as i32;
                    for j in (1..=n_ls).rev() {
                        let bit = (abs_q >> j) & 1;
                        enc.encode_icdf(bit as u32, &LSB_ICDF, 8);
                    }
                    enc.encode_icdf((abs_q & 1) as u32, &LSB_ICDF, 8);
                }
            }
        }

        /* Sign encoding */
        encode_signs(
            enc,
            pulses,
            frame_length,
            signal_type,
            quant_offset_type,
            &sum_pulses,
        );

        enc.tell()
    }

    /// Full `decode_pulses` round trip: encode `pulses` (length
    /// `frame_length`), decode, and require the magnitudes *and* signs
    /// to come back exactly, plus the decoder to have consumed exactly
    /// the symbols the encoder wrote (tell equality).
    fn assert_round_trip(
        pulses: &[i16],
        frame_length: usize,
        signal_type: i32,
        quant_offset_type: i32,
        rate_level: usize,
    ) {
        let mut padded = pulses.to_vec();
        let mut enc = RangeEncoder::new();
        let enc_tell = encode_pulses(
            &mut enc,
            signal_type,
            quant_offset_type,
            rate_level,
            &mut padded,
            frame_length,
        );
        let bytes = enc.done();

        let iter = (frame_length + SHELL_CODEC_FRAME_LENGTH - 1) >> LOG2_SHELL_CODEC_FRAME_LENGTH;
        let mut out = vec![0i16; iter * SHELL_CODEC_FRAME_LENGTH];
        let mut dec = RangeDecoder::new(&bytes);
        decode_pulses(
            &mut dec,
            &mut out,
            signal_type,
            quant_offset_type,
            frame_length,
        )
        .expect("decode_pulses failed on a valid encoding");
        assert_eq!(
            dec.tell(),
            enc_tell,
            "decoder consumed a different number of symbols than the encoder wrote"
        );
        assert_eq!(&out[..frame_length], &padded[..frame_length]);
        /* The padded tail of a partial final block (10 ms @ 12 kHz)
         * must decode back to zeros. */
        assert!(out[frame_length..].iter().all(|&q| q == 0));
    }

    /// All 16-way nonnegative compositions of `k` (positions for the
    /// pulses), exhaustively.
    fn all_compositions(k: usize, len: usize) -> Vec<Vec<i32>> {
        let mut out = Vec::new();
        let mut cur = vec![0usize; len];
        fn rec(idx: usize, left: usize, cur: &mut Vec<usize>, out: &mut Vec<Vec<i32>>) {
            if idx + 1 == cur.len() {
                cur[idx] = left;
                out.push(cur.iter().map(|&c| c as i32).collect());
                return;
            }
            for v in 0..=left {
                cur[idx] = v;
                rec(idx + 1, left - v, cur, out);
            }
        }
        if k == 0 {
            out.push(vec![0i32; len]);
        } else {
            rec(0, k, &mut cur, &mut out);
        }
        out
    }

    #[test]
    fn shell_round_trip_exhaustive_small_sums() {
        /* Every 16-sample pulse pattern with total 0..=4 pulses. */
        for k in 0..=4usize {
            for pulses in all_compositions(k, SHELL_CODEC_FRAME_LENGTH) {
                let mut enc = RangeEncoder::new();
                shell_encoder(&mut enc, &pulses);
                let bytes = enc.done();

                let mut out = vec![0i16; SHELL_CODEC_FRAME_LENGTH];
                let mut dec = RangeDecoder::new(&bytes);
                if k == 0 {
                    /* Zero-count blocks never reach the shell decoder;
                     * decode_pulses memsets them instead. Feed the
                     * decoder a count of 1 for the trivial all-zero
                     * vector to at least exercise the split tree. */
                    continue;
                }
                shell_decoder(&mut dec, &mut out, k as i32)
                    .unwrap_or_else(|e| panic!("shell decode failed for {pulses:?}: {e}"));
                let got: Vec<i32> = out.iter().map(|&v| v as i32).collect();
                assert_eq!(got, pulses, "k={k}");
            }
        }
    }

    #[test]
    fn shell_round_trip_random_up_to_max_pulses() {
        let mut rng = XorShift(0x5EED_1A2B);
        for k in 5..=16 {
            for _ in 0..150 {
                /* Distribute k pulses over the 16 positions. */
                let mut pulses = vec![0i32; SHELL_CODEC_FRAME_LENGTH];
                for _ in 0..k {
                    pulses[rng.next_u32() as usize % SHELL_CODEC_FRAME_LENGTH] += 1;
                }
                let mut enc = RangeEncoder::new();
                shell_encoder(&mut enc, &pulses);
                let bytes = enc.done();

                let mut out = vec![0i16; SHELL_CODEC_FRAME_LENGTH];
                let mut dec = RangeDecoder::new(&bytes);
                shell_decoder(&mut dec, &mut out, k).unwrap();
                let got: Vec<i32> = out.iter().map(|&v| v as i32).collect();
                assert_eq!(got, pulses, "k={k}");
            }
        }
    }

    #[test]
    fn decode_pulses_round_trip_all_frame_lengths_and_types() {
        let frame_lengths = [80, 120, 160, 240, 320]; /* 10 ms @ 8/12/16 kHz, 20 ms @ 12/16 kHz */
        let mut rng = XorShift(0xC0FF_EE01);
        for &frame_length in &frame_lengths {
            for signal_type in 0..3i32 {
                for quant_offset_type in 0..2i32 {
                    for case in 0..12 {
                        /* Mix of sparse, dense, and LSB-heavy vectors;
                         * |q| <= 128 like the reference's opus_int8
                         * quantization indices (keeps nLshifts <= 7,
                         * safely below the 10-shift table shift). */
                        let mut pulses = vec![0i16; frame_length];
                        let density = 1 + (case % 3);
                        for p in pulses.iter_mut() {
                            if rng.next_range(0, density * 3) == 0 {
                                let mag = rng.next_range(1, if case % 4 == 3 { 128 } else { 8 });
                                *p = if rng.next_u32() & 1 == 0 { mag } else { -mag } as i16;
                            }
                        }
                        let rate_level = rng.next_range(0, (N_RATE_LEVELS - 2) as i32) as usize; /* 0..=8: rate level 9 is never encoded */
                        assert_round_trip(
                            &pulses,
                            frame_length,
                            signal_type,
                            quant_offset_type,
                            rate_level,
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn decode_pulses_round_trip_saturated_block() {
        /* A full block of 16 samples all carrying |q| = 128 drives the
         * per-block halving loop to its practical maximum for i8
         * indices (2048 -> 7 shifts) and exercises LSBs + signs on
         * every sample. */
        for signal_type in 0..3i32 {
            for sign_flip in [false, true] {
                let pulses: Vec<i16> = (0..320)
                    .map(|i| {
                        let mag = 128 - (i % 5) as i16;
                        if (i / 7) % 2 == 0 && !sign_flip {
                            mag
                        } else {
                            -mag
                        }
                    })
                    .collect();
                assert_round_trip(&pulses, 320, signal_type, 0, 4);
                assert_round_trip(&pulses, 160, signal_type, 1, 0);
            }
        }
    }

    #[test]
    fn zero_pulse_blocks_are_memset() {
        /* Rate level + all-zero per-block counts: the decoder reads no
         * shell/LSB/sign symbols at all and writes zeros. */
        let mut enc = RangeEncoder::new();
        enc.encode_icdf(0, &RATE_LEVELS_ICDF[0], 8);
        for _ in 0..20 {
            enc.encode_icdf(0, &PULSES_PER_BLOCK_ICDF[0], 8);
        }
        let enc_tell = enc.tell();
        let bytes = enc.done();

        let mut out = [1234i16; 320];
        let mut dec = RangeDecoder::new(&bytes);
        decode_pulses(&mut dec, &mut out, 0, 0, 320).unwrap();
        assert_eq!(dec.tell(), enc_tell);
        assert!(out.iter().all(|&q| q == 0));
    }

    #[test]
    fn lsb_escape_then_zero_consumes_lsb_bits_but_no_signs() {
        /* A block whose count decodes as escape-then-zero gets LSB
         * refinement (nLS = 1) and is then sign-marked via
         * `sum_pulses |= nLS << 5` — yet with all-zero magnitudes it
         * must consume exactly the LSB bits and no sign symbols. */
        let mut enc = RangeEncoder::new();
        enc.encode_icdf(0, &RATE_LEVELS_ICDF[0], 8); /* rate level 0 */
        enc.encode_icdf(17, &PULSES_PER_BLOCK_ICDF[0], 8); /* escape */
        enc.encode_icdf(0, &PULSES_PER_BLOCK_ICDF[9], 8); /* count 0 */
        for _ in 0..16 {
            enc.encode_icdf(0, &LSB_ICDF, 8); /* nLS = 1 LSB per sample */
        }
        let enc_tell = enc.tell();
        let bytes = enc.done();

        let mut out = [77i16; 16];
        let mut dec = RangeDecoder::new(&bytes);
        decode_pulses(&mut dec, &mut out, 0, 0, 16).unwrap();
        assert_eq!(dec.tell(), enc_tell, "sign symbols must not be consumed");
        assert!(out.iter().all(|&q| q == 0));
    }

    #[test]
    fn tenth_lsb_shift_reads_offset_table() {
        /* From the 10th shift on, the escape loop reads the last rate
         * level's table OFFSET BY ONE (removing the escape symbol).
         * Craft that exact symbol sequence: 9 escapes at row 9, then
         * the final count from the shifted table. */
        for final_count in [0u32, 1, 16] {
            let mut enc = RangeEncoder::new();
            enc.encode_icdf(3, &RATE_LEVELS_ICDF[0], 8);
            enc.encode_icdf(17, &PULSES_PER_BLOCK_ICDF[3], 8); /* 1st shift */
            for _ in 0..9 {
                enc.encode_icdf(17, &PULSES_PER_BLOCK_ICDF[9], 8); /* shifts 2..10 */
            }
            /* 10th read: shifted table (escape not decodable). */
            enc.encode_icdf(final_count, &PULSES_PER_BLOCK_ICDF[9][1..], 8);
            /* Shell coding of `final_count` pulses in one block. */
            if final_count > 0 {
                let mut pulses = vec![0i32; 16];
                for p in pulses.iter_mut().take(final_count as usize) {
                    *p = 1;
                }
                shell_encoder(&mut enc, &pulses);
            }
            /* nLshifts = 10: ten LSB bits per sample (all zero — the
             * magnitudes are the shell values shifted left). */
            for _ in 0..16 {
                for _ in 0..10 {
                    enc.encode_icdf(0, &LSB_ICDF, 8);
                }
            }
            /* Signs: after LSB refinement the nonzero samples (one for
             * count 1 at position 0; all 16 for count 16) each consume
             * one sign symbol (all positive here). The decoder's table
             * entry is min(sum_pulses & 0x1F, 6) — the mask recovers
             * the raw shell count, not the marked value. */
            if final_count > 0 {
                let icdf = [SIGN_ICDF[(final_count as usize & 0x1F).min(6)], 0];
                let n_signs = if final_count == 16 { 16 } else { 1 };
                for _ in 0..n_signs {
                    enc.encode_icdf(1, &icdf, 8);
                }
            }
            let enc_tell = enc.tell();
            let bytes = enc.done();

            let mut out = vec![0i16; 16];
            let mut dec = RangeDecoder::new(&bytes);
            decode_pulses(&mut dec, &mut out, 0, 0, 16).unwrap();
            assert_eq!(dec.tell(), enc_tell);
            /* Total = count << 10 pins both the shell count and the
             * exact shift count of 10 (9 shifts would give << 9). */
            let total: i32 = out.iter().map(|&q| q.unsigned_abs() as i32).sum();
            assert_eq!(total, final_count as i32 * (1 << 10));
        }
    }

    #[test]
    fn lsb_refinement_restores_magnitude_and_sign() {
        /* Every sample = +/-3 in one block: sum 48 needs two halvings
         * (48 -> 24 -> 12), so the shell code carries 12 and two LSB
         * bits per sample restore |q| = 3; signs reattach afterwards. */
        let mut pulses = vec![0i16; 16];
        for (i, p) in pulses.iter_mut().enumerate() {
            *p = if i % 2 == 0 { 3 } else { -3 };
        }
        assert_round_trip(&pulses, 16, 2, 0, 5);
    }

    /// Independent RFC 6716 §4.2.7.8.5 formulation (u32 LCG, MSB test,
    /// wrapping accumulation) used as the dither reference.
    fn rfc_dither_reference(pulses: &[i16], seed: i8, offset_q10: i32) -> Vec<i32> {
        let adjust = QUANT_LEVEL_ADJUST_Q10 << 4;
        let mut s = seed as u32;
        pulses
            .iter()
            .map(|&q| {
                s = s
                    .wrapping_mul(RAND_MULTIPLIER as u32)
                    .wrapping_add(RAND_INCREMENT as u32);
                let mut e = (q as i32) << 14;
                if q > 0 {
                    e -= adjust;
                } else if q < 0 {
                    e += adjust;
                }
                e += offset_q10 << 4;
                if s & 0x8000_0000 != 0 {
                    e = -e;
                }
                s = s.wrapping_add(q as i32 as u32);
                e
            })
            .collect()
    }

    #[test]
    fn reconstruct_excitation_hand_computed_vector() {
        /* pulses [1, -1, 0, 3], voiced / low offset (32), seed 0:
         * LCG states 907633515 (+), -1457346361 (-), -1648464791 (-),
         * 1591146792 (+) give 15616, 14592, -512, 48384. */
        let pulses = [1i16, -1, 0, 3];
        let mut exc = [0i32; 4];
        reconstruct_excitation(&mut exc, &pulses, 0, 2, 0);
        assert_eq!(exc, [15616, 14592, -512, 48384]);
    }

    #[test]
    fn reconstruct_excitation_matches_rfc_lcg_formulation() {
        let mut rng = XorShift(0x0D17_ED01);
        let offsets: [[i16; 2]; 2] = [[100, 240], [32, 100]];
        for case in 0..40 {
            let len = 80 + (case % 5) * 60; /* 80..320 */
            let pulses: Vec<i16> = (0..len)
                .map(|_| {
                    if rng.next_range(0, 2) == 0 {
                        0
                    } else {
                        rng.next_range(-9, 9) as i16
                    }
                })
                .collect();
            let seed = rng.next_range(0, 3) as i8;
            let signal_type = rng.next_range(0, 2) as i8; /* 0: inactive, 2: voiced */
            let quant_offset_type = rng.next_range(0, 1) as i8;
            let offset_q10 =
                offsets[(signal_type >> 1) as usize][quant_offset_type as usize] as i32;

            let mut exc = vec![0i32; len];
            reconstruct_excitation(&mut exc, &pulses, seed, signal_type, quant_offset_type);
            assert_eq!(exc, rfc_dither_reference(&pulses, seed, offset_q10));
        }
    }

    #[test]
    fn reconstruct_excitation_dither_signs_follow_lcg() {
        /* All-zero pulses leave only the offset; the per-sample sign
         * must follow the LCG sequence (seed 0: +,-,-,-,...). */
        let mut exc = [0i32; 4];
        reconstruct_excitation(&mut exc, &[0i16; 4], 0, 0, 1); /* offset 240 */
        let expect = |neg: bool| if neg { -(240 << 4) } else { 240 << 4 };
        assert_eq!(
            exc,
            [expect(false), expect(true), expect(true), expect(true)]
        );

        /* Different seeds start on different LCG phases. */
        let mut a = [0i32; 1];
        let mut b = [0i32; 1];
        reconstruct_excitation(&mut a, &[0i16], 1, 0, 0);
        reconstruct_excitation(&mut b, &[0i16], 3, 0, 0);
        /* From the precomputed table: seed 1 first state is positive,
         * seed 3 first state is positive, but seed 0's second state is
         * negative while seed 2's first is positive. */
        assert_eq!(a[0], 100 << 4); /* seed 1, first LCG state positive */
        assert_eq!(b[0], 100 << 4); /* seed 3, first LCG state positive */
    }

    #[test]
    fn reconstruct_excitation_offset_and_adjust_by_type() {
        /* The offset depends on both types and the adjust applies only
         * to nonzero pulses; with the dither disabled by choosing a
         * seed whose first LCG state is positive (seed 0). */
        let cases = [
            (0i8, 0i8, 100i32), /* inactive/unvoiced, low */
            (0, 1, 240),        /* inactive/unvoiced, high */
            (2, 0, 32),         /* voiced, low */
            (2, 1, 100),        /* voiced, high */
        ];
        for (st, qot, offset) in cases {
            let mut exc = [0i32; 3];
            reconstruct_excitation(&mut exc, &[0, 1, -1], 0, st, qot);
            /* Seed 0's LCG states for pulses [0, 1, -1] (with the
             * per-sample pulse accumulation) carry signs +, -, +. */
            assert_eq!(exc[0], offset << 4, "zero pulse: st={st} qot={qot}");
            assert_eq!(
                exc[1],
                -((1 << 14) - (QUANT_LEVEL_ADJUST_Q10 << 4) + (offset << 4))
            );
            assert_eq!(
                exc[2],
                -(1 << 14) + (QUANT_LEVEL_ADJUST_Q10 << 4) + (offset << 4)
            );
        }
    }
}
