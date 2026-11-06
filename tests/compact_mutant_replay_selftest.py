#!/usr/bin/env python3
"""Self-test of the replay tool: what `verify`, `merge`, `check` and `select` accept and refuse.

The acceptance of a mutant rests on `verify`, so the properties it relies on are pinned here with
synthetic evidence: a kill needs the whole evidence, a run on another version of the file is stale (and,
with --strict, so is a run on other sources), a gap needs a survival on the previous tree, an equivalence
needs a pin and a reviewer, and a batch that was never closed by a green baseline is no evidence. The
fingerprint of the sources is pinned too. No cargo command runs.

    python -m unittest tests/compact_mutant_replay_selftest.py
"""

import contextlib
import copy
import hashlib
import io
import json
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import compact_mutant_replay as tool  # noqa: E402

SOURCE = "fn a() {}\nlet anchor = 1;\nfn b() {}\n"
NAMED = "t::named"


def digest(text):
    return hashlib.sha256(tool.lf(text).encode("utf-8")).hexdigest()


def run_of(text=SOURCE, **changes):
    """One recorded run with the whole evidence of a kill."""
    before = digest(text)
    run = {
        "status": "killed",
        "anchor_matches": 1,
        "compiled": True,
        "failed_tests": [NAMED],
        "sha256_lf": {"before": before, "mutated": "m" * 64, "restored": before},
        "sha256_bytes": {"before": "b" * 64, "restored": "b" * 64},
        "restored_identically": True,
        "baseline_green": True,
    }
    run.update(changes)
    return run


def mutant(**fields):
    base = {
        "id": "m1",
        "file": "src/f.rs",
        "old": "let anchor = 1;",
        "new": "let anchor = 2;",
        "cmd": "test x",
        "expect": [NAMED],
        "runs": {"local": run_of()},
    }
    base.update(fields)
    return base


class Tree:
    """A checkout holding one source file."""

    def __init__(self, text=SOURCE):
        self._directory = tempfile.TemporaryDirectory()
        self.root = Path(self._directory.name)
        (self.root / "src").mkdir()
        (self.root / "src" / "f.rs").write_text(text, encoding="utf-8")

    def close(self):
        self._directory.cleanup()


