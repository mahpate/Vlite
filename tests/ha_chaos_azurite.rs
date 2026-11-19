//! ITEM-035: repeatable process-level Azurite drills (TEST-006/008/009/015).
//! Local emulator evidence only, not ACA or managed-identity qualification.
//! Run ignored tests with VLITE_AZURITE_BLOB_ENDPOINT; VLITE_CHAOS_SEED
//! replays timing/choice and VLITE_CHAOS_ITERATIONS repeats each drill (1..=20).
//!
//! # Blocked drills
//! TEST-007 (double writer), TEST-014 (paused writer after promotion), and
//! TEST-008's loss-report match need a runtime path creating/claiming
//! `<prefix>authority.json`. Writer startup claim is outside ITEM-024's
//! serving wiring and is not wired; these are deliberately not placeholders.

#![cfg(feature = "server")]

use base64::Engine as _;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    cell::Cell,
    collections::BTreeMap,
    io::{self, BufRead, BufReader, Read, Write},
    net::{Shutdown, SocketAddr, TcpListener, TcpStream, ToSocketAddrs},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{Arc, Condvar, Mutex, mpsc},
    thread::{self, JoinHandle},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const ACCOUNT: &str = "devstoreaccount1";
// Azurite's published development key, not a credential.
const WELL_KNOWN_KEY: &str =
    "Eby8vdM02xNOcqFlqUwJPLlmEtlCDXJ1OUzFT50uSRZ6IFsuFq2UVErCz4I6tq/K1SZFPTOtr/KBHBeksoGMGw==";
const TOKEN: &str = "azurite_chaos_0123456789_ABCDEFGHIJKLMNOPQRSTUVWX";
const ARCHIVE_INTERVAL_SECS: u64 = 60;
const STALENESS_BOUND_MS: u64 = 10_000;
const POLL_INTERVAL_MS: u64 = 250;
const ARCHIVE_WAIT: Duration = Duration::from_secs(100);

thread_local! {
    static SEED: Cell<u64> = const { Cell::new(0) };
}

macro_rules! check {
    ($condition:expr, $($message:tt)+) => {
        assert!($condition, "seed={}: {}", SEED.get(), format_args!($($message)+))
    };
}

struct Random(u64);

impl Random {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn delay(&mut self) -> Duration {
        Duration::from_millis(10 + self.next() % 191)
    }
}

fn drill(name: &str, mut run: impl FnMut(&str, &mut Random)) {
    let Ok(endpoint) = std::env::var("VLITE_AZURITE_BLOB_ENDPOINT") else {
        eprintln!("skipping: VLITE_AZURITE_BLOB_ENDPOINT is unset");
        return;
    };
    let seed = std::env::var("VLITE_CHAOS_SEED").map_or_else(
        |_| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos() as u64
        },
        |text| text.parse().expect("VLITE_CHAOS_SEED must be a u64"),
    );
    SEED.set(seed);
    let iterations = std::env::var("VLITE_CHAOS_ITERATIONS").map_or(1, |text| {
        text.parse::<usize>()
            .expect("VLITE_CHAOS_ITERATIONS must be an integer")
    });
    check!((1..=20).contains(&iterations), "iterations must be 1..=20");
    let mut random = Random(if seed == 0 { 0x9e3779b97f4a7c15 } else { seed });
    for iteration in 1..=iterations {
        let started = Instant::now();
        eprintln!("{name}: seed={seed}, iteration={iteration}/{iterations}");
        run(endpoint.trim_end_matches('/'), &mut random);
        eprintln!(
            "{name}: seed={seed}, iteration={iteration}, elapsed={:?}",
            started.elapsed()
        );
    }
}

fn eventually(what: &str, within: Duration, mut probe: impl FnMut() -> bool) {
    let deadline = Instant::now() + within;
    while !probe() {
        check!(Instant::now() < deadline, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(50));
    }
}

/// Holds one matching object transfer after forwarding a partial body.
/// This makes the fault hit an actual archive/apply window, not a guessed timer.
struct Gate {
    method: &'static str,
    claimed: bool,
    hit: bool,
}

