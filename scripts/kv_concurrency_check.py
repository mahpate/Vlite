#!/usr/bin/env python3
"""Independent REST KV concurrency oracle; expectations: docs/API.md, KV sections.

Run `python scripts/kv_concurrency_check.py self-test` or `... run --help`.
Only the standard library is needed. Histories include wall-clock request
intervals, monotonic timestamps, complete bodies, and minimized counterexamples.
The oracle is exhaustive per key, not a global cross-key linearizability proof.
"""

from __future__ import annotations

import argparse
import base64
import collections
import concurrent.futures
import http.client
import json
import os
from pathlib import Path
import queue
import random
import re
import secrets
import subprocess
import sys
import tempfile
import threading
import time
import urllib.parse

ROOT = Path(__file__).resolve().parents[1]
TOLERANCE_MS = 5
NAMESPACES = ("north", "south", "west")
KEYS = tuple(f"{prefix}/{i:02}" for prefix in ("a", "b") for i in range(7))
ADDRESSES = tuple((ns, key) for ns in NAMESPACES for key in KEYS)
MAX_U64 = 2**64 - 1


def unknown(op):
    body = op["response"]
    error = body.get("error") if isinstance(body, dict) else None
    return op["status"] in (0, 503) or (
        isinstance(error, dict) and error.get("code") == "commit_indeterminate"
    )


def decimal(value):
    if not isinstance(value, str) or not re.fullmatch(r"0|[1-9][0-9]*", value):
        raise ValueError(f"noncanonical decimal: {value!r}")
    result = int(value)
    if result > MAX_U64:
        raise ValueError("decimal exceeds uint64")
    return result


def entry(body):
    if set(body) != {"key", "value_base64", "version", "expires_at_ms"}:
        raise ValueError("wrong entry fields")
    value = body["value_base64"]
    if base64.b64encode(base64.b64decode(value, validate=True)).decode() != value:
        raise ValueError("noncanonical base64")
    version = decimal(body["version"])
    if not version:
        raise ValueError("issued version is zero")
    expiry = body["expires_at_ms"]
    return value, version, None if expiry is None else decimal(expiry)


def expected_matches(expected, live):
    if expected == "any":
        return True
    if expected == "absent":
        return live is None
    return live is not None and live[1] == int(expected["revision"])


def transitions(op, state, earliest, observed):
    """All legal state/time outcomes at this operation's linearization point."""
    high, live = state
    start = max(earliest, op["start_ms"])
    if start > op["end_ms"]:
        return
    possibilities = []
    if live is None or live[2] is None or start < live[2] + TOLERANCE_MS:
        possibilities.append((live, start))
    if live is not None and live[2] is not None:
        expired_at = max(start, live[2] - TOLERANCE_MS)
        if expired_at <= op["end_ms"]:
            possibilities.append((None, expired_at))
    for current, stamp in possibilities:
        kind, status = op["kind"], op["status"]
        if kind == "read":
            if unknown(op) or current == op["observed"]:
                yield (high, current), stamp
            continue
        matches = expected_matches(op["request"]["expected"], current)
        if unknown(op):
            yield (high, current), stamp  # failure/no commit is also possible
        elif status == 412:
            if not matches:
                yield (high, current), stamp  # mismatches are strictly read-only
            continue
        elif status != 200:
            continue
        if not matches:
            continue
        if kind == "delete":
            if unknown(op) or op["response"]["deleted"] == (current is not None):
                yield (high, None), stamp
        else:
            req = op["request"]
            expiry = req.get("expires_at_ms")
            expiry = None if expiry is None else int(expiry)
            if unknown(op):
                versions = observed.get(req["value_base64"], set()) | {high + 1}
            else:
                versions = {int(op["response"]["version"])}
            for version in versions:
                if high < version <= MAX_U64:
                    yield (version, (req["value_base64"], version, expiry)), stamp


def solve(ops, initial, observed):
    """Interval-order search. Equal states retain only their earliest time."""
    full = (1 << len(ops)) - 1
    stack = [(full, state, stamp) for state, stamp in initial.items()]
    visited, finals = {}, {}
    while stack:
        remaining, state, stamp = stack.pop()
        marker = remaining, state
        if visited.get(marker, float("inf")) <= stamp:
            continue
        visited[marker] = stamp
        if not remaining:
            finals[state] = min(finals.get(state, float("inf")), stamp)
            continue
        indices = [i for i in range(len(ops)) if remaining & (1 << i)]
        deadline = min(ops[i]["end_ms"] for i in indices)
        for i in indices:
            op = ops[i]
            if op["start_ms"] > deadline:
                continue
            for next_state, next_stamp in transitions(op, state, stamp, observed):
                if next_stamp <= deadline:
                    stack.append((remaining ^ (1 << i), next_state, next_stamp))
        if len(visited) > 250000:
            raise RuntimeError("oracle search budget exceeded; result is INCONCLUSIVE")
    return finals


