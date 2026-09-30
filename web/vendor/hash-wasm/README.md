# hash-wasm (Argon2id)

Derives the key for the lab's password-protected TrustMark marks (`web/lab/tmseal.js`), with the same
Argon2id settings as the Rust engine. Loaded only by that page.

- File: `argon2.umd.min.js` from the `dist/` folder of the npm package
  [`hash-wasm` 4.12.0](https://www.npmjs.com/package/hash-wasm/v/4.12.0), unmodified. The WebAssembly is
  embedded in the file, so the page's CSP needs `'wasm-unsafe-eval'` (it has it).
- Source: [Daninet/hash-wasm](https://github.com/Daninet/hash-wasm).
- Licence: MIT, see `LICENSE`.
