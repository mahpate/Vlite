//! Managed-runtime file compaction, end to end against the real binary. The
//! qualified profile is Linux ext4; these tests use the test profile
//! (`VLITE_COMPACTION_TEST_PROFILE=1`, which only lifts the size floors and the
//! filesystem gate) and the `compaction-test-hooks` fault points.

use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::TcpStream,
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::mpsc,
    time::{Duration, Instant},
};

const TOKEN: &str = "compaction_admin_secret_0123456789_ABCDEFGHIJ";
const READ: &str = "compaction_read_secret_0123456789_ABCDEFGHIJK";
const USER: &str = "operator";
const PASSWORD: &str = "correct horse battery staple 42";
const PAYLOAD: &str = "QUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFB";

struct Fixture {
    dir: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        Self {
            dir: tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap(),
        }
    }
    fn db(&self) -> PathBuf {
        self.dir.path().join("live.vlite")
    }
    fn admin(&self) -> PathBuf {
        self.dir.path().join("admin")
    }
    fn start(&self, faults: &str) -> Server {
        Server::start(self, faults, true)
    }
}

struct Server {
    child: Child,
    address: String,
}

struct Reply {
    status: u16,
    head: String,
    body: Value,
}

impl Reply {
    fn header(&self, name: &str) -> Option<&str> {
        self.head.lines().find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.trim()
                .eq_ignore_ascii_case(name)
                .then_some(value.trim())
        })
    }
    fn code(&self) -> (u16, Value) {
        (self.status, self.body["error"]["code"].clone())
    }
}

