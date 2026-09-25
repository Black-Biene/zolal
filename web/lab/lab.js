import { embed, TEXT_BYTES } from "./wm.js?v=dev";

const $ = id => document.getElementById(id);
const say = (box, kind, text) => { box.className = "status " + kind; box.textContent = text; };

// ---- tabs ----------------------------------------------------------------------------------------------

const tabs = [...document.querySelectorAll('[role="tab"]')];
for (const t of tabs) t.onclick = () => {
  for (const u of tabs) {
    u.setAttribute("aria-selected", u === t);
    u.tabIndex = u === t ? 0 : -1;
    $(u.getAttribute("aria-controls")).hidden = u !== t;
  }
  if (t.id !== "tab-read") stopCamera();
};

// ---- make ----------------------------------------------------------------------------------------------

const bytesOf = s => new TextEncoder().encode(s).length;
$("make-text").oninput = () => {
  const n = bytesOf($("make-text").value);
  $("make-count").textContent = `${n} / ${TEXT_BYTES} bytes${n > TEXT_BYTES ? " — too long" : ""}`;
};

let marked = null, markedName = "marked";
$("make").onclick = async () => {
  const box = $("make-status"), file = $("make-photo").files[0], text = $("make-text").value;
  if (!file) return say(box, "err", "Choose a photo first.");
  if (!text.trim()) return say(box, "err", "Type a text to hide.");
  if (bytesOf(text) > TEXT_BYTES) return say(box, "err", `The text is too long (max ${TEXT_BYTES} bytes).`);
  say(box, "busy", "Working…");
  await new Promise(r => setTimeout(r, 30));
  try {
    const bmp = await createImageBitmap(file);
    marked = embed(bmp, text);
    markedName = file.name.replace(/\.[^.]*$/, "") || "marked";
    const view = $("make-canvas");
    view.width = marked.width; view.height = marked.height;
    view.getContext("2d").drawImage(marked, 0, 0);
    $("make-result").hidden = false;
    say(box, "ok", `Done: ${marked.width}×${marked.height}. Now test reading it.`);
  } catch (e) {
    say(box, "err", "Couldn't use this photo: " + e.message);
  }
};

$("make-save").onclick = () => marked.toBlob(b => {
  const a = document.createElement("a");
  a.href = URL.createObjectURL(b); a.download = markedName + "-marked.png"; a.click();
  setTimeout(() => URL.revokeObjectURL(a.href), 60_000);
}, "image/png");

$("make-full").onclick = () => {
  const c = $("full-canvas");
  c.width = marked.width; c.height = marked.height;
  c.getContext("2d").drawImage(marked, 0, 0);
  $("fullscreen").hidden = false;
  $("fullscreen").requestFullscreen?.().catch(() => {});
};
$("fullscreen").onclick = () => {
  $("fullscreen").hidden = true;
  if (document.fullscreenElement) document.exitFullscreen();
};

// ---- read: camera ----------------------------------------------------------------------------------------

let stream = null;
function stopCamera() {
  stream?.getTracks().forEach(t => t.stop());
  stream = null;
  $("camera").hidden = true;
}

$("read-camera").onclick = async () => {
  $("corners").hidden = true;
  try {
    stream = await navigator.mediaDevices.getUserMedia({
      // ask for 4K: browsers often default to 720p, too coarse for the pattern
      video: { facingMode: "environment", width: { ideal: 3840 }, height: { ideal: 2160 } }, audio: false,
    });
  } catch (e) {
    return alert("Camera not available: " + e.message);
  }
  const v = $("video");
  v.srcObject = stream;
  await v.play();
  $("camera").hidden = false;
  const s = stream.getVideoTracks()[0].getSettings();
  $("cam-res").textContent = `Camera: ${s.width ?? v.videoWidth}×${s.height ?? v.videoHeight}` +
    ((s.width ?? v.videoWidth) < 1920 ? " (low; the phone's own camera app gives sharper shots, see below)" : "");
};

$("capture").onclick = () => {
  const v = $("video"), c = document.createElement("canvas");
  c.width = v.videoWidth; c.height = v.videoHeight;
  c.getContext("2d").drawImage(v, 0, 0);
  stopCamera();
  // start the box on the guide frame (inset 10% sides, 12% top/bottom)
  const w = c.width, h = c.height;
  openEditor(c, [0.1 * w, 0.12 * h, 0.9 * w, 0.88 * h]);
};

$("read-file").onchange = async () => {
  const file = $("read-file").files[0];
  if (!file) return;
  stopCamera();
  const bmp = await createImageBitmap(file), c = document.createElement("canvas");
  c.width = bmp.width; c.height = bmp.height;
  c.getContext("2d").drawImage(bmp, 0, 0);
  const w = c.width, h = c.height;
  openEditor(c, [0, 0, w, h]);
  $("read-file").value = "";
};

// ---- read: crop box -----------------------------------------------------------------------------------

// The box is kept in shot pixels: [x0, y0, x1, y1]. The reader gets its corners and snaps them to the
// picture's real edges (which may be a tilted quadrilateral).
let shot = null, box = null, corners = null, shotInfo = "";

