"""Spread-spectrum watermark prototype (classical, no ML).

The canonical-size luminance plane is cut into small cells. A key-seeded permutation gives each
of the K bits an equal share of cells, scattered over the whole image, and each cell a key-seeded
chip (+1/-1). The mark is chip * bit_sign per cell, smoothed so it has no block edges, and scaled
by a texture mask so flat areas (sky, walls) get almost nothing. Decoding: rectify, remove the
picture itself with a high-pass, average each cell, multiply by its chip, and sum per bit.
"""
import numpy as np
import cv2

CANON_W = 1024


def canon_size(h, w):
    return int(round(h * CANON_W / w / 16) * 16), CANON_W


def layout(shape, cell, k, key):
    H, W = shape
    gh, gw = H // cell, W // cell
    n = gh * gw
    r = np.random.default_rng(key)
    chips = r.choice([-1.0, 1.0], n)
    owner = np.empty(n, int)
    owner[r.permutation(n)] = np.arange(n) % k  # each bit gets ~n/k scattered cells
    return gh, gw, chips.reshape(gh, gw), owner.reshape(gh, gw)


def mask(Y, floor=0.15):
    """Where changes hide: local texture, smoothed. 0..1."""
    hp = Y - cv2.GaussianBlur(Y, (0, 0), 3)
    act = cv2.GaussianBlur(np.abs(hp), (0, 0), 6)
    m = np.clip(act / 10.0, 0, 1)
    return floor + (1 - floor) * m


def embed(img_bgr, bits, cell=12, strength=4.0, key=1, ch=0):
    Hc, Wc = canon_size(*img_bgr.shape[:2])
    img = cv2.resize(img_bgr, (Wc, Hc), interpolation=cv2.INTER_AREA)
    ycc = cv2.cvtColor(img, cv2.COLOR_BGR2YCrCb).astype(np.float32)
    Y = ycc[..., ch]
    gh, gw, chips, owner = layout(Y.shape, cell, len(bits), key)
    sign = np.where(np.asarray(bits)[owner] == 1, 1.0, -1.0)
    grid = (chips * sign).astype(np.float32)
    # smooth the cell pattern: cubic upsampling then a light blur, so there are no hard edges
    field = cv2.resize(grid, (gw * cell, gh * cell), interpolation=cv2.INTER_CUBIC)
    field = cv2.GaussianBlur(field, (0, 0), cell / 6)
    full = np.zeros_like(Y)
    full[:gh * cell, :gw * cell] = field
    full /= (np.abs(full).max() + 1e-6)
    m = mask(ycc[..., 0], floor=0.5 if ch else 0.15)
    Y2 = Y + strength * m * full * 2
    ycc[..., ch] = np.clip(Y2, 0, 255)
    return cv2.cvtColor(np.round(ycc).astype(np.uint8), cv2.COLOR_YCrCb2BGR)


def rectify(img_bgr, corners, shape):
    Hc, Wc = shape
    if corners is None:
        return cv2.resize(img_bgr, (Wc, Hc), interpolation=cv2.INTER_AREA)
    dst = np.float32([[0, 0], [Wc - 1, 0], [Wc - 1, Hc - 1], [0, Hc - 1]])
    M = cv2.getPerspectiveTransform(np.float32(corners), dst)
    return cv2.warpPerspective(img_bgr, M, (Wc, Hc), flags=cv2.INTER_LINEAR)