#[derive(Default)]
struct ProxyState {
    cut: bool,
    stopped: bool,
    gate: Option<Gate>,
    sockets: BTreeMap<u64, (TcpStream, TcpStream)>,
}

struct ProxyShared {
    state: Mutex<ProxyState>,
    changed: Condvar,
}

struct Proxy {
    endpoint: String,
    shared: Arc<ProxyShared>,
    worker: Option<JoinHandle<()>>,
}

impl Proxy {
    fn start(endpoint: &str) -> Self {
        let authority = endpoint
            .strip_prefix("http://")
            .unwrap()
            .split('/')
            .next()
            .unwrap();
        let upstream: SocketAddr = authority.to_socket_addrs().unwrap().next().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let endpoint = format!("http://{}/{ACCOUNT}", listener.local_addr().unwrap());
        let shared = Arc::new(ProxyShared {
            state: Mutex::new(ProxyState::default()),
            changed: Condvar::new(),
        });
        let server = shared.clone();
        let worker = thread::spawn(move || {
            let mut workers: Vec<JoinHandle<()>> = Vec::new();
            let mut id = 0;
            loop {
                if server.state.lock().unwrap().stopped {
                    break;
                }
                match listener.accept() {
                    Ok((client, _)) => {
                        if server.state.lock().unwrap().cut {
                            let _ = client.shutdown(Shutdown::Both);
                            continue;
                        }
                        id += 1;
                        let shared = server.clone();
                        workers.push(thread::spawn(move || {
                            let result = forward(client, upstream, &shared, id);
                            let mut state = shared.state.lock().unwrap();
                            state.sockets.remove(&id);
                            if let Err(error) = result
                                && !state.cut
                                && !state.stopped
                            {
                                eprintln!("fault proxy connection: {error}");
                            }
                        }));
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("fault proxy accept: {error}"),
                }
                let mut index = 0;
                while index < workers.len() {
                    if workers[index].is_finished() {
                        workers.swap_remove(index).join().unwrap();
                    } else {
                        index += 1;
                    }
                }
            }
            for worker in workers {
                worker.join().unwrap();
            }
        });
        Self {
            endpoint,
            shared,
            worker: Some(worker),
        }
    }

    fn arm(&self, method: &'static str) {
        self.shared.state.lock().unwrap().gate = Some(Gate {
            method,
            claimed: false,
            hit: false,
        });
    }

    fn hit(&self) -> bool {
        self.shared
            .state
            .lock()
            .unwrap()
            .gate
            .as_ref()
            .is_some_and(|gate| gate.hit)
    }

    fn cut(&self) {
        let mut state = self.shared.state.lock().unwrap();
        state.cut = true;
        state.gate = None;
        for (client, upstream) in state.sockets.values() {
            let _ = client.shutdown(Shutdown::Both);
            let _ = upstream.shutdown(Shutdown::Both);
        }
        self.shared.changed.notify_all();
    }

    fn restore(&self) {
        let mut state = self.shared.state.lock().unwrap();
        state.cut = false;
        state.gate = None;
        self.shared.changed.notify_all();
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        self.cut();
        self.shared.state.lock().unwrap().stopped = true;
        self.shared.changed.notify_all();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn forward(
    mut client: TcpStream,
    upstream: SocketAddr,
    shared: &ProxyShared,
    id: u64,
) -> io::Result<()> {
    // Windows accepts inherit the listener's nonblocking mode.
    client.set_nonblocking(false)?;
    client.set_nodelay(true)?;
    let mut remote = TcpStream::connect_timeout(&upstream, Duration::from_secs(5))?;
    remote.set_nodelay(true)?;
    for stream in [&client, &remote] {
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;
        stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    }
    {
        let mut state = shared.state.lock().unwrap();
        if state.cut || state.stopped {
            client.shutdown(Shutdown::Both)?;
            return Ok(());
        }
        state
            .sockets
            .insert(id, (client.try_clone()?, remote.try_clone()?));
    }
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        let mut byte = [0];
        client.read_exact(&mut byte)?;
        head.push(byte[0]);
        if head.len() > 64 * 1024 {
            return Err(io::Error::other("oversized proxy request header"));
        }
    }
    let head = String::from_utf8(head).map_err(io::Error::other)?;
    let request = head.lines().next().unwrap_or_default();
    let gated = {
        let mut state = shared.state.lock().unwrap();
        if let Some(gate) = &mut state.gate
            && !gate.claimed
            && request.starts_with(gate.method)
            && request.contains("/objects/")
        {
            gate.claimed = true;
            true
        } else {
            false
        }
    };
    // Close each upstream response so subsequent HTTP requests also pass
    // through the header-aware fault hook rather than an opaque keepalive.
    for line in head.split("\r\n").filter(|line| !line.is_empty()) {
        if !line.to_ascii_lowercase().starts_with("connection:") {
            write!(remote, "{line}\r\n")?;
        }
    }
    remote.write_all(b"Connection: close\r\n\r\n")?;
    if gated {
        let mut byte = [0];
        if request.starts_with("PUT ") {
            client.read_exact(&mut byte)?;
            remote.write_all(&byte)?;
        } else {
            let mut response_head = Vec::new();
            while !response_head.ends_with(b"\r\n\r\n") {
                remote.read_exact(&mut byte)?;
                response_head.push(byte[0]);
                if response_head.len() > 64 * 1024 {
                    return Err(io::Error::other("oversized proxy response header"));
                }
            }
            client.write_all(&response_head)?;
            remote.read_exact(&mut byte)?;
            client.write_all(&byte)?;
        }
        let mut state = shared.state.lock().unwrap();
        if let Some(gate) = &mut state.gate {
            gate.hit = true;
        }
        while state.gate.is_some() && !state.cut && !state.stopped {
            state = shared.changed.wait(state).unwrap();
        }
        if state.cut || state.stopped {
            return Ok(());
        }
    }
    let mut request_reader = client.try_clone()?;
    let mut request_writer = remote.try_clone()?;
    let upload = thread::spawn(move || {
        let result = io::copy(&mut request_reader, &mut request_writer);
        let _ = request_writer.shutdown(Shutdown::Write);
        result
    });
    let result = io::copy(&mut remote, &mut client);
    let _ = client.shutdown(Shutdown::Both);
    let _ = remote.shutdown(Shutdown::Both);
    let _ = upload.join();
    result.map(|_| ())
}

struct Proc {
    child: Child,
    address: String,
}

impl Proc {
    fn spawn(mut command: Command) -> Self {
        command
            .env_remove("VLITE_AUTH_TOKEN_FILE")
            .env_remove("VLITE_ADMIN_USERNAME")
            .env_remove("VLITE_ADMIN_PASSWORD_FILE")
            .env_remove("VLITE_PROVIDERS_FILE")
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        let child = command.spawn().unwrap();
        let mut process = Self {
            child,
            address: String::new(),
        };
        let stdout = process.child.stdout.take().unwrap();
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { return };
                let _ = sender.send(line);
            }
        });
        let line = receiver.recv_timeout(Duration::from_secs(120));
        check!(line.is_ok(), "server startup failed: {line:?}");
        let line = line.unwrap();
        check!(
            line.starts_with("LISTENING http://"),
            "unexpected stdout: {line}"
        );
        process.address = line.trim_start_matches("LISTENING http://").to_owned();
        process
    }

    fn call(&self, method: &str, path: &str, body: Value) -> Reply {
        call(
            &self.address,
            method,
            path,
            &if body.is_null() {
                String::new()
            } else {
                body.to_string()
            },
            &[("Authorization", &bearer(TOKEN))],
        )
    }

    fn kill(&mut self) {
        self.child.kill().unwrap();
        self.child.wait().unwrap();
    }
}

