#!/usr/bin/env python3
"""Graph-off baseline capture and measurement (feature-agent-graph ITEM-003/ITEM-006).

Subcommands
-----------
fixtures [--rev 347b01d] [--work DIR] [--check]
    Extracts the exact baseline commit with `git archive` into an isolated
    temporary directory (never the working tree), builds and runs
      * the capture program tests/graph_baseline/capture.rs + scenario.rs as a
        throwaway crate depending on that extraction (default features), and
      * that revision's `vlite-mcp` (`--features mcp`) for the REG-008 MCP
        transcript,
    all OFFLINE into per-revision target directories, and copies the baseline
    `src/openapi.json` (served verbatim through `include_str!`). Writes
    tests/fixtures/graph_baseline/ with MANIFEST.sha256 (SHA-256 of decoded
    `.hex` bytes and LF-normalized text). With --check, compares the fresh
    capture against the committed fixtures instead of writing (V3 and all
    text byte-exact; V4/V5 log files after the same REG-003 masking of
    random lineage/salt/nonce bytes as tests/graph_regression.rs).

measure [--rev REV | --worktree] [--profile smoke|reference] [--runs N]
        [--reference-machine-confirmed] [--storage-description TEXT] [--output PATH]
    REG-010/DO-005 measurement driver. Builds `vlite-bench` and `vlite-ffi`
    (release, default features) for the baseline revision (isolated) or the
    working tree, runs the workloads through tests/measure_benchmark.py (peak
    RSS) N times, records binary sizes, toolchain, commit and machine facts,
    and runs the REG-013 artifact feature-set assertion
    (tests/graph_build_boundary.py). `smoke` uses the small CI workloads only;
    `reference` adds the seeded 100,000 x 384 dataset and REQUIRES
    --reference-machine-confirmed because DO-005 numbers are only valid on the
    agreed reference machine. Prints JSON; writes a file only with --output.

Uses only the Python standard library, git and cargo.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import platform
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from graph_compat_check import cargo_env, extract, resolve, run_cmd  # noqa: E402

ROOT = Path(__file__).resolve().parents[1]
FIXTURE_DIR = ROOT / "tests" / "fixtures" / "graph_baseline"
SCENARIO_DIR = ROOT / "tests" / "graph_baseline"
BASELINE_REV = "347b01d"
EXE = ".exe" if os.name == "nt" else ""

# REG-008 MCP transcript: lines sent to `vlite-mcp --db <fixture>`; the
# fixture is the same tiny database tests/graph_surface_parity.rs builds.
MCP_REQUESTS = [
    {"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "graph-baseline", "version": "0"}}},
    {"jsonrpc": "2.0", "method": "notifications/initialized"},
    {"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}},
    {"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "vlite_list_collections", "arguments": {}}},
    {"jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": {"name": "vlite_search", "arguments": {"vector": [0.1, 0.1], "k": 2}}},
    {"jsonrpc": "2.0", "id": 5, "method": "tools/call", "params": {"name": "vlite_get", "arguments": {"id": "2"}}},
    {"jsonrpc": "2.0", "id": 6, "method": "tools/call", "params": {"name": "vlite_list", "arguments": {"limit": 2}}},
    {"jsonrpc": "2.0", "id": 7, "method": "tools/call", "params": {"name": "vlite_index_status", "arguments": {}}},
    {"jsonrpc": "2.0", "id": 8, "method": "tools/call", "params": {"name": "vlite_upsert", "arguments": {"id": "9", "vector": [3.0, 3.0]}}},
    {"jsonrpc": "2.0", "id": 9, "method": "tools/call", "params": {"name": "nope", "arguments": {}}},
    {"jsonrpc": "2.0", "id": 10, "method": "graph/unknown", "params": {}},
]

MCP_DB_MAIN = r"""
fn main() {
    let path = std::env::args().nth(1).unwrap();
    let mut db = vlite::Database::create(&path, 2).unwrap();
    db.put(1, &[0.0, 0.0], b"origin").unwrap();
    db.put(2, &[1.0, 1.0], b"near").unwrap();
    db.put(3, &[2.0, 2.0], b"far").unwrap();
    db.close().unwrap();
}
"""


def sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def canonical(path: Path) -> bytes:
    data = path.read_bytes()
    if path.suffix == ".hex":
        return bytes.fromhex("".join(data.decode("ascii").split()))
    return data.replace(b"\r\n", b"\n")


def masked(data: bytes, encrypted: bool) -> bytes:
    """REG-003 shape of a log file; mirrors `masked` in tests/graph_regression.rs.

    V4/V5 lineage (header bytes 16..24) and header CRC (28..32) are random per
    file; encrypted files additionally have a random descriptor salt and
    per-frame nonces, so the descriptor and sealed bodies become lengths."""
    header = bytearray(data[:32])
    if int.from_bytes(data[8:12], "little") >= 4:
        header[16:24] = b"\xee" * 8
        header[28:32] = b"\xee" * 4
    if not encrypted:
        return bytes(header) + data[32:]
    out = bytearray(header)
    first = data.index(b"VREC", 32)
    out += b"DESC" + (first - 32).to_bytes(8, "little")
    offset = first
    while offset < len(data):
        if data[offset:offset + 4] != b"VREC":
            raise ValueError(f"frame expected at {offset}")
        prefix = data[offset:offset + 36]
        sequence = prefix[8:16]
        body = offset + 36
        footer = next(
            at for at in range(body, len(data) - 15)
            if data[at:at + 4] == b"VCMT" and data[at + 8:at + 16] == sequence
            and (at + 16 == len(data) or data[at + 16:at + 20] == b"VREC")
        )
        out += prefix + b"BODY" + (footer - body).to_bytes(8, "little") + b"VCMT" + sequence
        offset = footer + 16
    return bytes(out)


def comparable(path: Path) -> bytes:
    """What --check compares: masked log bytes, and the manifest without the
    digests of randomized log files (their masked shape is compared instead)."""
    data = canonical(path)
    if path.name.endswith(".vlite.hex"):
        return masked(data, path.name.startswith("v5_encrypted"))
    if path.name == "MANIFEST.sha256":
        return b"\n".join(line for line in data.split(b"\n") if b".vlite.hex" not in line or b"v3." in line)
    return data


def throwaway_crate(work: Path, name: str, source: Path, files: dict[str, str], features: str = "") -> Path:
    """Builds a binary crate depending only on the extracted revision, offline."""
    crate = work / name
    if crate.exists():
        shutil.rmtree(crate)
    (crate / "src").mkdir(parents=True)
    for file, text in files.items():
        (crate / "src" / file).write_text(text, encoding="utf-8", newline="\n")
    feature_list = f", features = [{features}]" if features else ""
    (crate / "Cargo.toml").write_text(
        f"[package]\nname = \"{name}\"\nversion = \"0.0.0\"\nedition = \"2024\"\npublish = false\n\n"
        f"[dependencies]\nvlite = {{ path = \"{source.as_posix()}\"{feature_list} }}\n\n[workspace]\n",
        encoding="utf-8",
    )
    shutil.copyfile(source / "Cargo.lock", crate / "Cargo.lock")
    target = work / f"target-{name}"
    run_cmd(["cargo", "build", "--offline", "--quiet", "--target-dir", str(target)], cwd=crate, env=cargo_env())
    return target / "debug" / f"{name}{EXE}"


def capture(rev: str, work: Path, out: Path) -> None:
    sha = resolve(rev)
    source = extract(rev, sha, work)
    scenario = (SCENARIO_DIR / "scenario.rs").read_text(encoding="utf-8")
    capture_rs = (SCENARIO_DIR / "capture.rs").read_text(encoding="utf-8")
    # capture.rs becomes main.rs verbatim; its `mod scenario;` resolves to src/scenario.rs.
    exe = throwaway_crate(work, "graph-baseline-capture", source, {"main.rs": capture_rs, "scenario.rs": scenario})
    scratch = work / "capture-scratch"
    if scratch.exists():
        shutil.rmtree(scratch)
    run_cmd([str(exe), str(out), str(scratch)])

    # REG-008: served OpenAPI = the baseline file verbatim (include_str!).
    (out / "openapi.json").write_bytes((source / "src" / "openapi.json").read_bytes().replace(b"\r\n", b"\n"))

    # REG-008: MCP transcript from the baseline's own vlite-mcp binary.
    target = work / f"target-mcp-{rev}"
    run_cmd(["cargo", "build", "--offline", "--locked", "--quiet", "--features", "mcp", "--bin", "vlite-mcp", "--target-dir", str(target)], cwd=source, env=cargo_env())
    mcp = target / "debug" / f"vlite-mcp{EXE}"
    make_db = throwaway_crate(work, "graph-baseline-mcpdb", source, {"main.rs": MCP_DB_MAIN})
    stdin = "".join(json.dumps(request, separators=(",", ":")) + "\n" for request in MCP_REQUESTS)
    (out / "mcp_requests.jsonl").write_text(stdin, encoding="utf-8", newline="\n")
    for mode in ("read-only", "writable"):
        db = work / f"mcp-{mode}.vlite"
        if db.exists():
            db.unlink()
        run_cmd([str(make_db), str(db)])
        result = subprocess.run([str(mcp), "--db", str(db), f"--{mode}"], input=stdin, capture_output=True, text=True, encoding="utf-8", check=True)
        responses = [json.loads(line) for line in result.stdout.splitlines() if line.strip()]
        (out / f"mcp_{mode.replace('-', '_')}.json").write_text(json.dumps(responses, indent=2, sort_keys=True) + "\n", encoding="utf-8", newline="\n")

    readme = out / "README.md"
    names = sorted(path.name for path in out.iterdir() if path.name not in {"MANIFEST.sha256", "README.md"})
    lines = [f"{sha256(canonical(out / name))}  {name}" for name in names]
    (out / "MANIFEST.sha256").write_text(
        f"# Graph-off golden captures from baseline commit {sha} (`git archive`, isolated offline build).\n"
        "# SHA-256 of decoded `.hex` bytes and LF-normalized text.\n"
        "# Generated by: python scripts/graph_baseline.py fixtures\n" + "\n".join(lines) + "\n",
        encoding="ascii",
        newline="\n",
    )
    readme.write_text(README.format(sha=sha), encoding="utf-8", newline="\n")


README = """# Graph-off golden baseline (feature-agent-graph EPIC-001 ITEM-003)

