import sys, numpy as np, cv2
from skimage.metrics import peak_signal_noise_ratio as psnr, structural_similarity as ssim
from channels import load_images, CHANNELS
import spread as wm2, conv

def run(cell, strength, K, rate, trials=8, corner_err=3.0, refine=False):
    n = K // rate - (conv.K - 1)
    imgs = load_images(); ok = {c: 0 for c in CHANNELS}; tot = {c: 0 for c in CHANNELS}; qual = []
    for name, img in imgs.items():
        shape = wm2.canon_size(*img.shape[:2])
        msg = np.random.default_rng(len(name)).integers(0, 2, n)
        code = conv.encode(msg, rate)
        bits = np.concatenate([code, np.zeros(K - len(code), np.int8)])
        w = wm2.embed(img, bits, cell=cell, strength=strength, ch=2)
        o = cv2.resize(img, (shape[1], shape[0]), interpolation=cv2.INTER_AREA)
        qual.append((psnr(o, w), ssim(o, w, channel_axis=2)))
        cv2.imwrite(f"out/final_{name}_c{cell}_s{strength}.png", w)
        for cname, chan in CHANNELS.items():
            for t in range(trials):
                r = np.random.default_rng(7919 * t + len(cname) + len(name))
                got, corners = chan(w, r)
                if corners is not None:
                    corners = corners + r.uniform(-corner_err, corner_err, (4, 2)).astype(np.float32)
                if refine and corners is not None:
                    corners = wm2.refine_corners(got, corners, K, cell, shape)
                soft = wm2.extract(got, K, cell=cell, corners=corners, shape=shape, ch=2)[:len(code)]
                soft = np.clip(soft / (np.median(np.abs(soft)) + 1e-9), -3, 3)
                ok[cname] += np.array_equal(conv.decode(soft, n, rate), msg); tot[cname] += 1
    q = np.mean(qual, axis=0)
    print(f"cell {cell} str {strength} raw {K} rate 1/{rate} -> {n} bits = {n // 8} bytes | PSNR {q[0]:.1f} SSIM {q[1]:.3f} | "
          + "  ".join(f"{c} {100 * ok[c] / tot[c]:.0f}%" for c in CHANNELS), flush=True)

if __name__ == "__main__":
    import os; os.makedirs("out", exist_ok=True)
    for a in sys.argv[1:]:
        c, s, k, r, e = a.split(","); print(f"corner error ±{e}px + refine:", flush=True); run(int(c), float(s), int(k), int(r), corner_err=float(e), refine=True, trials=4)
