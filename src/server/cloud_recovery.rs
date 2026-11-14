//! Offline, isolated cloud restore for operators (feature-backup-ha ITEM-007,
//! backup-only part): `vlite-server cloud-sets` and `vlite-server
//! cloud-restore`.
//!
//! The restore runs without a server. Every local precondition (target file,
//! output path, the fresh admin's name and password) is checked before any
//! network access. It then pins the published set and, in a private staging
//! directory next to the output (owner-only on Unix), downloads and verifies
//! it, restores and probes a scratch copy (drill), restores the database
//! with a new lineage and imports the recovery bundle rekey-only: imported
//! accounts are disabled with unusable passwords, the fresh super admin is
//! the only usable sign-in, no credential, session or webhook is written.
//! Both stores are re-read and the fresh admin authenticated before the
//! output is published with one rename into a new `--output` directory, so
//! the output appears complete or not at all. The pin is always released.
//!
//! Nothing is selected or activated: the operator starts the restored copy
//! explicitly. Only the lineage-changing rekey restore exists; the
//! identity-preserving seed mode belongs to the deferred HA work and is
//! refused.
use super::backup_policy::{SHARED_KEY_ENV, bundle_summary, inspection, lineage_hex};
use super::cloud_backup::{self as cloud, PinKind, Sets, Target};
use super::*;
use crate::maintenance;
use std::io::{Read, Write};
use std::path::{Path as FilePath, PathBuf};

const MAX_TARGET_FILE: u64 = 64 * 1024;
const PIN_REASON: &str = "isolated-restore";
const PASSWORD_LABEL: &str = "--admin-password-file";
/// Published layout inside `--output`.
const DATABASE_FILE: &str = "database.vlite";
const ADMIN_DIR: &str = "admin";
/// Staging directories next to `--output`, and their owner lock.
const STAGING_PREFIX: &str = ".vlite-cloud-restore-";
const STAGING_LOCK: &str = "owner.lock";
/// Age after which an unlocked staging directory without a taken lock is
/// treated as abandoned.
const STALE_UNLOCKED: std::time::Duration = std::time::Duration::from_secs(3600);

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
    /// New output directory: must not exist, its parent must. It appears
    /// complete, holding `database.vlite` and `admin/`, or not at all.
    pub output: PathBuf,
    pub admin_username: String,
    pub admin_password_file: PathBuf,
    pub mode: RestoreMode,
}

/// Everything checked before the network is touched.
struct Plan {
    set_id: String,
    /// Canonical directory that receives `output` and the staging directory.
    parent: PathBuf,
    output: PathBuf,
    username: String,
    password: String,
}

fn refused(code: &'static str, message: impl Into<String>) -> RecoveryError {
    RecoveryError::new(code, message)
}

fn io_failure(what: &str, error: impl std::fmt::Display) -> RecoveryError {
    RecoveryError::new("restore_io_error", format!("{what}: {error}"))
}

/// A path for operators, without the `\\?\` prefix Windows canonical paths
/// carry.
fn display(path: &FilePath) -> String {
    let text = path.display().to_string();
    if !cfg!(windows) {
        return text;
    }
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = text.strip_prefix(r"\\?\") {
        rest.to_owned()
    } else {
        text
    }
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

/// The output must be new: an existing path (including an empty directory
/// left by an interrupted restore) is never reused or replaced.
fn require_absent(path: &FilePath) -> Result<(), RecoveryError> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Err(refused(
            "restore_destination_exists",
            format!(
                "{} already exists; a restore only creates a new --output directory (if an interrupted restore left it empty, remove it and retry)",
                display(path)
            ),
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(io_failure("cannot inspect --output", e)),
    }
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

    // Staging directories share the parent; never let --output name one.
    let name = match request.output.file_name() {
        Some(name) if !name.to_string_lossy().starts_with(STAGING_PREFIX) => name.to_owned(),
        _ => {
            return Err(refused(
                "invalid_input",
                "--output must name a new directory (not ., .. or a name starting with .vlite-cloud-restore-)",
            ));
        }
    };
    let parent = match request.output.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => FilePath::new("."),
    };
    // Resolved once: staging and publication use this exact directory.
    let parent = std::fs::canonicalize(parent)
        .ok()
        .filter(|path| path.is_dir())
        .ok_or_else(|| refused("invalid_input", "the --output parent directory must exist"))?;
    let output = parent.join(name);
    require_absent(&output)?;

    let (username, password) = users::fresh_admin(
        &request.admin_username,
        &request.admin_password_file,
        PASSWORD_LABEL,
    )?;
    Ok(Plan {
        set_id: request.set_id.clone(),
        parent,
        output,
        username,
        password,
    })
}

