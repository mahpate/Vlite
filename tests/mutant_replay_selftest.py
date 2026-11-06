#!/usr/bin/env python
"""Self-test of tests/mutant_replay.py (standard library only, no cargo, a few seconds).

The replay tools are called as black boxes, so the test gives them stand-ins: throw-away git
repositories under the temp directory hold a tiny src/, tests/ and Cargo stubs plus stub copies of
tests/k71_mutants.py and tests/compact_mutant_replay.py. A stub follows the CLI and the results
contract of the real tool, applies old -> new, reports KILLED when the mutated text contains
MUTANT_KILLS and SURVIVED otherwise, restores the file, and leaves its argv, cwd and
CARGO_TARGET_DIR in a log so the tests can see where and how it was called. Nothing under the
repository that holds this file is touched; the ledgers on disk are only read.

    python tests/mutant_replay_selftest.py
"""
import contextlib
import hashlib
import importlib.util
import io
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

sys.dont_write_bytecode = True
HERE = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location("mutant_replay", HERE / "mutant_replay.py")
tool = importlib.util.module_from_spec(spec)
spec.loader.exec_module(tool)

K71_STUB = r'''"""Stand-in for tests/k71_mutants.py: the same CLI and results file, no cargo."""
import argparse
import json
import os
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


def note(**fields):
    folder = os.environ.get("STUB_LOG_DIR")
    if folder:
        name = "%d-%d.json" % (os.getpid(), time.time_ns())
        Path(folder, name).write_text(json.dumps(fields), encoding="utf-8")


def rendezvous():
    folder = os.environ.get("STUB_RENDEZVOUS_DIR")
    count = int(os.environ.get("STUB_RENDEZVOUS_COUNT", "0"))
    if not folder or not count:
        return None
    Path(folder, str(os.getpid())).write_text("here", encoding="utf-8")
    deadline = time.time() + 10
    while time.time() < deadline:
        if len(os.listdir(folder)) >= count:
            return True
        time.sleep(0.02)
    return False


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--list", required=True)
    parser.add_argument("--target-dir", required=True)
    parser.add_argument("--out", required=True)
    parser.add_argument("--ids")
    parser.add_argument("--shard")
    parser.add_argument("--timeout", type=int, default=3600)
    parser.add_argument("--no-offline", action="store_true")
    args = parser.parse_args()
    mutants = json.loads(Path(args.list).read_text(encoding="utf-8"))
    assert isinstance(mutants, list), "k71_mutants.py loads --list as a JSON list"
    note(
        tool="k71",
        argv=sys.argv[1:],
        cwd=os.getcwd(),
        root=str(ROOT),
        target=os.environ.get("CARGO_TARGET_DIR"),
        build_jobs=os.environ.get("CARGO_BUILD_JOBS"),
        incremental=os.environ.get("CARGO_INCREMENTAL"),
        ids=[m["id"] for m in mutants],
        keys=sorted(mutants[0]) if mutants else [],
        rendezvous=rendezvous(),
    )
    results = {}
    for mutant in mutants:
        if "STUB_DIE" in mutant["desc"]:
            sys.exit(3)
        path = ROOT / mutant["file"]
        original = path.read_bytes()
        text = original.decode("utf-8")
        plain = text.replace("\r\n", "\n")
        anchors = plain.count(mutant["old"])
        if anchors != 1:
            raise SystemExit(
                "%s: anchor occurs %d times, expected 1" % (mutant["id"], anchors)
            )
        mutated = plain.replace(mutant["old"], mutant["new"])
        try:
            body = mutated.replace("\n", "\r\n") if "\r\n" in text else mutated
            path.write_bytes(body.encode("utf-8"))
            if "STUB_INVALID" in mutant["desc"]:
                status = "INVALID"
            else:
                status = "KILLED" if "MUTANT_KILLS" in mutated else "SURVIVED"
        finally:
            path.write_bytes(original)
        restored = path.read_bytes() == original and "STUB_RESTORE_FAIL" not in mutant["desc"]
        results[mutant["id"]] = {
            "desc": mutant["desc"],
            "status": status,
            "failed_tests": ["stub::fails"] if status == "KILLED" else [],
            "restored": restored,
            "seconds": 2,
            "tail": "" if status == "KILLED" else "stub tail",
        }
        Path(args.out).write_text(json.dumps(results, indent=1), encoding="utf-8")
        print("%s: %s" % (mutant["id"], status), flush=True)
        if not restored:
            print("RESTORE FAILED", file=sys.stderr)
            return 2
    return 0


if __name__ == "__main__":
    sys.exit(main())
'''

COMPACT_STUB = r'''"""Stand-in for tests/compact_mutant_replay.py: `run LIST OUT`."""
import argparse
import hashlib
import json
import os
import sys
import time
from pathlib import Path


def sha(text):
    return hashlib.sha256(text.encode("utf-8")).hexdigest()


def note(**fields):
    folder = os.environ.get("STUB_LOG_DIR")
    if folder:
        name = "%d-%d.json" % (os.getpid(), time.time_ns())
        Path(folder, name).write_text(json.dumps(fields), encoding="utf-8")


def write(path, summary):
    Path(path).write_text(json.dumps(summary, indent=1) + "\n", encoding="utf-8")


def main():
    parser = argparse.ArgumentParser()
    modes = parser.add_subparsers(dest="mode", required=True)
    run = modes.add_parser("run")
    run.add_argument("list")
    run.add_argument("out")
    run.add_argument("--root", default=str(Path(__file__).resolve().parents[1]))
    run.add_argument("--machine", default="local")
    run.add_argument("--ids", default="")
    run.add_argument("--timeout", type=int, default=10800)
    run.add_argument("--resume", action="store_true")
    args = parser.parse_args()
    data = json.loads(Path(args.list).read_text(encoding="utf-8"))
    only = set(filter(None, args.ids.split(",")))
    mutants = [m for m in data["mutants"] if not only or m["id"] in only]
    root = Path(args.root)
    note(
        tool="compact",
        argv=sys.argv[1:],
        cwd=os.getcwd(),
        target=os.environ.get("CARGO_TARGET_DIR"),
        ids=[m["id"] for m in mutants],
        top_keys=sorted(data),
    )
    commands = sorted({m["cmd"] for m in mutants})
    green = {"green": True, "tests": 1, "passed": 1, "seconds": 0}
    summary = {
        "machine": args.machine,
        "environment": {
            "os": "stub",
            "python": "3",
            "rustc": "r",
            "cargo": "c",
            "head": "h",
            "tree_clean": True,
        },
        "sources": {"sha256": "s" * 64, "files": 1},
        "list_base": data.get("base"),
        "baseline_before": {command: dict(green) for command in commands},
        "results": [],
    }
    write(args.out, summary)
    for mutant in mutants:
        desc = mutant.get("desc", "")
        if "STUB_DIE" in desc:
            sys.exit(3)
        path = root / mutant["file"]
        original = path.read_bytes()
        text = original.decode("utf-8")
        plain = text.replace("\r\n", "\n")
        mutated = plain.replace(mutant["old"], mutant["new"], 1)
        try:
            body = mutated.replace("\n", "\r\n") if "\r\n" in text else mutated
            path.write_bytes(body.encode("utf-8"))
        finally:
            path.write_bytes(original)
        restored = path.read_bytes() == original and "STUB_RESTORE_FAIL" not in desc
        if "STUB_COMPILE_ERROR" in desc:
            status = "compile-error"
        else:
            status = "killed" if "MUTANT_KILLS" in mutated else "survived"
        summary["results"].append(
            {
                "id": mutant["id"],
                "file": mutant["file"],
                "anchor_matches": plain.count(mutant["old"]),
                "sha256_lf": {
                    "before": sha(plain),
                    "mutated": sha(mutated),
                    "restored": sha(plain),
                },
                "sha256_bytes": {"before": "b" * 64, "restored": "b" * 64},
                "compiled": status != "compile-error",
                "status": status,
                "failed_tests": ["stub::fails"] if status == "killed" else [],
                "restored_identically": restored,
                "seconds": 1,
                "detail": "stub detail" if status == "compile-error" else "",
            }
        )
        write(args.out, summary)
        print("%s: %s (1 s)" % (mutant["id"], status), flush=True)
        if not restored:
            raise SystemExit("%s: the file was not restored byte-identically" % mutant["id"])
    summary["baseline_after"] = {command: dict(green) for command in commands}
    summary["killed"] = sum(r["status"] == "killed" for r in summary["results"])
    summary["total"] = len(summary["results"])
    summary["tree_clean_after"] = True
    write(args.out, summary)
    return 0 if summary["killed"] == summary["total"] else 1


if __name__ == "__main__":
    sys.exit(main())
'''

SRC_A = "fn a() {\n    let x = 1;\n    let y = 2;\n}\n"
SRC_B = "fn b() {\n    let z = 3;\n}\n"
LIB = "cargo test --locked --offline --lib"
KILLS = " // MUTANT_KILLS"
GIT_IDENTITY = ["-c", "user.name=Selftest", "-c", "user.email=selftest@example.invalid"]
GIT_VARIABLES = ("GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE", "GIT_COMMON_DIR", "GIT_PREFIX")
TEMPLATE_FILES = {
    ".gitignore": "/target/\n*.keep\n",
    "Cargo.toml": '[package]\nname = "demo"\nversion = "0.0.0"\n',
    "Cargo.lock": "# lock\n",
    "src/a.rs": SRC_A,
    "src/b.rs": SRC_B,
    "tests/t1.rs": "fn t1() {}\n",
    "tests/t2.rs": "fn t2() {}\n",
    "tests/common/mod.rs": "pub fn helper() {}\n",
    "tests/k71_mutants.py": K71_STUB,
    "tests/compact_mutant_replay.py": COMPACT_STUB,
}
TEMPLATE = {}


def sha(text):
    return hashlib.sha256(text.encode("utf-8")).hexdigest()


def norm(path):
    return os.path.normcase(os.path.realpath(str(path)))


def put(path, text):
    """Write exact bytes: no newline translation on Windows."""
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(text.encode("utf-8"))


def git(cwd, *args):
    done = subprocess.run(
        ["git"] + list(args),
        cwd=str(cwd),
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        universal_newlines=True,
    )
    if done.returncode != 0:
        raise AssertionError("git %s failed: %s" % (" ".join(args), done.stderr))
    return done.stdout.strip()


def setUpModule():
    if shutil.which("git") is None:
        raise unittest.SkipTest("git is not installed")
    holder = tempfile.TemporaryDirectory(prefix="mr-template-")
    TEMPLATE["holder"] = holder
    TEMPLATE["path"] = Path(holder.name) / "repo"
    for name, text in TEMPLATE_FILES.items():
        put(TEMPLATE["path"] / name, text)
    git(TEMPLATE["path"], "init", "-q")
    git(TEMPLATE["path"], "config", "core.autocrlf", "false")
    git(TEMPLATE["path"], "add", "-A")
    git(
        TEMPLATE["path"],
        *GIT_IDENTITY,
        "-c",
        "commit.gpgsign=false",
        "commit",
        "-q",
        "--no-verify",
        "-m",
        "t",
    )


