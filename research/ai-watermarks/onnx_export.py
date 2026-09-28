"""Build browser-sized TrustMark reader models in models/ and check them on a camera shot.

Usage:
  python onnx_export.py                        # build models/decoder_Q.onnx and models/detector_Q_*.onnx
  python onnx_export.py check SHOT.jpg         # run the ONNX detector + decoder on a camera photo

The decoder is Adobe's own ONNX file (already fp16). The picture detector has no ONNX release, so it is
exported from the PyTorch checkpoint, then shrunk: fp16w stores weights as fp16 (maths stays fp32), int8 is
onnxruntime's dynamic quantisation. The int8 decoder was tested and dropped: it lost camera decodes.
Needs: requirements-export.txt (the Pages workflow runs this to put the models on the live lab page)
"""
import hashlib
import sys
import time
import urllib.request
from pathlib import Path

import numpy as np
from PIL import Image

M = Path("models")
DECODER_URL = "https://cai-watermark.adobe.net/watermarking/trustmark-models/decoder_Q.onnx"
DECODER_SHA256 = "ee3268f057c9dabef680e169302f5973d0589feea86189ed229a896cc3aa88df"


def build():
    import onnx
    import torch
    from onnx import TensorProto, helper, numpy_helper
    from onnxruntime.quantization import QuantType, quantize_dynamic
    from trustmark import TrustMark

    M.mkdir(exist_ok=True)
    if not (M / "decoder_Q.onnx").exists():
        urllib.request.urlretrieve(DECODER_URL, M / "decoder_Q.onnx")
    # the web page serves this file, so refuse anything but the version tested here
    if hashlib.sha256((M / "decoder_Q.onnx").read_bytes()).hexdigest() != DECODER_SHA256:
        sys.exit("decoder_Q.onnx does not match the tested version (DECODER_SHA256)")

    tm = TrustMark(verbose=False, model_type="Q", loadRemover=False, loadBBoxDetector=True, device="cpu")

    class Detector(torch.nn.Module):  # (3,H,W) in [0,1] -> boxes in pixels, scores
        def __init__(self, m):
            super().__init__()
            self.m = m

        def forward(self, x):
            d = self.m([x])[0][0]
            return d["boxes"], d["scores"]

    fp32 = M / "detector_Q_fp32.onnx"
    torch.onnx.export(Detector(tm.detector.yolo.yolo_model.eval()), (torch.rand(3, 1024, 768),), fp32,
                      input_names=["image"], output_names=["boxes", "scores"],
                      dynamic_axes={"image": {1: "h", 2: "w"}}, opset_version=17, dynamo=False)

    # onnxconverter_common's fp16 pass crashes on this graph, so store big weights as fp16 + a Cast node
    m = onnx.load(fp32)
    for t in [t for t in m.graph.initializer if t.data_type == TensorProto.FLOAT and np.prod(t.dims) > 1000]:
        m.graph.initializer.remove(t)
        m.graph.initializer.append(numpy_helper.from_array(numpy_helper.to_array(t).astype(np.float16), t.name + "_h"))
        m.graph.node.insert(0, helper.make_node("Cast", [t.name + "_h"], [t.name], to=TensorProto.FLOAT))
    onnx.save(m, M / "detector_Q_fp16w.onnx")
    quantize_dynamic(fp32, M / "detector_Q_int8.onnx", weight_type=QuantType.QInt8)
    for f in sorted(M.glob("*.onnx")):
        print(f"{f.stat().st_size / 1e6:6.1f} MB  {f}")


def check(shot):
    import onnxruntime as ort
    from trustmark import TrustMark

    ecc = TrustMark(verbose=False, model_type="Q", loadRemover=False, device="cpu").ecc
    dec = ort.InferenceSession(M / "decoder_Q.onnx", providers=["CPUExecutionProvider"])
    full = Image.open(shot).convert("RGB")
    for f in ["detector_Q_fp32.onnx", "detector_Q_fp16w.onnx", "detector_Q_int8.onnx"]:
        det = ort.InferenceSession(M / f, providers=["CPUExecutionProvider"])
        for side in [1600, 1024, 640]:  # longest edge fed to the detector
            s = side / max(full.size)
            im = full.resize((round(full.width * s), round(full.height * s)), Image.BILINEAR)
            t = time.time()
            boxes, scores = det.run(None, {"image": np.asarray(im, np.float32).transpose(2, 0, 1) / 255})
            ms = (time.time() - t) * 1000
            found = None
            for b in boxes:  # like TrustMark's DETECTFIRST: try each box until one decodes
                crop = full.crop(tuple(int(v / s) for v in b)).resize((256, 256), Image.BILINEAR)
                x = (np.asarray(crop, np.float32) / 255 * 2 - 1).transpose(2, 0, 1)[None]
                text, ok, _ = ecc.decode_bitstream(dec.run(None, {"image": x})[0] > 0, "text")[0]
                if ok:
                    found = text
                    break
            print(f"{f:24} {side:5}px {ms:5.0f} ms  boxes={len(boxes)}  -> {found}")


if __name__ == "__main__":
    check(sys.argv[2]) if sys.argv[1:2] == ["check"] else build()
