// Camera-readable hidden text, lab version: a JavaScript port of research/camera-watermark.
//
// The mark lives in the blue-yellow chroma (Cb) of the photo, on a grid of ~10,400 cells of 10 px. The grid's
// shape follows the photo's (snapped to one of a few standard ratios) so cells stay square on wide or tall
// pictures; each of 1024 coded bits owns ~10 scattered cells with a +1/-1 chip. Reading rectifies the picture
// from its four corners, estimates the ratio from them, and tries the nearest standard ratios.
//
// Lab only: the layout key is fixed and the text isn't encrypted yet.

export const CELL = 10, RAW = 1024, RATE = 3;
export const MSG_BYTES = 41, MSG_BITS = MSG_BYTES * 8, TEXT_BYTES = MSG_BYTES - 3; // length byte + CRC-16
const STRENGTH = 4, KEY = 1;

// ---- layout ------------------------------------------------------------------------------------------

function mulberry32(seed) {
  return () => {
    seed |= 0; seed = seed + 0x6D2B79F5 | 0;
    let t = Math.imul(seed ^ seed >>> 15, 1 | seed);
    t = t + Math.imul(t ^ t >>> 7, 61 | t) ^ t;
    return ((t ^ t >>> 14) >>> 0) / 4294967296;
  };
}

// Standard width:height ratios. A photo uses the nearest one (in log scale), which keeps its cells within
// ~12% of square.
const RATIOS = [1 / 3, 2 / 5, 1 / 2, 9 / 16, 2 / 3, 3 / 4, 1, 4 / 3, 3 / 2, 16 / 9, 2, 5 / 2, 3];
const CELLS = 102 * 102;

const layouts = new Map();
function layout(r) {
  let L = layouts.get(r);
  if (!L) {
    const ratio = RATIOS[r], gw = Math.round(Math.sqrt(CELLS * ratio)), gh = Math.round(CELLS / gw);
    const n = gw * gh, rand = mulberry32(KEY * 131 + r);
    const chips = new Int8Array(n), owner = new Int32Array(n), perm = new Int32Array(n);
    for (let i = 0; i < n; i++) { chips[i] = rand() < 0.5 ? -1 : 1; perm[i] = i; }
    for (let i = n - 1; i > 0; i--) { const j = Math.floor(rand() * (i + 1)); [perm[i], perm[j]] = [perm[j], perm[i]]; }
    for (let i = 0; i < n; i++) owner[perm[i]] = i % RAW;
    L = { gw, gh, chips, owner };
    layouts.set(r, L);
  }
  return L;
}

// Standard ratios nearest to `ratio`, closest first.
const nearestRatios = (ratio, k) => RATIOS.map((q, i) => [Math.abs(Math.log(q / ratio)), i])
  .sort((a, b) => a[0] - b[0]).slice(0, k).map(([, i]) => i);

// ---- image helpers ----------------------------------------------------------------------------------

// Gaussian blur approximated by three box blurs (separable, O(1) per pixel), edges clamped.
function boxesForGauss(sigma, n = 3) {
  const wIdeal = Math.sqrt((12 * sigma * sigma / n) + 1);
  let wl = Math.floor(wIdeal); if (wl % 2 === 0) wl--;
  const wu = wl + 2;
  const m = Math.round((12 * sigma * sigma - n * wl * wl - 4 * n * wl - 3 * n) / (-4 * wl - 4));
  return Array.from({ length: n }, (_, i) => i < m ? wl : wu);
}
function boxH(src, dst, w, h, r) {
  const iarr = 1 / (r + r + 1);
  for (let y = 0; y < h; y++) {
    const row = y * w; let acc = 0;
    for (let x = -r - 1; x < r; x++) acc += src[row + Math.min(w - 1, Math.max(0, x))];
    for (let x = 0; x < w; x++) {
      acc += src[row + Math.min(w - 1, x + r)] - src[row + Math.max(0, x - r - 1)];
      dst[row + x] = acc * iarr;
    }
  }
}
function boxV(src, dst, w, h, r) {
  const iarr = 1 / (r + r + 1);
  for (let x = 0; x < w; x++) {
    let acc = 0;
    for (let y = -r - 1; y < r; y++) acc += src[Math.min(h - 1, Math.max(0, y)) * w + x];
    for (let y = 0; y < h; y++) {
      acc += src[Math.min(h - 1, y + r) * w + x] - src[Math.max(0, y - r - 1) * w + x];
      dst[y * w + x] = acc * iarr;
    }
  }
}
export function blur(src, w, h, sigma) {
  let a = Float32Array.from(src), b = new Float32Array(src.length);
  for (const size of boxesForGauss(sigma)) {
    const r = (size - 1) / 2;
    boxH(a, b, w, h, r); boxV(b, a, w, h, r);
  }
  return a;
}