impl Drop for Proc {
    fn drop(&mut self) {
        match self.child.try_wait() {
            Ok(Some(_)) => {}
            _ => {
                let _ = self.child.kill();
            }
        }
        let _ = self.child.wait();
    }
}

struct Server(Proc);

impl Server {
    fn start(database: &Path, admin: &Path, key: &Path) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_vlite-server"));
        command
            .arg(database)
            .args(["--create-if-missing", "2", "--bind", "127.0.0.1:0"])
            .args(["--serve-mode", "standalone"])
            .env("VLITE_AUTH_TOKEN", TOKEN)
            .env("VLITE_ADMIN_DIR", admin)
            .env("VLITE_CLOUD_BACKUP_SHARED_KEY_FILE", key);
        Self(Proc::spawn(command))
    }

    fn admin(&self, method: &str, path: &str, body: Value) -> Reply {
        self.0.call(method, path, body)
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

    fn number(&self, name: &str) -> u64 {
        let value = self
            .head
            .lines()
            .filter_map(|line| line.split_once(':'))
            .find_map(|(key, value)| key.eq_ignore_ascii_case(name).then_some(value.trim()));
        check!(value.is_some(), "missing {name}: {self:?}");
        let number = value.unwrap().parse();
        check!(number.is_ok(), "invalid {name}: {self:?}");
        number.unwrap()
    }
}

