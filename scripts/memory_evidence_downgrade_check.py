#!/usr/bin/env python3
"""Cross-version check of item evidence references (Python 3.11+).

    python -B scripts/memory_evidence_downgrade_check.py --new-bin PATH --old-bin PATH

`--new-bin` is a vlite-server of this release (0.7.1 or later) and `--old-bin` a 0.7.0
vlite-server, both built with the features of the documented release build. A disposable
database is provisioned; the new server writes memories, one of them holding an evidence
reference (`{"collection": "notes", "record": "777"}`); the old server then opens the same
file with the same bindings file and is probed; the new server forgets the memory; the old
server is probed once more. The expectations are the behaviour the 0.7.1 design note
(docs/projects/roadmap/m71-memory-followups.md, section 5.4) and docs/API.md document:
under 0.7.0 every memory operation of the scope that holds such a memory answers 500, with
no panic and no silent loss, other scopes and the evidence record are unaffected, and
forgetting the memory with the new server makes the scope usable again. The script prints
every answer and exits 1 on any difference. It touches only a private temporary directory.
"""

from __future__ import annotations

import argparse
import base64
from pathlib import Path
import shutil
import sys
import tempfile
from typing import Any

sys.path.insert(0, str(Path(__file__).resolve().parent))
import memory_live_e2e as live  # noqa: E402

REFERENCE = [{"collection": "notes", "record": "777"}]


def describe(reply: live.Reply) -> str:
    try:
        body = reply.json()
    except ValueError:
        return f"{reply.status} (not JSON)"
    if isinstance(body, dict) and "error" in body:
        error = body["error"]
        feature = error.get("details", {}).get("feature")
        return f"{reply.status} {error.get('code')}" + (f" feature={feature}" if feature else "")
    if isinstance(body, dict) and "memories" in body:
        return f"{reply.status} memories={[view['id'] for view in body['memories']]}"
    if isinstance(body, dict) and "outcomes" in body:
        return f"{reply.status} outcomes={[(o['id'], o['action']) for o in body['outcomes']]}" \
               f" replayed={body.get('replayed')}"
    return f"{reply.status}"


