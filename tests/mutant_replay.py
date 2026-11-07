#!/usr/bin/env python
"""Plan, replay and merge hand-mutant ledgers with the replay tools the repository already has.

Why: tests/k71_mutants.py and tests/compact_mutant_replay.py replay a ledger one mutant at a time
in one tree, and every mutant is a cargo build, so a whole ledger takes hours. This front end only
changes how that work is scheduled, never what is measured. It splits a ledger over CI jobs
(`plan`, `run --shard`), runs several isolated git worktrees at once (`run --jobs`), replays only
the mutants whose inputs changed (`run --only-changed`) and merges the shard results into one
verdict table (`merge`). Anchors, restores and verdicts stay in the two existing tools: they are
called as black boxes, from the copy inside the tree under test, and are never edited.

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

Isolated trees and warm starts (--jobs N with N > 1): worker i replays in the git worktree
<work-dir>/tree-i with CARGO_TARGET_DIR <work-dir>/target-i. The worktree is detached at HEAD of
--root and receives the uncommitted changes of tracked files (`git diff --binary HEAD`); untracked
files are not copied and a warning says so. Both paths stay after the run and are reused as they
are: cargo fingerprints and the metadata hashes of a build embed the absolute path of the
workspace, so a target directory is only warm for the path it was built in. Move or recreate the
tree and cargo rebuilds everything. The first run with N jobs is a cold start: every dependency
and the crate are compiled once in each job's target directory, which is the slow part of a
replay. A later run that finds the same paths is a warm start: only the crate and the tests that
the changed sources affect are rebuilt. No timings are given because they depend on the machine,
the crate and N; compare a first and a second run on your own. With --jobs 1 the tool replays in
--root itself and uses <work-dir>/target-0. The default work dir is <root>/target/mutant-replay,
so `cargo clean` removes it too (then run `git worktree prune`). To delete the state by hand, run
`git worktree remove --force <work-dir>/tree-I` for every I and then remove the work dir. Use one
run per work dir at a time. A restore that fails leaves a mutated file in the tree it happened in
(exit 2), and so does a tool that is killed hard: in --root fix it with `git checkout`; the job
trees are reset at the start of the next run.

Anchors: both tools stop at a mutant whose old text does not occur exactly once in its file, which
would leave every mutant after it without a verdict. `run` therefore checks every anchor in the
tree it replays in, never hands a stale one to a tool, and reports it invalid with tool_status
"anchor" and the reason in `tail`; `plan` warns about them. `merge --ids` limits the table to the
ids a replay was limited to, so that a partial replay does not report the others as missing.

--only-changed is for intermediate heads. Every run stores, per ledger path as given, the
fingerprint of each mutant it replayed in <work-dir>/record.json (--record overrides it): hashes of
the mutant text, of the baseline command, of the mutated file and of the test sources the command
selects, all over LF-normalized text so Windows and Linux agree. Test sources are the Cargo files;
for --test NAME tests/NAME.rs or tests/NAME/, tests/common, tests/fixtures and every file those
declare with `mod NAME;` or #[path = "..."], include! or embed with include_str! and
include_bytes!, followed from module to module; for a command with no target flag, --tests or
--all-targets every Rust file under tests/ as well; src/ (any file) and build.rs for the library,
a binary, examples or no --test; crates/ too for -p or --workspace. A test that builds a path at
run time and reads that file, or starts a binary, depends on sources this does not follow.
--only-changed skips a mutant whose stored fingerprint is unchanged and whose stored verdict is the
expected one. For a compact ledger it also accepts the run the ledger carries when no record
exists: expected status, one anchor, compiled, restored identically, green baseline, and the
hashes of the current file and of the current file with old replaced by new. It trusts that
nothing outside those inputs, for example a source file other than the mutated one, flips a
verdict. That is why it is for intermediate heads only: the independent second-machine replay, and
any `--min-machines 2` evidence, is needed once, at the final head, and is run without
--only-changed so that every mutant is replayed on exactly that tree.

Results: `run --out` writes one normalized document {tool, kind, ledger, shard, jobs, machine,
head, started_at, finished_at, wall_seconds, results, skipped, unsupported}. `results` maps each
replayed id to {status (killed, survived or invalid), tool_status (the word of the tool itself),
failed_tests (at most 5), seconds, restored, expected, fingerprint}. A selected mutant that its
tool never reported (the tool died, or the whole job timed out) is invalid with tool_status
"no-result". For a compact ledger `--native-out FILE` also writes the union of the workers' own
OUT files, which the compact tool's own `merge` reads.

Exit codes of `run`: 0 when the replay completed (survivors and invalid verdicts are data), 1 for
tool, ledger or usage errors and for a tool that died before every selected mutant had a result,
2 when a restore failed. `merge` exits 0 when no replayable mutant is unexpected or missing, else 1
(0 with --report-only).

Usage:
  python tests/mutant_replay.py plan --root . --ledger tests/k71_mutants.json --shards 20
  python tests/mutant_replay.py run --root . --ledger tests/k71_mutants.json --shard 3/20 \\
      --out results/shard-3.json
  python tests/mutant_replay.py run --root . --ledger tests/k71_mutants.json --jobs 4 \\
      --only-changed --out results.json
  python tests/mutant_replay.py merge --root . --ledger tests/k71_mutants.json --results results/ \\
      --summary summary.md
  python tests/mutant_replay.py changed --root . --ledger tests/k71_mutants.json
"""
import argparse
import collections
import concurrent.futures
import datetime
import hashlib
import json
import os
import platform
import posixpath
import re
import shlex
import shutil
import stat
import subprocess
import sys
import tempfile
import threading
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
UNTRACKED_DIRS = ("src", "crates", "tests", "proto", "examples", "bindings")
TOTAL_COLUMNS = (
    "mutants",
    "killed",
    "survived",
    "invalid",
    "missing",
    "skipped",
    "unsupported",
    "unexpected",
)
MISSING = hashlib.sha256(b"missing").hexdigest()
PRINT_LOCK = threading.Lock()
# Rust source files pull other files in with `mod NAME;` (maybe with #[path = "..."]) and
# include!, and embed data files with include_str! and include_bytes!.
MODULE_LINE = re.compile(
    r"^[ \t]*(?:#\[[^\]\n]*\][ \t]*)*(?:pub(?:\([^)\n]*\))?[ \t]+)?mod[ \t]+([A-Za-z_]\w*)[ \t]*;"
)
PATH_ATTRIBUTE = re.compile(r'#\[\s*path\s*=\s*"([^"\n]+)"\s*\]')
INCLUDE_MACRO = re.compile(r'\binclude(_str|_bytes)?!\s*\(\s*"([^"\n]+)"\s*\)')
SINGLE_TARGET_FLAGS = frozenset(
    (
        "--lib",
        "--bin",
        "--bins",
        "--example",
        "--examples",
        "--test",
        "--doc",
        "--bench",
        "--benches",
    )
)
CLOSURE_LIMIT = 5000


