#!/usr/bin/env python3
"""Disposable, document-driven agent-memory live qualification (Python 3.11+).

    python -B scripts/memory_live_e2e.py self-test
    python -B scripts/memory_live_e2e.py run --json memory-live.json
    python -B scripts/memory_live_e2e.py run --server-bin PATH --only rest,default

Only the standard library is needed. Builds, generated clients, credentials and
databases live in a private temporary child of --work (or the system temp dir).
--keep preserves that child, including disposable secrets: protect it accordingly.
No mutations are retried, no existing database is opened, and no server source or
server tests supply expectations. Authority is serving-memory-contract sections
3-6/8, serving-wire-conventions sections 2-3, gap1-adapter-mapping sections 1/2/4/6,
SECURITY's Memory-bound resources, the client/tool guides, and independent wire
fixtures. First-slice mapping rulings take precedence over illustrative future
capabilities. OPEN-LE identifiers denote missing example prerequisites, not passes.
"""

from __future__ import annotations

import argparse
import base64
import copy
import dataclasses
import datetime as dt
import hashlib
import http.client
import http.server
import importlib
import json
import os
from pathlib import Path
import queue
import re
import secrets
import shutil
import signal
import subprocess
import sys
import tempfile
import threading
import time
from typing import Any, Callable
from urllib.parse import urlsplit


ROOT = Path(__file__).resolve().parents[1]
SECTIONS = ("rest", "default", "python", "javascript", "dotnet", "cli", "mcp", "docs")
CAP = 8 * 1024 * 1024
SCOPE = {"agent": "live-e2e"}
OTHER_SCOPE = {"agent": "live-e2e-other"}
FEED = {"mode": "ordinary_changes", "temporal_complete": False,
        "ttl_complete": False, "consumer_purge_acknowledged": False}
NOTICE = {"live_state_only": True, "secure_erasure": False,
          "external_copies": "operator_managed", "restore_requires_manifest": True,
          "backup_history_purge": "not_verified"}
ROUTES = {"remember": "remember", "recall": "recall", "facts": "facts/query",
          "correct-facts": "facts/correct", "consolidate": "consolidate",
          "forget": "forget", "receipt": "receipts/lookup", "erasure": "erasures/get",
          "resume-erasure": "erasures/resume"}
REFUSALS = {"facts": "facts", "correct-facts": "facts",
            "erasure": "erasure_jobs", "resume-erasure": "erasure_jobs"}
CONTRACT = "docs/projects/roadmap/serving-memory-contract.md"
MAPPING = "docs/projects/roadmap/gap1-adapter-mapping.md"
DOCS = (CONTRACT, "docs/projects/roadmap/serving-wire-conventions.md", MAPPING,
        "docs/SECURITY.md", "docs/API.md", "docs/TOOLS.md", "docs/MCP.md",
        "clients/README.md", "clients/python/README.md", "clients/javascript/README.md")


def now() -> str:
    return dt.datetime.now(dt.timezone.utc).isoformat()


def encode(value: Any) -> bytes:
    return json.dumps(value, ensure_ascii=False, separators=(",", ":"), allow_nan=False).encode("utf-8")


def strict_json(data: bytes | str) -> Any:
    def pairs(entries: list[tuple[str, Any]]) -> dict[str, Any]:
        result: dict[str, Any] = {}
        for key, value in entries:
            if key in result:
                raise ValueError(f"duplicate JSON key {key!r}")
            result[key] = value
        return result

    def constant(value: str) -> Any:
        raise ValueError(f"nonfinite JSON value {value}")

    text = data.decode("utf-8") if isinstance(data, bytes) else data
    return json.loads(text, object_pairs_hook=pairs, parse_constant=constant)


def require(condition: Any, message: str) -> None:
    # Do not use assert: qualification must work under python -O too.
    if not condition:
        raise AssertionError(message)


def prerequisite(value: Any, message: str) -> None:
    if value is None:
        raise SkipCheck("blocked by failed prerequisite: " + message)


def decimal(value: Any, signed: bool = False) -> None:
    pattern = r"(?:0|-?[1-9][0-9]*)" if signed else r"(?:0|[1-9][0-9]*)"
    require(isinstance(value, str) and re.fullmatch(pattern, value) is not None,
            f"expected canonical decimal string, got {value!r}")
    low, high = (-(2**63), 2**63 - 1) if signed else (0, 2**64 - 1)
    require(low <= int(value) <= high, f"out of range integer {value}")


class SkipCheck(Exception):
    pass


@dataclasses.dataclass
class Result:
    section: str
    check: str
    status: str
    reason: str
    clause: str
    category: str = ""
    exchanges: list[int] = dataclasses.field(default_factory=list)


class Report:
    def __init__(self, quiet: bool = False):
        self.results: list[Result] = []
        self.traffic: list[dict[str, Any]] = []
        self.secret_values: set[str] = set()
        self.lock = threading.RLock()
        self.quiet = quiet

    def secret(self, token: str) -> str:
        with self.lock:
            self.secret_values.add(token)
        return token

    def redact(self, value: Any) -> Any:
        if isinstance(value, dict):
            return {self.redact(str(k)): ("[REDACTED]" if str(k).lower() in
                    {"authorization", "token", "access_token", "password"} else self.redact(v))
                    for k, v in value.items()}
        if isinstance(value, (tuple, list)):
            return [self.redact(v) for v in value]
        if isinstance(value, str):
            with self.lock:
                tokens = sorted(self.secret_values, key=len, reverse=True)
            for token in tokens:
                value = value.replace(token, "[REDACTED]")
            return re.sub(r"vlk_[A-Za-z0-9_-]+", "[REDACTED]", value)
        return value

    def event(self, **event: Any) -> int:
        with self.lock:
            index = len(self.traffic)
            self.traffic.append({"index": index, "timestamp": now(), **self.redact(event)})
            return index

    def check(self, section: str, name: str, clause: str, fn: Callable[[], Any]) -> Any:
        start = len(self.traffic)
        value = None
        try:
            value = fn()
            status, reason = "PASS", ""
        except SkipCheck as exc:
            status, reason = "SKIP", str(exc)
        except Exception as exc:
            status, reason = "FAIL", f"{type(exc).__name__}: {exc}"
        exchanges = list(range(start, len(self.traffic)))
        category = ""
        if status == "FAIL":
            missing = False
            for event in self.traffic[start:]:
                response = event.get("response", {})
                response_value = response.get("json")
                item = response_value.get("error") if isinstance(response_value, dict) else None
                code = item.get("code") if isinstance(item, dict) else None
                missing |= (response.get("status") == 404 and code in (None, "not_found") and
                            "/v1/memory/" in event.get("request", {}).get("path", ""))
            category = ("environment/setup failure" if section == "setup" else
                        "route not implemented in this checkout" if missing else "behavior differs from documents")
        result = Result(section, name, status, self.redact(reason), clause, category, exchanges)
        self.results.append(result)
        if not self.quiet:
            print(f"{status:4} {section}/{name}" + (f": {result.reason}" if reason else ""), flush=True)
        return value

    def skip(self, section: str, name: str, reason: str) -> None:
        def skipped() -> None:
            raise SkipCheck(reason)
        self.check(section, name, "", skipped)

    def summary(self, sections: tuple[str, ...] | list[str]) -> None:
        print("\nSection     PASS  FAIL  SKIP")
        for section in sections:
            rows = [r for r in self.results if r.section == section]
            counts = [sum(r.status == s for r in rows) for s in ("PASS", "FAIL", "SKIP")]
            print(f"{section:11} {counts[0]:4}  {counts[1]:4}  {counts[2]:4}")
        print(f"Overall: {'FAIL' if self.failed else 'PASS'}; "
              f"{len(self.results)} checks; {len(self.traffic)} recorded exchanges")

    def section_summary(self, section: str) -> None:
        rows = [r for r in self.results if r.section == section]
        status = "FAIL" if any(r.status == "FAIL" for r in rows) else (
            "PASS" if any(r.status == "PASS" for r in rows) else "SKIP")
        print(f"{section}: {status}; " + ", ".join(
            f"{sum(r.status == s for r in rows)} {s}" for s in ("PASS", "FAIL", "SKIP")), flush=True)

    @property
    def failed(self) -> bool:
        return any(r.status == "FAIL" for r in self.results)

    def write(self, path: Path) -> None:
        output = {"generated_at": now(), "authority": list(DOCS),
                  "results": [dataclasses.asdict(r) for r in self.results],
                  "exchanges": self.traffic}
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(json.dumps(self.redact(output), ensure_ascii=True, indent=2) + "\n", encoding="utf-8")


@dataclasses.dataclass
class Reply:
    status: int
    headers: dict[str, str]
    raw: bytes

    def json(self) -> Any:
        return strict_json(self.raw)


def body_record(raw: bytes) -> dict[str, Any]:
    try:
        text = raw.decode("utf-8")
    except UnicodeDecodeError:
        return {"bytes": len(raw), "base64": base64.b64encode(raw).decode("ascii")}
    try:
        value = strict_json(text)
    except (ValueError, RecursionError):
        return {"bytes": len(raw), "text": text}
    return {"bytes": len(raw), "text": text, "json": value}


class HTTP:
    def __init__(self, url: str, token: str, report: Report):
        parsed = urlsplit(url)
        require(parsed.scheme == "http" and parsed.hostname == "127.0.0.1",
                "live harness only connects to loopback HTTP")
        self.url, self.host, self.port = url, parsed.hostname, parsed.port
        self.token, self.report = token, report

    def request(self, method: str, path: str, body: Any = None, *, token: str | None = None,
                raw: bytes | None = None, content_type: str = "application/json",
                headers: dict[str, str] | None = None) -> Reply:
        data = raw if raw is not None else (encode(body) if body is not None else b"")
        outgoing = {"Accept": "application/json"}
        auth = self.token if token is None else token
        if auth:
            outgoing["Authorization"] = "Bearer " + auth
        if method not in ("GET", "HEAD") or data:
            outgoing["Content-Type"] = content_type
        outgoing.update(headers or {})
        request = {"method": method, "path": path, "headers": outgoing, **body_record(data)}
        conn = http.client.HTTPConnection(self.host, self.port, timeout=15)
        try:
            conn.request(method, path, body=data if method not in ("GET", "HEAD") or data else None,
                         headers=outgoing)
            response = conn.getresponse()
            reply = Reply(response.status, {k.lower(): v for k, v in response.getheaders()}, response.read())
            self.report.event(kind="http", request=request,
                              response={"status": reply.status, "headers": reply.headers, **body_record(reply.raw)})
            return reply
        except (OSError, http.client.HTTPException) as exc:
            self.report.event(kind="http", request=request, response={"transport_error": str(exc)})
            raise
        finally:
            conn.close()

    def memory(self, verb: str, body: Any, binding: str = "notes", **kwargs: Any) -> Reply:
        return self.request("POST", f"/v1/memory/{binding}/{ROUTES[verb]}", body, **kwargs)


