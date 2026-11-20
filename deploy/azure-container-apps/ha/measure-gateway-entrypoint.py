#!/usr/bin/env python3
"""Disposable gateway runtime; Python standard library, no Azure operations.

Invoke: python3 /usr/local/bin/measure-gateway-entrypoint.py --config ABS_JSON
Only this flag is accepted. UTF-8 JSON files are <=65536 bytes, unknown keys
are errors. Common keys (all required): operation, role, work_dir (existing
private 0700 directory), source_revision (40 lowercase hex), source_dirty_paths
(newline-separated porcelain entries, "" when clean). The latter two must
match VLITE_RESOURCE_SOURCE_REVISION / VLITE_RESOURCE_SOURCE_DIRTY_PATHS.
Optional native-command timeout_secs: 1..3590, default 3590;
server/load_program/control_program
default /usr/local/bin/{vlite-server,ha-meas004,ha-meas005}. Programs are absolute.

Operations and additional keys:
* bootstrap (role driver): native_config = absolute native bootstrap JSON.
  output_dir must be a NEW child of work_dir. Runs the real offline bootstrap
  once. A persistent create-only attempt marker forbids uncertain repeats.
  Adds measurement.json (source, fixture digest, lineage) to the native output;
  include this provenance file in every serving/driver projection.
* export (driver): bootstrap_dir, generation (lowercase alphanumeric 1..12),
  export_role (driver|gateway|writer|reader), output_file (NEW private JSON).
  Writes {"schema":"vlite-meas-secrets/1","generation":...,"role":...,
  "bootstrap_dir":...,"sha256":...,"compressed_bytes":...,
  "secrets":[{"name":"m-ROLE-GENERATION-N","value":"BASE64"}]}.
  This is PRIVATE ACA secret material, NEVER console evidence. Values are
  <=3000 ASCII chars, <=64 projections / role, <=144000 compressed bytes.
  The gzip JSON bundle contains only role-allowed relative bootstrap files;
  database.vlite is NEVER a secret. Export all roles before publish.
* restore (any role): projection_files (ordered absolute files containing ONLY
  each secret's base64 value), generation, sha256 (compressed bundle digest),
  bootstrap_dir (NEW private destination, SAME absolute path as bootstrap).
  Restores verified role-specific files; no driver admin/seed in backends.
  Mount control-target.json separately at the SAME absolute path named in the
  native configs, with each role's own managed-identity client_id. The target is
  not part of any bundle; serving roles need only control-container read grants.
* copy_fixture (writer|reader): fixture_file, fixture_sha256, database_file
  (NEW path beneath work_dir). Copies <=16 MiB, digest-verified, 0600. The image
  fixture MUST be the SAME database as the bootstrap directory's fixture;
  compare its digest with bootstrap's returned fixture_sha256. An independently
  bootstrapped image fixture has different tenant IDs/lineage and is NOT valid.
* publish (driver): bootstrap_dir. Uses gateway.json's managed-identity control
  target/object, admin/ and directory.json; one create-only publish, no retries.
* serve (gateway|writer|reader, MEAS-004): bootstrap_dir; backend also
  database_file, fixture_sha256. Owns the real server with stdout/stderr
  discarded for duration_secs (1..7200, default 7200), then kills/reaps it.
  This separate app lifetime allows bounded setup plus a <=3600 s load job.
  Early exit/interruption fails the trial; there is no automatic child restart.
* observe (same role/files, MEAS-005): runs ha-meas005's owned-child observer,
  NOT an extra sidecar. Optional observe_interval_ms (10..5000, default 50),
  duration_secs (1..3540, default 1200), ready_timeout_ms (10..1000, default 1000).
  Both serve/observe require actual ACA replica
  identity is mandatory. Gateway serves gateway --config; writer uses writer,
  full durability; optional reader uses reader WITHOUT --durability. A serving
  attempt marker refuses container restarts reusing the same ephemeral work.
* load (driver): native_config (exact ha-meas004 config from runtime contract).
* mutate (driver): native_config (exact ha-meas005 mode=run config).
* analyze (driver): native_config (exact mode=analyze config). No reader required;
  inventory must explicitly include all real replicas, >=1 gateway and backend.
* clock (any role): no extra keys; runs mode=clock_sample.
* transport (any role): evidence_file, evidence_schema (one of the four native
  measurement schemas), native_exit_code (0|2). Re-exports a private report,
  e.g. after replica-specific collection; validates provenance/schema first.

load/mutate/analyze native configs are passed as FILES, never invented flags;
their unknown-field/range checks remain native. No inline credentials, shell,
native stderr, admin environment variables or native diagnostics are forwarded.
All outputs are bounded. Successful public receipts and unchanged native
evidence use VLITE-MEAS BEGIN <gzip-sha256> <gzip-bytes>, numbered 76-character
base64 chunks, END <count>. Incomplete native exit-2 JSON is framed unchanged
and this process exits 2, WITHOUT a FAILED marker that would discard evidence.
Operational errors print only VLITE-MEAS FAILED 1 and a static error code.
Bootstrap/publish failure means unknown_do_not_retry; reconcile externally.
Private files/attempt markers persist until orchestrator finally cleanup.
"""

import base64
import gzip
import hashlib
import io
import json
import math
import os
from pathlib import Path
import re
import signal
import stat
import subprocess
import sys
import tempfile
import threading
import time
from urllib.parse import urlsplit
import zlib


