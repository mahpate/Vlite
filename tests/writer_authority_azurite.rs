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
        Self::spawn(writer_command(database, target, key))
    }

    fn spawn(mut command: Command) -> Self {
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

#[test]
#[ignore = "needs a local Azurite Blob endpoint in VLITE_AZURITE_BLOB_ENDPOINT"]
fn runtime_archive_advances_authority_and_failover_restores_the_base() {
    let Ok(endpoint) = std::env::var("VLITE_AZURITE_BLOB_ENDPOINT") else {
        eprintln!("skipping: VLITE_AZURITE_BLOB_ENDPOINT is unset");
        return;
    };
    for existing in [false, true] {
        let endpoint = endpoint.trim_end_matches('/').to_owned();
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
                format!("vlite-runtime-{unique:x}-data"),
                format!("vlite-runtime-{unique:x}-coord"),
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
        let target_value = json!({
            "account":ACCOUNT, "container":containers.names[0],
            "coordination_container":containers.names[1], "prefix":"e2e/runtime/",
            "endpoint":endpoint, "auth":"emulator_shared_key"
        });
        let target = directory.path().join("target.json");
        std::fs::write(&target, target_value.to_string()).unwrap();
        let key = directory.path().join("emulator.key");
        std::fs::write(&key, key_text.trim()).unwrap();
        let admin = directory.path().join("admin");
        let mut initial = command();
        initial
            .arg(&database)
            .args(["--insecure-local", "--bind", "127.0.0.1:0"])
            .env("VLITE_ADMIN_DIR", &admin)
            .env("VLITE_CLOUD_BACKUP_SHARED_KEY_FILE", &key);
        let initial = Server::spawn(initial);
        let configured = client()
            .put(format!("{}/v1/admin/cloud-backup", initial.url))
            .json(
                &json!({"target":target_value,"enabled":true,"interval_secs":604800,
            "drill_every":0,"archive":{"enabled":true,"interval_secs":60}}),
            )
            .send()
            .unwrap();
        assert!(configured.status().is_success(), "{configured:?}");
        let configured: Value = configured.json().unwrap();
        let installation = configured["installation_id"].as_str().unwrap();
        drop(initial);
        operator(
            &["authority", "init"],
            &target,
            &key,
            &[
                "--installation-id",
                installation,
                "--scope-id",
                "runtime",
                "--lineage",
                &lineage,
            ],
        );
        let mut start = writer_command(&database, &target, &key);
        start
            .env("VLITE_ADMIN_DIR", &admin)
            .env("VLITE_CLOUD_BACKUP_SHARED_KEY_FILE", &key);
        let replacement_path = directory.path().join("replacement.vlite");
        if existing {
            std::fs::copy(&database, &replacement_path).unwrap();
        }
        let mut writer = Server::spawn(start);
        let inserted = client()
            .put(format!("{}/v1/vectors/2", writer.url))
            .json(&json!({"vector":[2.0, 3.0]}))
            .send()
            .unwrap();
        assert!(inserted.status().is_success(), "{inserted:?}");
        let published = client()
            .post(format!("{}/v1/admin/cloud-backup/run", writer.url))
            .send()
            .unwrap();
        assert_eq!(published.status().as_u16(), 201, "{published:?}");
        let published: Value = published.json().unwrap();
        let shown = operator(&["authority", "show"], &target, &key, &[]);
        assert_eq!(shown["head"]["set_id"], published["set_id"]);
        assert_eq!(shown["head"]["epoch"], 1);
        let baseline_sequence = shown["head"]["sequence"].as_u64().unwrap();
        let report = operator(&["failover", "report"], &target, &key, &[]);
        assert_eq!(report["report"]["archived"], shown["head"]);
        writer.write(1);
        let deadline = Instant::now() + Duration::from_secs(100);
        loop {
            let status: Value = client()
                .get(format!("{}/v1/admin/cloud-backup", writer.url))
                .send()
                .unwrap()
                .json()
                .unwrap();
            if status["status"]["archive"]["last_checkpoint_sequence"]
                .as_u64()
                .is_some_and(|sequence| sequence > baseline_sequence)
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "archive did not advance: {status}"
            );
            std::thread::sleep(Duration::from_millis(250));
        }
        let shown = operator(&["authority", "show"], &target, &key, &[]);
        assert!(shown["head"]["sequence"].as_u64().unwrap() > baseline_sequence);
        assert_eq!(shown["head"]["index_sha256"].as_str().unwrap().len(), 64);
        let report = operator(&["failover", "report"], &target, &key, &[]);
        assert_eq!(report["report"]["archived"], shown["head"]);
        let report = &report["report"];
        let transfer = operator(
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
        assert_eq!(transfer["result"]["record"]["base"], shown["head"]);
        let deadline = Instant::now() + Duration::from_secs(6);
        loop {
            if let Some(status) = writer.child.try_wait().unwrap() {
                assert_eq!(status.code(), Some(1));
                break;
            }
            assert!(Instant::now() < deadline, "former writer did not stop");
            std::thread::sleep(Duration::from_millis(20));
        }
        let mut start = writer_command(&replacement_path, &target, &key);
        start
            .env("VLITE_ADMIN_DIR", &admin)
            .env("VLITE_CLOUD_BACKUP_SHARED_KEY_FILE", &key);
        let replacement = Server::spawn(start);
        assert_eq!(
            client()
                .get(format!("{}/ready", replacement.url))
                .send()
                .unwrap()
                .status()
                .as_u16(),
            200
        );
        for id in [1, 2] {
            assert_eq!(
                client()
                    .get(format!("{}/v1/vectors/{id}", replacement.url))
                    .send()
                    .unwrap()
                    .status()
                    .as_u16(),
                200
            );
        }
        assert_eq!(
            directory
                .path()
                .join("replacement.vlite.superseded-epoch1")
                .exists(),
            existing
        );
        let shown = operator(&["authority", "show"], &target, &key, &[]);
        assert_eq!(shown["epoch"], 2);
        assert_eq!(shown["writer_present"], true);
        let published = client()
            .post(format!("{}/v1/admin/cloud-backup/run", replacement.url))
            .send()
            .unwrap();
        assert_eq!(published.status().as_u16(), 201, "{published:?}");
        let published: Value = published.json().unwrap();
        let shown = operator(&["authority", "show"], &target, &key, &[]);
        assert_eq!(shown["head"]["epoch"], 2);
        assert_eq!(shown["head"]["set_id"], published["set_id"]);
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

struct ArchiveWriter {
    _containers: Containers,
    directory: tempfile::TempDir,
    database: std::path::PathBuf,
    target: std::path::PathBuf,
    key: std::path::PathBuf,
    policy: std::path::PathBuf,
}

impl ArchiveWriter {
    fn new() -> Self {
        let endpoint = std::env::var("VLITE_AZURITE_BLOB_ENDPOINT").expect("Azurite endpoint");
        let key_text =
            std::env::var("VLITE_AZURITE_ACCOUNT_KEY").unwrap_or_else(|_| DEVELOPMENT_KEY.into());
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let containers = Containers {
            endpoint: endpoint.trim_end_matches('/').into(),
            key: base64::engine::general_purpose::STANDARD
                .decode(key_text.trim())
                .unwrap(),
            names: [
                format!("vlite-policy-{unique:x}-data"),
                format!("vlite-policy-{unique:x}-coord"),
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
        let target_value = json!({
            "account":ACCOUNT,"container":containers.names[0],
            "coordination_container":containers.names[1],"prefix":"e2e/policy/",
            "endpoint":containers.endpoint,"auth":"emulator_shared_key"
        });
        let target = directory.path().join("target.json");
        let key = directory.path().join("emulator.key");
        let policy = directory.path().join("policy.json");
        std::fs::write(&target, target_value.to_string()).unwrap();
        std::fs::write(&key, key_text.trim()).unwrap();
        std::fs::write(
            &policy,
            json!({
                "target":target_value,"enabled":true,"interval_secs":604800,
                "drill_every":0,"archive":{"enabled":true,"interval_secs":60}
            })
            .to_string(),
        )
        .unwrap();
        operator(
            &["authority", "init"],
            &target,
            &key,
            &[
                "--installation-id",
                "0123456789abcdef",
                "--scope-id",
                "policy",
                "--lineage",
                &lineage,
            ],
        );
        Self {
            _containers: containers,
            directory,
            database,
            target,
            key,
            policy,
        }
    }

    fn start_command(&self) -> Command {
        let mut start = writer_command(&self.database, &self.target, &self.key);
        start
            .arg("--archive-policy-file")
            .arg(&self.policy)
            .env("VLITE_CLOUD_BACKUP_SHARED_KEY_FILE", &self.key);
        start
    }

    fn state(&self) -> Value {
        serde_json::from_slice(
            &std::fs::read(
                self.directory
                    .path()
                    .join("writer.vlite.cloud-backup")
                    .join("state.json"),
            )
            .unwrap(),
        )
        .unwrap()
    }

    fn wait_head(&self, sequence: u64, epoch: u64) -> Value {
        let deadline = Instant::now() + Duration::from_secs(100);
        loop {
            let shown = operator(&["authority", "show"], &self.target, &self.key, &[]);
            if shown["head"]["epoch"] == epoch
                && shown["head"]["sequence"]
                    .as_u64()
                    .is_some_and(|s| s > sequence)
            {
                return shown;
            }
            assert!(
                Instant::now() < deadline,
                "archive did not advance: {shown}; {}",
                self.state()
            );
            std::thread::sleep(Duration::from_millis(250));
        }
    }
}

#[test]
#[ignore = "needs a local Azurite Blob endpoint in VLITE_AZURITE_BLOB_ENDPOINT"]
fn startup_policy_publishes_initial_base_and_restart_uses_the_existing_runtime() {
    let env = ArchiveWriter::new();
    let writer = Server::spawn(env.start_command());
    let initial = operator(&["authority", "show"], &env.target, &env.key, &[]);
    let base_sequence = initial["head"]["sequence"].as_u64().expect("initial base");
    assert_eq!(env.state()["status"]["published"], 1);
    assert_eq!(
        std::fs::read_to_string(
            env.directory
                .path()
                .join("writer.vlite.cloud-backup")
                .join("installation-id")
        )
        .unwrap(),
        "0123456789abcdef"
    );
    for id in 1..=3 {
        let response = client()
            .put(format!("{}/v1/vectors/{id}", writer.url))
            .json(&json!({"vector":[1.0,2.0]}))
            .send()
            .unwrap();
        assert!(response.status().is_success(), "{response:?}");
    }
    env.wait_head(base_sequence, 1);
    drop(writer);
    let report = operator(&["failover", "report"], &env.target, &env.key, &[]);
    let report = &report["report"];
    operator(
        &["failover", "execute"],
        &env.target,
        &env.key,
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
    let replacement = Server::spawn(env.start_command());
    assert_eq!(
        env.state()["status"]["published"],
        1,
        "restart forced another initial base"
    );
    replacement.write(2);
    env.wait_head(base_sequence + 3, 2);
    assert_eq!(
        env.state()["status"]["published"],
        2,
        "new epoch needs one scheduled base"
    );
}

#[test]
#[ignore = "needs a local Azurite Blob endpoint in VLITE_AZURITE_BLOB_ENDPOINT"]
fn startup_policy_refuses_target_and_persisted_installation_mismatches() {
    let env = ArchiveWriter::new();
    let original = std::fs::read(&env.policy).unwrap();
    let mut policy: Value = serde_json::from_slice(&original).unwrap();
    policy["target"]["prefix"] = json!("wrong/");
    std::fs::write(&env.policy, policy.to_string()).unwrap();
    let output = env.start_command().output().unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("writer_authority_target_mismatch"));
    std::fs::write(&env.policy, original).unwrap();
    let runtime = env.directory.path().join("writer.vlite.cloud-backup");
    std::fs::create_dir(&runtime).unwrap();
    std::fs::write(runtime.join("installation-id"), "fedcba9876543210").unwrap();
    let output = env.start_command().output().unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("writer_authority_installation_mismatch")
    );
}

#[test]
fn nonpersistent_authority_startup_refuses_before_target_access() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("never-created.vlite");
    let output = command()
        .arg(&database)
        .args([
            "--serve-mode",
            "writer",
            "--insecure-local",
            "--create-if-missing",
            "2",
            "--durability",
            "none",
            "--authority-target-file",
        ])
        .arg(directory.path().join("missing-target.json"))
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).unwrap();
    let error: Value = serde_json::from_str(stderr.lines().last().unwrap()).unwrap();
    assert!(
        error["error"]
            .as_str()
            .unwrap()
            .starts_with("writer_authority_base_unverified:"),
        "{stderr}"
    );
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
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