class Probe:
    def __init__(self) -> None:
        self.failures: list[str] = []

    def expect(self, phase: str, name: str, reply: live.Reply, status: int,
               code: str | None = None, feature: str | None = None) -> live.Reply:
        answer = describe(reply)
        wanted = f"{status}" + (f" {code}" if code else "") + (f" feature={feature}" if feature else "")
        verdict = "ok  " if answer.startswith(wanted) else "FAIL"
        print(f"  {verdict} {phase:8s} {name:58s} {answer}")
        if verdict == "FAIL":
            self.failures.append(f"{phase} {name}: wanted {wanted}, got {answer}")
        return reply


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--new-bin", required=True)
    parser.add_argument("--old-bin", required=True)
    parser.add_argument("--work", help="parent directory of the private temporary workspace")
    args = parser.parse_args()
    new, old = Path(args.new_bin).resolve(), Path(args.old_bin).resolve()
    live.require(new.is_file() and old.is_file(), "--new-bin and --old-bin must exist")
    parent = Path(args.work).resolve() if args.work else Path(tempfile.gettempdir()).resolve()
    parent.mkdir(parents=True, exist_ok=True)
    work = Path(tempfile.mkdtemp(prefix="vlite-evidence-compat-", dir=parent))
    report = live.Report(quiet=True)
    env = live.Environment(work, report, new)
    probe = Probe()
    try:
        with env.server(configured=False, create=True) as server:
            env.provision(server.http)

        holding = {**live.item("ref-1"), "evidence": REFERENCE}
        with env.server() as server:
            raw = live.HTTP(server.url, env.keys["raw"], report)
            payload = base64.b64encode(b"proof").decode("ascii")
            put = raw.request("PUT", "/v1/collections/evidence/vectors/777",
                              {"vector": [1, 0, 0], "payload_base64": payload})
            live.require(put.status in (200, 201, 204), f"evidence record: {put.status} {put.raw!r}")
            live.mutation(live.success(raw.memory("remember", live.remember("k-ref", holding))), "remember")
            live.mutation(live.success(raw.memory("remember", live.remember("k-plain", live.item("plain-1")))),
                          "remember")
            live.mutation(live.success(raw.memory("remember", live.remember(
                "k-other", live.item("other-1"), scope=live.OTHER_SCOPE))), "remember", live.OTHER_SCOPE)
            seen = live.success(raw.memory("recall", live.recall("ref-1")))["memories"][0]["evidence"]
            live.require(seen and seen[0]["record"] == "777", f"the new server does not report the record: {seen}")
            print("new server: the reference attached and recall reports record 777")

        by_vector = {"scope": live.SCOPE, "vector": [1, 0, 0], "limit": 5}
        forget = {"scope": live.SCOPE, "idempotency_key": "f-ref", "mode": "logical_delete",
                  "target": {"ids": [{"id": "ref-1", "revision": "1"}]}}

        env.binary = old
        print("old server (0.7.0) over the same file, scope holding the memory:")
        with env.server() as server:
            memory = live.HTTP(server.url, env.keys["memory"], report)
            raw = live.HTTP(server.url, env.keys["raw"], report)
            ask = lambda name, reply, *want, **detail: probe.expect("0.7.0", name, reply, *want, **detail)
            ask("recall by seed, the memory with the reference", memory.memory("recall", live.recall("ref-1")),
                500, "internal_error")
            ask("recall by seed, another memory of the scope", memory.memory("recall", live.recall("plain-1")),
                500, "internal_error")
            ask("recall by vector", memory.memory("recall", by_vector), 500, "internal_error")
            ask("new remember", memory.memory("remember", live.remember("k-new", live.item("new-1"))),
                500, "internal_error")
            ask("forget the memory", memory.memory("forget", forget), 500, "internal_error")
            ask("consolidate the memory", memory.memory(
                "consolidate", live.consolidation("c-ref", "ref-1", "1", "sum-1")), 500, "internal_error")
            ask("replay of a remember without a reference", memory.memory(
                "remember", live.remember("k-plain", live.item("plain-1"))), 200)
            ask("replay of the remember with the reference", memory.memory(
                "remember", live.remember("k-ref", holding)), 409, "memory_capability_unavailable", "evidence")
            ask("recall in another scope", memory.memory("recall", live.recall(
                "other-1", scope=live.OTHER_SCOPE)), 200)
            ask("new remember in another scope", memory.memory("remember", live.remember(
                "k-new-other", live.item("new-2"), scope=live.OTHER_SCOPE)), 200)
            record = ask("raw read of the evidence record", raw.request(
                "GET", "/v1/collections/evidence/vectors/777"), 200)
            if record.status == 200 and record.json().get("payload_base64") != payload:
                probe.failures.append("0.7.0 raw read: the evidence record changed")
            panics = [text for _, text in server.output if "panick" in text.lower()]
            if server.process.poll() is not None or panics:
                probe.failures.append(f"0.7.0 server did not stay up cleanly: {panics[:2]}")
            print(f"  server alive: {server.process.poll() is None}, panic lines: {len(panics)}")

        env.binary = new
        print("new server forgets the memory:")
        with env.server() as server:
            raw = live.HTTP(server.url, env.keys["raw"], report)
            probe.expect("0.7.1", "forget the memory with the reference", raw.memory("forget", forget), 200)
            probe.expect("0.7.1", "the evidence record is deleted with it", raw.request(
                "GET", "/v1/collections/evidence/vectors/777"), 404)

        env.binary = old
        print("old server (0.7.0) after the memory was forgotten:")
        with env.server() as server:
            memory = live.HTTP(server.url, env.keys["memory"], report)
            ask = lambda name, reply, *want, **detail: probe.expect("0.7.0", name, reply, *want, **detail)
            ask("recall by seed", memory.memory("recall", live.recall("plain-1")), 200)
            ask("recall by vector", memory.memory("recall", by_vector), 200)
            ask("new remember", memory.memory("remember", live.remember("k-after", live.item("after-1"))), 200)
    finally:
        shutil.rmtree(work, ignore_errors=True)
    if probe.failures:
        print("\nDIFFERENCES:")
        for line in probe.failures:
            print("  " + line)
        return 1
    print("\nOK: the behaviour matches the documented downgrade rules")
    return 0


if __name__ == "__main__":
    sys.exit(main())
