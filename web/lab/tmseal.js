// Password layer for a TrustMark payload, browser twin of research/ai-watermarks/tmseal.py (see there for the
// format): 61 bits = nonce (9) | ciphertext (36, six 6-bit characters) | tag (16). WebCrypto only.

export const ALPHABET = " abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789.";
const CHARS = 6, NONCE = 9, TAG = 16, ROUNDS = 600_000;
const enc = new TextEncoder(), subtle = crypto.subtle;

const pack = bits => {  // "0101…" -> bytes, MSB first, zero-padded
  const out = new Uint8Array(Math.ceil(bits.length / 8));
  for (let i = 0; i < bits.length; i++) if (bits[i] === "1") out[i >> 3] |= 128 >> (i & 7);
  return out;
};
const bitsOf = (buf, n) => [...new Uint8Array(buf)].map(b => b.toString(2).padStart(8, "0")).join("").slice(0, n);
const concat = (...parts) => new Uint8Array(parts.flatMap(p => [...p]));

async function hmacKey(password, nonce) {
  const pw = await subtle.importKey("raw", enc.encode(password.normalize("NFC")), "PBKDF2", false, ["deriveBits"]);
  const k = await subtle.deriveBits(
    { name: "PBKDF2", hash: "SHA-256", salt: concat(enc.encode("zolal-tm1"), pack(nonce)), iterations: ROUNDS }, pw, 256);
  return subtle.importKey("raw", k, { name: "HMAC", hash: "SHA-256" }, false, ["sign"]);
}
const mac = (key, ...parts) => subtle.sign("HMAC", key, concat(...parts));

// 61-bit string -> text, or null if the password is wrong or the bits aren't a sealed payload.
export async function openSealed(bits, password) {
  const nonce = bits.slice(0, NONCE), ct = bits.slice(NONCE, NONCE + 6 * CHARS), tag = bits.slice(NONCE + 6 * CHARS);
  const key = await hmacKey(password, nonce);
  // a 16-bit tag makes a timing-safe compare moot: there is no online oracle here, only this page
  if (bitsOf(await mac(key, enc.encode("tag"), pack(nonce + ct)), TAG) !== tag) return null;
  const ks = bitsOf(await mac(key, enc.encode("enc")), ct.length);
  let text = "";
  for (let i = 0; i < ct.length; i += 6) {
    let v = 0;
    for (let j = 0; j < 6; j++) v = 2 * v + (ct[i + j] !== ks[i + j] ? 1 : 0);
    text += ALPHABET[v];
  }
  return text.trimEnd();
}
