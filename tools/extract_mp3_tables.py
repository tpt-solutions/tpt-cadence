#!/usr/bin/env python3
"""Extracts the ISO/IEC 11172-3 / 13818-3 Annex B table data from the
public-domain minimp3 decoder (https://github.com/lieff/minimp3) and emits
`tables.rs` for tpt-av-cadence-mp3.

The tables are standard, non-copyrightable data (ISO Annex B); minimp3 is
used only as the machine-readable source and is dedicated to the public
domain (CC0-equivalent). Run from the repo root after downloading:

    curl -s https://raw.githubusercontent.com/lieff/minimp3/master/minimp3.h -o /tmp/minimp3.h
    python tools/extract_mp3_tables.py /tmp/minimp3.h
"""

import re
import sys

FLOAT_TABLES = [
    "g_expfrac",
    "g_pow43",
    "g_pan",
    "g_aa",
    "g_twid9",
    "g_twid3",
    "g_mdct_window",
    "g_sec",
    "g_win",
]
INT_TABLES = {
    "g_scf_long": "u8",
    "g_scf_short": "u8",
    "g_scf_mixed": "u8",
    "g_scf_partitions": "u8",
    "g_scfc_decode": "u8",
    "g_mod": "u8",
    "g_preamp": "u8",
    "tabs": "i16",
    "tab32": "u8",
    "tab33": "u8",
    "tabindex": "i16",
    "g_linbits": "u8",
    "g_hz": "u32",
    "halfrate": "u8",
}

RENAME = {
    "g_scf_long": "SCF_LONG",
    "g_scf_short": "SCF_SHORT",
    "g_scf_mixed": "SCF_MIXED",
    "g_scf_partitions": "SCF_PARTITIONS",
    "g_scfc_decode": "SCFC_DECODE",
    "g_mod": "SCF_MOD",
    "g_preamp": "PREAMP",
    "g_expfrac": "EXPFRAC",
    "g_pow43": "POW43",
    "tabs": "HUFF_TABS",
    "tab32": "COUNT1_TAB_A",
    "tab33": "COUNT1_TAB_B",
    "tabindex": "TAB_INDEX",
    "g_linbits": "LINBITS",
    "g_pan": "PAN",
    "g_aa": "AA",
    "g_twid9": "TWID9",
    "g_twid3": "TWID3",
    "g_mdct_window": "MDCT_WINDOW",
    "g_sec": "SEC",
    "g_win": "SYN_WIN",
    "g_hz": "BASE_HZ",
    "halfrate": "HALF_RATE",
}


def split_nums(text):
    text = text.replace("{", " ").replace("}", " ")
    return [t for t in re.split(r"[\s,]+", text.strip()) if t]


def row_widths(dims):
    """Parses `[8][23]` -> [8, 23]; single-dim or expression dims -> None (flat)."""
    try:
        ws = [int(w.strip()) for w in re.findall(r"\[([^\]]+)\]", dims)]
    except ValueError:
        return None
    return ws if len(ws) > 1 else None


def pad_rows(text, dims, default=0):
    """C zero-fills omitted trailing initializers; restore a flat, aligned list."""
    ws = row_widths(dims)
    if ws is None or len(ws) != 2:
        return split_nums(text)
    rows = re.findall(r"\{([^{}]*)\}", text)
    rows = [split_nums(r) for r in rows if r.strip()]
    if len(rows) != ws[0]:
        raise ValueError(f"row count {len(rows)} != {ws[0]}")
    flat = []
    for r in rows:
        if len(r) > ws[1]:
            raise ValueError(f"row too long: {len(r)} > {ws[1]}")
        flat.extend(r + [str(default)] * (ws[1] - len(r)))
    return flat


def fmt_float(tok):
    tok = tok.rstrip("fF")
    f = float(tok)
    if f == int(f) and abs(f) < 1e30 and "e" not in tok.lower():
        return f"{int(f)}.0"
    s = repr(f)
    if "." not in s and "e" not in s:
        s += ".0"
    return s


def emit_table(out, rust_name, ty, nums, dims, fmt):
    """Emits a flat or 2-D-nested Rust const, mirroring the C dimensions."""
    ws = row_widths(dims)
    if ws is None or len(ws) != 2:
        out.append(f"pub(super) static {rust_name}: [{ty}; {len(nums)}] = [")
        for i in range(0, len(nums), 16):
            out.append("    " + ", ".join(nums[i : i + 16]) + ",")
        out.append("];")
    else:
        rows, cols = ws
        out.append(f"pub(super) static {rust_name}: [[{ty}; {cols}]; {rows}] = [")
        for r in range(rows):
            cells = nums[r * cols : (r + 1) * cols]
            out.append("    [" + ", ".join(fmt(c) for c in cells) + "],")
        out.append("];")
    out.append(f"// dims [{dims}], {len(nums)} entries")
    out.append("")


def main():
    src = open(sys.argv[1], encoding="utf-8").read()
    out = []
    out.append("//! Standard table data for MPEG-1/2/2.5 Layer III (ISO/IEC 11172-3 and")
    out.append("//! ISO/IEC 13818-3 Annex B). These are non-copyrightable standard tables;")
    out.append("//! they were mechanically extracted from the public-domain `minimp3` decoder")
    out.append("//! (https://github.com/lieff/minimp3) to eliminate transcription errors, and")
    out.append("//! spot-verified against the published spec values.")
    out.append("//!")
    out.append("//! Everything here is `pub(super)`: an implementation detail of the crate.")
    out.append("#![allow(clippy::all)]")
    out.append("")

    for name in FLOAT_TABLES:
        m = re.search(r"static const float " + name + r"((?:\[[^]]*\])+)\s*=\s*\{(.*?)\};", src, re.S)
        if not m:
            sys.exit(f"missing table {name}")
        dims = m.group(1)
        nums = [fmt_float(t) for t in pad_rows(m.group(2), dims)]
        ty = "f32"
        rust_name = RENAME[name]
        emit_table(out, rust_name, ty, nums, dims, fmt=lambda s: s)

    for name, ty in INT_TABLES.items():
        m = re.search(
            r"static const (?:uint8_t|uint16_t|int16_t|unsigned|uint32_t) " + name + r"((?:\[[^]]*\])+)\s*=\s*\{(.*?)\};",
            src,
            re.S,
        )
        if not m:
            sys.exit(f"missing table {name}")
        raw = pad_rows(m.group(2), m.group(1))
        nums = []
        for t in raw:
            if ty == "i16":
                v = int(t, 0) & 0xFFFF
                if v >= 0x8000:
                    v -= 0x10000
                nums.append(str(v))
            else:
                nums.append(str(int(t, 0) & 0xFF))
        dims = m.group(1)
        nums = []
        for t in raw:
            v = int(t, 0)
            if ty == "i16":
                v &= 0xFFFF
                if v >= 0x8000:
                    v -= 0x10000
            elif ty == "u8":
                v &= 0xFF
            nums.append(str(v))
        rust_name = RENAME[name]
        emit_table(out, rust_name, ty, nums, dims, fmt=lambda s: s)

    open(sys.argv[2], "w", encoding="utf-8").write("\n".join(out) + "\n")
    print(f"wrote {sys.argv[2]}")


if __name__ == "__main__":
    main()
