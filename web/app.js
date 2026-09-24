// Everything below runs locally: the shim and the wasm are this site's own files (see the CSP in index.html),
// both are loaded up front, and files only ever go into an in-memory filesystem and back out as downloads.
import { WASI, File, OpenFile, PreopenDirectory, ConsoleStdout } from "./vendor/browser_wasi_shim/index.js";

const $ = id => document.getElementById(id);
const wasm = fetch("zolal-cli.wasm").then(r => WebAssembly.compileStreaming(r));
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
  const update = () => {
    const files = [...input.files];
    drop.classList.toggle("filled", files.length > 0);
    hint.textContent = !files.length ? empty
      : files.length === 1 ? `${files[0].name} · ${human(files[0].size)}`
      : `${files.length} files · ${human(files.reduce((s, f) => s + f.size, 0))}`;
  };
  input.addEventListener("change", update);
  drop.addEventListener("dragover", e => { e.preventDefault(); drop.classList.add("over"); });
  drop.addEventListener("dragleave", () => drop.classList.remove("over"));
  drop.addEventListener("drop", e => {
    e.preventDefault(); drop.classList.remove("over");
    if (!e.dataTransfer.files.length) return;
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

// ---- hide ----------------------------------------------------------------------------------------------

const VERDICT = {
  Natural: ["ok", "The size change looks normal for this kind of file."],
  Noticeable: ["warn", "It's noticeably bigger than a typical file like it. Fine for a casual look; a larger " +
    "cover file would hide it better."],
  Suspicious: ["warn", "It's much bigger than a normal file like it, so someone comparing sizes would notice. " +
    "Try a larger cover file, or a video."],
};

$("hide").onclick = async () => {
  const box = $("hide-status"), carrier = $("hide-carrier").files[0], payloads = [...$("hide-payload").files];
  const pass = $("hide-pass").value;
  if (!carrier) return show(box, "err", "Choose a cover file first.");
  if (!payloads.length) return show(box, "err", "Choose at least one file to hide.");
  if (!pass) return show(box, "err", "Enter a passphrase.");

  const button = $("hide");
  button.disabled = true;
  busy(box, "Encrypting and hiding…");
  await paint();
  try {
    const names = uniqueNames(payloads);
    const files = { [CARRIER]: await bytes(carrier) };
    for (let i = 0; i < payloads.length; i++) files[names[i]] = await bytes(payloads[i]);
    const r = await run(["hide", CARRIER, OUTPUT, ...names.map(n => "./" + n)], files, pass);
    const out = r.files.get(OUTPUT);
    if (r.code !== 0 || !out) {
      const msg = r.stderr.filter(l => l.startsWith("error: ")).map(l => l.slice(7)).join(" ") || "Something went wrong.";
      return show(box, "err", [strong("Couldn't hide the files. "), msg[0].toUpperCase() + msg.slice(1)]);
    }
    download(carrier.name, out.data);

    const m = r.stdout.join("\n").match(/size .+ -> .+ \(x[\d.]+, (\w+)\)/);
    const [kind, note] = VERDICT[m?.[1]] ?? VERDICT.Natural;
    const again = el("button", "again", `Download ${carrier.name} again`);
    again.onclick = () => download(carrier.name, out.data);
    show(box, kind,
      [strong("Done. "), `${payloads.length === 1 ? "Your file is" : `${payloads.length} files are`} hidden in ` +
        `${carrier.name}, which has been downloaded.`],
      el("p", "small", `${human(carrier.size)} → ${human(out.data.length)}. ${note}`),
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
    const list = el("ul", "files", ...found.map(([name, f]) => {
      const b = el("button", "", "Download");
      b.onclick = () => download(name, f.data);
      return el("li", "", el("span", "name", name), el("span", "size", human(f.data.length)), b);
    }));
    show(box, "ok", [strong("Found "), `${found.length} file${found.length > 1 ? "s" : ""}.`], list);
  } finally {
    button.disabled = false;
  }
};
