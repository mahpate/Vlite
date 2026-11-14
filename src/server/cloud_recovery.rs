//! Offline, isolated cloud restore for operators (feature-backup-ha ITEM-007,
//! backup-only part): `vlite-server cloud-sets` and `vlite-server
//! cloud-restore`.
//!
//! The restore runs without a server. Every local precondition (target file,
//! paths, the fresh admin's name and password) is checked before any network
//! access. It then pins the published set, downloads and verifies it,
//! restores and probes a scratch copy (drill), restores the set to a new
//! database path with a new lineage and imports the recovery bundle rekey-only
//! into a new, empty admin directory: imported accounts are disabled with
//! unusable passwords, the fresh super admin is the only usable sign-in, no
//! credential, session or webhook is written. The pin is always released.
//!
//! Nothing is selected or activated: the operator starts the restored copy
//! explicitly. Only the lineage-changing rekey restore exists; the
//! identity-preserving seed mode belongs to the deferred HA work and is
//! refused.
use super::backup_policy::{SHARED_KEY_ENV, bundle_summary, inspection, lineage_hex};
use super::cloud_backup::{self as cloud, Sets, Target};
use super::*;
use crate::maintenance;
use std::io::Read;
use std::path::{Path as FilePath, PathBuf};

const MAX_TARGET_FILE: u64 = 64 * 1024;
const PIN_REASON: &str = "isolated-restore";
const PASSWORD_LABEL: &str = "--admin-password-file";

/// A refused or failed offline recovery command.
#[derive(Debug)]
pub struct RecoveryError {
    pub code: &'static str,
    pub message: String,
}

impl RecoveryError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    /// The same `{"error":{"code","message"}}` shape as the admin API.
    pub fn to_json(&self) -> Value {
        json!({"error":{"code":self.code,"message":self.message}})
    }
}

impl std::fmt::Display for RecoveryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for RecoveryError {}

impl From<ApiError> for RecoveryError {
    fn from(error: ApiError) -> Self {
        // The shared helpers hide internal details from API clients; there is
        // no server log behind this command, so say what the class means.
        let message = if error.code == "internal_error" {
            "a local I/O or internal error stopped the command (for example a full disk or a permission problem); nothing was selected or activated".into()
        } else {
            error.message
        };
        Self {
            code: error.code,
            message,
        }
    }
}

/// Which restore to run. Only `Rekey` is implemented.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RestoreMode {
    /// New lineage, imported access disabled, fresh super admin.
    Rekey,
    /// Identity-preserving standby seed: deferred HA scope, always refused.
    Seed,
}

impl std::str::FromStr for RestoreMode {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, String> {
        match text {
            "rekey" => Ok(Self::Rekey),
            "seed" => Ok(Self::Seed),
            _ => Err("--mode must be rekey or seed".into()),
        }
    }
}

/// An isolated restore request (`vlite-server cloud-restore`).
pub struct CloudRestore {
    /// JSON cloud target, the same shape as the policy's `target`.
    pub target_file: PathBuf,
    /// Emulator account key file; falls back to the process setting.
    pub shared_key_file: Option<PathBuf>,
    pub set_id: String,
    /// New database file; must not exist, its directory must.
    pub database: PathBuf,
    /// New admin directory: absent (created) or an empty directory.
    pub admin_directory: PathBuf,
    pub admin_username: String,
    pub admin_password_file: PathBuf,
    pub mode: RestoreMode,
}

/// Everything checked before the network is touched.
struct Plan {
    set_id: String,
    database: PathBuf,
    parent: PathBuf,
    admin: PathBuf,
    admin_parent: PathBuf,
    create_admin: bool,
    username: String,
    password: String,
}

fn refused(code: &'static str, message: impl Into<String>) -> RecoveryError {
    RecoveryError::new(code, message)
}

fn io_failure(what: &str, error: impl std::fmt::Display) -> RecoveryError {
    RecoveryError::new("restore_io_error", format!("{what}: {error}"))
}

