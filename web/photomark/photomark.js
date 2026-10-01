// Photo mark: a short text hidden in a photo so that it survives screenshots, camera shots and chat apps.
//
// Hide: seal the text with the password (tmseal.js), BCH-encode it (bch.js), run Adobe's TrustMark encoder on
// the photo and blend the change in as TrustMark's Python encode() does. Reveal: the picture detector finds
// the marked picture in a camera shot, the decoder reads 100 bits, BCH corrects them, the seal opens. All
// three ONNX models run in onnxruntime-web (on the graphics chip where possible) and are kept on the device.
// research/ai-watermarks/ has how they are built (onnx_export.py) and every measurement behind the numbers here.
//
// No DOM beyond canvases: pages pass callbacks for progress and status. Used by the main page and the lab.
import { correct, decodePayload, encode } from "./bch.js?v=dev";
import { ALPHABET, capacity, openSealed, seal } from "./tmseal.js?v=dev";

export { ALPHABET };

const tick = () => new Promise(r => setTimeout(r, 30));  // let the page paint before heavy work

// ---- models and runtime ----------------------------------------------------------------------------------

// Models are kept on the device in Cache Storage, so they download once, not per visit or per deploy, and so
// is the runtime's WebAssembly. Bump the version when a model file changes (onnx_export.py or Adobe's pinned
// files) or when the vendored onnxruntime-web is updated: the old copies are then deleted and new ones fetched.
const MODEL_CACHE = "zolal-tm-models-2";
export const DETECTORS = { fast: "detector_Q_fp16w.onnx", small: "detector_Q_int8.onnx" };
const ENCODER = "encoder_Q.onnx", DECODER = "decoder_Q.onnx";
const url = name => new URL("models/" + name, import.meta.url).href;
const VENDOR = new URL("../vendor/onnxruntime-web/", import.meta.url).href;

// Where the models run. Reading (detector and decoder) uses the graphics chip (WebGPU) where the browser offers
// it: on a MacBook it found the picture 2.2× faster (1.3 s instead of 2.8 s) and read bits 8.7× faster (19 ms
// instead of 165 ms), with identical boxes and bits. Hiding (the encoder) always uses the CPU build: on WebGPU
// its output differed by up to 0.15 from the CPU's (its whole change is at most ~0.56) and the mark didn't read
// back, and the WebGPU build's own CPU path ran it ~6× slower. Elsewhere, or if WebGPU fails, everything uses
// the CPU build, one thread (threads need cross-origin isolation, which GitHub Pages can't turn on).
const RUNTIMES = {
  cpu: { module: "ort.wasm.min.mjs", wasm: "ort-wasm-simd-threaded.wasm", ep: "wasm" },  // 14 MB
  webgpu: { module: "ort.webgpu.min.mjs", wasm: "ort-wasm-simd-threaded.asyncify.wasm", ep: "webgpu" },  // 27 MB
};
const rts = {};  // loaded onnxruntime-web modules, by runtime name
let reader = "cpu";  // the runtime the detector and decoder use
const runtimeOf = name => (name === ENCODER ? "cpu" : reader);
const sessions = {};
let modelCache;  // undefined until opened; null where Cache Storage is unavailable (some private windows)

async function openCache() {
  if (modelCache !== undefined) return modelCache;
  try {
    modelCache = await caches.open(MODEL_CACHE);
    for (const key of await caches.keys()) {
      if (key.startsWith("zolal-tm-models-") && key !== MODEL_CACHE) await caches.delete(key);
    }
    navigator.storage?.persist?.().catch(() => {});  // ask the browser not to evict ~150 MB of models
  } catch { modelCache = null; }
  return modelCache;
}

