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
//! the output appears complete or not at all. The pin is always released;
//! if that fails after a publication, the command fails with
//! `restore_cleanup_failed` and keeps the output.
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

/// Exit status of a command that published its output but could not
/// clean up after itself (`restore_cleanup_failed`), distinct from the
/// general failure status 1 so scripts never treat it as success or as
/// "nothing happened".
pub const CLEANUP_FAILED_EXIT: u8 = 3;

/// A refused or failed offline recovery command.
#[derive(Debug)]
pub struct RecoveryError {
    pub code: &'static str,
    pub message: String,
    /// The report of a restore whose output was published even though the
    /// command failed afterwards (`restore_cleanup_failed`).
    pub report: Option<Value>,
}

impl RecoveryError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            report: None,
        }
    }

    /// The same `{"error":{"code","message"}}` shape as the admin API, plus
    /// the published restore's `report` when there is one.
    pub fn to_json(&self) -> Value {
        let mut body = json!({"error":{"code":self.code,"message":self.message}});
        if let Some(report) = &self.report {
            body["report"] = report.clone();
        }
        body
    }

    /// The process exit status for this failure.
    pub fn exit_status(&self) -> u8 {
        if self.code == "restore_cleanup_failed" {
            CLEANUP_FAILED_EXIT
        } else {
            1
        }
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
            report: None,
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
    // Position only: serde diagnostics quote unknown fields and values.
    let target: Target = serde_json::from_slice(&bytes).map_err(|e| {
        invalid(format!(
            "the target file is not a valid target (line {}, column {}): it must be the policy's target object with only its documented fields",
            e.line(),
            e.column()
        ))
    })?;
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
/// left by an interrupted restore) is never reused or replaced. Like every
/// diagnostic here, the refusal does not repeat the argument's value.
fn require_absent(path: &FilePath) -> Result<(), RecoveryError> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Err(refused(
            "restore_destination_exists",
            "--output already exists; a restore only creates a new --output directory (if an interrupted restore left it empty, remove it and retry)",
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

/// Whether `name` is one that `Staging::create` generates.
fn generated_staging_name(name: &std::ffi::OsStr) -> bool {
    name.to_str()
        .and_then(|name| name.strip_prefix(STAGING_PREFIX))
        .is_some_and(|random| {
            random.len() == cloud::PRIVATE_DIR_RANDOM
                && random.bytes().all(|b| b.is_ascii_alphanumeric())
        })
}

/// Whether a staging directory holds only what a restore writes there.
fn only_staging_entries(path: &FilePath) -> bool {
    let Ok(entries) = std::fs::read_dir(path) else {
        return false;
    };
    entries.into_iter().all(|entry| {
        let Ok((name, kind)) = entry.and_then(|e| Ok((e.file_name(), e.file_type()?))) else {
            return false;
        };
        match name.to_str() {
            Some(STAGING_LOCK) => kind.is_file(),
            Some("download" | "output") => kind.is_dir(),
            _ => false,
        }
    })
}

/// Reads a lock file as `Staging::create` writes it: empty until it is
/// locked (`Some(false)`), then the owner's process ID and a newline
/// (`Some(true)`). Anything else is not a restore's lock (`None`).
fn lock_written(lock: &mut std::fs::File) -> Option<bool> {
    let mut bytes = Vec::new();
    Read::take(&mut *lock, 12).read_to_end(&mut bytes).ok()?;
    match bytes.as_slice() {
        [] => Some(false),
        [pid @ .., b'\n']
            if (1..=10).contains(&pid.len()) && pid.iter().all(u8::is_ascii_digit) =>
        {
            Some(true)
        }
        _ => None,
    }
}

/// Removes staging directories that interrupted restores left in `parent`:
/// those whose lock is free after it was taken (the PID is written under
/// the lock), and those that are over an hour old without a taken lock. A
/// live restore holds its lock, whatever its age, and is never touched.
/// Only a directory with a generated name, holding nothing but a restore's
/// entries and a lock in the expected format, is a candidate: anything else
/// is left alone.
fn remove_stale_staging(parent: &FilePath) -> usize {
    let Ok(entries) = std::fs::read_dir(parent) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        if !generated_staging_name(&entry.file_name()) {
            continue;
        }
        let path = entry.path();
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if !meta.is_dir() || meta.file_type().is_symlink() || !only_staging_entries(&path) {
            continue;
        }
        let old = meta
            .modified()
            .ok()
            .and_then(|at| at.elapsed().ok())
            .is_some_and(|age| age >= STALE_UNLOCKED);
        let stale = match std::fs::File::open(path.join(STAGING_LOCK)) {
            Ok(mut lock) => {
                lock.try_lock().is_ok()
                    && match lock_written(&mut lock) {
                        Some(written) => written || old,
                        None => false,
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

#[cfg(test)]
thread_local! {
    /// Test-only: runs just before publication, given the `--output` path.
    #[allow(clippy::type_complexity)]
    static BEFORE_PUBLISH: std::cell::RefCell<Option<Box<dyn FnOnce(&FilePath)>>> =
        const { std::cell::RefCell::new(None) };
}

/// Moves `from` to `to` only while nothing exists at `to`. Unlike
/// `std::fs::rename`, it never replaces a file or an empty directory. A
/// file system without such a rename returns `Unsupported`.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn rename_no_replace(from: &FilePath, to: &FilePath) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let c_path = |path: &FilePath| {
        std::ffi::CString::new(path.as_os_str().as_bytes())
            .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))
    };
    let (from, to) = (c_path(from)?, c_path(to)?);
    // SAFETY: both pointers are NUL-terminated strings that outlive the call.
    #[cfg(target_os = "linux")]
    let status = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            from.as_ptr(),
            libc::AT_FDCWD,
            to.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    // SAFETY: as above.
    #[cfg(target_os = "macos")]
    let status = unsafe { libc::renamex_np(from.as_ptr(), to.as_ptr(), libc::RENAME_EXCL) };
    if status == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    // EINVAL, ENOSYS or ENOTSUP: the file system has no exclusive rename.
    Err(match error.raw_os_error() {
        Some(libc::EINVAL | libc::ENOSYS | libc::ENOTSUP) => {
            std::io::Error::new(std::io::ErrorKind::Unsupported, error)
        }
        _ => error,
    })
}

/// Windows: `MoveFileExW` without `MOVEFILE_REPLACE_EXISTING`.
#[cfg(windows)]
fn rename_no_replace(from: &FilePath, to: &FilePath) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn MoveFileExW(existing: *const u16, new: *const u16, flags: u32) -> i32;
    }
    let wide = |path: &FilePath| -> Vec<u16> {
        path.as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    };
    let (from, to) = (wide(from), wide(to));
    // SAFETY: both pointers are NUL-terminated UTF-16 strings that outlive the
    // call. Flags 0 omit MOVEFILE_REPLACE_EXISTING (and copying), so an
    // existing destination fails the move.
    if unsafe { MoveFileExW(from.as_ptr(), to.as_ptr(), 0) } != 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Other platforms have no rename without replacement here: refused.
#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn rename_no_replace(_from: &FilePath, _to: &FilePath) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "no rename without replacement on this platform",
    ))
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
    if released {
        return Ok(report);
    }
    // The output is complete and stays; the command still fails, so a
    // script cannot mistake a leftover retention hold for a clean run.
    let route = format!(
        "DELETE /v1/admin/cloud-backup/sets/{id}/pins/{}?force=true",
        pin.id
    );
    report["pin_id"] = json!(pin.id);
    report["pin_removal"] = json!(route);
    Err(RecoveryError {
        code: "restore_cleanup_failed",
        message: format!(
            "the restore was published to --output and is usable (see the report), but its pin {} on set {id} could not be released and blocks retention of that set: remove it with {route}",
            pin.id
        ),
        report: Some(report),
    })
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

    // One rename that never replaces: whatever appeared at --output since it
    // was checked (a file, a link or an empty directory) fails the
    // publication and is left untouched; nothing is claimed or removed there.
    #[cfg(test)]
    BEFORE_PUBLISH.with(|hook| hook.take().map(|hook| hook(&plan.output)));
    match rename_no_replace(&staged, &plan.output) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            return Err(refused(
                "restore_destination_exists",
                "--output appeared while the restore ran; it was left untouched and nothing was published",
            ));
        }
        Err(e) if e.kind() == std::io::ErrorKind::Unsupported => {
            return Err(refused(
                "restore_publication_unsupported",
                "the --output file system cannot rename without replacing an existing entry; choose an --output parent on a local file system",
            ));
        }
        Err(e) => return Err(io_failure("cannot publish the restored output", e)),
    }
    if let Err(e) = sync_directory(&plan.parent) {
        return Err(refused(
            "restore_publication_indeterminate",
            format!(
                "the restored output was moved to --output but the move could not be made durable ({e}); check that --output exists and holds database.vlite and admin/ before using it"
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
    use crate::property_graph::{FindNodes, Neighbors, NodeSelector, ReadOptions};
    use crate::{CollectionSettings, CollectionSpec, Expected, Metadata, MetadataValue};
    use std::sync::atomic::{AtomicUsize, Ordering};

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
            "credentials": [{"id":"abcdefghijkl","name":"ci","tenant_id":null,"collections":["*"],
                "operations":["read"],"state":"active","expires_unix_ms":99}],
            "webhook_subscriptions": 2,
            "secret_references": [{"name":"admin_token","configured":true,"action":"provision"}],
            "excluded": recovery_bundle::EXCLUDED,
        }))
        .unwrap()
    }

    fn setup() -> Env {
        setup_with(|database| {
            for id in 1..=4u64 {
                database
                    .put(id, &[id as f32, 1.0, -0.5], format!("p{id}").as_bytes())
                    .unwrap();
            }
        })
    }

    /// Publishes one set whose database `fill` populated.
    fn setup_with(fill: impl FnOnce(&mut Database)) -> Env {
        let meta = Arc::new(Memory::default());
        let data = Arc::new(Memory::default());
        let sets = Sets::new(meta.clone(), data.clone(), false, "vlite/prod/".into());
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.vlite");
        let mut database = Database::create(&source, 3).unwrap();
        fill(&mut database);
        database.close().unwrap();
        let verification = maintenance::verify(&source).unwrap();
        let detail = maintenance::inspect_detail(&source).unwrap();
        let database = Upload::file(ROLE_DATABASE, source).unwrap();
        let policy = cloud::fake::policy_capture();
        let bundle = recovery_bundle::bundle(
            config(),
            INSTALL,
            detail.lineage,
            &database.sha256,
            policy,
            1,
        );
        let uploads = vec![
            database,
            Upload::bytes(ROLE_BUNDLE, serde_json::to_vec(&bundle).unwrap()),
        ];
        let at = now();
        let set_id = cloud::new_set_id(at).unwrap();
        let manifest = Manifest::new(
            &set_id,
            INSTALL,
            at,
            &verification,
            &detail,
            &uploads,
            cloud::fake::policy_capture(),
        );
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
        // never reused, and the refusal does not repeat the path (CR-06).
        std::fs::create_dir(env.dir.path().join("empty-SENTINEL-c6")).unwrap();
        for existing in ["source.vlite", "empty-SENTINEL-c6"] {
            let request = CloudRestore {
                output: env.dir.path().join(existing),
                ..env.request()
            };
            let error = env.restore(&request).unwrap_err();
            assert_eq!(error.code, "restore_destination_exists", "{error}");
            let text = error.to_json().to_string();
            assert!(!text.contains(existing), "{text}");
            assert!(!text.contains(&display(env.dir.path())), "{text}");
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
            [
                "empty-SENTINEL-c6",
                "large",
                "remote-key.json",
                "unknown.json",
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

    /// The data-store name of the set's object with `role`.
    fn object(env: &Env, role: &str) -> String {
        let prefix = format!("vlite/prod/sets/{}/", env.set_id);
        let manifest = env.data.raw(&format!("{prefix}manifest.json")).unwrap();
        let manifest: Value = serde_json::from_slice(&manifest).unwrap();
        let object = manifest["objects"]
            .as_array()
            .unwrap()
            .iter()
            .find(|object| object["role"] == role)
            .unwrap();
        format!("{prefix}objects/{}", object["sha256"].as_str().unwrap())
    }

    fn edit(store: &Memory, name: &str, change: impl FnOnce(&mut Vec<u8>)) {
        let mut bytes = store.raw(name).unwrap();
        change(&mut bytes);
        store.set_raw(name, &bytes);
    }

    fn set_state(env: &Env, state: &str) {
        let key = format!("vlite/prod/sets/{}/record.json", env.set_id);
        edit(&env.meta, &key, |bytes| {
            let mut record: Value = serde_json::from_slice(bytes).unwrap();
            record["state"] = json!(state);
            *bytes = serde_json::to_vec(&record).unwrap();
        });
    }

    /// Rewrites the manifest and rebinds the record to the new bytes, so the
    /// chain still matches and only the manifest's content is wrong.
    fn rewrite_manifest(env: &Env, change: impl FnOnce(&mut Value)) {
        let name = format!("vlite/prod/sets/{}/manifest.json", env.set_id);
        let mut manifest: Value = serde_json::from_slice(&env.data.raw(&name).unwrap()).unwrap();
        change(&mut manifest);
        let bytes = serde_json::to_vec(&manifest).unwrap();
        env.data.set_raw(&name, &bytes);
        let key = format!("vlite/prod/sets/{}/record.json", env.set_id);
        edit(&env.meta, &key, |record| {
            let mut value: Value = serde_json::from_slice(record).unwrap();
            value["manifest_sha256"] = json!(cloud::sha_hex(&bytes));
            *record = serde_json::to_vec(&value).unwrap();
        });
    }

    #[test]
    fn a_rebound_manifest_with_invalid_content_is_refused_with_nothing_left() {
        // Review F004-VALIDATE: the record -> manifest chain matches, but the
        // policy capture is outside the admitted policy's bounds.
        type Case = (&'static str, fn(&mut Value), &'static str);
        let cases: [Case; 3] = [
            (
                "keep_sets below the floor",
                |m| m["policy"]["keep_sets"] = json!(2),
                "manifest policy capture is invalid",
            ),
            (
                "keep_days zero",
                |m| m["policy"]["keep_days"] = json!(0),
                "manifest policy capture is invalid",
            ),
            (
                "keep_sets above the maximum",
                |m| m["policy"]["keep_sets"] = json!(1001),
                "manifest policy capture is invalid",
            ),
        ];
        for (case, change, message) in cases {
            let env = setup();
            rewrite_manifest(&env, change);
            let error = env.restore(&env.request()).unwrap_err();
            assert_eq!(error.code, "cloud_set_corrupt", "{case}: {error}");
            assert!(error.message.contains(message), "{case}: {error}");
            assert_eq!(env.pins(), 0, "{case}: the pin is released");
            assert!(env.leftovers().is_empty(), "{case}: {:?}", env.leftovers());
        }
    }

    #[test]
    fn corrupt_missing_or_unpublished_sets_are_refused_with_nothing_left() {
        fn manifest(env: &Env) -> String {
            format!("vlite/prod/sets/{}/manifest.json", env.set_id)
        }
        type Case = (&'static str, fn(&Env), &'static str);
        let cases: [Case; 10] = [
            (
                "manifest changed",
                |env| edit(&env.data, &manifest(env), |b| b[0] ^= 0xff),
                "cloud_set_corrupt",
            ),
            (
                "manifest missing",
                |env| env.data.remove(&manifest(env)),
                "cloud_set_corrupt",
            ),
            (
                "database truncated",
                |env| {
                    edit(&env.data, &object(env, ROLE_DATABASE), |b| {
                        b.truncate(b.len() - 1)
                    })
                },
                "cloud_set_corrupt",
            ),
            (
                "database extended",
                |env| edit(&env.data, &object(env, ROLE_DATABASE), |b| b.push(0)),
                "cloud_set_corrupt",
            ),
            (
                "database missing",
                |env| env.data.remove(&object(env, ROLE_DATABASE)),
                "cloud_set_corrupt",
            ),
            (
                "bundle changed",
                |env| edit(&env.data, &object(env, ROLE_BUNDLE), |b| b[0] ^= 0xff),
                "cloud_set_corrupt",
            ),
            (
                "bundle missing",
                |env| env.data.remove(&object(env, ROLE_BUNDLE)),
                "cloud_set_corrupt",
            ),
            (
                "staging",
                |env| set_state(env, "staging"),
                "cloud_set_unavailable",
            ),
            (
                "delete pending",
                |env| set_state(env, "delete_pending"),
                "cloud_set_unavailable",
            ),
            (
                "tombstoned",
                |env| set_state(env, "tombstoned"),
                "cloud_set_unavailable",
            ),
        ];
        for (case, damage, code) in cases {
            let env = setup();
            damage(&env);
            let error = env.restore(&env.request()).unwrap_err();
            assert_eq!(error.code, code, "{case}: {error}");
            assert_eq!(env.pins(), 0, "{case}: the pin is released");
            assert!(env.leftovers().is_empty(), "{case}: {:?}", env.leftovers());
        }
    }

    /// Two collections, a delete, metadata, and a graph with an edge, an
    /// attachment and a deleted node.
    fn mixed(database: &mut Database) {
        let lang = Metadata::from([("lang".into(), MetadataValue::String("en".into()))]);
        database
            .create_collection(
                "docs",
                CollectionSpec {
                    dimensions: 2,
                    settings: CollectionSettings::default(),
                },
            )
            .unwrap();
        for id in 1..=4u64 {
            database
                .put(id, &[id as f32, 1.0, -0.5], format!("p{id}").as_bytes())
                .unwrap();
        }
        database.delete(4).unwrap();
        database
            .collection_mut("docs")
            .unwrap()
            .put_with_metadata(10, &[1.0, 0.0], b"d10", &lang)
            .unwrap();
        database.create_graph("people").unwrap();
        let mut graph = database.graph_mut("people").unwrap();
        graph.create_node("ada", &["Person"], &lang).unwrap();
        graph
            .create_node("bob", &["Person"], &Metadata::new())
            .unwrap();
        graph
            .create_edge(Some("knows"), "ada", "KNOWS", "bob", &Metadata::new())
            .unwrap();
        graph.attach("ada", "docs", 10).unwrap();
        graph.create_node("gone", &[], &Metadata::new()).unwrap();
        graph.delete_node("gone", Expected::Any).unwrap();
    }

    /// Everything a reader observes, in canonical order.
    fn observed(path: &FilePath) -> String {
        let database = Database::open(path).unwrap();
        let collections = database.collections().unwrap();
        let mut out = format!("{collections:?}\n");
        for info in &collections {
            let collection = database.collection(&info.name).unwrap();
            for id in 0..=12 {
                if let Some(record) = collection.get(id).unwrap() {
                    let revision = collection.revision(id).unwrap();
                    out += &format!("{}/{id} {record:?} {revision:?}\n", info.name);
                }
            }
        }
        for info in database.list_graphs().unwrap() {
            let graph = database.graph(&info.name).unwrap();
            let read = ReadOptions::default();
            let nodes = graph.find_nodes(&FindNodes::default(), &read).unwrap();
            assert!(nodes.cursor.is_none());
            out += &format!("{} {:?}\n", info.name, nodes.output);
            for node in &nodes.output {
                let request = Neighbors::new(NodeSelector::Id(node.id));
                let edges = graph.neighbors(&request, &read).unwrap();
                out += &format!("{} {:?}\n", node.key, edges.output.edges);
            }
        }
        out
    }

    /// The same later writes, so allocation high-water marks are compared.
    fn write_more(path: &FilePath) -> u64 {
        let mut database = Database::open(path).unwrap();
        let mut graph = database.graph_mut("people").unwrap();
        let node = graph.create_node("fresh", &[], &Metadata::new()).unwrap();
        graph
            .create_edge(None, "fresh", "KNOWS", "ada", &Metadata::new())
            .unwrap();
        database.put(5, &[5.0, 1.0, -0.5], b"p5").unwrap();
        database.close().unwrap();
        node.id
    }

    #[test]
    fn mixed_data_is_restored_exactly_and_keeps_allocating_after_reopen() {
        let env = setup_with(mixed);
        // The set is pinned before any payload is read.
        let pins = Arc::new(AtomicUsize::new(usize::MAX));
        let (meta, seen) = (env.meta.clone(), pins.clone());
        let key = format!("vlite/prod/sets/{}/record.json", env.set_id);
        env.data.hook(Op::Get, "/objects/", move || {
            let record: cloud::Record = serde_json::from_slice(&meta.raw(&key).unwrap()).unwrap();
            seen.store(record.pins.len(), Ordering::SeqCst);
        });
        let source = env.dir.path().join("source.vlite");
        let before = observed(&source);
        assert!(
            before.contains("ada") && before.contains("docs/10"),
            "{before}"
        );
        let request = env.request();
        env.restore(&request).unwrap();
        assert_eq!(pins.load(Ordering::SeqCst), 1);

        let restored = request.output.join("database.vlite");
        assert_eq!(observed(&restored), before);
        let scratch = env.dir.path().join("scratch.vlite");
        std::fs::copy(&source, &scratch).unwrap();
        let expected = write_more(&scratch);
        assert_eq!(write_more(&restored), expected, "node IDs are not reused");
        assert_eq!(observed(&restored), observed(&scratch));
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
    fn publication_never_replaces_what_appeared_at_the_output() {
        let env = setup();
        let output = env.request().output;
        type Plant = fn(&FilePath);
        let plants: [(&str, Plant); 2] = [
            ("empty directory", |path| std::fs::create_dir(path).unwrap()),
            ("file", |path| {
                std::fs::write(path, b"operator data").unwrap()
            }),
        ];
        for (what, plant) in plants {
            BEFORE_PUBLISH.with(|hook| *hook.borrow_mut() = Some(Box::new(plant)));
            let error = env.restore(&env.request()).unwrap_err();
            assert_eq!(error.code, "restore_destination_exists", "{what}: {error}");
            assert!(!error.message.contains("restored"), "{what}: {error}");
            assert_eq!(env.pins(), 0, "{what}");
            assert_eq!(env.leftovers(), ["restored"], "{what}");
            if output.is_dir() {
                assert!(names(&output).is_empty(), "{what}");
                std::fs::remove_dir(&output).unwrap();
            } else {
                assert_eq!(std::fs::read(&output).unwrap(), b"operator data");
                std::fs::remove_file(&output).unwrap();
            }
        }
        let report = env.restore(&env.request()).unwrap();
        assert_eq!(report["stale_staging_removed"], 0);
        assert_eq!(names(&output), ["admin", "database.vlite"]);
    }

    #[test]
    fn rename_no_replace_moves_only_onto_a_free_name() {
        let dir = tempfile::tempdir().unwrap();
        let from = dir.path().join("from");
        std::fs::create_dir(&from).unwrap();
        std::fs::write(from.join("data"), b"x").unwrap();
        let empty = dir.path().join("empty");
        std::fs::create_dir(&empty).unwrap();
        let file = dir.path().join("file");
        std::fs::write(&file, b"keep").unwrap();
        for taken in [&empty, &file] {
            let error = rename_no_replace(&from, taken).unwrap_err();
            assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists, "{error}");
        }
        assert!(names(&empty).is_empty());
        assert_eq!(std::fs::read(&file).unwrap(), b"keep");
        assert!(from.join("data").exists());
        let to = dir.path().join("to");
        rename_no_replace(&from, &to).unwrap();
        assert!(!from.exists());
        assert_eq!(std::fs::read(to.join("data")).unwrap(), b"x");
    }

    #[test]
    fn stale_staging_cleanup_spares_live_recent_and_unrelated_entries() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let live = Staging::create(root).unwrap();
        let crashed = Staging::create(root).unwrap().abandon();
        assert!(generated_staging_name(crashed.file_name().unwrap()));
        let generated =
            |tag: char| root.join(format!("{STAGING_PREFIX}{}", tag.to_string().repeat(12)));
        let staged = |path: &FilePath, lock: &[u8]| {
            std::fs::create_dir(path).unwrap();
            std::fs::create_dir(path.join("output")).unwrap();
            std::fs::write(path.join(STAGING_LOCK), lock).unwrap();
        };
        // Created but not yet locked (or never locked) and recent: kept.
        let unlocked = generated('u');
        staged(&unlocked, b"");
        let fresh = generated('f');
        std::fs::create_dir(&fresh).unwrap();
        let file = generated('x');
        std::fs::write(&file, b"x").unwrap();
        // Review N-02: entries that only look like staging are kept.
        let junk_lock = generated('j');
        staged(&junk_lock, b"operator notes");
        let extra = generated('e');
        staged(&extra, b"4242\n");
        std::fs::write(extra.join("notes.txt"), b"keep").unwrap();
        let wrong_kind = generated('w');
        std::fs::create_dir(&wrong_kind).unwrap();
        std::fs::write(wrong_kind.join("output"), b"keep").unwrap();
        std::fs::write(wrong_kind.join(STAGING_LOCK), b"4242\n").unwrap();
        let named = root.join(format!("{STAGING_PREFIX}operator-copy"));
        staged(&named, b"4242\n");
        // A crashed restore's layout under a generated name: removed.
        let pid_lock = generated('p');
        staged(&pid_lock, b"4242\n");
        std::fs::create_dir(root.join("restored")).unwrap();

        assert!(crashed.exists());
        assert_eq!(remove_stale_staging(root), 2);
        assert!(!crashed.exists() && !pid_lock.exists());
        for kept in [
            live.path(),
            &unlocked,
            &fresh,
            &file,
            &junk_lock,
            &extra,
            &wrong_kind,
            &named,
            &root.join("restored"),
        ] {
            assert!(kept.exists(), "{kept:?}");
        }
        assert_eq!(std::fs::read(extra.join("notes.txt")).unwrap(), b"keep");
        let live_path = live.path().to_owned();
        drop(live);
        assert!(
            !live_path.exists(),
            "a finished restore removes its staging"
        );
    }

    #[test]
    fn an_unreleased_pin_fails_the_command_but_keeps_the_published_output() {
        let env = setup();
        let record = format!("vlite/prod/sets/{}/record.json", env.set_id);
        // The pin write succeeds; every later record write fails.
        env.meta
            .fault(Op::Put, &record, Fault::Refuse, 1, usize::MAX);
        let request = env.request();
        let error = env.restore(&request).unwrap_err();
        assert_eq!(error.code, "restore_cleanup_failed");
        assert_eq!(error.exit_status(), CLEANUP_FAILED_EXIT);
        assert_ne!(error.exit_status(), 1);
        let report = error.report.as_ref().expect("the published report is kept");
        assert_eq!(report["pin_released"], false);
        let pin = report["pin_id"].as_str().unwrap();
        assert!(cloud::valid_pin_id(pin), "{pin}");
        let route = format!(
            "DELETE /v1/admin/cloud-backup/sets/{}/pins/{pin}?force=true",
            env.set_id
        );
        assert_eq!(report["pin_removal"], route.as_str());
        assert!(error.message.contains(&route), "{}", error.message);
        // The message names --output, not its value; the report has it.
        let output = display(&request.output);
        assert!(!error.message.contains(&output), "{}", error.message);
        assert_eq!(report["output"], output.as_str());
        let body = error.to_json();
        assert_eq!(body["error"]["code"], "restore_cleanup_failed");
        assert_eq!(body["report"]["pin_id"], pin);
        // The output was published before the failure and stays usable.
        assert_eq!(names(&request.output), ["admin", "database.vlite"]);
        assert_eq!(env.pins(), 1);
        // Ordinary failures keep the general status.
        assert_eq!(refused("x", "y").exit_status(), 1);
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
