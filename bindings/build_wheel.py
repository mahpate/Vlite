"""Build a host-native VLite wheel. No registry upload or cross-compilation."""
import argparse
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    root = Path(__file__).resolve().parents[1]
    output = args.output.resolve()
    env = dict(os.environ)
    # Use a known artifact directory, independent of user Cargo target-dir settings.
    with tempfile.TemporaryDirectory(prefix="vlite-wheel-") as temporary:
        temporary = Path(temporary)
        target = temporary / "target"
        env["CARGO_TARGET_DIR"] = str(target)
        if env.get("CARGO_BUILD_TARGET"):
            raise RuntimeError("Build wheels on the target host; CARGO_BUILD_TARGET must be unset")
        if sys.platform == "darwin":
            import sysconfig
            env.setdefault("MACOSX_DEPLOYMENT_TARGET", sysconfig.get_config_var("MACOSX_DEPLOYMENT_TARGET") or "11.0")
        subprocess.run(["cargo", "build", "--locked", "--release", "-p", "vlite-ffi",
                        "--manifest-path", str(root / "Cargo.toml")], env=env, check=True)
        name = "vlite_ffi.dll" if sys.platform == "win32" else "libvlite_ffi.dylib" if sys.platform == "darwin" else "libvlite_ffi.so"
        library = target / "release" / name
        # Validate that this host/interpreter can load the exact artifact before packaging.
        subprocess.run([sys.executable, "-c",
                        "import ctypes,sys; lib=ctypes.CDLL(sys.argv[1]); "
                        "lib.vlite_abi_version.restype=ctypes.c_uint32; "
                        "assert lib.vlite_abi_version()==1, 'Native artifact ABI mismatch'",
                        str(library)], check=True)
        source = temporary / "python"
        shutil.copytree(root / "bindings" / "python", source,
                        ignore=shutil.ignore_patterns("build", "dist", "*.egg-info", "__pycache__"))
        env["VLITE_NATIVE_LIBRARY"] = str(library)
        output.mkdir(parents=True, exist_ok=True)
        subprocess.run([sys.executable, "-m", "pip", "wheel", "--no-deps", "--wheel-dir",
                        str(output), str(source)], env=env, check=True)


if __name__ == "__main__":
    main()
