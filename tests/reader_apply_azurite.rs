//! TEST-006: local Azurite evidence for reader enrollment, archive apply,
//! position/age reporting and fail-closed freshness admission. This does not
//! qualify ACA, managed identity or a production staleness guarantee.

#![cfg(feature = "server")]

use base64::Engine as _;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::TcpStream,
    path::Path,
    process::{Child, Command, Stdio},
    sync::mpsc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const ACCOUNT: &str = "devstoreaccount1";
/// The emulator's published development key (not a secret); override with
/// VLITE_AZURITE_ACCOUNT_KEY for an emulator started with another key.
const WELL_KNOWN_KEY: &str =
    "Eby8vdM02xNOcqFlqUwJPLlmEtlCDXJ1OUzFT50uSRZ6IFsuFq2UVErCz4I6tq/K1SZFPTOtr/KBHBeksoGMGw==";
const TOKEN: &str = "azurite_e2e_0123456789_ABCDEFGHIJKLMNOPQRSTUVWX";
const ARCHIVE_INTERVAL_SECS: u64 = 60;
const STALENESS_BOUND_MS: u64 = 10_000;

struct Server {
    child: Child,
    address: String,
    token: &'static str,
}

impl Server {
    fn start(database: &Path, admin: &Path, token: &'static str, env: &[(&str, &Path)]) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_vlite-server"));
        command
            .arg(database)
            .args(["--create-if-missing", "2", "--bind", "127.0.0.1:0"])
            .args(["--serve-mode", "standalone"])
            .env("VLITE_AUTH_TOKEN", token)
            .env("VLITE_ADMIN_DIR", admin)
            .env_remove("VLITE_AUTH_TOKEN_FILE")
            .env_remove("VLITE_ADMIN_USERNAME")
            .env_remove("VLITE_ADMIN_PASSWORD_FILE")
            .env_remove("VLITE_CLOUD_BACKUP_SHARED_KEY_FILE")
            .env_remove("VLITE_PROVIDERS_FILE");
        for (name, value) in env {
            command.env(name, value);
        }
        let mut child = command.stdout(Stdio::piped()).spawn().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let mut line = String::new();
            BufReader::new(stdout).read_line(&mut line).unwrap();
            let _ = sender.send(line);
        });
        let line = receiver
            .recv_timeout(Duration::from_secs(120))
            .expect("server startup timed out");
        let address = line
            .trim()
            .strip_prefix("LISTENING http://")
            .expect("startup address")
            .to_owned();
        Self {
            child,
            address,
            token,
        }
    }

    fn admin(&self, method: &str, path: &str, body: Value) -> Reply {
        call(
            &self.address,
            method,
            path,
            &if body.is_null() {
                String::new()
            } else {
                body.to_string()
            },
            &[("Authorization", &bearer(self.token))],
        )
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        if self.child.try_wait().unwrap().is_none() {
            self.child.kill().unwrap();
        }
        self.child.wait().unwrap();
    }
}

/// A child `vlite-server`: stdout announces the address, stderr is collected.
struct Proc {
    child: Child,
    address: String,
    events: mpsc::Receiver<String>,
    seen: Vec<String>,
}

impl Proc {
    fn spawn(mut command: Command) -> Self {
        command
            .env_remove("VLITE_AUTH_TOKEN_FILE")
            .env_remove("VLITE_ADMIN_USERNAME")
            .env_remove("VLITE_ADMIN_PASSWORD_FILE")
            .env_remove("VLITE_CLOUD_BACKUP_SHARED_KEY_FILE")
            .env_remove("VLITE_PROVIDERS_FILE")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            // Its own process group, so Ctrl-Break reaches only this child.
            const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
            command.creation_flags(CREATE_NEW_PROCESS_GROUP);
        }
        let mut child = command.spawn().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (lines, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { return };
                let _ = lines.send(line);
            }
        });
        let stderr = child.stderr.take().unwrap();
        let (events_tx, events) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines() {
                let Ok(line) = line else { return };
                let _ = events_tx.send(line);
            }
        });
        let mut process = Self {
            child,
            address: String::new(),
            events,
            seen: Vec::new(),
        };
        match receiver.recv_timeout(Duration::from_secs(120)) {
            Ok(line) => {
                process.address = line
                    .strip_prefix("LISTENING http://")
                    .unwrap_or_else(|| panic!("unexpected stdout {line}"))
                    .to_owned();
            }
            Err(_) => {
                let _ = process.child.kill();
                let _ = process.child.wait();
                process.drain_events();
                panic!("startup failed: {:?}", process.seen);
            }
        }
        process
    }

    fn drain_events(&mut self) {
        while let Ok(line) = self.events.try_recv() {
            self.seen.push(line);
        }
    }

    fn call(&self, method: &str, path: &str, body: &str, headers: &[(&str, &str)]) -> Reply {
        call(&self.address, method, path, body, headers)
    }
}