def success(reply: Reply, status: int = 200) -> dict[str, Any]:
    require(reply.status == status, f"expected HTTP {status}, got {reply.status}: {reply.raw[:1000]!r}")
    value = reply.json()
    require(isinstance(value, dict) and "error" not in value, "success must be a JSON object without error")
    return value


def error(reply: Reply, status: int | tuple[int, ...], code: str, *,
          feature: str | None = None, field: bool = False) -> dict[str, Any]:
    statuses = (status,) if isinstance(status, int) else status
    require(reply.status in statuses, f"expected HTTP {statuses}/{code}, got {reply.status}: {reply.raw[:1000]!r}")
    body = reply.json()
    require(isinstance(body, dict) and set(body) == {"error"}, "error envelope must contain only error")
    item = body["error"]
    require(isinstance(item, dict) and {"code", "message"} <= item.keys() <= {"code", "message", "details"},
            "invalid error object shape")
    require(item["code"] == code and isinstance(item["message"], str) and bool(item["message"]),
            f"expected {code}, got {item!r}")
    if "details" in item:
        details = item["details"]
        require(isinstance(details, dict) and details.keys() <=
                {"field", "limit", "requested", "feature", "category"}, "unknown error details")
        for key, value in details.items():
            require(isinstance(value, str), f"details.{key} must be a string")
            if key in ("limit", "requested"):
                decimal(value)
    if feature:
        require(item.get("details", {}).get("feature") == feature, f"missing details.feature={feature}")
    if field:
        require(bool(item.get("details", {}).get("field")), "precondition/budget must name details.field")
    return item


def item(identifier: str, kind: str = "entity", content: str | None = None) -> dict[str, Any]:
    return {"id": identifier, "kind": kind, "content": content or f"Memory {identifier}",
            "observed_at_ms": "90", "source": {"source_id": "live", "source_kind": "caller"}}


def remember(key: str, *items: dict[str, Any], scope: dict[str, Any] | None = None) -> dict[str, Any]:
    return {"scope": scope or SCOPE, "idempotency_key": key, "items": list(items)}


def recall(*ids: str, scope: dict[str, Any] | None = None) -> dict[str, Any]:
    return {"scope": scope or SCOPE, "entity_seeds": list(ids), "limit": 20}


def consolidation(key: str, source: str, revision: str, output: str) -> dict[str, Any]:
    summary = item(output, "episodic", "Caller-produced summary")
    summary["source"]["transformation_id"] = "live-summary-v1"
    return {"scope": SCOPE, "idempotency_key": key,
            "inputs": [{"id": source, "revision": revision}], "output": summary}


def forgetting(key: str, identifier: str, revision: str) -> dict[str, Any]:
    return {"scope": SCOPE, "idempotency_key": key,
            "target": {"ids": [{"id": identifier, "revision": revision}]}, "mode": "logical_delete"}


def mutation(value: dict[str, Any], operation: str, scope: dict[str, Any] = SCOPE,
             tenant: str = "@root", replayed: bool = False) -> dict[str, Any]:
    require({"receipt", "replayed", "outcomes", "feed", "remaining_work"} <= value.keys(),
            "missing mutation envelope fields")
    require(value["replayed"] is replayed, f"expected replayed={replayed}")
    require(value["feed"] == FEED, "feed must disclose ordinary, incomplete changes")
    require(value["remaining_work"] == "0", "small mutation must finish with remaining_work='0'")
    receipt = value["receipt"]
    require(isinstance(receipt.get("id"), str) and bool(receipt["id"]), "missing opaque receipt ID")
    require(receipt["operation"] == operation and receipt["scope"] == {"tenant": tenant, **scope},
            "receipt operation/effective scope mismatch")
    require(receipt["digest_version"] == "memory-request-v1", "unexpected digest version")
    decimal(receipt["sequence"])
    decimal(receipt["retry_until_ms"])
    require(int(receipt["retry_until_ms"]) > 0, "receipt must advertise a positive retention boundary")
    require(isinstance(value["outcomes"], list), "outcomes must be an ordered array")
    for outcome in value["outcomes"]:
        require(isinstance(outcome.get("id"), str), "outcome missing ID")
        require(outcome.get("action") in {"created", "merged", "noop", "deleted", "corrected",
                                         "expired", "retained_shared"}, "unknown outcome action")
        decimal(outcome["revision"])
    if operation == "forget":
        require(value.get("state") == "logical", "logical delete must not claim physical erasure")
        notice = value.get("erasure_notice", {})
        require(all(notice.get(k) == v and type(notice.get(k)) is type(v) for k, v in NOTICE.items()),
                "missing or misleading erasure notice")
        # Mapping section 1 explicitly discloses first-slice manifest omission.
    return value


def recalled(value: dict[str, Any], scope: dict[str, Any] = SCOPE,
             tenant: str = "@root") -> list[dict[str, Any]]:
    require(value.get("ranking_version") == "memory-ranking-v1" and value.get("selection") == "weighted",
            "missing ranking diagnostics")
    meta = value["meta"]
    require(meta["scopes"] == [{"tenant": tenant, **scope}], "recall widened or changed effective scope")
    require(meta["complete"] is True and meta["truncation"] == [], "unexpected incomplete small read")
    require(meta["limitations"] == ["retained_state_only"], "missing retained-state caveat")
    for key in ("as_of_ms", "ttl_clock_ms", "sequence", "candidates_examined",
                "edges_charged", "vector_components", "mmr_components", "evidence_bytes"):
        decimal(meta[key], signed=key == "as_of_ms")
    memories = value["memories"]
    require(isinstance(memories, list), "memories must be an array")
    for view in memories:
        require(view["scope"] == {"tenant": tenant, **scope}, "foreign scope in recall")
        require(view["stale"] is False and view["status"] == "accepted", "unexpected stale/status label")
        for key in ("revision", "observed_at_ms", "created_at_ms", "updated_at_ms"):
            decimal(view[key], signed=key == "observed_at_ms")
        require(view["segment"] is None and isinstance(view["evidence"], list), "invalid entity/episodic view")
        scores = view["scores"]
        require({"R", "D", "I", "S"} <= scores.keys(), "missing ranking components")
        require(scores["D"] is None and scores["S"] == scores["R"], "default ranking meaning changed")
        for provenance in view.get("provenance", []):
            require(re.fullmatch(r"v1:[0-9a-f]{64}", provenance["content_digest"]) is not None,
                    "invalid provenance content digest")
    return memories


