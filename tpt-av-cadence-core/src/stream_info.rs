//! The `StreamInfo` metadata struct.

use crate::error::{CadenceError, Result};
use crate::format::{ChannelLayout, Format};

/// Metadata about a decoded audio stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamInfo {
    /// Sample rate in Hz (e.g., 44100, 48000, 96000).
    pub sample_rate: u32,
    /// Number of audio channels.
    pub channels: u16,
    /// Channel layout (e.g., Stereo, 5.1, Mono).
    pub channel_layout: ChannelLayout,
    /// Original bit depth of the source (e.g., 16, 24, 32).
    pub bit_depth: u16,
    /// Total number of sample frames, if known.
    /// `None` for live streams or formats without frame counts.
    pub total_frames: Option<u64>,
    /// The source audio format.
    pub format: Format,
}

impl StreamInfo {
    /// Builds a `StreamInfo`, deriving the channel layout from the channel
    /// count via [`ChannelLayout::from_channel_count`].
    pub fn new(format: Format, sample_rate: u32, channels: u16, bit_depth: u16) -> Self {
        StreamInfo {
            sample_rate,
            channels,
            channel_layout: ChannelLayout::from_channel_count(channels),
            bit_depth,
            total_frames: None,
            format,
        }
    }

    /// Sanity-checks the metadata: positive sample rate, at least one channel,
    /// and a plausible bit depth.
    pub fn validate(&self) -> Result<()> {
        if self.sample_rate == 0 {
            return Err(CadenceError::InvalidFormat(format!(
                "invalid sample rate {} (must be non-zero)",
                self.sample_rate
            )));
        }
        if self.channels == 0 {
            return Err(CadenceError::InvalidFormat(
                "invalid channel count 0 (must be at least 1)".to_string(),
            ));
        }
        if self.bit_depth == 0 || self.bit_depth > 64 {
            return Err(CadenceError::InvalidFormat(format!(
                "invalid bit depth {} (must be in 1..=64)",
                self.bit_depth
            )));
        }
        Ok(())
    }
}
