#!/usr/bin/env python3
"""Replay hand-written mutants one build at a time and keep their evidence.

A list is JSON: {"campaign", "base", "mutants": [{"id", "desc", "file", "old",
"new", "cmd", "expect"?}]}. Each mutant is one anchored replacement: `old` must
occur exactly once in `file`. `cmd` is the argument list of one cargo test
command (for example "test --locked --offline --lib -- compact::source").

  python tests/compact_mutant_replay.py check LIST.json
  python tests/compact_mutant_replay.py run LIST.json OUT.json [--machine NAME] [--ids a,b]
  python tests/compact_mutant_replay.py merge LIST.json OUT.json [OUT.json ...]

`run` records, for every mutant, the evidence of the 0.7.1 acceptance form:

  1. the anchor matches exactly once in the tree under test;
  2. the mutated tree compiles (`cargo ... --no-run`);
  3. the named test(s) fail, with their names;
  4. SHA-256 of the file before, mutated and restored (over LF-normalized
     content, so the values compare across Windows and Linux checkouts, plus the
     raw bytes before and after on this machine);
  5. the unmodified selector is green before the first mutant and again after
     the last, and every restore is byte-identical;
  6. one mutant per build;
  7. a `machine` label, so a replay on another machine is a second run.

A compile error, a crash, a timeout, or a run that executes no test never counts
as a kill. Tests must not sleep or read the wall clock; this tool does not
change that.
"""

import argparse
import hashlib
import json
import os
import platform
import re
import shlex
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
FAILED = re.compile(r"^test (\S+) \.\.\. FAILED", re.M)
RUNNING = re.compile(r"^running (\d+) tests?$", re.M)
RESULT = re.compile(r"^test result: (?:ok|FAILED)\. (\d+) passed; (\d+) failed", re.M)
TIMEOUT = 3 * 3600


def sha256(data):
    return hashlib.sha256(data).hexdigest()


def lf(text):
    return text.replace("\r\n", "\n")


def read_list(path):
    data = json.loads(Path(path).read_text(encoding="utf-8"))
    ids = [mutant["id"] for mutant in data["mutants"]]
    if len(ids) != len(set(ids)):
        raise SystemExit("duplicate mutant IDs")
    return data


def tool(root, *command):
    try:
        done = subprocess.run(command, cwd=root, capture_output=True, text=True, timeout=60)
        return done.stdout.strip()
    except (OSError, subprocess.SubprocessError):
        return ""


def environment(root):
    return {
        "os": platform.platform(),
        "python": platform.python_version(),
        "rustc": tool(root, "rustc", "--version"),
        "cargo": tool(root, "cargo", "--version"),
        "head": tool(root, "git", "rev-parse", "HEAD"),
        "tree_clean": tool(root, "git", "status", "--porcelain", "--untracked-files=no") == "",
    }


def check(root, data):
    problems = 0
    for mutant in data["mutants"]:
        path = root / mutant["file"]
        if not path.is_file():
            print(f"{mutant['id']}: no such file {mutant['file']}")
            problems += 1
            continue
        count = lf(path.read_text(encoding="utf-8")).count(lf(mutant["old"]))
        same = lf(mutant["old"]) == lf(mutant["new"])
        status = "ok" if count == 1 and not same else "ANCHOR-ERROR"
        print(f"{mutant['id']}: {status} (matches {count}{', no change' if same else ''})")
        problems += status != "ok"
    print(f"{len(data['mutants'])} mutants, {problems} problems")
    return problems == 0


def cargo(root, arguments, timeout, build_only=False):
    words = shlex.split(arguments)
    if build_only:
        at = words.index("--") if "--" in words else len(words)
        words = words[:at] + ["--no-run"] + words[at:]
    started = time.time()
    try:
        done = subprocess.run(
            ["cargo", *words],
            cwd=root,
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            timeout=timeout,
        )
        return done.returncode, done.stdout + done.stderr, round(time.time() - started)
    except subprocess.TimeoutExpired as error:
        partial = (error.stdout or "") + (error.stderr or "")
        if isinstance(partial, bytes):
            partial = partial.decode("utf-8", "replace")
        return None, partial, round(time.time() - started)


def counts(output):
    ran = sum(int(n) for n in RUNNING.findall(output))
    passed = sum(int(p) for p, _ in RESULT.findall(output))
    failed = sorted(set(FAILED.findall(output)))
    return ran, passed, failed


def baseline(root, commands, timeout):
    results = {}
    for arguments in commands:
        code, output, seconds = cargo(root, arguments, timeout)
        ran, passed, failed = counts(output)
        green = code == 0 and ran > 0 and not failed
        results[arguments] = {"green": green, "tests": ran, "passed": passed, "seconds": seconds}
        print(f"baseline {'green' if green else 'NOT GREEN'}: {arguments} ({ran} tests)", flush=True)
        if not green:
            print(output[-3000:])
    return results


