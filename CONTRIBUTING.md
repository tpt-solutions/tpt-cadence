# Contributing to tpt-cadence

Thank you for helping build a pure-Rust, permissively licensed audio codec suite.

## Licensing Terms

tpt-cadence is dual-licensed under MIT OR Apache-2.0. By contributing code, you agree that:

1. **Your code will be dual-licensed** under the same MIT OR Apache-2.0 terms, and you have
   the right to grant that license.
2. **You will not introduce any dependency** whose tree contains GPL, LGPL, AGPL, or MPL
   code. This is enforced automatically by `cargo-deny` in CI (`cargo deny check licenses`).
3. All license headers/crate metadata must carry `license = "MIT OR Apache-2.0"`.

Before opening a PR, run:

```sh
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo deny check licenses
```

## Conformance-Test Requirement

**Every new decoder must include bit-exact conformance tests against a reference
implementation before it can be marked stable.** A PR adding or extending a decoder
without conformance coverage will not be merged.

Requirements:

- Tests live in the crate's `tests/conformance.rs`.
- Reference vectors come from the format's official test suite where one exists
  (FLAC → official FLAC test suite, AAC → ITU-T vectors, Opus → RFC 6716 vectors,
  MP3 → mpg123 conformance streams). Uncompressed formats may generate vectors
  programmatically.
- Cross-check against FFmpeg via the shared harness in `tpt-av-cadence-test-utils`
  (`assert_bit_exact_vs_ffmpeg`). The harness skips gracefully when FFmpeg is not
  installed locally, but CI compares against a pinned FFmpeg build.
- For lossless formats, verify the decoded output against the stream's embedded
  checksum (e.g., FLAC STREAMINFO MD5) in addition to reference vectors.

## Real-Time Safety Contract

All decoders implement `tpt_av_cadence_core::Decoder`. The contract is non-negotiable:

- **All heap allocation happens in `init()`/`open()`.** After that, `decode()` must be:
  - allocation-free (no `Vec::push` growth, no `format!`, no `Box::new`),
  - lock-free (no mutexes; no contended atomics),
  - panic-free (no `unwrap`/`expect`/slice-index panics — return `Result` instead).
- `decode()` writes only into the caller-provided buffer.
- `seek()` MAY allocate and block; it is not real-time safe and that is fine.

Reviewers will reject allocation or fallible-unwrapping inside decode paths.

## Fuzzing

Every parser must be fuzzed (`cargo-fuzz` targets in `fuzz/`, property tests with
`proptest` in each crate's test suite) and must provably never panic on malformed
input. Include a `proptest` never-panic test for your parser in your PR.

## Code Style

- `rustfmt` defaults; no custom formatting.
- Clippy clean (`-D warnings`).
- Prefer explicit `Result` returns over panicking operations, especially in
  decode paths.

## Reporting Bugs

Open a GitHub issue with a minimal reproducer (ideally a malformed or valid file
that decodes incorrectly). Security-sensitive bugs (panics on hostile input in
audio-thread paths) — please mark the issue clearly.