def say(text="", stream=None):
    """Print one line; the lock keeps the lines of concurrent workers whole."""
    stream = stream or sys.stdout
    with PRINT_LOCK:
        try:
            print(text, file=stream, flush=True)
        except UnicodeEncodeError:
            # A console that cannot show a character must not stop a replay that runs for hours.
            encoding = getattr(stream, "encoding", None) or "ascii"
            print(text.encode(encoding, "replace").decode(encoding), file=stream, flush=True)


def sha(data):
    return hashlib.sha256(data).hexdigest()


def lf_text(text):
    return text.replace("\r\n", "\n")


def lf_bytes(data):
    return data.replace(b"\r\n", b"\n")


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


def remove_tree(path):
    """rmtree that also removes the read-only files git creates on Windows."""

    def clear(function, target, _error):
        os.chmod(target, stat.S_IWRITE)
        function(target)

    if sys.version_info >= (3, 12):
        shutil.rmtree(str(path), onexc=clear)
    else:
        shutil.rmtree(str(path), onerror=clear)


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


# --- fingerprints ---------------------------------------------------------------------------


def command_words(command):
    try:
        words = shlex.split(command)
    except ValueError:
        words = command.split()
    if words and words[0] == "cargo":
        words = words[1:]
    # Words after a bare `--` go to the test harness (libtest has its own --test flag).
    return words[: words.index("--")] if "--" in words else words


def parse_selection(command):
    """(names of the --test targets, selects the library or a binary, selects a package)."""
    words = command_words(command)
    tests, library, package = [], False, False
    index = 0
    while index < len(words):
        flag, equals, value = words[index].partition("=")
        index += 1
        if flag in ("--test", "--bin", "--example", "-p", "--package", "--manifest-path"):
            if not equals and index < len(words):
                value = words[index]
                index += 1
            if flag == "--test":
                if value:
                    tests.append(value)
            elif flag in ("--bin", "--example"):
                library = True
            else:
                package = True
        elif flag in ("--lib", "--bins", "--examples", "--all-targets", "--doc"):
            library = True
        elif flag in ("--workspace", "--all"):
            package = True
    return tests, library, package


def selects_every_test(command):
    """True when the command runs every integration test target (no target flag, or --tests)."""
    flags = {word.partition("=")[0] for word in command_words(command) if word.startswith("-")}
    return bool(flags & {"--tests", "--all-targets"}) or not flags & SINGLE_TARGET_FLAGS


def module_declarations(text):
    """[(module name, explicit #[path] or None)] of the `mod NAME;` lines of Rust source text."""
    lines = text.split("\n")
    found = []
    for number, line in enumerate(lines):
        match = MODULE_LINE.match(line)
        if not match:
            continue
        explicit = PATH_ATTRIBUTE.search(line)
        before = number - 1
        while not explicit and before >= 0 and lines[before].lstrip().startswith("#["):
            explicit = PATH_ATTRIBUTE.search(lines[before])
            before -= 1
        found.append((match.group(1), explicit.group(1).replace("\\", "/") if explicit else None))
    return found


