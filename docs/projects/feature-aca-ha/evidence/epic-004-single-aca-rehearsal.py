"""EPIC-004 single-ACA no-regression rehearsal (evidence helper, not a test).

Usage: python epic-004-single-aca-rehearsal.py <baseline-vlite-server> <candidate-vlite-server>

Runs the same observable checks against a baseline build (the released
version in production) and the candidate build, on loopback in temporary
directories, and fails if any check fails or the two builds disagree:

* "template": the deploy.ps1 shape (`--create-if-missing 3 --create-format 4
  --allow-remote`, VLITE_AUTH_TOKEN, VLITE_ADMIN_DIR).
* "live": the template plus the overrides the live app carries
  (`--durability full`, VLITE_TRUSTED_PROXY=1), including a crash restart.
* "upgrade": a database and admin directory written by the baseline are
  served by the candidate (data, a tenant credential, an account) and then
  reopened by the baseline again.

Each check records a deterministic observation (a status code or a small
fact) that is compared between the builds. It never contacts Azure.
"""
import json, os, re, secrets, shutil, signal, subprocess, sys, tempfile, threading, time, urllib.error, urllib.request

if len(sys.argv) != 3:
    sys.exit(__doc__)
BASE, CAND = (os.path.abspath(path) for path in sys.argv[1:])
WINDOWS = os.name == "nt"
PASSWORD = "correct-horse-battery-staple"
results = []
dirs = []


def http(addr, method, path, body=None, token=None, headers=None):
    data = None if body is None else json.dumps(body).encode()
    req = urllib.request.Request(f"http://{addr}{path}", data=data, method=method)
    if token:
        req.add_header("Authorization", f"Bearer {token}")
    if data is not None:
        req.add_header("Content-Type", "application/json")
    for key, value in (headers or {}).items():
        req.add_header(key, value)
    try:
        reply = urllib.request.urlopen(req, timeout=30)
    except urllib.error.HTTPError as error:
        reply = error
    except urllib.error.URLError:
        return 0, None, None
    try:
        raw = reply.read()
        head = reply.headers
        parsed = json.loads(raw) if raw and head.get_content_type() == "application/json" else raw
        return reply.status if hasattr(reply, "status") else reply.code, head, parsed
    finally:
        reply.close()


class Server:
    def __init__(self, exe, work, token, live, label):
        self.exe, self.work, self.token, self.live, self.label = exe, work, token, live, label
        self.runs = 0

    def start(self):
        self.runs += 1
        args = [self.exe, os.path.join(self.work, "vectors.vlite"), "--create-if-missing", "3",
                "--create-format", "4", "--bind", "127.0.0.1:0", "--allow-remote"]
        env = {k: v for k, v in os.environ.items() if not k.startswith("VLITE_")}
        env["VLITE_AUTH_TOKEN"] = self.token
        env["VLITE_ADMIN_DIR"] = os.path.join(self.work, "admin")
        if self.live:
            args += ["--durability", "full"]
            env["VLITE_TRUSTED_PROXY"] = "1"
        self.err = open(os.path.join(self.work, f"stderr-{self.runs}.log"), "w")
        flags = subprocess.CREATE_NEW_PROCESS_GROUP if WINDOWS else 0
        self.proc = subprocess.Popen(args, env=env, stdout=subprocess.PIPE, stderr=self.err,
                                     creationflags=flags, text=True)
        line = []
        reader = threading.Thread(target=lambda: line.append(self.proc.stdout.readline()), daemon=True)
        reader.start()
        reader.join(120)
        if not line or not line[0].startswith("LISTENING http://"):
            self.proc.kill()
            sys.exit(f"{self.label}: no LISTENING line: {line}")
        self.addr = line[0].strip()[len("LISTENING http://"):]

    def call(self, method, path, body=None, token=True, headers=None):
        return http(self.addr, method, path, body, self.token if token is True else token, headers)

    def stop(self):
        """Graceful stop; returns (exit code, statuses other than 200 or refused seen on /ready)."""
        seen, done = set(), threading.Event()

        def sample():
            while not done.is_set():
                seen.add(http(self.addr, "GET", "/ready", None, self.token)[0])
                time.sleep(0.02)

        thread = threading.Thread(target=sample, daemon=True)
        thread.start()
        os.kill(self.proc.pid, signal.CTRL_BREAK_EVENT if WINDOWS else signal.SIGTERM)
        try:
            code = self.proc.wait(60)
        except subprocess.TimeoutExpired:
            self.proc.kill()
            code = None
        done.set()
        thread.join()
        self.err.close()
        return code, sorted(seen - {0, 200})

    def kill(self):
        self.proc.kill()
        self.proc.wait()
        self.err.close()

    def events(self):
        log = open(os.path.join(self.work, f"stderr-{self.runs}.log")).read()
        names = [json.loads(line).get("event") for line in log.splitlines() if line.startswith("{")]
        return sorted(set(names))


