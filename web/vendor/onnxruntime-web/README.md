# ONNX Runtime Web (WebAssembly)

Runs the TrustMark reader models on the lab's `tm.html` test page. Loaded only by that page.

- Files: `ort.wasm.min.mjs`, `ort-wasm-simd-threaded.mjs` and `ort-wasm-simd-threaded.wasm` from the `dist/`
  folder of the npm package [`onnxruntime-web` 1.30.0](https://www.npmjs.com/package/onnxruntime-web/v/1.30.0),
  unmodified. CPU (WebAssembly) only; the WebGPU builds are not included.
- Source: [microsoft/onnxruntime](https://github.com/microsoft/onnxruntime).
- Licence: MIT, see `LICENSE`.

To update, replace all three files from one release; the `.mjs` and `.wasm` must match.