// A file's bytes: from the device's cache, else downloaded (reporting each chunk) and cached.
async function fileBytes(href, onChunk = () => {}) {
  const cache = await openCache(), hit = cache && await cache.match(href);
  if (hit) return hit.arrayBuffer();
  const res = await fetch(href);
  if (!res.ok) throw new Error(`${href.split("/").pop()} is missing (HTTP ${res.status})`);
  const parts = [];
  for (const reader = res.body.getReader(); ;) {
    const { done, value } = await reader.read();
    if (done) break;
    parts.push(value); onChunk(value.length);
  }
  const blob = new Blob(parts);
  // a full disk shouldn't stop this visit; the file just downloads again next time
  if (cache) await cache.put(href, new Response(blob)).catch(() => {});
  return blob.arrayBuffer();
}

async function wantsWebGPU(prefer) {
  if (prefer === "cpu" || !navigator.gpu) return false;
  try { return !!await navigator.gpu.requestAdapter(); } catch { return false; }
}

async function runtime(name) {
  if (!rts[name]) {
    const r = RUNTIMES[name], mod = await import(VENDOR + r.module);
    mod.env.wasm.wasmPaths = VENDOR;
    mod.env.wasm.numThreads = 1;
    mod.env.wasm.wasmBinary = await fileBytes(VENDOR + r.wasm);  // from the cache, not fetched by the runtime
    rts[name] = mod;
  }
  return rts[name];
}

async function createSession(name) {
  const rt = runtimeOf(name);
  return (await runtime(rt)).InferenceSession.create(await fileBytes(url(name)), { executionProviders: [RUNTIMES[rt].ep] });
}

function model(name) {
  sessions[name] ??= loadModels().then(() => createSession(name)).catch(e => { delete sessions[name]; throw e; });
  return sessions[name];
}

let loading = null;
// Download what the device doesn't have yet, then prepare every model, so hiding and revealing start at once.
// onProgress({ phase: "download", done, total }) in bytes, then ({ phase: "prepare", step, steps }).
// Resolves to { kept: whether the files stay on the device, engine: what reading runs on, "webgpu" or
// "cpu" }. `engine: "cpu"` keeps reading on the CPU too (for comparison in the lab).
export function loadModels({ detector = DETECTORS.fast, onProgress = () => {}, engine: prefer = "auto" } = {}) {
  loading ??= (async () => {
    const cache = await openCache();
    const names = [ENCODER, DECODER, detector];
    const gpu = await wantsWebGPU(prefer);
    const files = [...names.map(url), ...["cpu", ...(gpu ? ["webgpu"] : [])].map(r => VENDOR + RUNTIMES[r].wasm)];
    const missing = [];
    for (const h of files) if (!(cache && await cache.match(h))) missing.push(h);
    if (missing.length) {
      // sizes first, so progress covers the whole download
      const sizes = await Promise.all(missing.map(h => fetch(h, { method: "HEAD" })
        .then(r => +r.headers.get("content-length") || 0)));
      const total = sizes.reduce((a, b) => a + b, 0);
      let done = 0;
      onProgress({ phase: "download", done, total });
      for (const h of missing) await fileBytes(h, k => { done += k; onProgress({ phase: "download", done, total }); });
    }
    const prepare = async () => {
      for (const [i, n] of names.entries()) {
        onProgress({ phase: "prepare", step: i + 1, steps: names.length }); await tick();
        sessions[n] = createSession(n);
        await sessions[n];
      }
    };
    reader = gpu ? "webgpu" : "cpu";
    try { await prepare(); } catch (e) {
      if (reader === "cpu") throw e;
      console.warn("photo mark: WebGPU failed, reading on the CPU", e);
      reader = "cpu";
      for (const k of Object.keys(sessions)) delete sessions[k];
      await prepare();
    }
    return { kept: cache !== null, engine: reader };
  })().catch(e => { loading = null; throw e; });
  return loading;
}

// ---- pixels --------------------------------------------------------------------------------------------

