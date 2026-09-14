//! # tpt-av-cadence-test-utils
//!
//! Shared conformance-testing harness for the `tpt-cadence` codec crates.
//!
//! - [`reference`]: decode a file with FFmpeg (CLI subprocess) and assert the
//!   suite's decoder output is bit-exact against it.
//! - [`fuzz`]: deterministic pseudo-random generators and byte-mutation
//!   helpers for never-panic property tests.
//! - [`md5`]: RFC 1321 MD5, used to verify FLAC's embedded STREAMINFO
//!   checksum and available for any other conformance need.

pub mod fuzz;
pub mod md5;
pub mod reference;
