"""Release manifest rejects incomplete or altered artifact sets."""
import hashlib
import importlib.util
from pathlib import Path
import tempfile
import unittest

spec = importlib.util.spec_from_file_location("assets", Path(__file__).resolve().parents[1] / "bindings" / "release_assets.py")
assets = importlib.util.module_from_spec(spec)
spec.loader.exec_module(assets)


class ReleaseAssets(unittest.TestCase):
    def test_verified_manifest(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            payload = directory / "vlite-test.zip"
            payload.write_bytes(b"release")
            digest = hashlib.sha256(b"release").hexdigest()
            payload.with_name(payload.name + ".sha256").write_text(f"{digest}  {payload.name}\n", encoding="ascii")
            result = assets.assemble(directory, "0.1.0", "a" * 40)
            self.assertEqual(result["source_commit"], "a" * 40)
            self.assertEqual(result["assets"], [{"name": payload.name, "bytes": 7, "sha256": digest}])
            for line in (directory / "SHA256SUMS").read_text().splitlines():
                expected, name = line.split("  ")
                self.assertEqual(expected, hashlib.sha256((directory / name).read_bytes()).hexdigest())
            with self.assertRaises(ValueError):
                assets.assemble(directory, "0.1.0", "a" * 40)

    def test_reject_missing_or_altered_checksum(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            payload = directory / "vlite-test.zip"
            payload.write_bytes(b"release")
            with self.assertRaises(ValueError):
                assets.assemble(directory, "0.1.0", "a" * 40)
            payload.with_name(payload.name + ".sha256").write_text("incorrect\n")
            with self.assertRaises(ValueError):
                assets.assemble(directory, "0.1.0", "a" * 40)

    def test_reject_bad_identity(self):
        with tempfile.TemporaryDirectory() as temporary:
            for version, commit in [("latest", "a" * 40), ("0.1.0", "short")]:
                with self.assertRaises(ValueError):
                    assets.assemble(Path(temporary), version, commit)


if __name__ == "__main__":
    unittest.main()