const lumaOf = (r, g, b) => 0.299 * r + 0.587 * g + 0.114 * b;
const cbOf = (r, g, b) => (b - lumaOf(r, g, b)) * 0.564 + 128;

// Bicubic (a = -0.75, like OpenCV) upsampling of a small grid by an integer factor.
function cubic(t) {
  const a = -0.75, x = Math.abs(t);
  return x <= 1 ? ((a + 2) * x - (a + 3)) * x * x + 1 : x < 2 ? ((a * x - 5 * a) * x + 8 * a) * x - 4 * a : 0;
}
function upsampleCubic(grid, gw, gh, f) {
  const W = gw * f, H = gh * f, tmp = new Float32Array(gh * W), out = new Float32Array(W * H);
  const pass = (get, n, m, put) => {
    for (let i = 0; i < n * f; i++) {
      const s = (i + 0.5) / f - 0.5, i0 = Math.floor(s);
      const w = [-1, 0, 1, 2].map(k => cubic(s - (i0 + k)));
      for (let j = 0; j < m; j++) {
        let v = 0;
        for (let k = 0; k < 4; k++) v += w[k] * get(Math.min(n - 1, Math.max(0, i0 - 1 + k)), j);
        put(i, j, v);
      }
    }
  };
  pass((x, y) => grid[y * gw + x], gw, gh, (x, y, v) => { tmp[y * W + x] = v; });
  pass((y, x) => tmp[y * W + x], gh, W, (y, x, v) => { out[y * W + x] = v; });
  return out;
}

// ---- error correction: convolutional code, constraint length 7, soft Viterbi -------------------------

const K = 7, NS = 1 << (K - 1), GENS = { 2: [0o171, 0o133], 3: [0o133, 0o171, 0o165] };
const parity = x => { let p = 0; while (x) { p ^= x & 1; x >>= 1; } return p; };
function tables(rate) {
  const out = [], nxt = [];
  for (let s = 0; s < NS; s++) for (let b = 0; b < 2; b++) {
    const reg = (b << (K - 1)) | s;
    out.push(GENS[rate].map(g => parity(reg & g)));
    nxt.push(reg >> 1);
  }
  return { out, nxt };
}
export function convEncode(bits, rate = RATE) {
  const { out, nxt } = tables(rate), res = [];
  let s = 0;
  for (const b of [...bits, ...new Array(K - 1).fill(0)]) { res.push(...out[s * 2 + b]); s = nxt[s * 2 + b]; }
  return res;
}
export function convDecode(soft, n, rate = RATE) {
  const { out, nxt } = tables(rate), steps = n + K - 1;
  let metric = new Float64Array(NS).fill(-Infinity); metric[0] = 0;
  const back = new Int32Array(steps * NS), bitAt = new Int8Array(steps * NS);
  for (let t = 0; t < steps; t++) {
    const next = new Float64Array(NS).fill(-Infinity);
    for (let s = 0; s < NS; s++) {
      if (metric[s] === -Infinity) continue;
      for (let b = 0; b < (t >= n ? 1 : 2); b++) {
        const o = out[s * 2 + b];
        let g = 0;
        for (let j = 0; j < rate; j++) g += (o[j] ? 1 : -1) * soft[t * rate + j];
        const d = nxt[s * 2 + b], m = metric[s] + g;
        if (m > next[d]) { next[d] = m; back[t * NS + d] = s; bitAt[t * NS + d] = b; }
      }
    }
    metric = next;
  }
  const bits = new Array(steps);
  for (let t = steps - 1, s = 0; t >= 0; t--) { bits[t] = bitAt[t * NS + s]; s = back[t * NS + s]; }
  return bits.slice(0, n);
}

// ---- message framing (lab): [length][text...][CRC-16] ------------------------------------------------

