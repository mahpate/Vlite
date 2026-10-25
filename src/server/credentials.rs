//! Scoped, expiring, rotatable API credentials (F9).
//!
//! Tokens are `vlk_<id>_<secret>`; only the SHA-256 of the secret is stored,
//! in `<admin dir>/credentials.json`, replaced atomically. Every mutation is
//! audited (synced) before it is persisted and only then becomes visible, so
//! no success is reported for a change that is not durable and audited. A
//! failure after the atomic replacement (directory fsync) is reported as
//! `credentials_commit_indeterminate`: memory follows the replaced file.
use super::*;
use base64::prelude::BASE64_URL_SAFE_NO_PAD;
use serde::Serialize;
use std::{
    io::{Read, Write},
    path::{Path as FilePath, PathBuf},
};

pub(super) const TOKEN_PREFIX: &str = "vlk_";
pub(super) const CAPACITY: usize = 256;
const MAX_FILE_BYTES: u64 = 1024 * 1024;
const MAX_LIFETIME_MS: u64 = 365 * 24 * 3600 * 1000;
const FILE: &str = "credentials.json";
const ID_ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Op {
    Read,
    Search,
    SearchBuild,
    Write,
    Import,
    Index,
    Collections,
    Diagnostics,
    Backup,
    /// EPIC-011 graph reads on the graphs named by `Entry::graphs`.
    #[cfg(feature = "graph")]
    GraphRead,
    /// EPIC-011 graph lifecycle and batch writes on the bound graphs.
    #[cfg(feature = "graph")]
    GraphWrite,
}

#[cfg(not(feature = "graph"))]
const OPS: &[(Op, &str)] = &[
    (Op::Read, "read"),
    (Op::Search, "search"),
    (Op::SearchBuild, "search_build"),
    (Op::Write, "write"),
    (Op::Import, "import"),
    (Op::Index, "index"),
    (Op::Collections, "collections"),
    (Op::Diagnostics, "diagnostics"),
    (Op::Backup, "backup"),
];

/// Builds without `graph` do not know the graph operations, so a credential
/// store naming them fails to load there (fail closed).
#[cfg(feature = "graph")]
const OPS: &[(Op, &str)] = &[
    (Op::Read, "read"),
    (Op::Search, "search"),
    (Op::SearchBuild, "search_build"),
    (Op::Write, "write"),
    (Op::Import, "import"),
    (Op::Index, "index"),
    (Op::Collections, "collections"),
    (Op::Diagnostics, "diagnostics"),
    (Op::Backup, "backup"),
    (Op::GraphRead, "graph_read"),
    (Op::GraphWrite, "graph_write"),
];

fn bit(op: Op) -> u16 {
    1 << OPS.iter().position(|(o, _)| *o == op).expect("listed op")
}

fn parse_ops(names: &[String]) -> Result<u16, ApiError> {
    if names.is_empty() {
        return Err(unprocessable("operations must not be empty"));
    }
    names.iter().try_fold(0u16, |mask, name| {
        OPS.iter()
            .find(|(_, n)| n == name)
            .map(|(op, _)| mask | bit(*op))
            .ok_or_else(|| {
                unprocessable(format!("unknown operation; expected one of {}", op_list()))
            })
    })
}

fn op_list() -> String {
    OPS.iter().map(|(_, n)| *n).collect::<Vec<_>>().join(", ")
}

fn op_names(mask: u16) -> Vec<&'static str> {
    OPS.iter()
        .filter(|(op, _)| mask & bit(*op) != 0)
        .map(|(_, n)| *n)
        .collect()
}

/// An authenticated scoped principal. Contains no secret.
pub(super) struct Grant {
    pub id: String,
    /// Display name chosen at issue time; not a secret and not unique.
    pub name: String,
    pub tenant_id: Option<String>,
    pub requests_per_second: Option<u32>,
    pub max_vectors: Option<u64>,
    pub max_vectors_per_collection: Option<u64>,
    pub all: bool,
    pub collections: Vec<(String, u16)>,
    ops: u16,
    pub lineage: u64,
    pub expires: u64,
    /// Graph names the graph operations apply to; `["*"]` is every graph.
    /// Empty when the credential has no graph operation.
    #[cfg(feature = "graph")]
    pub graphs: Vec<String>,
}

