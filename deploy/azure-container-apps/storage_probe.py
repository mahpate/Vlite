"""Fail closed before starting VLite on an experimental Azure Files mount."""

import os
from pathlib import Path
import subprocess
import sys
import tempfile


def probe(directory):
    import fcntl

    directory = Path(directory)
    if not directory.is_mount():
        raise RuntimeError(f"{directory} is not a mounted volume; refusing ephemeral storage")
    with tempfile.TemporaryDirectory(prefix=".vlite-probe-", dir=directory) as scratch:
        scratch = Path(scratch)
        source = scratch / "source"
        destination = scratch / "destination"
        with source.open("w+b") as handle:
            handle.write(b"vlite-storage-probe")
            handle.flush()
            os.fsync(handle.fileno())
            fcntl.flock(handle, fcntl.LOCK_EX | fcntl.LOCK_NB)
            child = """
import errno, fcntl, sys
blocked = sys.argv[2] == "blocked"
with open(sys.argv[1], "r+b") as handle:
    try:
        fcntl.flock(handle, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except OSError as error:
        if blocked and error.errno in (errno.EACCES, errno.EAGAIN):
            sys.exit(0)
        raise
if blocked:
    sys.exit("exclusive lock did not exclude a second process")
"""
            subprocess.run([sys.executable, "-c", child, str(source), "blocked"], check=True, timeout=30)
            fcntl.flock(handle, fcntl.LOCK_UN)
            subprocess.run([sys.executable, "-c", child, str(source), "available"], check=True, timeout=30)
        destination.write_bytes(b"old")
        os.replace(source, destination)
        if destination.read_bytes() != b"vlite-storage-probe":
            raise RuntimeError("atomic replacement did not expose the new file")
        fd = os.open(scratch, os.O_RDONLY | os.O_DIRECTORY)
        try:
            os.fsync(fd)
        finally:
            os.close(fd)
    fd = os.open(directory, os.O_RDONLY | os.O_DIRECTORY)
    try:
        os.fsync(fd)
    finally:
        os.close(fd)


if __name__ == "__main__":
    if os.getuid() == 0:
        os.setgroups([])
        os.setgid(10001)
        os.setuid(10001)
    probe("/data")
    print("VLite storage primitives passed as UID 10001; crash recovery remains unqualified.", flush=True)
