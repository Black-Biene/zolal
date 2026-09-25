// Reading is heavy (a few hundred rectify-and-score passes), so it runs off the page's thread.
import { read } from "./wm.js?v=dev";

self.onmessage = ({ data: { img, corners } }) => {
  const started = performance.now();
  const result = read(img, corners, { onStep: step => self.postMessage({ progress: step }) });
  self.postMessage({ done: true, ...result, ms: Math.round(performance.now() - started) });
};