fn read_target(path: &FilePath) -> Result<Target, RecoveryError> {
    let invalid = |message: String| refused("invalid_target", message);
    let meta = std::fs::metadata(path)
        .map_err(|e| invalid(format!("cannot read the target file: {e}")))?;
    if !meta.is_file() || meta.len() > MAX_TARGET_FILE {
        return Err(invalid(
            "the target file must be a regular file of at most 64 KiB".into(),
        ));
    }
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .and_then(|file| file.take(MAX_TARGET_FILE + 1).read_to_end(&mut bytes))
        .map_err(|e| invalid(format!("cannot read the target file: {e}")))?;
    let target: Target = serde_json::from_slice(&bytes)
        .map_err(|e| invalid(format!("the target file is not a valid target: {e}")))?;
    target.validate().map_err(invalid)?;
    Ok(target)
}

/// The emulator key: the explicit file, else the process setting, and only
/// for emulator auth (an explicit key with managed identity is refused by
/// the connector).
fn shared_key(
    target: &Target,
    explicit: Option<&FilePath>,
) -> Result<Option<Vec<u8>>, RecoveryError> {
    let path = match explicit {
        Some(path) => Some(path.to_path_buf()),
        None if target.auth == cloud::AuthMode::EmulatorSharedKey => {
            std::env::var_os(SHARED_KEY_ENV).map(PathBuf::from)
        }
        None => None,
    };
    path.map(|path| {
        cloud::azure::read_shared_key(&path).map_err(|m| refused("invalid_shared_key", m))
    })
    .transpose()
}

fn connect(target_file: &FilePath, key_file: Option<&FilePath>) -> Result<Sets, RecoveryError> {
    let target = read_target(target_file)?;
    let key = shared_key(&target, key_file)?;
    Ok(cloud::azure::connect(&target, key)?)
}

/// Lists the target's sets (`vlite-server cloud-sets`): read-only.
pub fn cloud_sets(
    target_file: &FilePath,
    shared_key_file: Option<&FilePath>,
) -> Result<Value, RecoveryError> {
    let sets = connect(target_file, shared_key_file)?;
    let owner = sets.owner()?;
    let listing = sets.list()?;
    let tombstoned: Vec<Value> = listing
        .tombstoned
        .iter()
        .map(|(id, objects)| json!({"set_id":id,"data_objects":objects}))
        .collect();
    Ok(json!({"owner_installation_id":owner,"sets":listing.sets,"tombstoned":tombstoned}))
}

/// Runs an isolated rekey-only restore (`vlite-server cloud-restore`).
pub fn cloud_restore(request: &CloudRestore) -> Result<Value, RecoveryError> {
    let plan = prepare(request)?;
    let sets = connect(&request.target_file, request.shared_key_file.as_deref())?;
    restore_with(&plan, &sets)
}

fn prepare(request: &CloudRestore) -> Result<Plan, RecoveryError> {
    if request.mode == RestoreMode::Seed {
        return Err(refused(
            "cloud_seed_mode_deferred",
            "the identity-preserving seed restore belongs to the deferred HA standby work; only --mode rekey (new lineage, rekey-only access) is supported",
        ));
    }
    if !cloud::valid_set_id(&request.set_id) {
        return Err(refused(
            "invalid_input",
            "--set must be a backup set ID such as 20250101T000000Z-0123456789abcdef",
        ));
    }
    read_target(&request.target_file)?;

    let database = request.database.clone();
    match std::fs::symlink_metadata(&database) {
        Ok(_) => {
            return Err(refused(
                "restore_destination_exists",
                format!(
                    "{} already exists; a restore never replaces a file",
                    database.display()
                ),
            ));
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(io_failure("cannot inspect --database", e)),
    }
    let parent = match database.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
        _ => PathBuf::from("."),
    };
    if !std::fs::metadata(&parent).is_ok_and(|m| m.is_dir()) {
        return Err(refused(
            "invalid_input",
            "the --database directory must exist",
        ));
    }

    let admin = request.admin_directory.clone();
    let admin_parent = match admin.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
        _ => PathBuf::from("."),
    };
    let create_admin = match std::fs::symlink_metadata(&admin) {
        Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink() => {
            let mut entries =
                std::fs::read_dir(&admin).map_err(|e| io_failure("cannot read --admin-dir", e))?;
            if entries.next().is_some() {
                return Err(refused(
                    "restore_admin_dir_not_empty",
                    "--admin-dir must be a new or empty directory; a restore never reuses an admin directory",
                ));
            }
            false
        }
        Ok(_) => {
            return Err(refused(
                "restore_admin_dir_not_empty",
                "--admin-dir must be a real directory",
            ));
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if !std::fs::metadata(&admin_parent).is_ok_and(|m| m.is_dir()) {
                return Err(refused(
                    "invalid_input",
                    "the --admin-dir parent directory must exist",
                ));
            }
            true
        }
        Err(e) => return Err(io_failure("cannot inspect --admin-dir", e)),
    };

    let (username, password) = users::fresh_admin(
        &request.admin_username,
        &request.admin_password_file,
        PASSWORD_LABEL,
    )?;
    Ok(Plan {
        set_id: request.set_id.clone(),
        database,
        parent,
        admin,
        admin_parent,
        create_admin,
        username,
        password,
    })
}