class ReplayToolTest(unittest.TestCase):
    def setUp(self):
        self.tree = Tree()
        self.addCleanup(self.tree.close)

    def verdict(self, data, minimum=1, strict=False):
        with contextlib.redirect_stdout(io.StringIO()) as out:
            accepted = tool.verify(self.tree.root, data, minimum, strict)
        return accepted, out.getvalue()

    def test_a_kill_with_the_whole_evidence_is_accepted(self):
        accepted, out = self.verdict({"mutants": [mutant()]})
        self.assertTrue(accepted, out)

    def test_each_missing_piece_of_evidence_keeps_the_mutant_open(self):
        broken = {
            "survived": run_of(status="survived"),
            "compile error": run_of(status="compile-error", compiled=False),
            "anchor twice": run_of(anchor_matches=2),
            "named test did not fail": run_of(failed_tests=["t::other"]),
            "baseline not green": run_of(baseline_green=False),
            "restored differently": run_of(restored_identically=False),
            "restore hash differs": run_of(
                sha256_lf={"before": digest(SOURCE), "mutated": "m" * 64, "restored": "x" * 64}
            ),
        }
        for name, run in broken.items():
            with self.subTest(name):
                accepted, out = self.verdict({"mutants": [mutant(runs={"local": run})]})
                self.assertFalse(accepted, out)
                self.assertIn("OPEN", out)

    def test_a_run_on_another_version_of_the_file_is_stale_not_accepted(self):
        stale = run_of(text=SOURCE + "// changed\n")
        accepted, out = self.verdict({"mutants": [mutant(runs={"local": stale})]})
        self.assertFalse(accepted)
        self.assertIn("STALE", out)

    def test_strict_counts_only_runs_replayed_on_the_sources_of_this_tree(self):
        here = tool.fingerprint(self.tree.root)["sha256"]
        bound = run_of(sources_sha256=here)
        for name, run, expected in (
            ("bound to this tree", bound, True),
            ("bound to another tree", run_of(sources_sha256="o" * 64), False),
            ("no fingerprint", run_of(), False),
        ):
            with self.subTest(name):
                data = {"mutants": [mutant(runs={"local": run})]}
                self.assertTrue(self.verdict(data)[0], "the mutated file is the same: the default accepts it")
                accepted, out = self.verdict(data, strict=True)
                self.assertEqual(accepted, expected, out)
                if not expected:
                    self.assertIn("STALE", out)
        # A test or a helper that changes in another file stales the bound run; the mutated file is untouched.
        data = {"mutants": [mutant(runs={"local": bound})]}
        self.assertIn("1 of 1 valid run(s) were replayed on exactly this source tree", self.verdict(data)[1])
        (self.tree.root / "src" / "g.rs").write_text("#[test]\nfn named() {}\n", encoding="utf-8")
        accepted, out = self.verdict(data)
        self.assertTrue(accepted, out)
        self.assertIn("0 of 1 valid run(s) were replayed on exactly this source tree", out)
        accepted, out = self.verdict(data, strict=True)
        self.assertFalse(accepted)
        self.assertIn("STALE", out)

    def test_an_anchor_that_no_longer_matches_once_keeps_the_mutant_open(self):
        accepted, out = self.verdict({"mutants": [mutant(old="let missing = 1;")]})
        self.assertFalse(accepted)
        self.assertIn("anchor matches 0", out)

    def test_two_machines_are_needed_where_the_list_asks_for_them(self):
        one = {"mutants": [mutant()], "two_machines": ["m1"]}
        self.assertFalse(self.verdict(one)[0])
        two = {"mutants": [mutant(runs={"local": run_of(), "peer": run_of()})], "two_machines": ["m1"]}
        self.assertTrue(self.verdict(two)[0])
        three = {"mutants": [mutant(min_machines=2)]}
        self.assertFalse(self.verdict(three)[0])

    def test_machines_that_disagree_on_the_mutated_file_are_refused(self):
        other = run_of(sha256_lf={"before": digest(SOURCE), "mutated": "n" * 64, "restored": digest(SOURCE)})
        data = {"mutants": [mutant(runs={"local": run_of(), "peer": other})]}
        accepted, out = self.verdict(data)
        self.assertFalse(accepted)
        self.assertIn("disagree", out)

    def test_runs_on_two_versions_of_the_file_are_judged_on_the_version_in_the_tree(self):
        earlier = run_of(text=SOURCE + "// an earlier tree\n")
        # The mutated file of the earlier tree is another file: that is no disagreement.
        earlier["sha256_lf"]["mutated"] = "e" * 64
        data = {"mutants": [mutant(runs={"local": run_of(), "earlier": earlier})]}
        accepted, out = self.verdict(data)
        self.assertTrue(accepted, out)
        self.assertIn("stale on earlier", out)
        # On the earlier tree the same ledger judges the other run, and the first one is stale.
        (self.tree.root / "src" / "f.rs").write_text(SOURCE + "// an earlier tree\n", encoding="utf-8")
        accepted, out = self.verdict(data)
        self.assertTrue(accepted, out)
        self.assertIn("stale on local", out)
        # Two machines that disagree on the same version are still refused.
        other = run_of(text=SOURCE + "// an earlier tree\n")
        other["sha256_lf"]["mutated"] = "n" * 64
        both = {"mutants": [mutant(runs={"earlier": earlier, "other": other})]}
        accepted, out = self.verdict(both)
        self.assertFalse(accepted)
        self.assertIn("disagree", out)

    def test_a_gap_must_be_measured_as_a_survival_on_the_previous_tree(self):
        survived = run_of(status="survived", failed_tests=[])
        self.assertTrue(self.verdict({"mutants": [mutant(gap="v0", before={"local": survived})]})[0])
        self.assertFalse(self.verdict({"mutants": [mutant(gap="v0")]})[0], "not measured")
        killed_before = {"local": run_of()}
        accepted, out = self.verdict({"mutants": [mutant(gap="v0", before=killed_before)]})
        self.assertFalse(accepted)
        self.assertIn("not a gap", out)
        unclosed = {"local": run_of(status="survived", failed_tests=[], baseline_green=False)}
        self.assertFalse(self.verdict({"mutants": [mutant(gap="v0", before=unclosed)]})[0])

    def test_an_equivalent_mutant_needs_a_pin_a_pin_mutant_and_a_reviewer(self):
        survived = run_of(status="survived", failed_tests=[])
        pin = mutant(id="pin", expect=[NAMED], runs={"local": run_of()})
        argument = {"argument": "a", "invariant": "i", "pinned_by": [NAMED], "pin_mutant": "pin"}
        pending = mutant(runs={"local": survived}, equivalent={**argument, "review": "pending"})
        accepted, out = self.verdict({"mutants": [pending, pin]})
        self.assertFalse(accepted)
        self.assertIn("awaits independent review", out)
        reviewed = mutant(runs={"local": survived}, equivalent={**argument, "review": "R. Eviewer"})
        self.assertTrue(self.verdict({"mutants": [reviewed, pin]})[0])
        unpinned = mutant(runs={"local": survived}, equivalent={**argument, "pinned_by": [], "review": "R"})
        self.assertFalse(self.verdict({"mutants": [unpinned, pin]})[0])
        no_pin_mutant = copy.deepcopy(pin)
        no_pin_mutant["expect"] = []
        self.assertFalse(self.verdict({"mutants": [reviewed, no_pin_mutant]})[0])
        killed = mutant(equivalent={**argument, "review": "R"})
        self.assertFalse(self.verdict({"mutants": [killed, pin]})[0], "an equivalent mutant must survive")

    def test_mutants_that_another_ledger_replays_must_be_in_that_ledger(self):
        reused = {"ledger": "ledger.json", "obligations": {"m1": "R-1"}, "kit": ["K-1"]}
        data = {"mutants": [mutant()], "reused": reused}
        self.assertTrue(self.verdict(data)[0], "a tree without the ledger cannot say")
        (self.tree.root / "ledger.json").write_text(json.dumps([{"id": "R-1"}]), encoding="utf-8")
        accepted, out = self.verdict(data)
        self.assertFalse(accepted)
        self.assertIn("K-1 is not in ledger.json", out)
        (self.tree.root / "ledger.json").write_text(
            json.dumps({"mutants": [{"id": "R-1"}, {"id": "K-1"}]}), encoding="utf-8"
        )
        self.assertTrue(self.verdict(data)[0])
        survived = {"status": "survived", "restored_identically": True}
        unrestored = {"status": "killed", "restored_identically": False}
        for final in (survived, unrestored):
            (self.tree.root / "ledger.json").write_text(
                json.dumps({"mutants": [{"id": "R-1", "final": final}, {"id": "K-1", "final": {"status": "killed", "restored_identically": True}}]}),
                encoding="utf-8",
            )
            accepted, out = self.verdict(data)
            self.assertFalse(accepted, final)
            self.assertIn("R-1 has no restored kill in ledger.json", out)

    def test_check_wants_every_anchor_exactly_once(self):
        with contextlib.redirect_stdout(io.StringIO()):
            self.assertTrue(tool.check(self.tree.root, {"mutants": [mutant()]}))
            self.assertFalse(tool.check(self.tree.root, {"mutants": [mutant(old="fn ")]}), "twice")
            self.assertFalse(tool.check(self.tree.root, {"mutants": [mutant(old="nowhere")]}), "never")
            self.assertFalse(tool.check(self.tree.root, {"mutants": [mutant(new="let anchor = 1;")]}), "no change")

    def test_select_narrows_the_list_and_refuses_an_unknown_id(self):
        data = {"mutants": [mutant(), mutant(id="m2")]}
        self.assertEqual([m["id"] for m in tool.select(data, "m2")["mutants"]], ["m2"])
        self.assertEqual(len(tool.select(data, "")["mutants"]), 2)
        with self.assertRaises(SystemExit):
            tool.select(data, "m1,nope")


