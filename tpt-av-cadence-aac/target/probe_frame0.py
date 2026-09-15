import numpy as np

D = "tpt-av-cadence-aac/target/dump"
REF = "tpt-av-cadence-aac/tests/data/tone_ref.f32"

def f32(path):
    return np.fromfile(path, dtype="<f4")

ref = f32(REF)
print("ref samples:", len(ref), "ref[0:8]:", ref[:8], "max|ref| first frame:", np.abs(ref[:1024]).max())

M = 1024
n = np.arange(2 * M)
k = np.arange(M)
# ISO natural kernel (B): cos(pi/2048 * (n + 512.5) * (2k+1)), scale -1/1024
K = np.cos(np.pi / 2048.0 * np.outer(n + 512.5, 2 * k + 1.0))
scale = -1.0 / 1024.0

L = np.sin(np.pi * (np.arange(M) + 0.5) / 2048.0)  # sine half window
W = np.concatenate([L, L[::-1]])

for tag in ["pre", "post"]:
    c = f32(f"{D}/{tag}_0_0.f32")
    y = (K @ c.astype(np.float64)) * scale
    out0 = (y * W)[:1024]
    for off in [0, 1024]:
        r = ref[off:off+1024].astype(np.float64)
        err = np.abs(out0 - r).max()
        corr = np.corrcoef(out0, r)[0, 1] if off + 1024 <= len(ref) else float("nan")
        print(f"{tag}-TNS vs ref[{off}:{off+1024}]: max_err={err:.6g} corr={corr:.6f} out_max={np.abs(out0).max():.4g}")