impl Server {
    fn start(f: &Fixture, faults: &str, admin: bool) -> Self {
        let password = f.dir.path().join("password.txt");
        std::fs::write(&password, PASSWORD).unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_vlite-server"));
        command
            .arg(f.db())
            .args(["--create-if-missing", "2", "--bind", "127.0.0.1:0"])
            .env("VLITE_AUTH_TOKEN", TOKEN)
            .env("VLITE_READ_ONLY_TOKEN", READ)
            .env("VLITE_COMPACTION_TEST_PROFILE", "1")
            .env("VLITE_COMPACTION_FAULT", faults)
            .env_remove("VLITE_AUTH_TOKEN_FILE")
            .env_remove("VLITE_READ_ONLY_TOKEN_FILE")
            .env_remove("VLITE_PROVIDERS_FILE")
            .env_remove("VLITE_ADMIN_DIR")
            .env_remove("VLITE_ADMIN_USERNAME")
            .env_remove("VLITE_ADMIN_PASSWORD_FILE")
            .stdout(Stdio::piped())
            .stderr(if std::env::var_os("VLITE_TEST_STDERR").is_some() {
                Stdio::inherit()
            } else {
                Stdio::null()
            });
        if admin {
            command
                .env("VLITE_ADMIN_DIR", f.admin())
                .env("VLITE_ADMIN_USERNAME", USER)
                .env("VLITE_ADMIN_PASSWORD_FILE", &password);
        }
        let mut child = command.spawn().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let mut line = String::new();
            let _ = BufReader::new(stdout).read_line(&mut line);
            let _ = sender.send(line);
        });
        let line = receiver
            .recv_timeout(Duration::from_secs(120))
            .expect("server startup timed out");
        let address = line
            .trim()
            .strip_prefix("LISTENING http://")
            .unwrap_or_else(|| panic!("startup line: {line:?}"))
            .to_owned();
        Self { child, address }
    }

    fn send(&self, method: &str, path: &str, headers: &[(&str, &str)], body: &Value) -> Reply {
        let body = if body.is_null() {
            String::new()
        } else {
            body.to_string()
        };
        let mut stream = TcpStream::connect(&self.address).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(120)))
            .unwrap();
        let mut request = format!(
            "{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n",
            body.len()
        );
        for (name, value) in headers {
            request.push_str(&format!("{name}: {value}\r\n"));
        }
        request.push_str("\r\n");
        request.push_str(&body);
        stream.write_all(request.as_bytes()).unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        let (head, body) = response.split_once("\r\n\r\n").unwrap();
        let status = head.split_whitespace().nth(1).unwrap().parse().unwrap();
        Reply {
            status,
            head: head.to_owned(),
            body: serde_json::from_str(body).unwrap_or(Value::Null),
        }
    }

    fn admin(&self, method: &str, path: &str, extra: &[(&str, &str)], body: &Value) -> Reply {
        let auth = format!("Bearer {TOKEN}");
        let mut headers = vec![("Authorization", auth.as_str())];
        headers.extend_from_slice(extra);
        self.send(method, path, &headers, body)
    }

    fn call(&self, method: &str, path: &str, body: Value) -> Reply {
        self.admin(method, path, &[], &body)
    }

    fn status(&self) -> Value {
        let reply = self.call("GET", "/v1/admin/compaction", Value::Null);
        assert_eq!(reply.status, 200, "{}", reply.body);
        reply.body
    }

    fn submit(&self, key: &str) -> Reply {
        let version = self.status()["settings_version"].clone();
        self.call(
            "POST",
            "/v1/admin/compaction/jobs",
            json!({"expected_version": version, "acknowledge_downtime": true,
                "acknowledge_history": true, "idempotency_key": key}),
        )
    }

    fn compact(&self, key: &str) -> Value {
        let reply = self.submit(key);
        assert_eq!(reply.status, 202, "{}", reply.body);
        self.finish(reply.body["job"]["id"].as_str().unwrap())
    }

    fn job(&self, id: &str) -> Value {
        let reply = self.call(
            "GET",
            &format!("/v1/admin/compaction/jobs/{id}"),
            Value::Null,
        );
        assert_eq!(reply.status, 200, "{}", reply.body);
        reply.body["job"].clone()
    }

    fn finish(&self, id: &str) -> Value {
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            let job = self.job(id);
            if !job["outcome"].is_null() {
                return job;
            }
            assert!(Instant::now() < deadline, "job did not finish: {job}");
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn wait_phase(&self, id: &str, phase: &str) {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let job = self.job(id);
            if job["phase"] == phase {
                return;
            }
            assert!(job["outcome"].is_null(), "finished early: {job}");
            assert!(
                Instant::now() < deadline,
                "phase {phase} not reached: {job}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn wait_fence(&self, fence: &str) -> Value {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let status = self.status();
            if status["fence"] == fence {
                return status;
            }
            assert!(Instant::now() < deadline, "{status}");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Waits for a crash-point fault to abort the process.
    fn wait_exit(mut self) {
        let deadline = Instant::now() + Duration::from_secs(120);
        while self.child.try_wait().unwrap().is_none() {
            assert!(Instant::now() < deadline, "server did not crash");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn login(&self) -> String {
        let reply = self.send(
            "POST",
            "/v1/auth/login",
            &[("X-Vlite-Request", "1")],
            &json!({"username": USER, "password": PASSWORD}),
        );
        assert_eq!(reply.status, 200, "{}", reply.body);
        reply
            .header("set-cookie")
            .expect("session cookie")
            .split(';')
            .next()
            .unwrap()
            .to_owned()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        if self.child.try_wait().unwrap().is_none() {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
    }
}

fn key(n: u32) -> String {
    format!("00000000-0000-4000-8000-{n:012x}")
}

/// Garbage to reclaim: repeatedly overwritten records; the last round wins.
fn churn(server: &Server, base: u64, rounds: u32) {
    for round in 0..rounds {
        for id in base..base + 10 {
            let reply = server.call(
                "PUT",
                &format!("/v1/vectors/{id}"),
                json!({"vector": [id as f32, round as f32], "payload_base64": PAYLOAD}),
            );
            assert_eq!(reply.status, 204, "{}", reply.body);
        }
    }
}

fn assert_churned(server: &Server, base: u64, round: f64) {
    for id in base..base + 10 {
        let reply = server.call("GET", &format!("/v1/vectors/{id}"), Value::Null);
        assert_eq!(reply.status, 200, "{}", reply.body);
        assert_eq!(reply.body["vector"], json!([id as f64, round]));
        assert_eq!(reply.body["payload_base64"], PAYLOAD);
    }
}

fn batch(server: &Server, ops: Value) -> Reply {
    server.call("POST", "/v1/graphs/g/batch", json!({"ops": ops}))
}

fn node(server: &Server, key: &str) -> Value {
    let reply = server.call(
        "POST",
        "/v1/graphs/g/nodes/get",
        json!({"node": {"key": key}}),
    );
    assert_eq!(reply.status, 200, "{}", reply.body);
    reply.body["node"].clone()
}

fn etag(server: &Server, id: u64) -> String {
    let reply = server.call("GET", &format!("/v1/vectors/{id}"), Value::Null);
    assert_eq!(reply.status, 200, "{}", reply.body);
    reply.header("etag").expect("etag").to_owned()
}

fn put_if_match(server: &Server, id: u64, tag: &str) -> u16 {
    server
        .admin(
            "PUT",
            &format!("/v1/vectors/{id}"),
            &[("If-Match", tag)],
            &json!({"vector": [9.0, 9.0]}),
        )
        .status
}

fn names(dir: PathBuf, prefixes: &[&str]) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| prefixes.iter().any(|p| n.starts_with(p)))
        .collect();
    names.sort();
    names
}

fn restore_files(f: &Fixture) -> Vec<String> {
    names(f.admin(), &["restore-"])
}

fn work_files(f: &Fixture) -> Vec<String> {
    names(
        f.admin().join("compaction"),
        &["recovery-", "staging-", "freeze-"],
    )
}

fn created_id(reply: &Reply) -> u64 {
    assert_eq!(reply.status, 200, "{}", reply.body);
    reply.body["outcomes"][0]["node"]["id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap()
}

#[test]
fn manual_compaction_preserves_data_cas_ids_and_lineage_and_repeats() {
    let f = Fixture::new();
    let server = f.start("");
    let status = server.status();
    assert_eq!(status["capability"], true, "{status}");
    assert_eq!(status["available"], true, "{status}");
    assert_eq!(status["settings"]["enabled"], false, "off by default");
    assert_eq!(status["settings"]["history_policy"], "unacknowledged");
    assert!(
        status["reasons"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["code"] == "history_ack_required"),
        "{status}"
    );
    let caps = server.call("GET", "/v1/capabilities", Value::Null);
    assert_eq!(caps.status, 200);
    assert_eq!(caps.body["features"]["compaction"], true, "{}", caps.body);

    churn(&server, 1, 8);
    let reply = server.call("POST", "/v1/graphs", json!({"graph": "g"}));
    assert_eq!(reply.status, 201, "{}", reply.body);
    let reply = batch(
        &server,
        json!([
            {"op": "create_node", "key": "a", "labels": ["Doc"]},
            {"op": "create_node", "key": "b"},
            {"op": "create_edge", "key": "ab", "source": "a", "relationship": "LINKS", "destination": "b"},
        ]),
    );
    assert_eq!(reply.status, 200, "{}", reply.body);
    let deleted_high = created_id(&batch(
        &server,
        json!([{"op": "create_node", "key": "probe"}]),
    ));
    let reply = batch(&server, json!([{"op": "delete_node", "key": "probe"}]));
    assert_eq!(reply.status, 200, "{}", reply.body);
    let stale_revision = node(&server, "a")["revision"].clone();
    let reply = batch(
        &server,
        json!([{"op": "patch_node", "key": "a", "add_labels": ["Seen"]}]),
    );
    assert_eq!(reply.status, 200, "{}", reply.body);
    let stale_tag = etag(&server, 1);
    churn(&server, 1, 1);

    // Validation: acknowledgements, a UUID key and the settings version.
    let version = server.status()["settings_version"].clone();
    let reply = server.call(
        "POST",
        "/v1/admin/compaction/jobs",
        json!({"expected_version": version, "acknowledge_downtime": false,
            "acknowledge_history": true, "idempotency_key": key(1)}),
    );
    assert_eq!(reply.code(), (422, json!("acknowledgement_required")));
    let reply = server.call(
        "POST",
        "/v1/admin/compaction/jobs",
        json!({"expected_version": version, "acknowledge_downtime": true,
            "acknowledge_history": true, "idempotency_key": "not-a-uuid"}),
    );
    assert_eq!(reply.status, 422);
    let reply = server.call(
        "POST",
        "/v1/admin/compaction/jobs",
        json!({"expected_version": 999, "acknowledge_downtime": true,
            "acknowledge_history": true, "idempotency_key": key(2)}),
    );
    assert_eq!(reply.code(), (409, json!("settings_version_conflict")));

    assert_eq!(server.status()["file"]["name"], "live.vlite");
    let accepted = server.submit(&key(3));
    assert_eq!(accepted.status, 202, "{}", accepted.body);
    assert_eq!(accepted.body["replayed"], false);
    let id = accepted.body["job"]["id"].as_str().unwrap().to_owned();
    let lineage = accepted.body["job"]["lineage"].clone();
    assert!(lineage.is_string(), "{}", accepted.body);
    let replay = server.submit(&key(3));
    assert_eq!(replay.status, 202, "{}", replay.body);
    assert_eq!(replay.body["replayed"], true);
    assert_eq!(replay.body["job"]["id"], id.as_str());
    let reply = server.call(
        "POST",
        "/v1/admin/compaction/jobs",
        json!({"expected_version": 2, "acknowledge_downtime": true,
            "acknowledge_history": true, "idempotency_key": key(3)}),
    );
    assert_eq!(reply.code(), (409, json!("idempotency_key_reused")));
    let job = server.finish(&id);
    assert_eq!(job["outcome"], "succeeded", "{job}");
    assert_eq!(job["trigger"], "manual");
    assert_eq!(job["lineage"], lineage, "lineage is preserved");
    assert_eq!(job["selection"], "committed");
    let before = job["before_bytes"].as_u64().unwrap();
    let after = job["after_bytes"].as_u64().unwrap();
    assert!(after < before, "{job}");
    assert_eq!(job["saved_bytes"].as_u64().unwrap(), before - after);
    let sequence = job["sequence"].as_u64().unwrap();

    let status = server.status();
    let candidate = job["candidate"].as_str().unwrap().to_owned();
    assert!(candidate.starts_with("restore-"), "{job}");
    assert_eq!(status["file"]["name"], candidate.as_str(), "{status}");
    assert_eq!(status["selection"]["candidate"], candidate.as_str());
    assert_eq!(status["selection"]["job"], id.as_str());
    assert_eq!(status["fence"], "open");
    assert_eq!(status["recovery"]["required"], false);
    assert_eq!(status["recovery"]["retained_copies"], 1);
    assert_eq!(status["last_job"]["id"], id.as_str());
    assert!(status["job"].is_null());
    assert!(f.db().exists(), "the original source file is retained");
    assert_eq!(restore_files(&f), vec![candidate.clone()]);
    assert_eq!(
        names(f.admin().join("compaction"), &["staging-", "freeze-"]),
        Vec::<String>::new()
    );

    // Records, CAS and allocator high-waters survive the new generation.
    assert_churned(&server, 1, 0.0);
    assert_eq!(
        put_if_match(&server, 1, &stale_tag),
        412,
        "a stale ETag stays stale"
    );
    let fresh = etag(&server, 2);
    assert_eq!(put_if_match(&server, 2, &fresh), 204);
    let a = node(&server, "a");
    assert!(
        a["labels"].as_array().unwrap().contains(&json!("Seen")),
        "{a}"
    );
    let reply = batch(
        &server,
        json!([{"op": "patch_node", "key": "a", "add_labels": ["X"], "expected": {"revision": stale_revision}}]),
    );
    assert!(
        reply.status == 409 || reply.status == 412,
        "stale graph CAS: {}",
        reply.body
    );
    let reply = batch(
        &server,
        json!([{"op": "patch_node", "key": "a", "add_labels": ["X"], "expected": {"revision": a["revision"]}}]),
    );
    assert_eq!(reply.status, 200, "{}", reply.body);
    let revision: u64 = reply.body["revision"].as_str().unwrap().parse().unwrap();
    assert!(
        revision > sequence,
        "next revision {revision} exceeds S {sequence}"
    );
    let fresh_id = created_id(&batch(
        &server,
        json!([{"op": "create_node", "key": "fresh"}]),
    ));
    assert!(
        fresh_id > deleted_high,
        "IDs are never reused: {fresh_id} <= {deleted_high}"
    );
    let reply = server.call(
        "POST",
        "/v1/graphs/g/edges/get",
        json!({"edge": {"key": "ab"}}),
    );
    assert_eq!(reply.status, 200, "{}", reply.body);
    assert_eq!(reply.body["edge"]["source_key"], "a");
    // Restore selection is pinned by the committed compaction authority.
    let reply = server.call(
        "POST",
        "/v1/admin/restores/select",
        json!({"name": candidate}),
    );
    assert_eq!(reply.code(), (409, json!("compaction_selection_pinned")));

    // Repeat after new writes.
    churn(&server, 20, 6);
    let second = server.compact(&key(4));
    assert_eq!(second["outcome"], "succeeded", "{second}");
    assert_eq!(second["lineage"], lineage);
    assert_eq!(second["source"], candidate.as_str());
    let second_candidate = second["candidate"].as_str().unwrap().to_owned();
    assert_ne!(second_candidate, candidate);
    let status = server.status();
    assert_eq!(status["selection"]["candidate"], second_candidate.as_str());
    assert!(
        status["selection"]["epoch"].as_u64().unwrap() >= 2,
        "{status}"
    );
    assert_eq!(status["recovery"]["retained_copies"], 2);
    drop(server);

    // The restart opens the selected file, never the CLI source.
    let server = f.start("");
    let status = server.status();
    assert_eq!(
        status["file"]["name"],
        second_candidate.as_str(),
        "{status}"
    );
    assert_eq!(status["last_job"]["outcome"], "succeeded");
    assert_churned(&server, 20, 5.0);
    assert!(node(&server, "fresh")["id"].is_string());
    assert!(
        node(&server, "a")["labels"]
            .as_array()
            .unwrap()
            .contains(&json!("X"))
    );
}

#[test]
fn crash_points_before_selection_keep_the_source_and_after_select_the_candidate() {
    for (fault, selected) in [
        ("crash_after_freeze", false),
        ("crash_after_build", false),
        ("crash_after_intent", true),
        ("crash_after_selection", true),
    ] {
        let f = Fixture::new();
        let server = f.start(fault);
        churn(&server, 1, 6);
        let reply = server.submit(&key(1));
        assert_eq!(reply.status, 202, "{fault}: {}", reply.body);
        let id = reply.body["job"]["id"].as_str().unwrap().to_owned();
        server.wait_exit();
        let server = f.start("");
        let status = server.status();
        let last = &status["last_job"];
        assert_eq!(last["id"], id.as_str(), "{fault}: {status}");
        assert!(status["job"].is_null(), "{fault}: {status}");
        if selected {
            assert_eq!(last["outcome"], "succeeded", "{fault}: {status}");
            assert_eq!(
                status["file"]["name"], last["candidate"],
                "{fault}: {status}"
            );
            assert_eq!(status["selection"]["job"], id.as_str());
            assert_eq!(status["recovery"]["retained_copies"], 1);
        } else {
            assert_eq!(
                (last["outcome"].clone(), last["code"].clone()),
                (json!("failed"), json!("interrupted")),
                "{fault}: {status}"
            );
            assert_eq!(status["file"]["name"], "live.vlite", "{fault}: {status}");
            assert!(status["selection"].is_null());
            assert!(restore_files(&f).is_empty(), "{fault}: candidate removed");
            assert!(work_files(&f).is_empty(), "{fault}: {:?}", work_files(&f));
        }
        assert_eq!(status["fence"], "open", "{fault}: {status}");
        assert_churned(&server, 1, 5.0);
        // Writable afterwards, and the write survives a restart.
        let reply = server.call("PUT", "/v1/vectors/99", json!({"vector": [1.0, 2.0]}));
        assert_eq!(reply.status, 204, "{fault}");
        drop(server);
        let server = f.start("");
        let reply = server.call("GET", "/v1/vectors/99", Value::Null);
        assert_eq!(reply.status, 200, "{fault}");
    }
}

#[test]
fn selection_effect_error_fences_until_restart_finishes_the_selection() {
    let f = Fixture::new();
    let server = f.start("selection_effect_error");
    churn(&server, 1, 6);
    let reply = server.submit(&key(1));
    assert_eq!(reply.status, 202, "{}", reply.body);
    let id = reply.body["job"]["id"].as_str().unwrap().to_owned();
    let status = server.wait_fence("recovery");
    assert_eq!(
        status["recovery"]["code"], "selection_outcome_unknown",
        "{status}"
    );
    assert_eq!(status["job"]["selection"], "outcome_unknown", "{status}");
    // Data routes are fenced; sign-in and the control plane keep working.
    let reply = server.call("GET", "/v1/vectors/1", Value::Null);
    assert_eq!(reply.code(), (503, json!("recovery_required")));
    let reply = server.call("PUT", "/v1/vectors/1", json!({"vector": [0.0, 0.0]}));
    assert_eq!(reply.code(), (503, json!("recovery_required")));
    let cookie = server.login();
    let reply = server.send(
        "GET",
        "/v1/admin/compaction",
        &[("Cookie", &cookie)],
        &Value::Null,
    );
    assert_eq!(reply.status, 200, "{}", reply.body);
    let reply = server.submit(&key(2));
    assert_eq!(
        reply.code(),
        (503, json!("recovery_required")),
        "{}",
        reply.body
    );
    let reply = server.call(
        "POST",
        &format!("/v1/admin/compaction/jobs/{id}/cancel"),
        Value::Null,
    );
    assert_eq!(reply.status, 409, "{}", reply.body);
    drop(server);

    let server = f.start("");
    let status = server.status();
    assert_eq!(status["fence"], "open", "{status}");
    assert_eq!(status["last_job"]["outcome"], "succeeded", "{status}");
    assert_eq!(status["file"]["name"], status["last_job"]["candidate"]);
    assert_churned(&server, 1, 5.0);
}

#[test]
fn intent_write_failure_never_reopens_the_source_and_restart_keeps_it() {
    let f = Fixture::new();
    let server = f.start("state_write_error");
    churn(&server, 1, 6);
    let reply = server.submit(&key(1));
    assert_eq!(reply.status, 202, "{}", reply.body);
    let status = server.wait_fence("recovery");
    assert_eq!(
        status["recovery"]["code"], "selection_intent_uncertain",
        "{status}"
    );
    assert_eq!(server.call("GET", "/v1/vectors/1", Value::Null).status, 503);
    drop(server);
    let server = f.start("");
    let status = server.status();
    assert_eq!(status["fence"], "open", "{status}");
    assert_eq!(status["file"]["name"], "live.vlite", "{status}");
    assert_eq!(status["last_job"]["code"], "interrupted", "{status}");
    assert!(status["selection"].is_null());
    assert!(restore_files(&f).is_empty());
    assert_churned(&server, 1, 5.0);
}

#[test]
fn cancel_build_failure_reopen_failure_and_no_benefit_are_reported() {
    // Cancel during a slow build: the source stays authoritative.
    let f = Fixture::new();
    let server = f.start("slow_build");
    churn(&server, 1, 6);
    let reply = server.submit(&key(1));
    assert_eq!(reply.status, 202, "{}", reply.body);
    let id = reply.body["job"]["id"].as_str().unwrap().to_owned();
    server.wait_phase(&id, "building");
    let fenced = server.call("GET", "/v1/vectors/1", Value::Null);
    assert_eq!(fenced.code(), (503, json!("maintenance_in_progress")));
    assert!(fenced.header("retry-after").is_some(), "{}", fenced.head);
    let status = server.status();
    assert_eq!(status["phase"], "building");
    assert_eq!(status["fence"], "maintenance");
    let reply = server.call(
        "POST",
        &format!("/v1/admin/compaction/jobs/{id}/cancel"),
        Value::Null,
    );
    assert_eq!(reply.status, 202, "{}", reply.body);
    assert_eq!(reply.body["job"]["cancel_requested"], true);
    let job = server.finish(&id);
    assert_eq!(
        (job["outcome"].clone(), job["code"].clone()),
        (json!("cancelled"), json!("cancelled")),
        "{job}"
    );
    let reply = server.call(
        "POST",
        &format!("/v1/admin/compaction/jobs/{id}/cancel"),
        Value::Null,
    );
    assert_eq!(reply.code(), (409, json!("job_finished")));
    assert_eq!(server.status()["file"]["name"], "live.vlite");
    assert!(restore_files(&f).is_empty());
    assert!(work_files(&f).is_empty(), "{:?}", work_files(&f));
    let reply = server.call("PUT", "/v1/vectors/1", json!({"vector": [3.0, 3.0]}));
    assert_eq!(reply.status, 204);
    let reply = server.call(
        "GET",
        "/v1/admin/compaction/jobs/cj-0-000000000000",
        Value::Null,
    );
    assert_eq!(reply.code(), (404, json!("not_found")));
    drop(server);

    // Build failure.
    let server = f.start("build_fail");
    let job = server.compact(&key(2));
    assert_eq!(
        (job["outcome"].clone(), job["code"].clone()),
        (json!("failed"), json!("build_failed")),
        "{job}"
    );
    let status = server.status();
    assert_eq!(status["file"]["name"], "live.vlite");
    assert_eq!(status["fence"], "open");
    assert!(restore_files(&f).is_empty());
    let reply = server.call("GET", "/v1/vectors/1", Value::Null);
    assert_eq!(
        (reply.status, reply.body["vector"].clone()),
        (200, json!([3.0, 3.0]))
    );
    // A manual failure neither suspends nor backs off the schedule.
    assert_eq!(status["schedule"]["failures"], 0);
    assert_eq!(status["schedule"]["suspended"], false);
    drop(server);

    // Reopen failure after commit: recovery fence, restart opens the candidate.
    let server = f.start("reopen_fail");
    let job = server.compact(&key(3));
    assert_eq!(
        (job["outcome"].clone(), job["code"].clone()),
        (json!("failed"), json!("reopen_failed")),
        "{job}"
    );
    assert_eq!(job["selection"], "committed");
    assert_eq!(server.status()["fence"], "recovery");
    assert_eq!(server.call("GET", "/v1/vectors/1", Value::Null).status, 503);
    drop(server);
    let server = f.start("");
    let status = server.status();
    assert_eq!(status["fence"], "open", "{status}");
    assert_eq!(status["file"]["name"], job["candidate"], "{status}");
    let reply = server.call("GET", "/v1/vectors/1", Value::Null);
    assert_eq!(
        (reply.status, reply.body["vector"].clone()),
        (200, json!([3.0, 3.0]))
    );

    // No benefit: an already compact file is skipped and nothing changes.
    let job = server.compact(&key(4));
    assert_eq!(job["outcome"], "skipped", "{job}");
    assert_eq!(job["code"], "no_material_savings", "{job}");
    let after = server.status();
    assert_eq!(after["file"]["name"], status["file"]["name"]);
    assert_eq!(after["fence"], "open");
    assert_eq!(restore_files(&f).len(), 1, "{:?}", restore_files(&f));
}

#[test]
fn access_control_settings_and_unmanaged_runtime() {
    let f = Fixture::new();
    let server = f.start("");
    // Roles: no credential 401, read-only token 403 on every compaction route.
    assert_eq!(
        server
            .send("GET", "/v1/admin/compaction", &[], &Value::Null)
            .status,
        401
    );
    let read = format!("Bearer {READ}");
    for (method, path) in [
        ("GET", "/v1/admin/compaction"),
        ("PUT", "/v1/admin/compaction/config"),
        ("POST", "/v1/admin/compaction/jobs"),
        ("GET", "/v1/admin/compaction/jobs/x"),
        ("POST", "/v1/admin/compaction/jobs/x/cancel"),
    ] {
        let reply = server.send(method, path, &[("Authorization", &read)], &json!({}));
        assert_eq!(reply.status, 403, "{method} {path}: {}", reply.body);
    }
    // A signed-in session needs the CSRF request header for mutations.
    let cookie = server.login();
    let reply = server.send(
        "GET",
        "/v1/admin/compaction",
        &[("Cookie", &cookie)],
        &Value::Null,
    );
    assert_eq!(reply.status, 200, "{}", reply.body);
    let defaults = reply.body["settings"].clone();
    let version = reply.body["settings_version"].as_u64().unwrap();
    let put = json!({"expected_version": version, "settings": defaults});
    let reply = server.send(
        "PUT",
        "/v1/admin/compaction/config",
        &[("Cookie", &cookie)],
        &put,
    );
    assert_eq!(reply.code(), (403, json!("csrf_rejected")));
    let reply = server.send(
        "PUT",
        "/v1/admin/compaction/config",
        &[
            ("Cookie", &cookie),
            ("X-Vlite-Request", "1"),
            ("Sec-Fetch-Site", "cross-site"),
        ],
        &put,
    );
    assert_eq!(reply.status, 403);
    let reply = server.send(
        "PUT",
        "/v1/admin/compaction/config",
        &[("Cookie", &cookie), ("X-Vlite-Request", "1")],
        &put,
    );
    assert_eq!(reply.status, 200, "{}", reply.body);
    assert_eq!(reply.body["settings_version"], version + 1);

    // Version conflicts, bounds and the enable rules.
    let reply = server.call("PUT", "/v1/admin/compaction/config", put.clone());
    assert_eq!(reply.code(), (409, json!("settings_version_conflict")));
    let version = version + 1;
    for (field, value) in [
        ("daily_start_utc", json!("24:00")),
        ("daily_start_utc", json!("2:00")),
        ("window_minutes", json!(4)),
        ("window_minutes", json!(121)),
        ("max_run_seconds", json!(1800)),
        ("max_source_bytes", json!(2u64 << 30)),
        ("min_growth_percent", json!(5)),
        ("cooldown_hours", json!(12)),
        ("history_policy", json!("forget")),
    ] {
        let mut bad = defaults.clone();
        bad[field] = value.clone();
        let reply = server.call(
            "PUT",
            "/v1/admin/compaction/config",
            json!({"expected_version": version, "settings": bad}),
        );
        assert_eq!(
            reply.code(),
            (422, json!("invalid_settings")),
            "{field}={value}"
        );
    }
    let mut unknown = defaults.clone();
    unknown["extra"] = json!(1);
    let reply = server.call(
        "PUT",
        "/v1/admin/compaction/config",
        json!({"expected_version": version, "settings": unknown}),
    );
    assert_eq!(reply.status, 422);
    let mut settings = defaults.clone();
    settings["enabled"] = json!(true);
    let reply = server.call(
        "PUT",
        "/v1/admin/compaction/config",
        json!({"expected_version": version, "settings": settings}),
    );
    assert_eq!(reply.code(), (409, json!("history_ack_required")));
    settings["history_policy"] = json!("resnapshot_allowed");
    let reply = server.call(
        "PUT",
        "/v1/admin/compaction/config",
        json!({"expected_version": version, "settings": settings}),
    );
    assert_eq!(
        reply.code(),
        (409, json!("history_ack_required")),
        "consent is explicit"
    );
    settings["daily_start_utc"] = json!("03:15");
    let reply = server.call(
        "PUT",
        "/v1/admin/compaction/config",
        json!({"expected_version": version, "settings": settings, "acknowledge_history": true}),
    );
    assert_eq!(reply.status, 200, "{}", reply.body);
    assert_eq!(reply.body["settings"]["enabled"], true);
    assert_eq!(reply.body["history_acknowledged"], true);
    let next = reply.body["schedule"]["next_window_unix_ms"]
        .as_u64()
        .unwrap();
    assert_eq!(next % 86_400_000, (3 * 60 + 15) * 60_000, "UTC start");
    drop(server);

    // Settings and consent persist across a restart.
    let server = f.start("");
    let status = server.status();
    assert_eq!(status["settings"]["enabled"], true, "{status}");
    assert_eq!(status["settings"]["daily_start_utc"], "03:15");
    assert_eq!(status["history_acknowledged"], true);
    assert_eq!(status["settings_version"], version + 1);
    drop(server);

    // Without an admin directory there is no managed runtime.
    let other = Fixture::new();
    let server = Server::start(&other, "", false);
    let reply = server.call("GET", "/v1/admin/compaction", Value::Null);
    assert_eq!(reply.status, 200, "{}", reply.body);
    assert_eq!(reply.body["capability"], false);
    assert_eq!(reply.body["reasons"][0]["code"], "managed_runtime_required");
    let reply = server.call(
        "POST",
        "/v1/admin/compaction/jobs",
        json!({"expected_version": 1, "acknowledge_downtime": true,
            "acknowledge_history": true, "idempotency_key": key(1)}),
    );
    assert_eq!(reply.code(), (409, json!("managed_runtime_required")));
    let caps = server.call("GET", "/v1/capabilities", Value::Null);
    assert_eq!(caps.body["features"]["compaction"], false, "{}", caps.body);
}

#[test]
fn schedule_runs_once_per_window_after_measured_growth() {
    let f = Fixture::new();
    let server = f.start("");
    churn(&server, 1, 1);
    let minute = (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        / 60)
        % 1440;
    let start = (minute + 1440 - 1) % 1440;
    let status = server.status();
    let mut settings = status["settings"].clone();
    settings["enabled"] = json!(true);
    settings["history_policy"] = json!("resnapshot_allowed");
    settings["daily_start_utc"] = json!(format!("{:02}:{:02}", start / 60, start % 60));
    settings["window_minutes"] = json!(120);
    settings["max_run_seconds"] = json!(60);
    settings["min_file_bytes"] = json!(0);
    settings["min_growth_bytes"] = json!(1);
    settings["min_growth_percent"] = json!(10);
    settings["cooldown_hours"] = json!(24);
    let reply = server.call(
        "PUT",
        "/v1/admin/compaction/config",
        json!({"expected_version": status["settings_version"], "settings": settings,
            "acknowledge_history": true}),
    );
    assert_eq!(reply.status, 200, "{}", reply.body);
    assert_eq!(reply.body["schedule"]["in_window"], true, "{}", reply.body);
    // The first evaluation only samples the baseline; no growth, no run.
    let deadline = Instant::now() + Duration::from_secs(30);
    while server.status()["schedule"]["baseline_bytes"].is_null() {
        assert!(Instant::now() < deadline, "baseline not sampled");
        std::thread::sleep(Duration::from_millis(200));
    }
    std::thread::sleep(Duration::from_millis(1500));
    assert!(
        server.status()["last_job"].is_null(),
        "no run without growth"
    );
    churn(&server, 1, 10);
    let deadline = Instant::now() + Duration::from_secs(60);
    let job = loop {
        let status = server.status();
        if status["last_job"]["outcome"].is_string() {
            break status["last_job"].clone();
        }
        assert!(Instant::now() < deadline, "{status}");
        std::thread::sleep(Duration::from_millis(200));
    };
    assert_eq!(job["trigger"], "scheduled", "{job}");
    assert_eq!(job["outcome"], "succeeded", "{job}");
    assert!(job["occurrence"].is_string(), "{job}");
    assert_churned(&server, 1, 9.0);
    let status = server.status();
    assert_eq!(status["schedule"]["last_occurrence"], job["occurrence"]);
    let eligible = status["schedule"]["next_eligible_unix_ms"]
        .as_u64()
        .unwrap();
    assert!(
        eligible >= job["finished_unix_ms"].as_u64().unwrap() + 23 * 3_600_000,
        "{status}"
    );
    // More growth in the same occurrence (and inside the cooldown) never runs again.
    churn(&server, 1, 10);
    std::thread::sleep(Duration::from_secs(3));
    assert_eq!(server.status()["last_job"]["id"], job["id"]);
    drop(server);
    let server = f.start("");
    let status = server.status();
    assert_eq!(status["schedule"]["last_occurrence"], job["occurrence"]);
    assert_eq!(status["schedule"]["next_eligible_unix_ms"], eligible);
    assert_eq!(status["last_job"]["id"], job["id"]);
}


#[test]
fn concurrent_writers_sessions_pins_refusals_and_audit() {
    let f = Fixture::new();
    let server = f.start("slow_build");
    churn(&server, 0, 6);
    let feed = server.call("GET", "/v1/changes?limit=1", Value::Null);
    assert_eq!(feed.status, 200, "{}", feed.body);
    let early = feed.body["next_cursor"].as_str().unwrap().to_owned();
    // A kept session and a session revoked before compaction.
    let kept = server.login();
    let revoked = server.login();
    let reply = server.send(
        "POST",
        "/v1/auth/logout",
        &[("Cookie", &revoked), ("X-Vlite-Request", "1")],
        &Value::Null,
    );
    assert!(reply.status < 300, "{}", reply.body);

    // Writers race the whole job: every acknowledged write must survive.
    let acked = std::sync::Mutex::new(Vec::new());
    let done = std::sync::atomic::AtomicBool::new(false);
    let (job, refused) = std::thread::scope(|scope| {
        let writers: Vec<_> = (0..3u64)
            .map(|w| {
                let (server, acked, done) = (&server, &acked, &done);
                scope.spawn(move || {
                    let mut refused = 0u32;
                    let mut n = 0u64;
                    while !done.load(std::sync::atomic::Ordering::SeqCst) {
                        let id = 1_000 + w * 100_000 + n;
                        n += 1;
                        let reply = server.call(
                            "PUT",
                            &format!("/v1/vectors/{id}"),
                            json!({"vector": [id as f32, 7.0]}),
                        );
                        match reply.status {
                            204 => acked.lock().unwrap().push(id),
                            503 => {
                                assert_eq!(reply.code().1, json!("maintenance_in_progress"));
                                refused += 1;
                                std::thread::sleep(Duration::from_millis(20));
                            }
                            other => panic!("unexpected {other}: {}", reply.body),
                        }
                    }
                    refused
                })
            })
            .collect();
        std::thread::sleep(Duration::from_millis(300));
        let job = server.compact(&key(77));
        std::thread::sleep(Duration::from_millis(300));
        done.store(true, std::sync::atomic::Ordering::SeqCst);
        let refused: u32 = writers.into_iter().map(|w| w.join().unwrap()).sum();
        (job, refused)
    });
    assert_eq!(job["outcome"], "succeeded", "{job}");
    assert!(refused > 0, "writes during maintenance are refused, not queued");
    let acked = acked.into_inner().unwrap();
    assert!(acked.len() > 3, "writers made progress: {}", acked.len());
    for id in &acked {
        let reply = server.call("GET", &format!("/v1/vectors/{id}"), Value::Null);
        assert_eq!(reply.status, 200, "acknowledged write {id} lost");
        assert_eq!(reply.body["vector"], json!([*id as f64, 7.0]));
    }
    assert_churned(&server, 0, 5.0);
    // History before the compacted image answers a precise error, never a gap.
    let reply = server.call("GET", &format!("/v1/changes?cursor={early}"), Value::Null);
    assert_eq!(reply.status, 409, "{}", reply.body);
    let code = reply.body["error"]["code"].as_str().unwrap();
    assert!(
        matches!(code, "cursor_before_history" | "cursor_compacted"),
        "{}",
        reply.body
    );

    // Sessions live across the generation; a revoked one stays revoked.
    let reply = server.send(
        "GET",
        "/v1/admin/compaction",
        &[("Cookie", &kept)],
        &Value::Null,
    );
    assert_eq!(reply.status, 200, "{}", reply.body);
    let reply = server.send(
        "GET",
        "/v1/admin/compaction",
        &[("Cookie", &revoked)],
        &Value::Null,
    );
    assert_eq!(reply.status, 401, "{}", reply.body);

    // The committed selection pins restore selection.
    let restore = restore_files(&f)
        .into_iter()
        .find(|n| Some(n.as_str()) != job["candidate"].as_str());
    let name = restore.unwrap_or_else(|| "restore-1-1-0.vlite".into());
    let reply = server.call("POST", "/v1/admin/restores/select", json!({"name": name}));
    assert_eq!(reply.code(), (409, json!("compaction_selection_pinned")));

    // The audit log records the job without any secret.
    let audit = std::fs::read_to_string(f.admin().join("audit.jsonl")).unwrap();
    for event in ["compaction_requested", "compaction_accepted", "compaction_finished"] {
        assert!(audit.contains(event), "{event} missing");
    }
    for secret in [TOKEN, READ, PASSWORD, kept.as_str()] {
        assert!(!audit.contains(secret), "audit leaks a secret");
    }

    // A webhook subscription is state compaction cannot carry: refused.
    let reply = server.call(
        "POST",
        "/v1/admin/webhooks",
        json!({"url": "http://127.0.0.1:9/hook", "events": ["put"],
            "secret": "correct-horse-battery-staple"}),
    );
    assert_eq!(reply.status, 201, "{}", reply.body);
    let status = server.status();
    assert!(
        status["reasons"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["code"] == "webhook_subscriptions_present"),
        "{status}"
    );
    assert_eq!(status["available"], false);
    let reply = server.submit(&key(78));
    assert_ne!(reply.status, 202, "{}", reply.body);
    assert_eq!(reply.body["error"]["code"], "webhook_subscriptions_present");
}


#[test]
fn missing_selected_candidate_serves_authenticated_recovery_and_bad_auth_fails_closed() {
    let f = Fixture::new();
    let server = f.start("");
    churn(&server, 1, 6);
    let job = server.compact(&key(1));
    assert_eq!(job["outcome"], "succeeded", "{job}");
    drop(server);
    let candidate = f.admin().join(job["candidate"].as_str().unwrap());
    let aside = f.dir.path().join("aside.vlite");
    std::fs::rename(&candidate, &aside).unwrap();

    // The committed selection never falls back to the older source.
    let server = f.start("");
    let status = server.status();
    assert_eq!(status["fence"], "recovery", "{status}");
    assert!(
        status["recovery"]["code"] == "selected_candidate_invalid"
            || status["recovery"]["code"] == "selected_candidate_open_failed",
        "{status}"
    );
    assert_eq!(status["recovery"]["retained_copies"], 1, "{status}");
    assert_eq!(
        server.call("GET", "/v1/vectors/1", Value::Null).code(),
        (503, json!("recovery_required"))
    );
    let cookie = server.login();
    let reply = server.send(
        "GET",
        "/v1/admin/compaction",
        &[("Cookie", &cookie)],
        &Value::Null,
    );
    assert_eq!(reply.status, 200, "{}", reply.body);
    assert_eq!(
        server
            .send("GET", "/v1/admin/compaction", &[], &Value::Null)
            .status,
        401,
        "recovery mode still requires authentication"
    );
    drop(server);

    // Restoring the selected file resumes normal service.
    std::fs::rename(&aside, &candidate).unwrap();
    let server = f.start("");
    assert_eq!(server.status()["fence"], "open");
    assert_churned(&server, 1, 5.0);
    drop(server);

    // A malformed account store fails closed: the server does not start.
    std::fs::write(f.admin().join("users.json"), b"{not json").unwrap();
    let started = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f.start("")));
    assert!(started.is_err(), "server must not start with a malformed auth store");
}
