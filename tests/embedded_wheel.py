"""Verify and install one self-contained wheel in an isolated environment."""
import argparse
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import venv
import zipfile


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("directory", type=Path)
    args = parser.parse_args()
    wheels = list(args.directory.glob("vlite_embedded-*.whl"))
    if len(wheels) != 1:
        raise RuntimeError("Expected exactly one embedded wheel in the directory")
    wheel = wheels[0].resolve()
    name = "vlite_ffi.dll" if sys.platform == "win32" else "libvlite_ffi.dylib" if sys.platform == "darwin" else "libvlite_ffi.so"
    with zipfile.ZipFile(wheel) as bundle:
        names = bundle.namelist()
        assert "vlite_embedded/" + name in names
        assert "vlite_embedded/py.typed" in names
        assert any(path.endswith("/licenses/LICENSE") for path in names)
        metadata = bundle.read(next(path for path in names if path.endswith(".dist-info/WHEEL"))).decode()
        assert "Root-Is-Purelib: false" in metadata
        assert "Tag: py3-none-" in metadata and "Tag: py3-none-any" not in metadata
    with tempfile.TemporaryDirectory(prefix="vlite-wheel-test-") as directory:
        directory = Path(directory)
        venv.EnvBuilder(with_pip=True).create(directory / "venv")
        python = directory / "venv" / ("Scripts/python.exe" if os.name == "nt" else "bin/python")
        subprocess.run([str(python), "-m", "pip", "install", "--no-deps", "--no-index", str(wheel)], check=True)
        code = """
import tempfile
from pathlib import Path
from vlite_embedded import NativeLibrary, EmbeddedError
native = NativeLibrary()
with tempfile.TemporaryDirectory() as directory:
    path = Path(directory) / 'wheel.vlite'
    with native.create(path, 2) as db:
        db.put('18446744073709551615', [1,0], payload_base64='AP8=')
        assert db.search([1,0],1)[0]['id']=='18446744073709551615'
        assert db.get_many(['0','18446744073709551615'])[0] is None
        db.build_index()
        assert db.search_approximate_ready([1,0],1)[0]['distance']==0
    with native.open(path) as db:
        assert db.get('18446744073709551615')['payload_base64']=='AP8='
print('PASS installed native wheel: automatic library loading, CRUD, search and reopen')
"""
        # -I excludes PYTHONPATH and cwd imports. No server or external DLL path.
        subprocess.run([str(python), "-I", "-c", code], cwd=directory, check=True, timeout=60)


if __name__ == "__main__":
    main()