impl Grant {
    /// The grant behind a signed-in editor or viewer: every collection the
    /// tenant (if any) owns, limited to `ops`, valid until the session ends.
    pub(super) fn for_user(
        id: String,
        name: String,
        tenant_id: Option<String>,
        ops: &[Op],
        lineage: u64,
        expires: u64,
    ) -> Self {
        Self {
            id,
            name,
            tenant_id,
            requests_per_second: None,
            max_vectors: None,
            max_vectors_per_collection: None,
            all: true,
            collections: Vec::new(),
            ops: ops.iter().fold(0, |mask, op| mask | bit(*op)),
            lineage,
            expires,
            #[cfg(feature = "graph")]
            graphs: Vec::new(),
        }
    }
    pub(super) fn qualify(&self, name: &str) -> String {
        self.tenant_id
            .as_ref()
            .map_or_else(|| name.to_owned(), |id| format!("t{id}__{name}"))
    }
    pub(super) fn owns(&self, name: &str) -> bool {
        self.tenant_id
            .as_ref()
            .is_none_or(|id| name.starts_with(&format!("t{id}__")))
    }
    pub(super) fn allows(&self, op: Op) -> bool {
        self.ops & bit(op) != 0
    }

    /// Cheap name pre-check before the collection is resolved.
    pub(super) fn names(&self, name: &str) -> bool {
        self.owns(name) && (self.all || self.collections.iter().any(|(n, _)| n == name))
    }

    /// Binding check: database lineage plus exact collection ID and name.
    /// A dropped and recreated collection has a new ID and never matches.
    pub(super) fn covers(&self, lineage: Option<u64>, collection: u16, name: &str) -> bool {
        lineage == Some(self.lineage)
            && self.owns(name)
            && (self.all
                || self
                    .collections
                    .iter()
                    .any(|(n, id)| *id == collection && n == name))
    }

    /// Graph name binding. Names, not IDs: a dropped graph's name may be
    /// recreated and is then covered again (graphs hold no vector data of
    /// their own; attachments are authorized per collection).
    #[cfg(feature = "graph")]
    pub(super) fn graph_allowed(&self, name: &str) -> bool {
        self.tenant_id.is_none() && self.graphs.iter().any(|g| g == "*" || g == name)
    }

    pub(super) fn json(&self) -> Value {
        #[cfg(feature = "graph")]
        if !self.graphs.is_empty() {
            let mut value = self.json_base();
            value["graphs"] = json!(self.graphs);
            return value;
        }
        self.json_base()
    }

    fn json_base(&self) -> Value {
        json!({"credential_id": self.id,
            "collections": if self.all { vec!["*".to_owned()] } else {
                self.collections.iter().map(|(n, _)| self.tenant_id.as_ref()
                    .and_then(|id| n.strip_prefix(&format!("t{id}__"))).unwrap_or(n).to_owned()).collect() },
            "tenant_id":self.tenant_id,"requests_per_second":self.requests_per_second,
            "max_vectors":self.max_vectors,"max_vectors_per_collection":self.max_vectors_per_collection,
            "operations": op_names(self.ops), "expires_unix_ms": self.expires})
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Scope {
    name: String,
    id: u16,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    id: String,
    name: String,
    all: bool,
    collections: Vec<Scope>,
    operations: Vec<String>,
    /// Hex database lineage the grant is bound to.
    lineage: String,
    secret_sha256: String,
    created_unix_ms: u64,
    expires_unix_ms: u64,
    revoked_unix_ms: Option<u64>,
    rotated_from: Option<String>,
    #[serde(default)]
    tenant_id: Option<String>,
    #[serde(default)]
    requests_per_second: Option<u32>,
    #[serde(default)]
    max_vectors: Option<u64>,
    #[serde(default)]
    max_vectors_per_collection: Option<u64>,
    /// EPIC-011: graph name binding for `graph_read` / `graph_write`.
    #[cfg(feature = "graph")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    graphs: Option<Vec<String>>,
}

impl Entry {
    fn state(&self, now: u64) -> &'static str {
        if self.revoked_unix_ms.is_some() {
            "revoked"
        } else if now >= self.expires_unix_ms {
            "expired"
        } else {
            "active"
        }
    }

    fn view(&self, now: u64) -> Value {
        #[cfg(feature = "graph")]
        if let Some(graphs) = &self.graphs {
            let mut value = self.view_base(now);
            value["graphs"] = json!(graphs);
            return value;
        }
        self.view_base(now)
    }

    fn view_base(&self, now: u64) -> Value {
        json!({"id": self.id, "name": self.name,
            "collections": if self.all { vec!["*".to_owned()] } else {
                self.collections.iter().map(|s| s.name.clone()).collect() },
            "operations": self.operations, "lineage": self.lineage,
            "created_unix_ms": self.created_unix_ms, "expires_unix_ms": self.expires_unix_ms,
            "revoked_unix_ms": self.revoked_unix_ms, "rotated_from": self.rotated_from,
            "state": self.state(now), "tenant_id": self.tenant_id,
            "requests_per_second": self.requests_per_second,
            "max_vectors": self.max_vectors, "max_vectors_per_collection": self.max_vectors_per_collection})
    }

    fn validate(&self) -> Option<()> {
        let id_ok = self.id.len() == 12 && self.id.bytes().all(|b| ID_ALPHABET.contains(&b));
        let hash_ok = self.secret_sha256.len() == 64
            && self.secret_sha256.bytes().all(|b| b.is_ascii_hexdigit());
        let lineage_ok = u64::from_str_radix(&self.lineage, 16).is_ok_and(|l| l != 0);
        (id_ok
            && hash_ok
            && lineage_ok
            && self
                .requests_per_second
                .is_none_or(|r| r > 0 && r <= 1_000_000)
            && self
                .tenant_id
                .as_ref()
                .is_none_or(|id| id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit()))
            && parse_ops(&self.operations).is_ok_and(|ops| self.graph_scope(ops).is_ok())
            && (self.all == self.collections.is_empty())
            && !self.name.is_empty()
            && self.name.len() <= 64)
            .then_some(())
    }
}