impl Drop for Proc {
    fn drop(&mut self) {
        if self.child.try_wait().unwrap().is_none() {
            self.child.kill().unwrap();
        }
        self.child.wait().unwrap();
    }
}

#[derive(Debug)]
struct Reply {
    status: u16,
    head: String,
    body: Value,
}

impl Reply {
    fn code(&self) -> Option<&str> {
        self.body["error"]["code"].as_str()
    }

    fn header_number(&self, name: &str, radix: u32) -> u64 {
        let value = self
            .head
            .lines()
            .filter_map(|line| line.split_once(':'))
            .find_map(|(key, value)| key.eq_ignore_ascii_case(name).then_some(value.trim()))
            .unwrap_or_else(|| panic!("missing {name}: {self:?}"));
        u64::from_str_radix(value, radix).unwrap_or_else(|_| panic!("non-numeric {name}: {self:?}"))
    }
}

fn dechunk(mut body: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let line_end = body.windows(2).position(|w| w == b"\r\n").unwrap();
        let size = std::str::from_utf8(&body[..line_end]).unwrap();
        let size = usize::from_str_radix(size.split(';').next().unwrap().trim(), 16).unwrap();
        body = &body[line_end + 2..];
        if size == 0 {
            return out;
        }
        out.extend_from_slice(&body[..size]);
        body = &body[size + 2..];
    }
}

fn call(address: &str, method: &str, path: &str, body: &str, headers: &[(&str, &str)]) -> Reply {
    let mut stream = TcpStream::connect(address).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(60)))
        .unwrap();
    let mut request = format!(
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n",
        body.len()
    );
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str("\r\n");
    request.push_str(body);
    stream.write_all(request.as_bytes()).unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).unwrap();
    let split = response
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .unwrap_or_else(|| panic!("no response head: {response:?}"));
    let head = String::from_utf8(response[..split].to_vec()).unwrap();
    let mut payload = response[split + 4..].to_vec();
    if head
        .to_ascii_lowercase()
        .contains("transfer-encoding: chunked")
    {
        payload = dechunk(&payload);
    }
    Reply {
        status: head.split_whitespace().nth(1).unwrap().parse().unwrap(),
        body: serde_json::from_slice(&payload).unwrap_or(Value::Null),
        head,
    }
}

fn bearer(token: &str) -> String {
    format!("Bearer {token}")
}

