// TrustMark payload decoder: BCH_5 error correction (up to 5 wrong bits); plain 7-bit ASCII text for marks
// made without a password (tmseal.js opens the others).
// Written from Adobe's Python reference (trustmark/bchecc.py + datalayer.py, MIT), which follows the Linux
// kernel BCH layout: 61 data bits padded to 64, then 35 check bits; codeword c(x) = data(x)·x^35 + ecc(x),
// first bit = highest power. Field GF(2^7), polynomial x^7 + x^3 + 1 (137).

const M = 7, N = 127, T = 5, POLY = 137, DATA = 61, ECC = 35;
const EXP = new Array(2 * N), LOG = new Array(N + 1);
for (let i = 0, x = 1; i < N; i++) {
  EXP[i] = EXP[i + N] = x; LOG[x] = i;
  x <<= 1; if (x & (1 << M)) x ^= POLY;
}
const mul = (a, b) => (a && b ? EXP[LOG[a] + LOG[b]] : 0);

// Generator g(x): binary polynomial whose roots are alpha^1..alpha^2T and their conjugates. Coefficients over
// GF(2^7) while multiplying out; the result is binary (0/1), highest power first.
const GEN = (() => {
  const roots = new Set();
  for (let i = 1; i < 2 * T; i += 2) for (let r = i, j = 0; j < M; j++, r = (2 * r) % N) roots.add(r);
  let g = [1];
  for (const r of roots) {  // g *= (x + alpha^r)
    const next = [...g, 0];
    for (let i = 0; i < g.length; i++) next[i + 1] ^= mul(g[i], EXP[r]);
    g = next;
  }
  return g;  // length ECC + 1
})();

// "0101…" of 61 data bits -> 100 booleans: data, 35 check bits (remainder of data(x)·x^35 mod g, with the 3
// padding zeros counted as data), version 0001.
export function encode(data) {
  const r = [...data, "0", "0", "0"].map(Number).concat(new Array(ECC).fill(0));
  for (let i = 0; i < DATA + 3; i++) if (r[i]) for (let j = 0; j < GEN.length; j++) r[i + j] ^= GEN[j];
  return [...data].map(b => b === "1").concat(r.slice(DATA + 3).map(Boolean), [false, false, false, true]);
}

// bits: 100 booleans from the decoder model (logit > 0). Returns the 61 corrected data bits as a "0101…"
// string, or null if they aren't a BCH_5 codeword within 5 errors.
export function correct(bits) {
  // Bits 96–99 name the schema (0001 = BCH_5). No ECC covers them and we only write BCH_5, so they're
  // ignored: checking them rejected ~9% of good reads whose flipped bits landed there.
  if (bits.length !== 100) return null;
  const r = [...bits.slice(0, DATA), 0, 0, 0, ...bits.slice(DATA, DATA + ECC)].map(Number);
  const len = r.length;  // 99; bit k is the coefficient of x^(len-1-k)

  const syn = [];
  for (let j = 1; j <= 2 * T; j++) {
    let s = 0;
    for (let k = 0; k < len; k++) if (r[k]) s ^= EXP[(j * (len - 1 - k)) % N];
    syn.push(s);
  }
  if (syn.some(s => s)) {
    // Berlekamp–Massey: error locator sigma(x)
    let sigma = [1], prev = [1], L = 0, m = 1, b = 1;
    for (let n = 0; n < 2 * T; n++) {
      let d = syn[n];
      for (let i = 1; i <= L; i++) d ^= mul(sigma[i] || 0, syn[n - i]);
      if (!d) { m++; continue; }
      const coef = EXP[(LOG[d] - LOG[b] + N) % N], next = sigma.slice();
      for (let i = 0; i < prev.length; i++) next[i + m] = (next[i + m] || 0) ^ mul(coef, prev[i]);
      if (2 * L <= n) { prev = sigma; L = n + 1 - L; b = d; m = 1; } else m++;
      sigma = next;
    }
    if (L > T) return null;
    // Chien search over the shortened code: an error at power p makes sigma(alpha^-p) = 0
    const found = [];
    for (let p = 0; p < len; p++) {
      let v = 0;
      for (let i = 0; i < sigma.length; i++) if (sigma[i]) v ^= EXP[(LOG[sigma[i]] + i * (N - p)) % N];
      if (!v) found.push(len - 1 - p);
    }
    if (found.length !== L) return null;  // more errors than the code can fix
    for (const k of found) r[k] ^= 1;
  }
  if (r[61] || r[62] || r[63]) return null;  // padding is sent as 0, so a "fix" there is a miscorrection
  return r.slice(0, DATA).join("");
}

// Plain (no password) marks: 8 characters of 7-bit ASCII. Many tries (boxes × nudges × rotations) let random
// bits pass BCH now and then, so demand what a real mark has: bits 56–60 zero and printable ASCII. Sealed
// marks don't need this: their tag rejects random bits.
export function decodePayload(bits) {
  const r = correct(bits);
  if (r === null || r.slice(56).includes("1")) return null;
  let text = "";
  for (let i = 0; i < 56; i += 7) text += String.fromCharCode(parseInt(r.slice(i, i + 7), 2));
  text = text.replace(/\0+$/, "");
  return /^[\x20-\x7e]+$/.test(text) ? text.trim() : null;
}
