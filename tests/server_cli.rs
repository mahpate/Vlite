use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::TcpStream,
    path::Path,
    process::{Child, Command, Stdio},
    sync::mpsc,
    time::Duration,
};

struct Server {
    child: Child,
    address: String,
}

impl Server {
    fn start(path: &Path) -> Self {
        Self::start_with_source(path, false)
    }

    fn start_with_source(path: &Path, file: bool) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_vlite-server"));
        command
            .arg(path)
            .args(["--create-if-missing", "2", "--bind", "127.0.0.1:0"])
            .env_remove("VLITE_AUTH_TOKEN")
            .env_remove("VLITE_AUTH_TOKEN_FILE");
        if file {
            let token = path.with_extension("token");
            std::fs::write(&token, format!("{TOKEN}\r\n")).unwrap();
            command.arg("--token-file").arg(token);
        } else {
            command.env("VLITE_AUTH_TOKEN", TOKEN);
        }
        let mut child = command.stdout(Stdio::piped()).spawn().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let mut line = String::new();
            BufReader::new(stdout).read_line(&mut line).unwrap();
            let _ = sender.send(line);
        });
        let mut server = Self {
            child,
            address: String::new(),
        };
        let line = receiver
            .recv_timeout(Duration::from_secs(15))
            .expect("server startup timed out");
        server.address = line
            .trim()
            .strip_prefix("LISTENING http://")
            .expect("startup address")
            .to_owned();
        server
    }

    fn request(&self, method: &str, path: &str, body: Value) -> (u16, Value) {
        let body = body.to_string();
        let mut stream = TcpStream::connect(&self.address).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        write!(stream,
            "{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nAuthorization: Bearer {TOKEN}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",body.len()
        ).unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        let (head, body) = response.split_once("\r\n\r\n").unwrap();
        let status = head.split_whitespace().nth(1).unwrap().parse().unwrap();
        (
            status,
            if status == 204 {
                Value::Null
            } else {
                serde_json::from_str(body).unwrap()
            },
        )
    }
}

const TOKEN: &str = "test_secret_0123456789_ABCDEFGHIJKLMNOPQRSTUVWXYZ";

impl Drop for Server {
    fn drop(&mut self) {
        if self.child.try_wait().unwrap().is_none() {
            self.child.kill().unwrap();
        }
        self.child.wait().unwrap();
    }
}

fn fails(path: &Path, args: &[&str]) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_vlite-server"))
        .arg(path)
        .arg("--insecure-local")
        .env_remove("VLITE_AUTH_TOKEN")
        .env_remove("VLITE_AUTH_TOKEN_FILE")
        .args(args)
        .output()
        .unwrap();
    assert!(!output.status.success(), "{output:?}");
    String::from_utf8(output.stderr).unwrap()
}

#[test]
fn authentication_configuration_fails_closed_before_creating_a_database() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("no-create.vlite");
    let secret = directory.path().join("token");
    std::fs::write(&secret, "short").unwrap();
    let run = |args: &[&str]| {
        let output = Command::new(env!("CARGO_BIN_EXE_vlite-server"))
            .arg(&path)
            .args(["--create-if-missing", "2", "--bind", "127.0.0.1:0"])
            .args(args)
            .env_remove("VLITE_AUTH_TOKEN")
            .env_remove("VLITE_AUTH_TOKEN_FILE")
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(!path.exists());
        String::from_utf8(output.stderr).unwrap()
    };
    assert!(run(&[]).contains("select exactly one"));
    assert!(run(&["--token-file", secret.to_str().unwrap()]).contains("base64url"));
    assert!(
        run(&["--insecure-local", "--token-file", secret.to_str().unwrap()])
            .contains("select exactly one")
    );
    assert!(fails(&path, &["--allow-remote", "--bind", "0.0.0.0:0"]).contains("loopback"));
}

