#!/usr/bin/env python3
"""Independent, standard-library REST differential check.

python -m scripts.rest_differential_check self-test
python -m scripts.rest_differential_check run --candidate HEAD

Only archived commits are built. Requests are driven by the reference contract;
the candidate implementation and tests are never used to generate expectations.
"""

from __future__ import annotations

import argparse
import base64
import contextlib
import copy
import hashlib
import http.client
import http.server
import json
import os
from pathlib import Path
import queue
import re
import socket
import struct
import subprocess
import sys
import tarfile
import tempfile
import threading
import time
import traceback
import urllib.parse
import zlib

ROOT = Path(__file__).resolve().parents[1]
METHODS = {"get", "post", "put", "patch", "delete", "head", "options"}
ADMIN = "rest_diff_admin_012345678901234567890123456789"
READ = "rest_diff_read_0123456789012345678901234567890"
PASSWORD = "Rest-diff-password-0123456789!"
VECTOR = [1.0, 0.0, 0.0, 0.0]
ALLOWLIST = {
    "GET /v1/capabilities /body/features/kv": "New optional KV capability; no legacy capability changes.",
    **{f"GET /v1/capabilities /body/routes +{route}":
       "New KV route advertised in addition to the unchanged ordered legacy routes."
       for route in ("/v1/collections/{collection}/kv/entries",
                     "/v1/collections/{collection}/kv/entry")},
}
# Only the explicitly named additions are accepted, never removals/value changes.
# OpenAPI schema tightenings reviewed and accepted for 0.7.0 (release notes,
# Compatibility): rerank.lambda is valid only with mode "mmr", and a custom mode
# needs a provider. The 0.6.1 server already refused every request these
# conditions exclude with 422 invalid_input, which the differential's negative
# case confirms. Matching is exact on the issue text; anything else still fails.
ACCEPTED_ISSUES = {
    f"{where} new constraint needs review":
        "Conditional rerank constraints (lambda only with mmr, provider only with custom) restate runtime rules."
    for where in (
        "/paths/~1v1~1collections~1{c}~1search/post/requestBody/content/application~1json/schema/properties/rerank/allOf",
        "/paths/~1v1~1search/post/requestBody/content/application~1json/schema/properties/rerank/allOf",
        "/components/schemas/SearchRequest/properties/rerank/allOf",
        "/components/schemas/RerankOptions/allOf",
    )
}
# Reviewed and accepted for 0.7.1 (release notes, Fixed; docs/API.md, Record
# expiry and metadata): the record put `ttl_ms` minimum is 1. A 0.7.0 server
# already failed every `ttl_ms` of 0 (500 after a write), so the tightening only
# turns that failure into a 422 refusal. Matching is exact on the issue text.
ACCEPTED_ISSUES.update({
    f"{where}/properties/ttl_ms/minimum lower bound raised":
        "Record put ttl_ms is at least 1 (0 was a 500 after a write in 0.7.0, a 422 with no write since 0.7.1)."
    for where in (
        "/paths/~1v1~1collections~1{c}~1vectors~1{id}/put/requestBody/content/application~1json/schema",
        "/paths/~1v1~1vectors~1{id}/put/requestBody/content/application~1json/schema",
        "/components/schemas/Put",
    )
})
VOLATILE = {
    "created_unix_ms", "updated_unix_ms", "last_login_unix_ms",
    "suspended_unix_ms", "expires_unix_ms", "timestamp_ms", "unix_ms",
    "uptime_seconds", "request_duration_seconds_total",
    "hnsw_rebuild_and_search_seconds_total", "elapsed_ms", "duration_ms",
    "started_unix_ms", "finished_unix_ms", "last_success_unix_ms",
    "last_attempt_unix_ms", "last_run_unix_ms", "next_run_unix_ms",
    "version", "server_version", "request_id",
    "verified_unix_ms", "revoked_unix_ms", "timestamp_unix_ms", "ts_ms",
    "duration_ns", "last_tick_unix_ms", "uptime_s", "max_us", "p50_us",
    "p99_us", "p50_ms", "p95_ms", "p99_ms",
    "backup_drill_last_success_unix_ms", "backup_last_success_unix_ms",
    "hnsw_last_build_seconds",
}
HEADERS = {
    "content-type", "allow", "www-authenticate", "retry-after", "cache-control",
    "content-security-policy", "x-content-type-options", "x-frame-options",
    "referrer-policy", "permissions-policy", "strict-transport-security",
    "access-control-allow-origin", "access-control-allow-methods",
    "access-control-allow-headers", "vary", "location", "etag", "set-cookie",
}


def command(args, cwd=ROOT, env=None):
    result = subprocess.run(args, cwd=cwd, env=env, capture_output=True,
                            text=True, encoding="utf-8", errors="replace")
    if result.returncode:
        raise RuntimeError(f"{args!r}: exit {result.returncode}\n"
                           f"{result.stdout[-3000:]}\n{result.stderr[-6000:]}")
    return result.stdout.strip()


def resolve(rev):
    return command(["git", "rev-parse", "--verify", f"{rev}^{{commit}}"])


def extract(sha, work):
    source = work / f"src-{sha}"
    stamp = source / ".archive-sha"
    if stamp.exists() and stamp.read_text().strip() == sha:
        return source
    if source.exists():
        raise RuntimeError(f"Incomplete extraction at {source}; choose a fresh --work")
    source.mkdir()
    archive = work / f"{sha}.tar"
    try:
        with archive.open("wb") as stream:
            subprocess.run(["git", "archive", "--format=tar", sha], cwd=ROOT,
                           stdout=stream, check=True)
        with tarfile.open(archive) as stream:
            stream.extractall(source, filter="data")
        stamp.write_text(sha, encoding="ascii")
    finally:
        archive.unlink(missing_ok=True)
    return source


def build(sha, work):
    source = extract(sha, work)
    target = work / f"target-{sha}"
    env = dict(os.environ, CARGO_INCREMENTAL="0", CARGO_NET_OFFLINE="true",
               CARGO_TARGET_DIR=str(target))
    print(f"Building {sha} (locked, offline, release)", flush=True)
    command(["cargo", "build", "--locked", "--offline", "--release",
             "--features", "server", "--bin", "vlite-server"], source, env)
    return source, target / "release" / ("vlite-server.exe" if os.name == "nt" else "vlite-server")


