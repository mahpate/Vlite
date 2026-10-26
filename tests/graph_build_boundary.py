"""Keep the internal vlite-graph crate isolated and pin graph into shipped artifacts (GATE-008)."""
from pathlib import Path
import re
import subprocess
import tomllib
import unittest

ROOT = Path(__file__).resolve().parents[1]
CRATE = ROOT / "crates" / "vlite-graph"
# The pure engine lives in the root package so `cargo package -p vlite` ships it;
# the workspace-only crate compiles the same files through one `#[path]` module.
ENGINE = ROOT / "src" / "property_graph" / "engine"
HARNESS = '#[path = "../../../src/property_graph/engine/mod.rs"]'
# GATE-008 (approved by the owner on 2026-10-25 for v0.5.0): distributed
# artifacts must include these opt-in features; `default` must not.
SHIPPED_GRAPH_FEATURES = {"graph", "mcp-proxy"}
# Artifact-producing `cargo build --release --bins` commands in each file.
ARTIFACT_BUILDS = {"Dockerfile": 1, ".github/workflows/release.yml": 2}
# Artifact-producing vlite-ffi builds (C library, wheels, Node packages) per file.
FFI_BUILDS = {"Dockerfile": 1, ".github/workflows/release.yml": 1,
              "bindings/build_wheel.py": 1, "bindings/node/build.mjs": 1}
# Release archive recipes that must list the graph-capable command lines.
GRAPH_BINARIES = ["vlite-graph-cli", "vlite-mcp"]


def load_toml(path):
    return tomllib.loads(path.read_text(encoding="utf-8"))


def cargo_tree(*args):
    command = ["cargo", "tree", "--locked", "--target", "all", "--prefix", "none", *args]
    return subprocess.run(command, cwd=ROOT, check=True, capture_output=True, text=True).stdout.splitlines()