def record(obs, label, name, value, ok=True):
    obs[name] = value
    results.append((f"{label} {name}", ok, value))
    print(("PASS " if ok else "FAIL ") + f"{label} {name}: {value}")


def cookie_of(head):
    for value in head.get_all("Set-Cookie") or []:
        pair = value.split(";", 1)[0]
        if pair.endswith("=") or "=" not in pair:
            continue
        name, content = pair.split("=", 1)
        if content:
            return pair
    return None


def exercise(s, obs, tag):
    r = lambda name, value, ok=True: record(obs, s.label, f"{tag}/{name}", value, ok)
    tok = s.token
    r("health no credential", s.call("GET", "/health", token=None)[0], s.call("GET", "/health", token=None)[0] == 200)
    ready = s.call("GET", "/ready", token=None)[0]
    r("ready no credential", ready, ready == 401)
    ready = s.call("GET", "/ready")[0]
    r("ready credential", ready, ready == 200)
    status = s.call("GET", "/v1/vectors/1", token=None)[0]
    r("data no credential", status, status == 401)
    status, _, body = s.call("GET", "/v1/database")
    r("database format", (status, body.get("format_version")), status == 200 and body.get("format_version") == 4)
    status, _, body = s.call("PUT", "/v1/vectors/1", {"vector": [1, 2, 3]})
    r("default write", status, 200 <= status < 300)
    status, _, body = s.call("POST", "/v1/search", {"vector": [1, 2, 3], "k": 1})
    r("default search", status, status == 200 and '"1"' in json.dumps(body))

    status, _, body = s.call("POST", "/v1/collections", {"name": "docs", "dimensions": 3})
    r("collection create", status, status == 201)
    status = s.call("PUT", "/v1/collections/docs/vectors/1", {"vector": [1, 0, 0]})[0]
    r("collection write", status, 200 <= status < 300)
    status = s.call("POST", "/v1/collections/docs/batches",
                    {"operations": [{"op": "put", "id": "2", "vector": [0, 1, 0]}]})[0]
    r("collection batch", status, 200 <= status < 300)
    status, _, body = s.call("POST", "/v1/collections/docs/search", {"vector": [2, 0, 0], "k": 1})
    r("collection search", (status, body["hits"][0]["id"] if status == 200 else None),
      status == 200 and body["hits"][0]["id"] == "1")
    status, _, body = s.call("GET", "/v1/collections/docs")
    r("collection count", (status, body.get("live_records") if status == 200 else None),
      status == 200 and body.get("live_records") == 2)

    status, _, body = s.call("POST", "/v1/admin/tenants", {"name": f"rehearsal-{tag}"})
    r("tenant create", status, status == 201)
    tenant_id = body["tenant"]["id"]
    far = int(time.time() * 1000) + 3_600_000
    status, _, body = s.call("POST", "/v1/admin/credentials", {
        "name": "rehearsal", "collections": ["*"],
        "operations": ["read", "search", "write", "collections", "import", "diagnostics"],
        "expires_unix_ms": far, "tenant_id": tenant_id})
    r("credential create", status, status == 201)
    tenant_token = body["token"]
    status = http(s.addr, "POST", "/v1/collections", {"name": "mine", "dimensions": 3}, tenant_token)[0]
    r("tenant collection create", status, status == 201)
    status = http(s.addr, "PUT", "/v1/collections/mine/vectors/1", {"vector": [0, 0, 1]}, tenant_token)[0]
    r("tenant write", status, 200 <= status < 300)
    status, _, body = http(s.addr, "POST", "/v1/collections/mine/search", {"vector": [0, 0, 1], "k": 1}, tenant_token)
    r("tenant search", (status, body["hits"][0]["id"] if status == 200 else None),
      status == 200 and body["hits"][0]["id"] == "1")
    r("tenant refused on admin users", http(s.addr, "GET", "/v1/admin/users", None, tenant_token)[0],
      http(s.addr, "GET", "/v1/admin/users", None, tenant_token)[0] == 403)
    r("tenant refused on credentials", http(s.addr, "GET", "/v1/admin/credentials", None, tenant_token)[0],
      http(s.addr, "GET", "/v1/admin/credentials", None, tenant_token)[0] == 403)

    name = f"user{tag.replace('/', '')}"
    status, _, body = s.call("POST", "/v1/admin/users", {
        "username": name, "password": PASSWORD, "role": "super_admin", "must_change_password": False})
    r("account create", status, status == 201)
    sign_in(s, r, name)

    status, head, body = s.call("GET", "/dashboard", token=None)
    r("dashboard page", status, status == 200)
    assets = sorted(set(re.findall(rb'(?:src|href)="(/[^"#?]+)"', body if isinstance(body, bytes) else b"")))
    r("dashboard assets", {a.decode(): s.call("GET", a.decode(), token=None)[0] for a in assets},
      all(s.call("GET", a.decode(), token=None)[0] == 200 for a in assets))
    r("auth config", s.call("GET", "/v1/auth/config", token=None)[0], True)
    for path in ("/v1/capabilities", "/metrics", "/v1/admin/compaction", "/v1/admin/cloud-backup",
                 "/v1/admin/users", "/v1/admin/tenants", "/v1/admin/credentials"):
        status = s.call("GET", path)[0]
        r(f"GET {path}", status, status == 200)
    # Added after the baseline release: the baseline answers 404, the candidate reports standalone.
    status, _, body = s.call("GET", "/v1/admin/serve-status")
    mode = body.get("mode") if status == 200 else None
    expected = (404, None) if s.label.startswith("baseline") else (200, "standalone")
    r("serve-status (new route)", (status, mode), (status, mode) == expected)
    obs.pop(f"{tag}/serve-status (new route)", None)
    return tenant_token


