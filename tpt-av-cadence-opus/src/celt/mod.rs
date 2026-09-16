//! CELT decoder internals (`celt/*` in libopus 1.5.2).
//!
//! Ported from the reference implementation with the same float32 operation
//! order so output is bit-exact with libopus built without `-ffast-math`.
//!
//! Implemented:
//! - [`tables`]: extracted static-mode 48 kHz tables (window, FFT bitrev +
//!   twiddles, MDCT twiddles, band/allocation tables, CWRS + pulse caches).
//! - [`fft`]: the kiss FFT kernel (4 substates, radix 2/3/4/5 butterflies).
//! - [`mdct`]: backward MDCT (synthesis) with caller-provided scratch.
//! - [`cwrs`]: PVQ pulse-vector decoding over the static U(N,K) table.
//! - [`math`]: float-build mathops (`isqrt32`, `celt_exp2`, `fast_atan2f`).
//! - [`laplace`]: Laplace-coded integers (coarse energies).
//! - [`rate`]: per-band pulse/fine-bit allocation with skip/intensity/
//!   dual-stereo signaling.
//! - [`quant_bands`]: coarse/fine/final band-energy unquantization.
//! - [`vq`]: pyramid vector quantization decode, spread rotation,
//!   renormalization.
//! - [`bands`]: `quant_all_bands` recursion, denormalization,
//!   anti-collapse.
//! - [`pitch`]: pitch postfilter + PLC pitch search.
//! - [`celt_lpc`]: LPC analysis/filtering for packet-loss concealment.
//! - [`decoder`]: the full `CeltDecoder` assembly (normal frames, DTX and
//!   pitch-based PLC, deemphasis).
//!
//! Future: SILK layer, hybrid integration, conformance vs official vectors.

pub mod bands;
pub mod celt_lpc;
pub mod cwrs;
pub mod decoder;
pub mod fft;
pub mod laplace;
pub mod math;
pub mod mdct;
pub mod pitch;
pub mod quant_bands;
pub mod rate;
pub mod tables;
pub mod vq;

pub use decoder::CeltDecoder;