@contextlib.contextmanager
def server(binary, work):
    with tempfile.TemporaryDirectory(prefix="server-", dir=work) as temporary:
        folder = Path(temporary)
        token = folder / "admin-token"
        token.write_text(ADMIN, encoding="ascii")
        password = folder / "password"
        password.write_text(PASSWORD, encoding="ascii")
        admin_dir = folder / "admin"
        admin_dir.mkdir()
        env = {k: v for k, v in os.environ.items()
               if not k.startswith(("VLITE_", "OTEL_"))}
        env.update(VLITE_ADMIN_DIR=str(admin_dir), VLITE_READ_ONLY_TOKEN=READ,
                   VLITE_ADMIN_USERNAME="restadmin",
                   VLITE_ADMIN_PASSWORD_FILE=str(password))
        lines = queue.Queue()
        with (folder / "server.log").open("w+", encoding="utf-8") as log:
            process = subprocess.Popen(
                [str(binary), str(folder / "database.vlite"),
                 "--create-if-missing", "4", "--bind", "127.0.0.1:0",
                 "--token-file", str(token)], env=env, stdout=subprocess.PIPE,
                stderr=log, text=True, encoding="utf-8", errors="replace")

            def read_stdout():
                for line in process.stdout:
                    lines.put(line)

            reader = threading.Thread(target=read_stdout, daemon=True)
            reader.start()
            try:
                deadline = time.monotonic() + 90
                while time.monotonic() < deadline:
                    if process.poll() is not None:
                        log.flush()
                        raise RuntimeError(f"Server exited {process.returncode}: "
                                           + (folder / "server.log").read_text())
                    try:
                        line = lines.get(timeout=0.1)
                    except queue.Empty:
                        continue
                    match = re.match(r"LISTENING http://127\.0\.0\.1:(\d+)", line)
                    if match:
                        port = int(match[1])
                        response = request(port, "GET", "/health")
                        if response["status"] != 200:
                            raise RuntimeError(f"Health probe failed: {response}")
                        yield port
                        break
                else:
                    raise RuntimeError("Server did not emit LISTENING within 90 seconds")
            finally:
                if process.poll() is None:
                    process.terminate()
                    try:
                        process.wait(timeout=10)
                    except subprocess.TimeoutExpired:
                        process.kill()
                        process.wait(timeout=10)
                reader.join(timeout=5)
                process.stdout.close()


def request(port, method, path, body=None, token=None, extra=None):
    headers = {"Content-Type": "application/json", "Connection": "close"}
    if token:
        headers["Authorization"] = f"Bearer {token}"
    headers.update(extra or {})
    payload = body if isinstance(body, bytes) else (
        json.dumps(body, separators=(",", ":")).encode() if body is not None else None)
    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=30)
    try:
        connection.request(method, path, body=payload, headers=headers)
        response = connection.getresponse()
        raw = response.read()
        pairs = response.getheaders()
        meaningful = {}
        for key, value in pairs:
            if key.lower() in HEADERS:
                meaningful.setdefault(key.lower(), []).append(value)
        try:
            decoded = json.loads(raw)
            kind = "json"
        except (ValueError, UnicodeDecodeError):
            decoded = raw.decode("utf-8", errors="replace")
            kind = "text"
        return {"status": response.status, "headers": meaningful,
                "kind": kind, "body": decoded}
    finally:
        connection.close()


def pointer(path):
    return "/" + "/".join(str(p).replace("~", "~0").replace("/", "~1") for p in path)


class Normalizer:
    def __init__(self):
        self.replacements = {}
        self.groups = {}

    def bind(self, value, group):
        if value is None:
            return
        text = str(value)
        if text not in self.replacements:
            self.groups[group] = self.groups.get(group, 0) + 1
            self.replacements[text] = f"<{group}:{self.groups[group]}>"

    def text(self, value):
        for raw in sorted(self.replacements, key=len, reverse=True):
            # Decimal record IDs are never bound; generated numeric jobs are
            # replaced only as complete values to avoid corrupting numbers.
            if value == raw:
                return self.replacements[raw]
            if len(raw) >= 8:
                value = value.replace(raw, self.replacements[raw])
        return value

    def body(self, value, path=()):
        key = str(path[-1]) if path else ""
        if (value is not None and key in VOLATILE
                and not ({"metadata", "properties"} & set(path))
                and (key != "version" or len(path) == 1)):
            return {"volatile_type": type(value).__name__}
        if key == "crc32" and "manifest" in path and value is not None:
            return {"snapshot_checksum_type": type(value).__name__}
        if key == "segment_base64" and isinstance(value, str):
            return self.segment(value)
        if key == "message" and isinstance(value, str) and value.startswith("{"):
            try:
                message = json.loads(value)
            except ValueError:
                pass
            else:
                return {"json_log_message": self.body(message, (*path, "json_log"))}
        if value is not None and key in {"cursor", "next_cursor"} and isinstance(value, str):
            return {"cursor_type": "string", "length": len(value),
                    "encoding": "hex" if re.fullmatch("[0-9a-f]+", value) else "opaque"}
        if isinstance(value, dict):
            return {k: self.body(v, (*path, k)) for k, v in sorted(value.items())}
        if isinstance(value, list):
            return [self.body(v, (*path, i)) for i, v in enumerate(value)]
        if isinstance(value, str):
            return self.text(value)
        return value

    def segment(self, encoded):
        data = base64.b64decode(encoded, validate=True)
        if len(data) < 84 or data[:8] != b"VLSEG001":
            raise RuntimeError("Unsupported replication segment envelope")
        lineage, base, last, head, header_length, frame_length = struct.unpack_from("<QQQQIQ", data, 8)
        end = 52 + header_length + frame_length
        if len(data) != end + 32:
            raise RuntimeError("Replication segment length mismatch")
        header = data[52:52 + header_length]
        frames = data[52 + header_length:end]
        digest_input = data[:44] + header + data[44:52] + frames
        if hashlib.sha256(digest_input).digest() != data[end:]:
            raise RuntimeError("Replication SHA-256 verification failed")
        if header_length != 32 or header[:8] != b"VLITEDB\0":
            raise RuntimeError("Unsupported database header in replication segment")
        if zlib.crc32(header[:28]) != struct.unpack_from("<I", header, 28)[0]:
            raise RuntimeError("Replication file-header CRC mismatch")
        normalized = []
        offset = 0
        while offset < len(frames):
            frame = frames[offset:offset + 36]
            if len(frame) != 36 or frame[:4] != b"VREC":
                raise RuntimeError("Invalid replication frame header")
            if zlib.crc32(frame[:32]) != struct.unpack_from("<I", frame, 32)[0]:
                raise RuntimeError("Replication frame-header CRC mismatch")
            payload_length = struct.unpack_from("<I", frame, 28)[0]
            # This fixed exchange creates v5 op-4 records and v5 metadata,
            # collection, vector-state and graph frames (full-body lengths).
            if frame[4] == 1:
                payload_length += 4 * struct.unpack_from("<I", frame, 24)[0]
            commit = offset + 36 + payload_length
            footer = frames[commit:commit + 16]
            if len(footer) != 16 or footer[:4] != b"VCMT":
                raise RuntimeError("Invalid replication commit footer")
            if zlib.crc32(frames[offset:commit]) != struct.unpack_from("<I", footer, 4)[0]:
                raise RuntimeError("Replication commit CRC mismatch")
            body = frames[offset + 36:commit]
            for raw in sorted(self.replacements, key=len, reverse=True):
                if len(raw) >= 8:
                    body = body.replace(raw.encode(), self.replacements[raw].encode())
            normalized.append({"header": frame[:32].hex(), "body": body.hex(),
                               "sequence_footer": footer[8:].hex()})
            offset = commit + 16
        return {"magic": "VLSEG001", "lineage": self.text(f"{lineage:016x}"),
                "base": base, "last": last, "head": head,
                "database_header": (header[:16] + header[24:28]).hex(),
                "frames": normalized, "integrity": "SHA-256 and all CRCs verified"}

    def prometheus(self, text):
        lines = []
        for line in text.splitlines():
            parts = line.split()
            name = parts[0].removeprefix("vlite_") if parts else ""
            if len(parts) == 2 and name in VOLATILE:
                float(parts[1])
                line = parts[0] + " <volatile-number>"
            lines.append(line)
        return "\n".join(lines)

    def response(self, response):
        result = copy.deepcopy(response)
        if result["kind"] == "json":
            result["body"] = self.body(result["body"])
        elif result["body"].startswith("# TYPE vlite_"):
            result["body"] = self.text(self.prometheus(result["body"]))
        else:
            result["body"] = self.text(result["body"])
        result["headers"] = {k: [self.text(v) for v in values]
                             for k, values in result["headers"].items()}
        return result


