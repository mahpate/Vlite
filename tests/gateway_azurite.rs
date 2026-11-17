//! feature-aca-ha EPIC-004 local tier (ITEM-017): a DB-free gateway and
//! gateway-trusted backends, all real `vlite-server` processes, sharing
//! control state in a local Azurite Blob emulator. Ignored by default: it
//! needs a running emulator, for example
//!
//! ```text
//! docker run -d --name vlite-azurite-gw -p 127.0.0.1:10000:10000 \
//!   mcr.microsoft.com/azure-storage/azurite azurite-blob --blobHost 0.0.0.0 \
//!   --skipApiVersionCheck --inMemoryPersistence
//! VLITE_AZURITE_BLOB_ENDPOINT=http://127.0.0.1:10000/devstoreaccount1 \
//!   cargo test --features server --test gateway_azurite -- --ignored
//! ```
//!
//! Local evidence only: no ACA ingress, managed identity, replica scaling,
//! fencing (OD-002) or measured timing (MEAS-004/005/007) is exercised.

use base64::Engine as _;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const ACCOUNT: &str = "devstoreaccount1";
/// The emulator's published development key (not a secret); override with
/// VLITE_AZURITE_ACCOUNT_KEY for an emulator started with another key.
const WELL_KNOWN_KEY: &str =
    "Eby8vdM02xNOcqFlqUwJPLlmEtlCDXJ1OUzFT50uSRZ6IFsuFq2UVErCz4I6tq/K1SZFPTOtr/KBHBeksoGMGw==";
