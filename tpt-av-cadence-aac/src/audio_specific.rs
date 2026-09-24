//! AudioSpecificConfig parsing (ISO/IEC 14496-3 §1.6.2.1).

use crate::CadenceError;
use crate::Result;

/// Parsed AudioSpecificConfig (AAC-LC with optional explicit HE-AAC signaling).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioSpecificConfig {
    /// Explicit-signaling audio object type (2 = AAC-LC, 5 = HE-AAC,
    /// 29 = HE-AACv2). For escape-coded values this is the decoded value.
    pub object_type: u8,
    /// Sampling-frequency index used by the AAC-LC core transform.
    pub sampling_frequency_index: u8,
    pub channel_configuration: u8,
    /// Output sample rate after explicit SBR doubling, when signaled.
    pub extension_sampling_frequency_index: Option<u8>,
    /// True when AOT 29 explicitly signals HE-AACv2/PS in the container.
    /// The SBR payload still has to be present in the raw data blocks.
    pub ps_signaled: bool,
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
    /// Returns the effective output sample rate. Explicit SBR signaling
    /// selects the extension frequency; otherwise this is the core rate.
    pub fn sample_rate(&self) -> Result<u32> {
        let index = self
            .extension_sampling_frequency_index
            .unwrap_or(self.sampling_frequency_index);
        crate::adts::SAMPLING_FREQUENCIES
            .get(index as usize)
            .copied()
            .ok_or_else(|| {
                CadenceError::UnsupportedFeature(
                    "ASC sampling frequency index is reserved".to_string(),
                )
            })
    }

    fn core_sample_rate(&self) -> Result<u32> {
        crate::adts::SAMPLING_FREQUENCIES
            .get(self.sampling_frequency_index as usize)
            .copied()
            .ok_or_else(|| {
                CadenceError::UnsupportedFeature(
                    "ASC core sampling frequency index is reserved".to_string(),
                )
            })
    }

    /// Parses an AudioSpecificConfig from an MP4 `esds` DecoderSpecificInfo.
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let mut br = crate::bitreader::BitReader::new(bytes);
        let object_type = read_audio_object_type(&mut br);
        let explicit_he = matches!(object_type, 5 | 29);

        if explicit_he {
            let core_index = br.read_bits(4) as u8;
            if core_index == 0x0f {
                return Err(CadenceError::UnsupportedFeature(
                    "explicit ASC core sampling frequency is not supported".to_string(),
                ));
            }
            let channel_configuration = br.read_bits(4) as u8;
            let extension_index = br.read_bits(4) as u8;
            if extension_index == 0x0f {
                return Err(CadenceError::UnsupportedFeature(
                    "explicit ASC extension sampling frequency is not supported".to_string(),
                ));
            }
            let extension_type = read_audio_object_type(&mut br);
            if extension_type != 5 {
                return Err(CadenceError::UnsupportedFeature(format!(
                    "HE-AAC extension audio object type {extension_type} is not SBR (5)"
                )));
            }
            if extension_index == 0x0f
                || extension_index >= crate::adts::SAMPLING_FREQUENCIES.len() as u8
            {
                return Err(CadenceError::UnsupportedFeature(
                    "HE-AAC extension sampling frequency index is reserved".to_string(),
                ));
            }
            let config = Self {
                object_type,
                sampling_frequency_index: core_index,
                channel_configuration,
                extension_sampling_frequency_index: Some(extension_index),
                ps_signaled: object_type == 29,
                program_config: None,
            };
            config.core_sample_rate()?;
            config.sample_rate()?;
            read_ga_specific_config(&mut br)?;
            // sbrPresentFlag is signalled as zero to indicate explicit
            // backwards-compatible SBR signaling. A value of one means the
            // stream explicitly says SBR is absent, contradicting AOT 5/29.
            if br.read_bit() {
                return Err(CadenceError::CorruptData(
                    "explicit HE-AAC config disables its signaled SBR extension".to_string(),
                ));
            }
            return Ok(config);
        }

        if object_type != 2 {
            return Err(CadenceError::UnsupportedFeature(format!(
                "audio object type {object_type} is not AAC-LC (2) or explicit HE-AAC (5/29)"
            )));
        }
        let sampling_frequency_index = br.read_bits(4) as u8;
        let channel_configuration = br.read_bits(4) as u8;
        read_ga_specific_config(&mut br)?;

        let program_config = if channel_configuration == 0 {
            Some(parse_program_config_element(&mut br)?)
        } else {
            None
        };

        Ok(Self {
            object_type,
            sampling_frequency_index,
            channel_configuration,
            extension_sampling_frequency_index: None,
            ps_signaled: false,
            program_config,
        })
    }
}

