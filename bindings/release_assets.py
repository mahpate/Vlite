"""Verify release payloads and write a source-linked checksum manifest."""
import argparse
import hashlib
import json
from pathlib import Path
import re


def assemble(directory: Path, version: str, commit: str):
    if not re.fullmatch(r"\d+\.\d+\.\d+", version):
        raise ValueError("Expected a numeric release version")
    if not re.fullmatch(r"[0-9a-f]{40}", commit):
        raise ValueError("Expected a full source commit SHA")
    files = sorted(directory.iterdir())
    payloads = [path for path in files if path.name.endswith((".zip", ".whl", ".tgz"))]
    if not payloads:
        raise ValueError("No release payloads")
    expected = {path.name for path in payloads} | {path.name + ".sha256" for path in payloads}
    if {path.name for path in files} != expected or any(not path.is_file() for path in files):
        raise ValueError("Unexpected release files or missing sidecar checksums")
    assets = []
    for path in payloads:
        digest = hashlib.sha256(path.read_bytes()).hexdigest()
        if (directory / (path.name + ".sha256")).read_text(encoding="ascii") != f"{digest}  {path.name}\n":
            raise ValueError(f"Checksum mismatch: {path.name}")
        assets.append({"name": path.name, "bytes": path.stat().st_size, "sha256": digest})
    manifest = {"version": version, "source_commit": commit, "assets": assets}
    with (directory / "release-manifest.json").open("x", encoding="utf-8", newline="\n") as output:
        json.dump(manifest, output, indent=2)
        output.write("\n")
    with (directory / "SHA256SUMS").open("x", encoding="ascii", newline="\n") as output:
        for path in sorted(directory.iterdir()):
            if path.name != "SHA256SUMS":
                output.write(f"{hashlib.sha256(path.read_bytes()).hexdigest()}  {path.name}\n")
    return manifest


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("directory", type=Path)
    parser.add_argument("--version", required=True)
    parser.add_argument("--commit", required=True)
    args = parser.parse_args()
    assemble(args.directory, args.version, args.commit)
