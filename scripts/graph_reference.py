"""Collect five graph benchmark runs on the recorded Windows reference host.

Build the latency and allocation examples first. This runner never builds, changes
Git state, or stops other processes. It records raw output, source/binary hashes,
peak working set and competing CPU activity; allocation metrics come from the
separate memory example, not from working set.

The reference host is never idle: its security and management agents keep the
guest about 5% busy with bursts near 45%. The background gates are therefore
collection-quality checks calibrated to that measured floor, not an idle claim.
"""

from __future__ import annotations

import argparse
import ctypes
from ctypes import wintypes
from datetime import datetime, timezone
import hashlib
import json
import os
from pathlib import Path
import subprocess
import tempfile
import time

ROOT = Path(__file__).resolve().parents[1]
SAMPLE_SECONDS = 0.5
IDLE_SECONDS = 5
PROBE_ATTEMPTS = 12
# A passive three-minute survey of the reference host measured a 4.99% mean
# background and a 45.1% worst half-second sample, so a gate of 2% / 10% could
# never pass. These allow about twice that mean and the observed burst size.
MAX_BACKGROUND_MEAN = 0.10
MAX_BACKGROUND_SAMPLE = 0.50


class MemoryCounters(ctypes.Structure):
    _fields_ = [
        ("cb", wintypes.DWORD),
        ("PageFaultCount", wintypes.DWORD),
        *[(name, ctypes.c_size_t) for name in (
            "PeakWorkingSetSize", "WorkingSetSize", "QuotaPeakPagedPoolUsage",
            "QuotaPagedPoolUsage", "QuotaPeakNonPagedPoolUsage",
            "QuotaNonPagedPoolUsage", "PagefileUsage", "PeakPagefileUsage",
        )],
    ]


class CpuClock:
    def __init__(self):
        if os.name != "nt":
            raise RuntimeError("this DO-005 collection protocol requires Windows")
        self.kernel = ctypes.WinDLL("kernel32", use_last_error=True)
        self.psapi = ctypes.WinDLL("psapi", use_last_error=True)
        times = [ctypes.POINTER(wintypes.FILETIME)] * 4
        self.kernel.GetSystemTimes.argtypes = times[:3]
        self.kernel.GetSystemTimes.restype = wintypes.BOOL
        self.kernel.GetProcessTimes.argtypes = [wintypes.HANDLE, *times]
        self.kernel.GetProcessTimes.restype = wintypes.BOOL
        self.kernel.OpenProcess.argtypes = [wintypes.DWORD, wintypes.BOOL, wintypes.DWORD]
        self.kernel.OpenProcess.restype = wintypes.HANDLE
        self.kernel.CloseHandle.argtypes = [wintypes.HANDLE]
        self.kernel.CloseHandle.restype = wintypes.BOOL
        self.psapi.GetProcessMemoryInfo.argtypes = [
            wintypes.HANDLE, ctypes.POINTER(MemoryCounters), wintypes.DWORD,
        ]
        self.psapi.GetProcessMemoryInfo.restype = wintypes.BOOL

    @staticmethod
    def seconds(value):
        return ((value.dwHighDateTime << 32) | value.dwLowDateTime) / 10_000_000

    def busy(self):
        idle, kernel, user = (wintypes.FILETIME() for _ in range(3))
        if not self.kernel.GetSystemTimes(
            ctypes.byref(idle), ctypes.byref(kernel), ctypes.byref(user)
        ):
            raise ctypes.WinError(ctypes.get_last_error())
        return self.seconds(kernel) + self.seconds(user) - self.seconds(idle)

    def child_cpu(self, handle):
        created, exited, kernel, user = (wintypes.FILETIME() for _ in range(4))
        if not self.kernel.GetProcessTimes(
            handle, ctypes.byref(created), ctypes.byref(exited),
            ctypes.byref(kernel), ctypes.byref(user),
        ):
            raise ctypes.WinError(ctypes.get_last_error())
        return self.seconds(kernel) + self.seconds(user)

    def peak_rss(self, handle):
        counters = MemoryCounters()
        counters.cb = ctypes.sizeof(counters)
        if not self.psapi.GetProcessMemoryInfo(handle, ctypes.byref(counters), counters.cb):
            raise ctypes.WinError(ctypes.get_last_error())
        return counters.PeakWorkingSetSize