// pixels of a canvas as a CHW float tensor, values mapped by f, for a session of runtime `rt`
function tensor(canvas, dims, f, rt = reader) {
  const { width: w, height: h } = canvas, px = canvas.getContext("2d").getImageData(0, 0, w, h).data;
  const out = new Float32Array(3 * w * h);
  for (let i = 0; i < w * h; i++) for (let c = 0; c < 3; c++) out[c * w * h + i] = f(px[4 * i + c] / 255);
  return new rts[rt].Tensor("float32", out, dims);
}

function draw(src, w, h, sx = 0, sy = 0, sw = src.width, sh = src.height, target) {
  const c = target || Object.assign(document.createElement("canvas"), { width: w, height: h });
  const g = c.getContext("2d", { willReadFrequently: true });
  g.imageSmoothingQuality = "high";
  g.drawImage(src, sx, sy, sw, sh, 0, 0, w, h);
  return c;
}

function rotated(bmp, quarter) {
  if (!quarter) return bmp;
  const c = document.createElement("canvas"), odd = quarter % 2;
  c.width = odd ? bmp.height : bmp.width; c.height = odd ? bmp.width : bmp.height;
  const g = c.getContext("2d");
  g.translate(c.width / 2, c.height / 2); g.rotate(quarter * Math.PI / 2);
  g.drawImage(bmp, -bmp.width / 2, -bmp.height / 2);
  return c;
}

const bitmap = source => source instanceof Blob ? createImageBitmap(source) : source;

// ---- format --------------------------------------------------------------------------------------------

// Four-quarter marks, for texts longer than a single mark holds: the photo is split 2×2 (0 top-left, 1 top-right,
// 2 bottom-left, 3 bottom-right) and each quarter gets its own mark with BCH_3 (fixes 3 wrong bits; real
// camera reads had 0–1 on good crops), i.e. 75 bits = its index (2) + 73 bits. Quarters 0–2 carry the sealed
// 219-bit payload (32 characters); quarter 3 carries their XOR, so any one quarter may be unreadable.
const CHUNK = 73, QUAD_BITS = 3 * CHUNK;
export const SINGLE = capacity(61), QUAD = capacity(QUAD_BITS);
const xor = (...parts) => [...parts[0]].map((_, i) => parts.reduce((v, p) => v ^ +p[i], 0)).join("");
const quartersOf = ([x1, y1, x2, y2]) => {
  const mx = (x1 + x2) / 2, my = (y1 + y2) / 2;
  return [[x1, y1, mx, my], [mx, y1, x2, my], [x1, my, mx, y2], [mx, my, x2, y2]];
};

// What a text needs: its length, characters that can't be hidden, and whether it fits one mark or four.
export function measure(text) {
  const bad = [...new Set([...text].filter(c => !ALPHABET.includes(c)))];
  return { length: text.length, bad, quad: text.length > SINGLE, fits: !bad.length && text.length <= QUAD };
}

// ---- hide ----------------------------------------------------------------------------------------------

// iOS Safari refuses canvases above ~16.7 million pixels, so bigger photos (48 MP) are scaled down first.
const MAX_PIXELS = 16_000_000;

