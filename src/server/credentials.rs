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
    GraphRead,
    /// EPIC-011 graph lifecycle and batch writes on the bound graphs.
    GraphWrite,
}

/// Builds that predate graph operations do not know them, so a credential
/// store naming them fails to load there (fail closed).
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

/// Grant id prefix of a signed-in dashboard account (`users::user_grant`).
const USER_PREFIX: &str = "user:";

/// The grant id (and audit actor) of dashboard account `account_id`.
pub(super) fn user_principal_id(account_id: &str) -> String {
    format!("{USER_PREFIX}{account_id}")
}

/// The dashboard account a `user_principal_id` names; `None` for any other
/// grant id.
pub(super) fn user_account_id(principal_id: &str) -> Option<&str> {
    principal_id.strip_prefix(USER_PREFIX)
}

/// Operations of an editor account (`users::AccountRole::Editor`), before
/// its graph knobs narrow them. The single source for the role, the
/// gateway-forwarded principal check (`user_ops`) and the identity view.
pub(super) const EDITOR_OPS: &[Op] = &[
    Op::Read,
    Op::Search,
    Op::SearchBuild,
    Op::Write,
    Op::Import,
    Op::Index,
    Op::Collections,
    Op::GraphRead,
    Op::GraphWrite,
];
/// Operations of a viewer account (`users::AccountRole::Viewer`).
pub(super) const VIEWER_OPS: &[Op] = &[Op::Read, Op::Search, Op::GraphRead];

/// Every operation an editor or viewer account can hold; a forwarded user
/// principal naming any other is refused.
fn user_ops() -> u16 {
    EDITOR_OPS
        .iter()
        .chain(VIEWER_OPS)
        .fold(0, |mask, op| mask | bit(*op))
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
    pub api_credential: bool,
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
    /// Graph names the graph operations apply to; `["*"]` is every graph the
    /// principal's tenant (if any) owns. Names are unqualified: a tenant
    /// binding `g` covers the stored graph `t{tenant}__g`. Empty when the
    /// principal has no graph operation.
    pub graphs: Vec<String>,
}