def check(history, addresses=ADDRESSES):
    violations, by_key, observed = [], collections.defaultdict(list), {}

    def fail(reason, ops, initial=None):
        result = {"reason": reason, "history": ops}
        if initial is not None:
            result["initial_states"] = [
                {"high_water": state[0], "entry": state[1],
                 "time_ms": None if stamp == -float("inf") else stamp}
                for state, stamp in initial.items()
            ]
        violations.append(result)

    for op in history:
        try:
            if op["end_ms"] < op["start_ms"]:
                raise ValueError("wall clock moved backwards during request")
            if unknown(op):
                if op["kind"] in ("put", "delete"):
                    by_key[(op["namespace"], op["key"])].append(op)
                continue
            status, kind, body = op["status"], op["kind"], op["response"]
            if not isinstance(body, dict):
                raise ValueError("response is not a JSON object")
            if kind in ("put", "delete") and status == 412:
                if body.get("error", {}).get("code") != "precondition_failed":
                    raise ValueError("412 without precondition_failed")
                by_key[(op["namespace"], op["key"])].append(op)
            elif kind in ("put", "delete") and status == 200:
                if kind == "put":
                    if set(body) != {"version", "expires_at_ms"}:
                        raise ValueError("wrong put result fields")
                    if not decimal(body["version"]):
                        raise ValueError("issued version is zero")
                    if body["expires_at_ms"] != op["request"].get("expires_at_ms"):
                        raise ValueError("put returned a different expiry")
                elif set(body) != {"deleted"} or type(body["deleted"]) is not bool:
                    raise ValueError("wrong delete result")
                by_key[(op["namespace"], op["key"])].append(op)
            elif kind == "get" and status in (200, 404):
                if status == 404 and body.get("error", {}).get("code") != "kv_not_found":
                    raise ValueError("404 without kv_not_found")
                value = entry(body) if status == 200 else None
                if value is not None and body["key"] != op["key"]:
                    raise ValueError("get returned a different key")
                by_key[(op["namespace"], op["key"])].append(
                    dict(op, kind="read", observed=value)
                )
            elif kind == "list" and status == 200:
                if set(body) != {"items", "next_after"}:
                    raise ValueError("wrong list envelope")
                items, cursor, req = body["items"], body["next_after"], op["request"]
                keys = [item["key"] for item in items]
                if keys != sorted(set(keys), key=lambda key: key.encode("utf-8")):
                    raise ValueError("unordered or repeated list keys")
                if len(items) > req["limit"]:
                    raise ValueError("page exceeds limit")
                if any(not key.startswith(req["prefix"]) or
                       key.encode() <= req.get("after", "").encode() for key in keys):
                    raise ValueError("list violated prefix/exclusive cursor")
                if cursor is not None and (
                    not keys or cursor != keys[-1] or len(keys) != req["limit"]
                ):
                    raise ValueError("next_after is not the full page's last key")
                values = {item["key"]: entry(item) for item in items}
                known = {key for ns, key in addresses if ns == op["namespace"]}
                if set(keys) - known:
                    raise ValueError("list returned an unrequested/unknown key")
                if cursor is not None and not any(
                    key.startswith(req["prefix"]) and key.encode() > cursor.encode()
                    for key in known
                ):
                    raise ValueError("next_after has no possible matching successor")
                for key in known:
                    if (key.startswith(req["prefix"]) and
                            key.encode() > req.get("after", "").encode() and
                            (cursor is None or key.encode() <= cursor.encode())):
                        by_key[(op["namespace"], key)].append(
                            dict(op, kind="read", key=key, observed=values.get(key))
                        )
            else:
                raise ValueError(f"unexpected HTTP {status} for {kind}")
        except (ValueError, KeyError, TypeError) as error:
            fail(str(error), [op])

    for address, ops in by_key.items():
        ops.sort(key=lambda op: (op["start_ms"], op["id"]))
        versions = collections.defaultdict(set)
        for op in ops:
            if op["kind"] == "read" and op["observed"] is not None:
                value, version, _ = op["observed"]
                versions[value].add(version)
        observed[address] = versions
        states, prior = {(0, None): -float("inf")}, []
        offset = 0
        while offset < len(ops):
            end, finish = offset + 1, ops[offset]["end_ms"]
            while end < len(ops) and ops[end]["start_ms"] <= finish:
                finish = max(finish, ops[end]["end_ms"])
                end += 1
            component = ops[offset:end]
            next_states = solve(component, states, versions)
            if not next_states:
                minimal = component[:]
                for candidate in component:
                    reduced = [op for op in minimal if op is not candidate]
                    if reduced and not solve(reduced, states, versions):
                        minimal = reduced
                fail(f"no legal per-key serialization for {address!r}",
                     prior[-1:] + minimal, states)
                break
            states, prior, offset = next_states, component, end
    return violations


