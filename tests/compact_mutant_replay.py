#!/usr/bin/env python3
"""Replay hand-written mutants one build at a time and keep their evidence.

A list is JSON: {"campaign", "base", "mutants": [{"id", "desc", "file", "old",
"new", "cmd", "expect"?, "min_machines"?, "gap"?, "equivalent"?}]}. Each mutant is one anchored
replacement: `old` must occur exactly once in `file`. `cmd` is the argument list of one
cargo test command (for example "test --locked --offline --lib -- compact::source").
`expect` names the tests that must fail. `min_machines` raises the number of machines
that `verify` requires for this mutant above its default; a top-level `two_machines`
list does the same for many IDs at once. `gap` names the tree on which the mutant was a
test gap (it survived there): `verify` then needs a run on that tree, kept under
`before`, that survived with the whole evidence. `equivalent` is {"argument",
"invariant", "pinned_by", "pin_mutant", "caveat"?, "review"}: a mutant that no test can
tell from the original, with the invariant it rests on, the test that pins that invariant,
a second mutant that this test kills, what the argument does not cover (`caveat`), and the
reviewer ("pending" until an independent review has read it). A top-level `reused` names
mutants that another ledger replays:
{"ledger": a file of the tree, "obligations": {this list's ID: the ledger's ID}, "kit": [IDs the
ledger carries under the same ID]}; `verify` checks that the ledger holds them.

  python tests/compact_mutant_replay.py check LIST.json [--ids a,b]
  python tests/compact_mutant_replay.py run LIST.json OUT.json [--machine NAME] [--ids a,b]
  python tests/compact_mutant_replay.py finish LIST.json OUT.json
  python tests/compact_mutant_replay.py merge LIST.json OUT.json [OUT.json ...] [--before] [--ids a,b]
  python tests/compact_mutant_replay.py verify LIST.json [--min-machines N]

`check`, `run`, `finish` and `verify` take `--root DIR` to work in another checkout.
`check` needs no build: it proves that every anchor matches exactly once in the tree.
`run` checks only the mutants it is about to run, so a list can be replayed on an older
tree that lacks some anchors. `merge --before` keeps the runs as the state before the
lane (the previous tag, or the tree before a lane's tests), not as another machine.
`finish` closes a batch that was stopped between two mutants (it runs the unmodified
selectors once more and records them as the baseline after the last mutant).
`verify` re-checks the recorded runs against the tree it runs in (a run replayed on an
older tree is stale: replay it again) and exits non-zero unless every mutant has runs
on at least N machines, each a kill with the whole evidence below.

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


def crashed(output):
    """A toolchain crash (a killed or aborted compiler), not a compile error."""
    markers = re.search(r"\(exit code: 0x[0-9a-fA-F]+|\(signal: \d+|SIGSEGV|SIGKILL|out of memory", output)
    return markers is not None and "error[E" not in output


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
        for attempt in (1, 2):
            code, output, _ = cargo(root, mutant["cmd"], timeout, build_only=True)
            record["build_attempts"] = attempt
            if code in (0, None) or not crashed(output):
                break
        record["compiled"] = code == 0
        if code is None:
            record["status"] = "timeout"
        elif code != 0:
            record["status"] = "crash" if crashed(output) else "compile-error"
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
        # Only a kill or a survival is final; a crash, a timeout or an unexecuted run is replayed.
        earlier = json.loads(Path(out_path).read_text("utf-8"))["results"]
        done = {r["id"]: r for r in earlier if r["status"] in ("killed", "survived")}
    commands = sorted({m["cmd"] for m in mutants})
    summary = {
        "machine": machine,
        "environment": environment(root),
        "list_base": data.get("base"),
        "baseline_before": baseline(root, commands, timeout),
        "results": list(done.values()),
    }
    # A red selector never produces a kill, but it must not hide the other selectors.
    red = {cmd for cmd, result in summary["baseline_before"].items() if not result["green"]}
    if len(red) == len(commands):
        write(out_path, summary)
        raise SystemExit("the unmodified selector is not green; no mutant was run")

    def save():
        write(out_path, summary)

    save()
    for mutant in mutants:
        if mutant["id"] in done:
            continue
        if mutant["cmd"] in red:
            record = {"id": mutant["id"], "file": mutant["file"], "status": "baseline-red"}
        else:
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
    Path(path).parent.mkdir(parents=True, exist_ok=True)
    with open(path, "w", encoding="utf-8", newline="\n") as handle:
        handle.write(json.dumps(summary, indent=1) + "\n")


def finish(root, data, out_path, timeout):
    """Close a batch that was stopped between two mutants.

    Runs the unmodified selectors of the mutants that were recorded again and keeps
    that as the baseline after the last mutant. Refuses to run on a modified tree.
    """
    summary = json.loads(Path(out_path).read_text("utf-8"))
    if "baseline_after" in summary:
        print("the batch is already finished")
        return True
    if not environment(root)["tree_clean"]:
        raise SystemExit("the tree is modified: restore it before finishing the batch")
    command_of = {m["id"]: m["cmd"] for m in data["mutants"]}
    commands = sorted({command_of[r["id"]] for r in summary["results"] if r["id"] in command_of})
    summary["baseline_after"] = baseline(root, commands, timeout)
    summary["killed"] = sum(r["status"] == "killed" for r in summary["results"])
    summary["total"] = len(summary["results"])
    summary["tree_clean_after"] = True
    summary["stopped_early"] = True
    write(out_path, summary)
    print(f"{summary['killed']} of {summary['total']} killed; stopped early, baseline after recorded")
    return all(b["green"] for b in summary["baseline_after"].values())


def select(data, ids):
    """The list narrowed to the comma-separated IDs (all of it when there are none)."""
    only = set(filter(None, ids.split(",")))
    unknown = only - {m["id"] for m in data["mutants"]}
    if unknown:
        raise SystemExit("unknown mutant IDs: " + ", ".join(sorted(unknown)))
    return {**data, "mutants": [m for m in data["mutants"] if not only or m["id"] in only]}


def merge(list_path, out_paths, before=False, ids=""):
    data = read_list(list_path)
    index = {m["id"]: m for m in data["mutants"]}
    only = {m["id"] for m in select(data, ids)["mutants"]}
    section = "before" if before else "runs"
    for out_path in out_paths:
        summary = json.loads(Path(out_path).read_text(encoding="utf-8"))
        machine = summary["machine"]
        for record in summary["results"]:
            if record["id"] not in only:
                continue
            kept = {k: v for k, v in record.items() if k not in ("detail", "id")}
            kept["environment"] = {
                k: summary["environment"][k] for k in ("os", "rustc", "head")
            }
            # Green before the first and again after the last mutant of the batch.
            kept["baseline_green"] = (
                bool(summary.get("baseline_after"))
                and all(b["green"] for b in summary["baseline_before"].values())
                and all(b["green"] for b in summary["baseline_after"].values())
            )
            index[record["id"]].setdefault(section, {})[machine] = kept
    write(list_path, data)


def verify(root, data, minimum):
    """Re-check the recorded evidence against the tree under `root`.

    A mutant is accepted when it has runs on at least `minimum` machines (or its own
    `min_machines`) and every run is a kill with the whole evidence, and the file it
    was replayed on is the file in this tree. A run replayed on an older tree is
    stale, never a failure of the mutant: replay it again on this tree.

    A mutant with an `equivalent` record is accepted only when every run survived
    with the same evidence (compiled, restored, green baseline) and the record names
    its reviewer; until then it waits for review. It must also name the test that
    pins the invariant it rests on and a pin mutant that this test kills.
    """
    names = {m["id"]: m for m in data["mutants"]}
    counts = {"accepted": 0, "stale": 0, "open": 0}
    for mutant in data["mutants"]:
        needed = mutant.get(
            "min_machines", 2 if mutant["id"] in data.get("two_machines", ()) else minimum
        )
        path = root / mutant["file"]
        text = lf(path.read_text(encoding="utf-8"))
        here = sha256(text.encode("utf-8"))
        runs = mutant.get("runs", {})
        equivalent = mutant.get("equivalent")
        wanted = "survived" if equivalent else "killed"
        problems, stale, good = [], [], 0
        if text.count(lf(mutant["old"])) != 1:
            problems.append(f"anchor matches {text.count(lf(mutant['old']))}")
        for machine, run_ in sorted(runs.items()):
            fails = []
            if run_.get("status") != wanted:
                fails.append(run_.get("status"))
            if run_.get("anchor_matches") != 1 or run_.get("compiled") is not True:
                fails.append("anchor or compile")
            expected = mutant.get("expect", [])
            if not equivalent and not all(n in run_.get("failed_tests", []) for n in expected):
                fails.append("named test did not fail")
            sums, raw = run_.get("sha256_lf", {}), run_.get("sha256_bytes", {})
            if not (sums.get("before") == sums.get("restored") and raw.get("before") == raw.get("restored")):
                fails.append("restore")
            if run_.get("restored_identically") is not True or run_.get("baseline_green") is not True:
                fails.append("restore or baseline")
            if sums.get("before") != here:
                stale.append(machine)
            elif not fails:
                good += 1
            else:
                problems.append(f"{machine}: {', '.join(map(str, fails))}")
        shapes = {
            (r.get("sha256_lf", {}).get("before"), r.get("sha256_lf", {}).get("mutated"))
            for r in runs.values()
        }
        if len(shapes) > 1:
            problems.append("machines disagree on the file or the mutant")
        pending = ""
        if equivalent:
            pin = names.get(equivalent.get("pin_mutant", ""))
            if not equivalent.get("pinned_by") or pin is None or not pin.get("expect"):
                problems.append("an equivalence needs pinned_by and a pin_mutant with expect")
            if str(equivalent.get("review", "pending")).strip().lower() in ("", "pending"):
                pending = "equivalence awaits independent review"
        gap, measured = mutant.get("gap"), 0
        if gap:
            # The tree before the lane's tests must not kill it: that is what makes the gap measured.
            for machine, run_ in sorted(mutant.get("before", {}).items()):
                if run_.get("status") != "survived":
                    problems.append(f"{machine}: not a gap on {gap} ({run_.get('status')})")
                elif (
                    run_.get("anchor_matches") == 1
                    and run_.get("compiled") is True
                    and run_.get("restored_identically") is True
                    and run_.get("baseline_green") is True
                ):
                    measured += 1
                else:
                    problems.append(f"{machine}: the run on {gap} lacks evidence")
            if not measured:
                problems.append(f"the gap on {gap} is not measured")
        if problems:
            state = "OPEN"
        elif stale and not good:
            state = "STALE"
        elif good < needed or pending:
            state = "OPEN"
        else:
            state = "ok"
        counts["accepted" if state == "ok" else ("stale" if state == "STALE" else "open")] += 1
        notes = problems + ([pending] if pending else []) + ([f"stale on {', '.join(stale)}"] if stale else [])
        if gap and measured:
            notes.append(f"survives on {gap}: {measured} run(s)")
        detail = "; ".join(notes)
        print(f"{mutant['id']}: {state} ({good} valid run(s){'; ' + detail if detail else ''})")
    reused = data.get("reused")
    if reused:
        # The seam mutants that another ledger replays: this list names them and does not repeat them.
        ids = sorted(set(reused.get("obligations", {}).values()) | set(reused.get("kit", [])))
        ledger = root / reused["ledger"]
        if not ledger.is_file():
            print(f"reused: {len(ids)} seam mutants rest on {reused['ledger']}, which is not in this tree")
        else:
            held = json.loads(ledger.read_text(encoding="utf-8"))
            entries = {entry["id"]: entry for entry in (held["mutants"] if isinstance(held, dict) else held)}
            for wanted_id in ids:
                entry = entries.get(wanted_id)
                final = (entry or {}).get("final")
                if entry is None:
                    note = "is not in"
                elif isinstance(final, dict) and (
                    final.get("status") != "killed" or final.get("restored_identically") is not True
                ):
                    note = "has no restored kill in"
                else:
                    continue
                print(f"reused: {wanted_id} {note} {reused['ledger']}")
                counts["open"] += 1
    print(f"{counts['accepted']} accepted, {counts['stale']} stale, {counts['open']} open of {len(data['mutants'])}")
    return counts["stale"] == 0 and counts["open"] == 0


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawTextHelpFormatter)
    common = argparse.ArgumentParser(add_help=False)
    common.add_argument("--root", default=str(ROOT), help="the checkout to use (default: this one)")
    sub = parser.add_subparsers(dest="mode", required=True)
    one = sub.add_parser("check", parents=[common])
    one.add_argument("list")
    one.add_argument("--ids", default="")
    two = sub.add_parser("run", parents=[common])
    two.add_argument("list")
    two.add_argument("out")
    two.add_argument("--machine", default="local")
    two.add_argument("--ids", default="")
    two.add_argument("--timeout", type=int, default=TIMEOUT)
    two.add_argument("--resume", action="store_true")
    three = sub.add_parser("merge")
    three.add_argument("list")
    three.add_argument("out", nargs="+")
    three.add_argument("--before", action="store_true", help="keep the runs as the state before the lane (the gap)")
    three.add_argument("--ids", default="", help="keep only these mutants of the runs")
    four = sub.add_parser("verify", parents=[common])
    four.add_argument("list")
    four.add_argument("--min-machines", type=int, default=1)
    five = sub.add_parser("finish", parents=[common])
    five.add_argument("list")
    five.add_argument("out")
    five.add_argument("--timeout", type=int, default=TIMEOUT)
    args = parser.parse_args()
    os.environ.setdefault("CARGO_INCREMENTAL", "0")
    if args.mode == "merge":
        merge(args.list, args.out, args.before, args.ids)
        return 0
    root = Path(args.root).resolve()
    data = read_list(args.list)
    if args.mode == "finish":
        return 0 if finish(root, data, args.out, args.timeout) else 1
    if args.mode == "verify":
        return 0 if verify(root, data, args.min_machines) else 1
    chosen = select(data, args.ids)
    anchored = check(root, chosen)
    if args.mode == "check" or not anchored:
        return 0 if anchored else 1
    return 0 if run(root, chosen, args.out, args.machine, set(), args.timeout, args.resume) else 1


if __name__ == "__main__":
    sys.exit(main())