impl Entry {
    #[cfg(feature = "graph")]
    fn graph_scope(&self, ops: u16) -> Result<(), ApiError> {
        graph_scope(ops, self.graphs.as_deref(), self.tenant_id.is_some())
    }

    #[cfg(not(feature = "graph"))]
    fn graph_scope(&self, _: u16) -> Result<(), ApiError> {
        Ok(())
    }
}

/// EPIC-011 graph scope rules, shared by issue and load:
/// - `graph_read` / `graph_write` require a `graphs` binding and a binding
///   requires one of them (collection scope never implies graph scope);
/// - the binding is `["*"]` or up to 64 distinct valid graph names;
/// - tenant credentials cannot hold graph scope (graphs are not tenant
///   qualified).
#[cfg(feature = "graph")]
fn graph_scope(ops: u16, graphs: Option<&[String]>, tenant: bool) -> Result<(), ApiError> {
    let graph_ops = ops & (bit(Op::GraphRead) | bit(Op::GraphWrite)) != 0;
    let Some(graphs) = graphs else {
        return if graph_ops {
            Err(unprocessable(
                "graph_read and graph_write require a graphs binding",
            ))
        } else {
            Ok(())
        };
    };
    if !graph_ops {
        return Err(unprocessable(
            "graphs requires graph_read or graph_write in operations",
        ));
    }
    if tenant {
        return Err(unprocessable("tenant credentials cannot hold graph scope"));
    }
    let valid = |name: &String| {
        let bytes = name.as_bytes();
        (1..=63).contains(&bytes.len())
            && bytes[0].is_ascii_lowercase()
            && bytes[1..]
                .iter()
                .all(|&b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
    };
    let distinct = graphs
        .iter()
        .enumerate()
        .all(|(i, name)| !graphs[..i].contains(name));
    if graphs == ["*"]
        || (!graphs.is_empty() && graphs.len() <= 64 && distinct && graphs.iter().all(valid))
    {
        Ok(())
    } else {
        Err(unprocessable(
            "graphs must be [\"*\"] or 1..64 distinct graph names",
        ))
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoreFile {
    schema: u32,
    credentials: Vec<Entry>,
}

pub(super) struct Store {
    directory: PathBuf,
    entries: Mutex<Vec<Entry>>,
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

fn random_id() -> Result<String, ApiError> {
    let mut bytes = [0u8; 12];
    getrandom::fill(&mut bytes).map_err(internal)?;
    Ok(bytes
        .iter()
        .map(|b| ID_ALPHABET[(*b & 31) as usize] as char)
        .collect())
}

fn random_secret() -> Result<String, ApiError> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(internal)?;
    Ok(BASE64_URL_SAFE_NO_PAD.encode(bytes))
}

fn unauthorized() -> ApiError {
    ApiError::new(
        StatusCode::UNAUTHORIZED,
        "unauthorized",
        "valid bearer token required",
    )
}

impl Store {
    /// Loads (bounded, validated) or starts empty. Invalid content fails
    /// startup rather than silently dropping credentials.
    pub(super) fn load(directory: &FilePath) -> std::io::Result<Self> {
        let path = directory.join(FILE);
        let entries = match std::fs::symlink_metadata(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => return Err(error),
            Ok(meta) => {
                if !meta.is_file() || meta.file_type().is_symlink() {
                    return Err(std::io::Error::other(
                        "credentials.json must be a regular file",
                    ));
                }
                if meta.len() > MAX_FILE_BYTES {
                    return Err(std::io::Error::other("credentials.json exceeds 1 MiB"));
                }
                let mut text = Vec::new();
                std::fs::File::open(&path)?
                    .take(MAX_FILE_BYTES + 1)
                    .read_to_end(&mut text)?;
                let file: StoreFile = serde_json::from_slice(&text)
                    .map_err(|_| std::io::Error::other("credentials.json is invalid"))?;
                let mut ids = std::collections::BTreeSet::new();
                if file.schema != 1
                    || file.credentials.len() > CAPACITY
                    || file
                        .credentials
                        .iter()
                        .any(|e| e.validate().is_none() || !ids.insert(e.id.clone()))
                {
                    return Err(std::io::Error::other("credentials.json is invalid"));
                }
                file.credentials
            }
        };
        Ok(Self {
            directory: directory.to_owned(),
            entries: Mutex::new(entries),
        })
    }

    /// Constant-time verification. Unknown IDs still hash and compare.
    pub(super) fn authenticate(&self, token: &str, now: u64) -> Result<Arc<Grant>, ApiError> {
        let rest = token.strip_prefix(TOKEN_PREFIX).ok_or_else(unauthorized)?;
        let (id, secret) = rest.split_once('_').ok_or_else(unauthorized)?;
        if id.len() != 12 || secret.len() != 43 {
            return Err(unauthorized());
        }
        let presented = Sha256::digest(secret.as_bytes());
        let entries = self.entries.lock().map_err(internal)?;
        let entry = entries.iter().find(|e| e.id == id);
        let stored = entry
            .and_then(|e| {
                let mut bytes = [0u8; 32];
                for (i, chunk) in e.secret_sha256.as_bytes().chunks(2).enumerate() {
                    bytes[i] = u8::from_str_radix(std::str::from_utf8(chunk).ok()?, 16).ok()?;
                }
                Some(bytes)
            })
            .unwrap_or([0xa5; 32]);
        let matched = bool::from(presented.as_slice().ct_eq(&stored));
        let Some(entry) = entry.filter(|_| matched) else {
            return Err(unauthorized());
        };
        if entry.revoked_unix_ms.is_some() {
            return Err(unauthorized());
        }
        if now >= entry.expires_unix_ms {
            return Err(ApiError::new(
                StatusCode::UNAUTHORIZED,
                "credential_expired",
                "credential expired; request a new one",
            ));
        }
        Ok(Arc::new(Grant {
            id: entry.id.clone(),
            name: entry.name.clone(),
            tenant_id: entry.tenant_id.clone(),
            requests_per_second: entry.requests_per_second,
            max_vectors: entry.max_vectors,
            max_vectors_per_collection: entry.max_vectors_per_collection,
            all: entry.all,
            collections: entry
                .collections
                .iter()
                .map(|s| (s.name.clone(), s.id))
                .collect(),
            ops: parse_ops(&entry.operations).map_err(|_| unauthorized())?,
            lineage: u64::from_str_radix(&entry.lineage, 16).map_err(|_| unauthorized())?,
            expires: entry.expires_unix_ms,
            #[cfg(feature = "graph")]
            graphs: entry.graphs.clone().unwrap_or_default(),
        }))
    }

    /// Credential states bound to one tenant: `[active, expired, revoked]`.
    pub(super) fn tenant_states(&self, tenant: &str, now: u64) -> Result<[u64; 3], ApiError> {
        let mut counts = [0u64; 3];
        for entry in self.entries.lock().map_err(internal)?.iter() {
            if entry.tenant_id.as_deref() == Some(tenant) {
                counts[match entry.state(now) {
                    "active" => 0,
                    "expired" => 1,
                    _ => 2,
                }] += 1;
            }
        }
        Ok(counts)
    }

    /// Writes a temp file, fsyncs it, atomically replaces the store and
    /// fsyncs the directory. Failures before the replacement leave the old
    /// file in place; a failure after it means the new content is visible
    /// but its durability is unconfirmed.
    fn persist(&self, entries: &[Entry]) -> Result<(), Persist> {
        let unavailable = |_| Persist::NotReplaced;
        let text = serde_json::to_vec(&StoreFile {
            schema: 1,
            credentials: entries.to_vec(),
        })
        .map_err(|_| Persist::NotReplaced)?;
        let mut file = tempfile::NamedTempFile::new_in(&self.directory).map_err(unavailable)?;
        file.write_all(&text).map_err(unavailable)?;
        file.as_file().sync_all().map_err(unavailable)?;
        file.persist(self.directory.join(FILE))
            .map_err(|_| Persist::NotReplaced)?;
        #[cfg(test)]
        if tests::FAIL_DIRECTORY_SYNC.with(|fail| fail.get()) {
            return Err(Persist::Replaced);
        }
        admin::sync_directory(&self.directory).map_err(|_| Persist::Replaced)
    }

    /// Audit (synced) → persist → publish.
    ///
    /// - Audit failure or a persist failure before the file replacement:
    ///   503, memory and disk unchanged (a true no-change).
    /// - Failure after the replacement (directory fsync): the new content
    ///   is already what the store reads, so memory is reconciled to it and
    ///   503 `credentials_commit_indeterminate` reports that the change IS
    ///   in effect but may not survive a crash. It is never reported as
    ///   success and never as "no change".
    fn commit(
        &self,
        audit: &audit::Log,
        mut entries: std::sync::MutexGuard<'_, Vec<Entry>>,
        next: Vec<Entry>,
        event: Value,
    ) -> Result<(), ApiError> {
        audit.append(event.clone(), true).map_err(|_| {
            ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "audit_unavailable",
                "persistent audit could not be written; no change was made",
            )
        })?;
        let outcome = self.persist(&next);
        let name = event["event"].as_str().unwrap_or("credential").to_owned();
        let credential = event["credential_id"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        match outcome {
            Ok(()) => {
                *entries = next;
                Ok(())
            }
            Err(Persist::NotReplaced) => {
                let mut failed = event;
                failed["event"] = json!(format!("{name}_failed"));
                let _ = audit.append(failed, true);
                Err(ApiError::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "credentials_unavailable",
                    "credential store could not be written; no change was made",
                ))
            }
            Err(Persist::Replaced) => {
                *entries = next;
                let mut unknown = event;
                unknown["event"] = json!(format!("{name}_durability_unknown"));
                let _ = audit.append(unknown, true);
                Err(ApiError::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "credentials_commit_indeterminate",
                    format!(
                        "the change to credential {credential} is in effect but its durability \
                         could not be confirmed and it may be lost after a crash; no token is \
                         returned: list credentials and revoke or re-issue as needed"
                    ),
                ))
            }
        }
    }
}

enum Persist {
    NotReplaced,
    Replaced,
}

pub(super) fn routes() -> Router<Service> {
    Router::new()
        .route("/v1/admin/credentials", get(list).post(create))
        .route("/v1/admin/credentials/{id}/rotate", post(rotate))
        .route("/v1/admin/credentials/{id}/revoke", post(revoke))
        .route("/v1/admin/audit/persistent", get(persistent_audit))
}

fn store(service: &Service) -> Result<(Arc<Store>, Arc<audit::Log>), ApiError> {
    match (&service.admin.credentials, &service.admin.audit_log) {
        (Some(store), Some(log)) => Ok((store.clone(), log.clone())),
        _ => Err(ApiError::new(
            StatusCode::CONFLICT,
            "credentials_disabled",
            "scoped credentials require an admin token and VLITE_ADMIN_DIR",
        )),
    }
}

fn now() -> u64 {
    admin::now_ms() as u64
}

async fn list(State(service): State<Service>) -> Result<Json<Value>, ApiError> {
    let (store, _) = store(&service)?;
    let now = now();
    let entries = store.entries.lock().map_err(internal)?;
    Ok(Json(json!({
        "items": entries.iter().map(|e| e.view(now)).collect::<Vec<_>>(),
        "capacity": CAPACITY,
    })))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Create {
    name: String,
    collections: Vec<String>,
    operations: Vec<String>,
    expires_unix_ms: u64,
    tenant_id: Option<String>,
    requests_per_second: Option<u32>,
    max_vectors: Option<u64>,
    max_vectors_per_collection: Option<u64>,
    #[cfg(feature = "graph")]
    graphs: Option<Vec<String>>,
}

/// Adds an entry, pruning inactive ones only when at capacity.
fn admit(mut entries: Vec<Entry>, entry: Entry, now: u64) -> Result<Vec<Entry>, ApiError> {
    if entries.len() >= CAPACITY {
        entries.retain(|e| e.state(now) == "active");
    }
    if entries.len() >= CAPACITY {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "credential_limit",
            "too many active credentials; revoke some first",
        ));
    }
    entries.push(entry);
    Ok(entries)
}

