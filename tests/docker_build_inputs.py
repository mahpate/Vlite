"""Keep Rust build-script and protobuf inputs available to container builds."""
from pathlib import Path
import unittest

ROOT = Path(__file__).resolve().parents[1]


class DockerInputs(unittest.TestCase):
    def test_build_stages_copy_codegen_inputs(self):
        for name in ("Dockerfile", "bindings/manylinux.Dockerfile", "bindings/node/Dockerfile"):
            with self.subTest(dockerfile=name):
                text = (ROOT / name).read_text(encoding="utf-8")
                self.assertIn("COPY build.rs ./", text)
                self.assertIn("COPY proto ./proto", text)
        ignores = (ROOT / ".dockerignore").read_text(encoding="utf-8").splitlines()
        for rule in ("!build.rs", "!proto/", "!proto/**"):
            self.assertIn(rule, ignores)


if __name__ == "__main__":
    unittest.main()
