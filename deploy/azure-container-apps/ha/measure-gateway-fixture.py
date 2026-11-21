#!/usr/bin/env python3
"""Linux operator-host exact-fixture preparation for MEAS-004/005 (offline).

Usage (Linux/WSL only, Python standard library, no Azure/Docker execution):
  python3 measure-gateway-fixture.py prepare --plan ABS_PLAN_JSON
  python3 measure-gateway-fixture.py cleanup

The work root is fixed at /measurement: an existing, EMPTY, non-symlink 0700
directory owned by the caller, the same absolute path as the gateway-measure
container work directory. `prepare` runs the real native bootstrap ONCE through
measure-gateway-entrypoint.py at /measurement/bootstrap, so generated absolute
paths equal container paths. From that same result it exports the driver,
gateway, writer and reader private projections to /measurement/exports/ROLE.json
and writes /measurement/image/{Dockerfile,database.vlite}: a private fixture
layer FROM the digest-pinned gateway-measure image in the run's private ACR.
Every export is decoded and compared byte-for-byte with the bootstrap tree, its
bootstrap_dir/generation/source/receipt digest and measurement.json fixture
digest/lineage are checked, and the image fixture digest must equal the
bootstrap receipt. Generated token/seed values must not occur in the fixture.
Any failure prints only VLITE-MEAS FAILED 1 and a static code, then removes all
children of /measurement. Nothing is retried; never bootstrap a role separately.

Plan JSON keys (all required, unknown keys refused): server (absolute Linux
vlite-server built from the source revision), writer_url, audience,
control_target {account, container, coordination_container, prefix} (written
as managed_identity, no client_id), control_object, generation, registry,
base_image (REGISTRY.azurecr.io/REPO@sha256:DIGEST), fixture_repository.
VLITE_RESOURCE_SOURCE_REVISION / VLITE_RESOURCE_SOURCE_DIRTY_PATHS supply source.

stdout is one public JSON summary: digests, file paths, the conservative
credential deadline (bootstrap start + 2 h) and an UNEXECUTED planned
`az acr build`. Exports are private secret material; they and the bootstrap
tree persist only until the harness distributes them, then run `cleanup`.
"""

import importlib.util
import os
from pathlib import Path
import re
import shutil
import stat
import subprocess
import sys
import time


HERE = Path(__file__).resolve().parent
_SPEC = importlib.util.spec_from_file_location(
    "measure_gateway_entrypoint", HERE / "measure-gateway-entrypoint.py")
entry = importlib.util.module_from_spec(_SPEC)
_SPEC.loader.exec_module(entry)
Failure, require = entry.Failure, entry.require

WORK_ROOT = Path("/measurement")
ROLES = ("driver", "gateway", "writer", "reader")
FIXTURE_FILE = "/opt/vlite-measure/fixture/database.vlite"
CREDENTIAL_LIFETIME_MS = 7_200_000
PLAN_KEYS = {"server", "writer_url", "audience", "control_target", "control_object",
             "generation", "registry", "base_image", "fixture_repository"}
TARGET_KEYS = {"account", "container", "coordination_container", "prefix"}
REPOSITORY = "[a-z0-9]+(?:[._/-][a-z0-9]+)*"


def is_linux():
    return sys.platform.startswith("linux")


def now_ms():
    return time.time_ns() // 1_000_000


def copy_fixture_file(source, destination):
    entry.write_new(destination, entry.read_bytes(source, entry.FIXTURE_LIMIT))


def work_root(empty):
    require(is_linux(), "linux_host_required")
    root = entry.private_dir(str(WORK_ROOT))
    require(not empty or not any(root.iterdir()), "work_root_not_empty")
    return root


def clear(root):
    for child in list(root.iterdir()):
        if child.is_dir() and not child.is_symlink():
            shutil.rmtree(child)
        else:
            child.unlink()


def private_mkdir(path):
    path.mkdir(mode=0o700)
    path.chmod(0o700)
    return path


