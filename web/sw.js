// Offline support. Network first so a new deploy is picked up whenever there is a connection; the cache is the
// fallback when there isn't. Only this site's own files are ever requested, so only those are cached.
const CACHE = "zolal-?v=dev";
const SHIM = "vendor/browser_wasi_shim/";
const FILES = [
  "./", "index.html", "manifest.webmanifest", "style.css?v=dev", "app.js?v=dev", "icon-32.png?v=dev", "icon-64.png?v=dev", "icon-180.png?v=dev", "zolal-cli.wasm?v=dev",
  "vendor/libheif/libheif.js?v=dev", "vendor/libheif/libheif.wasm?v=dev",
  ...["index", "wasi", "wasi_defs", "fd", "fs_mem", "fs_opfs", "strace", "debug"].map(f => SHIM + f + ".js"),
];

self.addEventListener("install", e => {
  e.waitUntil(caches.open(CACHE).then(c => c.addAll(FILES)).then(() => self.skipWaiting()));
});
// Only this worker's own per-deploy caches are cleared: the photo mark's models live in a cache of their own.
self.addEventListener("activate", e => e.waitUntil(caches.keys()
  .then(keys => Promise.all(keys.filter(k => k.startsWith("zolal-?v=") && k !== CACHE).map(k => caches.delete(k))))
  .then(() => self.clients.claim())));

// Cross-origin isolation, which browsers require before a page may use several CPU cores (SharedArrayBuffer):
// GitHub Pages can't send the two headers, so this worker adds them to the pages listed here. Same-origin
// files need nothing more. Scripts get the headers too, so the runtime's worker threads are isolated as well.
// Only the lab's test bench for now; the main page follows once phones have been tested.
const ISOLATED_PAGES = /\/lab\/tm\.html$/;
function isolate(res) {
  if (!res || res.type === "opaque" || res.type === "opaqueredirect") return res;
  const headers = new Headers(res.headers);
  headers.set("Cross-Origin-Opener-Policy", "same-origin");
  headers.set("Cross-Origin-Embedder-Policy", "require-corp");
  return new Response(res.body, { status: res.status, statusText: res.statusText, headers });
}

self.addEventListener("fetch", e => {
  const url = new URL(e.request.url);
  // The photo mark's ~150 MB of models and its runtime's WebAssembly keep their own versioned cache
  // (photomark/photomark.js); copying them into this per-deploy cache would store them twice and fetch them
  // again on every deploy.
  const ownCache = url.pathname.includes("/photomark/models/")
    || /\/vendor\/onnxruntime-web\/.*\.wasm$/.test(url.pathname);
  if (e.request.method !== "GET" || url.origin !== location.origin || ownCache) return;
  const page = e.request.mode === "navigate";
  const wrap = page ? (ISOLATED_PAGES.test(url.pathname) ? isolate : r => r)
    : /\.m?js$/.test(url.pathname) ? isolate : r => r;
  e.respondWith(fetch(e.request).then(res => {
    if (res.ok) { const copy = res.clone(); caches.open(CACHE).then(c => c.put(e.request, copy)); }
    return wrap(res);
  }).catch(() => caches.match(e.request, { ignoreSearch: true }).then(wrap)));
});
