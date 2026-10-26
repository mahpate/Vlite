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
  const graphCreate = op === "graph" && parsed.request.op === "create";
  if (op === "create" || op === "open" || graphCreate || ["create", "open", "open_from_bytes"].includes(featureOp)) {
    handles.add(response.result.handle);
  }
  if (op === "close") handles.delete(handle);
  return response;
}
function typedExecute(handle, request, vectors, typed, dimensions) {
  const parsed = JSON.parse(request);
  if (addon.typedAvailable()) {
    if (typed === "search_f32") {
      return JSON.parse(addon.searchF32(BigInt(handle), Buffer.from(request), vectors));
    }
    if (typed === "batch_put_f32") {
      return JSON.parse(addon.batchPutF32(BigInt(handle), Buffer.from(request), vectors));
    }
    const responses = addon.searchManyF32(BigInt(handle), Buffer.from(request), vectors, dimensions);
    const result = [];
    for (const item of responses) {
      const response = JSON.parse(item);
      if (!response.ok) return response;
      result.push(response.result);
    }
    return { ok: true, result };
  }
  if (typed === "search_f32") {
    return executeFallback(handle, { ...parsed, vector: Array.from(vectors) });
  }
  if (typed === "search_many_f32") {
    const result = [];
    for (let offset = 0; offset < vectors.length; offset += dimensions) {
      const response = executeFallback(handle, {
        ...parsed, vector: Array.from(vectors.subarray(offset, offset + dimensions)),
      });
      if (!response.ok) return response;
      result.push(response.result);
    }
    return { ok: true, result };
  }
  const operations = parsed.operations.map((operation, index) => ({
    op: "put", ...operation,
    vector: Array.from(vectors.subarray(index * parsed.dimensions, (index + 1) * parsed.dimensions)),
  }));
  return executeFallback(handle, { op: "batch", operations, ...optionalCollection(parsed.collection) });
}
function executeFallback(handle, value) {
  const request = JSON.stringify(value);
  if (Buffer.byteLength(request) > 8 * 1024 * 1024) {
    return { ok: false, error: { code: "invalid_input", message: "Request exceeds 8 MiB" } };
  }
  return execute(handle, request);
}
function optionalCollection(collection) {
  return collection === undefined ? {} : { collection };
}
parentPort.on("message", ({ id, handle, request, shutdown, typed, vectors, dimensions }) => {
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
      if (typed) {
        parentPort.postMessage({ id, response: typedExecute(handle, request, vectors, typed, dimensions) });
        return;
      }
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