class GraphBuildBoundary(unittest.TestCase):
    def test_crate_manifest_is_locked_to_vlite(self):
        root = load_toml(ROOT / "Cargo.toml")
        package = load_toml(CRATE / "Cargo.toml")["package"]
        for key in ("version", "edition", "rust-version"):
            self.assertEqual(package[key], root["package"][key], key)
        self.assertIs(package["publish"], False)
        self.assertIn("#![forbid(unsafe_code)]", (CRATE / "src" / "lib.rs").read_text(encoding="utf-8"))
        self.assertIn("crates/vlite-graph", root["workspace"]["members"])

    def test_crate_has_no_dependencies(self):
        tree = cargo_tree("-p", "vlite-graph", "--all-features", "-e", "normal,build,dev")
        self.assertEqual(len(tree), 1, tree)
        self.assertTrue(tree[0].startswith("vlite-graph v"), tree)

    def test_unpublished_crate_stays_workspace_only(self):
        root = load_toml(ROOT / "Cargo.toml")
        # Even an optional unpublished dependency prevents packaging vlite.
        for table in ("dependencies", "dev-dependencies", "build-dependencies"):
            self.assertNotIn("vlite-graph", root.get(table, {}), table)
        for target in root.get("target", {}).values():
            for table in ("dependencies", "dev-dependencies", "build-dependencies"):
                self.assertNotIn("vlite-graph", target.get(table, {}), table)
        # `graph` only compiles the in-package engine; it enables nothing else
        # and is never on by default.
        self.assertEqual(root["features"]["graph"], [])
        self.assertNotIn("graph", root["features"]["default"])
        # `mcp-proxy` (ITEM-043) layers the graph tools over a remote server;
        # it is never on by default but is pinned into artifacts (GATE-008).
        self.assertIn("graph", root["features"]["mcp-proxy"])
        self.assertNotIn("mcp-proxy", root["features"]["default"])
        for features in ((), ("--all-features",)):
            with self.subTest(features=features):
                tree = cargo_tree("-p", "vlite", "-e", "normal", *features)
                self.assertFalse(any(line.startswith("vlite-graph v") for line in tree), tree)

    def test_crate_is_a_thin_harness_over_the_shared_engine(self):
        sources = sorted(path.relative_to(CRATE).as_posix() for path in (CRATE / "src").rglob("*") if path.is_file())
        self.assertEqual(sources, ["src/lib.rs"])
        lib = (CRATE / "src" / "lib.rs").read_text(encoding="utf-8")
        self.assertEqual(lib.count(HARNESS), 1)
        self.assertIn("pub use engine::*;", lib)
        self.assertTrue((ENGINE / "mod.rs").is_file())
        root_lib = (ROOT / "src" / "lib.rs").read_text(encoding="utf-8")
        self.assertIn('#[cfg(feature = "graph")]\npub mod property_graph;', root_lib)

    def test_engine_is_pure_and_module_relative(self):
        files = sorted(ENGINE.rglob("*.rs"))
        self.assertGreater(len(files), 5)
        self.assertIn("#![forbid(unsafe_code)]", (ENGINE / "mod.rs").read_text(encoding="utf-8"))
        for path in files:
            # Code only: doc comments may describe the rules they follow.
            text = "\n".join(re.sub(r"//.*", "", line) for line in path.read_text(encoding="utf-8").splitlines())
            with self.subTest(file=path.relative_to(ROOT).as_posix()):
                # `crate::` would resolve differently in the crate and in vlite.
                self.assertNotRegex(text, r"\bcrate::")
                self.assertNotRegex(text, r"\bunsafe\b")
                self.assertNotRegex(text, r"\b(use|extern crate)\s+vlite\b")
                self.assertNotRegex(text, r"\bstd::(fs|io|net|process|thread|env)\b")

    def test_package_ships_the_engine(self):
        listing = subprocess.run(
            ["cargo", "package", "--list", "--allow-dirty", "--locked", "-p", "vlite"],
            cwd=ROOT, check=True, capture_output=True, text=True,
        ).stdout.splitlines()
        shipped = {line.replace("\\", "/") for line in listing}
        for path in ENGINE.rglob("*.rs"):
            self.assertIn(path.relative_to(ROOT).as_posix(), shipped)
        self.assertIn("src/property_graph/mod.rs", shipped)
        self.assertFalse([line for line in shipped if line.startswith("crates/")])

    def test_artifact_builds_pin_shipped_features(self):
        features = load_toml(ROOT / "Cargo.toml")["features"]
        self.assertTrue(SHIPPED_GRAPH_FEATURES <= set(features))
        # Every non-default feature, including graph and mcp-proxy, is pinned.
        expected = sorted(set(features) - {"default"})
        self.assertTrue(SHIPPED_GRAPH_FEATURES <= set(expected))
        for name, count in ARTIFACT_BUILDS.items():
            builds = [line for line in (ROOT / name).read_text(encoding="utf-8").splitlines()
                      if "cargo build" in line and "--release" in line]
            with self.subTest(file=name):
                self.assertFalse([line for line in builds if "--all-features" in line])
                pinned = [re.search(r"--features (\S+)", line) for line in builds if "--bins" in line]
                self.assertEqual(len(pinned), count)
                for match in pinned:
                    self.assertIsNotNone(match)
                    self.assertEqual(sorted(match.group(1).split(",")), expected)

    def test_ffi_artifact_builds_enable_graph(self):
        ffi = load_toml(ROOT / "bindings" / "c" / "Cargo.toml")["features"]
        self.assertEqual(ffi["graph"], ["vlite/graph"])
        # The crate default stays graph-free; only artifact recipes opt in.
        self.assertNotIn("graph", ffi.get("default", []))
        for name, count in FFI_BUILDS.items():
            text = (ROOT / name).read_text(encoding="utf-8")
            with self.subTest(file=name):
                if name.endswith((".py", ".mjs")):
                    builds = re.findall(r'"build", "--locked", "--release", "-p", "vlite-ffi", ([^\]]*)', text)
                    self.assertEqual(len(builds), count)
                    for rest in builds:
                        self.assertIn('"--features", "graph"', rest)
                else:
                    builds = [line for line in text.splitlines()
                              if "cargo build" in line and "--release" in line and "-p vlite-ffi" in line]
                    self.assertEqual(len(builds), count)
                    for line in builds:
                        self.assertRegex(line, r"-p vlite-ffi --features graph\b")

    def test_release_archives_ship_graph_binaries(self):
        # FOLLOWUP-001/ITEM-043: the graph command line and the MCP server
        # (with graph tools and `--server` proxy mode) ship in every release
        # archive, built by the pinned `--bins` commands above.
        bins = load_toml(ROOT / "Cargo.toml")["bin"]
        graph_bins = [entry["name"] for entry in bins if "graph" in entry.get("required-features", [])]
        self.assertEqual(graph_bins, ["vlite-graph-cli"])
        release = (ROOT / ".github" / "workflows" / "release.yml").read_text(encoding="utf-8")
        archives = re.findall(r'for name in \["vlite-server",[^\]]*\]', release)
        self.assertEqual(len(archives), ARTIFACT_BUILDS[".github/workflows/release.yml"])
        for archive in archives:
            for name in GRAPH_BINARIES:
                with self.subTest(binary=name):
                    self.assertIn(f'"{name}"', archive)


if __name__ == "__main__":
    unittest.main()