impl Grant {
    /// The grant behind a signed-in editor or viewer: every collection the
    /// tenant (if any) owns, limited to `ops`, valid until the session ends.
    /// Graph operations in `ops` cover every graph the tenant owns until the
    /// caller narrows `graphs`; vector records reached through graph batches
    /// still need a collection the tenant owns.
    pub(super) fn for_user(
        id: String,
        name: String,
        tenant_id: Option<String>,
        ops: &[Op],
        lineage: u64,
        expires: u64,
    ) -> Self {
        let ops = ops.iter().fold(0, |mask, op| mask | bit(*op));
        Self {
            id,
            api_credential: false,
            name,
            tenant_id,
            requests_per_second: None,
            max_vectors: None,
            max_vectors_per_collection: None,
            all: true,
            collections: Vec::new(),
            ops,
            lineage,
            expires,
            graphs: if ops & (bit(Op::GraphRead) | bit(Op::GraphWrite)) == 0 {
                Vec::new()
            } else {
                vec!["*".to_owned()]
            },
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
    /// Whether every operation the grant allows is one of `ops`.
    pub(super) fn allows_only(&self, ops: &[Op]) -> bool {
        let listed = ops.iter().fold(0, |mask, op| mask | bit(*op));
        self.ops & !listed == 0
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
    /// their own; attachments are authorized per collection). `stored` is the
    /// stored graph name: a tenanted principal reaches only `t{tenant}__*`
    /// graphs, and its bound names are qualified the same way.
    pub(super) fn graph_allowed(&self, stored: &str) -> bool {
        self.owns(stored)
            && self
                .graphs
                .iter()
                .any(|g| g == "*" || self.qualify(g) == stored)
    }

    pub(super) fn json(&self) -> Value {
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

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Scope {
    name: String,
    id: u16,
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
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

    fn recovery(&self, now: u64) -> RecoveryCredential {
        RecoveryCredential {
            id: self.id.clone(),
            name: self.name.clone(),
            tenant_id: self.tenant_id.clone(),
            collections: if self.all {
                vec!["*".to_owned()]
            } else {
                self.collections.iter().map(|s| s.name.clone()).collect()
            },
            operations: self.operations.clone(),
            graphs: self.graphs.clone(),
            state: self.state(now).to_owned(),
            expires_unix_ms: self.expires_unix_ms,
            requests_per_second: self.requests_per_second,
            max_vectors: self.max_vectors,
            max_vectors_per_collection: self.max_vectors_per_collection,
        }
    }

    fn view(&self, now: u64) -> Value {
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
        (id_ok && hash_ok).then_some(())?;
        self.validate_scope()
    }

    /// Every rule except the id and secret-hash shapes. A forwarded user
    /// principal (OD-004) has neither, so it is checked against these alone.
    fn validate_scope(&self) -> Option<()> {
        let lineage_ok = u64::from_str_radix(&self.lineage, 16).is_ok_and(|l| l != 0);
        (lineage_ok
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

    /// The principal this entry authenticates as. Rejects nothing that
    /// `validate` accepts.
    fn grant(&self) -> Option<Grant> {
        Some(Grant {
            id: self.id.clone(),
            api_credential: true,
            name: self.name.clone(),
            tenant_id: self.tenant_id.clone(),
            requests_per_second: self.requests_per_second,
            max_vectors: self.max_vectors,
            max_vectors_per_collection: self.max_vectors_per_collection,
            all: self.all,
            collections: self
                .collections
                .iter()
                .map(|s| (s.name.clone(), s.id))
                .collect(),
            ops: parse_ops(&self.operations).ok()?,
            lineage: u64::from_str_radix(&self.lineage, 16).ok()?,
            expires: self.expires_unix_ms,
            graphs: self.graphs.clone().unwrap_or_default(),
        })
    }
}

/// A scoped principal as a gateway forwards it in a signed envelope
/// (feature-aca-ha ITEM-009): the grant of an `Entry` without its secret
/// hash, creation, revocation or rotation state. A backend accepts it only
/// under the rules `Entry::validate` applies to a stored credential.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Principal {
    id: String,
    name: String,
    all: bool,
    collections: Vec<Scope>,
    operations: Vec<String>,
    lineage: String,
    expires_unix_ms: u64,
    tenant_id: Option<String>,
    requests_per_second: Option<u32>,
    max_vectors: Option<u64>,
    max_vectors_per_collection: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    graphs: Option<Vec<String>>,
}

impl Principal {
    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) fn of(grant: &Grant) -> Self {
        Self {
            id: grant.id.clone(),
            name: grant.name.clone(),
            all: grant.all,
            collections: grant
                .collections
                .iter()
                .map(|(name, id)| Scope {
                    name: name.clone(),
                    id: *id,
                })
                .collect(),
            operations: op_names(grant.ops).into_iter().map(str::to_owned).collect(),
            lineage: format!("{:016x}", grant.lineage),
            expires_unix_ms: grant.expires,
            tenant_id: grant.tenant_id.clone(),
            requests_per_second: grant.requests_per_second,
            max_vectors: grant.max_vectors,
            max_vectors_per_collection: grant.max_vectors_per_collection,
            graphs: (!grant.graphs.is_empty()).then(|| grant.graphs.clone()),
        }
    }

    pub(super) fn tenant_id(&self) -> Option<&str> {
        self.tenant_id.as_deref()
    }

    pub(super) fn expires_unix_ms(&self) -> u64 {
        self.expires_unix_ms
    }

    /// The grant, or `None` when the principal breaks a credential rule.
    ///
    /// OD-004: a gateway forwards a signed-in tenant dashboard user as
    /// `user:<32 hex>`. Such a principal is accepted only in the shape
    /// `Grant::for_user` gives an editor or viewer (a tenant, the tenant's
    /// whole scope, no rate or quota limit, editor operations at most) and
    /// yields a session grant, never an API credential. Builds without this
    /// rule refuse it (fail closed).
    pub(super) fn grant(&self) -> Option<Grant> {
        let user = user_account_id(&self.id);
        if let Some(user) = user {
            let shaped = user.len() == 32
                && user.bytes().all(|b| b.is_ascii_hexdigit())
                && self.tenant_id.is_some()
                && self.all
                && self.collections.is_empty()
                && self.requests_per_second.is_none()
                && self.max_vectors.is_none()
                && self.max_vectors_per_collection.is_none()
                && parse_ops(&self.operations).is_ok_and(|ops| ops & !user_ops() == 0);
            if !shaped {
                return None;
            }
        }
        let entry = Entry {
            id: self.id.clone(),
            name: self.name.clone(),
            all: self.all,
            collections: self.collections.clone(),
            operations: self.operations.clone(),
            lineage: self.lineage.clone(),
            // Never compared: only satisfies the shape `validate` checks.
            secret_sha256: "0".repeat(64),
            created_unix_ms: 0,
            expires_unix_ms: self.expires_unix_ms,
            revoked_unix_ms: None,
            rotated_from: None,
            tenant_id: self.tenant_id.clone(),
            requests_per_second: self.requests_per_second,
            max_vectors: self.max_vectors,
            max_vectors_per_collection: self.max_vectors_per_collection,
            graphs: self.graphs.clone(),
        };
        if user.is_none() {
            entry.validate()?;
            return entry.grant();
        }
        entry.validate_scope()?;
        let mut grant = entry.grant()?;
        grant.api_credential = false;
        Some(grant)
    }
}

/// Credentials as the shared control state carries them (feature-aca-ha
/// ITEM-011): the stored entries verbatim, secret hashes, lineage bindings
/// and revocation included, so it is as sensitive as `credentials.json`
/// (SEC-004). It is only ever built from a live store, never from a recovery
/// inventory, which has no hashes (rekey-only).
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub(super) struct Snapshot(Vec<Entry>);

impl Snapshot {
    /// The rules `Store::load` applies to `credentials.json`.
    pub(super) fn valid(&self) -> bool {
        let mut ids = std::collections::BTreeSet::new();
        self.0.len() <= CAPACITY
            && self
                .0
                .iter()
                .all(|e| e.validate().is_some() && ids.insert(e.id.as_str()))
    }

    pub(super) fn authenticate(&self, token: &str, now: u64) -> Result<Arc<Grant>, ApiError> {
        verify(&self.0, token, now)
    }

    /// Revokes `id` in this snapshot (idempotent: an earlier revocation time
    /// is kept). 404 `credential_not_found` when the snapshot lacks it.
    pub(super) fn revoke(&mut self, id: &str, now: u64) -> Result<(), ApiError> {
        let index = find(&self.0, id)?;
        self.0[index].revoked_unix_ms.get_or_insert(now);
        Ok(())
    }

    /// Keeps `current`'s restrictions on the credentials both hold: a
    /// revocation stays and the earlier expiry wins, so republishing an
    /// older admin directory never revives or extends a credential.
    /// Credentials missing here are not brought back.
    pub(super) fn keep_restrictions(&mut self, current: &Snapshot) {
        for entry in &mut self.0 {
            if let Some(old) = current.0.iter().find(|old| old.id == entry.id) {
                entry.expires_unix_ms = entry.expires_unix_ms.min(old.expires_unix_ms);
                if entry.revoked_unix_ms.is_none() {
                    entry.revoked_unix_ms = old.revoked_unix_ms;
                }
            }
        }
    }
}

#[cfg_attr(not(test), allow(dead_code))]
impl Snapshot {
    /// Both views must name exactly the same fresh principals and secrets;
    /// only their database lineage differs.
    pub(super) fn migration_views_match(
        &self,
        target: &Self,
        tenant: &str,
        source_lineage: u64,
        target_lineage: u64,
    ) -> bool {
        let scoped = |view: &Self, lineage| {
            view.valid()
                && view.0.iter().all(|entry| {
                    entry.tenant_id.as_deref() == Some(tenant)
                        && u64::from_str_radix(&entry.lineage, 16).ok() == Some(lineage)
                })
        };
        scoped(self, source_lineage)
            && scoped(target, target_lineage)
            && self.0.len() == target.0.len()
            && self.0.iter().all(|entry| {
                target.0.iter().any(|other| {
                    let mut rebound = entry.clone();
                    rebound.lineage = other.lineage.clone();
                    rebound == *other
                })
            })
    }

    /// Replace only this tenant, retiring all previous hashes. Restrictions
    /// are merged by stable principal ID before the candidate is published.
    pub(super) fn with_migration_view(&self, tenant: &str, view: &Self) -> Option<Self> {
        let mut next = self.clone();
        next.keep_migration_tenant(view, tenant);
        next.keep_restrictions(self);
        next.valid().then_some(next)
    }

    pub(super) fn migration_view_is_current(&self, tenant: &str, view: &Self) -> bool {
        let mut restricted = view.clone();
        restricted.keep_restrictions(self);
        let current: Vec<_> = self
            .0
            .iter()
            .filter(|entry| entry.tenant_id.as_deref() == Some(tenant))
            .collect();
        current.len() == restricted.0.len()
            && restricted.0.iter().all(|entry| current.contains(&entry))
    }

    /// A stale local credential file must never undo a published rekey.
    pub(super) fn keep_migration_tenant(&mut self, current: &Self, tenant: &str) {
        self.0
            .retain(|entry| entry.tenant_id.as_deref() != Some(tenant));
        self.0.extend(
            current
                .0
                .iter()
                .filter(|entry| entry.tenant_id.as_deref() == Some(tenant))
                .cloned(),
        );
    }

    /// Replaces or adds each of `ids` with its entry in `current`, so a
    /// stale local credential file cannot drop or restore one published
    /// only in control state. An ID absent from `current` is left alone.
    pub(super) fn keep_ids<'a>(&mut self, current: &Self, ids: impl IntoIterator<Item = &'a str>) {
        for id in ids {
            if let Some(entry) = current.0.iter().find(|entry| entry.id == id) {
                self.0.retain(|kept| kept.id != id);
                self.0.push(entry.clone());
            }
        }
    }
}

/// One scoped credential in a recovery bundle: its identity, grant and
/// non-secret limits, for the re-issue report. It has no secret hash or
/// lineage and is never turned back into a credential (rekey-only).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RecoveryCredential {
    pub id: String,
    pub name: String,
    pub tenant_id: Option<String>,
    /// Collection names, or `["*"]` for every collection.
    pub collections: Vec<String>,
    pub operations: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub graphs: Option<Vec<String>>,
    pub state: String,
    pub expires_unix_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requests_per_second: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_vectors: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_vectors_per_collection: Option<u64>,
}

/// Migration inventory retains the original stored collection identities,
/// unlike general recovery, but never a secret hash or rotation lineage.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct MigrationCredential {
    pub inventory: RecoveryCredential,
    pub source_collections: Vec<MigrationCollection>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct MigrationCollection {
    pub stored_name: String,
    pub id: u16,
}

#[cfg_attr(not(test), allow(dead_code))]
impl MigrationCredential {
    pub(super) fn check(&self, tenant: &str) -> Result<(), &'static str> {
        let inventory = &self.inventory;
        inventory.check()?;
        if inventory.tenant_id.as_deref() != Some(tenant) {
            return Err("a migration credential belongs to another tenant");
        }
        let all = inventory.collections == ["*"];
        let mut names = std::collections::BTreeSet::new();
        if (all && !self.source_collections.is_empty())
            || (!all
                && (self.source_collections.is_empty()
                    || self.source_collections.len() != inventory.collections.len()))
            || self
                .source_collections
                .iter()
                .enumerate()
                .any(|(i, scope)| {
                    !scope.stored_name.starts_with(&format!("t{tenant}__"))
                        || !crate::format::valid_collection_name(&scope.stored_name)
                        || !names.insert(&scope.stored_name)
                        || inventory.collections.get(i) != Some(&scope.stored_name)
                })
        {
            return Err("a migration collection scope is invalid");
        }
        let operations = parse_ops(&inventory.operations).map_err(|_| "invalid operations")?;
        graph_scope(operations, inventory.graphs.as_deref(), true)
            .map_err(|_| "invalid graph scopes")
    }

    pub(super) fn active(&self, now: u64) -> bool {
        self.inventory.state == "active" && now < self.inventory.expires_unix_ms
    }
}

#[cfg_attr(not(test), allow(dead_code))]
impl Snapshot {
    /// Selection happens before constructing any serializable inventory.
    pub(super) fn migration_inventory(
        &self,
        tenant: &str,
        lineage: u64,
        now: u64,
    ) -> Result<Vec<MigrationCredential>, &'static str> {
        self.0
            .iter()
            .filter(|entry| entry.tenant_id.as_deref() == Some(tenant))
            .map(|entry| {
                if entry.validate().is_none()
                    || u64::from_str_radix(&entry.lineage, 16).ok() != Some(lineage)
                {
                    return Err("a selected credential has an invalid source lineage or grant");
                }
                let credential = MigrationCredential {
                    inventory: entry.recovery(now),
                    source_collections: entry
                        .collections
                        .iter()
                        .map(|scope| MigrationCollection {
                            stored_name: scope.name.clone(),
                            id: scope.id,
                        })
                        .collect(),
                };
                credential.check(tenant)?;
                Ok(credential)
            })
            .collect()
    }
}

/// Offline migration only. Call after tenant/target-scope certification; this
/// does not alter ordinary issuance, rotation, recovery or a live store.
#[cfg_attr(not(test), allow(dead_code))]
pub(super) fn migration_rekey(
    tenant: &str,
    inventory: &[MigrationCredential],
    source_lineage: u64,
    target_lineage: u64,
    now: u64,
) -> Result<(Snapshot, Snapshot, Vec<String>), ApiError> {
    if source_lineage == 0 || target_lineage == 0 || source_lineage == target_lineage {
        return Err(unprocessable(
            "migration requires distinct nonzero lineages",
        ));
    }
    let mut ids = std::collections::BTreeSet::new();
    if inventory.len() > CAPACITY
        || inventory.iter().any(|credential| {
            credential.check(tenant).is_err() || !ids.insert(credential.inventory.id.as_str())
        })
    {
        return Err(unprocessable("invalid migration credential inventory"));
    }
    let (mut source, mut target, mut tokens) = (Vec::new(), Vec::new(), Vec::new());
    for credential in inventory.iter().filter(|credential| credential.active(now)) {
        let grant = &credential.inventory;
        let secret = random_secret()?;
        let entry = Entry {
            id: grant.id.clone(),
            name: grant.name.clone(),
            all: grant.collections == ["*"],
            collections: credential
                .source_collections
                .iter()
                .map(|scope| Scope {
                    name: scope.stored_name.clone(),
                    id: scope.id,
                })
                .collect(),
            operations: grant.operations.clone(),
            lineage: format!("{source_lineage:016x}"),
            secret_sha256: sha256_hex(secret.as_bytes()),
            created_unix_ms: now,
            expires_unix_ms: grant.expires_unix_ms,
            revoked_unix_ms: None,
            rotated_from: None,
            tenant_id: grant.tenant_id.clone(),
            requests_per_second: grant.requests_per_second,
            max_vectors: grant.max_vectors,
            max_vectors_per_collection: grant.max_vectors_per_collection,
            graphs: grant.graphs.clone(),
        };
        tokens.push(token(&entry, &secret));
        target.push(Entry {
            lineage: format!("{target_lineage:016x}"),
            ..entry.clone()
        });
        source.push(entry);
    }
    Ok((Snapshot(source), Snapshot(target), tokens))
}

/// design-tenant-provisioning ITEM-010 (PRD §3.4 step 7): the first
/// credential of a tenant a managed `create` is provisioning, as the
/// control-state view publishes it. Holds only the secret's hash.
#[cfg_attr(not(test), allow(dead_code))]
pub(super) struct Candidate(Entry);

#[cfg_attr(not(test), allow(dead_code))]
impl Candidate {
    pub(super) fn id(&self) -> &str {
        &self.0.id
    }

