"""CI routing, qualification and cache wiring regressions (standard library only)."""
import contextlib
import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
import re
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
           for rust in ("1.89.0", "stable") for shard in (1, 2, 3)]
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
        self.assertEqual(len(shards), 2 * policy.WINDOWS_SHARDS)
        for shard in shards:
            with self.subTest(shard=shard["name"]):
                rest = [job for job in jobs if job is not shard]
                self.assertFalse(policy.full_ci_jobs(rest))
                for conclusion in ("skipped", "cancelled", "failure"):
                    self.assertFalse(policy.full_ci_jobs(rest + [dict(shard, conclusion=conclusion)]))
        wrong_total = dict(shards[0], name="windows-tests (1.89.0, shard 1/2)")
        self.assertFalse(policy.full_ci_jobs([wrong_total] + [j for j in jobs if j is not shards[0]]))
        self.assertFalse(policy.full_ci_jobs(jobs + [dict(shards[0], name="windows-tests (stable, shard 4/3)")]))

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
        # One runner per bucket, buckets 1..N of N once each, per toolchain.
        self.assertIn(f"        shard: {list(range(1, shards + 1))}\n", job)
        self.assertIn('        toolchain: ["1.89.0", stable]\n', job)
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
            if "cargo nextest run" in step or "--all-targets --all-features" in step:
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
        self.assertIn("if: inputs.test_proof && matrix.os != 'windows-latest'\n", step)
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


REPO = "mahpate/Vlite"
SHA = "0123456789abcdef" * 2 + "01234567"
WORKFLOW_ID = 77
RUNS = f"repos/{REPO}/actions/workflows/ci.yml/runs?head_sha={SHA}&per_page=100"
GUARD_IF = "startsWith(github.ref, 'refs/tags/v') && needs.ci-guard.outputs.verified == 'true'"
PUBLICATION_MARKERS = ("packages: write", "contents: write", "id-token: write", "push: true",
                       "imagetools create", "gh release", "gh-action-pypi-publish")


def ci_run(**overrides):
    """A ci.yml run at SHA that passes rules 1-6 of the release guard."""
    run = dict(id=5, run_attempt=1, workflow_id=WORKFLOW_ID, path=".github/workflows/ci.yml",
               head_sha=SHA, event="workflow_dispatch", head_branch="release/0.7.4",
               repository={"full_name": REPO}, head_repository={"full_name": REPO},
               status="completed", conclusion="success",
               html_url=f"https://github.com/{REPO}/actions/runs/5")
    run.update(overrides)
    return run


def guard_jobs():
    return qualified_jobs() + [dict(name="CI required", conclusion="success")]


class Runaway(BaseException):
    """A guard that would never stop; BaseException so that the guard cannot swallow it."""


class FakeGitHub:
    """GitHub API fakes on a fake clock. `listings` gives the runs per evaluation (the last repeats)."""

    def __init__(self, listings, jobs=None, refetch=None, tags=None, cost=0, total=None):
        self.listings, self.jobs, self.refetch = listings, jobs or {}, refetch or {}
        self.tags, self.cost, self.total = tags or {}, cost, total
        self.now, self.sleeps, self.calls, self.evaluations = 1000.0, [], [], 0
        self.current = []

    def clock(self):
        return self.now

    def sleep(self, seconds):
        if seconds <= 0 or len(self.sleeps) > 1000:
            raise Runaway(f"sleep({seconds}) after {len(self.sleeps)} sleeps")
        self.sleeps.append((self.now, seconds))
        self.now += seconds

    def api(self, endpoint):
        self.calls.append(endpoint)
        self.now += self.cost
        if endpoint == RUNS:
            self.current = self.listings[min(self.evaluations, len(self.listings) - 1)]
            self.evaluations += 1
            total = len(self.current) if self.total is None else self.total
            return [dict(total_count=total, workflow_runs=self.current)]
        match = re.fullmatch(rf"repos/{REPO}/actions/runs/(\d+)/attempts/(\d+)/jobs\?per_page=100", endpoint)
        if match:
            jobs = self.jobs.get((int(match[1]), int(match[2])), guard_jobs())
            return [dict(total_count=len(jobs), jobs=jobs[:20]), dict(total_count=len(jobs), jobs=jobs[20:])]
        raise AssertionError(f"unexpected paginated endpoint {endpoint}")

    def api_object(self, endpoint):
        self.calls.append(endpoint)
        self.now += self.cost
        if endpoint == f"repos/{REPO}/actions/workflows/ci.yml":
            return dict(id=WORKFLOW_ID, path=".github/workflows/ci.yml")
        if endpoint.startswith(f"repos/{REPO}/commits/refs/tags/"):
            return dict(sha=self.tags[endpoint.rsplit("/", 1)[1]])
        match = re.fullmatch(rf"repos/{REPO}/actions/runs/(\d+)", endpoint)
        if match:
            run_id = int(match[1])
            return self.refetch.get(run_id) or next(run for run in self.current if run["id"] == run_id)
        raise AssertionError(f"unexpected endpoint {endpoint}")


def flaky(call, errors):
    """Raise each error in turn, then behave like `call`."""
    errors = list(errors)

    def wrapped(endpoint):
        if errors:
            raise errors.pop(0)
        return call(endpoint)
    return wrapped


OTHER = "1" * 40
WHEEL = "vlite_client-0.7.4-py3-none-any.whl"


