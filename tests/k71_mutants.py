#!/usr/bin/env python
"""Replays the hand mutants of the 0.7.1 key-value, input and audit follow-ups.

`tests/k71_mutants.json` is a list of {id, desc, file, old, new, cmd}. A mutant replaces the
text `old` (which must occur exactly once in `file`, ignoring CRLF) with `new`, runs `cmd`
from the repository root and records whether a test failed. An entry with `same_edit_as` is the
edit of the entry it names, killed by another suite: the two count as one mutant. The file is
always restored byte for byte and its SHA-256 is checked, so a replay leaves the tree as it
found it.

Statuses:
  KILLED    `cmd` failed and the output names at least one failing test
  SURVIVED  `cmd` passed and at least one test ran: no test notices the mutant
  INVALID   `cmd` failed without a failing test (compile error, harness failure, timeout) or
            passed without running a test (a filter that selects nothing): not a kill; fix
            the mutant or the command

Usage:
  python tests/k71_mutants.py --check                         # anchors only, nothing is run
  python tests/k71_mutants.py --target-dir DIR --out RESULTS.json [--ids ID,ID] [--shard I/N]
                              [--list tests/k71_mutants.json] [--timeout SECONDS] [--no-offline]

The anchors match the sources at the commit that added the list; run it on a tree where the
mutated functions are unchanged. Use a private --target-dir: every mutant rebuilds.

Target-dir reuse: the target dir is only a build cache, so keep it between runs. A second replay
of the same checkout then starts warm (dependencies and unchanged test binaries are not built
again), while a fresh directory pays a cold start (every dependency, once). Cargo embeds the
absolute workspace path in its fingerprints, so a kept directory is warm only for the path it was
built in. `tests/mutant_replay.py run` keeps one target dir per worker under its work dir (default
target/mutant-replay) and, with --jobs N, replays in N isolated worktrees at once.

Intermediate heads: `tests/mutant_replay.py run --only-changed` replays only the mutants whose text,
selected test files, baseline command or mutated source file differ from what it recorded, so a
campaign does not start over at every head. An independent second-machine replay is needed only at
the final head, and it runs without --only-changed so that every mutant ran on exactly that tree.
"""
import argparse
import hashlib
import json
import os
import re
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


def sha(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def anchors(mutant: dict, original: bytes):
    text = original.decode("utf-8")
    return text, "\r\n" in text, text.replace("\r\n", "\n").count(mutant["old"])


def apply(original: bytes, mutant: dict) -> bytes:
    text, crlf, count = anchors(mutant, original)
    if count != 1:
        raise SystemExit(f"{mutant['id']}: anchor occurs {count} times, expected exactly once")
    text = text.replace("\r\n", "\n").replace(mutant["old"], mutant["new"])
    return (text.replace("\n", "\r\n") if crlf else text).encode("utf-8")


def classify(returncode: int, output: str):
    failed = re.findall(r"^test (\S+) \.\.\. FAILED", output, re.M)
    if returncode == 0:
        # A filter that selects no test passes: that is not a survivor.
        passed = sum(int(count) for count in re.findall(r"^test result: ok\. (\d+) passed", output, re.M))
        return ("SURVIVED" if passed else "INVALID"), []
    if failed or "test result: FAILED" in output:
        return "KILLED", failed
    return "INVALID", []


def check(mutants: list) -> int:
    problems = 0
    seen = set()
    for mutant in mutants:
        if mutant["id"] in seen:
            print(f"{mutant['id']}: duplicate id")
            problems += 1
        seen.add(mutant["id"])
        if mutant["old"] == mutant["new"]:
            print(f"{mutant['id']}: the replacement equals the anchor")
            problems += 1
        path = ROOT / mutant["file"]
        if not path.exists():
            print(f"{mutant['id']}: missing file {mutant['file']}")
            problems += 1
            continue
        count = anchors(mutant, path.read_bytes())[2]
        if count != 1:
            print(f"{mutant['id']}: anchor occurs {count} times in {mutant['file']}")
            problems += 1
    print(f"{len(mutants)} mutants checked, {problems} problems")
    return 1 if problems else 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--list", default=str(ROOT / "tests" / "k71_mutants.json"))
    parser.add_argument("--check", action="store_true", help="only verify the anchors")
    parser.add_argument("--target-dir", help="CARGO_TARGET_DIR for the test builds")
    parser.add_argument("--out", help="results file, updated after every mutant")
    parser.add_argument("--ids", help="comma-separated ids to run")
    parser.add_argument("--shard", help="I/N: run every N-th mutant starting at I")
    parser.add_argument("--timeout", type=int, default=3600, help="seconds per mutant")
    parser.add_argument("--no-offline", action="store_true", help="drop --offline from every command")
    args = parser.parse_args()

    mutants = json.loads(Path(args.list).read_text(encoding="utf-8"))
    if args.check:
        return check(mutants)
    if not args.target_dir or not args.out:
        parser.error("--target-dir and --out are required unless --check is given")
    if args.no_offline:
        for mutant in mutants:
            mutant["cmd"] = mutant["cmd"].replace(" --offline", "")
    if args.ids:
        wanted = set(args.ids.split(","))
        mutants = [m for m in mutants if m["id"] in wanted]
    if args.shard:
        index, total = (int(part) for part in args.shard.split("/"))
        mutants = [m for position, m in enumerate(mutants) if position % total == index]
    env = dict(os.environ, CARGO_TARGET_DIR=args.target_dir, CARGO_INCREMENTAL="0")
    out = Path(args.out)
    results = json.loads(out.read_text(encoding="utf-8")) if out.exists() else {}

    for mutant in mutants:
        path = ROOT / mutant["file"]
        original = path.read_bytes()
        digest = sha(original)
        mutated = apply(original, mutant)
        started = time.time()
        try:
            path.write_bytes(mutated)
            try:
                result = subprocess.run(
                    mutant["cmd"], shell=True, cwd=ROOT, env=env, capture_output=True,
                    text=True, encoding="utf-8", errors="replace", timeout=args.timeout,
                )
                output = result.stdout + result.stderr
                status, failed = classify(result.returncode, output)
                tail = output[-1500:]
            except subprocess.TimeoutExpired:
                status, failed, tail = "INVALID", [], "timeout"
        finally:
            path.write_bytes(original)
            restored = sha(path.read_bytes()) == digest
        results[mutant["id"]] = {
            "desc": mutant["desc"], "status": status, "failed_tests": failed,
            "restored": restored, "seconds": round(time.time() - started),
            "tail": tail if status != "KILLED" else "",
        }
        out.write_text(json.dumps(results, indent=1), encoding="utf-8")
        print(f"{mutant['id']}: {status} {failed[:3]} restored={restored}", flush=True)
        if not restored:
            print("RESTORE FAILED", file=sys.stderr)
            return 2
    return 0


if __name__ == "__main__":
    sys.exit(main())
