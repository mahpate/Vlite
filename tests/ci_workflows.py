"""CI routing, qualification and cache wiring regressions (standard library only)."""
import contextlib
import importlib.util
import io
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile
import textwrap
import unittest
from unittest.mock import Mock, patch

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("ci_policy", ROOT / "scripts/ci_policy.py")
policy = importlib.util.module_from_spec(spec)
spec.loader.exec_module(policy)
spec = importlib.util.spec_from_file_location("ci_test_proof", ROOT / "scripts/ci_test_proof.py")
proof = importlib.util.module_from_spec(spec)
spec.loader.exec_module(proof)


def qualified_jobs():
    """The jobs of a full CI run that qualifies an image, all green."""
    return (
        [dict(name=f"native ({os}, {rust})", conclusion="success")
         for os in ("ubuntu-latest", "ubuntu-24.04-arm", "windows-latest", "macos-latest")
         for rust in ("1.89.0", "stable")]
        + [dict(name=f"windows-tests ({rust}, shard {shard}/3)", conclusion="success")
           for rust in policy.WINDOWS_TOOLCHAINS for shard in (1, 2, 3)]
        + [dict(name=name, conclusion="success") for name in ("lint", "docker", "default-features")]
    )


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
        jobs = qualified_jobs()
        self.assertTrue(policy.full_ci_jobs(jobs))
        self.assertFalse(policy.full_ci_jobs(jobs[1:]))
        for conclusion in ("skipped", "cancelled", "failure"):
            self.assertFalse(policy.full_ci_jobs([dict(jobs[0], conclusion=conclusion)] + jobs[1:]))
        self.assertFalse(policy.full_ci_jobs(jobs[:-1]))
        for name in ("lint", "docker", "default-features"):
            with self.subTest(job=name):
                self.assertFalse(policy.full_ci_jobs([job for job in jobs if job["name"] != name]))
        self.assertFalse(policy.full_ci_jobs([dict(jobs[0], name="native (unknown, stable)")] + jobs[1:]))

    def test_every_windows_test_shard_must_succeed_to_qualify_an_image(self):
        jobs = qualified_jobs()
        shards = [job for job in jobs if job["name"].startswith("windows-tests (")]
        self.assertEqual(len(shards), len(policy.WINDOWS_TOOLCHAINS) * policy.WINDOWS_SHARDS)
        for shard in shards:
            with self.subTest(shard=shard["name"]):
                rest = [job for job in jobs if job is not shard]
                self.assertFalse(policy.full_ci_jobs(rest))
                for conclusion in ("skipped", "cancelled", "failure"):
                    self.assertFalse(policy.full_ci_jobs(rest + [dict(shard, conclusion=conclusion)]))
        wrong_total = dict(shards[0], name="windows-tests (stable, shard 1/2)")
        self.assertFalse(policy.full_ci_jobs([wrong_total] + [j for j in jobs if j is not shards[0]]))
        self.assertFalse(policy.full_ci_jobs(jobs + [dict(shards[0], name="windows-tests (stable, shard 4/3)")]))
        # The MSRV no longer runs Windows buckets: such a job is not part of the expected set.
        self.assertFalse(policy.full_ci_jobs(jobs + [dict(shards[0], name="windows-tests (1.89.0, shard 1/3)")]))

    def test_manual_ci_always_forces_full_matrix(self):
        self.check_route("workflow_dispatch", {}, b"README.md\0", "true", "true")

    def test_manual_ci_can_leave_out_only_the_docker_job(self):
        self.check_route("workflow_dispatch", {}, b"", "true", "false", CI_DOCKER_INPUT="false")
        self.check_route("workflow_dispatch", {}, b"", "true", "true", CI_DOCKER_INPUT="true")
        self.check_route("workflow_dispatch", {}, b"", "true", "true", CI_DOCKER_INPUT="")
        # Only a manual run reads the input: a push or pull request cannot opt out of the Docker job.
        self.check_route("push", {"before": "abc"}, b"Dockerfile\0", "true", "true", CI_DOCKER_INPUT="false")

    def test_push_routing(self):
        for paths, full, docker in ((b"README.md\0docs/API.md\0", "false", "false"),
                                    (b"src/dashboard/index.html\0", "true", "false"),
                                    (b"src/lib.rs\0tests/server_kv.rs\0Cargo.lock\0", "true", "false"),
                                    (b"src/lib.rs\0compose.yaml\0", "true", "true"),
                                    (b"", "true", "false")):
            with self.subTest(paths=paths):
                self.check_route("push", {"before": "abc"}, paths, full, docker)
        # Without a base the changes are unknown, so everything runs.
        self.check_route("push", {"before": "0" * 40}, b"README.md\0", "true", "true")

    def test_pr_routing_uses_base_sha(self):
        pull_request = {"pull_request": {"base": {"sha": "abc"}}}
        self.check_route("pull_request", pull_request, b"README.md\0", "false", "false")
        self.check_route("pull_request", pull_request, b"src/server.rs\0", "true", "false")
        self.check_route("pull_request", pull_request, b"deploy/helm/vlite/Chart.yaml\0", "true", "true")

    def test_docker_job_follows_the_container_files(self):
        for path in ("Dockerfile", "compose.yaml", "compose.tls.yaml", ".dockerignore",
                     "deploy/helm/vlite/values.yaml", "deploy/dashboard.conf",
                     "deploy/azure-container-apps/deploy.ps1", "scripts/docker-dashboard.sh",
                     "tests/docker_setup.py", ".github/workflows/ci.yml",
                     "tests/rest_e2e.py", "tests/tls_e2e.py", "tests/dashboard_e2e.py",
                     "tests/requirements-dashboard.txt"):
            with self.subTest(path=path):
                self.assertTrue(policy.touches_docker(["src/lib.rs", path]))
        for path in ("src/lib.rs", "src/server.rs", "Cargo.lock", "README.md", "tests/rest_graph.rs",
                     "scripts/ci_policy.py", "bindings/manylinux.Dockerfile", "bindings/node/Dockerfile",
                     "docs/DOCKER-OPERATIONS.md", ".github/workflows/release.yml",
                     ".github/workflows/deployment-image.yml", "deployments/x.yaml", "my-compose.yaml",
                     "tests/docker_build_inputs.py", ""):
            with self.subTest(path=path):
                self.assertFalse(policy.touches_docker([path]))
        self.assertFalse(policy.touches_docker([]))

    def check_route(self, kind, event, paths, expected, docker="false", **env):
        with tempfile.TemporaryDirectory() as directory:
            event_path, output_path = Path(directory) / "event.json", Path(directory) / "output"
            event_path.write_text(json.dumps(event), encoding="utf-8")
            env = dict(env, GITHUB_EVENT_PATH=str(event_path), GITHUB_OUTPUT=str(output_path),
                       GITHUB_EVENT_NAME=kind, GITHUB_SHA="head")
            with patch.dict(os.environ, env), patch.object(
                policy.subprocess, "check_output", return_value=paths
            ) as diff:
                policy.route()
            self.assertEqual(output_path.read_text(), f"full={expected}\ndocker={docker}\n")
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
        self.assertEqual(text.count("Swatinem/rust-cache@"), 8)
        # Pull requests never write a cache; the sharded Windows job lets only its first bucket write.
        self.assertEqual(text.count("save-if: ${{ github.event_name != 'pull_request' }}"), 7)
        self.assertEqual(
            text.count("save-if: ${{ github.event_name != 'pull_request' && matrix.shard == 1 }}"), 1
        )
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

    def test_docker_job_is_routed_by_its_own_output(self):
        text = (ROOT / ".github/workflows/ci.yml").read_text(encoding="utf-8")
        header, _ = text.split("\njobs:\n", 1)
        self.assertIn("  workflow_dispatch:\n    inputs:\n      docker:\n", header)
        self.assertIn("        type: boolean\n        default: true\n", header)
        changes = text.split("\n  changes:\n", 1)[1].split("\n  documentation:\n", 1)[0]
        self.assertIn("      docker: ${{ steps.route.outputs.docker }}\n", changes)
        self.assertIn("          CI_DOCKER_INPUT: ${{ inputs.docker }}\n", changes)
        docker = text.split("\n  docker:\n", 1)[1].split("\n  go-client:\n", 1)[0]
        self.assertIn("    if: needs.changes.outputs.docker == 'true'\n", docker)
        # Every other full-matrix job keeps following `full`.
        for name, after in (("native", "windows-tests"), ("windows-tests", "clients"),
                            ("manylinux", "node-linux-package"), ("node-linux-package", "lint"),
                            ("lint", "docker"), ("go-client", "dotnet-client"),
                            ("dotnet-client", "python-integrations"), ("wasm", "default-features"),
                            ("default-features", "required")):
            job = text.split(f"\n  {name}:\n", 1)[1].split(f"\n  {after}:\n", 1)[0]
            self.assertIn("    if: needs.changes.outputs.full == 'true'\n", job)

    def required_check(self, full, docker, overrides=None):
        """Run the CI required job's own script against a made-up `needs` context."""
        text = (ROOT / ".github/workflows/ci.yml").read_text(encoding="utf-8")
        required = text.split("\n  required:\n", 1)[1]
        needs = re.search(r"needs: \[(.*?)\]", required).group(1).split(", ")
        self.assertIn("windows-tests", needs)
        self.assertIn("default-features", needs)
        always = {"changes", "documentation", "clients", "python-integrations", "helm-chart"}
        jobs = {}
        for name in needs:
            if name in always:
                result = "success"
            elif name == "docker":
                result = "success" if docker == "true" else "skipped"
            else:
                result = "success" if full == "true" else "skipped"
            jobs[name] = {"result": result, "outputs": {}}
        jobs["changes"]["outputs"] = {"full": full, "docker": docker}
        for name, result in (overrides or {}).items():
            jobs[name]["result"] = result
        script = textwrap.dedent(required.split("        run: |\n", 1)[1])
        with patch.dict(os.environ, {"RESULTS": json.dumps(jobs)}):
            exec(compile(script, "ci-required", "exec"), {})

    def test_required_check_expects_the_docker_job_only_when_it_was_routed(self):
        self.required_check("true", "true")
        self.required_check("true", "false")
        self.required_check("false", "false")
        for full, docker, overrides in (
            ("true", "true", {"docker": "skipped"}),
            ("true", "true", {"docker": "failure"}),
            ("true", "true", {"windows-tests": "failure"}),
            ("true", "true", {"default-features": "failure"}),
            ("true", "true", {"default-features": "skipped"}),
            ("true", "true", {"native": "skipped"}),
            ("true", "false", {"docker": "success"}),
            ("true", "false", {"docker": "failure"}),
            ("false", "false", {"native": "success"}),
            ("false", "false", {"windows-tests": "success"}),
            ("false", "false", {"default-features": "success"}),
            ("false", "false", {"documentation": "skipped"}),
            ("false", "true", {}),
            ("maybe", "true", {}),
            ("true", "", {}),
        ):
            with self.subTest(full=full, docker=docker, overrides=overrides):
                with self.assertRaises(AssertionError):
                    self.required_check(full, docker, overrides)

    def test_browser_checks_reuse_the_already_tested_runtime(self):
        text = (ROOT / ".github/workflows/ci.yml").read_text(encoding="utf-8")
        self.assertIn("docker build --target dashboard --tag vlite-dashboard-ui:test .", text)
        browser = text.split("- name: Verify dashboard with Chromium against Docker\n")[1]
        self.assertIn("VLITE_IMAGE: vlite-smoke:local", browser)
        self.assertIn("VLITE_E2E_NO_BUILD: \"1\"", browser)

    def test_native_x86_64_jobs_require_avx2(self):
        text = (ROOT / ".github/workflows/ci.yml").read_text(encoding="utf-8")
        native = text.split("\n  native:\n", 1)[1].split("\n  windows-tests:\n", 1)[0]
        self.assertIn(
            "    env:\n"
            "      VLITE_REQUIRE_AVX2: ${{ (matrix.os == 'ubuntu-latest' || "
            "matrix.os == 'windows-latest') && '1' || '0' }}\n",
            native,
        )
        windows = text.split("\n  windows-tests:\n", 1)[1].split("\n  clients:\n", 1)[0]
        self.assertIn("    env:\n      VLITE_REQUIRE_AVX2: \"1\"\n", windows)
        default_features = text.split("\n  default-features:\n", 1)[1].split("\n  required:\n", 1)[0]
        self.assertIn("    runs-on: ubuntu-latest\n    env:\n      VLITE_REQUIRE_AVX2: \"1\"\n", default_features)
        self.assertEqual(text.count("VLITE_REQUIRE_AVX2:"), 3)

    def test_windows_suite_is_split_into_exact_hash_buckets(self):
        text = (ROOT / ".github/workflows/ci.yml").read_text(encoding="utf-8")
        job = text.split("\n  windows-tests:\n", 1)[1].split("\n  clients:\n", 1)[0]
        shards = policy.WINDOWS_SHARDS
        self.assertEqual(shards, 3)
        # One runner per bucket, buckets 1..N of N once each; stable only (the MSRV is checked in `native`).
        self.assertIn(f"        shard: {list(range(1, shards + 1))}\n", job)
        self.assertIn("        toolchain: [stable]\n", job)
        self.assertIn("shard ${{ matrix.shard }}/" + str(shards) + ")", job)
        self.assertEqual(job.count("--partition hash:${{ matrix.shard }}/" + str(shards)), 1)
        self.assertEqual(job.count("cargo nextest run"), 1)
        self.assertIn("--workspace --all-targets --all-features --test-threads 2 --partition", job)
        self.assertIn("fail-fast: false", job)
        # Only the cache of bucket 1 is written: three identical uploads would race for one key.
        self.assertIn("save-if: ${{ github.event_name != 'pull_request' && matrix.shard == 1 }}", job)
        # The suite is not repeated in `native`: its Windows cells keep only the small steps.
        native = text.split("\n  native:\n", 1)[1].split("\n  windows-tests:\n", 1)[0]
        for step in native.split("\n      - ")[1:]:
            command = " ".join(line for line in step.splitlines() if line.strip().startswith("run:"))
            if "cargo nextest run" in command or ("cargo test" in command and "--all-features" in command):
                self.assertIn("matrix.os != 'windows-latest'", step)

    def test_ci_builds_write_line_tables_instead_of_full_debug_info(self):
        text = (ROOT / ".github/workflows/ci.yml").read_text(encoding="utf-8")
        header, jobs = text.split("\njobs:\n", 1)
        env = header.split("\nenv:\n", 1)[1]
        for name in ("CARGO_PROFILE_DEV_DEBUG", "CARGO_PROFILE_TEST_DEBUG"):
            self.assertIn(f"  {name}: ${{{{ inputs.debuginfo || 'line-tables-only' }}}}\n", env)
        option = header.split("      debuginfo:\n", 1)[1].split("\n\n", 1)[0]
        self.assertIn("type: choice\n", option)
        self.assertIn('options: [line-tables-only, "0", "2"]\n', option)
        self.assertTrue(option.rstrip().endswith("default: line-tables-only"), option)
        # A job-level value would shadow the dispatch choice.
        self.assertNotIn("CARGO_PROFILE", jobs)
        # Line tables keep panic backtraces readable; "0" and "2" stay available by dispatch only.
        self.assertNotIn("CARGO_PROFILE_DEV_DEBUG: 0", text)

    def test_windows_jobs_keep_cargo_home_and_temp_on_the_local_disk(self):
        text = (ROOT / ".github/workflows/ci.yml").read_text(encoding="utf-8")
        jobs = text.split("\njobs:\n", 1)[1]
        step = "      - uses: actions/checkout@v5\n      - uses: ./.github/actions/windows-fast-disk\n"
        self.assertEqual(jobs.count("./.github/actions/windows-fast-disk"), 3)
        for name in ("native", "windows-tests", "clients"):
            body = re.split(r"\n  [a-z0-9-]+:\n", jobs.split(f"\n  {name}:\n", 1)[1], maxsplit=1)[0]
            self.assertIn(step, body, name)
            # Before the toolchain and the cache: rust-toolchain keeps a CARGO_HOME that is already
            # set and rust-cache reads it to find the registry.
            moved = body.index("windows-fast-disk")
            self.assertLess(moved, body.index("dtolnay/rust-toolchain"), name)
            self.assertLess(moved, body.index("Swatinem/rust-cache"), name)
        action = (ROOT / ".github/actions/windows-fast-disk/action.yml").read_text(encoding="utf-8")
        self.assertIn("if: runner.os == 'Windows'", action)
        for line in ("CARGO_HOME=D:\\cargo", "TMP=D:\\tmp", "TEMP=D:\\tmp", "D:\\cargo\\bin"):
            self.assertIn(line, action)
        self.assertNotIn("uses:", action)

    def test_dev_fast_scripts_are_checked_on_every_os_without_a_build(self):
        text = (ROOT / ".github/workflows/ci.yml").read_text(encoding="utf-8")
        clients = text.split("\n  clients:\n", 1)[1].split("\n  manylinux:\n", 1)[0]
        step = clients.split("- name: Verify the opt-in dev-fast environment scripts", 1)[1].split("\n      - ", 1)[0]
        self.assertIn("run: python tests/dev_fast_env.py", step)
        # No OS condition: Linux (bash, dash), macOS (bash, zsh) and Windows (PowerShell) each test their own.
        self.assertNotIn("if:", step)
        self.assertEqual(text.count("dev_fast_env.py"), 1)
        # Only the scripts apply sccache or a linker: none of it is committed Cargo configuration.
        self.assertFalse((ROOT / ".cargo").exists())

    def test_msrv_runs_the_suite_on_ubuntu_only_and_is_checked_elsewhere(self):
        text = (ROOT / ".github/workflows/ci.yml").read_text(encoding="utf-8")
        native = text.split("\n  native:\n", 1)[1].split("\n  windows-tests:\n", 1)[0]
        # Every system still builds the MSRV: the matrix keeps both toolchains.
        self.assertIn('        toolchain: ["1.89.0", stable]\n', native)
        steps = {s.split("\n", 1)[0]: s for s in native.split("\n      - ")[1:]}
        suite = "matrix.os != 'windows-latest' && (matrix.toolchain == 'stable' || matrix.os == 'ubuntu-latest')"
        for name in ("name: Test full feature suite with Nextest on Linux and macOS", "name: Doc tests of every crate"):
            self.assertIn(f"if: {suite}\n", steps[name], name)
        install = [step for step in native.split("\n      - ")[1:] if "install-action" in step]
        self.assertEqual(len(install), 1)
        self.assertIn(f"if: {suite}\n", install[0])
        check = steps["name: Check the full feature suite builds on the MSRV (its tests run on ubuntu-latest)"]
        self.assertIn("if: matrix.toolchain != 'stable' && matrix.os != 'ubuntu-latest'\n", check)
        self.assertTrue(check.endswith("run: cargo check --locked --workspace --all-targets --all-features"), check)
        self.assertIn("if: matrix.os == 'windows-latest' && matrix.toolchain == 'stable'\n",
                      steps["name: Doc tests of vlite-graph"])
        # Stable is fully tested everywhere: Windows through the buckets, the rest through Nextest.
        windows = text.split("\n  windows-tests:\n", 1)[1].split("\n  clients:\n", 1)[0]
        self.assertNotIn("1.89.0", windows)
        self.assertEqual(policy.WINDOWS_TOOLCHAINS, ("stable",))
        self.assertEqual(policy.TOOLCHAINS, ("1.89.0", "stable"))

    def test_compact_method_inventory_runs_with_documentation_gates(self):
        text = (ROOT / ".github/workflows/ci.yml").read_text(encoding="utf-8")
        documentation = text.split("\n  documentation:\n", 1)[1].split("\n  native:\n", 1)[0]
        self.assertIn("python tests/compact_method_matrix.py", documentation)
        self.assertIn("python tests/api_reference.py", documentation)

    def test_graph_oracles_run_before_ci_routing(self):
        text = (ROOT / ".github/workflows/ci.yml").read_text(encoding="utf-8")
        changes = text.split("\n  changes:\n", 1)[1].split("\n  documentation:\n", 1)[0]
        for oracle in (
            "compact_ann_oracle",
            "compact_graph_oracle",
            "compact_graph_overlay_oracle",
            "compact_graph_property_oracle",
            "compact_graph_label_oracle",
            "compact_graph_attachment_oracle",
            "compact_graph_schema_oracle",
            "temporal_read_oracle",
            "ingestion_reference",
            "ingestion_schema_reference",
            "memory_erasure_oracle",
            "memory_wire_oracle",
            "graph_subset_oracle",
            "graph_subset_write_oracle",
            "lease_ledger_oracle",
        ):
            command = f"python tests/{oracle}.py --self-test"
            self.assertIn(command, changes)
            self.assertLess(changes.index(command), changes.index("ci_policy.py route"))

    def test_the_replay_driver_self_test_runs_before_ci_routing(self):
        text = (ROOT / ".github/workflows/ci.yml").read_text(encoding="utf-8")
        changes = text.split("\n  changes:\n", 1)[1].split("\n  documentation:\n", 1)[0]
        command = "python tests/mutant_replay_selftest.py"
        self.assertIn(f"      - run: {command}\n", changes)
        self.assertLess(changes.index(command), changes.index("ci_policy.py route"))
        self.assertTrue((ROOT / "tests/mutant_replay_selftest.py").is_file())

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

    def test_every_os_runs_the_all_features_suite_with_pinned_nextest(self):
        workflow = (ROOT / ".github/workflows/ci.yml").read_text(encoding="utf-8")
        # One install per job that runs nextest: `native` (not on Windows) and `windows-tests`.
        self.assertEqual(workflow.count("taiki-e/install-action@5488ac5a951645ce95062214902ac4b6bc1a1037"), 2)
        self.assertEqual(workflow.count("tool: cargo-nextest@0.9.149"), 2)
        native = workflow.split("\n  native:\n", 1)[1].split("\n  windows-tests:\n", 1)[0]
        install = [s for s in native.split("\n      - ")[1:] if "install-action" in s]
        self.assertEqual(len(install), 1)
        self.assertIn("if: matrix.os != 'windows-latest'", install[0])
        # Windows is bounded to two tests at a time per bucket; Linux and macOS use nextest's default
        # of one test per CPU, which is what the per-binary thread pool of `cargo test` already did.
        self.assertIn(
            "cargo nextest run --locked --workspace --all-targets --all-features --test-threads 2 --partition",
            workflow,
        )
        self.assertIn("run: cargo nextest run --locked --workspace --all-targets --all-features\n", native)
        self.assertEqual(workflow.count("cargo nextest run"), 2)
        # No target is dropped: nextest runs what `cargo test --all-targets --all-features` ran.
        self.assertNotIn("cargo test --locked --workspace --all-targets --all-features", workflow)
        self.assertNotIn("cargo nextest run --locked --workspace --all-targets --test-threads", workflow)
        # The default-feature suite runs once (Linux stable); other cells only compile it.
        self.assertIn("cargo check --locked --workspace --all-targets\n", workflow)
        self.assertIn("run: cargo test --locked --workspace --all-targets\n", workflow)

    def test_test_count_proofs_are_opt_in_and_match_the_suite(self):
        text = (ROOT / ".github/workflows/ci.yml").read_text(encoding="utf-8")
        option = text.split("\njobs:\n", 1)[0].split("      test_proof:\n", 1)[1]
        self.assertIn("type: boolean\n        default: false\n", option)
        windows = text.split("\n  windows-tests:\n", 1)[1].split("\n  clients:\n", 1)[0]
        step = windows.split("- name: Prove the buckets are disjoint and complete\n", 1)[1]
        self.assertIn("if: inputs.test_proof && matrix.shard == 1\n", step)
        self.assertIn(f"python scripts/ci_test_proof.py shards {policy.WINDOWS_SHARDS} -- --locked "
                      "--workspace --all-targets --all-features", step)
        native = text.split("\n  native:\n", 1)[1].split("\n  windows-tests:\n", 1)[0]
        step = native.split("- name: Prove Nextest lists the same tests as cargo test\n", 1)[1].split("\n      - ")[0]
        self.assertIn("if: inputs.test_proof && matrix.os != 'windows-latest' && "
                      "(matrix.toolchain == 'stable' || matrix.os == 'ubuntu-latest')\n", step)
        self.assertIn("python3 scripts/ci_test_proof.py parity -- --locked --workspace --all-targets "
                      "--all-features", step)

    def test_doc_tests_still_run_next_to_nextest(self):
        text = (ROOT / ".github/workflows/ci.yml").read_text(encoding="utf-8")
        native = text.split("\n  native:\n", 1)[1].split("\n  windows-tests:\n", 1)[0]
        # Nextest runs no doc tests and `--all-targets` never did, so each OS has a doc step. The
        # Windows cells build no all-features suite, so they keep the cheap per-crate step.
        steps = {s.split("\n", 1)[0]: s for s in native.split("\n      - ")[1:]}
        every = steps["name: Doc tests of every crate"]
        self.assertIn("if: matrix.os != 'windows-latest'", every)
        self.assertTrue(every.endswith("run: cargo test --locked --workspace --all-features --doc"), every)
        graph = steps["name: Doc tests of vlite-graph"]
        self.assertIn("if: matrix.os == 'windows-latest'", graph)
        self.assertTrue(graph.endswith("run: cargo test --locked -p vlite-graph --doc"), graph)
        self.assertNotIn("cargo test --locked -p vlite-graph\n", text)

    def test_default_features_suite_runs_next_to_the_all_features_suite(self):
        text = (ROOT / ".github/workflows/ci.yml").read_text(encoding="utf-8")
        native = text.split("\n  native:\n", 1)[1].split("\n  windows-tests:\n", 1)[0]
        # It left the longest job: `native` keeps only the compile check of the default features.
        self.assertNotIn("run: cargo test --locked --workspace --all-targets\n", native)
        self.assertNotIn("Default features suite", native)
        check = [s for s in native.split("\n      - ")[1:] if s.endswith("run: cargo check --locked --workspace --all-targets")]
        self.assertEqual(len(check), 1)
        self.assertIn("if: matrix.os != 'ubuntu-latest' || matrix.toolchain != 'stable'\n", check[0])
        job = text.split("\n  default-features:\n", 1)[1].split("\n  required:\n", 1)[0]
        self.assertIn("    needs: changes\n    if: needs.changes.outputs.full == 'true'\n    runs-on: ubuntu-latest\n", job)
        self.assertIn('      VLITE_REQUIRE_AVX2: "1"\n', job)
        self.assertIn("          toolchain: stable\n", job)
        self.assertTrue(job.rstrip().endswith("run: cargo test --locked --workspace --all-targets"), job)
        # The image gate (full_ci_jobs) also expects it: test_skipped_or_partial_matrix_cannot_qualify_an_image.

    def test_the_slowest_tests_start_first_in_nextest(self):
        import tomllib
        config = tomllib.loads((ROOT / ".config/nextest.toml").read_text(encoding="utf-8"))
        required = tuple(int(part) for part in config["nextest-version"].split("."))
        pinned = re.search(r"tool: cargo-nextest@(\d+\.\d+\.\d+)\n", (ROOT / ".github/workflows/ci.yml").read_text(encoding="utf-8"))
        self.assertLessEqual(required, tuple(int(part) for part in pinned.group(1).split(".")))
        overrides = config["profile"]["default"]["overrides"]
        self.assertTrue(overrides)
        names = []
        for override in overrides:
            self.assertEqual(set(override), {"filter", "priority"})
            self.assertTrue(1 <= override["priority"] <= 100, override)
            found = re.findall(r"test\(=([\w:]+)\)", override["filter"])
            self.assertEqual(" | ".join(f"test(={name})" for name in found), override["filter"])
            names += found
        self.assertEqual(len(names), len(set(names)))
        # Each name is a test that exists, so a rename is noticed instead of silently dropping the priority.
        sources = [path.read_text(encoding="utf-8", errors="replace")
                   for folder in ("src", "tests") for path in (ROOT / folder).rglob("*.rs")]
        for name in names:
            function = name.rsplit("::", 1)[-1]
            with self.subTest(test=name):
                self.assertTrue(any(re.search(rf"\bfn {function}\b", source) for source in sources), name)