// Mark one region [L, T, RW, RH] of the photo with 100 bits, as TrustMark's Python encode() does: Adobe's
// encoder at 256×256, then the change it made is scaled up and added to the full-resolution pixels `px`.
// The cover is read from the canvas `full`, which still holds the original: px is written back at the end.
async function markRegion(enc, full, px, W, [L, T, RW, RH], bits, strength) {
  const cover = tensor(draw(full, 256, 256, L, T, RW, RH), [1, 3, 256, 256], v => v * 2 - 1, "cpu");
  const out = await enc.run({
    [enc.inputNames[0]]: cover,
    [enc.inputNames[1]]: new rts.cpu.Tensor("float32", Float32Array.from(bits, Number), [1, 100]),
  });

  // residual = what the encoder changed, minus any colour shift (per-channel mean), as in TrustMark
  const st = out[enc.outputNames[0]].data, cv = cover.data, N = 256 * 256, res = new Float32Array(3 * N);
  for (let c = 0; c < 3; c++) {
    let sum = 0;
    for (let i = c * N; i < (c + 1) * N; i++) sum += res[i] = Math.max(-1, Math.min(1, st[i])) - cv[i];
    for (let i = c * N; i < (c + 1) * N; i++) res[i] -= sum / N;
  }

  // Scale the residual up to the region (bilinear, like torch interpolate with align_corners=False), add it,
  // and fade it in over the outer 1% (max 50 px) of the region. TrustMark overwrites corners with the
  // column blend; the smaller of the row and column fade is used here, a difference too small to matter.
  const f = Math.min(50, Math.max(1, Math.floor(Math.min(RW, RH) * 0.01)));
  const axis = (n, i) => {
    const p = Math.max(0, (i + 0.5) * 256 / n - 0.5), i0 = Math.min(255, Math.floor(p));
    const d = Math.min(i, n - 1 - i);
    return [i0, Math.min(255, i0 + 1), p - i0, d < f ? (d + 1) / f : 1];
  };
  const xs = Array.from({ length: RW }, (_, x) => axis(RW, x));
  for (let y = 0; y < RH; y++) {
    const [y0, y1, wy, ay] = axis(RH, y);
    for (let x = 0; x < RW; x++) {
      const [x0, x1, wx, ax] = xs[x], a = Math.min(ay, ax), i = ((T + y) * W + L + x) * 4;
      for (let c = 0; c < 3; c++) {
        const o = c * N, r = (1 - wy) * ((1 - wx) * res[o + y0 * 256 + x0] + wx * res[o + y0 * 256 + x1])
          + wy * ((1 - wx) * res[o + y1 * 256 + x0] + wx * res[o + y1 * 256 + x1]);
        const orig = px[i + c], wm = Math.floor((Math.max(-1, Math.min(1, strength * r + orig / 127.5 - 1)) + 1) * 127.5);
        px[i + c] = a === 1 ? wm : Math.floor(a * wm + (1 - a) * orig);
      }
    }
  }
}

