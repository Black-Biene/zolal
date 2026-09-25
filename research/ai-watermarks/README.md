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