def tearDownModule():
    TEMPLATE["holder"].cleanup()


def mutant(name, file="src/a.rs", old="let x = 1;", new="let x = 2;", cmd=LIB, desc="", **extra):
    entry = {"id": name, "desc": desc, "file": file, "old": old, "new": new, "cmd": cmd}
    entry.update(extra)
    return entry


def standard_ledger():
    """Two library mutants in src/a.rs and two --test mutants in src/b.rs."""
    return [
        mutant("m1", new="let x = 2;" + KILLS),
        mutant("m2", old="let y = 2;", new="let y = 3;", equivalent=True),
        mutant(
            "m3",
            "src/b.rs",
            "let z = 3;",
            "let z = 4;" + KILLS,
            "cargo test --locked --offline --test t1 -- f",
        ),
        mutant(
            "m4",
            "src/b.rs",
            "let z = 3;",
            "let z = 5;" + KILLS,
            "cargo test --locked --offline --test t2",
        ),
    ]


def compact_ledger(entries):
    return {"campaign": "demo", "base": "0" * 40, "mutants": entries}


@contextlib.contextmanager
def edited(path, transform):
    """Change a file for the duration of the block."""
    original = Path(path).read_bytes()
    put(path, transform(original.decode("utf-8")))
    try:
        yield
    finally:
        Path(path).write_bytes(original)


class ToolTest(unittest.TestCase):
    """A scratch directory, a clean environment and helpers to call the tool."""

    def setUp(self):
        scratch = tempfile.TemporaryDirectory(prefix="mr-test-")
        self.addCleanup(scratch.cleanup)
        self.base = Path(scratch.name)
        patched = mock.patch.dict(os.environ)
        patched.start()
        self.addCleanup(patched.stop)
        for name in list(os.environ):
            if name.startswith("STUB_") or name in GIT_VARIABLES:
                del os.environ[name]
        for name in (
            "GITHUB_OUTPUT", "GITHUB_STEP_SUMMARY", "CARGO_BUILD_JOBS", "CARGO_TARGET_DIR"
        ):
            os.environ.pop(name, None)
        self.logs = self.base / "stub-logs"
        self.logs.mkdir()
        os.environ["STUB_LOG_DIR"] = str(self.logs)

    def invoke(self, *argv):
        out, err = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            code = tool.main([str(item) for item in argv])
        return code, out.getvalue(), err.getvalue()

    def make_repo(self, name="repo"):
        repo = self.base / name
        shutil.copytree(str(TEMPLATE["path"]), str(repo))
        return repo

    def ledger_file(self, data, name="ledger.json"):
        path = self.base / name
        put(path, json.dumps(data, indent=1))
        return path

    def load(self, data):
        return tool.load_ledger(self.ledger_file(data))

    def stub_logs(self):
        return [json.loads(p.read_text(encoding="utf-8")) for p in sorted(self.logs.glob("*.json"))]

    def run_args(self, repo, ledger, *extra, **named):
        work = named.get("work", self.base / "work")
        out = named.get("out", self.base / "out.json")
        return [
            "run", "--root", repo, "--ledger", ledger, "--out", out, "--work-dir", work
        ] + list(extra)


class LedgerTests(ToolTest):
    def test_a_json_list_is_a_list_ledger(self):
        ledger = self.load(standard_ledger())
        self.assertEqual(ledger.kind, "list")
        self.assertIsNone(ledger.top)
        self.assertEqual([m["id"] for m in ledger.mutants], ["m1", "m2", "m3", "m4"])
        self.assertEqual(sorted(ledger.mutants[0]), ["cmd", "desc", "file", "id", "new", "old"])
        self.assertEqual(ledger.unsupported, [])

    def test_a_dict_of_cargo_commands_is_a_list_ledger(self):
        data = {"campaign": "memory", "mutants": [mutant("a"), mutant("b", result="equivalent")]}
        ledger = self.load(data)
        self.assertEqual(ledger.kind, "list")
        self.assertEqual(ledger.top["campaign"], "memory")
        self.assertEqual(ledger.expected_map(), {"a": "killed", "b": "survived"})

    def test_a_dict_of_argument_lists_is_a_compact_ledger(self):
        ledger = self.load(
            compact_ledger(
                [mutant("c1", cmd="test --locked --lib"), mutant("c2", cmd="test --lib")]
            )
        )
        self.assertEqual(ledger.kind, "compact")
        self.assertEqual(ledger.top["base"], "0" * 40)
        self.assertEqual([m["id"] for m in ledger.mutants], ["c1", "c2"])

    def test_entries_with_edits_or_gaps_are_unsupported_but_kept_in_the_ids(self):
        several = {
            "id": "multi",
            "file": "src/a.rs",
            "cmd": "cargo test",
            "edits": [{"file": "src/a.rs", "old": "x", "new": "y"}],
        }
        entries = [
            mutant("ok"),
            several,
            {"id": "nocmd", "file": "src/a.rs", "old": "x", "new": "y"},
            {"id": "nofile", "old": "x", "new": "y", "cmd": "cargo test"},
            dict(mutant("both"), edits=[]),
        ]
        ledger = self.load(entries)
        self.assertEqual([m["id"] for m in ledger.mutants], ["ok"])
        self.assertEqual(ledger.unsupported, ["multi", "nocmd", "nofile", "both"])
        self.assertEqual(ledger.known_ids(), {"ok", "multi", "nocmd", "nofile", "both"})

    def test_other_shapes_are_refused_with_the_supported_ones(self):
        reserved = {"mutants": [{"id": "M01", "description": "x", "test": "t", "result": "killed"}]}
        for name, data in (
            ("a number", 3),
            ("a dict without mutants", {"campaign": "x"}),
            ("entries without old/new", reserved),
            (
                "mixed commands",
                {"mutants": [mutant("a", cmd="cargo test"), mutant("b", cmd="test --lib")]},
            ),
        ):
            with self.subTest(name), self.assertRaises(SystemExit) as caught:
                self.load(data)
            self.assertIn("supported shapes", str(caught.exception))
        for name, data in (
            ("no id", [{"file": "x"}]),
            ("a comma in an id", [mutant("a,b")]),
            ("not an object", [1]),
        ):
            with self.subTest(name), self.assertRaises(SystemExit) as caught:
                self.load(data)
            self.assertIn("id", str(caught.exception))

    def test_an_empty_ledger_is_valid(self):
        self.assertEqual(self.load([]).mutants, [])
        self.assertEqual(self.load({"mutants": []}).kind, "list")
        self.assertEqual(self.load({"base": "x", "mutants": []}).kind, "compact")

    def test_duplicate_ids_are_refused_even_across_unsupported_entries(self):
        for data in (
            [mutant("a"), mutant("b"), mutant("a")], [mutant("a"), {"id": "a", "edits": []}]
        ):
            with self.assertRaises(SystemExit) as caught:
                self.load(data)
            self.assertIn("duplicate", str(caught.exception))

    def test_unreadable_files_are_refused(self):
        with self.assertRaises(SystemExit):
            tool.load_ledger(self.base / "missing.json")
        put(self.base / "bad.json", "{not json")
        with self.assertRaises(SystemExit):
            tool.load_ledger(self.base / "bad.json")

    def test_a_byte_order_mark_is_tolerated(self):
        path = self.base / "bom.json"
        path.write_bytes(b"\xef\xbb\xbf" + json.dumps([mutant("a")]).encode("utf-8"))
        self.assertEqual(len(tool.load_ledger(path).mutants), 1)

    def test_expected_verdicts(self):
        self.assertEqual(tool.expected_of({"id": "a"}), "killed")
        self.assertEqual(tool.expected_of({"equivalent": {"argument": "x"}}), "survived")
        self.assertEqual(tool.expected_of({"result": "equivalent"}), "survived")
        self.assertEqual(tool.expected_of({"expected": "survived"}), "survived")
        for entry in (
            {"equivalent": False},
            {"result": "killed"},
            {"expected": "killed"},
            {"equivalent": None},
        ):
            self.assertEqual(tool.expected_of(entry), "killed")

    def test_survivors_ok_forces_survived_and_checks_the_ids(self):
        ledger = self.load([mutant("a"), mutant("b"), {"id": "u", "edits": []}])
        self.assertEqual(ledger.expected_map(["b"]), {"a": "killed", "b": "survived"})
        ledger.expected_map(["u"])
        with self.assertRaises(SystemExit):
            ledger.expected_map(["nope"])


class PlanTests(ToolTest):
    def plan(self, data, *extra):
        code, out, err = self.invoke(
            "plan", "--root", self.base, "--ledger", self.ledger_file(data), *extra
        )
        self.assertEqual(code, 0, err)
        self.assertEqual(len(out.strip().splitlines()), 1)
        return json.loads(out)

    def many(self, count):
        return [mutant("m%d" % number) for number in range(count)]

    def test_the_shards_never_exceed_the_mutants(self):
        plan = self.plan(self.many(3))
        self.assertEqual(
            plan,
            {"kind": "list", "count": 3, "total_shards": 3, "shards": [0, 1, 2], "unsupported": []},
        )

    def test_the_shards_are_capped_by_the_request_and_by_max_shards(self):
        self.assertEqual(self.plan(self.many(25))["total_shards"], 20)
        self.assertEqual(self.plan(self.many(25), "--shards", "5")["shards"], [0, 1, 2, 3, 4])
        self.assertEqual(self.plan(self.many(25), "--max-shards", "4")["total_shards"], 4)
        self.assertEqual(
            self.plan(self.many(25), "--shards", "30", "--max-shards", "30")["total_shards"], 25
        )
        self.assertEqual(self.plan(self.many(1), "--shards", "0")["total_shards"], 1)

    def test_the_id_filter_narrows_the_count(self):
        plan = self.plan(self.many(6), "--ids", "m1,m4,m5")
        self.assertEqual((plan["count"], plan["total_shards"]), (3, 3))
        with self.assertRaises(SystemExit) as caught:
            self.plan(self.many(3), "--ids", "m1,nope")
        self.assertIn("nope", str(caught.exception))

    def test_no_mutants_means_no_shards(self):
        for data in ([], [{"id": "u", "file": "x", "edits": []}]):
            plan = self.plan(data)
            self.assertEqual((plan["count"], plan["total_shards"], plan["shards"]), (0, 0, []))
        self.assertEqual(self.plan([{"id": "u", "file": "x", "edits": []}])["unsupported"], ["u"])

    def test_unsupported_ids_are_listed(self):
        plan = self.plan(self.many(2) + [{"id": "u", "file": "x", "edits": []}])
        self.assertEqual((plan["count"], plan["unsupported"]), (2, ["u"]))
        self.assertEqual(
            self.plan(self.many(2) + [{"id": "u", "edits": []}], "--ids", "m0")["unsupported"], []
        )

    def test_the_github_output_gets_the_matrix_lines(self):
        output = self.base / "github-output"
        output.write_bytes(b"earlier=1\n")
        os.environ["GITHUB_OUTPUT"] = str(output)
        self.plan(self.many(3))
        expected = b"earlier=1\nkind=list\ncount=3\ntotal_shards=3\nshards=[0,1,2]\n"
        self.assertEqual(output.read_bytes(), expected)
        self.plan(compact_ledger([mutant("c", cmd="test --lib")]))
        self.assertTrue(
            output.read_bytes().endswith(b"kind=compact\ncount=1\ntotal_shards=1\nshards=[0]\n")
        )

    def test_the_ledger_path_is_looked_up_under_the_root(self):
        root = self.make_repo()
        put(root / "tests" / "demo.json", json.dumps(self.many(2)))
        code, out, _ = self.invoke("plan", "--root", root, "--ledger", "tests/demo.json")
        self.assertEqual((code, json.loads(out)["count"]), (0, 2))