def differences(base, candidate, path=()):
    found = []
    if type(base) is not type(candidate):
        return [{"path": pointer(path), "reference": base, "candidate": candidate,
                 "change": "type"}]
    if isinstance(base, dict):
        for key in sorted(base.keys() | candidate.keys()):
            at = (*path, key)
            if key not in candidate:
                found.append({"path": pointer(at), "reference": base[key],
                              "candidate": "<absent>", "change": "removed"})
            elif key not in base:
                found.append({"path": pointer(at), "reference": "<absent>",
                              "candidate": candidate[key], "change": "added"})
            else:
                found.extend(differences(base[key], candidate[key], at))
    elif isinstance(base, list):
        if len(base) != len(candidate):
            found.append({"path": pointer(path), "reference": base,
                          "candidate": candidate, "change": "array length"})
        else:
            for i, (left, right) in enumerate(zip(base, candidate)):
                found.extend(differences(left, right, (*path, i)))
    elif base != candidate:
        found.append({"path": pointer(path), "reference": base,
                      "candidate": candidate, "change": "value"})
    return found


def document(source):
    spec = json.loads((source / "src" / "openapi.json").read_text(encoding="utf-8"))
    graph = source / "src" / "openapi_graph.json"
    if graph.exists():
        extra = json.loads(graph.read_text(encoding="utf-8"))
        spec["paths"].update(extra["paths"])
        spec.setdefault("components", {}).setdefault("schemas", {}).update(
            extra.get("components", {}).get("schemas", {}))
    return spec


def operations(spec):
    return {(method.upper(), path): value
            for path, methods in spec["paths"].items()
            for method, value in methods.items() if method in METHODS}


def compatibility(base, candidate):
    """Conservative structural comparison, including referenced component schemas."""
    issues, additions = [], []
    ignored = {"description", "summary", "example", "examples", "title",
               "operationId", "externalDocs", "tags"}
    input_schemas = set()

    def references(value):
        if isinstance(value, dict):
            ref = value.get("$ref", "")
            if ref.startswith("#/components/schemas/"):
                name = ref.split("/")[-1]
                if name not in input_schemas:
                    input_schemas.add(name)
                    references(base.get("components", {}).get("schemas", {}).get(name, {}))
            for item in value.values():
                references(item)
        elif isinstance(value, list):
            for item in value:
                references(item)

    for operation in operations(base).values():
        references(operation.get("requestBody", {}))
        references(operation.get("parameters", []))

    def expand(value, root, seen=()):
        if isinstance(value, dict):
            ref = value.get("$ref")
            if ref and ref.startswith("#/") and ref not in seen:
                target = root
                for part in ref[2:].split("/"):
                    target = target[part.replace("~1", "/").replace("~0", "~")]
                return expand({**target, **{k: v for k, v in value.items() if k != "$ref"}},
                              root, (*seen, ref))
            return {k: expand(v, root, seen) for k, v in value.items() if k not in ignored}
        if isinstance(value, list):
            return [expand(v, root, seen) for v in value]
        return value

    def walk(left, right, path=()):
        if left == right:
            return
        if isinstance(left, dict) and isinstance(right, dict) and "enum" in left and "allOf" in right:
            def accepts(value, schema):
                if schema.keys() - {"enum", "const", "type", "minLength", "maxLength",
                                    "pattern", "allOf", "default"}:
                    return False
                if "enum" in schema and value not in schema["enum"]:
                    return False
                if "const" in schema and value != schema["const"]:
                    return False
                if "type" in schema and schema["type"] == "string" and not isinstance(value, str):
                    return False
                if isinstance(value, str):
                    if len(value) < schema.get("minLength", 0) or len(value) > schema.get("maxLength", float("inf")):
                        return False
                    if "pattern" in schema and not re.search(schema["pattern"], value):
                        return False
                return all(accepts(value, item) for item in schema.get("allOf", []))

            if all(accepts(value, right) for value in left["enum"]):
                additions.append(f"{pointer(path)} widened finite enum (all legacy values checked)")
                return
        if isinstance(left, dict) and isinstance(right, dict) and "anyOf" in right:
            branches = right["anyOf"]
            if left in branches:
                if len(branches) > 1:
                    additions.append(f"{pointer(path)} additional response/schema alternatives")
                return
            if "anyOf" in left and all(branch in branches for branch in left["anyOf"]):
                additions.append(f"{pointer(path)} additional response/schema alternatives")
                return
        if isinstance(left, dict) and isinstance(right, dict):
            for key, value in left.items():
                if key in ignored:
                    continue
                at = (*path, key)
                if key not in right:
                    # Dropping an input constraint is widening, not narrowing.
                    if key == "required" and "responses" in path:
                        issues.append(f"{pointer(at)} required response guarantees removed")
                    elif key not in {"required", "enum", "minimum", "maximum",
                                   "minLength", "maxLength", "minItems", "maxItems",
                                   "pattern", "additionalProperties"}:
                        issues.append(f"{pointer(at)} removed")
                    continue
                other = right[key]
                if key == "required" and isinstance(value, bool):
                    if not value and other:
                        issues.append(f"{pointer(at)} input now required")
                elif key == "required" and isinstance(value, list):
                    response = "responses" in path or (
                        path[:2] == ("components", "schemas") and len(path) > 2
                        and path[2] not in input_schemas)
                    if response and not set(value) <= set(other):
                        issues.append(f"{pointer(at)} required guarantees removed")
                    if not response and not set(other) <= set(value):
                        issues.append(f"{pointer(at)} new required input")
                elif key == "enum" and isinstance(value, list):
                    if not all(item in other for item in value):
                        issues.append(f"{pointer(at)} enum narrowed")
                    for item in other:
                        if item not in value:
                            additions.append(f"{pointer(at)} +{item!r}")
                elif key == "type" and isinstance(value, (str, list)):
                    old_types = value if isinstance(value, list) else [value]
                    new_types = other if isinstance(other, list) else [other]
                    if not set(old_types) <= set(new_types):
                        issues.append(f"{pointer(at)} type narrowed")
                elif key in {"minimum", "exclusiveMinimum", "minItems", "minLength", "minProperties"}:
                    if other > value:
                        issues.append(f"{pointer(at)} lower bound raised")
                elif key in {"maximum", "exclusiveMaximum", "maxItems", "maxLength", "maxProperties"}:
                    if other < value:
                        issues.append(f"{pointer(at)} upper bound lowered")
                elif key == "parameters" and isinstance(value, list):
                    before = {(p.get("in"), p.get("name")): p for p in value}
                    after = {(p.get("in"), p.get("name")): p for p in other}
                    for identity, parameter in before.items():
                        if identity not in after:
                            issues.append(f"{pointer(at)} parameter {identity} removed")
                        else:
                            walk(parameter, after[identity], (*at, str(identity)))
                    for identity in after.keys() - before.keys():
                        if after[identity].get("required"):
                            issues.append(f"{pointer(at)} required parameter {identity} added")
                        else:
                            additions.append(f"{pointer(at)} optional parameter {identity}")
                elif key == "additionalProperties" and value is True and other is False:
                    issues.append(f"{pointer(at)} unknown fields now rejected")
                elif key in {"pattern", "const", "$ref"} and value != other:
                    issues.append(f"{pointer(at)} changed (manual compatibility review required)")
                elif key not in {"default"}:
                    walk(value, other, at)
            for key in right.keys() - left.keys():
                if key not in ignored:
                    additions.append(pointer((*path, key)))
                    if key == "required" and right[key] and "responses" not in path:
                        issues.append(f"{pointer((*path, key))} required input added")
                    if key in {"enum", "pattern", "minimum", "maximum", "minLength",
                               "maxLength", "minItems", "maxItems", "allOf", "not",
                               "additionalProperties", "type", "const", "oneOf", "anyOf"}:
                        issues.append(f"{pointer((*path, key))} new constraint needs review")
        elif isinstance(left, list) and isinstance(right, list):
            for i, value in enumerate(left):
                if i >= len(right):
                    issues.append(f"{pointer((*path, i))} removed")
                else:
                    walk(value, right[i], (*path, i))
        elif left != right:
            issues.append(f"{pointer(path)} changed")

    expanded_base, expanded_candidate = expand(base, base), expand(candidate, candidate)
    walk(expanded_base["paths"], expanded_candidate["paths"], ("paths",))
    walk(expanded_base.get("components", {}), expanded_candidate.get("components", {}), ("components",))
    accepted = sorted(issue for issue in set(issues) if issue in ACCEPTED_ISSUES)
    issues = [issue for issue in issues if issue not in ACCEPTED_ISSUES]
    return {"issues": issues, "accepted": accepted, "additions": sorted(set(additions)),
            "new_operations": sorted(f"{m} {p}" for m, p in
                                     operations(candidate).keys() - operations(base).keys())}


