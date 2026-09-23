#!/usr/bin/env python3
"""Generates a Markdown conformance dashboard from the crates' own test
output.

This doesn't invent new measurements — every decoder crate's conformance
test suite already prints its own SNR/bit-exactness numbers (see e.g.
`tpt-av-cadence-aac/tests/conformance.rs`'s `eprintln!("{what}: SNR={snr}
dB, ...")` calls) as part of running normally. This script runs each
crate's test suite with `--nocapture`, scrapes those lines with a small set
of regexes covering the patterns already in use across the workspace, and
assembles the results into one report instead of leaving them scattered
across separate `cargo test` runs' scrollback.

Usage:
    python3 tools/conformance_dashboard.py [--output FILE]

Run from the repository root. Requires the sibling `tpt-av-test` repo to be
checked out one directory up (see `tpt-av-cadence-test-utils/Cargo.toml`'s
path dependency) for the workspace to build at all.
"""

from __future__ import annotations

import argparse
import datetime
import re
import subprocess
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent

# (crate, cargo test args, human label)
CRATES = [
    ("tpt-av-cadence-wav", [], "WAV (bit-exact, no SNR — pass/fail only)"),
    ("tpt-av-cadence-aiff", [], "AIFF (bit-exact, no SNR — pass/fail only)"),
    ("tpt-av-cadence-flac", [], "FLAC (bit-exact via embedded MD5 — pass/fail only)"),
    ("tpt-av-cadence-mp3", ["--test", "conformance"], "MP3 (FFmpeg-oracle SNR, needs FFmpeg on PATH)"),
    ("tpt-av-cadence-aac", ["--test", "conformance"], "AAC-LC / HE-AAC (FFmpeg-oracle SNR)"),
    ("tpt-av-cadence-vorbis", ["--test", "conformance"], "Ogg Vorbis I (FFmpeg-oracle SNR)"),
]

# Regexes covering every "SNR" print style actually used in this workspace's
# test files today. Each must capture a label and a dB value.
SNR_PATTERNS = [
    re.compile(r"^(?P<label>.+?):\s*SNR=(?P<snr>-?[\d.]+)\s*dB", re.MULTILINE),
    re.compile(r"^(?P<label>.+?):\s*SNR\s+(?P<snr>-?[\d.]+)\s*dB", re.MULTILINE),
    re.compile(r"^(?P<label>\S+)\s+HE-AAC:\s*SNR=(?P<snr>-?[\d.]+)\s*dB", re.MULTILINE),
]

TEST_RESULT_RE = re.compile(
    r"test result: (?P<outcome>ok|FAILED)\. (?P<passed>\d+) passed; (?P<failed>\d+) failed;"
    r" (?P<ignored>\d+) ignored"
)


def run_crate(crate: str, extra_args: list[str]) -> str:
    cmd = ["cargo", "test", "-p", crate, "--release", *extra_args, "--", "--nocapture"]
    proc = subprocess.run(
        cmd,
        cwd=REPO_ROOT,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        timeout=1800,
    )
    return proc.stdout


def extract_snr_lines(output: str) -> list[tuple[str, float]]:
    found: list[tuple[str, float]] = []
    seen = set()
    for pattern in SNR_PATTERNS:
        for m in pattern.finditer(output):
            label = m.group("label").strip()
            snr = float(m.group("snr"))
            key = (label, snr)
            if key in seen:
                continue
            seen.add(key)
            found.append((label, snr))
    return found


def summarize_test_results(output: str) -> list[tuple[str, int, int, int]]:
    return [
        (m.group("outcome"), int(m.group("passed")), int(m.group("failed")), int(m.group("ignored")))
        for m in TEST_RESULT_RE.finditer(output)
    ]


def render_markdown(sections: list[tuple[str, str, list[tuple[str, float]], list[tuple[str, int, int, int]]]]) -> str:
    now = datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%d %H:%M UTC")
    lines = [
        "# Conformance dashboard",
        "",
        f"Generated {now} by `tools/conformance_dashboard.py`. Numbers are scraped from each",
        "crate's own conformance test output (see that crate's `tests/conformance.rs`) — this",
        "script doesn't measure anything independently, it just aggregates what's already printed.",
        "",
    ]
    for crate, label, snrs, results in sections:
        lines.append(f"## {label}")
        lines.append("")
        total_passed = sum(p for _, p, _, _ in results)
        total_failed = sum(f for _, _, f, _ in results)
        total_ignored = sum(i for _, _, _, i in results)
        lines.append(
            f"`cargo test -p {crate}`: **{total_passed} passed**, {total_failed} failed, "
            f"{total_ignored} ignored."
        )
        lines.append("")
        if snrs:
            lines.append("| Stream | SNR (dB) |")
            lines.append("| :--- | ---: |")
            for stream_label, snr in snrs:
                lines.append(f"| {stream_label} | {snr:.2f} |")
            lines.append("")
        else:
            lines.append(
                "_No per-stream SNR lines emitted by this crate's default test run — either it's a "
                "bit-exact crate that asserts equality directly instead of printing an SNR, or its "
                "FFmpeg-oracle comparison was skipped because FFmpeg isn't on `PATH` in this "
                "environment (check the raw test output for a \"skipping\"/\"FFmpeg not on PATH\" line)._"
            )
            lines.append("")
    return "\n".join(lines)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--output", default=str(REPO_ROOT / "CONFORMANCE.md"))
    args = parser.parse_args()

    sections = []
    for crate, extra_args, label in CRATES:
        print(f"==> running {crate}", file=sys.stderr)
        output = run_crate(crate, extra_args)
        snrs = extract_snr_lines(output)
        results = summarize_test_results(output)
        sections.append((crate, label, snrs, results))

    report = render_markdown(sections)
    Path(args.output).write_text(report)
    print(f"wrote {args.output}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