    pub(super) fn tenant(&self) -> Option<&str> {
        self.0.tenant_id.as_deref()
    }
}

#[cfg(test)]
impl Candidate {
    /// This candidate's grant under credential `id` and `secret`, with its
    /// token: models an ID collision, or a retry holding the same candidate.
    pub(super) fn with_identity_for_test(mut self, id: &str, secret: &str) -> (Self, String) {
        self.0.id = id.to_owned();
        self.0.secret_sha256 = sha256_hex(secret.as_bytes());
        let token = token(&self.0, secret);
        (self, token)
    }
}

/// Checks a candidate's grant by [`candidate`]'s rules without minting
/// anything: lets a caller refuse a bad grant before it persists anything.
pub(super) fn candidate_grant(
    name: &str,
    operations: &[String],
    graphs: Option<&[String]>,
    expires_unix_ms: u64,
    now: u64,
) -> Result<u16, ApiError> {
    if name.is_empty() || name.len() > 64 || name.chars().any(char::is_control) {
        return Err(unprocessable("name must be 1..64 printable characters"));
    }
    let ops = parse_ops(operations)?;
    graph_scope(ops, graphs, true)?;
    if expires_unix_ms <= now || expires_unix_ms > now + MAX_LIFETIME_MS {
        return Err(unprocessable(
            "expires_unix_ms must be in the future and at most 365 days ahead",
        ));
    }
    Ok(ops)
}

/// Mints a candidate bound to `tenant` and the seeded `lineage`, covering
/// every collection the tenant owns, by the same rules as `create` (name,
/// operations, graph binding, lifetime); 422 otherwise. Returns its token
/// once: the secret is never stored.
#[cfg_attr(not(test), allow(dead_code))]
pub(super) fn candidate(
    tenant: &str,
    lineage: u64,
    name: &str,
    operations: &[String],
    graphs: Option<&[String]>,
    expires_unix_ms: u64,
    now: u64,
) -> Result<(Candidate, String), ApiError> {
    let ops = candidate_grant(name, operations, graphs, expires_unix_ms, now)?;
    let secret = random_secret()?;
    let entry = Entry {
        id: random_id()?,
        name: name.to_owned(),
        all: true,
        collections: Vec::new(),
        operations: op_names(ops).into_iter().map(str::to_owned).collect(),
        lineage: format!("{lineage:016x}"),
        secret_sha256: sha256_hex(secret.as_bytes()),
        created_unix_ms: now,
        expires_unix_ms,
        revoked_unix_ms: None,
        rotated_from: None,
        tenant_id: Some(tenant.to_owned()),
        requests_per_second: None,
        max_vectors: None,
        max_vectors_per_collection: None,
        graphs: graphs.map(<[String]>::to_vec),
    };
    entry
        .validate()
        .ok_or_else(|| unprocessable("candidate credential is invalid"))?;
    let token = token(&entry, &secret);
    Ok((Candidate(entry), token))
}

#[cfg_attr(not(test), allow(dead_code))]
impl Snapshot {
    /// `self` with `candidate` added and each of `revoked` that is bound to
    /// the candidate's tenant revoked (an earlier revocation time is kept);
    /// every other credential stays as it is. 409 `credential_candidate_exists`
    /// if its ID is taken, 409 `credential_limit` at capacity: nothing is
    /// pruned to make room. 409 `candidate_revocation_unconfirmed` if one of
    /// `revoked` would stay live (bound elsewhere).
    pub(super) fn with_candidate(
        &self,
        candidate: &Candidate,
        revoked: &[String],
        now: u64,
    ) -> Result<Self, ApiError> {
        if self.0.iter().any(|entry| entry.id == candidate.0.id) {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "credential_candidate_exists",
                "a credential with the candidate's ID is already published",
            ));
        }
        if self.0.len() >= CAPACITY {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "credential_limit",
                "too many active credentials; revoke some first",
            ));
        }
        let mut next = self.clone();
        for entry in &mut next.0 {
            if entry.tenant_id == candidate.0.tenant_id && revoked.contains(&entry.id) {
                entry.revoked_unix_ms.get_or_insert(now);
            }
        }
        if next
            .0
            .iter()
            .any(|entry| revoked.contains(&entry.id) && entry.state(now) == "active")
        {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "candidate_revocation_unconfirmed",
                "a replaced candidate is bound to another tenant and stays live",
            ));
        }
        next.0.push(candidate.0.clone());
        if !next.valid() {
            return Err(unprocessable("candidate credential is invalid"));
        }
        Ok(next)
    }

    /// Revokes each of `ids` this snapshot holds, whether or not it has
    /// expired (an earlier revocation time is kept), so a regressed clock
    /// or a stale view cannot make it usable again. An ID the snapshot does
    /// not hold is skipped; every other credential stays as it is.
    pub(super) fn revoke_present(&mut self, ids: &[String], now: u64) {
        for entry in &mut self.0 {
            if ids.contains(&entry.id) {
                entry.revoked_unix_ms.get_or_insert(now);
            }
        }
    }

    /// Whether this snapshot holds exactly `candidate` (secret hash, grant,
    /// tenant and lineage) and it is active at `now`.
    pub(super) fn publishes(&self, candidate: &Candidate, now: u64) -> bool {
        self.0
            .iter()
            .any(|entry| *entry == candidate.0 && entry.state(now) == "active")
    }
}