const TOKEN: &str = "gateway_e2e_0123456789_ABCDEFGHIJKLMNOPQRSTUVWX";
const S_CTL_MS: u64 = 2_000;
const PROPAGATION_MS: u64 = 1_500;

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
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

    /// Waits (bounded) for a stderr JSON event with this name and code.
    fn wait_event(&mut self, name: &str, code: Option<&str>) {
        let deadline = Instant::now() + Duration::from_secs(30);
        let matches = |line: &str| {
            serde_json::from_str::<Value>(line).is_ok_and(|value| {
                value["event"] == name && code.is_none_or(|code| value["code"] == code)
            })
        };
        if self.seen.iter().any(|line| matches(line)) {
            return;
        }
        loop {
            match self
                .events
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            {
                Ok(line) => {
                    let hit = matches(&line);
                    self.seen.push(line);
                    if hit {
                        return;
                    }
                }
                Err(_) => panic!("no {name}/{code:?} event in {:?}", self.seen),
            }
        }
    }

    fn signal(&self) {
        let pid = self.child.id();
        #[cfg(unix)]
        assert!(
            Command::new("kill")
                .args(["-TERM", &pid.to_string()])
                .status()
                .unwrap()
                .success()
        );
        #[cfg(windows)]
        {
            #[link(name = "kernel32")]
            unsafe extern "system" {
                fn GenerateConsoleCtrlEvent(event: u32, group: u32) -> i32;
            }
            const CTRL_BREAK_EVENT: u32 = 1;
            // SAFETY: plain Win32 call with a process group ID we created.
            let sent = unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid) };
            assert_ne!(sent, 0, "{}", std::io::Error::last_os_error());
        }
    }

    fn wait_exit(&mut self) -> ExitStatus {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            assert!(Instant::now() < deadline, "process did not exit");
            std::thread::sleep(Duration::from_millis(20));
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
    let secs = now_ms() / 1000;
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

/// Creates or deletes a container with a SharedKey-signed request.
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

fn cli(args: &[&std::ffi::OsStr]) -> (bool, Value, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_vlite-server"))
        .args(args)
        .env_remove("VLITE_CLOUD_BACKUP_SHARED_KEY_FILE")
        .env_remove("VLITE_ADMIN_DIR")
        .output()
        .unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();
    let value = serde_json::from_str(stdout.trim()).unwrap_or(Value::Null);
    (output.status.success(), value, stderr)
}

/// A standalone server owning `admin` (credential and tenant issuance).
fn standalone(database: &Path, admin: &Path) -> Proc {
    let mut command = Command::new(env!("CARGO_BIN_EXE_vlite-server"));
    command
        .arg(database)
        .args(["--create-if-missing", "2", "--bind", "127.0.0.1:0"])
        .env("VLITE_AUTH_TOKEN", TOKEN)
        .env("VLITE_ADMIN_DIR", admin);
    Proc::spawn(command)
}

/// A gateway-trusted backend: no admin directory, envelopes only.
fn backend(database: &Path, trust: &Path) -> Proc {
    let mut command = Command::new(env!("CARGO_BIN_EXE_vlite-server"));
    command
        .arg(database)
        .args(["--bind", "127.0.0.1:0", "--gateway-trust-file"])
        .arg(trust)
        .env("VLITE_AUTH_TOKEN", TOKEN)
        .env_remove("VLITE_ADMIN_DIR");
    Proc::spawn(command)
}

fn gateway(config: &Path) -> Proc {
    let mut command = Command::new(env!("CARGO_BIN_EXE_vlite-server"));
    command
        .args(["gateway", "--config"])
        .arg(config)
        .env_remove("VLITE_AUTH_TOKEN")
        .env_remove("VLITE_ADMIN_DIR");
    Proc::spawn(command)
}

fn admin(server: &Proc, method: &str, path: &str, body: Value) -> Value {
    let reply = server.call(
        method,
        path,
        &body.to_string(),
        &[("Authorization", &bearer(TOKEN))],
    );
    assert!(reply.status < 300, "{method} {path}: {reply:?}");
    reply.body
}

/// Issues a tenant credential; returns (token, credential id, lineage).
fn issue(server: &Proc, tenant: &str, rps: Option<u32>) -> (String, String, u64) {
    let mut body = json!({"name":"gw","collections":["*"],
        "operations":["read","write","collections","import","diagnostics"],
        "expires_unix_ms":now_ms() + 3_600_000,"tenant_id":tenant});
    if let Some(rps) = rps {
        body["requests_per_second"] = json!(rps);
    }
    let created = admin(server, "POST", "/v1/admin/credentials", body);
    let lineage = created["credential"]["lineage"].as_str().unwrap();
    (
        created["token"].as_str().unwrap().to_owned(),
        created["credential"]["id"].as_str().unwrap().to_owned(),
        u64::from_str_radix(lineage, 16).unwrap(),
    )
}

/// The gateway's view of a "TCP reset after the request was sent": reads one
/// whole request, forwards it to the backend, reads the response head and
/// then drops the connection without answering.
struct Relay {
    address: String,
    requests: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Relay {
    fn start(backend: String) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let requests = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let (count, done) = (requests.clone(), stop.clone());
        let thread = std::thread::spawn(move || {
            for stream in listener.incoming() {
                if done.load(Ordering::SeqCst) {
                    return;
                }
                let Ok(mut client) = stream else { continue };
                client
                    .set_read_timeout(Some(Duration::from_secs(30)))
                    .unwrap();
                let mut request = Vec::new();
                let mut buffer = [0u8; 8192];
                let head_end = loop {
                    let n = client.read(&mut buffer).unwrap_or(0);
                    if n == 0 {
                        break None;
                    }
                    request.extend_from_slice(&buffer[..n]);
                    if let Some(at) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                        break Some(at + 4);
                    }
                };
                let Some(head_end) = head_end else { continue };
                let head = String::from_utf8_lossy(&request[..head_end]).to_ascii_lowercase();
                let length = head
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length:"))
                    .map_or(0, |value| value.trim().parse::<usize>().unwrap());
                while request.len() < head_end + length {
                    let n = client.read(&mut buffer).unwrap();
                    assert!(n > 0, "truncated request");
                    request.extend_from_slice(&buffer[..n]);
                }
                count.fetch_add(1, Ordering::SeqCst);
                let mut upstream = TcpStream::connect(&backend).unwrap();
                upstream.write_all(&request).unwrap();
                upstream
                    .set_read_timeout(Some(Duration::from_secs(30)))
                    .unwrap();
                let mut response = Vec::new();
                while !response.windows(4).any(|w| w == b"\r\n\r\n") {
                    let n = upstream.read(&mut buffer).unwrap();
                    if n == 0 {
                        break;
                    }
                    response.extend_from_slice(&buffer[..n]);
                }
                // The backend applied the write; the caller never hears.
                drop(upstream);
                let _ = client.shutdown(std::net::Shutdown::Both);
            }
        });
        Self {
            address,
            requests,
            stop,
            thread: Some(thread),
        }
    }
}

