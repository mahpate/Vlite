//! design-tenant-provisioning ITEM-010: the offline seed engine and its
//! create-only receipt (DD-001, DD-003).
//!
//! [`seed`] creates one operation's database as a single V5 file
//! (graph-enabled) with explicit dimensions, commit timestamps off and the
//! encryption choice the [`SeedConfig`] states, initialized only from the
//! strict empty manifest `{"mode":"empty"}`. After close and sync it hashes
//! the exact file bytes and writes a create-only receipt binding the
//! operation ID, configuration digest, lineage and database digest. A rerun
//! returns the receipt only if it and the bytes still verify; a missing,
//! partial, mismatched or unreadable artifact is preserved and refused
//! (`seed_artifact_unverified`), never overwritten, re-seeded or rekeyed.
//!
//! [`seed_create`] composes this with the operation journal offline: the
//! configuration comes from the create's retained writer profile and the
//! receipt's seed is journaled through `Operations::record_seed`.
//!
//! Not decided here, so callers must supply them and nothing in production
//! calls this yet:
//! - the operation-owned staging directory: the caller passes an existing
//!   directory; proving it is the tenant's isolated host/mount is open
//!   (OD-002, isolated host/mount evidence);
//! - qualification of the retained writer profile and execution environment
//!   (OD-006); extracting its seed configuration does not qualify it;
//! - the key: a secret reference is bound into the configuration digest,
//!   but no repository contract resolves it to key bytes or generates and
//!   stores a new key, so the caller supplies the resolved key and the
//!   receipt cannot prove it is the referenced secret version.
use super::*;
use crate::{CreateOptions, Database, EncryptionKey, FormatVersion, MAX_DIMENSIONS};
use std::io::Write as _;
use std::path::{Path, PathBuf};

pub(super) const RECEIPT_SCHEMA: &str = "vlite.tenant-seed-receipt/1";
/// Separates configuration digests from content digests.
const CONFIG_DOMAIN: &[u8] = b"vlite.tenant-seed-config.v1\n";
const MAX_SECRET_REF: usize = 256;

/// The key policy of a new database: a secret reference, never key bytes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "mode", content = "secret_ref", rename_all = "snake_case")]
pub(super) enum Encryption {
    None,
    SecretReference(String),
}

/// The immutable initial database configuration of one create (DD-001).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct SeedConfig {
    dimensions: usize,
    encryption: Encryption,
    /// SHA-256 of the exact retained initialization manifest.
    initialization: String,
}

/// The only initialization manifest accepted: `{"mode":"empty"}`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    #[allow(dead_code)]
    mode: Mode,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum Mode {
    Empty,
}

pub(super) fn unsupported(message: &'static str) -> ApiError {
    ApiError::new(
        StatusCode::UNPROCESSABLE_ENTITY,
        "seed_configuration_unsupported",
        message,
    )
}

fn unverified() -> ApiError {
    conflict(
        "seed_artifact_unverified",
        "the operation's seed artifact is missing, partial or does not match its receipt; it is preserved and automatic recovery is refused",
    )
}

fn staging_unavailable() -> ApiError {
    ApiError::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "seed_staging_unavailable",
        "the seed staging directory could not be read",
    )
}

fn failed() -> ApiError {
    ApiError::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "seed_failed",
        "the seed could not be written; any partial artifact is preserved and refused on retry",
    )
}

impl SeedConfig {
    /// Builds the pinned configuration from the verified retained create
    /// inputs; this performs no seeding or secret resolution.
    pub(super) fn from_loaded(loaded: &Loaded) -> Result<Self, ApiError> {
        aca::seed_config(loaded)
    }

    /// Refuses any unsupported choice before a side effect.
    pub(super) fn new(
        dimensions: usize,
        encryption: Encryption,
        manifest: &[u8],
    ) -> Result<Self, ApiError> {
        Self::validate_choices(dimensions, &encryption)?;
        if serde_json::from_slice::<Manifest>(manifest).is_err() {
            return Err(unsupported(
                "only the initialization manifest {\"mode\":\"empty\"} is supported",
            ));
        }
        Ok(Self {
            dimensions,
            encryption,
            initialization: sha256_hex(manifest),
        })
    }

    pub(super) fn validate_choices(
        dimensions: usize,
        encryption: &Encryption,
    ) -> Result<(), ApiError> {
        if !(1..=MAX_DIMENSIONS).contains(&dimensions) {
            return Err(unsupported("seed dimensions are unsupported"));
        }
        if let Encryption::SecretReference(reference) = encryption
            && (reference.is_empty()
                || reference.len() > MAX_SECRET_REF
                || reference
                    .chars()
                    .any(|c| c.is_whitespace() || c.is_control()))
        {
            return Err(unsupported("seed encryption secret reference is invalid"));
        }
        Ok(())
    }