class Fingerprints(object):
    """The hashes that decide whether an earlier verdict still holds in the tree under `root`."""

    def __init__(self, root):
        self.root = Path(root)
        self._files = {}
        self._walks = {}
        self._tests = {}
        self._lists = {}

    def file_hash(self, relative):
        """SHA-256 of the LF-normalized file; the literal text "missing" when it is not there."""
        relative = posix(relative)
        if relative not in self._files:
            try:
                data = (self.root / relative).read_bytes()
            except OSError:
                self._files[relative] = MISSING
            else:
                self._files[relative] = sha(lf_bytes(data))
        return self._files[relative]

    def source_text(self, relative):
        try:
            return lf_text((self.root / posix(relative)).read_bytes().decode("utf-8"))
        except (OSError, ValueError):
            return None

    def walk(self, folder, suffixes=(".rs",)):
        """Relative paths of the matching files under `folder` (all files when `suffixes` is
        None); build output is never an input."""
        key = (folder, suffixes)
        if key not in self._walks:
            found = []
            for directory, names, files in os.walk(str(self.root / folder)):
                names[:] = [name for name in names if name != "target"]
                for name in files:
                    if suffixes is None or name.endswith(suffixes):
                        relative = os.path.relpath(os.path.join(directory, name), str(self.root))
                        found.append(posix(relative))
            self._walks[key] = sorted(found)
        return self._walks[key]

    def is_file(self, relative):
        return (self.root / relative).is_file()

    def referenced(self, relative, text):
        """(compiled, embedded): the existing files that one Rust source file declares as modules
        or include!s, and the data files it embeds with include_str! or include_bytes!."""
        directory, name = posixpath.split(relative)
        bases = [directory] + ([posixpath.join(directory, name[:-3])] if name != "mod.rs" else [])
        compiled, embedded = [], []

        def keep(candidates, into):
            for candidate in candidates:
                clean = posixpath.normpath(candidate)
                inside_root = clean.split("/")[0] != ".." and not clean.startswith("/")
                if inside_root and ":" not in clean and self.is_file(clean):
                    into.append(clean)

        for module, explicit in module_declarations(text):
            if explicit:
                keep([posixpath.join(base, explicit) for base in bases], compiled)
            else:
                for base in bases:
                    keep([posixpath.join(base, module + ".rs")], compiled)
                    keep([posixpath.join(base, module, "mod.rs")], compiled)
        for kind, target in INCLUDE_MACRO.findall(text):
            target = posixpath.join(directory, target.replace("\\", "/"))
            keep([target], embedded if kind else compiled)
        return compiled, embedded

    def closure(self, entries):
        """The entry files, every file they pull in as a module or include!, those files' own
        modules and so on, and every data file any of them embeds."""
        found, queue = set(), list(entries)
        while queue and len(found) < CLOSURE_LIMIT:
            relative = queue.pop()
            if relative in found:
                continue
            found.add(relative)
            text = self.source_text(relative) if relative.endswith(".rs") else None
            if text is None:
                continue
            compiled, embedded = self.referenced(relative, text)
            queue += compiled
            found.update(embedded)
        return found

    def test_files(self, command):
        """The files whose content decides what the tests of `command` do (see the module doc)."""
        key = " ".join(command.split())
        if key not in self._lists:
            self._lists[key] = self._test_files(command)
        return self._lists[key]

    def _test_files(self, command):
        tests, library, package = parse_selection(command)
        everything = selects_every_test(command)
        files, entries = ["Cargo.toml", "Cargo.lock"], []
        for name in tests:
            folder = self.root / "tests" / name
            if folder.is_dir():
                entries += self.walk("tests/" + name)
            if not folder.is_dir() or (self.root / "tests" / (name + ".rs")).is_file():
                files.append("tests/%s.rs" % name)
                entries.append("tests/%s.rs" % name)
        if everything:
            entries += self.walk("tests")
        if tests or everything:
            entries += self.walk("tests/common")
            files += self.walk("tests/fixtures", None)
        files += self.closure(entries)
        if library or not tests:
            files += self.walk("src", None)
            if (self.root / "build.rs").is_file():
                files.append("build.rs")
        if package:
            files += self.walk("crates", (".rs", "Cargo.toml"))
        return sorted(set(files))

    def tests_hash(self, command):
        files = tuple(self.test_files(command))
        if files not in self._tests:
            digest = hashlib.sha256()
            for relative in files:
                digest.update(("%s\0%s\n" % (relative, self.file_hash(relative))).encode("utf-8"))
            self._tests[files] = digest.hexdigest()
        return self._tests[files]

    def of(self, mutant):
        """The four fingerprints of a mutant: its text, its command, its tests and its file."""
        text = "\0".join([posix(mutant["file"]), lf_text(mutant["old"]), lf_text(mutant["new"])])
        command = " ".join(mutant["cmd"].split())
        if command.startswith("cargo "):
            command = command[len("cargo ") :]
        return {
            "mutant": sha(text.encode("utf-8")),
            "cmd": sha(command.encode("utf-8")),
            "tests": self.tests_hash(mutant["cmd"]),
            "file": self.file_hash(mutant["file"]),
        }

    def mutated_hash(self, mutant):
        """Hash of the LF-normalized file with `old` replaced by `new`.

        None unless `old` occurs exactly once, which is what both tools require of an anchor.
        """
        text = self.source_text(mutant["file"])
        old, new = lf_text(mutant["old"]), lf_text(mutant["new"])
        if text is None or text.count(old) != 1:
            return None
        return sha(text.replace(old, new, 1).encode("utf-8"))