fn now() -> u64 {
    admin::now_ms() as u64
}

/// Makes a new directory entry durable (Unix; Windows has no directory sync).
fn sync_directory(dir: &FilePath) -> Result<(), RecoveryError> {
    #[cfg(unix)]
    std::fs::File::open(dir)
        .and_then(|d| d.sync_all())
        .map_err(|e| io_failure("cannot sync the --admin-dir parent", e))?;
    #[cfg(not(unix))]
    let _ = dir;
    Ok(())
}

fn restore_with(plan: &Plan, sets: &Sets) -> Result<Value, RecoveryError> {
    let id = &plan.set_id;
    let pin = sets.pin(id, PIN_REASON, now())?;
    let result = restore_pinned(plan, sets, &pin.id);
    let released = sets.unpin(id, &pin.id, now()).is_ok();
    let mut report = match result {
        Ok(report) => report,
        Err(mut error) => {
            if !released {
                error.message = format!(
                    "{}; the pin {} on set {id} could not be released: remove it with DELETE /v1/admin/cloud-backup/sets/{id}/pins/{}",
                    error.message, pin.id, pin.id
                );
            }
            return Err(error);
        }
    };
    report["pin_released"] = json!(released);
    if !released {
        report["pin_id"] = json!(pin.id);
        report["pin_warning"] = json!(format!(
            "the pin blocks retention of this set until removed with DELETE /v1/admin/cloud-backup/sets/{id}/pins/{}",
            pin.id
        ));
    }
    Ok(report)
}

