// TrustMark lab: a test bench for the photo mark (web/photomark/photomark.js), which the main page also uses.
// It adds what testing needs: the decoder's log, the crop that decoded, and the detector and size settings.
import { DETECTORS, QUAD, SINGLE, hideText, loadModels, measure, revealText } from "../photomark/photomark.js?v=dev";

const $ = id => document.getElementById(id);
let box = "status";  // the status line of the tab in use
const say = (kind, text) => { $(box).className = "status " + kind; $(box).textContent = text; };
const log = line => { $("log").textContent += line + "\n"; };
const MB = n => (n / 1e6).toFixed(0);

// On page load: download whatever the device doesn't have yet, with one progress bar, then prepare every model,
// so the first Hide or Reveal starts at once. The buttons stay off until then.
async function prepare() {
  const bar = $("load-bar"), text = $("load-text"), buttons = [$("m-go"), $("r-go")];
  buttons.forEach(b => { b.disabled = true; });
  try {
    const kept = await loadModels({
      detector: $("det").value,
      onProgress: p => {
        if (p.phase === "download") {
          bar.max = p.total || 1; bar.value = p.done;
          text.textContent = `Downloading the models once: ${MB(p.done)} / ${MB(p.total)} MB. Wi-Fi recommended.`;
        } else {
          bar.removeAttribute("value");  // indeterminate while the models are prepared
          text.textContent = `Preparing the models (${p.step} of ${p.steps})…`;
        }
      },
    });
    bar.hidden = true;
    text.textContent = kept
      ? "Ready. The models are saved on this device, so this works offline next time."
      : "Ready. (This browser can't keep the models, so they download again next visit.)";
    buttons.forEach(b => { b.disabled = false; });
  } catch (err) {
    bar.hidden = true;
    text.textContent = `Couldn't get the models: ${err.message}. Reload the page to try again.`;
  }
}
for (const [key, file] of Object.entries(DETECTORS)) $("det").querySelector(`[data-k="${key}"]`).value = file;

// ---- reveal: the picture (taken or chosen), the password, then the Reveal button ----------------------------

let shot = null;
for (const id of ["shoot", "pick"]) $(id).onchange = e => {
  const file = e.target.files[0];
  e.target.value = "";  // choosing the same file again should still count
  if (!file) return;
  shot = file;
  URL.revokeObjectURL($("r-thumb").src);
  $("r-thumb").src = URL.createObjectURL(file); $("r-thumb").hidden = false;
  $("status").className = "status"; $("status").textContent = ""; $("log").textContent = ""; $("crop").hidden = true;
};
$("r-go").onclick = async () => {
  box = "status";
  if (!shot) return say("err", "Take or choose the picture first.");
  const password = $("pw").value;
  $("r-go").disabled = true; $("log").textContent = ""; $("crop").hidden = true;
  try {
    const r = await revealText(shot, password, {
      detector: $("det").value, side: +$("side").value, onStatus: t => say("busy", t), onLog: log,
      onCrop: c => { $("crop").getContext("2d").drawImage(c, 0, 0); $("crop").hidden = false; },
    });
    if (r.text) say("ok", `Found: ${r.text}`);
    else if (r.locked) say("err", password
      ? "Found a mark, but this password doesn't open it."
      : "Found a mark that needs a password. Type it above and read again.");
    else say("err", "No hidden text found. Get closer so the picture fills more of the shot, and hold still.");
  } catch (err) { say("err", "Error: " + err.message); log(String(err.stack || err)); }
  $("r-go").disabled = false;
};

// ---- hide ---------------------------------------------------------------------------------------------------

$("m-go").onclick = async () => {
  box = "m-status";
  const file = $("m-photo").files[0], text = $("m-text").value, password = $("m-pw").value;
  if (!file) return say("err", "Choose a photo first.");
  if (!text) return say("err", "Type a text to hide.");
  if (!measure(text).fits) return say("err", "The text has characters that can't be hidden, or is too long.");
  if (!password) return say("err", "Choose a password.");
  $("m-result").hidden = true;
  try {
    const r = await hideText(file, text, password, { onStatus: t => say("busy", t) });
    const view = $("m-view"), k = Math.min(1, 1200 / Math.max(r.width, r.height));
    view.width = Math.round(r.width * k); view.height = Math.round(r.height * k);
    view.getContext("2d").drawImage(r.canvas, 0, 0, view.width, view.height);
    $("m-result").hidden = false;
    $("m-save").onclick = () => r.canvas.toBlob(b => {
      const a = document.createElement("a");
      a.href = URL.createObjectURL(b); a.download = (file.name.replace(/\.[^.]*$/, "") || "photo") + "-marked.png";
      a.click(); setTimeout(() => URL.revokeObjectURL(a.href), 60_000);
    }, "image/png");
    if (!r.ok) return say("err", `Made the picture, but it didn't read back (${r.readBack ?? "nothing"}). Try another photo.`);
    say("ok", `Hidden: “${text}” in ${r.width}×${r.height}${r.scaled ? " (scaled down)" : ""}` +
      `${r.quad ? ", four quarters" : ""}, ${r.ms} ms. It reads back. Now save it.`);
  } catch (err) { say("err", "Error: " + err.message); console.error(err); }
};

// How much room is left: up to 6 characters fit one mark over the whole photo (the sturdiest), up to 32 use
// four quarter marks.
$("m-text").oninput = () => {
  const { length: n, bad, fits } = measure($("m-text").value), count = $("m-count");
  count.className = "hint" + (fits ? "" : " count-bad");
  if (bad.length) return void (count.textContent = `Not allowed: ${bad.join(" ")}. Use letters, digits, space or “.”`);
  count.textContent = n <= SINGLE
    ? `${SINGLE - n} of ${SINGLE} left for one mark over the whole photo (the sturdiest). Longer texts, up to ${QUAD}, use four marks.`
    : `${QUAD - n} of ${QUAD} left. Four marks, one per quarter: any one quarter may be unreadable.`;
};
$("m-text").oninput();

const tabs = [...document.querySelectorAll('[role="tab"]')];
for (const tab of tabs) tab.onclick = () => {
  for (const u of tabs) {
    u.setAttribute("aria-selected", u === tab);
    u.tabIndex = u === tab ? 0 : -1;
    $(u.getAttribute("aria-controls")).hidden = u !== tab;
  }
};

prepare();
