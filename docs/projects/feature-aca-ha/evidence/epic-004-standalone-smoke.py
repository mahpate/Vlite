"""EPIC-004 standalone no-regression smoke run (evidence helper, not a test).

Usage: python epic-004-standalone-smoke.py <path-to-vlite-server[.exe]>

Launches the binary with the single-ACA deploy.ps1 argument shape and env
(`<db> --create-if-missing 3 --create-format 4 --bind ... --allow-remote`,
VLITE_AUTH_TOKEN, VLITE_ADMIN_DIR; every other VLITE_* variable removed),
on loopback, in a temporary directory. Checks the public contract, stops it
with the platform's graceful signal, restarts it and checks the write
survived. Exit 0 only if every check passes. Run it against a HEAD build and
the candidate build to compare.
"""
import json, os, secrets, shutil, signal, subprocess, sys, tempfile, threading, urllib.error, urllib.request

if len(sys.argv) != 2:
    sys.exit(__doc__)
EXE = os.path.abspath(sys.argv[1])
WORK = tempfile.mkdtemp(prefix="vlite-standalone-smoke-")
os.makedirs(os.path.join(WORK, "admin"))
TOKEN = secrets.token_urlsafe(32)
ARGS = [EXE, os.path.join(WORK, "vectors.vlite"), "--create-if-missing", "3",
        "--create-format", "4", "--bind", "127.0.0.1:0", "--allow-remote"]
ENV = {k: v for k, v in os.environ.items() if not k.startswith("VLITE_")}
ENV["VLITE_AUTH_TOKEN"] = TOKEN
ENV["VLITE_ADMIN_DIR"] = os.path.join(WORK, "admin")
WINDOWS = os.name == "nt"
results = []


def check(name, ok, detail=""):
    results.append((name, ok))
    print(("PASS " if ok else "FAIL ") + name + (f" ({detail})" if detail else ""))


def launch(run):
    err = open(os.path.join(WORK, f"stderr-{run}.log"), "w")
    flags = subprocess.CREATE_NEW_PROCESS_GROUP if WINDOWS else 0
    proc = subprocess.Popen(ARGS, env=ENV, stdout=subprocess.PIPE, stderr=err,
                            creationflags=flags, text=True)
    line = []
    reader = threading.Thread(target=lambda: line.append(proc.stdout.readline()), daemon=True)
    reader.start()
    reader.join(120)
    if not line or not line[0].startswith("LISTENING http://"):
        proc.kill()
        sys.exit(f"no LISTENING line: {line}")
    return proc, err, line[0].strip()[len("LISTENING http://"):]


def call(addr, method, path, body=None, token=TOKEN):
    data = None if body is None else json.dumps(body).encode()
    req = urllib.request.Request(f"http://{addr}{path}", data=data, method=method)
    if token:
        req.add_header("Authorization", f"Bearer {token}")
    if data is not None:
        req.add_header("Content-Type", "application/json")
    try:
        with urllib.request.urlopen(req, timeout=30) as reply:
            raw = reply.read()
            is_json = reply.headers.get_content_type() == "application/json"
            return reply.status, (json.loads(raw) if raw and is_json else raw)
    except urllib.error.HTTPError as error:
        raw = error.read()
        try:
            return error.code, json.loads(raw)
        except ValueError:
            return error.code, raw


def stop(proc, err):
    os.kill(proc.pid, signal.CTRL_BREAK_EVENT if WINDOWS else signal.SIGTERM)
    try:
        code = proc.wait(60)
    except subprocess.TimeoutExpired:
        proc.kill()
        code = None
    err.close()
    return code


proc, err, addr = launch(1)
check("health 200 without credentials", call(addr, "GET", "/health", token=None)[0] == 200)
# Standalone /ready needs any valid credential (only /health is public); the
# ACA probes in deploy.ps1 use /health.
check("ready 401 without credentials", call(addr, "GET", "/ready", token=None)[0] == 401)
check("ready 200 with the token", call(addr, "GET", "/ready")[0] == 200)
status, body = call(addr, "GET", "/v1/vectors/1", token=None)
check("data 401 without credentials", status == 401, f"{status} {body}")
status, body = call(addr, "GET", "/v1/database")
check("database format 4", status == 200 and body.get("format_version") == 4, f"{status} {body}")
status, body = call(addr, "PUT", "/v1/vectors/1", {"vector": [1, 2, 3]})
check("write 2xx", 200 <= status < 300, f"{status} {body}")
status, body = call(addr, "POST", "/v1/search", {"vector": [1, 2, 3], "k": 1})
check("search finds the write", status == 200 and '"1"' in json.dumps(body), f"{status}")
check("auth config 200", call(addr, "GET", "/v1/auth/config", token=None)[0] == 200)
check("dashboard 200", call(addr, "GET", "/dashboard", token=None)[0] == 200)
status, body = call(addr, "GET", "/v1/admin/serve-status")
check("serve-status standalone", status == 200 and body.get("mode") == "standalone", f"{status}")
code = stop(proc, err)
check("graceful exit 0 on the stop signal", code == 0, f"exit={code}")
log = open(os.path.join(WORK, "stderr-1.log")).read()
check("no gateway/control-state events", "gateway" not in log and "control_state" not in log)
events = [json.loads(line).get("event") for line in log.splitlines() if line.startswith("{")]
print("events:", " ".join(f"{name}x{events.count(name)}" for name in dict.fromkeys(events)))

proc, err, addr = launch(2)
status, body = call(addr, "GET", "/v1/vectors/1")
check("write survives restart", status == 200 and body.get("vector") == [1.0, 2.0, 3.0], f"{status}")
code = stop(proc, err)
check("second graceful exit 0", code == 0, f"exit={code}")

failed = [name for name, ok in results if not ok]
print(f"SUMMARY {len(results) - len(failed)}/{len(results)} passed"
      + (f"; failed: {failed}; logs kept in {WORK}" if failed else ""))
if not failed:
    shutil.rmtree(WORK, ignore_errors=True)
sys.exit(1 if failed else 0)
