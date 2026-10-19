import importlib.util
import json
from pathlib import Path
import tempfile
import unittest
import zipfile

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("candidate", ROOT / "bindings" / "release_candidate.py")
candidate = importlib.util.module_from_spec(spec)
spec.loader.exec_module(candidate)


class CandidateTests(unittest.TestCase):
    def test_manifest_coverage_tampering_and_paths(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "candidate.zip"
            manifest = {"schema_version": 1, "files": {"library": candidate.digest(b"native")}}
            for name, body, valid in [("library", b"native", True),
                                      ("library", b"tampered", False),
                                      ("../library", b"native", False),
                                      ("extra", b"native", False)]:
                with zipfile.ZipFile(path, "w") as bundle:
                    bundle.writestr(name, body)
                    bundle.writestr("manifest.json", json.dumps(manifest))
                if valid:
                    self.assertEqual(candidate.verify(path), manifest)
                else:
                    with self.assertRaises(ValueError):
                        candidate.verify(path)


if __name__ == "__main__":
    unittest.main()
