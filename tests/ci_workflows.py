"""CI routing, qualification and cache wiring regressions (standard library only)."""
import importlib.util
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("ci_policy", ROOT / "scripts/ci_policy.py")
policy = importlib.util.module_from_spec(spec)
spec.loader.exec_module(policy)


class Policy(unittest.TestCase):
    def test_only_markdown_documentation_can_skip_native(self):
        self.assertTrue(policy.docs_only(["README.md", "docs/API.md"]))
        for paths in ([], [""], ["src/dashboard/app.js"], ["site/assets/theme.css"],
                      ["site/dashboard.html"], ["docs/example.rs"], ["Cargo.lock"],
                      [".github/workflows/ci.yml"], ["README.md", "tests/rest_graph.rs"]):
            with self.subTest(paths=paths):
                self.assertFalse(policy.docs_only(paths))

    def test_only_exact_successful_main_ci_is_trusted(self):
        run = dict(head_sha="abc", head_branch="main", event="push", conclusion="success")
        self.assertTrue(policy.trusted_run(run, "abc"))
        self.assertTrue(policy.trusted_run(dict(run, event="workflow_dispatch"), "abc"))
        for field, value in (("head_sha", "old"), ("head_branch", "feature"),
                             ("event", "pull_request"), ("conclusion", "failure")):
            self.assertFalse(policy.trusted_run(dict(run, **{field: value}), "abc"))

    def test_skipped_or_partial_matrix_cannot_qualify_an_image(self):
        jobs = [
            dict(name=f"native ({os}, {rust})", conclusion="success")
            for os in ("ubuntu-latest", "ubuntu-24.04-arm", "windows-latest", "macos-latest")
            for rust in ("1.89.0", "stable")
        ] + [dict(name=name, conclusion="success") for name in ("lint", "docker")]
        self.assertTrue(policy.full_ci_jobs(jobs))
        self.assertFalse(policy.full_ci_jobs(jobs[1:]))
        for conclusion in ("skipped", "cancelled", "failure"):
            self.assertFalse(policy.full_ci_jobs([dict(jobs[0], conclusion=conclusion)] + jobs[1:]))
        self.assertFalse(policy.full_ci_jobs(jobs[:-1]))
        self.assertFalse(policy.full_ci_jobs([dict(jobs[0], name="native (unknown, stable)")] + jobs[1:]))

    def test_manual_ci_always_forces_full_matrix(self):
        self.check_route("workflow_dispatch", {}, b"README.md\0", "true")

    def test_push_routing(self):
        for paths, result in ((b"README.md\0docs/API.md\0", "false"),
                              (b"src/dashboard/index.html\0", "true"),
                              (b"", "true")):
            self.check_route("push", {"before": "abc"}, paths, result)
        self.check_route("push", {"before": "0" * 40}, b"README.md\0", "true")

    def test_pr_routing_uses_base_sha(self):
        self.check_route("pull_request", {"pull_request": {"base": {"sha": "abc"}}},
                         b"README.md\0", "false")

    def check_route(self, kind, event, paths, expected):
        with tempfile.TemporaryDirectory() as directory:
            event_path, output_path = Path(directory) / "event.json", Path(directory) / "output"
            event_path.write_text(json.dumps(event), encoding="utf-8")
            env = dict(GITHUB_EVENT_PATH=str(event_path), GITHUB_OUTPUT=str(output_path),
                       GITHUB_EVENT_NAME=kind, GITHUB_SHA="head")
            with patch.dict(os.environ, env), patch.object(
                policy.subprocess, "check_output", return_value=paths
            ) as diff:
                policy.route()
            self.assertEqual(output_path.read_text(), f"full={expected}\n")
            if diff.called:
                self.assertEqual(diff.call_args.args[0], ["git", "diff", "--name-only", "-z", "abc", "head"])

    def test_image_gate_fails_closed_before_api_access_on_non_main(self):
        with patch.dict(os.environ, {"GITHUB_REF": "refs/heads/feature"}), patch.object(
            policy, "gh_pages"
        ) as api:
            with self.assertRaisesRegex(RuntimeError, "on main"):
                policy.image_ci()
            api.assert_not_called()

    def test_image_gate_rejects_missing_qualification(self):
        env = dict(GITHUB_REF="refs/heads/main", GITHUB_REPOSITORY="mahpate/Vlite", GITHUB_SHA="abc")
        for runs in ([], [dict(head_sha="abc", head_branch="main", event="pull_request",
                              conclusion="success")]):
            with patch.dict(os.environ, env), patch.object(policy, "gh_pages", return_value=runs):
                with self.assertRaisesRegex(RuntimeError, "No successful full"):
                    policy.image_ci()

    def test_image_gate_checks_the_qualified_run_attempt(self):
        run = dict(head_sha="abc", head_branch="main", event="push", conclusion="success",
                   id=123, run_attempt=2, html_url="https://github.com/mahpate/Vlite/actions/runs/123")
        with tempfile.TemporaryDirectory() as directory:
            summary = Path(directory) / "summary"
            env = dict(GITHUB_REF="refs/heads/main", GITHUB_REPOSITORY="mahpate/Vlite",
                       GITHUB_SHA="abc", GITHUB_STEP_SUMMARY=str(summary))
            with patch.dict(os.environ, env), patch.object(
                policy, "gh_pages", side_effect=[[run], ["jobs"]]
            ) as api, patch.object(policy, "full_ci_jobs", return_value=True) as qualify:
                policy.image_ci()
            qualify.assert_called_once_with(["jobs"])
            self.assertIn("/runs/123/attempts/2/jobs", api.call_args.args[0])
            self.assertIn(run["html_url"], summary.read_text())

    def test_rust_caches_are_isolated_and_fork_writes_disabled(self):
        text = (ROOT / ".github/workflows/ci.yml").read_text(encoding="utf-8")
        self.assertEqual(text.count("Swatinem/rust-cache@"), 6)
        self.assertEqual(text.count("save-if: ${{ github.event_name != 'pull_request' }}"), 6)
        self.assertIn("key: ${{ matrix.os }}-${{ matrix.toolchain }}", text)
        self.assertIn("cancel-in-progress: true", text)

    def test_image_gate_precedes_publish_and_explicitly_skips_retesting(self):
        text = (ROOT / ".github/workflows/deployment-image.yml").read_text(encoding="utf-8")
        self.assertLess(text.index("ci_policy.py image-ci"), text.index("docker/login-action"))
        self.assertIn("actions: read", text)
        self.assertIn("VLITE_IMAGE_TESTS=0", text)
        self.assertIn("buildkit-cache-dance@", text)
        self.assertIn("actions/cache@", text)
        self.assertIn("docker-ci-v2-release-${{ runner.arch }}-", text)

    def test_docker_cache_export_excludes_large_debug_test_binaries(self):
        for workflow in ("ci.yml", "deployment-image.yml"):
            text = (ROOT / ".github/workflows" / workflow).read_text(encoding="utf-8")
            self.assertIn('"target": "/app/target/release"', text)
            self.assertNotIn('"target": "/app/target"', text)
            self.assertNotIn('"target": "/app/target/debug"', text)

    def test_browser_checks_reuse_the_already_tested_runtime(self):
        text = (ROOT / ".github/workflows/ci.yml").read_text(encoding="utf-8")
        self.assertIn("docker build --target dashboard --tag vlite-dashboard-ui:test .", text)
        browser = text.split("- name: Verify dashboard with Chromium against Docker\n")[1]
        self.assertIn("VLITE_IMAGE: vlite-smoke:local", browser)
        self.assertIn("VLITE_E2E_NO_BUILD: \"1\"", browser)

    def test_graph_workspace_suite_only_repeats_unique_doctests(self):
        text = (ROOT / ".github/workflows/ci.yml").read_text(encoding="utf-8")
        self.assertIn("cargo test --locked --workspace --all-targets --all-features", text)
        self.assertIn("cargo test --locked -p vlite-graph --doc", text)
        self.assertNotIn("cargo test --locked -p vlite-graph\n", text)

    def test_client_ffi_builds_share_a_cached_target_directory(self):
        workflow = (ROOT / ".github/workflows/ci.yml").read_text(encoding="utf-8")
        self.assertIn(". -> target/node-default", workflow)
        self.assertNotIn("target/ffi-release", workflow)
        self.assertIn(
            "bindings/build_wheel.py --target-dir target/node-default --output embedded-wheels",
            workflow,
        )
        self.assertIn("../../target/node-default", workflow)
        self.assertIn("target/node-default/release/${{ runner.os", workflow)
        node = (ROOT / "bindings/node/build.mjs").read_text(encoding="utf-8")
        self.assertIn('join(root, "target", "node-default")', node)

    def test_windows_uses_pinned_bounded_nextest_without_dropping_targets(self):
        workflow = (ROOT / ".github/workflows/ci.yml").read_text(encoding="utf-8")
        self.assertIn("taiki-e/install-action@5488ac5a951645ce95062214902ac4b6bc1a1037", workflow)
        self.assertIn("tool: cargo-nextest@0.9.150", workflow)
        self.assertIn(
            "cargo nextest run --locked --workspace --all-targets --all-features --test-threads 2",
            workflow,
        )
        self.assertIn("cargo nextest run --locked --workspace --all-targets --test-threads 2", workflow)
        self.assertIn("cargo test --locked --workspace --all-targets --all-features", workflow)
        self.assertIn("run: cargo test --locked --workspace --all-targets\n", workflow)


if __name__ == "__main__":
    unittest.main()
