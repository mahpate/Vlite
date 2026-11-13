"""Qualify single-file compaction on one local Linux filesystem (profile linux-local-ext4-v1).

Runs on Linux only, with the Python standard library, against a vlite-server built WITHOUT
the compaction test hooks (the production profile: real size floors, real filesystem gate,
30-second scheduler tick):

    cargo build --release --features server --bin vlite-server
    python3 tests/compaction_qualification.py --server target/release/vlite-server \
        --data /data --unqualified /tmp/vlite-unqualified [--source-mib 960] [--evidence out.json]

The documented run uses a disposable Docker named volume (ext4 on the Docker host) mounted at
/data inside a python:3.12-slim container, and the container's overlay root as the
unqualified location. The data directory is wiped first, so never point it at real data.

Checks, in order (every check asserts; the script exits non-zero on the first failure):
  1. Filesystem evidence: mount type, device, exclusive flock, file+directory fsync, atomic
     rename and same-device admin directory.
  2. Refusal: the same binary on an unqualified filesystem reports unqualified_filesystem and
     refuses a start; the qualified volume reports the profile available.
  3. Scheduled run: default-off settings, then a UTC window at the current minute with
     history consent; measured growth over the baseline starts one scheduled run.
  4. Manual run at the qualified maximum source size with concurrent writers: duration,
     unavailability span, peak memory, every acknowledged write, vector/graph CAS, IDs.
  5. Process kill during the build and during the switch, then restart: data intact, no
     stale selection, a later run succeeds.
"""

import argparse
import base64
import fcntl
import json
import os
import shutil
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.request
import uuid
from pathlib import Path

TOKEN = "compaction_qualification_admin_0123456789_ABCDEFG"
DIM = 64
PAYLOAD = base64.b64encode(os.urandom(3072)).decode()
MIB = 1 << 20
EVIDENCE = {}


def log(message):
    print(f"[{time.strftime('%H:%M:%S')}] {message}", flush=True)


def mountinfo(path):
    path = os.path.realpath(path)
    best = None
    with open("/proc/self/mountinfo", encoding="utf-8") as handle:
        for line in handle:
            left, right = line.split(" - ", 1)
            point = left.split(" ")[4].encode().decode("unicode_escape")
            inside = path == point or path.startswith(point.rstrip("/") + "/")
            if inside and (best is None or len(point) >= len(best[0])):
                best = (point, right.split(" ")[0], right.split(" ")[1])
    return {"mount_point": best[0], "fstype": best[1], "source": best[2]}


def probe_filesystem(directory):
    """The primitives the coordinator relies on, measured on the target mount."""
    directory.mkdir(parents=True, exist_ok=True)
    result = mountinfo(directory)
    lock_path = directory / "probe.lock"
    first = open(lock_path, "w")
    fcntl.flock(first, fcntl.LOCK_EX | fcntl.LOCK_NB)
    second = open(lock_path, "w")
    try:
        fcntl.flock(second, fcntl.LOCK_EX | fcntl.LOCK_NB)
        result["exclusive_lock"] = False
    except BlockingIOError:
        result["exclusive_lock"] = True
    second.close()
    first.close()
    data = os.urandom(8 * MIB)
    started = time.monotonic()
    temporary = directory / "probe.tmp"
    with open(temporary, "wb") as handle:
        handle.write(data)
        handle.flush()
        os.fsync(handle.fileno())
    result["file_fsync_8mib_ms"] = round((time.monotonic() - started) * 1000, 1)
    started = time.monotonic()
    os.replace(temporary, directory / "probe.bin")
    descriptor = os.open(directory, os.O_RDONLY)
    os.fsync(descriptor)
    os.close(descriptor)
    result["rename_dir_fsync_ms"] = round((time.monotonic() - started) * 1000, 1)
    result["rename_atomic"] = (directory / "probe.bin").read_bytes() == data and not temporary.exists()
    sub = directory / "admin-probe"
    sub.mkdir(exist_ok=True)
    result["same_device_admin_dir"] = os.stat(sub).st_dev == os.stat(directory / "probe.bin").st_dev
    for path in (lock_path, directory / "probe.bin"):
        path.unlink()
    sub.rmdir()
    usage = shutil.disk_usage(directory)
    result["free_gib"] = round(usage.free / (1 << 30), 1)
    assert result["exclusive_lock"] and result["rename_atomic"] and result["same_device_admin_dir"], result
    return result


