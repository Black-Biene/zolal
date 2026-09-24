// Offline support. Network first so a new deploy is picked up whenever there is a connection; the cache is the
// fallback when there isn't. Only this site's own files are ever requested, so only those are cached.
const CACHE = "zolal";
const SHIM = "vendor/browser_wasi_shim/";
const FILES = [
  "./", "index.html", "style.css", "app.js", "icon.svg", "zolal-cli.wasm",
  ...["index", "wasi", "wasi_defs", "fd", "fs_mem", "fs_opfs", "strace", "debug"].map(f => SHIM + f + ".js"),
];

self.addEventListener("install", e => {
  e.waitUntil(caches.open(CACHE).then(c => c.addAll(FILES)).then(() => self.skipWaiting()));
});
self.addEventListener("activate", e => e.waitUntil(caches.keys()
  .then(keys => Promise.all(keys.filter(k => k !== CACHE).map(k => caches.delete(k))))
  .then(() => self.clients.claim())));

self.addEventListener("fetch", e => {
  if (e.request.method !== "GET" || new URL(e.request.url).origin !== location.origin) return;
  e.respondWith(fetch(e.request).then(res => {
    if (res.ok) { const copy = res.clone(); caches.open(CACHE).then(c => c.put(e.request, copy)); }
    return res;
  }).catch(() => caches.match(e.request, { ignoreSearch: true })));
});
