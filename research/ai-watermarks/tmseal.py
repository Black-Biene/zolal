"""Password layer for a TrustMark payload: nonce (9) | ciphertext | tag (16) | zero padding.

The text is 6-bit characters from ALPHABET: a single mark's 61 bits hold 6, a four-quarter mark's 219 bits
hold 32. K = Argon2id(password, salt "zolal-tm2" + nonce), with the Rust engine's settings (48 MiB, 3 passes,
1 lane, 32 bytes); the keystream is HMAC(K, "enc") and the tag HMAC(K, "tag" + nonce|ciphertext), truncated.
The tag rejects a wrong password and random reads that happen to pass BCH (1 in 65536). "zolal-tm2" names
this format; tm1 (PBKDF2) marks were test marks only and no longer open. web/lab/tmseal.js is the browser
twin; `python tmseal.py` checks this file. Needs: pip install argon2-cffi
"""
import hmac
import secrets
import unicodedata

from argon2.low_level import Type, hash_secret_raw

ALPHABET = " abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789."
NONCE, TAG = 9, 16
# the engine's Argon2id settings (crates/zolal-core/src/crypto/kdf.rs)
ARGON2 = dict(time_cost=3, memory_cost=48 * 1024, parallelism=1, hash_len=32, type=Type.ID)


def capacity(bits):
    return (bits - NONCE - TAG) // 6


def _pack(bits):  # "0101…" -> bytes, MSB first, zero-padded
    bits += "0" * (-len(bits) % 8)
    return bytes(int(bits[i:i + 8], 2) for i in range(0, len(bits), 8))


def _bits(data, n):
    return "".join(f"{b:08b}" for b in data)[:n]


def _key(password, nonce):
    pw = unicodedata.normalize("NFC", password).encode()
    return hash_secret_raw(pw, b"zolal-tm2" + _pack(nonce), **ARGON2)


def _tag(k, nonce, ct):
    return _bits(hmac.new(k, b"tag" + _pack(nonce + ct), "sha256").digest(), TAG)


def seal(text, password, nonce=None, bits=61):
    chars = capacity(bits)
    if len(text) > chars or any(c not in ALPHABET for c in text):
        raise ValueError(f"text must be up to {chars} characters from: {ALPHABET!r}")
    nonce = nonce or f"{secrets.randbits(NONCE):0{NONCE}b}"
    k = _key(password, nonce)
    pt = "".join(f"{ALPHABET.index(c):06b}" for c in text.ljust(chars))
    ks = _bits(hmac.new(k, b"enc", "sha256").digest(), len(pt))
    ct = "".join(str(int(a) ^ int(b)) for a, b in zip(pt, ks))
    return (nonce + ct + _tag(k, nonce, ct)).ljust(bits, "0")


def open_(bits, password):
    """Bit string -> text, or None if the password is wrong or it isn't a sealed payload."""
    n = 6 * capacity(len(bits))
    nonce, ct, tag = bits[:NONCE], bits[NONCE:NONCE + n], bits[NONCE + n:NONCE + n + TAG]
    if "1" in bits[NONCE + n + TAG:]:
        return None
    k = _key(password, nonce)
    if not hmac.compare_digest(_tag(k, nonce, ct), tag):
        return None
    ks = _bits(hmac.new(k, b"enc", "sha256").digest(), len(ct))
    pt = "".join(str(int(a) ^ int(b)) for a, b in zip(ct, ks))
    return "".join(ALPHABET[int(pt[i:i + 6], 2)] for i in range(0, len(pt), 6)).rstrip()


if __name__ == "__main__":
    b = seal("Hi.42", "correct horse")
    assert len(b) == 61 and open_(b, "correct horse") == "Hi.42"
    assert open_(b, "wrong horse") is None
    assert seal("x", "pw", "000000001") == seal("x", "pw", "000000001") != seal("x", "pw", "000000010")
    long = "come to lingen at 6pm. bring tea"
    b = seal(long, "pw", bits=219)
    assert len(b) == 219 and capacity(219) == 32 and open_(b, "pw") == long
    print("ok")