CONFIG_LIMIT = 65536
REPORT_LIMIT = 4 * 1024 * 1024
FIXTURE_LIMIT = 16 * 1024 * 1024
PROJECTION_CHARS = 3000
MAX_PROJECTIONS = 64
BUNDLE_LIMIT = 144000
SCHEMAS = {"vlite-meas004/1", "vlite-meas005/1",
           "vlite-meas005-observer/1", "vlite-meas005-clock/1"}
COMMON = {"operation", "role", "work_dir", "source_revision",
          "source_dirty_paths", "timeout_secs", "server", "load_program",
          "control_program"}
EXTRA = {
    "bootstrap": {"native_config"}, "export": {"bootstrap_dir", "generation",
        "export_role", "output_file"},
    "restore": {"projection_files", "generation", "sha256", "bootstrap_dir"},
    "copy_fixture": {"fixture_file", "fixture_sha256", "database_file"},
    "publish": {"bootstrap_dir"},
    "serve": {"bootstrap_dir", "database_file", "fixture_sha256", "duration_secs"},
    "observe": {"bootstrap_dir", "database_file", "fixture_sha256",
                "observe_interval_ms", "duration_secs", "ready_timeout_ms"},
    "load": {"native_config"}, "mutate": {"native_config"},
    "analyze": {"native_config"}, "clock": set(),
    "transport": {"evidence_file", "evidence_schema", "native_exit_code"},
}
BASE_FILES = {
    "gateway": {"gateway.json", "public.json", "signing.key", "measurement.json"},
    "writer": {"trust.json", "public.json", "backend.token", "measurement.json"},
    "reader": {"trust.json", "public.json", "backend.token", "measurement.json"},
    "driver": {"gateway.json", "trust.json", "public.json", "signing.key",
               "backend.token", "directory.json", "manifest.json",
               "load.expected.json", "suspension.expected.json", "measurement.json"},
}


class Failure(Exception):
    """Only static codes may cross the console boundary."""


def require(condition, code="invalid_config"):
    if not condition:
        raise Failure(code)


def integer(value, low, high):
    require(type(value) is int and low <= value <= high)
    return value


def object_keys(value, allowed, required=()):
    require(isinstance(value, dict) and set(value) <= set(allowed)
            and set(required) <= set(value))
    return value


def absolute(value):
    require(isinstance(value, str) and 0 < len(value) <= 2048
            and not any(ord(c) < 32 for c in value))
    path = Path(value)
    require(path.is_absolute() and ".." not in path.parts, "unsafe_path")
    for ancestor in (path, *path.parents):
        require(not ancestor.is_symlink()
                and not (ancestor.exists()
                         and getattr(ancestor.lstat(), "st_file_attributes", 0) & 1024),
                "unsafe_path")
    return path


def private_dir(path):
    path = absolute(str(path))
    require(path.is_dir(), "private_directory_required")
    if os.name != "nt":
        require(path.stat().st_uid == os.getuid()
                and stat.S_IMODE(path.stat().st_mode) & 0o077 == 0,
                "private_directory_required")
    return path


def read_bytes(path, limit=CONFIG_LIMIT):
    path = absolute(str(path))
    require(path.is_file(), "file_unavailable")
    with path.open("rb") as stream:
        data = stream.read(limit + 1)
    require(len(data) <= limit, "file_limit")
    return data


def json_bytes(data):
    def pairs(items):
        result = {}
        for key, value in items:
            require(key not in result, "duplicate_json_key")
            result[key] = value
        return result
    try:
        result = json.loads(data.decode("utf-8"), object_pairs_hook=pairs,
                            parse_constant=lambda _: (_ for _ in ()).throw(Failure("invalid_json")))
    except (ValueError, UnicodeError, RecursionError):
        raise Failure("invalid_json") from None
    require(isinstance(result, dict), "json_object_required")
    return result


def read_json(path, limit=CONFIG_LIMIT):
    return json_bytes(read_bytes(path, limit))


def encode(value):
    return json.dumps(value, separators=(",", ":"), ensure_ascii=True,
                      allow_nan=False).encode("ascii")


def write_new(path, data):
    path = absolute(str(path))
    private_dir(path.parent)
    descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(descriptor, "wb") as stream:
        stream.write(data)
        stream.flush()
        os.fsync(stream.fileno())


def digest(data):
    return hashlib.sha256(data).hexdigest()


def hex_digest(value):
    require(isinstance(value, str) and re.fullmatch("[a-f0-9]{64}", value))
    return value


def generation(value):
    require(isinstance(value, str) and re.fullmatch("[a-z0-9]{1,12}", value))
    return value


def source(config):
    return {"revision": config["source_revision"],
            "dirty_paths": config["source_dirty_paths"].splitlines()}


def origin(value):
    require(isinstance(value, str))
    parsed = urlsplit(value)
    require(parsed.scheme == "https" and parsed.hostname and not parsed.username
            and not parsed.password and not parsed.query and not parsed.fragment
            and parsed.path in ("", "/"), "invalid_origin")