def cpu_summary(samples, logical_cpus):
    elapsed = sum(sample["elapsed_seconds"] for sample in samples)
    competing = sum(sample["background_cpu_seconds"] for sample in samples)
    fractions = [
        sample["background_cpu_seconds"] / (sample["elapsed_seconds"] * logical_cpus)
        for sample in samples if sample["elapsed_seconds"] >= 0.1
    ]
    mean = competing / (elapsed * logical_cpus)
    peak = max(fractions, default=0.0)
    return {
        "mean_background_cpu_fraction": mean,
        "max_background_cpu_fraction": peak,
        "quiet": mean <= MAX_BACKGROUND_MEAN and peak <= MAX_BACKGROUND_SAMPLE,
        "samples": samples,
    }


def idle_probe(clock, logical_cpus):
    samples = []
    for _ in range(int(IDLE_SECONDS / SAMPLE_SECONDS)):
        before, start = clock.busy(), time.perf_counter()
        time.sleep(SAMPLE_SECONDS)
        elapsed = time.perf_counter() - start
        samples.append({
            "elapsed_seconds": elapsed,
            "background_cpu_seconds": max(0.0, clock.busy() - before),
        })
    return cpu_summary(samples, logical_cpus)


def calm_preflight(probe, attempts=PROBE_ATTEMPTS):
    probes = []
    for _ in range(attempts):
        probes.append(probe())
        if probes[-1]["quiet"]:
            break
    return probes