class MutantsWorkflow(unittest.TestCase):
    """.github/workflows/mutants.yml: a dispatch-only, read-only fan-out of a mutant ledger."""

    @classmethod
    def setUpClass(cls):
        cls.text = (ROOT / ".github/workflows/mutants.yml").read_text(encoding="utf-8")
        cls.header = cls.text.split("\njobs:\n", 1)[0]

    def job(self, name):
        rest = self.text.split(f"\n  {name}:\n", 1)[1]
        return re.split(r"\n  [a-z0-9-]+:\n", rest, maxsplit=1)[0]

    def test_it_is_started_by_hand_and_can_only_read(self):
        self.assertIn("\non:\n  workflow_dispatch:\n", self.header)
        for trigger in ("push", "pull_request", "pull_request_target", "schedule", "workflow_run", "release"):
            self.assertNotIn(f"\n  {trigger}:", self.header)
        self.assertIn("\npermissions:\n  contents: read\n\n", self.header)
        for forbidden in ("git commit", "git push", "git tag", "gh pr ", "gh release", "secrets.", "id-token"):
            self.assertNotIn(forbidden, self.text)
        self.assertEqual(self.text.count("persist-credentials: false"), self.text.count("actions/checkout@"))

    def test_actions_are_pinned_and_every_job_runs_on_ubuntu(self):
        for use in re.findall(r"uses: (\S+)", self.text):
            if not use.startswith("actions/"):
                self.assertRegex(use, r"@[0-9a-f]{40}$", use)
        self.assertEqual(len(re.findall(r"Swatinem/rust-cache@[0-9a-f]{40} # v2\n", self.text)), 1)
        self.assertEqual(len(re.findall(r"dtolnay/rust-toolchain@[0-9a-f]{40} ", self.text)), 1)
        self.assertEqual(set(re.findall(r"runs-on: (\S+)", self.text)), {"ubuntu-latest"})

    def test_inputs_cover_the_ledger_the_tree_the_shards_and_an_id_filter(self):
        for name in ("ledger", "ref", "shards", "ids", "survivors_ok", "jobs", "timeout"):
            self.assertIn(f"\n      {name}:\n", self.header)
        self.assertIn("default: tests/k71_mutants.json", self.header)
        self.assertIn("ref: ${{ inputs.ref || github.sha }}", self.text)

    def test_shards_never_exceed_twenty(self):
        plan, replay = self.job("plan"), self.job("replay")
        self.assertIn('[ "$SHARDS" -gt 20 ]', plan)
        self.assertIn("--max-shards 20", plan)
        self.assertIn("max-parallel: 20", replay)
        self.assertIn("shard: ${{ fromJSON(needs.plan.outputs.shards) }}", replay)
        self.assertIn("fail-fast: false", replay)

    def test_inputs_reach_the_shell_through_the_environment_only(self):
        for name, body in (("plan", self.job("plan")), ("replay", self.job("replay")),
                           ("summary", self.job("summary"))):
            for line in body.splitlines():
                if line.lstrip().startswith(("run:", "python3 ", "if ")) or "$(" in line:
                    self.assertNotIn("${{ inputs.", line, (name, line))
        self.assertNotIn("run: ${{", self.text)

    def test_every_mutant_id_of_every_ledger_passes_the_id_filter(self):
        match = re.search(r"ids_pattern='([^']+)'", self.job("plan"))
        self.assertIsNotNone(match, "the plan job must check ids against ids_pattern")
        pattern = re.compile(match.group(1))
        for good in ("lines-fix/lf-mcp-cap-64-mib", "a/b,c/d", "a/b, c.d:e_f-g"):
            self.assertTrue(pattern.fullmatch(good), good)
        for bad in ("a;b", "$(id)", "`id`", "a|b", "a&b", "a>b", "a'b", 'a"b', "a\nb", "a\\b", "a*b"):
            self.assertFalse(pattern.fullmatch(bad), repr(bad))
        seen = 0
        for path in sorted((ROOT / "tests").glob("*mutants*.json")):
            data = json.loads(path.read_text(encoding="utf-8-sig"))
            entries = data.get("mutants") if isinstance(data, dict) else data
            for entry in entries if isinstance(entries, list) else []:
                name = entry.get("id") if isinstance(entry, dict) else None
                if isinstance(name, str):
                    seen += 1
                    self.assertTrue(pattern.fullmatch(name), f"{path.name}: {name}")
        self.assertGreater(seen, 200)

    def test_the_tree_is_pinned_once_by_the_plan_job(self):
        plan = self.job("plan")
        self.assertIn("tree_sha: ${{ steps.sha.outputs.tree_sha }}", plan)
        self.assertIn('echo "tree_sha=$(git -C tree rev-parse HEAD)" >> "$GITHUB_OUTPUT"', plan)
        self.assertEqual(plan.count("ref: ${{ inputs.ref || github.sha }}\n          path: tree\n"), 1)
        for name in ("replay", "summary"):
            body = self.job(name)
            self.assertIn("ref: ${{ needs.plan.outputs.tree_sha }}\n          path: tree\n", body)
            self.assertNotIn("inputs.ref", body)

    def test_each_mutant_has_a_timeout_and_a_rerun_can_replace_the_artifacts(self):
        plan, replay = self.job("plan"), self.job("replay")
        self.assertIn('[ "$TIMEOUT" -gt 7200 ]', plan)
        self.assertIn("TIMEOUT: ${{ inputs.timeout }}", replay)
        self.assertIn('--timeout "$TIMEOUT"', replay)
        self.assertEqual(self.text.count("uses: actions/upload-artifact@v4"), 2)
        self.assertEqual(self.text.count("          overwrite: true\n"), 2)

    def test_stale_anchors_are_reported_by_the_plan(self):
        plan = self.job("plan")
        self.assertIn("stale: ${{ steps.plan.outputs.stale }}", plan)
        self.assertIn("if: ${{ steps.plan.outputs.stale != '0' }}", plan)

    def test_each_shard_uploads_one_json_and_a_final_job_merges_them(self):
        replay, summary = self.job("replay"), self.job("summary")
        self.assertIn('--out "results/shard-$SHARD.json"', replay)
        self.assertIn("name: mutants-shard-${{ matrix.shard }}", replay)
        self.assertIn("path: results/shard-${{ matrix.shard }}.json", replay)
        self.assertIn("save-if: ${{ matrix.shard == 0 }}", replay)
        self.assertIn("needs: [plan, replay]", summary)
        self.assertIn("pattern: mutants-shard-*", summary)
        self.assertIn("merge-multiple: true", summary)
        self.assertIn('--summary "$GITHUB_STEP_SUMMARY"', summary)
        self.assertIn("--wall-seconds", summary)
        # A partial replay (the ids input) is merged over the same ids, never over the whole ledger.
        self.assertIn('--ids "$IDS"', summary)
        self.assertIn("IDS: ${{ inputs.ids }}", summary)
        # The table is written even when some shards failed.
        self.assertIn("if: ${{ !cancelled() && needs.plan.result == 'success' }}", summary)

    def test_the_replay_driver_comes_from_the_dispatched_commit(self):
        for name in ("plan", "replay", "summary"):
            body = self.job(name)
            self.assertIn("path: tools\n          sparse-checkout: tests/mutant_replay.py\n", body)
            self.assertIn("tools/tests/mutant_replay.py", body)
        self.assertTrue((ROOT / "tests/mutant_replay.py").is_file())


