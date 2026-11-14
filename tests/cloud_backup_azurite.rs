//! End-to-end cloud backup and isolated restore against a local Azurite Blob
//! emulator. Ignored by default: it needs a running emulator, for example
//!
//! ```text
//! docker run -d --name vlite-azurite -p 127.0.0.1:10000:10000 \
//!   mcr.microsoft.com/azure-storage/azurite azurite-blob --blobHost 0.0.0.0 \
//!   --skipApiVersionCheck --inMemoryPersistence
//! VLITE_AZURITE_BLOB_ENDPOINT=http://127.0.0.1:10000/devstoreaccount1 \
//!   cargo test --features server --test cloud_backup_azurite -- --ignored
//! ```
//!
//! Emulator evidence only: it does not qualify Azure managed identity, RBAC
//! or immutability policies.

use base64::Engine as _;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::TcpStream,
    path::Path,
    process::{Child, Command, Stdio},
    sync::mpsc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const ACCOUNT: &str = "devstoreaccount1";
/// The emulator's published development key (not a secret); override with
/// VLITE_AZURITE_ACCOUNT_KEY for an emulator started with another key.
const WELL_KNOWN_KEY: &str =
    "Eby8vdM02xNOcqFlqUwJPLlmEtlCDXJ1OUzFT50uSRZ6IFsuFq2UVErCz4I6tq/K1SZFPTOtr/KBHBeksoGMGw==";
