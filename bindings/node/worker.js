import { parentPort, workerData } from "node:worker_threads";
import { createRequire } from "node:module";

const addon = createRequire(import.meta.url)(workerData.addon);
addon.load(workerData.library);
const handles = new Set();
function execute(handle, request) {
  const response = JSON.parse(addon.execute(BigInt(handle), Buffer.from(request)));
  if (!response.ok) return response;
  const parsed = JSON.parse(request);
  const op = parsed.op;
  const featureOp = op === "features" ? parsed.request.op : undefined;
  if (op === "create" || op === "open" || ["create", "open", "open_from_bytes"].includes(featureOp)) {
    handles.add(response.result.handle);
  }
  if (op === "close") handles.delete(handle);
  return response;
}
parentPort.on("message", ({ id, handle, request, shutdown }) => {
  try {
    if (shutdown) {
      let firstError;
      for (const value of handles) {
        const response = execute(value, '{"op":"close"}');
        handles.delete(value);
        if (!response.ok && !firstError) firstError = response;
      }
      parentPort.postMessage({ id, response: firstError ?? { ok: true, result: null } });
      parentPort.close();
    } else {
      // Native close consumes a handle even when close returns an engine error.
      if (JSON.parse(request).op === "close") handles.delete(handle);
      parentPort.postMessage({ id, response: execute(handle, request) });
    }
  } catch {
    // A bridge failure makes operation outcome uncertain. Do not retry it.
    parentPort.postMessage({ id, response: { ok: false, error: {
      code: "native_failure", message: "Native bridge failed; mutation outcome may be unknown",
    } } });
  }
});