def load_plan(path):
    plan = entry.read_json(entry.absolute(path))
    require(isinstance(plan, dict) and set(plan) == PLAN_KEYS, "invalid_plan")
    entry.absolute(plan["server"])
    target = plan["control_target"]
    require(isinstance(target, dict) and set(target) == TARGET_KEYS, "invalid_plan")
    registry = plan["registry"]
    require(isinstance(registry, str) and re.fullmatch("[a-z0-9]{5,50}", registry)
            and isinstance(plan["base_image"], str)
            and re.fullmatch(re.escape(registry) + r"\.azurecr\.io/" + REPOSITORY
                             + "@sha256:[a-f0-9]{64}", plan["base_image"])
            and isinstance(plan["fixture_repository"], str)
            and re.fullmatch(REPOSITORY, plan["fixture_repository"])
            and len(plan["fixture_repository"]) <= 128
            and isinstance(plan["generation"], str)
            and re.fullmatch("[a-z0-9]{1,12}", plan["generation"]), "invalid_plan")
    return plan


def call(config):
    code, data = entry.Runtime(config).execute()
    require(code == 0, "native_failed")
    return entry.json_bytes(data)


def verify_export(path, role, receipt, expected):
    """Decode one private projection and compare it with the bootstrap tree."""
    try:
        document = entry.read_json(path)
        require(set(document) == {"schema", "role", "generation", "bootstrap_dir", "sha256",
                                  "compressed_bytes", "secrets"}
                and document["schema"] == "vlite-meas-secrets/1" and document["role"] == role
                and document["generation"] == expected["generation"]
                and document["bootstrap_dir"] == str(expected["root"])
                and document["sha256"] == receipt.get("sha256")
                and document["compressed_bytes"] == receipt.get("compressed_bytes")
                and receipt.get("export_role") == role
                and len(document["secrets"]) == receipt.get("projections"))
        values = []
        for index, item in enumerate(document["secrets"], 1):
            require(item.get("name") == f"m-{role}-{expected['generation']}-{index}"
                    and isinstance(item.get("value"), str))
            values.append(item["value"])
        compressed = entry.base64.b64decode("".join(values), validate=True)
        require(entry.digest(compressed) == document["sha256"]
                and len(compressed) == document["compressed_bytes"])
        bundle = entry.json_bytes(entry.gunzip(compressed, 2 * 1024 * 1024))
        require(bundle.get("schema") == "vlite-meas-bundle/1" and bundle.get("role") == role
                and bundle.get("generation") == expected["generation"]
                and bundle.get("bootstrap_dir") == str(expected["root"])
                and bundle.get("source") == expected["source"])
        files = bundle.get("files")
        require(isinstance(files, dict) and set(files) == expected["files"][role])
        for name, value in files.items():
            data = entry.base64.b64decode(value, validate=True)
            require(data == (expected["root"] / name).read_bytes())
        metadata = entry.json_bytes(entry.base64.b64decode(files["measurement.json"]))
        require(metadata.get("fixture_sha256") == expected["fixture_sha256"]
                and metadata.get("lineage") == expected["lineage"])
    except (Failure, ValueError, TypeError, KeyError, AttributeError, OSError):
        raise Failure("export_verification_failed") from None


def role_files(root):
    tree = [p.relative_to(root).as_posix() for p in entry.bounded_tree(root) if p.is_file()]
    return {role: {name for name in tree if entry.role_file(name, role)} for role in ROLES}


def prepare(plan_path):
    root = work_root(empty=True)
    plan = load_plan(plan_path)
    revision = os.environ.get("VLITE_RESOURCE_SOURCE_REVISION", "")
    dirty = os.environ.get("VLITE_RESOURCE_SOURCE_DIRTY_PATHS", "")
    require(re.fullmatch("[a-f0-9]{40}", revision), "invalid_provenance")
    try:
        return build(root, plan, revision, dirty)
    except BaseException:
        clear(root)
        raise