function crc16(bytes) {
  let c = 0xFFFF;
  for (const x of bytes) {
    c ^= x << 8;
    for (let i = 0; i < 8; i++) c = c & 0x8000 ? (c << 1) ^ 0x1021 : c << 1;
    c &= 0xFFFF;
  }
  return c;
}
export function frame(text) {
  const t = new TextEncoder().encode(text);
  if (t.length > TEXT_BYTES) throw new Error(`Text is ${t.length} bytes; the limit is ${TEXT_BYTES}.`);
  const b = new Uint8Array(MSG_BYTES);
  b[0] = t.length; b.set(t, 1);
  const c = crc16(b.subarray(0, MSG_BYTES - 2));
  b[MSG_BYTES - 2] = c >> 8; b[MSG_BYTES - 1] = c & 0xFF;
  const bits = [];
  for (const x of b) for (let i = 7; i >= 0; i--) bits.push((x >> i) & 1);
  return bits;
}
function unframe(bits) {
  const b = new Uint8Array(MSG_BYTES);
  for (let i = 0; i < MSG_BITS; i++) b[i >> 3] |= bits[i] << (7 - (i & 7));
  const ok = crc16(b.subarray(0, MSG_BYTES - 2)) === (b[MSG_BYTES - 2] << 8 | b[MSG_BYTES - 1]);
  if (!ok || b[0] > TEXT_BYTES) return null;
  return new TextDecoder("utf-8", { fatal: false }).decode(b.subarray(1, 1 + b[0]));
}

// ---- embed ----------------------------------------------------------------------------------------------

// Returns a canvas holding the marked photo, resized so its shorter side is 1024 px.
export function embed(source, text) {
  const sw = source.width, sh = source.height, s = 1024 / Math.min(sw, sh);
  const W = Math.round(sw * s), H = Math.round(sh * s);
  const canvas = document.createElement("canvas");
  canvas.width = W; canvas.height = H;
  const ctx = canvas.getContext("2d", { willReadFrequently: true });
  ctx.imageSmoothingQuality = "high";
  ctx.drawImage(source, 0, 0, W, H);
  const img = ctx.getImageData(0, 0, W, H), px = img.data;

  const coded = convEncode(frame(text));
  const bits = new Int8Array(RAW); bits.set(coded);
  const L = layout(nearestRatios(W / H, 1)[0]), PW = L.gw * CELL, PH = L.gh * CELL;
  const grid = new Float32Array(L.gw * L.gh);
  for (let i = 0; i < grid.length; i++) grid[i] = L.chips[i] * (bits[L.owner[i]] ? 1 : -1);
  const up = blur(upsampleCubic(grid, L.gw, L.gh, CELL), PW, PH, CELL / 6);
  let peak = 0; for (const v of up) peak = Math.max(peak, Math.abs(v));

  // texture mask from luminance: flat areas get half strength
  const Y = new Float32Array(W * H);
  for (let i = 0; i < W * H; i++) Y[i] = lumaOf(px[4 * i], px[4 * i + 1], px[4 * i + 2]);
  const lo = blur(Y, W, H, 3), act = new Float32Array(W * H);
  for (let i = 0; i < W * H; i++) act[i] = Math.abs(Y[i] - lo[i]);
  const actS = blur(act, W, H, 6);

  for (let y = 0; y < H; y++) for (let x = 0; x < W; x++) {
    // bilinear sample of the pattern stretched over the photo
    const gx = (x + 0.5) * PW / W - 0.5, gy = (y + 0.5) * PH / H - 0.5;
    if (gx < 0 || gy < 0 || gx >= PW - 1 || gy >= PH - 1) continue;
    const x0 = gx | 0, y0 = gy | 0, fx = gx - x0, fy = gy - y0, o = y0 * PW + x0;
    const f = (up[o] * (1 - fx) + up[o + 1] * fx) * (1 - fy) + (up[o + PW] * (1 - fx) + up[o + PW + 1] * fx) * fy;
    const i = y * W + x, m = 0.5 + 0.5 * Math.min(1, actS[i] / 10);
    const d = STRENGTH * m * (f / peak) * 2; // change in Cb
    px[4 * i + 2] = Math.max(0, Math.min(255, Math.round(px[4 * i + 2] + 1.773 * d)));
    px[4 * i + 1] = Math.max(0, Math.min(255, Math.round(px[4 * i + 1] - 0.344 * d)));
  }
  ctx.putImageData(img, 0, 0);
  return canvas;
}

// ---- read -----------------------------------------------------------------------------------------------

