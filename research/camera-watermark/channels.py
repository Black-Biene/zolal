import sys, json, math
import numpy as np
import cv2
from skimage import data
from skimage.metrics import peak_signal_noise_ratio as psnr, structural_similarity as ssim
from block_tilt import embed, extract, canon_size, grid

rng = np.random.default_rng(7)


def load_images():
    imgs = {}
    for name in ["astronaut", "chelsea", "coffee", "rocket"]:
        rgb = getattr(data, name)()
        imgs[name] = cv2.cvtColor(rgb, cv2.COLOR_RGB2BGR)
    houses = None  # add your own test photos here
    if houses is not None:
        imgs["houses"] = houses
    return imgs


def jpeg(img, q):
    ok, buf = cv2.imencode(".jpg", img, [cv2.IMWRITE_JPEG_QUALITY, q])
    return cv2.imdecode(buf, cv2.IMREAD_COLOR)


def ch_telegram(img, r):
    h, w = img.shape[:2]
    s = min(1.0, 1280 / max(h, w))
    if s < 1:
        img = cv2.resize(img, (int(w * s), int(h * s)), interpolation=cv2.INTER_AREA)
    return jpeg(img, 82), None


def ch_telegram_harsh(img, r):
    h, w = img.shape[:2]
    s = 800 / max(h, w)
    img = cv2.resize(img, (int(w * s), int(h * s)), interpolation=cv2.INTER_AREA)
    return jpeg(img, 70), None


def camera(img, r, blur, noise, frac, jitter, contrast=(0.8, 1.0)):
    """Photograph `img` shown on a screen / paper with a phone. Returns frame and true corners."""
    FW, FH = 1920, 1080
    h, w = img.shape[:2]
    pw = FW * r.uniform(*frac)
    ph = pw * h / w
    if ph > FH * 0.95:
        ph = FH * 0.95
        pw = ph * w / h
    cx, cy = FW / 2 + r.uniform(-100, 100), FH / 2 + r.uniform(-60, 60)
    base = np.float32([[cx - pw / 2, cy - ph / 2], [cx + pw / 2, cy - ph / 2],
                       [cx + pw / 2, cy + ph / 2], [cx - pw / 2, cy + ph / 2]])
    corners = base + r.uniform(-jitter, jitter, (4, 2)).astype(np.float32) * np.float32([pw, ph])
    src = np.float32([[0, 0], [w - 1, 0], [w - 1, h - 1], [0, h - 1]])
    M = cv2.getPerspectiveTransform(src, corners)
    bg = np.full((FH, FW, 3), r.uniform(20, 80), np.float32)
    frame = cv2.warpPerspective(img.astype(np.float32), M, (FW, FH), dst=bg,
                                flags=cv2.INTER_LINEAR, borderMode=cv2.BORDER_TRANSPARENT)
    f = frame / 255.0
    f = np.clip(f * r.uniform(*contrast) + r.uniform(-0.06, 0.06), 0, 1)
    f = f ** r.uniform(0.8, 1.25)
    f = f * r.uniform(0.9, 1.1, 3)  # white balance
    f = cv2.GaussianBlur(f, (0, 0), r.uniform(*blur))
    f = f + r.normal(0, r.uniform(*noise) / 255.0, f.shape)
    f = np.clip(f * 255, 0, 255).astype(np.uint8)
    return jpeg(f, 85), corners


def ch_screen(img, r):
    return camera(img, r, blur=(0.8, 1.8), noise=(3, 6), frac=(0.55, 0.8), jitter=0.05)


def ch_screen_far(img, r):
    return camera(img, r, blur=(1.2, 2.2), noise=(4, 8), frac=(0.35, 0.5), jitter=0.06)


def ch_print(img, r):
    return camera(img, r, blur=(2.0, 3.0), noise=(5, 8), frac=(0.55, 0.8), jitter=0.04,
                  contrast=(0.6, 0.8))


CHANNELS = {"telegram": ch_telegram, "telegram_harsh": ch_telegram_harsh,
            "screen": ch_screen, "screen_far": ch_screen_far, "print": ch_print}


def rs_payload_bytes(n_bits, ber, target=0.99):
    """Largest k for an RS code over n = n_bits//8 bytes that decodes with prob >= target,
    assuming independent bit errors (bits are scattered by key)."""
    n = min(n_bits // 8, 255)
    q = 1 - (1 - ber) ** 8
    best = 0
    for k in range(1, n):
        t = (n - k) // 2
        # P(#byte errors <= t)
        p = sum(math.comb(n, i) * q ** i * (1 - q) ** (n - i) for i in range(t + 1))
        if p >= target:
            best = k
    return best


def run(b, strength, trials, corner_err):
    imgs = load_images()
    results = {c: [] for c in CHANNELS}
    quality = []
    for name, img in imgs.items():
        Hc, Wc = canon_size(*img.shape[:2])
        n = len(grid((Hc, Wc), b))
        bits = rng.integers(0, 2, n)
        wmk = embed(img, bits, b=b, strength=strength)
        orig = cv2.resize(img, (Wc, Hc), interpolation=cv2.INTER_AREA)
        quality.append((psnr(orig, wmk), ssim(orig, wmk, channel_axis=2)))
        cv2.imwrite(f"out/{name}_b{b}_s{strength}.png", wmk)
        for cname, ch in CHANNELS.items():
            for t in range(trials):
                r = np.random.default_rng(1000 * t + hash(cname) % 997)
                got, corners = ch(wmk, r)
                if corners is not None:
                    corners = corners + r.uniform(-corner_err, corner_err, (4, 2)).astype(np.float32)
                soft = extract(got, n, b=b, corners=corners, out_shape=(Hc, Wc))
                ber = float(np.mean((soft > 0) != (bits == 1)))
                results[cname].append((n, ber))
    q = np.mean(quality, axis=0)
    print(f"\nblock {b}px, strength {strength}, corner error ±{corner_err}px:"
          f"  PSNR {q[0]:.1f} dB, SSIM {q[1]:.3f}")
    for cname, rs in results.items():
        bers = np.array([x[1] for x in rs])
        n = rs[0][0]
        worst = np.percentile(bers, 90)
        pay = rs_payload_bytes(n, worst)
        print(f"  {cname:15s} bits/img {n:5d}  BER mean {bers.mean():.3f}  p90 {worst:.3f}"
              f"  -> ~{pay} bytes reliable (RS, 1 block)")


if __name__ == "__main__":
    import os
    os.makedirs("out", exist_ok=True)
    for b, s, ce in [(int(a), float(bb), float(c)) for a, bb, c in (x.split(",") for x in sys.argv[1:])]:
        run(b, s, trials=6, corner_err=ce)
