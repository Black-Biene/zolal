# Zolal — project handoff

Everything a new chat needs to continue this project. Last updated 2026-09-26.

## The project

**Zolal** hides encrypted files or text inside ordinary photos, videos, songs and PDFs. Owner: Black Biene
(hi@blackbiene.dev, https://blackbiene.dev).

- Repo: https://github.com/Black-Biene/zolal (renamed from `zolal-core`; old links redirect). Public.
- Live site: https://black-biene.github.io/zolal/ (every push to `main` rebuilds it via
  `.github/workflows/pages.yml`, about a minute).
- Camera test page ("lab"): https://black-biene.github.io/zolal/lab/ (not linked from the main page, noindex).
- Licence: Apache-2.0; `NOTICE` requires crediting Black Biene. `CONTRIBUTING.md` invites pull requests.
- There is no app any more: the iOS bridge (`zolal-ffi`, `build-xcframework.sh`) was deleted on request.
  Zolal is the Rust engine, the CLI and the web page.

## Working preferences (the owner asked for these)

- **Commits:** author and committer `Black Biene <hi@blackbiene.dev>`, and **no Claude/AI attribution** in
  commit messages or anywhere in the repo. Commit with
  `GIT_COMMITTER_NAME="Black Biene" GIT_COMMITTER_EMAIL="hi@blackbiene.dev" git commit --author="Black Biene <hi@blackbiene.dev>"`.
- **Pushing:** push finished work to `main` (the owner wants it live). Work in progress can go to a branch
  first; the session branch so far was `claude/optimistic-einstein-6noiuv`.
- The owner writes in English (sometimes asks for explanations in Persian), tests on an iPhone and a MacBook,
  and prefers plain, short explanations and a clear recommendation over long option lists.
- The page must stay private: everything runs on the device, nothing is uploaded, and it only loads its own
  files (Content-Security-Policy). Don't add third-party scripts or CDNs; vendor dependencies into `web/vendor/`.

## Layout

| Path | What |
|---|---|
| `crates/zolal-core` | Rust engine: Argon2id + XChaCha20-Poly1305 STREAM envelope, markerless; carriers JPEG (trailer / APP15), MP4 (`free` box), PDF (unreferenced object), **MP3 (PRIV frame in the ID3v2 tag)** |
| `crates/zolal-cli` | CLI (`hide`, `reveal`, `clean`, `probe`); compiled to `wasm32-wasip1` for the web page |
| `web/` | The site: `index.html`, `app.js`, `style.css`, `sw.js` (offline), `manifest.webmanifest`, icons |
| `web/vendor/` | `browser_wasi_shim` 0.4.1 (MIT) and `libheif` WASM from libheif-js 1.23.2 (LGPL-3.0, for HEIC) |
| `web/lab/` | Camera-readable hidden text experiment (`wm.js` engine, `lab.js` UI, `worker.js`); `tm.html`/`tm.js`/`bch.js`: TrustMark reader |
| `research/camera-watermark/` | Python prototype + simulated channels + results for our own method |
| `research/ai-watermarks/` | `tm_test.py`: test kit for Adobe TrustMark, run on the owner's laptop |