class ReleaseCi(unittest.TestCase):
    """scripts/ci_policy.py release-ci: the release skips its native tests only on proof from CI."""

    @staticmethod
    def green_jobs():
        return [dict(name=name, conclusion="success") for name in sorted(policy.RELEASE_TEST_JOBS)]

    @staticmethod
    def run_of(**fields):
        return dict(dict(head_sha="abc", event="push", conclusion="success", id=1, run_attempt=1,
                         html_url="https://github.com/mahpate/Vlite/actions/runs/1"), **fields)

    def gate(self, side_effect):
        with tempfile.TemporaryDirectory() as directory:
            output, summary = Path(directory) / "output", Path(directory) / "summary"
            env = dict(GITHUB_REPOSITORY="mahpate/Vlite", GITHUB_SHA="abc",
                       GITHUB_OUTPUT=str(output), GITHUB_STEP_SUMMARY=str(summary))
            with patch.dict(os.environ, env), patch.object(
                policy, "gh_pages", side_effect=side_effect
            ) as api, contextlib.redirect_stdout(io.StringIO()):
                policy.release_ci()
            return output.read_text(), summary.read_text(), api

    def test_the_release_needs_the_stable_suite_of_every_system(self):
        self.assertEqual(policy.RELEASE_TEST_JOBS, frozenset({
            "native (ubuntu-latest, stable)", "native (ubuntu-24.04-arm, stable)",
            "native (windows-latest, stable)", "native (macos-latest, stable)",
            "windows-tests (stable, shard 1/3)", "windows-tests (stable, shard 2/3)",
            "windows-tests (stable, shard 3/3)",
        }))
        # These are the names ci.yml really produces.
        text = (ROOT / ".github/workflows/ci.yml").read_text(encoding="utf-8")
        native = text.split("\n  native:\n", 1)[1].split("\n  windows-tests:\n", 1)[0]
        self.assertIn("        os: [ubuntu-latest, ubuntu-24.04-arm, windows-latest, macos-latest]\n", native)
        self.assertRegex(native, r"toolchain: \[[^\]]*\bstable\]")
        self.assertNotIn("\n    name:", native)
        windows = text.split("\n  windows-tests:\n", 1)[1].split("\n  clients:\n", 1)[0]
        self.assertIn("    name: windows-tests (${{ matrix.toolchain }}, shard ${{ matrix.shard }}/3)\n", windows)
        self.assertRegex(windows, r"toolchain: \[[^\]]*\bstable\]")
        self.assertIn("        shard: [1, 2, 3]\n", windows)

    def test_only_a_green_push_or_dispatch_of_the_exact_commit_counts(self):
        run = dict(head_sha="abc", event="push", conclusion="success")
        self.assertTrue(policy.release_run(run, "abc"))
        self.assertTrue(policy.release_run(dict(run, event="workflow_dispatch"), "abc"))
        # The commit is what was tested, so the branch it ran on does not matter.
        self.assertTrue(policy.release_run(dict(run, head_branch="feature"), "abc"))
        for field, value in (("head_sha", "old"), ("event", "pull_request"), ("event", "schedule"),
                             ("conclusion", "failure"), ("conclusion", "cancelled"), ("conclusion", None)):
            with self.subTest(field=field, value=value):
                self.assertFalse(policy.release_run(dict(run, **{field: value}), "abc"))

    def test_every_stable_test_job_must_have_succeeded(self):
        jobs = self.green_jobs()
        self.assertTrue(policy.release_tests_ran(jobs))
        self.assertFalse(policy.release_tests_ran([]))
        for job in jobs:
            with self.subTest(job=job["name"]):
                rest = [other for other in jobs if other is not job]
                self.assertFalse(policy.release_tests_ran(rest))
                for conclusion in ("skipped", "cancelled", "failure", None):
                    self.assertFalse(policy.release_tests_ran(rest + [dict(job, conclusion=conclusion)]))
        self.assertFalse(policy.release_tests_ran(jobs + [dict(jobs[0], conclusion="failure")]))
        # Other jobs, the MSRV cells included, are not part of the proof.
        others = [dict(name=name, conclusion="skipped")
                  for name in ("native (windows-latest, 1.89.0)", "windows-tests (1.89.0, shard 1/3)",
                               "docker", "lint")]
        self.assertTrue(policy.release_tests_ran(jobs + others))

    def test_a_green_run_of_this_commit_skips_the_release_tests(self):
        run = self.run_of(event="workflow_dispatch", id=7, run_attempt=3,
                          html_url="https://github.com/mahpate/Vlite/actions/runs/7")
        output, summary, api = self.gate([[run], self.green_jobs()])
        self.assertEqual(output, "green=true\n")
        self.assertIn(run["html_url"], summary)
        self.assertIn("not repeated", summary)
        runs_url, jobs_url = (call.args[0] for call in api.call_args_list)
        self.assertIn("repos/mahpate/Vlite/actions/workflows/ci.yml/runs?head_sha=abc&status=success", runs_url)
        self.assertIn("repos/mahpate/Vlite/actions/runs/7/attempts/3/jobs", jobs_url)
        # A run that skipped the tests does not stop the search: a later one can still qualify.
        output, _, _ = self.gate([[self.run_of(id=1), self.run_of(id=2)], [], self.green_jobs()])
        self.assertEqual(output, "green=true\n")

    def test_anything_less_runs_the_tests_as_before(self):
        docs_only = [dict(name="documentation", conclusion="success"),
                     dict(name="native (ubuntu-latest, stable)", conclusion="skipped")]
        cases = {
            "no run": [[]],
            "another commit": [[self.run_of(head_sha="old")]],
            "a pull request run": [[self.run_of(event="pull_request")]],
            "a run that did not succeed": [[self.run_of(conclusion="failure")]],
            "a documentation-only run": [[self.run_of()], docs_only],
            "a missing bucket": [[self.run_of()], self.green_jobs()[1:]],
        }
        for name, effects in cases.items():
            with self.subTest(name):
                output, summary, _ = self.gate(effects)
                self.assertEqual(output, "green=false\n")
                self.assertIn("the native test suites run in this release", summary)

    def test_an_unreadable_api_never_fails_the_job(self):
        for failure in (subprocess.CalledProcessError(1, "gh"), FileNotFoundError("gh"),
                        ValueError("not json"), KeyError("html_url")):
            with self.subTest(failure=type(failure).__name__):
                output, summary, _ = self.gate(failure)
                self.assertEqual(output, "green=false\n")
                self.assertIn(type(failure).__name__, summary)
        output, _, _ = self.gate([[dict(head_sha="abc")]])
        self.assertEqual(output, "green=false\n")


