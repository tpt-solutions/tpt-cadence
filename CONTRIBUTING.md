# Contributing to tpt-cadence

Thank you for your interest in `tpt-cadence`.

## This project does not accept code pull requests

`tpt-cadence` is developed solely by TPT Solutions. External pull requests that add or
change decoder/encoder logic, tests, or other crate code will not be reviewed or merged —
send a bug report or feature request instead (see below).

**Exception: examples and documentation.** Small pull requests limited to `examples/`
files, doc comments, README/CONTRIBUTING/DESIGN/CHANGELOG prose, or fixing a typo/broken
link are welcome and will be reviewed. Keep these PRs narrowly scoped to non-code-behavior
changes; anything touching `src/` decode/encode logic should go through an issue first.

## Reporting Bugs

Contributions are welcome in the form of **GitHub issues**:

- Bug reports — ideally with a minimal reproducer (a malformed or valid file that decodes
  incorrectly). Please clearly mark security-sensitive bugs (panics on hostile input in
  audio-thread paths).
- Feature requests.
- Questions about the project's design or roadmap (see [DESIGN.md](DESIGN.md) and
  [todo.md](todo.md)).

## Release preparation

Release preparation is manual and non-publishing. Run
`python tools/release_prep.py --check` to validate the current workspace
version and changelog. To generate a local patch, run
`python tools/release_prep.py --prepare VERSION`; the manual
`release-prep` GitHub Actions workflow performs the same operation and uploads
`Cargo.toml` and `CHANGELOG.md` as artifacts. The tooling never creates a
commit, tag, push, or crates.io publication.

## Licensing

`tpt-cadence` is dual-licensed under MIT OR Apache-2.0.
