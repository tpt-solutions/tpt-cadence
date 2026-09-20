//! AudioSpecificConfig parsing (ISO/IEC 14496-3 §1.6.2.1).

use crate::CadenceError;
use crate::Result;

/// Parsed AudioSpecificConfig (AAC-LC subset).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioSpecificConfig {
    /// Audio object type (2 = AAC-LC).
    pub object_type: u8,
    pub sampling_frequency_index: u8,
    pub channel_configuration: u8,
    /// Program Config Element carried in the ASC itself (present exactly
    /// when `channel_configuration == 0`). Channel configuration 0 streams
    /// are shaped by this plan; the channel count is its summed channel
    /// total.
    pub program_config: Option<AacPcePlan>,
}

/// A parsed program config element: `(element type, tag)` entries in
/// declaration order — type 0 = SCE, 1 = CPE, 2 = LFE — plus the element
/// counts per position class (front, side, back, LFE).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AacPcePlan {
    pub entries: Vec<(u8, u8)>,
    pub class_counts: [usize; 4],
}

impl AacPcePlan {
    /// Total number of channels the plan declares.
    pub fn channel_count(&self) -> usize {
        self.entries
            .iter()
            .map(|&(ty, _)| 1 + usize::from(ty == PCE_CPE))
            .sum()
    }
}

impl AudioSpecificConfig {
    pub fn sample_rate(&self) -> Result<u32> {
        crate::adts::SAMPLING_FREQUENCIES
            .get(self.sampling_frequency_index as usize)
            .copied()
            .ok_or_else(|| {
                CadenceError::UnsupportedFeature(
                    "ASC sampling frequency index is reserved".to_string(),
                )
            })
    }

    /// Parses the config from bytes (e.g. the `esds` DecoderSpecificInfo of
    /// an MP4 track). Only audio object type 2 (AAC-LC) is supported.
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let mut br = crate::bitreader::BitReader::new(bytes);
        let mut object_type = br.read_bits(5) as u8;
        if object_type == 31 {
            object_type = 32 + br.read_bits(6) as u8;
        }
        if object_type == 5 || object_type == 29 {
            return Err(CadenceError::UnsupportedFeature(
                "HE-AAC (SBR/PS) explicit signaling is not supported".to_string(),
            ));
        }
        if object_type != 2 {
            return Err(CadenceError::UnsupportedFeature(format!(
                "audio object type {object_type} is not AAC-LC (2)"
            )));
        }
        let sampling_frequency_index = br.read_bits(4) as u8;
        let channel_configuration = br.read_bits(4) as u8;

        // GASpecificConfig
        let frame_length_flag = br.read_bits(1);
        if frame_length_flag != 0 {
            return Err(CadenceError::UnsupportedFeature(
                "960/480-sample frames are not supported".to_string(),
            ));
        }
        if br.read_bits(1) != 0 {
            return Err(CadenceError::UnsupportedFeature(
                "dependsOnCoreCoder is not supported".to_string(),
            ));
        }
        if br.read_bits(1) != 0 {
            return Err(CadenceError::UnsupportedFeature(
                "ASC extension flag is not supported".to_string(),
            ));
        }

        // Channel configuration 0: the program config element follows the
        // GASpecificConfig in the ASC itself and defines the channel plan.
        let program_config = if channel_configuration == 0 {
            Some(parse_program_config_element(&mut br)?)
        } else {
            None
        };

        Ok(AudioSpecificConfig {
            object_type,
            sampling_frequency_index,
            channel_configuration,
            program_config,
        })
    }
}

const PCE_SCE: u8 = 0;
const PCE_CPE: u8 = 1;
const PCE_LFE: u8 = 2;

/// program_config_element as carried in the ASC (ISO/IEC 14496-3 4.4.4):
/// tag, object type, sampling index, then front/side/back/LFE channel
/// element lists, associated-data and coupling tag lists, byte alignment,
/// and a comment field. Returns the (type, tag) plan plus the element
/// counts per position class (front, side, back, LFE).
fn parse_program_config_element(
    br: &mut crate::bitreader::BitReader,
) -> Result<AacPcePlan, CadenceError> {
    let _instance_tag = br.read_bits(4);
    let _object_type = br.read_bits(2);
    let _sampling_frequency_index = br.read_bits(4) as u8;
    let num_front = br.read_bits(4) as usize;
    let num_side = br.read_bits(4) as usize;
    let num_back = br.read_bits(4) as usize;
    let num_lfe = br.read_bits(2) as usize;
    let num_assoc = br.read_bits(3) as usize;
    let num_cc = br.read_bits(4) as usize;
    let mono_mixdown = br.read_bit();
    if mono_mixdown {
        br.read_bits(4);
    }
    let stereo_mixdown = br.read_bit();
    if stereo_mixdown {
        br.read_bits(4);
    }
    if br.read_bit() {
        br.read_bits(3); // matrix_mixdown_idx + pseudo_surround_enable
    }

    let mut plan = Vec::new();
    for (count, ty) in [
        (num_front, PCE_SCE),
        (num_side, PCE_SCE),
        (num_back, PCE_SCE),
    ] {
        for _ in 0..count {
            let is_cpe = br.read_bit();
            let tag = br.read_bits(4) as u8;
            plan.push((if is_cpe { PCE_CPE } else { ty }, tag));
        }
    }
    for _ in 0..num_lfe {
        let tag = br.read_bits(4) as u8;
        plan.push((PCE_LFE, tag));
    }
    // Associated-data (4-bit) and coupling-element (5-bit) tag lists are
    // declarations only; actual elements appear in the raw data blocks.
    br.skip_bits(4 * num_assoc + 5 * num_cc);
    br.byte_align();
    let comment_bytes = br.read_bits(8) as usize;
    br.skip_bytes(comment_bytes);
    Ok(AacPcePlan {
        entries: plan,
        class_counts: [num_front, num_side, num_back, num_lfe],
    })
}