class FingerprintTest(unittest.TestCase):
    TREE = {
        "Cargo.toml": "[package]\n",
        "src/lib.rs": 'fn a() { include_str!("../site/asset.txt"); include_bytes!(\n    "../site/other.bin"\n); }\n',
        "tests/t.rs": "#[test]\nfn t() {}\n",
        "tests/fixtures/f.json": "{}\n",
        "site/asset.txt": "asset\n",
        "site/other.bin": "bin\n",
    }

    def setUp(self):
        self._directory = tempfile.TemporaryDirectory()
        self.addCleanup(self._directory.cleanup)
        self.root = Path(self._directory.name)
        for name, text in self.TREE.items():
            self.put(name, text)

    def put(self, name, text):
        path = self.root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(text.encode("utf-8"))

    def read(self, name):
        return (self.root / name).read_bytes().decode("utf-8")

    def sources(self):
        return tool.fingerprint(self.root)["sha256"]

    def test_the_fingerprint_follows_what_cargo_builds_and_reads(self):
        before = self.sources()
        self.assertEqual(tool.fingerprint(self.root)["files"], len(self.TREE))
        for name in self.TREE:
            with self.subTest(f"changing {name}"):
                original = self.read(name)
                self.put(name, original + "// changed\n")
                self.assertNotEqual(self.sources(), before)
                self.put(name, original)
                self.assertEqual(self.sources(), before)
        for name in ("docs/note.md", "site/unrelated.txt", "tests/ledger_mutants.json", "tests/tool.py", "pool-results/r.json"):
            with self.subTest(f"adding {name}"):
                self.put(name, "not a source\n")
                self.assertEqual(self.sources(), before)

    def test_a_directory_named_target_is_a_module_but_a_cargo_target_directory_is_not(self):
        before = self.sources()
        self.put("src/target/mod.rs", "fn module() {}\n")
        self.assertNotEqual(self.sources(), before)
        build = self.sources()
        self.put("src/out/CACHEDIR.TAG", "Signature: 8a477f597d28d172789f06886806bc55\n")
        self.put("src/out/debug.rs", "fn built() {}\n")
        self.assertEqual(self.sources(), build, "a build directory is not a source")

    def test_line_endings_do_not_change_the_fingerprint(self):
        before = self.sources()
        for name in ("src/lib.rs", "tests/fixtures/f.json"):
            self.put(name, self.read(name).replace("\n", "\r\n"))
        self.assertEqual(self.sources(), before)

    def test_a_file_that_is_included_but_not_there_is_ignored(self):
        before = self.sources()
        self.put("src/more.rs", 'fn m() { include_str!("../nowhere.txt"); }\n')
        self.assertNotEqual(self.sources(), before, "the new Rust file itself counts")
        self.assertEqual(tool.fingerprint(self.root)["files"], len(self.TREE) + 1)


