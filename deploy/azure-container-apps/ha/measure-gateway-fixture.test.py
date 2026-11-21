"""Offline tests for the Linux exact-fixture preparation.

Run on Linux/WSL: python3 -B measure-gateway-fixture.test.py
No native binary, Docker or Azure command is executed; the single native
bootstrap is replaced by an in-process fake that writes a realistic tree.
"""

import base64
import contextlib
import gzip
import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
import re
import stat
import sys
import tempfile
import unittest
from unittest.mock import patch


sys.dont_write_bytecode = True
HERE = Path(__file__).resolve().parent
SPEC = importlib.util.spec_from_file_location(
    "gateway_fixture", HERE / "measure-gateway-fixture.py")
fixture = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(fixture)
entry = fixture.entry

SECRETS = {
    "backend.token": b"backend-secret-never-log",
    "load-0.token": b"load-secret-never-log",
    "load-1.token": b"revoke-secret-never-log",
    "suspension-0.token": b"suspension-secret-never-log",
    "signing.key": b"signing-seed-never-log",
}
DATABASE = b"nonsecret synthetic database fixture v4"
BASE = "vlitemeasrun.azurecr.io/vlite-gateway-measure@sha256:" + "b" * 64
REVISION = "c" * 40


def put(path, value):
    if isinstance(value, (dict, list)):
        value = json.dumps(value).encode()
    path.write_bytes(value)
    path.chmod(0o600)


class FakeNative:
    """Stand-in for `vlite-server gateway-measure bootstrap --config`."""

    def __init__(self, database=DATABASE):
        self.calls = []
        self.database = database

    def __call__(self, argv, timeout, work, serving=False):
        self.calls.append(list(argv))
        native = json.loads(Path(argv[4]).read_bytes())
        root = Path(native["output_dir"])
        root.mkdir(mode=0o700)
        control = native["control_state"]
        put(root / "gateway.json", {"bind": "0.0.0.0:8080", "control_state": control,
            "public_key_bundle_file": str(root / "public.json"),
            "signing_key_file": str(root / "signing.key")})
        put(root / "trust.json", {"audience": native["audience"], "control_state": control,
            "public_key_bundle_file": str(root / "public.json")})
        put(root / "public.json", {"keys": []})
        for name, value in SECRETS.items():
            put(root / name, value)
        put(root / "load.expected.json", {"id": 1})
        put(root / "suspension.expected.json", {"id": 1})
        put(root / "database.vlite", self.database)
        put(root / "manifest.json", {"schema": "vlite-gateway-measure-bootstrap/1",
            "lineage": 4242, "writer_url": native["writer_url"],
            "path": "/v1/collections/probe/vectors/1",
            "credentials": [{"purpose": "load", "id": "c0", "tenant": "t0",
                             "token_file": str(root / "load-0.token")},
                            {"purpose": "revocation", "id": "c1", "tenant": "t0",
                             "token_file": str(root / "load-1.token")},
                            {"purpose": "suspension", "id": "c2", "tenant": "t1",
                             "token_file": str(root / "suspension-0.token")}]})
        put(root / "directory.json", [
            {"tenant": t, "kind": "dedicated", "authority_mode": "off",
             "writer": native["writer_url"], "audience": native["audience"],
             "lineage": 4242, "read_routing": "writer_only"} for t in ("t0", "t1")])
        (root / "admin").mkdir(mode=0o700)
        put(root / "admin" / "credentials.json", {"hashes": "private"})
        put(root / "admin" / "tenants.json", {})
        return 0, entry.encode({"schema": "vlite-gateway-measure-bootstrap/1",
            "qualification": "proposal_evidence_only", "output_dir": str(root),
            "lineage": 4242, "control_state_published": False,
            "authority_mode": "off", "read_routing": "writer_only"})