@contextlib.contextmanager
def proxy(port):
    class Proxy(http.server.BaseHTTPRequestHandler):
        mode = ""

        def do_GET(self):
            upstream = request(port, "GET", self.path)
            body = upstream["body"]
            status = upstream["status"]
            if self.mode == "status":
                status = 202 if status != 202 else 203
            if self.mode == "body":
                body = dict(body, self_test_mutation=True)
            self.send_response(status)
            for key, values in upstream["headers"].items():
                for value in values:
                    if key == "content-type" and self.mode == "header":
                        value = "application/self-test"
                    self.send_header(key, value)
            self.end_headers()
            self.wfile.write(json.dumps(body).encode())

        def log_message(self, *_):
            pass

    listener = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Proxy)
    thread = threading.Thread(target=listener.serve_forever, daemon=True)
    thread.start()
    try:
        yield listener.server_port, Proxy
    finally:
        listener.shutdown()
        listener.server_close()
        thread.join()


def self_test(binary, work):
    evidence = []
    with server(binary, work) as port, proxy(port) as (proxy_port, handler):
        normalizer = Normalizer()
        original = normalizer.response(request(port, "GET", "/health"))
        for mode, expected in (("status", "/status"), ("header", "/headers/content-type"),
                               ("body", "/body/self_test_mutation")):
            handler.mode = mode
            changes = differences(original, normalizer.response(request(proxy_port, "GET", "/health")))
            if not any(change["path"].startswith(expected) for change in changes):
                raise RuntimeError(f"Self-test did not detect proxy {mode} mutation: {changes}")
            evidence.append(f"proxy {mode}: detected {expected}")
        segment = request(port, "GET", "/v1/admin/replication/export", token=ADMIN)["body"]["segment_base64"]
        normalizer.segment(segment)
        corrupted = bytearray(base64.b64decode(segment))
        corrupted[20] ^= 1
        try:
            normalizer.segment(base64.b64encode(corrupted).decode())
        except RuntimeError as error:
            if "SHA-256" not in str(error):
                raise
        else:
            raise RuntimeError("Corrupt replication data escaped integrity verification")
        evidence.append("replication SHA-256 corruption detected before canonicalization")
    normalizer = Normalizer()
    for key in sorted(VOLATILE):
        if differences(normalizer.body({key: 1}), normalizer.body({key: 2})):
            raise RuntimeError(f"Volatile field {key} not normalized")
        if not differences(normalizer.body({key: 1}), normalizer.body({key: "bad"})):
            raise RuntimeError(f"Volatile field {key} lost type information")
    control = {"id": "17", "revision": "2", "records": 3, "healthy": True,
               "vector": VECTOR, "metadata": {"name": "a"}, "error": {"code": "x"}}
    for key in control:
        modified = dict(control, **{key: "changed"})
        if not differences(normalizer.body(control), normalizer.body(modified)):
            raise RuntimeError(f"Nonvolatile field {key} was masked")
    for path in (("metadata", "version"), ("properties", "timestamp_ms"),
                 ("model", "version"), ("metrics", "requests_total")):
        before, after = 1, 2
        for key in reversed(path):
            before, after = {key: before}, {key: after}
        if not differences(normalizer.body(before), normalizer.body(after)):
            raise RuntimeError(f"Nonvolatile nested field {path} was masked")
    if differences(normalizer.body({"next_cursor": "abcdef"}),
                   normalizer.body({"next_cursor": "123456"})):
        raise RuntimeError("Cursor values were not normalized")
    if not differences(normalizer.body({"next_cursor": "abcdef"}),
                       normalizer.body({"next_cursor": None})):
        raise RuntimeError("Cursor nullability was masked")
    left, right = Normalizer(), Normalizer()
    left.bind("a" * 32, "tenant")
    right.bind("b" * 32, "tenant")
    if differences(left.body({"id": "a" * 32}), right.body({"id": "b" * 32})):
        raise RuntimeError("Consistent generated identifier mappings failed")
    if not differences(left.body({"id": "a" * 32}), right.body({"id": "c" * 32})):
        raise RuntimeError("Inconsistent generated identifier reuse was masked")
    evidence.append(f"{len(VOLATILE)} volatile fields mask values, preserve types; "
                    f"{len(control)} nonvolatile controls remain sensitive")
    if not differences([1, 2], [2, 1]):
        raise RuntimeError("Array order was lost")
    spec = {"paths": {"/x": {"get": {"responses": {"200": {"description": "ok"}},
                                  "parameters": [{"in": "query", "name": "q",
                                                  "schema": {"type": "string", "enum": ["a", "b"]}}]}}},
            "components": {"schemas": {"X": {"type": "object", "properties": {"x": {"type": "string"}}}}}}
    modified = copy.deepcopy(spec)
    modified["paths"]["/x"]["get"]["parameters"][0]["schema"]["enum"] = ["a"]
    if not compatibility(spec, modified)["issues"]:
        raise RuntimeError("OpenAPI narrowing self-test failed")
    for mode in ("operation", "parameter", "status", "type", "required"):
        changed = copy.deepcopy(spec)
        if mode == "operation":
            del changed["paths"]["/x"]["get"]
        elif mode == "parameter":
            changed["paths"]["/x"]["get"]["parameters"] = []
        elif mode == "status":
            del changed["paths"]["/x"]["get"]["responses"]["200"]
        elif mode == "type":
            changed["components"]["schemas"]["X"]["properties"]["x"]["type"] = "integer"
        else:
            changed["paths"]["/x"]["get"]["parameters"][0]["required"] = True
        if not compatibility(spec, changed)["issues"]:
            raise RuntimeError(f"OpenAPI {mode} mutation escaped detection")
    sample_spec = {"paths": {}}
    left, right = Exchange(0, sample_spec), Exchange(1, sample_spec)
    sample = {"request": {"path": "/v1/capabilities"},
              "response": {"status": 200, "kind": "json", "headers": {},
                           "body": {"features": {"old": True}, "routes": ["old-a", "old-b"]}}}
    left.responses = [copy.deepcopy(sample)]
    right.responses = [copy.deepcopy(sample)]
    right.responses[0]["response"]["body"]["features"]["kv"] = True
    right.responses[0]["response"]["body"]["routes"].append("/v1/collections/{collection}/kv/entry")
    if compare_exchanges(left, right):
        raise RuntimeError("Reviewed additive allowlist failed")
    for mode in ("status", "header", "field", "order", "removed", "kv-value"):
        changed = copy.deepcopy(right.responses)
        result = right.responses[0]["response"]
        if mode == "status":
            result["status"] = 201
        elif mode == "header":
            result["headers"]["allow"] = ["PATCH"]
        elif mode == "field":
            result["body"]["features"]["old"] = False
        elif mode == "order":
            result["body"]["routes"][:2] = ["old-b", "old-a"]
        elif mode == "removed":
            del result["body"]["features"]["old"]
        else:
            result["body"]["features"]["kv"] = False
        if not compare_exchanges(left, right):
            raise RuntimeError(f"Allowlist masked {mode} regression")
        right.responses = changed
    evidence.append("array order and OpenAPI enum narrowing detected")
    evidence.append("nested metadata/model versions, metric counts, cursor shape and identity consistency remain sensitive")
    evidence.append("OpenAPI operation/parameter/status/type/required mutations detected; allowlist rejects six nonadditive controls")
    return evidence