class ShardTests(ToolTest):
    def test_a_shard_takes_the_positions_congruent_to_its_index(self):
        mutants = [{"id": "m%d" % number} for number in range(7)]
        self.assertEqual([m["id"] for m in tool.pick_shard(mutants, 1, 3)], ["m1", "m4"])
        every = [m["id"] for index in range(3) for m in tool.pick_shard(mutants, index, 3)]
        self.assertEqual(sorted(every), sorted(m["id"] for m in mutants))

    def test_the_id_filter_comes_before_the_shard(self):
        ledger = self.load([mutant("m%d" % number) for number in range(6)])
        chosen = tool.select_mutants(ledger, ["m1", "m2", "m4", "m5"], (1, 2))
        self.assertEqual([m["id"] for m in chosen], ["m2", "m5"])
        self.assertEqual([m["id"] for m in tool.select_mutants(ledger, [], (2, 3))], ["m2", "m5"])

    def test_bad_shards_are_refused(self):
        for text in ("3/2", "-1/2", "a/b", "1/0", "1", "1/2/3", ""):
            with self.subTest(text), self.assertRaises(SystemExit):
                tool.parse_shard(text)
        self.assertEqual(tool.parse_shard("0/1"), (0, 1))


class FingerprintTests(ToolTest):
    FILES = {
        "Cargo.toml": "a\n",
        "Cargo.lock": "b\n",
        "build.rs": "fn main() {}\n",
        "src/a.rs": "a\n",
        "src/sub/b.rs": "b\n",
        "src/target/skipped.rs": "x\n",
        "tests/t1.rs": "t1\n",
        "tests/t2.rs": "t2\n",
        "tests/dir/main.rs": "main\n",
        "tests/dir/helper.rs": "helper\n",
        "tests/common/mod.rs": "common\n",
        "crates/x/src/lib.rs": "lib\n",
        "crates/x/Cargo.toml": "x\n",
    }

    def tree(self, name="tree"):
        root = self.base / name
        for relative, text in self.FILES.items():
            put(root / relative, text)
        return root

    def test_the_selection_of_a_command(self):
        for command, expected in (
            (
                "cargo test --locked --offline --features tools --test import_lines -- jsonl_line",
                (["import_lines"], False, False),
            ),
            ("cargo test --lib kv::tests", ([], True, False)),
            ("cargo test --bin vlite-mcp kv_mcp::tests", ([], True, False)),
            ("cargo test --test a --features x --test b", (["a", "b"], False, False)),
            ("cargo test --test=a", (["a"], False, False)),
            ("cargo test --lib -- --test foo", ([], True, False)),
            ("cargo test --test t1 --lib", (["t1"], True, False)),
            ("test --all-targets", ([], True, False)),
            ("cargo test", ([], False, False)),
            ("cargo test -p vlite-graph --doc", ([], True, True)),
            ("cargo test --workspace", ([], False, True)),
            ("cargo test --test", ([], False, False)),
        ):
            with self.subTest(command):
                self.assertEqual(tool.parse_selection(command), expected)

    def test_the_selected_test_files(self):
        fingerprints = tool.Fingerprints(self.tree())
        base = ["Cargo.lock", "Cargo.toml"]
        for command, expected in (
            ("cargo test --lib", base + ["build.rs", "src/a.rs", "src/sub/b.rs"]),
            ("cargo test", base + ["build.rs", "src/a.rs", "src/sub/b.rs"]),
            ("test --bin x", base + ["build.rs", "src/a.rs", "src/sub/b.rs"]),
            ("cargo test --test t1 -- f", base + ["tests/common/mod.rs", "tests/t1.rs"]),
            (
                "cargo test --test dir",
                base + ["tests/common/mod.rs", "tests/dir/helper.rs", "tests/dir/main.rs"],
            ),
            ("cargo test --test nope", base + ["tests/common/mod.rs", "tests/nope.rs"]),
            (
                "cargo test --test t2 --test t1",
                base + ["tests/common/mod.rs", "tests/t1.rs", "tests/t2.rs"],
            ),
            (
                "cargo test --test t1 --lib",
                base + [
                    "build.rs", "src/a.rs", "src/sub/b.rs", "tests/common/mod.rs", "tests/t1.rs"
                ],
            ),
            (
                "cargo test -p x",
                base + [
                    "build.rs",
                    "crates/x/Cargo.toml",
                    "crates/x/src/lib.rs",
                    "src/a.rs",
                    "src/sub/b.rs",
                ],
            ),
        ):
            with self.subTest(command):
                self.assertEqual(fingerprints.test_files(command), expected)

    def test_a_folder_and_a_file_of_one_test_target_are_both_inputs(self):
        root = self.tree()
        put(root / "tests" / "dir.rs", "module\n")
        self.assertIn("tests/dir.rs", tool.Fingerprints(root).test_files("cargo test --test dir"))

    def test_only_the_selected_sources_change_the_tests_fingerprint(self):
        root = self.tree()

        def hashes():
            fingerprints = tool.Fingerprints(root)
            return {c: fingerprints.tests_hash(c) for c in ("--lib", "--test t1", "--test t2")}

        before = hashes()
        for relative, changed in (
            ("src/a.rs", {"--lib"}),
            ("tests/t1.rs", {"--test t1"}),
            ("tests/t2.rs", {"--test t2"}),
            ("tests/common/mod.rs", {"--test t1", "--test t2"}),
            ("Cargo.lock", {"--lib", "--test t1", "--test t2"}),
            ("src/target/skipped.rs", set()),
            ("tests/dir/main.rs", set()),
        ):
            with self.subTest(relative), edited(root / relative, lambda text: text + "// edit\n"):
                after = hashes()
                self.assertEqual({c for c in before if before[c] != after[c]}, changed)

    def test_hashes_use_lf_text_and_a_missing_file_hashes_the_word_missing(self):
        lf, crlf = self.base / "lf", self.base / "crlf"
        put(lf / "src" / "a.rs", "one\ntwo\n")
        put(crlf / "src" / "a.rs", "one\r\ntwo\r\n")
        self.assertEqual(tool.Fingerprints(lf).file_hash("src/a.rs"), sha("one\ntwo\n"))
        self.assertEqual(tool.Fingerprints(crlf).file_hash("src/a.rs"), sha("one\ntwo\n"))
        self.assertEqual(tool.Fingerprints(lf).file_hash("src\\a.rs"), sha("one\ntwo\n"))
        self.assertEqual(tool.Fingerprints(lf).file_hash("src/none.rs"), sha("missing"))
        self.assertEqual(
            tool.Fingerprints(lf).tests_hash("--lib"), tool.Fingerprints(crlf).tests_hash("--lib")
        )

    def test_the_mutant_and_command_fingerprints(self):
        fingerprints = tool.Fingerprints(self.tree())
        base = mutant("a", old="x\ny", new="x\nz", cmd="cargo test  --lib")
        same = dict(base, old="x\r\ny", new="x\r\nz", cmd="test --lib", id="other", desc="changed")
        self.assertEqual(fingerprints.of(base), fingerprints.of(same))
        for change in ({"old": "x\nq"}, {"new": "x\nq"}, {"file": "src/b.rs"}):
            self.assertNotEqual(
                fingerprints.of(base)["mutant"], fingerprints.of(dict(base, **change))["mutant"]
            )
        self.assertNotEqual(
            fingerprints.of(base)["cmd"], fingerprints.of(dict(base, cmd="test --lib --x"))["cmd"]
        )
        self.assertEqual(sorted(fingerprints.of(base)), ["cmd", "file", "mutant", "tests"])

    def test_the_mutated_hash_needs_one_anchor(self):
        root = self.base / "mutated"
        put(root / "src" / "a.rs", "a\r\nb\r\na\r\n")
        fingerprints = tool.Fingerprints(root)
        self.assertEqual(fingerprints.mutated_hash(mutant("m", old="b", new="c")), sha("a\nc\na\n"))
        self.assertIsNone(fingerprints.mutated_hash(mutant("m", old="a", new="c")))
        self.assertIsNone(fingerprints.mutated_hash(mutant("m", old="q", new="c")))
        self.assertIsNone(
            fingerprints.mutated_hash(mutant("m", file="src/none.rs", old="a", new="c"))
        )