// Hide `text` in `source` (a File/Blob, or an ImageBitmap/canvas the page decoded itself, e.g. from HEIC).
// Returns the marked canvas and whether the text reads straight back from it.
// `strength` scales the mark, as TrustMark's WM_STRENGTH does. 1.0 survives screens, screenshots and chat apps;
// printing needs more. Adobe's FAQ says 1.5; on the owner's office printer a 1.0 print came back with ~20 of 96
// bits wrong and a 1.5 print with ~13 (5 are fixable), while a 2.0 print read correctly (2.5 didn't, once).
// Cost: ~6 dB less PSNR (40 -> 34 dB on the owner's photos), noticeable up close in calm areas.
export const PRINT_STRENGTH = 2;
export async function hideText(source, text, password, { onStatus = () => {}, strength = 1 } = {}) {
  onStatus("Opening the photo…"); await tick();
  const bmp = await bitmap(source);
  const s = Math.min(1, Math.sqrt(MAX_PIXELS / (bmp.width * bmp.height)));
  const W = Math.round(bmp.width * s), H = Math.round(bmp.height * s);
  // on white: transparent areas (a PNG floor plan, say) would otherwise turn black in the JPEG
  const full = Object.assign(document.createElement("canvas"), { width: W, height: H });
  const g = full.getContext("2d", { willReadFrequently: true });
  g.fillStyle = "#fff"; g.fillRect(0, 0, W, H);
  g.imageSmoothingQuality = "high"; g.drawImage(bmp, 0, 0, W, H);
  const img = g.getImageData(0, 0, W, H), px = img.data;

  // TrustMark marks the whole photo, or only a centred square when it's wider than 2:1
  const side = Math.min(W, H), square = Math.max(W, H) / side > 2;
  const [L, T, RW, RH] = square ? [(W - side) >> 1, (H - side) >> 1, side, side] : [0, 0, W, H];
  const quad = text.length > SINGLE;
  const regions = quad  // integer quarters of the region, in index order
    ? [[L, T, RW >> 1, RH >> 1], [L + (RW >> 1), T, RW - (RW >> 1), RH >> 1],
      [L, T + (RH >> 1), RW >> 1, RH - (RH >> 1)], [L + (RW >> 1), T + (RH >> 1), RW - (RW >> 1), RH - (RH >> 1)]]
    : [[L, T, RW, RH]];

  onStatus("Locking the text with the password…"); await tick();
  const payload = await seal(text, password, quad ? QUAD_BITS : 61);
  const chunks = quad && [0, 1, 2].map(k => payload.slice(k * CHUNK, (k + 1) * CHUNK));
  const marks = quad
    ? [...chunks, xor(...chunks)].map((c, k) => encode(k.toString(2).padStart(2, "0") + c, 3))
    : [encode(payload)];
  const enc = await model(ENCODER);
  onStatus("Hiding the text…"); await tick();
  const t = performance.now();
  for (let k = 0; k < regions.length; k++) await markRegion(enc, full, px, W, regions[k], marks[k], strength);
  g.putImageData(img, 0, 0);
  const ms = Math.round(performance.now() - t);

  // check: read each mark straight back, as the Python kit's digital test does
  const dec = await model(DECODER);
  onStatus("Checking it reads back…"); await tick();
  const bits = [];
  for (const [x, y, w, h] of regions) {
    const out = (await dec.run({ image: tensor(draw(full, 256, 256, x, y, w, h), [1, 3, 256, 256], v => v * 2 - 1) })).output.data;
    bits.push([...out].map(v => v > 0));
  }
  let readBack = null;
  if (!quad) {
    const data = correct(bits[0]);
    readBack = data && await openSealed(data, password);
  } else {
    const data = bits.map(b => correct(b, 3)), ok = data.every((d, k) => d && parseInt(d.slice(0, 2), 2) === k);
    readBack = ok ? await openSealed(data.slice(0, 3).map(d => d.slice(2)).join(""), password) : null;
  }
  return { canvas: full, width: W, height: H, scaled: s < 1, quad, ms, ok: readBack === text.trimEnd(), readBack };
}

// ---- reveal --------------------------------------------------------------------------------------------

// Detector on region [x, y, w, h] of img, scaled so its longest edge is `side`. Boxes come back in img pixels.
async function detect(det, img, side, [x, y, w, h] = [0, 0, img.width, img.height]) {
  const s = side / Math.max(w, h), small = draw(img, Math.round(w * s), Math.round(h * s), x, y, w, h);
  const { boxes, scores } = await det.run({ image: tensor(small, [3, small.height, small.width], v => v) });
  return [...scores.data].map((score, i) => ({
    score, box: [...boxes.data.slice(4 * i, 4 * i + 4)].map((v, k) => v / s + (k % 2 ? y : x)),
  }));
}

// Crop nudges, as (shrink/grow g, shift dx, dy) in fractions of the box. A single mark's box is usually tight;
// a quarter is guessed by splitting a box in four, so it gets wider moves.
const WHOLE = [0, 0.01, 0.02, -0.01].flatMap(g => [[0, 0], [0, .01], [0, -.01], [.01, 0], [-.01, 0]].map(([x, y]) => [g, x, y]));
const QUARTER = [[0, 0, 0], [.02, 0, 0], [-.02, 0, 0], [.02, .02, 0], [.02, -.02, 0], [.02, 0, .02], [.02, 0, -.02],
  [.04, 0, 0], [0, .02, 0], [0, -.02, 0], [0, 0, .02], [0, 0, -.02]];
const MAX_DECODES = 300;  // per reveal, all turns together: ~75 s on a phone when nothing is there