def clean_env(work: Path) -> dict[str, str]:
    env = {k: v for k, v in os.environ.items()
           if not k.upper().startswith(("VLITE_", "OTEL_")) and
           k.upper() not in {"HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "NO_PROXY", "CARGO_TARGET_DIR"}}
    env.update({"PYTHONDONTWRITEBYTECODE": "1", "CARGO_TARGET_DIR": str(work / "target"),
                "CARGO_NET_OFFLINE": "true", "DOTNET_CLI_HOME": str(work / "dotnet-home"),
                "DOTNET_CLI_TELEMETRY_OPTOUT": "1", "DOTNET_SKIP_FIRST_TIME_EXPERIENCE": "1",
                "DOTNET_GENERATE_ASPNET_CERTIFICATE": "false", "DOTNET_NOLOGO": "1",
                "MSBUILDDISABLENODEREUSE": "1",
                "NUGET_PACKAGES": str(work / "nuget"), "NO_PROXY": "127.0.0.1,localhost,::1"})
    return env


class Process:
    def __init__(self, argv: list[str], env: dict[str, str], cwd: Path, report: Report):
        self.report, self.argv = report, argv
        self.process = subprocess.Popen(argv, cwd=cwd, env=env, stdin=subprocess.PIPE,
                                        stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        self.lines: queue.Queue[tuple[str, bytes | None]] = queue.Queue()
        self.output: list[tuple[str, str]] = []
        self.threads = []
        for name in ("stdout", "stderr"):
            stream = getattr(self.process, name)
            thread = threading.Thread(target=self._read, args=(name, stream), daemon=True)
            thread.start()
            self.threads.append(thread)

    def _read(self, name: str, stream: Any) -> None:
        for line in iter(stream.readline, b""):
            self.output.append((name, self.report.redact(line.decode("utf-8", errors="replace"))))
            self.lines.put((name, line))
        self.lines.put((name, None))

    def close(self) -> None:
        if self.process.poll() is None:
            self.process.terminate()
            try:
                self.process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait(timeout=10)
        for thread in self.threads:
            thread.join(timeout=3)
        for stream in (self.process.stdin, self.process.stdout, self.process.stderr):
            if stream:
                stream.close()

    def __enter__(self) -> Process:
        return self

    def __exit__(self, *_: Any) -> None:
        self.close()

    def rpc(self, method: str, params: dict[str, Any], identifier: int, timeout: float = 20) -> Any:
        message = {"jsonrpc": "2.0", "id": identifier, "method": method, "params": params}
        self.notify(message)
        end = time.monotonic() + timeout
        while time.monotonic() < end:
            try:
                name, line = self.lines.get(timeout=max(0.01, end - time.monotonic()))
            except queue.Empty:
                break
            if name != "stdout":
                continue
            require(line is not None, f"MCP closed stdout: {self.output[-8:]}")
            reply = strict_json(line)
            self.report.event(kind="json-rpc", request=message, response=reply)
            if reply.get("id") == identifier:
                require("error" not in reply, f"JSON-RPC error: {reply}")
                return reply["result"]
        raise TimeoutError(f"MCP {method} did not respond: {self.output[-8:]}")

    def notify(self, message: dict[str, Any]) -> None:
        require(self.process.stdin is not None, "process has no stdin")
        self.report.event(kind="json-rpc-send", request=message, response={})
        self.process.stdin.write(encode(message) + b"\n")
        self.process.stdin.flush()


class Server(Process):
    def __init__(self, binary: Path, work: Path, env: dict[str, str], report: Report,
                 token: str, bindings: Path | None = None, create: bool = False):
        argv = [str(binary), str(work / "memory.vlite"), "--token-file", str(work / "admin.token"),
                "--bind", "127.0.0.1:0", "--durability", "full"]
        if create:
            argv += ["--create-if-missing", "3"]
        if bindings:
            argv += ["--memory-bindings-file", str(bindings)]
        super().__init__(argv, env, work, report)
        try:
            end = time.monotonic() + 40
            while time.monotonic() < end:
                if self.process.poll() is not None:
                    raise RuntimeError(f"server exited {self.process.returncode}: {self.output[-12:]}")
                try:
                    _, line = self.lines.get(timeout=0.2)
                except queue.Empty:
                    continue
                if line:
                    match = re.search(rb"LISTENING\s+(http://127\.0\.0\.1:\d+)", line)
                    if match:
                        self.url = match.group(1).decode("ascii")
                        self.http = HTTP(self.url, token, report)
                        require(self.http.request("GET", "/health", token="").status == 200,
                                "LISTENING server is not responsive")
                        return
            raise TimeoutError(f"no loopback LISTENING line: {self.output[-12:]}")
        except BaseException:
            self.close()
            raise


class RecordingProxy:
    """Capture SDK/CLI/tool HTTP verbatim without environment proxies or redirects."""

    def __init__(self, target: str, report: Report):
        parsed = urlsplit(target)
        require(parsed.hostname == "127.0.0.1", "proxy target must be loopback")
        self.target, self.report = parsed, report
        owner = self

        class Handler(http.server.BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *_: Any) -> None:
                pass

            def handle_request(self) -> None:
                raw = self.rfile.read(int(self.headers.get("Content-Length", "0")))
                headers = {k: v for k, v in self.headers.items()
                           if k.lower() not in {"host", "connection", "transfer-encoding"}}
                connection = http.client.HTTPConnection(owner.target.hostname, owner.target.port, timeout=15)
                try:
                    connection.request(self.command, self.path, raw or None, headers)
                    response = connection.getresponse()
                    data = response.read()
                    response_headers = dict(response.getheaders())
                    owner.report.event(kind="proxied-http",
                                       request={"method": self.command, "path": self.path,
                                                "headers": headers, **body_record(raw)},
                                       response={"status": response.status, "headers": response_headers,
                                                 **body_record(data)})
                    self.send_response(response.status)
                    for key, value in response_headers.items():
                        if key.lower() not in {"connection", "transfer-encoding", "content-length"}:
                            self.send_header(key, value)
                    self.send_header("Content-Length", str(len(data)))
                    self.send_header("Connection", "close")
                    self.end_headers()
                    self.wfile.write(data)
                except (OSError, http.client.HTTPException) as exc:
                    owner.report.event(kind="proxy-transport-error", request={"path": self.path},
                                       response={"transport_error": str(exc)})
                    self.close_connection = True
                finally:
                    connection.close()

            do_GET = do_POST = do_PUT = do_DELETE = do_HEAD = do_PATCH = handle_request

        self.server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.server.daemon_threads = True
        self.url = f"http://127.0.0.1:{self.server.server_port}"
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()

    def __enter__(self) -> RecordingProxy:
        return self

    def __exit__(self, *_: Any) -> None:
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=5)


def command(argv: list[str], work: Path, env: dict[str, str], report: Report,
            timeout: int = 120, input_data: bytes | None = None) -> subprocess.CompletedProcess[bytes]:
    process = subprocess.Popen(argv, cwd=work, env=env, stdin=subprocess.PIPE,
                               stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                               start_new_session=os.name != "nt")
    try:
        stdout, stderr = process.communicate(input_data, timeout=timeout)
    except BaseException as exc:
        if process.poll() is None:
            if os.name == "nt":
                # PID-scoped tree termination, including cargo/rustc and MSBuild.
                killed = subprocess.run(["taskkill", "/PID", str(process.pid), "/T", "/F"],
                                        stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=20)
                if killed.returncode != 0 and process.poll() is None:
                    process.kill()
            else:
                os.killpg(process.pid, signal.SIGKILL)
        stdout, stderr = process.communicate()
        report.event(kind="command", request={"argv": argv, "stdin": body_record(input_data or b"")},
                     response={"interrupted": type(exc).__name__, "stdout": stdout.decode(errors="replace"),
                               "stderr": stderr.decode(errors="replace")})
        raise
    finally:
        for stream in (process.stdin, process.stdout, process.stderr):
            if stream:
                stream.close()
    completed = subprocess.CompletedProcess(argv, process.returncode, stdout, stderr)
    report.event(kind="command", request={"argv": argv, "stdin": body_record(input_data or b"")},
                 response={"exit_code": completed.returncode,
                           "stdout": completed.stdout.decode("utf-8", errors="replace"),
                           "stderr": completed.stderr.decode("utf-8", errors="replace")})
    return completed


class Environment:
    def __init__(self, work: Path, report: Report, binary: Path):
        self.work, self.report, self.binary = work, report, binary
        self.env = clean_env(work)
        self.env["VLITE_ADMIN_DIR"] = str(work / "admin")
        self.token = report.secret(secrets.token_urlsafe(40))
        self.read_token = report.secret(secrets.token_urlsafe(40))
        (work / "admin").mkdir()
        (work / "admin.token").write_text(self.token + "\n", encoding="ascii")
        (work / "readonly.token").write_text(self.read_token + "\n", encoding="ascii")
        self.env["VLITE_READ_ONLY_TOKEN_FILE"] = str(work / "readonly.token")
        self.keys: dict[str, str] = {}
        self.ids: dict[str, str] = {}
        self.tenants: list[str] = []
        self.binding_file = work / "bindings.json"

    def server(self, configured: bool = True, create: bool = False) -> Server:
        return Server(self.binary, self.work, self.env, self.report, self.token,
                      self.binding_file if configured else None, create)

    def provision(self, http: HTTP) -> None:
        # Existing administrative REST APIs; no direct edits of persisted stores.
        for graph in ("notes_graph", "other_graph"):
            reply = http.request("POST", "/v1/graphs", {"graph": graph})
            require(reply.status in (200, 201), f"graph setup failed: {reply.status} {reply.raw!r}")
        reply = http.request("POST", "/v1/collections",
                             {"name": "evidence", "dimensions": 3, "settings": {"text_fields": ["text"]}})
        require(reply.status in (200, 201), f"collection setup failed: {reply.status} {reply.raw!r}")
        operations = ["graph_read", "graph_write", "read", "write", "search"]

        def credential(name: str, ops: list[str], graphs: list[str], tenant: str | None = None) -> None:
            request: dict[str, Any] = {"name": name, "collections": ["*"], "graphs": graphs,
                                       "operations": ops, "expires_unix_ms": int(time.time() * 1000) + 3600000}
            if tenant:
                request["tenant_id"] = tenant
            response = http.request("POST", "/v1/admin/credentials", request)
            require(response.status in (200, 201), f"credential setup failed: {response.status} {response.raw!r}")
            value = response.json()
            self.keys[name] = self.report.secret(value["token"])
            identity = value.get("credential", value)["id"]
            require(re.fullmatch(r"[a-z2-7]{12}", identity), "credential API did not return a binding principal ID")
            self.ids[name] = identity

        credential("memory", operations, ["notes_graph", "other_graph"])
        credential("no-grants", operations, ["notes_graph"])
        credential("other-binding", operations, ["other_graph"])
        credential("read-only", ["graph_read", "read", "search"], ["notes_graph"])
        credential("raw", operations, ["notes_graph"])
        scopes = [SCOPE, OTHER_SCOPE, {"agent": "a"},
                  {"agent": "assistant", "user": "ada"}, {"agent": "agent-1"},
                  *({"agent": language} for language in ("python", "javascript", "dotnet", "cli", "mcp"))]

        def binding(name: str, tenant: str, graph: str, grants: list[dict[str, Any]],
                    evidence: str | None = None) -> dict[str, Any]:
            return {"binding": name, "tenant": tenant, "generation": 1, "state": "active",
                    "graph": graph, "evidence_collection": evidence,
                    "schema_version": "memory-schema-v1", "ranking_version": "memory-ranking-v1",
                    "digest_version": "memory-request-v1", "receipt_window_ms": 86400000, "grants": grants}

        def grant(name: str, raw: bool = False) -> dict[str, Any]:
            return {"principal": self.ids[name], "scopes": scopes, "raw_access": raw}

        bindings = [binding("notes", "@root", "notes_graph",
                            [grant("memory"), grant("read-only"), grant("raw", True)], "evidence"),
                    binding("other", "@root", "other_graph", [grant("memory"), grant("other-binding")])]
        for number in (1, 2):
            response = http.request("POST", "/v1/admin/tenants", {"name": f"live-{number}"})
            require(response.status in (200, 201), f"tenant setup failed: {response.raw!r}")
            value = response.json()
            tenant = value.get("tenant", value)["id"]
            self.tenants.append(tenant)
            stored = f"t{tenant}__notes_graph"
            response = http.request("POST", "/v1/graphs", {"graph": stored})
            require(response.status in (200, 201), f"tenant graph setup failed: {response.raw!r}")
            credential(f"tenant-{number}", ["graph_read", "graph_write"], ["notes_graph"], tenant)
            bindings.append(binding("notes", tenant, "notes_graph", [grant(f"tenant-{number}")]))
        self.binding_file.write_bytes(encode({"version": "memory-bindings-v1", "bindings": bindings}))


def run_rest(http: HTTP, env: Environment, report: Report) -> None:
    check = lambda name, fn, clause=CONTRACT + " sections 3,6,8": report.check("rest", name, clause, fn)
    observer = item("observer")
    observer["references"] = [{"id": name, "revision": "0"} for name in ("entity", "episode")]
    base = remember("rest-create", observer, item("entity"), item("episode", "episodic"))
    check("no-credential-before-validation",
          lambda: error(http.memory("remember", {}, token=""), 401, "unauthorized"))
    check("wrong-credential-before-validation",
          lambda: error(http.memory("remember", {}, token="deliberately-invalid"), 401, "unauthorized"))

    def denied_bindings() -> None:
        replies = [http.memory("recall", recall("absent"), binding="unknown"),
                   http.memory("recall", recall("absent"), token=env.keys["no-grants"]),
                   http.memory("recall", recall("absent"), token=env.keys["other-binding"])]
        for reply in replies:
            error(reply, 403, "forbidden_scope")
        require(len({reply.raw for reply in replies}) == 1, "unknown/ungranted bindings disclose existence")
    check("unknown-and-ungranted-byte-identical", denied_bindings, MAPPING + " section 2")
    check("ungranted-exact-scope",
          lambda: error(http.memory("recall", recall("absent", scope={"agent": "ungranted"})),
                        403, "forbidden_scope"))
    check("read-only-token-mutation",
          lambda: error(http.memory("remember", base, token=env.read_token), 403, "read_only"))
    check("read-only-scoped-key-mutation",
          lambda: error(http.memory("remember", base, token=env.keys["read-only"]), 403, "forbidden_scope"))
    for verb, feature in REFUSALS.items():
        check(f"{verb}-refusal-before-body-validation",
              lambda v=verb, f=feature: error(http.memory(v, {"scope": SCOPE}), 409,
                                             "memory_capability_unavailable", feature=f),
              MAPPING + " sections 1,2")
        check(f"{verb}-authorization-before-refusal",
              lambda v=verb: error(http.memory(v, {"scope": SCOPE}, token=env.keys["no-grants"]),
                                  403, "forbidden_scope"), MAPPING + " section 2")

    malformed = [
        ("duplicate-key", b'{"scope":{"agent":"live-e2e"},"scope":{"agent":"live-e2e"},"text":"Ada"}', (400,)),
        ("nested-duplicate", b'{"scope":{"agent":"live-e2e","agent":"live-e2e"},"text":"Ada"}', (400,)),
        ("unknown-field", encode({**recall("entity"), "unknown": True}), (422,)),
        ("wrong-type", encode({**recall("entity"), "limit": "20"}), (422,)),
        ("boolean-integer", encode({**recall("entity"), "limit": True}), (422,)),
        ("trailing-data", encode(recall("entity")) + b"{}", (400,)),
        ("empty-body", b"", (400,)),
        ("required-null", encode({**recall("entity"), "scope": None}), (422,)),
    ]
    for name, raw, statuses in malformed:
        code = "invalid_json" if statuses == (400,) else "invalid_input"
        check(name, lambda raw=raw, statuses=statuses, code=code:
              error(http.memory("recall", None, raw=raw), statuses, code))
    check("wrong-content-type", lambda: error(http.memory("recall", recall("entity"),
          content_type="text/plain"), 415, "invalid_json"))
    check("semantic-bound-details", lambda: error(http.memory("recall",
          {**recall("entity"), "limits": {"candidates": 1001}}), 422, "invalid_input", field=True))

    boundary_seed = check("transport-boundary-seed", lambda: mutation(success(
        http.memory("remember", remember("boundary-seed", item("boundary")))), "remember"))

    def boundary(size: int) -> None:
        prerequisite(boundary_seed, "transport-boundary-seed")
        request = encode(recall("boundary"))
        raw = b" " * (size - len(request)) + request
        require(len(raw) == size, "harness generated incorrect byte boundary")
        reply = http.memory("recall", None, raw=raw)
        if size == CAP:
            recalled(success(reply))
        else:
            error(reply, 413, "invalid_json")
    check("body-exactly-8-MiB", lambda: boundary(CAP))
    check("body-8-MiB-plus-one", lambda: boundary(CAP + 1))

    saved = check("remember-entity-and-episodic", lambda: mutation(success(http.memory("remember", base)), "remember"))

    def exact_replay() -> None:
        prerequisite(saved, "initial remember")
        value = mutation(success(http.memory("remember", base)), "remember", replayed=True)
        require(value == {**saved, "replayed": True}, "replay changed receipt, outcomes or retention")
    check("idempotency-exact-replay", exact_replay)
    check("idempotency-changed-content", lambda: error(http.memory("remember",
          remember("rest-create", item("entity", content="changed"))), 409, "idempotency_conflict"))
    check("idempotency-scope-local", lambda: mutation(success(http.memory("remember",
          remember("rest-create", item("scope-item"), scope=OTHER_SCOPE))), "remember", OTHER_SCOPE))
    check("idempotency-binding-local", lambda: mutation(success(http.memory("remember",
          remember("rest-create", item("binding-item")), binding="other")), "remember"))
    check("remember-receipt-refused", lambda: error(http.memory("receipt",
          {"scope": SCOPE, "operation": "remember", "idempotency_key": "rest-create"}),
          409, "memory_capability_unavailable", feature="remember_receipt_lookup"), MAPPING + " section 3")

    def read_state() -> None:
        prerequisite(saved, "initial remember")
        views = recalled(success(http.memory("recall", recall("observer"))))
        found = {v["id"]: v for v in views}
        require({"observer", "entity", "episode"} <= found.keys(), "remembered items missing from REFERENCES expansion")
        for supplied in base["items"]:
            view = found[supplied["id"]]
            require(view["kind"] == supplied["kind"] and view["content"] == supplied["content"],
                    "content/kind did not round-trip")
            require(view["revision"] == "1" and view["observed_at_ms"] == "90", "wrong revision/observation")
            expected = "v1:" + hashlib.sha256(encode(supplied["content"])).hexdigest()
            require(any(p["content_digest"] == expected for p in view.get("provenance", [])),
                    "content digest must hash the JSON-quoted content")
    check("recall-state-and-decimal-strings", read_state)

    def merge_request(revision: str, key: str) -> dict[str, Any]:
        updated = item("entity")
        updated["source"]["source_id"] = "additional"
        updated.update({"expected": {"revision": revision}, "merge": "merge_provenance",
                        "provenance": [{"source_id": "additional", "source_kind": "caller"}]})
        return remember(key, updated)
    check("revision-guard", lambda: error(http.memory("remember", merge_request("999", "bad-rev")),
                                         412, "precondition_failed"))
    def merge() -> dict[str, Any]:
        prerequisite(saved, "initial remember")
        result = mutation(success(http.memory("remember", merge_request("1", "merge"))), "remember")
        require(result["outcomes"][0] == {"id": "entity", "action": "merged", "revision": "2"},
                "merge did not return revision 2")
        return result
    merged = check("merge-increments-logical-revision", merge)
    check("recall-read-only-key", lambda: recalled(success(http.memory(
        "recall", recall("entity"), token=env.keys["read-only"]))))
    check("integer-input-string-output", lambda: mutation(success(http.memory("remember",
          remember("integer-input", {**item("integer"), "observed_at_ms": 0}))), "remember"))

    plan = consolidation("summary", "episode", "1", "summary")
    for name, patch, feature in (
        ("consolidate-dry-run", {"dry_run": True}, "consolidate_plan"),
        ("consolidate-delete", {"inputs": [{"id": "episode", "revision": "1",
                                           "disposition": "logical_delete"}]}, "consolidate_delete"),
    ):
        check(name, lambda p=patch, f=feature: error(http.memory("consolidate", {**plan, **p}), 409,
              "memory_capability_unavailable", feature=f), MAPPING + " section 1")
    check("consolidate-duplicate-input", lambda: error(http.memory("consolidate",
          {**plan, "inputs": plan["inputs"] * 2}), 422, "invalid_input"))
    invalid_output = copy.deepcopy(plan)
    del invalid_output["output"]["source"]["transformation_id"]
    check("consolidate-transformation-required",
          lambda: error(http.memory("consolidate", invalid_output), 422, "invalid_input"))
    check("consolidate-stale-input", lambda: error(http.memory("consolidate",
          {**plan, "inputs": [{"id": "episode", "revision": "99"}]}), 412, "precondition_failed"))
    consolidated = check("consolidate-C1", lambda: mutation(success(http.memory("consolidate", plan)), "consolidate"))

    def consolidation_state() -> None:
        prerequisite(consolidated, "consolidate-C1")
        views = recalled(success(http.memory("recall", recall("observer"))))
        found = {v["id"]: v for v in views}
        require("episode" in found, "C1 must retain the source")
        require(consolidated["outcomes"] == [{"id": "summary", "action": "created", "revision": "1"}],
                "C1 must create the caller's episodic summary at revision 1")
    check("consolidate-retains-source", consolidation_state)

    def lookup(operation: str, key: str, original: dict[str, Any] | None) -> None:
        prerequisite(original, f"initial {operation}")
        result = success(http.memory("receipt", {"scope": SCOPE, "operation": operation,
                                                "idempotency_key": key}))["result"]
        mutation(result, operation, replayed=True)
        require(result == {**original, "replayed": True}, "receipt lookup changed original stable result")
    check("consolidate-receipt-lookup", lambda: lookup("consolidate", "summary", consolidated))
    check("missing-receipt", lambda: error(http.memory("receipt",
          {"scope": SCOPE, "operation": "consolidate", "idempotency_key": "absent"}), 404, "receipt_not_found"))
    check("forget-stale-revision", lambda: error(http.memory("forget", forgetting("bad-forget", "entity", "1")),
                                                412, "precondition_failed"))
    def delete() -> dict[str, Any]:
        prerequisite(merged, "merge-increments-logical-revision")
        return mutation(success(http.memory("forget", forgetting("forget", "entity", "2"))), "forget")
    deleted = check("forget-logical-delete-notice", delete)
    check("forget-receipt-lookup", lambda: lookup("forget", "forget", deleted))

    def after_delete() -> None:
        prerequisite(deleted, "forget-logical-delete-notice")
        views = recalled(success(http.memory("recall", recall("observer"))))
        require(all(v["id"] != "entity" for v in views), "forgotten memory remains visible")
    check("forget-then-recall", after_delete)
    check("forget-missing-target", lambda: error(http.memory("forget", forgetting("missing", "absent", "1")),
                                                404, "memory_not_found"))
    def replay_after_delete() -> None:
        prerequisite(deleted, "forget-logical-delete-notice")
        exact_replay()
    check("replay-after-delete", replay_after_delete)

    def raw_denials() -> None:
        # Without raw_access a reserved resource answers exactly like a missing one, even for
        # the administrator token and a credential that names every collection
        # (docs/SECURITY.md Memory-bound resources). The grant check still runs first, so a
        # graph-scoped credential is not compared on graphs outside its binding.
        graph = "/v1/graphs/notes_graph"
        for token, method, path, body, status, code in (
            (env.token, "POST", graph + "/nodes/get", {"node": {"key": "x"}}, 409, "graph_not_found"),
            (env.token, "POST", graph + "/batch", {"ops": [{"op": "create_node", "key": "x"}]},
             409, "graph_not_found"),
            (env.token, "GET", "/v1/collections/evidence/vectors/1", None, 404, "collection_not_found"),
            (env.keys["memory"], "GET", "/v1/collections/evidence/vectors/1", None, 404, "collection_not_found"),
            (env.token, "PUT", "/v1/collections/evidence/vectors/1", {"vector": [1, 0, 0]},
             404, "collection_not_found"),
            (env.keys["memory"], "PUT", "/v1/collections/evidence/vectors/1", {"vector": [1, 0, 0]},
             404, "collection_not_found"),
            (env.token, "POST", "/v1/collections/evidence/search", {"vector": [1, 0, 0], "k": 1},
             404, "collection_not_found"),
            (env.keys["memory"], "POST", "/v1/collections/evidence/search", {"vector": [1, 0, 0], "k": 1},
             404, "collection_not_found"),
        ):
            reserved = http.request(method, path, body, token=token)
            error(reserved, status, code)
            absent = path.replace("notes_graph", "absent_graph").replace("evidence", "absent_evidence")
            missing = http.request(method, absent, body, token=token)
            require(reserved.status == missing.status and reserved.json() == missing.json(),
                    f"{method} {path}: a reserved resource answered differently from a missing one")
    check("raw-reserved-answers-like-missing", raw_denials, "docs/SECURITY.md Memory-bound resources")
    check("raw-access-still-needs-normal-grants", lambda: require(
        http.request("GET", "/v1/collections/evidence/vectors", token=env.keys["raw"]).status == 200,
        "explicit raw_access with ordinary read permission should permit access"),
        "docs/SECURITY.md Memory-bound resources")

    def tenant_isolation() -> None:
        for number, tenant in enumerate(env.tenants, 1):
            key = env.keys[f"tenant-{number}"]
            mutation(success(http.memory("remember", remember("tenant-key", item("tenant-item",
                     content=f"tenant-{number}")), token=key)), "remember", tenant=tenant)
            views = recalled(success(http.memory("recall", recall("tenant-item"), token=key)), tenant=tenant)
            require([v["content"] for v in views] == [f"tenant-{number}"], "cross-tenant memory leaked")
    check("tenant-derived-identity-and-isolation", tenant_isolation)

    def request_id() -> None:
        reply = http.memory("recall", recall("boundary"), headers={"x-request-id": "live-e2e-request"})
        success(reply)
        require(bool(reply.headers.get("x-request-id")), "the response carries no x-request-id")
    check("request-id-present", request_id)
    check("complete-feed-refusal", lambda: error(http.memory("remember",
          {**remember("complete-feed", item("never-created")), "require_complete_feed": True}),
          409, "memory_feed_unsupported"), MAPPING + " section 6")


def run_default(http: HTTP, report: Report) -> None:
    for verb in ROUTES:
        report.check("default", verb + "-not-mounted", MAPPING + " section 6 B1a",
                     lambda v=verb: require(http.memory(v, {"scope": SCOPE}).status == 404,
                                             "unconfigured memory route must be plain 404"))

    def vector_roundtrip() -> None:
        require(http.request("PUT", "/v1/vectors/42", {"vector": [1, 0, 0], "payload_base64": "bGl2ZQ=="}).status
                == 204, "legacy vector write failed")
        record = success(http.request("GET", "/v1/vectors/42"))
        require(record["id"] == "42" and record["vector"] == [1, 0, 0] and
                record["payload_base64"] == "bGl2ZQ==", "legacy vector read changed")
        hits = success(http.request("POST", "/v1/search", {"vector": [1, 0, 0], "k": 1, "mode": "exact"}))["hits"]
        require(hits[0]["id"] == "42" and hits[0]["distance"] == 0, "legacy exact search changed")
        require(success(http.request("DELETE", "/v1/vectors/42"))["deleted"] is True, "legacy delete changed")
    report.check("default", "ordinary-vector-roundtrip", "docs/REFERENCE.md Routes and requests", vector_roundtrip)


def sdk_requests(language: str) -> list[dict[str, Any]]:
    scope = {"agent": language}
    saved = remember(language + "-remember", item(language), scope=scope)
    plan = consolidation(language + "-consolidate", language, "1", language + "-summary")
    plan["scope"] = scope
    deleted = forgetting(language + "-forget", language, "1")
    deleted["scope"] = scope
    refusals = []
    for verb in ("facts", "correct-facts", "erasure", "resume-erasure"):
        request = strict_json((ROOT / "tests" / "fixtures" / "memory_cli" / (verb + ".json")).read_bytes())
        request["scope"] = scope
        refusals.append(request)
    return [saved, recall(language, scope=scope), plan,
            {"scope": scope, "operation": "consolidate", "idempotency_key": language + "-consolidate"},
            deleted, {"scope": scope, "operation": "forget", "idempotency_key": language + "-forget"},
            *refusals]


def verify_sdk(values: list[Any], language: str) -> None:
    scope = {"agent": language}
    require(len(values) == 10, "SDK did not return all ten calls across its nine methods")
    if language == "dotnet":
        # Typed optional scope members serialize as null; wire presence was
        # captured separately by the recording proxy.
        def scopes(value: Any) -> None:
            if isinstance(value, dict):
                for key in ("scope",):
                    effective = value.get(key)
                    if isinstance(effective, dict):
                        for optional in ("user", "session"):
                            if effective.get(optional) is None:
                                effective.pop(optional, None)
                for effective in value.get("scopes", []):
                    for optional in ("user", "session"):
                        if effective.get(optional) is None:
                            effective.pop(optional, None)
                for child in value.values():
                    scopes(child)
            elif isinstance(value, list):
                for child in value:
                    scopes(child)
        scopes(values)
    for value, operation in zip(values[:6], ("remember", "recall", "consolidate",
                                             "consolidate receipt", "forget", "forget receipt")):
        require("status" not in value, f"SDK {operation} returned HTTP {value.get('status')}/"
                f"{value.get('code')} instead of the full successful result")
    mutation(values[0], "remember", scope)
    views = recalled(values[1], scope)
    require(any(v["id"] == language for v in views), "SDK recall lost remembered item")
    mutation(values[2], "consolidate", scope)
    require(values[3]["result"] == {**values[2], "replayed": True}, "SDK consolidate receipt differs")
    mutation(values[4], "forget", scope)
    require(values[5]["result"] == {**values[4], "replayed": True}, "SDK forget receipt differs")
    for refusal, feature in zip(values[6:], ("facts", "facts", "erasure_jobs", "erasure_jobs")):
        require(refusal["status"] == 409 and refusal["code"] == "memory_capability_unavailable" and
                refusal["details"]["feature"] == feature, "SDK lost refusal status/code/details")
        require(bool(refusal["request_id"]), "SDK lost request ID on error")


def python_client() -> Any:
    path = str(ROOT / "clients" / "python")
    if path not in sys.path:
        sys.path.insert(0, path)
    return importlib.import_module("vlite_client")


def run_python(url: str, token: str, report: Report) -> None:
    def run() -> None:
        module = python_client()
        memory = module.Client(url, token).memory("notes")
        requests = sdk_requests("python")
        values = []
        methods = ("remember", "recall", "consolidate", "get_receipt", "forget", "get_receipt",
                   "find_facts", "correct_facts", "get_erasure", "resume_erasure")
        for name, request in zip(methods, requests):
            try:
                values.append(getattr(memory, name)(request))
            except module.VLiteError as exc:
                values.append({"status": exc.status, "code": exc.code, "details": exc.details,
                               "request_id": exc.request_id})
        verify_sdk(values, "python")
    report.check("python", "facade-lifecycle-and-coded-refusal", "clients/python/README.md Agent memory", run)


def install_js_package(work: Path) -> None:
    target = work / "node_modules" / "@vlite" / "client"
    if not target.exists():
        shutil.copytree(ROOT / "clients" / "javascript", target,
                        ignore=shutil.ignore_patterns("node_modules", ".git"))


def run_javascript(url: str, token: str, work: Path, env: dict[str, str], report: Report) -> None:
    node = shutil.which("node")
    if not node:
        report.skip("javascript", "facade-lifecycle-and-coded-refusal", "node is not on PATH")
        return

    def run() -> None:
        install_js_package(work)
        requests = work / "javascript-requests.json"
        requests.write_bytes(encode(sdk_requests("javascript")))
        script = work / "javascript-live.mjs"
        script.write_text('''import { Client, VLiteError } from "@vlite/client";
import { readFileSync } from "node:fs";
const m = new Client(process.env.VLITE_URL, process.env.VLITE_AUTH_TOKEN).memory("notes");
const requests = JSON.parse(readFileSync(process.env.LIVE_REQUESTS, "utf8"));
const values = [];
for (const [i, method] of ["remember","recall","consolidate","getReceipt","forget","getReceipt",
                         "findFacts","correctFacts","getErasure","resumeErasure"].entries()) {
  try { values.push(await m[method](requests[i])); }
  catch (e) {
    if (!(e instanceof VLiteError)) throw e;
    values.push({status:e.status, code:e.code, details:e.details, request_id:e.requestId});
  }
}
console.log(JSON.stringify(values));
''', encoding="utf-8")
        result = command([node, str(script)], work,
                         {**env, "VLITE_URL": url, "VLITE_AUTH_TOKEN": token,
                          "LIVE_REQUESTS": str(requests)}, report)
        require(result.returncode == 0, f"JavaScript client failed: {result.stderr.decode(errors='replace')}")
        verify_sdk(strict_json(result.stdout), "javascript")
    report.check("javascript", "facade-lifecycle-and-coded-refusal", "clients/javascript/README.md Memory operations", run)


def dotnet_project(work: Path) -> Path:
    project = work / "dotnet"
    if project.exists():
        return project
    project.mkdir()
    shutil.copytree(ROOT / "clients" / "dotnet" / "VLite.Client", project / "VLite.Client",
                    ignore=shutil.ignore_patterns("bin", "obj"))
    (project / "NuGet.Config").write_text(
        '<configuration><packageSources><clear /></packageSources></configuration>', encoding="utf-8")
    (project / "Live.csproj").write_text(r'''<Project Sdk="Microsoft.NET.Sdk">
<PropertyGroup><OutputType>Exe</OutputType><TargetFramework>net8.0</TargetFramework>
<ImplicitUsings>enable</ImplicitUsings><Nullable>enable</Nullable></PropertyGroup>
<ItemGroup><Compile Remove="VLite.Client\\**\\*.cs" />
<ProjectReference Include="VLite.Client\\VLite.Client.csproj" /></ItemGroup></Project>''', encoding="utf-8")
    return project


def run_dotnet(url: str, token: str, work: Path, env: dict[str, str], report: Report) -> None:
    binary = shutil.which("dotnet")
    if not binary:
        report.skip("dotnet", "typed-facade-lifecycle-and-refusal", "dotnet is not on PATH")
        return

    def run() -> None:
        project = dotnet_project(work)
        requests = work / "dotnet-requests.json"
        requests.write_bytes(encode(sdk_requests("dotnet")))
        (project / "Program.cs").write_text('''using VLite;
using System.Text.Json;
using var client = new Client(Environment.GetEnvironmentVariable("VLITE_URL")!,
                        Environment.GetEnvironmentVariable("VLITE_AUTH_TOKEN")!);
var memory = client.Memory("notes");
using var requests = JsonDocument.Parse(File.ReadAllText(Environment.GetEnvironmentVariable("LIVE_REQUESTS")!));
T Read<T>(int index) => JsonSerializer.Deserialize<T>(requests.RootElement[index].GetRawText())!;
var values = new List<object>();
async Task Add<T>(Func<Task<T>> operation) {
    try { values.Add((await operation())!); }
    catch (VLiteException e) {
        values.Add(new {status=e.Status, code=e.Code, details=new {feature=e.Details?.Feature},
                        request_id=e.RequestId});
    }
}
await Add(() => memory.RememberAsync(Read<MemoryRememberRequest>(0)));
await Add(() => memory.RecallAsync(Read<MemoryRecallRequest>(1)));
await Add(() => memory.ConsolidateAsync(Read<MemoryConsolidateRequest>(2)));
await Add(() => memory.GetReceiptAsync(Read<MemoryReceiptRequest>(3)));
await Add(() => memory.ForgetAsync(Read<MemoryForgetRequest>(4)));
await Add(() => memory.GetReceiptAsync(Read<MemoryReceiptRequest>(5)));
await Add(() => memory.FindFactsAsync(Read<MemoryFactsRequest>(6)));
await Add(() => memory.CorrectFactsAsync(Read<MemoryCorrectionRequest>(7)));
await Add(() => memory.GetErasureAsync(Read<MemoryErasureRequest>(8)));
await Add(() => memory.ResumeErasureAsync(Read<MemoryResumeRequest>(9)));
Console.WriteLine(JsonSerializer.Serialize(values));
''', encoding="utf-8")
        result = command([binary, "run", "--project", str(project / "Live.csproj"), "--configuration", "Release",
                          "--disable-build-servers", "-p:UseSharedCompilation=false"],
                         project, {**env, "VLITE_URL": url, "VLITE_AUTH_TOKEN": token,
                                   "LIVE_REQUESTS": str(requests)}, report, timeout=180)
        require(result.returncode == 0, f".NET client failed: {result.stdout[-3000:]!r} {result.stderr[-3000:]!r}")
        lines = [line for line in result.stdout.splitlines() if line.startswith(b"[{")]
        require(len(lines) == 1, "missing .NET structured result")
        verify_sdk(strict_json(lines[0]), "dotnet")
    report.check("dotnet", "typed-facade-lifecycle-and-refusal", "clients/README.md .NET agent-memory operations", run)


def cli_result(completed: subprocess.CompletedProcess[bytes], verb: str) -> dict[str, Any]:
    if verb in REFUSALS:
        require(completed.returncode == 4 and not completed.stdout,
                f"CLI refusal must exit 4 with empty stdout; got {completed.returncode}: {completed.stderr[:1000]!r}")
        lines = completed.stderr.splitlines()
        require(len(lines) == 1, "CLI failure must print one coded JSON line")
        result = strict_json(lines[0])
        require(set(result) == {"error"} and set(result["error"]) == {"code", "message"} and
                result["error"]["code"] == "memory_capability_unavailable", "CLI lost coded refusal")
        return result
    require(completed.returncode == 0 and not completed.stderr, f"CLI success failed: {completed.stderr!r}")
    require(len(completed.stdout.splitlines()) == 1, "CLI must print exactly one success JSON line")
    value = strict_json(completed.stdout)
    require(isinstance(value, dict) and "error" not in value, "invalid CLI success envelope")
    return value


def run_cli(binary: Path | None, url: str, token: str, work: Path,
            env: dict[str, str], report: Report) -> None:
    if binary is None or not binary.is_file():
        report.skip("cli", "binary", "vlite-memory-cli not found; supply --cli-bin or build all binaries")
        return
    requests = sdk_requests("cli")
    fixtures = {verb: strict_json((ROOT / "tests" / "fixtures" / "memory_cli" / (verb + ".json")).read_bytes())
                for verb in ROUTES}
    requests_by_verb = {"remember": requests[0], "recall": requests[1], "consolidate": requests[2],
                        "receipt": requests[3], "forget": requests[4]}
    for verb, request in fixtures.items():
        request = copy.deepcopy(requests_by_verb.get(verb, request))
        request["scope"] = {"agent": "cli"}
        path = work / (verb + ".json")
        path.write_bytes(encode(request))

    def execute(verb: str) -> None:
        result = command([str(binary), "--url", url, "--binding", "notes", "--token-env", "VLITE_TOKEN",
                          verb, "--request", str(work / (verb + ".json"))],
                         work, {**env, "VLITE_TOKEN": token}, report)
        value = cli_result(result, verb)
        if verb in ("remember", "consolidate", "forget"):
            mutation(value, verb, {"agent": "cli"})
        elif verb == "recall":
            require(any(v["id"] == "cli" for v in recalled(value, {"agent": "cli"})), "CLI lost remembered item")
        elif verb == "receipt":
            mutation(value["result"], "consolidate", {"agent": "cli"}, replayed=True)
    for verb in ("remember", "recall", "consolidate", "receipt", "forget",
                 "facts", "correct-facts", "erasure", "resume-erasure"):
        report.check("cli", verb, "docs/TOOLS.md Remote agent memory", lambda v=verb: execute(v))

    def https_required() -> None:
        # A non-loopback HTTP URL must be refused locally, before any connection.
        result = command([str(binary), "--url", "http://0.0.0.0:9", "--binding", "notes",
                          "--token-env", "VLITE_TOKEN", "recall", "--request", str(work / "recall.json")],
                         work, {**env, "VLITE_TOKEN": token}, report, timeout=10)
        require(result.returncode == 2 and not result.stdout, "non-loopback plaintext was not refused locally")
        value = strict_json(result.stderr)
        require(isinstance(value.get("error", {}).get("code"), str), "HTTPS refusal lacks coded error")
    report.check("cli", "HTTPS-required-except-loopback", "docs/TOOLS.md Remote agent memory", https_required)


def start_mcp(binary: Path, url: str, token: str, work: Path,
              env: dict[str, str], report: Report, writable: bool) -> Process:
    process = Process([str(binary), "--server", url, "--token-env", "VLITE_TOKEN",
                       "--writable" if writable else "--read-only"], {**env, "VLITE_TOKEN": token}, work, report)
    try:
        process.rpc("initialize", {"protocolVersion": "2024-11-05", "capabilities": {},
                                   "clientInfo": {"name": "memory-live-e2e", "version": "1"}}, 1)
        process.notify({"jsonrpc": "2.0", "method": "notifications/initialized"})
        return process
    except BaseException:
        process.close()
        raise


def tool_value(result: dict[str, Any], code: str | None = None) -> Any:
    if code:
        require(result.get("isError") is True, "MCP refusal was success-shaped")
        value = result["structuredContent"]["error"]
        require(isinstance(value, str) and value.startswith(code + ": "),
                f"MCP failure must be a {code}: <message> string, got {value!r}")
        return value
    else:
        require(result.get("isError", False) is False, f"MCP call failed: {result}")
        value = result["structuredContent"]
    content = result["content"]
    require(len(content) == 1 and content[0]["type"] == "text", "MCP requires one JSON text block")
    require(strict_json(content[0]["text"]) == result["structuredContent"], "MCP text/structured content differ")
    return value


def run_mcp(binary: Path | None, url: str, token: str, work: Path,
            env: dict[str, str], report: Report) -> None:
    if binary is None or not binary.is_file():
        report.skip("mcp", "binary", "vlite-mcp not found; supply --mcp-bin or build all binaries")
        return
    fixtures = strict_json((ROOT / "tests" / "fixtures" / "memory_tools" / "exchanges.json").read_bytes())["cases"]
    expected = {case["name"]: case for case in fixtures}

    def readonly() -> None:
        with start_mcp(binary, url, token, work, env, report, False) as process:
            tools = process.rpc("tools/list", {}, 2)["tools"]
            memory = {t["name"] for t in tools if t["name"].startswith("vlite_memory_")}
            require(memory == {n for n, c in expected.items() if c["read_only"]},
                    "read-only MCP must list exactly the four memory read tools")
            result = process.rpc("tools/call", {"name": "vlite_memory_remember",
                                  "arguments": {"binding": "notes", **sdk_requests("mcp")[0]}}, 3)
            require(result.get("isError") is True, "read-only MCP permitted a write")
    report.check("mcp", "read-only-discovery-and-write-denial", "docs/MCP.md Server-backed agent memory", readonly)

    def writable() -> None:
        with start_mcp(binary, url, token, work, env, report, True) as process:
            tools = process.rpc("tools/list", {}, 2)["tools"]
            memory = {t["name"]: t for t in tools if t["name"].startswith("vlite_memory_")}
            require(memory.keys() == expected.keys(), "writable MCP must list all nine memory tools")
            for name, tool in memory.items():
                schema = tool["inputSchema"]
                require(schema.get("additionalProperties") is False and
                        "binding" in schema["required"] and "request" not in schema["properties"],
                        f"{name} must use closed, flat arguments")
                require(tool["annotations"]["readOnlyHint"] is expected[name]["read_only"],
                        f"{name} has wrong readOnlyHint")
            requests = sdk_requests("mcp")
            saved = tool_value(process.rpc("tools/call", {"name": "vlite_memory_remember",
                               "arguments": {"binding": "notes", **requests[0]}}, 3))
            mutation(saved, "remember", {"agent": "mcp"})
            views = tool_value(process.rpc("tools/call", {"name": "vlite_memory_recall",
                               "arguments": {"binding": "notes", **requests[1]}}, 4))
            require(any(v["id"] == "mcp" for v in recalled(views, {"agent": "mcp"})), "MCP recall lost item")
            tool_value(process.rpc("tools/call", {"name": "vlite_memory_facts",
                       "arguments": {"binding": "notes", **requests[6]}}, 5), "memory_capability_unavailable")
    report.check("mcp", "nine-tools-remember-recall-refusal", "docs/MCP.md Server-backed agent memory", writable)


def fenced_examples() -> list[tuple[str, int, str, str]]:
    examples = []
    for relative in DOCS:
        text = (ROOT / Path(relative)).read_text(encoding="utf-8")
        for match in re.finditer(r"(?m)^```([^\n]*)\n(.*?)^```[ \t]*$", text, re.S):
            language, body = match.group(1).strip().lower(), match.group(2)
            if (re.search(r"\.memory\(|\.Memory\(|vlite-memory-cli|vlite_memory_|vlite-mcp --server", body)
                    or (language == "jsonl" and '"operation":"remember"' in body)
                    or ("/v1/memory/" in body and language not in ("yaml", "proto"))):
                line = text.count("\n", 0, match.start()) + 1
                if language == "jsonl":
                    examples.extend((relative, line + offset + 1, language, text)
                                    for offset, text in enumerate(body.splitlines()) if text.strip())
                else:
                    examples.append((relative, line, language, body))
    return examples


def run_docs(http: HTTP, url: str, token: str, binary_cli: Path | None, binary_mcp: Path | None,
             work: Path, env: dict[str, str], report: Report) -> None:
    examples = fenced_examples()
    require(bool(examples), "no fenced memory examples found in authority documents")
    for number, (relative, line, language, body) in enumerate(examples, 1):
        name = f"{relative}:{line}"

        def execute(language: str = language, body: str = body, number: int = number) -> None:
            report.event(kind="documentation", request={"source": relative, "line": line,
                                                       "language": language, "example": body}, response={})
            if language == "python":
                python_client()
                script = work / f"docs-{number}.py"
                script.write_text(body, encoding="utf-8")
                result = command([sys.executable, "-B", str(script)], work,
                                 {**env, "PYTHONPATH": str(ROOT / "clients" / "python"),
                                  "VLITE_URL": url, "VLITE_AUTH_TOKEN": token}, report)
                require(result.returncode == 0, f"unaltered Python example failed: {result.stderr!r}")
            elif language in ("javascript", "js"):
                node = shutil.which("node")
                if not node:
                    raise SkipCheck("node is not on PATH")
                install_js_package(work)
                script = work / f"docs-{number}.mjs"
                script.write_text(body, encoding="utf-8")
                result = command([node, str(script)], work,
                                 {**env, "VLITE_URL": url, "VLITE_AUTH_TOKEN": token}, report)
                require(result.returncode == 0, f"unaltered JavaScript example failed: {result.stderr!r}")
            elif language == "jsonl":
                # The document explicitly labels these independently seeded exchanges,
                # not clock/receipt-ID assertions against an uncontrolled live clock.
                for text in body.splitlines():
                    if not text.strip():
                        continue
                    exchange = strict_json(text)
                    verb = exchange["operation"].replace("_", "-")
                    request = exchange["request"]
                    if verb == "forget":
                        seed = item("m1", "episodic", "Document forget input")
                        success(http.memory("remember", remember("docs-seed", seed, scope={"agent": "a"})))
                        for revision in range(1, 7):
                            merged = {**seed, "expected": {"revision": str(revision)}, "merge": "merge_provenance",
                                      "source": {"source_id": f"docs-{revision}", "source_kind": "caller"}}
                            success(http.memory("remember", remember(f"docs-merge-{revision}", merged,
                                                                     scope={"agent": "a"})))
                    reply = http.memory(verb, request)
                    if verb in REFUSALS:
                        error(reply, 409, "memory_capability_unavailable", feature=REFUSALS[verb])
                    elif verb == "consolidate":
                        error(reply, 409, "memory_capability_unavailable", feature="consolidate_plan")
                    elif verb in ("remember", "forget"):
                        mutation(success(reply), verb, {"agent": "a"})
                    elif verb == "receipt":
                        mutation(success(reply)["result"], "forget", {"agent": "a"}, replayed=True)
                    else:
                        require(any(v["id"] == "e1" for v in recalled(success(reply), {"agent": "a"})),
                                "document recall did not find its seeded entity")
            elif language in ("powershell", "sh", "shell", "text"):
                commands = [s for s in body.splitlines() if s.strip() and not s.lstrip().startswith("#")]
                if all(s.startswith("vlite-memory-cli ") for s in commands):
                    if binary_cli is None:
                        raise SkipCheck("vlite-memory-cli unavailable")
                    # The guide names files but never defines their contents. Do not
                    # replace the documented request with a harness-invented example.
                    raise SkipCheck("OPEN-LE-01: guide names remember.json/recall.json without their contents")
                if len(commands) == 1 and commands[0].startswith("vlite-mcp --server "):
                    if binary_mcp is None:
                        raise SkipCheck("vlite-mcp unavailable")
                    with start_mcp(binary_mcp, url, token, work, env, report, False) as process:
                        names = {t["name"] for t in process.rpc("tools/list", {}, 2)["tools"]}
                        require("vlite_memory_recall" in names, "document MCP command lacks recall")
                else:
                    raise SkipCheck("OPEN-LE-02: fenced declaration is not a standalone executable memory example")
            elif language in ("csharp", "cs"):
                if not shutil.which("dotnet"):
                    raise SkipCheck("dotnet is not on PATH")
                raise SkipCheck("OPEN-LE-03: C# fragment leaves client and observedAtMs undefined; only URL/token substitution allowed")
            else:
                raise SkipCheck(f"OPEN-LE-04: no executable interpretation for {language!r} fence")
        report.check("docs", name, f"{relative}:{line} (executed without changing example code)", execute)


def build_binaries(work: Path, env: dict[str, str], report: Report) -> tuple[Path, Path, Path]:
    result = command(["cargo", "build", "--locked", "--release", "--features", "grpc,tools,mcp-proxy",
                      "--bin", "vlite-server", "--bin", "vlite-memory-cli", "--bin", "vlite-mcp"],
                     ROOT, env, report, timeout=1800)
    require(result.returncode == 0, f"release build failed (offline, no external services): "
            f"{result.stderr.decode(errors='replace')[-6000:]}")
    suffix = ".exe" if os.name == "nt" else ""
    folder = work / "target" / "release"
    binaries = tuple(folder / (name + suffix) for name in ("vlite-server", "vlite-memory-cli", "vlite-mcp"))
    require(all(p.is_file() for p in binaries), "build did not produce all three binaries")
    return binaries


def run(args: argparse.Namespace) -> int:
    report = Report()
    selected = tuple(args.only.split(",")) if args.only else SECTIONS
    require(all(s in SECTIONS for s in selected), "--only contains an unknown section")
    parent = Path(args.work).resolve() if args.work else Path(tempfile.gettempdir()).resolve()
    require(not parent.is_relative_to(ROOT), "--work and the system temp directory must be outside the checkout")
    parent.mkdir(parents=True, exist_ok=True)
    work = Path(tempfile.mkdtemp(prefix="vlite-memory-live-", dir=parent))
    if os.name != "nt":
        work.chmod(0o700)
    try:
        if args.server_bin:
            server_bin = Path(args.server_bin).resolve()
            require(server_bin.is_file(), "--server-bin does not exist")
            suffix = ".exe" if os.name == "nt" else ""
            cli = Path(args.cli_bin).resolve() if args.cli_bin else server_bin.with_name("vlite-memory-cli" + suffix)
            mcp = Path(args.mcp_bin).resolve() if args.mcp_bin else server_bin.with_name("vlite-mcp" + suffix)
        else:
            binaries = report.check("setup", "build", "documented release build", lambda:
                                    build_binaries(work, clean_env(work), report))
            if binaries is None:
                for section in selected:
                    report.skip(section, "blocked", "release build failed; see setup/build")
                return 1
            server_bin, cli, mcp = binaries
            if args.cli_bin:
                cli = Path(args.cli_bin).resolve()
            if args.mcp_bin:
                mcp = Path(args.mcp_bin).resolve()
        environment = Environment(work, report, server_bin)
        if "default" in selected:
            default_dir = work / "default"
            default_dir.mkdir()
            default_env = Environment(default_dir, report, server_bin)
            with default_env.server(configured=False, create=True) as server:
                run_default(server.http, report)
            report.section_summary("default")
        if any(s != "default" for s in selected):
            with environment.server(configured=False, create=True) as server:
                environment.provision(server.http)
            with environment.server() as server:
                http = HTTP(server.url, environment.keys["memory"], report)
                if "rest" in selected:
                    run_rest(http, environment, report)
                    report.section_summary("rest")
                with RecordingProxy(server.url, report) as proxy:
                    for section, fn in (
                        ("python", lambda: run_python(proxy.url, environment.keys["memory"], report)),
                        ("javascript", lambda: run_javascript(proxy.url, environment.keys["memory"], work,
                                                             environment.env, report)),
                        ("dotnet", lambda: run_dotnet(proxy.url, environment.keys["memory"], work, environment.env, report)),
                        ("cli", lambda: run_cli(cli, proxy.url, environment.keys["memory"], work, environment.env, report)),
                        ("mcp", lambda: run_mcp(mcp, proxy.url, environment.keys["memory"], work, environment.env, report)),
                        ("docs", lambda: run_docs(http, proxy.url, environment.keys["memory"], cli, mcp, work,
                                                 environment.env, report)),
                    ):
                        if section in selected:
                            try:
                                fn()
                            except Exception as exc:
                                def failed(exc: Exception = exc) -> None:
                                    raise exc
                                report.check(section, "section-setup", "documented adapter setup", failed)
                            report.section_summary(section)
        return int(report.failed)
    except (Exception, KeyboardInterrupt) as exc:
        def failed() -> None:
            raise RuntimeError(f"{type(exc).__name__}: {exc}") from exc
        report.check("setup", "environment", "documented disposable server provisioning", failed)
        for section in selected:
            if not any(r.section == section for r in report.results):
                report.skip(section, "blocked", "server/build/provisioning failed; see setup failure and recorded exchanges")
        return 1
    finally:
        try:
            report.summary(("setup", *selected))
            if args.json:
                report.write(Path(args.json).resolve())
        finally:
            if args.keep:
                print(f"Kept private disposable workspace (contains credentials): {work}")
            else:
                shutil.rmtree(work)


class FakeMemory:
    """Small independent HTTP double, not a model of production internals."""

    def __init__(self, fault: str = ""):
        self.fault = fault
        self.memories: dict[str, dict[str, Any]] = {}
        self.saved: dict[str, tuple[dict[str, Any], dict[str, Any]]] = {}
        self.sequence = 6
        self.token = "self-test-disposable-token"
        owner = self

        class Handler(http.server.BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *_: Any) -> None:
                pass

            def do_POST(self) -> None:
                raw = self.rfile.read(int(self.headers.get("Content-Length", "0")))
                status, value = owner.respond(self.path, self.headers, raw)
                data = encode(value)
                self.send_response(status)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(data)))
                if self.headers.get("x-request-id") and owner.fault != "request-id":
                    self.send_header("x-request-id", self.headers["x-request-id"])
                self.end_headers()
                self.wfile.write(data)

        self.server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.server.daemon_threads = True
        self.url = f"http://127.0.0.1:{self.server.server_port}"
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()

    def __enter__(self) -> FakeMemory:
        return self

    def __exit__(self, *_: Any) -> None:
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=5)

    def respond(self, path: str, headers: Any, raw: bytes) -> tuple[int, dict[str, Any]]:
        def fail(status: int, code: str, feature: str | None = None) -> tuple[int, dict[str, Any]]:
            error_body: dict[str, Any] = {"code": code, "message": "Fake sanitized error"}
            if feature and self.fault != "capability-details":
                error_body["details"] = {"feature": feature}
            return status, {"error": error_body}
        if headers.get("Authorization") != "Bearer " + self.token and self.fault != "authentication":
            return fail(401, "unauthorized")
        if "/unknown/" in path:
            return fail(404 if self.fault == "binding-disclosure" else 403,
                        "memory_not_found" if self.fault == "binding-disclosure" else "forbidden_scope")
        if headers.get("Content-Type") != "application/json":
            return fail(415, "invalid_json")
        if len(raw) > CAP:
            return fail(413, "invalid_json")
        try:
            value = json.loads(raw) if self.fault == "duplicates" else strict_json(raw)
        except (ValueError, UnicodeDecodeError):
            return fail(400, "invalid_json")
        if not isinstance(value, dict):
            return fail(422, "invalid_json")
        verb = next((v for v, suffix in ROUTES.items() if path.endswith("/" + suffix)), "")
        if verb in REFUSALS:
            return fail(409, "memory_capability_unavailable", REFUSALS[verb])
        if verb == "receipt" and value.get("operation") == "remember":
            return fail(409, "memory_capability_unavailable", "remember_receipt_lookup")
        if verb == "recall":
            if "unknown" in value and self.fault != "unknown-fields":
                return fail(422, "invalid_input")
            memories = []
            for identifier in value.get("entity_seeds", []):
                if identifier not in self.memories:
                    continue
                supplied = self.memories[identifier]
                content = supplied["content"]
                memories.append({"id": identifier, "revision": "1", "scope": {"tenant": "@root", **SCOPE},
                    "kind": supplied["kind"], "content": content, "observed_at_ms": "90",
                    "created_at_ms": "100", "updated_at_ms": "100", "importance": 0.5,
                    "status": "accepted", "stale": False, "segment": None, "evidence": [],
                    "scores": {"R": 0, "D": None, "I": 0.5, "S": 0},
                    "provenance": [{"source_id": "live", "source_kind": "caller",
                                    "content_digest": "v1:" + hashlib.sha256(encode(content)).hexdigest()}]})
            return 200, {"memories": memories, "ranking_version": "memory-ranking-v1", "selection": "weighted",
                         "meta": {"scopes": [{"tenant": "@root", **SCOPE}], "as_of_ms": "100",
                                  "ttl_clock_ms": "100", "sequence": str(self.sequence), "complete": True,
                                  "truncation": [], "limitations": [] if self.fault == "limitations" else
                                  ["retained_state_only"], "mode": "bounded_expansion", "candidates_examined": "1",
                                  "edges_charged": "0", "vector_components": "0",
                                  "mmr_components": "0", "evidence_bytes": "0"}}
        key = value.get("idempotency_key", "")
        identity = verb + ":" + key
        if identity in self.saved:
            original_request, result = self.saved[identity]
            if original_request != value and self.fault != "idempotency-conflict":
                return fail(409, "idempotency_conflict")
            return 200, {**copy.deepcopy(result), "replayed": self.fault != "replay-flag"}
        self.sequence += 1
        if verb == "remember":
            self.memories.update({entry["id"]: entry for entry in value["items"]})
            outcomes = [{"id": entry["id"], "action": "created",
                         "revision": 1 if self.fault == "integer-output" else "1"} for entry in value["items"]]
        elif verb == "forget":
            identifiers = [v["id"] for v in value["target"]["ids"]]
            outcomes = [{"id": identifier, "action": "deleted", "revision": str(self.sequence)}
                        for identifier in identifiers]
            if self.fault != "forget-visibility":
                for identifier in identifiers:
                    self.memories.pop(identifier, None)
        else:
            return fail(404, "not_found")
        result = {"receipt": {"id": "fake-" + str(self.sequence), "operation": verb,
                  "scope": {"tenant": "@root", **SCOPE}, "sequence": str(self.sequence),
                  "retry_until_ms": "86400100", "digest_version": "memory-request-v1"},
                  "replayed": False, "outcomes": outcomes, "feed": FEED, "remaining_work": "0"}
        if verb == "forget":
            result.update({"state": "logical", "erasure_notice": {
                **NOTICE, "secure_erasure": self.fault == "secure-erasure"}})
        self.saved[identity] = (copy.deepcopy(value), copy.deepcopy(result))
        return 200, result


