//! SILK excitation encoding — bit-exact mirror of the decoder's
//! `decode_pulses` path (RFC 6716 §4.2.7.8.2–§4.2.7.8.5).
//!
//! Ports the reference encoder half:
//!
//! - [`encode_pulses`] (`silk/encode_pulses.c`): the per-block pulse-count
//!   preparation (absolute values, the `combine_and_check` halving loop that
//!   produces each block's shell sum and LSB-refinement shift count), the
//!   reference's rate-level search (the level minimizing the estimated
//!   sum-weighted-pulses cost from the `*_BITS_Q5` tables), the escape-shifted
//!   count symbols, [`shell_encoder`], the LSB refinement bits, and
//!   [`encode_signs`].
//!
//! The symbol stream written here decodes through
//! [`super::excitation::decode_pulses`] to exactly the `q[n]` vector passed
//! in; the round-trip is covered by tests (both directions assert bit-tell
//! equality like the decoder-side tests do).
//!
//! SOURCE: Xiph.Org libopus 1.5.2, `silk/encode_pulses.c`,
//! `silk/shell_coder.c` (`silk_shell_encoder`), `silk/code_signs.c`
//! (`silk_encode_signs`), `silk/define.h`, `silk/tables_gain.c`
//! (BSD-3-Clause).
#![allow(dead_code)]

use crate::range::RangeEncoder;
use crate::silk::excitation::{
    LOG2_SHELL_CODEC_FRAME_LENGTH, MAX_NB_SHELL_BLOCKS, N_RATE_LEVELS, SHELL_CODEC_FRAME_LENGTH,
    SILK_MAX_PULSES,
};
use crate::silk::tables::{
    LSB_ICDF, MAX_PULSES_TABLE, PULSES_PER_BLOCK_BITS_Q5, PULSES_PER_BLOCK_ICDF,
    RATE_LEVELS_BITS_Q5, RATE_LEVELS_ICDF, SHELL_CODE_TABLE0, SHELL_CODE_TABLE1, SHELL_CODE_TABLE2,
    SHELL_CODE_TABLE3, SHELL_CODE_TABLE_OFFSETS, SIGN_ICDF,
};

/// `combine_and_check` (`silk/encode_pulses.c`): sums adjacent pairs;
/// returns `None` when any partial sum exceeds `max_pulses` (the caller's
/// cue to halve the block and retry).
fn combine_and_check(pulses_comb: &mut [i32], pulses_in: &[i32], max_pulses: i32) -> bool {
    for (k, out) in pulses_comb.iter_mut().enumerate() {
        let sum = pulses_in[2 * k] + pulses_in[2 * k + 1];
        if sum > max_pulses {
            return false;
        }
        *out = sum;
    }
    true
}

/// `encode_split` (`silk/shell_coder.c`).
fn encode_split(enc: &mut RangeEncoder, p_child1: i32, p: i32, shell_table: &[u8]) {
    if p > 0 {
        let offset = SHELL_CODE_TABLE_OFFSETS[p as usize] as usize;
        enc.encode_icdf(p_child1 as u32, &shell_table[offset..], 8);
    }
}