def anchor_problems(fingerprints, kind, mutants):
    """{id: why} for the mutants whose anchor does not occur exactly once in their file.

    Both replay tools stop at such a mutant (k71_mutants.py raises, the compact tool refuses the
    whole run), which would leave every mutant after it without a verdict.
    """
    problems = {}
    for mutant in mutants:
        text = fingerprints.source_text(mutant["file"])
        if text is None:
            problems[mutant["id"]] = "cannot read " + mutant["file"]
            continue
        count = text.count(mutant["old"] if kind == "list" else lf_text(mutant["old"]))
        if count != 1:
            problems[mutant["id"]] = "the anchor occurs %d times in %s, expected exactly once" % (
                count,
                mutant["file"],
            )
    return problems


def carried_run(fingerprints, entry, expected):
    """The machine whose run, recorded in the ledger itself, still describes the current file."""
    runs = entry.get("runs")
    if not isinstance(runs, dict):
        return None
    here = fingerprints.file_hash(entry["file"])
    mutated = fingerprints.mutated_hash(entry)
    if here == MISSING or mutated is None:
        return None
    for machine, run in sorted(runs.items()):
        sums = run.get("sha256_lf") if isinstance(run, dict) else None
        if (
            isinstance(sums, dict)
            and run.get("status") == expected
            and run.get("anchor_matches") == 1
            and run.get("compiled") is True
            and run.get("restored_identically") is True
            and run.get("baseline_green") is True
            and sums.get("before") == here
            and sums.get("mutated") == mutated
        ):
            return machine
    return None


def split_changed(ledger, chosen, expected, fingerprints, records):
    """(mutants to replay, {id: reason} of the mutants whose earlier verdict still holds)."""
    todo, skipped = [], {}
    for mutant in chosen:
        name = mutant["id"]
        record = records.get(name)
        if isinstance(record, dict):
            same = record.get("fingerprint") == fingerprints.of(mutant)
            if same and record.get("status") == expected[name]:
                head = str(record.get("head") or "")[:12]
                skipped[name] = "unchanged since " + (head or "an earlier replay")
                continue
        elif ledger.kind == "compact" and carried_run(
            fingerprints, ledger.entries[name], expected[name]
        ):
            skipped[name] = "accepted by the ledger's recorded run"
            continue
        todo.append(mutant)
    return todo, skipped


def load_store(path):
    """The record of earlier replays: {ledger path: {id: {fingerprint, status, head, at}}}."""
    if not Path(path).is_file():
        return {}
    data = read_json(path)
    if not isinstance(data, dict):
        say("warning: ignoring the unreadable record file " + Path(path).name, sys.stderr)
        return {}
    return data


def store_records(store, ledger_path):
    """The records of one ledger; the store is repaired in place if that entry is not a dict."""
    records = store.get(ledger_path)
    if not isinstance(records, dict):
        records = store[ledger_path] = {}
    return records


# --- git and worktrees ----------------------------------------------------------------------