// Homography mapping the unit square's corners (0,0),(1,0),(1,1),(0,1) to four points.
function squareToQuad(q) {
  const [[x0, y0], [x1, y1], [x2, y2], [x3, y3]] = q;
  const dx1 = x1 - x2, dx2 = x3 - x2, dy1 = y1 - y2, dy2 = y3 - y2;
  const sx = x0 - x1 + x2 - x3, sy = y0 - y1 + y2 - y3;
  const det = dx1 * dy2 - dx2 * dy1;
  const g = (sx * dy2 - dx2 * sy) / det, h = (dx1 * sy - sx * dy1) / det;
  return [x1 - x0 + g * x1, x3 - x0 + h * x3, x0, y1 - y0 + g * y1, y3 - y0 + h * y3, y0, g, h];
}

// The picture's Cb plane, computed once per image (reading scores hundreds of corner guesses).
const cbCache = new WeakMap();
function cbPlane(img) {
  let cb = cbCache.get(img);
  if (!cb) {
    const px = img.data, n = img.width * img.height;
    cb = new Float32Array(n);
    for (let i = 0; i < n; i++) cb[i] = cbOf(px[4 * i], px[4 * i + 1], px[4 * i + 2]);
    cbCache.set(img, cb);
  }
  return cb;
}

// Sample the picture inside `corners` onto an NW x NH rectangle and return its Cb plane.
function rectifyCb(img, corners, NW, NH) {
  const [a, b, c, d, e, f, g, h] = squareToQuad(corners);
  const W = img.width, H = img.height, src = cbPlane(img), out = new Float32Array(NW * NH);
  for (let v = 0; v < NH; v++) {
    const t = (v + 0.5) / NH;
    for (let u = 0; u < NW; u++) {
      const s = (u + 0.5) / NW, z = g * s + h * t + 1;
      const x = (a * s + b * t + c) / z - 0.5, y = (d * s + e * t + f) / z - 0.5;
      const x0 = Math.max(0, Math.min(W - 2, Math.floor(x))), y0 = Math.max(0, Math.min(H - 2, Math.floor(y)));
      const fx = Math.min(1, Math.max(0, x - x0)), fy = Math.min(1, Math.max(0, y - y0));
      const i = y0 * W + x0;
      out[v * NW + u] = (src[i] * (1 - fx) + src[i + 1] * fx) * (1 - fy) + (src[i + W] * (1 - fx) + src[i + W + 1] * fx) * fy;
    }
  }
  return out;
}

const hann = n => Array.from({ length: n }, (_, i) => 0.5 - 0.5 * Math.cos(2 * Math.PI * (i + 1) / (n + 1)));

// Soft value per raw bit (positive means 1). `f` = 2 reads at half resolution: 4x faster, a little
// noisier, which is enough to judge alignment during the coarse steps.
export function softBits(img, corners, r, f = 1) {
  const L = layout(r), cell = CELL / f, NW = L.gw * cell, NH = L.gh * cell, win = hann(cell);
  const cb = rectifyCb(img, corners, NW, NH), lo = blur(cb, NW, NH, cell * 0.9), soft = new Float64Array(RAW);
  for (let cy = 0; cy < L.gh; cy++) for (let cx = 0; cx < L.gw; cx++) {
    let acc = 0;
    for (let j = 0; j < cell; j++) {
      const row = (cy * cell + j) * NW + cx * cell;
      for (let i = 0; i < cell; i++) {
        const hp = Math.max(-12, Math.min(12, cb[row + i] - lo[row + i]));
        acc += hp * win[i] * win[j];
      }
    }
    const idx = cy * L.gw + cx;
    soft[L.owner[idx]] += acc * L.chips[idx];
  }
  return soft;
}

const strength = soft => soft.reduce((s, v) => s + Math.abs(v), 0);

// Nudge each corner coordinate to where the hidden pattern is strongest, one coordinate at a time.
// `steps` are in pixels of a 1080p frame; `full` scores at full resolution (precise, ~6x slower) instead of
// half.
export function refine(img, corners, r, { steps = [16, 8, 4, 2, 1], full = false, onStep = () => {} } = {}) {
  const f = full ? 1 : 2, scale = Math.max(img.width, img.height) / 1920;
  let best = corners.map(p => p.slice()), bestScore = strength(softBits(img, best, r, f));
  for (const step of steps) {
    for (let round = 0; round < 2; round++) {
      let improved = false;
      for (let i = 0; i < 4; i++) for (let j = 0; j < 2; j++) for (const dir of [-1, 1]) {
        const c = best.map(p => p.slice());
        c[i][j] += dir * step * scale;
        const sc = strength(softBits(img, c, r, f));
        if (sc > bestScore) { best = c; bestScore = sc; improved = true; }
      }
      onStep(step);
      if (!improved) break;
    }
  }
  return best;
}

