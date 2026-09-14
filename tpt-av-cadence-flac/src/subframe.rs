//! Subframe decoding: Constant, Verbatim, Fixed, and LPC subframes
//! (RFC 9639 §9.2).

use tpt_av_cadence_core::{CadenceError, Result};

use crate::lpc;
use crate::rice;
use crate::stream::BitReader;

/// Decodes one subframe of `block_size` samples into `block`.
///
/// `frame_bps` is the frame's bit depth; the subframe may declare wasted
/// bits, narrowing its own effective depth. On return the block holds the
/// reconstructed (waste-shifted) samples.
pub fn decode_subframe(
    br: &mut BitReader,
    block: &mut [i32],
    block_size: usize,
    frame_bps: u16,
) -> Result<()> {
    let padding = br.read_bits(1)?;
    if padding != 0 {
        return Err(CadenceError::CorruptData(
            "subframe header padding bit is not zero".to_string(),
        ));
    }

    let subframe_type = br.read_bits(6)?;
    let wasted_flag = br.read_bits(1)?;
    let wasted: u32 = if wasted_flag == 1 {
        // k wasted bits are coded as k-1 zeroes then a one.
        br.read_unary()?.wrapping_add(1)
    } else {
        0
    };
    if wasted >= frame_bps as u32 {
        return Err(CadenceError::CorruptData(format!(
            "subframe wastes {wasted} bits but the frame is only {frame_bps} bits deep"
        )));
    }
    let sub_bps = frame_bps - wasted as u16;

    match subframe_type {
        0b000000 => {
            // CONSTANT
            let value = br.read_signed(sub_bps as u32)? as i32;
            for slot in block[..block_size].iter_mut() {
                *slot = value;
            }
        }
        0b000001 => {
            // VERBATIM
            for slot in block[..block_size].iter_mut() {
                *slot = br.read_signed(sub_bps as u32)? as i32;
            }
        }
        0b001000..=0b001100 => {
            // FIXED, order = low 3 bits (0..=4)
            let order = (subframe_type & 0x7) as usize;
            read_warmup(br, block, order, sub_bps as u32)?;
            rice::decode_residual(br, block, block_size, order)?;
            lpc::restore_fixed(&mut block[..block_size], order);
        }
        0b100000..=0b111111 => {
            // LPC, order = low 5 bits + 1 (1..=32)
            let order = ((subframe_type & 0x1F) as usize) + 1;
            read_warmup(br, block, order, sub_bps as u32)?;

            let precision_code = br.read_bits(4)?;
            if precision_code == 0b1111 {
                return Err(CadenceError::CorruptData(
                    "LPC coefficient precision code 0b1111 is invalid".to_string(),
                ));
            }
            let precision = precision_code as u32 + 1;

            let shift = br.read_signed(5)?;
            if shift < 0 {
                return Err(CadenceError::CorruptData(format!(
                    "LPC quantized coefficient shift {shift} is negative"
                )));
            }

            let mut coefs = [0i64; 32];
            for coef in coefs.iter_mut().take(order) {
                *coef = br.read_signed(precision)?;
            }

            rice::decode_residual(br, block, block_size, order)?;
            lpc::restore_lpc(&mut block[..block_size], &coefs[..order], shift as u32);
        }
        other => {
            return Err(CadenceError::CorruptData(format!(
                "reserved subframe type 0b{other:06b}"
            )));
        }
    }

    if wasted > 0 {
        for slot in block[..block_size].iter_mut() {
            *slot = slot.wrapping_shl(wasted);
        }
    }
    Ok(())
}

/// Reads the predictor order's warm-up samples (unencoded).
fn read_warmup(br: &mut BitReader, block: &mut [i32], order: usize, sub_bps: u32) -> Result<()> {
    if block.len() < order {
        return Err(CadenceError::CorruptData(
            "block size smaller than the predictor order".to_string(),
        ));
    }
    for slot in block[..order].iter_mut() {
        *slot = br.read_signed(sub_bps)? as i32;
    }
    Ok(())
}