fn token(entry: &Entry, secret: &str) -> String {
    format!("{TOKEN_PREFIX}{}_{secret}", entry.id)
}

async fn create(
    State(service): State<Service>,
    input: Result<Json<Create>, JsonRejection>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let (store, log) = store(&service)?;
    let input = body(input)?;
    let now = now();
    if input.name.is_empty() || input.name.len() > 64 || input.name.chars().any(char::is_control) {
        return Err(unprocessable("name must be 1..64 printable characters"));
    }
    let operations = parse_ops(&input.operations)?;
    #[cfg(feature = "graph")]
    graph_scope(
        operations,
        input.graphs.as_deref(),
        input.tenant_id.is_some(),
    )?;
    if input
        .requests_per_second
        .is_some_and(|r| r == 0 || r > 1_000_000)
    {
        return Err(unprocessable("requests_per_second must be 1..1000000"));
    }
    if let Some(id) = &input.tenant_id {
        service
            .admin
            .tenants
            .as_ref()
            .ok_or_else(forbidden_scope)?
            .get(id)
            .map_err(|error| {
                if error.code == "tenant_suspended" {
                    ApiError::new(
                        StatusCode::CONFLICT,
                        "tenant_suspended",
                        "tenant is suspended; resume it before issuing credentials",
                    )
                } else {
                    error
                }
            })?;
    }
    if input.expires_unix_ms <= now || input.expires_unix_ms > now + MAX_LIFETIME_MS {
        return Err(unprocessable(
            "expires_unix_ms must be in the future and at most 365 days ahead",
        ));
    }
    let all = input.collections == ["*"];
    if input.collections.is_empty() || (!all && input.collections.iter().any(|c| c == "*")) {
        return Err(unprocessable(
            "collections must be [\"*\"] or a nonempty list of names",
        ));
    }
    let names = input
        .collections
        .iter()
        .map(|name| {
            input
                .tenant_id
                .as_ref()
                .map_or_else(|| name.clone(), |id| format!("t{id}__{name}"))
        })
        .collect::<Vec<_>>();
    // Resolve names to IDs and capture the lineage under the database lock.
    let (lineage, scopes) = service
        .run(move |db| {
            db.ensure_healthy()?;
            let lineage = db.lineage().ok_or(Error::CollectionsRequireUpgrade)?;
            let mut scopes: Vec<Scope> = Vec::new();
            if !all {
                for name in &names {
                    if scopes.iter().all(|s| &s.name != name) {
                        scopes.push(Scope {
                            name: name.clone(),
                            id: db.resolve(name)?,
                        });
                    }
                }
            }
            Ok((lineage, scopes))
        })
        .await?;
    let secret = random_secret()?;
    let entry = Entry {
        id: random_id()?,
        name: input.name,
        all,
        collections: scopes,
        operations: op_names(operations)
            .into_iter()
            .map(str::to_owned)
            .collect(),
        lineage: format!("{lineage:016x}"),
        secret_sha256: sha256_hex(secret.as_bytes()),
        created_unix_ms: now,
        expires_unix_ms: input.expires_unix_ms,
        revoked_unix_ms: None,
        rotated_from: None,
        tenant_id: input.tenant_id,
        requests_per_second: input.requests_per_second,
        max_vectors: input.max_vectors,
        max_vectors_per_collection: input.max_vectors_per_collection,
        #[cfg(feature = "graph")]
        graphs: input.graphs,
    };
    let view = entry.view(now);
    let token = token(&entry, &secret);
    let event = json!({"event":"credential_created","credential_id":entry.id,
        "collections":view["collections"],"operations":view["operations"],
        "expires_unix_ms":entry.expires_unix_ms});
    tokio::task::spawn_blocking(move || {
        let entries = store.entries.lock().map_err(internal)?;
        let next = admit(entries.clone(), entry, now)?;
        store.commit(&log, entries, next, event)
    })
    .await
    .map_err(internal)??;
    Ok((
        StatusCode::CREATED,
        Json(json!({"credential": view, "token": token})),
    ))
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Rotate {
    grace_seconds: Option<u64>,
}

fn credential_id(input: Result<Path<String>, PathRejection>) -> Result<String, ApiError> {
    let not_found = || {
        ApiError::new(
            StatusCode::NOT_FOUND,
            "credential_not_found",
            "credential not found",
        )
    };
    let Path(id) = input.map_err(|_| not_found())?;
    if id.len() != 12 || !id.bytes().all(|b| ID_ALPHABET.contains(&b)) {
        return Err(not_found());
    }
    Ok(id)
}

fn find(entries: &[Entry], id: &str) -> Result<usize, ApiError> {
    entries.iter().position(|e| e.id == id).ok_or_else(|| {
        ApiError::new(
            StatusCode::NOT_FOUND,
            "credential_not_found",
            "credential not found",
        )
    })
}

async fn rotate(
    State(service): State<Service>,
    id: Result<Path<String>, PathRejection>,
    input: axum::body::Bytes,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let (store, log) = store(&service)?;
    let id = credential_id(id)?;
    let input: Rotate = optional(&input)?;
    let grace = input.grace_seconds.unwrap_or(3600);
    if grace > 86_400 {
        return Err(unprocessable("grace_seconds must be 0..86400"));
    }
    tokio::task::spawn_blocking(move || {
        let now = now();
        let entries = store.entries.lock().map_err(internal)?;
        let index = find(&entries, &id)?;
        let old = &entries[index];
        if old.state(now) != "active" {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "credential_inactive",
                "only active credentials can be rotated",
            ));
        }
        let secret = random_secret()?;
        let entry = Entry {
            id: random_id()?,
            secret_sha256: sha256_hex(secret.as_bytes()),
            created_unix_ms: now,
            revoked_unix_ms: None,
            rotated_from: Some(old.id.clone()),
            ..old.clone()
        };
        let mut next = entries.clone();
        next[index].expires_unix_ms = old.expires_unix_ms.min(now + grace * 1000);
        let previous = next[index].view(now);
        let view = entry.view(now);
        let token = token(&entry, &secret);
        let event = json!({"event":"credential_rotated","credential_id":entry.id,
            "rotated_from":id,"previous_expires_unix_ms":next[index].expires_unix_ms});
        let next = admit(next, entry, now)?;
        store.commit(&log, entries, next, event)?;
        Ok((
            StatusCode::CREATED,
            Json(json!({"credential": view, "token": token, "previous": previous})),
        ))
    })
    .await
    .map_err(internal)?
}

