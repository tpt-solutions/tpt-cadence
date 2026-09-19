//! # tpt-av-cadence-opus
//!
//! Opus decoder (IETF RFC 6716) for the `tpt-cadence` suite.
//!
//! Implemented:
//! - [`packet`]: complete packet framing (TOC, codes 0–3, padding, DTX,
//!   configuration tables).
//! - [`range`]: the bit-exact range decoder from §4.1 (with a matching
//!   §5.1 encoder used by tests), including icdf, logp bits, raw bits,
//!   and uniformly-distributed integers.
//! - [`celt`]: the full CELT layer — kiss FFT, backward MDCT, CWRS/PVQ,
//!   band allocation, energy unquantization, the band recursion
//!   (`quant_all_bands`), anti-collapse, the pitch postfilter, deemphasis,
//!   and packet-loss concealment, assembled into [`celt::CeltDecoder`].
//! - [`silk`]: the full SILK layer — entropy tables, side-info indices,
//!   stereo mid/side prediction, NLSF/LPC decode, pitch/LTP, excitation,
//!   the synthesis core, PLC + CNG, and the resampler, assembled into
//!   [`silk::decoder::SilkDecoder`].
//! - [`decoder`]: the top-level [`OpusDecoder`] — a port of libopus's
//!   `opus_decoder.c` state machine driving SILK-only, CELT-only, and
//!   hybrid packets with mode-transition crossfades, 5 ms CELT redundancy
//!   frames, hybrid low-band mixing, and DTX/PLC. (The older
//!   `decode_celt_only_packet`/`decode_silk_only_packet` entry points
//!   remain for single-mode use; cross-mode state continuity needs
//!   `OpusDecoder`.)
//! - [`ogg_opus`]: the Ogg Opus (RFC 7845) container — `OpusHead`/
//!   `OpusTags` parsing, pre-skip/end-trim granule bookkeeping, output
//!   gain, and `OggOpusDecoder`/`OggOpusReader` implementing the core
//!   [`Decoder`](tpt_av_cadence_core::Decoder)/
//!   [`FormatReader`](tpt_av_cadence_core::FormatReader) traits over a
//!   `.opus`/`.ogg` byte source.
//!
//! Conformance: all 12 official RFC 6716 test vectors decode with a 100%
//! `final_range` match on every packet; the SILK-only vectors are
//! bit-exact in PCM, and the rest reach 37–110 dB SNR against a live
//! libopus 1.5.2 build (see `todo.md` for the accepted float-ULP
//! residual). Run `tests/conformance.rs` with `OPUS_TESTVECTORS_DIR` set.
//!
//! See DESIGN.md §4 for the architecture.

pub mod celt;
pub(crate) mod debug;
pub mod decoder;
pub mod ogg_opus;
pub mod packet;
pub mod range;
pub mod silk;

pub use decoder::{
    celt_end_band, celt_frame_size, decode_celt_only_packet, decode_silk_only_packet, OpusDecoder,
    OUTPUT_CHANNELS,
};
pub use ogg_opus::{OggOpusDecoder, OggOpusReader, OpusHead};
pub use packet::{Bandwidth, FrameDuration, Mode, Packet, Toc};
pub use range::{RangeDecoder, RangeEncoder};

// Re-exported so the celt modules can refer to `crate::{CadenceError,
// Result}` like the other decoder crates do.
pub use tpt_av_cadence_core::{CadenceError, Result};
