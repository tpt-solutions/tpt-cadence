//! # tpt-av-cadence-vorbis
//!
//! Ogg Vorbis decoder for the `tpt-cadence` suite — not yet implemented.
//!
//! This crate is scaffolded (see DESIGN.md §3 for the planned module tree)
//! and tracked in todo.md. Coming work:
//!
//! - Ogg page layer (capture, CRC, page assembly) as the container front end
//! - Header decoding (identification/comment/setup) and Vorbis codebooks
//! - Floor/residue decoding, MDCT synthesis, and channel coupling
//! - Conformance tests against reference Vorbis streams
