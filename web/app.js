// Everything below runs locally: the shim and the wasm are this site's own files (see the CSP in index.html),
// both are loaded up front, and files only ever go into an in-memory filesystem and back out as downloads.
import { WASI, File, OpenFile, PreopenDirectory, ConsoleStdout } from "./vendor/browser_wasi_shim/index.js";

const $ = id => document.getElementById(id);
const wasm = fetch("zolal-cli.wasm?v=dev").then(r => WebAssembly.compileStreaming(r));
wasm.catch(() => {
  for (const b of document.querySelectorAll(".primary")) b.disabled = true;
  show($("hide-status"), "err", [strong("Couldn't load. "), "Check your connection and reload the page."]);
});

// Lets the page open again without a connection once it has been visited.
if ("serviceWorker" in navigator) navigator.serviceWorker.register("sw.js").catch(() => {});

// Every failed reveal looks the same and takes at least this long, so the result doesn't tell whoever is
// watching whether the file holds anything (see zolal-core's error.rs).
const REVEAL_MIN_MS = 2500;
const CARRIER = ".zolal-carrier", OUTPUT = ".zolal-output";

// ---- engine --------------------------------------------------------------------------------------------

async function run(args, files, pass) {
  const stdout = [], stderr = [];
  const dir = new PreopenDirectory(".", new Map(
    Object.entries(files).map(([n, d]) => [n, new File(d)])));
  const wasi = new WASI(["zolal-cli", ...args], [`ZOLAL_PASS=${pass}`], [
    new OpenFile(new File([])),
    ConsoleStdout.lineBuffered(l => stdout.push(l)), ConsoleStdout.lineBuffered(l => stderr.push(l)), dir]);
  const inst = await WebAssembly.instantiate(await wasm, { wasi_snapshot_preview1: wasi.wasiImport });
  let code;
  try { code = wasi.start(inst); } catch (e) { code = 1; stderr.push("error: " + e); }
  return { code, stdout, stderr, files: dir.dir.contents };
}

// ---- small helpers -------------------------------------------------------------------------------------

const bytes = f => f.arrayBuffer().then(b => new Uint8Array(b));
const human = n => n < 1024 ? `${n} B` : n < 1048576 ? `${(n / 1024).toFixed(1)} KB` : `${(n / 1048576).toFixed(1)} MB`;
const paint = () => new Promise(r => requestAnimationFrame(() => setTimeout(r, 30)));
const sleep = ms => new Promise(r => setTimeout(r, ms));
const el = (tag, cls, ...kids) => {
  const e = document.createElement(tag);
  if (cls) e.className = cls;
  e.append(...kids);
  return e;
};
const strong = t => el("strong", "", t);
function show(box, kind, ...paras) {
  box.className = "status " + kind;
  box.replaceChildren(...paras.map(p => p instanceof Node ? p : el("p", "", ...[p].flat())));
}
function busy(box, text) {
  box.className = "status busy";
  box.replaceChildren(el("span", "spinner"), text);
}
function download(name, data) {
  const a = document.createElement("a");
  a.href = URL.createObjectURL(new Blob([data]));
  a.download = name;
  a.click();
  setTimeout(() => URL.revokeObjectURL(a.href), 60_000);
}
// Payload names are stored in the hidden bundle; keep them as the user sees them, but unique and flat.
// Takes anything with a `name` (a File, or the message placeholder).
function uniqueNames(list) {
  const seen = new Set([CARRIER, OUTPUT]);
  return list.map(f => {
    const base = f.name.replace(/[\\/\u0000]/g, "_").replace(/^\.+$/, "file") || "file";
    let name = base, i = 2;
    while (seen.has(name)) name = base.replace(/(\.[^.]*)?$/, ` (${i++})$1`);
    seen.add(name);
    return name;
  });
}

// ---- tabs ----------------------------------------------------------------------------------------------

const tabs = [...document.querySelectorAll('[role="tab"]')];
function select(tab) {
  for (const t of tabs) {
    const on = t === tab;
    t.setAttribute("aria-selected", on);
    t.tabIndex = on ? 0 : -1;
    $(t.getAttribute("aria-controls")).hidden = !on;
  }
}
for (const t of tabs) {
  t.onclick = () => select(t);
  t.onkeydown = e => {
    if (e.key !== "ArrowLeft" && e.key !== "ArrowRight") return;
    const next = tabs[(tabs.indexOf(t) + 1) % tabs.length];
    select(next); next.focus();
  };
}

// ---- file drop areas -----------------------------------------------------------------------------------

