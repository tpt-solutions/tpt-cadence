//! AudioSpecificConfig parsing (ISO/IEC 14496-3 §1.6.2.1).

use crate::CadenceError;
use crate::Result;

/// Parsed AudioSpecificConfig (AAC-LC subset).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioSpecificConfig {
    /// Audio object type (2 = AAC-LC).
    pub object_type: u8,
    pub sampling_frequency_index: u8,
    pub channel_configuration: u8,
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

        Ok(AudioSpecificConfig {
            object_type,
            sampling_frequency_index,
            channel_configuration,
        })
    }
}
