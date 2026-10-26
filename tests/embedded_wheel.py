"""Verify and install one self-contained wheel in an isolated environment."""
import argparse
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import struct
import venv
import zipfile


def verify_macos_architecture(binary, filename, metadata):
    assert binary[:4] == bytes.fromhex("cffaedfe"), "Expected a thin 64-bit Mach-O library"
    cpu = struct.unpack_from("<I", binary, 4)[0]
    architecture = {0x0100000C: "arm64", 0x01000007: "x86_64"}.get(cpu)
    assert architecture is not None, "Unsupported Mach-O architecture"
    assert filename.endswith("_" + architecture + ".whl"), "Wheel architecture differs from native library"
    assert f"_{architecture}\n" in metadata and "universal2" not in metadata


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("directory", type=Path)
    parser.add_argument("--platform-tag", help="Require an audited platform tag in filename and WHEEL metadata")
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
        if sys.platform == "darwin":
            # A universal Python interpreter does not make a thin Rust library universal.
            verify_macos_architecture(bundle.read("vlite_embedded/" + name), wheel.name, metadata)
        if args.platform_tag:
            assert wheel.name.endswith("-" + args.platform_tag + ".whl")
            assert f"Tag: py3-none-{args.platform_tag}\n" in metadata
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
        assert db.search([1,0],1,metric="cosine")[0]['distance']==0
        assert db.search([1,0],1,metric="inner_product")[0]['distance']==-1
        assert db.get_many(['0','18446744073709551615'])[0] is None
        db.build_index()
        assert db.search_approximate_ready([1,0],1)[0]['distance']==0
        db.build_index(metric="cosine")
        assert db.search_approximate_ready([1,0],1,metric="cosine")[0]['distance']==0
    with native.open(path) as db:
        assert db.get('18446744073709551615')['payload_base64']=='AP8='
    advanced = Path(directory) / 'collections.vlite'
    with native.create(advanced, 2, format=4) as db:
        db.create_collection('documents', 2)
        db.put('1', [1,0], collection='documents')
        try:
            db.get('1')
        except EmbeddedError as error:
            assert error.code == 'not_found'
        else:
            raise AssertionError('collection record leaked into default')
        assert db.get('1', collection='documents')['revision']
        assert db.search_auto([1,0], 1, collection='documents')['hits'][0]['id']=='1'
    with native.open(advanced) as db:
        assert db.get('1', collection='documents')['vector']==[1,0]
    # Release wheels ship the opt-in graph commands (GATE-008, v0.5.0).
    assert native.capabilities() == {'graph': True}
    with native.create_with_graph(Path(directory) / 'graph.vlite', 2) as db:
        assert db.is_graph_enabled()
        assert db.create_graph('g') == {'name': 'g', 'node_count': 0, 'edge_count': 0}
        assert db.list_graphs()[0]['name'] == 'g'
print('PASS installed native wheel: automatic loading, legacy and v4 collections, graph, CRUD, search and reopen')
"""
        # -I excludes PYTHONPATH and cwd imports. No server or external DLL path.
        subprocess.run([str(python), "-I", "-c", code], cwd=directory, check=True, timeout=60)


if __name__ == "__main__":
    main()
