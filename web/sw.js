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

self.addEventListener("fetch", e => {
  const url = new URL(e.request.url);
  // The photo mark's ~150 MB of models keep their own versioned cache (photomark/photomark.js); copying them
  // into this per-deploy cache would store them twice and fetch them again on every deploy.
  if (e.request.method !== "GET" || url.origin !== location.origin || url.pathname.includes("/photomark/models/")) return;
  e.respondWith(fetch(e.request).then(res => {
    if (res.ok) { const copy = res.clone(); caches.open(CACHE).then(c => c.put(e.request, copy)); }
    return res;
  }).catch(() => caches.match(e.request, { ignoreSearch: true })));
});