METADATA = {
    "tag": {"type": "string", "value": "a"},
    "text": {"type": "string", "value": "fixed searchable document"},
    "number": {"type": "integer", "value": "7"},
    "place": {"type": "geopoint", "value": {"latitude": 0.0, "longitude": 0.0}},
}
PUT = {"vector": VECTOR, "metadata": METADATA, "payload_base64": "Zml4ZWQ="}
SEARCH = {"vector": VECTOR, "k": 1}


def example(schema, spec):
    """Only reference OpenAPI schemas supply the generic input examples."""
    if "$ref" in schema:
        return example(spec["components"]["schemas"][schema["$ref"].split("/")[-1]], spec)
    if "const" in schema:
        return schema["const"]
    if "enum" in schema:
        return next((v for v in schema["enum"] if v is not None), None)
    if "oneOf" in schema or "anyOf" in schema:
        return example(schema.get("oneOf", schema.get("anyOf"))[0], spec)
    kind = schema.get("type")
    if isinstance(kind, list):
        kind = next(v for v in kind if v != "null")
    if kind == "object" or "properties" in schema:
        return {key: example(schema.get("properties", {}).get(key, {}), spec)
                for key in schema.get("required", [])}
    if kind == "array":
        return [example(schema.get("items", {}), spec)
                for _ in range(schema.get("minItems", 0))]
    if kind in {"integer", "number"}:
        return schema.get("default", max(1, schema.get("minimum", 0)))
    if kind == "boolean":
        return schema.get("default", False)
    if kind == "string":
        return "fixed"
    return {}


