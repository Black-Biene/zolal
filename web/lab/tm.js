// TrustMark reader test: picture detector (finds the marked picture in a camera shot) + decoder (100 bits),
// both ONNX models run by onnxruntime-web on the CPU, then BCH error correction. Mirrors TrustMark's Python
// decode(DETECTFIRST=True, ROTATION=True); see research/ai-watermarks/onnx_export.py for the models.
import * as ort from "../vendor/onnxruntime-web/ort.wasm.min.mjs";
import { decodePayload } from "./bch.js?v=dev";

ort.env.wasm.wasmPaths = new URL("../vendor/onnxruntime-web/", import.meta.url).href;
ort.env.wasm.numThreads = 1;  // threads need cross-origin isolation, which GitHub Pages can't turn on

const $ = id => document.getElementById(id);
const say = (kind, text) => { $("status").className = "status " + kind; $("status").textContent = text; };
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

async function read(file) {
  $("log").textContent = ""; $("crop").hidden = true;
  say("busy", "Opening the photo…"); await tick();
  const bmp = await createImageBitmap(file);
  log(`photo: ${bmp.width}×${bmp.height}, ${(file.size / 1e6).toFixed(1)} MB`);
  const det = await model($("det").value), dec = await model("decoder_Q.onnx");
  const side = +$("side").value;

  for (let q = 0; q < 4; q++) {  // like TrustMark's ROTATION: try 0°, 90°, 180°, 270°
    const img = rotated(bmp, q), s = side / Math.max(img.width, img.height);
    say("busy", `Looking for the picture${q ? ` (turned ${q * 90}°)` : ""}…`); await tick();
    let t = performance.now();
    const small = draw(img, Math.round(img.width * s), Math.round(img.height * s));
    const { boxes, scores } = await det.run({ image: tensor(small, [3, small.height, small.width], v => v) });
    log(`${q * 90}°: detector ${ms(t)}, ${scores.data.length} box(es)`);

    for (let i = 0; i < scores.data.length; i++) {
      const [x1, y1, x2, y2] = [...boxes.data.slice(4 * i, 4 * i + 4)].map(v => v / s);
      const at = [x1 / img.width, y1 / img.height, x2 / img.width, y2 / img.height].map(v => v.toFixed(3)).join(" ");
      log(`  box ${i + 1} [${at}] score ${scores.data[i].toFixed(2)}`);
      // The decoder needs the crop within ~1% of the picture's edges, and the browser's resize makes the box
      // differ slightly from Python's, so also try it shrunk/grown (g) and shifted (dx, dy), as box fractions.
      const w = x2 - x1, h = y2 - y1;
      t = performance.now();
      let tries = 0;
      for (const g of [0, 0.01, 0.02, -0.01]) for (const [dx, dy] of [[0, 0], [0, .01], [0, -.01], [.01, 0], [-.01, 0]]) {
        tries++;
        const crop = draw(img, 256, 256, x1 + (g + dx) * w, y1 + (g + dy) * h, (1 - 2 * g) * w, (1 - 2 * g) * h);
        const out = (await dec.run({ image: tensor(crop, [1, 3, 256, 256], v => v * 2 - 1) })).output.data;
        const text = decodePayload([...out].map(v => v > 0));
        if (text === null) continue;
        log(`  decoded on try ${tries} (shrink ${g}, shift ${dx},${dy}) in ${ms(t)}`);
        draw(crop, 256, 256, 0, 0, 256, 256, $("crop")); $("crop").hidden = false;
        return say("ok", `Found: ${text}`);
      }
      log(`  nothing after ${tries} tries, ${ms(t)}`);
    }
  }
  say("err", "No hidden text found. Get closer so the picture fills more of the shot, and hold still.");
}

for (const id of ["shoot", "pick"]) $(id).onchange = async e => {
  const file = e.target.files[0];
  e.target.value = "";  // choosing the same file again should read it again
  if (!file) return;
  try { await read(file); } catch (err) { say("err", "Error: " + err.message); log(String(err.stack || err)); }
};