def git(cwd, *args, **options):
    try:
        return subprocess.run(
            ["git"] + list(args),
            cwd=str(cwd),
            input=options.get("input"),
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
    except OSError as error:
        raise SystemExit("cannot run git: %s" % error)


def git_failure(done, what):
    detail = shorten(done.stderr.decode("utf-8", "replace"), 80)
    return SystemExit("%s: %s" % (what, detail or "git exited with %d" % done.returncode))


def head_of(root):
    done = git(root, "rev-parse", "HEAD")
    return done.stdout.decode("utf-8", "replace").strip() if done.returncode == 0 else ""


def same_path(first, second):
    first, second = (os.path.normcase(os.path.realpath(str(p))) for p in (first, second))
    return first == second


def inside(path, folder):
    path, folder = (os.path.normcase(os.path.realpath(str(p))) for p in (path, folder))
    return path != folder and os.path.commonpath([path, folder]) == folder


def common_dir(cwd):
    done = git(cwd, "rev-parse", "--git-common-dir")
    if done.returncode != 0:
        return None
    return os.path.join(str(cwd), done.stdout.decode("utf-8", "replace").strip())


def is_worktree_of(common, tree):
    """True when `tree` is a worktree of the repository whose git directory is `common`.

    The check comes first because git walks up to an enclosing repository: a tree without its own
    .git inside <root>/target would otherwise make `checkout --force` hit the root itself.
    """
    if not (tree / ".git").exists():
        return False
    done = git(tree, "rev-parse", "--show-toplevel", "--git-common-dir")
    lines = done.stdout.decode("utf-8", "replace").splitlines()
    return (
        done.returncode == 0
        and len(lines) == 2
        and same_path(lines[0], tree)
        and same_path(os.path.join(str(tree), lines[1]), common)
    )


def prepare_tree(root, common, tree, work, head):
    """Make `tree` a clean detached worktree of `head`, keeping it if it is already one."""
    if tree.exists():
        if is_worktree_of(common, tree):
            moved = git(tree, "checkout", "--detach", "--force", head)
            if moved.returncode == 0:
                git(tree, "clean", "-fdq")
                return "reused"
        if not inside(tree, work) or not tree.name.startswith("tree-"):
            raise SystemExit("refusing to replace %s: it is not a job tree of %s" % (tree, work))
        remove_tree(tree)
    added = git(root, "worktree", "add", "--detach", "--force", str(tree), head)
    if added.returncode != 0:
        raise git_failure(added, "cannot add the worktree %s" % tree)
    return "added"


def require_tool(kind, tree):
    tool = Path(tree) / "tests" / TOOLS[kind]
    if tool.is_file():
        return tool
    if kind == "compact":
        raise SystemExit(
            "this ledger needs tests/compact_mutant_replay.py in the tree under test\n"
            "(%s)\nrun from a ref that has it, committed if --jobs is above 1" % tree
        )
    raise SystemExit("tests/k71_mutants.py is missing in the tree under test (%s)" % tree)


def uncommitted_patch(root):
    """The uncommitted changes of tracked files as a patch that `git apply` reads back.

    The options pin the format: a user's diff.noprefix or external diff driver would otherwise
    produce a patch that does not apply at the other end.
    """
    done = git(
        root,
        "diff",
        "--binary",
        "--no-color",
        "--no-ext-diff",
        "--no-textconv",
        "--src-prefix=a/",
        "--dst-prefix=b/",
        "HEAD",
    )
    if done.returncode != 0:
        raise git_failure(done, "cannot read the uncommitted changes of %s" % root)
    return done.stdout


def prepare_trees(root, work, count, head, kind):
    """Create or refresh the job trees tree-0..tree-(count-1) and copy the uncommitted changes."""
    if not head:
        raise SystemExit("--jobs above 1 needs --root to be a git checkout with a commit")
    common = common_dir(root)
    patch = uncommitted_patch(root)
    others = git(root, "ls-files", "--others", "--exclude-standard", "-z", "--", *UNTRACKED_DIRS)
    untracked = len([name for name in others.stdout.split(b"\0") if name])
    if untracked:
        say(
            "warning: %d untracked source file(s) are not copied to the job trees" % untracked,
            sys.stderr,
        )
    trees = []
    for index in range(count):
        tree = work / ("tree-%d" % index)
        prepare_tree(root, common, tree, work, head)
        if patch.strip():
            applied = git(tree, "apply", "--binary", "--whitespace=nowarn", "-", input=patch)
            if applied.returncode != 0:
                raise git_failure(applied, "cannot copy the uncommitted changes to %s" % tree)
        # After the patch: a tool that the root deleted without committing is missing here too.
        require_tool(kind, tree)
        trees.append(tree)
    return trees


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


def job_environment(target, workers):
    env = dict(os.environ)
    env["CARGO_TARGET_DIR"] = str(target)
    env["CARGO_INCREMENTAL"] = "0"
    env.setdefault("CARGO_BUILD_JOBS", str(max(1, (os.cpu_count() or 1) // workers)))
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


def drain(process):
    """Let a child that is still running finish; a tool that is stopped halfway through a mutant
    leaves the mutated file behind, one that finishes restores it."""
    try:
        for _line in process.stdout:
            pass
    except (OSError, ValueError):
        pass
    return process.wait()


def run_job(config, job, scratch):
    """Run the tool once in `job.tree`, forwarding its output; never raises."""
    outcome = Outcome(job)
    prefix = "[job %d] " % job.index if config["prefixed"] else ""
    process = None
    try:
        list_file = scratch / ("ledger-%d.json" % job.index)
        out_file = scratch / ("out-%d.json" % job.index)
        command = tool_command(config, job, list_file, out_file)
        process = subprocess.Popen(
            command,
            cwd=str(job.tree),
            env=job_environment(job.target, config["workers"]),
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            universal_newlines=True,
            encoding="utf-8",
            errors="replace",
            bufsize=1,
        )
        showing = True
        for line in process.stdout:
            line = line.rstrip("\r\n")
            if "RESTORE FAILED" in line or "not restored byte-identically" in line:
                outcome.restore_failed = True
            if showing:
                try:
                    say(prefix + line)
                except Exception:  # a console that went away must not abandon the tool
                    showing = False
        outcome.code = process.wait()
        if out_file.is_file():
            outcome.raw = read_json(out_file)
        else:
            outcome.error = "the tool wrote no results file (exit %s)" % outcome.code
    except Exception as error:  # a worker must not take the others down
        outcome.error = "%s: %s" % (type(error).__name__, shorten(error, 80))
    finally:
        if process is not None:
            if process.poll() is None:
                drain(process)
            process.stdout.close()
    return outcome


def stale_record(why, expected, fingerprint):
    """The record of a mutant that was never handed to a tool because its anchor is stale."""
    return {
        "status": "invalid",
        "tool_status": "anchor",
        "failed_tests": [],
        "seconds": 0,
        "restored": None,
        "expected": expected,
        "fingerprint": fingerprint,
        "tail": why,
    }


def seconds_of(value):
    if isinstance(value, (int, float)) and not isinstance(value, bool):
        return int(round(value))
    return 0


def make_record(entry, statuses, restored_key, tail_key, expected, fingerprint):
    """The normalized record of one mutant from the raw entry of a tool (None: no result)."""
    if not isinstance(entry, dict):
        return {
            "status": "invalid",
            "tool_status": "no-result",
            "failed_tests": [],
            "seconds": 0,
            "restored": None,
            "expected": expected,
            "fingerprint": fingerprint,
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
        "fingerprint": fingerprint,
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


def normalize_outcome(config, outcome, expected, prints):
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
        records[name] = make_record(
            entry, statuses, restored_key, tail_key, expected[name], prints[name]
        )
    return records


def merge_baselines(documents, key):
    """Union of the baselines; a red selector of any worker is never hidden by a green one."""
    merged = {}
    for document in documents:
        for command, result in (document.get(key) or {}).items():
            if command not in merged or (merged[command].get("green") and not result.get("green")):
                merged[command] = result
    return merged


def union_native(documents, order):
    """One compact-native OUT file from the OUT files of the workers (for its own `merge`)."""
    documents = [d for d in documents if isinstance(d, dict)]
    if not documents:
        return None
    first = documents[0]
    position = {name: index for index, name in enumerate(order)}
    results = {}
    for document in documents:
        for record in document.get("results") or []:
            if isinstance(record, dict) and "id" in record:
                results.setdefault(record["id"], record)
    ordered = sorted(results.values(), key=lambda r: position.get(r["id"], len(position)))
    union = {
        "machine": first.get("machine"),
        "environment": first.get("environment"),
        "sources": first.get("sources"),
        "list_base": first.get("list_base"),
        "baseline_before": merge_baselines(documents, "baseline_before"),
        "results": ordered,
    }
    # Evidence that a batch finished: one unfinished worker must not borrow the others' proof.
    if all("baseline_after" in d for d in documents):
        union["baseline_after"] = merge_baselines(documents, "baseline_after")
    union["killed"] = sum(r.get("status") == "killed" for r in ordered)
    union["total"] = len(ordered)
    clean = [d["tree_clean_after"] for d in documents if "tree_clean_after" in d]
    if clean:
        union["tree_clean_after"] = all(value is True for value in clean)
    return union


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
    root = existing_dir(args.root)
    ledger = load_ledger(resolve_ledger(root, args.ledger))
    ids = parse_ids(args.ids)
    chosen = select_mutants(ledger, ids)
    count = len(chosen)
    stale = anchor_problems(Fingerprints(root), ledger.kind, chosen)
    total = 0 if count == 0 else max(1, min(args.shards, args.max_shards, count))
    shards = list(range(total))
    plan = {
        "kind": ledger.kind,
        "count": count,
        "total_shards": total,
        "shards": shards,
        "stale": len(stale),
        "unsupported": unsupported_of(ledger, ids),
    }
    say(json.dumps(plan, separators=(",", ":")))
    if stale:
        say(
            "warning: %d mutant(s) have an anchor that does not occur exactly once and will be "
            "reported invalid: %s" % (len(stale), listing(stale)),
            sys.stderr,
        )
    output = os.environ.get("GITHUB_OUTPUT")
    if output:
        lines = ["kind=%s" % ledger.kind, "count=%d" % count, "total_shards=%d" % total]
        lines.append("stale=%d" % len(stale))
        lines.append("shards=" + json.dumps(shards, separators=(",", ":")))
        write_text(output, "\n".join(lines) + "\n", append=True)
    return 0


def cmd_changed(args):
    root = existing_dir(args.root)
    ledger = load_ledger(resolve_ledger(root, args.ledger))
    expected = ledger.expected_map(parse_ids(args.survivors_ok))
    shard = parse_shard(args.shard) if args.shard else None
    chosen = select_mutants(ledger, parse_ids(args.ids), shard)
    work = work_dir_of(root, args.work_dir)
    record_path = Path(args.record) if args.record else work / "record.json"
    records = store_records(load_store(record_path), args.ledger)
    todo, skipped = split_changed(ledger, chosen, expected, Fingerprints(root), records)
    for mutant in todo:
        say(mutant["id"])
    say("# %d of %d" % (len(todo), len(chosen)))
    for name, reason in skipped.items():
        say("# skip %s: %s" % (name, reason))
    return 0


def cmd_run(args):
    started_at, began = utc_now(), time.monotonic()
    root = existing_dir(args.root)
    ledger = load_ledger(resolve_ledger(root, args.ledger))
    if args.jobs < 1:
        raise SystemExit("--jobs must be at least 1")
    expected = ledger.expected_map(parse_ids(args.survivors_ok))
    ids = parse_ids(args.ids)
    shard = parse_shard(args.shard) if args.shard else None
    chosen = select_mutants(ledger, ids, shard)
    order = [m["id"] for m in chosen]
    work = work_dir_of(root, args.work_dir)
    record_path = Path(args.record) if args.record else work / "record.json"
    store = load_store(record_path)
    records = store_records(store, args.ledger)
    head = head_of(root)
    fingerprints = Fingerprints(root)
    prints = {m["id"]: fingerprints.of(m) for m in chosen}
    todo, skipped = chosen, {}
    if args.only_changed:
        todo, skipped = split_changed(ledger, chosen, expected, fingerprints, records)
        say("only changed: %d of %d mutants to replay" % (len(todo), len(chosen)))
    if args.native_out and ledger.kind != "compact":
        say("warning: --native-out is only used for compact ledgers", sys.stderr)
    document = {
        "tool": TOOLS[ledger.kind],
        "kind": ledger.kind,
        "ledger": args.ledger,
        "shard": "%d/%d" % shard if shard else None,
        "jobs": args.jobs,
        "machine": args.machine or platform.node() or "local",
        "head": head or None,
        "started_at": started_at,
        "finished_at": None,
        "wall_seconds": None,
        "results": {},
        "skipped": skipped,
        "unsupported": unsupported_of(ledger, ids),
    }
    results = {}

    def flush():
        document["results"] = {name: results[name] for name in order if name in results}
        document["finished_at"] = utc_now()
        document["wall_seconds"] = round(time.monotonic() - began, 1)
        write_json(args.out, document)

    def remember(new_records):
        stamp = utc_now()
        for name, record in new_records.items():
            if record["status"] in ("killed", "survived") and record["restored"] is not False:
                records[name] = {
                    "fingerprint": record["fingerprint"],
                    "status": record["status"],
                    "head": head,
                    "at": stamp,
                }
        write_json(record_path, store)

    outcomes = []
    if todo:
        if args.jobs == 1:
            require_tool(ledger.kind, root)
        work.mkdir(parents=True, exist_ok=True)
        workers = min(args.jobs, len(todo))
        trees = [root] if args.jobs == 1 else prepare_trees(root, work, workers, head, ledger.kind)
        jobs = []
        for index in range(workers):
            target = work / ("target-%d" % index)
            target.mkdir(parents=True, exist_ok=True)
            jobs.append(Job(index, trees[index], target, todo[index::workers]))
        stale = {}
        for job in jobs:
            problems = anchor_problems(Fingerprints(job.tree), ledger.kind, job.mutants)
            job.mutants = [m for m in job.mutants if m["id"] not in problems]
            stale.update(problems)
        if stale:
            for name, why in stale.items():
                results[name] = stale_record(why, expected[name], prints[name])
            notice = "warning: %d mutant(s) have a stale anchor and are reported invalid: %s"
            say(notice % (len(stale), listing(stale)), sys.stderr)
            flush()
        jobs = [job for job in jobs if job.mutants]
        config = {
            "kind": ledger.kind,
            "ledger": ledger,
            "timeout": args.timeout,
            "no_offline": args.no_offline,
            "machine": document["machine"],
            "workers": workers,
            "prefixed": args.jobs > 1,
        }
        replayed = len(todo) - len(stale)
        say("replaying %d mutants with %d job(s): %s" % (replayed, len(jobs), TOOLS[ledger.kind]))
        # The scratch files (ledger subsets and raw tool output) never outlive the run.
        with tempfile.TemporaryDirectory(prefix="mutant-replay-") as scratch:
            with concurrent.futures.ThreadPoolExecutor(max_workers=workers) as executor:
                futures = [executor.submit(run_job, config, job, Path(scratch)) for job in jobs]
                for future in concurrent.futures.as_completed(futures):
                    outcome = future.result()
                    outcomes.append(outcome)
                    normal = normalize_outcome(config, outcome, expected, prints)
                    results.update(normal)
                    flush()
                    remember(normal)
    flush()
    if args.native_out and ledger.kind == "compact":
        union = union_native([o.raw for o in outcomes], order)
        if union is None:
            say("warning: nothing was replayed, --native-out is not written", sys.stderr)
        else:
            write_json(args.native_out, union)
    return report_run(args, results, skipped, outcomes)


def restore_failed(outcome, results):
    """The tool said so, or a record of the worker says the file was not restored."""
    return outcome.restore_failed or any(
        results[mutant["id"]]["restored"] is False for mutant in outcome.job.mutants
    )


def report_run(args, results, skipped, outcomes):
    """Print the short summary of a run and return its exit code."""
    count = collections.Counter(record["status"] for record in results.values())
    say(
        "replayed %d: %d killed, %d survived, %d invalid; skipped %d"
        % (len(results), count["killed"], count["survived"], count["invalid"], len(skipped))
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
        say("job %d: no result for %s (%s)" % (outcome.job.index, names, reason), sys.stderr)
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
    """Results, skipped ids and unsupported ids of all files; a later file wins, except that a
    valid record is never replaced by an invalid one."""
    results, skipped, unsupported = {}, {}, []
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
        skipped.update(document.get("skipped") or {})
        unsupported += [n for n in document.get("unsupported") or [] if n not in unsupported]
    return results, skipped, unsupported


def verdict_of(name, results, skipped):
    record = results.get(name)
    if record is not None and record.get("status") in ("killed", "survived"):
        return record["status"]
    if name in skipped:
        return "skipped"
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
    results, skipped, extra_unsupported = combine(documents)
    ids = parse_ids(getattr(args, "ids", ""))
    chosen = select_mutants(ledger, ids)
    rows = []
    for mutant in chosen:
        name = mutant["id"]
        record = results.get(name) or {}
        seconds = record.get("seconds")
        rows.append(
            {
                "id": name,
                "expected": expected[name],
                "verdict": verdict_of(name, results, skipped),
                "tool_status": record.get("tool_status"),
                "seconds": seconds if isinstance(seconds, (int, float)) else None,
                "failed_tests": list(record.get("failed_tests") or []),
                "tail": record.get("tail") or "",
            }
        )
    count = collections.Counter(row["verdict"] for row in rows)
    unexpected = [row for row in rows if row["verdict"] not in (row["expected"], "skipped")]
    unsupported = unsupported_of(ledger, ids)
    unsupported += [n for n in extra_unsupported if n not in unsupported]
    totals = {
        "mutants": len(rows),
        "killed": count["killed"],
        "survived": count["survived"],
        "invalid": count["invalid"],
        "missing": count["missing"],
        "skipped": count["skipped"],
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
    run.add_argument("--jobs", type=int, default=1, help="isolated worktrees to run at once")
    run.add_argument(
        "--work-dir", help="trees, target dirs and record (default <root>/target/mutant-replay)"
    )
    run.add_argument(
        "--only-changed", action="store_true", help="skip mutants whose inputs did not change"
    )
    run.add_argument("--record", help="record of earlier replays (default <work-dir>/record.json)")
    run.add_argument("--timeout", type=int, default=3600, help="seconds per mutant (default 3600)")
    run.add_argument("--machine", help="machine label of the compact tool (default: the host name)")
    run.add_argument("--no-offline", action="store_true", help="forwarded to k71_mutants.py only")
    run.add_argument("--survivors-ok", default="", help="comma-separated ids that may survive")
    run.add_argument(
        "--native-out", help="compact ledgers: one OUT file for the compact tool's merge"
    )

    merge = add("merge", cmd_merge, "merge result files into a verdict summary")
    merge.add_argument("--results", nargs="+", required=True, help="result files or directories")
    merge.add_argument("--ids", default="", help="comma-separated ids the replay was limited to")
    merge.add_argument("--survivors-ok", default="", help="comma-separated ids that may survive")
    merge.add_argument(
        "--wall-seconds", type=float, help="wall time to report (default: from the files)"
    )
    merge.add_argument("--summary", help="write the Markdown summary here")
    merge.add_argument("--json", help="write the machine-readable summary here")
    merge.add_argument(
        "--report-only", action="store_true", help="exit 0 whatever the verdicts are"
    )

    changed = add("changed", cmd_changed, "list the ids --only-changed would replay")
    changed.add_argument("--ids", default="", help="comma-separated ids to look at")
    changed.add_argument("--shard", help="I/N")
    changed.add_argument(
        "--record", help="record of earlier replays (default <work-dir>/record.json)"
    )
    changed.add_argument("--work-dir", help="default <root>/target/mutant-replay")
    changed.add_argument("--survivors-ok", default="", help="comma-separated ids that may survive")
    return parser


def main(argv=None):
    args = build_parser().parse_args(argv)
    return args.handler(args)


if __name__ == "__main__":
    for stream in (sys.stdout, sys.stderr):
        if hasattr(stream, "reconfigure"):
            stream.reconfigure(errors="replace")
    sys.exit(main())