class Exchange:
    def __init__(self, port, spec):
        self.port, self.spec = port, spec
        self.normalizer = Normalizer()
        self.normalizer.replacements[f"http://127.0.0.1:{port}"] = "<server-origin>"
        self.normalizer.replacements[f"127.0.0.1:{port}"] = "<server-address>"
        self.responses = []
        self.state = {}
        self.coverage = {f"{m} {p}": {"statuses": [], "credentials": [],
                                    "valid_statuses": [], "invalid": 0, "reason": ""}
                         for m, p in operations(spec)}
        self.cookie = ""
        self.csrf = ""
        self.webhook_port = None

    def discover(self, value):
        if isinstance(value, dict):
            if isinstance(value.get("lineage"), str):
                self.normalizer.bind(value["lineage"], "lineage")
            if isinstance(value.get("token"), str):
                self.normalizer.bind(value["token"], "token")
            if isinstance(value.get("name"), str) and re.fullmatch(
                    r"(backup|restore)-[0-9-]+\.vlite", value["name"]):
                self.normalizer.bind(value["name"], value["name"].split("-")[0])
            for marker, group in (("username", "user"), ("url", "webhook"),
                                  ("operations", "credential"), ("max_vectors", "tenant")):
                if marker in value and isinstance(value.get("id"), str):
                    self.normalizer.bind(value["id"], group)
                    break
            for name in ("rotated_from", "credential_id", "tenant_id", "user_id"):
                if value.get(name):
                    self.normalizer.bind(value[name], name.removesuffix("_id"))
            for item in value.values():
                self.discover(item)
        elif isinstance(value, list):
            for item in value:
                self.discover(item)

    def capture(self, response, group):
        body = response["body"]
        if not isinstance(body, dict) or response["status"] >= 400:
            return
        # These fields are generated by successful create APIs, not record IDs.
        for key in ("id", "job", "token", "secret", "name"):
            if key == "name" and group not in {"backup", "restore"}:
                continue
            value = body.get(key)
            if isinstance(value, (str, int)):
                if key != "job":
                    self.normalizer.bind(value, group if key in {"id", "name"} else key)
                self.state[f"{group}_{key}"] = value
        for key in ("tenant", "user", "credential", "subscription"):
            if isinstance(body.get(key), dict) and "id" in body[key]:
                value = body[key]["id"]
                self.normalizer.bind(value, key)
                if group == key or group.startswith(key + "-"):
                    self.state[f"{group}_id"] = value
        for value in response["headers"].get("set-cookie", []):
            name, _, secret = value.split(";", 1)[0].partition("=")
            if secret:
                self.normalizer.bind(secret, f"cookie-{name}")
        if group == "login":
            cookies = response["headers"].get("set-cookie", [])
            self.cookie = "; ".join(v.split(";", 1)[0] for v in cookies)
            for value in cookies:
                name, _, secret = value.split(";", 1)[0].partition("=")
                if "csrf" in name:
                    self.csrf = secret

    def send(self, label, method, path, body=None, role="admin", extra=None,
             operation=None, invalid=False, capture=None):
        token = {"admin": ADMIN, "read-only": READ, "none": None,
                 "tenant": self.state.get("tenant_token"),
                 "invalid": "invalid-credential"}[role]
        if role == "tenant" and token is None:
            raise RuntimeError("Tenant credential creation failed; cannot run credential matrix")
        if path in {"/v1/auth/login", "/v1/auth/password", "/v1/auth/logout"}:
            extra = {"Origin": f"http://127.0.0.1:{self.port}",
                     "X-VLite-Request": "1", **(extra or {})}
        response = request(self.port, method, path, body, token, extra)
        self.discover(response["body"])
        if path in {"/v1/auth/login", "/v1/auth/password"}:
            self.capture(response, "login")
        if capture:
            self.capture(response, capture)
        self.responses.append({
            "request": {"label": label, "method": method, "path": path,
                        "credential": role, "body": (
                            {"raw_base64": base64.b64encode(body).decode()}
                            if isinstance(body, bytes) else body), "headers": extra or {}},
            "response": response,
        })
        if method == "POST" and path.endswith("/imports") and response["status"] < 300:
            job = response["body"]["job"]
            self.send("import-first-chunk", "PUT", f"/v1/imports/{job}/chunks/1",
                      {"operations": [{"op": "put", "id": "19", **PUT}]}, role)
        if path == "/v1/auth/password" and response["status"] < 300:
            self.state["password"] = body["new_password"]
        if operation in self.coverage:
            coverage = self.coverage[operation]
            coverage["statuses"].append(response["status"])
            if label in {"valid", "fixture", "import-first-chunk"}:
                coverage["valid_statuses"].append(response["status"])
            if role not in coverage["credentials"]:
                coverage["credentials"].append(role)
            coverage["invalid"] += int(invalid)
        return response

    def setup(self):
        def seed(method, path, body=None, capture=None, role="admin"):
            operation = f"{method} {path}"
            response = self.send("fixture", method, path, body, role,
                                 operation=operation, capture=capture)
            if response["status"] >= 400:
                raise RuntimeError(f"Fixture request {method} {path} failed: {response}")
            return response["body"]

        seed("POST", "/v1/admin/tenants", {"name": "fixture_tenant"}, "tenant")
        tenant = self.state["tenant_id"]
        # Choose one future expiry for the pair, never separately per server.
        credentials = {"name": "fixture_key", "collections": ["*"],
                       "operations": ["read", "search", "search_build", "write",
                                      "import", "index", "collections", "diagnostics", "backup"],
                       "expires_unix_ms": self.spec["_expiry"], "tenant_id": tenant}
        result = seed("POST", "/v1/admin/credentials", credentials, "credential")
        self.state["tenant_token"] = result.get("token", result.get("secret"))
        if self.state["tenant_token"] is None:
            raise RuntimeError(f"Credential response has no token/secret: {result}")
        self.normalizer.bind(self.state["tenant_token"], "tenant-token")
        seed("POST", "/v1/admin/users",
             {"username": "fixture_user", "password": PASSWORD,
              "role": "viewer", "tenant_id": tenant}, "user")
        seed("POST", "/v1/collections", {"name": "fixed", "dimensions": 4,
                                       "settings": {"text_fields": ["text"]}})
        seed("POST", "/v1/collections", {"name": "fixed", "dimensions": 4}, role="tenant")
        seed("POST", "/v1/collections", {"name": "emptycollection", "dimensions": 4})
        seed("POST", "/v1/collections", {"name": "emptycollection", "dimensions": 4}, role="tenant")
        result = seed("POST", "/v1/admin/tenants", {"name": "delete_tenant"})
        self.state["delete_tenant_id"] = result["tenant"]["id"]
        for prefix in ("/v1", "/v1/collections/fixed"):
            seed("PUT", prefix + "/vectors/17", PUT)
            seed("PUT", prefix + "/vectors/18", dict(PUT, vector=[0.0, 1.0, 0.0, 0.0]))
        seed("POST", "/v1/graphs", {"graph": "fixedgraph"})
        seed("POST", "/v1/graphs", {"graph": "emptygraph"})
        seed("POST", "/v1/graphs/fixedgraph/batch",
             {"ops": [{"op": "create_node", "key": "a", "labels": ["Thing"]},
                      {"op": "create_node", "key": "b", "labels": ["Thing"]}]})
        seed("POST", "/v1/graphs/fixedgraph/batch",
             {"ops": [{"op": "create_edge", "key": "ab", "source": "a",
                       "destination": "b", "relationship": "LINK"}]})
        seed("POST", "/v1/admin/backups", {}, "backup")
        seed("POST", "/v1/collections/fixed/imports", {}, "import")
        self.state["abort_job"] = self.state["import_job"]
        seed("POST", "/v1/collections/fixed/imports", {}, "import")
        seed("POST", "/v1/auth/login",
             {"username": "restadmin", "password": PASSWORD}, "login", role="none")

    def path(self, template, method):
        identifier = "17"
        for prefix, group in (("/v1/admin/tenants", "tenant"),
                              ("/v1/admin/users", "user"),
                              ("/v1/admin/credentials", "credential"),
                              ("/v1/admin/webhooks", "webhook")):
            if template.startswith(prefix):
                identifier = str(self.state.get(f"{group}_id", "0" * 32))
        values = {"id": identifier, "c": "fixed", "collection": "fixed", "binding": "agent",
                  "graph": "fixedgraph", "job": str(self.state.get("import_job", "1")),
                  "n": "1", "name": self.state.get("backup_name", "missing.vlite")}
        if method == "DELETE":
            if template == "/v1/collections/{c}":
                values.update(c="emptycollection", collection="emptycollection")
            if template == "/v1/graphs/{graph}":
                values["graph"] = "emptygraph"
            if template == "/v1/admin/tenants/{id}":
                values["id"] = self.state["delete_tenant_id"]
        if template.endswith("/abort"):
            values["job"] = self.state["abort_job"]
        return re.sub(r"\{([^}]+)\}", lambda m: urllib.parse.quote(str(values[m[1]]), safe=""), template)

    def body(self, method, path, operation):
        schema = operation.get("requestBody", {}).get("content", {}).get(
            "application/json", {}).get("schema", {})
        body = example(schema, self.spec) if schema else ({} if method in {"POST", "PUT", "PATCH"} else None)
        if path.endswith("/settings"):
            return {"metric": "squared_l2"} if "/collections/" in path else {"name": "fixture_tenant"}
        if path.endswith("/vectors/{id}") and method == "PUT":
            return PUT
        if path.endswith("/metadata"):
            return {"metadata": METADATA}
        if path.endswith("/batches") or "/chunks/" in path:
            return {"operations": [{"op": "put", "id": "19", **PUT}]}
        if path.endswith("/search"):
            return SEARCH
        leaf = path.rsplit("/", 1)[-1]
        overrides = {
            "grouped": {"vector": VECTOR, "field": "tag", "groups": 1, "hits_per_group": 1},
            "recommend": {"positive_ids": ["17"], "k": 1},
            "facets": {"field": "tag"},
            "numeric": {"field": "number"},
            "geo": {**SEARCH, "field": "place", "latitude": 0.0, "longitude": 0.0,
                    "radius_meters": 1000.0},
            "configure": {"name": "other", "dimensions": 4, "metric": "squared_l2",
                          "active_version": "v1", "history": 1},
            "sparse": {"name": "sparse", "vector": [[1, 1.0]], "k": 1},
            "dense-sparse": {"dense_name": "other", "dense": VECTOR, "sparse_name": "sparse",
                             "sparse": [[1, 1.0]], "k": 1},
            "query": {"query": "SEARCH default NEAR $q K 1", "vectors": {"q": VECTOR}},
            "embed": {"text": "fixed searchable document", "provider": "hashing"},
            "quality": {"samples": 1, "k": 1, "ef_search": 16, "seed": "42"},
        }
        if "/collections/" in path:
            if path.endswith("/vectors/{id}/named"):
                return {"dense": {"other": [{"version": "v1", "vector": VECTOR}]},
                        "sparse": {"sparse": [[1, 1.0]]}}
            if path.endswith("/search/named"):
                return {"name": "other", **SEARCH}
            if leaf in overrides:
                return overrides[leaf]
        if path.endswith("/search/disk-ann"):
            return {**SEARCH, "ef_search": 16}
        if path == "/v1/collections":
            return {"name": "operation_collection", "dimensions": 4} if method == "POST" else None
        if path == "/v1/admin/tenants" and method == "POST":
            return {"name": "operation_tenant"}
        if path == "/v1/admin/users" and method == "POST":
            return {"username": "operation_user", "password": PASSWORD, "role": "viewer"}
        if path == "/v1/admin/credentials" and method == "POST":
            return {"name": "operation_key", "collections": ["*"], "operations": ["read"],
                    "expires_unix_ms": self.spec["_expiry"]}
        if path == "/v1/admin/webhooks" and method == "POST":
            return {"url": f"http://127.0.0.1:{self.webhook_port}/closed",
                    "secret": "rest-diff-webhook-secret", "events": ["put"]}
        if path == "/v1/admin/restores":
            return {"backup": self.state["backup_name"]}
        if path == "/v1/admin/restores/select":
            return {"name": self.state.get("restore_name", "missing.vlite")}
        if path.endswith("/password"):
            if "/users/" in path:
                return {"password": PASSWORD, "must_change_password": False}
            return {"current_password": PASSWORD, "new_password": PASSWORD + "changed"}
        if path == "/v1/auth/login":
            return {"username": "restadmin", "password": self.state.get("password", PASSWORD)}
        if path == "/v1/admin/users/{id}" and method == "PUT":
            return {"disabled": False}
        if path == "/v1/graphs" and method == "POST":
            return {"graph": "operation_graph"}
        if "/graphs/" in path:
            graph_bodies = {
                "get": {"node": {"key": "a"}} if "/nodes/" in path else {"edge": {"key": "ab"}},
                "find": {"label": "Thing", "limit": 1},
                "neighbors": {"node": {"key": "a"}},
                "traverse": {"start": [{"key": "a"}]},
                "shortest-path": {"source": {"key": "a"}, "destination": {"key": "b"}},
                "explain": {"operation": "get-node", "node": {"key": "a"}},
                "batch": {"ops": [{"op": "upsert_node", "key": "a", "labels": ["Thing"]}]},
                "schema": {"schema": {"labels": {"Thing": {"properties": {
                    "tag": {"type": "string", "required": False}}}}}},
                "retrieve": {"pivot": {"collection": "fixed", "vector": VECTOR, "k": 1}},
                "query": {"query": 'MATCH (n KEY "a") RETURN n'},
            }
            if method in {"POST", "PUT"} and leaf in graph_bodies:
                return graph_bodies[leaf]
        return body

    def run(self, closed_port):
        self.webhook_port = closed_port
        self.setup()
        inventory = operations(self.spec)

        def order(item):
            (method, path), _ = item
            # Keep dependent resources and credentials alive until reads finish.
            phase = 1
            if "/vectors/configure" in path:
                phase = 0
            if "/index/" in path and "rebuild" not in path:
                phase = 2
            if "/search" in path or "/graphs/" in path:
                phase = 3
            if "/imports/" in path:
                phase = 4
            if path.endswith("/finish"):
                phase = 5
            if path.endswith("/abort"):
                phase = 6
            if "webhooks" in path:
                phase = 6
            if "restores" in path:
                phase = 7
            if method == "DELETE":
                phase = 8
            if "/revoke" in path or "/suspend" in path or path.endswith("/logout"):
                phase = 9
            if path == "/v1/admin/tenants/{id}" and method == "DELETE":
                phase = 10
            return phase, path, method

        for (method, template), operation in sorted(inventory.items(), key=order):
            identity = f"{method} {template}"
            path = self.path(template, method)
            body = self.body(method, template, operation)
            for role in ("read-only", "none", "tenant", "invalid"):
                self.send("credential-matrix", method, path, body, role,
                          operation=identity, invalid=role in {"none", "invalid"})
            if operation.get("requestBody") or method in {"POST", "PUT", "PATCH"}:
                self.send("malformed-json", method, path, b'{"broken":', operation=identity, invalid=True)
                self.send("wrong-type", method, path, b'["wrong-type"]', operation=identity, invalid=True)
            elif "{" in template or operation.get("parameters"):
                missing = re.sub(r"\{[^}]+\}", "nonexistent", template)
                self.send("nonexistent-or-invalid-query", method, missing + "?limit=bad",
                          operation=identity, invalid=True)
                self.send("invalid-credential", method, path, body, "invalid",
                          operation=identity, invalid=True)
            capture = None
            if method == "POST":
                capture = {
                    "/v1/admin/tenants": "tenant-extra", "/v1/admin/users": "user-extra",
                    "/v1/admin/credentials": "credential-extra", "/v1/admin/webhooks": "webhook",
                    "/v1/admin/backups": "backup", "/v1/admin/backups/schedule/run": "backup",
                    "/v1/admin/restores": "restore", "/v1/collections/{c}/imports": "import",
                    "/v1/auth/login": "login",
                }.get(template)
            extra = None
            if template in {"/v1/auth/password", "/v1/auth/logout"}:
                if template.endswith("/logout"):
                    self.send("session-refresh", "POST", "/v1/auth/login",
                              {"username": "restadmin", "password": self.state.get("password", PASSWORD)},
                              "none", capture="login")
                extra = {"Cookie": self.cookie, "X-CSRF-Token": self.csrf,
                         "Origin": f"http://127.0.0.1:{self.port}"}
                self.send("token-auth-control", method, path, body, operation=identity)
            if template.endswith("/index/hnsw/rebuild") and method == "POST":
                time.sleep(0.05)
                self.send("settle-tenant-rebuild", "POST", path + "/poll", {}, "tenant")
                self.send("settle-prior-rebuild", "POST", path + "/poll", {})
            response = self.send("valid", method, path, body, extra=extra,
                                 role="none" if extra else "admin",
                                 operation=identity, capture=capture)
            if template == "/v1/changes" and response["status"] == 200:
                cursor = response["body"].get("next_cursor")
                if cursor:
                    self.send("cursor-resume", "GET", "/v1/changes?cursor=" + urllib.parse.quote(cursor),
                              operation=identity)
                    self.send("invalid-cursor", "GET", "/v1/changes?cursor=damaged",
                              operation=identity, invalid=True)
            if template.endswith("/search") and response["status"] == 200:
                for mode in ("exact", "approximate", "approximate_ready", "auto", "hybrid"):
                    search = dict(SEARCH, mode=mode)
                    if mode == "hybrid":
                        search["text"] = "fixed"
                    self.send("search-" + mode, method, path, search, operation=identity)
                self.send("filtered-search", method, path, dict(SEARCH, filter=[
                    {"op": "eq", "field": "tag", "value": METADATA["tag"]}]), operation=identity)
                for rerank in ({"mode": "exact"}, {"mode": "mmr", "lambda": 0.5},
                               {"mode": "exact", "lambda": 0.5}):
                    self.send("rerank-probe", method, path, dict(SEARCH, rerank=rerank),
                              operation=identity, invalid="lambda" in rerank and rerank["mode"] == "exact")
            if "/webhooks/" in template and method == "GET" and response["status"] == 200:
                if "last_success_ms" not in response["body"]:
                    raise RuntimeError("Webhook status omitted last_success_ms")
                if response["body"]["last_success_ms"] is not None:
                    raise RuntimeError("Closed-loopback webhook unexpectedly delivered")

        for label, method, path, body, extra in [
            ("unknown-route", "GET", "/definitely-not-a-route", None, None),
            ("wrong-method", "TRACE", "/v1/database", None, None),
            ("oversize-body", "PUT", "/v1/vectors/99", b" " * (16 * 1024 * 1024), None),
            ("wrong-content-type", "PUT", "/v1/vectors/99", PUT, {"Content-Type": "text/plain"}),
            ("invalid-json", "PUT", "/v1/vectors/99", b"{", None),
            ("docs-route", "GET", "/docs", None, None),
            ("prometheus", "GET", "/metrics?format=prometheus", None, None),
        ]:
            response = self.send(label, method, path, body, extra=extra)
            expected = {"unknown-route": 404, "wrong-method": 405, "oversize-body": 413,
                        "wrong-content-type": 415, "invalid-json": 400}.get(label)
            if expected is not None and response["status"] != expected:
                self.state.setdefault("probe_issues", []).append(
                    f"{label}: expected {expected}, observed {response['status']}")
        return self.responses


