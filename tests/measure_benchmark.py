"""Run one release benchmark child and attach measured total peak resident memory."""

import ctypes
import json
import os
import subprocess
import sys


def run():
    if len(sys.argv) < 2:
        raise SystemExit("usage: python tests/measure_benchmark.py <vlite-bench executable> [arguments]")
    child = subprocess.Popen(sys.argv[1:], stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    if os.name == "nt":
        from ctypes import wintypes

        class Counters(ctypes.Structure):
            _fields_ = [
                ("cb", wintypes.DWORD), ("PageFaultCount", wintypes.DWORD),
                *[(name, ctypes.c_size_t) for name in [
                    "PeakWorkingSetSize", "WorkingSetSize", "QuotaPeakPagedPoolUsage",
                    "QuotaPagedPoolUsage", "QuotaPeakNonPagedPoolUsage",
                    "QuotaNonPagedPoolUsage", "PagefileUsage", "PeakPagefileUsage"
                ]]
            ]

        kernel = ctypes.WinDLL("kernel32", use_last_error=True)
        psapi = ctypes.WinDLL("psapi", use_last_error=True)
        kernel.OpenProcess.argtypes = [wintypes.DWORD, wintypes.BOOL, wintypes.DWORD]
        kernel.OpenProcess.restype = wintypes.HANDLE
        kernel.CloseHandle.argtypes = [wintypes.HANDLE]
        psapi.GetProcessMemoryInfo.argtypes = [wintypes.HANDLE, ctypes.POINTER(Counters), wintypes.DWORD]
        psapi.GetProcessMemoryInfo.restype = wintypes.BOOL
        handle = kernel.OpenProcess(0x0410, False, child.pid)
        if not handle:
            child.communicate()
            raise ctypes.WinError(ctypes.get_last_error())
        try:
            stdout, stderr = child.communicate()
            counters = Counters()
            counters.cb = ctypes.sizeof(counters)
            if not psapi.GetProcessMemoryInfo(handle, ctypes.byref(counters), counters.cb):
                raise ctypes.WinError(ctypes.get_last_error())
            peak = counters.PeakWorkingSetSize
            source = "GetProcessMemoryInfo.PeakWorkingSetSize"
        finally:
            kernel.CloseHandle(handle)
    else:
        import resource
        stdout, stderr = child.communicate()
        if sys.platform not in ("linux", "darwin"):
            raise RuntimeError("peak RSS measurement supports Windows, Linux and macOS only")
        peak = resource.getrusage(resource.RUSAGE_CHILDREN).ru_maxrss
        if sys.platform == "linux":
            peak *= 1024
        source = "getrusage(RUSAGE_CHILDREN).ru_maxrss"
    if child.returncode:
        raise RuntimeError(f"benchmark exited {child.returncode}: {stderr.decode()}")
    result = json.loads(stdout)
    if peak <= 0:
        raise RuntimeError("process peak resident memory was not available")
    result["process_peak_rss_bytes"] = peak
    result["process_peak_rss_source"] = source
    result["process_peak_rss_scope"] = "whole benchmark child including dataset, database, exact oracle, index and queries; not marginal index memory"
    print(json.dumps(result, separators=(",", ":")))


if __name__ == "__main__":
    run()