class Reply:
    def __init__(self, status, body, headers):
        self.status, self.body, self.headers = status, body, headers


class Server:
    def __init__(self, binary, directory, create=True):
        self.binary, self.directory = binary, directory
        self.process = None
        self.start(create)

    def start(self, create=False):
        env = {key: value for key, value in os.environ.items() if not key.startswith("VLITE_")}
        env.update({"VLITE_AUTH_TOKEN": TOKEN, "VLITE_ADMIN_DIR": str(self.directory / "admin")})
        argv = [str(self.binary), str(self.directory / "live.vlite"), "--bind", "127.0.0.1:0"]
        if create:
            argv += ["--create-if-missing", str(DIM)]
        self.process = subprocess.Popen(argv, env=env, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True)
        line = self.process.stdout.readline()
        assert line.startswith("LISTENING http://"), f"server did not start: {line!r}"
        self.base = line.strip().removeprefix("LISTENING ")

    def call(self, method, path, body=None, headers=None, timeout=120):
        data = None if body is None else json.dumps(body).encode()
        request = urllib.request.Request(self.base + path, data=data, method=method, headers={
            "Authorization": f"Bearer {TOKEN}", "Content-Type": "application/json", **(headers or {})})
        try:
            with urllib.request.urlopen(request, timeout=timeout) as response:
                raw = response.read()
                return Reply(response.status, json.loads(raw) if raw else None, response.headers)
        except urllib.error.HTTPError as error:
            raw = error.read()
            try:
                body = json.loads(raw) if raw else None
            except ValueError:
                body = raw.decode(errors="replace")
            return Reply(error.code, body, error.headers)

    def ok(self, method, path, body=None):
        reply = self.call(method, path, body)
        assert reply.status < 300, (method, path, reply.status, reply.body)
        return reply.body

    def status(self):
        return self.ok("GET", "/v1/admin/compaction")

    def submit(self):
        version = self.status()["settings_version"]
        reply = self.call("POST", "/v1/admin/compaction/jobs", {
            "expected_version": version, "acknowledge_downtime": True,
            "acknowledge_history": True, "idempotency_key": str(uuid.uuid4())})
        assert reply.status == 202, (reply.status, reply.body)
        return reply.body["job"]["id"]

    def job(self, job_id):
        reply = self.call("GET", f"/v1/admin/compaction/jobs/{job_id}", timeout=10)
        return reply.body["job"] if reply.status == 200 else None

    def finish(self, job_id, budget=1800):
        deadline = time.monotonic() + budget
        while time.monotonic() < deadline:
            try:
                job = self.job(job_id)
            except OSError:
                job = None
            if job and job.get("outcome"):
                return job
            time.sleep(0.5)
        raise AssertionError(f"job {job_id} did not finish")

    def peak_rss_mib(self):
        with open(f"/proc/{self.process.pid}/status", encoding="utf-8") as handle:
            for line in handle:
                if line.startswith("VmHWM:"):
                    return round(int(line.split()[1]) / 1024, 1)
        return None

    def kill(self):
        if self.process and self.process.poll() is None:
            self.process.kill()
        if self.process:
            self.process.wait(60)

    def file_bytes(self):
        return self.status()["file"]["bytes"]


def vector(vector_id, round_number):
    return [float(vector_id % 997), float(round_number)] + [0.5] * (DIM - 2)


def churn(server, ids, rounds, start_round=0):
    """Overwrites the same records in atomic batches so the file accumulates dead space."""
    for round_number in range(start_round, start_round + rounds):
        for first in range(ids.start, ids.stop, 500):
            operations = [{"op": "put", "id": str(i), "vector": vector(i, round_number), "payload_base64": PAYLOAD}
                          for i in range(first, min(first + 500, ids.stop))]
            reply = server.call("POST", "/v1/batches", {"operations": operations})
            assert reply.status == 204, (reply.status, reply.body)
    return start_round + rounds


def grow_to(server, target_bytes, ids, round_number):
    while server.file_bytes() < target_bytes:
        round_number = churn(server, ids, 1, round_number)
    return round_number