def compare_exchanges(left, right):
    findings = []
    if len(left.responses) != len(right.responses):
        raise RuntimeError("Exchange request counts differ")
    for reference, candidate in zip(left.responses, right.responses):
        before = left.normalizer.response(reference["response"])
        after = right.normalizer.response(candidate["response"])
        # OpenAPI is compared structurally, rather than as an equality body.
        if reference["request"]["path"] == "/openapi.json":
            before["body"] = after["body"] = "<mechanical OpenAPI comparison>"
        if reference["request"]["path"] == "/v1/capabilities" and before["status"] == after["status"] == 200:
            old, new = before["body"], after["body"]
            if ("GET /v1/capabilities /body/features/kv" in ALLOWLIST
                    and "kv" not in old.get("features", {})
                    and new.get("features", {}).get("kv") is True):
                del new["features"]["kv"]
            if isinstance(old.get("routes"), list) and isinstance(new.get("routes"), list):
                new["routes"] = [
                    route for route in new["routes"]
                    if route in old["routes"] or
                    f"GET /v1/capabilities /body/routes +{route}" not in ALLOWLIST]
        changes = differences(before, after)
        if changes:
            findings.append({"classification": "unexplained difference; regression candidate",
                             "request": left.normalizer.body(reference["request"]),
                             "candidate_request": right.normalizer.body(candidate["request"]),
                             "reference_response": before, "candidate_response": after,
                             "differences": changes})
    return findings