fn bearer(token: &str) -> String {
    format!("Bearer {token}")
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
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    write!(stream, "{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n", body.len()).unwrap();
    for (name, value) in headers {
        write!(stream, "{name}: {value}\r\n").unwrap();
    }
    write!(stream, "\r\n{body}").unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).unwrap();
    let split = response.windows(4).position(|w| w == b"\r\n\r\n");
    check!(split.is_some(), "missing HTTP response: {response:?}");
    let split = split.unwrap();
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
        .unwrap()
        .split('/')
        .next()
        .unwrap();
    let mut stream = TcpStream::connect(authority).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    write!(stream, "{verb} {path}?restype=container HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\nx-ms-date: {date}\r\nx-ms-version: {version}\r\nAuthorization: SharedKey {ACCOUNT}:{signature}\r\nContent-Length: 0\r\n\r\n").unwrap();
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
            match std::panic::catch_unwind(|| container(&self.endpoint, &self.key, "DELETE", name))
            {
                Ok(202 | 404) => {}
                Ok(status) => eprintln!("Azurite cleanup for {name} returned {status}"),
                Err(_) => eprintln!("Azurite cleanup for {name} failed"),
            }
        }
    }
}

struct Fixture {
    root: PathBuf,
    key_file: PathBuf,
    target_file: PathBuf,
    writer_target: Value,
    proxy: Proxy,
    _containers: Containers,
    _directory: tempfile::TempDir,
}

impl Fixture {
    fn new(endpoint: &str, proxy_writer: bool, proxy_reader: bool) -> Self {
        check!(
            endpoint.ends_with(&format!("/{ACCOUNT}")),
            "expected path-style emulator endpoint"
        );
        let key_text =
            std::env::var("VLITE_AZURITE_ACCOUNT_KEY").unwrap_or_else(|_| WELL_KNOWN_KEY.into());
        let key = base64::engine::general_purpose::STANDARD
            .decode(key_text.trim())
            .unwrap();
        let unique = format!(
            "{:x}-{:x}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let containers = Containers {
            endpoint: endpoint.to_owned(),
            key,
            names: [
                format!("chaos-data-{unique}"),
                format!("chaos-coord-{unique}"),
            ],
        };
        for name in &containers.names {
            let status = container(endpoint, &containers.key, "PUT", name);
            check!(status == 201, "container {name}: {status}");
        }
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let key_file = root.join("emulator.key");
        std::fs::write(&key_file, key_text.trim()).unwrap();
        let proxy = Proxy::start(endpoint);
        let target = |proxied| {
            json!({
                "account":ACCOUNT, "container":containers.names[0],
                "coordination_container":containers.names[1], "prefix":"chaos/",
                "endpoint": if proxied { &proxy.endpoint } else { endpoint },
                "auth":"emulator_shared_key"
            })
        };
        let writer_target = target(proxy_writer);
        let target_file = root.join("target.json");
        std::fs::write(&target_file, target(proxy_reader).to_string()).unwrap();
        std::fs::create_dir(root.join("writer-admin")).unwrap();
        Self {
            root,
            key_file,
            target_file,
            writer_target,
            proxy,
            _containers: containers,
            _directory: directory,
        }
    }

