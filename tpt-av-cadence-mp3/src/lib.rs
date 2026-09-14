//! # tpt-av-cadence-mp3
//!
//! MPEG-1/2 audio Layer III decoder for the `tpt-cadence` suite — not yet implemented.
//!
//! This crate is scaffolded (see DESIGN.md §3 for the planned module tree)
//! and tracked in todo.md. Coming work:
//!
//! - Huffman table decoding and requantization
//! - Polyphase filterbank synthesis (32 subbands, IMDCT + windowing)
//! - Joint stereo (mid/side, intensity) processing
//! - Conformance tests against mpg123 reference streams