for (const drop of document.querySelectorAll(".drop")) {
  const input = drop.querySelector("input"), hint = drop.querySelector(".drop-hint");
  const empty = hint.textContent;
  const update = async () => {
    const files = [...input.files];
    drop.classList.toggle("filled", files.length > 0);
    hint.textContent = !files.length ? empty
      : files.length === 1 ? `${files[0].name} · ${human(files[0].size)}`
      : `${files.length} files · ${human(files.reduce((s, f) => s + f.size, 0))}`;
    if (input.id === "hide-carrier") {
      $("step2").disabled = !files.length;
      const plan = files.length && await carrierPlan(files[0]);
      if (plan?.label && input.files[0] === files[0]) hint.textContent += ` · ${plan.label}`;
    }
  };
  input.addEventListener("change", update);
  drop.addEventListener("dragover", e => { e.preventDefault(); if (!input.disabled) drop.classList.add("over"); });
  drop.addEventListener("dragleave", () => drop.classList.remove("over"));
  drop.addEventListener("drop", e => {
    e.preventDefault(); drop.classList.remove("over");
    if (input.disabled || !e.dataTransfer.files.length) return;
    if (!input.multiple && e.dataTransfer.files.length > 1) {
      const dt = new DataTransfer(); dt.items.add(e.dataTransfer.files[0]); input.files = dt.files;
    } else input.files = e.dataTransfer.files;
    update();
  });
}

for (const peek of document.querySelectorAll(".peek")) {
  const input = $(peek.getAttribute("aria-controls"));
  peek.onclick = () => {
    const hidden = input.type === "password";
    input.type = hidden ? "text" : "password";
    peek.textContent = hidden ? "Hide" : "Show";
    peek.setAttribute("aria-label", (hidden ? "Hide" : "Show") + " passphrase");
  };
}

// ---- cover file: pass through what the engine takes, convert what it doesn't ---------------------------
//
// The engine hides in JPEG, MP4, PDF and MP3. Other photos are redrawn as JPEG; a MOV is relabelled as MP4,
// which it already is structurally (both are ISO media files), so the video is never re-encoded.

const ascii = (b, from, to) => String.fromCharCode(...b.subarray(from, to));
const HEIF_BRANDS = ["heic", "heix", "hevc", "heim", "heis", "mif1", "msf1", "avif", "avis"];

function sniff(b) {
  if (b[0] === 0xFF && b[1] === 0xD8) return "jpeg";
  if (ascii(b, 0, 5) === "%PDF-") return "pdf";
  if (ascii(b, 0, 3) === "ID3" || (b[0] === 0xFF && (b[1] & 0xE6) === 0xE2)) return "mp3";
  if (ascii(b, 4, 8) === "ftyp") {
    const brand = ascii(b, 8, 12);
    return brand === "qt  " ? "mov" : HEIF_BRANDS.includes(brand) ? "image" : "mp4";
  }
  if (["moov", "wide", "mdat", "free", "skip", "pnot"].includes(ascii(b, 4, 8))) return "old-mov";
  return "other";
}

const isImage = f => f.type.startsWith("image/") || /\.(png|webp|gif|bmp|tiff?|heic|heif|avif)$/i.test(f.name);
const baseName = f => f.name.replace(/\.[^.]*$/, "") || "file";

// What will happen to this cover file: `kind` from sniff, and a short label for the file picker.
async function carrierPlan(file) {
  const kind = sniff(await bytes(file.slice(0, 16)));
  if (kind === "mov") return { kind, label: "will be saved as MP4" };
  if (kind === "image" || (kind === "other" && isImage(file))) return { kind: "image", label: "will be saved as JPEG" };
  if (kind === "old-mov" || kind === "other") return { kind, label: "not supported" };
  return { kind };
}

async function prepareCarrier(file) {
  const { kind } = await carrierPlan(file);
  const ext = (file.name.match(/\.([^.]+)$/)?.[1] ?? "").toUpperCase() || "this format";
  switch (kind) {
    case "jpeg": case "pdf": case "mp3": case "mp4":
      return { data: await bytes(file), name: file.name };
    case "mov":
      return { data: movToMp4(await bytes(file)), name: baseName(file) + ".mp4",
        note: "Converted from MOV to MP4; the video itself wasn't re-encoded." };
    case "image": {
      const { data, resized } = await toJpeg(file, ext, kind);
      return { data, name: baseName(file) + ".jpg",
        note: `Converted from ${ext} to JPEG${resized ? " and resized to fit" : ""}.` };
    }
    case "old-mov":
      throw new Error("This video uses an old QuickTime layout that can't be converted. Export it as MP4 and try again.");
    default:
      throw new Error("This file type can't be used as a cover. Use a photo, a video (MP4 or MOV), an MP3 song or a PDF.");
  }
}

