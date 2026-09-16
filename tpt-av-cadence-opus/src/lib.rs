//! # tpt-av-cadence-opus
//!
//! Opus decoder (IETF RFC 6716) for the `tpt-cadence` suite — in progress.
//!
//! Implemented so far:
//! - [`packet`]: complete packet framing (TOC, codes 0–3, padding, DTX,
//!   configuration tables).
//! - [`range`]: the bit-exact range decoder from §4.1 (with a matching
//!   §5.1 encoder used by tests), including icdf, logp bits, raw bits,
//!   and uniformly-distributed integers.
//! - [`celt`]: the full CELT layer — kiss FFT, backward MDCT, CWRS/PVQ,
//!   band allocation, energy unquantization, the band recursion
//!   (`quant_all_bands`), anti-collapse, the pitch postfilter, deemphasis,
//!   and packet-loss concealment, assembled into [`celt::CeltDecoder`].
//!
//! Future: the SILK layer, hybrid mode integration, and a top-level
//! `Decoder` impl over `packet` + SILK/CELT (tracked in todo.md).
//!
//! See DESIGN.md §4 for the architecture.

pub mod celt;
pub mod packet;
pub mod range;

pub use packet::{Bandwidth, FrameDuration, Mode, Packet, Toc};
pub use range::{RangeDecoder, RangeEncoder};

// Re-exported so the celt modules can refer to `crate::{CadenceError,
// Result}` like the other decoder crates do.
pub use tpt_av_cadence_core::{CadenceError, Result};