impl Drop for Relay {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(&self.address);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct Paths {
    target: PathBuf,
    key: PathBuf,
    admin: PathBuf,
}

fn control_state(paths: &Paths, verb: &str, extra: &[&std::ffi::OsStr]) -> Value {
    let mut args: Vec<&std::ffi::OsStr> = vec![
        "control-state".as_ref(),
        verb.as_ref(),
        "--target-file".as_ref(),
        paths.target.as_os_str(),
        "--object".as_ref(),
        "gateway/control-state.json".as_ref(),
        "--shared-key-file".as_ref(),
        paths.key.as_os_str(),
        "--admin-dir".as_ref(),
        paths.admin.as_os_str(),
    ];
    args.extend_from_slice(extra);
    let (ok, value, stderr) = cli(&args);
    assert!(ok, "control-state {verb}: {value} {stderr}");
    assert!(value["generation"].as_u64().is_some(), "{value}");
    value
}

fn write_directory(path: &Path, routes: &[Value]) -> PathBuf {
    std::fs::write(path, serde_json::to_vec(routes).unwrap()).unwrap();
    path.to_owned()
}

fn ready_generation(process: &Proc) -> Option<u64> {
    let reply = process.call("GET", "/ready", "", &[]);
    (reply.status == 200).then(|| reply.body["generation"].as_u64().unwrap())
}

#[test]
#[ignore = "needs a local Azurite Blob endpoint in VLITE_AZURITE_BLOB_ENDPOINT"]
fn gateway_and_trusted_backends_serve_isolate_and_fail_closed() {
    let endpoint = std::env::var("VLITE_AZURITE_BLOB_ENDPOINT")
        .expect("set VLITE_AZURITE_BLOB_ENDPOINT, e.g. http://127.0.0.1:10000/devstoreaccount1");
    let endpoint = endpoint.trim_end_matches('/').to_owned();
    let key_text =
        std::env::var("VLITE_AZURITE_ACCOUNT_KEY").unwrap_or_else(|_| WELL_KNOWN_KEY.into());
    let account_key = base64::engine::general_purpose::STANDARD
        .decode(key_text.trim())
        .unwrap();
    let unique = format!("{:x}", now_ms() % 0xffff_ffff_ffff);
    let data = format!("vlite-gw-data-{unique}");
    let coordination = format!("vlite-gw-coord-{unique}");
    for name in [&data, &coordination] {
        assert_eq!(
            container(&endpoint, &account_key, "PUT", name),
            201,
            "{name}"
        );
    }

    let directory = tempfile::tempdir_in(".").unwrap();
    let root = directory.path();
    let paths = Paths {
        target: root.join("target.json"),
        key: root.join("emulator.key"),
        admin: root.join("admin"),
    };
    std::fs::write(&paths.key, format!("{}\n", key_text.trim())).unwrap();
    std::fs::write(
        &paths.target,
        json!({"account":ACCOUNT,"container":data,"coordination_container":coordination,
            "prefix":"gw/","endpoint":endpoint,"auth":"emulator_shared_key"})
        .to_string(),
    )
    .unwrap();

    // Gateway signing key: the secret seed and the public bundle are
    // separate files; the command prints neither key.
    let signing_key = root.join("gateway.signing-key");
    let bundle = root.join("gateway.public.json");
    let (ok, value, stderr) = cli(&[
        "gateway-keygen".as_ref(),
        "--kid".as_ref(),
        "k1".as_ref(),
        "--generation".as_ref(),
        "1".as_ref(),
        "--signing-key-out".as_ref(),
        signing_key.as_os_str(),
        "--public-bundle-out".as_ref(),
        bundle.as_os_str(),
    ]);
    assert!(ok, "{value} {stderr}");
    let secret = std::fs::read_to_string(&signing_key).unwrap();
    assert!(!value.to_string().contains(secret.trim()) && !stderr.contains(secret.trim()));

    // Standalone servers issue tenants and credentials into one admin
    // directory, and the shared pool gets data to compare against.
    let pool_db = root.join("pool.vlite");
    let dedicated_db = root.join("dedicated.vlite");
    let pool = standalone(&pool_db, &paths.admin);
    let tenant = |name: &str| {
        admin(&pool, "POST", "/v1/admin/tenants", json!({"name":name}))["tenant"]["id"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    let (t1, t2, t3) = (tenant("one"), tenant("two"), tenant("three"));
    let (c1, _, pool_lineage) = issue(&pool, &t1, None);
    let (c2, c2_id, _) = issue(&pool, &t2, None);
    let (c1_rate, _, _) = issue(&pool, &t1, Some(2));
    let (c1_half, _, _) = issue(&pool, &t1, Some(1));
    let t1_auth = bearer(&c1);
    let as_t1: &[(&str, &str)] = &[("Authorization", &t1_auth)];
    assert_eq!(
        pool.call(
            "POST",
            "/v1/collections",
            r#"{"name":"docs","dimensions":2}"#,
            as_t1
        )
        .status,
        201
    );
    assert_eq!(
        pool.call(
            "PUT",
            "/v1/collections/docs/vectors/1",
            r#"{"vector":[1.0,0.0]}"#,
            as_t1
        )
        .status,
        204
    );
    let reads = [
        ("GET", "/v1/collections/docs/vectors/1", ""),
        ("GET", "/v1/collections", ""),
        (
            "POST",
            "/v1/collections/docs/search",
            r#"{"vector":[1.0,0.0],"k":1}"#,
        ),
        ("GET", "/v1/collections/docs/kv/entries?namespace=n", ""),
    ];
    let expected: Vec<(u16, Value)> = reads
        .iter()
        .map(|(method, path, body)| {
            let reply = pool.call(method, path, body, as_t1);
            (reply.status, reply.body)
        })
        .collect();
    drop(pool);
    let dedicated = standalone(&dedicated_db, &paths.admin);
    let (c3, _, dedicated_lineage) = issue(&dedicated, &t3, None);
    drop(dedicated);

    // Bootstrap: publish generation 1 from the admin directory (no routes yet).
    let routes_file = root.join("routes.json");
    write_directory(&routes_file, &[]);
    let published = control_state(
        &paths,
        "publish",
        &["--directory-file".as_ref(), routes_file.as_os_str()],
    );
    assert_eq!(published["generation"], 1);
    let again = cli(&[
        "control-state".as_ref(),
        "publish".as_ref(),
        "--target-file".as_ref(),
        paths.target.as_os_str(),
        "--object".as_ref(),
        "gateway/control-state.json".as_ref(),
        "--shared-key-file".as_ref(),
        paths.key.as_os_str(),
        "--admin-dir".as_ref(),
        paths.admin.as_os_str(),
    ]);
    assert!(
        !again.0 && again.2.contains("control_state_exists"),
        "{again:?}"
    );

    let control = json!({"target_file":paths.target,"object":"gateway/control-state.json",
        "shared_key_file":paths.key});
    let trust = |audience: &str, name: &str| {
        let path = root.join(name);
        std::fs::write(
            &path,
            json!({"audience":audience,"public_key_bundle_file":bundle,
                "control_state":control,"s_ctl_ms":S_CTL_MS,"poll_interval_ms":200,
                "skew_ms":1000,"max_lifetime_ms":5000,
                "readiness_propagation_ms":PROPAGATION_MS})
            .to_string(),
        )
        .unwrap();
        path
    };
    let backend_a = backend(&pool_db, &trust("shared-pool", "trust-a.json"));
    let backend_b = backend(&dedicated_db, &trust("tenant-three", "trust-b.json"));
    let a_url = format!("http://{}", backend_a.address);
    let routes = |pool_writer: &str| {
        vec![
            json!({"tenant":t1,"kind":"shared-pool","writer":pool_writer,
                "audience":"shared-pool","lineage":pool_lineage}),
            json!({"tenant":t2,"kind":"shared-pool","writer":a_url,
                "audience":"shared-pool","lineage":pool_lineage}),
            json!({"tenant":t3,"kind":"dedicated","writer":format!("http://{}", backend_b.address),
                "audience":"tenant-three","lineage":dedicated_lineage,"epoch":1}),
        ]
    };
    write_directory(&routes_file, &routes(&a_url));
    let routed = control_state(
        &paths,
        "update",
        &["--directory-file".as_ref(), routes_file.as_os_str()],
    );
    let generation = routed["generation"].as_u64().unwrap();
    assert_eq!(generation, 2);
    for process in [&backend_a, &backend_b] {
        eventually(
            "backend at the routed generation",
            Duration::from_secs(10),
            || ready_generation(process) == Some(generation),
        );
    }

    let config = |name: &str| {
        let path = root.join(name);
        std::fs::write(
            &path,
            json!({"bind":"127.0.0.1:0","allow_remote":false,"control_state":control,
                "s_ctl_ms":S_CTL_MS,"poll_interval_ms":200,"max_replicas":2,
                "envelope_lifetime_ms":5000,"public_key_bundle_file":bundle,
                "signing_key_file":signing_key,"active_kid":"k1",
                "readiness_propagation_ms":PROPAGATION_MS,"drain_timeout_ms":5000,
                "max_request_bytes":1048576,"max_in_flight":64,
                "upstream_timeout_ms":10000,"max_buckets":1024})
            .to_string(),
        )
        .unwrap();
        path
    };
    let g1_config = config("gateway-1.json");
    let mut g1 = gateway(&g1_config);
    let mut g2 = gateway(&config("gateway-2.json"));
    assert_eq!(ready_generation(&g1), Some(generation));
    assert_eq!(g1.call("GET", "/health", "", &[]).status, 200);
    // TEST-012/AC-011: the gateway's status output says its replicas add no
    // write capacity.
    g1.wait_event("gateway_started", None);
    assert!(
        g1.seen.iter().any(|line| {
            serde_json::from_str::<Value>(line).is_ok_and(|event| {
                event["event"] == "gateway_started"
                    && event["write_capacity"]
                        .as_str()
                        .is_some_and(|text| text.contains("no write capacity"))
            })
        }),
        "{:?}",
        g1.seen
    );

    // Equivalence: the same reads through either gateway replica answer
    // exactly as the standalone server did.
    for gateway in [&g1, &g2] {
        for ((method, path, body), expected) in reads.iter().zip(&expected) {
            let reply = gateway.call(method, path, body, as_t1);
            assert_eq!(
                (reply.status, &reply.body),
                (expected.0, &expected.1),
                "{path}"
            );
        }
    }
    // Writes and queries go to the tenant's writer.
    let reply = g1.call(
        "PUT",
        "/v1/collections/docs/vectors/2",
        r#"{"vector":[0.0,1.0]}"#,
        as_t1,
    );
    assert_eq!(reply.status, 204, "{reply:?}");
    assert_eq!(
        g2.call("GET", "/v1/collections/docs/vectors/2", "", as_t1)
            .status,
        200
    );

    // Isolation on the shared pool: T2 does not see T1's collection.
    let t2_auth = bearer(&c2);
    let as_t2: &[(&str, &str)] = &[("Authorization", &t2_auth)];
    assert_eq!(
        g1.call("GET", "/v1/collections/docs/vectors/1", "", as_t2)
            .status,
        404
    );
    let listed = g1.call("GET", "/v1/collections", "", as_t2);
    assert_eq!(listed.status, 200);
    assert!(!listed.body.to_string().contains("docs"), "{listed:?}");
    // The dedicated tenant reaches its own backend and nothing else.
    let t3_auth = bearer(&c3);
    let as_t3: &[(&str, &str)] = &[("Authorization", &t3_auth)];
    assert_eq!(
        g1.call(
            "POST",
            "/v1/collections",
            r#"{"name":"own","dimensions":2}"#,
            as_t3
        )
        .status,
        201
    );
    assert_eq!(
        g1.call("GET", "/v1/collections/docs/vectors/1", "", as_t3)
            .status,
        404
    );

    // Denials at the gateway, before anything is forwarded.
    let reply = g1.call("GET", "/v1/collections", "", &[]);
    assert_eq!(reply.status, 401, "{reply:?}");
    let reply = g1.call(
        "GET",
        "/v1/collections",
        "",
        &[("Authorization", &bearer(TOKEN))],
    );
    assert_eq!(reply.status, 401, "{reply:?}");
    let reply = g1.call("GET", "/v1/admin/credentials", "", as_t1);
    assert_eq!(reply.status, 403, "{reply:?}");
    let reply = g1.call("GET", "/v1/collections/docs/../docs", "", as_t1);
    assert!(matches!(reply.status, 400 | 404), "{reply:?}");
    // A caller cannot smuggle a principal or an admin bearer through.
    let reply = g1.call(
        "GET",
        "/v1/collections/docs/vectors/1",
        "",
        &[
            ("Authorization", &t2_auth),
            ("X-VLite-Principal", "forged"),
            ("Cookie", "vlite_session=forged"),
        ],
    );
    assert_eq!(reply.status, 404, "{reply:?}");

    // The trusted backend refuses everything but a valid envelope; its
    // readiness probe is unauthenticated and needs no admin token.
    assert_eq!(backend_a.call("GET", "/ready", "", &[]).status, 200);
    for headers in [
        vec![("Authorization", t1_auth.clone())],
        vec![("Authorization", bearer(TOKEN))],
        vec![("X-VLite-Principal", "garbage".to_owned())],
    ] {
        let headers: Vec<(&str, &str)> = headers.iter().map(|(n, v)| (*n, v.as_str())).collect();
        let reply = backend_a.call("GET", "/v1/collections", "", &headers);
        assert_eq!(reply.status, 401, "{headers:?} {reply:?}");
    }

    // Restart independence: a replaced replica serves from the shared
    // state with no local state of its own; the other replica is untouched.
    g1.signal();
    assert!(g1.wait_exit().success());
    let mut g1 = gateway(&g1_config);
    assert_eq!(
        g1.call("GET", "/v1/collections/docs/vectors/1", "", as_t1)
            .status,
        200
    );

    // Edge rate limit: rps 2 shared by max_replicas 2 is one request per
    // second per replica; rps 1 is below one per replica and fails closed.
    let rate_auth = bearer(&c1_rate);
    let as_rate: &[(&str, &str)] = &[("Authorization", &rate_auth)];
    assert_eq!(g1.call("GET", "/v1/collections", "", as_rate).status, 200);
    let limited = g1.call("GET", "/v1/collections", "", as_rate);
    assert_eq!(
        (limited.status, limited.code()),
        (429, Some("rate_limited")),
        "{limited:?}"
    );
    assert!(limited.head.to_ascii_lowercase().contains("retry-after: 1"));
    let half = g1.call(
        "GET",
        "/v1/collections",
        "",
        &[("Authorization", &bearer(&c1_half))],
    );
    assert_eq!(
        (half.status, half.code()),
        (503, Some("rate_limit_share_below_one")),
        "{half:?}"
    );

    // Revocation and suspension propagate through the control state to
    // every replica within the poll interval (bounded here by S_ctl).
    control_state(
        &paths,
        "update",
        &["--revoke-credential".as_ref(), c2_id.as_ref()],
    );
    for gateway in [&g1, &g2] {
        eventually("revocation", Duration::from_millis(S_CTL_MS * 3), || {
            gateway.call("GET", "/v1/collections", "", as_t2).status == 401
        });
    }
    let suspended = control_state(
        &paths,
        "update",
        &["--suspend-tenant".as_ref(), t3.as_ref()],
    );
    for gateway in [&g1, &g2] {
        eventually("suspension", Duration::from_millis(S_CTL_MS * 3), || {
            gateway.call("GET", "/v1/collections", "", as_t3).code() == Some("tenant_suspended")
        });
    }
    // The backends advance their own minimum A from the poll, not only from
    // envelopes they happen to see (backend B got no request since then).
    let after = suspended["generation"].as_u64().unwrap();
    eventually("backend minimum", Duration::from_secs(10), || {
        let reply = backend_b.call("GET", "/ready", "", &[]);
        reply.status == 200 && reply.body["minimum_generation"].as_u64() == Some(after)
    });
    control_state(&paths, "update", &["--resume-tenant".as_ref(), t3.as_ref()]);
    eventually("resume", Duration::from_millis(S_CTL_MS * 3), || {
        g1.call("GET", "/v1/collections", "", as_t3).status == 200
    });

    // A backend connection lost after the write was sent is reported as an
    // unknown outcome, never replayed; the write is applied exactly once.
    let relay = Relay::start(backend_a.address.clone());
    write_directory(&routes_file, &routes(&format!("http://{}", relay.address)));
    let relayed = control_state(
        &paths,
        "update",
        &["--directory-file".as_ref(), routes_file.as_os_str()],
    )["generation"]
        .as_u64()
        .unwrap();
    eventually("relay route", Duration::from_secs(10), || {
        ready_generation(&g1) == Some(relayed)
    });
    let reply = g1.call(
        "PUT",
        "/v1/collections/docs/vectors/3",
        r#"{"vector":[0.5,0.5]}"#,
        as_t1,
    );
    assert_eq!(
        (reply.status, reply.code()),
        (502, Some("write_outcome_unknown")),
        "{reply:?}"
    );
    assert_eq!(relay.requests.load(Ordering::SeqCst), 1, "no replay");
    drop(relay);
    write_directory(&routes_file, &routes(&a_url));
    let restored = control_state(
        &paths,
        "update",
        &["--directory-file".as_ref(), routes_file.as_os_str()],
    )["generation"]
        .as_u64()
        .unwrap();
    eventually("restored route", Duration::from_secs(10), || {
        ready_generation(&g1) == Some(restored)
    });
    let written = g1.call("GET", "/v1/collections/docs/vectors/3", "", as_t1);
    assert_eq!(written.status, 200, "{written:?}");

    // Drain: readiness reports draining before the listener closes, then
    // the replica exits cleanly within the bounded drain.
    g2.signal();
    // The draining event is logged as readiness flips, before the listener
    // closes; waiting for it only removes the signal-delivery race.
    g2.wait_event("draining", None);
    let drained = g2.call("GET", "/ready", "", &[]);
    assert_eq!(
        (drained.status, drained.code()),
        (503, Some("draining")),
        "{drained:?}"
    );
    assert!(g2.wait_exit().success());
    g2.wait_event("server_shutdown", None);

    // Store outage: every process becomes not-ready after S_ctl and denies
    // data requests; poll failures are logged, not swallowed.
    assert_eq!(
        container(&endpoint, &account_key, "DELETE", &coordination),
        202
    );
    for process in [&g1, &backend_a, &backend_b] {
        eventually(
            "stale control state",
            Duration::from_millis(S_CTL_MS * 4),
            || process.call("GET", "/ready", "", &[]).status == 503,
        );
    }
    let denied = g1.call("GET", "/v1/collections", "", as_t1);
    assert_eq!(
        (denied.status, denied.code()),
        (503, Some("control_state_stale"))
    );
    g1.wait_event("control_state_poll_failed", None);

    // A recreated object at a lower generation is refused, not adopted.
    eventually("container recreated", Duration::from_secs(30), || {
        container(&endpoint, &account_key, "PUT", &coordination) == 201
    });
    control_state(
        &paths,
        "publish",
        &["--directory-file".as_ref(), routes_file.as_os_str()],
    );
    g1.wait_event("control_state_poll_failed", Some("control_state_regressed"));
    assert_eq!(g1.call("GET", "/ready", "", &[]).status, 503);
    assert_eq!(backend_a.call("GET", "/ready", "", &[]).status, 503);

    drop((g1, backend_a, backend_b));
    for name in [&data, &coordination] {
        container(&endpoint, &account_key, "DELETE", name);
    }
}