class ReleaseWorkflow(unittest.TestCase):
    """.github/workflows/release.yml: only the native test steps may be skipped, and only on proof."""

    @classmethod
    def setUpClass(cls):
        cls.text = (ROOT / ".github/workflows/release.yml").read_text(encoding="utf-8")

    def job(self, name):
        rest = self.text.split(f"\n  {name}:\n", 1)[1]
        return re.split(r"\n  [a-z0-9-]+:\n", rest, maxsplit=1)[0]

    def test_ci_green_can_only_read_the_actions_api_and_cannot_fail_the_release(self):
        job = self.job("ci-green")
        self.assertNotIn("needs:", job)
        self.assertIn("    permissions:\n      contents: read\n      actions: read\n", job)
        self.assertIn("    outputs:\n      green: ${{ steps.ci.outputs.green }}\n", job)
        self.assertIn("persist-credentials: false", job)
        step = job.split("      - name: Look for a successful CI run of this exact commit\n", 1)[1]
        self.assertIn("        id: ci\n        continue-on-error: true\n", step)
        self.assertIn("          GH_TOKEN: ${{ github.token }}\n", step)
        self.assertTrue(step.rstrip().endswith("run: python3 scripts/ci_policy.py release-ci"), step)
        # No other job reads the Actions API, and nothing gains a write permission.
        self.assertEqual(self.text.count("actions: read"), 1)
        self.assertEqual(self.text.count("contents: write"), 1)
        self.assertTrue(self.text.startswith("name: VLite release\n"))
        self.assertIn("\npermissions:\n  contents: read\n", self.text)

    def test_only_the_two_native_test_steps_wait_for_the_answer(self):
        skip = ("      - name: Test the full feature suite (skipped when CI already ran it on this commit)\n"
                "        if: needs.ci-green.outputs.green != 'true'\n"
                "        run: cargo test --locked --workspace --all-targets --all-features\n")
        # ci-green only saves work: a job that failed or was skipped must not stop the release, it
        # must leave the tests to run. Only a failed identity check or a cancelled run may.
        guard = "    if: ${{ !cancelled() && needs.identity.result == 'success' }}\n"
        for name in ("native", "linux-arm64"):
            with self.subTest(name):
                job = self.job(name)
                self.assertIn("    needs: [identity, ci-green]\n", job)
                self.assertIn(guard, job)
                self.assertNotIn("needs.ci-green.result", job)
                self.assertNotIn("always()", job)
                self.assertIn(skip, job + "\n")
                self.assertEqual(job.count("ci-green.outputs"), 1, "one condition, on the test step")
                self.assertNotIn("      - run: cargo test --locked --workspace --all-targets --all-features", job)
        native, arm = self.job("native"), self.job("linux-arm64")
        for command in ("cargo fmt --check", "cargo clippy --locked --workspace --all-targets --all-features -- -D warnings",
                        "cargo check --locked --no-default-features --lib", "cargo build --locked --release --bins",
                        "python tests/grpc_smoke.py", "cargo build --locked --release -p vlite-ffi",
                        "python tests/embedded_e2e.py", "python tests/embedded_wheel.py embedded-wheels",
                        "npm --prefix bindings/node test", "node bindings/node/pack.mjs node-packages"):
            self.assertIn(command, native)
        for command in ("cargo build --locked --release --bins", "python tests/grpc_smoke.py",
                        "name: native-Linux-ARM64"):
            self.assertIn(command, arm)

    def test_images_wheels_and_publishing_are_unchanged(self):
        self.assertIn("    needs: [native, linux-arm64, image-test, manylinux, clients]\n", self.job("image-build"))
        for name in ("image-build", "image-publish"):
            self.assertIn("    if: startsWith(github.ref, 'refs/tags/v')\n", self.job(name))
        self.assertIn("    needs: image-build\n", self.job("image-publish"))
        for name in ("clients", "manylinux", "image-test"):
            self.assertIn("    needs: identity\n", self.job(name))
        # The container suite and the wheels never wait for, or skip because of, the CI answer.
        for name in ("clients", "manylinux", "image-test", "image-build", "image-publish", "image-verify", "release"):
            self.assertNotIn("ci-green.outputs", self.job(name), name)
        self.assertIn("python3 tests/dashboard_e2e.py", self.job("image-test"))


