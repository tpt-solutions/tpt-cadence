# crates.io publish tracker

Manual checklist for publishing the `tpt-av-cadence-*` crates to crates.io. Release
automation (`tools/release_prep.py`, `.github/workflows/release-prep.yml`) never publishes, so
every upload below is a deliberate manual step.

Workspace version: `0.1.0` (`[workspace.package]` in `Cargo.toml`). License: MIT OR Apache-2.0.

## Rules

1. Publish in the dependency order below. A crate can only be published after everything it
   depends on is live on crates.io.
2. Dry-run first: `cargo publish -p <crate> --dry-run`. Publish only after it passes.
3. After each publish, wait for the crates.io index to update before publishing dependents.
   Dependents' dry-runs will fail until their dependencies are live. That is expected.
4. Tick the row in the status table and add a log entry in the same commit.
5. Never publish a crate marked **Never** or **Hold**.

## Pre-flight (once, before the first upload)

- [ ] Working tree clean on `master`; CI green on the commit being released.
- [ ] `CHANGELOG.md` entry for `0.1.0` in each published crate (`tools/release_prep.py` validates the workspace changelog).
- [ ] `cargo test --workspace` passes; `cargo deny check licenses` passes.
- [ ] `cargo package -p <crate> --list` for each publishable crate shows the README, both LICENSE files, and CHANGELOG, and nothing from `fuzz/`, `tests/data/`, or other large fixtures that should not ship.
- [ ] Non-dev dependencies of every publishable crate carry a version (checked: all use `workspace = true`, which resolves to `0.1.0`). Path-only **dev**-dependencies are stripped by cargo at publish time.
- [ ] crates.io account has 2FA enabled; API token created with publish scope; `cargo login` done.
- [ ] Crate names are free or already owned by the project (`cargo search tpt-av-cadence` / crates.io lookup).

## Status

Order is the publish order. "Deps" lists internal crates that must be live first.

| # | Crate | Deps | Decision | Dry-run | Published | Date | Notes |
|---|-------|------|----------|---------|-----------|------|-------|
| 1 | `tpt-av-cadence-core` | none | **Publish** | [x] | [x] | 2026-11-04 | Traits, `StreamInfo`, error types. Everything else depends on it. |
| 2 | `tpt-av-cadence-ogg` | core | **Publish** | [x] | [x] | 2026-11-04 | Ogg container (RFC 3533). Needed by vorbis and opus. |
| 3 | `tpt-av-cadence-pcm` | core | **Publish** | [x] | [x] | 2026-11-04 | Raw headerless PCM. |
| 4 | `tpt-av-cadence-wav` | core | **Publish** | [x] | [x] | 2026-11-04 | RIFF/WAVE PCM and float. |
| 5 | `tpt-av-cadence-aiff` | core | **Publish** | [x] | [x] | 2026-11-04 | AIFF/AIFC. |
| 6 | `tpt-av-cadence-flac` | core | **Publish** | [x] | [x] | 2026-11-04 | FLAC decoder and encoder. |
| 7 | `tpt-av-cadence-vorbis` | core, ogg | **Publish** | [x] | [x] | 2026-11-04 | Ogg Vorbis I decoder and encoder. |
| 8 | `tpt-av-cadence-opus` | core, ogg | **Publish** | [x] | [x] | 2026-11-04 | Opus decoder and encoder, Ogg Opus. |
| 9 | `tpt-av-cadence-aac` | core | **Hold** | — | — | | Patent doubt. AAC encoder rejected 2026-09-26. See [On hold](#on-hold). |
| 10 | `tpt-av-cadence-mp3` | core | **Hold** | — | — | | Patent doubt. MP3 work frozen 2026-10-07; crate contains an encoder. See [On hold](#on-hold). |
| — | `tpt-av-cadence-cli` | all | **Never** | — | — | | `publish = false`. |
| — | `tpt-av-cadence-test-utils` | core | **Never** | — | — | | `publish = false`. Test harness only. |
| — | `tpt-av-cadence-wasm-demo` | all | **Never** | — | — | | `publish = false`. Smoke demo, not an API. |

Publish commands, one block per crate, in order:

```sh
cargo publish -p tpt-av-cadence-core --dry-run && cargo publish -p tpt-av-cadence-core
cargo publish -p tpt-av-cadence-ogg --dry-run && cargo publish -p tpt-av-cadence-ogg
cargo publish -p tpt-av-cadence-pcm --dry-run && cargo publish -p tpt-av-cadence-pcm
cargo publish -p tpt-av-cadence-wav --dry-run && cargo publish -p tpt-av-cadence-wav
cargo publish -p tpt-av-cadence-aiff --dry-run && cargo publish -p tpt-av-cadence-aiff
cargo publish -p tpt-av-cadence-flac --dry-run && cargo publish -p tpt-av-cadence-flac
cargo publish -p tpt-av-cadence-vorbis --dry-run && cargo publish -p tpt-av-cadence-vorbis
cargo publish -p tpt-av-cadence-opus --dry-run && cargo publish -p tpt-av-cadence-opus
```

Wait for the index to show each crate before running the next line that depends on it.

## Post-publish

- [ ] Tag the release: `git tag v0.1.0` and push the tag (only after all published rows are ticked).
- [ ] Confirm docs.rs built each crate.
- [ ] Smoke test from a scratch project outside the repo: `cargo add tpt-av-cadence-wav` and decode a file.
- [ ] Update the README maturity row, which currently says "0.1.0 packaging prepared but not yet published".
- [ ] Update `todo.md` to record the publish.

## On hold

**`tpt-av-cadence-aac`** and **`tpt-av-cadence-mp3`** are not published. The user has put both
on hold because their patent status is in doubt:

- AAC: the encoder was rejected on 2026-09-26. The decoder is still in the crate, and the
  AAC-related patent question is the same one that applies to the encoder.
- MP3: frozen on 2026-10-07. The crate contains both an encoder and a decoder.

Before either crate is published, the user needs to decide that the patent position is
acceptable. If they are published later, review the README and the decoder-only and encoder
wording first.

## Log

- 2026-11-04: Published 0.1.0 of core, ogg, pcm, wav, aiff, flac, vorbis, opus to crates.io (crates.io rate limit on new crates forced retries for flac, vorbis, opus). Tracker created. Eight crates marked for publish; `aac` and `mp3` on hold; `cli`, `test-utils`, and `wasm-demo` marked never. 
