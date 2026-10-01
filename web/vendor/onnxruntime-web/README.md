# ONNX Runtime Web

Runs the photo mark's models (`web/photomark/photomark.js`). Loaded only when the photo mark is used.

- Files, from the `dist/` folder of the npm package
  [`onnxruntime-web` 1.30.0](https://www.npmjs.com/package/onnxruntime-web/v/1.30.0), unmodified:
  - CPU build: `ort.wasm.min.mjs`, `ort-wasm-simd-threaded.mjs`, `ort-wasm-simd-threaded.wasm` (14 MB). Always
    used for hiding, and for reading where WebGPU isn't available.
  - WebGPU build: `ort.webgpu.min.mjs`, `ort-wasm-simd-threaded.asyncify.mjs`,
    `ort-wasm-simd-threaded.asyncify.wasm` (27 MB). Used for reading on the graphics chip where the browser has
    WebGPU. Downloaded only on such devices.
- The `.wasm` files are kept in the photo mark's own on-device cache, next to the models.
- Source: [microsoft/onnxruntime](https://github.com/microsoft/onnxruntime).
- Licence: MIT, see `LICENSE`.

To update, replace all six files from one release (each `.mjs` and its `.wasm` must match), then bump
`MODEL_CACHE` in `photomark.js` so devices fetch the new `.wasm`.
