// Password layer for a TrustMark payload, browser twin of research/ai-watermarks/tmseal.py (see there for the
// format): nonce (9) | ciphertext (6-bit characters) | tag (16) | zero padding. A single mark's 61 bits hold
// 6 characters; a four-quarter mark's 219 bits hold 32. Key: Argon2id with the Rust engine's settings, via the
// vendored hash-wasm (WebCrypto has no Argon2); HMAC-SHA256 from WebCrypto.
import "../vendor/hash-wasm/argon2.umd.min.js";  // defines globalThis.hashwasm

export const ALPHABET = " abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789.";
const NONCE = 9, TAG = 16;
const enc = new TextEncoder(), subtle = crypto.subtle;

// characters that fit in a payload of `bits` bits
export const capacity = bits => Math.floor((bits - NONCE - TAG) / 6);

const pack = bits => {  // "0101…" -> bytes, MSB first, zero-padded
  const out = new Uint8Array(Math.ceil(bits.length / 8));
  for (let i = 0; i < bits.length; i++) if (bits[i] === "1") out[i >> 3] |= 128 >> (i & 7);
  return out;
};
const bitsOf = (buf, n) => [...new Uint8Array(buf)].map(b => b.toString(2).padStart(8, "0")).join("").slice(0, n);
const concat = (...parts) => new Uint8Array(parts.flatMap(p => [...p]));

// "zolal-tm2" names this format (tm1 used PBKDF2 and was only ever test marks). The settings are the engine's
// (crates/zolal-core/src/crypto/kdf.rs): 48 MiB, 3 passes, 1 lane, 32 bytes.
// A reveal can try several payloads with the same nonce, and Argon2id is the slow step, so keys are kept per
// password and nonce for the life of the page.
const keys = new Map();
function hmacKey(password, nonce) {
  const id = password + "\u0000" + nonce;
  if (!keys.has(id)) {
    keys.set(id, globalThis.hashwasm.argon2id({
      password: enc.encode(password.normalize("NFC")), salt: concat(enc.encode("zolal-tm2"), pack(nonce)),
      parallelism: 1, iterations: 3, memorySize: 48 * 1024, hashLength: 32, outputType: "binary",
    }).then(k => subtle.importKey("raw", k, { name: "HMAC", hash: "SHA-256" }, false, ["sign"])));
  }
  return keys.get(id);
}
const mac = (key, ...parts) => subtle.sign("HMAC", key, concat(...parts));

const keystream = async (key, n) => bitsOf(await mac(key, enc.encode("enc")), n);  // n ≤ 256

// text -> `bits`-bit string with a fresh random nonce.
export async function seal(text, password, bits = 61) {
  const chars = capacity(bits);
  if (text.length > chars || [...text].some(c => !ALPHABET.includes(c))) {
    throw new Error(`Use up to ${chars} characters: letters, digits, space or "."`);
  }
  const nonce = [...crypto.getRandomValues(new Uint8Array(2))].map(b => b.toString(2).padStart(8, "0")).join("").slice(0, NONCE);
  const key = await hmacKey(password, nonce);
  const pt = [...text.padEnd(chars)].map(c => ALPHABET.indexOf(c).toString(2).padStart(6, "0")).join("");
  const ks = await keystream(key, pt.length);
  const ct = [...pt].map((b, i) => (b === ks[i] ? "0" : "1")).join("");
  return (nonce + ct + bitsOf(await mac(key, enc.encode("tag"), pack(nonce + ct)), TAG)).padEnd(bits, "0");
}

// bit string -> text, or null if the password is wrong or the bits aren't a sealed payload.
export async function openSealed(bits, password) {
  const n = 6 * capacity(bits.length);
  const nonce = bits.slice(0, NONCE), ct = bits.slice(NONCE, NONCE + n), tag = bits.slice(NONCE + n, NONCE + n + TAG);
  if (bits.slice(NONCE + n + TAG).includes("1")) return null;  // padding is written as zeros
  const key = await hmacKey(password, nonce);
  // a 16-bit tag makes a timing-safe compare moot: there is no online oracle here, only this page
  if (bitsOf(await mac(key, enc.encode("tag"), pack(nonce + ct)), TAG) !== tag) return null;
  const ks = await keystream(key, ct.length);
  let text = "";
  for (let i = 0; i < ct.length; i += 6) {
    let v = 0;
    for (let j = 0; j < 6; j++) v = 2 * v + (ct[i + j] !== ks[i + j] ? 1 : 0);
    text += ALPHABET[v];
  }
  return text.trimEnd();
}