#[test]
fn real_http_acknowledged_data_survives_process_termination_and_restart() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("network.vlite");
    let server = Server::start(&path);
    assert_eq!(server.request("GET", "/ready", Value::Null).0, 200);
    assert_eq!(
        server
            .request(
                "POST",
                "/v1/batches",
                json!({"operations":[
                    {"op":"put","id":"18446744073709551615","vector":[1,2],"payload_base64":"AP8="},
                    {"op":"put","id":"0","vector":[4,5]},
                    {"op":"delete","id":"0"}
                ]})
            )
            .0,
        204
    );
    assert!(fails(&path, &["--bind", "127.0.0.1:0"]).contains("already locked"));
    drop(server);
    let bytes = std::fs::read(&path).unwrap();
    let server = Server::start_with_source(&path, true);
    let (status, record) = server.request("GET", "/v1/vectors/18446744073709551615", Value::Null);
    assert_eq!(status, 200);
    assert_eq!(record["payload_base64"], "AP8=");
    assert_eq!(server.request("GET", "/v1/vectors/0", Value::Null).0, 404);
    assert_eq!(
        server
            .request("POST", "/v1/search", json!({"vector":[1,2],"k":1}))
            .1["hits"][0]["distance"],
        0.0
    );
    drop(server);
    assert_eq!(std::fs::read(path).unwrap(), bytes);
}

#[test]
fn startup_never_overwrites_repairs_or_silently_changes_dimensions() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("startup.vlite");
    assert!(fails(&path, &["--bind", "127.0.0.1:0"]).contains("startup_or_shutdown_failure"));
    assert!(!path.exists());
    assert!(fails(&path, &["--create-if-missing", "0"]).contains("dimensions"));
    assert!(
        fails(&path, &["--create-if-missing", "2", "--bind", "0.0.0.0:0"])
            .contains("--allow-remote")
    );
    assert!(!path.exists());
    vlite::Database::create(&path, 3).unwrap().close().unwrap();
    let before = std::fs::read(&path).unwrap();
    assert!(
        fails(
            &path,
            &["--create-if-missing", "2", "--bind", "127.0.0.1:0"]
        )
        .contains("expected 2")
    );
    assert_eq!(std::fs::read(&path).unwrap(), before);
    let mut db = vlite::Database::open(&path).unwrap();
    db.put(1, &[1.0, 2.0, 3.0], b"acknowledged").unwrap();
    db.close().unwrap();
    let mut truncated = std::fs::read(&path).unwrap();
    truncated.pop();
    std::fs::write(&path, &truncated).unwrap();
    assert!(
        fails(
            &path,
            &["--create-if-missing", "3", "--bind", "127.0.0.1:0"]
        )
        .contains("incomplete trailing write")
    );
    assert_eq!(std::fs::read(&path).unwrap(), truncated);
    std::fs::write(&path, b"corrupt; never replace").unwrap();
    assert!(
        fails(
            &path,
            &["--create-if-missing", "2", "--bind", "127.0.0.1:0"]
        )
        .contains("startup_or_shutdown_failure")
    );
    assert_eq!(std::fs::read(path).unwrap(), b"corrupt; never replace");
}

#[test]
fn create_format_selects_v4_and_invalid_operational_options_fail_before_creation() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("v4.vlite");
    assert!(
        fails(&path, &["--create-format", "5", "--create-if-missing", "2"])
            .contains("create-format")
    );
    assert!(fails(&path, &["--create-format", "4"]).contains("--create-if-missing"));
    let output = Command::new(env!("CARGO_BIN_EXE_vlite-server"))
        .arg(&path)
        .args([
            "--insecure-local",
            "--create-if-missing",
            "2",
            "--bind",
            "127.0.0.1:0",
        ])
        .env_remove("VLITE_AUTH_TOKEN")
        .env_remove("VLITE_AUTH_TOKEN_FILE")
        .env("VLITE_BACKUP_INTERVAL_SECS", "3600")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("admin directory"),
        "{output:?}"
    );
    assert!(!path.exists());

    let mut command = Command::new(env!("CARGO_BIN_EXE_vlite-server"));
    command
        .arg(&path)
        .args([
            "--create-if-missing",
            "2",
            "--create-format",
            "4",
            "--bind",
            "127.0.0.1:0",
        ])
        .env_remove("VLITE_AUTH_TOKEN_FILE")
        .env("VLITE_AUTH_TOKEN", TOKEN);
    let mut child = command.stdout(Stdio::piped()).spawn().unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    let server = Server {
        child,
        address: line
            .trim()
            .strip_prefix("LISTENING http://")
            .unwrap()
            .to_owned(),
    };
    let (status, created) = server.request(
        "POST",
        "/v1/collections",
        json!({"name":"docs","dimensions":3}),
    );
    assert_eq!(status, 201, "{created}");
    assert_eq!(
        server.request("GET", "/v1/capabilities", Value::Null).1["format_version"],
        4
    );
    drop(server);
    let db = vlite::Database::open(&path).unwrap();
    assert_eq!(db.format_version(), 4);
}

// TEST-008: `--grpc-bind`, `--ann-snapshot` and the shared graceful shutdown.