class Client:
    def __init__(self, base, token):
        self.host = urllib.parse.urlsplit(base).netloc
        self.token, self.local = token, threading.local()
        self.history, self.lock = [], threading.Lock()

    def request(self, method, path, body=None, **metadata):
        connection = getattr(self.local, "connection", None)
        if connection is None:
            connection = self.local.connection = http.client.HTTPConnection(
                self.host, timeout=15
            )
        op = dict(metadata, method=method, path=path, request=body or {},
                  start_ms=time.time_ns() / 1e6, start_ns=time.monotonic_ns())
        try:
            connection.request(method, path, None if body is None else json.dumps(body),
                               {"Authorization": f"Bearer {self.token}",
                                "Content-Type": "application/json"})
            response = connection.getresponse()
            raw = response.read().decode()
            op.update(status=response.status, response=json.loads(raw))
        except (OSError, http.client.HTTPException) as error:
            connection.close()
            self.local.connection = None
            op.update(status=0, response={"transport_error": str(error)})
        except (ValueError, UnicodeError) as error:
            op.update(status=-1, response={"invalid_response": str(error)})
        op.update(end_ms=time.time_ns() / 1e6, end_ns=time.monotonic_ns())
        with self.lock:
            op["id"] = len(self.history)
            self.history.append(op)
        return op

    def close(self):
        connection = getattr(self.local, "connection", None)
        if connection is not None:
            connection.close()


def kv_request(client, kind, ns, key=None, **req):
    req = dict(namespace=ns, **req)
    if key is not None:
        req["key"] = key
    path = "/v1/collections/kvcheck/kv/" + ("entries" if kind == "list" else "entry")
    method = {"put": "PUT", "delete": "DELETE"}.get(kind, "GET")
    if kind == "put":
        return client.request(method, path, req, kind=kind, namespace=ns, key=key)
    query = dict(req)
    if isinstance(query.get("expected"), dict):
        query["expected"] = query["expected"]["revision"]
    op = client.request(method, path + "?" + urllib.parse.urlencode(query),
                        kind=kind, namespace=ns, key=key)
    op["request"] = req
    return op


def page_walk(client, ns, prefix="", limit=3, sequence=None, phase="load"):
    after, seen = None, set()
    while True:
        req = dict(prefix=prefix, limit=limit)
        if after is not None:
            req["after"] = after
        op = kv_request(client, "list", ns, **req)
        op.update(sequence=sequence, phase=phase)
        if op["status"] != 200 or not isinstance(op["response"], dict):
            return
        after = op["response"].get("next_after")
        if after is None or after in seen:
            return
        seen.add(after)
        if len(seen) > len(KEYS):
            return  # malformed cursors are diagnosed by the oracle


def worker(client, number, deadline, seed):
    rng, revisions = random.Random(seed + number), {}
    counter = 0
    try:
        while time.monotonic() < deadline:
            time.sleep(0.01)
            counter += 1
            ns, key = rng.choice(ADDRESSES)
            choice = rng.random()
            if choice < 0.18:
                page_walk(client, ns, rng.choice(("", "a/", "b/")),
                          rng.randint(1, 4), f"{number}:{counter}")
                continue
            expected = rng.choice(("any", "absent", {
                "revision": revisions.get((ns, key), "0")
            }))
            if choice < 0.58:
                req = dict(expected=expected, value_base64=base64.b64encode(
                    f"{number}:{counter}:{rng.getrandbits(64)}".encode()).decode())
                if rng.random() < 0.35:
                    req["expires_at_ms"] = str(int(time.time() * 1000) +
                                              rng.randint(150, 500))
                op = kv_request(client, "put", ns, key, **req)
            elif choice < 0.78:
                op = kv_request(client, "delete", ns, key, expected=expected)
            else:
                op = kv_request(client, "get", ns, key)
            if op["status"] == 200 and "version" in op["response"]:
                revisions[(ns, key)] = op["response"]["version"]
    finally:
        client.close()