    fn writer(&self) -> Server {
        Server::start(
            &self.root.join("writer.vlite"),
            &self.root.join("writer-admin"),
            &self.key_file,
        )
    }

    fn baseline(&self, writer: &Server) {
        let reply = writer.admin(
            "PUT",
            "/v1/admin/cloud-backup",
            json!({
                "target": self.writer_target, "enabled":true, "interval_secs":604800,
                "drill_every":0, "archive":{"enabled":true,"interval_secs":ARCHIVE_INTERVAL_SECS}
            }),
        );
        check!(reply.status == 200, "configure archive: {reply:?}");
        insert(writer, 1..=2);
        let reply = writer.admin("POST", "/v1/admin/cloud-backup/run", Value::Null);
        check!(reply.status == 201, "publish baseline: {reply:?}");
        let set_id = reply.body["set_id"].as_str().unwrap();
        eventually("Published backup set", Duration::from_secs(30), || {
            let reply = writer.admin("GET", "/v1/admin/cloud-backup/sets", Value::Null);
            check!(reply.status == 200, "backup sets: {reply:?}");
            reply.body["sets"]
                .as_array()
                .unwrap()
                .iter()
                .any(|set| set["set_id"] == set_id && set["record"]["state"] == "published")
        });
    }

    fn reader(&self, name: &str) -> Proc {
        let follower = self.root.join(name);
        let config = self.root.join(format!("{name}.json"));
        std::fs::write(
            &config,
            json!({
                "target_file":self.target_file, "shared_key_file":self.key_file,
                "follower_dir":follower, "capacity_bytes":1073741824u64,
                "staleness_bound_ms":STALENESS_BOUND_MS, "poll_interval_ms":POLL_INTERVAL_MS
            })
            .to_string(),
        )
        .unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_vlite-server"));
        command
            .arg(follower.join("reader-a").join("database.vlite"))
            .args(["--serve-mode", "reader", "--reader-apply-file"])
            .arg(config)
            .args(["--bind", "127.0.0.1:0"])
            .env("VLITE_AUTH_TOKEN", TOKEN)
            .env_remove("VLITE_ADMIN_DIR")
            .env_remove("VLITE_CLOUD_BACKUP_SHARED_KEY_FILE")
            .env_remove("VLITE_ENCRYPTION_KEY")
            .env_remove("VLITE_ENCRYPTION_KEY_HEX");
        let reader = Proc::spawn(command);
        eventually("reader readiness", Duration::from_secs(30), || {
            reader.call("GET", "/ready", Value::Null).status == 200
        });
        reader
    }
}

fn insert(writer: &Server, ids: std::ops::RangeInclusive<u64>) {
    for id in ids {
        let reply = writer.admin(
            "PUT",
            &format!("/v1/vectors/{id}"),
            json!({"vector":[id,1]}),
        );
        check!(reply.status == 204, "acknowledged insert {id}: {reply:?}");
    }
}

fn refused(reply: &Reply) {
    check!(
        reply.status == 503 && matches!(reply.code(), Some("reader_stale" | "reader_apply_failed")),
        "expected fail-closed reader: {reply:?}"
    );
}

/// Read newer IDs before older ones: a concurrent generation swap cannot
/// falsely look like a gap. All acknowledged writes are append-only here.
fn observe(reader: &Proc, writer: &Server, last_id: u64, sequence: &mut u64) -> bool {
    let mut complete = true;
    let mut newer_visible = false;
    for id in (1..=last_id).rev() {
        let path = format!("/v1/vectors/{id}");
        let reply = reader.call("GET", &path, Value::Null);
        match reply.status {
            200 => {
                let age = reply.number("x-vlite-applied-age-ms");
                check!(
                    age <= STALENESS_BOUND_MS,
                    "stale successful read: {reply:?}"
                );
                let applied = reply.number("x-vlite-applied-sequence");
                check!(
                    applied >= *sequence,
                    "sequence regressed from {sequence}: {reply:?}"
                );
                *sequence = applied;
                let source = writer.admin("GET", &path, Value::Null);
                check!(
                    source.status == 200 && source.body["vector"] == reply.body["vector"],
                    "reader diverged at {id}: reader={reply:?}, writer={source:?}"
                );
                newer_visible = true;
            }
            404 => {
                check!(
                    !newer_visible,
                    "archive gap: missing acknowledged {id} before a served newer write"
                );
                complete = false;
            }
            _ => {
                refused(&reply);
                complete = false;
            }
        }
    }
    complete
}