async fn revoke(
    State(service): State<Service>,
    id: Result<Path<String>, PathRejection>,
) -> Result<Json<Value>, ApiError> {
    let (store, log) = store(&service)?;
    let id = credential_id(id)?;
    tokio::task::spawn_blocking(move || {
        let now = now();
        let entries = store.entries.lock().map_err(internal)?;
        let index = find(&entries, &id)?;
        if entries[index].revoked_unix_ms.is_some() {
            return Ok(Json(json!({"credential": entries[index].view(now)})));
        }
        let mut next = entries.clone();
        next[index].revoked_unix_ms = Some(now);
        let view = next[index].view(now);
        store.commit(
            &log,
            entries,
            next,
            json!({"event":"credential_revoked","credential_id":id}),
        )?;
        Ok(Json(json!({"credential": view})))
    })
    .await
    .map_err(internal)?
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AuditQuery {
    limit: Option<usize>,
}

async fn persistent_audit(
    State(service): State<Service>,
    query: Result<Query<AuditQuery>, QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    let Query(query) = query.map_err(|error| {
        ApiError::new(StatusCode::BAD_REQUEST, "invalid_query", error.body_text())
    })?;
    let limit = query.limit.unwrap_or(100);
    if !(1..=1000).contains(&limit) {
        return Err(unprocessable("limit must be 1..1000"));
    }
    let Some(log) = service.admin.audit_log.clone() else {
        return Ok(Json(json!({"enabled": false, "items": []})));
    };
    let items = tokio::task::spawn_blocking(move || log.tail(limit))
        .await
        .map_err(internal)?
        .map_err(internal)?;
    Ok(Json(json!({"enabled": true, "items": items})))
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use std::cell::Cell;

    thread_local! {
        pub(super) static FAIL_DIRECTORY_SYNC: Cell<bool> = const { Cell::new(false) };
    }

    fn entry(id: &str) -> Entry {
        Entry {
            id: id.to_owned(),
            name: "svc".to_owned(),
            all: true,
            collections: Vec::new(),
            operations: vec!["read".to_owned()],
            lineage: "1".to_owned(),
            secret_sha256: "ab".repeat(32),
            created_unix_ms: 1,
            expires_unix_ms: u64::MAX,
            revoked_unix_ms: None,
            rotated_from: None,
            tenant_id: None,
            requests_per_second: None,
            max_vectors: None,
            max_vectors_per_collection: None,
            #[cfg(feature = "graph")]
            graphs: None,
        }
    }

    fn event(id: &str) -> Value {
        json!({"event": "credential_created", "credential_id": id})
    }

    #[test]
    fn failure_after_replacement_reconciles_memory_and_is_indeterminate_not_no_change() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::load(dir.path()).unwrap();
        let log = audit::Log::open(dir.path()).unwrap();
        FAIL_DIRECTORY_SYNC.with(|fail| fail.set(true));
        let result = store.commit(
            &log,
            store.entries.lock().unwrap(),
            vec![entry("aaaaaaaaaaaa")],
            event("aaaaaaaaaaaa"),
        );
        FAIL_DIRECTORY_SYNC.with(|fail| fail.set(false));
        let error = result.unwrap_err();
        assert_eq!(error.status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(error.code, "credentials_commit_indeterminate");
        assert!(error.message.contains("aaaaaaaaaaaa") && error.message.contains("in effect"));
        // Memory matches the replaced file: no memory-old / disk-new split.
        assert_eq!(store.entries.lock().unwrap().len(), 1);
        assert_eq!(
            Store::load(dir.path())
                .unwrap()
                .entries
                .lock()
                .unwrap()
                .len(),
            1
        );
        let events = log.tail(10).unwrap();
        let names = events
            .iter()
            .filter_map(|e| e["event"].as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            [
                "credential_created",
                "credential_created_durability_unknown"
            ]
        );
        // The next successful commit restores normal semantics.
        let next = vec![entry("aaaaaaaaaaaa"), entry("bbbbbbbbbbbb")];
        store
            .commit(
                &log,
                store.entries.lock().unwrap(),
                next,
                event("bbbbbbbbbbbb"),
            )
            .unwrap();
        assert_eq!(
            Store::load(dir.path())
                .unwrap()
                .entries
                .lock()
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn failure_before_replacement_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::load(dir.path()).unwrap();
        let log = audit::Log::open(dir.path()).unwrap();
        // A directory at the store path makes the atomic replacement fail.
        std::fs::create_dir(dir.path().join(FILE)).unwrap();
        let error = store
            .commit(
                &log,
                store.entries.lock().unwrap(),
                vec![entry("aaaaaaaaaaaa")],
                event("aaaaaaaaaaaa"),
            )
            .unwrap_err();
        assert_eq!(error.code, "credentials_unavailable");
        assert!(error.message.contains("no change"));
        assert!(store.entries.lock().unwrap().is_empty());
        assert!(dir.path().join(FILE).is_dir());
        let names = log
            .tail(10)
            .unwrap()
            .iter()
            .filter_map(|e| e["event"].as_str().map(str::to_owned))
            .collect::<Vec<_>>();
        assert_eq!(names, ["credential_created", "credential_created_failed"]);
    }
}
