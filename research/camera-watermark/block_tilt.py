"""Camera-robust watermark prototype (classical, no ML).

Each block of the canonical-size luminance image carries one bit in the sign of
(DCT(0,1) - DCT(1,0)): whether the block leans "left-right" or "top-bottom". Low
frequencies survive blur, resampling and JPEG; a difference of two coefficients cancels
global brightness/contrast changes. Strength adapts to local texture.
"""
import numpy as np
import cv2

CANON_W = 1024


def canon_size(h, w):
    return int(round(h * CANON_W / w / 8) * 8), CANON_W


def dct_basis(b):
    """Unit-energy 2-D DCT basis images for (0,1) and (1,0) on a b x b block."""
    x = (np.arange(b) + 0.5) / b
    c1 = np.cos(np.pi * x)  # first cosine along one axis
    h = np.outer(np.ones(b), c1)  # varies along x -> (0,1)
    v = h.T  # varies along y -> (1,0)
    h /= np.linalg.norm(h)
    v /= np.linalg.norm(v)
    return h, v


def grid(shape, b):
    H, W = shape
    return [(r, c) for r in range(0, H - b + 1, b) for c in range(0, W - b + 1, b)]


def to_y(img_bgr):
    ycc = cv2.cvtColor(img_bgr, cv2.COLOR_BGR2YCrCb).astype(np.float32)
    return ycc


def embed(img_bgr, bits, b=32, strength=6.0, key=1):
    """Return watermarked image at canonical size. bits: array of 0/1 of len <= blocks."""
    Hc, Wc = canon_size(*img_bgr.shape[:2])
    img = cv2.resize(img_bgr, (Wc, Hc), interpolation=cv2.INTER_AREA)
    ycc = to_y(img)
    Y = ycc[..., 0]
    hb, vb = dct_basis(b)
    d = hb - vb  # direction that raises (h - v)
    d /= np.linalg.norm(d)
    cells = grid(Y.shape, b)
    rng = np.random.default_rng(key)
    order = rng.permutation(len(cells))  # key-seeded scatter of bit positions
    assert len(bits) <= len(cells), (len(bits), len(cells))
    for i, bit in enumerate(bits):
        r, c = cells[order[i]]
        blk = Y[r:r + b, c:c + b]
        cur = float((blk * hb).sum() - (blk * vb).sum())
        # texture-adaptive margin: busier blocks hide a bigger change
        tex = blk.std()
        margin = strength * b * (0.6 + min(tex, 40) / 40)
        target = margin if bit else -margin
        # only push as far as needed (informed embedding)
        need = target - cur
        if (bit and cur >= margin) or (not bit and cur <= -margin):
            continue
        # moving along d by t changes (h-v) by t*sqrt(2)
        t = need / np.sqrt(2)
        Y[r:r + b, c:c + b] = blk + t * d
    ycc[..., 0] = np.clip(Y, 0, 255)
    return cv2.cvtColor(ycc.astype(np.uint8), cv2.COLOR_YCrCb2BGR)


def extract(img_bgr, n, b=32, key=1, corners=None, out_shape=None):
    """Soft values (positive = 1) for n bits. corners: 4 points (tl,tr,br,bl) of the picture
    inside img_bgr; the region is rectified to canonical size first."""
    Hc, Wc = out_shape
    if corners is not None:
        dst = np.float32([[0, 0], [Wc - 1, 0], [Wc - 1, Hc - 1], [0, Hc - 1]])
        M = cv2.getPerspectiveTransform(np.float32(corners), dst)
        img = cv2.warpPerspective(img_bgr, M, (Wc, Hc), flags=cv2.INTER_LINEAR)
    else:
        img = cv2.resize(img_bgr, (Wc, Hc), interpolation=cv2.INTER_AREA)
    Y = to_y(img)[..., 0]
    hb, vb = dct_basis(b)
    cells = grid(Y.shape, b)
    order = np.random.default_rng(key).permutation(len(cells))
    soft = np.empty(n)
    for i in range(n):
        r, c = cells[order[i]]
        blk = Y[r:r + b, c:c + b]
        soft[i] = (blk * hb).sum() - (blk * vb).sum()
    return soft