Generated ONLY from baseline commit `{sha}` by

    python scripts/graph_baseline.py fixtures

which extracts that commit with `git archive` into a temporary directory and
builds everything offline there; the working tree is never used as the
baseline. `python scripts/graph_baseline.py fixtures --check` regenerates and
compares without writing.

| File | Content | Consumer |
|------|---------|----------|
| `<variant>.vlite.hex` | file bytes after `scenario::populate` + close | REG-003 (V3 byte-exact; V4/V5 lineage/CRC and encrypted salt/nonce/ciphertext masked), REG-004 |
| `<variant>.ops.txt` | `{{:?}}` of every populate result/error | REG-006 |
| `<variant>.report.txt` | reopened state, searches, parse, change feed, replica bootstrap | REG-005, REG-007 |
| `<variant>.state.txt` | state + searches only | REG-004 |
| `<variant>.maintenance.txt` | backup/restore/compact/verify results and each output's header version and state | REG-004 |
| `openapi.json` | served `/openapi.json` bytes (LF) | REG-008 |
| `mcp_requests.jsonl` | the fixed MCP request transcript | REG-008 |
| `mcp_read_only.json`, `mcp_writable.json` | `vlite-mcp` responses to that transcript on a 3-record default-format file | REG-008 |

Variants: `v3`, `v4`, `v5_plain`, `v5_encrypted`, `v5_timestamps` (see
`tests/graph_baseline/scenario.rs`). `.hex` text is whitespace-insensitive;
`MANIFEST.sha256` hashes decoded bytes and LF-normalized text.
"""


def check_or_write(args: argparse.Namespace) -> int:
    work = Path(args.work or Path(tempfile.gettempdir()) / "vlite-graph-baseline")
    work.mkdir(parents=True, exist_ok=True)
    if not args.check:
        if FIXTURE_DIR.exists():
            shutil.rmtree(FIXTURE_DIR)
        FIXTURE_DIR.mkdir(parents=True)
        capture(args.rev, work, FIXTURE_DIR)
        print(f"wrote {len(list(FIXTURE_DIR.iterdir()))} files to {FIXTURE_DIR}")
        return 0
    fresh = work / "check-out"
    if fresh.exists():
        shutil.rmtree(fresh)
    fresh.mkdir()
    capture(args.rev, work, fresh)
    failures = []
    names = {path.name for path in fresh.iterdir()} | {path.name for path in FIXTURE_DIR.iterdir()}
    for name in sorted(names):
        a, b = fresh / name, FIXTURE_DIR / name
        if not a.exists() or not b.exists() or comparable(a) != comparable(b):
            failures.append(name)
    for name in failures:
        print(f"DIFFERS: {name}")
    print("PASS: committed fixtures reproduce from the baseline" if not failures else "FAIL")
    return 1 if failures else 0


# ---------------------------------------------------------------------------
# REG-010 / DO-005 measurement driver.
# ---------------------------------------------------------------------------

SMOKE_WORKLOADS = {
    # The existing CI `vlite-bench` workloads (.github/workflows/ci.yml, compose smoke).
    "exact_small": ["64", "8", "8", "16", "5", "42"],
    "filtered_exact": ["--filter-every", "10", "1024", "8", "10", "64", "10", "42"],
    "hnsw_small": ["--ann", "16", "100", "100", "256", "8", "10", "64", "5", "42"],
}
REFERENCE_WORKLOADS = {
    # Ten queries keep exact work within vlite-bench's 500 million component ceiling.
    "exact_100k_384": ["100000", "384", "10", "1000", "10", "42"],
    "filtered_100k_384": ["--filter-every", "10", "100000", "384", "10", "1000", "10", "42"],
    "hnsw_100k_384": ["--ann", "16", "200", "64", "100000", "384", "10", "1000", "10", "42"],
}


def machine(storage_description: str | None) -> dict:
    facts = {"os": platform.platform(), "machine": platform.machine(), "processor": platform.processor(), "logical_cpus": os.cpu_count()}
    if os.name == "nt":
        import ctypes

        class Status(ctypes.Structure):
            _fields_ = [("length", ctypes.c_ulong), ("load", ctypes.c_ulong)] + [(name, ctypes.c_ulonglong) for name in ("total", "avail", "page_total", "page_avail", "virt_total", "virt_avail", "ext")]

        status = Status()
        status.length = ctypes.sizeof(Status)
        if ctypes.windll.kernel32.GlobalMemoryStatusEx(ctypes.byref(status)):
            facts["ram_bytes"] = status.total
    elif hasattr(os, "sysconf"):
        try:
            facts["ram_bytes"] = os.sysconf("SC_PAGE_SIZE") * os.sysconf("SC_PHYS_PAGES")
        except (ValueError, OSError):
            pass
    facts["storage"] = storage_description or "unrecorded"
    return facts


def build_release(source: Path, target: Path) -> dict[str, Path]:
    env = cargo_env()
    run_cmd(["cargo", "build", "--offline", "--locked", "--release", "--quiet", "--bin", "vlite-bench", "--target-dir", str(target)], cwd=source, env=env)
    run_cmd(["cargo", "build", "--offline", "--locked", "--release", "--quiet", "-p", "vlite-ffi", "--target-dir", str(target)], cwd=source, env=env)
    ffi = {"nt": "vlite_ffi.dll"}.get(os.name, "libvlite_ffi.dylib" if sys.platform == "darwin" else "libvlite_ffi.so")
    return {"vlite-bench": target / "release" / f"vlite-bench{EXE}", "vlite-ffi": target / "release" / ffi}


def measure(args: argparse.Namespace) -> int:
    if args.profile == "reference" and not args.reference_machine_confirmed:
        print("refusing: the reference profile is only valid on the DO-005 reference machine; pass --reference-machine-confirmed", file=sys.stderr)
        return 2
    if args.profile == "reference" and not args.storage_description:
        print("refusing: the reference profile requires --storage-description for DO-005", file=sys.stderr)
        return 2
    work = Path(args.work or Path(tempfile.gettempdir()) / "vlite-graph-baseline")
    work.mkdir(parents=True, exist_ok=True)
    if args.worktree:
        label, source, sha = "worktree (NOT a baseline)", ROOT, resolve("HEAD")
        dirty = run_cmd(["git", "status", "--porcelain"], cwd=ROOT).stdout.strip() != ""
        target = ROOT / "target" / "graph-baseline-measure"
    else:
        sha = resolve(args.rev)
        label, source, dirty = f"baseline {args.rev}", extract(args.rev, sha, work), False
        target = work / f"target-release-{args.rev}"
    binaries = build_release(source, target)
    workloads = dict(SMOKE_WORKLOADS)
    if args.profile == "reference":
        workloads.update(REFERENCE_WORKLOADS)
    results = {}
    for name, workload in workloads.items():
        runs = []
        for _ in range(args.runs):
            output = run_cmd([sys.executable, str(ROOT / "tests" / "measure_benchmark.py"), str(binaries["vlite-bench"]), *workload]).stdout
            parsed = [json.loads(line) for line in output.splitlines() if line.strip().startswith("{")]
            runs.append(parsed[-1] if parsed else output.strip())
        results[name] = {"args": workload, "runs": runs}
    boundary = run_cmd([sys.executable, str(ROOT / "tests" / "graph_build_boundary.py")], cwd=ROOT, check=False)
    report = {
        "label": label,
        "commit": sha,
        "working_tree_dirty": dirty,
        "profile": args.profile,
        "runs_per_workload": args.runs,
        "reference_machine_confirmed": bool(args.reference_machine_confirmed),
        "toolchain": run_cmd(["rustc", "-vV"], env=cargo_env()).stdout.strip().splitlines(),
        "machine": machine(args.storage_description),
        "binary_sizes": {name: path.stat().st_size for name, path in binaries.items()},
        "workloads": results,
        "reg013_artifact_feature_sets": "pass" if boundary.returncode == 0 else (boundary.stdout + boundary.stderr)[-2000:],
        "limitations": [
            "REG-010(b) open/replay time is vlite-bench `reopen_ms`; put throughput is `ingest_vectors_per_second`",
            "REG-013 assertion runs tests/graph_build_boundary.py against the working tree's artifact build files",
        ]
        + (["smoke profile: not DO-005 baseline values (no 100,000 x 384 dataset, not on a confirmed reference machine)"] if args.profile == "smoke" else []),
    }
    text = json.dumps(report, indent=2)
    print(text)
    if args.output:
        Path(args.output).write_text(text + "\n", encoding="utf-8")
    return 0 if boundary.returncode == 0 else 1


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    commands = parser.add_subparsers(dest="command", required=True)
    fixtures = commands.add_parser("fixtures")
    fixtures.add_argument("--rev", default=BASELINE_REV)
    fixtures.add_argument("--work")
    fixtures.add_argument("--check", action="store_true")
    measure_parser = commands.add_parser("measure")
    source = measure_parser.add_mutually_exclusive_group()
    source.add_argument("--rev", default=BASELINE_REV)
    source.add_argument("--worktree", action="store_true")
    measure_parser.add_argument("--profile", choices=["smoke", "reference"], default="smoke")
    measure_parser.add_argument("--runs", type=int, default=5)
    measure_parser.add_argument("--reference-machine-confirmed", action="store_true")
    measure_parser.add_argument("--storage-description")
    measure_parser.add_argument("--work")
    measure_parser.add_argument("--output")
    args = parser.parse_args()
    return check_or_write(args) if args.command == "fixtures" else measure(args)


if __name__ == "__main__":
    raise SystemExit(main())