def run_process(command, clock, logical_cpus):
    samples = []
    with tempfile.TemporaryFile() as stdout, tempfile.TemporaryFile() as stderr:
        start, system_start = time.perf_counter(), clock.busy()
        child = subprocess.Popen(command, cwd=ROOT, stdout=stdout, stderr=stderr)
        handle = clock.kernel.OpenProcess(0x0410, False, child.pid)
        if not handle:
            error = ctypes.get_last_error()
            child.wait()
            raise ctypes.WinError(error)
        previous_time, previous_system, previous_child = start, system_start, 0.0
        try:
            while True:
                try:
                    child.wait(timeout=SAMPLE_SECONDS)
                except subprocess.TimeoutExpired:
                    pass
                now, system_now, child_now = (
                    time.perf_counter(), clock.busy(), clock.child_cpu(handle)
                )
                system_delta = system_now - previous_system
                child_delta = child_now - previous_child
                samples.append({
                    "elapsed_seconds": now - previous_time,
                    "system_cpu_seconds": system_delta,
                    "benchmark_cpu_seconds": child_delta,
                    "background_cpu_seconds": max(0.0, system_delta - child_delta),
                })
                previous_time, previous_system, previous_child = now, system_now, child_now
                if child.returncode is not None:
                    peak_rss = clock.peak_rss(handle)
                    if peak_rss <= 0:
                        raise RuntimeError("benchmark peak working set was not available")
                    break
        finally:
            clock.kernel.CloseHandle(handle)
            # Never terminate a workload on a monitor failure; let this bounded
            # child close its database and temporary files normally.
            child.wait()
        stdout.seek(0)
        stderr.seek(0)
        return {
            "command": [str(part) for part in command],
            "returncode": child.returncode,
            "elapsed_seconds": time.perf_counter() - start,
            "stdout": stdout.read().decode("utf-8"),
            "stderr": stderr.read().decode("utf-8"),
            "process_peak_working_set_bytes": peak_rss,
            "cpu": cpu_summary(samples, logical_cpus),
        }


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def source_manifest():
    paths = {ROOT / name for name in ("Cargo.toml", "Cargo.lock", "build.rs")}
    for directory in ("src", "examples", "crates"):
        paths.update((ROOT / directory).rglob("*.rs"))
        paths.update((ROOT / directory).rglob("Cargo.toml"))
    paths.add(Path(__file__).resolve())
    return {
        str(path.relative_to(ROOT)).replace("\\", "/"): digest(path)
        for path in sorted(paths) if path.is_file()
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--latency-exe", type=Path, required=True)
    parser.add_argument("--memory-exe", type=Path, required=True)
    parser.add_argument("--machine-json", type=Path, required=True)
    parser.add_argument("--work-dir", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--runs", type=int, default=5)
    parser.add_argument("--nodes", type=int, default=100_000)
    parser.add_argument("--edges", type=int, default=1_000_000)
    parser.add_argument("--queries", type=int, default=1_000)
    args = parser.parse_args()
    if min(args.runs, args.nodes, args.queries) < 1 or args.edges < 0:
        parser.error("runs/nodes/queries must be positive and edges nonnegative")
    machine = json.loads(args.machine_json.read_text(encoding="utf-8-sig"))
    identity = {
        "cpu": any("8370C" in cpu["Name"] for cpu in machine["cpu"]),
        "logical_cpus": machine["logical_cpus"] == 16,
        "ram": machine["ram_bytes"] == 68_665_831_424,
        "os": machine["os_version"] == "10.0.26300",
        "volume": machine["volume"]["DriveLetter"] == "Q"
        and machine["volume"]["FileSystem"] == "ReFS",
        "disk": machine["disk"]["FriendlyName"] == "Msft Virtual Disk"
        and machine["disk"]["BusType"] == "SAS",
    }
    if not all(identity.values()) or machine["active_builds"]:
        raise RuntimeError(f"reference host mismatch or active builds: {identity}, {machine['active_builds']}")
    if args.work_dir.resolve().drive.upper() != "Q:":
        raise RuntimeError("reference database files must use the recorded Q: volume")
    args.work_dir.mkdir(parents=True, exist_ok=True)
    manifest = source_manifest()
    report = {
        "schema_version": 1,
        "started_utc": datetime.now(timezone.utc).isoformat(),
        "commit": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip(),
        "working_tree_dirty": bool(subprocess.check_output(
            ["git", "status", "--porcelain"], cwd=ROOT, text=True
        ).strip()),
        "toolchain": subprocess.check_output(["rustc", "-vV"], text=True).splitlines(),
        "machine": machine,
        "reference_identity_checks": identity,
        "source_sha256": manifest,
        "binary_sha256": {name: digest(path) for name, path in (
            ("latency", args.latency_exe), ("memory", args.memory_exe)
        )},
        "protocol": {
            "runs": args.runs, "nodes": args.nodes, "edges": args.edges,
            "queries": args.queries, "seed": 1, "idle_probe_seconds": IDLE_SECONDS,
            "probe_attempts_max": PROBE_ATTEMPTS,
            "cpu_sample_seconds": SAMPLE_SECONDS,
            "max_background_mean_fraction": MAX_BACKGROUND_MEAN,
            "max_background_sample_fraction": MAX_BACKGROUND_SAMPLE,
            "cpu_scope": "Windows guest CPU; system busy minus benchmark process CPU",
            "rss_scope": "whole benchmark process, not allocator category bytes",
        },
        "runs": [],
    }
    clock = CpuClock()
    logical_cpus = machine["logical_cpus"]
    try:
        for number in range(1, args.runs + 1):
            probes = calm_preflight(lambda: idle_probe(clock, logical_cpus))
            repetition = {
                "number": number,
                "preflight": probes[-1],
                "preflight_rejected": probes[:-1],
            }
            report["runs"].append(repetition)
            if not repetition["preflight"]["quiet"]:
                raise RuntimeError(
                    f"repetition {number} refused: background load stayed above the gate "
                    f"for {len(probes)} probes"
                )
            for name, executable in (("latency", args.latency_exe), ("memory", args.memory_exe)):
                with tempfile.TemporaryDirectory(prefix=f"graph-{number}-{name}-", dir=args.work_dir) as work:
                    command = [
                        str(executable.resolve()), "--nodes", str(args.nodes),
                        "--edges", str(args.edges), "--queries", str(args.queries),
                        "--seed", "1", "--dir", work,
                    ]
                    result = run_process(command, clock, logical_cpus)
                repetition[name] = result
                if result["returncode"] != 0:
                    raise RuntimeError(f"{name} repetition {number} failed: {result['stderr']}")
                cpu = result["cpu"]
                if not cpu["quiet"]:
                    raise RuntimeError(
                        f"{name} repetition {number} had competing CPU load: mean "
                        f"{cpu['mean_background_cpu_fraction']:.1%}, max "
                        f"{cpu['max_background_cpu_fraction']:.1%}"
                    )
                print(
                    f"{name} repetition {number}: {result['elapsed_seconds']:.2f}s, background "
                    f"mean {cpu['mean_background_cpu_fraction']:.1%} "
                    f"max {cpu['max_background_cpu_fraction']:.1%}",
                    flush=True,
                )
            if source_manifest() != manifest:
                raise RuntimeError("benchmark sources changed during collection")
        report["completed"] = True
    finally:
        report["finished_utc"] = datetime.now(timezone.utc).isoformat()
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")


if __name__ == "__main__":
    main()