const TOKEN: &str = "azurite_e2e_0123456789_ABCDEFGHIJKLMNOPQRSTUVWX";
const RESTORED_TOKEN: &str = "azurite_e2e_restored_0123456789_ABCDEFGHIJKLM";
const OWNER_PASSWORD: &str = "owner password for the source 42";
const RESTORER_PASSWORD: &str = "restorer password for the copy 42";

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

    fn call(&self, method: &str, path: &str, body: Value, extra: &str) -> (u16, String, Value) {
        let body = if body.is_null() {
            String::new()
        } else {
            body.to_string()
        };
        let mut stream = TcpStream::connect(&self.address).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(300)))
            .unwrap();
        write!(
            stream,
            "{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n{extra}Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        let (head, body) = response.split_once("\r\n\r\n").unwrap();
        let status = head.split_whitespace().nth(1).unwrap().parse().unwrap();
        let value = serde_json::from_str(body).unwrap_or(Value::Null);
        (status, head.to_owned(), value)
    }

    fn admin(&self, method: &str, path: &str, body: Value) -> (u16, Value) {
        let auth = format!("Authorization: Bearer {}\r\n", self.token);
        let (status, _, value) = self.call(method, path, body, &auth);
        (status, value)
    }

    fn login(&self, username: &str, password: &str) -> (u16, Value) {
        let (status, _, value) = self.call(
            "POST",
            "/v1/auth/login",
            json!({"username":username,"password":password}),
            "X-VLite-Request: 1\r\n",
        );
        (status, value)
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

fn cli(args: &[&std::ffi::OsStr]) -> (bool, Value, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_vlite-server"))
        .args(args)
        .env_remove("VLITE_CLOUD_BACKUP_SHARED_KEY_FILE")
        .output()
        .unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();
    let value = serde_json::from_str(stdout.trim()).unwrap_or(Value::Null);
    (output.status.success(), value, stderr)
}

#[test]
#[ignore = "needs a local Azurite Blob endpoint in VLITE_AZURITE_BLOB_ENDPOINT"]
fn azurite_backup_restore_and_isolated_rekey_recovery() {
    let endpoint = std::env::var("VLITE_AZURITE_BLOB_ENDPOINT")
        .expect("set VLITE_AZURITE_BLOB_ENDPOINT, e.g. http://127.0.0.1:10000/devstoreaccount1");
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
    let data = format!("vlite-data-{unique}");
    let coordination = format!("vlite-coord-{unique}");
    for name in [&data, &coordination] {
        assert_eq!(container(&endpoint, &key, "PUT", name), 201, "{name}");
    }

    let directory = tempfile::tempdir_in(".").unwrap();
    let root = directory.path();
    let key_file = root.join("emulator.key");
    std::fs::write(&key_file, format!("{}\n", key_text.trim())).unwrap();
    let owner_password = root.join("owner.password");
    std::fs::write(&owner_password, OWNER_PASSWORD).unwrap();
    let target = json!({"account":ACCOUNT,"container":data,
        "coordination_container":coordination,"prefix":"e2e/source/",
        "endpoint":endpoint,"auth":"emulator_shared_key"});

    // Source installation: data, a policy, a published set, drill, pins and
    // the in-place (route) restore.
    let source_db = root.join("source.vlite");
    let source_admin = root.join("source-admin");
    std::fs::create_dir(&source_admin).unwrap();
    let username = std::ffi::OsString::from("owner");
    let source = Server::start(
        &source_db,
        &source_admin,
        TOKEN,
        &[
            ("VLITE_ADMIN_USERNAME", Path::new(&username)),
            ("VLITE_ADMIN_PASSWORD_FILE", &owner_password),
            ("VLITE_CLOUD_BACKUP_SHARED_KEY_FILE", &key_file),
        ],
    );
    for id in 0..8 {
        let (status, body) = source.admin(
            "PUT",
            &format!("/v1/vectors/{id}"),
            json!({"vector":[id, 1]}),
        );
        assert_eq!(status, 204, "{body}");
    }
    // Large payloads push the database past the 4 MiB single-put limit, so
    // the set's database object is uploaded as staged blocks (no per-block
    // ETag) and committed with a conditional block list.
    for batch in 0..3 {
        let operations = (0..5)
            .map(|i| {
                let id = 100 + batch * 5 + i;
                let payload = vec![(id % 251) as u8; 400 * 1024];
                json!({"op":"put","id":id.to_string(),"vector":[id, 2],
                    "payload_base64":base64::engine::general_purpose::STANDARD.encode(payload)})
            })
            .collect::<Vec<_>>();
        let (status, body) =
            source.admin("POST", "/v1/batches", json!({ "operations": operations }));
        assert_eq!(status, 204, "{body}");
    }
    let (status, body) = source.admin(
        "PUT",
        "/v1/admin/cloud-backup",
        json!({"target":target,"enabled":false}),
    );
    assert_eq!(status, 200, "{body}");
    let (status, run) = source.admin("POST", "/v1/admin/cloud-backup/run", Value::Null);
    assert_eq!(status, 201, "{run}");
    let set_id = run["set_id"].as_str().expect("published set id").to_owned();
    let (status, sets) = source.admin("GET", "/v1/admin/cloud-backup/sets", Value::Null);
    assert_eq!(status, 200, "{sets}");
    assert_eq!(sets["sets"][0]["set_id"], set_id, "{sets}");
    assert_eq!(sets["sets"][0]["record"]["state"], "published", "{sets}");
    assert_eq!(sets["sets"][0]["error"], Value::Null, "{sets}");
    let (status, drill) = source.admin(
        "POST",
        &format!("/v1/admin/cloud-backup/sets/{set_id}/drill"),
        Value::Null,
    );
    assert_eq!(status, 200, "{drill}");
    assert_eq!(drill["ok"], true, "{drill}");
    assert_eq!(drill["recorded"], true, "{drill}");
    let (status, pin) = source.admin(
        "POST",
        &format!("/v1/admin/cloud-backup/sets/{set_id}/pins"),
        json!({"reason":"e2e legal hold"}),
    );
    assert_eq!(status, 201, "{pin}");
    let pin_id = pin["pin"]["id"].as_str().unwrap().to_owned();
    let (status, body) = source.admin(
        "DELETE",
        &format!("/v1/admin/cloud-backup/sets/{set_id}/pins/{pin_id}"),
        Value::Null,
    );
    assert_eq!(status, 200, "{body}");
    let (status, restored) = source.admin(
        "POST",
        &format!("/v1/admin/cloud-backup/sets/{set_id}/restore"),
        json!({}),
    );
    assert_eq!(status, 201, "{restored}");
    assert_eq!(restored["rekey_only"], true);
    assert_eq!(restored["pin_released"], true);
    assert!(
        source_admin
            .join(restored["name"].as_str().unwrap())
            .is_file()
    );
    let (status, seed) = source.admin(
        "POST",
        &format!("/v1/admin/cloud-backup/sets/{set_id}/restore"),
        json!({"mode":"seed"}),
    );
    assert_eq!(status, 409, "{seed}");
    assert_eq!(seed["error"]["code"], "cloud_seed_mode_deferred", "{seed}");
    let (status, collected) = source.admin("POST", "/v1/admin/cloud-backup/collect", Value::Null);
    assert_eq!(status, 200, "{collected}");
    let (status, view) = source.admin("GET", "/v1/admin/cloud-backup", Value::Null);
    assert_eq!(status, 200, "{view}");
    assert!(!view.to_string().contains(key_text.trim()), "{view}");
    assert!(
        source_admin
            .join("cloud-backup")
            .join("policy.json")
            .is_file()
    );
    let source_lineage = restored["source_lineage"].clone();
    drop(source);

    // Isolated, offline rekey-only restore from the cloud into a new file.
    let target_file = root.join("target.json");
    std::fs::write(&target_file, target.to_string()).unwrap();
    let (ok, listing, stderr) = cli(&[
        "cloud-sets".as_ref(),
        "--target-file".as_ref(),
        target_file.as_os_str(),
        "--shared-key-file".as_ref(),
        key_file.as_os_str(),
    ]);
    assert!(ok, "{stderr}");
    assert_eq!(listing["sets"][0]["set_id"], set_id, "{listing}");
    let restorer_password = root.join("restorer.password");
    std::fs::write(&restorer_password, RESTORER_PASSWORD).unwrap();
    let copy = root.join("recovered.vlite");
    let copy_admin = root.join("recovered-admin");
    let (ok, report, stderr) = cli(&[
        "cloud-restore".as_ref(),
        "--target-file".as_ref(),
        target_file.as_os_str(),
        "--shared-key-file".as_ref(),
        key_file.as_os_str(),
        "--set".as_ref(),
        set_id.as_ref(),
        "--database".as_ref(),
        copy.as_os_str(),
        "--admin-dir".as_ref(),
        copy_admin.as_os_str(),
        "--admin-username".as_ref(),
        "restorer".as_ref(),
        "--admin-password-file".as_ref(),
        restorer_password.as_os_str(),
    ]);
    assert!(ok, "{stderr}");
    assert_eq!(report["rekey_only"], true, "{report}");
    assert_eq!(report["pin_released"], true, "{report}");
    assert_eq!(report["source_lineage"], source_lineage, "{report}");
    assert_ne!(report["lineage"], report["source_lineage"], "{report}");
    assert!(!report.to_string().contains(RESTORER_PASSWORD));
    assert!(!copy_admin.join("credentials.json").exists());
    assert!(!copy_admin.join("webhooks.json").exists());
    assert!(!copy_admin.join("cloud-backup").exists());

    // The recovered copy starts on its own, with a new token: data is back,
    // only the fresh admin can sign in, the imported owner stays disabled.
    let recovered = Server::start(&copy, &copy_admin, RESTORED_TOKEN, &[]);
    let (status, record) = recovered.admin("GET", "/v1/vectors/3", Value::Null);
    assert_eq!(status, 200, "{record}");
    assert_eq!(record["vector"], json!([3.0, 1.0]));
    let (status, record) = recovered.admin("GET", "/v1/vectors/114", Value::Null);
    assert_eq!(status, 200, "{record}");
    let payload = base64::engine::general_purpose::STANDARD
        .decode(record["payload_base64"].as_str().unwrap())
        .unwrap();
    assert_eq!(payload, vec![114u8; 400 * 1024]);
    assert!(
        std::fs::metadata(&copy).unwrap().len() > 4 * 1024 * 1024,
        "the restored database exercised the staged-block path"
    );
    let (status, signed_in) = recovered.login("restorer", RESTORER_PASSWORD);
    assert_eq!(status, 200, "{signed_in}");
    let (status, refused) = recovered.login("owner", OWNER_PASSWORD);
    assert_eq!(
        status, 401,
        "the old password is not resurrected: {refused}"
    );
    let (status, users) = recovered.admin("GET", "/v1/admin/users", Value::Null);
    assert_eq!(status, 200, "{users}");
    let owner = users["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|user| user["username"] == "owner")
        .expect("the source account is kept for audit");
    assert_eq!(owner["disabled"], true, "{owner}");
    assert_eq!(owner["must_change_password"], true, "{owner}");
    let (status, view) = recovered.admin("GET", "/v1/admin/cloud-backup", Value::Null);
    assert_eq!(status, 200, "{view}");
    assert!(
        view["policy"].is_null(),
        "no cloud policy is restored: {view}"
    );
    drop(recovered);

    for name in [&data, &coordination] {
        container(&endpoint, &key, "DELETE", name);
    }
}