def sign_in(s, r, name):
    headers = {"sec-fetch-site": "same-origin", "x-vlite-request": "1"}
    if s.live:
        headers["x-forwarded-proto"] = "https"
    status, head, body = http(s.addr, "POST", "/v1/auth/login",
                              {"username": name, "password": PASSWORD}, None, headers)
    r("login", status, status == 200)
    cookie = cookie_of(head) if status == 200 else None
    r("login cookie", cookie is not None, cookie is not None)
    if cookie is None:
        return
    headers["cookie"] = cookie
    status, _, body = http(s.addr, "GET", "/v1/auth/session", None, None, headers)
    r("session", (status, body.get("method") if status == 200 else None),
      status == 200 and body.get("method") == "password")
    status = http(s.addr, "GET", "/v1/collections", None, None, headers)[0]
    r("session lists collections", status, status == 200)
    status = http(s.addr, "POST", "/v1/auth/logout", None, None, headers)[0]
    r("logout", status, status == 200)
    status = http(s.addr, "GET", "/v1/auth/session", None, None, headers)[0]
    r("session after logout", status, status == 401)


def persists(s, obs, tag, ids):
    r = lambda name, value, ok=True: record(obs, s.label, f"{tag}/{name}", value, ok)
    status, _, body = s.call("GET", "/v1/vectors/1")
    r("default record", (status, body.get("vector") if status == 200 else None),
      status == 200 and body.get("vector") == [1.0, 2.0, 3.0])
    for ident in ids:
        status, _, body = s.call("GET", f"/v1/collections/docs/vectors/{ident}")
        r(f"collection record {ident}", status, status == 200)


