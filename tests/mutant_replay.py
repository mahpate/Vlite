#!/usr/bin/env python
"""Plan, replay and merge hand-mutant ledgers with the replay tools the repository already has.

Why: tests/k71_mutants.py and tests/compact_mutant_replay.py replay a ledger one mutant at a time
in one tree, and every mutant is a cargo build, so a whole ledger takes hours. This front end only
changes how that work is scheduled, never what is measured. It splits a ledger over CI jobs
(`plan`, `run --shard`) and merges the shard results into one verdict table (`merge`). Anchors,
restores and verdicts stay in the two existing tools: they are called as black boxes, from the copy
inside the tree under test, and are never edited.

Supported ledgers (`load_ledger` detects the kind):
  list     a JSON list of {id, desc, file, old, new, cmd}, or a dict {"mutants": [...]} of such
           entries whose cmd starts with "cargo "       -> <tree>/tests/k71_mutants.py
  compact  a dict {"campaign", "base", "mutants": [...]} whose cmd is the argument list after
           cargo, for example "test --locked --lib"     -> <tree>/tests/compact_mutant_replay.py
Entries with `edits` (several replacements) or without file/old/new/cmd are `unsupported`: they
are never replayed, every output lists their ids, and `merge` reports them without failing on
them. Any other JSON shape is refused. The compact tool is not on every ref: a compact ledger
needs tests/compact_mutant_replay.py in the tree under test and `run` stops with a message when
it is missing. A mutant is expected to be killed, unless the ledger calls it equivalent
(`equivalent` is set, `result` is "equivalent" or `expected` is "survived") or --survivors-ok
names it.

Sharding: `plan` caps the shards at --max-shards and at the number of mutants, and shard I of N
takes the mutants at positions p with p % N == I. Neighbours in a ledger usually share one cargo
build, so interleaving spreads that cost over every shard. A CI job runs `run --shard I/N` on a
fresh runner with the default work dir (mutants.yml saves and restores its target dir with
rust-cache), uploads its --out file, and one `merge` job combines the files.

Kept target dir: the worker replays in --root itself with CARGO_TARGET_DIR <work-dir>/target-0.
The default work dir is <root>/target/mutant-replay, so `cargo clean` removes it too. The target
dir stays after the run, so the next run on the same checkout starts warm: only the crate and the
tests that the changed sources affect are rebuilt, not every dependency. Use one run per work dir
at a time. A restore that fails leaves a mutated file in --root (exit 2): fix it with
`git checkout`.

Results: `run --out` writes one normalized document {tool, kind, ledger, shard, machine,
head, started_at, finished_at, wall_seconds, results, unsupported}. `results` maps each
replayed id to {status (killed, survived or invalid), tool_status (the word of the tool itself),
failed_tests (at most 5), seconds, restored, expected}. A selected mutant that its
tool never reported (the tool died, or the whole job timed out) is invalid with tool_status
"no-result". For a compact ledger `--native-out FILE` also writes the OUT file of the compact
tool, which its own `merge` reads.

Exit codes of `run`: 0 when the replay completed (survivors and invalid verdicts are data), 1 for
tool, ledger or usage errors and for a tool that died before every selected mutant had a result,
2 when a restore failed. `merge` exits 0 when no replayable mutant is unexpected or missing, else 1
(0 with --report-only).

Usage:
  python tests/mutant_replay.py plan --root . --ledger tests/k71_mutants.json --shards 20
  python tests/mutant_replay.py run --root . --ledger tests/k71_mutants.json --shard 3/20 \\
      --out results/shard-3.json
  python tests/mutant_replay.py merge --root . --ledger tests/k71_mutants.json --results results/ \\
      --summary summary.md
"""
import argparse
import collections
import datetime
import json
import os
import platform
import subprocess
import sys
import tempfile
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
TOOLS = {"list": "k71_mutants.py", "compact": "compact_mutant_replay.py"}
K71_STATUS = {"KILLED": "killed", "SURVIVED": "survived", "INVALID": "invalid"}
COMPACT_STATUS = {"killed": "killed", "survived": "survived"}
SHAPES = (
    "supported shapes:\n"
    "  - a JSON list of {id, desc, file, old, new, cmd}\n"
    '  - a dict {"mutants": [...]} of such entries whose cmd starts with "cargo "\n'
    '  - a dict {"campaign", "base", "mutants": [...]} whose cmd is the argument list after cargo'
)
TOTAL_COLUMNS = (
    "mutants",
    "killed",
    "survived",
    "invalid",
    "missing",
    "unsupported",
    "unexpected",
)