def assert_vectors(server, ids, round_number, sample=50):
    step = max(1, (ids.stop - ids.start) // sample)
    for vector_id in range(ids.start, ids.stop, step):
        body = server.ok("GET", f"/v1/vectors/{vector_id}")
        assert body["vector"][:2] == vector(vector_id, round_number)[:2], (vector_id, body["vector"][:2])
        assert body["payload_base64"] == PAYLOAD


class Writers:
    """Concurrent single-record writers plus a reader that times the unavailability span."""

    def __init__(self, server, base_id, threads=4):
        self.server, self.base_id = server, base_id
        self.acked, self.refused, self.errors = {}, 0, []
        self.unavailable = []
        self.stop = threading.Event()
        self.lock = threading.Lock()
        self.threads = [threading.Thread(target=self.write, args=(n,), daemon=True) for n in range(threads)]
        self.threads.append(threading.Thread(target=self.read, daemon=True))
        for thread in self.threads:
            thread.start()

    def write(self, lane):
        counter = 0
        while not self.stop.is_set():
            vector_id = self.base_id + lane * 100_000 + counter % 1000
            counter += 1
            try:
                reply = self.server.call("PUT", f"/v1/vectors/{vector_id}",
                                         {"vector": vector(vector_id, counter)}, timeout=60)
            except OSError as error:
                with self.lock:
                    self.errors.append(str(error))
                time.sleep(0.05)
                continue
            with self.lock:
                if reply.status == 204:
                    self.acked[vector_id] = counter
                elif reply.status == 503:
                    self.refused += 1
                    assert (reply.body or {}).get("error", {}).get("code") == "maintenance_in_progress", reply.body
                else:
                    self.errors.append(f"{reply.status} {reply.body}")
            if reply.status == 503:
                time.sleep(0.2)

    def read(self):
        started = None
        while not self.stop.is_set():
            try:
                status = self.server.call("GET", "/v1/vectors/1", timeout=10).status
            except OSError:
                status = 0
            now = time.monotonic()
            if status == 200 and started is not None:
                self.unavailable.append(now - started)
                started = None
            elif status != 200 and started is None:
                started = now
            time.sleep(0.05)

    def finish(self):
        self.stop.set()
        for thread in self.threads:
            thread.join(120)
        for vector_id, counter in self.acked.items():
            body = self.server.ok("GET", f"/v1/vectors/{vector_id}")
            assert body["vector"][:2] == vector(vector_id, counter)[:2], ("acknowledged write lost", vector_id)
        assert not [e for e in self.errors if not e.startswith("<urlopen")], self.errors[:5]
        return {"acknowledged_records": len(self.acked), "refused_503": self.refused,
                "max_unavailable_seconds": round(max(self.unavailable, default=0.0), 2)}


def graph_setup(server):
    server.ok("POST", "/v1/graphs", {"graph": "g"})
    server.ok("POST", "/v1/graphs/g/batch", {"ops": [
        {"op": "create_node", "key": "a", "labels": ["Doc"]},
        {"op": "create_node", "key": "b"},
        {"op": "create_edge", "key": "ab", "source": "a", "relationship": "LINKS", "destination": "b"}]})
    probe = server.ok("POST", "/v1/graphs/g/batch", {"ops": [{"op": "create_node", "key": "probe"}]})
    server.ok("POST", "/v1/graphs/g/batch", {"ops": [{"op": "delete_node", "key": "probe"}]})
    return int(probe["outcomes"][0]["node"]["id"])


def node(server, key):
    return server.ok("POST", "/v1/graphs/g/nodes/get", {"node": {"key": key}})["node"]


def check_cas(server, stale_revision, stale_etag, deleted_high, sequence):
    reply = server.call("PUT", "/v1/vectors/1", {"vector": vector(1, 0)}, {"If-Match": stale_etag})
    assert reply.status == 412, ("stale ETag", reply.status)
    reply = server.call("POST", "/v1/graphs/g/batch", {"ops": [
        {"op": "patch_node", "key": "a", "add_labels": ["X"], "expected": {"revision": stale_revision}}]})
    assert reply.status in (409, 412), ("stale graph CAS", reply.status, reply.body)
    current = node(server, "a")
    reply = server.ok("POST", "/v1/graphs/g/batch", {"ops": [
        {"op": "patch_node", "key": "a", "add_labels": ["X"], "expected": {"revision": current["revision"]}}]})
    assert int(reply["revision"]) > sequence, ("revision must exceed the compacted sequence", reply, sequence)
    fresh = server.ok("POST", "/v1/graphs/g/batch", {"ops": [{"op": "create_node", "key": f"fresh-{sequence}"}]})
    assert int(fresh["outcomes"][0]["node"]["id"]) > deleted_high, "node IDs must not be reused"
    edge = server.ok("POST", "/v1/graphs/g/edges/get", {"edge": {"key": "ab"}})
    assert edge["edge"]["key"] == "ab", edge


def refusal(binary, directory):
    """The same binary on an unqualified mount refuses compaction and keeps serving."""
    shutil.rmtree(directory, ignore_errors=True)
    directory.mkdir(parents=True)
    server = Server(binary, directory)
    try:
        status = server.status()
        codes = [reason["code"] for reason in status["reasons"]]
        assert status["capability"] is True and status["test_profile"] is False, status
        assert status["available"] is False and "unqualified_filesystem" in codes, status
        reply = server.call("POST", "/v1/admin/compaction/jobs", {
            "expected_version": status["settings_version"], "acknowledge_downtime": True,
            "acknowledge_history": True, "idempotency_key": str(uuid.uuid4())})
        assert reply.status in (409, 422), (reply.status, reply.body)
        server.ok("PUT", "/v1/vectors/1", {"vector": vector(1, 0)})
        return {"mount": mountinfo(directory), "reasons": codes, "start_status": reply.status,
                "start_code": (reply.body or {}).get("error", {}).get("code")}
    finally:
        server.kill()
        shutil.rmtree(directory, ignore_errors=True)


def scheduled_run(server, ids):
    """A window at the current UTC minute plus measured growth starts exactly one scheduled run."""
    status = server.status()
    assert status["settings"]["enabled"] is False, "automatic compaction is off by default"
    round_number = grow_to(server, 80 * MIB, ids, 0)
    now = time.gmtime()
    settings = dict(status["settings"], enabled=True, daily_start_utc=f"{now.tm_hour:02}:{now.tm_min:02}",
                    window_minutes=10, max_run_seconds=300, min_file_bytes=64 * MIB, min_growth_bytes=64 * MIB,
                    min_growth_percent=10, cooldown_hours=24, history_policy="resnapshot_allowed")
    server.ok("PUT", "/v1/admin/compaction/config", {"expected_version": status["settings_version"], "settings": settings,
                                                     "acknowledge_history": True})
    deadline = time.monotonic() + 120
    while server.status()["schedule"].get("baseline_bytes") is None:
        assert time.monotonic() < deadline, "the scheduler never recorded a baseline"
        time.sleep(2)
    baseline = server.status()["schedule"]["baseline_bytes"]
    round_number = grow_to(server, baseline + max(64 * MIB, baseline // 10) + 4 * MIB, ids, round_number)
    grown = server.file_bytes()
    deadline = time.monotonic() + 180
    while True:
        status = server.status()
        last = status.get("last_job") or {}
        if last.get("trigger") == "scheduled":
            break
        assert time.monotonic() < deadline, ("no scheduled run started", status["schedule"], status["reasons"])
        time.sleep(2)
    job = server.finish(last["id"])
    assert job["outcome"] == "succeeded", job
    assert_vectors(server, ids, round_number - 1)
    status = server.status()
    assert status["schedule"]["next_eligible_unix_ms"] > time.time() * 1000 + 23 * 3_600_000, status["schedule"]
    disabled = dict(status["settings"], enabled=False)
    server.ok("PUT", "/v1/admin/compaction/config", {"expected_version": status["settings_version"], "settings": disabled})
    return round_number, {"baseline_bytes": baseline, "grown_bytes": grown, "before_bytes": job["before_bytes"],
                          "after_bytes": job["after_bytes"], "outcome": job["outcome"],
                          "cooldown_until_hours": round((status["schedule"]["next_eligible_unix_ms"] / 1000 - time.time()) / 3600, 1)}


def manual_at_maximum(server, ids, round_number, target_bytes):
    """The largest qualified source, with concurrent writers, graph CAS and ID checks."""
    deleted_high = graph_setup(server)
    stale_revision = node(server, "a")["revision"]
    server.ok("POST", "/v1/graphs/g/batch", {"ops": [{"op": "patch_node", "key": "a", "add_labels": ["Seen"]}]})
    started = time.monotonic()
    round_number = grow_to(server, target_bytes, ids, round_number)
    stale_etag = server.call("GET", "/v1/vectors/1").headers["ETag"]
    round_number = churn(server, ids, 1, round_number)  # every record (and the stale ETag) moves on
    log(f"source grown to {server.file_bytes() / MIB:.0f} MiB in {time.monotonic() - started:.0f}s")
    writers = Writers(server, base_id=10_000_000)
    time.sleep(2)
    begun = time.monotonic()
    job = server.finish(server.submit())
    duration = time.monotonic() - begun
    time.sleep(2)
    concurrency = writers.finish()
    assert job["outcome"] == "succeeded", job
    assert job["after_bytes"] < job["before_bytes"], job
    assert server.status()["file"]["name"] == job["candidate"], "the server uses the selected candidate"
    assert_vectors(server, ids, round_number - 1)
    check_cas(server, stale_revision, stale_etag, deleted_high, job["sequence"])
    return round_number, {"before_bytes": job["before_bytes"], "after_bytes": job["after_bytes"],
                          "duration_seconds": round(duration, 1), "peak_rss_mib": server.peak_rss_mib(),
                          "concurrency": concurrency}


def kill_during(server, ids, round_number, phases, label):
    """SIGKILL the whole process while a run is in one of `phases`, restart and verify."""
    round_number = churn(server, ids, 2, round_number)
    source = server.status()["file"]["name"]
    job_id = server.submit()
    seen = None
    deadline = time.monotonic() + 600
    while seen is None:
        job = server.job(job_id) if server.process.poll() is None else None
        assert job and not job.get("outcome"), ("finished before the kill point", job)
        if job["phase"] in phases:
            seen = job["phase"]
        assert time.monotonic() < deadline, "kill phase not reached"
        time.sleep(0.01)
    server.kill()
    server.start()
    deadline = time.monotonic() + 300
    while True:
        try:
            status = server.status()
            if status["fence"] == "open" and status["job"] is None:
                break
        except OSError:
            pass
        assert time.monotonic() < deadline, "server did not settle after the kill"
        time.sleep(1)
    assert status["recovery"]["required"] is False, status["recovery"]
    assert_vectors(server, ids, round_number - 1)
    last = status["last_job"] or {}
    result = {"phase_killed": seen, "file_before": source, "file_after": status["file"]["name"],
              "recorded_outcome": last.get("outcome"), "recorded_code": last.get("code"),
              "selection": last.get("selection")}
    after = server.finish(server.submit())
    assert after["outcome"] in ("succeeded", "skipped"), after
    assert_vectors(server, ids, round_number - 1)
    result["next_run"] = after["outcome"]
    log(f"{label}: {result}")
    return round_number, result


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--server", required=True, type=Path)
    parser.add_argument("--data", required=True, type=Path)
    parser.add_argument("--unqualified", required=True, type=Path)
    parser.add_argument("--source-mib", type=int, default=960)
    parser.add_argument("--evidence", type=Path)
    args = parser.parse_args()
    assert sys.platform.startswith("linux"), "Linux only"
    for child in args.data.iterdir() if args.data.exists() else ():
        shutil.rmtree(child) if child.is_dir() else child.unlink()
    EVIDENCE["kernel"] = os.uname().release
    EVIDENCE["filesystem"] = probe_filesystem(args.data)
    assert EVIDENCE["filesystem"]["fstype"] == "ext4", EVIDENCE["filesystem"]
    log(f"filesystem: {EVIDENCE['filesystem']}")
    EVIDENCE["refusal"] = refusal(args.server, args.unqualified)
    log(f"refusal: {EVIDENCE['refusal']}")
    server = Server(args.server, args.data)
    try:
        status = server.status()
        assert status["profile"] == "linux-local-ext4-v1" and status["test_profile"] is False, status
        codes = [reason["code"] for reason in status["reasons"]]
        assert "unqualified_filesystem" not in codes, codes
        ids = range(1, 2001)
        round_number, EVIDENCE["scheduled"] = scheduled_run(server, ids)
        log(f"scheduled: {EVIDENCE['scheduled']}")
        round_number, EVIDENCE["manual_maximum"] = manual_at_maximum(server, ids, round_number, args.source_mib * MIB)
        log(f"manual: {EVIDENCE['manual_maximum']}")
        round_number, EVIDENCE["kill_building"] = kill_during(server, ids, round_number, ("freezing", "building"), "kill during build")
        round_number, EVIDENCE["kill_switch"] = kill_during(server, ids, round_number, ("validating", "selecting", "reopening"), "kill during switch")
        EVIDENCE["final"] = {"file": server.status()["file"], "retained": server.status()["recovery"]}
    finally:
        server.kill()
    text = json.dumps(EVIDENCE, indent=2)
    if args.evidence:
        args.evidence.write_text(text + "\n", encoding="utf-8")
    print(text)
    print("compaction qualification passed")


if __name__ == "__main__":
    main()
