# Zolal

Hide encrypted files and messages inside ordinary photos, videos, songs and PDFs. The result still opens and
plays normally.

**[Try it in your browser](https://black-biene.github.io/zolal/)**: it runs entirely on your device,
uploads nothing and works offline once loaded.

## How it works

Zolal uses *container* techniques: the encrypted data goes into space the file format already ignores, so
the picture, video or document itself is untouched.

| Carrier | Where the data goes |
|---|---|
| JPEG | After the end-of-image marker, or in APP15 segments |
| MP4 | In a `free` box |
| PDF | In an unreferenced object |
| MP3 | In a private frame of the ID3 tag (where the title and cover art live) |

- **Encryption:** Argon2id (48 MiB, t=3) derives the key; XChaCha20-Poly1305 encrypts in 64 KiB STREAM chunks.
- **Markerless:** nothing in the file says "Zolal" or gives a version or length. Only the passphrase can
  confirm something is there.
- **Other formats:** the web page converts HEIC, PNG and other photos to JPEG, and relabels MOV as MP4
  (the video isn't re-encoded). HEIC conversion needs Safari; other browsers can't open HEIC.
- **Streaming:** memory use doesn't grow with file size. The biggest allocation is Argon2id's 48 MiB.

**Send carriers as files.** WhatsApp, Telegram and most chat apps compress photos and videos when they're
sent as media, which deletes the hidden data. Send them as a document/file, by email, or in a .zip.

**What it's not:** forensic-grade steganography. The file gets bigger, so someone comparing sizes can tell
something was added. It protects against a casual look, not a trained analyst. The page tells you how
noticeable the size change is.

## Getting started

Needs Rust 1.88 or newer.

```bash
# hide a file in a photo, get it back, or remove it
cargo run -p zolal-cli -- hide   photo.jpg out.jpg secret.pdf --pass 'pw'
cargo run -p zolal-cli -- reveal out.jpg recovered/            --pass 'pw'
cargo run -p zolal-cli -- clean  out.jpg plain.jpg             --pass 'pw'
```

Set `ZOLAL_PASS` instead of `--pass` to keep the passphrase out of your shell history.

### Run the web version locally

```bash
cargo build --release -p zolal-cli --target wasm32-wasip1
cp target/wasm32-wasip1/release/zolal-cli.wasm web/
python3 -m http.server -d web
```

Every push to `main` publishes `web/` to GitHub Pages. The page only loads its own files: a
Content-Security-Policy blocks every other site, and a service worker lets it open offline.

## Reading the code

The engine is in `crates/zolal-core`. Start with its module docs (`cargo doc --open`): `src/lib.rs`, then
`src/crypto/mod.rs` and `src/carrier/jpeg.rs`.

The engine tells "wrong passphrase" apart from "nothing hidden", but the page never shows the difference: it
would tell anyone holding a file whether something is inside. Every failed reveal gets the same message and
takes the same time. `src/error.rs` explains why.

## Contributing

Issues and pull requests are welcome: bug fixes, new carrier formats, docs, UI. Read
[CONTRIBUTING.md](CONTRIBUTING.md) for how to run the checks and send a pull request. Report security
problems privately to hi@blackbiene.dev.

## Licence

Copyright 2026 [Black Biene](https://blackbiene.dev). Licensed under the [Apache License 2.0](LICENSE).

You're free to use Zolal in your own projects, including commercial and closed-source ones. In return,
you must **credit Black Biene** by keeping the [`NOTICE`](NOTICE) file with any copy or product you build
on it, and say which files you changed.