fn delayed_fault(delay: Duration, mut monitor: impl FnMut()) {
    let start = Instant::now();
    eventually("seeded fault delay", Duration::from_secs(5), || {
        monitor();
        start.elapsed() >= delay
    });
}

#[test]
#[ignore = "needs a local Azurite Blob endpoint in VLITE_AZURITE_BLOB_ENDPOINT"]
fn chaos_reader_killed_mid_apply_restarts_from_verified_position() {
    drill("D1 reader killed mid-apply", |endpoint, random| {
        let fixture = Fixture::new(endpoint, false, true);
        let writer = fixture.writer();
        fixture.baseline(&writer);
        let mut reader = fixture.reader("follower");
        let mut sequence = 0;
        check!(
            observe(&reader, &writer, 2, &mut sequence),
            "baseline not visible"
        );
        let initial = sequence;
        fixture.proxy.arm("GET ");
        insert(&writer, 3..=18);
        eventually("reader archive object transfer", ARCHIVE_WAIT, || {
            observe(&reader, &writer, 18, &mut sequence);
            fixture.proxy.hit()
        });
        delayed_fault(random.delay(), || {
            observe(&reader, &writer, 18, &mut sequence);
        });
        reader.kill();
        drop(reader);
        fixture.proxy.restore();
        let reader = fixture.reader("follower");
        eventually(
            "restarted reader reaches verified archive",
            ARCHIVE_WAIT,
            || observe(&reader, &writer, 18, &mut sequence),
        );
        check!(
            sequence > initial,
            "restarted reader did not advance beyond {initial}"
        );
    });
}

#[test]
#[ignore = "needs a local Azurite Blob endpoint in VLITE_AZURITE_BLOB_ENDPOINT"]
fn chaos_blob_outage_fails_reader_closed_and_recovers() {
    drill("D2 reader Blob outage", |endpoint, random| {
        let fixture = Fixture::new(endpoint, false, true);
        let writer = fixture.writer();
        fixture.baseline(&writer);
        let reader = fixture.reader("follower");
        let mut sequence = 0;
        check!(
            observe(&reader, &writer, 2, &mut sequence),
            "baseline not visible"
        );
        let initial = sequence;
        delayed_fault(random.delay(), || {
            observe(&reader, &writer, 2, &mut sequence);
        });
        fixture.proxy.cut();
        let cut = Instant::now();
        let horizon = Duration::from_millis(STALENESS_BOUND_MS + 2 * POLL_INTERVAL_MS + 5_000);
        let mut closed = false;
        eventually(
            "all reads fail closed throughout outage deadline",
            horizon + Duration::from_secs(2),
            || {
                for id in 1..=2 {
                    let reply = reader.call("GET", &format!("/v1/vectors/{id}"), Value::Null);
                    if reply.status == 200 {
                        check!(!closed, "reader reopened during outage: {reply:?}");
                        check!(
                            reply.number("x-vlite-applied-age-ms") <= STALENESS_BOUND_MS,
                            "stale successful outage read: {reply:?}"
                        );
                        check!(
                            cut.elapsed() < horizon,
                            "reader did not close by outage deadline: {reply:?}"
                        );
                    } else {
                        refused(&reply);
                        closed = true;
                    }
                }
                cut.elapsed() >= horizon
            },
        );
        check!(closed, "outage never closed reader");
        refused(&reader.call("GET", "/ready", Value::Null));
        fixture.proxy.restore();
        eventually(
            "reader ready after restore",
            Duration::from_secs(30),
            || reader.call("GET", "/ready", Value::Null).status == 200,
        );
        insert(&writer, 3..=4);
        eventually("recovered reader advances", ARCHIVE_WAIT, || {
            observe(&reader, &writer, 4, &mut sequence)
        });
        check!(sequence > initial, "reader did not advance after recovery");
    });
}