fn now() -> u64 {
    admin::now_ms() as u64
}

/// Makes directory entries durable (Unix; Windows has no directory sync).
fn sync_directory(dir: &FilePath) -> std::io::Result<()> {
    #[cfg(unix)]
    std::fs::File::open(dir)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = dir;
    Ok(())
}

/// One restore's private staging directory next to `--output` (same volume,
/// so publication is a single rename). Its locked `owner.lock`, which holds
/// the process ID once locked, marks it as live for other restores' cleanup.
struct Staging {
    lock: Option<std::fs::File>,
    dir: Option<tempfile::TempDir>,
}

impl Staging {
    fn create(parent: &FilePath) -> std::io::Result<Self> {
        let dir = cloud::private_dir(parent, STAGING_PREFIX)?;
        let mut lock = cloud::create_private_file(&dir.path().join(STAGING_LOCK))?;
        lock.try_lock().map_err(std::io::Error::from)?;
        lock.write_all(format!("{}\n", std::process::id()).as_bytes())?;
        Ok(Self {
            lock: Some(lock),
            dir: Some(dir),
        })
    }

    fn path(&self) -> &FilePath {
        self.dir
            .as_ref()
            .map_or(FilePath::new(""), |dir| dir.path())
    }

    /// Test-only crash: releases the lock but leaves the directory behind.
    #[cfg(test)]
    fn abandon(mut self) -> PathBuf {
        drop(self.lock.take());
        self.dir
            .take()
            .map(tempfile::TempDir::keep)
            .unwrap_or_default()
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        // Unlock and close before removal: Windows cannot delete open files.
        if let Some(lock) = self.lock.take() {
            let _ = lock.unlock();
        }
        drop(self.dir.take());
    }
}