class NormalizeTests(ToolTest):
    PRINT = {"mutant": "m", "cmd": "c", "tests": "t", "file": "f"}

    def outcome(self, ids, raw):
        job = tool.Job(0, "tree", "target", [{"id": name} for name in ids])
        found = tool.Outcome(job)
        found.raw = raw
        return found

    def normalize(self, kind, ids, raw):
        expected = {name: "killed" for name in ids}
        prints = {name: self.PRINT for name in ids}
        found = self.outcome(ids, raw)
        return tool.normalize_outcome({"kind": kind}, found, expected, prints), found

    def test_k71_results_map_to_the_normalized_record(self):
        raw = {
            "a": {
                "status": "KILLED",
                "failed_tests": list("123456789"),
                "restored": True,
                "seconds": 7,
                "tail": "",
            },
            "b": {
                "status": "SURVIVED",
                "failed_tests": [],
                "restored": True,
                "seconds": 3.6,
                "tail": "x",
            },
            "c": {
                "status": "INVALID",
                "failed_tests": [],
                "restored": True,
                "seconds": 1,
                "tail": "error:  could\nnot compile",
            },
            "d": {"status": "WEIRD", "restored": False},
        }
        records, found = self.normalize("list", ["a", "b", "c", "d", "e"], raw)
        self.assertEqual(
            records["a"],
            {
                "status": "killed",
                "tool_status": "KILLED",
                "failed_tests": ["1", "2", "3", "4", "5"],
                "seconds": 7,
                "restored": True,
                "expected": "killed",
                "fingerprint": self.PRINT,
            },
        )
        self.assertEqual((records["b"]["status"], records["b"]["seconds"]), ("survived", 4))
        self.assertNotIn("tail", records["b"])
        self.assertEqual(
            (records["c"]["status"], records["c"]["tail"]), ("invalid", "error: could not compile")
        )
        self.assertEqual(
            (records["d"]["status"], records["d"]["tool_status"], records["d"]["restored"]),
            ("invalid", "WEIRD", False),
        )
        self.assertEqual(found.missing, ["e"])

    def test_compact_results_map_to_the_normalized_record(self):
        raw = {
            "results": [
                {
                    "id": "a",
                    "status": "killed",
                    "failed_tests": ["t"],
                    "restored_identically": True,
                    "seconds": 3,
                },
                {"id": "b", "status": "survived", "restored_identically": True, "seconds": 4},
                {
                    "id": "c",
                    "status": "compile-error",
                    "restored_identically": True,
                    "detail": "boom",
                    "seconds": 1,
                },
                {"id": "d", "status": "baseline-red"},
                {"id": "other", "status": "killed"},
            ]
        }
        records, found = self.normalize("compact", ["a", "b", "c", "d", "e"], raw)
        self.assertEqual(
            [records[n]["status"] for n in "abcd"], ["killed", "survived", "invalid", "invalid"]
        )
        self.assertEqual(
            [records[n]["tool_status"] for n in "abcd"],
            ["killed", "survived", "compile-error", "baseline-red"],
        )
        self.assertEqual((records["a"]["failed_tests"], records["a"]["restored"]), (["t"], True))
        self.assertEqual((records["c"]["tail"], records["d"]["restored"]), ("boom", None))
        self.assertNotIn("other", records)
        self.assertEqual(found.missing, ["e"])

    def test_a_selected_mutant_without_a_result_is_invalid(self):
        for kind, raw in (
            ("list", None),
            ("list", {}),
            ("compact", None),
            ("compact", {"results": []}),
            ("list", [1]),
        ):
            records, found = self.normalize(kind, ["a"], raw)
            self.assertEqual(
                records["a"],
                {
                    "status": "invalid",
                    "tool_status": "no-result",
                    "failed_tests": [],
                    "seconds": 0,
                    "restored": None,
                    "expected": "killed",
                    "fingerprint": self.PRINT,
                },
            )
            self.assertEqual(found.missing, ["a"])

    def test_the_native_compact_file_is_the_union_of_the_workers(self):
        green, red = {"green": True}, {"green": False}
        first = {
            "machine": "box",
            "environment": {"os": "x"},
            "sources": {"sha256": "s"},
            "list_base": "base",
            "baseline_before": {"c1": green, "c2": green},
            "baseline_after": {"c1": green, "c2": green},
            "results": [{"id": "b", "status": "survived"}, {"id": "a", "status": "killed"}],
            "killed": 1,
            "total": 2,
            "tree_clean_after": True,
        }
        second = {
            "machine": "box",
            "environment": {"os": "y"},
            "sources": {"sha256": "t"},
            "list_base": "base",
            "baseline_before": {"c1": red, "c3": green},
            "baseline_after": {"c1": green, "c3": green},
            "results": [{"id": "c", "status": "killed"}],
            "killed": 1,
            "total": 1,
            "tree_clean_after": False,
        }
        union = tool.union_native([first, second, None], ["a", "b", "c"])
        self.assertEqual([r["id"] for r in union["results"]], ["a", "b", "c"])
        self.assertEqual((union["killed"], union["total"]), (2, 3))
        self.assertEqual(
            (union["machine"], union["environment"], union["sources"], union["list_base"]),
            ("box", {"os": "x"}, {"sha256": "s"}, "base"),
        )
        self.assertEqual(union["baseline_before"], {"c1": red, "c2": green, "c3": green})
        self.assertEqual(sorted(union["baseline_after"]), ["c1", "c2", "c3"])
        self.assertFalse(union["tree_clean_after"])
        unfinished = dict(second)
        del unfinished["baseline_after"]
        self.assertNotIn("baseline_after", tool.union_native([first, unfinished], ["a"]))
        self.assertIsNone(tool.union_native([None], []))


