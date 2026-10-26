"""Shared helpers for the separate-host DO-005 graph-off campaign (stdlib only, Windows).

Nothing here changes the CPU acceptance policy; the policy and the sampling
implementation come from the pinned scripts/graph_reference.py (see
do005_campaign.load_monitor).
"""

from __future__ import annotations

import base64
import ctypes
import hashlib
import json
import os
import re
import struct
import subprocess
import sys
from datetime import datetime, timezone
from pathlib import Path

CLONE = Path(r"Q:\vlite")
TOOLCHAIN = Path(r"Q:\vlite-campaign\toolchain")
RUSTUP_TOOLCHAIN = "1.99.1-x86_64-pc-windows-msvc"
# CreateProcess resolves the executable with the parent's PATH, so the tools are always named by absolute path.
CARGO = str(TOOLCHAIN / "cargo" / "bin" / "cargo.exe")
RUSTC = str(TOOLCHAIN / "cargo" / "bin" / "rustc.exe")
RUSTUP = str(TOOLCHAIN / "cargo" / "bin" / "rustup.exe")
LABELS = ("hist", "cur")
LABEL_NAMES = {"hist": "historical", "cur": "current origin/main"}


def now_utc() -> str:
    return datetime.now(timezone.utc).isoformat(timespec="milliseconds")