fn read_audio_object_type(br: &mut crate::bitreader::BitReader<'_>) -> u8 {
    let object_type = br.read_bits(5) as u8;
    if object_type == 31 {
        32 + br.read_bits(6) as u8
    } else {
        object_type
    }
}

fn read_ga_specific_config(br: &mut crate::bitreader::BitReader<'_>) -> Result<()> {
    if br.read_bits(1) != 0 {
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
    Ok(())
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
        br.read_bits(3);
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
    br.skip_bits(4 * num_assoc + 5 * num_cc);
    br.byte_align();
    let comment_bytes = br.read_bits(8) as usize;
    br.skip_bytes(comment_bytes);
    Ok(AacPcePlan {
        entries: plan,
        class_counts: [num_front, num_side, num_back, num_lfe],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packed(fields: &[(u32, u32)]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut accumulator = 0u32;
        let mut count = 0u32;
        for &(value, width) in fields {
            accumulator = (accumulator << width) | value;
            count += width;
            while count >= 8 {
                count -= 8;
                out.push((accumulator >> count) as u8);
            }
        }
        if count > 0 {
            out.push((accumulator << (8 - count)) as u8);
        }
        out
    }

    fn he_aac_fields(
        object_type: u32,
        core: u32,
        channels: u32,
        extension: u32,
        sbr_flag: u32,
    ) -> Vec<u8> {
        packed(&[
            (object_type, 5),
            (core, 4),
            (channels, 4),
            (extension, 4),
            (5, 5),
            (0, 1),
            (0, 1),
            (0, 1),
            (sbr_flag, 1),
        ])
    }

    #[test]
    fn parses_explicit_he_aac_signaling() {
        let config = AudioSpecificConfig::parse(&he_aac_fields(5, 4, 1, 3, 0)).unwrap();
        assert_eq!(config.object_type, 5);
        assert_eq!(config.sampling_frequency_index, 4);
        assert_eq!(config.extension_sampling_frequency_index, Some(3));
        assert_eq!(config.channel_configuration, 1);
        assert!(!config.ps_signaled);
        assert_eq!(config.sample_rate().unwrap(), 48_000);
    }

    #[test]
    fn parses_explicit_heaacv2_metadata() {
        let config = AudioSpecificConfig::parse(&he_aac_fields(29, 4, 1, 3, 0)).unwrap();
        assert_eq!(config.object_type, 29);
        assert!(config.ps_signaled);
        assert_eq!(config.sample_rate().unwrap(), 48_000);
    }

    #[test]
    fn rejects_invalid_explicit_heaac_fields() {
        for (core, extension) in [(15, 3), (4, 15)] {
            let err =
                AudioSpecificConfig::parse(&he_aac_fields(5, core, 1, extension, 0)).unwrap_err();
            assert!(matches!(err, CadenceError::UnsupportedFeature(_)));
        }
        let err = AudioSpecificConfig::parse(&he_aac_fields(5, 4, 1, 3, 1)).unwrap_err();
        assert!(matches!(err, CadenceError::CorruptData(_)));
    }

    #[test]
    fn aac_lc_parsing_is_unchanged() {
        let config = AudioSpecificConfig::parse(&[0x12, 0x08]).unwrap();
        assert_eq!(config.object_type, 2);
        assert_eq!(config.sampling_frequency_index, 4);
        assert_eq!(config.extension_sampling_frequency_index, None);
        assert!(!config.ps_signaled);
        assert_eq!(config.sample_rate().unwrap(), 44_100);
    }
}