class FixturePreparationTests(unittest.TestCase):
    def setUp(self):
        outer = tempfile.TemporaryDirectory(prefix="vlite-fixture-test-")
        self.addCleanup(outer.cleanup)
        self.outer = Path(outer.name)
        self.root = self.outer / "measurement"
        self.root.mkdir(mode=0o700)
        self.plan_dir = self.outer / "plan"
        self.plan_dir.mkdir(mode=0o700)
        for item in (patch.object(fixture, "WORK_ROOT", self.root),
                     patch.object(fixture, "is_linux", return_value=True),
                     patch.dict(os.environ, {"VLITE_RESOURCE_SOURCE_REVISION": REVISION,
                                             "VLITE_RESOURCE_SOURCE_DIRTY_PATHS": ""})):
            item.start()
            self.addCleanup(item.stop)
        self.plan = {"server": "/usr/local/bin/vlite-server",
            "writer_url": "https://writer.internal.example",
            "audience": "measurement-writer",
            "control_target": {"account": "vlitemeasrunst", "container": "control-data",
                               "coordination_container": "control-coordination",
                               "prefix": "isolated-run/"},
            "control_object": "gateway/control.json", "generation": "g1",
            "registry": "vlitemeasrun", "base_image": BASE,
            "fixture_repository": "vlite-gateway-measure-fixture"}

    def write_plan(self, **changes):
        plan = {**self.plan, **changes}
        path = self.plan_dir / "plan.json"
        if path.exists():
            path.unlink()
        put(path, plan)
        return path

    def run_prepare(self, native=None, **changes):
        native = native or FakeNative()
        out, err = io.StringIO(), io.StringIO()
        with patch.object(entry, "run_native", side_effect=native), \
                contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            code = fixture.main(["prepare", "--plan", str(self.write_plan(**changes))])
        return code, out.getvalue(), err.getvalue(), native

    def assert_no_secret(self, *texts):
        for text in texts:
            for value in SECRETS.values():
                self.assertNotIn(value.decode(), text)
            self.assertNotIn("vlitemeasrunst", text)

    def assert_failed(self, result, code):
        status, out, err, native = result
        self.assertEqual(status, 1)
        self.assertEqual(out, "VLITE-MEAS FAILED 1\n")
        self.assertEqual(err, code + "\n")
        self.assert_no_secret(out, err)
        return native

    def decode_export(self, path):
        document = json.loads(Path(path).read_bytes())
        payload = "".join(item["value"] for item in document["secrets"])
        compressed = base64.b64decode(payload)
        return document, compressed, json.loads(gzip.decompress(compressed))

    def test_single_bootstrap_four_verified_role_exports_and_exact_fixture_context(self):
        clock = iter([1_000_000, 1_000_500])
        with patch.object(fixture, "now_ms", side_effect=lambda: next(clock)):
            status, out, err, native = self.run_prepare()
        self.assertEqual((status, err), (0, ""), err)
        self.assertEqual(len(native.calls), 1)  # one bootstrap; no per-role bootstrap
        self.assertEqual(native.calls[0][1:4], ["gateway-measure", "bootstrap", "--config"])
        self.assert_no_secret(out, err)
        summary = json.loads(out)
        bootstrap = self.root / "bootstrap"
        sha = hashlib.sha256(DATABASE).hexdigest()
        self.assertEqual(summary["schema"], "vlite-meas-fixture-prep/1")
        self.assertEqual(summary["qualification"], "proposal_evidence_only")
        self.assertEqual(summary["bootstrap_dir"], str(bootstrap))
        self.assertEqual(summary["fixture_sha256"], sha)
        self.assertEqual(summary["credentials_expire_not_before_unix_ms"], 1_000_000 + 7_200_000)
        self.assertEqual(summary["source"], {"revision": REVISION, "dirty_paths": []})
        target = json.loads((self.root / "control-target.json").read_bytes())
        self.assertEqual(target["auth"], "managed_identity")
        self.assertNotIn("client_id", target)
        self.assertEqual(set(summary["exports"]), {"driver", "gateway", "writer", "reader"})
        for role, receipt in summary["exports"].items():
            path = Path(receipt["file"])
            self.assertEqual(path.parent, self.root / "exports")
            self.assertEqual(stat.S_IMODE(path.stat().st_mode), 0o600)
            document, compressed, bundle = self.decode_export(path)
            self.assertEqual(document["bootstrap_dir"], str(bootstrap))
            self.assertEqual(bundle["bootstrap_dir"], str(bootstrap))
            self.assertEqual(hashlib.sha256(compressed).hexdigest(), receipt["sha256"])
            self.assertEqual(receipt["projections"], len(document["secrets"]))
            self.assertNotIn("database.vlite", bundle["files"])
            self.assertTrue(entry.BASE_FILES[role] <= set(bundle["files"]))
            metadata = json.loads(base64.b64decode(bundle["files"]["measurement.json"]))
            self.assertEqual((metadata["fixture_sha256"], metadata["lineage"]), (sha, 4242))
            for name, value in bundle["files"].items():
                self.assertEqual(base64.b64decode(value), (bootstrap / name).read_bytes())
        writer = self.decode_export(summary["exports"]["writer"]["file"])[2]["files"]
        self.assertNotIn("signing.key", writer)
        self.assertNotIn("admin/credentials.json", writer)
        context = Path(summary["image"]["context_dir"])
        self.assertEqual(context, self.root / "image")
        self.assertEqual(sorted(p.name for p in context.iterdir()), ["Dockerfile", "database.vlite"])
        self.assertEqual((context / "database.vlite").read_bytes(), DATABASE)
        self.assertEqual((context / "Dockerfile").read_text(),
            f"FROM {BASE}\n"
            "COPY --chown=10001:10001 database.vlite "
            "/opt/vlite-measure/fixture/database.vlite\n"
            "RUN chmod 0400 /opt/vlite-measure/fixture/database.vlite\n")
        image = f"vlite-gateway-measure-fixture:g1-{sha[:12]}"
        self.assertEqual(summary["image"]["fixture_file"], "/opt/vlite-measure/fixture/database.vlite")
        self.assertEqual(summary["image"]["image"], f"vlitemeasrun.azurecr.io/{image}")
        self.assertEqual(summary["image"]["planned_build"], ["az", "acr", "build",
            "--registry", "vlitemeasrun", "--platform", "linux/amd64", "--no-logs",
            "--file", str(context / "Dockerfile"), "--image", image, str(context)])
        self.assertEqual(summary["executed"], [])

    def test_refuses_unsafe_hosts_and_roots_without_bootstrap_or_cleanup(self):
        with patch.object(fixture, "is_linux", return_value=False):
            native = self.assert_failed(self.run_prepare(), "linux_host_required")
        self.assertEqual(native.calls, [])
        put(self.root / "stale", b"previous run")
        native = self.assert_failed(self.run_prepare(), "work_root_not_empty")
        self.assertEqual(native.calls, [])
        self.assertTrue((self.root / "stale").exists())  # never deletes unknown state
        (self.root / "stale").unlink()
        self.root.chmod(0o750)
        self.assert_failed(self.run_prepare(), "private_directory_required")
        self.root.chmod(0o700)
        link = self.outer / "link"
        link.symlink_to(self.root, target_is_directory=True)
        with patch.object(fixture, "WORK_ROOT", link):
            self.assert_failed(self.run_prepare(), "unsafe_path")
        with patch.object(fixture, "WORK_ROOT", self.outer / "missing"):
            self.assert_failed(self.run_prepare(), "private_directory_required")

    def test_production_root_is_fixed(self):
        self.assertEqual(SPEC.loader.get_source("gateway_fixture").count(
            'WORK_ROOT = Path("/measurement")'), 1)

    def test_plan_validation(self):
        for change, code in (
                ({"base_image": "vlitemeasrun.azurecr.io/vlite-gateway-measure:latest"}, "invalid_plan"),
                ({"base_image": "docker.io/library/debian@sha256:" + "b" * 64}, "invalid_plan"),
                ({"registry": "otherregistry"}, "invalid_plan"),
                ({"generation": "Bad"}, "invalid_plan"),
                ({"unknown": 1}, "invalid_plan"),
                ({"server": "relative/server"}, "unsafe_path"),
                ({"control_target": {**self.plan["control_target"], "auth": "shared_key"}},
                 "invalid_plan")):
            with self.subTest(change=change):
                native = self.assert_failed(self.run_prepare(**change), code)
                self.assertEqual(native.calls, [])
                self.assertEqual(list(self.root.iterdir()), [])
        with patch.dict(os.environ, {"VLITE_RESOURCE_SOURCE_REVISION": ""}):
            self.assert_failed(self.run_prepare(), "invalid_provenance")

    def test_bootstrap_failure_is_unknown_never_retried_and_cleaned(self):
        def failed(argv, timeout, work, serving=False):
            failed.calls.append(argv)
            return 1, b"native output with backend-secret-never-log"
        failed.calls = []
        self.assert_failed(self.run_prepare(native=failed), "unknown_do_not_retry")
        self.assertEqual(len(failed.calls), 1)
        self.assertEqual(list(self.root.iterdir()), [])

    def test_secret_in_fixture_fails_closed_and_cleans(self):
        native = FakeNative(DATABASE + SECRETS["load-0.token"])
        self.assert_failed(self.run_prepare(native=native), "fixture_secret_detected")
        self.assertEqual(len(native.calls), 1)
        self.assertEqual(list(self.root.iterdir()), [])

    def test_image_fixture_digest_mismatch_fails_closed_and_cleans(self):
        original = fixture.copy_fixture_file
        def tampered(source, destination):
            original(source, destination)
            destination.write_bytes(b"independent database")
        with patch.object(fixture, "copy_fixture_file", side_effect=tampered):
            self.assert_failed(self.run_prepare(), "fixture_checksum")
        self.assertEqual(list(self.root.iterdir()), [])

    def test_expired_credentials_fail_closed_and_clean(self):
        clock = iter([1_000_000, 1_000_000 + 7_200_000])
        with patch.object(fixture, "now_ms", side_effect=lambda: next(clock)):
            self.assert_failed(self.run_prepare(), "credentials_expired")
        self.assertEqual(list(self.root.iterdir()), [])

    def test_export_tampering_fails_closed_and_cleans(self):
        original = entry.Runtime.export
        for name, mutate in (
                ("bootstrap_dir", lambda d, b: d.update(bootstrap_dir="/elsewhere/bootstrap")),
                ("generation", lambda d, b: d.update(generation="g2")),
                ("bundle_dir", lambda d, b: b.update(bootstrap_dir="/elsewhere/bootstrap")),
                ("missing", lambda d, b: b["files"].pop("public.json")),
                ("extra", lambda d, b: b["files"].update({"database.vlite": "AA=="})),
                ("content", lambda d, b: b["files"].update(
                    {"trust.json": base64.b64encode(b"{}").decode()})),
                ("lineage", lambda d, b: b["files"].update({"measurement.json": base64.b64encode(
                    json.dumps({**json.loads(base64.b64decode(b["files"]["measurement.json"])),
                                "lineage": 7}).encode()).decode()}))):
            def export(runtime, mutate=mutate):
                code, data = original(runtime)
                if runtime.c["export_role"] == "writer":
                    path = Path(runtime.c["output_file"])
                    document, _, bundle = self.decode_export(path)
                    mutate(document, bundle)
                    compressed = gzip.compress(json.dumps(bundle).encode(), mtime=0)
                    payload = base64.b64encode(compressed).decode()
                    document["secrets"] = [{"name": "m-writer-g1-1", "value": payload}]
                    document["sha256"] = hashlib.sha256(compressed).hexdigest()
                    document["compressed_bytes"] = len(compressed)
                    path.write_bytes(json.dumps(document).encode())
                    receipt = json.loads(data)
                    receipt.update(sha256=document["sha256"], compressed_bytes=len(compressed),
                                   projections=1)
                    data = entry.encode(receipt)
                return code, data
            with self.subTest(name=name), patch.object(entry.Runtime, "export", export):
                self.assert_failed(self.run_prepare(), "export_verification_failed")
                self.assertEqual(list(self.root.iterdir()), [])

    def test_receipt_digest_mismatch_fails_closed(self):
        original = entry.Runtime.export
        def export(runtime):
            code, data = original(runtime)
            return code, entry.encode({**json.loads(data), "sha256": "0" * 64})
        with patch.object(entry.Runtime, "export", export):
            self.assert_failed(self.run_prepare(), "export_verification_failed")

    def test_cleanup_removes_children_only(self):
        self.assertEqual(self.run_prepare()[0], 0)
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            self.assertEqual(fixture.main(["cleanup"]), 0)
        self.assertTrue(self.root.is_dir())
        self.assertEqual(list(self.root.iterdir()), [])
        self.assertEqual(json.loads(out.getvalue()), {"schema": "vlite-meas-fixture-cleanup/1",
                                                       "removed": True})
        err = io.StringIO()
        with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(err):
            self.assertEqual(fixture.main(["unknown"]), 1)
        self.assertEqual(err.getvalue(), "usage_prepare_or_cleanup\n")


class PackagingTests(unittest.TestCase):
    def test_gateway_measure_image_installs_python_and_entrypoint_only_there(self):
        text = (HERE.parents[2] / "Dockerfile").read_text(encoding="utf-8").replace("\r\n", "\n")
        stage = text.split("FROM debian:bookworm-slim AS gateway-measure\n")[1].split("\nFROM ")[0]
        self.assertIn("apt-get install -y --no-install-recommends python3", stage)
        self.assertIn("COPY deploy/azure-container-apps/ha/measure-gateway-entrypoint.py "
                      "/usr/local/bin/measure-gateway-entrypoint.py", stage)
        api = text.split("FROM debian:bookworm-slim AS runtime\n")[1].split(
            "FROM source AS gateway-measure-build\n")[0]
        self.assertNotIn("python3", api)
        self.assertNotIn("measure-gateway-entrypoint", api)
        ignores = (HERE.parents[2] / ".dockerignore").read_text(encoding="utf-8").splitlines()
        self.assertTrue({"!deploy/", "!deploy/**"} <= set(ignores)
                        or "!deploy/azure-container-apps/ha/measure-gateway-entrypoint.py" in ignores)


if __name__ == "__main__":
    unittest.main()