def sha256_bytes(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def sha256_file(path) -> str:
    digest = hashlib.sha256()
    with Path(path).open("rb") as handle:
        for chunk in iter(lambda: handle.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


def write_json(path, value) -> None:
    Path(path).write_text(json.dumps(value, indent=2) + "\n", encoding="utf-8")


def read_json(path):
    return json.loads(Path(path).read_text(encoding="utf-8-sig"))


def git(*args, cwd=CLONE, check=True) -> str:
    result = subprocess.run(["git", *args], cwd=cwd, capture_output=True, text=True, encoding="utf-8", errors="replace")
    if check and result.returncode != 0:
        raise RuntimeError(f"git {' '.join(args)} failed ({result.returncode}): {result.stderr.strip()}")
    return result.stdout.strip()


def toolchain_env(online: bool) -> dict:
    """One fixed Rust toolchain for every cargo/rustc call; inherited Rust/Cargo settings are dropped."""
    env = {k: v for k, v in os.environ.items() if not re.match(r"^(CARGO|RUSTUP|RUSTC|RUSTDOC|RUSTFLAGS|RUST_|VLITE_)", k, re.I)}
    env.update(
        CARGO_HOME=str(TOOLCHAIN / "cargo"),
        RUSTUP_HOME=str(TOOLCHAIN / "rustup"),
        RUSTUP_TOOLCHAIN=RUSTUP_TOOLCHAIN,
        RUSTUP_AUTO_INSTALL="0",
        CARGO_INCREMENTAL="0",
        CARGO_TERM_COLOR="never",
    )
    env["PATH"] = os.pathsep.join([str(TOOLCHAIN / "cargo" / "bin"), str(TOOLCHAIN / "python"), env.get("PATH", "")])
    if not online:
        env["CARGO_NET_OFFLINE"] = "true"
    return env


def dropped_env_names() -> list[str]:
    return sorted(k for k in os.environ if re.match(r"^(CARGO|RUSTUP|RUSTC|RUSTDOC|RUSTFLAGS|RUST_|VLITE_)", k, re.I))


def powershell(script: str, timeout: int = 240):
    """Runs a Windows PowerShell script; returns (parsed JSON or None, raw stdout, stderr)."""
    body = "[Console]::OutputEncoding=[Text.Encoding]::UTF8;$ProgressPreference='SilentlyContinue';" + script
    encoded = base64.b64encode(body.encode("utf-16-le")).decode("ascii")
    result = subprocess.run(
        ["powershell.exe", "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-EncodedCommand", encoded],
        capture_output=True, text=True, encoding="utf-8", errors="replace", timeout=timeout,
    )
    text = result.stdout.strip()
    try:
        return (json.loads(text) if text else None), text, result.stderr.strip()
    except json.JSONDecodeError:
        return None, text, result.stderr.strip()


def native(command: list[str], timeout: int = 60) -> dict:
    try:
        result = subprocess.run(command, capture_output=True, timeout=timeout)
    except (OSError, subprocess.SubprocessError) as error:
        return {"command": command, "error": str(error)}
    def decode(data: bytes) -> str:
        text = data.decode("utf-16-le", errors="replace") if data[:2] in (b"\xff\xfe",) or (len(data) > 3 and data[1:2] == b"\x00") else data.decode("utf-8", errors="replace")
        return text.replace("\x00", "").strip()
    return {"command": command, "returncode": result.returncode, "stdout": decode(result.stdout), "stderr": decode(result.stderr)}


# --- Windows process CPU census (diagnostics only; never part of the acceptance policy) ---------

_NT = None


def _nt():
    global _NT
    if _NT is None:
        ntdll = ctypes.WinDLL("ntdll")
        ntdll.NtQuerySystemInformation.argtypes = [ctypes.c_ulong, ctypes.c_void_p, ctypes.c_ulong, ctypes.POINTER(ctypes.c_ulong)]
        ntdll.NtQuerySystemInformation.restype = ctypes.c_long
        _NT = ntdll
    return _NT


def process_snapshot() -> dict:
    """pid/create-time keyed cumulative CPU seconds of every process (SystemProcessInformation)."""
    ntdll = _nt()
    size = 1 << 21
    while True:
        buffer = ctypes.create_string_buffer(size)
        needed = ctypes.c_ulong(0)
        status = ntdll.NtQuerySystemInformation(5, buffer, size, ctypes.byref(needed)) & 0xFFFFFFFF
        if status == 0:
            break
        if status == 0xC0000004:
            size = max(size * 2, needed.value + (1 << 16))
            continue
        raise OSError(f"NtQuerySystemInformation failed: {status:#x}")
    base = ctypes.addressof(buffer)
    raw = buffer.raw
    offset, processes = 0, {}
    while True:
        next_offset = struct.unpack_from("<I", raw, offset)[0]
        create, user, kernel = struct.unpack_from("<qqq", raw, offset + 32)
        name_length = struct.unpack_from("<H", raw, offset + 56)[0]
        name_pointer = struct.unpack_from("<Q", raw, offset + 64)[0]
        pid = struct.unpack_from("<Q", raw, offset + 80)[0]
        start = name_pointer - base
        name = raw[start:start + name_length].decode("utf-16-le", errors="replace") if name_length and 0 <= start < len(raw) else ""
        if pid == 0:
            name = "System Idle Process"
        processes[(pid, create)] = {"name": name, "cpu_seconds": (user + kernel) / 10_000_000}
        if next_offset == 0:
            break
        offset += next_offset
    return {"taken_utc": now_utc(), "monotonic": __import__("time").perf_counter(), "processes": processes}


def top_cpu_contributors(before: dict, after: dict, limit: int = 12, ignore_pids=(), logical_cpus: int | None = None) -> dict:
    """CPU seconds consumed between two snapshots, summed by image name (processes that exited in between are not seen)."""
    totals: dict[str, float] = {}
    for key, now in after["processes"].items():
        if key[0] == 0 or key[0] in ignore_pids:
            continue
        then = before["processes"].get(key)
        used = now["cpu_seconds"] - (then["cpu_seconds"] if then else 0.0)
        if used > 0:
            totals[now["name"]] = totals.get(now["name"], 0.0) + used
    elapsed = max(after["monotonic"] - before["monotonic"], 1e-9)
    ranked = sorted(totals.items(), key=lambda item: item[1], reverse=True)[:limit]
    return {
        "interval_seconds": elapsed,
        "logical_cpus": logical_cpus,
        "top": [{"image": name, "cpu_seconds": round(seconds, 4), "share_of_all_cpus": round(seconds / (elapsed * logical_cpus), 5) if logical_cpus else None} for name, seconds in ranked],
        "total_cpu_seconds": round(sum(totals.values()), 4),
        "note": "processes that started and exited between the two snapshots are not visible",
    }


def global_memory() -> dict:
    class Status(ctypes.Structure):
        _fields_ = [("length", ctypes.c_ulong), ("load", ctypes.c_ulong)] + [(name, ctypes.c_ulonglong) for name in ("total", "avail", "page_total", "page_avail", "virt_total", "virt_avail", "ext")]

    status = Status()
    status.length = ctypes.sizeof(Status)
    if not ctypes.windll.kernel32.GlobalMemoryStatusEx(ctypes.byref(status)):
        raise ctypes.WinError()
    return {"ram_bytes": status.total, "available_bytes": status.avail, "pagefile_total_bytes": status.page_total, "pagefile_available_bytes": status.page_avail}


def log(message: str) -> None:
    print(f"[{now_utc()}] {message}", flush=True)


def require_windows() -> None:
    if os.name != "nt":
        raise SystemExit("the DO-005 CPU monitor is Windows-specific; measurements from another platform would be a protocol change")


if __name__ == "__main__":
    sys.exit("library module")