def replay(root, mutant, timeout):
    path = root / mutant["file"]
    original = path.read_bytes()
    text = original.decode("utf-8")
    newline = "\r\n" if "\r\n" in text else "\n"
    base = lf(text)
    old, new = lf(mutant["old"]), lf(mutant["new"])
    record = {
        "id": mutant["id"],
        "file": mutant["file"],
        "anchor_matches": base.count(old),
        "sha256_lf": {"before": sha256(base.encode("utf-8"))},
        "sha256_bytes": {"before": sha256(original)},
    }
    if record["anchor_matches"] != 1 or old == new:
        record["status"] = "anchor-error"
        return record
    mutated = base.replace(old, new, 1)
    record["sha256_lf"]["mutated"] = sha256(mutated.encode("utf-8"))
    started = time.time()
    try:
        path.write_bytes(mutated.replace("\n", newline).encode("utf-8"))
        code, output, _ = cargo(root, mutant["cmd"], timeout, build_only=True)
        record["compiled"] = code == 0
        if code is None:
            record["status"] = "timeout"
        elif code != 0:
            record["status"] = "compile-error"
            record["detail"] = output[-1500:]
        else:
            code, output, _ = cargo(root, mutant["cmd"], timeout)
            ran, passed, failed = counts(output)
            record.update({"tests": ran, "passed": passed, "failed_tests": failed})
            if code is None:
                record["status"] = "timeout"
            elif ran == 0:
                record["status"] = "unexecuted"
            elif failed:
                record["status"] = "killed"
                expected = mutant.get("expect", [])
                record["expect_met"] = all(name in failed for name in expected)
            elif code == 0:
                record["status"] = "survived"
            else:
                record["status"] = "crash"
                record["detail"] = output[-1500:]
    finally:
        path.write_bytes(original)
    restored = path.read_bytes()
    record["sha256_bytes"]["restored"] = sha256(restored)
    record["sha256_lf"]["restored"] = sha256(lf(restored.decode("utf-8")).encode("utf-8"))
    record["restored_identically"] = restored == original
    record["seconds"] = round(time.time() - started)
    return record


def run(root, data, out_path, machine, only, timeout, resume):
    mutants = [m for m in data["mutants"] if not only or m["id"] in only]
    done = {}
    if resume and Path(out_path).is_file():
        done = {r["id"]: r for r in json.loads(Path(out_path).read_text("utf-8"))["results"]}
    commands = sorted({m["cmd"] for m in mutants})
    summary = {
        "machine": machine,
        "environment": environment(root),
        "list_base": data.get("base"),
        "baseline_before": baseline(root, commands, timeout),
        "results": list(done.values()),
    }
    if not all(b["green"] for b in summary["baseline_before"].values()):
        write(out_path, summary)
        raise SystemExit("the unmodified selector is not green; no mutant was run")

    def save():
        write(out_path, summary)

    save()
    for mutant in mutants:
        if mutant["id"] in done:
            continue
        record = replay(root, mutant, timeout)
        summary["results"].append(record)
        save()
        print(
            f"{record['id']}: {record['status']} ({record.get('seconds', 0)} s)"
            + (f" failed={record['failed_tests'][:3]}" if record.get("failed_tests") else ""),
            flush=True,
        )
        if not record.get("restored_identically", True):
            raise SystemExit(f"{record['id']}: the file was not restored byte-identically")
    summary["baseline_after"] = baseline(root, commands, timeout)
    summary["killed"] = sum(r["status"] == "killed" for r in summary["results"])
    summary["total"] = len(summary["results"])
    summary["tree_clean_after"] = environment(root)["tree_clean"]
    save()
    print(f"{summary['killed']} of {summary['total']} killed; tree clean: {summary['tree_clean_after']}")
    return summary["killed"] == summary["total"]


def write(path, summary):
    with open(path, "w", encoding="utf-8", newline="\n") as handle:
        handle.write(json.dumps(summary, indent=1) + "\n")


def merge(list_path, out_paths):
    data = read_list(list_path)
    index = {m["id"]: m for m in data["mutants"]}
    for out_path in out_paths:
        summary = json.loads(Path(out_path).read_text(encoding="utf-8"))
        machine = summary["machine"]
        for record in summary["results"]:
            kept = {k: v for k, v in record.items() if k not in ("detail", "id")}
            kept["environment"] = {
                k: summary["environment"][k] for k in ("os", "rustc", "head")
            }
            kept["baseline_green"] = all(
                b["green"] for b in summary["baseline_before"].values()
            ) and all(b["green"] for b in summary.get("baseline_after", {}).values())
            index[record["id"]].setdefault("runs", {})[machine] = kept
    write(list_path, data)


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawTextHelpFormatter)
    sub = parser.add_subparsers(dest="mode", required=True)
    one = sub.add_parser("check")
    one.add_argument("list")
    two = sub.add_parser("run")
    two.add_argument("list")
    two.add_argument("out")
    two.add_argument("--machine", default="local")
    two.add_argument("--ids", default="")
    two.add_argument("--timeout", type=int, default=TIMEOUT)
    two.add_argument("--resume", action="store_true")
    three = sub.add_parser("merge")
    three.add_argument("list")
    three.add_argument("out", nargs="+")
    args = parser.parse_args()
    os.environ.setdefault("CARGO_INCREMENTAL", "0")
    if args.mode == "merge":
        merge(args.list, args.out)
        return 0
    data = read_list(args.list)
    if args.mode == "check":
        return 0 if check(ROOT, data) else 1
    if not check(ROOT, data):
        return 1
    only = set(filter(None, args.ids.split(",")))
    return 0 if run(ROOT, data, args.out, args.machine, only, args.timeout, args.resume) else 1


if __name__ == "__main__":
    sys.exit(main())
