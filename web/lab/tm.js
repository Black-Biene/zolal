// TrustMark test. Hide: seal the text (tmseal.js), BCH-encode it, run Adobe's encoder on the photo and blend
// the change in like TrustMark's Python encode(). Reveal: picture detector (finds the marked picture in a
// camera shot) + decoder (100 bits), BCH error correction, then open the seal. All ONNX models run in
// onnxruntime-web on the CPU; see research/ai-watermarks/onnx_export.py for the models.
import * as ort from "../vendor/onnxruntime-web/ort.wasm.min.mjs";
import { correct, decodePayload, encode } from "./bch.js?v=dev";
import { openSealed, seal } from "./tmseal.js?v=dev";

ort.env.wasm.wasmPaths = new URL("../vendor/onnxruntime-web/", import.meta.url).href;
ort.env.wasm.numThreads = 1;  // threads need cross-origin isolation, which GitHub Pages can't turn on

const $ = id => document.getElementById(id);
let box = "status";  // the status line of the tab in use (model downloads report there too)
const say = (kind, text) => { $(box).className = "status " + kind; $(box).textContent = text; };
const log = line => { $("log").textContent += line + "\n"; };
const ms = t => `${Math.round(performance.now() - t)} ms`;
const tick = () => new Promise(r => setTimeout(r, 30));  // let the page paint before heavy work

const sessions = {};
async function model(name) {
  if (sessions[name]) return sessions[name];
  const res = await fetch("models/" + name);
  if (!res.ok) throw new Error(`models/${name} is missing (HTTP ${res.status})`);
  const total = +res.headers.get("content-length") || 0, parts = [];
  let got = 0;
  for (const reader = res.body.getReader(); ;) {
    const { done, value } = await reader.read();
    if (done) break;
    parts.push(value); got += value.length;
    say("busy", `Downloading ${name}: ${(got / 1e6).toFixed(0)}${total ? ` / ${(total / 1e6).toFixed(0)}` : ""} MB`);
  }
  const t = performance.now();
  say("busy", `Preparing ${name}…`); await tick();
  sessions[name] = await ort.InferenceSession.create(await new Blob(parts).arrayBuffer());
  log(`${name}: ${(got / 1e6).toFixed(1)} MB, ready in ${ms(t)}`);
  return sessions[name];
}

