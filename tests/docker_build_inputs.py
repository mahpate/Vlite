"""Keep Rust build-script, protobuf and workspace inputs available to container builds."""
from pathlib import Path
import re
import tomllib
import unittest

ROOT = Path(__file__).resolve().parents[1]


class DockerInputs(unittest.TestCase):
    def test_dashboard_image_rewrites_every_embedded_asset(self):
        html = (ROOT / "src/dashboard/index.html").read_text(encoding="utf-8")
        dockerfile = (ROOT / "Dockerfile").read_text(encoding="utf-8")
        for attribute, asset in re.findall(r'(src|href)="/dashboard/([^"]+)"', html):
            with self.subTest(asset=asset):
                self.assertTrue((ROOT / "src/dashboard" / asset).is_file())
                self.assertIn(
                    f"""s|{attribute}="/dashboard/{asset}"|{attribute}="/{asset}"|""",
                    dockerfile,
                )

    def test_build_stages_copy_codegen_inputs(self):
        for name in ("Dockerfile", "bindings/manylinux.Dockerfile", "bindings/node/Dockerfile"):
            with self.subTest(dockerfile=name):
                text = (ROOT / name).read_text(encoding="utf-8")
                self.assertIn("COPY build.rs ./", text)
                self.assertIn("COPY proto ./proto", text)
        ignores = (ROOT / ".dockerignore").read_text(encoding="utf-8").splitlines()
        for rule in ("!build.rs", "!proto/", "!proto/**"):
            self.assertIn(rule, ignores)

    def test_build_stages_copy_workspace_members(self):
        # Cargo loads every workspace member, even when building a single package.
        members = tomllib.loads((ROOT / "Cargo.toml").read_text(encoding="utf-8"))["workspace"]["members"]
        for name in ("Dockerfile", "bindings/manylinux.Dockerfile", "bindings/node/Dockerfile"):
            lines = (ROOT / name).read_text(encoding="utf-8").splitlines()
            copied = [match.group(1) for line in lines if (match := re.fullmatch(r"COPY (\S+) \./\1", line.strip()))]
            for member in members:
                with self.subTest(dockerfile=name, member=member):
                    self.assertTrue(any(member == path or member.startswith(path + "/") for path in copied))
        ignores = (ROOT / ".dockerignore").read_text(encoding="utf-8").splitlines()
        for member in members:
            parts = member.split("/")
            prefixes = ["/".join(parts[:end]) for end in range(1, len(parts) + 1)]
            with self.subTest(dockerignore=member):
                self.assertTrue(any(
                    f"!{prefix}/**" in ignores and all(f"!{parent}/" in ignores for parent in prefixes[:end])
                    for end, prefix in enumerate(prefixes, 1)))

    def test_build_stages_copy_explicit_cargo_targets(self):
        manifest = tomllib.loads((ROOT / "Cargo.toml").read_text(encoding="utf-8"))
        for name in ("Dockerfile", "bindings/manylinux.Dockerfile", "bindings/node/Dockerfile"):
            lines = (ROOT / name).read_text(encoding="utf-8").splitlines()
            copied = [match.group(1) for line in lines if (match := re.fullmatch(r"COPY (\S+) \./\1", line.strip()))]
            for kind, directory in (("example", "examples"), ("test", "tests")):
                for target in manifest.get(kind, []):
                    path = target.get("path", f"{directory}/{target['name']}.rs")
                    with self.subTest(dockerfile=name, target=path):
                        self.assertTrue((ROOT / path).is_file())
                        self.assertTrue(any(path == source or path.startswith(source + "/") for source in copied))


if __name__ == "__main__":
    unittest.main()