def asset(name, content=b"wheel", **extra):
    return dict(name=name, bytes=len(content), sha256=hashlib.sha256(content).hexdigest(), **extra)


def release_files(folder, assets, manifest=None, sums=None):
    """release-manifest.json and SHA256SUMS as bindings/release_assets.py writes them."""
    folder.mkdir(parents=True)
    manifest = dict(version="0.7.4", source_commit=SHA, assets=assets) if manifest is None else manifest
    data = (json.dumps(manifest, indent=2) + "\n").encode()
    (folder / "release-manifest.json").write_bytes(data)
    lines = [f"{entry['sha256']}  {entry['name']}" for entry in assets if isinstance(entry, dict)]
    lines.append(f"{hashlib.sha256(data).hexdigest()}  release-manifest.json")
    (folder / "SHA256SUMS").write_text("\n".join(sums(lines) if sums else lines) + "\n", encoding="ascii")
    return hashlib.sha256(data).hexdigest()


def at_sources(github, listings):
    """Serve the ci.yml runs of further commits (`listings` maps a SHA to its runs)."""
    original = github.api

    def api(endpoint):
        for sha, runs in listings.items():
            if endpoint == RUNS.replace(SHA, sha):
                github.calls.append(endpoint)
                github.current = runs
                return [dict(total_count=len(runs), workflow_runs=runs)]
        return original(endpoint)
    github.api = api


def job_pages(github, pages):
    """Answer every attempt-jobs listing with `pages`."""
    original = github.api

    def api(endpoint):
        if "/jobs?" in endpoint:
            github.calls.append(endpoint)
            return pages
        return original(endpoint)
    github.api = api