def say(text="", stream=None):
    """Print one line."""
    stream = stream or sys.stdout
    try:
        print(text, file=stream, flush=True)
    except UnicodeEncodeError:
        # A console that cannot show a character must not stop a replay that runs for hours.
        encoding = getattr(stream, "encoding", None) or "ascii"
        print(text.encode(encoding, "replace").decode(encoding), file=stream, flush=True)


def posix(path):
    return str(path).replace("\\", "/")


def utc_now():
    return datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def parse_time(text):
    if not isinstance(text, str):
        return None
    for form in ("%Y-%m-%dT%H:%M:%SZ", "%Y-%m-%dT%H:%M:%S.%fZ"):
        try:
            return datetime.datetime.strptime(text, form)
        except ValueError:
            continue
    return None


def shorten(text, width=60):
    text = " ".join(str(text).split())
    return text if len(text) <= width else text[: width - 3] + "..."


def write_text(path, text, append=False):
    """Write UTF-8 with LF. A plain write replaces the file atomically; `append` is for CI files."""
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    if append:
        with open(str(path), "a", encoding="utf-8", newline="\n") as handle:
            handle.write(text)
        return
    # A reader (or an interrupted run) must never see half a results file.
    temporary = path.with_name("%s.%d.tmp" % (path.name, os.getpid()))
    with open(str(temporary), "w", encoding="utf-8", newline="\n") as handle:
        handle.write(text)
    for attempt in range(5):
        try:
            os.replace(str(temporary), str(path))
            return
        except PermissionError:
            # On Windows a scanner or an editor can hold the target for a moment.
            if attempt == 4:
                raise
            time.sleep(0.1 * (attempt + 1))


def write_json(path, data):
    write_text(path, json.dumps(data, indent=1) + "\n")


def read_json(path, default=None):
    try:
        return json.loads(Path(path).read_text(encoding="utf-8-sig"))
    except (OSError, ValueError):
        return default


# --- ledgers -------------------------------------------------------------------------------


class Ledger(object):
    """A ledger split into what the replay tools can run and the ids they cannot run."""

    def __init__(self, path, kind, top, entries, unsupported):
        self.path = str(path)
        self.kind = kind
        self.top = top
        self.entries = entries
        self.unsupported = unsupported
        self.mutants = [
            {
                "id": entry["id"],
                "desc": entry.get("desc") if isinstance(entry.get("desc"), str) else "",
                "file": entry["file"],
                "old": entry["old"],
                "new": entry["new"],
                "cmd": entry["cmd"],
            }
            for entry in entries.values()
        ]

    def known_ids(self):
        return set(self.entries) | set(self.unsupported)

    def expected_map(self, survivors_ok=()):
        """Expected verdict of every replayable mutant; --survivors-ok forces "survived"."""
        forced = set(survivors_ok)
        unknown = forced - self.known_ids()
        if unknown:
            raise SystemExit("unknown mutant ids in --survivors-ok: " + listing(unknown))
        return {
            name: "survived" if name in forced else expected_of(entry)
            for name, entry in self.entries.items()
        }


def listing(names, limit=5):
    names = sorted(names)
    text = ", ".join(names[:limit])
    return text + (" (+%d more)" % (len(names) - limit) if len(names) > limit else "")


def expected_of(entry):
    """"survived" for a mutant the ledger itself calls equivalent, else "killed"."""
    if (
        entry.get("equivalent")
        or entry.get("result") == "equivalent"
        or entry.get("expected") == "survived"
    ):
        return "survived"
    return "killed"


def replayable(entry):
    """One anchored replacement and one command: what both replay tools take."""
    return (
        "edits" not in entry
        and all(isinstance(entry.get(key), str) and entry[key] for key in ("file", "cmd", "old"))
        and isinstance(entry.get("new"), str)
    )


def is_cargo(command):
    return command.lstrip().startswith("cargo ")