def start_server(binary, directory):
    token = secrets.token_urlsafe(32)
    token_path = directory / "admin.token"
    token_path.write_text(token, encoding="ascii")
    token_path.chmod(0o600)
    env = {key: value for key, value in os.environ.items()
           if not key.startswith("VLITE_")}
    env["VLITE_ADMIN_DIR"] = str(directory / "admin")
    process = subprocess.Popen(
        [str(binary), str(directory / "test.vlite"), "--create-if-missing", "4",
         "--token-file", str(token_path), "--bind", "127.0.0.1:0"],
        stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, env=env,
    )
    lines, output = queue.Queue(), []

    def collect():
        for line in process.stdout:
            output.append(line.rstrip())
            lines.put(line)

    reader = threading.Thread(target=collect, daemon=True)
    reader.start()
    try:
        deadline = time.monotonic() + 60
        while time.monotonic() < deadline:
            try:
                line = lines.get(timeout=0.2)
            except queue.Empty:
                if process.poll() is not None:
                    break
                continue
            match = re.search(r"LISTENING (http://127\.0\.0\.1:\d+)", line)
            if match:
                return process, Client(match[1], token), output, reader
        raise RuntimeError("server did not become ready: " + "\n".join(output))
    except BaseException:
        stop_server(process)
        reader.join(timeout=5)
        process.stdout.close()
        raise


def stop_server(process):
    if process.poll() is None:
        process.terminate()
        try:
            process.wait(timeout=10)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait()


def run(args):
    if args.duration <= 0 or args.threads <= 0:
        raise ValueError("duration and threads must be positive")
    binary = args.server_bin
    if binary is None:
        env = dict(os.environ, CARGO_INCREMENTAL="0")
        subprocess.run(["cargo", "build", "--locked", "--offline", "--release",
                        "--features", "server", "--bin", "vlite-server"],
                       cwd=ROOT, env=env, check=True)
        target = Path(os.environ.get("CARGO_TARGET_DIR", ROOT / "target"))
        binary = target / "release" / ("vlite-server.exe" if os.name == "nt"
                                     else "vlite-server")
    with tempfile.TemporaryDirectory(prefix="vlite-kv-check-") as directory:
        process, client, server_log, reader = start_server(
            Path(binary).resolve(), Path(directory)
        )
        try:
            setup = client.request("POST", "/v1/collections",
                                   {"name": "kvcheck", "dimensions": 1})
            if setup["status"] != 201:
                raise RuntimeError(f"collection creation failed: {setup['response']}")
            client.history.clear()
            wall, mono = time.time_ns() / 1e6, time.monotonic_ns()
            deadline = time.monotonic() + args.duration
            with concurrent.futures.ThreadPoolExecutor(args.threads) as executor:
                futures = [executor.submit(worker, client, i, deadline, args.seed)
                           for i in range(args.threads)]
                for future in futures:
                    future.result()
            load_ms = (time.monotonic_ns() - mono) / 1e6
            for ns, key in ADDRESSES:
                op = kv_request(client, "get", ns, key)
                op["phase"] = "final"
            for ns in NAMESPACES:
                page_walk(client, ns, limit=3, sequence=f"final:{ns}", phase="final")
            history = client.history
            drift = max(
                abs((op[f"{edge}_ms"] - wall) -
                    (op[f"{edge}_ns"] - mono) / 1e6)
                for op in history for edge in ("start", "end")
            )
            violations = check(history)
            if drift > TOLERANCE_MS:
                violations.append({"reason": "clock step exceeds tolerance",
                                   "drift_ms": drift, "history": []})
            counts = collections.Counter(op["kind"] for op in history)
            statuses = collections.Counter(str(op["status"]) for op in history)
            report = dict(duration_seconds=load_ms / 1000, threads=args.threads,
                          seed=args.seed, clock_tolerance_ms=TOLERANCE_MS,
                          clock_drift_ms=drift, operations=dict(counts),
                          statuses=dict(statuses),
                          unknown_outcomes=sum(unknown(op) for op in history),
                          violations=violations, history=history,
                          server_log=server_log)
            if args.json:
                Path(args.json).write_text(json.dumps(report, indent=2) + "\n",
                                           encoding="utf-8", newline="\r\n")
            print(json.dumps({key: value for key, value in report.items()
                              if key not in ("history", "server_log")}, indent=2))
            return bool(violations)
        finally:
            client.close()
            stop_server(process)
            reader.join(timeout=5)
            process.stdout.close()


