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
//!
//! The SILK and CELT decoding kernels are future work tracked in todo.md;
//! see DESIGN.md §4 for the architecture they will implement.

pub mod packet;
pub mod range;

pub use packet::{Bandwidth, FrameDuration, Mode, Packet, Toc};
pub use range::{RangeDecoder, RangeEncoder};
