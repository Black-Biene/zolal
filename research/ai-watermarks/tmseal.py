"""Password layer for a TrustMark payload: 61 bits = nonce (9) | ciphertext (36) | tag (16).

The text is up to 6 characters from ALPHABET (6 bits each). K = PBKDF2-SHA256(password, "zolal-tm1" + nonce,
600k rounds); the keystream is HMAC(K, "enc") and the tag HMAC(K, "tag" + nonce|ciphertext), truncated. The tag
rejects a wrong password and random reads that happen to pass BCH (1 in 65536). web/lab/tmseal.js is the
browser twin; `python tmseal.py` checks this file against itself.
"""
import hashlib
import hmac
import secrets
import unicodedata

ALPHABET = " abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789."
CHARS, NONCE, TAG = 6, 9, 16
ROUNDS = 600_000


def _pack(bits):  # "0101…" -> bytes, MSB first, zero-padded
    bits += "0" * (-len(bits) % 8)
    return bytes(int(bits[i:i + 8], 2) for i in range(0, len(bits), 8))


def _bits(data, n):
    return "".join(f"{b:08b}" for b in data)[:n]


def _key(password, nonce):
    pw = unicodedata.normalize("NFC", password).encode()
    return hashlib.pbkdf2_hmac("sha256", pw, b"zolal-tm1" + _pack(nonce), ROUNDS)


def _tag(k, nonce, ct):
    return _bits(hmac.new(k, b"tag" + _pack(nonce + ct), "sha256").digest(), TAG)


def seal(text, password, nonce=None):
    if len(text) > CHARS or any(c not in ALPHABET for c in text):
        raise ValueError(f"text must be up to {CHARS} characters from: {ALPHABET!r}")
    nonce = nonce or f"{secrets.randbits(NONCE):0{NONCE}b}"
    k = _key(password, nonce)
    pt = "".join(f"{ALPHABET.index(c):06b}" for c in text.ljust(CHARS))
    ks = _bits(hmac.new(k, b"enc", "sha256").digest(), len(pt))
    ct = "".join(str(int(a) ^ int(b)) for a, b in zip(pt, ks))
    return nonce + ct + _tag(k, nonce, ct)


def open_(bits, password):
    """61-bit string -> text, or None if the password is wrong or it isn't a sealed payload."""
    nonce, ct, tag = bits[:NONCE], bits[NONCE:NONCE + 6 * CHARS], bits[NONCE + 6 * CHARS:]
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
    print("ok", b)