// Save the captured frame and the box (in the file name) so a failing case can be analysed.
$("save-shot").onclick = () => shot.toBlob(b => {
  const a = document.createElement("a");
  a.href = URL.createObjectURL(b); a.download = `zolal-shot_${box.map(Math.round).join("_")}.png`; a.click();
  setTimeout(() => URL.revokeObjectURL(a.href), 60_000);
}, "image/png");

const boxCorners = ([x0, y0, x1, y1]) => [[x0, y0], [x1, y0], [x1, y1], [x0, y1]];

function openEditor(canvas, start) {
  shot = canvas; box = start;
  const view = $("shot");
  view.width = canvas.width; view.height = canvas.height;
  view.getContext("2d").drawImage(canvas, 0, 0);
  $("corners").hidden = false;
  $("read-status").className = "status";
  $("read-status").textContent = "";
  $("save-shot").hidden = true;
  requestAnimationFrame(placeBox);
}

function placeBox() {
  const view = $("shot"), s = view.clientWidth / view.width, c = $("crop");
  c.style.left = box[0] * s + "px"; c.style.top = box[1] * s + "px";
  c.style.width = (box[2] - box[0]) * s + "px"; c.style.height = (box[3] - box[1]) * s + "px";
}
addEventListener("resize", () => shot && placeBox());

// Drag the box body to move it, a corner grip to resize.
$("crop").onpointerdown = e => {
  const grip = e.target.dataset.g, view = $("shot"), el = e.target;
  el.setPointerCapture(e.pointerId);
  const r = view.getBoundingClientRect(), s = view.width / r.width, start = box.slice(), x0 = e.clientX, y0 = e.clientY;
  const W = view.width, H = view.height, MIN = 0.05 * Math.min(W, H);
  el.onpointermove = ev => {
    const dx = (ev.clientX - x0) * s, dy = (ev.clientY - y0) * s;
    let [a, b, c, d] = start;
    if (!grip) {
      const w = c - a, h = d - b;
      a = Math.max(0, Math.min(W - w, a + dx)); b = Math.max(0, Math.min(H - h, b + dy));
      c = a + w; d = b + h;
    } else {
      if (grip.includes("l")) a = Math.max(0, Math.min(c - MIN, a + dx));
      if (grip.includes("r")) c = Math.min(W, Math.max(a + MIN, c + dx));
      if (grip.includes("t")) b = Math.max(0, Math.min(d - MIN, b + dy));
      if (grip.includes("b")) d = Math.min(H, Math.max(b + MIN, d + dy));
    }
    box = [a, b, c, d];
    placeBox();
  };
  el.onpointerup = () => { el.onpointermove = null; };
};

// The pattern has ~100 cells across the picture's shorter side; below ~700 shot pixels each cell is too few
// pixels for the camera to resolve.
function tooSmall() {
  const side = Math.min(box[2] - box[0], box[3] - box[1]);
  return side < 700 ? ` The picture is small in this shot (about ${Math.round(side)} px across); move closer so ` +
    "it fills most of the frame." : "";
}

// Show the exact edges the reader found, as a thin outline on the shot.
function drawFound(quad) {
  const view = $("shot"), ctx = view.getContext("2d");
  ctx.drawImage(shot, 0, 0);
  ctx.strokeStyle = "#22c55e"; ctx.lineWidth = Math.max(2, view.width / 400);
  ctx.beginPath(); quad.forEach(([x, y], i) => i ? ctx.lineTo(x, y) : ctx.moveTo(x, y)); ctx.closePath(); ctx.stroke();
}

// ---- read: decode --------------------------------------------------------------------------------------

$("read").onclick = () => {
  const statusBox = $("read-status"), button = $("read");
  shotInfo = `${shot.width}x${shot.height}`;
  const img = shot.getContext("2d").getImageData(0, 0, shot.width, shot.height);
  button.disabled = true;
  say(statusBox, "busy", "Reading… (finding the picture's edges)");
  const worker = new Worker("worker.js?v=dev", { type: "module" });
  worker.onmessage = ({ data }) => {
    if (data.progress) return say(statusBox, "busy", `Reading… (aligning, step ${data.progress}px)`);
    worker.terminate();
    button.disabled = false;
    corners = data.corners;
    drawFound(corners);
    const diag = `signal ${Math.round(data.match * 100)}% · shot ${shotInfo} · ${(data.ms / 1000).toFixed(1)} s`;
    if (data.text !== null) say(statusBox, "ok", `Found: “${data.text}”  (${diag})`);
    else say(statusBox, "err", `No hidden text found (${diag}).${tooSmall()} Signal around 75–78% means no pattern was seen at ` +
      "all; 80–95% means it's there but too weak (a clean file reads about 99%). Try closer, steadier, or the " +
      "phone's own camera app.");
    $("save-shot").hidden = false;
  };
  worker.onerror = e => { button.disabled = false; say(statusBox, "err", "Error: " + e.message); };
  worker.postMessage({ img, corners: boxCorners(box) });
};