// Decode at these corners. `match` is how many raw bits agree with the best codeword: about 75–78% when there
// is no pattern at all (the decoder always finds the nearest codeword), about 99% for a clean file. A
// diagnostic for real-camera tests.
function decodeAt(img, corners, r) {
  const soft = softBits(img, corners, r).slice(0, (MSG_BITS + K - 1) * RATE);
  const sorted = Array.from(soft, Math.abs).sort((a, b) => a - b);
  const med = sorted[sorted.length >> 1] || 1;
  const bits = convDecode(Array.from(soft, v => Math.max(-3, Math.min(3, v / med))), MSG_BITS);
  const code = convEncode(bits);
  let agree = 0;
  for (let i = 0; i < code.length; i++) agree += (soft[i] > 0) === (code[i] === 1);
  return { text: unframe(bits), match: agree / code.length };
}

// ---- snapping rough corners to the picture's edges -----------------------------------------------------

// Luminance plane, lightly blurred, cached per image.
const lumaCache = new WeakMap();
function lumaPlane(img) {
  let y = lumaCache.get(img);
  if (!y) {
    const px = img.data, n = img.width * img.height;
    y = new Float32Array(n);
    for (let i = 0; i < n; i++) y[i] = lumaOf(px[4 * i], px[4 * i + 1], px[4 * i + 2]);
    y = blur(y, img.width, img.height, 1.5);
    lumaCache.set(img, y);
  }
  return y;
}

function sample(plane, W, H, x, y) {
  if (x < 0 || y < 0 || x >= W - 1 || y >= H - 1) return NaN;
  const x0 = x | 0, y0 = y | 0, fx = x - x0, fy = y - y0, i = y0 * W + x0;
  return (plane[i] * (1 - fx) + plane[i + 1] * fx) * (1 - fy) + (plane[i + W] * (1 - fx) + plane[i + W + 1] * fx) * fy;
}

// How strongly a straight edge runs from p to q: mean |brightness step| across the line, sampled away from
// the ends (corners may be rounded or cut off).
function edgeScore(Y, W, H, p, q, n) {
  let sum = 0, cnt = 0;
  for (let k = 0; k < 64; k++) {
    const t = 0.1 + 0.8 * k / 63, x = p[0] + (q[0] - p[0]) * t, y = p[1] + (q[1] - p[1]) * t;
    const d = sample(Y, W, H, x + 2 * n[0], y + 2 * n[1]) - sample(Y, W, H, x - 2 * n[0], y - 2 * n[1]);
    if (!Number.isNaN(d)) { sum += Math.abs(d); cnt++; }
  }
  return cnt > 32 ? sum / cnt : 0;
}

function intersect([p1, q1], [p2, q2]) {
  const d1 = [q1[0] - p1[0], q1[1] - p1[1]], d2 = [q2[0] - p2[0], q2[1] - p2[1]];
  const den = d1[0] * d2[1] - d1[1] * d2[0];
  if (Math.abs(den) < 1e-9) return null;
  const t = ((p2[0] - p1[0]) * d2[1] - (p2[1] - p1[1]) * d2[0]) / den;
  return [p1[0] + t * d1[0], p1[1] + t * d1[1]];
}