def load_ledger(path):
    """Read a ledger of one of the supported shapes; SystemExit with the shapes for any other."""
    path = Path(path)
    try:
        data = json.loads(path.read_text(encoding="utf-8-sig"))
    except (OSError, ValueError) as error:
        raise SystemExit("cannot read the ledger %s: %s" % (path, error))
    if isinstance(data, list):
        top, raw = None, data
    elif isinstance(data, dict) and isinstance(data.get("mutants"), list):
        top, raw = data, data["mutants"]
    else:
        raise SystemExit("%s is not a mutant ledger\n%s" % (path.name, SHAPES))
    names = []
    for position, entry in enumerate(raw):
        name = entry.get("id") if isinstance(entry, dict) else None
        if not isinstance(name, str) or not name or "," in name:
            raise SystemExit(
                "%s: entry %d has no usable id (a string without commas)" % (path.name, position)
            )
        names.append(name)
    repeated = [name for name, count in collections.Counter(names).items() if count > 1]
    if repeated:
        raise SystemExit("%s: duplicate mutant ids: %s" % (path.name, listing(repeated)))
    if raw and not any(("old" in e and "new" in e) or "edits" in e for e in raw):
        raise SystemExit("%s has entries without old/new or edits\n%s" % (path.name, SHAPES))
    entries, unsupported = {}, []
    for entry in raw:
        if replayable(entry):
            entries[entry["id"]] = entry
        else:
            unsupported.append(entry["id"])
    commands = [e["cmd"] for e in raw if isinstance(e.get("cmd"), str)]
    if top is None:
        kind = "list"
    elif commands and all(is_cargo(c) for c in commands):
        kind = "list"
    elif commands and not any(is_cargo(c) for c in commands):
        kind = "compact"
    elif commands:
        raise SystemExit("%s mixes cargo commands and argument lists\n%s" % (path.name, SHAPES))
    else:
        kind = "compact" if "base" in top else "list"
    return Ledger(path, kind, top, entries, unsupported)


def parse_ids(text):
    return [part.strip() for part in (text or "").split(",") if part.strip()]


def parse_shard(text):
    try:
        index, total = (int(part) for part in text.split("/"))
    except ValueError:
        raise SystemExit("--shard wants I/N, for example 3/20, not %r" % text)
    if total < 1 or not 0 <= index < total:
        raise SystemExit("--shard I/N needs 0 <= I < N, not %s" % text)
    return index, total


def pick_shard(mutants, index, total):
    """Shard `index` of `total`: the mutants at the positions p with p % total == index."""
    return [m for position, m in enumerate(mutants) if position % total == index]


def select_mutants(ledger, ids=(), shard=None):
    """The mutants a run works on: the id filter first, then the shard."""
    wanted = set(ids)
    unknown = wanted - ledger.known_ids()
    if unknown:
        raise SystemExit("unknown mutant ids: " + listing(unknown))
    chosen = [m for m in ledger.mutants if not wanted or m["id"] in wanted]
    return pick_shard(chosen, *shard) if shard else chosen


def unsupported_of(ledger, ids=()):
    wanted = set(ids)
    return [name for name in ledger.unsupported if not wanted or name in wanted]


# --- git and the replay tool ----------------------------------------------------------------