/// Repeats `probe` until it returns true or the bounded deadline passes.
fn eventually(what: &str, within: Duration, mut probe: impl FnMut() -> bool) {
    let deadline = Instant::now() + within;
    while !probe() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    let mut block = [0u8; 64];
    if key.len() > 64 {
        block[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let pad = |byte: u8| block.iter().map(|b| b ^ byte).collect::<Vec<_>>();
    let inner = Sha256::new()
        .chain_update(pad(0x36))
        .chain_update(message)
        .finalize();
    Sha256::new()
        .chain_update(pad(0x5c))
        .chain_update(inner)
        .finalize()
        .into()
}

fn http_date() -> String {
    const DAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let days = (secs / 86_400) as i64;
    // Civil-from-days (Howard Hinnant).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    let t = secs % 86_400;
    format!(
        "{}, {day:02} {} {year:04} {:02}:{:02}:{:02} GMT",
        DAYS[(days % 7) as usize],
        MONTHS[(month - 1) as usize],
        t / 3600,
        t / 60 % 60,
        t % 60
    )
}

/// Creates or deletes a container with a SharedKey-signed request: the
/// server itself never creates containers (least privilege).
fn container(endpoint: &str, key: &[u8], verb: &str, name: &str) -> u16 {
    let date = http_date();
    let version = "2021-08-06";
    let path = format!("/{ACCOUNT}/{name}");
    let to_sign = format!(
        "{verb}\n\n\n\n\n\n\n\n\n\n\n\nx-ms-date:{date}\nx-ms-version:{version}\n/{ACCOUNT}{path}\nrestype:container"
    );
    let signature =
        base64::engine::general_purpose::STANDARD.encode(hmac_sha256(key, to_sign.as_bytes()));
    let authority = endpoint
        .strip_prefix("http://")
        .and_then(|rest| rest.split('/').next())
        .expect("an http://host:port/devstoreaccount1 endpoint");
    let mut stream = TcpStream::connect(authority).unwrap();
    write!(
        stream,
        "{verb} {path}?restype=container HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\nx-ms-date: {date}\r\nx-ms-version: {version}\r\nAuthorization: SharedKey {ACCOUNT}:{signature}\r\nContent-Length: 0\r\n\r\n"
    )
    .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    response.split_whitespace().nth(1).unwrap().parse().unwrap()
}

struct Containers {
    endpoint: String,
    key: Vec<u8>,
    names: [String; 2],
}

impl Drop for Containers {
    fn drop(&mut self) {
        for name in &self.names {
            // Cleanup must attempt both containers even while unwinding.
            match std::panic::catch_unwind(|| container(&self.endpoint, &self.key, "DELETE", name))
            {
                Ok(202 | 404) => {}
                Ok(status) => eprintln!("Azurite cleanup for {name} returned {status}"),
                Err(_) => eprintln!("Azurite cleanup for {name} failed"),
            }
        }
    }
}

#[test]
#[ignore = "needs a local Azurite Blob endpoint in VLITE_AZURITE_BLOB_ENDPOINT"]
fn reader_applies_archive_reports_position_and_fails_closed_when_stale() {
    let Ok(endpoint) = std::env::var("VLITE_AZURITE_BLOB_ENDPOINT") else {
        eprintln!("skipping: VLITE_AZURITE_BLOB_ENDPOINT is unset");
        return;
    };
    let endpoint = endpoint.trim_end_matches('/').to_owned();
    assert!(
        endpoint.ends_with(&format!("/{ACCOUNT}")),
        "the emulator endpoint must be path-style for {ACCOUNT}"
    );
    let key_text =
        std::env::var("VLITE_AZURITE_ACCOUNT_KEY").unwrap_or_else(|_| WELL_KNOWN_KEY.into());
    let key = base64::engine::general_purpose::STANDARD
        .decode(key_text.trim())
        .unwrap();
    let unique = format!(
        "{:x}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
            % 0xffff_ffff_ffff
    );
    let containers = Containers {
        endpoint: endpoint.clone(),
        key,
        names: [
            format!("vlite-data-{unique}"),
            format!("vlite-coord-{unique}"),
        ],
    };
    for name in &containers.names {
        assert_eq!(container(&endpoint, &containers.key, "PUT", name), 201);
    }
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let key_file = root.join("emulator.key");
    std::fs::write(&key_file, format!("{}\n", key_text.trim())).unwrap();
    let target = json!({
        "account":ACCOUNT, "container":containers.names[0],
        "coordination_container":containers.names[1], "prefix":"e2e/reader/",
        "endpoint":endpoint, "auth":"emulator_shared_key"
    });
    let target_file = root.join("target.json");
    std::fs::write(&target_file, target.to_string()).unwrap();
    let admin = root.join("writer-admin");
    std::fs::create_dir(&admin).unwrap();
    let writer = Server::start(
        &root.join("writer.vlite"),
        &admin,
        TOKEN,
        &[("VLITE_CLOUD_BACKUP_SHARED_KEY_FILE", &key_file)],
    );
    // archive_now is cfg(test)-only, not available in the server binary.
    // Keep scheduled snapshots far away so visibility proves archive apply.
    let configured = writer.admin(
        "PUT",
        "/v1/admin/cloud-backup",
        json!({
            "target":target, "enabled":true, "interval_secs":604800,
            "drill_every":0,
            "archive":{"enabled":true,"interval_secs":ARCHIVE_INTERVAL_SECS}
        }),
    );
    assert_eq!(configured.status, 200, "{configured:?}");
    for id in 1..=2 {
        let inserted = writer.admin(
            "PUT",
            &format!("/v1/vectors/{id}"),
            json!({"vector":[id,1]}),
        );
        assert_eq!(inserted.status, 204, "{inserted:?}");
    }
    let run = writer.admin("POST", "/v1/admin/cloud-backup/run", Value::Null);
    assert_eq!(run.status, 201, "{run:?}");
    let set_id = run.body["set_id"].as_str().expect("published set id");
    eventually("Published backup set", Duration::from_secs(30), || {
        let sets = writer.admin("GET", "/v1/admin/cloud-backup/sets", Value::Null);
        assert_eq!(sets.status, 200, "{sets:?}");
        sets.body["sets"]
            .as_array()
            .unwrap()
            .iter()
            .any(|set| set["set_id"] == set_id && set["record"]["state"] == "published")
    });

    let follower_dir = root.join("follower");
    let config_file = root.join("reader.json");
    std::fs::write(
        &config_file,
        json!({
            "target_file":target_file, "shared_key_file":key_file,
            "follower_dir":follower_dir, "capacity_bytes":1073741824u64,
            "staleness_bound_ms":STALENESS_BOUND_MS, "poll_interval_ms":250
        })
        .to_string(),
    )
    .unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_vlite-server"));
    command
        .arg(follower_dir.join("reader-a").join("database.vlite"))
        .args(["--serve-mode", "reader", "--reader-apply-file"])
        .arg(&config_file)
        .args(["--bind", "127.0.0.1:0"])
        .env("VLITE_AUTH_TOKEN", TOKEN)
        .env_remove("VLITE_ADMIN_DIR")
        .env_remove("VLITE_ENCRYPTION_KEY")
        .env_remove("VLITE_ENCRYPTION_KEY_HEX");
    let reader = Proc::spawn(command);
    let authorization = bearer(TOKEN);
    let headers = [("Authorization", authorization.as_str())];
    eventually("reader readiness", Duration::from_secs(30), || {
        reader.call("GET", "/ready", "", &headers).status == 200
    });
    let mut initial_sequence = None;
    for id in 1..=2 {
        let reply = reader.call("GET", &format!("/v1/vectors/{id}"), "", &headers);
        assert_eq!(reply.status, 200, "{reply:?}");
        assert_eq!(reply.body["vector"], json!([id as f64, 1.0]));
        reply.header_number("x-vlite-lineage", 16);
        reply.header_number("x-vlite-binding-generation", 10);
        assert!(reply.header_number("x-vlite-applied-age-ms", 10) <= STALENESS_BOUND_MS);
        initial_sequence = Some(reply.header_number("x-vlite-applied-sequence", 10));
    }
    let initial_sequence = initial_sequence.unwrap();
    let committed = Instant::now();
    for id in 3..=4 {
        let inserted = writer.admin(
            "PUT",
            &format!("/v1/vectors/{id}"),
            json!({"vector":[id,2]}),
        );
        assert_eq!(inserted.status, 204, "{inserted:?}");
    }
    let apply_timeout =
        Duration::from_secs(ARCHIVE_INTERVAL_SECS + 30) + Duration::from_millis(STALENESS_BOUND_MS);
    eventually("new records and advanced sequence", apply_timeout, || {
        let mut visible = true;
        for id in 3..=4 {
            let reply = reader.call("GET", &format!("/v1/vectors/{id}"), "", &headers);
            assert!(
                matches!(reply.status, 200 | 404),
                "reader must stay healthy while following: {reply:?}"
            );
            if reply.status == 404 {
                visible = false;
                continue;
            }
            assert_eq!(reply.body["vector"], json!([id as f64, 2.0]));
            assert!(reply.header_number("x-vlite-applied-sequence", 10) > initial_sequence);
        }
        visible
    });
    eprintln!(
        "writer insert batch to reader visibility: {} ms",
        committed.elapsed().as_millis()
    );
    let refused = reader.call("PUT", "/v1/vectors/5", r#"{"vector":[5,2]}"#, &headers);
    assert_eq!(refused.status, 403, "{refused:?}");
    assert_eq!(refused.code(), Some("reader_mode"), "{refused:?}");

    drop(writer);
    // Stopping the writer alone leaves a readable head that the reader can
    // keep confirming. Remove data too: a missing set may report
    // reader_apply_failed before reader_stale; exactly those codes qualify.
    assert_eq!(
        container(&endpoint, &containers.key, "DELETE", &containers.names[0]),
        202
    );
    std::thread::sleep(Duration::from_millis(STALENESS_BOUND_MS + 1));
    for path in ["/ready", "/v1/vectors/3"] {
        let reply = reader.call("GET", path, "", &headers);
        assert_eq!(reply.status, 503, "{path}: {reply:?}");
        assert!(
            matches!(reply.code(), Some("reader_stale" | "reader_apply_failed")),
            "{path}: {reply:?}"
        );
    }
}