#[test]
#[ignore = "needs a local Azurite Blob endpoint in VLITE_AZURITE_BLOB_ENDPOINT"]
fn chaos_writer_killed_during_archive_never_publishes_a_gap() {
    drill("D3 interrupted writer archive", |endpoint, random| {
        let fixture = Fixture::new(endpoint, true, false);
        let mut writer = fixture.writer();
        fixture.baseline(&writer);
        let reader = fixture.reader("existing-follower");
        let mut sequence = 0;
        check!(
            observe(&reader, &writer, 2, &mut sequence),
            "baseline not visible"
        );
        fixture.proxy.arm("PUT ");
        insert(&writer, 3..=10);
        eventually("partial archive object upload", ARCHIVE_WAIT, || {
            observe(&reader, &writer, 10, &mut sequence);
            fixture.proxy.hit()
        });
        delayed_fault(random.delay(), || {
            observe(&reader, &writer, 10, &mut sequence);
        });
        let cut = random.next() & 1 == 0;
        eprintln!(
            "D3 seed={}: fault={}",
            SEED.get(),
            if cut { "proxy cut" } else { "writer kill" }
        );
        if cut {
            fixture.proxy.cut();
            eventually(
                "writer reports interrupted archive",
                Duration::from_secs(30),
                || {
                    observe(&reader, &writer, 10, &mut sequence);
                    let status = writer.admin("GET", "/v1/admin/cloud-backup", Value::Null);
                    check!(status.status == 200, "archive status: {status:?}");
                    status.body["status"]["archive"]["failures"]
                        .as_u64()
                        .unwrap_or(0)
                        > 0
                },
            );
        }
        writer.0.kill();
        drop(writer);
        fixture.proxy.restore();
        let writer = fixture.writer();
        insert(&writer, 11..=18);
        let fresh = fixture.reader("new-follower");
        let mut fresh_sequence = 0;
        eventually(
            "both readers reach a contiguous archive after writer restart",
            ARCHIVE_WAIT,
            || {
                let old = observe(&reader, &writer, 18, &mut sequence);
                let new = observe(&fresh, &writer, 18, &mut fresh_sequence);
                old && new
            },
        );
        check!(
            sequence == fresh_sequence,
            "reader positions differ: {sequence} vs {fresh_sequence}"
        );
    });
}

#[test]
#[ignore = "needs a local Azurite Blob endpoint in VLITE_AZURITE_BLOB_ENDPOINT"]
fn chaos_failover_report_refuses_without_authority() {
    drill("D4 missing failover authority", |endpoint, _| {
        let fixture = Fixture::new(endpoint, false, false);
        let mut command = Command::new(env!("CARGO_BIN_EXE_vlite-server"));
        command
            .args(["failover", "report", "--target-file"])
            .arg(&fixture.target_file)
            .arg("--shared-key-file")
            .arg(&fixture.key_file)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let child = command.spawn().unwrap();
        let mut process = Proc {
            child,
            address: String::new(),
        };
        eventually("failover refusal exit", Duration::from_secs(30), || {
            process.child.try_wait().unwrap().is_some()
        });
        let status = process.child.wait().unwrap();
        let mut stdout = String::new();
        process
            .child
            .stdout
            .take()
            .unwrap()
            .read_to_string(&mut stdout)
            .unwrap();
        let mut stderr = String::new();
        process
            .child
            .stderr
            .take()
            .unwrap()
            .read_to_string(&mut stderr)
            .unwrap();
        check!(
            !status.success(),
            "report succeeded without authority: {stdout}"
        );
        check!(stdout.is_empty(), "refused report wrote stdout: {stdout}");
        let error: Value = serde_json::from_str(stderr.lines().last().unwrap()).unwrap();
        check!(
            error["error"]
                .as_str()
                .is_some_and(|error| error.starts_with("failover_authority_missing: ")),
            "wrong refusal: {stderr}"
        );
    });
}