fn restore_pinned(plan: &Plan, sets: &Sets, pin_id: &str) -> Result<Value, RecoveryError> {
    let id = &plan.set_id;
    // Same volume as the destination; removed with everything in it.
    let work = tempfile::Builder::new()
        .prefix(".vlite-cloud-restore-")
        .tempdir_in(&plan.parent)
        .map_err(|e| io_failure("cannot create the download directory", e))?;
    let fetched = sets.fetch(id, pin_id, work.path())?;
    let drill = maintenance::drill(&fetched.database, work.path()).map_err(|e| {
        refused(
            "cloud_drill_failed",
            format!("the downloaded set failed its drill: {e}"),
        )
    })?;

    // Only an empty directory is ever removed: never the operator's data.
    let cleanup_admin = || {
        if plan.create_admin {
            let _ = std::fs::remove_dir(&plan.admin);
        }
    };
    if plan.create_admin {
        std::fs::create_dir(&plan.admin).map_err(|e| io_failure("cannot create --admin-dir", e))?;
        if let Err(error) = sync_directory(&plan.admin_parent) {
            cleanup_admin();
            return Err(error);
        }
    }
    // Durable on return: the restore syncs the file and its directory.
    let restored = match maintenance::restore(&fetched.database, &plan.database) {
        Ok(restored) => restored,
        Err(e) => {
            cleanup_admin();
            return Err(io_failure("cannot restore the database", e));
        }
    };
    let undo = |error: RecoveryError| {
        let _ = std::fs::remove_file(&plan.database);
        cleanup_admin();
        error
    };
    let lineage = maintenance::inspect_detail(&plan.database)
        .map_err(|e| undo(io_failure("cannot inspect the restored database", e)))?
        .lineage;
    let imported = recovery_bundle::import(
        &fetched.bundle,
        &plan.admin,
        Some((&plan.username, &plan.password)),
    )
    .map_err(|e| {
        undo(refused(
            "restore_import_failed",
            format!("the recovery bundle could not be imported: {e}"),
        ))
    })?;

    Ok(json!({
        "mode": "rekey",
        "rekey_only": true,
        "set_id": id,
        "database": plan.database.display().to_string(),
        "admin_directory": plan.admin.display().to_string(),
        "inspection": inspection(&restored),
        "lineage": lineage_hex(lineage),
        "source_lineage": fetched.manifest.source_lineage,
        "source_installation_id": fetched.record.installation_id,
        "verified_bytes": fetched.verification.bytes,
        "crc32": fetched.verification.crc32,
        "drill": {"scratch_lineage": lineage_hex(drill.scratch_lineage),
            "collections": drill.collections, "probes": drill.probes},
        "bundle": bundle_summary(&fetched.bundle),
        "import": imported,
        "next": [
            "provision a new admin bearer token (VLITE_AUTH_TOKEN_FILE); old tokens and credentials are not restored",
            format!("start the restored copy explicitly: vlite-server {} with VLITE_ADMIN_DIR={}", plan.database.display(), plan.admin.display()),
            format!("sign in as {} and reset passwords before re-enabling imported accounts", plan.username),
            "re-issue the scoped credentials and re-create the webhook subscriptions listed under import",
            "configure cloud backup with a new prefix: the source prefix belongs to the source installation",
        ],
    }))
}

#[cfg(test)]
mod tests {
    use super::cloud::fake::{Fault, Memory, Op};
    use super::cloud::{Manifest, Publication, ROLE_BUNDLE, ROLE_DATABASE, SetState, Upload};
    use super::*;

    const INSTALL: &str = "0123456789abcdef";
    const PASSWORD: &str = "correct horse battery staple 42";

    struct Env {
        meta: Arc<Memory>,
        data: Arc<Memory>,
        sets: Sets,
        dir: tempfile::TempDir,
        set_id: String,
        source_lineage: Option<u64>,
    }

    fn config() -> recovery_bundle::Config {
        serde_json::from_value(json!({
            "tenants": [{"id": "c".repeat(32), "name": "Contoso", "max_vectors": 10}],
            "accounts": [
                {"username":"ops","role":"super_admin","disabled":false},
                {"username":"reader","role":"viewer","tenant_id":"c".repeat(32),"disabled":true}
            ],
            "credentials": [{"id":"abcdefghijkl","name":"ci","tenant_id":null,"state":"active"}],
            "webhook_subscriptions": 2,
            "secret_references": [{"name":"admin_token","configured":true,"action":"provision"}],
            "excluded": recovery_bundle::EXCLUDED,
        }))
        .unwrap()
    }