// Candidate positions for the picture's real edges near roughly placed corners. For each side, try shifting
// both of its ends sideways (up to 12% of the shot's size) and keep the `k` strongest straight edges that are
// clearly apart; a strong edge inside the photo (a horizon, a wall) can outscore the border, so the caller
// picks among them. Returns, per side, a list of [p, q] lines.
function edgeCandidates(img, corners, k = 3) {
  const { width: W, height: H } = img, Y = lumaPlane(img), R = Math.round(0.12 * Math.max(W, H));
  const sides = [];
  for (let side = 0; side < 4; side++) {
    const p = corners[side], q = corners[(side + 1) % 4];
    const len = Math.hypot(q[0] - p[0], q[1] - p[1]) || 1;
    const n = [-(q[1] - p[1]) / len, (q[0] - p[0]) / len];
    const at = (a, b) => [[p[0] + a * n[0], p[1] + a * n[1]], [q[0] + b * n[0], q[1] + b * n[1]]];
    const coarse = [];
    for (let a = -R; a <= R; a += 4) for (let b = -R; b <= R; b += 4) {
      const [pp, qq] = at(a, b);
      coarse.push([edgeScore(Y, W, H, pp, qq, n), a, b]);
    }
    coarse.sort((x, y) => y[0] - x[0]);
    const picked = [];
    for (const [, a, b] of coarse) {
      if (picked.some(([pa, pb]) => Math.abs(pa - a) < 12 && Math.abs(pb - b) < 12)) continue;
      // polish to 1 px
      let best = [a, b], bestScore = -1;
      for (let da = -4; da <= 4; da++) for (let db = -4; db <= 4; db++) {
        const [pp, qq] = at(a + da, b + db), sc = edgeScore(Y, W, H, pp, qq, n);
        if (sc > bestScore) { bestScore = sc; best = [a + da, b + db]; }
      }
      picked.push(best);
      if (picked.length === k) break;
    }
    sides.push(picked.map(([a, b]) => at(a, b)));
  }
  return sides;
}

const quadFrom = s => [intersect(s[3], s[0]), intersect(s[0], s[1]), intersect(s[1], s[2]), intersect(s[2], s[3])];

// Move roughly placed corners onto the picture's real edges, using the hidden pattern to choose between
// candidate edges: every combination of each side's strongest candidates is scored by how clearly the pattern
// shows (at half resolution) and the best wins. Returns { corners, r } or null if nothing plausible was found.
export function snapToEdges(img, corners) {
  const cand = edgeCandidates(img, corners);
  let best = null;
  for (const s0 of cand[0]) for (const s1 of cand[1]) for (const s2 of cand[2]) for (const s3 of cand[3]) {
    const q = quadFrom([s0, s1, s2, s3]);
    if (q.some(c => !c)) continue;
    for (const r of nearestRatios(ratioOf(q), 2)) {
      const sc = strength(softBits(img, q, r, 2));
      if (!best || sc > best.score) best = { corners: q, r, score: sc };
    }
  }
  return best;
}

// Width:height of the picture from its corners (averaging opposite sides; perspective makes this rough).
export function ratioOf(c) {
  const d = (p, q) => Math.hypot(q[0] - p[0], q[1] - p[1]);
  return (d(c[0], c[1]) + d(c[3], c[2])) / (d(c[0], c[3]) + d(c[1], c[2]));
}

// Decode the text from a picture whose corners (tl, tr, br, bl) are roughly known. Cheapest first, stopping as
// soon as the checksum passes: the corners as given, then snapped to the picture's edges, then a fast
// half-resolution alignment, then a precise full-resolution one. Each stage tries the standard ratios nearest
// the corners' shape (later stages only the most promising). Returns { text | null, match, corners, stage }.
export function read(img, corners, { onStep } = {}) {
  let best = { text: null, match: 0, corners, stage: 0 };
  const attempt = (c, r, stage) => {
    const res = { ...decodeAt(img, c, r), corners: c, stage, r };
    if (res.text !== null || res.match > best.match) best = res;
    return res.text !== null;
  };
  const cands = nearestRatios(ratioOf(corners), 3);
  for (const r of cands) if (attempt(corners, r, 0)) return best;
  // hand-placed corners on a phone are tens of pixels off: snap them to the picture's edges
  const snap = snapToEdges(img, corners);
  const snapped = snap ? snap.corners : corners;
  const cands2 = snap ? [snap.r, ...nearestRatios(ratioOf(snapped), 3).filter(r => r !== snap.r)].slice(0, 3)
    : nearestRatios(ratioOf(snapped), 3);
  for (const r of cands2) if (attempt(snapped, r, 1)) return best;
  const order = [...cands2].sort((a, b) => (best.r === b) - (best.r === a));
  for (const r of order.slice(0, 2)) {
    const c = refine(img, snapped, r, { onStep });
    if (attempt(c, r, 2)) return best;
  }
  const r = best.r ?? cands2[0];
  attempt(refine(img, best.corners, r, { steps: [4, 2, 1], full: true, onStep }), r, 3);
  return best;
}
