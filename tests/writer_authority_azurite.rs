//! Local TEST-007/014 evidence, not ACA or production fencing qualification.
#![cfg(feature = "server")]

use base64::Engine as _;
use reqwest::blocking::Client;
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader},
    path::Path,
    process::{Child, Command, Stdio},
    sync::mpsc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use vlite::{Database, FormatVersion};

const ACCOUNT: &str = "devstoreaccount1";
// Published emulator development key, not a production credential.
const DEVELOPMENT_KEY: &str =
    "Eby8vdM02xNOcqFlqUwJPLlmEtlCDXJ1OUzFT50uSRZ6IFsuFq2UVErCz4I6tq/K1SZFPTOtr/KBHBeksoGMGw==";
const RECHECK_MS: u64 = 500;

fn client() -> Client {
    Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap()
}

fn http_date() -> String {
    const DAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let days = (seconds / 86_400) as i64;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    let time = seconds % 86_400;
    format!(
        "{}, {day:02} {} {year:04} {:02}:{:02}:{:02} GMT",
        DAYS[(days % 7) as usize],
        MONTHS[(month - 1) as usize],
        time / 3600,
        time / 60 % 60,
        time % 60
    )
}

struct Containers {
    endpoint: String,
    key: Vec<u8>,
    names: [String; 2],
}

impl Containers {
    fn request(&self, method: reqwest::Method, name: &str) -> u16 {
        let date = http_date();
        let version = "2021-08-06";
        let path = format!("/{ACCOUNT}/{name}");
        let canonical = format!(
            "{method}\n\n\n\n\n\n\n\n\n\n\n\nx-ms-date:{date}\nx-ms-version:{version}\n/{ACCOUNT}{path}\nrestype:container"
        );
        let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, &self.key);
        let signature = base64::engine::general_purpose::STANDARD
            .encode(ring::hmac::sign(&key, canonical.as_bytes()).as_ref());
        client()
            .request(
                method,
                format!("{}/{name}?restype=container", self.endpoint),
            )
            .header("x-ms-date", date)
            .header("x-ms-version", version)
            .header("Authorization", format!("SharedKey {ACCOUNT}:{signature}"))
            .send()
            .unwrap()
            .status()
            .as_u16()
    }
}

impl Drop for Containers {
    fn drop(&mut self) {
        for name in &self.names {
            match std::panic::catch_unwind(|| self.request(reqwest::Method::DELETE, name)) {
                Ok(202 | 404) => {}
                Ok(status) => eprintln!("Azurite container cleanup returned {status}"),
                Err(_) => eprintln!("Azurite container cleanup failed"),
            }
        }
    }
}

fn command() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_vlite-server"));
    for name in [
        "VLITE_AUTH_TOKEN",
        "VLITE_AUTH_TOKEN_FILE",
        "VLITE_READ_ONLY_TOKEN",
        "VLITE_READ_ONLY_TOKEN_FILE",
        "VLITE_ADMIN_DIR",
        "VLITE_SERVE_MODE",
        "VLITE_PROVIDERS_FILE",
        "VLITE_MEMORY_BINDINGS_FILE",
        "VLITE_CLOUD_BACKUP_SHARED_KEY_FILE",
    ] {
        command.env_remove(name);
    }
    command
}