Checks: `cargo fmt --all`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`,
`cargo deny check` (not installed in the cloud container). Web build: see README.

## What the main site does (all done and live)

- Light, app-like design with the Zolal app icon and its blue; fits one phone screen; installable to the
  home screen; works offline after one visit.
- **Hide:** 1) choose a cover file (step 2 stays locked until then), 2) **Files or Text** switch, 3)
  passphrase. Warning to send results **as a file/document**, not as a photo/video (chat apps recompress).
- Cover conversion: HEIC/PNG/WebP/GIF/AVIF → JPEG (HEIC via Safari or bundled libheif); MOV relabelled as MP4
  (not re-encoded); MP3 supported natively.
- **Reveal:** shows text inline, files as downloads; every failure looks identical and takes ≥ 2.5 s (so it
  doesn't reveal whether a file holds anything).
- Security: strict CSP (self only), service worker cache is versioned per deploy (`?v=dev` replaced with the
  commit hash in the Pages workflow).

## The camera-readable text experiment (in progress)

Goal: hide a short, password-protected text in a normal-looking picture so a phone camera pointed at it (on a
screen, printed, or received through Telegram as a normal photo) can read it back.

### Our own method (`web/lab/`, `research/camera-watermark/`)

Spread-spectrum pattern in the blue-yellow chroma (Cb) channel, ~10,400 cells, 1024 raw bits, rate-1/3
convolutional code + soft Viterbi → 41 bytes (38 bytes of text + length + CRC-16). Grid shape follows the
picture's aspect ratio (13 standard ratios). Invisible 16×16-cell finder blocks in each corner. Reader: crop
box → finder search → edge snapping fallback → coordinate-descent alignment → decode.

Results:
- **Simulation:** Telegram-like compression 100%; camera at screen ~90–100%; with the owner's exact laptop
  setup 9/10 near, 7/10 far.
- **Reading from a file works on the owner's iPhone.**
- **Real phone camera: never succeeded** (best: signal 84% = pattern partly seen, too weak).
- **Visible on plain/white pictures** (blue-yellow mottling on a white diagram, owner found it "very obvious").
  Two recent changes made that worse: the clipping fix near white (now carries the full pattern onto white)
  and finder blocks at 1.4× strength. On busy photos it is invisible.

Known lesson: finding the picture in a camera shot is the hard part; with perfect corners 18/20 simulated laptop
shots decode.

### Adobe TrustMark (`research/ai-watermarks/`) — the promising path

AI watermark, MIT licence, 100 bits (61 usable with BCH_5 → 8 ASCII characters). The owner ran `tm_test.py`
on a MacBook:
- PSNR 41.2 dB; survived PNG, JPEG 90, 1280 px q80, 800 px q70, 512 px q50.
- **An iPhone photo of the marked picture on the laptop screen decoded `zolal123`** — but only with TrustMark's
  picture-detector model (`loadBBoxDetector=True`, `decode(..., DETECTFIRST=True)`); plain crops failed.

This is the **first real-camera success**.

## Next steps

1. **Owner runs more TrustMark tests** (`python tm_test.py read ...`): distance, ~30° angle, sent through
   Telegram as a normal photo, the white diagram (+ check `out/*-compare.png` for visibility), print.
2. ~~Model sizes~~ done: `research/ai-watermarks/onnx_export.py` builds ONNX reader models; smallest working
   pair is int8 detector (42 MB) + Adobe's fp16 decoder (47 MB) = 89 MB. See that folder's README.
   **Browser reader works on the iPhone:** `web/lab/tm.html` (models in git-ignored `web/lab/models/`, serve
   locally with `.claude/launch.json`) read a fresh camera shot in ~4.5 s, and a camera shot sent through
   Telegram. Live at https://black-biene.github.io/zolal/lab/tm.html (linked from the lab page): the Pages
   workflow builds the models with `onnx_export.py` and caches them. Scan close up, like a QR code; distance is
   not a goal. **Password layer done** (`tmseal.py` / `web/lab/tmseal.js`): 6 characters, PBKDF2 key, 16-bit
   tag; mark on the page's *Hide* tab or with `tm_test.py mark PHOTO TEXT PASSWORD`. Details and limits in the
   research README. **Up to 32 characters** with four quarter marks and a spare (research README). Next idea: WebGPU for speed (ORT's webgpu build, ~28 MB), with the CPU path as fallback.
3. If TrustMark holds up: run it in the browser with ONNX Runtime Web (Adobe ships ONNX models for its JS/Rust
   ports), vendor the models into `web/` (MIT allows it), and design a compact password layer (61 bits leaves
   ~5–6 characters after a check; e.g. Argon2id-derived keystream + short check value).
4. Meanwhile, for our own lab method: restore invisibility (lower strength on plain areas, finder gain back
   to 1.0) and state that it suits busy photos only.
5. Earlier backlog: a CI workflow running fmt/clippy/tests/deny on pull requests; a file-size limit on phones
   (the page loads whole files into memory); optionally a "remove hidden content" button (engine has `clean`).

## Cloud environment notes

- Blocked hosts: Adobe model host (`cai-watermark.adobe.net`), Hugging Face, `dl.fbaipublicfiles.com`,
  `people.eecs.berkeley.edu`, GitHub API. PyPI, npm, crates.io and `raw.githubusercontent.com` work. The owner
  can allow hosts in the environment's Network access settings.
- The container can restart; anything outside the repo (the scratchpad, Python venvs, test frames) is lost.
  Rebuild a venv with `research/camera-watermark/requirements.txt`.