// Same bytes with the file-type brands changed from QuickTime to MP4. Every box keeps its size, so nothing
// inside the file moves and the video's own offsets stay valid.
function movToMp4(data) {
  const out = data.slice();
  const size = new DataView(out.buffer).getUint32(0);
  const put = (at, s) => { for (let i = 0; i < 4; i++) out[at + i] = s.charCodeAt(i); };
  put(8, "mp42");
  for (let at = 16; at + 4 <= Math.min(size, out.length); at += 4) if (ascii(out, at, at + 4) === "qt  ") put(at, "isom");
  return out;
}

// libheif (vendor/libheif, LGPL-3.0, loaded unmodified as its own file) decodes HEIC where the browser can't:
// Safari opens HEIC itself, Chrome and Firefox don't. Loaded only when a HEIC actually needs it.
let libheif = null;
function loadLibheif() {
  libheif ??= (async () => {
    // This build instantiates synchronously, so it needs the wasm handed to it rather than fetching it.
    const [wasmBinary] = await Promise.all([
      fetch("vendor/libheif/libheif.wasm?v=dev").then(r => r.ok ? r.arrayBuffer() : Promise.reject(r.status)),
      new Promise((resolve, reject) => {
        const script = document.createElement("script");
        script.src = "vendor/libheif/libheif.js?v=dev";
        script.onload = resolve;
        script.onerror = reject;
        document.head.append(script);
      }),
    ]);
    let ready;
    const initialized = new Promise((resolve, reject) => { ready = resolve; setTimeout(reject, 30_000); });
    const lib = self.libheif({ wasmBinary, onRuntimeInitialized: () => ready() });
    await initialized;
    return lib;
  })().catch(e => { libheif = null; throw e; });
  return libheif;
}

async function decodeHeic(file) {
  const lib = await loadLibheif();
  const images = new lib.HeifDecoder().decode(await bytes(file));
  const image = images.find(i => i.is_primary()) ?? images[0];
  if (!image) throw new Error("not a readable HEIC");
  const width = image.get_width(), height = image.get_height();
  const pixels = await new Promise((resolve, reject) => image.display(
    { data: new Uint8ClampedArray(width * height * 4), width, height },
    d => d ? resolve(d) : reject(new Error("HEIC decode failed"))));
  return createImageBitmap(new ImageData(pixels.data, width, height));
}

// Redraw a photo as JPEG. The browser opens PNG, WebP, GIF and AVIF itself, and HEIC falls back to libheif.
// Very large photos are scaled to 16 megapixels, the most a canvas can hold on iPhones.
async function toJpeg(file, ext, kind) {
  let img;
  try { img = await createImageBitmap(file); } catch {
    const heic = kind === "image" || /HEI[CF]/.test(ext);
    try {
      if (!heic) throw new Error("unsupported");
      img = await decodeHeic(file);
    } catch {
      throw new Error(heic
        ? "Couldn't read this HEIC photo. It may be damaged; try exporting it as JPEG."
        : `This browser can't open this ${ext} image. Convert it to JPEG first.`);
    }
  }
  const MAX_PIXELS = 16_000_000;
  const scale = Math.min(1, Math.sqrt(MAX_PIXELS / (img.width * img.height)));
  const canvas = document.createElement("canvas");
  canvas.width = Math.round(img.width * scale);
  canvas.height = Math.round(img.height * scale);
  const ctx = canvas.getContext("2d");
  ctx.fillStyle = "#fff"; // transparent areas become white, not black
  ctx.fillRect(0, 0, canvas.width, canvas.height);
  ctx.drawImage(img, 0, 0, canvas.width, canvas.height);
  img.close();
  const blob = await new Promise(r => canvas.toBlob(r, "image/jpeg", 0.92));
  if (!blob) throw new Error("Couldn't convert this photo to JPEG. Convert it first and try again.");
  return { data: await bytes(blob), resized: scale < 1 };
}

// ---- hide ----------------------------------------------------------------------------------------------

// A typed message travels as a text file with this name, and is shown as text when revealed.
const MESSAGE = "Message.txt";

const VERDICT = {
  Natural: ["ok", "The size change looks normal for this kind of file."],
  Noticeable: ["warn", "It's noticeably bigger than a typical file like it. Fine for a casual look; a larger " +
    "cover file would hide it better."],
  Suspicious: ["warn", "It's much bigger than a normal file like it, so someone comparing sizes would notice. " +
    "Try a larger cover file, or a video."],
};

const plural = (n, word) => `${n} ${word}${n === 1 ? "" : "s"}`;

