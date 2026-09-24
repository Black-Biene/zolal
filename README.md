# zolal-core

Hides encrypted files inside JPEG, MP4 and PDF carriers using *container* techniques: the payload lives in
structurally unused space (after a JPEG's EOI, in an MP4 `free` box, in an unreferenced PDF object), so the
carrier still opens normally in any viewer.

**Not forensic-grade steganography.** A file-size check reveals that something was added; the threat model is a
casual snooper, not an analyst. `plausibility()` exists to keep a UI honest about that.

Crypto: Argon2id (48 MiB, t=3) → XChaCha20-Poly1305 in STREAM/BE32 chunks of 64 KiB. Markerless: the envelope
carries no version or length in the clear, and the AEAD tag is the only validator.

| Crate | Role |
|---|---|
| `zolal-core` | The engine: crypto envelope, payload bundling, container carriers |
| `zolal-ffi` | C ABI (hand-written `include/zolal.h`) as a static library for Swift |
| `zolal-cli` | Developer CLI. Also what the browser demo runs |

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo deny check

cargo run -p zolal-cli -- hide   photo.jpg out.jpg secret.pdf --pass 'pw'
cargo run -p zolal-cli -- reveal out.jpg recovered/            --pass 'pw'
cargo run -p zolal-cli -- clean  out.jpg plain.jpg             --pass 'pw'
```

## Browser demo

`web/` runs the CLI as WebAssembly (`wasm32-wasip1`) in the page, with an in-memory filesystem. No server, no
upload. The Pages workflow builds the wasm and publishes `web/`. It is a demo front end, not the audited app.

Everything the page loads is its own: the WASI shim is vendored in `web/vendor/` (`@bjorn3/browser_wasi_shim`
0.4.1, MIT OR Apache-2.0), and a Content-Security-Policy stops it from loading or sending anything to another
origin. The shim and wasm load up front, so the page keeps working offline once loaded; `sw.js` caches it so it
also reopens offline after one visit.

```bash
cargo build --release -p zolal-cli --target wasm32-wasip1
cp target/wasm32-wasip1/release/zolal-cli.wasm web/ && python3 -m http.server -d web
```

## Design

Rationale lives in the module docs (`cargo doc --open`): start with `src/lib.rs`, then `src/crypto/mod.rs` and
`src/carrier/jpeg.rs`. Three things worth knowing first:

- **Streaming, always.** The API takes paths, not byte arrays; memory does not grow with file size. The largest
  allocation is Argon2id's 48 MiB working set.
- **Markerless.** Candidate regions come from container structure; only the AEAD tag confirms one is ours.
- **Precise errors, uniform UI.** The engine distinguishes "wrong passphrase" from "nothing hidden" so
  integrators can debug. Showing that difference to whoever holds the file reveals whether it holds anything —
  read `src/error.rs` before designing a reveal screen.

Licence: Apache-2.0, see `LICENSE` and `NOTICE`.
