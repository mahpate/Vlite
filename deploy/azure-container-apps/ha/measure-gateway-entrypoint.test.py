"""Offline contract tests. Run: python -B measure-gateway-entrypoint.test.py."""

import base64
import contextlib
import copy
import gzip
import importlib.util
import io
import json
import os
from pathlib import Path
import signal
import stat
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch


sys.dont_write_bytecode = True
SPEC = importlib.util.spec_from_file_location(
    "gateway_entrypoint", Path(__file__).with_name("measure-gateway-entrypoint.py"))
runtime = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(runtime)


class RuntimeTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="vlite-runtime-test-")
        self.addCleanup(self.temporary.cleanup)
        self.work = Path(self.temporary.name)
        self.work.chmod(0o700)
        self.root = self.work / "bootstrap"
        self.target = self.work / "target.json"
        self.put(self.target, {"account": "vlitemeast31st", "container": "control-data",
            "coordination_container": "control-coordination", "prefix": "isolated-run/",
            "auth": "managed_identity", "client_id": "private-client-identity"})
        self.config = {"operation": "clock", "role": "driver", "work_dir": str(self.work),
            "source_revision": "a" * 40, "source_dirty_paths": "", "timeout_secs": 60,
            "server": str(self.work / "vlite-server"),
            "load_program": str(self.work / "ha-meas004"),
            "control_program": str(self.work / "ha-meas005")}
        self.env = patch.dict(os.environ, {
            "VLITE_RESOURCE_SOURCE_REVISION": "a" * 40,
            "VLITE_RESOURCE_SOURCE_DIRTY_PATHS": "",
            "CONTAINER_APP_REPLICA_NAME": "replica-one"})
        self.env.start()
        self.addCleanup(self.env.stop)

    def put(self, path, value):
        if isinstance(value, (dict, list)):
            value = json.dumps(value).encode()
        elif isinstance(value, str):
            value = value.encode()
        path.write_bytes(value)
        path.chmod(0o600)
        return str(path)

    def config_for(self, operation, role="driver", **extra):
        return {**self.config, "operation": operation, "role": role, **extra}

    def fixture(self, with_metadata=True):
        self.root.mkdir(mode=0o700)
        control = {"target_file": str(self.target), "object": "gateway/control.json"}
        gateway = {"bind": "0.0.0.0:8080", "allow_remote": True,
            "control_state": control, "s_ctl_ms": 30000, "poll_interval_ms": 100,
            "max_replicas": 2, "public_key_bundle_file": str(self.root / "public.json"),
            "signing_key_file": str(self.root / "signing.key"), "active_kid": "measurement",
            "envelope_lifetime_ms": 60000, "readiness_propagation_ms": 100,
            "drain_timeout_ms": 5000, "max_request_bytes": 65536, "max_in_flight": 4096,
            "upstream_timeout_ms": 5000, "max_buckets": 1024}
        trust = {"audience": "measurement-writer", "control_state": control,
                 "s_ctl_ms": 30000, "poll_interval_ms": 100, "skew_ms": 1000,
                 "max_lifetime_ms": 60000, "readiness_propagation_ms": 100,
                 "public_key_bundle_file": str(self.root / "public.json")}
        for name in runtime.BASE_FILES["driver"]:
            if name != "measurement.json":
                self.put(self.root / name, {})
        self.put(self.root / "gateway.json", gateway)
        self.put(self.root / "trust.json", trust)
        self.put(self.root / "signing.key", "signing-seed-never-log")
        self.put(self.root / "backend.token", "backend-secret-never-log")
        self.put(self.root / "load-0.token", "load-secret-never-log")
        self.put(self.root / "load-1.token", "revoke-secret-never-log")
        self.put(self.root / "suspension-0.token", "suspension-secret-never-log")
        self.put(self.root / "database.vlite", b"nonsecret synthetic database fixture")
        if with_metadata:
            self.put(self.root / "measurement.json", self.metadata(
                b"nonsecret synthetic database fixture"))
        self.put(self.root / "manifest.json", {
            "schema": "vlite-gateway-measure-bootstrap/1", "lineage": 123,
            "writer_url": "https://writer.internal",
            "path": "/v1/collections/probe/vectors/1",
            "credentials": [
                {"purpose": purpose, "id": f"credential-{index}", "tenant": tenant,
                 "token_file": str(self.root / name)}
                for index, (purpose, tenant, name) in enumerate((
                    ("load", "tenant-load", "load-0.token"),
                    ("revocation", "tenant-load", "load-1.token"),
                    ("suspension", "tenant-suspension", "suspension-0.token")))]})
        self.put(self.root / "directory.json", [
            {"tenant": tenant, "kind": "dedicated", "audience": "measurement-writer",
             "authority_mode": "off", "read_routing": "writer_only", "lineage": 123,
             "writer": "https://writer.internal"}
            for tenant in ("tenant-load", "tenant-suspension")])
        (self.root / "admin").mkdir(mode=0o700)
        self.put(self.root / "admin" / "credentials.json", {"private": "credential-hashes"})
        self.put(self.root / "admin" / "tenants.json", {})
        self.put(self.root / "admin" / "audit.jsonl", b"")
        return gateway

    def metadata(self, fixture=b"fixture"):
        return {"schema": "vlite-meas-artifacts/1", "source": runtime.source(self.config),
                "fixture_sha256": runtime.digest(fixture), "lineage": 123}

    def report(self, schema="vlite-meas005-clock/1", **extra):
        result = {"schema": schema, "qualification": "proposal_evidence_only",
                  "source": runtime.source(self.config)}
        if schema.endswith("clock/1"):
            result.update(start_unix_ms=1000, end_unix_ms=1001, elapsed_ms=1)
        elif schema == "vlite-meas004/1":
            result.update(order="ABBA", windows=[{}])
        elif schema == "vlite-meas005/1":
            result.update(stage="mutations", complete=False,
                          mutation_and_ingress_checks_complete=True, mutations=[])
        else:
            result.update(replica="replica-one", role="gateway", boot_id="boot-one",
                          owns_serving_child=True, terminated_early=False,
                          interrupted=False, events=[])
        return runtime.encode({**result, **extra})

    def call(self, config, code=0, output=None):
        if output is None:
            output = self.report()
        with patch.object(runtime, "run_native", return_value=(code, output)) as native:
            result = runtime.Runtime(config).execute()
        return result, native

    def fail(self, config, pattern="invalid|refused|required|mismatch", output=None, code=0):
        with patch.object(runtime, "run_native", return_value=(code, output or b"{}")) as native:
            with self.assertRaisesRegex(runtime.Failure, pattern):
                runtime.Runtime(config).execute()
        return native

    def test_clock_exact_native_config_and_program(self):
        def native(argv, timeout, work):
            self.assertEqual(argv[:2], [Path(self.config["control_program"]), "--config"])
            self.assertEqual(runtime.read_json(argv[2]), {"mode": "clock_sample"})
            self.assertEqual(timeout, 60)
            self.assertEqual(work, self.work)
            return 0, self.report()
        with patch.object(runtime, "run_native", side_effect=native):
            code, data = runtime.Runtime(self.config).execute()
        self.assertEqual(code, 0)
        self.assertEqual(runtime.verify_frame(runtime.frame(data)), data)
        self.assertFalse(any(self.work.glob("native-*")))

    def test_strict_outer_config_and_provenance(self):
        for override in ({"unknown": True}, {"timeout_secs": 0}, {"timeout_secs": True},
                         {"source_revision": "A" * 40}, {"role": "bogus"},
                         {"source_dirty_paths": " M src/private.rs"}):
            with self.subTest(override=override), self.assertRaises(runtime.Failure):
                runtime.Runtime({**self.config, **override})
        with patch.dict(os.environ, {"VLITE_RESOURCE_SOURCE_DIRTY_PATHS": " M x"}):
            with self.assertRaisesRegex(runtime.Failure, "source_environment"):
                runtime.Runtime(self.config)
        with patch.dict(os.environ, {}, clear=True):
            with self.assertRaisesRegex(runtime.Failure, "source_environment"):
                runtime.Runtime(self.config)
        with self.assertRaises(runtime.Failure):
            runtime.json_bytes(b'{"operation":"clock","operation":"bootstrap"}')
        with self.assertRaises(runtime.Failure):
            runtime.json_bytes(b'{"value":NaN}')

    def test_role_separation_is_pre_execution(self):
        for role in ("gateway", "writer", "reader"):
            for operation in ("bootstrap", "publish", "export", "mutate", "load", "analyze"):
                native = self.fail(self.config_for(operation, role), "role_refused")
                native.assert_not_called()
        self.fail(self.config_for("serve"), "role_refused")
        self.fail(self.config_for("observe"), "role_refused")
        self.fail(self.config_for("copy_fixture", "gateway"), "role_refused")

    def test_bootstrap_exact_arguments_and_no_retry(self):
        native_path = self.work / "bootstrap-config.json"
        self.put(native_path, {"output_dir": str(self.root),
            "control_state": {"target_file": str(self.target), "object": "gateway/control.json"},
            "writer_url": "https://writer.internal", "audience": "measurement-writer",
            "poll_interval_ms": 100, "max_replicas": 2, "revocations": 1})
        config = self.config_for("bootstrap", native_config=str(native_path))

        def native(argv, timeout, work):
            self.assertEqual(argv, [Path(self.config["server"]), "gateway-measure",
                                   "bootstrap", "--config", native_path])
            self.fixture(with_metadata=False)
            return 0, runtime.encode({"schema": "vlite-gateway-measure-bootstrap/1",
                "qualification": "proposal_evidence_only", "lineage": 123,
                "output_dir": str(self.root),
                "control_state_published": False, "authority_mode": "off",
                "read_routing": "writer_only"})
        with patch.object(runtime, "run_native", side_effect=native) as process:
            code, data = runtime.Runtime(config).execute()
            self.assertEqual(code, 0)
            self.assertEqual(json.loads(data)["fixture_sha256"],
                             runtime.digest((self.root / "database.vlite").read_bytes()))
            self.assertEqual(process.call_count, 1)
        self.assertTrue((self.work / "bootstrap.attempted").is_file())
        self.fail(config, "bootstrap_exists")

    def test_bootstrap_failure_unknown_not_repeated(self):
        path = self.work / "bootstrap-config.json"
        self.put(path, {"output_dir": str(self.root),
            "control_state": {"target_file": str(self.target), "object": "gateway/control.json"},
            "writer_url": "https://writer.internal", "audience": "measurement-writer"})
        config = self.config_for("bootstrap", native_config=str(path))
        process = self.fail(config, "unknown_do_not_retry", code=1,
                            output=b"native error contains private secrets")
        self.assertEqual(process.call_count, 1)
        process = self.fail(config, "unknown_do_not_retry")
        process.assert_not_called()

    def test_publish_exact_contract_and_uncertain_once(self):
        self.fixture()
        config = self.config_for("publish", bootstrap_dir=str(self.root))
        (code, data), process = self.call(config, output=runtime.encode({
            "generation": 1, "object": "gateway/control.json"}))
        self.assertEqual(code, 0)
        self.assertTrue(json.loads(data)["control_state_published"])
        self.assertEqual(process.call_args.args[0], [
            Path(self.config["server"]), "control-state", "publish",
            "--target-file", self.target, "--object", "gateway/control.json",
            "--admin-dir", self.root / "admin", "--directory-file", self.root / "directory.json"])
        self.fail(config, "unknown_do_not_retry").assert_not_called()

    def test_publish_failures_never_republish(self):
        self.fixture()
        config = self.config_for("publish", bootstrap_dir=str(self.root))
        self.fail(config, "unknown_do_not_retry", code=1)
        self.fail(config, "unknown_do_not_retry").assert_not_called()

    def test_bad_topology_or_shared_key_refused(self):
        self.fixture()
        config = self.config_for("publish", bootstrap_dir=str(self.root))
        self.put(self.root / "directory.json", [{"read_routing": "reader"}] * 2)
        self.fail(config, "fixture_topology_invalid").assert_not_called()
        target = runtime.read_json(self.target)
        target["auth"] = "emulator_shared_key"
        self.put(self.target, target)
        self.fail(config, "managed_identity_required").assert_not_called()

    def test_export_roles_private_bounded_and_no_database(self):
        self.fixture()
        for role in runtime.BASE_FILES:
            with self.subTest(role=role):
                output = self.work / (role + "-secrets.json")
                code, receipt = runtime.Runtime(self.config_for("export",
                    bootstrap_dir=str(self.root), export_role=role, generation="g1",
                    output_file=str(output))).execute()
                manifest = runtime.read_json(output, 1024 * 1024)
                self.assertEqual(code, 0)
                self.assertNotIn(b"secret-never-log", receipt)
                self.assertNotIn(b"signing-seed-never-log", receipt)
                self.assertNotIn(b'"value"', receipt)
                self.assertLessEqual(len(manifest["secrets"]), 64)
                for index, secret in enumerate(manifest["secrets"], 1):
                    self.assertEqual(secret["name"], f"m-{role}-g1-{index}")
                    self.assertLessEqual(len(secret["value"]), 3000)
                compressed = base64.b64decode("".join(s["value"] for s in manifest["secrets"]))
                self.assertEqual(runtime.digest(compressed), manifest["sha256"])
                files = json.loads(gzip.decompress(compressed))["files"]
                self.assertNotIn("database.vlite", files)
                if role != "driver":
                    self.assertFalse(any(name.startswith("admin/") for name in files))
                    self.assertNotIn("load-0.token", files)
                if role in ("writer", "reader"):
                    self.assertNotIn("signing.key", files)
                    self.assertNotIn("gateway.json", files)
                if os.name != "nt":
                    self.assertEqual(stat.S_IMODE(output.stat().st_mode), 0o600)

    def bundle(self, role="writer", **overrides):
        files = {name: base64.b64encode(b"{}").decode() for name in runtime.BASE_FILES[role]}
        files["measurement.json"] = base64.b64encode(runtime.encode(self.metadata())).decode()
        return {"schema": "vlite-meas-bundle/1", "role": role, "generation": "g1",
                "bootstrap_dir": str(self.root), "source": runtime.source(self.config),
                "files": files, **overrides}

    def restore_config(self, bundle):
        compressed = gzip.compress(runtime.encode(bundle), mtime=0)
        payload = base64.b64encode(compressed)
        paths = []
        for i in range(0, len(payload), 3000):
            paths.append(self.put(self.work / f"part-{i}", payload[i:i + 3000]))
        return self.config_for("restore", "writer", bootstrap_dir=str(self.root),
            generation="g1", sha256=runtime.digest(compressed), projection_files=paths)

    def test_restore_verified_and_create_only(self):
        config = self.restore_config(self.bundle())
        code, _ = runtime.Runtime(config).execute()
        self.assertEqual(code, 0)
        self.assertEqual(set(p.name for p in self.root.iterdir()), runtime.BASE_FILES["writer"])
        with self.assertRaises(FileExistsError):
            runtime.Runtime(config).execute()

    def test_export_restore_and_backend_fixture_digest_round_trip(self):
        self.fixture()
        output = self.work / "private-projections.json"
        runtime.Runtime(self.config_for("export", bootstrap_dir=str(self.root),
            export_role="writer", generation="g1", output_file=str(output))).execute()
        exported = runtime.read_json(output, 1024 * 1024)
        paths = [self.put(self.work / secret["name"], secret["value"])
                 for secret in exported["secrets"]]
        original = self.work / "original"
        self.root.rename(original)
        runtime.Runtime(self.config_for("restore", "writer", bootstrap_dir=str(self.root),
            projection_files=paths, sha256=exported["sha256"], generation="g1")).execute()
        metadata = runtime.read_json(self.root / "measurement.json")
        runtime.Runtime(self.config_for("copy_fixture", "writer",
            fixture_file=str(original / "database.vlite"),
            fixture_sha256=metadata["fixture_sha256"],
            database_file=str(self.work / "database.vlite"))).execute()
        self.assertEqual((self.work / "database.vlite").read_bytes(),
                         (original / "database.vlite").read_bytes())
        self.assertFalse((self.root / "admin").exists())
        self.assertFalse((self.root / "signing.key").exists())
        self.assertFalse((self.root / "database.vlite").exists())

    def test_restore_checksum_role_generation_provenance_and_traversal(self):
        for override in ({"role": "driver"}, {"generation": "g2"},
                         {"source": None}, {"bootstrap_dir": str(self.work)}):
            config = self.restore_config(self.bundle(**override))
            self.fail(config, "secret_bundle_identity")
            self.assertFalse(self.root.exists())
        for name in ("signing.key", "admin/credentials.json", "../escape", "/absolute"):
            bundle = self.bundle()
            bundle["files"][name] = base64.b64encode(b"private").decode()
            self.fail(self.restore_config(bundle), "role_artifact_refused")
        config = self.restore_config(self.bundle())
        config["sha256"] = "0" * 64
        self.fail(config, "secret_checksum")
        config = self.restore_config(self.bundle())
        self.put(Path(config["projection_files"][0]), b"not-base64!")
        self.fail(config, "secret_projection_invalid")

    def test_fixture_copy_private_digest_checked_and_never_overwritten(self):
        image = self.work / "image-fixture.vlite"
        data = b"nonsecret fixture"
        self.put(image, data)
        private = self.work / "writer"
        private.mkdir(mode=0o700)
        destination = private / "database.vlite"
        config = self.config_for("copy_fixture", "writer", fixture_file=str(image),
            fixture_sha256=runtime.digest(data), database_file=str(destination))
        code, _ = runtime.Runtime(config).execute()
        self.assertEqual(code, 0)
        self.assertEqual(destination.read_bytes(), data)
        with self.assertRaises(FileExistsError):
            runtime.Runtime(config).execute()
        config["fixture_sha256"] = "0" * 64
        self.fail(config, "fixture_checksum")
        self.assertEqual(destination.read_bytes(), data)

    def test_writer_reader_gateway_native_observer_argument_shapes(self):
        for role in ("gateway", "writer", "reader"):
            with self.subTest(role=role), tempfile.TemporaryDirectory(
                    prefix="role-", dir=self.work) as temporary:
                root = Path(temporary)
                root.chmod(0o700)
                for name in runtime.BASE_FILES[role]:
                    self.put(root / name, {})
                control = {"target_file": str(self.target), "object": "gateway/control.json"}
                self.put(root / ("gateway.json" if role == "gateway" else "trust.json"),
                         {"control_state": control,
                          "public_key_bundle_file": str(root / "public.json"),
                          **({"signing_key_file": str(root / "signing.key")}
                             if role == "gateway" else {})})
                self.put(root / "measurement.json", self.metadata())
                config = self.config_for("observe", role, bootstrap_dir=str(root), duration_secs=20)
                database = self.work / (role + ".vlite")
                if role != "gateway":
                    self.put(database, b"fixture")
                    config.update(database_file=str(database),
                                  fixture_sha256=runtime.digest(b"fixture"))

                def native(argv, timeout, work):
                    native_config = runtime.read_json(argv[2])
                    self.assertEqual(native_config["mode"], "observe")
                    self.assertEqual(native_config["role"], "gateway" if role == "gateway" else "backend")
                    self.assertEqual(native_config["ready_url"], "http://127.0.0.1:8080/ready")
                    args = native_config["server"]["args"]
                    if role == "gateway":
                        self.assertEqual(args, ["gateway", "--config", str(root / "gateway.json")])
                    else:
                        expected = [str(database), "--serve-mode", role]
                        if role == "writer":
                            expected += ["--durability", "full"]
                        expected += ["--bind", "0.0.0.0:8080", "--allow-remote",
                            "--token-file", str(root / "backend.token"),
                            "--gateway-trust-file", str(root / "trust.json")]
                        self.assertEqual(args, expected)
                        self.assertNotIn("--insecure-local", args)
                    return 0, self.report("vlite-meas005-observer/1",
                                          role=native_config["role"])
                with patch.object(runtime, "run_native", side_effect=native):
                    code, _ = runtime.Runtime(config).execute()
                self.assertEqual(code, 0)

    def test_serving_refuses_admin_and_missing_actual_identity(self):
        self.root.mkdir(mode=0o700)
        for name in runtime.BASE_FILES["gateway"]:
            self.put(self.root / name, {})
        self.put(self.root / "measurement.json", self.metadata())
        self.put(self.root / "admin.token", "private-admin")
        config = self.config_for("observe", "gateway", bootstrap_dir=str(self.root))
        self.fail(config, "role_artifact_refused").assert_not_called()
        (self.root / "admin.token").unlink()
        with patch.dict(os.environ, {}, clear=True):
            os.environ["VLITE_RESOURCE_SOURCE_REVISION"] = "a" * 40
            os.environ["VLITE_RESOURCE_SOURCE_DIRTY_PATHS"] = ""
            self.fail(config, "replica_identity_required").assert_not_called()

    def test_serve_preserves_interrupted_observer_exit_two(self):
        self.fixture()
        # A serving role gets its filtered projection, not the driver's tree.
        root = self.work / "gateway-role"
        root.mkdir(mode=0o700)
        for name in runtime.BASE_FILES["gateway"]:
            self.put(root / name, (self.root / name).read_bytes())
        gateway = runtime.read_json(root / "gateway.json")
        gateway.update(signing_key_file=str(root / "signing.key"),
                       public_key_bundle_file=str(root / "public.json"))
        self.put(root / "gateway.json", gateway)
        output = self.report("vlite-meas005-observer/1", interrupted=True)
        (code, data), _ = self.call(self.config_for("observe", "gateway",
            bootstrap_dir=str(root), duration_secs=20), code=2, output=output)
        self.assertEqual((code, data), (2, output))
        self.assertEqual(runtime.verify_frame(runtime.frame(data)), output)
        self.fail(self.config_for("observe", "gateway", bootstrap_dir=str(root),
                  duration_secs=20), "unknown_do_not_retry").assert_not_called()

    def test_backend_rejects_independent_image_fixture_even_with_matching_copy_hash(self):
        self.fixture()
        root = self.work / "writer-role"
        root.mkdir(mode=0o700)
        for name in runtime.BASE_FILES["writer"]:
            self.put(root / name, (self.root / name).read_bytes())
        database = self.work / "different-fixture.vlite"
        self.put(database, b"different independently bootstrapped image fixture")
        config = self.config_for("observe", "writer", bootstrap_dir=str(root),
            database_file=str(database), fixture_sha256=runtime.digest(database.read_bytes()),
            duration_secs=20)
        self.fail(config, "fixture_checksum").assert_not_called()

    def test_meas004_serves_real_server_without_finite_observer_window(self):
        self.fixture()
        root = self.work / "writer-role"
        root.mkdir(mode=0o700)
        for name in runtime.BASE_FILES["writer"]:
            self.put(root / name, (self.root / name).read_bytes())
        trust = runtime.read_json(root / "trust.json")
        trust["public_key_bundle_file"] = str(root / "public.json")
        self.put(root / "trust.json", trust)
        fixture = (self.root / "database.vlite").read_bytes()
        database = self.work / "writer.vlite"
        self.put(database, fixture)
        config = self.config_for("serve", "writer", bootstrap_dir=str(root),
            database_file=str(database), fixture_sha256=runtime.digest(fixture))
        (code, data), process = self.call(config, output=b"")
        self.assertEqual(code, 0)
        self.assertTrue(json.loads(data)["lifetime_complete"])
        self.assertEqual(process.call_args.args[1], 7200)
        self.assertEqual(process.call_args.kwargs, {"serving": True})
        self.assertEqual(process.call_args.args[0], [
            Path(self.config["server"]), str(database), "--serve-mode", "writer",
            "--durability", "full", "--bind", "0.0.0.0:8080", "--allow-remote",
            "--token-file", str(root / "backend.token"),
            "--gateway-trust-file", str(root / "trust.json")])
        self.fail(config, "unknown_do_not_retry").assert_not_called()

    def test_load_native_config_untouched(self):
        self.fixture()
        path = self.work / "load.json"
        native_config = {"gateway_config": str(self.root / "gateway.json"),
            "token_file": str(self.root / "load-0.token"),
            "expected_body_file": str(self.root / "load.expected.json"),
            "gateway_url": "https://gateway.internal", "direct_url": "https://writer.internal",
            "rates": [5], "duration_secs": 1, "warmup_secs": 0, "recovery_secs": 0,
            "max_in_flight": 4, "timeout_ms": 1000, "requested": {"routing": "writer_only"}}
        self.put(path, native_config)
        output = self.report("vlite-meas004/1")
        (code, data), process = self.call(self.config_for("load", native_config=str(path)),
                                        output=output)
        self.assertEqual((code, data), (0, output))
        self.assertEqual(process.call_args.args[0],
                         [Path(self.config["load_program"]), "--config", path])
        native_config["invented_native_flag"] = True
        self.put(path, native_config)
        self.fail(self.config_for("load", native_config=str(path))).assert_not_called()

    def test_mutation_unknown_exit_two_preserved_not_retried(self):
        self.fixture()
        path = self.work / "mutate.json"
        self.put(path, {"mode": "run", "server": self.config["server"],
            "bootstrap_dir": str(self.root), "target_file": str(self.target),
            "object": "gateway/control.json", "gateway_url": "https://gateway.internal",
            "poll_interval_ms": 100, "mutations": 3, "observation_deadline_ms": 1000,
            "seed": 1, "requested": {"routing": "writer_only", "reader_replicas": "0"}})
        output = self.report("vlite-meas005/1", mutation_and_ingress_checks_complete=False,
            mutations=[{"generation": None, "outcome": "unknown_do_not_retry"}])
        config = self.config_for("mutate", native_config=str(path))
        (code, data), process = self.call(config, code=2, output=output)
        self.assertEqual((code, data), (2, output))
        self.assertEqual(process.call_args.args[0],
                         [Path(self.config["control_program"]), "--config", path])
        self.assertEqual(runtime.verify_frame(runtime.frame(data)), output)
        self.assertNotIn("FAILED", runtime.frame(data))
        self.fail(config, "unknown_do_not_retry").assert_not_called()

    def test_analysis_without_reader_retains_unknown_clocks(self):
        mutation_path = self.work / "mutations.json"
        self.put(mutation_path, self.report("vlite-meas005/1"))
        replicas = []
        for role in ("gateway", "backend"):
            report_path = self.work / (role + "-report.json")
            self.put(report_path, self.report("vlite-meas005-observer/1", role=role, replica=role))
            replicas.append({"role": role, "replica": role, "report_file": str(report_path),
                             "clock_offset_ms": None, "clock_uncertainty_ms": None})
        config_path = self.work / "analyze.json"
        native_config = {"mode": "analyze", "mutation_evidence_file": str(mutation_path),
                         "replicas": replicas, "fixed_inventory_confirmed": True}
        self.put(config_path, native_config)
        output = self.report("vlite-meas005/1", stage="analysis", complete=False,
            replicas=[{"role": "backend", "successful_upper_ms": {"count": 0, "p99": None},
                       "logical_feed_polls": None, "physical_blob_GETs": None,
                       "samples": [{"latency_ms": None, "censoring": "clock_unknown"}]}])
        (code, data), process = self.call(self.config_for("analyze", native_config=str(config_path)),
                                        code=2, output=output)
        self.assertEqual((code, data), (2, output))
        self.assertEqual(process.call_args.args[0][-1], config_path)
        self.assertIsNone(json.loads(data)["replicas"][0]["successful_upper_ms"]["p99"])
        native_config["replicas"].append(copy.deepcopy(replicas[0]))
        self.put(config_path, native_config)
        self.fail(self.config_for("analyze", native_config=str(config_path)),
                  "duplicate_replica").assert_not_called()

    def test_evidence_schema_provenance_malformed_exit_and_secret_refusal(self):
        cases = [
            b"[]", b"not JSON secret", b'{"a":NaN}',
            self.report(source=None), self.report(source={"revision": "b" * 40, "dirty_paths": []}),
            self.report(unknown="private native diagnostics"),
            self.report(start_unix_ms=None), self.report(client_id="private-client-identity"),
            self.report(uncertainty={"token": "private"}),
            self.report(uncertainty={"description": "Bearer private"}),
        ]
        for data in cases:
            with self.subTest(data=data), self.assertRaises(runtime.Failure):
                runtime.validate_evidence(data, "vlite-meas005-clock/1", self.config, 0)
        with self.assertRaisesRegex(runtime.Failure, "native_exit_mismatch"):
            runtime.validate_evidence(self.report("vlite-meas005/1"),
                                      "vlite-meas005/1", self.config, 2)
        self.put(self.work / "sensitive.token", "load-secret-never-log")
        self.fail(self.config, "evidence_secret_detected",
                  output=self.report(uncertainty={"detail": "load-secret-never-log"}))

    def test_transport_corruption_truncation_sequence_and_bounded_gzip(self):
        data = self.report(uncertainty={"unknown": None})
        frame = runtime.frame(data)
        self.assertEqual(runtime.verify_frame(frame), data)
        lines = frame.splitlines()
        corrupt = lines.copy()
        corrupt[0] = corrupt[0].replace(runtime.digest(gzip.compress(data, mtime=0)), "0" * 64)
        for value in ("\n".join(corrupt), "\n".join(lines[:-1]),
                      "\n".join(lines + [lines[-1]]),
                      "\n".join([lines[0], lines[1].replace(" 1 ", " 2 "), *lines[2:]]),
                      frame + "VLITE-MEAS FAILED 1\n"):
            with self.subTest(value=value), self.assertRaises(runtime.Failure):
                runtime.verify_frame(value)
        with self.assertRaisesRegex(runtime.Failure, "decompression_limit"):
            runtime.gunzip(gzip.compress(b"x" * 101), 100)
        with self.assertRaisesRegex(runtime.Failure, "invalid_gzip"):
            runtime.gunzip(bytes.fromhex("1f8b08000000000000ff070000000000000000"), 100)

    def test_main_keeps_incomplete_json_but_never_native_diagnostics(self):
        path = self.work / "entrypoint.json"
        evidence = self.work / "incomplete.json"
        data = self.report("vlite-meas005/1", stage="analysis", complete=False, replicas=[])
        self.put(evidence, data)
        self.put(path, self.config_for("transport", evidence_file=str(evidence),
            evidence_schema="vlite-meas005/1", native_exit_code=2))
        stdout, stderr = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stderr):
            code = runtime.main(["--config", str(path)])
        self.assertEqual(code, 2)
        self.assertEqual(runtime.verify_frame(stdout.getvalue()), data)
        self.assertEqual(stderr.getvalue(), "")
        self.put(evidence, b"malformed secret native diagnostics")
        stdout, stderr = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stderr):
            code = runtime.main(["--config", str(path)])
        self.assertEqual(code, 1)
        self.assertEqual(stdout.getvalue(), "VLITE-MEAS FAILED 1\n")
        self.assertNotIn("secret", stderr.getvalue())

    def test_process_io_is_bounded_and_secret_environment_removed(self):
        class FakeChild:
            pid = 123456789
            returncode = 0
            stdout = io.BytesIO(b'{"result":true}')
            stderr = io.BytesIO(b"private-native-diagnostic")

            def poll(self):
                return 0

            def wait(self, timeout):
                return 0
        with patch.dict(os.environ, {"VLITE_ADMIN_DIR": "private-admin",
                                     "VLITE_AUTH_TOKEN": "private-token"}):
            with patch.object(subprocess, "Popen", return_value=FakeChild()) as process:
                with patch.object(os, "killpg", create=True):
                    code, data = runtime.run_native(["absolute-program"], 1, self.work)
        self.assertEqual((code, data), (0, b'{"result":true}'))
        env = process.call_args.kwargs["env"]
        self.assertNotIn("VLITE_ADMIN_DIR", env)
        self.assertNotIn("VLITE_AUTH_TOKEN", env)
        self.assertEqual(env["VLITE_RESOURCE_SOURCE_REVISION"], "a" * 40)
        self.assertEqual(process.call_args.kwargs["stdin"], subprocess.DEVNULL)

    def test_process_timeout_and_output_limit_kill_and_reap(self):
        for oversized in (False, True):
            class FakeChild:
                pid = 123456789
                returncode = None
                stdout = io.BytesIO(b"x" * (runtime.REPORT_LIMIT + 1) if oversized else b"")
                stderr = io.BytesIO()

                def __init__(self):
                    self.killed = False
                    self.waited = False

                def poll(self):
                    return None

                def kill(self):
                    self.killed = True

                def wait(self, timeout):
                    self.waited = True
                    return 1
            child = FakeChild()
            with patch.object(subprocess, "Popen", return_value=child):
                with patch.object(os, "killpg", create=True) as killgroup:
                    with self.assertRaisesRegex(runtime.Failure, "native_timeout|native_output_limit"):
                        runtime.run_native(["absolute-program"], 0, self.work)
            self.assertTrue(child.waited)
            if os.name != "nt":
                killgroup.assert_called_once_with(child.pid, signal.SIGKILL)
            else:
                self.assertTrue(child.killed)

    def test_serving_discards_native_logs_and_reaps_at_window_end(self):
        class FakeChild:
            pid = 123456789
            returncode = None

            def poll(self):
                return None

            def kill(self):
                pass

            def wait(self, timeout):
                return -9
        with patch.object(subprocess, "Popen", return_value=FakeChild()) as process:
            with patch.object(os, "killpg", create=True):
                self.assertEqual(runtime.run_native(["absolute-program"], 0, self.work,
                                                     serving=True), (0, b""))
        self.assertEqual(process.call_args.kwargs["stdout"], subprocess.DEVNULL)
        self.assertEqual(process.call_args.kwargs["stderr"], subprocess.DEVNULL)
        with patch.object(FakeChild, "poll", return_value=0):
            with patch.object(subprocess, "Popen", return_value=FakeChild()):
                with patch.object(os, "killpg", create=True):
                    with self.assertRaisesRegex(runtime.Failure, "serving_child_terminated"):
                        runtime.run_native(["absolute-program"], 60, self.work, serving=True)

    def test_limits_and_unsafe_paths(self):
        too_large = self.work / "oversize"
        self.put(too_large, b"x" * (runtime.CONFIG_LIMIT + 1))
        with self.assertRaisesRegex(runtime.Failure, "file_limit"):
            runtime.read_bytes(too_large)
        with self.assertRaises(runtime.Failure):
            runtime.absolute("relative.json")
        with self.assertRaises(runtime.Failure):
            runtime.absolute(str(self.work / ".." / "outside-private-work"))
        with self.assertRaises(runtime.Failure):
            runtime.generation("g-1")
        for url in ("http://writer.internal", "https://user:secret@writer.internal",
                    "https://writer.internal/route", "https://writer.internal?sig=secret"):
            with self.assertRaises(runtime.Failure):
                runtime.origin(url)
        if os.name != "nt":
            public = self.work / "public"
            public.mkdir(mode=0o755)
            with self.assertRaisesRegex(runtime.Failure, "private_directory_required"):
                runtime.private_dir(public)
            link = self.work / "link"
            link.symlink_to(self.target)
            with self.assertRaisesRegex(runtime.Failure, "unsafe_path"):
                runtime.read_bytes(link)


if __name__ == "__main__":
    unittest.main()
