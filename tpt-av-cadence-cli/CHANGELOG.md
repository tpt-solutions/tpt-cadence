# Changelog

All notable changes to `tpt-av-cadence-cli` will be documented in this file.

This crate is `publish = false`. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Added

- `cadence` binary with two subcommands: `info <file>` prints stream metadata,
  and `decode <file> [-o out.wav]` writes 16-bit PCM WAV, or raw interleaved
  `f32` on stdout when `-o` is omitted.
- Format auto-detection from the file extension. Ogg files are sniffed to tell
  Vorbis from Opus.
- Per-format Cargo features (`wav`, `aiff`, `flac`, `mp3`, `aac`, `opus`,
  `vorbis`). All are on by default.

### Changed

- Output is buffered in bulk. The `f32` to `i16` conversion now rounds instead
  of truncating. This is an intentional 1-LSB change in decoded WAV output
  (performance pass, 2026-10-06).
