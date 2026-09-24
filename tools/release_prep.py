#!/usr/bin/env python3
"""Validate and prepare a local release patch without publishing anything.

The default mode is read-only validation. ``--prepare VERSION`` updates the
workspace version metadata and inserts a dated changelog heading in the local
checkout only. This tool never commits, tags, pushes, or publishes.
"""

from __future__ import annotations

import argparse
import datetime as dt
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
CARGO = ROOT / "Cargo.toml"
CHANGELOG = ROOT / "CHANGELOG.md"
VERSION_RE = re.compile(r"^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?$")


def workspace_version() -> str:
    text = CARGO.read_text(encoding="utf-8")
    match = re.search(r'^\[workspace\.package\]\s+.*?^version\s*=\s*"([^"]+)"', text, re.MULTILINE | re.DOTALL)
    if match is None:
        raise ValueError("Cargo.toml has no [workspace.package] version")
    return match.group(1)


def validate(version: str | None = None) -> str:
    current = workspace_version()
    if not VERSION_RE.fullmatch(current):
        raise ValueError(f"invalid workspace version: {current!r}")
    if version is not None and not VERSION_RE.fullmatch(version):
        raise ValueError(f"invalid requested version: {version!r}")
    changelog = CHANGELOG.read_text(encoding="utf-8")
    if "## [Unreleased]" not in changelog:
        raise ValueError("CHANGELOG.md has no [Unreleased] section")
    if changelog.count("## [Unreleased]") != 1:
        raise ValueError("CHANGELOG.md must contain exactly one [Unreleased] section")
    return current


def prepare(version: str) -> None:
    current = validate(version)
    if version == current:
        raise ValueError(f"requested version is already current: {version}")
    cargo = CARGO.read_text(encoding="utf-8")
    updated = re.sub(
        r'(^\[workspace\.package\].*?^version\s*=\s*")[^"]+(")',
        rf"\g<1>{version}\2",
        cargo,
        count=1,
        flags=re.MULTILINE | re.DOTALL,
    )
    updated = re.sub(r'(version\s*=\s*")[^"]+(")', rf"\g<1>{version}\2", updated)
    CARGO.write_text(updated, encoding="utf-8", newline="\n")

    changelog = CHANGELOG.read_text(encoding="utf-8")
    date = dt.date.today().isoformat()
    heading = f"## [{version}] - {date}"
    if heading in changelog:
        raise ValueError(f"changelog already contains {heading}")
    changelog = changelog.replace("## [Unreleased]", f"## [Unreleased]\n\n{heading}", 1)
    CHANGELOG.write_text(changelog, encoding="utf-8", newline="\n")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--check", action="store_true", help="validate only (default)")
    parser.add_argument("--prepare", metavar="VERSION", help="write a local release-prep patch")
    args = parser.parse_args()
    if args.prepare and args.check:
        parser.error("--check and --prepare are mutually exclusive")
    try:
        current = validate(args.prepare)
        if args.prepare:
            prepare(args.prepare)
            print(f"Prepared local release metadata for {args.prepare} (was {current}).")
            print("No commit, tag, push, or publish operation was performed.")
        else:
            print(f"Release metadata valid: {current}.")
        return 0
    except (OSError, ValueError) as error:
        print(f"release-prep: error: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
