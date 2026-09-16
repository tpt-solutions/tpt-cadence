#!/usr/bin/env python3
"""Mechanical Python port of minimp3's Layer III decode path, for bring-up
debugging of tpt-av-cadence-mp3. Parses the standard tables straight from the
downloaded minimp3.h and decodes granule-by-granule with stage dumps.

Usage: python tools/minimp3_py.py <file.mp3> <frames> <out.f32>
"""
import math
import re
import struct
import sys

SRC = open(__file__.rsplit("/", 1)[0].rsplit("\\", 1)[0] + "/minimp3.h", encoding="utf-8").read()


def c_table(name):
    m = re.search(r"static const [a-z0-9_ ]+ " + name + r"((\[[^]]*\])+)\s*=\s*\{(.*?)\};", SRC, re.S)
    body = m.group(3).replace("{", " ").replace("}", " ")
    toks = [t for t in re.split(r"[\s,]+", body.strip()) if t]
    vals = []
    for t in toks:
        t = t.rstrip("fF")
        vals.append(float(t) if ("." in t or "e" in t.lower()) else int(t, 0))
    return vals


tabs = c_table("tabs")
tab32 = c_table("tab32")
tab33 = c_table("tab33")
tabindex = c_table("tabindex")
g_linbits = c_table("g_linbits")
g_pow43 = c_table("g_pow43")
g_scf_long = [c_table("g_scf_long")[i * 23 : (i + 1) * 23] for i in range(8)]
g_scf_short = [c_table("g_scf_short")[i * 40 : (i + 1) * 40] for i in range(8)]
g_scf_mixed = [c_table("g_scf_mixed")[i * 40 : (i + 1) * 40] for i in range(8)]
g_scf_partitions = [c_table("g_scf_partitions")[i * 28 : (i + 1) * 28] for i in range(3)]
g_scfc_decode = c_table("g_scfc_decode")
g_mod = c_table("g_mod")
g_preamp = c_table("g_preamp")
g_expfrac = c_table("g_expfrac")
g_pan = c_table("g_pan")
g_aa0 = c_table("g_aa")[:8]
g_aa1 = c_table("g_aa")[8:]
g_twid9 = c_table("g_twid9")
g_twid3 = c_table("g_twid3")
g_mdct_win = [c_table("g_mdct_window")[:18], c_table("g_mdct_window")[18:]]
g_sec = c_table("g_sec")
g_win = c_table("g_win")

BITS_DEQ = -1
MAX_SCFI = (255 + BITS_DEQ * 4 - 210 + 3) & ~3


def ldexp_q2(y, exp_q2):
    while True:
        e = min(30 * 4, exp_q2)
        y *= g_expfrac[e & 3] * (1 << 30 >> (e >> 2))
        exp_q2 -= e
        if exp_q2 <= 0:
            return y


class Bs:
    def __init__(self, buf):
        self.buf = buf
        self.pos = 0
        self.limit = len(buf) * 8

    def get(self, n):
        s = self.pos & 7
        p = self.pos >> 3
        self.pos += n
        if self.pos > self.limit:
            return 0
        cache = 0
        nxt = self.buf[p] & (255 >> s) if p < len(self.buf) else 0
        p += 1
        shl = n + s
        while (shl := shl - 8) > 0:
            cache |= nxt << shl
            nxt = self.buf[p] if p < len(self.buf) else 0
            p += 1
        return cache | (nxt >> -shl)


def pow43(x):
    if x < 129:
        return g_pow43[16 + x]
    mult = 256
    if x < 1024:
        mult = 16
        x <<= 3
    sign = 2 * x & 64
    frac = ((x & 63) - sign) / ((x & ~63) + sign)
    return g_pow43[16 + ((x + sign) >> 6)] * (1.0 + frac * (4.0 / 3.0 + frac * (2.0 / 9.0))) * mult


class Huff:
    def __init__(self, buf, pos):
        self.buf = buf
        self.next = pos >> 3
        b = lambda i: buf[i] if i < len(buf) else 0
        self.cache = (((b(self.next) * 256 + b(self.next + 1)) * 256 + b(self.next + 2)) * 256 + b(self.next + 3)) << (pos & 7)
        self.sh = (pos & 7) - 8
        self.next += 4

    def bspos(self):
        return self.next * 8 - 24 + self.sh

    def peek(self, n):
        return (self.cache & 0xFFFFFFFF) >> (32 - n) if n else 0

    def flush(self, n):
        self.cache = (self.cache << n) & 0xFFFFFFFF
        self.sh += n

    def check(self):
        while self.sh >= 0:
            b = self.buf[self.next] if self.next < len(self.buf) else 0
            self.cache |= b << self.sh
            self.sh -= 8
            self.next += 1

    def signed(self):
        return (self.cache >> 31) & 1


