"""Test Adobe TrustMark (an AI image watermark) for Zolal's camera idea.

Usage:
  python tm_test.py mark PHOTO.jpg "zolal12"   # hide a short text, run digital tests, save results in out/
  python tm_test.py read SHOT.jpg              # read a camera photo / screenshot / received file

`mark` saves out/<name>-marked.png (the picture to show on screen, print or send), out/<name>-compare.png
(original | marked | 10x difference, to judge visibility) and prints how it survives Telegram-like
compression. `read` tries several ways (whole image, TrustMark's own picture detector, with rotation, and
centred crops) and prints what each found. Paste the printed output back into the chat.
"""
import sys
import time
from io import BytesIO
from pathlib import Path

import numpy as np
from PIL import Image

OUT = Path("out")


def load_trustmark(detector=False):
    from trustmark import TrustMark
    t = time.time()
    # Q is the variant Adobe recommends; BCH_5 error correction is the default (~61 usable bits)
    tm = TrustMark(verbose=False, model_type="Q", loadRemover=False, loadBBoxDetector=detector)
    print(f"  (TrustMark loaded in {time.time() - t:.1f}s, text capacity {tm.schemaCapacity()} bits"
          f" = about {tm.schemaCapacity() // 7} ASCII characters)")
    return tm


def jpeg(img, quality, max_side=None):
    if max_side and max(img.size) > max_side:
        s = max_side / max(img.size)
        img = img.resize((round(img.width * s), round(img.height * s)), Image.LANCZOS)
    buf = BytesIO()
    img.convert("RGB").save(buf, "JPEG", quality=quality)
    return Image.open(BytesIO(buf.getvalue())).convert("RGB")


def psnr(a, b):
    a, b = np.asarray(a, float), np.asarray(b, float)
    mse = ((a - b) ** 2).mean()
    return 99.0 if mse == 0 else 10 * np.log10(255 ** 2 / mse)


def decode(tm, img, **kw):
    try:
        secret, present, schema = tm.decode(img, MODE="text", **kw)
        return (secret if present else None), present
    except Exception as e:  # keep going: one failing method shouldn't hide the others
        return f"ERROR {type(e).__name__}: {e}", False


def cmd_mark(photo, text):
    OUT.mkdir(exist_ok=True)
    tm = load_trustmark()
    cover = Image.open(photo).convert("RGB")
    name = Path(photo).stem
    print(f"Cover: {photo} {cover.size[0]}x{cover.size[1]}, text: {text!r} ({len(text)} chars)")

    t = time.time()
    marked = tm.encode(cover, text, MODE="text")
    print(f"  encoded in {time.time() - t:.1f}s")
    marked_path = OUT / f"{name}-marked.png"
    marked.save(marked_path)

    # visibility: PSNR, and a side-by-side with the difference amplified 10x
    q = psnr(cover, marked)
    diff = np.clip(np.abs(np.asarray(cover, int) - np.asarray(marked, int)) * 10, 0, 255).astype(np.uint8)
    w, h = cover.size
    cmp = Image.new("RGB", (w * 3, h), "white")
    cmp.paste(cover, (0, 0)); cmp.paste(marked, (w, 0)); cmp.paste(Image.fromarray(diff), (2 * w, 0))
    cmp.save(OUT / f"{name}-compare.png")
    print(f"  saved {marked_path} and {OUT / (name + '-compare.png')}  (PSNR {q:.1f} dB; above ~40 is hard to see)")

    print("Digital tests (1 = found the right text):")
    tests = {
        "as saved (PNG)": marked,
        "JPEG q90": jpeg(marked, 90),
        "Telegram-like: 1280px, JPEG q80": jpeg(marked, 80, 1280),
        "harsh: 800px, JPEG q70": jpeg(marked, 70, 800),
        "very harsh: 512px, JPEG q50": jpeg(marked, 50, 512),
    }
    for label, img in tests.items():
        got, _ = decode(tm, img)
        print(f"  {'1' if got == text else '0'}  {label:34s} -> {got!r}")
    print(f"\nNow show {marked_path} on a screen (or print it), photograph it with your phone, and run:\n"
          f"  python tm_test.py read YOUR_SHOT.jpg")


def crops(img):
    w, h = img.size
    yield "whole image", img
    for f in (0.9, 0.8, 0.7, 0.6, 0.5):
        cw, ch = int(w * f), int(h * f)
        yield f"centre crop {int(f * 100)}%", img.crop(((w - cw) // 2, (h - ch) // 2, (w + cw) // 2, (h + ch) // 2))


def cmd_read(shot):
    img = Image.open(shot).convert("RGB")
    print(f"Shot: {shot} {img.size[0]}x{img.size[1]}")
    tm = load_trustmark()
    found = False
    for label, im in crops(img):
        got, present = decode(tm, im)
        print(f"  {'FOUND' if got and not str(got).startswith('ERROR') else '  -  '}  {label:18s} -> {got!r}")
        found |= bool(got) and not str(got).startswith("ERROR")
    print("With TrustMark's own picture detector (finds the marked picture inside the photo):")
    try:
        tmd = load_trustmark(detector=True)
        for kw in ({"DETECTFIRST": True}, {"DETECTFIRST": True, "ROTATION": True}):
            got, present = decode(tmd, img, **kw)
            print(f"  {'FOUND' if got and not str(got).startswith('ERROR') else '  -  '}  {str(kw):38s} -> {got!r}")
            found |= bool(got) and not str(got).startswith("ERROR")
    except Exception as e:
        print(f"  detector unavailable: {type(e).__name__}: {e}")
    print("\nResult:", "FOUND the hidden text" if found else "nothing found",
          "\nTip: if nothing was found, crop the shot to just the picture (e.g. in Preview) and run read again.")


if __name__ == "__main__":
    if len(sys.argv) >= 4 and sys.argv[1] == "mark":
        cmd_mark(sys.argv[2], sys.argv[3])
    elif len(sys.argv) == 3 and sys.argv[1] == "read":
        cmd_read(sys.argv[2])
    else:
        print(__doc__)
