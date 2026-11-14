//! Azure Blob cloud backup sets (feature-backup-ha ITEM-005/006/007).
//!
//! Layout under the configured prefix `{p}`:
//!
//! - coordination container: `{p}owner.json` (the installation that owns the
//!   prefix), `{p}catalog.json` (the in-flight retention decision) and per set
//!   `{p}sets/{id}/record.json` plus an empty `{p}sets/{id}/tombstone` marker;
//! - data container: `{p}sets/{id}/objects/{sha256}` (content-addressed,
//!   created once) and `{p}sets/{id}/manifest.json`, written after every object.
//!
//! A set is restorable only when its record is `published`. Publication is an
//! ETag compare-and-swap from the exact `staging` revision this publisher
//! created, after the objects and the manifest were written and verified.
//! Records are never deleted (tombstones are retained), so a stale publisher
//! can neither recreate nor publish a collected set. Pins live in the record
//! and can only be added to a published set; deletion first moves an unpinned
//! published record to `delete_pending` by the same CAS, so a pin and a
//! deletion cannot both win. Blob leases are not used: the CAS already gives
//! the exclusion and a lease would only add liveness.
use super::*;
use crate::maintenance;
use serde::{Serialize, de::DeserializeOwned};
use std::io::{BufWriter, Read, Write};
use std::path::{Path as FilePath, PathBuf};

pub(super) mod azure;

pub(super) const RECORD_SCHEMA: &str = "vlite.cloud-backup-record/1";
pub(super) const MANIFEST_SCHEMA: &str = "vlite.cloud-backup-manifest/1";
const OWNER_SCHEMA: &str = "vlite.cloud-backup-owner/1";
const CATALOG_SCHEMA: &str = "vlite.cloud-backup-catalog/1";
/// Largest coordination object (record, owner, catalog, manifest) accepted.
const META_MAX: u64 = 1024 * 1024;
pub(super) const MAX_PINS: usize = 16;
/// Active (non-tombstoned) sets a listing accepts under one prefix.
const MAX_SETS: usize = 10_000;
pub(super) const ROLE_DATABASE: &str = "database";
pub(super) const ROLE_BUNDLE: &str = "recovery_bundle";

// ------------------------------------------------------------------ store

/// Outcome classes of one object-store request. `Unknown` means the request
/// may or may not have been applied (timeout, throttling, server error).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum StoreError {
    Conflict,
    NotFound,
    Unknown(String),
    Failed(String),
}

impl From<StoreError> for ApiError {
    fn from(error: StoreError) -> Self {
        match error {
            StoreError::Conflict => ApiError::new(
                StatusCode::CONFLICT,
                "cloud_conflict",
                "a conditional cloud write lost to a concurrent writer; retry",
            ),
            StoreError::NotFound => ApiError::new(
                StatusCode::NOT_FOUND,
                "cloud_object_not_found",
                "the cloud object does not exist",
            ),
            StoreError::Unknown(detail) => ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "cloud_store_unavailable",
                format!("the cloud store did not confirm the request ({detail}); retry"),
            ),
            StoreError::Failed(detail) => ApiError::new(
                StatusCode::BAD_GATEWAY,
                "cloud_store_failed",
                format!("the cloud store refused the request ({detail})"),
            ),
        }
    }
}

pub(super) enum Cond<'a> {
    /// Create only if absent (`If-None-Match: *`).
    Create,
    /// Replace only the exact revision (`If-Match: <etag>`).
    Match(&'a str),
}

pub(super) struct Blob {
    pub bytes: Vec<u8>,
    pub etag: String,
}

/// Blocking object store. Implementations never follow redirects, never log
/// credentials and keep ETags verbatim.
pub(super) trait ObjectStore: Send + Sync {
    /// Whole object, refused as `Failed` when larger than `max`.
    fn get(&self, name: &str, max: u64) -> Result<Blob, StoreError>;
    /// Streams the object into `out`; refused when larger than `max`.
    fn download(&self, name: &str, out: &mut dyn Write, max: u64) -> Result<u64, StoreError>;
    fn put(&self, name: &str, bytes: &[u8], cond: Cond<'_>) -> Result<String, StoreError>;
    fn put_file(&self, name: &str, path: &FilePath, cond: Cond<'_>) -> Result<String, StoreError>;
    /// Deleting a missing object succeeds.
    fn delete(&self, name: &str) -> Result<(), StoreError>;
    fn list(&self, prefix: &str) -> Result<Vec<String>, StoreError>;
}

// ----------------------------------------------------------------- target

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum AuthMode {
    /// Microsoft Entra token from the platform managed identity endpoint.
    #[default]
    ManagedIdentity,
    /// Account shared key, accepted only for a loopback emulator endpoint.
    EmulatorSharedKey,
}

/// Where the sets live. Holds no secret: managed identity needs none and the
/// emulator key is supplied per process from a file, never persisted.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Target {
    pub account: String,
    /// Data container: objects and manifests (WORM-capable; uploader needs no
    /// delete).
    pub container: String,
    /// Coordination container for records, pins and the catalog. Without it
    /// both roles share `container`, which cannot separate the permissions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coordination_container: Option<String>,
    pub prefix: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    #[serde(default)]
    pub auth: AuthMode,
    /// User-assigned managed identity client ID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
}

fn container_name(name: &str, field: &str) -> Result<(), String> {
    let ok = (3..=63).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !name.starts_with('-')
        && !name.ends_with('-')
        && !name.contains("--");
    if ok {
        Ok(())
    } else {
        Err(format!(
            "{field} must be a 3-63 character Azure container name (lowercase letters, digits, single hyphens)"
        ))
    }
}

pub(super) fn is_loopback(url: &reqwest::Url) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };
    let host = host.trim_start_matches('[').trim_end_matches(']');
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

impl Target {
    pub(super) fn validate(&self) -> Result<(), String> {
        if !(3..=24).contains(&self.account.len())
            || !self
                .account
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        {
            return Err("account must be 3-24 lowercase letters or digits".into());
        }
        container_name(&self.container, "container")?;
        if let Some(coordination) = &self.coordination_container {
            container_name(coordination, "coordination_container")?;
            if *coordination == self.container {
                return Err("coordination_container must differ from container".into());
            }
        }
        let p = &self.prefix;
        if p.is_empty()
            || p.len() > 200
            || !p.ends_with('/')
            || p.starts_with('/')
            || p.contains("//")
            || !p.bytes().all(|b| {
                b.is_ascii_lowercase()
                    || b.is_ascii_digit()
                    || matches!(b, b'.' | b'_' | b'/' | b'-')
            })
            || p.trim_end_matches('/')
                .split('/')
                .any(|segment| segment == "." || segment == "..")
        {
            return Err(
                "prefix must be 1-200 characters of lowercase letters, digits, '.', '_', '-' and '/', end with '/', and contain no empty, '.' or '..' segment"
                    .into(),
            );
        }
        self.endpoint_url()?;
        if let Some(id) = &self.client_id {
            if self.auth != AuthMode::ManagedIdentity {
                return Err("client_id applies only to managed_identity".into());
            }
            if id.len() != 36 || !id.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-') {
                return Err("client_id must be a managed identity client ID (GUID)".into());
            }
        }
        Ok(())
    }

    /// Blob service base URL without a trailing slash.
    pub(super) fn endpoint_url(&self) -> Result<String, String> {
        let text = match &self.endpoint {
            Some(endpoint) => endpoint.trim_end_matches('/').to_owned(),
            None => format!("https://{}.blob.core.windows.net", self.account),
        };
        if text.len() > 512 {
            return Err("endpoint is too long".into());
        }
        let url = reqwest::Url::parse(&text).map_err(|_| "endpoint must be an absolute URL")?;
        if url.query().is_some()
            || url.fragment().is_some()
            || !url.username().is_empty()
            || url.password().is_some()
        {
            return Err("endpoint must not carry a query, fragment or credentials".into());
        }
        let loopback = is_loopback(&url);
        match url.scheme() {
            "https" => {}
            "http" if loopback => {}
            _ => return Err("endpoint must use https (http only for a loopback emulator)".into()),
        }
        if self.auth == AuthMode::EmulatorSharedKey && !loopback {
            return Err("emulator_shared_key is accepted only with a loopback endpoint".into());
        }
        // A bearer token is sent with every request: never to another host.
        let public = format!("https://{}.blob.core.windows.net", self.account);
        if self.auth == AuthMode::ManagedIdentity && text != public {
            return Err(format!(
                "managed_identity sends tokens only to the account's public-cloud endpoint {public}; omit endpoint"
            ));
        }
        Ok(text)
    }
}

// ------------------------------------------------------------ identifiers

fn random_hex() -> Result<String, ApiError> {
    let mut bytes = [0u8; 8];
    getrandom::fill(&mut bytes).map_err(internal)?;
    Ok(hex(&bytes))
}

/// `YYYYMMDDTHHMMSSZ-<16 hex>`: sorts by creation time, unique per publisher.
pub(super) fn new_set_id(now_ms: u64) -> Result<String, ApiError> {
    let secs = now_ms / 1000;
    let (year, month, day) = compaction::civil((secs / 86_400) as i64);
    let t = secs % 86_400;
    Ok(format!(
        "{year:04}{month:02}{day:02}T{:02}{:02}{:02}Z-{}",
        t / 3600,
        t / 60 % 60,
        t % 60,
        random_hex()?
    ))
}

fn lower_hex(bytes: &[u8]) -> bool {
    bytes
        .iter()
        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b))
}

pub(super) fn valid_set_id(id: &str) -> bool {
    let b = id.as_bytes();
    b.len() == 33
        && b[..8].iter().all(u8::is_ascii_digit)
        && b[8] == b'T'
        && b[9..15].iter().all(u8::is_ascii_digit)
        && b[15] == b'Z'
        && b[16] == b'-'
        && lower_hex(&b[17..])
}

