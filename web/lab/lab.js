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
      video: { facingMode: "environment", width: { ideal: 1920 }, height: { ideal: 1080 } }, audio: false,
    });
  } catch (e) {
    return alert("Camera not available: " + e.message);
  }
  const v = $("video");
  v.srcObject = stream;
  await v.play();
  $("camera").hidden = false;
};

$("capture").onclick = () => {
  const v = $("video"), c = document.createElement("canvas");
  c.width = v.videoWidth; c.height = v.videoHeight;
  c.getContext("2d").drawImage(v, 0, 0);
  stopCamera();
  // start the dots on the guide frame (inset 10% sides, 12% top/bottom)
  const w = c.width, h = c.height;
  openEditor(c, [[0.1 * w, 0.12 * h], [0.9 * w, 0.12 * h], [0.9 * w, 0.88 * h], [0.1 * w, 0.88 * h]]);
};

$("read-file").onchange = async () => {
  const file = $("read-file").files[0];
  if (!file) return;
  stopCamera();
  const bmp = await createImageBitmap(file), c = document.createElement("canvas");
  c.width = bmp.width; c.height = bmp.height;
  c.getContext("2d").drawImage(bmp, 0, 0);
  const w = c.width, h = c.height;
  openEditor(c, [[0, 0], [w, 0], [w, h], [0, h]]);
  $("read-file").value = "";
};

// ---- read: corner editor -------------------------------------------------------------------------------

let shot = null, corners = null;
const dots = [...document.querySelectorAll(".dot")];

function openEditor(canvas, start) {
  shot = canvas; corners = start;
  const view = $("shot");
  view.width = canvas.width; view.height = canvas.height;
  view.getContext("2d").drawImage(canvas, 0, 0);
  $("corners").hidden = false;
  $("read-status").className = "status";
  $("read-status").textContent = "";
  requestAnimationFrame(placeDots);
}

function placeDots() {
  const view = $("shot"), s = view.clientWidth / view.width;
  dots.forEach((d, i) => { d.style.left = corners[i][0] * s + "px"; d.style.top = corners[i][1] * s + "px"; });
}
addEventListener("resize", () => shot && placeDots());

for (const d of dots) {
  d.onpointerdown = e => {
    d.setPointerCapture(e.pointerId);
    const i = +d.dataset.i, view = $("shot");
    d.onpointermove = ev => {
      const r = view.getBoundingClientRect(), s = view.width / r.width;
      corners[i] = [Math.max(0, Math.min(view.width, (ev.clientX - r.left) * s)),
                    Math.max(0, Math.min(view.height, (ev.clientY - r.top) * s))];
      placeDots();
    };
    d.onpointerup = () => { d.onpointermove = null; };
  };
}

// ---- read: decode --------------------------------------------------------------------------------------

$("read").onclick = () => {
  const box = $("read-status"), button = $("read");
  const img = shot.getContext("2d").getImageData(0, 0, shot.width, shot.height);
  button.disabled = true;
  say(box, "busy", "Reading… (aligning the corners)");
  const worker = new Worker("worker.js?v=dev", { type: "module" });
  worker.onmessage = ({ data }) => {
    if (data.progress) return say(box, "busy", `Reading… (aligning, step ${data.progress}px)`);
    worker.terminate();
    button.disabled = false;
    corners = data.corners;
    placeDots();
    if (data.text !== null) say(box, "ok", `Found: “${data.text}”  (${(data.ms / 1000).toFixed(1)} s)`);
    else say(box, "err", `No hidden text found (${(data.ms / 1000).toFixed(1)} s). Move the dots closer to the ` +
      "corners, or capture again closer and steadier.");
  };
  worker.onerror = e => { button.disabled = false; say(box, "err", "Error: " + e.message); };
  worker.postMessage({ img, corners });
};