def probe_fake(fake: FakeMemory) -> Report:
    report = Report(quiet=True)
    report.secret(fake.token)
    http = HTTP(fake.url, fake.token, report)
    check = lambda name, fn: report.check("probe", name, CONTRACT + " sections 3,5,6,8", fn)
    check("authentication", lambda: error(http.memory("recall", recall("m"), token="wrong"), 401, "unauthorized"))
    check("binding-disclosure", lambda: error(http.memory("recall", recall("m"), binding="unknown"),
                                             403, "forbidden_scope"))
    check("duplicates", lambda: error(http.memory("recall", None,
          raw=b'{"scope":{"agent":"live-e2e"},"text":"Ada","text":"Ada"}'), 400, "invalid_json"))
    check("unknown-fields", lambda: error(http.memory("recall", {**recall("m"), "unknown": True}),
                                          422, "invalid_input"))
    check("capability-details", lambda: error(http.memory("facts", {"scope": SCOPE}), 409,
          "memory_capability_unavailable", feature="facts"))
    saved = remember("fake-create", item("m"))
    check("integer-output", lambda: mutation(success(http.memory("remember", saved)), "remember"))
    check("replay-flag", lambda: mutation(success(http.memory("remember", saved)), "remember", replayed=True))
    check("idempotency-conflict", lambda: error(http.memory("remember", remember("fake-create", item("m",
          content="changed"))), 409, "idempotency_conflict"))
    check("limitations", lambda: recalled(success(http.memory("recall", recall("m")))))
    check("secure-erasure", lambda: mutation(success(http.memory("forget", forgetting("fake-forget", "m", "1"))),
                                             "forget"))
    check("forget-visibility", lambda: require(not recalled(success(http.memory("recall", recall("m")))),
                                              "forgotten item is still visible"))

    def request_id() -> None:
        response = http.memory("recall", recall("m"), headers={"x-request-id": "self-test-request"})
        require(response.headers.get("x-request-id") == "self-test-request", "request ID not echoed")
    check("request-id", request_id)
    return report