    fn encrypted(&self) -> bool {
        self.encryption != Encryption::None
    }

    /// SHA-256 over every pinned choice, including the fixed ones, so a
    /// later default can never change what an existing receipt means.
    pub(super) fn digest(&self) -> String {
        #[derive(Serialize)]
        struct Canonical<'a> {
            storage: &'static str,
            format: &'static str,
            graph: bool,
            commit_timestamps: bool,
            dimensions: usize,
            encryption: &'a Encryption,
            initialization: &'a str,
        }
        let mut bytes = CONFIG_DOMAIN.to_vec();
        serde_json::to_writer(
            &mut bytes,
            &Canonical {
                storage: "single_file",
                format: "v5",
                graph: true,
                commit_timestamps: false,
                dimensions: self.dimensions,
                encryption: &self.encryption,
                initialization: &self.initialization,
            },
        )
        .expect("serializing to memory cannot fail");
        sha256_hex(&bytes)
    }
}

/// The create-only receipt of one seeded database (DD-003).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SeedReceipt {
    pub(super) schema: String,
    pub(super) operation_id: String,
    pub(super) configuration_digest: String,
    pub(super) lineage: u64,
    /// SHA-256 of the exact database bytes after close and sync.
    pub(super) database_digest: String,
}

impl SeedReceipt {
    /// What `Operations::record_seed` journals.
    pub(super) fn seed(&self) -> Seed {
        Seed {
            lineage: self.lineage,
            digest: self.database_digest.clone(),
        }
    }
}

/// The operation's database and receipt paths inside `dir`.
fn paths(dir: &Path, operation_id: &str) -> Result<(PathBuf, PathBuf), ApiError> {
    if !is_hex(operation_id, 32) {
        return Err(invalid("operation ID is invalid"));
    }
    Ok((
        dir.join(format!("{operation_id}.vlite")),
        dir.join(format!("{operation_id}.seed-receipt.json")),
    ))
}

/// The key must be present exactly when the configuration references one.
fn check_key<'a>(
    config: &SeedConfig,
    key: Option<&'a EncryptionKey>,
) -> Result<Option<&'a EncryptionKey>, ApiError> {
    match (config.encrypted(), key) {
        (true, None) => Err(conflict(
            "seed_key_unavailable",
            "the seed configuration references an encryption key that was not supplied",
        )),
        (false, Some(_)) => Err(unsupported(
            "an encryption key was supplied for an unencrypted seed configuration",
        )),
        _ => Ok(key),
    }
}

/// Whether `path` exists (as anything; a link is never followed).
fn present(path: &Path) -> Result<bool, ApiError> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(_) => Err(staging_unavailable()),
    }
}

/// The bytes of a regular file; anything else is unverified.
fn read_regular(path: &Path) -> Result<Vec<u8>, ApiError> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_file() => {
            std::fs::read(path).map_err(|_| staging_unavailable())
        }
        Ok(_) => Err(unverified()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Err(unverified()),
        Err(_) => Err(staging_unavailable()),
    }
}

/// Opens the exact bytes, checks every pinned property and returns the
/// lineage.
fn inspect(
    bytes: Vec<u8>,
    config: &SeedConfig,
    key: Option<&EncryptionKey>,
) -> Result<u64, ApiError> {
    let db = Database::open_from_bytes(bytes, key).map_err(|_| unverified())?;
    match db.lineage() {
        Some(lineage)
            if lineage != 0
                && db.format_version() == 5
                && db.dimensions() == config.dimensions
                && !db.has_commit_timestamps()
                && db.is_encrypted() == config.encrypted()
                && db.is_empty() =>
        {
            Ok(lineage)
        }
        _ => Err(unverified()),
    }
}

fn sync_directory(dir: &Path) -> Result<(), ApiError> {
    super::super::cloud_recovery::sync_directory(dir).map_err(|_| failed())
}

