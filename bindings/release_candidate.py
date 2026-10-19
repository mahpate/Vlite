"""Assemble and verify a local embedded release candidate; never publishes."""
import argparse
import hashlib
import io
import json
from pathlib import Path
import platform
import subprocess
import sys
import tempfile
import tomllib
import zipfile

ROOT = Path(__file__).resolve().parents[1]


def digest(data):
    return hashlib.sha256(data).hexdigest()


def verify(archive):
    """Validate names, manifest coverage and every payload checksum before use."""
    with zipfile.ZipFile(archive) as bundle:
        names = bundle.namelist()
        if len(names) != len(set(names)) or "manifest.json" not in names:
            raise ValueError("Duplicate archive entries or missing manifest")
        for name in names:
            if name.startswith(("/", "\\")) or "\\" in name or ":" in name or ".." in name.split("/"):
                raise ValueError("Unsafe archive path")
        manifest = json.loads(bundle.read("manifest.json"))
        if manifest.get("schema_version") != 1:
            raise ValueError("Unsupported candidate manifest")
        if set(names) != set(manifest["files"]) | {"manifest.json"}:
            raise ValueError("Manifest does not cover the archive exactly")
        for name, expected in manifest["files"].items():
            if digest(bundle.read(name)) != expected:
                raise ValueError("Checksum mismatch: " + name)
        return manifest


def assemble(wheel, output):
    version = tomllib.loads((ROOT / "Cargo.toml").read_text())["package"]["version"]
    wheel = wheel.resolve(strict=True)
    if not wheel.name.startswith(f"vlite_embedded-{version}-py3-none-") or wheel.name.endswith("-any.whl"):
        raise ValueError("Expected a matching versioned host-native embedded wheel")
    library = "vlite_ffi.dll" if sys.platform == "win32" else "libvlite_ffi.dylib" if sys.platform == "darwin" else "libvlite_ffi.so"
    with zipfile.ZipFile(wheel) as bundle:
        native = bundle.read("vlite_embedded/" + library)
    # Verify the exact installed wheel, not a different source-tree library.
    with tempfile.TemporaryDirectory(prefix="vlite-candidate-check-") as temporary:
        directory = Path(temporary)
        (directory / wheel.name).write_bytes(wheel.read_bytes())
        subprocess.run([sys.executable, str(ROOT / "tests" / "embedded_wheel.py"), str(directory)], check=True)
    files = {
        wheel.name: wheel.read_bytes(),
        library: native,
        "LICENSE": (ROOT / "LICENSE").read_bytes(),
        "vlite.h": (ROOT / "bindings" / "c" / "vlite.h").read_bytes(),
        "EMBEDDED.md": (ROOT / "bindings" / "README.md").read_bytes(),
        "REFERENCE.md": (ROOT / "docs" / "REFERENCE.md").read_bytes(),
        "INSTALL.txt": (
            f"VLite {version} embedded release candidate (not a stable release).\n\n"
            f"Install: python -m pip install --no-index --no-deps {wheel.name}\n"
            "Then: from vlite_embedded import NativeLibrary; native = NativeLibrary()\n"
            "The wheel bundles the native library; no Rust or server is needed at runtime.\n"
            "Run on the matching OS/architecture/runtime. Linux wheels are host-specific,\n"
            "not manylinux-certified. Do not load untrusted native code.\n"
            "The standalone library/header support dynamic C ABI loading.\n"
            "Windows static import linking requires generating an import library separately.\n"
            "Keep one owner per database file. Back up before upgrades.\n"
            "This candidate passed isolated wheel installation and CRUD/search/reopen checks\n"
            "on its build host. This is not cross-platform certification or a signature.\n"
            "See https://github.com/mahpate/Vlite/blob/main/bindings/README.md for the guide.\n"
        ).encode(),
    }
    sources = ["Cargo.toml", "Cargo.lock", "src/lib.rs", "src/format.rs",
               "src/hnsw.rs", "src/metadata.rs", "bindings/c/src/lib.rs",
               "bindings/c/Cargo.toml", "bindings/c/vlite.h",
               "bindings/python/setup.py", "bindings/python/pyproject.toml",
               "bindings/python/vlite_embedded/__init__.py", "bindings/build_wheel.py",
               "tests/embedded_wheel.py", "bindings/release_candidate.py"]
    manifest = {
        "schema_version": 1, "version": version, "kind": "release-candidate",
        "build_host": {"platform": platform.platform(), "machine": platform.machine(),
                       "python": platform.python_version()},
        "validation": "isolated installed-wheel CRUD, search, index, multi-get and reopen",
        "source_hashes": {name: digest((ROOT / name).read_bytes()) for name in sources},
        "files": {name: digest(data) for name, data in files.items()},
        "provenance_note": "Source hashes describe the packaging checkout, not proof of wheel build origin; CI must build the wheel from the same checkout.",
    }
    output = output.resolve()
    # Exclusive publication prevents replacement of an existing candidate.
    stream = io.BytesIO()
    with zipfile.ZipFile(stream, "w", zipfile.ZIP_DEFLATED) as bundle:
        for name, data in files.items():
            bundle.writestr(name, data)
        bundle.writestr("manifest.json", json.dumps(manifest, indent=2) + "\n")
    stream.seek(0)
    verify(stream)
    with output.open("xb") as destination:
        destination.write(stream.getvalue())
    print(f"Verified candidate: {output}\nSHA256 {digest(output.read_bytes())}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--wheel", type=Path)
    parser.add_argument("--output", type=Path)
    parser.add_argument("--verify", type=Path)
    args = parser.parse_args()
    if args.verify is not None:
        if args.wheel or args.output:
            parser.error("--verify cannot be combined with --wheel/--output")
        verify(args.verify)
        print("PASS candidate checksums and manifest coverage")
    else:
        if not args.wheel or not args.output:
            parser.error("--wheel and --output are required")
        if args.output.exists():
            parser.error("output must be a new file")
        assemble(args.wheel, args.output)


if __name__ == "__main__":
    main()