def self_test():
    def op(i, kind, request=None, response=None, status=200, **extra):
        return dict(id=i, kind=kind, namespace="n", key="a", request=request or {},
                    response=response or {}, status=status, start_ms=i * 10,
                    end_ms=i * 10 + 1, **extra)

    def put(i, version, expected="any", expiry=None, value="YQ=="):
        return op(i, "put", dict(expected=expected, value_base64=value,
                                expires_at_ms=expiry),
                  dict(version=str(version), expires_at_ms=expiry))

    def get(i, version=1, value="YQ==", expiry=None):
        return op(i, "get", response=dict(key="a", value_base64=value,
                                          version=str(version), expires_at_ms=expiry))

    addresses = (("n", "a"), ("n", "b"))
    clean = [put(1, 1, "absent"), get(2),
             op(3, "put", {"expected": "absent"},
                {"error": {"code": "precondition_failed"}}, 412),
             get(4), put(5, 2, {"revision": "1"}), get(6, 2)]
    if check(clean, addresses):
        raise AssertionError("clean history rejected")
    cases = {
        "lost update": [put(1, 1), put(2, 2, {"revision": "1"}),
                        put(3, 3, {"revision": "1"})],
        "repeated revision": [put(1, 1), put(2, 1)],
        "stale acknowledged read": [put(1, 1), put(2, 2, value="Yg=="), get(3)],
        "skipped list key": [put(1, 1), op(
            2, "list", {"prefix": "", "limit": 2},
            {"items": [], "next_after": None})],
        "visible after expiry": [put(1, 1, expiry="15"), get(4, expiry="15")],
        "412 changed state": clean[:3] + [get(4, 2)],
        "absent replaced live": [put(1, 1), put(2, 2, "absent")],
        "decreasing revision": [put(1, 2), put(2, 1)],
        "missing final scan": [put(1, 1), op(2, "get", response={
            "error": {"code": "kv_not_found"}}, status=404)],
        "cursor not last returned": [put(1, 1), op(
            2, "list", {"prefix": "", "limit": 1},
            {"items": [get(2)["response"]], "next_after": "b"})],
    }
    race = [put(1, 1), put(2, 2, {"revision": "1"}),
            put(3, 3, {"revision": "1"})]
    race[1].update(start_ms=20, end_ms=40)
    race[2].update(start_ms=20, end_ms=40)
    cases["two concurrent CAS successes for one revision"] = race
    for name, history in cases.items():
        if not check(history, addresses):
            raise AssertionError(f"corruption not detected: {name}")
        print(f"PASS detects {name}")
    ambiguous = put(1, 1)
    ambiguous.update(status=503, response={"error": {"code": "commit_indeterminate"}})
    for observed in (get(2), op(2, "get", response={
            "error": {"code": "kv_not_found"}}, status=404)):
        if check([ambiguous, observed], addresses):
            raise AssertionError("unknown outcome interpretation rejected")
    concurrent = [put(1, 1), put(2, 2, {"revision": "1"})]
    concurrent[0].update(start_ms=10, end_ms=40)
    concurrent[1].update(start_ms=20, end_ms=30)
    if check(concurrent, addresses):
        raise AssertionError("legal overlapping writes rejected")
    b_put, b_get = put(2, 2), get(4, 2)
    b_put["key"], b_get["key"], b_get["response"]["key"] = "b", "b", "b"
    paged = [put(1, 1), b_put, op(
        3, "list", {"prefix": "", "limit": 1},
        {"items": [get(3)["response"]], "next_after": "a"}), op(
        4, "list", {"prefix": "", "after": "a", "limit": 1},
        {"items": [b_get["response"]], "next_after": None})]
    if check(paged, addresses):
        raise AssertionError("clean paged history rejected")
    paged[-1]["response"]["items"] = []
    if not check(paged, addresses):
        raise AssertionError("stable key skipped between pages was not detected")
    expiry = [put(1, 1, expiry="15"), op(
        3, "get", response={"error": {"code": "kv_not_found"}}, status=404),
        put(4, 2, "absent")]
    if check(expiry, addresses):
        raise AssertionError("legal expiry followed by absent CAS rejected")
    print("PASS detects stable key skipped between pages")
    print("PASS clean histories: overlap, paging, expiry, both unknown interpretations")
    return 0


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)
    sub.add_parser("self-test")
    run_parser = sub.add_parser("run")
    run_parser.add_argument("--server-bin", type=Path)
    run_parser.add_argument("--duration", type=float, default=60)
    run_parser.add_argument("--threads", type=int, default=16)
    run_parser.add_argument("--seed", type=int, default=20261104)
    run_parser.add_argument("--json", type=Path)
    args = parser.parse_args()
    try:
        return self_test() if args.command == "self-test" else run(args)
    except (OSError, ValueError, RuntimeError, subprocess.SubprocessError) as error:
        print(f"ERROR: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
