# libheif (WebAssembly)

Decodes HEIC photos in browsers that can't (Chrome, Firefox), so they can be converted to JPEG. Loaded only
when needed.

- Files: `libheif.js` and `libheif.wasm` from the `libheif-wasm/` folder of the npm package
  [`libheif-js` 1.23.2](https://www.npmjs.com/package/libheif-js/v/1.23.2), unmodified.
- Source: [catdad-experiments/libheif-js](https://github.com/catdad-experiments/libheif-js) (build scripts),
  [strukturag/libheif](https://github.com/strukturag/libheif) and
  [strukturag/libde265](https://github.com/strukturag/libde265) (the libraries).
- Licence: GNU LGPL v3, see `LICENSE` (libheif) and `LICENSE-libheif-js` (the package).

To use a different build, replace both files with a compatible `libheif-js` release; `app.js` only relies on
`libheif(options)`, `HeifDecoder`, `is_primary`, `get_width`, `get_height` and `display`.
