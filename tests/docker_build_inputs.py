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

    def test_release_images_build_only_the_shipped_binaries(self):
        dockerfile = (ROOT / "Dockerfile").read_text(encoding="utf-8")
        self.assertIn("ARG VLITE_IMAGE_BINS=all\n", dockerfile.replace("\r\n", "\n"))
        copied = re.findall(r"COPY --from=build /out/(\S+) ", dockerfile)
        shipped = re.search(r'shipped\) shipped="([^"]*)"', dockerfile)
        self.assertIsNotNone(shipped)
        self.assertEqual(re.findall(r"--bin (\S+)", shipped.group(1)), copied)
        self.assertIn("${shipped:---bins}", dockerfile)
        release = (ROOT / ".github/workflows/release.yml").read_text(encoding="utf-8")
        self.assertNotIn("setup-qemu-action", release)
        # Native per-architecture builds push by digest; image-publish merges them.
        self.assertIn("runner: ubuntu-24.04-arm", release)
        self.assertEqual(release.count("push-by-digest=true,name-canonical=true,push=true"), 3)
        self.assertEqual(release.count("VLITE_IMAGE_BINS=shipped"), 2)
        self.assertIn("docker buildx imagetools create", release)

    def test_build_cache_outputs_and_test_stage_are_independent(self):
        text = (ROOT / "Dockerfile").read_text(encoding="utf-8")
        test = text.split("FROM source AS test\n")[1].split("FROM source AS build\n")[0]
        self.assertNotIn("VLITE_IMAGE_TESTS", test)
        self.assertIn(
            "cargo nextest run --locked --workspace --all-targets --all-features --test-threads 2",
            test,
        )
        self.assertIn(
            "amd64) nextest_arch=x86_64; "
            "nextest_sha=32d1d8c0567b7a80bee2bad8bb09125c846da96bc009b2de11ec08557290c770",
            test,
        )
        self.assertIn(
            "arm64) nextest_arch=aarch64; "
            "nextest_sha=6f82e375187cf3a1f3e336879a51940c315a5d0ce7253f7e517ad28295b74189",
            test,
        )
        self.assertIn("cargo-nextest-0.9.149-", test)
        self.assertIn("sha256sum -c -", test)
        self.assertNotIn("cargo build", test)
        self.assertIn('CMD ["true"]', test)
        self.assertEqual(text.count("target=/app/target/debug,sharing=locked"), 2)
        self.assertEqual(text.count("target=/app/target/release,sharing=locked"), 1)
        self.assertIn("&& cp target/release/vlite-smoke", text)
        self.assertNotIn("COPY --from=build /app/target/", text)


if __name__ == "__main__":
    unittest.main()
