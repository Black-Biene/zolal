# AI watermark tests

Can an existing AI watermark carry a short Zolal text that a phone camera can read back from a screen or a
print? This folder holds a test kit for [Adobe TrustMark](https://github.com/adobe/trustmark) (MIT licence,
100-bit payload, about 8 ASCII characters of text with its default error correction). It runs on a laptop;
the model files download on first use.

## Setup (macOS / Linux)

```bash
cd research/ai-watermarks
python3 -m venv venv && source venv/bin/activate
pip install trustmark pillow numpy      # also installs PyTorch (a large download, CPU is fine)
```

## Test

```bash
# 1. hide a short text (keep it to about 8 characters) in a busy photo; runs digital tests too
python tm_test.py mark photo.jpg "zolal123"

# 2. open out/photo-marked.png on the laptop screen, take a photo of it with your phone's camera app,
#    copy that photo to the laptop, then:
python tm_test.py read IMG_1234.jpg
```

`out/photo-compare.png` shows original | marked | difference ×10, to judge visibility. Try one busy photo
(street, trees) and one plain picture (a diagram) to see both cases. Paste the printed output into the chat.

## Results so far

First run on a MacBook, with an iPhone photo (3024×4032) of the marked picture on the laptop screen:

| Test | Result |
|---|---|
| Visibility | PSNR 41.2 dB (750×562 photo) |
| Saved PNG, JPEG q90, 1280 px q80, 800 px q70, 512 px q50 | all read `zolal123` |
| iPhone shot, whole image or centred crops | nothing found |
| iPhone shot with TrustMark's picture detector (`DETECTFIRST`), with and without rotation | read `zolal123` |

The detector model (`trustmark_bbox_Q`) is what makes camera reading work: it finds the marked picture inside
the shot, which was the unsolved part of our own method. Capacity is 61 bits with the default error
correction (8 ASCII characters).

Still to test: distance, angle, a picture sent through Telegram as a normal photo, a plain diagram, print.

## Password layer

`python tm_test.py mark photo.jpg "Hi.42" PASSWORD` seals the text (`tmseal.py`; `web/lab/tmseal.js` opens it
in the page, which has a password field). The 61 bits: nonce 9 | ciphertext 36 | tag 16.

- Text: up to 6 characters of `a–z A–Z 0–9 space .` (6 bits each).
- Key: PBKDF2-SHA256, 600,000 rounds, salt `zolal-tm1` + nonce (built into WebCrypto and Python, no extra
  library; the main site's Argon2id would need one on both sides). Keystream and tag: HMAC-SHA256 with that key.
- Tag: rejects a wrong password, and random bits that happen to pass BCH (1 in 65,536). It replaces the
  printable-ASCII stopgap for sealed marks; marks without a password still read as before.
- Checked: 40 Python-sealed payloads open in JS (incl. Persian and accented passwords), 40 wrong passwords
  rejected; in the browser a sealed picture reads with the right password, says "this password doesn't open
  it" with a wrong one (stops after the first real mark instead of retrying) and "needs a password" with none.

Limits, stated plainly: anyone can tell a picture carries a TrustMark mark (the bits are public; only the text
is secret). A stolen picture can be attacked offline: each password guess costs one PBKDF2 run and the tag
filters wrong ones, so only a strong password protects 6 characters. The 9-bit nonce means two marks made
with the same password share a keystream 1 time in 512, so use a new password per picture for anything real.

## Hiding in the browser

The lab page's *Hide* tab does what `tm_test.py mark PHOTO TEXT PASSWORD` does, on the phone: seal
(`tmseal.js`), BCH-encode (`bch.js`, equal to Python on 2000/2000 random payloads), Adobe's `encoder_Q.onnx`
(17 MB) at 256×256, then TrustMark's post-processing in JS (residual minus its colour shift, bilinear
upscale, 1% edge fade). It reads the result back before offering the PNG. On the owner's 12 MP photo:
0.6 s on the MacBook, PSNR 39.4 dB (at 1200 px), and a 1200 px JPEG copy read back at quality 90, 70 and 50.
Photos above 16 MP are scaled down first (iOS canvas limit).

## Longer texts: four quarters with a spare

One mark holds 100 bits, so 6 characters with a password. For more, the page's *Hide* tab splits the photo
2×2 (0 top-left, 1 top-right, 2 bottom-left, 3 bottom-right) and marks each quarter separately with BCH_3
(fixes 3 wrong bits instead of 5; on real camera shots good crops had 0–1 wrong bits): 75 bits = the
quarter's index (2) + 73. Quarters 0–2 hold a 219-bit sealed payload, **32 characters**; quarter 3 holds their
XOR, so any one quarter may be unreadable. Texts of up to 6 characters still use one mark (sturdier).

- First test (plain texts, `out/myphoto-tiled.png`): a hand crop read 3 of 4 quarters from a camera shot and
  from a Telegram copy; the top-right quarter (plain bright table, strong moiré) read in neither. An automatic
  search over the detector's boxes also found 3 of 4 in both.