def control_source(value):
    object_keys(value, {"target_file", "object", "shared_key_file"},
                {"target_file", "object"})
    require(value.get("shared_key_file") is None, "managed_identity_required")
    target_path = absolute(value["target_file"])
    target = read_json(target_path)
    object_keys(target, {"account", "container", "coordination_container", "prefix",
                         "auth", "client_id"},
                {"account", "container", "coordination_container", "prefix", "auth"})
    require(target["auth"] == "managed_identity", "managed_identity_required")
    require(re.fullmatch("[a-z0-9]{3,24}", target["account"] or "")
            and not target["account"].startswith("asendme")
            and target["account"] not in {"vliteapp", "vliteenv", "vlitedata"})
    for field in ("container", "coordination_container"):
        require(isinstance(target[field], str)
                and re.fullmatch("[a-z0-9][a-z0-9-]{1,61}[a-z0-9]", target[field]))
    require(target["container"] != target["coordination_container"])
    name = value["object"]
    require(isinstance(name, str) and len(name) <= 256
            and re.fullmatch("[a-z0-9._/-]+", name)
            and not any(p in ("", ".", "..") for p in name.split("/"))
            and name not in {"owner.json", "catalog.json", "authority.json"}
            and not name.startswith("sets/"))
    require(isinstance(target["prefix"], str) and 0 < len(target["prefix"]) <= 256)
    return target_path


def role_file(name, role):
    if name in BASE_FILES[role]:
        return True
    if role != "driver":
        return False
    return bool(re.fullmatch(r"(?:load-[0-9]{1,3}|suspension-0)\.token", name)
                or (re.fullmatch(r"admin/[A-Za-z0-9._-]{1,128}", name)
                    and name not in ("admin/.", "admin/..")))


def bounded_tree(root, limit=512):
    for count, path in enumerate(root.rglob("*"), 1):
        require(count <= limit, "artifact_count_limit")
        absolute(str(path))
        require(path.is_file() or path.is_dir(), "unsafe_artifact")
        yield path


def check_role_tree(root, role):
    private_dir(root)
    files = set()
    for path in bounded_tree(root):
        if path.is_file():
            name = path.relative_to(root).as_posix()
            require(role_file(name, role), "role_artifact_refused")
            files.add(name)
    require(BASE_FILES[role] <= files, "role_artifact_missing")


def artifact_provenance(root, expected):
    metadata = read_json(root / "measurement.json")
    require(metadata.get("schema") == "vlite-meas-artifacts/1"
            and metadata.get("source") == expected, "artifact_provenance")
    hex_digest(metadata.get("fixture_sha256"))
    return metadata


def validate_topology(root, expected=None):
    gateway = read_json(root / "gateway.json")
    trust = read_json(root / "trust.json")
    require(gateway.get("control_state") == trust.get("control_state"),
            "control_source_mismatch")
    target = control_source(gateway["control_state"])
    require(gateway.get("public_key_bundle_file") == str(root / "public.json")
            and gateway.get("signing_key_file") == str(root / "signing.key")
            and trust.get("public_key_bundle_file") == str(root / "public.json"),
            "artifact_path_mismatch")
    routes = json.loads(read_bytes(root / "directory.json"))
    require(isinstance(routes, list) and len(routes) == 2, "fixture_topology_invalid")
    manifest = read_json(root / "manifest.json")
    require(manifest.get("schema") == "vlite-gateway-measure-bootstrap/1")
    if expected is not None:
        metadata = artifact_provenance(root, expected)
        require(metadata.get("lineage") == manifest.get("lineage"), "artifact_lineage")
    origin(manifest["writer_url"])
    for route in routes:
        require(isinstance(route, dict) and route.get("authority_mode") == "off"
                and route.get("read_routing") == "writer_only"
                and route.get("lineage") == manifest.get("lineage")
                and route.get("writer") == manifest["writer_url"]
                and not route.get("reader") and not route.get("epoch"),
                "fixture_topology_invalid")
    return gateway, target