// Decode `rect` of ctx.img with each nudge until check(bits, crop) returns something other than undefined.
async function scan(ctx, [x1, y1, x2, y2], nudges, check) {
  const w = x2 - x1, h = y2 - y1;
  for (const [g, dx, dy] of nudges) {
    if (ctx.decodes >= MAX_DECODES) return null;
    ctx.decodes++;
    const crop = draw(ctx.img, 256, 256, x1 + (g + dx) * w, y1 + (g + dy) * h, (1 - 2 * g) * w, (1 - 2 * g) * h);
    const out = (await ctx.dec.run({ image: tensor(crop, [1, 3, 256, 256], v => v * 2 - 1) })).output.data;
    const r = await check([...out].map(v => v > 0), crop);
    if (r !== undefined) return r;
  }
  return null;
}

// Nudged crops keep yielding the same payload, and the password check is the slow step: run it once each.
async function open(ctx, data) {
  if (!ctx.opened.has(data)) ctx.opened.set(data, await openSealed(data, ctx.password));
  return ctx.opened.get(data);
}

// A single mark: with a password only sealed marks count (their tag rejects random bits); without one, plain
// marks. `locked` records a mark that passed error correction but didn't open ("wrong password").
async function checkWhole(ctx, bits, crop) {
  let text = null;
  if (!ctx.password) {
    text = decodePayload(bits);
    if (text === null && correct(bits) !== null) ctx.locked = true;
  } else {
    const data = correct(bits);
    // BCH_5 lets a random word through about once in 100,000 tries, so one that passes is a real mark
    if (data !== null && (text = await open(ctx, data)) === null) ctx.locked = true;
  }
  if (text !== null) { ctx.onCrop(crop); return text; }
  if (ctx.locked) return null;  // a real mark that doesn't open: other crops would give the same bits
}

// A quarter: BCH_3 lets ~8% of random words through, so each index can collect wrong chunks too. Every mix of
// three indices (0–2, or two of them plus the XOR quarter 3) is tried; the tag picks the right one.
// A mix that doesn't open says nothing on its own: random chunks never open. Only four quarters whose XOR
// checks out (73 bits can't match by chance) prove a real mark, and only then is the password to blame. A
// printout photographed from a plain page once collected dozens of random "quarters" and was reported as a
// wrong password.
async function checkQuarter(ctx, bits) {
  const data = correct(bits, 3);
  if (data === null) return;
  const k = parseInt(data.slice(0, 2), 2), chunk = data.slice(2);
  if (ctx.quads[k].has(chunk)) return true;
  ctx.quads[k].add(chunk);
  ctx.onLog(`  quarter ${k} read (${ctx.decodes} decodes so far)`);
  if (ctx.quads.filter(q => q.size).length < 3) return true;
  const [q0, q1, q2, q3] = ctx.quads.map(q => [...q].slice(-4));  // newest few per index keeps this small
  if (ctx.password) {
    const payloads = new Set();
    for (const a of q0) for (const b of q1) for (const c of q2) payloads.add(a + b + c);
    for (const d of q3) {
      for (const b of q1) for (const c of q2) payloads.add(xor(d, b, c) + b + c);
      for (const a of q0) for (const c of q2) payloads.add(a + xor(d, a, c) + c);
      for (const a of q0) for (const b of q1) payloads.add(a + b + xor(d, a, b));
    }
    for (const p of payloads) { const text = await open(ctx, p); if (text !== null) return text; }
  }
  if (q0.some(a => q1.some(b => q2.some(c => q3.includes(xor(a, b, c)))))) {
    ctx.sure = ctx.locked = true;
    return null;
  }
  return true;
}

const fmt = (b, img) => [b[0] / img.width, b[1] / img.height, b[2] / img.width, b[3] / img.height]
  .map(v => v.toFixed(3)).join(" ");

