import numpy as np

D = "tpt-av-cadence-aac/target/dump"
REF = "tpt-av-cadence-aac/tests/data/tone_ref.f32"

ref = np.fromfile(REF, dtype="<f4")

# Per-block max profile
print("block max profile:", np.round([np.abs(ref[i*1024:(i+1)*1024]).max() for i in range(12)], 4))

# Estimate steady tone (block 10, far from onset)
seg = ref[10*1024:11*1024].astype(np.float64)
sp = np.abs(np.fft.rfft(seg * np.hanning(len(seg))))
kpk = sp.argmax()
# parabolic interp
a, b, c = sp[kpk-1], sp[kpk], sp[kpk+1]
d = 0.5*(a-c)/(a-2*b+c)
f0 = (kpk + d) * 44100 / len(seg)
print(f"tone freq ~ {f0:.2f} Hz, amp ~ {2*sp.max()/ (len(seg)/2) / (0.5):.5f}")

X = np.fromfile(f"{D}/pre_3_0.f32", dtype="<f4")  # steady ONLY_LONG frame
nz = np.nonzero(np.abs(X) > 1e-3)[0]
print(f"frame 3: {len(nz)} nonzero bins, range {nz.min()}..{nz.max()}" if len(nz) else "frame 3 all zero")
top = np.argsort(-np.abs(X))[:12]
print("top bins:", sorted(top.tolist()))
print("mags:", np.round(np.abs(X[np.sort(top)]), 2))

# Dequant lattice check: |X[k]| / sf should be cbrt(integer)
# sf for the main bands: sfo = 54 => sf = 2**(54/4) (from notes). Group bands by unique |X|/sf
sf = 2 ** (54 / 4)
r = np.abs(X[np.abs(X) > 0]) / sf
q = np.round(r ** 3)
lat_err = np.abs(r - np.cbrt(q)).max()
print(f"lattice check (sf=2^13.5): max |X/sf - cbrt(n)| = {lat_err:.4g}, n range {q.min():.0f}..{q.max():.0f}")

# What a clean 1kHz-tone MDCT lobe looks like: energy near k* = f0*2048/44100
print(f"expected MDCT center bin ~ {f0*2048/44100:.1f}")