/// A server with captured stderr JSON events and an optional gRPC address.
struct Launched {
    server: Server,
    #[cfg_attr(not(feature = "grpc"), allow(dead_code))]
    grpc: Option<String>,
    events: mpsc::Receiver<String>,
}

fn launch(path: &Path, extra: &[&str]) -> Launched {
    let mut command = Command::new(env!("CARGO_BIN_EXE_vlite-server"));
    command
        .arg(path)
        .args(["--create-if-missing", "2", "--bind", "127.0.0.1:0"])
        .args(extra)
        .env_remove("VLITE_AUTH_TOKEN_FILE")
        .env("VLITE_AUTH_TOKEN", TOKEN)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // Its own process group, so Ctrl-Break reaches only the server.
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        command.creation_flags(CREATE_NEW_PROCESS_GROUP);
    }
    let mut child = command.spawn().unwrap();
    let (lines, receiver) = mpsc::channel();
    let stdout = child.stdout.take().unwrap();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { return };
            let _ = lines.send(line);
        }
    });
    let (events_tx, events) = mpsc::channel();
    let stderr = child.stderr.take().unwrap();
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines() {
            let Ok(line) = line else { return };
            let _ = events_tx.send(line);
        }
    });
    let mut server = Server {
        child,
        address: String::new(),
    };
    let line = receiver
        .recv_timeout(Duration::from_secs(30))
        .expect("server startup timed out");
    server.address = line
        .strip_prefix("LISTENING http://")
        .expect("REST address")
        .to_owned();
    let grpc = extra.contains(&"--grpc-bind").then(|| {
        let line = receiver
            .recv_timeout(Duration::from_secs(30))
            .expect("gRPC address");
        let scheme = if extra.contains(&"--grpc-tls-cert") {
            "LISTENING grpcs://"
        } else {
            "LISTENING grpc://"
        };
        line.strip_prefix(scheme)
            .expect("gRPC address line")
            .to_owned()
    });
    Launched {
        server,
        grpc,
        events,
    }
}

impl Launched {
    /// The next stderr JSON line with this event name.
    fn event(&self, name: &str) -> Value {
        loop {
            let line = self
                .events
                .recv_timeout(Duration::from_secs(60))
                .unwrap_or_else(|_| panic!("no {name} event"));
            if let Ok(value) = serde_json::from_str::<Value>(&line)
                && value["event"] == name
            {
                return value;
            }
        }
    }

    /// Every remaining stderr line (after the process exited).
    fn rest_of_stderr(&self) -> Vec<String> {
        self.events.try_iter().collect()
    }