/// `silk_shell_encoder` (`silk/shell_coder.c`): encodes one block of 16
/// nonnegative pulse amplitudes whose total is in `1..=16`.
pub(crate) fn shell_encoder(enc: &mut RangeEncoder, pulses0: &[i32]) {
    debug_assert_eq!(pulses0.len(), SHELL_CODEC_FRAME_LENGTH);
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
    debug_assert!((1..=SILK_MAX_PULSES).contains(&pulses4));

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

/// `silk_encode_signs` (`silk/code_signs.c`): one symbol per nonzero
/// magnitude, in blocks whose marked pulse count is positive.
fn encode_signs(
    enc: &mut RangeEncoder,
    pulses: &[i16],
    length: usize,
    signal_type: i32,
    quant_offset_type: i32,
    sum_pulses: &[i32; MAX_NB_SHELL_BLOCKS],
) {
    let mut icdf = [0u8; 2]; /* icdf[1] = 0 */
    let row = 7 * (quant_offset_type + (signal_type << 1)) as usize;
    let icdf_ptr = &SIGN_ICDF[row..];
    let n_blocks = (length + SHELL_CODEC_FRAME_LENGTH / 2) >> LOG2_SHELL_CODEC_FRAME_LENGTH;
    for i in 0..n_blocks {
        let p = sum_pulses[i];
        if p > 0 {
            icdf[0] = icdf_ptr[(p & 0x1F).min(6) as usize];
            for &q in &pulses[i * SHELL_CODEC_FRAME_LENGTH..(i + 1) * SHELL_CODEC_FRAME_LENGTH] {
                if q != 0 {
                    /* silk_enc_map: positive -> 1, negative -> 0 */
                    let sym = if q > 0 { 1u32 } else { 0u32 };
                    enc.encode_icdf(sym, &icdf, 8);
                }
            }
        }
    }
}

/// `silk_encode_pulses` (`silk/encode_pulses.c`): writes the quantization
/// indices to the range encoder.
///
/// `pulses` holds the signed per-sample quantization indices; it must be at
/// least the shell-block-rounded `frame_length` long (a 10 ms @ 12 kHz
/// frame's partial final block is zero-padded by the caller, exactly as the
/// reference memsets it). The rate level is chosen by the reference's rule:
/// the level whose `*_BITS_Q5` cost estimate for *these* block sums is
/// minimal (no bitrate budget involved — the estimate uses the escape cost
/// for blocks needing LSB refinement).
pub(crate) fn encode_pulses(
    enc: &mut RangeEncoder,
    signal_type: i32,
    quant_offset_type: i32,
    pulses: &[i16],
    frame_length: usize,
) {
    /****************************/
    /* Prepare for shell coding */
    /****************************/
    let mut iter = frame_length >> LOG2_SHELL_CODEC_FRAME_LENGTH;
    if iter << LOG2_SHELL_CODEC_FRAME_LENGTH < frame_length {
        debug_assert!(frame_length == 12 * 10);
        iter += 1;
        debug_assert!(pulses.len() >= iter * SHELL_CODEC_FRAME_LENGTH);
    }
    debug_assert!(iter <= MAX_NB_SHELL_BLOCKS);

    /* Take the absolute value of the pulses */
    let mut abs_pulses = [0i32; MAX_NB_SHELL_BLOCKS * SHELL_CODEC_FRAME_LENGTH];
    for i in 0..frame_length {
        abs_pulses[i] = (pulses[i] as i32).abs();
    }

    /* Calc sum pulses per shell code frame */
    let mut sum_pulses = [0i32; MAX_NB_SHELL_BLOCKS];
    let mut n_rshifts = [0i32; MAX_NB_SHELL_BLOCKS];
    /* The reference re-uses one `pulses_comb` buffer with decreasing
     * valid lengths (8/4/2); split into per-level buffers — the value
     * semantics are identical. */
    let mut comb8 = [0i32; 8];
    let mut comb4 = [0i32; 4];
    let mut comb2 = [0i32; 2];
    for i in 0..iter {
        let block =
            &mut abs_pulses[i * SHELL_CODEC_FRAME_LENGTH..(i + 1) * SHELL_CODEC_FRAME_LENGTH];
        n_rshifts[i] = 0;
        loop {
            /* 1+1 -> 2 */
            let mut scale_down = !combine_and_check(&mut comb8, block, MAX_PULSES_TABLE[0] as i32);
            /* 2+2 -> 4 */
            scale_down |= !combine_and_check(&mut comb4, &comb8, MAX_PULSES_TABLE[1] as i32);
            /* 4+4 -> 8 */
            scale_down |= !combine_and_check(&mut comb2, &comb4, MAX_PULSES_TABLE[2] as i32);
            /* 8+8 -> 16 */
            scale_down |= !combine_and_check(
                &mut sum_pulses[i..i + 1],
                &comb2,
                MAX_PULSES_TABLE[3] as i32,
            );

            if scale_down {
                /* We need to downscale the quantization signal */
                n_rshifts[i] += 1;
                for v in block.iter_mut() {
                    *v >>= 1;
                }
            } else {
                break;
            }
        }
    }

    /**************/
    /* Rate level */
    /**************/
    /* find rate level that leads to fewest bits for coding of pulses per
     * block info */
    let mut min_sum_bits_q5 = i32::MAX;
    let mut rate_level_index = 0usize;
    for k in 0..N_RATE_LEVELS - 1 {
        let n_bits_ptr = &PULSES_PER_BLOCK_BITS_Q5[k];
        let mut sum_bits_q5 = RATE_LEVELS_BITS_Q5[(signal_type >> 1) as usize][k] as i32;
        for i in 0..iter {
            if n_rshifts[i] > 0 {
                sum_bits_q5 += n_bits_ptr[(SILK_MAX_PULSES + 1) as usize] as i32;
            } else {
                sum_bits_q5 += n_bits_ptr[sum_pulses[i] as usize] as i32;
            }
        }
        if sum_bits_q5 < min_sum_bits_q5 {
            min_sum_bits_q5 = sum_bits_q5;
            rate_level_index = k;
        }
    }
    enc.encode_icdf(
        rate_level_index as u32,
        &RATE_LEVELS_ICDF[(signal_type >> 1) as usize],
        8,
    );

    /***************************************************/
    /* Sum-Weighted-Pulses Encoding                    */
    /***************************************************/
    let cdf_ptr = &PULSES_PER_BLOCK_ICDF[rate_level_index];
    for i in 0..iter {
        if n_rshifts[i] == 0 {
            enc.encode_icdf(sum_pulses[i] as u32, cdf_ptr, 8);
        } else {
            enc.encode_icdf((SILK_MAX_PULSES + 1) as u32, cdf_ptr, 8);
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

    /******************/
    /* Shell Encoding */
    /******************/
    for i in 0..iter {
        if sum_pulses[i] > 0 {
            shell_encoder(
                enc,
                &abs_pulses[i * SHELL_CODEC_FRAME_LENGTH..(i + 1) * SHELL_CODEC_FRAME_LENGTH],
            );
        }
    }

    /****************/
    /* LSB Encoding */
    /****************/
    for i in 0..iter {
        if n_rshifts[i] > 0 {
            let n_ls = n_rshifts[i] - 1;
            for k in 0..SHELL_CODEC_FRAME_LENGTH {
                let abs_q = (pulses[i * SHELL_CODEC_FRAME_LENGTH + k] as i32).abs();
                for j in (1..=n_ls).rev() {
                    let bit = (abs_q >> j) & 1;
                    enc.encode_icdf(bit as u32, &LSB_ICDF, 8);
                }
                enc.encode_icdf((abs_q & 1) as u32, &LSB_ICDF, 8);
            }
        }
    }

    /****************/
    /* Encode signs */
    /****************/
    encode_signs(
        enc,
        pulses,
        frame_length,
        signal_type,
        quant_offset_type,
        &sum_pulses,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::range::RangeDecoder;
    use crate::silk::excitation::decode_pulses;

    /// Small deterministic PRNG (xorshift32) so the property test needs
    /// no external crate.
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

    /// Encode through [`encode_pulses`], decode through the real decoder,
    /// and require exact recovery of every sample plus tell() equality.
    fn assert_round_trip(
        pulses: &[i16],
        frame_length: usize,
        signal_type: i32,
        quant_offset_type: i32,
    ) {
        let iter = (frame_length + SHELL_CODEC_FRAME_LENGTH - 1) >> LOG2_SHELL_CODEC_FRAME_LENGTH;
        let mut padded = pulses.to_vec();
        padded.resize(iter * SHELL_CODEC_FRAME_LENGTH, 0);

        let mut enc = RangeEncoder::new();
        encode_pulses(
            &mut enc,
            signal_type,
            quant_offset_type,
            &padded,
            frame_length,
        );
        let enc_tell = enc.tell();
        let bytes = enc.done();

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
        assert_eq!(dec.tell(), enc_tell);
        assert_eq!(&out[..frame_length], &padded[..frame_length]);
        assert!(out[frame_length..].iter().all(|&q| q == 0));
    }

    #[test]
    fn round_trip_all_frame_lengths_and_types() {
        let frame_lengths = [80usize, 120, 160, 240, 320];
        let mut rng = XorShift(0xC0FF_EE02);
        for &frame_length in &frame_lengths {
            for signal_type in 0..3i32 {
                for quant_offset_type in 0..2i32 {
                    for case in 0..12 {
                        let mut pulses = vec![0i16; frame_length];
                        let density = 1 + (case % 3);
                        for p in pulses.iter_mut() {
                            if rng.next_range(0, density * 3) == 0 {
                                let mag = rng.next_range(1, if case % 4 == 3 { 128 } else { 8 });
                                *p = if rng.next_u32() & 1 == 0 { mag } else { -mag } as i16;
                            }
                        }
                        assert_round_trip(&pulses, frame_length, signal_type, quant_offset_type);
                    }
                }
            }
        }
    }

    #[test]
    fn round_trip_extremes() {
        /* All-zero (rate level + zero counts, no signs) and
         * saturated-magnitude (many LSB shifts) vectors. */
        assert_round_trip(&[0i16; 320], 320, 1, 0);
        assert_round_trip(&[0i16; 160], 160, 2, 1);

        let dense: Vec<i16> = (0..320)
            .map(|i| (if i % 2 == 0 { 1 } else { -1 }) * (127 - (i % 9)) as i16)
            .collect();
        assert_round_trip(&dense, 320, 2, 0);
        assert_round_trip(&dense[..120], 120, 0, 1);

        /* ±3 block: shell count 12 + two LSB refinements per sample. */
        let lsb: Vec<i16> = (0..16).map(|i| if i % 2 == 0 { 3 } else { -3 }).collect();
        assert_round_trip(&lsb, 160, 2, 0);
    }
}