class MergeTest(unittest.TestCase):
    def setUp(self):
        self._directory = tempfile.TemporaryDirectory()
        self.addCleanup(self._directory.cleanup)
        self.dir = Path(self._directory.name)
        self.list = self.dir / "list.json"
        first, second = mutant(id="m1"), mutant(id="m2")
        for entry in (first, second):
            entry.pop("runs")
        tool.write(self.list, {"mutants": [first, second]})

    def batch(self, name, closed=True, green=True):
        record = {"id": "m1", "status": "killed", "compiled": True}
        other = {"id": "m2", "status": "killed", "compiled": True}
        summary = {
            "machine": name,
            "environment": {"os": "o", "rustc": "r", "head": "h", "python": "p"},
            "baseline_before": {"c": {"green": green}},
            "results": [record, other],
        }
        if closed:
            summary["baseline_after"] = {"c": {"green": True}}
        path = self.dir / f"{name}.json"
        tool.write(path, summary)
        return path

    def merged(self):
        return {m["id"]: m for m in json.loads(self.list.read_text(encoding="utf-8"))["mutants"]}

    def test_a_batch_without_its_closing_baseline_is_not_green(self):
        tool.merge(self.list, [self.batch("open", closed=False)])
        self.assertFalse(self.merged()["m1"]["runs"]["open"]["baseline_green"])
        tool.merge(self.list, [self.batch("closed")])
        self.assertTrue(self.merged()["m1"]["runs"]["closed"]["baseline_green"])

    def test_a_red_baseline_before_the_first_mutant_is_not_green(self):
        tool.merge(self.list, [self.batch("red", green=False)])
        self.assertFalse(self.merged()["m1"]["runs"]["red"]["baseline_green"])

    def test_the_fingerprint_of_a_batch_is_kept_with_each_run(self):
        path = self.batch("fingerprinted")
        summary = json.loads(path.read_text(encoding="utf-8"))
        summary["sources"] = {"sha256": "s" * 64, "files": 3}
        tool.write(path, summary)
        tool.merge(self.list, [path, self.batch("old")])
        runs = self.merged()["m1"]["runs"]
        self.assertEqual(runs["fingerprinted"]["sources_sha256"], "s" * 64)
        self.assertNotIn("sources_sha256", runs["old"], "a batch from before the fingerprint has none")

    def test_before_runs_and_filters_go_where_they_belong(self):
        tool.merge(self.list, [self.batch("earlier")], before=True, ids="m2")
        merged = self.merged()
        self.assertIn("earlier", merged["m2"]["before"])
        self.assertNotIn("runs", merged["m2"])
        self.assertNotIn("before", merged["m1"])


if __name__ == "__main__":
    unittest.main()