def run_native(argv, timeout, work, serving=False):
    """Drain bounded pipes concurrently; never print native output/diagnostics."""
    env = {k: v for k, v in os.environ.items()
           if not k.startswith("VLITE_") or k.startswith("VLITE_RESOURCE_SOURCE_")}
    env["TMPDIR"] = str(work)
    env["TEMP"] = str(work)
    env["TMP"] = str(work)
    # Native programs may create their own files; enforce private creation.
    kwargs = {"umask": 0o077, "start_new_session": True} if os.name != "nt" else {}
    try:
        child = subprocess.Popen([str(a) for a in argv], stdin=subprocess.DEVNULL,
                                 stdout=subprocess.DEVNULL if serving else subprocess.PIPE,
                                 stderr=subprocess.DEVNULL if serving else subprocess.PIPE,
                                 cwd=str(work), env=env, **kwargs)
    except OSError:
        raise Failure("native_spawn_failed") from None
    output = bytearray()
    overflow = threading.Event()
    pipe_error = threading.Event()
    interrupted = threading.Event()

    def drain(stream, keep):
        count = 0
        try:
            while True:
                data = stream.read(8192)
                if not data:
                    break
                count += len(data)
                if count > REPORT_LIMIT:
                    overflow.set()
                elif keep:
                    output.extend(data)
        except OSError:
            pipe_error.set()
        finally:
            try:
                stream.close()
            except OSError:
                pipe_error.set()

    threads = [] if serving else [
        threading.Thread(target=drain, args=(child.stdout, True), daemon=True),
        threading.Thread(target=drain, args=(child.stderr, False), daemon=True)]
    previous = {}

    def terminate(signum, _frame):
        interrupted.set()
        if child.poll() is None:
            try:
                child.send_signal(signum)
            except ProcessLookupError:
                pass

    try:
        if threading.current_thread() is threading.main_thread():
            for signum in (signal.SIGINT, signal.SIGTERM):
                previous[signum] = signal.signal(signum, terminate)
        for thread in threads:
            thread.start()
        deadline = time.monotonic() + timeout
        grace = None
        while child.poll() is None:
            if interrupted.is_set() and grace is None:
                grace = time.monotonic() + 5
            require(not overflow.is_set(), "native_output_limit")
            require(not pipe_error.is_set(), "native_pipe_error")
            if time.monotonic() >= deadline:
                require(serving, "native_timeout")
                require(not interrupted.is_set(), "serving_interrupted")
                return 0, b""
            require(grace is None or time.monotonic() < grace, "native_interrupted")
            time.sleep(0.02)
        require(not serving, "serving_interrupted" if interrupted.is_set()
                else "serving_child_terminated")
        for thread in threads:
            thread.join(2)
        require(not any(t.is_alive() for t in threads), "native_pipe_timeout")
        require(not overflow.is_set(), "native_output_limit")
        require(not pipe_error.is_set(), "native_pipe_error")
        return child.returncode, bytes(output)
    finally:
        # Kill only the process group created for this invocation, including an
        # observer's owned server if the supervisor itself timed out.
        if os.name != "nt":
            try:
                os.killpg(child.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
        elif child.poll() is None:
            child.kill()
        child.wait(timeout=5)
        for thread in threads:
            if thread.ident is not None:
                thread.join(2)
        for signum, handler in previous.items():
            signal.signal(signum, handler)


def frame(data):
    require(len(data) <= REPORT_LIMIT, "evidence_limit")
    compressed = gzip.compress(data, mtime=0)
    payload = base64.b64encode(compressed).decode("ascii")
    lines = [f"VLITE-MEAS BEGIN {digest(compressed)} {len(compressed)}"]
    chunks = [payload[n:n + 76] for n in range(0, len(payload), 76)]
    lines.extend(f"VLITE-MEAS {n} {chunk}" for n, chunk in enumerate(chunks, 1))
    lines.append(f"VLITE-MEAS END {len(chunks)}")
    return "\n".join(lines) + "\n"


def gunzip(data, limit):
    try:
        with gzip.GzipFile(fileobj=io.BytesIO(data)) as stream:
            result = stream.read(limit + 1)
    except (OSError, EOFError, zlib.error):
        raise Failure("invalid_gzip") from None
    require(len(result) <= limit, "decompression_limit")
    return result


def verify_frame(text):
    """Strict transport verifier, also used by offline tests/collectors."""
    require(isinstance(text, str) and len(text) <= 2 * REPORT_LIMIT, "transport_limit")
    lines = text.splitlines()
    require(len(lines) >= 3, "transport_incomplete")
    begin = re.fullmatch(r"VLITE-MEAS BEGIN ([a-f0-9]{64}) ([0-9]{1,8})", lines[0])
    end = re.fullmatch(r"VLITE-MEAS END ([0-9]{1,6})", lines[-1])
    require(begin and end and int(end[1]) == len(lines) - 2, "transport_incomplete")
    chunks = []
    for index, line in enumerate(lines[1:-1], 1):
        match = re.fullmatch(r"VLITE-MEAS ([0-9]+) ([A-Za-z0-9+/=]{1,76})", line)
        require(match and int(match[1]) == index, "transport_sequence")
        require(index == len(lines) - 2 or len(match[2]) == 76, "transport_chunk")
        chunks.append(match[2])
    try:
        compressed = base64.b64decode("".join(chunks), validate=True)
    except ValueError:
        raise Failure("transport_base64") from None
    require(len(compressed) == int(begin[2]), "transport_length")
    require(digest(compressed) == begin[1], "transport_checksum")
    data = gunzip(compressed, REPORT_LIMIT)
    json_bytes(data)
    return data


def validate_evidence(data, schema, config, code):
    require(schema in SCHEMAS, "evidence_schema")
    report = json_bytes(data)
    require(report.get("schema") == schema
            and report.get("qualification") == "proposal_evidence_only",
            "evidence_schema")
    require(report.get("source") == source(config), "evidence_provenance")
    require(code in (0, 2), "native_failed")
    shared = {"schema", "qualification", "source", "requested", "limits", "uncertainty"}
    fields = {
        "vlite-meas004/1": {"order", "request_class", "generation",
                            "authenticated_policy", "windows", "deltas"},
        "vlite-meas005/1": {"stage", "complete", "mutation_and_ingress_checks_complete",
                            "mutations", "replicas", "fixed_inventory_confirmed"},
        "vlite-meas005-observer/1": {"replica", "role", "app", "revision",
            "identity_source", "boot_id", "owns_serving_child", "terminated_early",
            "interrupted", "start_unix_ms", "end_unix_ms", "elapsed_ms", "polls",
            "readiness_failures", "max_sample_start_gap_ms", "max_clock_jump_ms", "events"},
        "vlite-meas005-clock/1": {"start_unix_ms", "end_unix_ms", "elapsed_ms"},
    }
    require(set(report) <= shared | fields[schema], "evidence_shape")
    if schema == "vlite-meas005/1":
        require(report.get("stage") in ("mutations", "analysis")
                and type(report.get("complete")) is bool, "evidence_shape")
        if report["stage"] == "mutations":
            require(type(report.get("mutation_and_ingress_checks_complete")) is bool
                    and isinstance(report.get("mutations"), list)
                    and len(report["mutations"]) <= 100, "evidence_shape")
            incomplete = not report["mutation_and_ingress_checks_complete"]
        else:
            require(isinstance(report.get("replicas"), list)
                    and len(report["replicas"]) <= 64, "evidence_shape")
            incomplete = not report["complete"]
        require((code == 2) == incomplete, "native_exit_mismatch")
    elif schema == "vlite-meas005-observer/1":
        require(type(report.get("terminated_early")) is bool
                and type(report.get("interrupted")) is bool
                and report.get("owns_serving_child") is True
                and isinstance(report.get("events"), list)
                and len(report["events"]) <= 2048
                and isinstance(report.get("replica"), str)
                and report["replica"]
                and report.get("role") in ("gateway", "backend"), "evidence_shape")
        require((code == 2) == (report["terminated_early"] or report["interrupted"]),
                "native_exit_mismatch")
    elif schema == "vlite-meas004/1":
        require(code == 0 and report.get("order") == "ABBA"
                and isinstance(report.get("windows"), list)
                and 1 <= len(report["windows"]) <= 25, "evidence_shape")
    else:
        require(code == 0 and all(isinstance(report.get(k), (int, float))
                and not isinstance(report[k], bool) and math.isfinite(report[k])
                for k in ("start_unix_ms", "end_unix_ms", "elapsed_ms")),
                "evidence_shape")
    # Native evidence contains no private config or credential fields.
    forbidden = {"token", "bearer", "signing_key", "signing_key_file", "client_id",
                 "principal_id", "subscription_id", "tenant_id", "admin_dir",
                 "gateway_config", "target_file", "token_file", "envelope"}

    def inspect(value):
        if isinstance(value, dict):
            require(not forbidden.intersection(k.lower() for k in value),
                    "evidence_private_field")
            for item in value.values():
                inspect(item)
        elif isinstance(value, list):
            for item in value:
                inspect(item)
        elif isinstance(value, str):
            require(not re.search(r"(?i)(?:bearer\s|https?://|[?&](?:sig|token)=)", value),
                    "evidence_private_value")
    inspect(report)
    return report


class Runtime:
    def __init__(self, config):
        object_keys(config, COMMON | set().union(*EXTRA.values()),
                    {"operation", "role", "work_dir", "source_revision", "source_dirty_paths"})
        self.c = config
        self.op = config["operation"]
        self.role = config["role"]
        require(self.op in EXTRA and self.role in BASE_FILES)
        object_keys(config, COMMON | EXTRA[self.op])
        driver_ops = {"bootstrap", "export", "publish", "load", "mutate", "analyze"}
        require(self.op not in driver_ops or self.role == "driver", "role_refused")
        require(self.op not in ("serve", "observe") or self.role != "driver", "role_refused")
        require(self.op != "copy_fixture" or self.role in ("writer", "reader"), "role_refused")
        require(isinstance(config["source_revision"], str)
                and re.fullmatch("[a-f0-9]{40}", config["source_revision"])
                and isinstance(config["source_dirty_paths"], str)
                and len(config["source_dirty_paths"].encode("utf-8")) <= 16384,
                "invalid_provenance")
        require(os.environ.get("VLITE_RESOURCE_SOURCE_REVISION") == config["source_revision"]
                and os.environ.get("VLITE_RESOURCE_SOURCE_DIRTY_PATHS")
                == config["source_dirty_paths"], "source_environment_mismatch")
        self.work = private_dir(config["work_dir"])
        self.private_roots = {self.work}
        self.private_values = set()
        self.timeout = integer(config.get("timeout_secs", 3590), 1, 3590)
        self.server = absolute(config.get("server", "/usr/local/bin/vlite-server"))
        self.load = absolute(config.get("load_program", "/usr/local/bin/ha-meas004"))
        self.control = absolute(config.get("control_program", "/usr/local/bin/ha-meas005"))

    def destination(self, value):
        path = absolute(value)
        require(path != self.work and self.work in path.parents, "destination_outside_work")
        return path

    def root(self):
        root = private_dir(self.c["bootstrap_dir"])
        self.private_roots.add(root)
        return root

    def control_target(self, value):
        target = control_source(value)
        document = read_json(target)
        for field in ("client_id", "account"):
            text = document.get(field)
            if isinstance(text, str) and len(text) >= 8:
                self.private_values.add(text.encode("utf-8"))
        return target

    def receipt(self, **fields):
        return encode({"schema": "vlite-meas-runtime/1",
                       "qualification": "proposal_evidence_only",
                       "operation": self.op, "source": source(self.c), **fields})

    def invoke(self, argv, uncertain=False):
        try:
            code, data = run_native(argv, self.timeout, self.work)
            require(code == 0 or (argv[0] == self.control and code == 2), "native_failed")
            return code, data
        except Failure:
            if uncertain:
                raise Failure("unknown_do_not_retry") from None
            raise

    def attempt(self, name):
        try:
            write_new(self.work / (name + ".attempted"), b"unknown_do_not_retry\n")
        except FileExistsError:
            raise Failure("unknown_do_not_retry") from None

    def evidence(self, data, schema, code):
        report = validate_evidence(data, schema, self.c, code)
        # Defense in depth for the private files available to this role. Never
        # alter evidence into a success-shaped redacted approximation.
        values = self.private_values.copy()
        for root in self.private_roots:
            for path in bounded_tree(root, 1024):
                if path.is_file() and (path.suffix == ".token" or path.name == "signing.key"):
                    value = read_bytes(path).strip()
                    if len(value) >= 8:
                        values.add(value)
        text = encode(report)
        require(not any(value in data or value in text for value in values),
                "evidence_secret_detected")
        return report

    def bootstrap(self):
        path = absolute(self.c["native_config"])
        native = read_json(path)
        object_keys(native, {"output_dir", "control_state", "writer_url", "audience",
                             "poll_interval_ms", "max_replicas", "revocations"},
                    {"output_dir", "control_state", "writer_url", "audience"})
        root = self.destination(native["output_dir"])
        private_dir(root.parent)
        require(not root.exists(), "bootstrap_exists")
        self.control_target(native["control_state"])
        origin(native["writer_url"])
        require(isinstance(native["audience"], str) and 1 <= len(native["audience"]) <= 96
                and not any(ord(c) < 32 for c in native["audience"]))
        integer(native.get("poll_interval_ms", 1000), 1, 5000)
        integer(native.get("max_replicas", 2), 1, 64)
        integer(native.get("revocations", 10), 1, 100)
        self.attempt("bootstrap")
        _, data = self.invoke([self.server, "gateway-measure", "bootstrap", "--config", path], True)
        report = json_bytes(data)
        require(report.get("schema") == "vlite-gateway-measure-bootstrap/1"
                and report.get("qualification") == "proposal_evidence_only"
                and report.get("control_state_published") is False
                and report.get("authority_mode") == "off"
                and report.get("read_routing") == "writer_only", "bootstrap_output_invalid")
        validate_topology(root)
        fixture = read_bytes(root / "database.vlite", FIXTURE_LIMIT)
        require(fixture, "fixture_empty")
        manifest = read_json(root / "manifest.json")
        require(type(manifest.get("lineage")) is int
                and manifest["lineage"] == report.get("lineage")
                and report.get("output_dir") == str(root), "bootstrap_output_invalid")
        write_new(root / "measurement.json", encode({"schema": "vlite-meas-artifacts/1",
            "source": source(self.c), "fixture_sha256": digest(fixture),
            "lineage": manifest["lineage"]}))
        return 0, self.receipt(fixture_sha256=digest(fixture), authority_mode="off",
                               read_routing="writer_only", control_state_published=False)

    def export(self):
        root = self.root()
        role = self.c["export_role"]
        require(role in BASE_FILES)
        gen = generation(self.c["generation"])
        validate_topology(root, source(self.c))
        files = {}
        for path in bounded_tree(root):
            if path.is_file():
                name = path.relative_to(root).as_posix()
                if role_file(name, role):
                    files[name] = base64.b64encode(read_bytes(path)).decode("ascii")
        require(BASE_FILES[role] <= set(files), "role_artifact_missing")
        bundle = {"schema": "vlite-meas-bundle/1", "role": role, "generation": gen,
                  "bootstrap_dir": str(root), "source": source(self.c), "files": files}
        compressed = gzip.compress(encode(bundle), mtime=0)
        require(len(compressed) <= BUNDLE_LIMIT, "secret_bundle_limit")
        payload = base64.b64encode(compressed).decode("ascii")
        values = [payload[i:i + PROJECTION_CHARS]
                  for i in range(0, len(payload), PROJECTION_CHARS)]
        require(len(values) <= MAX_PROJECTIONS, "secret_projection_limit")
        projection = {"schema": "vlite-meas-secrets/1", "role": role, "generation": gen,
                      "bootstrap_dir": str(root), "sha256": digest(compressed),
                      "compressed_bytes": len(compressed),
                      "secrets": [{"name": f"m-{role}-{gen}-{i}", "value": v}
                                  for i, v in enumerate(values, 1)]}
        write_new(self.destination(self.c["output_file"]), encode(projection))
        return 0, self.receipt(export_role=role, generation=gen, sha256=digest(compressed),
                               compressed_bytes=len(compressed), projections=len(values))

    def restore(self):
        paths = self.c["projection_files"]
        require(isinstance(paths, list) and 1 <= len(paths) <= MAX_PROJECTIONS)
        require(len(set(paths)) == len(paths))
        gen = generation(self.c["generation"])
        payload = b"".join(read_bytes(p, PROJECTION_CHARS) for p in paths)
        try:
            compressed = base64.b64decode(payload, validate=True)
        except ValueError:
            raise Failure("secret_projection_invalid") from None
        require(len(compressed) <= BUNDLE_LIMIT
                and digest(compressed) == hex_digest(self.c["sha256"]), "secret_checksum")
        bundle = json_bytes(gunzip(compressed, 2 * 1024 * 1024))
        object_keys(bundle, {"schema", "role", "generation", "bootstrap_dir", "source", "files"},
                    {"schema", "role", "generation", "bootstrap_dir", "source", "files"})
        root = self.destination(self.c["bootstrap_dir"])
        require(bundle["schema"] == "vlite-meas-bundle/1"
                and bundle["role"] == self.role and bundle["generation"] == gen
                and bundle["source"] == source(self.c)
                and bundle["bootstrap_dir"] == str(root), "secret_bundle_identity")
        files = bundle["files"]
        require(isinstance(files, dict) and BASE_FILES[self.role] <= set(files)
                and len(files) <= 256, "secret_bundle_files")
        decoded = {}
        for name, value in files.items():
            require(role_file(name, self.role), "role_artifact_refused")
            try:
                data = base64.b64decode(value, validate=True)
            except (TypeError, ValueError):
                raise Failure("secret_file_invalid") from None
            require(len(data) <= CONFIG_LIMIT, "secret_file_limit")
            decoded[name] = data
        metadata = json_bytes(decoded["measurement.json"])
        require(metadata.get("schema") == "vlite-meas-artifacts/1"
                and metadata.get("source") == source(self.c), "artifact_provenance")
        hex_digest(metadata.get("fixture_sha256"))
        private_dir(root.parent)
        root.mkdir(mode=0o700)  # create-only, including after a partial restore
        for name, data in decoded.items():
            path = root.joinpath(*name.split("/"))
            if path.parent != root and not path.parent.exists():
                path.parent.mkdir(mode=0o700)
            write_new(path, data)
        check_role_tree(root, self.role)
        return 0, self.receipt(restored_role=self.role, generation=gen)

    def copy_fixture(self):
        data = read_bytes(self.c["fixture_file"], FIXTURE_LIMIT)
        require(data and digest(data) == hex_digest(self.c["fixture_sha256"]),
                "fixture_checksum")
        destination = self.destination(self.c["database_file"])
        private_dir(destination.parent)
        write_new(destination, data)
        return 0, self.receipt(fixture_sha256=digest(data), storage="private_ephemeral")

    def publish(self):
        root = self.root()
        gateway, target = validate_topology(root, source(self.c))
        self.control_target(gateway["control_state"])
        private_dir(root / "admin")
        for name in ("credentials.json", "tenants.json"):
            read_bytes(root / "admin" / name, 1024 * 1024)
        self.attempt("publish")
        _, data = self.invoke([self.server, "control-state", "publish",
                               "--target-file", target, "--object",
                               gateway["control_state"]["object"], "--admin-dir",
                               root / "admin", "--directory-file", root / "directory.json"], True)
        report = json_bytes(data)
        require(report.get("generation") == 1
                and report.get("object") == gateway["control_state"]["object"],
                "unknown_do_not_retry")
        return 0, self.receipt(generation=1, control_state_published=True,
                               authority_mode="off", read_routing="writer_only")

    def native_config(self, value, schema):
        with tempfile.TemporaryDirectory(prefix="native-", dir=self.work) as temporary:
            directory = Path(temporary)
            if os.name != "nt":
                directory.chmod(0o700)
            path = directory / "config.json"
            write_new(path, encode(value))
            code, data = self.invoke([self.control, "--config", path])
        self.evidence(data, schema, code)
        return code, data

    def serve(self):
        root = self.root()
        check_role_tree(root, self.role)
        metadata = artifact_provenance(root, source(self.c))
        require(os.environ.get("CONTAINER_APP_REPLICA_NAME"), "replica_identity_required")
        observing = self.op == "observe"
        duration = integer(self.c.get("duration_secs", 1200 if observing else 7200),
                           1, 3540 if observing else 7200)
        if observing:
            require(duration + 5 <= self.timeout, "observer_budget")
        if self.role == "gateway":
            require("database_file" not in self.c and "fixture_sha256" not in self.c)
            gateway = read_json(root / "gateway.json")
            self.control_target(gateway["control_state"])
            require(gateway.get("public_key_bundle_file") == str(root / "public.json")
                    and gateway.get("signing_key_file") == str(root / "signing.key"),
                    "artifact_path_mismatch")
            args = ["gateway", "--config", str(root / "gateway.json")]
        else:
            data = read_bytes(self.destination(self.c["database_file"]), FIXTURE_LIMIT)
            require(digest(data) == hex_digest(self.c["fixture_sha256"])
                    == metadata["fixture_sha256"], "fixture_checksum")
            trust = read_json(root / "trust.json")
            self.control_target(trust["control_state"])
            require(trust.get("public_key_bundle_file") == str(root / "public.json"),
                    "artifact_path_mismatch")
            args = [self.c["database_file"], "--serve-mode", self.role]
            if self.role == "writer":
                args += ["--durability", "full"]
            args += ["--bind", "0.0.0.0:8080", "--allow-remote", "--token-file",
                     str(root / "backend.token"), "--gateway-trust-file", str(root / "trust.json")]
        if not observing:
            self.attempt("serve-" + self.role)
            run_native([self.server, *args], duration, self.work, serving=True)
            return 0, self.receipt(role=self.role, duration_secs=duration,
                                   lifetime_complete=True, termination="bounded_window")
        config = {"mode": "observe", "server": {"program": str(self.server), "args": args},
                  "ready_url": "http://127.0.0.1:8080/ready",
                  "role": "gateway" if self.role == "gateway" else "backend",
                  "interval_ms": integer(self.c.get("observe_interval_ms", 50), 10, 5000),
                  "duration_secs": duration,
                  "timeout_ms": integer(self.c.get("ready_timeout_ms", 1000), 10, 1000)}
        self.attempt("serve-" + self.role)
        code, data = self.native_config(config, "vlite-meas005-observer/1")
        report = json_bytes(data)
        require(report["replica"] == os.environ["CONTAINER_APP_REPLICA_NAME"]
                and report["role"] == config["role"], "observer_identity_mismatch")
        return code, data

    def driver(self):
        path = absolute(self.c["native_config"])
        native = read_json(path)
        if self.op == "load":
            required = {"gateway_config", "token_file", "expected_body_file",
                        "gateway_url", "direct_url", "requested"}
            object_keys(native, required | {"rates", "duration_secs", "warmup_secs",
                        "recovery_secs", "max_in_flight", "timeout_ms"}, required)
            for field in ("gateway_config", "token_file", "expected_body_file"):
                read_bytes(absolute(native[field]))
            self.private_values.add(read_bytes(native["token_file"]).strip())
            gateway = read_json(native["gateway_config"])
            root = private_dir(Path(native["gateway_config"]).parent)
            self.private_roots.add(root)
            artifact_provenance(root, source(self.c))
            self.control_target(gateway["control_state"])
            for field in ("gateway_url", "direct_url"):
                origin(native[field])
            program, schema = self.load, "vlite-meas004/1"
        elif self.op == "mutate":
            required = {"mode", "server", "bootstrap_dir", "target_file", "object",
                        "gateway_url", "requested"}
            object_keys(native, required | {"shared_key_file", "poll_interval_ms",
                        "observation_deadline_ms", "mutations", "seed"}, required)
            require(native["mode"] == "run" and native["server"] == str(self.server))
            root = private_dir(native["bootstrap_dir"])
            self.private_roots.add(root)
            gateway, target = validate_topology(root, source(self.c))
            self.control_target(gateway["control_state"])
            private_dir(root / "admin")
            for name in ("credentials.json", "tenants.json"):
                read_bytes(root / "admin" / name, 1024 * 1024)
            require(native["target_file"] == str(target)
                    and native["object"] == gateway["control_state"]["object"]
                    and native.get("shared_key_file") is None, "control_source_mismatch")
            require(native.get("poll_interval_ms", 1000) == gateway["poll_interval_ms"],
                    "poll_interval_mismatch")
            origin(native["gateway_url"])
            self.attempt("mutate")
            program, schema = self.control, "vlite-meas005/1"
        else:
            object_keys(native, {"mode", "mutation_evidence_file", "replicas",
                                "fixed_inventory_confirmed"},
                        {"mode", "mutation_evidence_file", "replicas"})
            require(native["mode"] == "analyze")
            evidence = read_bytes(native["mutation_evidence_file"], REPORT_LIMIT)
            mutation = json_bytes(evidence)
            mutation_code = 0 if mutation.get("mutation_and_ingress_checks_complete") is True else 2
            self.evidence(evidence, "vlite-meas005/1", mutation_code)
            replicas = native["replicas"]
            require(isinstance(replicas, list) and 1 <= len(replicas) <= 64)
            identities = set()
            for replica in replicas:
                object_keys(replica, {"replica", "role", "report_file",
                                     "clock_offset_ms", "clock_uncertainty_ms"},
                            {"replica", "role", "report_file"})
                require(replica["replica"] not in identities, "duplicate_replica")
                identities.add(replica["replica"])
                report_data = read_bytes(replica["report_file"], REPORT_LIMIT)
                report = json_bytes(report_data)
                report_code = 2 if report.get("terminated_early") or report.get("interrupted") else 0
                self.evidence(report_data, "vlite-meas005-observer/1", report_code)
                require(report["replica"] == replica["replica"]
                        and report["role"] == replica["role"], "observer_identity_mismatch")
            program, schema = self.control, "vlite-meas005/1"
        code, data = self.invoke([program, "--config", path])
        report = self.evidence(data, schema, code)
        if self.op != "load":
            require(report["stage"] == ("mutations" if self.op == "mutate" else "analysis"),
                    "evidence_stage")
        return code, data

    def execute(self):
        if self.op in ("load", "mutate", "analyze"):
            return self.driver()
        if self.op == "clock":
            return self.native_config({"mode": "clock_sample"}, "vlite-meas005-clock/1")
        if self.op == "observe":
            return self.serve()
        if self.op == "transport":
            data = read_bytes(self.c["evidence_file"], REPORT_LIMIT)
            code = integer(self.c["native_exit_code"], 0, 2)
            self.evidence(data, self.c["evidence_schema"], code)
            return code, data
        return getattr(self, self.op)()


def main(argv=None):
    try:
        args = sys.argv[1:] if argv is None else argv
        require(len(args) == 2 and args[0] == "--config", "usage_config_file_required")
        code, data = Runtime(read_json(absolute(args[1]))).execute()
        sys.stdout.write(frame(data))
        return code
    except Failure as error:
        sys.stdout.write("VLITE-MEAS FAILED 1\n")
        sys.stderr.write(str(error) + "\n")
        return 1
    except (OSError, ValueError, TypeError, KeyError, OverflowError, RecursionError, RuntimeError,
            subprocess.SubprocessError):
        sys.stdout.write("VLITE-MEAS FAILED 1\n")
        sys.stderr.write("runtime_operation_failed\n")
        return 1


if __name__ == "__main__":
    sys.exit(main())