// pixels of a canvas as a CHW float tensor, values mapped by f
function tensor(canvas, dims, f) {
  const { width: w, height: h } = canvas, px = canvas.getContext("2d").getImageData(0, 0, w, h).data;
  const out = new Float32Array(3 * w * h);
  for (let i = 0; i < w * h; i++) for (let c = 0; c < 3; c++) out[c * w * h + i] = f(px[4 * i + c] / 255);
  return new ort.Tensor("float32", out, dims);
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

const fmt = (b, img) => [b[0] / img.width, b[1] / img.height, b[2] / img.width, b[3] / img.height]
  .map(v => v.toFixed(3)).join(" ");

// Detector on region [x, y, w, h] of img, scaled so its longest edge is `side`. Boxes come back in img pixels.
async function detect(det, img, side, [x, y, w, h] = [0, 0, img.width, img.height]) {
  const s = side / Math.max(w, h), small = draw(img, Math.round(w * s), Math.round(h * s), x, y, w, h);
  const { boxes, scores } = await det.run({ image: tensor(small, [3, small.height, small.width], v => v) });
  return [...scores.data].map((score, i) => ({
    score, box: [...boxes.data.slice(4 * i, 4 * i + 4)].map((v, k) => v / s + (k % 2 ? y : x)),
  }));
}

// The decoder needs the crop within ~1% of the picture's edges, and the browser's resize makes the box differ
// slightly from Python's, so try it as is, shrunk/grown (g) and shifted (dx, dy), in fractions of the box.
// With a password only sealed marks count (their 16-bit tag rejects random bits); without one, plain marks.
// `seen` records a mark that passed error correction but didn't open, to tell "wrong password" from "nothing".
async function payloadText(bits, password, seen) {
  if (!password) {
    const text = decodePayload(bits);
    if (text === null && correct(bits) !== null) seen.locked = true;
    return text;
  }
  const data = correct(bits);
  if (data === null) return null;
  // nudged crops keep yielding the same payload; the password check is the slow step, so run it once each
  if (!seen.opened.has(data)) seen.opened.set(data, await openSealed(data, password));
  const text = seen.opened.get(data);
  if (text === null) seen.locked = true;
  return text;
}

async function decodeBox(dec, img, [x1, y1, x2, y2], label, password, seen) {
  const w = x2 - x1, h = y2 - y1, t = performance.now();
  let tries = 0;
  for (const g of [0, 0.01, 0.02, -0.01]) for (const [dx, dy] of [[0, 0], [0, .01], [0, -.01], [.01, 0], [-.01, 0]]) {
    tries++;
    const crop = draw(img, 256, 256, x1 + (g + dx) * w, y1 + (g + dy) * h, (1 - 2 * g) * w, (1 - 2 * g) * h);
    const out = (await dec.run({ image: tensor(crop, [1, 3, 256, 256], v => v * 2 - 1) })).output.data;
    const text = await payloadText([...out].map(v => v > 0), password, seen);
    if (seen.locked) break;  // a real mark that doesn't open: other crops would give the same bits
    if (text === null) continue;
    log(`  ${label}: decoded on try ${tries} (shrink ${g}, shift ${dx},${dy}) in ${ms(t)}`);
    draw(crop, 256, 256, 0, 0, 256, 256, $("crop")); $("crop").hidden = false;
    return text;
  }
  log(`  ${label}: nothing after ${tries} tries, ${ms(t)}`);
  return null;
}

async function read(file) {
  $("log").textContent = ""; $("crop").hidden = true;
  say("busy", "Opening the photo…"); await tick();
  const bmp = await createImageBitmap(file);
  log(`photo: ${bmp.width}×${bmp.height}, ${(file.size / 1e6).toFixed(1)} MB`);
  const det = await model($("det").value), dec = await model("decoder_Q.onnx");
  const side = +$("side").value, password = $("pw").value, seen = { locked: false, opened: new Map() };

  for (let q = 0; q < 4; q++) {  // like TrustMark's ROTATION: try 0°, 90°, 180°, 270°
    const img = rotated(bmp, q);
    say("busy", `Looking for the picture${q ? ` (turned ${q * 90}°)` : ""}…`); await tick();
    let t = performance.now();
    const found = await detect(det, img, side);
    log(`${q * 90}°: detector ${ms(t)}, ${found.length} box(es)`);

    // weak extra boxes cost ~10 s each on a phone and have never held the mark
    for (const [i, { box, score }] of found.filter((f, i) => !i || f.score >= 0.5).entries()) {
      log(`  box ${i + 1} [${fmt(box, img)}] score ${score.toFixed(2)}`);
      // Zoom: in a shot where the picture is smaller, the box can be ~20% loose. Detecting again inside the
      // box plus a 10% margin, where the picture fills the view, gives edges tight enough to decode.
      const [x1, y1, x2, y2] = box, mw = 0.1 * (x2 - x1), mh = 0.1 * (y2 - y1);
      const zx = Math.max(0, x1 - mw), zy = Math.max(0, y1 - mh);
      const region = [zx, zy, Math.min(img.width, x2 + mw) - zx, Math.min(img.height, y2 + mh) - zy];
      say("busy", "Zooming in on the picture…"); await tick();
      t = performance.now();
      const [top] = await detect(det, img, side, region);
      if (top) log(`  zoomed box [${fmt(top.box, img)}] in ${ms(t)}`);
      say("busy", "Reading…"); await tick();
      const text = (top && await decodeBox(dec, img, top.box, "zoomed", password, seen))
        ?? (seen.locked ? null : await decodeBox(dec, img, box, "first box", password, seen));
      if (text !== null) return say("ok", `Found: ${text}`);
      if (seen.locked) break;
    }
    // an upright view that clearly found the picture isn't fixed by turning it; skip the ~40 s of rotations
    if (seen.locked || (!q && found[0]?.score >= 0.9)) break;
  }
  if (seen.locked) return say("err", password
    ? "Found a mark, but this password doesn't open it."
    : "Found a mark that needs a password. Type it above and read again.");
  say("err", "No hidden text found. Get closer so the picture fills more of the shot, and hold still.");
}

for (const id of ["shoot", "pick"]) $(id).onchange = async e => {
  const file = e.target.files[0];
  e.target.value = "";  // choosing the same file again should read it again
  if (!file) return;
  box = "status";
  try { await read(file); } catch (err) { say("err", "Error: " + err.message); log(String(err.stack || err)); }
};

// ---- hide -------------------------------------------------------------------------------------------------

// iOS Safari refuses canvases above ~16.7 million pixels, so bigger photos (48 MP) are scaled down first.
const MAX_PIXELS = 16_000_000;

async function hide(file, text, password) {
  say("busy", "Opening the photo…"); await tick();
  const bmp = await createImageBitmap(file);
  const s = Math.min(1, Math.sqrt(MAX_PIXELS / (bmp.width * bmp.height)));
  const W = Math.round(bmp.width * s), H = Math.round(bmp.height * s);
  const full = draw(bmp, W, H), g = full.getContext("2d"), img = g.getImageData(0, 0, W, H), px = img.data;

  // TrustMark marks the whole photo, or only a centred square when it's wider than 2:1
  const side = Math.min(W, H), square = Math.max(W, H) / side > 2;
  const [L, T, RW, RH] = square ? [(W - side) >> 1, (H - side) >> 1, side, side] : [0, 0, W, H];

  const bits = encode(await seal(text, password));
  const enc = await model("encoder_Q.onnx");
  say("busy", "Hiding the text…"); await tick();
  const t = performance.now();
  const cover = tensor(draw(full, 256, 256, L, T, RW, RH), [1, 3, 256, 256], v => v * 2 - 1);
  const out = await enc.run({
    [enc.inputNames[0]]: cover,
    [enc.inputNames[1]]: new ort.Tensor("float32", Float32Array.from(bits, Number), [1, 100]),
  });

  // residual = what the encoder changed, minus any colour shift (per-channel mean), as in TrustMark
  const st = out[enc.outputNames[0]].data, cv = cover.data, N = 256 * 256, res = new Float32Array(3 * N);
  for (let c = 0; c < 3; c++) {
    let sum = 0;
    for (let i = c * N; i < (c + 1) * N; i++) sum += res[i] = Math.max(-1, Math.min(1, st[i])) - cv[i];
    for (let i = c * N; i < (c + 1) * N; i++) res[i] -= sum / N;
  }

  // Scale the residual up to the photo (bilinear, like torch interpolate with align_corners=False), add it,
  // and fade it in over the outer 1% (max 50 px) of the region. TrustMark overwrites corners with the
  // column blend; the smaller of the row and column fade is used here, a difference too small to matter.
  const f = Math.min(50, Math.max(1, Math.floor(side * 0.01)));
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
        const orig = px[i + c], wm = Math.floor((Math.max(-1, Math.min(1, r + orig / 127.5 - 1)) + 1) * 127.5);
        px[i + c] = a === 1 ? wm : Math.floor(a * wm + (1 - a) * orig);
      }
    }
  }
  g.putImageData(img, 0, 0);
  const took = ms(t);

  // check: read the result straight back, as the Python kit's digital test does
  const dec = await model("decoder_Q.onnx");
  say("busy", "Checking it reads back…"); await tick();
  const back = (await dec.run({ image: tensor(draw(full, 256, 256, L, T, RW, RH), [1, 3, 256, 256], v => v * 2 - 1) }))
    .output.data;
  const data = correct([...back].map(v => v > 0)), readBack = data && await openSealed(data, password);

  const view = $("m-view"), k = Math.min(1, 1200 / Math.max(W, H));
  view.width = Math.round(W * k); view.height = Math.round(H * k);
  draw(full, view.width, view.height, 0, 0, W, H, view);
  $("m-result").hidden = false;
  $("m-save").onclick = () => full.toBlob(b => {
    const a = document.createElement("a");
    a.href = URL.createObjectURL(b); a.download = (file.name.replace(/\.[^.]*$/, "") || "photo") + "-marked.png";
    a.click(); setTimeout(() => URL.revokeObjectURL(a.href), 60_000);
  }, "image/png");
  if (readBack !== text) return say("err", `Made the picture, but it didn't read back (${readBack ?? "nothing"}). Try another photo.`);
  say("ok", `Hidden: “${text}” in ${W}×${H}${s < 1 ? " (scaled down)" : ""}, ${took}. It reads back. Now save it.`);
}

$("m-go").onclick = async () => {
  box = "m-status";
  const file = $("m-photo").files[0], text = $("m-text").value, password = $("m-pw").value;
  if (!file) return say("err", "Choose a photo first.");
  if (!text) return say("err", "Type a text to hide.");
  if (!password) return say("err", "Choose a password.");
  $("m-result").hidden = true;
  try { await hide(file, text, password); } catch (err) { say("err", "Error: " + err.message); console.error(err); }
};

const tabs = [...document.querySelectorAll('[role="tab"]')];
for (const tab of tabs) tab.onclick = () => {
  for (const u of tabs) {
    u.setAttribute("aria-selected", u === tab);
    u.tabIndex = u === tab ? 0 : -1;
    $(u.getAttribute("aria-controls")).hidden = u !== tab;
  }
};