// Find and open a photo mark in `source` (a File/Blob or bitmap: a camera shot, a screenshot, a received photo).
// Resolves to { text } when found, { locked: true } when a mark is there but the password doesn't open it (or
// none was given), and { text: null } when no mark was found.
export async function revealText(source, password, {
  detector = DETECTORS.fast, side = 640, onStatus = () => {}, onLog = () => {}, onCrop = () => {},
} = {}) {
  const ms = t => `${Math.round(performance.now() - t)} ms`;
  onStatus("Opening the photo…"); await tick();
  const bmp = await bitmap(source);
  onLog(`photo: ${bmp.width}×${bmp.height}`);
  const det = await model(detector), dec = await model(DECODER);
  const ctx = { dec, password, locked: false, sure: false, opened: new Map(), onLog, onCrop, decodes: 0 };

  // Like TrustMark's ROTATION, the photo may be sideways (a landscape print shot in portrait, say). An upright
  // view that clearly finds the picture is read at once; otherwise every turn is scored first and the best
  // is read first, all sharing one budget of MAX_DECODES, so a photo with no mark fails in about a minute on a
  // phone instead of five.
  const views = [];
  for (let q = 0; q < 4; q++) {
    const img = rotated(bmp, q);
    onStatus(`Looking for the picture${q ? ` (turned ${q * 90}°)` : ""}…`); await tick();
    const t = performance.now();
    const found = (await detect(det, img, side)).filter((f, i) => !i || f.score >= 0.1);
    onLog(`${q * 90}°: detector ${ms(t)}, ${found.length} box(es) ${found.map(f => f.score.toFixed(2)).join(" ")}`);
    if (found.length) views.push({ q, img, found });
    if (!q && found[0]?.score >= 0.9) break;
  }
  views.sort((a, b) => b.found[0].score - a.found[0].score);

  for (const { q, img, found } of views) {
    Object.assign(ctx, { img, quads: [0, 1, 2, 3].map(() => new Set()) });

    // Zoom: in a shot where the picture is smaller, the box can be ~20% loose. Detecting again inside the
    // top box plus a 10% margin, where the picture fills the view, gives edges tight enough to decode.
    const [x1, y1, x2, y2] = found[0].box, mw = 0.1 * (x2 - x1), mh = 0.1 * (y2 - y1);
    const zx = Math.max(0, x1 - mw), zy = Math.max(0, y1 - mh);
    onStatus("Zooming in on the picture…"); await tick();
    let t = performance.now();
    const zoomed = await detect(det, img, side, [zx, zy, Math.min(img.width, x2 + mw) - zx, Math.min(img.height, y2 + mh) - zy]);
    onLog(`${q * 90}° zoomed: ${ms(t)}, top box [${zoomed[0] ? fmt(zoomed[0].box, img) : "none"}]`);
    const boxes = [...zoomed.filter((f, i) => !i || f.score >= 0.1), ...found].map(f => f.box);

    onStatus("Reading…"); await tick();
    t = performance.now();
    const before = ctx.decodes;
    // 1. one mark over the whole picture
    let text = await scan(ctx, boxes[0], WHOLE, (b, c) => checkWhole(ctx, b, c));
    if (text) return { text };
    // 2. four-quarter mark: a picture with four marks confuses the detector, which then reports halves or
    // single quarters, so the union of all boxes (usually the whole picture) and then every box are tried as
    // the whole picture split in four, and every box as one quarter
    const union = [0, 1, 2, 3].map(k => (k < 2 ? Math.min : Math.max)(...boxes.map(b => b[k])));
    for (const rect of [union, ...boxes].flatMap(quartersOf).concat(boxes)) {
      text = await scan(ctx, rect, QUARTER, b => checkQuarter(ctx, b));
      if (typeof text === "string") return { text };
      if (ctx.decodes >= MAX_DECODES || ctx.locked) break;
    }
    // 3. the detector's own first box, for a single mark
    if (!ctx.locked) {
      text = await scan(ctx, found[0].box, WHOLE, (b, c) => checkWhole(ctx, b, c));
      if (text) return { text };
    }
    onLog(`  ${ctx.decodes - before} decodes in ${ms(t)}, quarters seen: ${ctx.quads.map((s, k) => s.size ? k : "").join("") || "none"}`);
    if (ctx.locked || ctx.decodes >= MAX_DECODES) break;
  }
  return ctx.locked ? { locked: true } : { text: null };
}