def scratch():
    work = tempfile.mkdtemp(prefix="vlite-single-aca-")
    os.makedirs(os.path.join(work, "admin"))
    dirs.append(work)
    return work


def scenario(exe, label, live):
    work, token, obs = scratch(), secrets.token_urlsafe(32), {}
    s = Server(exe, work, token, live, label)
    tag = "live" if live else "template"
    s.start()
    exercise(s, obs, tag)
    code, odd = s.stop()
    record(obs, label, f"{tag}/graceful exit", code, code == 0)
    record(obs, label, f"{tag}/ready while stopping", odd, odd == [])
    events = s.events()
    record(obs, label, f"{tag}/no gateway events", [e for e in events if e and ("gateway" in e or "control_state" in e)], True)
    obs[f"{tag}/events"] = events
    s.start()
    persists(s, obs, f"{tag}-restart", ["1", "2"])
    if live:
        status = s.call("PUT", "/v1/collections/docs/vectors/99", {"vector": [9, 9, 9]})[0]
        record(obs, label, f"{tag}/write before crash", status, 200 <= status < 300)
        s.kill()
        s.start()
        status = s.call("GET", "/v1/collections/docs/vectors/99")[0]
        record(obs, label, f"{tag}/write survives a crash (durability full)", status, status == 200)
    code, odd = s.stop()
    record(obs, label, f"{tag}/second graceful exit", code, code == 0)
    return obs


def upgrade():
    work, token, obs = scratch(), secrets.token_urlsafe(32), {}
    first = Server(BASE, work, token, True, "baseline-writes")
    first.start()
    tenant_token = exercise(first, {}, "seed")
    first.stop()
    second = Server(CAND, work, token, True, "candidate-serves-baseline-files")
    second.start()
    persists(second, obs, "upgrade", ["1", "2"])
    status, _, body = second.call("GET", "/v1/database")
    record(obs, second.label, "upgrade/format", (status, body.get("format_version")), status == 200 and body.get("format_version") == 4)
    status, _, body = http(second.addr, "POST", "/v1/collections/mine/search", {"vector": [0, 0, 1], "k": 1}, tenant_token)
    record(obs, second.label, "upgrade/old tenant credential still works",
           (status, body["hits"][0]["id"] if status == 200 else None), status == 200 and body["hits"][0]["id"] == "1")
    record(obs, second.label, "upgrade/old account still signs in", None, True)
    sign_in(second, lambda name, value, ok=True: record(obs, second.label, f"upgrade/{name}", value, ok), "userseed")
    status = second.call("PUT", "/v1/collections/docs/vectors/3", {"vector": [0, 0, 1]})[0]
    record(obs, second.label, "upgrade/write on candidate", status, 200 <= status < 300)
    code, _ = second.stop()
    record(obs, second.label, "upgrade/candidate graceful exit", code, code == 0)
    third = Server(BASE, work, token, True, "baseline-reopens-candidate-files")
    third.start()
    persists(third, {}, "rollback", ["1", "2", "3"])
    code, _ = third.stop()
    record({}, third.label, "rollback/baseline graceful exit", code, code == 0)


def main():
    base = {"template": scenario(BASE, "baseline", False), "live": scenario(BASE, "baseline", True)}
    cand = {"template": scenario(CAND, "candidate", False), "live": scenario(CAND, "candidate", True)}
    upgrade()
    differences = []
    for key in base:
        for name in sorted(set(base[key]) | set(cand[key])):
            if base[key].get(name) != cand[key].get(name):
                differences.append((name, base[key].get(name), cand[key].get(name)))
    for name, old, new in differences:
        print(f"DIFF {name}: baseline={old} candidate={new}")
    failed = [name for name, ok, _ in results if not ok]
    print(f"SUMMARY {len(results) - len(failed)}/{len(results)} checks passed; "
          f"{len(differences)} baseline/candidate differences")
    if failed:
        print("failed:", failed)
    if not failed and not differences:
        for work in dirs:
            shutil.rmtree(work, ignore_errors=True)
    else:
        print("logs kept in", dirs)
    sys.exit(1 if failed or differences else 0)


main()
