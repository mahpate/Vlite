"""Build platform-specific ctypes wheels from an explicitly supplied native artifact."""
import os
from pathlib import Path
import shutil
import sys

from setuptools import Distribution, setup
from setuptools.command.build_py import build_py
from setuptools.command.bdist_wheel import bdist_wheel


class NativeBuild(build_py):
    def run(self):
        source = os.environ.get("VLITE_NATIVE_LIBRARY")
        if not source:
            raise RuntimeError("Use bindings/build_wheel.py to build a native wheel; VLITE_NATIVE_LIBRARY is required")
        source = Path(source).resolve(strict=True)
        expected = "vlite_ffi.dll" if sys.platform == "win32" else "libvlite_ffi.dylib" if sys.platform == "darwin" else "libvlite_ffi.so"
        if source.name != expected:
            raise RuntimeError(f"Expected host-native artifact named {expected}")
        super().run()
        destination = Path(self.build_lib) / "vlite_embedded"
        for name in ("vlite_ffi.dll", "libvlite_ffi.so", "libvlite_ffi.dylib"):
            stale = destination / name
            if stale.exists():
                stale.unlink()
        shutil.copy2(source, destination / expected)


class NativeWheel(bdist_wheel):
    def finalize_options(self):
        super().finalize_options()
        self.root_is_pure = False

    def get_tag(self):
        _, _, platform = super().get_tag()
        # ctypes uses the C ABI, not CPython's extension ABI.
        return "py3", "none", platform


class NativeDistribution(Distribution):
    def has_ext_modules(self):
        return True


setup(distclass=NativeDistribution,
      cmdclass={"build_py": NativeBuild, "bdist_wheel": NativeWheel})