    fn setup() -> Env {
        let meta = Arc::new(Memory::default());
        let data = Arc::new(Memory::default());
        let sets = Sets::new(meta.clone(), data.clone(), false, "vlite/prod/".into());
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.vlite");
        let mut database = Database::create(&source, 3).unwrap();
        for id in 1..=4u64 {
            database
                .put(id, &[id as f32, 1.0, -0.5], format!("p{id}").as_bytes())
                .unwrap();
        }
        drop(database);
        let verification = maintenance::verify(&source).unwrap();
        let detail = maintenance::inspect_detail(&source).unwrap();
        let bundle = recovery_bundle::bundle(config(), INSTALL, detail.lineage, 1);
        let uploads = vec![
            Upload::file(ROLE_DATABASE, source).unwrap(),
            Upload::bytes(ROLE_BUNDLE, serde_json::to_vec(&bundle).unwrap()),
        ];
        let at = now();
        let set_id = cloud::new_set_id(at).unwrap();
        let manifest = Manifest::new(&set_id, INSTALL, at, &verification, &detail, &uploads);
        match sets.publish(&manifest, &uploads, at).unwrap() {
            Publication::Published(record) => assert_eq!(record.state, SetState::Published),
            Publication::Pending(_) => panic!("unexpected pending publication"),
        }
        std::fs::write(dir.path().join("password"), format!("{PASSWORD}\n")).unwrap();
        std::fs::write(
            dir.path().join("target.json"),
            json!({"account":"devstoreaccount1","container":"vlite-data",
                "coordination_container":"vlite-meta","prefix":"vlite/prod/",
                "endpoint":"http://127.0.0.1:1/devstoreaccount1","auth":"emulator_shared_key"})
            .to_string(),
        )
        .unwrap();
        Env {
            meta,
            data,
            sets,
            source_lineage: detail.lineage,
            dir,
            set_id,
        }
    }

    impl Env {
        fn request(&self) -> CloudRestore {
            CloudRestore {
                target_file: self.dir.path().join("target.json"),
                shared_key_file: None,
                set_id: self.set_id.clone(),
                database: self.dir.path().join("restored.vlite"),
                admin_directory: self.dir.path().join("restored-admin"),
                admin_username: "Recovery".into(),
                admin_password_file: self.dir.path().join("password"),
                mode: RestoreMode::Rekey,
            }
        }

        fn restore(&self, request: &CloudRestore) -> Result<Value, RecoveryError> {
            restore_with(&prepare(request)?, &self.sets)
        }

        fn pins(&self) -> usize {
            let key = format!("vlite/prod/sets/{}/record.json", self.set_id);
            let record: cloud::Record =
                serde_json::from_slice(&self.meta.raw(&key).unwrap()).unwrap();
            record.pins.len()
        }

        /// Paths created next to the source by a restore, scratch included.
        fn leftovers(&self) -> Vec<String> {
            let mut names: Vec<String> = std::fs::read_dir(self.dir.path())
                .unwrap()
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .filter(|n| !["source.vlite", "password", "target.json"].contains(&n.as_str()))
                .collect();
            names.sort();
            names
        }
    }

    #[test]
    fn rekey_restore_creates_an_isolated_copy_with_only_the_fresh_admin_usable() {
        let env = setup();
        let request = env.request();
        let report = env.restore(&request).unwrap();

        assert_eq!(report["rekey_only"], true);
        assert_eq!(report["pin_released"], true);
        assert_eq!(env.pins(), 0);
        assert_eq!(report["source_installation_id"], INSTALL);
        assert_eq!(report["inspection"]["live_records"], 4);
        assert_eq!(report["drill"]["probes"], 1);
        let source = lineage_hex(env.source_lineage).unwrap();
        assert_eq!(report["source_lineage"], source.as_str());
        let restored = maintenance::inspect_detail(&request.database).unwrap();
        assert_eq!(
            report["lineage"],
            lineage_hex(restored.lineage).unwrap().as_str()
        );
        assert_ne!(
            restored.lineage, env.source_lineage,
            "a restore gets a new lineage"
        );

        let import = &report["import"];
        assert_eq!(import["accounts_imported_disabled"], 2);
        assert_eq!(import["accounts_written"], 3);
        assert_eq!(import["fresh_admin"], "recovery");
        assert_eq!(import["credentials_to_reissue"][0]["id"], "abcdefghijkl");
        assert_eq!(import["webhook_subscriptions_to_recreate"], 2);

        let admin = &request.admin_directory;
        let users: Value =
            serde_json::from_slice(&std::fs::read(admin.join("users.json")).unwrap()).unwrap();
        let users = users["users"].as_array().unwrap();
        for user in users {
            if user["username"] == "recovery" {
                assert_eq!(user["role"], "super_admin");
                assert_eq!(user["disabled"], false);
            } else {
                assert_eq!(user["disabled"], true, "{user}");
                assert_eq!(user["must_change_password"], true, "{user}");
            }
        }
        assert!(admin.join("tenants.json").is_file());
        for absent in ["credentials.json", "webhooks.json", "next-startup"] {
            assert!(!admin.join(absent).exists(), "{absent} must not be written");
        }
        // Only the two outputs remain next to the source: no scratch left.
        assert_eq!(env.leftovers(), ["restored-admin", "restored.vlite"]);
        let text = report.to_string();
        assert!(
            !text.contains(PASSWORD),
            "the report must not echo the password"
        );
    }

