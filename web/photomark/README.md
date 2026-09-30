# Photo mark

A short text (up to 32 characters) hidden in a photo so that it survives chat apps, screenshots and camera
shots, locked with a passphrase. Used by the main page (Hide → Photo mark, Reveal → A photo mark) and by the
lab's test bench (`lab/tm.html`). Everything runs on the device.

- `photomark.js`: hiding and reading; loads the models once and keeps them in Cache Storage.
- `tmseal.js`: the passphrase layer (Argon2id + HMAC-SHA256), format `zolal-tm2`, frozen.
- `bch.js`: TrustMark's error correction (BCH_5 for one mark, BCH_3 per quarter of a four-quarter mark).
- `models/`: built by the Pages workflow with `research/ai-watermarks/onnx_export.py`, not in git.

How it was measured and why it works this way: `research/ai-watermarks/README.md`.

## Third-party parts

- The models are [Adobe TrustMark](https://github.com/adobe/trustmark) (variant Q): Adobe's own ONNX
  encoder and decoder, and its picture detector converted to ONNX. MIT licence, see `LICENSE-trustmark`.
- [ONNX Runtime Web](../vendor/onnxruntime-web/README.md) runs them (MIT).
- [hash-wasm](../vendor/hash-wasm/README.md) provides Argon2id (MIT).
