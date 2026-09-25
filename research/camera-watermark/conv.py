"""Convolutional code (constraint length 7) with a soft-decision Viterbi decoder."""
import numpy as np

K = 7
NS = 1 << (K - 1)
GENS = {2: (0o171, 0o133), 3: (0o133, 0o171, 0o165)}


def _parity(x):
    x = np.asarray(x)
    p = np.zeros_like(x)
    while np.any(x):
        p ^= x & 1
        x = x >> 1
    return p


def _tables(rate):
    gens = GENS[rate]
    states = np.arange(NS)
    out = np.zeros((NS, 2, rate), np.int8)
    nxt = np.zeros((NS, 2), np.int64)
    for b in (0, 1):
        reg = (b << (K - 1)) | states  # newest bit on top
        for j, g in enumerate(gens):
            out[:, b, j] = _parity(reg & g)
        nxt[:, b] = reg >> 1
    return out, nxt


def encode(bits, rate=2):
    out, nxt = _tables(rate)
    s = 0
    res = []
    for b in list(bits) + [0] * (K - 1):  # flush
        res.extend(out[s, b])
        s = nxt[s, b]
    return np.array(res, np.int8)


def coded_len(n, rate=2):
    return (n + K - 1) * rate


def decode(soft, n, rate=2):
    """soft: positive means 1. Returns n decoded bits."""
    out, nxt = _tables(rate)
    sym = np.where(out == 1, 1.0, -1.0)  # (NS, 2, rate)
    steps = n + K - 1
    soft = np.asarray(soft, float)[:steps * rate].reshape(steps, rate)
    metric = np.full(NS, -np.inf)
    metric[0] = 0
    prev_state = np.zeros((steps, NS), np.int64)
    prev_bit = np.zeros((steps, NS), np.int8)
    src = np.repeat(np.arange(NS), 2)
    bitv = np.tile([0, 1], NS)
    dst = nxt.reshape(-1)
    for t in range(steps):
        gain = (sym * soft[t]).sum(axis=2).reshape(-1)  # per (state, bit)
        cand = metric[src] + gain
        if t >= n:  # tail: only zeros
            cand = np.where(bitv == 0, cand, -np.inf)
        new = np.full(NS, -np.inf)
        order = np.argsort(cand)  # write best last so it wins
        new[dst[order]] = cand[order]
        best_src = np.empty(NS, np.int64)
        best_bit = np.empty(NS, np.int8)
        best_src[dst[order]] = src[order]
        best_bit[dst[order]] = bitv[order]
        metric = new
        prev_state[t] = best_src
        prev_bit[t] = best_bit
    s = 0  # terminated in state 0
    bits = np.empty(steps, np.int8)
    for t in range(steps - 1, -1, -1):
        bits[t] = prev_bit[t, s]
        s = prev_state[t, s]
    return bits[:n]


if __name__ == "__main__":
    r = np.random.default_rng(0)
    for rate in (2, 3):
        for ber in (0.05, 0.08, 0.12):
            fails = 0
            for _ in range(50):
                m = r.integers(0, 2, 200)
                c = encode(m, rate)
                noisy = np.where(r.random(len(c)) < ber, 1 - c, c)
                soft = np.where(noisy == 1, 1.0, -1.0)
                fails += np.any(decode(soft, len(m), rate) != m)
            print(f"rate 1/{rate} hard BER {ber}: {fails}/50 blocks failed")
