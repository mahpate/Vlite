"""Exercise the Azure mount probe without Azure credentials or a real share."""

import importlib.util
import errno
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location(
    "storage_probe", ROOT / "deploy" / "azure-container-apps" / "storage_probe.py"
)
PROBE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(PROBE)
# Before Python 3.12, WindowsPath overrides is_mount() to raise NotImplementedError,
# so patching the base Path class has no effect there; patch the concrete class instead.
CONCRETE_PATH = type(Path())


def mounted(value):
    return patch.object(CONCRETE_PATH, "is_mount", return_value=value)


class StorageProbeTests(unittest.TestCase):
    def run_child_with_lock_result(self, arguments, **kwargs):
        blocked = arguments[-1] == "blocked"
        fake = unittest.mock.MagicMock()
        if blocked or self.always_denied:
            fake.flock.side_effect = PermissionError(errno.EACCES, "SMB lock denied")
        with patch.dict("sys.modules", {"fcntl": fake}), patch.object(sys, "argv", arguments[2:]):
            try:
                exec(arguments[2], {})
            except SystemExit as result:
                if result.code != 0:
                    raise AssertionError(result.code)

    def test_missing_mount_fails_before_writing(self):
        with tempfile.TemporaryDirectory() as directory, mounted(False):
            # fcntl is Linux-only, so make its import available on Windows too.
            with patch.dict("sys.modules", {"fcntl": unittest.mock.MagicMock()}):
                with self.assertRaisesRegex(RuntimeError, "not a mounted volume"):
                    PROBE.probe(directory)
            self.assertEqual(list(Path(directory).iterdir()), [])

    @unittest.skipUnless(__import__("sys").platform.startswith("linux"), "Linux filesystem primitives")
    def test_real_primitives_and_cleanup(self):
        with tempfile.TemporaryDirectory() as directory, mounted(True):
            PROBE.probe(directory)
            self.assertEqual(list(Path(directory).iterdir()), [])

    @unittest.skipUnless(__import__("sys").platform.startswith("linux"), "Linux filesystem primitives")
    def test_sync_failure_is_not_ignored(self):
        with tempfile.TemporaryDirectory() as directory, mounted(True):
            with patch.object(PROBE.os, "fsync", side_effect=OSError("sync unsupported")):
                with self.assertRaisesRegex(OSError, "sync unsupported"):
                    PROBE.probe(directory)
            self.assertEqual(list(Path(directory).iterdir()), [])

    @unittest.skipUnless(__import__("sys").platform.startswith("linux"), "Linux filesystem primitives")
    def test_directory_sync_failure_is_not_ignored(self):
        real_sync = PROBE.os.fsync
        calls = 0

        def sync(fd):
            nonlocal calls
            calls += 1
            if calls == 2:
                raise OSError("directory sync unsupported")
            real_sync(fd)

        with tempfile.TemporaryDirectory() as directory, mounted(True):
            with patch.object(PROBE.os, "fsync", side_effect=sync):
                with self.assertRaisesRegex(OSError, "directory sync unsupported"):
                    PROBE.probe(directory)
            self.assertEqual(list(Path(directory).iterdir()), [])

    @unittest.skipUnless(sys.platform.startswith("linux"), "Linux filesystem primitives")
    def test_smb_access_denied_only_accepted_during_contention(self):
        self.always_denied = False
        with tempfile.TemporaryDirectory() as directory, mounted(True):
            with patch.object(PROBE.subprocess, "run", side_effect=self.run_child_with_lock_result):
                PROBE.probe(directory)
            self.assertEqual(list(Path(directory).iterdir()), [])

    @unittest.skipUnless(sys.platform.startswith("linux"), "Linux filesystem primitives")
    def test_unconditionally_denied_lock_fails(self):
        self.always_denied = True
        with tempfile.TemporaryDirectory() as directory, mounted(True):
            with patch.object(PROBE.subprocess, "run", side_effect=self.run_child_with_lock_result):
                with self.assertRaises(PermissionError):
                    PROBE.probe(directory)
            self.assertEqual(list(Path(directory).iterdir()), [])


if __name__ == "__main__":
    unittest.main()