fn operator(verb: &[&str], target: &Path, key: &Path, extra: &[&str]) -> Value {
    let output = command()
        .args(verb)
        .arg("--target-file")
        .arg(target)
        .arg("--shared-key-file")
        .arg(key)
        .args(extra)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn writer_command(database: &Path, target: &Path, key: &Path) -> Command {
    let mut command = command();
    command
        .arg(database)
        .args([
            "--serve-mode",
            "writer",
            "--insecure-local",
            "--durability",
            "full",
        ])
        .args(["--bind", "127.0.0.1:0", "--authority-recheck-ms", "500"])
        .arg("--authority-target-file")
        .arg(target)
        .arg("--authority-shared-key-file")
        .arg(key);
    command
}

struct Server {
    child: Child,
    url: String,
}

impl Server {
    fn start(database: &Path, target: &Path, key: &Path) -> Self {
        let mut child = writer_command(database, target, key)
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let mut line = String::new();
            BufReader::new(stdout).read_line(&mut line).unwrap();
            let _ = sender.send(line);
        });
        let mut server = Self {
            child,
            url: String::new(),
        };
        let line = receiver
            .recv_timeout(Duration::from_secs(30))
            .expect("writer startup timed out");
        server.url = line
            .trim()
            .strip_prefix("LISTENING ")
            .unwrap_or_else(|| panic!("writer startup failed: {line}"))
            .to_owned();
        server
    }

    fn write(&self, epoch: u64) {
        let response = client()
            .put(format!("{}/v1/vectors/{epoch}", self.url))
            .json(&json!({"vector":[1.0, 2.0]}))
            .send()
            .unwrap();
        assert!(response.status().is_success(), "{response:?}");
        assert_eq!(
            response.headers()["x-vlite-epoch"].to_str().unwrap(),
            epoch.to_string()
        );
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

#[test]
#[ignore = "needs a local Azurite Blob endpoint in VLITE_AZURITE_BLOB_ENDPOINT"]
fn exclusive_startup_claim_and_idle_fenced_shutdown_across_failover() {
    let Ok(endpoint) = std::env::var("VLITE_AZURITE_BLOB_ENDPOINT") else {
        eprintln!("skipping: VLITE_AZURITE_BLOB_ENDPOINT is unset");
        return;
    };
    let endpoint = endpoint.trim_end_matches('/').to_owned();
    assert!(endpoint.ends_with(&format!("/{ACCOUNT}")));
    let key_text =
        std::env::var("VLITE_AZURITE_ACCOUNT_KEY").unwrap_or_else(|_| DEVELOPMENT_KEY.into());
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let containers = Containers {
        endpoint: endpoint.clone(),
        key: base64::engine::general_purpose::STANDARD
            .decode(key_text.trim())
            .unwrap(),
        names: [
            format!("vlite-authority-{unique:x}-data"),
            format!("vlite-authority-{unique:x}-coord"),
        ],
    };
    for name in &containers.names {
        assert_eq!(containers.request(reqwest::Method::PUT, name), 201);
    }
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("writer.vlite");
    let db = Database::create_with_format(&database, 2, FormatVersion::V4).unwrap();
    let lineage = format!("{:016x}", db.lineage().unwrap());
    db.close().unwrap();
    let rival = directory.path().join("rival.vlite");
    std::fs::copy(&database, &rival).unwrap();
    let target = directory.path().join("target.json");
    std::fs::write(
        &target,
        json!({
            "account":ACCOUNT, "container":containers.names[0],
            "coordination_container":containers.names[1], "prefix":"e2e/authority/",
            "endpoint":endpoint, "auth":"emulator_shared_key"
        })
        .to_string(),
    )
    .unwrap();
    let key = directory.path().join("emulator.key");
    std::fs::write(&key, key_text.trim()).unwrap();
    let initialized = operator(
        &["authority", "init"],
        &target,
        &key,
        &[
            "--installation-id",
            "test-installation",
            "--scope-id",
            "test-scope",
            "--lineage",
            &lineage,
        ],
    );
    assert_eq!(initialized["epoch"], 1);
    assert_eq!(initialized["writer_present"], false);
    let mut writer = Server::start(&database, &target, &key);
    writer.write(1);
    let claimed = operator(&["authority", "show"], &target, &key, &[]);
    assert_eq!(claimed["epoch"], 1);
    assert_eq!(claimed["writer_present"], true);
    assert!(claimed.get("writer_id").is_none());
    let second = writer_command(&rival, &target, &key).output().unwrap();
    assert!(!second.status.success());
    assert!(second.stdout.is_empty());
    assert!(
        String::from_utf8(second.stderr)
            .unwrap()
            .contains("writer_authority_claimed: ")
    );
    let report = operator(&["failover", "report"], &target, &key, &[]);
    let report = &report["report"];
    // Test-only simulated infrastructure acknowledgement; a real operator
    // must stop all old replicas before asserting zero.
    let bumped = operator(
        &["failover", "execute"],
        &target,
        &key,
        &[
            "--ack-report-id",
            report["report_id"].as_str().unwrap(),
            "--expected-epoch",
            &report["epoch"].to_string(),
            "--expected-revision",
            report["revision"].as_str().unwrap(),
            "--old-replicas",
            "0",
        ],
    );
    assert_eq!(bumped["result"]["record"]["epoch"], 2);
    let deadline = Instant::now() + Duration::from_millis(RECHECK_MS) + Duration::from_secs(5);
    loop {
        if let Some(status) = writer.child.try_wait().unwrap() {
            assert_eq!(status.code(), Some(1));
            break;
        }
        assert!(
            Instant::now() < deadline,
            "idle former writer did not self-stop"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let replacement = Server::start(&database, &target, &key);
    replacement.write(2);
}