    /// Sends the platform stop signal and waits for a graceful exit.
    fn stop(&mut self) -> std::process::ExitStatus {
        let pid = self.server.child.id();
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
        let deadline = std::time::Instant::now() + Duration::from_secs(600);
        loop {
            if let Some(status) = self.server.child.try_wait().unwrap() {
                return status;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "server did not drain and exit"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

#[test]
fn grpc_bind_combinations_are_validated_before_opening() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("grpc-flags.vlite");
    let create = ["--create-if-missing", "2"];
    #[cfg(feature = "grpc")]
    {
        let equal = fails(
            &path,
            &[
                create[0],
                create[1],
                "--bind",
                "127.0.0.1:18731",
                "--grpc-bind",
                "127.0.0.1:18731",
            ],
        );
        assert!(equal.contains("must differ from --bind"), "{equal}");
        let remote = fails(&path, &[create[0], create[1], "--grpc-bind", "0.0.0.0:0"]);
        assert!(remote.contains("--allow-remote"), "{remote}");
        let insecure = fails(
            &path,
            &[
                create[0],
                create[1],
                "--allow-remote",
                "--grpc-bind",
                "0.0.0.0:0",
            ],
        );
        assert!(insecure.contains("loopback"), "{insecure}");
        assert!(fails(&path, &["--grpc-bind", "not-an-address"]).contains("invalid"));
        let cert = directory.path().join("cert.pem");
        let key = directory.path().join("key.pem");
        std::fs::write(&cert, "not a certificate").unwrap();
        std::fs::write(&key, "not a key").unwrap();
        let (cert, key) = (cert.to_str().unwrap(), key.to_str().unwrap());
        let half = fails(
            &path,
            &[
                create[0],
                create[1],
                "--grpc-bind",
                "127.0.0.1:0",
                "--grpc-tls-cert",
                cert,
            ],
        );
        assert!(half.contains("must be given together"), "{half}");
        let unbound = fails(
            &path,
            &[
                create[0],
                create[1],
                "--grpc-tls-cert",
                cert,
                "--grpc-tls-key",
                key,
            ],
        );
        assert!(unbound.contains("requires --grpc-bind"), "{unbound}");
        let invalid = fails(
            &path,
            &[
                create[0],
                create[1],
                "--grpc-bind",
                "127.0.0.1:0",
                "--grpc-tls-cert",
                cert,
                "--grpc-tls-key",
                key,
            ],
        );
        assert!(invalid.contains("invalid gRPC TLS identity"), "{invalid}");
        let unreadable = fails(
            &path,
            &[
                create[0],
                create[1],
                "--grpc-bind",
                "127.0.0.1:0",
                "--grpc-tls-cert",
                "missing-cert.pem",
                "--grpc-tls-key",
                key,
            ],
        );
        assert!(
            unreadable.contains("cannot read --grpc-tls-cert"),
            "{unreadable}"
        );
        // A port conflict fails before the database is created.
        let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let taken = taken.local_addr().unwrap().to_string();
        let conflict = fails(&path, &[create[0], create[1], "--grpc-bind", &taken]);
        assert!(
            conflict.contains("startup_or_shutdown_failure"),
            "{conflict}"
        );
    }
    #[cfg(not(feature = "grpc"))]
    {
        let missing = fails(&path, &[create[0], create[1], "--grpc-bind", "127.0.0.1:0"]);
        assert!(
            missing.contains("--grpc-bind requires a build with the grpc feature"),
            "{missing}"
        );
    }
    assert!(!path.exists());
}

#[cfg(feature = "grpc")]
#[test]
fn grpc_listener_reports_health_and_both_listeners_drain_on_signal() {
    use tonic_health::pb::{HealthCheckRequest, health_client::HealthClient};
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("grpc.vlite");
    let mut launched = launch(&path, &["--grpc-bind", "127.0.0.1:0"]);
    let grpc = launched.grpc.clone().unwrap();
    assert_ne!(grpc, launched.server.address);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let status = runtime.block_on(async {
        let channel = tonic::transport::Channel::from_shared(format!("http://{grpc}"))
            .unwrap()
            .connect()
            .await
            .unwrap();
        let mut health = HealthClient::new(channel);
        let mut statuses = Vec::new();
        for service in ["", "vlite.v1.VLite"] {
            let response = health
                .check(HealthCheckRequest {
                    service: service.into(),
                })
                .await
                .unwrap();
            statuses.push(response.into_inner().status);
        }
        statuses
    });
    // Drop the client runtime so its connection closes during the drain.
    drop(runtime);
    assert_eq!(status, [1, 1], "SERVING");
    assert_eq!(launched.server.request("GET", "/ready", Value::Null).0, 200);
    let exit = launched.stop();
    assert!(exit.success(), "{exit:?}: {:?}", launched.rest_of_stderr());
    assert!(
        !launched
            .rest_of_stderr()
            .iter()
            .any(|line| line.contains("ann_snapshot")),
        "snapshots are opt-in"
    );
    assert!(!vlite::Database::ann_snapshot_path(&path).exists());
    // The datafile was closed cleanly and is reusable.
    vlite::Database::open(&path).unwrap().close().unwrap();
}

#[test]
fn ann_snapshot_is_opt_in_loads_after_graceful_restart_and_rejects_stale() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("snap.vlite");
    let sidecar = vlite::Database::ann_snapshot_path(&path);
    let flag = ["--ann-snapshot"];

    // Run 1: nothing to load; build an index; graceful stop saves it.
    let mut launched = launch(&path, &flag);
    let load = launched.event("ann_snapshot_load");
    assert_eq!(load["outcome"], "missing", "{load}");
    let operations = (0..200)
        .map(|i| json!({"op":"put","id":i.to_string(),"vector":[i as f32, (i % 7) as f32]}))
        .collect::<Vec<_>>();
    assert_eq!(
        launched
            .server
            .request("POST", "/v1/batches", json!({"operations":operations}))
            .0,
        204
    );
    let (status, built) = launched.server.request("POST", "/v1/index/hnsw", json!({}));
    assert!((200..300).contains(&status), "{status} {built}");
    let query = json!({"vector":[17,3],"k":5,"mode":"approximate_ready"});
    let (status, before) = launched.server.request("POST", "/v1/search", query.clone());
    assert_eq!(status, 200, "{before}");
    assert!(launched.stop().success());
    let save = launched.event("ann_snapshot_save");
    assert_eq!(save["outcome"], "saved", "{save}");
    assert!(save["indexes"].as_u64().unwrap() >= 1, "{save}");
    assert!(sidecar.exists());

    // Run 2: the sidecar loads and serves identical ready results without a build.
    let launched = launch(&path, &flag);
    let load = launched.event("ann_snapshot_load");
    assert_eq!(load["outcome"], "loaded", "{load}");
    assert!(load["indexes"].as_u64().unwrap() >= 1, "{load}");
    assert!(load["duration_ms"].is_u64());
    let (status, after) = launched.server.request("POST", "/v1/search", query.clone());
    assert_eq!(status, 200, "{after}");
    assert_eq!(after["hits"], before["hits"]);

    // A committed write followed by a kill (no save) leaves the sidecar stale.
    assert_eq!(
        launched
            .server
            .request("PUT", "/v1/vectors/5000", json!({"vector":[1,1]}))
            .0,
        204
    );
    drop(launched);
    let launched = launch(&path, &flag);
    let load = launched.event("ann_snapshot_load");
    assert_eq!(load["outcome"], "stale", "{load}");
    assert!(
        load["reason"].as_str().unwrap().starts_with("stale"),
        "{load}"
    );
    assert_eq!(
        launched
            .server
            .request("POST", "/v1/search", query.clone())
            .0,
        409,
        "a rejected sidecar installs nothing"
    );
    drop(launched);

    // Without the flag: no sidecar is read, written or reported.
    let before = std::fs::read(&sidecar).unwrap();
    let mut launched = launch(&path, &[]);
    assert_eq!(launched.server.request("GET", "/ready", Value::Null).0, 200);
    assert!(launched.stop().success());
    assert!(
        !launched
            .rest_of_stderr()
            .iter()
            .any(|line| line.contains("ann_snapshot")),
    );
    assert_eq!(std::fs::read(&sidecar).unwrap(), before);
    let other = directory.path().join("plain.vlite");
    let mut launched = launch(&other, &[]);
    assert!(launched.stop().success());
    assert!(!vlite::Database::ann_snapshot_path(&other).exists());
}

#[cfg(feature = "grpc")]
#[test]
fn grpc_listener_serves_native_tls_from_pem_files() {
    use rcgen::{BasicConstraints, CertificateParams, CertifiedIssuer, IsCa, KeyPair};
    use tonic::transport::{Certificate, Channel, ClientTlsConfig};
    use tonic_health::pb::{HealthCheckRequest, health_client::HealthClient};

    let directory = tempfile::tempdir().unwrap();
    let mut ca_params = CertificateParams::new(Vec::<String>::new()).unwrap();
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    let ca = CertifiedIssuer::self_signed(ca_params, KeyPair::generate().unwrap()).unwrap();
    let key = KeyPair::generate().unwrap();
    let cert = CertificateParams::new(vec!["localhost".to_owned()])
        .unwrap()
        .signed_by(&key, &ca)
        .unwrap();
    let cert_path = directory.path().join("server.pem");
    let key_path = directory.path().join("server.key");
    std::fs::write(&cert_path, cert.pem()).unwrap();
    std::fs::write(&key_path, key.serialize_pem()).unwrap();

    let path = directory.path().join("tls.vlite");
    let mut launched = launch(
        &path,
        &[
            "--grpc-bind",
            "127.0.0.1:0",
            "--grpc-tls-cert",
            cert_path.to_str().unwrap(),
            "--grpc-tls-key",
            key_path.to_str().unwrap(),
        ],
    );
    let port = launched
        .grpc
        .clone()
        .unwrap()
        .rsplit(':')
        .next()
        .unwrap()
        .to_owned();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let tls = ClientTlsConfig::new()
            .ca_certificate(Certificate::from_pem(ca.pem()))
            .domain_name("localhost");
        let channel = Channel::from_shared(format!("https://localhost:{port}"))
            .unwrap()
            .tls_config(tls)
            .unwrap()
            .connect()
            .await
            .unwrap();
        let status = HealthClient::new(channel)
            .check(HealthCheckRequest {
                service: "vlite.v1.VLite".into(),
            })
            .await
            .unwrap()
            .into_inner()
            .status;
        assert_eq!(status, 1, "SERVING");
    });
    // Drop the client runtime so its connection closes during the drain.
    drop(runtime);
    let exit = launched.stop();
    assert!(exit.success(), "{exit:?}: {:?}", launched.rest_of_stderr());
}