$("hide").onclick = async () => {
  const box = $("hide-status"), carrier = $("hide-carrier").files[0], payloads = [...$("hide-payload").files];
  const message = $("hide-message").value, pass = $("hide-pass").value;
  const hasMessage = message.trim() !== "";
  if (!carrier) return show(box, "err", "Step 1: choose a cover file first.");
  if (!payloads.length && !hasMessage) return show(box, "err", "Step 2: add a file or photo, or write a message.");
  if (!pass) return show(box, "err", "Step 3: set a passphrase.");

  const button = $("hide");
  button.disabled = true;
  busy(box, "Preparing the cover file…");
  await paint();
  try {
    let cover;
    try { cover = await prepareCarrier(carrier); } catch (e) {
      return show(box, "err", [strong("Can't use this cover file. "), e.message]);
    }
    busy(box, "Encrypting and hiding…");
    await paint();

    // The message goes first so it keeps its name; a file that happens to share it gets " (2)".
    const items = [...(hasMessage ? [{ name: MESSAGE }] : []), ...payloads];
    const names = uniqueNames(items);
    const files = { [CARRIER]: cover.data };
    for (let i = 0; i < items.length; i++) {
      files[names[i]] = items[i] instanceof Blob ? await bytes(items[i]) : new TextEncoder().encode(message);
    }
    const r = await run(["hide", CARRIER, OUTPUT, ...names.map(n => "./" + n)], files, pass);
    const out = r.files.get(OUTPUT);
    if (r.code !== 0 || !out) {
      const msg = r.stderr.filter(l => l.startsWith("error: ")).map(l => l.slice(7)).join(" ") || "Something went wrong.";
      return show(box, "err", [strong("Couldn't hide them. "), msg[0].toUpperCase() + msg.slice(1)]);
    }
    download(cover.name, out.data);

    const what = [hasMessage && "your message", payloads.length && plural(payloads.length, "file")]
      .filter(Boolean).join(" and ");
    const m = r.stdout.join("\n").match(/size .+ -> .+ \(x[\d.]+, (\w+)\)/);
    const [kind, note] = VERDICT[m?.[1]] ?? VERDICT.Natural;
    const again = el("button", "again", `Download ${cover.name} again`);
    again.onclick = () => download(cover.name, out.data);
    show(box, kind,
      [strong("Done. "), `${what[0].toUpperCase() + what.slice(1)} ${what.includes(" and ") || payloads.length > 1 ? "are" : "is"} ` +
        `hidden in ${cover.name}, which has been downloaded.`],
      ...(cover.note ? [el("p", "small", cover.note)] : []),
      el("p", "small", `${human(cover.data.length)} → ${human(out.data.length)}. ${note}`),
      el("p", "small", "Remember: send it as a file or document (or in a .zip), not as a photo or video. " +
        "WhatsApp, Telegram and other chat apps compress photos and videos, and that deletes what's hidden."),
      again);
  } finally {
    button.disabled = false;
  }
};

// ---- reveal --------------------------------------------------------------------------------------------

$("reveal").onclick = async () => {
  const box = $("reveal-status"), carrier = $("reveal-carrier").files[0], pass = $("reveal-pass").value;
  if (!carrier) return show(box, "err", "Choose the file to open first.");
  if (!pass) return show(box, "err", "Enter the passphrase.");

  const button = $("reveal");
  button.disabled = true;
  busy(box, "Checking…");
  const started = performance.now();
  await paint();
  try {
    const r = await run(["reveal", CARRIER, "."], { [CARRIER]: await bytes(carrier) }, pass);
    const found = [...r.files].filter(([n, f]) => n !== CARRIER && f instanceof File);
    if (r.code !== 0 || !found.length) {
      await sleep(REVEAL_MIN_MS - (performance.now() - started));
      return show(box, "err", [strong("Nothing could be opened. "),
        "Check the passphrase, and that this is the file something was hidden in."],
        el("p", "small", "If it was sent as a photo or video in WhatsApp, Telegram or another chat app, the app " +
          "may have compressed it and deleted what was hidden. Ask for it again as a file or document."));
    }
    const msg = found.find(([n]) => n === MESSAGE);
    const rest = found.filter(([n]) => n !== MESSAGE);
    const list = el("ul", "files", ...rest.map(([name, f]) => {
      const b = el("button", "", "Download");
      b.onclick = () => download(name, f.data);
      return el("li", "", el("span", "name", name), el("span", "size", human(f.data.length)), b);
    }));
    const what = [msg && "a message", rest.length && plural(rest.length, "file")].filter(Boolean).join(" and ");
    show(box, "ok", [strong("Found "), what + "."],
      ...(msg ? [el("div", "message", new TextDecoder().decode(msg[1].data))] : []),
      ...(rest.length ? [list] : []));
  } finally {
    button.disabled = false;
  }
};