/// Removes staging directories that interrupted restores left in `parent`:
/// those whose lock is free after it was taken (the PID is written under
/// the lock), and those that are over an hour old without a taken lock. A
/// live restore holds its lock, whatever its age, and is never touched.
fn remove_stale_staging(parent: &FilePath) -> usize {
    let Ok(entries) = std::fs::read_dir(parent) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        if !entry
            .file_name()
            .to_string_lossy()
            .starts_with(STAGING_PREFIX)
        {
            continue;
        }
        let path = entry.path();
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if !meta.is_dir() || meta.file_type().is_symlink() {
            continue;
        }
        let old = meta
            .modified()
            .ok()
            .and_then(|at| at.elapsed().ok())
            .is_some_and(|age| age >= STALE_UNLOCKED);
        let stale = match std::fs::File::open(path.join(STAGING_LOCK)) {
            Ok(mut lock) => {
                lock.try_lock().is_ok() && {
                    let mut byte = [0u8; 1];
                    old || lock.read(&mut byte).is_ok_and(|read| read == 1)
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => old,
            Err(_) => false,
        };
        if stale && std::fs::remove_dir_all(&path).is_ok() {
            removed += 1;
        }
    }
    removed
}

#[cfg(test)]
thread_local! {
    /// Test-only: the publication step at which to simulate a crash.
    static CRASH_AT: std::cell::Cell<Option<&'static str>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
fn crashes_at(point: &str) -> bool {
    CRASH_AT.with(|at| at.get() == Some(point))
}

fn restore_with(plan: &Plan, sets: &Sets) -> Result<Value, RecoveryError> {
    let id = &plan.set_id;
    let stale = remove_stale_staging(&plan.parent);
    let pin = sets.pin(id, PIN_REASON, PinKind::Operation, now())?;
    let result = restore_pinned(plan, sets, &pin.id);
    let released = sets.release(id, &pin.id, now()).is_ok();
    let mut report = match result {
        Ok(report) => report,
        Err(mut error) => {
            if !released {
                error.message = format!(
                    "{}; the pin {} on set {id} could not be released: remove it with DELETE /v1/admin/cloud-backup/sets/{id}/pins/{}?force=true",
                    error.message, pin.id, pin.id
                );
            }
            return Err(error);
        }
    };
    report["stale_staging_removed"] = json!(stale);
    report["pin_released"] = json!(released);
    if !released {
        report["pin_id"] = json!(pin.id);
        report["pin_warning"] = json!(format!(
            "the pin blocks retention of this set until removed with DELETE /v1/admin/cloud-backup/sets/{id}/pins/{}?force=true",
            pin.id
        ));
    }
    Ok(report)
}

/// Builds and verifies the whole output (database and admin stores) in
/// private staging, then publishes it with one no-replace rename: a crash
/// or failure leaves no partial `--output`.
fn restore_pinned(plan: &Plan, sets: &Sets, pin_id: &str) -> Result<Value, RecoveryError> {
    let id = &plan.set_id;
    let staging = Staging::create(&plan.parent)
        .map_err(|e| io_failure("cannot create the private staging directory", e))?;
    let download = staging.path().join("download");
    let staged = staging.path().join("output");
    let admin = staged.join(ADMIN_DIR);
    let database = staged.join(DATABASE_FILE);
    for dir in [&download, &staged, &admin] {
        cloud::create_private_subdir(dir)
            .map_err(|e| io_failure("cannot create the private staging directory", e))?;
    }

    let fetched = sets.fetch(id, pin_id, &download)?;
    let drill = maintenance::drill(&fetched.database, &download).map_err(|e| {
        refused(
            "cloud_drill_failed",
            format!("the downloaded set failed its drill: {e}"),
        )
    })?;
    let restored = maintenance::restore(&fetched.database, &database)
        .map_err(|e| io_failure("cannot restore the database", e))?;
    let lineage = maintenance::inspect_detail(&database)
        .map_err(|e| io_failure("cannot inspect the restored database", e))?
        .lineage;
    // Writes, re-reads and authenticates the rekeyed stores.
    let imported =
        recovery_bundle::import(&fetched.bundle, &admin, (&plan.username, &plan.password))
            .map_err(|e| {
                refused(
                    "restore_import_failed",
                    format!("the recovery bundle could not be imported: {e}"),
                )
            })?;
    let _ = std::fs::remove_dir_all(&download);
    #[cfg(test)]
    if crashes_at("staged") {
        staging.abandon();
        return Err(refused("test_crash", "simulated crash"));
    }
    for dir in [&admin, &staged] {
        sync_directory(dir).map_err(|e| io_failure("cannot sync the staged output", e))?;
    }

    // Claim the name, then move the staged output over the empty claim. The
    // rename fails rather than replacing anything that took its place.
    match std::fs::create_dir(&plan.output) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            return Err(require_absent(&plan.output).unwrap_err());
        }
        Err(e) => return Err(io_failure("cannot create --output", e)),
    }
    #[cfg(test)]
    if crashes_at("claimed") {
        staging.abandon();
        return Err(refused("test_crash", "simulated crash"));
    }
    if let Err(e) = std::fs::rename(&staged, &plan.output) {
        // Only an empty directory is ever removed: never the operator's data.
        let _ = std::fs::remove_dir(&plan.output);
        return Err(io_failure("cannot publish the restored output", e));
    }
    if let Err(e) = sync_directory(&plan.parent) {
        return Err(refused(
            "restore_publication_indeterminate",
            format!(
                "the restored output was moved to {} but the move could not be made durable ({e}); check that the directory exists and holds database.vlite and admin/ before using it",
                display(&plan.output)
            ),
        ));
    }
    drop(staging);

    let database = display(&plan.output.join(DATABASE_FILE));
    let admin = display(&plan.output.join(ADMIN_DIR));
    Ok(json!({
        "mode": "rekey",
        "rekey_only": true,
        "set_id": id,
        "output": display(&plan.output),
        "database": database,
        "admin_directory": admin,
        "staging_permissions": if cfg!(unix) {
            "owner-only: directories 0700, files 0600"
        } else {
            "inherited from the --output parent directory"
        },
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
            format!("start the restored copy explicitly: VLITE_ADMIN_DIR={admin} vlite-server {database}"),
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
                output: self.dir.path().join("restored"),
                admin_username: "Recovery".into(),
                admin_password_file: self.dir.path().join("password"),
                mode: RestoreMode::Rekey,
            }
        }

        fn restore(&self, request: &CloudRestore) -> Result<Value, RecoveryError> {
            restore_with(&prepare(request)?, &self.sets)
        }

        /// Runs the default restore, simulating a crash at `point`.
        fn restore_crashing(&self, point: &'static str) -> RecoveryError {
            CRASH_AT.with(|at| at.set(Some(point)));
            let result = self.restore(&self.request());
            CRASH_AT.with(|at| at.set(None));
            result.unwrap_err()
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

    fn names(dir: &FilePath) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[cfg(unix)]
    fn mode(path: &FilePath) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn rekey_restore_publishes_one_complete_output_with_only_the_fresh_admin_usable() {
        let env = setup();
        let source_bytes = std::fs::read(env.dir.path().join("source.vlite")).unwrap();
        let request = env.request();
        let report = env.restore(&request).unwrap();

        assert_eq!(report["rekey_only"], true);
        assert_eq!(report["pin_released"], true);
        assert_eq!(report["stale_staging_removed"], 0);
        let output = &request.output;
        assert_eq!(names(output), ["admin", "database.vlite"]);
        let canonical = std::fs::canonicalize(output).unwrap();
        assert_eq!(report["output"], display(&canonical).as_str());
        assert_eq!(
            report["database"],
            display(&canonical.join("database.vlite")).as_str()
        );
        assert_eq!(env.pins(), 0);
        assert_eq!(report["source_installation_id"], INSTALL);
        assert_eq!(report["inspection"]["live_records"], 4);
        assert_eq!(report["drill"]["probes"], 1);
        let source = lineage_hex(env.source_lineage).unwrap();
        assert_eq!(report["source_lineage"], source.as_str());
        let restored = maintenance::inspect_detail(output.join("database.vlite")).unwrap();
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

        let admin = &output.join("admin");
        assert_eq!(names(admin), ["tenants.json", "users.json"]);
        // Authentication matrix: only the fresh admin signs in.
        assert!(users::signs_in(admin, "recovery", PASSWORD));
        for name in ["ops", "reader", "recovery"] {
            for password in ["correct horse battery staple", "another long passphrase"] {
                assert!(!users::signs_in(admin, name, password), "{name}");
            }
        }
        assert!(!users::signs_in(admin, "ops", PASSWORD));
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
        // Only the output remains next to the source: no staging left, and
        // the source is untouched.
        assert_eq!(env.leftovers(), ["restored"]);
        assert_eq!(
            std::fs::read(env.dir.path().join("source.vlite")).unwrap(),
            source_bytes
        );
        #[cfg(unix)]
        {
            for dir in [output.clone(), admin.clone()] {
                assert_eq!(mode(&dir), 0o700, "{dir:?}");
            }
            for file in ["database.vlite", "admin/users.json", "admin/tenants.json"] {
                assert_eq!(mode(&output.join(file)), 0o600, "{file}");
            }
        }
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
        // The output must be new: a file or even an empty directory is
        // never reused.
        std::fs::create_dir(env.dir.path().join("empty")).unwrap();
        for existing in ["source.vlite", "empty"] {
            let request = CloudRestore {
                output: env.dir.path().join(existing),
                ..env.request()
            };
            check(request, "restore_destination_exists");
        }
        for output in [
            env.dir.path().join("missing").join("out"),
            env.dir.path().join("password").join("out"),
            env.dir.path().join(".vlite-cloud-restore-mine"),
            env.dir.path().join(".."),
            PathBuf::from("."),
        ] {
            let request = CloudRestore {
                output,
                ..env.request()
            };
            check(request, "invalid_input");
        }

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
            ["empty", "large", "remote-key.json", "unknown.json", "weak"]
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
    fn a_failed_import_publishes_nothing() {
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
    }

    #[test]
    fn a_crash_before_publication_leaves_no_output_and_a_retry_cleans_up() {
        let env = setup();
        assert_eq!(env.restore_crashing("staged").code, "test_crash");
        let left = env.leftovers();
        assert_eq!(left.len(), 1, "{left:?}");
        assert!(left[0].starts_with(STAGING_PREFIX), "{left:?}");
        let staging = env.dir.path().join(&left[0]);
        assert_eq!(names(&staging), ["output", "owner.lock"]);
        assert!(!env.request().output.exists());
        #[cfg(unix)]
        {
            for dir in ["", "output", "output/admin"] {
                assert_eq!(mode(&staging.join(dir)), 0o700, "{dir}");
            }
            for file in [
                "owner.lock",
                "output/database.vlite",
                "output/admin/users.json",
            ] {
                assert_eq!(mode(&staging.join(file)), 0o600, "{file}");
            }
        }

        let report = env.restore(&env.request()).unwrap();
        assert_eq!(report["stale_staging_removed"], 1);
        assert_eq!(env.leftovers(), ["restored"]);
        assert_eq!(names(&env.request().output), ["admin", "database.vlite"]);
    }

    #[test]
    fn a_crash_after_the_claim_leaves_an_empty_output_that_is_never_reused() {
        let env = setup();
        assert_eq!(env.restore_crashing("claimed").code, "test_crash");
        let output = env.request().output;
        assert!(names(&output).is_empty());
        let error = env.restore(&env.request()).unwrap_err();
        assert_eq!(error.code, "restore_destination_exists", "{error}");
        assert!(error.message.contains("remove it and retry"), "{error}");

        std::fs::remove_dir(&output).unwrap();
        let report = env.restore(&env.request()).unwrap();
        assert_eq!(report["stale_staging_removed"], 1);
        assert_eq!(names(&output), ["admin", "database.vlite"]);
        assert_eq!(env.leftovers(), ["restored"]);
    }

    #[test]
    fn stale_staging_cleanup_spares_live_recent_and_unrelated_entries() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let live = Staging::create(root).unwrap();
        let crashed = Staging::create(root).unwrap().abandon();
        // Created but not yet locked (or never locked) and recent: kept.
        let unlocked = root.join(format!("{STAGING_PREFIX}unlocked"));
        std::fs::create_dir(&unlocked).unwrap();
        std::fs::write(unlocked.join(STAGING_LOCK), b"").unwrap();
        let fresh = root.join(format!("{STAGING_PREFIX}fresh"));
        std::fs::create_dir(&fresh).unwrap();
        let file = root.join(format!("{STAGING_PREFIX}file"));
        std::fs::write(&file, b"x").unwrap();
        std::fs::create_dir(root.join("restored")).unwrap();

        assert!(crashed.exists());
        assert_eq!(remove_stale_staging(root), 1);
        assert!(!crashed.exists());
        for kept in [
            live.path(),
            &unlocked,
            &fresh,
            &file,
            &root.join("restored"),
        ] {
            assert!(kept.exists(), "{kept:?}");
        }
        let live_path = live.path().to_owned();
        drop(live);
        assert!(
            !live_path.exists(),
            "a finished restore removes its staging"
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
