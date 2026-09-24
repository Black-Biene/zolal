// Everything below runs locally: the shim and the wasm are this site's own files (see the CSP in index.html),
// both are loaded up front, and files only ever go into an in-memory filesystem and back out as downloads.
import { WASI, File, OpenFile, PreopenDirectory, ConsoleStdout } from "./vendor/browser_wasi_shim/index.js";

const $ = id => document.getElementById(id);
const out = $("out"), log = s => out.textContent += "\n" + s;
const bytes = f => f.arrayBuffer().then(b => new Uint8Array(b));
const wasm = fetch("zolal-cli.wasm").then(r => WebAssembly.compileStreaming(r));
wasm.then(() => out.textContent = "ready", e => out.textContent = "could not load the engine: " + e);

// Lets the page open again without a connection once it has been visited.
if ("serviceWorker" in navigator) navigator.serviceWorker.register("sw.js").catch(() => {});

async function run(args, files) {
  const dir = new PreopenDirectory(".", new Map(
    Object.entries(files).map(([n, d]) => [n, new File(d)])));
  const wasi = new WASI(["zolal-cli", ...args], [`ZOLAL_PASS=${$("pass").value}`], [
    new OpenFile(new File([])),
    ConsoleStdout.lineBuffered(log), ConsoleStdout.lineBuffered(log), dir]);
  const inst = await WebAssembly.instantiate(await wasm, { wasi_snapshot_preview1: wasi.wasiImport });
  try { wasi.start(inst); } catch (e) { log(String(e)); }
  return dir.dir.contents;
}
const save = (name, file) => {
  const a = document.createElement("a");
  a.href = URL.createObjectURL(new Blob([file.data])); a.download = name; a.click();
  setTimeout(() => URL.revokeObjectURL(a.href), 60_000);
};
const need = (...inputs) => {
  const missing = inputs.find(i => !i.files[0]);
  if (missing) out.textContent = "choose a file for: " + missing.labels[0].firstChild.textContent.trim();
  return !missing;
};

$("hide").onclick = async () => {
  if (!need($("carrier"), $("payload"))) return;
  out.textContent = "hiding…";
  const carrier = $("carrier").files[0];
  const c = await run(["hide", "carrier", "out", "secret"],
    { carrier: await bytes(carrier), secret: await bytes($("payload").files[0]) });
  if (c.get("out")) save("stego-" + carrier.name, c.get("out"));
};
$("reveal").onclick = async () => {
  if (!need($("carrier"))) return;
  out.textContent = "revealing…";
  const c = await run(["reveal", "carrier", "."], { carrier: await bytes($("carrier").files[0]) });
  for (const [n, f] of c) if (n !== "carrier") save(n, f);
};
