# Camera-readable hidden text (research)

Can a short, password-protected text be hidden in an ordinary-looking photo so that it can be read back by
pointing a phone camera at the photo (on a screen or on paper), and so that it survives being sent as a normal
photo through Telegram or WhatsApp? This folder holds the feasibility prototype. None of it ships in Zolal yet.

Everything here is **simulated**: the channels in `channels.py` model compression, screen or print capture,
perspective, blur, lighting, sensor noise and imprecise corner finding. They don't model moiré, a phone's own
sharpening and denoising, motion blur or glare. Real-device testing is done with the lab page in `web/lab/`.

## Method (`spread.py` + `conv.py`)

1. **Canvas.** The photo is resized to 1024 px wide; the luminance and the blue-yellow chroma (Cb) planes are
   worked on separately.
2. **Spread spectrum in Cb.** The plane is cut into 10 px cells. A key-seeded permutation gives each of the
   1024 coded bits about seven cells scattered over the whole picture, and each cell a +1/-1 chip. The mark is
   chip × bit sign per cell, bicubically upsampled and blurred so it has no block edges, and scaled by a
   texture mask so smooth areas get less.
3. **Error correction.** A rate-1/3, constraint-length-7 convolutional code with a soft-decision Viterbi
   decoder turns 1024 raw bits into 328 message bits (41 bytes).
4. **Reading.** Rectify the picture from its four corners, high-pass Cb to remove the photo itself, sum each
   bit's cells (weighted towards cell centres) times their chips, and Viterbi-decode.
5. **Corner refinement.** Rough corners are nudged one coordinate at a time (steps 8, 4, 2, 1 px) to maximise
   the total signal, the sum of |soft value| over all bits.

## Results

Four test photos, 8 random trials each per channel, rough corners off by up to ±3 px unless stated.
"Success" means the whole 41-byte message decoded exactly.

| Channel | 41 bytes (rate 1/3) | 63 bytes (rate 1/2) |
|---|---|---|
| Telegram-like: JPEG q82 | 100% | 100% |
| Telegram harsh: 800 px, JPEG q70 | 100% | 100% |
| Phone camera at a screen | 100% | 88% |
| Phone camera, far (picture 35–50% of the frame) | 94–100% | 62% |
| Printed then photographed | 97–100% | 81% |

Quality: PSNR 39.4 dB, SSIM 0.979 against the original. At normal size the difference isn't visible; up close,
large smooth areas show a very faint colour mottling.

Corner accuracy matters. Without refinement, ±6 px errors cut screen success to 91% and ±10 px to 53%. With
refinement, ±10 px gives 94–100% and ±16 px gives 94% (screen) and 56% (far).

### JavaScript version (`web/lab/`)

The lab page ports the method to the browser, with two changes: the 1024×1024 bit grid is stretched over the
photo whatever its shape (so the reader needn't know the aspect ratio), and the output's shorter side is
1024 px. Reading is staged, cheapest first, stopping as soon as a CRC-16 in the message passes: the corners as
given, then a fast half-resolution alignment, then a precise full-resolution one.

Same simulated channels, four photos (one message in Persian), four trials each, corners off by up to ±10 px:

| Channel | Decoded | Average read time (desktop Chromium) |
|---|---|---|
| Telegram-like | 16/16 | 0.1 s |
| Telegram harsh | 16/16 | 0.1 s |
| Camera at a screen | 16/16 | 0.7 s |
| Camera, far | 14/16 | 5.3 s |
| Printed | 16/16 | 2.4 s |

### What didn't work

- **Block tilt in luminance** (`block_tilt.py`): survives JPEG well (100–200 bytes) but the blocks are plainly
  visible and camera capture breaks it.
- **Spread spectrum in luminance**: invisible, but the photo's own detail is about ten times stronger than the
  mark, so 30% of bits flip even without any channel. Informed embedding (pushing only as much as needed)
  didn't converge. Chroma carries 2–4× less natural detail, which is what made it work.

## Running

```bash
python3 -m venv venv && venv/bin/pip install -r requirements.txt
# cell, strength, raw bits, code rate (2 or 3), corner error in px; runs with corner refinement
venv/bin/python run_messages.py 10,4,1024,3,3
# raw bit error rates without error correction: cell, strength, raw bits, channel (0=Y, 1=Cr, 2=Cb)
venv/bin/python run_bits.py 10,4,1024,2
```

## Open questions

- Real devices: moiré, phone image processing, motion blur, glare, printers.
- Finding the picture's corners automatically in the camera view.
- A compact encrypted format: about 7 bytes of overhead (salt and a short check) on Argon2id and ChaCha20,
  leaving about 34 bytes of text; a 6-bit Persian alphabet would fit about 45 letters.
- Detectability by steganalysis: expected to be possible for an expert, as with the rest of Zolal.