def self_test() -> int:
    report = Report()

    def failed_prerequisite() -> None:
        nested = Report(quiet=True)

        def fail() -> None:
            nested.event(request={"path": "/v1/memory/notes/remember"},
                         response={"status": 403, "json": {"error": {"code": "forbidden_scope"}}})
            raise AssertionError("injected failing prerequisite")

        value = nested.check("probe", "failure", "", fail)
        require(value is None, "failed check returned an error body as a successful prerequisite")
        nested.check("probe", "dependent", "", lambda: prerequisite(value, "failure"))
        require(nested.results[-1].status == "SKIP", "failed prerequisite produced a false PASS")
    report.check("self-test", "failed-prerequisites-never-pass", "honest dependency reporting", failed_prerequisite)
    with FakeMemory() as fake:
        correct = probe_fake(fake)
        report.check("self-test", "correct-fake-passes-all-probes", "independent fake",
                     lambda: require(not correct.failed, str([dataclasses.asdict(r) for r in correct.results
                                                               if r.status == "FAIL"])))
        with RecordingProxy(fake.url, correct) as proxy:
            report.check("self-test", "recording-proxy", "capture client requests",
                         lambda: success(HTTP(proxy.url, fake.token, correct).memory("recall", recall("m"))))
        serialized = json.dumps(correct.redact(correct.traffic))
        report.check("self-test", "credentials-redacted", "no credentials in reports",
                     lambda: require(fake.token not in serialized and "Bearer " + fake.token not in serialized,
                                     "report leaked disposable token"))
    faults = ("authentication", "binding-disclosure", "duplicates", "unknown-fields", "capability-details",
              "integer-output", "replay-flag", "idempotency-conflict", "limitations", "secure-erasure",
              "forget-visibility", "request-id")
    for fault in faults:
        with FakeMemory(fault) as fake:
            observed = probe_fake(fake)
            report.check("self-test", "detect-" + fault, "one independently injected documented defect",
                         lambda observed=observed, fault=fault: require(
                             any(r.check == fault and r.status == "FAIL" for r in observed.results),
                             f"injected {fault} was not detected by its own check"))
    report.summary(("self-test",))
    return int(report.failed)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    commands = parser.add_subparsers(dest="command", required=True)
    commands.add_parser("self-test", help="prove correct fake passes and twelve faulty fakes fail")
    live = commands.add_parser("run", help="qualify a disposable real server")
    for flag in ("server-bin", "cli-bin", "mcp-bin", "work", "json", "only"):
        live.add_argument("--" + flag)
    live.add_argument("--keep", action="store_true")
    args = parser.parse_args()
    return self_test() if args.command == "self-test" else run(args)


if __name__ == "__main__":
    sys.exit(main())