- The detector gets confused by four marks (it reports halves or single quarters), so the reader tries the
  union of all boxes and every box as the whole picture split in four, and every box as one quarter.
- BCH_3 lets ~8% of random words through, so each index keeps several candidates and the password tag
  picks the combination. When all four quarters agree (quarter 3 = XOR of 0–2, 73 bits) a failed tag means a
  wrong password, and the reader stops.
- `bch.js` BCH_3 = Python on 1000/1000 random payloads (and BCH_5 still 1000/1000); `tmseal` holds 32
  characters in 219 bits in both languages.
- In the browser (MacBook): hiding 32 characters in a 12 MP photo 1.8 s, reads back; a 1200 px JPEG copy
  reveals it after 23 decodes; a wrong password is reported after 24. Single marks (plain and sealed) still read.

## Browser-sized models (ONNX)

`python onnx_export.py` builds the reader models into `models/` (git-ignored; `pip install onnx onnxruntime`
first), and `python onnx_export.py check SHOT.jpg` runs them on a camera photo. Results on the iPhone shot
(laptop CPU, onnxruntime):

| Model | Size | Result |
|---|---|---|
| Decoder, Adobe's ONNX (already fp16) | 47 MB | same bits as PyTorch (0.1 of 100 differ on average), ~9 ms |
| Decoder, int8 | 24 MB | dropped: 5 bits differ on average, camera crops decoded 27/61 vs 32/61 |
| Detector fp32 (exported, no Adobe ONNX exists) | 166 MB | box identical to PyTorch |
| Detector, fp16 weights | 83 MB | reads `zolal123` at 1600, 1024 and 640 px input |
| Detector, int8 | 42 MB | reads `zolal123` at 1600, 1024 and 640 px input (640 px: ~0.1 s) |

Smallest working reader: int8 detector + fp16 decoder = **89 MB**, a one-time download. Only one real camera
shot so far, so the int8 detector needs the other camera tests before it is trusted.

In the browser (`web/lab/tm.html`, onnxruntime-web 1.30, one thread, MacBook Chrome, 640 px): the fp16
detector takes ~2.8 s and the int8 one ~7.3 s (WebAssembly has no fast int8 path, so fp16 is the default). Both
read `zolal123`, but only after nudging the box: the browser's resize moves it ~1% from Python's, enough to
break the decode, so the page also tries it shrunk 1–2%, grown 1% and shifted 1% (up to 20 decoder runs,
~0.18 s each).

**First iPhone run (Safari, fp16 detector, 640 px, new photo of the marked picture on the laptop screen):**
read `zolal123`. Models ready in 1.7 s + 0.2 s after download, detector 4.1 s, decoded on the first try in
0.3 s: about 4.5 s per read.

**Camera shot sent through Telegram as a normal photo** (`samples/telegram-camera.jpg`, 960×1280): Adobe's
PyTorch `read` finds nothing, but the ONNX pipeline reads `zolal123` (8/9 in `onnx_export.py check`; only the
int8 detector at 640 px misses), and the browser page reads it with every detector and size (at 640 px on the
11th–12th nudged try).

**From across the room** (`samples/distance.jpg`, picture ~680×830 px of a 4032×3024 shot, sideways): no
read. The mark itself survives (a hand-made crop, turned upright, decodes in 130/625 nudged variants), but the
detector's box is ~10% loose. Re-detecting inside the rough box ("zoom") fixed it at 1024 px in Python. Decision:
scan like a QR code, close up; long distance is not a goal, so the zoom step is not in the page.

**Normal distance, slightly from above** (`samples/fail-8843.jpg`, picture ~57% of the shot's width): the live
page failed on the iPhone. The mark was fine (5501/6561 crops around the true edges decode), but the
detector's box took in part of the window below the picture: ~18% too tall in Python, ~30% too wide in the
browser. So the page now zooms by default: it detects again inside the first box plus a 10% margin, where the
picture fills the view. In the browser the zoomed box decoded this shot on the first try, and the three
close-up shots on the first or second try. Cost: a second detector run, so a read takes ~6 s on the MacBook.
The page also skips the other rotations when the upright view found the picture with score ≥ 0.9 (the
failed read above spent ~60 s on them).

**False reads:** with dozens of tries per shot, random bits sometimes pass BCH_5 (the zoom test printed
`]w +D]U` and `CzekuS;AX`). `web/lab/bch.js` now also demands bits 56–63 zero (texts of up to 8 characters)
and printable ASCII, and ignores the 4 unprotected version bits: 3000/3000 damaged real marks (0–5 bad bits)
still decode exactly; random words accepted: 7 in 2 million. The password layer's check value must replace
this stopgap.

The decoder needs a tight crop: with each edge of the detector's box moved randomly by up to 2% of its size,
20/30 crops decode; at 6%, 13/30. A hand-held guide frame alone won't replace the detector.