def write_json(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n",
                    encoding="utf-8", newline="\r\n")


def markdown(report):
    lines = ["# REST differential check", "",
             f"Reference: `{report.get('reference', '?')}`; candidate: `{report.get('candidate', '?')}`.",
             f"Result: **{report['result']}**.", "",
             "## Coverage", "", "| Operation | Statuses | Credentials | Invalid probes | Skip reason |",
             "|---|---|---|---|---|"]
    for operation, coverage in report.get("coverage", {}).items():
        lines.append(f"| `{operation}` | {','.join(map(str, sorted(set(coverage['statuses']))))} | "
                     f"{','.join(coverage['credentials'])} | {coverage['invalid']} | {coverage['reason']} |")
    lines.extend(["", "## Findings", ""])
    for finding in report.get("findings", []):
        lines.extend(["```json", json.dumps(finding, indent=2), "```", ""])
    for issue in report.get("harness_issues", []):
        lines.append(f"- Harness issue: {issue}")
    contract = report.get("openapi", {})
    lines.extend(["", "## OpenAPI compatibility", "",
                  *["- " + issue for issue in contract.get("issues", [])],
                  "Accepted tightenings (explicit, reviewed):",
                  *[f"- {issue}: {ACCEPTED_ISSUES[issue]}" for issue in contract.get("accepted", [])],
                  "Additions:", *["- " + addition for addition in contract.get("additions", [])],
                  "New operations (not exercised):", *["- " + item for item in contract.get("new_operations", [])],
                  "", "## Self-test", "", *report.get("self_test", []),
                  "", "## Normalization and allowlist", "",
                  "JSON object keys sorted; arrays retain order. Volatile fields preserve presence, nulls and type:",
                  ", ".join(sorted(VOLATILE)) + ".",
                  "Generated API identifiers/secrets are mapped consistently per server; cursors retain type, length and encoding.",
                  "Date, content-length and connection framing headers are excluded. All configured meaningful headers compare exactly.",
                  f"ALLOWLIST: {json.dumps(ALLOWLIST)}."])
    return "\n".join(lines) + "\n"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    subcommands = parser.add_subparsers(dest="action", required=True)
    for name in ("run", "self-test"):
        sub = subcommands.add_parser(name)
        sub.add_argument("--reference", default="v0.6.1")
        sub.add_argument("--work", type=Path)
        sub.add_argument("--json", type=Path)
        sub.add_argument("--md", type=Path)
        if name == "run":
            sub.add_argument("--candidate", default="HEAD")
    args = parser.parse_args()
    report = {"result": "harness failure", "findings": [], "harness_issues": []}
    exit_code = 2
    with contextlib.ExitStack() as stack:
        if args.work:
            work = args.work.resolve()
            work.mkdir(parents=True, exist_ok=True)
        else:
            work = Path(stack.enter_context(tempfile.TemporaryDirectory(prefix="vlite-rest-diff-")))
        try:
            report["reference"] = resolve(args.reference)
            source, binary = build(report["reference"], work)
            report["self_test"] = self_test(binary, work)
            if args.action == "self-test":
                report["result"] = "self-test passed"
                exit_code = 0
            else:
                report["candidate"] = resolve(args.candidate)
                candidate_source, candidate_binary = build(report["candidate"], work)
                base, candidate = document(source), document(candidate_source)
                report["openapi"] = compatibility(base, candidate)
                base["_expiry"] = int(time.time() * 1000) + 30 * 86400 * 1000
                # Bind but do not listen: connection attempts must fail.
                closed = stack.enter_context(socket.socket())
                closed.bind(("127.0.0.1", 0))
                closed_port = closed.getsockname()[1]
                with server(binary, work) as port:
                    left = Exchange(port, base)
                    left.run(closed_port)
                with server(candidate_binary, work) as port:
                    right = Exchange(port, base)
                    right.run(closed_port)
                report["coverage"] = left.coverage
                report["requests"] = len(left.responses)
                report["findings"] = compare_exchanges(left, right)
                report["transcript"] = [
                    {"request": left.normalizer.body(item["request"]),
                     "response": left.normalizer.response(item["response"])}
                    for item in left.responses]
                for operation, coverage in left.coverage.items():
                    if not any(status < 400 for status in coverage["valid_statuses"]):
                        if operation == "POST /v1/admin/tls/reload":
                            coverage["reason"] = "TLS reload success requires a TLS listener; identical required HTTP-only settings test tls_disabled (409)."
                            continue
                        report["harness_issues"].append(
                            f"{operation}: no successful valid reference request; not claimed covered")
                report["harness_issues"].extend(left.state.get("probe_issues", []))
                report["harness_issues"].extend(right.state.get("probe_issues", []))
                if report["harness_issues"]:
                    report["result"], exit_code = "incomplete coverage", 2
                elif report["findings"] or report["openapi"]["issues"]:
                    report["result"], exit_code = "differences found", 1
                else:
                    report["result"], exit_code = "no unexplained differences", 0
        except (OSError, ValueError, RuntimeError, subprocess.SubprocessError,
                http.client.HTTPException, KeyError) as error:
            report["harness_issues"].append(str(error))
            print(f"Harness failure: {error!r}\n{traceback.format_exc()}", file=sys.stderr)
        if args.json:
            write_json(args.json, report)
        if args.md:
            args.md.parent.mkdir(parents=True, exist_ok=True)
            args.md.write_text(markdown(report), encoding="utf-8", newline="\r\n")
    print(json.dumps({key: value for key, value in report.items()
                      if key not in {"coverage", "findings", "transcript"}}, indent=2))
    print(f"{len(report['findings'])} differential findings")
    return exit_code


if __name__ == "__main__":
    sys.exit(main())
