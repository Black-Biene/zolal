import sys, numpy as np, cv2
from skimage.metrics import peak_signal_noise_ratio as psnr, structural_similarity as ssim
from channels import load_images, CHANNELS, rs_payload_bytes
import spread as wm2

def run(cell, strength, K, trials=5, corner_err=3.0, save=False, target=None, ch=0):
    imgs = load_images()
    res = {c: [] for c in CHANNELS}; qual = []
    for name, img in imgs.items():
        shape = wm2.canon_size(*img.shape[:2])
        bits = np.random.default_rng(hash(name) % 1000).integers(0, 2, K)
        if target is None: w = wm2.embed(img, bits, cell=cell, strength=strength, ch=ch)
        else: w, alpha = wm2.embed_informed(img, bits, cell=cell, target=target, amax=strength)
        o = cv2.resize(img, (shape[1], shape[0]), interpolation=cv2.INTER_AREA)
        qual.append((psnr(o, w), ssim(o, w, channel_axis=2)))
        if save: cv2.imwrite(f"out/ss_{name}_c{cell}_s{strength}.png", w)
        for cname, chan in CHANNELS.items():
            for t in range(trials):
                r = np.random.default_rng(1000 * t + len(cname))
                got, corners = chan(w, r)
                if corners is not None:
                    corners = corners + r.uniform(-corner_err, corner_err, (4, 2)).astype(np.float32)
                soft = wm2.extract(got, K, cell=cell, corners=corners, shape=shape, ch=ch)
                res[cname].append(float(np.mean((soft > 0) != (bits == 1))))
    q = np.mean(qual, axis=0)
    line = f"ch {ch} cell {cell:2d} str {strength:4.1f} K {K:4d} | PSNR {q[0]:.1f} SSIM {q[1]:.3f} |"
    for cname, b in res.items():
        b = np.array(b); line += f" {cname[:9]} {b.mean():.3f}/{np.percentile(b, 90):.3f}"
    print(line, flush=True)

if __name__ == "__main__":
    import os; os.makedirs("out", exist_ok=True)
    for a in sys.argv[1:]:
        c, s, k, h = a.split(","); run(int(c), float(s), int(k), save=True, ch=int(h))