/// Seeds the operation's database in `dir`, or returns its existing
/// receipt if that and the bytes verify. See the module documentation.
pub(super) fn seed(
    dir: &Path,
    operation_id: &str,
    config: &SeedConfig,
    key: Option<&EncryptionKey>,
) -> Result<SeedReceipt, ApiError> {
    let (database, receipt_path) = paths(dir, operation_id)?;
    let key = check_key(config, key)?;
    if !std::fs::metadata(dir).is_ok_and(|meta| meta.is_dir()) {
        return Err(staging_unavailable());
    }
    if present(&database)? || present(&receipt_path)? {
        return verify(dir, operation_id, config, key);
    }
    let options = CreateOptions {
        format: Some(FormatVersion::V5),
        encryption: key.cloned(),
        commit_timestamps: false,
    };
    // `create_new`: an existing file (a racing attempt) is never replaced.
    let db = match Database::create_with_options(&database, config.dimensions, options) {
        Ok(db) => db,
        Err(_) if present(&database)? => return verify(dir, operation_id, config, key),
        Err(_) => return Err(failed()),
    };
    drop(db);
    let bytes = (|| {
        // Write access only so Windows can flush; nothing is written.
        std::fs::OpenOptions::new()
            .write(true)
            .open(&database)?
            .sync_all()?;
        std::fs::read(&database)
    })()
    .map_err(|_| failed())?;
    let database_digest = sha256_hex(&bytes);
    let lineage = inspect(bytes, config, key)?;
    sync_directory(dir)?;

    let receipt = SeedReceipt {
        schema: RECEIPT_SCHEMA.to_owned(),
        operation_id: operation_id.to_owned(),
        configuration_digest: config.digest(),
        lineage,
        database_digest,
    };
    let mut body = serde_json::to_vec(&receipt).expect("serializing to memory cannot fail");
    body.push(b'\n');
    let mut file = match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&receipt_path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            return verify(dir, operation_id, config, key);
        }
        Err(_) => return Err(failed()),
    };
    // A torn write leaves a receipt that no longer parses: refused, kept.
    file.write_all(&body)
        .and_then(|()| file.sync_all())
        .map_err(|_| failed())?;
    drop(file);
    sync_directory(dir)?;
    Ok(receipt)
}

/// Verifies the operation's receipt and exact database bytes without
/// writing anything; every inconsistency is `seed_artifact_unverified`.
pub(super) fn verify(
    dir: &Path,
    operation_id: &str,
    config: &SeedConfig,
    key: Option<&EncryptionKey>,
) -> Result<SeedReceipt, ApiError> {
    let (database, receipt_path) = paths(dir, operation_id)?;
    let key = check_key(config, key)?;
    let receipt: SeedReceipt =
        serde_json::from_slice(&read_regular(&receipt_path)?).map_err(|_| unverified())?;
    if receipt.schema != RECEIPT_SCHEMA
        || receipt.operation_id != operation_id
        || receipt.configuration_digest != config.digest()
        || receipt.lineage == 0
        || !is_hex(&receipt.database_digest, 64)
    {
        return Err(unverified());
    }
    let bytes = read_regular(&database)?;
    if sha256_hex(&bytes) != receipt.database_digest
        || inspect(bytes, config, key)? != receipt.lineage
    {
        return Err(unverified());
    }
    Ok(receipt)
}

/// Offline composition: seeds create `operation_id`, claimed by `attempt`,
/// from its retained writer profile's seed configuration and journals the
/// receipt's seed through [`Operations::record_seed`]. Ownership and the
/// configuration are checked before any file is created. Once the journal
/// holds a seed step (settled or of unknown outcome), only its own verified
/// artifact is accepted: nothing is seeded again. A crash after the receipt
/// is durable is journaled on rerun; one before it leaves a database
/// without a receipt, which stays refused (DD-003). `dir` and `key` are
/// still caller-supplied; see the module documentation.
pub(super) fn seed_create(
    ops: &Operations,
    operation_id: &str,
    attempt: &str,
    dir: &Path,
    key: Option<&EncryptionKey>,
) -> Result<SeedReceipt, ApiError> {
    let config = SeedConfig::from_loaded(&ops.load(operation_id)?)?;
    let journal = ops.journal(operation_id)?;
    if journal.closed {
        return Err(ended());
    }
    if journal.attempt.as_deref() != Some(attempt) {
        return Err(not_owner());
    }
    let journaled = journal
        .steps
        .iter()
        .find(|step| step.action.resource == SEED)
        .map(|step| step.action.request_digest.as_str());
    let receipt = match journaled {
        Some(digest) => {
            let receipt = verify(dir, operation_id, &config, key)?;
            if receipt.database_digest != digest
                || recorded_seed(&journal).is_some_and(|seed| seed != receipt.seed())
            {
                return Err(unverified());
            }
            receipt
        }
        None => seed(dir, operation_id, &config, key)?,
    };
    ops.record_seed(operation_id, attempt, &receipt.seed())?;
    Ok(receipt)
}