class TestProof(unittest.TestCase):
    """scripts/ci_test_proof.py: the proofs that nextest and its buckets run the tests cargo test ran."""

    @staticmethod
    def listing(**binaries):
        """A `cargo nextest list --message-format json` document: binary -> {test: (ignored, matches)}."""
        suites = {}
        for binary, tests in binaries.items():
            suites[binary] = {
                "binary-path": f"C:\\work\\target\\debug\\deps\\{binary}.exe",
                "testcases": {
                    name: {"ignored": ignored,
                           "filter-match": {"status": "matches"} if matches
                           else {"status": "mismatch", "reason": "partition"}}
                    for name, (ignored, matches) in tests.items()
                },
            }
        return json.dumps({"test-count": 0, "rust-suites": suites})

    def test_binary_keys_ignore_directories_and_extensions(self):
        self.assertEqual(proof.binary_key("target/debug/deps/vlite-2e5104f9"), "vlite-2e5104f9")
        self.assertEqual(proof.binary_key("target\\debug\\deps\\kv_tools-ab12.exe"), "kv_tools-ab12")

    def test_cargo_list_is_read_per_binary_with_colons_in_names(self):
        output = (
            "   Compiling x\n     Running unittests src/lib.rs (target/debug/deps/vlite-aa)\n"
            "a::b::c: test\nslow: test\n\n2 tests, 0 benchmarks\n"
            "     Running tests/it.rs (target/debug/deps/it-bb)\nplain: test\nbench_one: benchmark\n"
            "     Doc-tests vlite\n0 tests, 0 benchmarks\n"
        )
        self.assertEqual(proof.read_cargo_list(output),
                         {("vlite-aa", "a::b::c"), ("vlite-aa", "slow"), ("it-bb", "plain"), ("it-bb", "bench_one")})

    def test_cargo_list_survives_the_colored_headings_ci_produces(self):
        # CI runs with CARGO_TERM_COLOR=always: the heading starts with escape codes.
        output = (
            "\x1b[1m\x1b[92m     Running\x1b[0m unittests src/lib.rs (target/debug/deps/vlite-aa)\n"
            "a::b: test\n\n1 test, 0 benchmarks\n"
            "\x1b[1m\x1b[92m     Running\x1b[0m tests/it.rs (target/debug/deps/it-bb)\nplain: test\n"
        )
        self.assertEqual(proof.read_cargo_list(output), {("vlite-aa", "a::b"), ("it-bb", "plain")})

    def test_parity_asks_cargo_for_plain_output_and_accepts_colored_output(self):
        colored = ("\x1b[1m\x1b[92m     Running\x1b[0m unittests src/lib.rs (target/debug/deps/vlite-aa)\n"
                   "a: test\n")
        cargo = Mock(returncode=0, stdout=colored)
        listing = Mock(returncode=0, stdout=self.listing(**{"vlite-aa": {"a": (False, True)}}), stderr="")
        with patch.object(proof.subprocess, "run", side_effect=[cargo, listing]) as run:
            self.assertEqual(proof.parity(["--locked", "--all-targets"]), 0)
        command = run.call_args_list[0].args[0]
        self.assertEqual(command[:4], ["cargo", "test", "--color", "never"])
        self.assertEqual(command[-2:], ["--", "--list"])

    def test_nextest_listing_keeps_the_ignored_flag_and_the_selection(self):
        document = self.listing(**{"vlite-aa": {"a": (False, True), "b": (True, False), "c": (False, False)}})
        listing = proof.read_nextest(document)
        self.assertEqual(listing["vlite-aa"]["b"], (True, False))
        self.assertEqual(proof.names(listing), {("vlite-aa", n) for n in "abc"})
        self.assertEqual(proof.names(listing, selected_only=True), {("vlite-aa", "a")})

    def test_buckets_must_be_disjoint_and_complete(self):
        whole = proof.read_nextest(self.listing(x={"a": (False, True), "b": (False, True), "c": (False, True)},
                                                y={"d": (False, True), "e": (True, False)}))

        def bucket(selected):
            return proof.read_nextest(self.listing(
                x={n: (False, n in selected) for n in "abc"}, y={"d": (False, "d" in selected), "e": (True, False)}))

        problems, sizes = proof.compare_buckets(whole, [bucket("ad"), bucket("b"), bucket("c")])
        self.assertEqual((problems, sizes), ([], [2, 1, 1]))
        problems, _ = proof.compare_buckets(whole, [bucket("ad"), bucket("bd"), bucket("c")])
        self.assertTrue(any("repeats" in p for p in problems), problems)
        problems, _ = proof.compare_buckets(whole, [bucket("ad"), bucket("b")])
        self.assertTrue(any("missing" in p for p in problems), problems)

    def test_empty_listings_prove_nothing(self):
        empty = proof.read_nextest(self.listing(x={"a": (False, False), "b": (True, False)}))
        problems, sizes = proof.compare_buckets(empty, [empty, empty])
        self.assertEqual((len(problems), sizes), (1, [0, 0]))
        self.assertIn("nothing was proved", problems[0])
        self.assertEqual(len(proof.compare_parity(set(), proof.read_nextest(self.listing()))), 1)
        listing = proof.read_nextest(self.listing(x={"a": (False, True)}))
        problems = proof.compare_parity(set(), listing)
        self.assertTrue(any("nothing was proved" in p for p in problems), problems)
        problems = proof.compare_parity({("x", "a")}, proof.read_nextest(self.listing()))
        self.assertTrue(any("nothing was proved" in p for p in problems), problems)
        self.assertEqual(proof.compare_parity({("x", "a")}, listing), [])

    def test_parity_compares_every_binary(self):
        listing = proof.read_nextest(self.listing(**{"vlite-aa": {"a": (False, True), "s": (True, False)},
                                                     "it-bb": {"p": (False, True)}}))
        same = {("vlite-aa", "a"), ("vlite-aa", "s"), ("it-bb", "p")}
        self.assertEqual(proof.compare_parity(same, listing), [])
        self.assertEqual(len(proof.compare_parity(same - {("it-bb", "p")}, listing)), 1)
        self.assertEqual(len(proof.compare_parity(same | {("other-cc", "z")}, listing)), 1)
        moved = (same - {("it-bb", "p")}) | {("vlite-aa", "p")}
        self.assertEqual(len(proof.compare_parity(moved, listing)), 2)

    def test_the_cli_refuses_unknown_modes(self):
        for argv in ([], ["shards", "3"], ["shards", "x", "--"], ["unknown", "--"]):
            with self.subTest(argv=argv):
                with self.assertRaises(SystemExit):
                    proof.main(argv)


if __name__ == "__main__":
    unittest.main()