    #[test]
    fn local_preconditions_are_refused_before_any_cloud_access() {
        let env = setup();
        // Any cloud access would fail loudly.
        env.meta.fault(Op::Get, "", Fault::Refuse, 0, usize::MAX);
        env.meta.fault(Op::Put, "", Fault::Refuse, 0, usize::MAX);
        let check = |request: CloudRestore, code: &str| {
            let error = env.restore(&request).expect_err(code);
            assert_eq!(error.code, code, "{error}");
        };

        let seed = CloudRestore {
            mode: RestoreMode::Seed,
            ..env.request()
        };
        check(seed, "cloud_seed_mode_deferred");
        let bad_id = CloudRestore {
            set_id: "../other".into(),
            ..env.request()
        };
        check(bad_id, "invalid_input");
        let existing = CloudRestore {
            database: env.dir.path().join("source.vlite"),
            ..env.request()
        };
        check(existing, "restore_destination_exists");
        let orphan = CloudRestore {
            database: env.dir.path().join("missing").join("x.vlite"),
            ..env.request()
        };
        check(orphan, "invalid_input");

        std::fs::create_dir(env.dir.path().join("used-admin")).unwrap();
        std::fs::write(env.dir.path().join("used-admin").join("users.json"), "{}").unwrap();
        let used = CloudRestore {
            admin_directory: env.dir.path().join("used-admin"),
            ..env.request()
        };
        check(used, "restore_admin_dir_not_empty");
        let file_admin = CloudRestore {
            admin_directory: env.dir.path().join("password"),
            ..env.request()
        };
        check(file_admin, "restore_admin_dir_not_empty");

        std::fs::write(env.dir.path().join("weak"), "password\n").unwrap();
        let weak = CloudRestore {
            admin_password_file: env.dir.path().join("weak"),
            ..env.request()
        };
        check(weak, "weak_password");
        std::fs::write(env.dir.path().join("large"), "x".repeat(5000)).unwrap();
        let large = CloudRestore {
            admin_password_file: env.dir.path().join("large"),
            ..env.request()
        };
        check(large, "invalid_password_file");
        let bad_name = CloudRestore {
            admin_username: "no spaces allowed".into(),
            ..env.request()
        };
        check(bad_name, "invalid_username");

        std::fs::write(
            env.dir.path().join("unknown.json"),
            json!({"account":"devstoreaccount1","container":"vlite-data","prefix":"p/",
                "auth":"managed_identity","sas":"never"})
            .to_string(),
        )
        .unwrap();
        let unknown = CloudRestore {
            target_file: env.dir.path().join("unknown.json"),
            ..env.request()
        };
        check(unknown, "invalid_target");
        std::fs::write(
            env.dir.path().join("remote-key.json"),
            json!({"account":"acct","container":"vlite-data","prefix":"p/",
                "endpoint":"https://acct.blob.core.windows.net","auth":"emulator_shared_key"})
            .to_string(),
        )
        .unwrap();
        let remote_key = CloudRestore {
            target_file: env.dir.path().join("remote-key.json"),
            ..env.request()
        };
        check(remote_key, "invalid_target");

        assert_eq!(
            env.leftovers(),
            [
                "large",
                "remote-key.json",
                "unknown.json",
                "used-admin",
                "weak"
            ]
        );
    }