class ReleaseGuard(unittest.TestCase):
    """scripts/ci_policy.py release-ci and the ci-guard jobs that gate every publication."""

    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.output, self.summary = Path(directory.name) / "output", Path(directory.name) / "summary"
        self.directory = Path(directory.name)

    def guard(self, github, **env):
        env = {key: value for key, value in (dict(
            GITHUB_REPOSITORY=REPO, GITHUB_SHA=SHA, GITHUB_REF="refs/tags/v0.7.4", GITHUB_REF_NAME="v0.7.4",
            GITHUB_OUTPUT=str(self.output), GITHUB_STEP_SUMMARY=str(self.summary)
        ) | env).items() if value is not None}
        self.output.unlink(missing_ok=True)
        self.summary.unlink(missing_ok=True)
        stdout, stderr = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stderr):
            code = policy.release_ci(env, github.api, github.api_object, github.sleep, github.clock)
        output = self.output.read_text() if self.output.is_file() else ""
        summary = self.summary.read_text() if self.summary.is_file() else ""
        if code:
            self.assertNotIn("verified=true", output)
        return code, output, summary, stderr.getvalue()

    def assert_failed(self, github, **env):
        code, output, summary, stderr = self.guard(github, **env)
        self.assertEqual(code, 1)
        self.assertNotIn("verified=true", output)
        return summary + stderr

    def report(self, github, **env):
        code, output, summary, _ = self.guard(github, **(dict(CI_GUARD_MODE="report", GITHUB_REF="refs/heads/release") | env))
        self.assertEqual(code, 0)
        self.assertEqual(output, "verified=false\n")
        self.assertEqual(github.sleeps, [])
        return summary

    def test_b01_a_qualified_dispatch_or_main_push_passes_at_once(self):
        for run in (ci_run(), ci_run(head_branch="v0.7.4"), ci_run(event="push", head_branch="main")):
            with self.subTest(event=run["event"], ref=run["head_branch"]):
                github = FakeGitHub([[run]])
                code, output, summary, _ = self.guard(github, CI_GUARD_MODE="enforce")
                self.assertEqual((code, output), (0, "verified=true\n"))
                self.assertEqual(github.sleeps, [])
                self.assertIn(run["html_url"], summary)
                self.assertIn(f"repos/{REPO}/actions/runs/5/attempts/1/jobs?per_page=100", github.calls)
                self.assertIn(f"repos/{REPO}/actions/runs/5", github.calls)

    def test_b02_a_missing_run_waits_for_the_grace_period_then_fails_with_the_remedy(self):
        github = FakeGitHub([[]])
        message = self.assert_failed(github)
        self.assertEqual(github.now - 1000, 15 * 60)
        self.assertEqual(github.evaluations, 16)
        self.assertTrue(all(seconds <= 60 for _, seconds in github.sleeps))
        self.assertIn("gh workflow run ci.yml --ref v0.7.4 -f docker=true", message)
        self.assertIn("Re-run failed jobs", message)
        github = FakeGitHub([[]])
        self.assert_failed(github, CI_MISSING_GRACE_MINUTES="0")
        self.assertEqual((github.sleeps, github.evaluations), ([], 1))
        self.assertIn("verdict: missing", self.report(FakeGitHub([[]])))

    def test_b03_a_pending_run_is_awaited_within_the_bound(self):
        github = FakeGitHub([[ci_run(status="queued", conclusion=None)],
                             [ci_run(status="in_progress", conclusion=None)], [ci_run()]])
        code, output, _, _ = self.guard(github)
        self.assertEqual((code, output), (0, "verified=true\n"))
        self.assertEqual([seconds for _, seconds in github.sleeps], [60, 60])
        for wait, cost in ((30, 0), (1, 7), (3, 13)):
            with self.subTest(wait=wait, cost=cost):
                github = FakeGitHub([[ci_run(status="in_progress", conclusion=None)]], cost=cost)
                message = self.assert_failed(github, CI_WAIT_MINUTES=str(wait), CI_MISSING_GRACE_MINUTES="0")
                deadline = 1000 + wait * 60
                self.assertTrue(all(at + seconds <= deadline for at, seconds in github.sleeps))
                self.assertGreaterEqual(github.now, deadline)
                self.assertLess(github.now, deadline + 4 * cost + 1)
                self.assertIn("status in_progress", message)
        self.assertIn("verdict: pending", self.report(FakeGitHub([[ci_run(status="waiting", conclusion=None)]])))

    def test_b04_failed_or_partial_runs_are_rejected(self):
        for conclusion in ("failure", "cancelled", "skipped", "timed_out", "startup_failure", None):
            with self.subTest(conclusion=conclusion):
                github = FakeGitHub([[ci_run(conclusion=conclusion)]])
                self.assertIn(f"conclusion {conclusion or 'null'}",
                              self.assert_failed(github, CI_MISSING_GRACE_MINUTES="0"))
                self.assertIn("verdict: rejected", self.report(FakeGitHub([[ci_run(conclusion=conclusion)]])))
        full = guard_jobs()
        for name, jobs in (
            ("docker skipped", [dict(job, conclusion="skipped") if job["name"] == "docker" else job for job in full]),
            ("docs-only", [dict(name="lint", conclusion="success"), dict(name="CI required", conclusion="success")]),
            ("no CI required", [job for job in full if job["name"] != "CI required"]),
            ("CI required failed", full[:-1] + [dict(name="CI required", conclusion="failure")]),
        ):
            with self.subTest(jobs=name):
                github = FakeGitHub([[ci_run()]], jobs={(5, 1): jobs})
                self.assertIn("partial CI (docs-only or Docker skipped)",
                              self.assert_failed(github, CI_MISSING_GRACE_MINUTES="0"))

    def test_b05_untrusted_or_unrelated_runs_never_pass(self):
        for run, verdict, reason in (
            (ci_run(event="pull_request"), "rejected", "event pull_request"),
            (ci_run(head_repository={"full_name": "fork/Vlite"}), "rejected", "foreign repository"),
            (ci_run(head_repository=None), "rejected", "foreign repository"),
            (ci_run(repository={"full_name": "other/Vlite"}), "rejected", "foreign repository"),
            (ci_run(workflow_id=78), "missing", None),
            (ci_run(path=".github/workflows/release.yml"), "missing", None),
            (ci_run(head_sha="f" * 40), "missing", None),
        ):
            with self.subTest(run=reason or verdict):
                github = FakeGitHub([[run]])
                message = self.assert_failed(github, CI_MISSING_GRACE_MINUTES="0")
                if reason:
                    self.assertIn(reason, message)
                summary = self.report(FakeGitHub([[run]]))
                self.assertIn(f"verdict: {verdict}", summary)
                self.assertNotIn("verdict: qualified", summary)
                self.assertFalse(any("/jobs" in call for call in github.calls))

    def test_b06_only_the_latest_attempt_counts(self):
        # Attempt 1 succeeded; attempt 2 is the latest and is still running or failed.
        github = FakeGitHub([[ci_run(run_attempt=2, status="in_progress", conclusion=None)]])
        self.assertIn("verdict: pending", self.report(github))
        self.assertFalse(any("/attempts/" in call for call in github.calls))
        self.assertIn("conclusion failure", self.assert_failed(
            FakeGitHub([[ci_run(run_attempt=2, conclusion="failure")]]), CI_MISSING_GRACE_MINUTES="0"))
        # A rerun starts between the jobs fetch and the re-fetch: re-evaluate on the new attempt.
        github = FakeGitHub([[ci_run()], [ci_run(run_attempt=2)]],
                            refetch={5: ci_run(run_attempt=2, status="queued", conclusion=None)})
        self.assertIn("verdict: pending", self.report(github))
        github = FakeGitHub([[ci_run()], [ci_run(run_attempt=2)]],
                            refetch={5: ci_run(run_attempt=2, status="queued", conclusion=None)})
        original = github.api_object

        def refetch_once(endpoint):
            result = original(endpoint)
            if endpoint == f"repos/{REPO}/actions/runs/5":
                github.refetch = {}
            return result
        github.api_object = refetch_once
        code, output, _, _ = self.guard(github)
        self.assertEqual((code, output), (0, "verified=true\n"))
        self.assertEqual(github.evaluations, 2)
        self.assertIn(f"repos/{REPO}/actions/runs/5/attempts/2/jobs?per_page=100", github.calls)
        # A newer attempt that already finished green still needs its own jobs checked.
        github = FakeGitHub([[ci_run()]], refetch={5: ci_run(run_attempt=2)})
        self.assertIn("verdict: pending", self.report(github))
        self.assertNotIn(f"repos/{REPO}/actions/runs/5/attempts/2/jobs?per_page=100", github.calls)
        github = FakeGitHub([[ci_run()]], refetch={5: ci_run(run_attempt=2, status="in_progress")})
        self.assertIn("rerun", self.assert_failed(github, CI_WAIT_MINUTES="2", CI_MISSING_GRACE_MINUTES="0"))

    def test_b07_api_failures_are_retried_three_times_then_fail_closed(self):
        timeout = policy.subprocess.TimeoutExpired(["gh", "api"], 60)
        for errors in ([RuntimeError("HTTP 502")] * 2, [timeout, timeout]):
            with self.subTest(errors=errors):
                github = FakeGitHub([[ci_run()]])
                github.api_object = flaky(github.api_object, errors)
                code, output, _, _ = self.guard(github)
                self.assertEqual((code, output), (0, "verified=true\n"))
                self.assertEqual([seconds for _, seconds in github.sleeps], [5, 15])
        for errors in ([RuntimeError("HTTP 401")] * 3, [timeout] * 3, [ValueError("Expecting value")] * 3):
            with self.subTest(errors=errors):
                github = FakeGitHub([[ci_run()]])
                github.api = flaky(github.api, errors)
                self.assertIn("error", self.assert_failed(github))
                self.assertEqual([seconds for _, seconds in github.sleeps], [5, 15])
        broken = (
            ("malformed JSON", dict(api_object=lambda endpoint: "<html>")),
            ("malformed pages", dict(api=lambda endpoint: {"message": "Not Found"})),
            ("missing key", dict(listings=[[{key: value for key, value in ci_run().items() if key != "status"}]])),
            ("truncated pagination", dict(total=2)),
        )
        for name, fault in broken:
            for mode in ("enforce", "report"):
                with self.subTest(fault=name, mode=mode):
                    github = FakeGitHub(fault.get("listings", [[ci_run()]]), total=fault.get("total"))
                    for attribute in ("api", "api_object"):
                        if attribute in fault:
                            setattr(github, attribute, fault[attribute])
                    code, output, summary, stderr = self.guard(
                        github, CI_GUARD_MODE=mode, GITHUB_REF="refs/heads/release")
                    self.assertEqual((code, output), (1, "verified=false\n"))
                    self.assertIn("verdict: error", summary)
                    self.assertIn("verdict: error", stderr)

    def test_b07_the_real_gh_wrapper_is_bounded(self):
        done = policy.subprocess.CompletedProcess(["gh"], 0, stdout='[{"total_count": 0}]', stderr="")
        with patch.object(policy.subprocess, "run", return_value=done) as run:
            self.assertEqual(policy.gh_slurp(RUNS), [{"total_count": 0}])
            self.assertEqual(run.call_args.args[0], ["gh", "api", "--paginate", "--slurp", RUNS])
            self.assertEqual(run.call_args.kwargs["timeout"], 60)
            policy.gh_object(f"repos/{REPO}/actions/runs/5")
            self.assertEqual(run.call_args.args[0], ["gh", "api", f"repos/{REPO}/actions/runs/5"])
            self.assertEqual(run.call_args.kwargs["timeout"], 60)
        for result in (policy.subprocess.TimeoutExpired(["gh"], 60),
                       policy.subprocess.CompletedProcess(["gh"], 1, stdout="", stderr="HTTP 404"),
                       policy.subprocess.CompletedProcess(["gh"], 0, stdout="not json", stderr="")):
            with self.subTest(result=result), patch.object(policy.subprocess, "run", side_effect=[result]):
                with self.assertRaises(Exception):
                    policy.gh_object("repos/x")
        # Wired through the guard, a timeout is one failed try and three of them fail closed.
        timeout = policy.subprocess.TimeoutExpired(["gh"], 60)
        sleeps = []
        with patch.object(policy.subprocess, "run", side_effect=[timeout] * 3) as run:
            env = dict(GITHUB_REPOSITORY=REPO, GITHUB_SHA=SHA, GITHUB_REF="refs/tags/v0.7.4",
                       GITHUB_OUTPUT=str(self.output), GITHUB_STEP_SUMMARY=str(self.summary))
            with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
                code = policy.release_ci(env, policy.gh_slurp, policy.gh_object, sleeps.append, lambda: 0)
        self.assertEqual((code, run.call_count, sleeps), (1, 3, [5, 15]))
        self.assertEqual(self.output.read_text(), "verified=false\n")

    def test_b07_report_mode_fails_when_its_summary_cannot_be_written(self):
        github = FakeGitHub([[ci_run()]])
        code, output, _, stderr = self.guard(github, CI_GUARD_MODE="report", GITHUB_REF="refs/heads/release",
                                             GITHUB_STEP_SUMMARY=str(self.directory))
        self.assertEqual((code, output), (1, "verified=false\n"))
        self.assertIn("verdict: error", stderr)

    def test_b08_modes_and_wait_bounds_are_validated_before_any_api_call(self):
        for env in (dict(CI_GUARD_MODE="Enforce"), dict(CI_GUARD_MODE="off"), dict(CI_GUARD_MODE="publish"),
                    dict(CI_GUARD_MODE="report", GITHUB_REF="refs/tags/v1.2.3"),
                    dict(CI_WAIT_MINUTES="-1"), dict(CI_WAIT_MINUTES="241"), dict(CI_WAIT_MINUTES="abc"),
                    dict(CI_WAIT_MINUTES="1.5"), dict(CI_WAIT_MINUTES=" 5"), dict(CI_MISSING_GRACE_MINUTES="x"),
                    dict(CI_WAIT_MINUTES="20", CI_MISSING_GRACE_MINUTES="30"),
                    dict(CI_WAIT_MINUTES="0", CI_MISSING_GRACE_MINUTES="1")):
            with self.subTest(env=env):
                github = FakeGitHub([[ci_run()]])
                self.assert_failed(github, **env)
                self.assertEqual(github.calls, [])
        for env in (dict(CI_GUARD_MODE=None), dict(CI_GUARD_MODE="enforce"),
                    dict(CI_WAIT_MINUTES="240", CI_MISSING_GRACE_MINUTES="240"),
                    dict(CI_WAIT_MINUTES="0", CI_MISSING_GRACE_MINUTES="0")):
            with self.subTest(env=env):
                code, output, _, _ = self.guard(FakeGitHub([[ci_run()]]), **env)
                self.assertEqual((code, output), (0, "verified=true\n"))
        summary = self.report(FakeGitHub([[ci_run()]]))
        self.assertIn("verdict: qualified", summary)

    def test_b09_the_tag_is_resolved_to_its_commit(self):
        endpoint = f"repos/{REPO}/commits/refs/tags/v0.7.4"
        for ci_sha in ("", None, SHA):
            with self.subTest(ci_sha=ci_sha):
                github = FakeGitHub([[ci_run()]], tags={"v0.7.4": SHA})
                code, output, _, _ = self.guard(github, CI_TAG="v0.7.4", CI_SHA=ci_sha, GITHUB_SHA="e" * 40)
                self.assertEqual((code, output), (0, "verified=true\n"))
                self.assertEqual(github.calls[0], endpoint)
                self.assertIn(RUNS, github.calls)
        github = FakeGitHub([[ci_run()]], tags={"v0.7.4": SHA})
        self.assertIn("v0.7.4", self.assert_failed(github, CI_TAG="v0.7.4", CI_SHA="e" * 40))
        self.assertEqual(github.calls, [endpoint])
        for tag in ("0.7.4", "v", "va", "v1;rm", "refs/tags/v1", "v1 ", "v1/../x"):
            with self.subTest(tag=tag):
                github = FakeGitHub([[ci_run()]], tags={tag: SHA})
                self.assert_failed(github, CI_TAG=tag)
                self.assertEqual(github.calls, [])
        for sha in ("abc", SHA.upper(), SHA[:-1], SHA + "0", "g" * 40, ""):
            with self.subTest(sha=sha):
                github = FakeGitHub([[ci_run()]])
                self.assertIn("40 lowercase hex", self.assert_failed(github, GITHUB_SHA=sha))
                self.assertEqual(github.calls, [])
                github = FakeGitHub([[ci_run()]], tags={"v0.7.4": sha})
                self.assertIn("40 lowercase hex", self.assert_failed(github, CI_TAG="v0.7.4"))
                self.assertEqual(github.calls, [endpoint])

    def jobs(self, workflow):
        text = (ROOT / ".github/workflows" / workflow).read_text(encoding="utf-8")
        header, body = text.split("\njobs:\n", 1)
        parts = re.split(r"^  ([a-z0-9-]+):\n", body, flags=re.M)
        return text, header, dict(zip(parts[1::2], parts[2::2]))

    def test_b10_every_publication_waits_for_an_enforced_guard(self):
        gated = {"release.yml": {"image-build", "image-publish", "image-verify", "release"},
                 "pypi.yml": {"publish"}}
        for workflow, expected in gated.items():
            text, _, jobs = self.jobs(workflow)
            self.assertNotIn("continue-on-error", text)
            # Downloading release assets is read-only; creating or editing a release publishes.
            publishing = {name for name, block in jobs.items()
                          if any(marker in block.replace("gh release download", "")
                                 for marker in PUBLICATION_MARKERS)}
            self.assertLessEqual(publishing, expected, workflow)
            self.assertNotIn("ci-guard", publishing)
            for name in expected:
                with self.subTest(workflow=workflow, job=name):
                    block = jobs[name]
                    needs = re.search(r"^    needs: (.+)$", block, re.M)[1]
                    self.assertIn("ci-guard", re.findall(r"[a-z0-9-]+", needs))
                    condition = re.search(r"^    if: (.+)$", block, re.M)[1]
                    self.assertIn("needs.ci-guard.outputs.verified == 'true'", condition)
            guard = jobs["ci-guard"]
            for line in ("      contents: read\n", "      actions: read\n",
                         "      verified: ${{ steps.guard.outputs.verified }}\n",
                         "      - id: guard\n", "        run: python3 scripts/ci_policy.py release-ci\n"):
                self.assertIn(line, guard)
            self.assertRegex(guard, r"\n    timeout-minutes: \d+\n")
            self.assertNotIn("id-token", guard)
            self.assertNotIn("write", guard)
        text, header, jobs = self.jobs("release.yml")
        self.assertIn("\n  workflow_dispatch:\n\npermissions:", header)
        self.assertNotIn("inputs:", header)
        self.assertIn("    needs: identity\n", jobs["ci-guard"])
        self.assertIn("    timeout-minutes: 150\n", jobs["ci-guard"])
        self.assertIn("CI_GUARD_MODE: ${{ startsWith(github.ref, 'refs/tags/v') && 'enforce' || 'report' }}\n",
                      jobs["ci-guard"])
        for name in gated["release.yml"]:
            self.assertIn(f"    if: {GUARD_IF}\n", jobs[name])
        for name in ("identity", "clients", "native", "linux-arm64", "manylinux", "image-test"):
            self.assertNotIn("ci-guard", jobs[name])
        for kept in ("python tests/release_assets.py", "python tests/wheel_platform.py",
                     "bindings/release_assets.py", "tests/dashboard_e2e.py"):
            self.assertIn(kept, text)
        text, _, jobs = self.jobs("pypi.yml")
        condition = ("github.event_name == 'workflow_dispatch' || (github.event.workflow_run.conclusion == "
                     "'success' && startsWith(github.event.workflow_run.head_branch, 'v'))")
        self.assertIn(f"    if: {condition}\n", jobs["ci-guard"])
        self.assertIn(f"    if: ({condition}) && needs.ci-guard.outputs.verified == 'true'\n", jobs["publish"])
        self.assertIn("    timeout-minutes: 15\n", jobs["ci-guard"])
        for line in ("CI_GUARD_MODE: enforce", "CI_WAIT_MINUTES: 0", "CI_MISSING_GRACE_MINUTES: 0",
                     "CI_TAG: ${{ github.event.inputs.tag || github.event.workflow_run.head_branch }}",
                     "CI_SHA: ${{ github.event.workflow_run.head_sha }}"):
            self.assertIn(f"          {line}\n", jobs["ci-guard"])
        self.assertIn("      id-token: write\n", jobs["publish"])
        source = (ROOT / "scripts/ci_policy.py").read_text(encoding="utf-8")
        table = source.rsplit("\n", 2)[-2]
        for entry in ('"route": route', '"image-ci": image_ci', '"release-ci": '):
            self.assertIn(entry, table)

    def test_b07_job_counts_must_match_the_collected_jobs(self):
        jobs = guard_jobs()
        self.assertEqual(len(jobs), 18)
        for name, pages in (("total_count 19", [dict(total_count=19, jobs=jobs)]),
                            ("no total_count", [dict(jobs=jobs[:9]), dict(jobs=jobs[9:])]),
                            ("total_count text", [dict(total_count="18", jobs=jobs)])):
            for mode in ("enforce", "report"):
                with self.subTest(pages=name, mode=mode):
                    github = FakeGitHub([[ci_run()]])
                    job_pages(github, pages)
                    code, output, summary, _ = self.guard(github, CI_GUARD_MODE=mode, GITHUB_REF="refs/heads/release")
                    self.assertEqual((code, output), (1, "verified=false\n"))
                    self.assertIn("verdict: error", summary)
                    self.assertEqual(sum("/jobs?" in call for call in github.calls), 3)
        github = FakeGitHub([[ci_run()]])
        job_pages(github, [dict(total_count=18, jobs=jobs[:9]), dict(total_count=18, jobs=jobs[9:])])
        code, output, _, _ = self.guard(github)
        self.assertEqual((code, output), (0, "verified=true\n"))
        self.assertIn("verdict: qualified", self.report(github))

    def test_b03_retry_backoff_never_sleeps_past_the_active_deadline(self):
        pending = ci_run(status="in_progress", conclusion=None)
        for name, runs, fail_at, expected in (
            # The reviewer's case: a transient failure exactly at the pending deadline.
            ("pending, fails at the deadline", [pending], [2], [(1000, 60)]),
            # After a `missing` verdict, the active deadline is the grace, not the wait.
            ("missing, fails at the grace", [], [2], [(1000, 60)]),
        ):
            with self.subTest(case=name):
                github = FakeGitHub([runs])
                listings = [0]
                original = github.api

                def api(endpoint, github=github, original=original, listings=listings, fail_at=fail_at):
                    if endpoint == RUNS:
                        listings[0] += 1
                        if listings[0] in fail_at:
                            raise RuntimeError("HTTP 502")
                    return original(endpoint)
                github.api = api
                wait = "2" if runs == [] else "1"
                self.assert_failed(github, CI_WAIT_MINUTES=wait, CI_MISSING_GRACE_MINUTES="1")
                self.assertEqual(github.sleeps, expected)
                self.assertEqual(github.now, 1060)
                self.assertEqual(listings[0], 3)
        # Before the first verdict the deadline is the overall bound: a lookup failing 3 s before
        # it sleeps 3 s, not 5, and the next retry does not sleep at all.
        github = FakeGitHub([[pending]])
        original, tries = github.api_object, []

        def late(endpoint):
            if endpoint == f"repos/{REPO}/actions/workflows/ci.yml":
                tries.append(github.now)
                if len(tries) == 1:
                    github.now = 1057
                if len(tries) < 3:
                    raise RuntimeError("HTTP 502")
            return original(endpoint)
        github.api_object = late
        self.assert_failed(github, CI_WAIT_MINUTES="1", CI_MISSING_GRACE_MINUTES="1")
        self.assertEqual(github.sleeps, [(1057, 3)])
        self.assertEqual(tries, [1000, 1060, 1060])
        self.assertEqual((github.now, github.evaluations), (1060, 1))

    def test_b07_with_no_wait_retries_are_immediate(self):
        for errors, code in (([RuntimeError("HTTP 502")] * 2, 0), ([RuntimeError("HTTP 502")] * 3, 1)):
            with self.subTest(errors=len(errors)):
                github = FakeGitHub([[ci_run()]])
                tries = []
                failing = flaky(github.api, errors)
                github.api = lambda endpoint: tries.append(endpoint) or failing(endpoint)
                result, output, _, _ = self.guard(github, CI_WAIT_MINUTES="0", CI_MISSING_GRACE_MINUTES="0")
                self.assertEqual(result, code)
                self.assertEqual(output, "verified=true\n" if code == 0 else "verified=false\n")
                self.assertEqual(github.sleeps, [])
                self.assertEqual(github.now, 1000)
                self.assertEqual(tries.count(RUNS), 3)

    def test_b07_a_malformed_run_or_refetch_fails_closed_without_retry(self):
        # Missing fields in a well-formed response are terminal: no retry, no sleep, no pass.
        for name, github in (
            ("listed run without status", FakeGitHub([[{k: v for k, v in ci_run().items() if k != "status"}]])),
            ("refetch without run_attempt", FakeGitHub(
                [[ci_run()]], refetch={5: {k: v for k, v in ci_run().items() if k != "run_attempt"}})),
            ("job without conclusion", FakeGitHub([[ci_run()]], jobs={(5, 1): [dict(name="CI required")]})),
        ):
            with self.subTest(case=name):
                message = self.assert_failed(github)
                self.assertIn("verdict: error", message)
                self.assertEqual((github.evaluations, github.sleeps), (1, []))

    def test_b12_pypi_qualifies_every_wheel_source_of_the_checksummed_manifest(self):
        env = dict(CI_TAG="v0.7.4", CI_SHA="", CI_WAIT_MINUTES="0", CI_MISSING_GRACE_MINUTES="0")
        other_runs = f"repos/{REPO}/actions/workflows/ci.yml/runs?head_sha={OTHER}&per_page=100"
        folder = self.directory / "plain"
        digest = release_files(folder, [asset(WHEEL), asset("vlite-0.7.4-linux.zip", b"zip", source_commit=OTHER)])
        github = FakeGitHub([[ci_run()]], tags={"v0.7.4": SHA})
        code, output, _, _ = self.guard(github, CI_MANIFEST_DIR=str(folder), **env)
        self.assertEqual((code, output), (0, f"verified=true\nmanifest_sha256={digest}\n"))
        self.assertNotIn(other_runs, github.calls)
        # Report mode never hands a manifest digest to a publisher.
        self.assertIn("verdict: qualified", self.report(FakeGitHub([[ci_run()]], tags={"v0.7.4": SHA}),
                                                        CI_MANIFEST_DIR=str(folder), **env))
        # A corrective wheel built at OTHER needs its own qualifying run.
        folder = self.directory / "corrective"
        digest = release_files(folder, [asset(WHEEL, source_commit=OTHER), asset("vlite_embedded-0.7.4.whl", b"e")])
        for name, runs, reason in (
            ("no run", [], "verdict: missing"),
            ("pending", [ci_run(id=6, head_sha=OTHER, status="in_progress", conclusion=None)], "verdict: pending"),
            ("failed", [ci_run(id=6, head_sha=OTHER, conclusion="failure")], "conclusion failure"),
        ):
            for ci_sha in ("", SHA):
                with self.subTest(source=name, ci_sha=ci_sha):
                    github = FakeGitHub([[ci_run()]], tags={"v0.7.4": SHA})
                    at_sources(github, {OTHER: runs})
                    message = self.assert_failed(github, CI_MANIFEST_DIR=str(folder), **(env | dict(CI_SHA=ci_sha)))
                    self.assertIn(reason, message)
                    self.assertIn(OTHER, message)
                    self.assertEqual(github.calls.count(RUNS), 1)
                    self.assertEqual(github.sleeps, [])
        github = FakeGitHub([[ci_run()]], tags={"v0.7.4": SHA})
        at_sources(github, {OTHER: [ci_run(id=6, head_sha=OTHER)]})
        code, output, summary, _ = self.guard(github, CI_MANIFEST_DIR=str(folder), **env)
        self.assertEqual((code, output), (0, f"verified=true\nmanifest_sha256={digest}\n"))
        self.assertIn(f"qualified for {SHA}", summary)
        self.assertIn(f"qualified for {OTHER}", summary)
        self.assertIn(f"repos/{REPO}/actions/runs/6/attempts/1/jobs?per_page=100", github.calls)

    def test_b12_a_malformed_or_mismatched_manifest_fails_closed(self):
        env = dict(CI_TAG="v0.7.4", CI_WAIT_MINUTES="0", CI_MISSING_GRACE_MINUTES="0")
        wheel = asset(WHEEL)
        good = dict(version="0.7.4", source_commit=SHA, assets=[wheel])
        cases = (
            ("manifest not in SHA256SUMS", dict(sums=lambda lines: lines[:-1])),
            ("manifest checksum mismatch", dict(sums=lambda lines: lines[:-1] + ["0" * 64 + "  release-manifest.json"])),
            ("malformed SHA256SUMS", dict(sums=lambda lines: lines + ["not a checksum line"])),
            ("SHA256SUMS path", dict(sums=lambda lines: [line.replace(WHEEL, "dir/" + WHEEL) for line in lines])),
            ("wheel not in SHA256SUMS", dict(sums=lambda lines: lines[1:])),
            ("version mismatch", dict(manifest=good | dict(version="0.7.5"))),
            ("default source is not the tag commit", dict(manifest=good | dict(source_commit=OTHER))),
            ("default source missing", dict(manifest={k: v for k, v in good.items() if k != "source_commit"})),
            ("asset hash mismatch", dict(manifest=good | dict(assets=[wheel | dict(sha256="0" * 64)]))),
            ("malformed asset source", dict(manifest=good | dict(assets=[wheel | dict(source_commit="abc")]))),
            ("upper-case asset source", dict(manifest=good | dict(assets=[wheel | dict(source_commit=OTHER.upper())]))),
            ("repeated asset", dict(manifest=good | dict(assets=[wheel, wheel]))),
            ("asset not an object", dict(manifest=good | dict(assets=[wheel, WHEEL]))),
            ("no wheels", dict(manifest=good | dict(assets=[]))),
            ("assets not a list", dict(manifest=good | dict(assets={WHEEL: wheel}))),
            ("manifest not an object", dict(manifest=[good])),
        )
        for name, fault in cases:
            with self.subTest(fault=name):
                folder = self.directory / name.replace(" ", "-")
                release_files(folder, [wheel], **fault)
                github = FakeGitHub([[ci_run()]], tags={"v0.7.4": SHA})
                self.assertIn("verdict: error", self.assert_failed(github, CI_MANIFEST_DIR=str(folder), **env))
        folder = self.directory / "json"
        release_files(folder, [wheel])
        (folder / "release-manifest.json").write_text("{not json", encoding="utf-8")
        data = (folder / "release-manifest.json").read_bytes()
        (folder / "SHA256SUMS").write_text(f"{wheel['sha256']}  {WHEEL}\n"
                                           f"{hashlib.sha256(data).hexdigest()}  release-manifest.json\n")
        for name, extra in (("invalid JSON", dict(CI_MANIFEST_DIR=str(folder))),
                            ("no files", dict(CI_MANIFEST_DIR=str(self.directory / "absent"))),
                            ("no tag", dict(CI_MANIFEST_DIR=str(folder), CI_TAG=None))):
            with self.subTest(fault=name):
                github = FakeGitHub([[ci_run()]], tags={"v0.7.4": SHA})
                self.assertIn("verdict: error", self.assert_failed(github, **(env | extra)))
                self.assertNotIn(RUNS, github.calls)

    def test_b12_pypi_uploads_only_wheels_of_the_guarded_manifest(self):
        text, _, jobs = self.jobs("pypi.yml")
        guard, publish = jobs["ci-guard"], jobs["publish"]
        self.assertIn("      manifest_sha256: ${{ steps.guard.outputs.manifest_sha256 }}\n", guard)
        self.assertIn("          CI_MANIFEST_DIR: release-manifest\n", guard)
        self.assertRegex(guard, r"\n {10}timeout \d+ gh release download \"\$TAG\" .*--dir release-manifest "
                                r"--pattern release-manifest\.json --pattern SHA256SUMS\n")
        self.assertLess(guard.index("gh release download"), guard.index("      - id: guard\n"))
        self.assertIn("--pattern '*.whl' --pattern release-manifest.json --pattern SHA256SUMS", publish)
        self.assertIn("          MANIFEST_SHA256: ${{ needs.ci-guard.outputs.manifest_sha256 }}\n", publish)
        self.assertNotIn("CI_MANIFEST_DIR", (ROOT / ".github/workflows/release.yml").read_text(encoding="utf-8"))
        script = textwrap.dedent(re.search(r"shell: python\n.*?\n        run: \|\n(.*?)\n      - uses: pypa/",
                                           publish, re.S)[1])
        wheels = {WHEEL: b"client", "vlite_client-0.7.4-cp312-linux_x86_64.whl": b"linux"}
        for name, change, ok in (
            ("selected", {}, True),
            ("guard saw another manifest", dict(guarded="0" * 64), False),
            ("no guarded manifest", dict(guarded=""), False),
            ("wheel not in the manifest", dict(unlisted=True), False),
            ("wheel differs from its manifest entry", dict(entry=b"other"), False),
        ):
            with self.subTest(case=name):
                folder = self.directory / name.replace(" ", "-")
                assets = [asset(wheel, content) for wheel, content in wheels.items()]
                if change.get("unlisted"):
                    assets = assets[1:]
                if "entry" in change:
                    assets[0] = asset(WHEEL, change["entry"])
                digest = release_files(folder / "release", assets)
                sums = (folder / "release" / "SHA256SUMS").read_text()
                for wheel, content in wheels.items():
                    (folder / "release" / wheel).write_bytes(content)
                    digest_line = f"{hashlib.sha256(content).hexdigest()}  {wheel}\n"
                    if digest_line not in sums:
                        sums += digest_line
                (folder / "release" / "SHA256SUMS").write_text(sums)
                done = policy.subprocess.run(
                    [policy.sys.executable, "-c", script], cwd=folder, capture_output=True, text=True,
                    env=os.environ | dict(TAG="v0.7.4", PREFIX="vlite_client",
                                          MANIFEST_SHA256=change.get("guarded", digest)))
                self.assertEqual(done.returncode == 0, ok, done.stderr)
                if ok:
                    self.assertEqual(sorted(path.name for path in (folder / "pypi-dist").iterdir()), [WHEEL])


if __name__ == "__main__":
    unittest.main()