class RunTests(ToolTest):
    def results(self, out=None):
        return json.loads(Path(out or self.base / "out.json").read_text(encoding="utf-8"))

    def test_one_job_replays_in_the_root_with_the_default_kept_target_dir(self):
        repo, ledger = self.make_repo(), self.ledger_file(standard_ledger())
        work, native = repo / "target" / "mutant-replay", self.base / "native.json"
        argv = [
            "run",
            "--root",
            repo,
            "--ledger",
            ledger,
            "--out",
            self.base / "out.json",
            "--native-out",
            native,
        ]
        code, text, err = self.invoke(*argv)
        self.assertEqual(code, 0, text + err)
        document = self.results()
        self.assertEqual(
            (document["tool"], document["kind"], document["shard"], document["jobs"]),
            ("k71_mutants.py", "list", None, 1),
        )
        self.assertEqual(document["head"], git(repo, "rev-parse", "HEAD"))
        self.assertEqual(document["ledger"], str(ledger))
        self.assertTrue(document["machine"])
        for key in ("started_at", "finished_at"):
            self.assertRegex(document[key], r"^\d{4}-\d\d-\d\dT\d\d:\d\d:\d\dZ$")
        self.assertIsInstance(document["wall_seconds"], float)
        self.assertEqual((document["skipped"], document["unsupported"]), ({}, []))
        statuses = {name: record["status"] for name, record in document["results"].items()}
        self.assertEqual(
            statuses, {"m1": "killed", "m2": "survived", "m3": "killed", "m4": "killed"}
        )
        self.assertEqual(list(document["results"]), ["m1", "m2", "m3", "m4"])
        record = document["results"]["m1"]
        self.assertEqual(
            sorted(record),
            [
                "expected",
                "failed_tests",
                "fingerprint",
                "restored",
                "seconds",
                "status",
                "tool_status",
            ],
        )
        self.assertEqual(
            (record["tool_status"], record["failed_tests"], record["restored"]),
            ("KILLED", ["stub::fails"], True),
        )
        self.assertEqual(document["results"]["m2"]["expected"], "survived")
        self.assertEqual(sorted(record["fingerprint"]), ["cmd", "file", "mutant", "tests"])
        (log,) = self.stub_logs()
        self.assertEqual((norm(log["cwd"]), norm(log["root"])), (norm(repo), norm(repo)))
        self.assertEqual(norm(log["target"]), norm(work / "target-0"))
        self.assertEqual(log["ids"], ["m1", "m2", "m3", "m4"])
        self.assertEqual(log["keys"], ["cmd", "desc", "file", "id", "new", "old"])
        self.assertEqual(log["incremental"], "0")
        argv = log["argv"]
        self.assertEqual(argv[argv.index("--timeout") + 1], "3600")
        self.assertEqual(norm(argv[argv.index("--target-dir") + 1]), norm(work / "target-0"))
        self.assertNotIn("--no-offline", argv)
        self.assertFalse(Path(argv[argv.index("--list") + 1]).exists())
        self.assertFalse(Path(argv[argv.index("--out") + 1]).exists())
        self.assertEqual(sorted(p.name for p in work.iterdir()), ["record.json", "target-0"])
        self.assertEqual(git(repo, "status", "--porcelain"), "")
        self.assertNotIn("[job", text)
        self.assertIn("m1: KILLED", text)
        self.assertFalse(native.exists())
        self.assertIn("--native-out is only used for compact ledgers", err)

    def test_three_jobs_use_three_kept_worktrees_at_once(self):
        repo, work = self.make_repo(), self.base / "work"
        ledger = self.ledger_file(standard_ledger()[:3])
        # A user setting that changes the patch format must not break the copy of local edits.
        git(repo, "config", "diff.noprefix", "true")
        with open(str(repo / "src" / "a.rs"), "ab") as handle:
            handle.write(b"// local edit\n")
        (self.base / "rendezvous").mkdir()
        os.environ.update(
            STUB_RENDEZVOUS_DIR=str(self.base / "rendezvous"), STUB_RENDEZVOUS_COUNT="3"
        )
        code, text, err = self.invoke(*self.run_args(repo, ledger, "--jobs", "3"))
        self.assertEqual(code, 0, text + err)
        trees = {norm(work / ("tree-%d" % index)): index for index in range(3)}
        logs = self.stub_logs()
        self.assertEqual(sorted(trees[norm(log["cwd"])] for log in logs), [0, 1, 2])
        for log in logs:
            index = trees[norm(log["cwd"])]
            self.assertEqual(norm(log["target"]), norm(work / ("target-%d" % index)))
            self.assertEqual(norm(log["root"]), norm(log["cwd"]))
            self.assertEqual(log["ids"], ["m%d" % (index + 1)])
            self.assertTrue(log["rendezvous"], "the three jobs did not run at the same time")
        for index, line in enumerate(("m1: KILLED", "m2: SURVIVED", "m3: KILLED")):
            self.assertIn("[job %d] %s" % (index, line), text.splitlines())
        document = self.results()
        self.assertEqual(document["jobs"], 3)
        self.assertEqual(list(document["results"]), ["m1", "m2", "m3"])
        self.assertEqual(
            [r["status"] for r in document["results"].values()], ["killed", "survived", "killed"]
        )
        listed = git(repo, "worktree", "list", "--porcelain")
        rows = [row for row in listed.splitlines() if row.startswith("worktree ")]
        registered = [norm(row[len("worktree ") :]) for row in rows]
        for index in range(3):
            self.assertIn(norm(work / ("tree-%d" % index)), registered)
        for index in range(3):
            self.assertIn(
                "// local edit",
                (work / ("tree-%d" % index) / "src" / "a.rs").read_text(encoding="utf-8"),
            )
        self.assertIn("// local edit", (repo / "src" / "a.rs").read_text(encoding="utf-8"))

        # A second run keeps the trees and the target directories and starts from HEAD again.
        (work / "target-0" / "warm.txt").write_text("cache", encoding="utf-8")
        (work / "tree-0" / "built.keep").write_text("ignored build output", encoding="utf-8")
        (work / "tree-1" / "stray.txt").write_text("untracked", encoding="utf-8")
        for stale in self.logs.glob("*.json"):
            stale.unlink()
        del os.environ["STUB_RENDEZVOUS_COUNT"]
        code, text, err = self.invoke(*self.run_args(repo, ledger, "--jobs", "3"))
        self.assertEqual(code, 0, text + err)
        self.assertEqual(sorted(trees[norm(log["cwd"])] for log in self.stub_logs()), [0, 1, 2])
        self.assertTrue((work / "target-0" / "warm.txt").is_file())
        self.assertTrue((work / "tree-0" / "built.keep").is_file())
        self.assertFalse((work / "tree-1" / "stray.txt").exists())
        self.assertEqual(
            (work / "tree-2" / "src" / "a.rs").read_text(encoding="utf-8").count("// local edit"), 1
        )
        self.assertEqual(listed, git(repo, "worktree", "list", "--porcelain"))

    def test_job_trees_follow_the_root_and_are_repaired(self):
        """Two mutants but four jobs asked, a CRLF checkout, a deleted and an untracked file."""
        repo = self.make_repo()
        ledger, work = self.ledger_file(standard_ledger()[:2]), repo / "target" / "mutant-replay"
        git(repo, "config", "core.autocrlf", "true")
        for relative in ("src/a.rs", "src/b.rs"):
            (repo / relative).unlink()
        git(repo, "checkout", "--", "src")
        self.assertIn(b"\r\n", (repo / "src" / "a.rs").read_bytes())
        with open(str(repo / "src" / "a.rs"), "ab") as handle:
            handle.write(b"// precious\r\n")
        (repo / "tests" / "t2.rs").unlink()
        put(repo / "src" / "new.rs", "fn n() {}\n")
        self.assertEqual(
            git(repo, "diff", "--numstat", "HEAD"), "1\t0\tsrc/a.rs\n0\t1\ttests/t2.rs"
        )
        argv = [
            "run",
            "--root",
            repo,
            "--ledger",
            ledger,
            "--out",
            self.base / "out.json",
            "--jobs",
            "4",
        ]
        code, text, err = self.invoke(*argv)
        self.assertEqual(code, 0, text + err)
        # Never more jobs than mutants, and the CPUs are shared between the jobs that run.
        self.assertEqual(sorted(p.name for p in work.glob("tree-*")), ["tree-0", "tree-1"])
        self.assertEqual(sorted(p.name for p in work.glob("target-*")), ["target-0", "target-1"])
        self.assertEqual(self.results()["jobs"], 4)
        self.assertEqual(
            [r["status"] for r in self.results()["results"].values()], ["killed", "survived"]
        )
        for log in self.stub_logs():
            self.assertEqual(log["build_jobs"], str(max(1, (os.cpu_count() or 1) // 2)))
        # A tree gets the uncommitted changes of tracked files, line endings kept, and nothing else.
        copied = (work / "tree-1" / "src" / "a.rs").read_bytes()
        self.assertEqual(copied.count(b"// precious"), 1)
        self.assertEqual(copied.count(b"\n"), copied.count(b"\r\n"))
        self.assertFalse((work / "tree-0" / "tests" / "t2.rs").exists())
        self.assertFalse((work / "tree-0" / "src" / "new.rs").exists())
        self.assertIn("warning: 1 untracked source file(s) are not copied", err)
        self.assertNotIn("warning", text)
        # A tree without its own .git must not make git fall back to the enclosing root.
        (work / "tree-0" / ".git").unlink()
        code, text, err = self.invoke(*argv)
        self.assertEqual(code, 0, text + err)
        self.assertTrue((work / "tree-0" / ".git").exists())
        self.assertEqual((work / "tree-0" / "src" / "a.rs").read_bytes().count(b"// precious"), 1)
        self.assertEqual((repo / "src" / "a.rs").read_bytes().count(b"// precious"), 1)
        self.assertFalse((repo / "tests" / "t2.rs").exists())
        self.assertTrue((repo / "src" / "new.rs").exists())
        # A tool that the root deleted without committing is missing in the trees too.
        (repo / "tests" / "compact_mutant_replay.py").unlink()
        compact = self.ledger_file(
            compact_ledger([mutant("c1", cmd="test --lib"), mutant("c2", cmd="test --lib")]),
            "compact.json",
        )
        with self.assertRaises(SystemExit) as caught:
            self.invoke(
                "run",
                "--root",
                repo,
                "--ledger",
                compact,
                "--out",
                self.base / "c.json",
                "--jobs",
                "2",
            )
        self.assertIn(
            "this ledger needs tests/compact_mutant_replay.py in the tree under test",
            str(caught.exception),
        )

    def test_ids_shard_and_tool_options(self):
        repo, ledger = self.make_repo(), self.ledger_file(standard_ledger())
        os.environ["CARGO_BUILD_JOBS"] = "7"
        code, text, err = self.invoke(
            *self.run_args(repo, ledger, "--shard", "1/2", "--timeout", "17", "--no-offline")
        )
        self.assertEqual(code, 0, text + err)
        (log,) = self.stub_logs()
        self.assertEqual(log["ids"], ["m2", "m4"])
        self.assertEqual(self.results()["shard"], "1/2")
        argv = log["argv"]
        self.assertEqual(argv[argv.index("--timeout") + 1], "17")
        self.assertIn("--no-offline", argv)
        self.assertEqual(log["build_jobs"], "7")
        for stale in self.logs.glob("*.json"):
            stale.unlink()
        code, text, err = self.invoke(
            *self.run_args(repo, ledger, "--ids", "m1,m2,m3", "--shard", "0/2")
        )
        self.assertEqual(code, 0, text + err)
        self.assertEqual(self.stub_logs()[0]["ids"], ["m1", "m3"])
        with self.assertRaises(SystemExit):
            self.invoke(*self.run_args(repo, ledger, "--ids", "nope"))

    def test_survivors_ok_unsupported_entries_and_invalid_verdicts(self):
        repo = self.make_repo()
        entries = standard_ledger()[:2] + [
            mutant("bad", "src/b.rs", "let z = 3;", "let z = 7;", desc="STUB_INVALID")
        ]
        entries.append({"id": "u", "file": "x", "cmd": "cargo test", "edits": []})
        code, text, err = self.invoke(
            *self.run_args(repo, self.ledger_file(entries), "--survivors-ok", "m1")
        )
        self.assertEqual(code, 0, text + err)
        document = self.results()
        self.assertEqual(
            [r["expected"] for r in document["results"].values()],
            ["survived", "survived", "killed"],
        )
        self.assertIn("differs: m1 expected survived, got killed", text)
        self.assertEqual(document["unsupported"], ["u"])
        self.assertEqual(self.stub_logs()[0]["ids"], ["m1", "m2", "bad"])
        # An invalid verdict is data, not an error of the tool: the exit code stays 0.
        record = document["results"]["bad"]
        self.assertEqual(
            (record["status"], record["tool_status"], record["tail"]),
            ("invalid", "INVALID", "stub tail"),
        )
        self.assertIn("differs: bad expected killed, got invalid", text)

    def test_a_tool_that_dies_leaves_no_result_records_and_exit_one(self):
        repo = self.make_repo()
        ledger = self.ledger_file(
            [standard_ledger()[0], mutant("dies", desc="STUB_DIE"), standard_ledger()[2]]
        )
        code, text, err = self.invoke(*self.run_args(repo, ledger))
        self.assertEqual(code, 1, text + err)
        results = self.results()["results"]
        self.assertEqual(results["m1"]["status"], "killed")
        for name in ("dies", "m3"):
            self.assertEqual(
                (results[name]["status"], results[name]["tool_status"]), ("invalid", "no-result")
            )
        self.assertIn("no result for dies, m3", err)

    def test_a_failed_restore_exits_two_and_says_so(self):
        repo = self.make_repo()
        ledger = self.ledger_file(
            [standard_ledger()[0], mutant("loose", desc="STUB_RESTORE_FAIL"), standard_ledger()[2]]
        )
        code, text, err = self.invoke(*self.run_args(repo, ledger))
        self.assertEqual(code, 2, text + err)
        self.assertIn("RESTORE FAILED", err)
        results = self.results()["results"]
        self.assertEqual(results["loose"]["restored"], False)
        self.assertEqual(results["m3"]["tool_status"], "no-result")

    def test_usage_errors_exit_with_one_not_two(self):
        repo, ledger = self.make_repo(), self.ledger_file(standard_ledger()[:1])
        for argv in (
            ["run", "--root", repo, "--ledger", ledger],
            ["run", "--root", repo, "--ledger", ledger, "--out", "o", "--jobs", "0"],
            ["run", "--root", repo, "--ledger", ledger, "--out", "o", "--shard", "5/2"],
            ["run", "--root", self.base / "none", "--ledger", ledger, "--out", "o"],
            ["frobnicate"],
        ):
            with self.subTest(argv), self.assertRaises(SystemExit) as caught:
                self.invoke(*argv)
            self.assertNotEqual(caught.exception.code, 2)

    def test_the_help_of_every_command_formats(self):
        for argv in (
            ["--help"],
            ["plan", "--help"],
            ["run", "--help"],
            ["merge", "--help"],
            ["changed", "--help"],
        ):
            with self.subTest(argv), self.assertRaises(SystemExit) as caught:
                self.invoke(*argv)
            self.assertEqual(caught.exception.code, 0)

    def test_the_k71_tool_must_exist_in_the_tree(self):
        repo, ledger = self.make_repo(), self.ledger_file(standard_ledger()[:1])
        (repo / "tests" / "k71_mutants.py").unlink()
        with self.assertRaises(SystemExit) as caught:
            self.invoke(*self.run_args(repo, ledger))
        self.assertIn("k71_mutants.py is missing", str(caught.exception))

    def test_a_compact_ledger_is_replayed_with_the_compact_tool(self):
        repo, work, native = self.make_repo(), self.base / "work", self.base / "native.json"
        entries = [
            mutant(
                "c1", new="let x = 2;" + KILLS, cmd="test --locked --lib", expect=["stub::fails"]
            ),
            mutant(
                "c2",
                old="let y = 2;",
                new="let y = 3;",
                cmd="test --locked --lib",
                equivalent={"argument": "x"},
            ),
        ]
        ledger = self.ledger_file(compact_ledger(entries))
        code, text, err = self.invoke(
            *self.run_args(
                repo,
                ledger,
                "--machine",
                "box-a",
                "--timeout",
                "99",
                "--native-out",
                native,
                "--no-offline",
            )
        )
        self.assertEqual(code, 0, text + err)
        document = self.results()
        self.assertEqual(
            (document["tool"], document["kind"], document["machine"]),
            ("compact_mutant_replay.py", "compact", "box-a"),
        )
        self.assertEqual(
            [r["status"] for r in document["results"].values()], ["killed", "survived"]
        )
        self.assertEqual(
            [r["expected"] for r in document["results"].values()], ["killed", "survived"]
        )
        self.assertEqual(document["results"]["c1"]["tool_status"], "killed")
        (log,) = self.stub_logs()
        argv = log["argv"]
        self.assertEqual(argv[0], "run")
        self.assertEqual(argv[argv.index("--machine") + 1], "box-a")
        self.assertEqual(argv[argv.index("--ids") + 1], "c1,c2")
        self.assertEqual(norm(argv[argv.index("--root") + 1]), norm(repo))
        self.assertEqual(argv[argv.index("--timeout") + 1], "99")
        self.assertNotIn("--no-offline", argv)
        self.assertEqual(log["top_keys"], ["base", "campaign", "mutants"])
        self.assertEqual(
            (norm(log["cwd"]), norm(log["target"])), (norm(repo), norm(work / "target-0"))
        )
        union = json.loads(native.read_text(encoding="utf-8"))
        self.assertEqual(
            (union["machine"], union["killed"], union["total"], union["list_base"]),
            ("box-a", 1, 2, "0" * 40),
        )
        self.assertEqual([r["id"] for r in union["results"]], ["c1", "c2"])
        self.assertEqual(sorted(union["baseline_after"]), ["test --locked --lib"])

    def test_compact_jobs_write_one_native_file(self):
        repo, native = self.make_repo(), self.base / "native.json"
        entries = [mutant("c%d" % n, new="let x = 2;" + KILLS, cmd="test --lib") for n in range(3)]
        entries[1]["cmd"] = "test --lib --features x"
        ledger = self.ledger_file(compact_ledger(entries))
        code, text, err = self.invoke(
            *self.run_args(repo, ledger, "--jobs", "2", "--machine", "box", "--native-out", native)
        )
        self.assertEqual(code, 0, text + err)
        union = json.loads(native.read_text(encoding="utf-8"))
        self.assertEqual([r["id"] for r in union["results"]], ["c0", "c1", "c2"])
        self.assertEqual((union["killed"], union["total"]), (3, 3))
        self.assertEqual(
            sorted(union["baseline_before"]), ["test --lib", "test --lib --features x"]
        )
        self.assertEqual(sorted(log["ids"] for log in self.stub_logs()), [["c0", "c2"], ["c1"]])
        self.assertIn("[job 0] c0: killed (1 s)", text)

    def test_a_compact_ledger_without_its_tool_names_the_ref_requirement(self):
        repo = self.make_repo()
        (repo / "tests" / "compact_mutant_replay.py").unlink()
        ledger = self.ledger_file(compact_ledger([mutant("c", cmd="test --lib")]))
        with self.assertRaises(SystemExit) as caught:
            self.invoke(*self.run_args(repo, ledger))
        self.assertIn(
            "this ledger needs tests/compact_mutant_replay.py in the tree under test",
            str(caught.exception),
        )
        self.assertFalse((self.base / "out.json").exists())

    def test_a_compact_tool_that_wrote_no_file_leaves_no_result_records(self):
        repo = self.make_repo()
        ledger = self.ledger_file(compact_ledger([mutant("c", desc="STUB_DIE", cmd="test --lib")]))
        native = self.base / "native.json"
        code, text, err = self.invoke(*self.run_args(repo, ledger, "--native-out", native))
        self.assertEqual(code, 1, text + err)
        self.assertEqual(self.results()["results"]["c"]["tool_status"], "no-result")

    def test_a_compact_failed_restore_exits_two(self):
        repo = self.make_repo()
        ledger = self.ledger_file(
            compact_ledger([mutant("c", desc="STUB_RESTORE_FAIL", cmd="test --lib")])
        )
        code, text, err = self.invoke(*self.run_args(repo, ledger))
        self.assertEqual(code, 2, text + err)
        self.assertIn("RESTORE FAILED", err)


class OnlyChangedTests(ToolTest):
    def setUp(self):
        super().setUp()
        self.repo = self.make_repo()
        self.work = self.base / "work"
        self.ledger = self.ledger_file(standard_ledger())

    def run_all(self, *extra, **named):
        ledger = named.get("ledger", self.ledger)
        code, text, err = self.invoke(*self.run_args(self.repo, ledger, *extra))
        self.assertEqual(code, 0, text + err)
        return text

    def changed(self, *extra, **named):
        ledger = named.get("ledger", self.ledger)
        code, out, err = self.invoke(
            "changed", "--root", self.repo, "--ledger", ledger, "--work-dir", self.work, *extra
        )
        self.assertEqual(code, 0, err)
        self.last_err = err
        lines = out.splitlines()
        ids = [line for line in lines if not line.startswith("#")]
        counts = [line for line in lines if re.match(r"# \d+ of \d+$", line)]
        skips = dict(
            line[len("# skip "):].split(": ", 1) for line in lines if line.startswith("# skip ")
        )
        self.assertEqual(len(counts), 1, out)
        return ids, counts[0], skips

    def test_a_run_records_what_it_replayed_and_nothing_else_needs_a_replay(self):
        self.assertEqual(self.changed()[:2], (["m1", "m2", "m3", "m4"], "# 4 of 4"))
        self.assertEqual(self.changed("--shard", "0/2")[:2], (["m1", "m3"], "# 2 of 2"))
        self.run_all()
        store = json.loads((self.work / "record.json").read_text(encoding="utf-8"))
        self.assertEqual(list(store), [str(self.ledger)])
        entry = store[str(self.ledger)]["m1"]
        self.assertEqual(sorted(entry), ["at", "fingerprint", "head", "status"])
        self.assertEqual(
            (entry["status"], entry["head"]), ("killed", git(self.repo, "rev-parse", "HEAD"))
        )
        ids, count, skips = self.changed()
        head = git(self.repo, "rev-parse", "HEAD")[:12]
        self.assertEqual((ids, count), ([], "# 0 of 4"))
        self.assertEqual(
            skips, {name: "unchanged since " + head for name in ("m1", "m2", "m3", "m4")}
        )
        for stale in self.logs.glob("*.json"):
            stale.unlink()
        text = self.run_all("--only-changed")
        self.assertEqual(self.stub_logs(), [])
        document = json.loads((self.base / "out.json").read_text(encoding="utf-8"))
        self.assertEqual(
            (document["results"], sorted(document["skipped"])), ({}, ["m1", "m2", "m3", "m4"])
        )
        self.assertIn("only changed: 0 of 4", text)
        # An edit of one selected test file makes exactly that mutant run again.
        with edited(self.repo / "tests" / "t1.rs", lambda text: text + "// edit\n"):
            code, out, err = self.invoke(
                "changed", "--root", self.repo, "--ledger", self.ledger, "--work-dir", self.work
            )
            skipped = [
                "# skip %s: unchanged since %s" % (name, head) for name in ("m1", "m2", "m4")
            ]
            self.assertEqual((code, out.splitlines()), (0, ["m3", "# 1 of 4"] + skipped))
            text = self.run_all("--only-changed")
            (log,) = self.stub_logs()
            self.assertEqual(log["ids"], ["m3"])
            document = json.loads((self.base / "out.json").read_text(encoding="utf-8"))
            self.assertEqual(list(document["results"]), ["m3"])
            self.assertEqual(sorted(document["skipped"]), ["m1", "m2", "m4"])
            self.assertIn("only changed: 1 of 4", text)
            self.assertEqual(self.changed()[:2], ([], "# 0 of 4"))

    def test_changes_that_matter_to_a_recorded_verdict(self):
        self.run_all()
        root = self.repo
        cases = (
            ("a mutated file", root / "src" / "a.rs", ["m1", "m2"]),
            ("a selected test file", root / "tests" / "t1.rs", ["m3"]),
            ("the other test file", root / "tests" / "t2.rs", ["m4"]),
            ("the shared test helpers", root / "tests" / "common" / "mod.rs", ["m3", "m4"]),
            ("the lock file", root / "Cargo.lock", ["m1", "m2", "m3", "m4"]),
            ("a file nobody selects", root / ".gitignore", []),
        )
        for name, path, expected in cases:
            with self.subTest(name), edited(path, lambda text: text + "// edit\n"):
                ids, count, skips = self.changed()
                self.assertEqual(ids, expected)
                self.assertEqual(count, "# %d of 4" % len(expected))
                self.assertEqual(
                    sorted(skips), [m for m in ("m1", "m2", "m3", "m4") if m not in expected]
                )

        def ledger_with(index, **changes):
            data = standard_ledger()
            data[index].update(changes)
            return self.ledger_file(data)

        self.assertEqual(self.changed(ledger=ledger_with(3, desc="only a description"))[0], [])
        self.assertEqual(
            self.changed(
                ledger=ledger_with(3, cmd="cargo   test --locked --offline  --test t2")
            )[0],
            [],
        )
        # Another filter after `--` is another baseline command: the verdict may differ.
        self.assertEqual(
            self.changed(
                ledger=ledger_with(3, cmd="cargo test --locked --offline --test t2 -- other")
            )[0],
            ["m4"],
        )
        self.assertEqual(
            self.changed(
                ledger=ledger_with(3, cmd="cargo test --locked --offline --features x --test t2")
            )[0],
            ["m4"],
        )
        self.assertEqual(self.changed(ledger=ledger_with(1, new="let y = 4;"))[0], ["m2"])
        self.assertEqual(self.changed(ledger=ledger_with(0, old="let x = 1; "))[0], ["m1"])
        added = standard_ledger() + [
            mutant("m5", "src/b.rs", "let z = 3;", "let z = 6;", "cargo test --lib")
        ]
        ids, count, _ = self.changed(ledger=self.ledger_file(added))
        self.assertEqual((ids, count), (["m5"], "# 1 of 5"))
        self.ledger_file(standard_ledger())
        # Another expectation is another verdict to reach: the recorded one no longer counts.
        ids, _, skips = self.changed("--survivors-ok", "m1")
        self.assertEqual((ids, "m1" in skips), (["m1"], False))
        # The shard and the id filter select before the change test does.
        with edited(root / "tests" / "common" / "mod.rs", lambda text: text + "// edit\n"):
            self.assertEqual(self.changed("--shard", "0/2")[:2], (["m3"], "# 1 of 2"))
            self.assertEqual(self.changed("--shard", "1/2")[:2], (["m4"], "# 1 of 2"))
            self.assertEqual(self.changed("--ids", "m1,m4")[:2], (["m4"], "# 1 of 2"))

    def test_the_record_keeps_only_clean_verdicts(self):
        data = [
            standard_ledger()[0],
            mutant("bad", desc="STUB_INVALID", old="let y = 2;", new="let y = 3;"),
            mutant("loose", desc="STUB_RESTORE_FAIL", old="let y = 2;", new="let y = 4;"),
            standard_ledger()[2],
        ]
        ledger = self.ledger_file(data)
        code, text, err = self.invoke(*self.run_args(self.repo, ledger))
        self.assertEqual(code, 2, text + err)
        store = json.loads((self.work / "record.json").read_text(encoding="utf-8"))
        self.assertEqual(sorted(store[str(ledger)]), ["m1"])

    def test_a_record_can_live_elsewhere_and_a_corrupt_one_is_ignored(self):
        elsewhere = self.base / "records" / "mine.json"
        put(elsewhere, "{broken")
        ids, count, _ = self.changed("--record", elsewhere)
        self.assertEqual((ids, count), (["m1", "m2", "m3", "m4"], "# 4 of 4"))
        self.assertIn("warning: ignoring the unreadable record file mine.json", self.last_err)
        # A record that is not an object is replaced, not trusted.
        put(elsewhere, json.dumps({str(self.ledger): ["not", "an", "object"]}))
        self.assertEqual(self.changed("--record", elsewhere)[1], "# 4 of 4")
        self.run_all("--record", elsewhere)
        self.assertFalse((self.work / "record.json").exists())
        self.assertEqual(self.changed("--record", elsewhere)[:2], ([], "# 0 of 4"))
        # The record is kept per ledger path, as given.
        other = self.ledger_file(standard_ledger(), "other.json")
        self.assertEqual(
            self.changed("--record", elsewhere, ledger=other)[:2],
            (["m1", "m2", "m3", "m4"], "# 4 of 4"),
        )


class CarriedRunTests(ToolTest):
    """Compact ledgers carry runs; an unchanged run is accepted, a stale one is replayed."""

    def setUp(self):
        super().setUp()
        self.repo = self.make_repo()
        self.work = self.base / "work"

    def entry(self, status="killed", **extra):
        text = SRC_A
        new = "let x = 2;" + KILLS
        run = {
            "status": status,
            "anchor_matches": 1,
            "compiled": True,
            "restored_identically": True,
            "baseline_green": True,
            "sha256_lf": {
                "before": sha(text),
                "mutated": sha(text.replace("let x = 1;", new)),
                "restored": sha(text),
            },
        }
        return mutant("c1", new=new, cmd="test --lib", runs={"box": run}, **extra)

    def changed(self, data):
        ledger = self.ledger_file(data)
        code, out, err = self.invoke(
            "changed", "--root", self.repo, "--ledger", ledger, "--work-dir", self.work
        )
        self.assertEqual(code, 0, err)
        return out.splitlines()

    def test_a_run_that_describes_the_current_file_is_accepted(self):
        lines = self.changed(compact_ledger([self.entry()]))
        self.assertEqual(lines, ["# 0 of 1", "# skip c1: accepted by the ledger's recorded run"])

    def test_an_equivalent_mutant_is_accepted_on_a_surviving_run(self):
        entry = dict(self.entry("survived"), equivalent={"argument": "x"})
        self.assertEqual(self.changed(compact_ledger([entry]))[0], "# 0 of 1")
        self.assertEqual(self.changed(compact_ledger([self.entry("survived")]))[0], "c1")

    def test_any_missing_piece_of_evidence_replays_the_mutant(self):
        broken = {
            "wrong status": lambda run: run.update(status="survived"),
            "not compiled": lambda run: run.update(compiled=False),
            "two anchors": lambda run: run.update(anchor_matches=2),
            "not restored": lambda run: run.update(restored_identically=False),
            "baseline not green": lambda run: run.update(baseline_green=False),
            "stale file": lambda run: run["sha256_lf"].update(before="0" * 64),
            "other mutated text": lambda run: run["sha256_lf"].update(mutated="0" * 64),
            "no hashes": lambda run: run.pop("sha256_lf"),
        }
        for name, spoil in broken.items():
            entry = self.entry()
            spoil(entry["runs"]["box"])
            with self.subTest(name):
                self.assertEqual(self.changed(compact_ledger([entry]))[0], "c1")

    def test_an_edit_of_the_file_makes_the_recorded_run_stale(self):
        data = compact_ledger([self.entry()])
        with edited(self.repo / "src" / "a.rs", lambda text: "// header\n" + text):
            self.assertEqual(self.changed(data)[0], "c1")
        crlf = SRC_A.replace("\n", "\r\n")
        with edited(self.repo / "src" / "a.rs", lambda text: crlf):
            self.assertEqual(self.changed(data)[0], "# 0 of 1")

    def test_an_anchor_that_is_not_unique_anymore_replays_the_mutant(self):
        data = compact_ledger([self.entry()])
        with edited(self.repo / "src" / "a.rs", lambda text: text + "let x = 1;\n"):
            self.assertEqual(self.changed(data)[0], "c1")

    def test_a_list_ledger_ignores_carried_runs(self):
        data = [self.entry()]
        self.assertEqual(self.changed(data)[0], "c1")

    def test_a_record_of_this_machine_wins_over_the_carried_run(self):
        data = compact_ledger([self.entry()])
        ledger = self.ledger_file(data)
        code, text, err = self.invoke(*self.run_args(self.repo, ledger, "--only-changed"))
        self.assertEqual(code, 0, text + err)
        self.assertEqual(self.stub_logs(), [])
        code, text, err = self.invoke(*self.run_args(self.repo, ledger))
        self.assertEqual(code, 0, text + err)
        self.assertEqual(len(self.stub_logs()), 1)
        lines = self.changed(data)
        self.assertEqual(lines[0], "# 0 of 1")
        self.assertIn("unchanged since", lines[1])


class MergeTests(ToolTest):
    HEAD = "0123456789abcdef0123456789abcdef01234567"

    def record(self, status, expected="killed", seconds=10, **extra):
        record = {
            "status": status,
            "tool_status": status.upper(),
            "failed_tests": ["t::first", "t::second"] if status == "killed" else [],
            "seconds": seconds,
            "restored": True,
            "expected": expected,
            "fingerprint": {"mutant": "a", "cmd": "b", "tests": "c", "file": "d"},
        }
        record.update(extra)
        return record

    def document(self, name, results, **extra):
        document = {
            "tool": "k71_mutants.py",
            "kind": "list",
            "ledger": "ledger.json",
            "shard": None,
            "jobs": 1,
            "machine": "box",
            "head": self.HEAD,
            "started_at": "2026-01-01T00:00:00Z",
            "finished_at": "2026-01-01T00:10:00Z",
            "wall_seconds": 600.0,
            "results": results,
            "skipped": {},
            "unsupported": [],
        }
        document.update(extra)
        path = self.base / "results" / name
        put(path, json.dumps(document))
        return path

    def merge(self, ledger, *paths, **named):
        extra = list(named.get("extra", ()))
        summary, report = self.base / "summary.md", self.base / "summary.json"
        argv = ["merge", "--root", self.base, "--ledger", self.ledger_file(ledger), "--results"]
        argv += [str(p) for p in paths] + ["--summary", summary, "--json", report] + extra
        code, out, err = self.invoke(*argv)
        self.last_err = err
        data = json.loads(report.read_text(encoding="utf-8")) if report.exists() else None
        return code, out, summary.read_text(encoding="utf-8"), data

    def good(self):
        return {
            "m1": self.record("killed"),
            "m2": self.record("survived", "survived"),
            "m3": self.record("killed"),
            "m4": self.record("killed"),
        }

    def test_a_replay_that_matches_the_ledger_passes(self):
        code, out, markdown, data = self.merge(
            standard_ledger(), self.document("a.json", self.good())
        )
        self.assertEqual(code, 0, out)
        self.assertEqual(out.rstrip("\n"), markdown.rstrip("\n"))
        self.assertTrue(data["ok"])
        self.assertEqual(
            data["totals"],
            {
                "mutants": 4,
                "killed": 3,
                "survived": 1,
                "invalid": 0,
                "missing": 0,
                "skipped": 0,
                "unsupported": 0,
                "unexpected": 0,
            },
        )
        self.assertIn("# Mutant replay: ", markdown.splitlines()[0])
        self.assertTrue(markdown.splitlines()[0].endswith(" @ " + self.HEAD[:12]))
        self.assertIn(
            "| mutants | killed | survived | invalid | missing | skipped | unsupported |"
            " unexpected |",
            markdown,
        )
        self.assertIn("| 4 | 3 | 1 | 0 | 0 | 0 | 0 | 0 |", markdown)
        self.assertIn("## Differences from the ledger", markdown)
        self.assertIn("None: every replayable mutant has the expected verdict.", markdown)
        self.assertIn("<details><summary>All mutants (4)</summary>", markdown)
        self.assertIn("| m1 | killed | killed | 10 | t::first |", markdown)
        self.assertIn("| m2 | survived | survived | 10 | - |", markdown)
        self.assertTrue(markdown.rstrip().endswith("</details>"))
        self.assertNotIn("\r", markdown)

    def test_unexpected_verdicts_fail_and_are_listed_with_their_details(self):
        results = self.good()
        results["m1"] = self.record("survived", tool_status="SURVIVED", seconds=33)
        results["m2"] = self.record("killed", "survived")
        results["m3"] = self.record(
            "invalid", tool_status="INVALID", tail="error: could not compile `demo`"
        )
        code, out, markdown, data = self.merge(standard_ledger(), self.document("a.json", results))
        self.assertEqual(code, 1, out)
        self.assertEqual([row["id"] for row in data["unexpected"]], ["m1", "m2", "m3"])
        self.assertEqual(data["totals"]["unexpected"], 3)
        differences = markdown.split("## Differences from the ledger")[1].split("<details>")[0]
        self.assertIn("| m1 | killed | survived | SURVIVED | - | 33 |", differences)
        self.assertIn("| m2 | survived | killed | KILLED | t::first | 10 |", differences)
        self.assertIn(
            "| m3 | killed | invalid | INVALID | ...error: could not compile `demo` | 10 |",
            differences,
        )
        self.assertNotIn("| m4 |", differences)
        code, _, _, _ = self.merge(
            standard_ledger(), self.document("a.json", results), extra=["--report-only"]
        )
        self.assertEqual(code, 0)

    def test_a_mutant_without_a_result_is_missing_and_fails(self):
        results = self.good()
        del results["m4"]
        code, out, markdown, data = self.merge(standard_ledger(), self.document("a.json", results))
        self.assertEqual(code, 1, out)
        self.assertEqual((data["totals"]["missing"], data["totals"]["unexpected"]), (1, 1))
        self.assertIn("| m4 | killed | missing | - | - | - |", markdown)
        code, *_ = self.merge(standard_ledger(), self.base / "results")
        self.assertEqual(code, 1)

    def test_a_skipped_mutant_counts_as_matching(self):
        results = self.good()
        del results["m3"]
        document = self.document("a.json", results, skipped={"m3": "unchanged since 0123456789ab"})
        code, out, markdown, data = self.merge(standard_ledger(), document)
        self.assertEqual(code, 0, out)
        self.assertEqual((data["totals"]["skipped"], data["totals"]["unexpected"]), (1, 0))
        self.assertIn("| m3 | killed | skipped | - | - |", markdown)

    def test_expected_survivors(self):
        ledger = [
            mutant("a", equivalent={"argument": "x"}),
            mutant("b", result="equivalent"),
            mutant("c", expected="survived"),
            mutant("d"),
            mutant("e"),
        ]
        results = {name: self.record("survived", "survived") for name in "abcd"}
        results["e"] = self.record("killed")
        document = self.document("a.json", results)
        code, out, _, data = self.merge(ledger, document)
        self.assertEqual((code, [row["id"] for row in data["unexpected"]]), (1, ["d"]))
        code, out, _, data = self.merge(ledger, document, extra=["--survivors-ok", "d"])
        self.assertEqual((code, data["unexpected"]), (0, []))
        with self.assertRaises(SystemExit):
            self.merge(ledger, document, extra=["--survivors-ok", "nope"])

    def test_unsupported_entries_are_reported_but_do_not_fail(self):
        ledger = standard_ledger() + [{"id": "u1", "file": "x", "cmd": "cargo test", "edits": []}]
        document = self.document("a.json", self.good(), unsupported=["u1", "stale"])
        code, out, markdown, data = self.merge(ledger, document)
        self.assertEqual(code, 0, out)
        self.assertEqual(data["unsupported"], ["u1", "stale"])
        self.assertEqual(data["totals"]["unsupported"], 2)
        self.assertEqual(data["totals"]["mutants"], 4)
        self.assertIn("## Unsupported entries (not replayed)", markdown)
        self.assertIn("u1, stale", markdown)

    def test_a_later_file_wins_but_a_valid_record_beats_an_invalid_one(self):
        first = self.document(
            "1.json", {"m1": self.record("killed"), "m2": self.record("survived", "survived")}
        )
        second = self.document(
            "2.json",
            {
                "m1": self.record("invalid", tool_status="no-result"),
                "m2": self.record("killed", "survived"),
            },
        )
        third = self.document("3.json", {"m3": self.record("killed"), "m4": self.record("killed")})
        code, out, _, data = self.merge(standard_ledger(), first, second, third)
        verdicts = {row["id"]: row["verdict"] for row in data["mutants"]}
        self.assertEqual(verdicts, {"m1": "killed", "m2": "killed", "m3": "killed", "m4": "killed"})
        code, out, _, data = self.merge(standard_ledger(), second, first, third)
        verdicts = {row["id"]: row["verdict"] for row in data["mutants"]}
        self.assertEqual((verdicts["m1"], verdicts["m2"]), ("killed", "survived"))
        invalid_later = self.document(
            "4.json", {"m1": self.record("invalid", tool_status="no-result")}
        )
        _, _, _, data = self.merge(
            standard_ledger(),
            invalid_later,
            self.document("5.json", {"m1": self.record("invalid", tool_status="x")}),
        )
        self.assertEqual(data["mutants"][0]["tool_status"], "x")

    def test_directories_are_searched_recursively_and_other_json_is_skipped(self):
        self.document(
            "a/1.json", {"m1": self.record("killed"), "m2": self.record("survived", "survived")}
        )
        self.document("b/c/2.json", {"m3": self.record("killed"), "m4": self.record("killed")})
        put(self.base / "results" / "notes.json", json.dumps({"hello": 1}))
        put(self.base / "results" / "broken.json", "{")
        code, out, _, data = self.merge(standard_ledger(), self.base / "results")
        self.assertEqual(code, 0, out)
        self.assertEqual(data["totals"]["killed"], 3)
        self.assertEqual(len(data["files"]), 2)
        self.assertIn("warning: skipping notes.json", self.last_err)
        self.assertIn("warning: skipping broken.json", self.last_err)
        self.assertNotIn("warning", out)
        for bad in (self.base / "results" / "notes.json", self.base / "nowhere"):
            with self.assertRaises(SystemExit):
                self.merge(standard_ledger(), bad)

    def test_wall_time_prefers_the_option_then_the_span_then_the_sum(self):
        first = self.document(
            "1.json",
            self.good(),
            started_at="2026-01-01T00:00:00Z",
            finished_at="2026-01-01T00:05:00Z",
        )
        second = self.document(
            "2.json", {}, started_at="2026-01-01T00:02:00Z", finished_at="2026-01-01T00:10:00Z"
        )
        _, _, markdown, data = self.merge(standard_ledger(), first, second)
        self.assertEqual(
            (data["wall_seconds"], data["wall_source"]), (600.0, "finished_at - started_at")
        )
        self.assertIn("Wall time: 10.0 min (finished_at - started_at)", markdown)
        self.assertAlmostEqual(data["minutes_per_mutant"]["wall"], 2.5)
        self.assertAlmostEqual(data["minutes_per_mutant"]["compute"], 40 / 4 / 60.0)
        self.assertIn("Minutes per mutant: 2.500 wall, 0.167 compute (4 replayable)", markdown)
        _, _, markdown, data = self.merge(
            standard_ledger(), first, second, extra=["--wall-seconds", "120"]
        )
        self.assertEqual((data["wall_seconds"], data["wall_source"]), (120.0, "--wall-seconds"))
        self.assertIn("Wall time: 2.0 min (--wall-seconds)", markdown)
        plain1 = self.document(
            "3.json", self.good(), started_at=None, finished_at=None, wall_seconds=60
        )
        plain2 = self.document("4.json", {}, started_at=None, finished_at=None, wall_seconds=30.0)
        _, _, markdown, data = self.merge(standard_ledger(), plain1, plain2)
        self.assertEqual((data["wall_seconds"], data["wall_source"]), (90, "sum of wall_seconds"))
        self.assertIn("Wall time: 1.5 min (sum of wall_seconds)", markdown)
        bare = self.document(
            "5.json", self.good(), started_at=None, finished_at=None, wall_seconds=None
        )
        _, _, markdown, data = self.merge(standard_ledger(), bare)
        self.assertIsNone(data["wall_seconds"])
        self.assertIn("Wall time: n/a (unknown)", markdown)
        self.assertIsNone(data["minutes_per_mutant"]["wall"])

    def test_the_head_comes_from_the_files_or_from_the_root(self):
        document = self.document("a.json", self.good())
        _, _, _, data = self.merge(standard_ledger(), document)
        self.assertEqual(data["head"], self.HEAD)
        other = self.document("b.json", {}, head="f" * 40)
        _, _, _, data = self.merge(standard_ledger(), document, other)
        self.assertEqual(data["head"], "mixed")
        nohead = self.document("c.json", self.good(), head=None)
        repo = self.make_repo()
        code, out, err = self.invoke(
            "merge",
            "--root",
            repo,
            "--ledger",
            self.ledger_file(standard_ledger()),
            "--results",
            nohead,
        )
        self.assertIn(git(repo, "rev-parse", "HEAD")[:12], out.splitlines()[0])

    def test_the_summary_is_appended_to_the_github_step_summary(self):
        step = self.base / "step.md"
        put(step, "earlier\n")
        os.environ["GITHUB_STEP_SUMMARY"] = str(step)
        document = self.document("a.json", self.good())
        code, out, err = self.invoke(
            "merge",
            "--root",
            self.base,
            "--ledger",
            self.ledger_file(standard_ledger()),
            "--results",
            document,
            "--summary",
            step,
        )
        self.assertEqual(code, 0, err)
        text = step.read_text(encoding="utf-8")
        self.assertTrue(text.startswith("earlier\n# Mutant replay: "))
        plain = self.base / "plain.md"
        put(plain, "old\n")
        self.invoke(
            "merge",
            "--root",
            self.base,
            "--ledger",
            self.ledger_file(standard_ledger()),
            "--results",
            document,
            "--summary",
            plain,
        )
        self.assertTrue(plain.read_text(encoding="utf-8").startswith("# Mutant replay: "))

    def test_ids_of_other_ledgers_are_ignored_and_mentioned(self):
        results = dict(self.good(), stranger=self.record("killed"))
        code, out, markdown, data = self.merge(standard_ledger(), self.document("a.json", results))
        self.assertEqual((code, data["ignored_results"]), (0, ["stranger"]))
        self.assertIn("Ignored results for 1 id(s) that are not in the ledger.", markdown)

    def test_a_run_and_its_merge_agree(self):
        repo, work = self.make_repo(), self.base / "work"
        ledger = self.ledger_file(standard_ledger())
        for shard in ("0/2", "1/2"):
            code, text, err = self.invoke(
                *self.run_args(
                    repo,
                    ledger,
                    "--shard",
                    shard,
                    out=self.base / "results" / ("s" + shard[0] + ".json"),
                    work=work,
                )
            )
            self.assertEqual(code, 0, text + err)
        code, out, err = self.invoke(
            "merge", "--root", repo, "--ledger", ledger, "--results", self.base / "results"
        )
        self.assertEqual(code, 0, out + err)
        self.assertIn("| 4 | 3 | 1 | 0 | 0 | 0 | 0 | 0 |", out)


class RealLedgerTests(ToolTest):
    ROOT = HERE.parent

    def ledger(self, name):
        path = self.ROOT / "tests" / name
        if not path.is_file():
            self.skipTest("%s is not in this tree" % name)
        return path

    def test_the_k71_ledger_has_206_replayable_entries(self):
        ledger = tool.load_ledger(self.ledger("k71_mutants.json"))
        self.assertEqual((ledger.kind, len(ledger.mutants), ledger.unsupported), ("list", 206, []))
        self.assertIsNone(ledger.top)
        self.assertEqual(set(ledger.expected_map().values()), {"killed"})

    def test_the_memory_ledger_has_265_entries_and_8_unsupported(self):
        ledger = tool.load_ledger(self.ledger("memory_followup_mutants.json"))
        self.assertEqual(ledger.kind, "list")
        self.assertEqual(len(ledger.mutants) + len(ledger.unsupported), 265)
        self.assertEqual(len(ledger.unsupported), 8)
        self.assertEqual(len(ledger.mutants), 257)
        self.assertIn("campaign", ledger.top)
        self.assertEqual(sorted(set(ledger.expected_map().values())), ["killed", "survived"])
        self.assertEqual(list(ledger.expected_map().values()).count("survived"), 6)

    def test_the_reserved_resource_ledger_is_refused(self):
        with self.assertRaises(SystemExit) as caught:
            tool.load_ledger(self.ledger("reserved_resource_mutants.json"))
        self.assertIn("supported shapes", str(caught.exception))

    def test_the_k71_ledger_plans_twenty_shards(self):
        code, out, err = self.invoke(
            "plan", "--root", self.ROOT, "--ledger", "tests/k71_mutants.json", "--shards", "20"
        )
        self.assertEqual(code, 0, err)
        plan = json.loads(out)
        self.assertEqual((plan["kind"], plan["count"], plan["total_shards"]), ("list", 206, 20))


if __name__ == "__main__":
    unittest.main()
