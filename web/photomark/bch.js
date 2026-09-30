// TrustMark payload codes: BCH_5 (fixes up to 5 wrong bits; single marks) and BCH_3 (fixes 3; one per quarter
// of a four-quarter mark); plain 7-bit ASCII text for marks made without a password (tmseal.js opens the rest).
// Written from Adobe's Python reference (trustmark/bchecc.py + datalayer.py, MIT), which follows the Linux
// kernel BCH layout: data bits zero-padded to whole bytes, then the check bits; codeword
// c(x) = data(x)·x^ecc + ecc(x), first bit = highest power. Field GF(2^7), polynomial x^7 + x^3 + 1 (137).

const M = 7, N = 127, POLY = 137;
const EXP = new Array(2 * N), LOG = new Array(N + 1);
for (let i = 0, x = 1; i < N; i++) {
  EXP[i] = EXP[i + N] = x; LOG[x] = i;
  x <<= 1; if (x & (1 << M)) x ^= POLY;
}
const mul = (a, b) => (a && b ? EXP[LOG[a] + LOG[b]] : 0);

// Generator g(x): binary polynomial whose roots are alpha^1..alpha^2T and their conjugates. Coefficients over
// GF(2^7) while multiplying out; the result is binary (0/1), highest power first.
function generator(T) {
  const roots = new Set();
  for (let i = 1; i < 2 * T; i += 2) for (let r = i, j = 0; j < M; j++, r = (2 * r) % N) roots.add(r);
  let g = [1];
  for (const r of roots) {  // g *= (x + alpha^r)
    const next = [...g, 0];
    for (let i = 0; i < g.length; i++) next[i + 1] ^= mul(g[i], EXP[r]);
    g = next;
  }
  return g;  // length ECC + 1
}
// T errors fixed; DATA bits (+PAD zeros to a whole byte); ECC = 7·T check bits; the last 4 of the 100 bits
// name the schema.
const CODES = {
  5: { T: 5, DATA: 61, PAD: 3, ECC: 35, VERSION: [0, 0, 0, 1], GEN: generator(5) },
  3: { T: 3, DATA: 75, PAD: 5, ECC: 21, VERSION: [0, 0, 1, 1], GEN: generator(3) },
};

// "0101…" of DATA bits -> 100 booleans: data, check bits (remainder of data(x)·x^ECC mod g, with the padding
// zeros counted as data), schema.
export function encode(data, t = 5) {
  const { DATA, PAD, ECC, VERSION, GEN } = CODES[t];
  const r = [...data.padEnd(DATA + PAD, "0")].map(Number).concat(new Array(ECC).fill(0));
  for (let i = 0; i < DATA + PAD; i++) if (r[i]) for (let j = 0; j < GEN.length; j++) r[i + j] ^= GEN[j];
  return [...data].map(b => b === "1").concat(r.slice(DATA + PAD).map(Boolean), VERSION.map(Boolean));
}

// bits: 100 booleans from the decoder model (logit > 0). Returns the corrected data bits as a "0101…" string,
// or null if they aren't a codeword of that code within t errors.
export function correct(bits, t = 5) {
  // The 4 schema bits have no ECC, so they're ignored: the caller says which code it expects (checking them
  // rejected ~9% of good reads whose flipped bits landed there).
  const { T, DATA, PAD, ECC } = CODES[t];
  if (bits.length !== 100) return null;
  const r = [...bits.slice(0, DATA), ...new Array(PAD).fill(0), ...bits.slice(DATA, DATA + ECC)].map(Number);
  const len = r.length;  // bit k is the coefficient of x^(len-1-k)

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
  if (r.slice(DATA, DATA + PAD).includes(1)) return null;  // padding is sent as 0: a "fix" there is wrong
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