    #[test]
    fn unavailable_or_corrupt_sets_leave_nothing_behind_and_release_the_pin() {
        let env = setup();
        let missing = CloudRestore {
            set_id: "20200101T000000Z-0123456789abcdef".into(),
            ..env.request()
        };
        assert_eq!(
            env.restore(&missing).unwrap_err().code,
            "cloud_set_not_found"
        );

        // Corrupt the database object (the largest data object).
        let object = env
            .data
            .names()
            .into_iter()
            .filter(|n| n.contains("/objects/"))
            .max_by_key(|n| env.data.raw(n).unwrap().len())
            .unwrap();
        let mut bytes = env.data.raw(&object).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 0xff;
        env.data.set_raw(&object, &bytes);
        let error = env.restore(&env.request()).unwrap_err();
        assert_eq!(error.code, "cloud_set_corrupt", "{error}");
        assert_eq!(env.pins(), 0, "the pin is released after a failure");
        assert!(env.leftovers().is_empty(), "{:?}", env.leftovers());
    }

    #[test]
    fn a_failed_import_removes_the_restored_copy_and_the_created_admin_directory() {
        let env = setup();
        // The fresh admin collides with an imported account.
        let colliding = CloudRestore {
            admin_username: "ops".into(),
            ..env.request()
        };
        let error = env.restore(&colliding).unwrap_err();
        assert_eq!(error.code, "restore_import_failed", "{error}");
        assert!(error.message.contains("already used"), "{error}");
        assert_eq!(env.pins(), 0);
        assert!(env.leftovers().is_empty(), "{:?}", env.leftovers());

        // An existing empty admin directory is kept, still empty.
        std::fs::create_dir(env.dir.path().join("empty-admin")).unwrap();
        let kept = CloudRestore {
            admin_username: "ops".into(),
            admin_directory: env.dir.path().join("empty-admin"),
            ..env.request()
        };
        env.restore(&kept).unwrap_err();
        assert_eq!(env.leftovers(), ["empty-admin"]);
        assert_eq!(
            std::fs::read_dir(env.dir.path().join("empty-admin"))
                .unwrap()
                .count(),
            0
        );
    }

    #[test]
    fn an_unreleased_pin_is_reported_with_its_removal_route() {
        let env = setup();
        let record = format!("vlite/prod/sets/{}/record.json", env.set_id);
        // The pin write succeeds; every later record write fails.
        env.meta
            .fault(Op::Put, &record, Fault::Refuse, 1, usize::MAX);
        let report = env.restore(&env.request()).unwrap();
        assert_eq!(report["pin_released"], false);
        let pin = report["pin_id"].as_str().unwrap();
        assert!(cloud::valid_pin_id(pin), "{pin}");
        assert!(
            report["pin_warning"]
                .as_str()
                .unwrap()
                .contains(&format!("/sets/{}/pins/{pin}", env.set_id))
        );
        assert_eq!(env.pins(), 1);
    }

    #[test]
    fn target_files_are_validated_and_emulator_keys_are_explicit() {
        let env = setup();
        let target = read_target(&env.dir.path().join("target.json")).unwrap();
        assert_eq!(target.auth, cloud::AuthMode::EmulatorSharedKey);
        std::fs::write(env.dir.path().join("key"), "not base64!").unwrap();
        let key = env.dir.path().join("key");
        assert_eq!(
            shared_key(&target, Some(&key)).unwrap_err().code,
            "invalid_shared_key"
        );
        std::fs::write(&key, "a2V5").unwrap();
        assert_eq!(
            shared_key(&target, Some(&key)).unwrap(),
            Some(b"key".to_vec())
        );
        let managed: Target = serde_json::from_value(
            json!({"account":"acct","container":"vlite-data","prefix":"p/"}),
        )
        .unwrap();
        assert_eq!(shared_key(&managed, None).unwrap(), None);
        // An explicit key with managed identity is refused by the connector.
        std::fs::write(
            env.dir.path().join("managed.json"),
            serde_json::to_vec(&managed).unwrap(),
        )
        .unwrap();
        let error = connect(&env.dir.path().join("managed.json"), Some(&key))
            .err()
            .unwrap();
        assert_eq!(error.code, "invalid_input", "{error}");
        assert!(error.message.contains("emulator_shared_key"), "{error}");
        assert!(read_target(&env.dir.path().join("absent.json")).is_err());
    }
}
