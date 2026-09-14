//! Partitioned Rice residual decoding (RFC 9639 §9.2.7).

use tpt_av_cadence_core::{CadenceError, Result};

use crate::stream::BitReader;

/// Decodes a partitioned-Rice residual into `block[predictor_order..block_size]`.
///
/// The 2-bit coding method field selects plain Rice (4-bit parameter) or
/// Rice2 (5-bit parameter) per RFC 9639 §9.2.7.
pub fn decode_residual(
    br: &mut BitReader,
    block: &mut [i32],
    block_size: usize,
    predictor_order: usize,
) -> Result<()> {
    let method = br.read_bits(2)?;
    let param_bits: u32 = match method {
        0 => 4,
        1 => 5,
        _ => {
            return Err(CadenceError::CorruptData(format!(
                "reserved residual coding method {method}"
            )))
        }
    };
    let escape: u64 = if param_bits == 4 { 0xF } else { 0x1F };

    let partition_order = br.read_bits(4)? as usize;
    let part_count = 1usize << partition_order;
    if partition_order > 0 {
        if block_size % part_count != 0 {
            return Err(CadenceError::CorruptData(format!(
                "block size {block_size} is not divisible by 2^{partition_order} partitions"
            )));
        }
        if block_size / part_count == 0 || (block_size >> partition_order) < predictor_order {
            return Err(CadenceError::CorruptData(
                "partition too small to hold warm-up samples".to_string(),
            ));
        }
    } else if block_size < predictor_order {
        return Err(CadenceError::CorruptData(
            "block smaller than the predictor order".to_string(),
        ));
    }

    let mut pos = predictor_order;
    for part in 0..part_count {
        let count = if partition_order == 0 {
            block_size - predictor_order
        } else if part == 0 {
            (block_size >> partition_order) - predictor_order
        } else {
            block_size >> partition_order
        };

        let param = br.read_bits(param_bits)?;
        if param == escape {
            // Escaped partition: raw big-endian two's-complement samples
            // with the width given by the next 5 bits (0 bits => all zero).
            let raw_bits = br.read_bits(5)? as u32;
            for slot in &mut block[pos..pos + count] {
                *slot = br.read_signed(raw_bits)? as i32;
            }
        } else {
            let p = param as u32;
            for slot in &mut block[pos..pos + count] {
                let quotient = br.read_unary()?;
                // Guard the shift; a legitimate quotient never gets this large.
                if quotient as u64 >= (1u64 << (56 - p)) {
                    return Err(CadenceError::CorruptData(
                        "Rice quotient overflows the residual range".to_string(),
                    ));
                }
                let remainder = br.read_bits(p)?;
                let uval = (quotient as u64) << p | remainder;
                // Zigzag: 0 -> 0, 1 -> -1, 2 -> 1, 3 -> -2, …
                let sval = ((uval >> 1) as i64) ^ -((uval & 1) as i64);
                // Residuals use wrapping 32-bit semantics (valid streams
                // never wrap; hostile ones are caught by the frame CRC).
                *slot = sval as i32;
            }
        }
        pos += count;
    }
    debug_assert_eq!(pos, block_size);
    Ok(())
}