def build(root, plan, revision, dirty):
    bootstrap = root / "bootstrap"
    target_file = root / "control-target.json"
    common = {"role": "driver", "work_dir": str(root), "source_revision": revision,
              "source_dirty_paths": dirty, "server": plan["server"]}
    entry.write_new(target_file, entry.encode({**plan["control_target"],
                                               "auth": "managed_identity"}))
    native = root / "bootstrap-config.json"
    entry.write_new(native, entry.encode({"output_dir": str(bootstrap),
        "control_state": {"target_file": str(target_file), "object": plan["control_object"]},
        "writer_url": plan["writer_url"], "audience": plan["audience"]}))
    config = root / "runtime-bootstrap.json"
    entry.write_new(config, entry.encode({**common, "operation": "bootstrap",
                                          "native_config": str(native)}))
    started = now_ms()
    receipt = call(entry.read_json(config))
    fixture_sha256 = entry.hex_digest(receipt.get("fixture_sha256"))
    manifest = entry.read_json(bootstrap / "manifest.json")
    gateway = entry.read_json(bootstrap / "gateway.json")
    require(gateway["control_state"]["target_file"] == str(target_file)
            and all(isinstance(c, dict) and Path(c.get("token_file", "")).parent == bootstrap
                    for c in manifest.get("credentials", [])), "artifact_path_mismatch")
    database = entry.read_bytes(bootstrap / "database.vlite", entry.FIXTURE_LIMIT)
    require(entry.digest(database) == fixture_sha256, "fixture_checksum")
    secrets = [entry.read_bytes(p).strip() for p in entry.bounded_tree(bootstrap)
               if p.is_file() and (p.suffix == ".token" or p.name == "signing.key")]
    require(all(len(s) >= 8 and s not in database for s in secrets), "fixture_secret_detected")
    expected = {"root": bootstrap, "generation": plan["generation"],
                "source": entry.source(common), "fixture_sha256": fixture_sha256,
                "lineage": manifest.get("lineage"), "files": role_files(bootstrap)}
    exports = private_mkdir(root / "exports")
    summary_exports = {}
    for role in ROLES:
        output = exports / f"{role}.json"
        config = root / f"runtime-export-{role}.json"
        entry.write_new(config, entry.encode({**common, "operation": "export",
            "bootstrap_dir": str(bootstrap), "generation": plan["generation"],
            "export_role": role, "output_file": str(output)}))
        result = call(entry.read_json(config))
        verify_export(output, role, result, expected)
        summary_exports[role] = {"file": str(output), "sha256": result["sha256"],
                                 "compressed_bytes": result["compressed_bytes"],
                                 "projections": result["projections"]}
    context = private_mkdir(root / "image")
    copy_fixture_file(bootstrap / "database.vlite", context / "database.vlite")
    # The fixture uses `az acr build`'s classic builder, unlike the base-image task.
    dockerfile = (f"FROM {plan['base_image']}\n"
                  f"COPY --chown=10001:10001 database.vlite {FIXTURE_FILE}\n"
                  f"RUN chmod 0400 {FIXTURE_FILE}\n")
    entry.write_new(context / "Dockerfile", dockerfile.encode("ascii"))
    require(sorted(p.name for p in context.iterdir()) == ["Dockerfile", "database.vlite"]
            and entry.digest(entry.read_bytes(context / "database.vlite", entry.FIXTURE_LIMIT))
            == fixture_sha256, "fixture_checksum")
    deadline = started + CREDENTIAL_LIFETIME_MS
    require(now_ms() < deadline, "credentials_expired")
    image = f"{plan['fixture_repository']}:{plan['generation']}-{fixture_sha256[:12]}"
    return {"schema": "vlite-meas-fixture-prep/1", "qualification": "proposal_evidence_only",
            "source": expected["source"], "bootstrap_dir": str(bootstrap),
            "control_target_file": str(target_file), "fixture_sha256": fixture_sha256,
            "generation": plan["generation"],
            "credentials_expire_not_before_unix_ms": deadline,
            "exports": summary_exports,
            "image": {"context_dir": str(context), "fixture_file": FIXTURE_FILE,
                      "base_image": plan["base_image"],
                      "image": f"{plan['registry']}.azurecr.io/{image}",
                      "planned_build": ["az", "acr", "build", "--registry", plan["registry"],
                                        "--platform", "linux/amd64", "--no-logs", "--file",
                                        str(context / "Dockerfile"), "--image", image,
                                        str(context)]},
            "executed": []}


def main(argv=None):
    args = sys.argv[1:] if argv is None else argv
    try:
        if len(args) == 3 and args[0] == "prepare" and args[1] == "--plan":
            result = prepare(args[2])
        elif args == ["cleanup"]:
            clear(work_root(empty=False))
            result = {"schema": "vlite-meas-fixture-cleanup/1", "removed": True}
        else:
            raise Failure("usage_prepare_or_cleanup")
        sys.stdout.write(entry.encode(result).decode("ascii") + "\n")
        return 0
    except Failure as error:
        sys.stdout.write("VLITE-MEAS FAILED 1\n")
        sys.stderr.write(str(error) + "\n")
        return 1
    except (OSError, ValueError, TypeError, KeyError, OverflowError, RecursionError,
            RuntimeError, subprocess.SubprocessError):
        sys.stdout.write("VLITE-MEAS FAILED 1\n")
        sys.stderr.write("runtime_operation_failed\n")
        return 1


if __name__ == "__main__":
    sys.exit(main())