def git(cwd, *args):
    try:
        return subprocess.run(
            ["git"] + list(args),
            cwd=str(cwd),
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
    except OSError as error:
        raise SystemExit("cannot run git: %s" % error)


def head_of(root):
    done = git(root, "rev-parse", "HEAD")
    return done.stdout.decode("utf-8", "replace").strip() if done.returncode == 0 else ""


def require_tool(kind, tree):
    tool = Path(tree) / "tests" / TOOLS[kind]
    if tool.is_file():
        return tool
    if kind == "compact":
        raise SystemExit(
            "this ledger needs tests/compact_mutant_replay.py in the tree under test\n"
            "(%s)\nrun from a ref that has it" % tree
        )
    raise SystemExit("tests/k71_mutants.py is missing in the tree under test (%s)" % tree)


# --- running the tools ----------------------------------------------------------------------


class Job(object):
    """One worker: the tree it replays in, its target directory and its share of the mutants."""

    def __init__(self, index, tree, target, mutants):
        self.index = index
        self.tree = Path(tree)
        self.target = Path(target)
        self.mutants = mutants


class Outcome(object):
    """What a worker left behind: its raw results file and whether the tool misbehaved."""

    def __init__(self, job):
        self.job = job
        self.code = None
        self.raw = None
        self.error = None
        self.restore_failed = False
        self.missing = []


def job_environment(target):
    env = dict(os.environ)
    env["CARGO_TARGET_DIR"] = str(target)
    env["CARGO_INCREMENTAL"] = "0"
    env.setdefault("PYTHONIOENCODING", "utf-8")
    return env


def tool_command(config, job, list_file, out_file):
    tool = job.tree / "tests" / TOOLS[config["kind"]]
    ledger = config["ledger"]
    if config["kind"] == "list":
        # k71_mutants.py loads its --list as a JSON list and applies --timeout per mutant.
        write_json(list_file, job.mutants)
        command = [sys.executable, str(tool), "--list", str(list_file)]
        command += ["--target-dir", str(job.target), "--out", str(out_file)]
        command += ["--timeout", str(config["timeout"])]
        return command + (["--no-offline"] if config["no_offline"] else [])
    payload = dict(ledger.top)
    payload["mutants"] = [ledger.entries[m["id"]] for m in job.mutants]
    write_json(list_file, payload)
    ids = ",".join(m["id"] for m in job.mutants)
    command = [sys.executable, str(tool), "run", str(list_file), str(out_file)]
    command += ["--machine", config["machine"], "--ids", ids, "--root", str(job.tree)]
    return command + ["--timeout", str(config["timeout"])]


def run_job(config, job, scratch):
    """Run the tool once in `job.tree`, forwarding its output; never raises."""
    outcome = Outcome(job)
    try:
        list_file = scratch / ("ledger-%d.json" % job.index)
        out_file = scratch / ("out-%d.json" % job.index)
        command = tool_command(config, job, list_file, out_file)
        process = subprocess.Popen(
            command,
            cwd=str(job.tree),
            env=job_environment(job.target),
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            universal_newlines=True,
            encoding="utf-8",
            errors="replace",
            bufsize=1,
        )
        for line in process.stdout:
            line = line.rstrip("\r\n")
            if "RESTORE FAILED" in line or "not restored byte-identically" in line:
                outcome.restore_failed = True
            say(line)
        outcome.code = process.wait()
        if out_file.is_file():
            outcome.raw = read_json(out_file)
        else:
            outcome.error = "the tool wrote no results file (exit %s)" % outcome.code
    except Exception as error:  # a failure is reported as the outcome, not as a traceback
        outcome.error = "%s: %s" % (type(error).__name__, shorten(error, 80))
    return outcome


def seconds_of(value):
    if isinstance(value, (int, float)) and not isinstance(value, bool):
        return int(round(value))
    return 0


def make_record(entry, statuses, restored_key, tail_key, expected):
    """The normalized record of one mutant from the raw entry of a tool (None: no result)."""
    if not isinstance(entry, dict):
        return {
            "status": "invalid",
            "tool_status": "no-result",
            "failed_tests": [],
            "seconds": 0,
            "restored": None,
            "expected": expected,
        }
    raw = entry.get("status") if isinstance(entry.get("status"), str) else "unknown"
    restored = entry.get(restored_key)
    record = {
        "status": statuses.get(raw, "invalid"),
        "tool_status": raw,
        "failed_tests": [str(name) for name in (entry.get("failed_tests") or [])][:5],
        "seconds": seconds_of(entry.get("seconds")),
        "restored": restored if isinstance(restored, bool) else None,
        "expected": expected,
    }
    tail = entry.get(tail_key)
    if record["status"] == "invalid" and isinstance(tail, str) and tail.strip():
        record["tail"] = " ".join(tail.split())[-300:]
    return record


def raw_table(kind, raw):
    """{id: raw entry} from the results file of the tool."""
    if kind == "list":
        return raw if isinstance(raw, dict) else {}
    results = raw.get("results") if isinstance(raw, dict) else None
    return {r["id"]: r for r in results or [] if isinstance(r, dict) and "id" in r}


def normalize_outcome(config, outcome, expected):
    """Normalized records of every mutant of the worker; the ids without a result are listed."""
    table = raw_table(config["kind"], outcome.raw)
    if config["kind"] == "list":
        statuses, restored_key, tail_key = K71_STATUS, "restored", "tail"
    else:
        statuses, restored_key, tail_key = COMPACT_STATUS, "restored_identically", "detail"
    records = {}
    for mutant in outcome.job.mutants:
        name = mutant["id"]
        entry = table.get(name)
        if entry is None:
            outcome.missing.append(name)
        records[name] = make_record(entry, statuses, restored_key, tail_key, expected[name])
    return records


# --- commands -------------------------------------------------------------------------------


def existing_dir(text):
    path = Path(text).resolve()
    if not path.is_dir():
        raise SystemExit("no such directory: %s" % text)
    return path


def resolve_ledger(root, given):
    path = Path(given)
    if path.is_absolute():
        return path
    inside_root = root / path
    return inside_root if inside_root.exists() or not path.exists() else path


def work_dir_of(root, text):
    return Path(text).resolve() if text else root / "target" / "mutant-replay"


def cmd_plan(args):
    ledger = load_ledger(resolve_ledger(existing_dir(args.root), args.ledger))
    ids = parse_ids(args.ids)
    count = len(select_mutants(ledger, ids))
    total = 0 if count == 0 else max(1, min(args.shards, args.max_shards, count))
    shards = list(range(total))
    plan = {
        "kind": ledger.kind,
        "count": count,
        "total_shards": total,
        "shards": shards,
        "unsupported": unsupported_of(ledger, ids),
    }
    say(json.dumps(plan, separators=(",", ":")))
    output = os.environ.get("GITHUB_OUTPUT")
    if output:
        lines = ["kind=%s" % ledger.kind, "count=%d" % count, "total_shards=%d" % total]
        lines.append("shards=" + json.dumps(shards, separators=(",", ":")))
        write_text(output, "\n".join(lines) + "\n", append=True)
    return 0


def cmd_run(args):
    started_at, began = utc_now(), time.monotonic()
    root = existing_dir(args.root)
    ledger = load_ledger(resolve_ledger(root, args.ledger))
    expected = ledger.expected_map(parse_ids(args.survivors_ok))
    ids = parse_ids(args.ids)
    shard = parse_shard(args.shard) if args.shard else None
    todo = select_mutants(ledger, ids, shard)
    order = [m["id"] for m in todo]
    work = work_dir_of(root, args.work_dir)
    head = head_of(root)
    if args.native_out and ledger.kind != "compact":
        say("warning: --native-out is only used for compact ledgers", sys.stderr)
    document = {
        "tool": TOOLS[ledger.kind],
        "kind": ledger.kind,
        "ledger": args.ledger,
        "shard": "%d/%d" % shard if shard else None,
        "machine": args.machine or platform.node() or "local",
        "head": head or None,
        "started_at": started_at,
        "finished_at": None,
        "wall_seconds": None,
        "results": {},
        "unsupported": unsupported_of(ledger, ids),
    }
    results = {}

    def flush():
        document["results"] = {name: results[name] for name in order if name in results}
        document["finished_at"] = utc_now()
        document["wall_seconds"] = round(time.monotonic() - began, 1)
        write_json(args.out, document)

    outcomes = []
    if todo:
        require_tool(ledger.kind, root)
        work.mkdir(parents=True, exist_ok=True)
        target = work / "target-0"
        target.mkdir(parents=True, exist_ok=True)
        job = Job(0, root, target, todo)
        config = {
            "kind": ledger.kind,
            "ledger": ledger,
            "timeout": args.timeout,
            "no_offline": args.no_offline,
            "machine": document["machine"],
        }
        say("replaying %d mutants: %s" % (len(todo), TOOLS[ledger.kind]))
        # The scratch files (ledger subsets and raw tool output) never outlive the run.
        with tempfile.TemporaryDirectory(prefix="mutant-replay-") as scratch:
            outcome = run_job(config, job, Path(scratch))
        outcomes.append(outcome)
        normal = normalize_outcome(config, outcome, expected)
        results.update(normal)
        flush()
    flush()
    if args.native_out and ledger.kind == "compact":
        raw = outcomes[0].raw if outcomes else None
        if not isinstance(raw, dict):
            say("warning: nothing was replayed, --native-out is not written", sys.stderr)
        else:
            write_json(args.native_out, raw)
    return report_run(args, results, outcomes)


def restore_failed(outcome, results):
    """The tool said so, or a record of the worker says the file was not restored."""
    return outcome.restore_failed or any(
        results[mutant["id"]]["restored"] is False for mutant in outcome.job.mutants
    )


def report_run(args, results, outcomes):
    """Print the short summary of a run and return its exit code."""
    count = collections.Counter(record["status"] for record in results.values())
    say(
        "replayed %d: %d killed, %d survived, %d invalid"
        % (len(results), count["killed"], count["survived"], count["invalid"])
    )
    differing = [(n, r) for n, r in results.items() if r["status"] != r["expected"]]
    for name, record in differing[:10]:
        say(
            "differs: %s expected %s, got %s"
            % (shorten(name, 40), record["expected"], record["status"])
        )
    if len(differing) > 10:
        say("differs: %d more" % (len(differing) - 10))
    say("wrote " + shorten(args.out, 80))
    failed = [outcome for outcome in outcomes if restore_failed(outcome, results)]
    for outcome in failed:
        say("!!! RESTORE FAILED in %s" % outcome.job.tree, sys.stderr)
        say("!!! a source file may still be mutated: check `git status` there", sys.stderr)
    if failed:
        return 2
    incomplete = [outcome for outcome in outcomes if outcome.error or outcome.missing]
    for outcome in incomplete:
        reason = outcome.error or "exit %s" % outcome.code
        names = listing(outcome.missing, 3)
        say("no result for %s (%s)" % (names, reason), sys.stderr)
    return 1 if incomplete else 0


def is_result_file(document):
    return isinstance(document, dict) and isinstance(document.get("results"), dict)


def load_result_files(paths):
    """[(path, document)] of the result files; a directory contributes every *.json under it."""
    documents = []
    for given in paths:
        path = Path(given)
        if path.is_dir():
            for found in sorted(path.rglob("*.json")):
                document = read_json(found)
                if is_result_file(document):
                    documents.append((found, document))
                else:
                    notice = "warning: skipping %s: not a replay result"
                    say(notice % shorten(found.name, 50), sys.stderr)
        elif path.is_file():
            document = read_json(path)
            if not is_result_file(document):
                raise SystemExit("%s is not a replay result file" % path)
            documents.append((path, document))
        else:
            raise SystemExit("no such results path: %s" % path)
    return documents


def combine(documents):
    """Results and unsupported ids of all files; a later file wins, except that a
    valid record is never replaced by an invalid one."""
    results, unsupported = {}, []
    for _path, document in documents:
        for name, record in document["results"].items():
            if not isinstance(record, dict):
                continue
            kept = results.get(name)
            worse = kept is not None and kept.get("status") != "invalid" and (
                record.get("status") == "invalid"
            )
            if not worse:
                results[name] = record
        unsupported += [n for n in document.get("unsupported") or [] if n not in unsupported]
    return results, unsupported


def verdict_of(name, results):
    record = results.get(name)
    if record is not None and record.get("status") in ("killed", "survived"):
        return record["status"]
    return "invalid" if record is not None else "missing"


def wall_time(documents, given):
    """(seconds, where it came from): --wall-seconds, else the span of the files, else their sum."""
    if given is not None:
        return given, "--wall-seconds"
    starts = [parse_time(d.get("started_at")) for _p, d in documents]
    ends = [parse_time(d.get("finished_at")) for _p, d in documents]
    starts, ends = [t for t in starts if t], [t for t in ends if t]
    if starts and ends:
        return (max(ends) - min(starts)).total_seconds(), "finished_at - started_at"
    sums = [d.get("wall_seconds") for _p, d in documents]
    sums = [value for value in sums if isinstance(value, (int, float))]
    return (sum(sums), "sum of wall_seconds") if sums else (None, "unknown")


def build_summary(args, ledger, documents, expected):
    results, extra_unsupported = combine(documents)
    rows = []
    for mutant in ledger.mutants:
        name = mutant["id"]
        record = results.get(name) or {}
        seconds = record.get("seconds")
        rows.append(
            {
                "id": name,
                "expected": expected[name],
                "verdict": verdict_of(name, results),
                "tool_status": record.get("tool_status"),
                "seconds": seconds if isinstance(seconds, (int, float)) else None,
                "failed_tests": list(record.get("failed_tests") or []),
                "tail": record.get("tail") or "",
            }
        )
    count = collections.Counter(row["verdict"] for row in rows)
    unexpected = [row for row in rows if row["verdict"] != row["expected"]]
    unsupported = list(ledger.unsupported)
    unsupported += [n for n in extra_unsupported if n not in unsupported]
    totals = {
        "mutants": len(rows),
        "killed": count["killed"],
        "survived": count["survived"],
        "invalid": count["invalid"],
        "missing": count["missing"],
        "unsupported": len(unsupported),
        "unexpected": len(unexpected),
    }
    heads = {d["head"] for _p, d in documents if isinstance(d.get("head"), str) and d["head"]}
    heads = sorted(heads)
    head = heads[0] if len(heads) == 1 else ("mixed" if heads else head_of(existing_dir(args.root)))
    wall, source = wall_time(documents, args.wall_seconds)
    replayable_count = len(rows)
    compute = sum(row["seconds"] or 0 for row in rows)
    per_mutant = {
        "wall": wall / 60.0 / replayable_count if wall is not None and replayable_count else None,
        "compute": compute / 60.0 / replayable_count if replayable_count else None,
    }
    return {
        "ledger": args.ledger,
        "kind": ledger.kind,
        "head": head or "",
        "ok": not unexpected and not totals["missing"],
        "totals": totals,
        "wall_seconds": wall,
        "wall_source": source,
        "minutes_per_mutant": per_mutant,
        "unsupported": unsupported,
        "unexpected": unexpected,
        "mutants": rows,
        "ignored_results": sorted(set(results) - set(expected)),
        "files": [posix(path) for path, _d in documents],
    }


def cell(text, width=70):
    return shorten(text, width).replace("|", "\\|")


def first_failure(row):
    if row["failed_tests"]:
        return row["failed_tests"][0]
    return "..." + row["tail"][-60:] if row["tail"] else "-"


def minutes(value):
    return "n/a" if value is None else "%.3f" % value


def table_row(*cells):
    return "| " + " | ".join(str(c) for c in cells) + " |"


def seconds_cell(row):
    return "-" if row["seconds"] is None else row["seconds"]


def render_markdown(summary):
    totals = summary["totals"]
    wall = summary["wall_seconds"]
    per = summary["minutes_per_mutant"]
    head = summary["head"][:12] or "unknown"
    lines = ["# Mutant replay: %s @ %s" % (summary["ledger"], head), ""]
    lines += [table_row(*TOTAL_COLUMNS), "|" + "|".join(["---:"] * len(TOTAL_COLUMNS)) + "|"]
    lines += [table_row(*(totals[name] for name in TOTAL_COLUMNS)), ""]
    lines.append(
        "Wall time: %s (%s)"
        % ("n/a" if wall is None else "%.1f min" % (wall / 60.0), summary["wall_source"])
    )
    lines.append(
        "Minutes per mutant: %s wall, %s compute (%d replayable)"
        % (minutes(per["wall"]), minutes(per["compute"]), totals["mutants"])
    )
    lines += ["", "## Differences from the ledger", ""]
    if summary["unexpected"]:
        lines += [
            "| id | expected | verdict | tool status | first failure | seconds |",
            "|---|---|---|---|---|---:|",
        ]
        for row in summary["unexpected"]:
            lines.append(
                table_row(
                    cell(row["id"], 50),
                    row["expected"],
                    row["verdict"],
                    cell(row["tool_status"] or "-", 30),
                    cell(first_failure(row)),
                    seconds_cell(row),
                )
            )
    else:
        lines.append("None: every replayable mutant has the expected verdict.")
    names = summary["unsupported"]
    if names:
        lines += ["", "## Unsupported entries (not replayed)", ""]
        lines.append(", ".join(names[:40]) + (" ..." if len(names) > 40 else ""))
    ignored = len(summary["ignored_results"])
    if ignored:
        lines += ["", "Ignored results for %d id(s) that are not in the ledger." % ignored]
    lines += ["", "<details><summary>All mutants (%d)</summary>" % len(summary["mutants"]), ""]
    lines += [
        "| id | expected | verdict | seconds | first failing test |",
        "|---|---|---|---:|---|",
    ]
    for row in summary["mutants"]:
        lines.append(
            table_row(
                cell(row["id"], 50),
                row["expected"],
                row["verdict"],
                seconds_cell(row),
                cell(first_failure(row)),
            )
        )
    return "\n".join(lines + ["", "</details>"]) + "\n"


def cmd_merge(args):
    root = existing_dir(args.root)
    ledger = load_ledger(resolve_ledger(root, args.ledger))
    expected = ledger.expected_map(parse_ids(args.survivors_ok))
    documents = load_result_files(args.results)
    summary = build_summary(args, ledger, documents, expected)
    text = render_markdown(summary)
    say(text.rstrip("\n"))
    if args.summary:
        # $GITHUB_STEP_SUMMARY collects the output of many steps, so it is appended to.
        step = os.environ.get("GITHUB_STEP_SUMMARY")
        mine = os.path.normcase(os.path.abspath(args.summary))
        shared = bool(step) and os.path.normcase(os.path.abspath(step)) == mine
        write_text(args.summary, text, append=shared)
    if args.json:
        write_json(args.json, summary)
    return 0 if summary["ok"] or args.report_only else 1


class Parser(argparse.ArgumentParser):
    """Usage errors exit with 1: 2 is reserved for a failed restore."""

    def error(self, message):
        self.print_usage(sys.stderr)
        raise SystemExit("%s: error: %s" % (self.prog, message))


def build_parser():
    parser = Parser(
        prog="mutant_replay.py",
        description=__doc__,
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    commands = parser.add_subparsers(dest="command", required=True)

    def add(name, handler, help_text):
        sub = commands.add_parser(name, help=help_text)
        sub.set_defaults(handler=handler)
        sub.add_argument(
            "--root", default=str(ROOT), help="the checkout under test (default: this one)"
        )
        sub.add_argument("--ledger", required=True, help="the mutant ledger (relative to --root)")
        return sub

    plan = add("plan", cmd_plan, "print the shards of a ledger as one JSON line")
    plan.add_argument("--ids", default="", help="comma-separated ids to plan for")
    plan.add_argument("--shards", type=int, default=20, help="shards wanted (default 20)")
    plan.add_argument("--max-shards", type=int, default=20, help="upper bound of the shards")

    run = add("run", cmd_run, "replay a ledger or one shard of it")
    run.add_argument("--out", required=True, help="normalized results file")
    run.add_argument("--shard", help="I/N: the mutants at the positions p with p %% N == I")
    run.add_argument("--ids", default="", help="comma-separated ids to replay")
    run.add_argument(
        "--work-dir", help="holds the kept target dir (default <root>/target/mutant-replay)"
    )
    run.add_argument("--timeout", type=int, default=3600, help="seconds per mutant (default 3600)")
    run.add_argument("--machine", help="machine label of the compact tool (default: the host name)")
    run.add_argument("--no-offline", action="store_true", help="forwarded to k71_mutants.py only")
    run.add_argument("--survivors-ok", default="", help="comma-separated ids that may survive")
    run.add_argument(
        "--native-out", help="compact ledgers: one OUT file for the compact tool's merge"
    )

    merge = add("merge", cmd_merge, "merge result files into a verdict summary")
    merge.add_argument("--results", nargs="+", required=True, help="result files or directories")
    merge.add_argument("--survivors-ok", default="", help="comma-separated ids that may survive")
    merge.add_argument(
        "--wall-seconds", type=float, help="wall time to report (default: from the files)"
    )
    merge.add_argument("--summary", help="write the Markdown summary here")
    merge.add_argument("--json", help="write the machine-readable summary here")
    merge.add_argument(
        "--report-only", action="store_true", help="exit 0 whatever the verdicts are"
    )
    return parser


def main(argv=None):
    args = build_parser().parse_args(argv)
    return args.handler(args)


if __name__ == "__main__":
    for stream in (sys.stdout, sys.stderr):
        if hasattr(stream, "reconfigure"):
            stream.reconfigure(errors="replace")
    sys.exit(main())