def read_scalefactors(scf, ist_pos, scf_size, scf_count, bs, scfsi):
    s = 0
    p = 0
    for i in range(4):
        cnt = scf_count[i]
        if cnt == 0:
            break
        if scfsi & 8:
            for k in range(cnt):
                scf[s + k] = ist_pos[p + k]
        else:
            bits = scf_size[i]
            if bits == 0:
                for k in range(cnt):
                    scf[s + k] = 0
                    ist_pos[p + k] = 0
            else:
                max_scf = (1 << bits) - 1 if scfsi < 0 else -1
                for k in range(cnt):
                    v = bs.get(bits)
                    ist_pos[p + k] = 255 if (scfsi < 0 and v == max_scf) else v
                    scf[s + k] = v
        s += cnt
        p += cnt
        scfsi *= 2
    for k in range(3):
        scf[s + k] = 0


def decode_scalefactors(hdr, ist_pos, bs, gr, ch):
    scf_shift = gr["scalefac_scale"] + 1
    scfsi = gr["scfsi"]
    part = g_scf_partitions[(1 if gr["n_short"] else 0) + (0 if gr["n_long"] else 0)]
    scf_size = [0, 0, 0, 0]
    iscf = [0] * 40
    k = 0
    if hdr["mpeg1"]:
        p = g_scfc_decode[gr["scalefac_compress"]]
        scf_size[0] = scf_size[1] = p >> 2
        scf_size[2] = scf_size[3] = p & 3
    else:
        ist = 1 if (hdr["i_stereo"] and ch) else 0
        sfc = gr["scalefac_compress"] >> ist
        k = ist * 12
        while sfc >= 0:
            modprod = 1
            for i in range(3, -1, -1):
                scf_size[i] = sfc // modprod % g_mod[k + i]
                modprod *= g_mod[k + i]
            sfc -= modprod
            k += 4
        part = part[k:]
        scfsi = -16
    read_scalefactors(iscf, ist_pos, scf_size, part, bs, scfsi)
    if gr["n_short"]:
        sh = 3 - scf_shift
        base = gr["n_long"]
        for i in range(0, gr["n_short"], 3):
            iscf[base + i + 0] = (iscf[base + i + 0] + gr["sbg"][0] << sh) & 0xFF
            iscf[base + i + 1] = (iscf[base + i + 1] + gr["sbg"][1] << sh) & 0xFF
            iscf[base + i + 2] = (iscf[base + i + 2] + gr["sbg"][2] << sh) & 0xFF
    elif gr["preflag"]:
        for i in range(10):
            iscf[11 + i] = (iscf[11 + i] + g_preamp[i]) & 0xFF
    gain_exp = gr["global_gain"] + BITS_DEQ * 4 - 210 - (2 if hdr["ms"] else 0)
    gain = ldexp_q2(float(1 << (MAX_SCFI // 4)), MAX_SCFI - gain_exp)
    out = [0.0] * 40
    for i in range(gr["n_long"] + gr["n_short"]):
        out[i] = ldexp_q2(gain, iscf[i] << scf_shift)
    return out, iscf


def huffman(dst, bs, gr, scf, layer3gr_limit):
    one = 0.0
    ireg = 0
    big_val_cnt = gr["big_values"]
    sfb = list(gr["sfbtab"])
    sfb_idx = 0
    scf_idx = 0
    d = 0
    while big_val_cnt > 0:
        tab_num = gr["table_select"][ireg]
        sfb_cnt = gr["region_count"][ireg]
        ireg += 1
        codebook = tabs[tabindex[tab_num]:]
        linbits = g_linbits[tab_num]
        while True:
            np = sfb[sfb_idx] // 2
            sfb_idx += 1
            pairs = min(big_val_cnt, np)
            one = scf[scf_idx]
            scf_idx += 1
            for _ in range(pairs):
                w = 5
                leaf = codebook[bs.peek(w)]
                while leaf < 0:
                    bs.flush(w)
                    w = leaf & 7
                    leaf = codebook[bs.peek(w) - (leaf >> 3)]
                bs.flush(leaf >> 8)
                for _ in range(2):
                    lsb = leaf & 0x0F
                    if lsb == 15 and linbits:
                        lsb += bs.peek(linbits)
                        bs.flush(linbits)
                        bs.check()
                        s = -1.0 if bs.signed() else 1.0
                        dst[d] = one * pow43(lsb) * s
                    else:
                        dst[d] = g_pow43[16 + lsb - 16 * bs.signed()] * one
                    bs.flush(1 if lsb else 0)
                    leaf >>= 4
                    d += 1
                bs.check()
            big_val_cnt -= np
            sfb_cnt -= 1
            if not (big_val_cnt > 0 and sfb_cnt >= 0):
                break
    book = tab33 if gr["count1_table"] else tab32
    np = 1 - big_val_cnt
    while True:
        if d + 4 > 1152:
            break
        leaf = book[bs.peek(4)]
        if not (leaf & 8):
            nbits = leaf & 3
            leaf = book[(leaf >> 3) + ((bs.cache << 4 & 0xFFFFFFFF) >> (32 - nbits))]
        bs.flush(leaf & 7)
        if bs.bspos() > layer3gr_limit:
            break
        for s in range(4):
            if s == 0 or s == 2:
                np -= 1
                if np == 0:
                    np = sfb[sfb_idx] // 2
                    sfb_idx += 1
                    if np == 0:
                        np = -1  # sentinel: avoid re-entering
                        break
                    one = scf[scf_idx]
                    scf_idx += 1
            if np == -1:
                break
            if leaf & (128 >> s):
                dst[d] = -one if bs.signed() else one
                bs.flush(1)
            d += 1
        if np == -1:
            break
        bs.check()
    return layer3gr_limit


def reorder(grbuf, off, sfb):
    src = grbuf[off:]
    dst = []
    i = 0
    j = 0
    while True:
        ln = sfb[j] if j < len(sfb) else 0
        if ln == 0:
            break
        for k in range(ln):
            dst.append(src[k])
            dst.append(src[ln + k])
            dst.append(src[2 * ln + k])
        j += 3
        src = src[3 * ln :]
    grbuf[off : off + len(dst)] = dst


def antialias(grbuf, nbands):
    for b in range(nbands):
        base = b * 18
        for i in range(8):
            u = grbuf[base + 18 + i]
            d = grbuf[base + 17 - i]
            grbuf[base + 18 + i] = u * g_aa0[i] - d * g_aa1[i]
            grbuf[base + 17 - i] = u * g_aa1[i] + d * g_aa0[i]


def dct3_9(y):
    s0, s2, s4, s6, s8 = y[0], y[2], y[4], y[6], y[8]
    t0 = s0 + s6 * 0.5
    s0 -= s6
    t4 = (s4 + s2) * 0.93969262
    t2 = (s8 + s2) * 0.76604444
    s6 = (s4 - s8) * 0.17364818
    s4 += s8 - s2
    s2 = s0 - s4 * 0.5
    y4 = s4 + s0
    s8 = t0 - t2 + s6
    s0 = t0 - t4 + t2
    s4 = t0 + t4 - s6
    s1, s3, s5, s7 = y[1], y[3], y[5], y[7]
    s3 *= 0.86602540
    t0 = (s5 + s1) * 0.98480775
    t4 = (s5 - s7) * 0.34202014
    t2 = (s1 + s7) * 0.64278761
    s1 = (s1 - s5 - s7) * 0.86602540
    s5 = t0 - s3 - t2
    s7 = t4 - s3 - t0
    s3 = t4 + s3 - t2
    y[0] = s4 - s7
    y[1] = s2 + s1
    y[2] = s0 - s3
    y[3] = s8 + s5
    y[5] = s8 - s5
    y[6] = s0 + s3
    y[7] = s2 - s1
    y[8] = s4 + s7
    y[4] = y4


def imdct12(x, dst, overlap):
    co = [0.0] * 3
    si = [0.0] * 3
    m1 = (x[6] + x[3]) * 0.86602540
    a1 = -x[0] - (x[12] + x[9]) * 0.5
    co[1] = -x[0] + (x[12] + x[9])
    co[0] = a1 + m1
    co[2] = a1 - m1
    m1 = (x[12] - x[9]) * 0.86602540
    a1 = x[15] - (x[6] - x[3]) * 0.5
    si[1] = x[15] + (x[6] - x[3])
    si[0] = a1 + m1
    si[2] = a1 - m1
    si[1] = -si[1]
    for i in range(3):
        ovl = overlap[i]
        sm = co[i] * g_twid3[3 + i] + si[i] * g_twid3[i]
        overlap[i] = co[i] * g_twid3[i] - si[i] * g_twid3[3 + i]
        dst[i] = ovl * g_twid3[2 - i] - sm * g_twid3[5 - i]
        dst[5 - i] = ovl * g_twid3[5 - i] + sm * g_twid3[2 - i]


def imdct36_band(grbuf, overlap, window):
    gb = grbuf
    ov = overlap
    co = [0.0] * 9
    si = [0.0] * 9
    co[0] = -gb[0]
    si[0] = gb[17]
    for i in range(4):
        si[8 - 2 * i] = gb[4 * i + 1] - gb[4 * i + 2]
        co[1 + 2 * i] = gb[4 * i + 1] + gb[4 * i + 2]
        si[7 - 2 * i] = gb[4 * i + 4] - gb[4 * i + 3]
        co[2 + 2 * i] = -(gb[4 * i + 3] + gb[4 * i + 4])
    dct3_9(co)
    dct3_9(si)
    for i in range(1, 8, 2):
        si[i] = -si[i]
    for i in range(9):
        ovl = ov[i]
        sm = co[i] * g_twid9[9 + i] + si[i] * g_twid9[i]
        ov[i] = co[i] * g_twid9[i] - si[i] * g_twid9[9 + i]
        gb[i] = ovl * window[i] - sm * window[9 + i]
        gb[17 - i] = ovl * window[9 + i] + sm * window[i]


def imdct_gr(grbuf, overlap, block_type, n_long_bands):
    if n_long_bands:
        for j in range(n_long_bands):
            imdct36_band(grbuf[j * 18 : j * 18 + 18], overlap[j * 9 : j * 9 + 9], g_mdct_win[0])
        grbuf = grbuf[n_long_bands * 18 :]
        overlap = overlap[n_long_bands * 9 :]
    rest = 32 - n_long_bands
    if block_type == 2:
        for b in range(rest):
            tmp = grbuf[b * 18 : b * 18 + 18][:]
            grbuf[b * 18 : b * 18 + 6] = overlap[b * 9 : b * 9 + 6]
            ov6 = overlap[b * 9 + 6 : b * 9 + 9]
            imdct12(tmp, grbuf[b * 18 + 6 : b * 18 + 12], ov6)
            imdct12(tmp[1:], grbuf[b * 18 + 12 : b * 18 + 18], ov6)
            imdct12(tmp[2:], overlap[b * 9 : b * 9 + 6], ov6)
    else:
        win = g_mdct_win[1] if block_type == 3 else g_mdct_win[0]
        for b in range(rest):
            imdct36_band(grbuf[b * 18 : b * 18 + 18], overlap[b * 9 : b * 9 + 9], win)


def change_sign(grbuf):
    off = 18
    while off < 576:
        for i in range(1, 18, 2):
            grbuf[off + i] = -grbuf[off + i]
        off += 36


def dct_ii(grbuf, n):
    for k in range(n):
        t = [[0.0] * 8 for _ in range(4)]
        for i in range(8):
            x0 = grbuf[k + i * 18]
            x1 = grbuf[k + (15 - i) * 18]
            x2 = grbuf[k + (16 + i) * 18]
            x3 = grbuf[k + (31 - i) * 18]
            t0 = x0 + x3
            t1 = x1 + x2
            t2 = (x1 - x2) * g_sec[3 * i]
            t3 = (x0 - x3) * g_sec[3 * i + 1]
            t[0][i] = t0 + t1
            t[1][i] = (t0 - t1) * g_sec[3 * i + 2]
            t[2][i] = t3 + t2
            t[3][i] = (t3 - t2) * g_sec[3 * i + 2]
        for g in range(4):
            x = t[g]
            x0, x1, x2, x3, x4, x5, x6, x7 = x
            xt = x0 - x7
            x0 += x7
            x7 = x1 - x6
            x1 += x6
            x6 = x2 - x5
            x2 += x5
            x5 = x3 - x4
            x3 += x4
            x4 = x0 - x3
            x0 += x3
            x3 = x1 - x2
            x1 += x2
            x[0] = x0 + x1
            x[4] = (x0 - x1) * 0.70710677
            x5 = x5 + x6
            x6 = (x6 + x7) * 0.70710677
            x7 = x7 + xt
            x3 = (x3 + x4) * 0.70710677
            x5 -= x7 * 0.198912367
            x7 += x5 * 0.382683432
            x5 -= x7 * 0.198912367
            x0 = xt - x6
            xt += x6
            x[1] = (xt + x7) * 0.50979561
            x[2] = (x4 + x3) * 0.54119611
            x[3] = (x0 - x5) * 0.60134488
            x[5] = (x0 + x5) * 0.89997619
            x[6] = (x4 - x3) * 1.30656302
            x[7] = (xt - x7) * 2.56291556
        base = k
        for i in range(7):
            grbuf[base] = t[0][i]
            grbuf[base + 18] = t[2][i] + t[3][i] + t[3][i + 1]
            grbuf[base + 36] = t[1][i] + t[1][i + 1]
            grbuf[base + 54] = t[2][i + 1] + t[3][i] + t[3][i + 1]
            base += 72
        grbuf[base] = t[0][7]
        grbuf[base + 18] = t[2][7] + t[3][7]
        grbuf[base + 36] = t[1][7]
        grbuf[base + 54] = t[3][7]


def scale_pcm(sample):
    return sample * (1.0 / 32768.0)


def synth_pair(pcm, pcm_off, nch, lins, z):
    a = (lins[z + 14 * 64] - lins[z]) * 29
    a += (lins[z + 64] + lins[z + 13 * 64]) * 213
    a += (lins[z + 12 * 64] - lins[z + 2 * 64]) * 459
    a += (lins[z + 3 * 64] + lins[z + 11 * 64]) * 2037
    a += (lins[z + 10 * 64] - lins[z + 4 * 64]) * 5153
    a += (lins[z + 5 * 64] + lins[z + 9 * 64]) * 6574
    a += (lins[z + 8 * 64] - lins[z + 6 * 64]) * 37489
    a += lins[z + 7 * 64] * 75038
    pcm[pcm_off] = scale_pcm(a)
    z += 2
    a = lins[z + 14 * 64] * 104
    a += lins[z + 12 * 64] * 1567
    a += lins[z + 10 * 64] * 9727
    a += lins[z + 8 * 64] * 64019
    a += lins[z + 6 * 64] * -9975
    a += lins[z + 4 * 64] * -45
    a += lins[z + 2 * 64] * 146
    a += lins[z] * -5
    pcm[pcm_off + 16 * nch] = scale_pcm(a)


def synth(xl, xr, pcm, pcm_off, nch, lins, base=0):
    zlin = 15 * 64 + base
    wi = 0
    dstr = pcm_off + nch - 1
    dstl = pcm_off
    lins[zlin + 4 * 15] = xl[18 * 16]
    lins[zlin + 4 * 15 + 1] = xr[18 * 16]
    lins[zlin + 4 * 15 + 2] = xl[0]
    lins[zlin + 4 * 15 + 3] = xr[0]
    lins[zlin + 4 * 31] = xl[1 + 18 * 16]
    lins[zlin + 4 * 31 + 1] = xr[1 + 18 * 16]
    lins[zlin + 4 * 31 + 2] = xl[1]
    lins[zlin + 4 * 31 + 3] = xr[1]
    synth_pair(pcm, dstr, nch, lins, base + 4 * 15 + 1)
    synth_pair(pcm, dstr + 32 * nch, nch, lins, base + 4 * 15 + 64 + 1)
    synth_pair(pcm, dstl, nch, lins, base + 4 * 15)
    synth_pair(pcm, dstl + 32 * nch, nch, lins, base + 4 * 15 + 64)
    for i in range(14, -1, -1):
        a = [0.0] * 4
        b = [0.0] * 4
        lins[zlin + 4 * i] = xl[18 * (31 - i)]
        lins[zlin + 4 * i + 1] = xr[18 * (31 - i)]
        lins[zlin + 4 * i + 2] = xl[1 + 18 * (31 - i)]
        lins[zlin + 4 * i + 3] = xr[1 + 18 * (31 - i)]
        lins[zlin + 4 * (i + 16)] = xl[1 + 18 * (1 + i)]
        lins[zlin + 4 * (i + 16) + 1] = xr[1 + 18 * (1 + i)]
        lins[zlin + 4 * (i - 16) + 2] = xl[18 * (1 + i)]
        lins[zlin + 4 * (i - 16) + 3] = xr[18 * (1 + i)]
        for step in range(8):
            w0 = g_win[wi]
            w1 = g_win[wi + 1]
            wi += 2
            vz = zlin + 4 * i - step * 64
            vy = zlin + 4 * i - (15 - step) * 64
            for j in range(4):
                sv = lins[vz + j]
                yv = lins[vy + j]
                if step == 0:
                    b[j] = sv * w1 + yv * w0
                    a[j] = sv * w0 - yv * w1
                elif step % 2 == 0:
                    b[j] += sv * w1 + yv * w0
                    a[j] += sv * w0 - yv * w1
                else:
                    b[j] += sv * w1 + yv * w0
                    a[j] += yv * w1 - sv * w0
        for off, val in (
            (dstr + (15 - i) * nch, a[1]),
            (dstr + (17 + i) * nch, b[1]),
            (dstl + (15 - i) * nch, a[0]),
            (dstl + (17 + i) * nch, b[0]),
            (dstr + (47 - i) * nch, a[3]),
            (dstr + (49 + i) * nch, b[3]),
            (dstl + (47 - i) * nch, a[2]),
            (dstl + (49 + i) * nch, b[2]),
        ):
            pcm[off] = scale_pcm(val)


def synth_granule(qmf_state, grbuf, nch, pcm, lins):
    for ch in range(nch):
        # slice-assign: Python slices copy, C mutates in place
        blk = grbuf[576 * ch : 576 * ch + 576]
        dct_ii(blk, 18)
        grbuf[576 * ch : 576 * ch + 576] = blk
    lins[: 15 * 64] = qmf_state
    for i in range(0, 18, 2):
        synth(grbuf[i:], grbuf[i + 576 * (nch - 1):], pcm, 32 * nch * i, nch, lins, i * 64)
    if nch == 1:
        for i in range(0, 15 * 64, 2):
            qmf_state[i] = lins[18 * 64 + i]
    else:
        qmf_state[:] = lins[18 * 64 : 18 * 64 + 960]


def decode_frame(data, off, dec):
    hdr_b = data[off : off + 4]
    assert hdr_b[0] == 0xFF
    mpeg1 = bool(hdr_b[1] & 0x08)
    not25 = bool(hdr_b[1] & 0x10)
    layer = (hdr_b[1] >> 1) & 3
    assert layer == 1
    br_idx = hdr_b[2] >> 4
    sr_idx = (hdr_b[2] >> 2) & 3
    mode = hdr_b[3] >> 6
    mode_ext = (hdr_b[3] >> 4) & 3
    crc = not (hdr_b[1] & 1)
    mono = mode == 3
    nch = 1 if mono else 2
    halfrate = c_table("halfrate")
    kbps = 2 * halfrate[(mpeg1 * 45) + (layer - 1) * 15 + br_idx]
    hz = [44100, 48000, 32000][sr_idx] >> (not mpeg1) >> (not not25)
    frame_bytes = (1152 if mpeg1 else 576) * kbps * 125 // hz
    padding = (hdr_b[2] >> 1) & 1
    total = frame_bytes + padding
    hdr = {
        "mpeg1": mpeg1,
        "mono": mono,
        "ms": mode == 1 and mode_ext & 2 != 0,
        "i_stereo": mode == 1 and mode_ext & 1 != 0,
    }
    si_len = [[9, 17], [17, 32]][1 if mpeg1 else 0][0 if mono else 1]
    hdr_len = 4 + (2 if crc else 0)
    if crc:
        stored = (data[off + 4] << 8) | data[off + 5]
        ccrc = 0xFFFF
        for b in data[off + 2 : off + hdr_len + si_len]:
            ccrc ^= b << 8
            for _ in range(8):
                ccrc = ((ccrc << 1) ^ 0x8005) & 0xFFFF if ccrc & 0x8000 else (ccrc << 1) & 0xFFFF
        assert ccrc == stored, "crc"
    bs = Bs(data[off + hdr_len : off + total])
    n_gr = nch * (2 if mpeg1 else 1)
    sr_tab = sr_idx + ((mpeg1 + not25) * 3)
    sr_tab = sr_tab - 1 if sr_tab else 0
    scfsi_stream = [0, 0]
    if mpeg1:
        mdb = bs.get(9)
        raw = bs.get(9 if mono else 11)
        if mono:
            scfsi_stream[0] = raw & 0xF
        else:
            scfsi_stream[0] = (raw >> 4) & 0xF
            scfsi_stream[1] = raw & 0xF
    else:
        pv = 5 if mono else 3
        mdb = bs.get(8 + pv) >> pv
    grs = []
    part23_sum = 0
    for g in range(n_gr):
        gr = {}
        gr["part23"] = bs.get(12)
        part23_sum += gr["part23"]
        gr["big_values"] = bs.get(9)
        gr["global_gain"] = bs.get(8)
        gr["scalefac_compress"] = bs.get(4 if mpeg1 else 9)
        gr["sfbtab"] = g_scf_long[sr_tab]
        gr["n_long"] = 22
        gr["n_short"] = 0
        gr["scfsi"] = 0
        gr["region_count"] = [0, 0, 255]
        if bs.get(1):
            bt = bs.get(2)
            assert bt != 0
            gr["block_type"] = bt
            gr["mixed"] = bs.get(1)
            gr["region_count"] = [7, 255, 255]
            if bt == 2:
                if not gr["mixed"]:
                    gr["sfbtab"] = g_scf_short[sr_tab]
                    gr["n_long"] = 0
                    gr["n_short"] = 39
                else:
                    gr["sfbtab"] = g_scf_mixed[sr_tab]
                    gr["n_long"] = 8 if mpeg1 else 6
                    gr["n_short"] = 30
            tables = bs.get(10) << 5
            gr["sbg"] = [bs.get(3), bs.get(3), bs.get(3)]
        else:
            gr["block_type"] = 0
            gr["mixed"] = 0
            tables = bs.get(15)
            gr["region_count"] = [bs.get(4), bs.get(3), 255]
            gr["sbg"] = [0, 0, 0]
            gr["scfsi"] = scfsi_stream[g % nch] if (mpeg1 and g >= nch) else 0
        gr["table_select"] = [(tables >> 10) & 31, (tables >> 5) & 31, tables & 31]
        gr["preflag"] = bool(bs.get(1)) if mpeg1 else gr["scalefac_compress"] >= 500
        gr["scalefac_scale"] = bs.get(1)
        gr["count1_table"] = bs.get(1)
        grs.append(gr)
    assert part23_sum + bs.pos <= bs.limit + mdb * 8, "reservoir"
    frame_bytes_left = (bs.limit - bs.pos) // 8
    bytes_have = min(dec["reserv_n"], mdb)
    src_off = dec["reserv_n"] - bytes_have
    maindata = dec["reserv"][src_off : src_off + bytes_have] + data[off + hdr_len + bs.pos // 8 : off + hdr_len + bs.pos // 8 + frame_bytes_left]
    md_len = len(maindata)
    success = dec["reserv_n"] >= mdb
    mbs = Bs(maindata)
    md_bits = Huff(maindata, 0)
    n_granules = 2 if mpeg1 else 1
    frames = 0
    for igr in range(n_granules):
        if not success:
            continue
        grbuf = [0.0] * 1152
        ist_pos = [[0] * 39, [0] * 39]
        cur = 0
        for ch in range(nch):
            gr = grs[igr * nch + ch]
            limit = cur + gr["part23"]
            mbs.pos = limit
            scf, iscf = decode_scalefactors(hdr, ist_pos[ch], mbs, gr, ch)
            huffman(grbuf, Huff(maindata, mbs.pos), gr, scf, limit)
            cur = limit
            if igr == 0 and ch == 0 and DEBUG:
                print("[py] ch0 scf", ["%.6g" % v for v in scf[: gr["n_long"] + gr["n_short"]]])
        if hdr["i_stereo"]:
            gr = grs[igr * nch]
            n_sfb = gr["n_long"] + gr["n_short"]
            max_blocks = 3 if gr["n_short"] else 1
            max_band = [-1, -1, -1]
            r = grbuf[576:]
            off2 = 0
            for i in range(n_sfb):
                w = gr["sfbtab"][i]
                k2 = 0
                while k2 < w:
                    if r[off2 + k2] != 0 or r[off2 + k2 + 1] != 0:
                        max_band[i % 3] = i
                        break
                    k2 += 2
                off2 += w
            if gr["n_long"]:
                m = max(max_band)
                max_band = [m, m, m]
            pos = ist_pos[1][:]
            default_pos = 3 if hdr["mpeg1"] else 0
            for i in range(max_blocks):
                itop = n_sfb - max_blocks + i
                prev2 = itop - max_blocks
                pos[itop] = default_pos if max_band[i] >= prev2 else ist_pos[1][prev2]
            # intensity process
            off2 = 0
            for i in range(n_sfb):
                w = gr["sfbtab"][i]
                ipos = pos[i]
                if i > max_band[i % 3] and ipos < (7 if hdr["mpeg1"] else 64):
                    s = 1.41421356 if hdr["ms"] else 1.0
                    if hdr["mpeg1"]:
                        kl, kr = g_pan[2 * ipos], g_pan[2 * ipos + 1]
                    else:
                        kl, kr = 1.0, ldexp_q2(1.0, ((ipos + 1) >> 1) << (grs[igr * nch + 1]["scalefac_compress"] & 1))
                        if ipos & 1:
                            kl, kr = kr, kl
                    for k2 in range(w):
                        l = grbuf[off2 + k2]
                        grbuf[576 + off2 + k2] = l * kr * s
                        grbuf[off2 + k2] = l * kl * s
                elif hdr["ms"]:
                    for k2 in range(w):
                        a = grbuf[off2 + k2]
                        b2 = grbuf[576 + off2 + k2]
                        grbuf[off2 + k2] = a + b2
                        grbuf[576 + off2 + k2] = a - b2
                off2 += w
        elif hdr["ms"]:
            for i in range(576):
                a = grbuf[i]
                b2 = grbuf[576 + i]
                grbuf[i] = a + b2
                grbuf[576 + i] = a - b2
        for ch in range(nch):
            gr = grs[igr * nch + ch]
            aa_bands = 31
            n_long_bands = (2 if gr["mixed"] else 0) << (1 if sr_tab == 1 else 0)
            if gr["n_short"]:
                aa_bands = n_long_bands - 1
                reorder(grbuf, ch * 576 + n_long_bands * 18, gr["sfbtab"][gr["n_long"] :])
            if aa_bands > 0:
                antialias(grbuf[ch * 576 : ch * 576 + 576], aa_bands)
            imdct_gr(grbuf[ch * 576 : ch * 576 + 576], dec["overlap"][ch], gr["block_type"], n_long_bands)
            change_sign(grbuf[ch * 576 : ch * 576 + 576])
        pcm = [0.0] * (576 * nch)
        synth_granule(dec["qmf"], grbuf, nch, pcm, dec["lins"])
        dec["pcm"].extend(pcm)
        frames += 576
        if DEBUG and igr == 0:
            print("[py] gr0 pcm", ["%.7f" % v for v in pcm[:24]])
    pos_byte = (cur + 7) // 8 if success else 0
    remains = md_len - pos_byte
    if remains > 511:
        pos_byte += remains - 511
        remains = 511
    dec["reserv"] = maindata[pos_byte : pos_byte + remains]
    dec["reserv_n"] = remains
    return frames, kbps, hz, nch, total


DEBUG = True


def main():
    path, nframes = sys.argv[1], int(sys.argv[2])
    data = open(path, "rb").read()
    if data[:3] == b"ID3":
        size = (data[6] & 0x7F) << 21 | (data[7] & 0x7F) << 14 | (data[8] & 0x7F) << 7 | (data[9] & 0x7F)
        off = 10 + size + (10 if data[5] & 0x10 else 0)
    else:
        off = 0
    dec = {"reserv": b"", "reserv_n": 0, "qmf": [0.0] * 960, "overlap": [[0.0] * 288, [0.0] * 288], "lins": [0.0] * (33 * 64), "pcm": []}
    for _ in range(nframes):
        frames, kbps, hz, nch, total = decode_frame(data, off, dec)
        print(f"frame at {off}: {frames} samples {kbps}kbps {hz}Hz ch={nch} span={total}")
        off += total
    out = struct.pack(f"<{len(dec['pcm'])}f", *dec["pcm"])
    open(sys.argv[3], "wb").write(out)
    print("wrote", len(dec["pcm"]), "samples")


if __name__ == "__main__":
    main()
