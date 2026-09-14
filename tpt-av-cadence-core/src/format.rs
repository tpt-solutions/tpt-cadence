//! Format enums describing the source codec, sample encoding, and channel layout.

/// The source audio format (container/codec) of a stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Format {
    /// Headerless raw PCM (no container, parameters supplied by the caller).
    RawPcm,
    /// Microsoft RIFF/WAVE container.
    Wav,
    /// Audio IFF (AIFF / AIFF-C).
    Aiff,
    /// Free Lossless Audio Codec.
    Flac,
    /// Advanced Audio Coding, Low Complexity profile.
    Aac,
    /// Opus (IETF RFC 6716).
    Opus,
    /// MPEG-1/2 audio Layer III.
    Mp3,
    /// Ogg Vorbis.
    Vorbis,
}

/// The sample encoding of the source stream.
///
/// Output from [`crate::Decoder::decode`] is always interleaved `f32` regardless
/// of the source sample format; this enum describes what the *source* stores.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SampleFormat {
    /// 8-bit signed integer.
    ///
    /// Note: WAV stores 8-bit samples *unsigned*; the WAV decoder converts to
    /// signed before scaling, so this variant always means signed 8-bit.
    Int8,
    /// 16-bit signed integer.
    Int16,
    /// 24-bit signed integer.
    Int24,
    /// 32-bit signed integer.
    Int32,
    /// IEEE-754 32-bit float.
    Float32,
    /// IEEE-754 64-bit float.
    Float64,
}

impl SampleFormat {
    /// Bit depth of a single sample (e.g. 16, 24, 32).
    pub fn bit_depth(self) -> u16 {
        match self {
            SampleFormat::Int8 => 8,
            SampleFormat::Int16 => 16,
            SampleFormat::Int24 => 24,
            SampleFormat::Int32 => 32,
            SampleFormat::Float32 => 32,
            SampleFormat::Float64 => 64,
        }
    }

    /// Bytes occupied by a single sample in the byte stream.
    pub fn bytes_per_sample(self) -> u16 {
        match self {
            SampleFormat::Int8 => 1,
            SampleFormat::Int16 => 2,
            SampleFormat::Int24 => 3,
            SampleFormat::Int32 => 4,
            SampleFormat::Float32 => 4,
            SampleFormat::Float64 => 8,
        }
    }

    /// Whether the sample encoding is IEEE-754 floating point.
    pub fn is_float(self) -> bool {
        matches!(self, SampleFormat::Float32 | SampleFormat::Float64)
    }
}

/// Channel arrangement of a stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChannelLayout {
    /// 1 channel.
    Mono,
    /// 2 channels (left, right).
    Stereo,
    /// 3 channels: left, right, LFE (2.1).
    Surround21,
    /// 4 channels: front left, front right, back left, back right (quadraphonic).
    Quad,
    /// 6 channels: front L/R, center, LFE, surround L/R (5.1).
    Surround51,
    /// 8 channels: 5.1 plus two surround-back channels (7.1).
    Surround71,
    /// A stream with no well-defined speaker mapping.
    Unspecified { channels: u16 },
}

impl ChannelLayout {
    /// Number of channels this layout describes.
    pub fn channels(self) -> u16 {
        match self {
            ChannelLayout::Mono => 1,
            ChannelLayout::Stereo => 2,
            ChannelLayout::Surround21 => 3,
            ChannelLayout::Quad => 4,
            ChannelLayout::Surround51 => 6,
            ChannelLayout::Surround71 => 8,
            ChannelLayout::Unspecified { channels } => channels,
        }
    }

    /// Maps a bare channel count to the canonical layout when one exists,
    /// falling back to [`ChannelLayout::Unspecified`].
    pub fn from_channel_count(channels: u16) -> Self {
        match channels {
            1 => ChannelLayout::Mono,
            2 => ChannelLayout::Stereo,
            3 => ChannelLayout::Surround21,
            4 => ChannelLayout::Quad,
            6 => ChannelLayout::Surround51,
            8 => ChannelLayout::Surround71,
            other => ChannelLayout::Unspecified { channels: other },
        }
    }
}