impl RecoveryCredential {
    /// Shape and bounds only; values are never echoed.
    pub(super) fn check(&self) -> Result<(), &'static str> {
        let text = |t: &String| !t.is_empty() && t.len() <= 256 && !t.chars().any(char::is_control);
        let valid = self.id.len() == 12
            && self.id.bytes().all(|b| ID_ALPHABET.contains(&b))
            && !self.name.is_empty()
            && self.name.len() <= 64
            && !self.name.chars().any(char::is_control)
            && self
                .tenant_id
                .as_ref()
                .is_none_or(|id| id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit()))
            && !self.collections.is_empty()
            && self.collections.len() <= 1024
            && self.collections.iter().all(text)
            && parse_ops(&self.operations).is_ok()
            && self
                .graphs
                .as_ref()
                .is_none_or(|g| g.len() <= 1024 && g.iter().all(text))
            // The issue rule (`create`, `Entry::validate`); the vector limits
            // accept any count there, so any u64 is valid here.
            && self
                .requests_per_second
                .is_none_or(|r| (1..=1_000_000).contains(&r))
            && matches!(self.state.as_str(), "active" | "expired" | "revoked");
        if valid {
            Ok(())
        } else {
            Err("a credential entry is invalid")
        }
    }
}

impl Entry {
    fn graph_scope(&self, ops: u16) -> Result<(), ApiError> {
        graph_scope(ops, self.graphs.as_deref(), self.tenant_id.is_some())
    }
}

