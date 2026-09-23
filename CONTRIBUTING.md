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

## Licensing

`tpt-cadence` is dual-licensed under MIT OR Apache-2.0.