pub(super) fn valid_pin_id(id: &str) -> bool {
    id.len() == 20 && id.starts_with("pin-") && lower_hex(&id.as_bytes()[4..])
}

fn is_sha256(text: &str) -> bool {
    text.len() == 64 && lower_hex(text.as_bytes())
}

pub(super) fn lineage_hex(lineage: u64) -> String {
    format!("{lineage:016x}")
}

fn sha_hex(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

/// Writer that hashes and counts what passes through.
pub(super) struct Hashing<W> {
    inner: W,
    hasher: Sha256,
    bytes: u64,
}

impl<W: Write> Hashing<W> {
    pub(super) fn new(inner: W) -> Self {
        Self {
            inner,
            hasher: Sha256::new(),
            bytes: 0,
        }
    }

    pub(super) fn finish(self) -> (W, String, u64) {
        (self.inner, hex(&self.hasher.finalize()), self.bytes)
    }
}

impl<W: Write> Write for Hashing<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.hasher.update(&buf[..n]);
        self.bytes += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

fn hash_file(path: &FilePath) -> std::io::Result<(String, u64)> {
    let mut file = std::fs::File::open(path)?;
    let mut writer = Hashing::new(std::io::sink());
    std::io::copy(&mut file, &mut writer)?;
    let (_, sha, bytes) = writer.finish();
    Ok((sha, bytes))
}

// ---------------------------------------------------------------- records

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum SetState {
    Staging,
    Published,
    DeletePending,
    Tombstoned,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Pin {
    pub id: String,
    pub reason: String,
    pub created_unix_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct DrillResult {
    pub ok: bool,
    pub unix_ms: u64,
    pub detail: String,
}

/// Coordination record: the only mutable per-set object.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Record {
    pub schema: String,
    pub set_id: String,
    pub installation_id: String,
    pub state: SetState,
    pub created_unix_ms: u64,
    pub updated_unix_ms: u64,
    /// Random per write; a publisher only publishes its own staging revision.
    pub revision: String,
    pub manifest_sha256: String,
    pub pins: Vec<Pin>,
    pub last_drill: Option<DrillResult>,
    pub delete_reason: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct DatabaseSummary {
    pub format_version: u32,
    pub dimensions: usize,
    pub live_records: usize,
    pub commits: u64,
    pub collections: usize,
    pub open_imports: usize,
    pub bytes: u64,
    pub crc32: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ObjectEntry {
    pub role: String,
    pub sha256: String,
    pub bytes: u64,
}

/// Immutable set description, written after every object it names.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Manifest {
    pub schema: String,
    pub set_id: String,
    pub installation_id: String,
    pub created_unix_ms: u64,
    pub vlite_version: String,
    pub source_lineage: Option<String>,
    /// Always false in this profile; an encrypted manifest is refused.
    pub encrypted: bool,
    pub rekey_only: bool,
    pub database: DatabaseSummary,
    pub objects: Vec<ObjectEntry>,
}

impl Manifest {
    pub(super) fn new(
        set_id: &str,
        installation_id: &str,
        now: u64,
        verification: &maintenance::Verification,
        detail: &maintenance::InspectionDetail,
        uploads: &[Upload],
    ) -> Self {
        let i = &verification.inspection;
        Self {
            schema: MANIFEST_SCHEMA.into(),
            set_id: set_id.into(),
            installation_id: installation_id.into(),
            created_unix_ms: now,
            vlite_version: env!("CARGO_PKG_VERSION").into(),
            source_lineage: detail.lineage.map(lineage_hex),
            encrypted: false,
            rekey_only: true,
            database: DatabaseSummary {
                format_version: i.format_version,
                dimensions: i.dimensions,
                live_records: i.live_records,
                commits: i.commits,
                collections: detail.collections,
                open_imports: detail.open_imports,
                bytes: verification.bytes,
                crc32: verification.crc32,
            },
            objects: uploads
                .iter()
                .map(|u| ObjectEntry {
                    role: u.role.into(),
                    sha256: u.sha256.clone(),
                    bytes: u.bytes,
                })
                .collect(),
        }
    }

    /// Profile check of a downloaded manifest: `(database, bundle)` entries.
    fn check(
        &self,
        set_id: &str,
        installation_id: &str,
    ) -> Result<(&ObjectEntry, &ObjectEntry), ApiError> {
        if self.schema != MANIFEST_SCHEMA {
            return Err(corrupt("unsupported manifest schema"));
        }
        if self.set_id != set_id || self.installation_id != installation_id {
            return Err(corrupt("manifest does not describe this set"));
        }
        if self.encrypted || !self.rekey_only {
            return Err(corrupt(
                "manifest profile is not restorable by this version",
            ));
        }
        if self.objects.len() != 2 || self.objects.iter().any(|o| !is_sha256(&o.sha256)) {
            return Err(corrupt("manifest object list is invalid"));
        }
        let role = |name: &str| {
            self.objects
                .iter()
                .find(|o| o.role == name)
                .ok_or_else(|| corrupt("manifest object roles are invalid"))
        };
        Ok((role(ROLE_DATABASE)?, role(ROLE_BUNDLE)?))
    }
}

pub(super) enum Source {
    File(PathBuf),
    Bytes(Vec<u8>),
}

/// One object to upload, hashed before the upload starts.
pub(super) struct Upload {
    pub role: &'static str,
    pub source: Source,
    pub sha256: String,
    pub bytes: u64,
}

impl Upload {
    pub(super) fn file(role: &'static str, path: PathBuf) -> std::io::Result<Self> {
        let (sha256, bytes) = hash_file(&path)?;
        Ok(Self {
            role,
            source: Source::File(path),
            sha256,
            bytes,
        })
    }

    pub(super) fn bytes(role: &'static str, data: Vec<u8>) -> Self {
        Self {
            role,
            sha256: sha_hex(&data),
            bytes: data.len() as u64,
            source: Source::Bytes(data),
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Owner {
    schema: String,
    installation_id: String,
    created_unix_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Catalog {
    schema: String,
    revision: u64,
    updated_unix_ms: u64,
    decision: Option<Decision>,
}

/// A retention decision is recorded before any deletion so a second janitor
/// cannot act on a stale view and an interrupted one is resumed.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Decision {
    id: String,
    created_unix_ms: u64,
    delete: Vec<String>,
    survivors: Vec<String>,
}

/// Publication handle persisted while the final CAS result is unknown.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Pending {
    pub set_id: String,
    pub manifest_sha256: String,
    pub staging_revision: String,
}

pub(super) enum Publication {
    Published(Record),
    /// The record could not be read back; resolve later with the handle.
    Pending(Pending),
}

enum Observed {
    Published(Record),
    Staging(Record, String),
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Step {
    Write,
    Done,
    Skip,
}

#[derive(Serialize)]
pub(super) struct Listed {
    pub set_id: String,
    pub record: Option<Record>,
    pub error: Option<String>,
    pub data_objects: usize,
}

pub(super) struct Listing {
    /// Active sets, newest first.
    pub sets: Vec<Listed>,
    /// Marker-tombstoned set IDs with their remaining data object counts.
    pub tombstoned: Vec<(String, usize)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Retention {
    pub enabled: bool,
    pub keep_sets: usize,
    pub keep_days: u64,
    pub orphan_grace_hours: u64,
}

#[derive(Debug, Serialize)]
pub(super) struct Failure {
    pub set_id: String,
    pub code: &'static str,
    pub message: String,
}

#[derive(Debug, Default, Serialize)]
pub(super) struct Collection {
    pub decision: Option<String>,
    pub deleted: Vec<String>,
    pub orphans_deleted: Vec<String>,
    pub resumed: Vec<String>,
    pub skipped: Vec<String>,
    pub swept: Vec<String>,
    pub unreadable: Vec<String>,
    pub failures: Vec<Failure>,
}

impl Collection {
    fn fail(&mut self, set_id: &str, error: ApiError) {
        self.failures.push(Failure {
            set_id: set_id.into(),
            code: error.code,
            message: error.message,
        });
    }
}

pub(super) struct Fetched {
    pub record: Record,
    pub manifest: Manifest,
    pub database: PathBuf,
    pub bundle: recovery_bundle::Bundle,
    pub verification: maintenance::Verification,
}

enum Condemn {
    Retention(String),
    Orphan(u64),
}

fn conflict(code: &'static str, message: impl Into<String>) -> ApiError {
    ApiError::new(StatusCode::CONFLICT, code, message)
}

fn prefix_owned() -> ApiError {
    conflict(
        "cloud_prefix_owned",
        "the cloud prefix belongs to another installation; configure a new prefix",
    )
}

fn corrupt(message: impl Into<String>) -> ApiError {
    ApiError::new(StatusCode::BAD_GATEWAY, "cloud_set_corrupt", message)
}

fn not_found(code: &'static str, message: &str) -> ApiError {
    ApiError::new(StatusCode::NOT_FOUND, code, message)
}

fn unresolved(message: &str) -> ApiError {
    ApiError::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "cloud_store_unavailable",
        message,
    )
}

fn publication_lost() -> ApiError {
    conflict(
        "cloud_publication_lost",
        "the staging record changed before publication (collected or replaced); the set was not published",
    )
}

fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>, ApiError> {
    serde_json::to_vec_pretty(value).map_err(internal)
}

fn invalid(key: &str, problem: impl std::fmt::Display) -> ApiError {
    ApiError::new(
        StatusCode::BAD_GATEWAY,
        "cloud_object_invalid",
        format!("{key} is not a valid object: {problem}"),
    )
}

fn decode<T: DeserializeOwned>(key: &str, bytes: &[u8]) -> Result<T, ApiError> {
    serde_json::from_slice(bytes).map_err(|e| invalid(key, e))
}

fn decode_record(key: &str, bytes: &[u8], set_id: &str) -> Result<Record, ApiError> {
    let record: Record = decode(key, bytes)?;
    if record.schema != RECORD_SCHEMA || record.set_id != set_id {
        return Err(invalid(key, "unsupported schema or another set"));
    }
    Ok(record)
}

/// Creates `key` with `bytes`, or accepts an identical existing object (the
/// earlier attempt's result was unknown). A different object is a conflict.
fn create_verified(store: &dyn ObjectStore, key: &str, bytes: &[u8]) -> Result<String, ApiError> {
    for attempt in 0..2 {
        match store.put(key, bytes, Cond::Create) {
            Ok(etag) => return Ok(etag),
            Err(StoreError::Conflict | StoreError::Unknown(_)) => match store.get(key, META_MAX) {
                Ok(blob) if blob.bytes == bytes => return Ok(blob.etag),
                Ok(_) => {
                    return Err(conflict(
                        "cloud_object_conflict",
                        format!("{key} already exists with different content"),
                    ));
                }
                Err(StoreError::NotFound) if attempt == 0 => {}
                Err(e) => return Err(e.into()),
            },
            Err(e) => return Err(e.into()),
        }
    }
    Err(unresolved(
        "the cloud object creation could not be confirmed",
    ))
}

// ------------------------------------------------------------------- sets

/// The set protocol over a data store and a coordination store (the same
/// container when `shared`).
#[derive(Clone)]
pub(super) struct Sets {
    meta: Arc<dyn ObjectStore>,
    data: Arc<dyn ObjectStore>,
    shared: bool,
    prefix: String,
}

impl Sets {
    pub(super) fn new(
        meta: Arc<dyn ObjectStore>,
        data: Arc<dyn ObjectStore>,
        shared: bool,
        prefix: String,
    ) -> Self {
        Self {
            meta,
            data,
            shared,
            prefix,
        }
    }

    fn owner_key(&self) -> String {
        format!("{}owner.json", self.prefix)
    }

    fn catalog_key(&self) -> String {
        format!("{}catalog.json", self.prefix)
    }

    fn set_dir(&self, id: &str) -> String {
        format!("{}sets/{id}/", self.prefix)
    }

    fn record_key(&self, id: &str) -> String {
        format!("{}record.json", self.set_dir(id))
    }

    fn tombstone_key(&self, id: &str) -> String {
        format!("{}tombstone", self.set_dir(id))
    }

    fn manifest_key(&self, id: &str) -> String {
        format!("{}manifest.json", self.set_dir(id))
    }

    fn object_key(&self, id: &str, sha256: &str) -> String {
        format!("{}objects/{sha256}", self.set_dir(id))
    }

    /// The installation that owns the prefix, if any.
    pub(super) fn owner(&self) -> Result<Option<String>, ApiError> {
        let key = self.owner_key();
        match self.meta.get(&key, META_MAX) {
            Ok(blob) => {
                let owner: Owner = decode(&key, &blob.bytes)?;
                if owner.schema != OWNER_SCHEMA {
                    return Err(invalid(&key, "unsupported schema"));
                }
                Ok(Some(owner.installation_id))
            }
            Err(StoreError::NotFound) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Claims the prefix for `installation` or confirms the claim. Another
    /// installation's prefix (for example the source of a restore) is refused.
    pub(super) fn ensure_owner(&self, installation: &str, now: u64) -> Result<(), ApiError> {
        for _ in 0..3 {
            match self.owner()? {
                Some(owner) if owner == installation => return Ok(()),
                Some(_) => return Err(prefix_owned()),
                None => {
                    let bytes = encode(&Owner {
                        schema: OWNER_SCHEMA.into(),
                        installation_id: installation.into(),
                        created_unix_ms: now,
                    })?;
                    match self.meta.put(&self.owner_key(), &bytes, Cond::Create) {
                        Ok(_) => return Ok(()),
                        Err(StoreError::Conflict | StoreError::Unknown(_)) => {}
                        Err(e) => return Err(e.into()),
                    }
                }
            }
        }
        Err(unresolved("the prefix owner marker could not be confirmed"))
    }

    /// Writes a set: staging record, objects, manifest, then the publication
    /// CAS. Any failure before the CAS leaves an unrestorable staging set that
    /// collection removes after the orphan grace period.
    pub(super) fn publish(
        &self,
        manifest: &Manifest,
        uploads: &[Upload],
        now: u64,
    ) -> Result<Publication, ApiError> {
        let id = &manifest.set_id;
        if !valid_set_id(id) || manifest.objects.len() != uploads.len() {
            return Err(internal("invalid cloud backup manifest"));
        }
        let manifest_bytes = encode(manifest)?;
        let staging = Record {
            schema: RECORD_SCHEMA.into(),
            set_id: id.clone(),
            installation_id: manifest.installation_id.clone(),
            state: SetState::Staging,
            created_unix_ms: now,
            updated_unix_ms: now,
            revision: random_hex()?,
            manifest_sha256: sha_hex(&manifest_bytes),
            pins: Vec::new(),
            last_drill: None,
            delete_reason: None,
        };
        let etag = create_verified(&*self.meta, &self.record_key(id), &encode(&staging)?)?;
        for upload in uploads {
            self.upload(id, upload)?;
        }
        create_verified(&*self.data, &self.manifest_key(id), &manifest_bytes)?;
        let pending = Pending {
            set_id: id.clone(),
            manifest_sha256: staging.manifest_sha256.clone(),
            staging_revision: staging.revision.clone(),
        };
        self.finish(&pending, Some((staging, etag)), now)
    }

    /// Completes a publication whose final CAS result was unknown.
    pub(super) fn resolve_pending(
        &self,
        pending: &Pending,
        now: u64,
    ) -> Result<Publication, ApiError> {
        self.finish(pending, None, now)
    }

    fn upload(&self, id: &str, upload: &Upload) -> Result<(), ApiError> {
        let key = self.object_key(id, &upload.sha256);
        for attempt in 0..2 {
            let result = match &upload.source {
                Source::File(path) => self.data.put_file(&key, path, Cond::Create),
                Source::Bytes(bytes) => self.data.put(&key, bytes, Cond::Create),
            };
            match result {
                Ok(_) => return Ok(()),
                Err(StoreError::Conflict | StoreError::Unknown(_)) => {
                    let mut check = Hashing::new(std::io::sink());
                    match self.data.download(&key, &mut check, upload.bytes) {
                        Ok(_) => {
                            let (_, sha, bytes) = check.finish();
                            if sha == upload.sha256 && bytes == upload.bytes {
                                return Ok(());
                            }
                            return Err(conflict(
                                "cloud_object_conflict",
                                format!("{key} already exists with different content"),
                            ));
                        }
                        Err(StoreError::NotFound) if attempt == 0 => {}
                        Err(e) => return Err(e.into()),
                    }
                }
                Err(e) => return Err(e.into()),
            }
        }
        Err(unresolved("the object upload could not be confirmed"))
    }

    fn observe(&self, pending: &Pending) -> Result<Observed, ApiError> {
        let key = self.record_key(&pending.set_id);
        let blob = match self.meta.get(&key, META_MAX) {
            Ok(blob) => blob,
            Err(StoreError::Unknown(_)) => return Ok(Observed::Unknown),
            Err(StoreError::NotFound) => return Err(publication_lost()),
            Err(e) => return Err(e.into()),
        };
        let record = decode_record(&key, &blob.bytes, &pending.set_id)?;
        if record.manifest_sha256 != pending.manifest_sha256 {
            return Err(publication_lost());
        }
        match record.state {
            SetState::Published => Ok(Observed::Published(record)),
            SetState::Staging if record.revision == pending.staging_revision => {
                Ok(Observed::Staging(record, blob.etag))
            }
            _ => Err(publication_lost()),
        }
    }

    fn finish(
        &self,
        pending: &Pending,
        mut current: Option<(Record, String)>,
        now: u64,
    ) -> Result<Publication, ApiError> {
        let key = self.record_key(&pending.set_id);
        for _ in 0..3 {
            let (record, etag) = match current.take() {
                Some(current) => current,
                None => match self.observe(pending)? {
                    Observed::Published(record) => return Ok(Publication::Published(record)),
                    Observed::Staging(record, etag) => (record, etag),
                    Observed::Unknown => return Ok(Publication::Pending(pending.clone())),
                },
            };
            let mut next = record;
            next.state = SetState::Published;
            next.updated_unix_ms = now;
            next.revision = random_hex()?;
            match self.meta.put(&key, &encode(&next)?, Cond::Match(&etag)) {
                Ok(_) => return Ok(Publication::Published(next)),
                Err(StoreError::Conflict | StoreError::Unknown(_) | StoreError::NotFound) => {}
                Err(e) => return Err(e.into()),
            }
        }
        match self.observe(pending)? {
            Observed::Published(record) => Ok(Publication::Published(record)),
            _ => Ok(Publication::Pending(pending.clone())),
        }
    }

    /// Active sets newest first, plus marker-tombstoned sets. Objects without
    /// a readable record are listed with an error and never collected.
    pub(super) fn list(&self) -> Result<Listing, ApiError> {
        #[derive(Default)]
        struct Seen {
            record: bool,
            tombstone: bool,
            data: usize,
        }
        let root = format!("{}sets/", self.prefix);
        let meta_names = self.meta.list(&root)?;
        let data_names = if self.shared {
            meta_names.clone()
        } else {
            self.data.list(&root)?
        };
        let mut seen: BTreeMap<String, Seen> = BTreeMap::new();
        let split = |name: &str| -> Option<(String, String)> {
            let (id, tail) = name.strip_prefix(&root)?.split_once('/')?;
            valid_set_id(id).then(|| (id.to_owned(), tail.to_owned()))
        };
        for name in &meta_names {
            if let Some((id, tail)) = split(name) {
                let entry = seen.entry(id).or_default();
                match tail.as_str() {
                    "record.json" => entry.record = true,
                    "tombstone" => entry.tombstone = true,
                    _ => {}
                }
            }
        }
        for name in &data_names {
            if let Some((id, tail)) = split(name) {
                if self.shared && (tail == "record.json" || tail == "tombstone") {
                    continue;
                }
                seen.entry(id).or_default().data += 1;
            }
        }
        if seen.values().filter(|s| !s.tombstone).count() > MAX_SETS {
            return Err(conflict(
                "cloud_too_many_sets",
                format!("more than {MAX_SETS} active sets under the prefix; enable retention"),
            ));
        }
        let mut listing = Listing {
            sets: Vec::new(),
            tombstoned: Vec::new(),
        };
        for (id, s) in seen.into_iter().rev() {
            if s.tombstone {
                listing.tombstoned.push((id, s.data));
                continue;
            }
            let mut listed = Listed {
                set_id: id.clone(),
                record: None,
                error: None,
                data_objects: s.data,
            };
            if !s.record {
                listed.error = Some("data objects without a coordination record".into());
            } else {
                let key = self.record_key(&id);
                match self.meta.get(&key, META_MAX) {
                    Ok(blob) => match decode_record(&key, &blob.bytes, &id) {
                        Ok(record) => listed.record = Some(record),
                        Err(e) => listed.error = Some(e.message),
                    },
                    Err(StoreError::NotFound) => {
                        listed.error = Some("the coordination record disappeared".into());
                    }
                    Err(e) => return Err(e.into()),
                }
            }
            listing.sets.push(listed);
        }
        // IDs from the same second sort by their random suffix, so "newest"
        // (retention, listing) is the record's creation time, then the ID.
        listing.sets.sort_by_cached_key(|l| {
            std::cmp::Reverse((
                l.record.as_ref().map_or(0, |r| r.created_unix_ms),
                l.set_id.clone(),
            ))
        });
        Ok(listing)
    }

    /// Reads, changes and conditionally replaces one record. `f` sees whether
    /// no write was attempted yet, and changes the record only for `Write`.
    pub(super) fn update(
        &self,
        id: &str,
        now: u64,
        mut f: impl FnMut(&mut Record, bool) -> Result<Step, ApiError>,
    ) -> Result<(Record, Step), ApiError> {
        if !valid_set_id(id) {
            return Err(not_found(
                "cloud_set_not_found",
                "the backup set does not exist",
            ));
        }
        let key = self.record_key(id);
        let mut first = true;
        for _ in 0..4 {
            let blob = match self.meta.get(&key, META_MAX) {
                Ok(blob) => blob,
                Err(StoreError::Unknown(_)) => continue,
                Err(StoreError::NotFound) => {
                    return Err(not_found(
                        "cloud_set_not_found",
                        "the backup set does not exist",
                    ));
                }
                Err(e) => return Err(e.into()),
            };
            let mut record = decode_record(&key, &blob.bytes, id)?;
            match f(&mut record, first)? {
                Step::Write => {}
                step => return Ok((record, step)),
            }
            record.updated_unix_ms = now;
            record.revision = random_hex()?;
            match self
                .meta
                .put(&key, &encode(&record)?, Cond::Match(&blob.etag))
            {
                Ok(_) => return Ok((record, Step::Write)),
                Err(StoreError::Conflict | StoreError::Unknown(_) | StoreError::NotFound) => {
                    first = false;
                }
                Err(e) => return Err(e.into()),
            }
        }
        Err(conflict(
            "cloud_conflict",
            "the backup set record kept changing; retry",
        ))
    }

    /// Pins a published set. Pins never expire; they block deletion until
    /// removed and are required to download a set.
    pub(super) fn pin(&self, id: &str, reason: &str, now: u64) -> Result<Pin, ApiError> {
        if reason.trim().is_empty() || reason.len() > 200 || reason.chars().any(char::is_control) {
            return Err(unprocessable(
                "pin reason must be 1-200 characters without control characters",
            ));
        }
        let pin = Pin {
            id: format!("pin-{}", random_hex()?),
            reason: reason.into(),
            created_unix_ms: now,
        };
        self.update(id, now, |record, _| {
            if record.pins.iter().any(|p| p.id == pin.id) {
                return Ok(Step::Done);
            }
            if record.state != SetState::Published {
                return Err(conflict(
                    "cloud_set_unavailable",
                    "only a published backup set can be pinned",
                ));
            }
            if record.pins.len() >= MAX_PINS {
                return Err(conflict(
                    "cloud_pins_full",
                    format!("the backup set already has {MAX_PINS} pins; remove one first"),
                ));
            }
            record.pins.push(pin.clone());
            Ok(Step::Write)
        })?;
        Ok(pin)
    }

    pub(super) fn unpin(&self, id: &str, pin_id: &str, now: u64) -> Result<(), ApiError> {
        self.update(id, now, |record, first| {
            match record.pins.iter().position(|p| p.id == pin_id) {
                Some(index) => {
                    record.pins.remove(index);
                    Ok(Step::Write)
                }
                None if first => Err(not_found("cloud_pin_not_found", "the pin does not exist")),
                None => Ok(Step::Done),
            }
        })
        .map(|_| ())
    }

    /// Removes the drill's pin and records its result on a published set.
    pub(super) fn record_drill(
        &self,
        id: &str,
        pin_id: &str,
        result: &DrillResult,
        now: u64,
    ) -> Result<Record, ApiError> {
        self.update(id, now, |record, _| {
            record.pins.retain(|p| p.id != pin_id);
            if record.state == SetState::Published {
                record.last_drill = Some(result.clone());
            }
            Ok(Step::Write)
        })
        .map(|(record, _)| record)
    }

    /// Downloads a pinned published set into the empty directory `dir` and
    /// verifies it against its manifest: hashes, sizes, full database
    /// validation, CRC, counts, lineage and the recovery bundle.
    pub(super) fn fetch(
        &self,
        id: &str,
        pin_id: &str,
        dir: &FilePath,
    ) -> Result<Fetched, ApiError> {
        let key = self.record_key(id);
        let blob = match self.meta.get(&key, META_MAX) {
            Err(StoreError::NotFound) => {
                return Err(not_found(
                    "cloud_set_not_found",
                    "the backup set does not exist",
                ));
            }
            other => other?,
        };
        let record = decode_record(&key, &blob.bytes, id)?;
        if record.state != SetState::Published {
            return Err(conflict(
                "cloud_set_unpublished",
                "the backup set is not published and cannot be restored",
            ));
        }
        if !record.pins.iter().any(|p| p.id == pin_id) {
            return Err(conflict(
                "cloud_set_unavailable",
                "the backup set is not pinned for this download",
            ));
        }
        let missing = |e: StoreError| match e {
            StoreError::NotFound => corrupt("a published set object is missing"),
            e => e.into(),
        };
        let manifest_blob = self
            .data
            .get(&self.manifest_key(id), META_MAX)
            .map_err(missing)?;
        if sha_hex(&manifest_blob.bytes) != record.manifest_sha256 {
            return Err(corrupt("the manifest does not match the published record"));
        }
        let manifest: Manifest = serde_json::from_slice(&manifest_blob.bytes)
            .map_err(|e| corrupt(format!("the manifest is invalid: {e}")))?;
        let (db, bundle_entry) = manifest.check(id, &record.installation_id)?;
        if bundle_entry.bytes > recovery_bundle::MAX_BYTES {
            return Err(corrupt("the recovery bundle is too large"));
        }

        let database = dir.join("database.vlite");
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&database)
            .map_err(internal)?;
        let mut writer = Hashing::new(BufWriter::new(file));
        self.data
            .download(&self.object_key(id, &db.sha256), &mut writer, db.bytes)
            .map_err(missing)?;
        let (buffered, sha, bytes) = writer.finish();
        let file = buffered.into_inner().map_err(internal)?;
        file.sync_all().map_err(internal)?;
        drop(file);
        if bytes != db.bytes || sha != db.sha256 {
            return Err(corrupt("the database object does not match its hash"));
        }

        let bundle_blob = self
            .data
            .get(
                &self.object_key(id, &bundle_entry.sha256),
                bundle_entry.bytes,
            )
            .map_err(missing)?;
        if bundle_blob.bytes.len() as u64 != bundle_entry.bytes
            || sha_hex(&bundle_blob.bytes) != bundle_entry.sha256
        {
            return Err(corrupt("the recovery bundle does not match its hash"));
        }
        let bundle = recovery_bundle::parse(&bundle_blob.bytes).map_err(corrupt)?;

        let verification = maintenance::verify(&database)
            .map_err(|e| corrupt(format!("the database copy failed verification: {e}")))?;
        let summary = &manifest.database;
        let i = &verification.inspection;
        if verification.bytes != summary.bytes
            || verification.crc32 != summary.crc32
            || i.format_version != summary.format_version
            || i.dimensions != summary.dimensions
            || i.live_records != summary.live_records
            || i.commits != summary.commits
            || i.incomplete_tail_bytes != 0
        {
            return Err(corrupt("the database copy differs from its manifest"));
        }
        let detail = maintenance::inspect_detail(&database)
            .map_err(|e| corrupt(format!("the database copy failed inspection: {e}")))?;
        if detail.lineage.map(lineage_hex) != manifest.source_lineage
            || detail.collections != summary.collections
            || detail.open_imports != summary.open_imports
        {
            return Err(corrupt("the database structure differs from its manifest"));
        }
        if bundle.installation_id != manifest.installation_id
            || bundle.source_lineage != manifest.source_lineage
        {
            return Err(corrupt("the recovery bundle belongs to another capture"));
        }
        Ok(Fetched {
            record,
            manifest,
            database,
            bundle,
            verification,
        })
    }

    fn catalog(&self, now: u64) -> Result<(Catalog, String), ApiError> {
        let key = self.catalog_key();
        for _ in 0..3 {
            match self.meta.get(&key, META_MAX) {
                Ok(blob) => {
                    let catalog: Catalog = decode(&key, &blob.bytes)?;
                    if catalog.schema != CATALOG_SCHEMA {
                        return Err(invalid(&key, "unsupported schema"));
                    }
                    return Ok((catalog, blob.etag));
                }
                Err(StoreError::NotFound) => {
                    let catalog = Catalog {
                        schema: CATALOG_SCHEMA.into(),
                        revision: 0,
                        updated_unix_ms: now,
                        decision: None,
                    };
                    match self.meta.put(&key, &encode(&catalog)?, Cond::Create) {
                        Ok(etag) => return Ok((catalog, etag)),
                        Err(StoreError::Conflict | StoreError::Unknown(_)) => {}
                        Err(e) => return Err(e.into()),
                    }
                }
                Err(StoreError::Unknown(_)) => {}
                Err(e) => return Err(e.into()),
            }
        }
        Err(unresolved("the retention catalog could not be read"))
    }

    /// Replaces the catalog revision `etag`. A concurrent change stops the
    /// collection; an unknown result is resolved by reading the decision back.
    fn write_catalog(
        &self,
        catalog: &Catalog,
        etag: &str,
        now: u64,
    ) -> Result<(Catalog, String), ApiError> {
        let key = self.catalog_key();
        match self.meta.put(&key, &encode(catalog)?, Cond::Match(etag)) {
            Ok(etag) => Ok((catalog.clone(), etag)),
            Err(StoreError::Conflict | StoreError::NotFound) => Err(conflict(
                "cloud_collection_conflict",
                "another collection changed the retention catalog; retry",
            )),
            Err(StoreError::Unknown(_)) => {
                let (current, etag) = self.catalog(now)?;
                let mine = current.decision.as_ref().map(|d| &d.id)
                    == catalog.decision.as_ref().map(|d| &d.id)
                    && current.revision == catalog.revision;
                if mine {
                    Ok((current, etag))
                } else {
                    Err(unresolved(
                        "the retention catalog update could not be confirmed",
                    ))
                }
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Moves a set to `delete_pending` (if `why` still applies) and finishes
    /// the deletion. Returns false when the set no longer qualifies.
    fn delete_set(&self, id: &str, why: &Condemn, now: u64) -> Result<bool, ApiError> {
        let (record, step) = self.update(id, now, |record, _| {
            let condemn = match (record.state, why) {
                (SetState::Published, Condemn::Retention(_)) => record.pins.is_empty(),
                (SetState::Staging, Condemn::Orphan(grace_ms)) => {
                    record.updated_unix_ms.saturating_add(*grace_ms) <= now
                }
                (SetState::DeletePending | SetState::Tombstoned, _) => return Ok(Step::Done),
                _ => false,
            };
            if !condemn {
                return Ok(Step::Skip);
            }
            record.state = SetState::DeletePending;
            record.delete_reason = Some(match why {
                Condemn::Retention(decision) => format!("retention decision {decision}"),
                Condemn::Orphan(_) => "unpublished staging set past the orphan grace period".into(),
            });
            Ok(Step::Write)
        })?;
        match (step, record.state) {
            (Step::Skip, _) => Ok(false),
            (_, SetState::Tombstoned) => Ok(true),
            _ => {
                self.finish_delete(id, now)?;
                Ok(true)
            }
        }
    }

    fn data_names(&self, id: &str) -> Result<Vec<String>, ApiError> {
        let dir = self.set_dir(id);
        let mut names = self.data.list(&dir)?;
        if self.shared {
            let (record, tombstone) = (self.record_key(id), self.tombstone_key(id));
            names.retain(|name| *name != record && *name != tombstone);
        }
        Ok(names)
    }

    /// Deletes a set's data objects, manifest first. Returns the count.
    fn sweep(&self, id: &str) -> Result<usize, ApiError> {
        let manifest = self.manifest_key(id);
        let mut names = self.data_names(id)?;
        names.sort_by_key(|name| *name != manifest);
        for name in &names {
            self.data.delete(name)?;
        }
        Ok(names.len())
    }

    fn mark(&self, id: &str) -> Result<(), ApiError> {
        match self.meta.put(&self.tombstone_key(id), b"", Cond::Create) {
            Ok(_) | Err(StoreError::Conflict) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    fn finish_delete(&self, id: &str, now: u64) -> Result<(), ApiError> {
        self.sweep(id)?;
        self.update(id, now, |record, _| match record.state {
            SetState::DeletePending => {
                record.state = SetState::Tombstoned;
                record.pins.clear();
                Ok(Step::Write)
            }
            SetState::Tombstoned => Ok(Step::Done),
            _ => Err(conflict(
                "cloud_conflict",
                "the backup set left delete_pending unexpectedly",
            )),
        })?;
        self.mark(id)
    }

    /// Janitor pass: resumes an interrupted retention decision, removes
    /// expired own staging sets, finishes pending deletions, sweeps objects a
    /// stale writer added after a tombstone and, when enabled, applies
    /// retention. Per-set failures are reported and do not stop the pass.
    pub(super) fn collect(
        &self,
        installation: &str,
        retention: &Retention,
        now: u64,
    ) -> Result<Collection, ApiError> {
        let mut out = Collection::default();
        // Only the prefix owner collects: a restored copy pointed at the old
        // prefix must never apply its retention to the original's sets.
        if self.owner()?.is_some_and(|owner| owner != installation) {
            return Err(prefix_owned());
        }
        let (mut catalog, mut etag) = self.catalog(now)?;
        if let Some(decision) = catalog.decision.clone() {
            let why = Condemn::Retention(decision.id.clone());
            for id in &decision.delete {
                match self.delete_set(id, &why, now) {
                    Ok(true) => out.resumed.push(id.clone()),
                    Ok(false) => out.skipped.push(id.clone()),
                    Err(e) => out.fail(id, e),
                }
            }
            catalog.decision = None;
            catalog.revision += 1;
            catalog.updated_unix_ms = now;
            (catalog, etag) = self.write_catalog(&catalog, &etag, now)?;
        }

        let listing = self.list()?;
        let grace_ms = retention.orphan_grace_hours.saturating_mul(3_600_000);
        for listed in &listing.sets {
            let id = &listed.set_id;
            let Some(record) = &listed.record else {
                out.unreadable.push(id.clone());
                continue;
            };
            let result = match record.state {
                SetState::Staging
                    if record.installation_id == installation
                        && record.updated_unix_ms.saturating_add(grace_ms) <= now =>
                {
                    self.delete_set(id, &Condemn::Orphan(grace_ms), now)
                        .map(|done| {
                            if done {
                                out.orphans_deleted.push(id.clone());
                            }
                        })
                }
                SetState::DeletePending => self
                    .finish_delete(id, now)
                    .map(|()| out.deleted.push(id.clone())),
                SetState::Tombstoned => self.sweep(id).and_then(|swept| {
                    if swept > 0 {
                        out.swept.push(id.clone());
                    }
                    self.mark(id)
                }),
                _ => Ok(()),
            };
            if let Err(e) = result {
                out.fail(id, e);
            }
        }
        for (id, data) in &listing.tombstoned {
            if *data > 0 {
                match self.sweep(id) {
                    Ok(_) => out.swept.push(id.clone()),
                    Err(e) => out.fail(id, e),
                }
            }
        }

        if !retention.enabled {
            return Ok(out);
        }
        let published: Vec<(&String, &Record)> = listing
            .sets
            .iter()
            .filter_map(|l| l.record.as_ref().map(|r| (&l.set_id, r)))
            .filter(|(_, r)| r.state == SetState::Published && r.installation_id == installation)
            .collect();
        let newest_drilled = published
            .iter()
            .find(|(_, r)| r.last_drill.as_ref().is_some_and(|d| d.ok))
            .map(|(id, _)| *id);
        let keep_ms = retention.keep_days.saturating_mul(86_400_000);
        let (mut delete, mut survivors) = (Vec::new(), Vec::new());
        for (index, (id, record)) in published.iter().enumerate() {
            let protected = index < retention.keep_sets
                || now.saturating_sub(record.created_unix_ms) < keep_ms
                || !record.pins.is_empty()
                || Some(*id) == newest_drilled;
            if protected {
                survivors.push((*id).clone());
            } else {
                delete.push((*id).clone());
            }
        }
        if delete.is_empty() {
            return Ok(out);
        }
        let decision = Decision {
            id: random_hex()?,
            created_unix_ms: now,
            delete,
            survivors,
        };
        catalog.decision = Some(decision.clone());
        catalog.revision += 1;
        catalog.updated_unix_ms = now;
        (catalog, etag) = self.write_catalog(&catalog, &etag, now)?;
        out.decision = Some(decision.id.clone());
        let why = Condemn::Retention(decision.id.clone());
        for id in &decision.delete {
            match self.delete_set(id, &why, now) {
                Ok(true) => out.deleted.push(id.clone()),
                Ok(false) => out.skipped.push(id.clone()),
                Err(e) => out.fail(id, e),
            }
        }
        catalog.decision = None;
        catalog.revision += 1;
        catalog.updated_unix_ms = now;
        if let Err(e) = self.write_catalog(&catalog, &etag, now) {
            // The next pass resumes the recorded decision.
            out.fail("", e);
        }
        Ok(out)
    }
}

// ------------------------------------------------------------------- fake

/// Deterministic in-memory store with fault injection for protocol tests.
#[cfg(test)]
pub(super) mod fake {
    use super::*;

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum Op {
        Get,
        Put,
        Delete,
        List,
    }

    /// `Before`: unknown result, not applied. `After`: applied, then an
    /// unknown result. `Refuse`: a definite failure, not applied.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum Fault {
        Before,
        After,
        Refuse,
    }

    struct Rule {
        op: Op,
        needle: String,
        fault: Fault,
        skip: usize,
        times: usize,
    }

    type Hook = (Op, String, Box<dyn FnOnce() + Send>);

    #[derive(Default)]
    pub struct Memory {
        objects: Mutex<BTreeMap<String, (Vec<u8>, u64)>>,
        counter: AtomicU64,
        rules: Mutex<Vec<Rule>>,
        hook: Mutex<Option<Hook>>,
    }

    impl Memory {
        /// Fails the matching operations after skipping `skip` of them.
        pub fn fault(&self, op: Op, needle: &str, fault: Fault, skip: usize, times: usize) {
            self.rules.lock().unwrap().push(Rule {
                op,
                needle: needle.into(),
                fault,
                skip,
                times,
            });
        }

        /// Runs `f` once, just before the next matching operation.
        pub fn hook(&self, op: Op, needle: &str, f: impl FnOnce() + Send + 'static) {
            *self.hook.lock().unwrap() = Some((op, needle.into(), Box::new(f)));
        }

        pub fn names(&self) -> Vec<String> {
            self.objects.lock().unwrap().keys().cloned().collect()
        }

        pub fn raw(&self, name: &str) -> Option<Vec<u8>> {
            self.objects
                .lock()
                .unwrap()
                .get(name)
                .map(|(b, _)| b.clone())
        }

        pub fn set_raw(&self, name: &str, bytes: &[u8]) {
            let tag = self.counter.fetch_add(1, Ordering::SeqCst) + 1;
            self.objects
                .lock()
                .unwrap()
                .insert(name.into(), (bytes.to_vec(), tag));
        }

        fn check(&self, op: Op, name: &str) -> Option<Fault> {
            let hook = {
                let mut slot = self.hook.lock().unwrap();
                if slot
                    .as_ref()
                    .is_some_and(|(o, n, _)| *o == op && name.contains(n.as_str()))
                {
                    slot.take()
                } else {
                    None
                }
            };
            if let Some((_, _, f)) = hook {
                f();
            }
            let mut rules = self.rules.lock().unwrap();
            for rule in rules.iter_mut() {
                if rule.op == op && rule.times > 0 && name.contains(rule.needle.as_str()) {
                    if rule.skip > 0 {
                        rule.skip -= 1;
                        continue;
                    }
                    rule.times -= 1;
                    return Some(rule.fault);
                }
            }
            None
        }

        fn read_fault(fault: Option<Fault>) -> Result<(), StoreError> {
            match fault {
                None => Ok(()),
                Some(Fault::Refuse) => Err(StoreError::Failed("injected refusal".into())),
                Some(_) => Err(StoreError::Unknown("injected".into())),
            }
        }

        fn etag(tag: u64) -> String {
            format!("\"{tag}\"")
        }
    }

    impl ObjectStore for Memory {
        fn get(&self, name: &str, max: u64) -> Result<Blob, StoreError> {
            Self::read_fault(self.check(Op::Get, name))?;
            let objects = self.objects.lock().unwrap();
            let (bytes, tag) = objects.get(name).ok_or(StoreError::NotFound)?;
            if bytes.len() as u64 > max {
                return Err(StoreError::Failed("object too large".into()));
            }
            Ok(Blob {
                bytes: bytes.clone(),
                etag: Self::etag(*tag),
            })
        }

        fn download(&self, name: &str, out: &mut dyn Write, max: u64) -> Result<u64, StoreError> {
            let blob = self.get(name, max)?;
            out.write_all(&blob.bytes)
                .map_err(|e| StoreError::Failed(e.to_string()))?;
            Ok(blob.bytes.len() as u64)
        }

        fn put(&self, name: &str, bytes: &[u8], cond: Cond<'_>) -> Result<String, StoreError> {
            let fault = self.check(Op::Put, name);
            match fault {
                Some(Fault::Before) => return Err(StoreError::Unknown("injected".into())),
                Some(Fault::Refuse) => return Err(StoreError::Failed("injected refusal".into())),
                _ => {}
            }
            let mut objects = self.objects.lock().unwrap();
            match (cond, objects.get(name)) {
                (Cond::Create, Some(_)) => return Err(StoreError::Conflict),
                (Cond::Match(_), None) => return Err(StoreError::Conflict),
                (Cond::Match(etag), Some((_, tag))) if Self::etag(*tag) != etag => {
                    return Err(StoreError::Conflict);
                }
                _ => {}
            }
            let tag = self.counter.fetch_add(1, Ordering::SeqCst) + 1;
            objects.insert(name.into(), (bytes.to_vec(), tag));
            if fault == Some(Fault::After) {
                return Err(StoreError::Unknown("injected".into()));
            }
            Ok(Self::etag(tag))
        }

        fn put_file(
            &self,
            name: &str,
            path: &FilePath,
            cond: Cond<'_>,
        ) -> Result<String, StoreError> {
            let bytes = std::fs::read(path).map_err(|e| StoreError::Failed(e.to_string()))?;
            self.put(name, &bytes, cond)
        }

        fn delete(&self, name: &str) -> Result<(), StoreError> {
            Self::read_fault(self.check(Op::Delete, name))?;
            self.objects.lock().unwrap().remove(name);
            Ok(())
        }

        fn list(&self, prefix: &str) -> Result<Vec<String>, StoreError> {
            Self::read_fault(self.check(Op::List, prefix))?;
            Ok(self
                .objects
                .lock()
                .unwrap()
                .keys()
                .filter(|k| k.starts_with(prefix))
                .cloned()
                .collect())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::{Fault, Memory, Op};
    use super::*;

    const INSTALL: &str = "0123456789abcdef";
    const HOUR: u64 = 3_600_000;
    const BASE: u64 = 1_700_000_000_000;

    struct Env {
        meta: Arc<Memory>,
        data: Arc<Memory>,
        sets: Sets,
        db: tempfile::TempDir,
    }

    fn setup() -> Env {
        let meta = Arc::new(Memory::default());
        let data = Arc::new(Memory::default());
        let sets = Sets::new(meta.clone(), data.clone(), false, "vlite/prod/".into());
        let db = tempfile::tempdir().unwrap();
        let path = db.path().join("source.vlite");
        let mut database = Database::create(&path, 3).unwrap();
        for id in 1..=5u64 {
            database
                .put(id, &[id as f32, 0.5, -1.0], format!("p{id}").as_bytes())
                .unwrap();
        }
        drop(database);
        Env {
            meta,
            data,
            sets,
            db,
        }
    }

    fn bundle_bytes(lineage: Option<u64>) -> Vec<u8> {
        let config: recovery_bundle::Config = serde_json::from_value(json!({
            "tenants": [], "accounts": [], "credentials": [],
            "webhook_subscriptions": 0, "secret_references": [],
            "excluded": recovery_bundle::EXCLUDED,
        }))
        .unwrap();
        serde_json::to_vec(&recovery_bundle::bundle(config, INSTALL, lineage, 1)).unwrap()
    }

    fn prepare(env: &Env, now: u64) -> (Manifest, Vec<Upload>) {
        prepare_as(env, now, &new_set_id(now).unwrap())
    }

    fn prepare_as(env: &Env, now: u64, id: &str) -> (Manifest, Vec<Upload>) {
        let path = env.db.path().join("source.vlite");
        let verification = maintenance::verify(&path).unwrap();
        let detail = maintenance::inspect_detail(&path).unwrap();
        let uploads = vec![
            Upload::file(ROLE_DATABASE, path).unwrap(),
            Upload::bytes(ROLE_BUNDLE, bundle_bytes(detail.lineage)),
        ];
        let manifest = Manifest::new(id, INSTALL, now, &verification, &detail, &uploads);
        (manifest, uploads)
    }

    fn publish(env: &Env, now: u64) -> String {
        let (manifest, uploads) = prepare(env, now);
        match env.sets.publish(&manifest, &uploads, now).unwrap() {
            Publication::Published(record) => {
                assert_eq!(record.state, SetState::Published);
                record.set_id
            }
            Publication::Pending(_) => panic!("unexpected pending publication"),
        }
    }

    fn record(env: &Env, id: &str) -> Record {
        let key = env.sets.record_key(id);
        serde_json::from_slice(&env.meta.raw(&key).unwrap()).unwrap()
    }

    fn data_objects(env: &Env, id: &str) -> usize {
        let dir = env.sets.set_dir(id);
        env.data
            .names()
            .iter()
            .filter(|n| n.starts_with(&dir))
            .count()
    }

    fn retention(enabled: bool) -> Retention {
        Retention {
            enabled,
            keep_sets: 2,
            keep_days: 0,
            orphan_grace_hours: 1,
        }
    }

    #[test]
    fn set_ids_sort_by_time_and_validate() {
        let a = new_set_id(BASE).unwrap();
        let b = new_set_id(BASE + 1000).unwrap();
        assert!(a.starts_with("20231114T221320Z-"), "{a}");
        assert!(valid_set_id(&a) && valid_set_id(&b) && a < b);
        assert!(!valid_set_id("20231114T221320Z-ABCDEF0123456789"));
        assert!(!valid_set_id("../x"));
        assert!(valid_pin_id("pin-0123456789abcdef"));
        assert!(!valid_pin_id("pin-0123"));
    }

    #[test]
    fn target_validation_refuses_unsafe_shapes() {
        let good = Target {
            account: "vlitebackup".into(),
            container: "vlite-data".into(),
            coordination_container: Some("vlite-coord".into()),
            prefix: "prod/eastus/".into(),
            endpoint: None,
            auth: AuthMode::ManagedIdentity,
            client_id: Some("01234567-89ab-cdef-0123-456789abcdef".into()),
        };
        assert!(good.validate().is_ok());
        assert_eq!(
            good.endpoint_url().unwrap(),
            "https://vlitebackup.blob.core.windows.net"
        );
        let emulator = Target {
            endpoint: Some("http://127.0.0.1:10000/devstoreaccount1/".into()),
            auth: AuthMode::EmulatorSharedKey,
            client_id: None,
            ..good.clone()
        };
        assert!(emulator.validate().is_ok());
        assert_eq!(
            emulator.endpoint_url().unwrap(),
            "http://127.0.0.1:10000/devstoreaccount1"
        );
        let bad: [fn(&mut Target); 13] = [
            |t| t.account = "UPPER".into(),
            |t| t.container = "a--b".into(),
            |t| t.coordination_container = Some("vlite-data".into()),
            |t| t.prefix = "prod".into(),
            |t| t.prefix = "/prod/".into(),
            |t| t.prefix = "prod//x/".into(),
            |t| t.prefix = "prod/../x/".into(),
            |t| t.prefix = "Prod/".into(),
            |t| t.endpoint = Some("http://example.com".into()),
            |t| t.endpoint = Some("https://example.com/?sig=x".into()),
            |t| t.endpoint = Some("https://user:pw@example.com".into()),
            |t| t.client_id = Some("not-a-guid".into()),
            |t| t.auth = AuthMode::EmulatorSharedKey,
        ];
        for (index, change) in bad.iter().enumerate() {
            let mut target = good.clone();
            change(&mut target);
            assert!(target.validate().is_err(), "case {index}");
        }
        let unknown = json!({"account":"a1b","container":"abc","prefix":"p/","sas":"x"});
        assert!(serde_json::from_value::<Target>(unknown).is_err());
    }

    #[test]
    fn managed_identity_tokens_go_only_to_the_account_blob_host() {
        let base = Target {
            account: "vlitebackup".into(),
            container: "vlite-data".into(),
            coordination_container: None,
            prefix: "prod/".into(),
            endpoint: None,
            auth: AuthMode::ManagedIdentity,
            client_id: None,
        };
        let with = |endpoint: &str| Target {
            endpoint: Some(endpoint.into()),
            ..base.clone()
        };
        assert!(base.validate().is_ok());
        assert!(
            with("https://vlitebackup.blob.core.windows.net/")
                .validate()
                .is_ok()
        );
        for endpoint in [
            "https://attacker.example.com",
            "https://other.blob.core.windows.net",
            "https://vlitebackup.blob.core.windows.net.example.com",
            "https://vlitebackup.blob.core.windows.net/devstoreaccount1",
            "https://vlitebackup.blob.core.windows.net:8443",
            "http://127.0.0.1:10000/devstoreaccount1",
            "https://localhost/",
        ] {
            let error = with(endpoint).validate().unwrap_err();
            assert!(error.contains("managed_identity"), "{endpoint}: {error}");
        }
    }

    #[test]
    fn published_set_round_trips_through_a_pinned_fetch() {
        let env = setup();
        let id = publish(&env, BASE);
        let listing = env.sets.list().unwrap();
        assert_eq!(listing.sets.len(), 1);
        assert_eq!(listing.sets[0].data_objects, 3);
        let dir = tempfile::tempdir().unwrap();
        let pin = env.sets.pin(&id, "restore drill", BASE + 1).unwrap();
        let fetched = env.sets.fetch(&id, &pin.id, dir.path()).unwrap();
        assert_eq!(fetched.verification.inspection.live_records, 5);
        assert_eq!(fetched.bundle.installation_id, INSTALL);
        assert_eq!(
            fetched.manifest.source_lineage,
            maintenance::inspect_detail(&fetched.database)
                .unwrap()
                .lineage
                .map(lineage_hex)
        );
        assert!(fetched.manifest.source_lineage.is_some());
        env.sets.unpin(&id, &pin.id, BASE + 2).unwrap();
        assert!(record(&env, &id).pins.is_empty());
    }

    #[test]
    fn fetch_requires_a_pin_and_detects_tampering() {
        let env = setup();
        let id = publish(&env, BASE);
        let dir = tempfile::tempdir().unwrap();
        let err = env
            .sets
            .fetch(&id, "pin-0000000000000000", dir.path())
            .err()
            .unwrap();
        assert_eq!(err.code, "cloud_set_unavailable");
        let pin = env.sets.pin(&id, "check", BASE).unwrap();
        let (manifest, _) = prepare(&env, BASE);
        let key = env.sets.object_key(&id, &manifest.objects[0].sha256);
        let mut bytes = env.data.raw(&key).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 0xff;
        env.data.set_raw(&key, &bytes);
        let err = env.sets.fetch(&id, &pin.id, dir.path()).err().unwrap();
        assert_eq!(err.code, "cloud_set_corrupt");
    }

    #[test]
    fn failed_publication_is_never_restorable_and_is_orphan_collected() {
        for needle in ["/objects/", "manifest.json"] {
            let env = setup();
            env.data.fault(Op::Put, needle, Fault::Refuse, 0, 1);
            let (manifest, uploads) = prepare(&env, BASE);
            let err = env.sets.publish(&manifest, &uploads, BASE).err().unwrap();
            assert_eq!(err.code, "cloud_store_failed");
            let id = manifest.set_id.clone();
            assert_eq!(record(&env, &id).state, SetState::Staging);
            let err = env.sets.pin(&id, "restore", BASE).err().unwrap();
            assert_eq!(err.code, "cloud_set_unavailable");
            // Within the grace period nothing is collected.
            let early = env
                .sets
                .collect(INSTALL, &retention(false), BASE + HOUR / 2)
                .unwrap();
            assert!(early.orphans_deleted.is_empty());
            let out = env
                .sets
                .collect(INSTALL, &retention(false), BASE + 2 * HOUR)
                .unwrap();
            assert_eq!(out.orphans_deleted, vec![id.clone()]);
            assert_eq!(record(&env, &id).state, SetState::Tombstoned);
            assert_eq!(data_objects(&env, &id), 0);
            assert!(env.meta.raw(&env.sets.tombstone_key(&id)).is_some());
        }
    }

    #[test]
    fn unknown_publication_results_converge() {
        // Applied but unconfirmed: the read-back proves publication.
        let env = setup();
        env.meta.fault(Op::Put, "record.json", Fault::After, 1, 1);
        let id = publish(&env, BASE);
        assert_eq!(record(&env, &id).state, SetState::Published);

        // Not applied and unreadable: a pending handle, resolved later.
        let env = setup();
        env.meta.fault(Op::Put, "record.json", Fault::Before, 1, 1);
        env.meta.fault(Op::Get, "record.json", Fault::Before, 0, 1);
        let (manifest, uploads) = prepare(&env, BASE);
        let pending = match env.sets.publish(&manifest, &uploads, BASE).unwrap() {
            Publication::Pending(pending) => pending,
            Publication::Published(_) => panic!("expected pending"),
        };
        assert_eq!(record(&env, &pending.set_id).state, SetState::Staging);
        match env.sets.resolve_pending(&pending, BASE + 1).unwrap() {
            Publication::Published(record) => assert_eq!(record.set_id, pending.set_id),
            Publication::Pending(_) => panic!("expected publication"),
        }
        // Resolving again is idempotent.
        assert!(matches!(
            env.sets.resolve_pending(&pending, BASE + 2).unwrap(),
            Publication::Published(_)
        ));
    }

    #[test]
    fn unknown_object_and_record_creation_is_verified_not_duplicated() {
        let env = setup();
        env.data.fault(Op::Put, "/objects/", Fault::After, 0, 2);
        env.meta.fault(Op::Put, "record.json", Fault::After, 0, 1);
        env.data.fault(Op::Put, "manifest.json", Fault::After, 0, 1);
        let id = publish(&env, BASE);
        assert_eq!(data_objects(&env, &id), 3);
    }

    #[test]
    fn a_collected_staging_set_cannot_be_published_or_recreated() {
        let env = setup();
        env.data
            .fault(Op::Put, "manifest.json", Fault::Refuse, 0, 1);
        let (manifest, uploads) = prepare(&env, BASE);
        assert!(env.sets.publish(&manifest, &uploads, BASE).is_err());
        let id = manifest.set_id.clone();
        let staging = record(&env, &id);
        env.sets
            .collect(INSTALL, &retention(false), BASE + 2 * HOUR)
            .unwrap();
        // The paused publisher resumes after collection.
        let pending = Pending {
            set_id: id.clone(),
            manifest_sha256: staging.manifest_sha256.clone(),
            staging_revision: staging.revision.clone(),
        };
        let err = env
            .sets
            .resolve_pending(&pending, BASE + 3 * HOUR)
            .err()
            .unwrap();
        assert_eq!(err.code, "cloud_publication_lost");
        let err = env.sets.publish(&manifest, &uploads, BASE).err().unwrap();
        assert_eq!(err.code, "cloud_object_conflict");
        assert_eq!(record(&env, &id).state, SetState::Tombstoned);
        // Objects a stale writer adds after the tombstone are swept.
        let late = env.sets.object_key(&id, &uploads[1].sha256);
        env.data.set_raw(&late, b"late");
        let out = env
            .sets
            .collect(INSTALL, &retention(false), BASE + 4 * HOUR)
            .unwrap();
        assert_eq!(out.swept, vec![id.clone()]);
        assert_eq!(data_objects(&env, &id), 0);
    }

    #[test]
    fn sets_from_one_second_are_ordered_by_creation_time() {
        let env = setup();
        let second = &new_set_id(BASE).unwrap()[..17];
        // Later sets get lower random suffixes than earlier ones.
        let ids: Vec<String> = ["ffffffffffffffff", "8888888888888888", "0000000000000000"]
            .iter()
            .enumerate()
            .map(|(i, suffix)| {
                let (id, now) = (format!("{second}{suffix}"), BASE + i as u64);
                let (manifest, uploads) = prepare_as(&env, now, &id);
                let published = env.sets.publish(&manifest, &uploads, now).unwrap();
                assert!(matches!(published, Publication::Published(_)));
                id
            })
            .collect();
        let listed: Vec<String> = env
            .sets
            .list()
            .unwrap()
            .sets
            .into_iter()
            .map(|l| l.set_id)
            .collect();
        assert_eq!(listed, vec![ids[2].clone(), ids[1].clone(), ids[0].clone()]);
        let out = env
            .sets
            .collect(INSTALL, &retention(true), BASE + 86_400_000)
            .unwrap();
        assert_eq!(out.deleted, vec![ids[0].clone()], "the oldest set goes");
    }

    #[test]
    fn retention_keeps_newest_pinned_and_last_drilled_sets() {
        let env = setup();
        let ids: Vec<String> = (0..5).map(|i| publish(&env, BASE + i * 1000)).collect();
        env.sets.pin(&ids[0], "legal", BASE).unwrap();
        let drill = env.sets.pin(&ids[1], "drill", BASE).unwrap();
        let ok = DrillResult {
            ok: true,
            unix_ms: BASE,
            detail: "ok".into(),
        };
        env.sets
            .record_drill(&ids[1], &drill.id, &ok, BASE)
            .unwrap();
        // Age protection keeps everything.
        let young = Retention {
            keep_days: 1,
            ..retention(true)
        };
        let out = env.sets.collect(INSTALL, &young, BASE + HOUR).unwrap();
        assert!(out.deleted.is_empty() && out.decision.is_none());
        let out = env
            .sets
            .collect(INSTALL, &retention(true), BASE + 3 * 86_400_000)
            .unwrap();
        assert_eq!(out.deleted, vec![ids[2].clone()]);
        assert!(out.decision.is_some() && out.failures.is_empty());
        assert_eq!(record(&env, &ids[2]).state, SetState::Tombstoned);
        assert_eq!(data_objects(&env, &ids[2]), 0);
        for kept in [&ids[0], &ids[1], &ids[3], &ids[4]] {
            assert_eq!(record(&env, kept).state, SetState::Published);
            assert_eq!(data_objects(&env, kept), 3);
        }
        let catalog: Catalog =
            serde_json::from_slice(&env.meta.raw(&env.sets.catalog_key()).unwrap()).unwrap();
        assert!(catalog.decision.is_none());
        // A deleted set can no longer be pinned.
        let err = env.sets.pin(&ids[2], "late", BASE).err().unwrap();
        assert_eq!(err.code, "cloud_set_unavailable");
    }

    #[test]
    fn a_concurrent_pin_wins_over_a_retention_deletion() {
        let env = setup();
        let ids: Vec<String> = (0..3).map(|i| publish(&env, BASE + i * 1000)).collect();
        let sets = env.sets.clone();
        let victim = ids[0].clone();
        let target = format!("{victim}/record.json");
        env.meta.hook(Op::Put, &target, move || {
            sets.pin(&victim, "operator hold", BASE).unwrap();
        });
        let out = env
            .sets
            .collect(INSTALL, &retention(true), BASE + 86_400_000)
            .unwrap();
        assert_eq!(out.skipped, vec![ids[0].clone()]);
        assert!(out.deleted.is_empty());
        let kept = record(&env, &ids[0]);
        assert_eq!(kept.state, SetState::Published);
        assert_eq!(kept.pins.len(), 1);
        assert_eq!(data_objects(&env, &ids[0]), 3);
    }

    #[test]
    fn an_interrupted_retention_decision_is_resumed() {
        let env = setup();
        let ids: Vec<String> = (0..3).map(|i| publish(&env, BASE + i * 1000)).collect();
        env.data
            .fault(Op::Delete, "manifest.json", Fault::Refuse, 0, 1);
        let out = env
            .sets
            .collect(INSTALL, &retention(true), BASE + 86_400_000)
            .unwrap();
        assert_eq!(out.failures.len(), 1);
        assert_eq!(record(&env, &ids[0]).state, SetState::DeletePending);
        // Simulate the janitor crashing before it cleared the decision.
        let decision = Catalog {
            schema: CATALOG_SCHEMA.into(),
            revision: 9,
            updated_unix_ms: BASE,
            decision: Some(Decision {
                id: "feedfacefeedface".into(),
                created_unix_ms: BASE,
                delete: vec![ids[0].clone()],
                survivors: vec![ids[1].clone(), ids[2].clone()],
            }),
        };
        env.meta.set_raw(
            &env.sets.catalog_key(),
            &serde_json::to_vec(&decision).unwrap(),
        );
        let out = env
            .sets
            .collect(INSTALL, &retention(false), BASE + 86_400_000)
            .unwrap();
        assert_eq!(out.resumed, vec![ids[0].clone()]);
        assert_eq!(record(&env, &ids[0]).state, SetState::Tombstoned);
        assert_eq!(data_objects(&env, &ids[0]), 0);
        let catalog: Catalog =
            serde_json::from_slice(&env.meta.raw(&env.sets.catalog_key()).unwrap()).unwrap();
        assert!(catalog.decision.is_none());
    }

    #[test]
    fn a_concurrent_catalog_change_stops_collection() {
        let env = setup();
        let ids: Vec<String> = (0..3).map(|i| publish(&env, BASE + i * 1000)).collect();
        env.sets.collect(INSTALL, &retention(false), BASE).unwrap();
        let meta = env.meta.clone();
        let key = env.sets.catalog_key();
        let bytes = meta.raw(&key).unwrap();
        env.meta
            .hook(Op::Put, "catalog.json", move || meta.set_raw(&key, &bytes));
        let err = env
            .sets
            .collect(INSTALL, &retention(true), BASE + 86_400_000)
            .err()
            .unwrap();
        assert_eq!(err.code, "cloud_collection_conflict");
        for id in &ids {
            assert_eq!(record(&env, id).state, SetState::Published);
        }
    }

    #[test]
    fn pins_are_bounded_and_unpin_is_idempotent_after_unknown_results() {
        let env = setup();
        let id = publish(&env, BASE);
        let err = env
            .sets
            .unpin(&id, "pin-0000000000000000", BASE)
            .err()
            .unwrap();
        assert_eq!(err.code, "cloud_pin_not_found");
        let pins: Vec<Pin> = (0..MAX_PINS)
            .map(|_| env.sets.pin(&id, "hold", BASE).unwrap())
            .collect();
        let err = env.sets.pin(&id, "hold", BASE).err().unwrap();
        assert_eq!(err.code, "cloud_pins_full");
        env.meta.fault(Op::Put, "record.json", Fault::After, 0, 1);
        env.sets.unpin(&id, &pins[0].id, BASE).unwrap();
        assert_eq!(record(&env, &id).pins.len(), MAX_PINS - 1);
        let err = env.sets.unpin(&id, &pins[0].id, BASE).err().unwrap();
        assert_eq!(err.code, "cloud_pin_not_found");
        let err = env
            .sets
            .pin("20231114T221320Z-0000000000000000", "x", BASE)
            .err()
            .unwrap();
        assert_eq!(err.code, "cloud_set_not_found");
    }

    #[test]
    fn the_prefix_owner_refuses_another_installation() {
        let env = setup();
        env.sets.ensure_owner(INSTALL, BASE).unwrap();
        env.sets.ensure_owner(INSTALL, BASE).unwrap();
        let err = env
            .sets
            .ensure_owner("fedcba9876543210", BASE)
            .err()
            .unwrap();
        assert_eq!(err.code, "cloud_prefix_owned");
        assert_eq!(env.sets.owner().unwrap().as_deref(), Some(INSTALL));
        // Another installation never collects (or applies retention) here.
        let ids: Vec<String> = (0..3).map(|i| publish(&env, BASE + i * 1000)).collect();
        let far = BASE + 30 * 86_400_000;
        let err = env
            .sets
            .collect("fedcba9876543210", &retention(true), far)
            .err()
            .unwrap();
        assert_eq!(err.code, "cloud_prefix_owned");
        assert_eq!(data_objects(&env, &ids[0]), 3);
        let out = env.sets.collect(INSTALL, &retention(true), far).unwrap();
        assert_eq!(out.deleted, vec![ids[0].clone()]);
    }

    #[test]
    fn listing_reports_unrecorded_data_without_collecting_it() {
        let env = setup();
        let stray = "vlite/prod/sets/20231114T221320Z-00000000000000aa/objects/x";
        env.data.set_raw(stray, b"x");
        let listing = env.sets.list().unwrap();
        assert_eq!(listing.sets.len(), 1);
        assert!(listing.sets[0].record.is_none() && listing.sets[0].error.is_some());
        let out = env
            .sets
            .collect(INSTALL, &retention(true), BASE + 86_400_000)
            .unwrap();
        assert_eq!(out.unreadable.len(), 1);
        assert!(env.data.raw(stray).is_some());
    }

    #[test]
    fn a_shared_container_keeps_records_out_of_data_counts() {
        let store = Arc::new(Memory::default());
        let mut env = setup();
        env.sets = Sets::new(store.clone(), store.clone(), true, "p/".into());
        env.meta = store.clone();
        env.data = store;
        let ids: Vec<String> = (0..3).map(|i| publish(&env, BASE + i * 1000)).collect();
        let listing = env.sets.list().unwrap();
        assert!(listing.sets.iter().all(|s| s.data_objects == 3));
        let out = env
            .sets
            .collect(INSTALL, &retention(true), BASE + 86_400_000)
            .unwrap();
        assert_eq!(out.deleted, vec![ids[0].clone()]);
        assert_eq!(record(&env, &ids[0]).state, SetState::Tombstoned);
        assert!(env.meta.raw(&env.sets.tombstone_key(&ids[0])).is_some());
    }
}