/// EPIC-011 graph scope rules, shared by issue and load:
/// - `graph_read` / `graph_write` require a `graphs` binding and a binding
///   requires one of them (collection scope never implies graph scope);
/// - the binding is `["*"]` or up to 64 distinct valid graph names;
/// - tenant credentials bind names within their tenant (`g` means the stored
///   graph `t{tenant}__g`), so each name must still be a valid graph name
///   once qualified.
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
    graph_binding(graphs, tenant)
}

/// Bytes `t{tenant_id}__` adds in front of a tenant-owned name.
const TENANT_PREFIX: usize = 1 + 32 + 2;

/// A non-empty graph binding: `["*"]` or 1..64 distinct graph names. Tenant
/// names are limited to 28 bytes so the qualified name fits in 63.
pub(super) fn graph_binding(graphs: &[String], tenant: bool) -> Result<(), ApiError> {
    let max = if tenant { 63 - TENANT_PREFIX } else { 63 };
    let valid = |name: &String| {
        let bytes = name.as_bytes();
        (1..=max).contains(&bytes.len())
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
    } else if tenant {
        Err(unprocessable(
            "graphs must be [\"*\"] or 1..64 distinct graph names of at most 28 bytes for a tenant",
        ))
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

/// Constant-time verification of a `vlk_` token against `entries`. Unknown
/// IDs still hash and compare.
fn verify(entries: &[Entry], token: &str, now: u64) -> Result<Arc<Grant>, ApiError> {
    let rest = token.strip_prefix(TOKEN_PREFIX).ok_or_else(unauthorized)?;
    let (id, secret) = rest.split_once('_').ok_or_else(unauthorized)?;
    if id.len() != 12 || secret.len() != 43 {
        return Err(unauthorized());
    }
    let presented = Sha256::digest(secret.as_bytes());
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
    Ok(Arc::new(entry.grant().ok_or_else(unauthorized)?))
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

    /// Recovery inventory (feature-backup-ha ITEM-003): the public view of
    /// every credential, never its secret hash. A restore reports these for
    /// re-issue and never recreates them (rekey-only).
    pub(super) fn recovery_inventory(&self, now: u64) -> Result<Vec<RecoveryCredential>, ApiError> {
        let entries = self.entries.lock().map_err(internal)?;
        Ok(entries.iter().map(|e| e.recovery(now)).collect())
    }

    /// The entries for a control-state snapshot (feature-aca-ha ITEM-011).
    pub(super) fn snapshot(&self) -> Result<Snapshot, ApiError> {
        Ok(Snapshot(self.entries.lock().map_err(internal)?.clone()))
    }

    /// Constant-time verification (see `verify`).
    pub(super) fn authenticate(&self, token: &str, now: u64) -> Result<Arc<Grant>, ApiError> {
        verify(&self.entries.lock().map_err(internal)?, token, now)
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
    // The route is administrator-only: a reserved name answers like a missing
    // one even for the administrator, who holds no raw access.
    let scope = service.scope(admin::Role::Admin);
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
                            id: scope.body_collection(db, name)?.id(),
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
        let view = store.revoke_id(&log, &id, now(), None)?;
        Ok(Json(json!({"credential": view})))
    })
    .await
    .map_err(internal)?
}

impl Store {
    /// Revokes credential `id` (idempotent) and returns its view. The audit
    /// event names `source` when the change does not come from the admin
    /// API (the offline `control-state update` command).
    pub(super) fn revoke_id(
        &self,
        audit: &audit::Log,
        id: &str,
        now: u64,
        source: Option<&str>,
    ) -> Result<Value, ApiError> {
        let entries = self.entries.lock().map_err(internal)?;
        let index = find(&entries, id)?;
        if entries[index].revoked_unix_ms.is_some() {
            return Ok(entries[index].view(now));
        }
        let mut next = entries.clone();
        next[index].revoked_unix_ms = Some(now);
        let view = next[index].view(now);
        let mut event = json!({"event":"credential_revoked","credential_id":id});
        if let Some(source) = source {
            event["source"] = json!(source);
        }
        self.commit(audit, entries, next, event)?;
        Ok(view)
    }
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
            graphs: None,
        }
    }

    /// A store holding one read credential per `(id, tenant)`, bound to
    /// `lineage`, and the plaintext tokens in the same order.
    pub(crate) fn issued(specs: &[(&str, Option<&str>)], lineage: u64) -> (Store, Vec<String>) {
        let mut tokens = Vec::new();
        let entries = specs
            .iter()
            .enumerate()
            .map(|(i, (id, tenant))| {
                let secret = format!("{i:0>43}");
                tokens.push(format!("{TOKEN_PREFIX}{id}_{secret}"));
                Entry {
                    lineage: format!("{lineage:016x}"),
                    secret_sha256: sha256_hex(secret.as_bytes()),
                    tenant_id: tenant.map(str::to_owned),
                    ..entry(id)
                }
            })
            .collect();
        let store = Store {
            directory: PathBuf::new(),
            entries: Mutex::new(entries),
        };
        (store, tokens)
    }

    /// What the revoke route commits, without its audit and file write.
    pub(crate) fn revoke(store: &Store, id: &str, at: u64) {
        let mut entries = store.entries.lock().unwrap();
        let entry = entries.iter_mut().find(|e| e.id == id).unwrap();
        entry.revoked_unix_ms = Some(at);
    }

    fn event(id: &str) -> Value {
        json!({"event": "credential_created", "credential_id": id})
    }

    #[test]
    fn recovery_inventory_keeps_the_non_secret_limits_through_bundle_and_import() {
        // Review RB03-QUOTAS: the re-issue report carries each credential's
        // rate and live-vector limits; credentials without limits have none.
        use super::super::recovery_bundle as bundle;
        let limited = Entry {
            requests_per_second: Some(250),
            max_vectors: Some(5_000),
            max_vectors_per_collection: Some(0),
            ..entry("abcdefghijkl")
        };
        let store = Store {
            directory: PathBuf::new(),
            entries: Mutex::new(vec![limited, entry("bbbbbbbbbbbb")]),
        };
        let config = bundle::Config {
            tenants: Vec::new(),
            accounts: Vec::new(),
            credentials: store.recovery_inventory(1).unwrap(),
            webhook_subscriptions: 0,
            secret_references: Vec::new(),
            excluded: bundle::EXCLUDED.iter().map(|s| (*s).to_owned()).collect(),
        };
        let policy = super::super::cloud_backup::fake::policy_capture();
        let sealed = bundle::bundle(config, "id", None, &"5c".repeat(32), policy, None, 1);
        let bytes = serde_json::to_vec(&sealed).unwrap();
        let parsed = bundle::parse(&bytes).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let fresh = ("restore-admin", "correct horse battery staple");
        let report = bundle::import(&parsed, dir.path(), fresh).unwrap();
        let reissue = &report["credentials_to_reissue"];
        assert_eq!(reissue[0]["id"], "abcdefghijkl");
        assert_eq!(reissue[0]["requests_per_second"], 250);
        assert_eq!(reissue[0]["max_vectors"], 5_000);
        assert_eq!(reissue[0]["max_vectors_per_collection"], 0);
        for absent in [
            "requests_per_second",
            "max_vectors",
            "max_vectors_per_collection",
        ] {
            assert!(reissue[1].get(absent).is_none(), "{absent}");
        }
        // The live issue rule for the rate applies to a bundled credential.
        for rate in [0, 1_000_001] {
            let mut value: Value = serde_json::from_slice(&bytes).unwrap();
            value["config"]["credentials"][0]["requests_per_second"] = json!(rate);
            let bytes = serde_json::to_vec(&value).unwrap();
            assert!(bundle::parse(&bytes).is_err(), "{rate}");
        }
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
    fn graph_scope_is_qualified_by_tenant() {
        let a = "ab".repeat(16);
        let b = "cd".repeat(16);
        let user = |tenant: Option<String>, ops: &[Op]| {
            Grant::for_user("user:1".into(), "u".into(), tenant, ops, 1, u64::MAX)
        };
        let viewer = user(Some(a.clone()), &[Op::Read, Op::GraphRead]);
        assert!(viewer.allows(Op::GraphRead) && !viewer.allows(Op::GraphWrite));
        assert!(viewer.graph_allowed(&format!("t{a}__g")));
        assert!(!viewer.graph_allowed("g") && !viewer.graph_allowed(&format!("t{b}__g")));
        assert_eq!(viewer.json()["graphs"], json!(["*"]));
        let open = user(None, &[Op::Read, Op::GraphRead]);
        assert!(open.graph_allowed("g") && open.graph_allowed(&format!("t{a}__g")));
        let no_graph = user(None, &[Op::Read]);
        assert!(!no_graph.graph_allowed("any"));
        assert!(no_graph.json().get("graphs").is_none());
        // A stored tenant credential's binding names graphs inside its tenant.
        let secret = "s".repeat(43);
        let hash: String = Sha256::digest(secret.as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let stored = |id: &str, tenant: Option<String>| {
            let mut stored = entry(id);
            stored.tenant_id = tenant;
            stored.operations = vec!["graph_read".to_owned()];
            stored.graphs = Some(vec!["g".to_owned()]);
            stored.secret_sha256 = hash.clone();
            stored
        };
        let dir = tempfile::tempdir().unwrap();
        let store = Store::load(dir.path()).unwrap();
        *store.entries.lock().unwrap() = vec![
            stored("aaaaaaaaaaaa", Some(a.clone())),
            stored("bbbbbbbbbbbb", None),
        ];
        let grant = |id: &str| {
            store
                .authenticate(&format!("{TOKEN_PREFIX}{id}_{secret}"), 1)
                .unwrap()
        };
        let tenanted = grant("aaaaaaaaaaaa");
        assert!(tenanted.graph_allowed(&format!("t{a}__g")));
        assert!(!tenanted.graph_allowed("g") && !tenanted.graph_allowed(&format!("t{a}__h")));
        assert!(!tenanted.graph_allowed(&format!("t{b}__g")));
        let open = grant("bbbbbbbbbbbb");
        assert!(open.graph_allowed("g") && !open.graph_allowed(&format!("t{a}__g")));
        // Tenant names must fit once qualified.
        assert!(graph_binding(&["a".repeat(28)], true).is_ok());
        assert!(graph_binding(&["a".repeat(29)], true).is_err());
        assert!(graph_binding(&["a".repeat(29)], false).is_ok());
        assert!(graph_binding(&[], true).is_err());
        assert!(stored("cccccccccccc", Some(a)).validate().is_some());
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

    /// OD-004: a gateway forwards a signed-in tenant user as a `user:<32 hex>`
    /// principal. It round-trips as a session grant (never an API
    /// credential), and only in the shape `Grant::for_user` makes.
    #[test]
    fn user_principals_round_trip_only_as_tenant_session_grants() {
        let tenant = "ab".repeat(16);
        let id = format!("user:{}", "0f".repeat(16));
        let user = |tenant: Option<String>| {
            Grant::for_user(
                id.clone(),
                "alice".into(),
                tenant,
                &[Op::Read, Op::Search, Op::GraphRead],
                0x1234,
                99,
            )
        };
        let grant = Principal::of(&user(Some(tenant.clone()))).grant().unwrap();
        assert_eq!(grant.id, id);
        assert!(!grant.api_credential);
        assert_eq!(grant.tenant_id.as_deref(), Some(tenant.as_str()));
        assert_eq!(grant.lineage, 0x1234);
        assert_eq!(grant.expires, 99);
        assert!(grant.all && grant.collections.is_empty());
        assert!(grant.allows(Op::Read) && grant.allows(Op::GraphRead));
        assert!(!grant.allows(Op::Write) && !grant.allows(Op::Diagnostics));
        assert_eq!(grant.graphs, ["*"]);

        // Refused: no tenant, a malformed id, limits a session never has,
        // or a collection list instead of the tenant's whole scope.
        assert!(Principal::of(&user(None)).grant().is_none());
        for bad in [
            "user:1",
            "user:",
            "user:zz",
            "users:0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f",
        ] {
            let mut grant = user(Some(tenant.clone()));
            grant.id = bad.to_owned();
            assert!(Principal::of(&grant).grant().is_none(), "{bad}");
        }
        let mut limited = user(Some(tenant.clone()));
        limited.requests_per_second = Some(5);
        assert!(Principal::of(&limited).grant().is_none());
        let mut quota = user(Some(tenant.clone()));
        quota.max_vectors = Some(5);
        assert!(Principal::of(&quota).grant().is_none());
        let mut listed = user(Some(tenant.clone()));
        listed.all = false;
        listed.collections = vec![(format!("t{tenant}__docs"), 1)];
        assert!(Principal::of(&listed).grant().is_none());
        // Only operations an editor or viewer account can hold.
        for op in [Op::Diagnostics, Op::Backup] {
            let elevated = Grant::for_user(
                id.clone(),
                "alice".into(),
                Some(tenant.clone()),
                &[Op::Read, op],
                0x1234,
                99,
            );
            assert!(Principal::of(&elevated).grant().is_none(), "{op:?}");
        }

        // An API credential principal is unchanged.
        let mut api = user(Some(tenant));
        api.id = "abcdefghijkl".to_owned();
        let api = Principal::of(&api).grant().unwrap();
        assert!(api.api_credential);
    }
}