def extract(img_bgr, k, cell=12, key=1, corners=None, shape=None, ch=0):
    img = rectify(img_bgr, corners, shape)
    Y = cv2.cvtColor(img, cv2.COLOR_BGR2YCrCb)[..., ch].astype(np.float32)
    # remove the picture: subtract a local mean at roughly the cell scale
    hp = Y - cv2.GaussianBlur(Y, (0, 0), cell * 0.9)
    # tame strong edges so they don't swamp the mark
    hp = np.clip(hp, -12, 12)
    gh, gw, chips, owner = layout(Y.shape, cell, k, key)
    cells = hp[:gh * cell, :gw * cell].reshape(gh, cell, gw, cell)
    # weight the centre of each cell (where the smoothed chip is strongest)
    w = np.hanning(cell + 2)[1:-1]
    w2 = np.outer(w, w)
    v = (cells * w2[None, :, None, :]).sum(axis=(1, 3)) * chips
    soft = np.bincount(owner.ravel(), weights=v.ravel(), minlength=k)
    return soft


def _field(amp_grid, shape, cell):
    gh, gw = amp_grid.shape
    f = cv2.resize(amp_grid.astype(np.float32), (gw * cell, gh * cell), interpolation=cv2.INTER_CUBIC)
    f = cv2.GaussianBlur(f, (0, 0), cell / 6)
    full = np.zeros(shape, np.float32)
    full[:gh * cell, :gw * cell] = f
    return full


def embed_informed(img_bgr, bits, cell=12, target=40.0, amax=8.0, key=1, passes=3):
    """Push each bit's statistic to at least `target` in the right direction, adding no more
    than needed, with the per-pixel change capped by amax * mask."""
    Hc, Wc = canon_size(*img_bgr.shape[:2])
    img = cv2.resize(img_bgr, (Wc, Hc), interpolation=cv2.INTER_AREA)
    ycc = cv2.cvtColor(img, cv2.COLOR_BGR2YCrCb).astype(np.float32)
    Y0 = ycc[..., 0].copy()
    k = len(bits)
    gh, gw, chips, owner = layout(Y0.shape, cell, k, key)
    sign = np.where(np.asarray(bits) == 1, 1.0, -1.0)
    m = mask(Y0)
    # unit response: how much one unit of amplitude on bit j moves its own statistic
    unit = _field(chips * sign[owner], Y0.shape, cell) * m
    img_u = ycc.copy(); img_u[..., 0] = 128 + unit * 20
    g = extract(cv2.cvtColor(np.clip(img_u, 0, 255).astype(np.uint8), cv2.COLOR_YCrCb2BGR), k, cell, key, None, (Hc, Wc)) / 20 * sign
    g = np.maximum(g, 1e-3)
    alpha = np.zeros(k)
    Y = Y0
    for _ in range(passes):
        cur = extract(cv2.cvtColor(np.clip(np.dstack([Y, ycc[..., 1], ycc[..., 2]]), 0, 255).astype(np.uint8),
                                   cv2.COLOR_YCrCb2BGR), k, cell, key, None, (Hc, Wc)) * sign
        alpha = np.clip(alpha + np.maximum(target - cur, 0) / g, 0, amax)
        Y = Y0 + _field(chips * (sign * alpha)[owner], Y0.shape, cell) * m
    ycc[..., 0] = np.clip(Y, 0, 255)
    return cv2.cvtColor(np.round(ycc).astype(np.uint8), cv2.COLOR_YCrCb2BGR), alpha


def refine_corners(img_bgr, corners, k, cell, shape, key=1, ch=2, steps=(8, 4, 2, 1), rounds=2):
    """Nudge each corner coordinate to maximise the pattern's total signal (sum of |soft|)."""
    c = np.float32(corners).copy()
    # work on a smaller canonical grid for speed: the signal survives half resolution poorly,
    # so keep full size but only score a subset of bits
    def score(cc):
        s = extract(img_bgr, k, cell=cell, key=key, corners=cc, shape=shape, ch=ch)
        return np.abs(s).sum()
    best = score(c)
    for step in steps:
        for _ in range(rounds):
            improved = False
            for i in range(4):
                for j in range(2):
                    for d in (-step, step):
                        cc = c.copy(); cc[i, j] += d
                        sc = score(cc)
                        if sc > best:
                            best, c, improved = sc, cc, True
            if not improved:
                break
    return c
