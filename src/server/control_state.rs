//! Shared control state (feature-aca-ha ITEM-008/011, FR-003, FR-012).
//!
//! One small versioned JSON object in the coordination store carries the
//! credential and tenant state every gateway replica authorizes from, with a
//! monotonically increasing generation `A`. Writers change it only by
//! compare-and-swap on the object's ETag (`If-Match`, or `If-None-Match: *`
//! for the first generation); a lost race re-reads and retries a bounded
//! number of times, and an unknown outcome is reported, never retried
//! blindly. Readers poll it into a disposable cache that is served for at
//! most S_ctl after the last confirmed read and refused after that (FM-004:
//! fail closed, never allow by default). S_ctl is injected (OD-005).
//!
//! The snapshot holds secret hashes, so it is as sensitive as
//! `credentials.json` (SEC-004). It never holds signing keys or plaintext
//! secrets: unknown fields are refused, and credentials are only imported
//! from a live store, never from a recovery inventory (rekey-only).
//!
//! It also carries the tenant directory the gateway routes by (ITEM-014,
//! PRD section 3.11): one record per tenant naming its deployment kind,
//! backend endpoints, envelope audience, lineage, epoch `E`, binding
//! generation and read-routing policy. Tenant status is not repeated there;
//! it is the tenant list of the same snapshot.
//
// Served by `vlite-server gateway` (gateway_mode) and gateway-trusted
// backends (gateway_trust) through a `Feed`; written by
// `vlite-server control-state publish|update`.
use super::cloud_backup::{Cond, ObjectStore, StoreError};
use super::*;
use serde::Serialize;
use std::io::Read as _;
use std::path::PathBuf;
use std::sync::{PoisonError, mpsc};
use std::time::{Duration, Instant};

pub(super) const SCHEMA: &str = "vlite.control-state/1";
/// Same bound as `credentials.json` and `tenants.json`.
const MAX_BYTES: u64 = 1024 * 1024;
/// Compare-and-swap attempts before a write reports contention.
const CAS_ATTEMPTS: usize = 4;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Snapshot {
    schema: String,
    /// `A`: 1 for the first published snapshot, +1 per accepted write.
    generation: u64,
    written_unix_ms: u64,
    tenants: Vec<tenants::Tenant>,
    credentials: credentials::Snapshot,
    /// Absent in objects written before the directory existed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    directory: Vec<Route>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    migrations: Vec<Migration>,
    /// design-tenant-provisioning ITEM-002: one lifecycle slot per tenant.
    /// Absent (and so byte-identical) until the first reservation; an older
    /// consumer refuses a snapshot that carries it (`deny_unknown_fields`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    lifecycle: Vec<Lifecycle>,
}

/// A provisioning operation's kind (design-tenant-provisioning §3.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(not(test), allow(dead_code))]
pub(super) enum OperationKind {
    Create,
    SetReaders,
    Migrate,
    Retire,
}

/// The per-tenant operation slot. `generation` is the slot's own counter:
/// 1 for the first reservation, +1 for each later one.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Lifecycle {
    pub(super) tenant: String,
    pub(super) generation: u64,
    pub(super) slot: Slot,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum Slot {
    /// Held by one operation. Non-routable until `receipt` is published with
    /// the route in the same generation.
    Reserved {
        operation_id: String,
        kind: OperationKind,
        request_digest: String,
        /// `route_digest` of the route the operation expects (None: the
        /// tenant must be unrouted, as for `create`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expected_route: Option<String>,
        cancellation_requested: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        receipt: Option<Receipt>,
    },
    /// The holder's terminal record is sealed; the slot may be reserved
    /// again.
    Released {
        operation_id: String,
        terminal_digest: String,
    },
}

/// Authoritative activation receipt: the route became routable at
/// control-state generation `generation`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Receipt {
    pub(super) operation_id: String,
    pub(super) request_digest: String,
    pub(super) generation: u64,
    pub(super) route_digest: String,
    pub(super) lineage: u64,
    pub(super) epoch: u64,
    pub(super) proof_digest: String,
}

/// At most one lifecycle slot per tenant; same bound as the tenant list.
const MAX_LIFECYCLE: usize = 1024;

fn hex_id(value: &str, len: usize) -> bool {
    value.len() == len
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// SHA-256 (lowercase hex) of a route's canonical JSON.
pub(super) fn route_digest(route: &Route) -> String {
    hex(&Sha256::digest(
        serde_json::to_vec(route).expect("a route always serializes"),
    ))
}

fn lifecycle_error(code: &'static str, message: &'static str) -> ApiError {
    ApiError::new(StatusCode::CONFLICT, code, message)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum AuthorityMode {
    Off,
}

/// How a tenant is deployed (PRD section 3.11; which tenants get which is
/// OD-011).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(super) enum Kind {
    /// Today's single standalone database, fronted unchanged: no reader and
    /// no epoch, because it has no failover to fence.
    SharedPool,
    /// The tenant's own writer app and optional reader app, fenced by `E`.
    Dedicated,
}

/// Where a tenant's staleness-tolerant reads may go (RD-007, FR-010).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum ReadRouting {
    /// Every request goes to the writer (the default).
    #[default]
    WriterOnly,
    /// Opt-in reader routing; not acted on until ITEM-021.
    Readers,
}

/// One tenant directory record.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Route {
    pub(super) tenant: String,
    pub(super) kind: Kind,
    /// Base URL of the writer app (`shared-pool`: the standalone server).
    pub(super) writer: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) reader: Option<String>,
    /// The envelope audience the tenant's backends verify.
    pub(super) audience: String,
    pub(super) lineage: u64,
    /// `E`: required for `dedicated`, absent for `shared-pool`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) epoch: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) authority_mode: Option<AuthorityMode>,
    /// The writer's memory-binding generation; a reader is eligible only at
    /// the same value (FM-009).
    #[serde(default)]
    pub(super) binding_generation: u64,
    #[serde(default)]
    pub(super) read_routing: ReadRouting,
}

impl Route {
    fn valid(&self, tenants: &[tenants::Tenant]) -> bool {
        fn text(value: &str) -> bool {
            !value.is_empty() && value.len() <= 2048 && !value.chars().any(char::is_control)
        }
        let shape = match self.kind {
            Kind::SharedPool => {
                self.authority_mode.is_none()
                    && self.reader.is_none()
                    && self.epoch.is_none()
                    && self.read_routing == ReadRouting::WriterOnly
            }
            Kind::Dedicated if self.authority_mode == Some(AuthorityMode::Off) => {
                self.epoch.is_none()
                    && self.reader.is_none()
                    && self.read_routing == ReadRouting::WriterOnly
            }
            Kind::Dedicated => {
                self.epoch.is_some_and(|epoch| epoch > 0)
                    && (self.read_routing == ReadRouting::WriterOnly || self.reader.is_some())
            }
        };
        shape
            && tenants::routable(tenants, &self.tenant)
            && text(&self.writer)
            && text(&self.audience)
            && self.reader.as_deref().is_none_or(text)
    }
}

#[cfg(test)]
impl Route {
    pub(super) fn dedicated_for_test(tenant: &str, epoch: u64) -> Self {
        Self {
            tenant: tenant.to_owned(),
            kind: Kind::Dedicated,
            writer: format!("https://writer-{tenant}.internal"),
            reader: Some(format!("https://reader-{tenant}.internal")),
            audience: format!("backend-{tenant}"),
            lineage: 0x1234,
            epoch: Some(epoch),
            authority_mode: None,
            binding_generation: 3,
            read_routing: ReadRouting::WriterOnly,
        }
    }

    pub(super) fn shared_for_test(tenant: &str) -> Self {
        Self {
            tenant: tenant.to_owned(),
            kind: Kind::SharedPool,
            writer: "https://shared-pool.internal".to_owned(),
            reader: None,
            audience: "shared-pool".to_owned(),
            lineage: 0x1234,
            epoch: None,
            authority_mode: None,
            binding_generation: 0,
            read_routing: ReadRouting::WriterOnly,
        }
    }
}

/// At most one record per tenant, each valid against `tenants`.
fn valid_directory(directory: &[Route], tenants: &[tenants::Tenant]) -> bool {
    let mut routed = std::collections::HashSet::new();
    directory
        .iter()
        .all(|route| routed.insert(route.tenant.as_str()) && route.valid(tenants))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(not(test), allow(dead_code))]
pub(super) enum Phase {
    Prepared,
    Frozen,
    TargetVerified,
    Validating,
    Released,
    RolledBack,
}

#[cfg_attr(not(test), allow(dead_code))]
impl Phase {
    fn terminal(self) -> bool {
        matches!(self, Self::Released | Self::RolledBack)
    }

    pub(super) fn can_follow(self, previous: Self) -> bool {
        matches!(
            (previous, self),
            (Self::Prepared, Self::Frozen)
                | (Self::Frozen, Self::TargetVerified)
                | (Self::TargetVerified, Self::Validating)
                | (Self::Validating, Self::Released)
                | (
                    Self::Frozen | Self::TargetVerified | Self::Validating,
                    Self::RolledBack
                )
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Migration {
    pub(super) id: String,
    pub(super) tenant: String,
    pub(super) phase: Phase,
    pub(super) source: Route,
    pub(super) target: Route,
    pub(super) source_lineage: u64,
    pub(super) target_lineage: u64,
    pub(super) updated_unix_ms: u64,
}

#[cfg_attr(not(test), allow(dead_code))]
impl Migration {
    fn valid(&self, tenants: &[tenants::Tenant]) -> bool {
        (1..=64).contains(&self.id.len())
            && self
                .id
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            && self.source.tenant == self.tenant
            && self.target.tenant == self.tenant
            && self.source.kind == Kind::SharedPool
            && self.target.kind == Kind::Dedicated
            && self.target.authority_mode == Some(AuthorityMode::Off)
            && self.source.valid(tenants)
            && self.target.valid(tenants)
            && self.source_lineage != 0
            && self.target_lineage != 0
            && self.source_lineage != self.target_lineage
            && self.source.lineage == self.source_lineage
            && self.target.lineage == self.target_lineage
    }

    fn same_identity(&self, other: &Self) -> bool {
        self.id == other.id
            && self.tenant == other.tenant
            && self.source == other.source
            && self.target == other.target
            && self.source_lineage == other.source_lineage
            && self.target_lineage == other.target_lineage
    }
}

fn unavailable(code: &'static str, message: &'static str) -> ApiError {
    ApiError::new(StatusCode::SERVICE_UNAVAILABLE, code, message)
}

impl Snapshot {
    /// The current local credentials and tenants, hashes and revocation and
    /// suspension state verbatim. Not yet published (generation 0).
    pub(super) fn import(
        credentials: &credentials::Store,
        tenants: Option<&tenants::Store>,
    ) -> Result<Self, ApiError> {
        Ok(Self {
            schema: SCHEMA.to_owned(),
            generation: 0,
            written_unix_ms: 0,
            tenants: match tenants {
                Some(tenants) => tenants.snapshot()?,
                None => Vec::new(),
            },
            credentials: credentials.snapshot()?,
            directory: Vec::new(),
            migrations: Vec::new(),
            lifecycle: Vec::new(),
        })
    }

    pub(super) fn generation(&self) -> u64 {
        self.generation
    }

    /// `self` with its tenant directory replaced.
    pub(super) fn with_directory(mut self, directory: Vec<Route>) -> Self {
        self.directory = directory;
        self
    }

    /// A fresh import of `credentials` and `tenants` that keeps this
    /// snapshot's directory. A bare `import` starts with no directory, so
    /// refreshing through it would silently unroute every tenant. Deleting a
    /// tenant that still has a route makes the result fail validation when
    /// published (`control_state_invalid`): unroute it first.
    ///
    /// Restrictions already published stay (security review, finding 1): a
    /// revoked credential stays revoked, the earlier expiry wins, and a
    /// deleted or suspended tenant stays so, even when the admin directory
    /// is an older copy. Resuming a tenant needs `with_suspended`.
    /// Migration records and activated credential views also stay: a local
    /// file may add restrictions but cannot restore retired secret hashes.
    pub(super) fn reimport(
        &self,
        credentials: &credentials::Store,
        tenants: Option<&tenants::Store>,
    ) -> Result<Self, ApiError> {
        let mut next = Self::import(credentials, tenants)?.with_directory(self.directory.clone());
        next.migrations = self.migrations.clone();
        next.lifecycle = self.lifecycle.clone();
        let imported_restrictions = next.credentials.clone();
        for migration in &self.migrations {
            if matches!(
                migration.phase,
                Phase::Validating | Phase::Released | Phase::RolledBack
            ) {
                next.credentials
                    .keep_migration_tenant(&self.credentials, &migration.tenant);
            }
        }
        next.credentials.keep_restrictions(&imported_restrictions);
        next.credentials.keep_restrictions(&self.credentials);
        tenants::keep_restrictions(&mut next.tenants, &self.tenants);
        Ok(next)
    }

    /// `self` with credential `id` revoked; nothing else changes, so a
    /// stale admin directory cannot republish older state with it.
    pub(super) fn with_revoked(mut self, id: &str, now: u64) -> Result<Self, ApiError> {
        self.credentials.revoke(id, now)?;
        Ok(self)
    }

    /// `self` with live tenant `id` suspended or resumed; nothing else
    /// changes.
    pub(super) fn with_suspended(
        mut self,
        id: &str,
        suspend: bool,
        now: u64,
    ) -> Result<Self, ApiError> {
        tenants::set_suspended_in(&mut self.tenants, id, suspend, now)?;
        Ok(self)
    }

    /// design-tenant-provisioning EPIC-002: `self` with `tenant` added and
    /// left unrouted (idempotent, see `tenants::insert_in`); nothing else
    /// changes.
    pub(super) fn with_tenant(&self, tenant: &tenants::Tenant) -> Result<Self, ApiError> {
        let mut next = self.clone();
        tenants::insert_in(&mut next.tenants, tenant)?;
        Ok(next)
    }

    /// The directory record for `tenant`, if it is routed.
    pub(super) fn record_for(&self, tenant: &str) -> Option<&Route> {
        self.directory.iter().find(|route| route.tenant == tenant)
    }

    /// Tenant admission (deleted: 503, suspended: 403) from this snapshot.
    pub(super) fn admit(&self, tenant: &str) -> Result<(), ApiError> {
        tenants::admit(&self.tenants, tenant).map(drop)
    }

    /// The tenant's quota record, as `tenants::Store::quota` answers it.
    pub(super) fn tenant_quota(&self, tenant: &str) -> Result<Option<tenants::Tenant>, ApiError> {
        tenants::quota_in(&self.tenants, tenant)
    }

    /// Authenticates a `vlk_` token against this snapshot, with the same
    /// credential and tenant admission rules as a standalone server.
    pub(super) fn authenticate(
        &self,
        token: &str,
        now: u64,
    ) -> Result<Arc<credentials::Grant>, ApiError> {
        let grant = self.credentials.authenticate(token, now)?;
        if let Some(tenant) = &grant.tenant_id {
            tenants::admit(&self.tenants, tenant)?;
        }
        Ok(grant)
    }

    fn invalid() -> ApiError {
        unavailable("control_state_invalid", "control state object is invalid")
    }

    fn parse(bytes: &[u8]) -> Result<Self, ApiError> {
        let snapshot: Self = serde_json::from_slice(bytes).map_err(|_| Self::invalid())?;
        if snapshot.schema != SCHEMA
            || snapshot.generation == 0
            || !tenants::valid_store(&snapshot.tenants)
            || !snapshot.credentials.valid()
            || !valid_directory(&snapshot.directory, &snapshot.tenants)
            || !snapshot.valid_migrations()
            || !snapshot.valid_lifecycle()
        {
            return Err(Self::invalid());
        }
        Ok(snapshot)
    }
}

#[cfg_attr(not(test), allow(dead_code))]
impl Snapshot {
    fn valid_migrations(&self) -> bool {
        let mut ids = std::collections::HashSet::new();
        let mut active = std::collections::HashSet::new();
        self.migrations.iter().all(|migration| {
            migration.valid(&self.tenants)
                && self.record_for(&migration.tenant).is_some()
                && ids.insert(migration.id.as_str())
                && (migration.phase.terminal() || active.insert(migration.tenant.as_str()))
        })
    }

    pub(super) fn migration(&self, id: &str) -> Option<&Migration> {
        self.migrations.iter().find(|migration| migration.id == id)
    }

    pub(super) fn prepare(
        &self,
        expected_generation: u64,
        migration_id: &str,
        migration: &Migration,
        now: u64,
    ) -> Result<Self, ApiError> {
        if self.generation != expected_generation
            || migration.id != migration_id
            || migration.phase != Phase::Prepared
            || !migration.valid(&self.tenants)
            || self.migration(migration_id).is_some()
            || self
                .migrations
                .iter()
                .any(|existing| existing.tenant == migration.tenant && !existing.phase.terminal())
            || self.record_for(&migration.tenant) != Some(&migration.source)
            || self.reserved(&migration.tenant)
        {
            return Err(tenant_migration::conflict());
        }
        let mut next = self.clone();
        let mut migration = migration.clone();
        migration.updated_unix_ms = now;
        next.migrations.push(migration);
        Ok(next)
    }

    fn migration_index(
        &self,
        expected_generation: u64,
        migration_id: &str,
        identity: &Migration,
    ) -> Result<usize, ApiError> {
        if self.generation != expected_generation {
            return Err(tenant_migration::conflict());
        }
        let index = self
            .migrations
            .iter()
            .position(|migration| migration.id == migration_id)
            .ok_or_else(tenant_migration::conflict)?;
        let migration = &self.migrations[index];
        let route = if matches!(migration.phase, Phase::Validating | Phase::Released) {
            &migration.target
        } else {
            &migration.source
        };
        if !migration.same_identity(identity) || self.record_for(&migration.tenant) != Some(route) {
            return Err(tenant_migration::conflict());
        }
        Ok(index)
    }

    fn with_phase(&self, index: usize, phase: Phase, now: u64) -> Self {
        let mut next = self.clone();
        next.migrations[index].phase = phase;
        next.migrations[index].updated_unix_ms = now;
        next
    }

    pub(super) fn freeze(
        &self,
        expected_generation: u64,
        migration_id: &str,
        identity: &Migration,
        now: u64,
    ) -> Result<Self, ApiError> {
        let index = self.migration_index(expected_generation, migration_id, identity)?;
        if self.migrations[index].phase != Phase::Prepared {
            return Err(tenant_migration::conflict());
        }
        Ok(self.with_phase(index, Phase::Frozen, now))
    }

    pub(super) fn target_verified(
        &self,
        expected_generation: u64,
        migration_id: &str,
        identity: &Migration,
        now: u64,
    ) -> Result<Self, ApiError> {
        let index = self.migration_index(expected_generation, migration_id, identity)?;
        if self.migrations[index].phase != Phase::Frozen {
            return Err(tenant_migration::conflict());
        }
        Ok(self.with_phase(index, Phase::TargetVerified, now))
    }

    fn switch_migration(
        &self,
        index: usize,
        phase: Phase,
        views: &tenant_migration::CredentialViews,
        now: u64,
    ) -> Result<Self, ApiError> {
        let migration = &self.migrations[index];
        if !views.source.migration_views_match(
            &views.target,
            &migration.tenant,
            migration.source_lineage,
            migration.target_lineage,
        ) {
            return Err(tenant_migration::conflict());
        }
        if phase == Phase::RolledBack
            && !self
                .credentials
                .migration_view_is_current(&migration.tenant, &views.target)
        {
            return Err(tenant_migration::conflict());
        }
        let (route, view) = if phase == Phase::Validating {
            (&migration.target, &views.target)
        } else {
            (&migration.source, &views.source)
        };
        let mut next = self.with_phase(index, phase, now);
        next.credentials = self
            .credentials
            .with_migration_view(&migration.tenant, view)
            .ok_or_else(tenant_migration::conflict)?;
        let current = next
            .directory
            .iter_mut()
            .find(|route| route.tenant == migration.tenant)
            .ok_or_else(tenant_migration::conflict)?;
        *current = route.clone();
        Ok(next)
    }

    pub(super) fn cutover(
        &self,
        expected_generation: u64,
        migration_id: &str,
        identity: &Migration,
        views: &tenant_migration::CredentialViews,
        now: u64,
    ) -> Result<Self, ApiError> {
        let index = self.migration_index(expected_generation, migration_id, identity)?;
        if self.migrations[index].phase != Phase::TargetVerified {
            return Err(tenant_migration::conflict());
        }
        self.switch_migration(index, Phase::Validating, views, now)
    }

    pub(super) fn rollback(
        &self,
        expected_generation: u64,
        migration_id: &str,
        identity: &Migration,
        views: &tenant_migration::CredentialViews,
        target_acked_mutations: bool,
        now: u64,
    ) -> Result<Self, ApiError> {
        let index = self.migration_index(expected_generation, migration_id, identity)?;
        let phase = self.migrations[index].phase;
        if phase == Phase::Released {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "migration_source_released",
                "migration source was irreversibly released",
            ));
        }
        if !Phase::RolledBack.can_follow(phase) {
            return Err(tenant_migration::conflict());
        }
        if target_acked_mutations {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "rollback_requires_reconciliation",
                "target acknowledged mutations; rollback requires reconciliation",
            ));
        }
        if phase == Phase::Validating {
            self.switch_migration(index, Phase::RolledBack, views, now)
        } else {
            Ok(self.with_phase(index, Phase::RolledBack, now))
        }
    }

    pub(super) fn release(
        &self,
        expected_generation: u64,
        migration_id: &str,
        identity: &Migration,
        ack_irreversible: bool,
        now: u64,
    ) -> Result<Self, ApiError> {
        let index = self.migration_index(expected_generation, migration_id, identity)?;
        if self.migrations[index].phase != Phase::Validating {
            return Err(tenant_migration::conflict());
        }
        if !ack_irreversible {
            return Err(ApiError::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "tenant_migration_ack_required",
                "source release requires explicit irreversible acknowledgement",
            ));
        }
        Ok(self.with_phase(index, Phase::Released, now))
    }
}

/// design-tenant-provisioning ITEM-002/003: the lifecycle slot. Every change
/// is one control-state CAS (`Objects::update`); these build the next state.
#[cfg_attr(not(test), allow(dead_code))]
impl Snapshot {
    fn valid_lifecycle(&self) -> bool {
        let mut tenants = std::collections::HashSet::new();
        self.lifecycle.len() <= MAX_LIFECYCLE
            && self.lifecycle.iter().all(|entry| {
                hex_id(&entry.tenant, 32)
                    && entry.generation > 0
                    && tenants.insert(entry.tenant.as_str())
                    && self.valid_slot(entry)
            })
    }

    fn valid_slot(&self, entry: &Lifecycle) -> bool {
        match &entry.slot {
            Slot::Released {
                operation_id,
                terminal_digest,
            } => hex_id(operation_id, 32) && hex_id(terminal_digest, 64),
            Slot::Reserved {
                operation_id,
                kind: _,
                request_digest,
                expected_route,
                cancellation_requested,
                receipt,
            } => {
                let shape = hex_id(operation_id, 32)
                    && hex_id(request_digest, 64)
                    && expected_route.as_deref().is_none_or(|d| hex_id(d, 64))
                    // A reservation excludes a legacy migration of the tenant.
                    && !self
                        .migrations
                        .iter()
                        .any(|m| m.tenant == entry.tenant && !m.phase.terminal());
                // A published receipt must still describe the routed tenant:
                // replacing the directory cannot silently drop or change it.
                shape
                    && receipt.as_ref().is_none_or(|receipt| {
                        !cancellation_requested
                            && receipt.operation_id == *operation_id
                            && receipt.request_digest == *request_digest
                            && hex_id(&receipt.proof_digest, 64)
                            && self.record_for(&entry.tenant).is_some_and(|route| {
                                route_digest(route) == receipt.route_digest
                                    && route.lineage == receipt.lineage
                                    && route.epoch == Some(receipt.epoch)
                            })
                    })
            }
        }
    }

    fn reserved(&self, tenant: &str) -> bool {
        self.lifecycle(tenant)
            .is_some_and(|entry| matches!(entry.slot, Slot::Reserved { .. }))
    }

    /// The tenant's lifecycle slot, if it was ever reserved.
    pub(super) fn lifecycle(&self, tenant: &str) -> Option<&Lifecycle> {
        self.lifecycle.iter().find(|entry| entry.tenant == tenant)
    }

    fn slot_index(&self, tenant: &str, operation_id: &str) -> Result<usize, ApiError> {
        self.lifecycle
            .iter()
            .position(|entry| {
                entry.tenant == tenant
                    && matches!(&entry.slot, Slot::Reserved { operation_id: held, .. } if held == operation_id)
            })
            .ok_or_else(|| {
                lifecycle_error(
                    "lifecycle_not_reserved",
                    "the tenant's lifecycle slot is not held by this operation",
                )
            })
    }

    /// Reserves the tenant's slot for `operation_id`. Refused while another
    /// operation holds it or a legacy migration of the tenant is active; a
    /// `create` needs an unrouted tenant, every other kind a routed one.
    pub(super) fn reserve(
        &self,
        tenant: &str,
        operation_id: &str,
        kind: OperationKind,
        request_digest: &str,
    ) -> Result<Self, ApiError> {
        if self.reserved(tenant)
            || self
                .migrations
                .iter()
                .any(|m| m.tenant == tenant && !m.phase.terminal())
        {
            return Err(lifecycle_error(
                "lifecycle_reserved",
                "another operation or migration holds the tenant",
            ));
        }
        if matches!(
            self.lifecycle(tenant).map(|e| &e.slot),
            Some(Slot::Released { operation_id: done, .. }) if done == operation_id
        ) {
            return Err(lifecycle_error(
                "operation_terminal",
                "the operation has already ended",
            ));
        }
        let route = self.record_for(tenant);
        if (kind == OperationKind::Create) != route.is_none() {
            return Err(lifecycle_error(
                "lifecycle_transition_unavailable",
                "the tenant's routing state does not allow this operation",
            ));
        }
        let mut next = self.clone();
        let slot = Slot::Reserved {
            operation_id: operation_id.to_owned(),
            kind,
            request_digest: request_digest.to_owned(),
            expected_route: route.map(route_digest),
            cancellation_requested: false,
            receipt: None,
        };
        match next.lifecycle.iter_mut().find(|e| e.tenant == tenant) {
            Some(entry) => {
                entry.generation = entry.generation.checked_add(1).ok_or_else(Self::invalid)?;
                entry.slot = slot;
            }
            None => next.lifecycle.push(Lifecycle {
                tenant: tenant.to_owned(),
                generation: 1,
                slot,
            }),
        }
        Ok(next)
    }

    /// Records a cancellation request. After activation a cancel cannot
    /// unpublish the route: 409 `retirement_required`.
    pub(super) fn request_cancel(
        &self,
        tenant: &str,
        operation_id: &str,
    ) -> Result<Self, ApiError> {
        let index = self.slot_index(tenant, operation_id)?;
        let mut next = self.clone();
        if let Slot::Reserved {
            cancellation_requested,
            receipt,
            ..
        } = &mut next.lifecycle[index].slot
        {
            if receipt.is_some() {
                return Err(lifecycle_error(
                    "retirement_required",
                    "the operation already activated its route; retire the tenant instead",
                ));
            }
            *cancellation_requested = true;
        }
        Ok(next)
    }

    /// Publishes a `create`'s writer-only dedicated route together with its
    /// receipt in one generation. Credential and tenant restrictions are the
    /// published ones, unchanged. Refused after a cancellation request.
    pub(super) fn activate(
        &self,
        tenant: &str,
        operation_id: &str,
        route: &Route,
        proof_digest: &str,
    ) -> Result<Self, ApiError> {
        let index = self.slot_index(tenant, operation_id)?;
        let Slot::Reserved {
            kind,
            request_digest,
            expected_route,
            cancellation_requested,
            receipt,
            ..
        } = &self.lifecycle[index].slot
        else {
            unreachable!("slot_index returns reserved slots");
        };
        if *cancellation_requested {
            return Err(lifecycle_error(
                "operation_cancelled",
                "cancellation was requested before activation",
            ));
        }
        let safe = route.tenant == tenant
            && route.kind == Kind::Dedicated
            && route.authority_mode.is_none()
            && route.epoch.is_some_and(|epoch| epoch > 0)
            && route.reader.is_none()
            && route.read_routing == ReadRouting::WriterOnly
            && hex_id(proof_digest, 64);
        if *kind != OperationKind::Create
            || receipt.is_some()
            || expected_route.is_some()
            || self.record_for(tenant).is_some()
            || !safe
        {
            return Err(lifecycle_error(
                "lifecycle_transition_unavailable",
                "the route or slot does not allow this activation",
            ));
        }
        let receipt = Receipt {
            operation_id: operation_id.to_owned(),
            request_digest: request_digest.clone(),
            generation: self.generation.checked_add(1).ok_or_else(Self::invalid)?,
            route_digest: route_digest(route),
            lineage: route.lineage,
            epoch: route.epoch.unwrap_or_default(),
            proof_digest: proof_digest.to_owned(),
        };
        let mut next = self.clone();
        next.directory.push(route.clone());
        if let Slot::Reserved { receipt: slot, .. } = &mut next.lifecycle[index].slot {
            *slot = Some(receipt);
        }
        Ok(next)
    }

    /// Releases the slot. Only a sealed terminal record (`Sealed`, which
    /// only `tenant_provisioning` seals) can do this, and its outcome must
    /// match the slot.
    pub(super) fn release_lifecycle(
        &self,
        sealed: &tenant_provisioning::Sealed,
    ) -> Result<Self, ApiError> {
        let index = self.slot_index(sealed.tenant_id(), sealed.operation_id())?;
        let Slot::Reserved {
            cancellation_requested,
            receipt,
            ..
        } = &self.lifecycle[index].slot
        else {
            unreachable!("slot_index returns reserved slots");
        };
        let matches = match sealed.outcome() {
            tenant_provisioning::TerminalOutcome::Succeeded { receipt: sealed } => {
                receipt.as_ref() == Some(sealed)
            }
            tenant_provisioning::TerminalOutcome::Cancelled => {
                *cancellation_requested && receipt.is_none()
            }
        };
        if !matches {
            return Err(lifecycle_error(
                "operation_terminal_conflict",
                "the sealed outcome does not match the lifecycle slot",
            ));
        }
        let mut next = self.clone();
        next.lifecycle[index].slot = Slot::Released {
            operation_id: sealed.operation_id().to_owned(),
            terminal_digest: sealed.digest().to_owned(),
        };
        Ok(next)
    }
}

impl Snapshot {
    /// `self` stamped as generation `generation`, serialized.
    fn publish(mut self, generation: u64, now: u64) -> Result<(Self, Vec<u8>), ApiError> {
        self.schema = SCHEMA.to_owned();
        self.generation = generation;
        self.written_unix_ms = now;
        let bytes = serde_json::to_vec(&self).map_err(|_| Self::invalid())?;
        // Never publish what a reader would refuse.
        Self::parse(&bytes)?;
        if bytes.len() as u64 > MAX_BYTES {
            return Err(Self::invalid());
        }
        Ok((self, bytes))
    }
}

/// The control-state object in a coordination store.
pub(super) struct Objects {
    store: Arc<dyn ObjectStore>,
    name: String,
}

impl Objects {
    pub(super) fn new(store: Arc<dyn ObjectStore>, name: impl Into<String>) -> Self {
        Self {
            store,
            name: name.into(),
        }
    }

    /// The published snapshot and its ETag.
    pub(super) fn read(&self) -> Result<(Snapshot, String), ApiError> {
        let blob = self
            .store
            .get(&self.name, MAX_BYTES)
            .map_err(|error| match error {
                StoreError::NotFound => {
                    unavailable("control_state_missing", "control state is not published")
                }
                StoreError::TooLarge => Snapshot::invalid(),
                _ => unavailable(
                    "control_state_unavailable",
                    "control state could not be read",
                ),
            })?;
        Ok((Snapshot::parse(&blob.bytes)?, blob.etag))
    }

    fn write(&self, bytes: &[u8], cond: Cond<'_>) -> Result<(), Option<ApiError>> {
        match self.store.put(&self.name, bytes, cond) {
            Ok(_) => Ok(()),
            Err(StoreError::Conflict) => Err(None),
            Err(StoreError::Unknown(_)) => Err(Some(unavailable(
                "control_state_indeterminate",
                "control state write outcome is unknown; read it before retrying",
            ))),
            Err(_) => Err(Some(unavailable(
                "control_state_unavailable",
                "control state could not be written",
            ))),
        }
    }

    /// Publishes `snapshot` as generation 1; refused if any exists.
    pub(super) fn create(&self, snapshot: Snapshot, now: u64) -> Result<Snapshot, ApiError> {
        let (snapshot, bytes) = snapshot.publish(1, now)?;
        match self.write(&bytes, Cond::Create) {
            Ok(()) => Ok(snapshot),
            Err(None) => Err(ApiError::new(
                StatusCode::CONFLICT,
                "control_state_exists",
                "control state is already published",
            )),
            Err(Some(error)) => Err(error),
        }
    }

    /// Replaces the snapshot with `change(current)` at generation `A + 1`.
    pub(super) fn update(
        &self,
        now: u64,
        mut change: impl FnMut(&Snapshot) -> Result<Snapshot, ApiError>,
    ) -> Result<Snapshot, ApiError> {
        for _ in 0..CAS_ATTEMPTS {
            let (current, etag) = self.read()?;
            let generation = current
                .generation
                .checked_add(1)
                .ok_or_else(Snapshot::invalid)?;
            let (next, bytes) = change(&current)?.publish(generation, now)?;
            match self.write(&bytes, Cond::Match(&etag)) {
                Ok(()) => return Ok(next),
                // Lost the race: re-read and apply the change to the winner.
                Err(None) => continue,
                Err(Some(error)) => return Err(error),
            }
        }
        Err(ApiError::new(
            StatusCode::CONFLICT,
            "control_state_conflict",
            "control state is changing too often; retry later",
        ))
    }

    /// One `control-state update` through the compare-and-swap. A
    /// revocation, suspension or resumption is applied to the published
    /// state just read, not taken from the admin directory, so a concurrent
    /// or stale admin directory cannot republish older credentials or
    /// tenants with it (security review, finding 1); a lost race re-applies
    /// it to the winner. Only `Edit::Reimport` reads the admin directory. A
    /// given `directory` replaces the tenant routes. With `expected`, the
    /// update is refused (409 `control_state_generation_mismatch`) unless
    /// the published generation is exactly that. A tenant whose lifecycle
    /// slot is reserved only takes restrictions (`keep_reservations`).
    pub(super) fn apply(
        &self,
        now: u64,
        edit: &Edit<'_>,
        directory: Option<&[Route]>,
        expected: Option<u64>,
    ) -> Result<Snapshot, ApiError> {
        self.update(now, |current| {
            if let Some(expected) = expected
                && current.generation != expected
            {
                return Err(ApiError::new(
                    StatusCode::CONFLICT,
                    "control_state_generation_mismatch",
                    format!(
                        "control state is at generation {}, not the expected {expected}; nothing was published",
                        current.generation
                    ),
                ));
            }
            let next = match edit {
                Edit::Revoke(id) => current.clone().with_revoked(id, now)?,
                Edit::Suspend(id, suspend) => current.clone().with_suspended(id, *suspend, now)?,
                Edit::Reimport(credentials, tenants) => {
                    current.reimport(credentials, Some(*tenants))?
                }
                Edit::Directory => current.clone(),
            };
            let next = match directory {
                Some(directory) => next.with_directory(directory.to_vec()),
                None => next,
            };
            keep_reservations(current, &next)?;
            Ok(next)
        })
    }
}

/// design-tenant-provisioning ITEM-007: a control-state edit (legacy
/// directory replacement, revoke, suspend/resume, admin re-import, so also
/// an admin tenant deletion) may change a tenant whose lifecycle slot is
/// reserved only by adding a restriction: a suspension, a revocation or an
/// earlier credential expiry. Its route, tenant record, tenant-bound
/// credentials and slot stay as published until the managed operation
/// releases the slot; otherwise 409 `lifecycle_reserved`, nothing written.
fn keep_reservations(current: &Snapshot, next: &Snapshot) -> Result<(), ApiError> {
    for entry in &current.lifecycle {
        if !matches!(entry.slot, Slot::Reserved { .. }) {
            continue;
        }
        let tenant = entry.tenant.as_str();
        let (before, after) = (
            TenantView::of(current, tenant)?,
            TenantView::of(next, tenant)?,
        );
        if current.record_for(tenant) != next.record_for(tenant)
            || current.lifecycle(tenant) != next.lifecycle(tenant)
            || !after.restricts(&before)
        {
            return Err(lifecycle_error(
                "lifecycle_reserved",
                "a managed operation holds the tenant; until it ends, a control-state edit may only suspend it or revoke its credentials",
            ));
        }
    }
    Ok(())
}

/// What `keep_reservations` compares: the tenant record without its
/// suspension time, and each tenant-bound credential without its
/// revocation and expiry, kept separately so they may only tighten.
struct TenantView {
    tenant: Option<Value>,
    suspended: bool,
    credentials: std::collections::BTreeMap<String, (Value, bool, u64)>,
}

impl TenantView {
    fn of(snapshot: &Snapshot, tenant: &str) -> Result<Self, ApiError> {
        let value = |v: Result<Value, serde_json::Error>| v.map_err(|_| Snapshot::invalid());
        let mut record = snapshot
            .tenants
            .iter()
            .find(|t| t.id == tenant)
            .map(|t| value(serde_json::to_value(t)))
            .transpose()?;
        let suspended = record
            .as_mut()
            .and_then(Value::as_object_mut)
            .and_then(|t| t.remove("suspended_unix_ms"))
            .is_some_and(|at| !at.is_null());
        let mut credentials = std::collections::BTreeMap::new();
        let Value::Array(entries) = value(serde_json::to_value(&snapshot.credentials))? else {
            return Err(Snapshot::invalid());
        };
        for mut entry in entries {
            let Some(fields) = entry.as_object_mut() else {
                return Err(Snapshot::invalid());
            };
            if fields.get("tenant_id").and_then(Value::as_str) != Some(tenant) {
                continue;
            }
            let revoked = fields
                .remove("revoked_unix_ms")
                .is_some_and(|at| !at.is_null());
            let expires = fields
                .remove("expires_unix_ms")
                .and_then(|at| at.as_u64())
                .ok_or_else(Snapshot::invalid)?;
            let id = fields
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(Snapshot::invalid)?
                .to_owned();
            credentials.insert(id, (entry, revoked, expires));
        }
        Ok(Self {
            tenant: record,
            suspended,
            credentials,
        })
    }

    /// Whether `self` is `before` with at most added restrictions.
    fn restricts(&self, before: &Self) -> bool {
        self.tenant == before.tenant
            && (self.suspended || !before.suspended)
            && self.credentials.len() == before.credentials.len()
            && before
                .credentials
                .iter()
                .all(|(id, (old, revoked, expires))| {
                    self.credentials
                        .get(id)
                        .is_some_and(|(new, now_revoked, now_expires)| {
                            new == old && (*now_revoked || !revoked) && now_expires <= expires
                        })
                })
    }
}

/// What one `control-state update` changes besides the routes.
pub(super) enum Edit<'a> {
    /// Revoke the credential.
    Revoke(String),
    /// Suspend (`true`) or resume (`false`) the live tenant.
    Suspend(String, bool),
    /// Re-import the admin directory's credentials and tenants; published
    /// restrictions stay (`Snapshot::reimport`).
    Reimport(&'a credentials::Store, &'a tenants::Store),
    /// Nothing; only the given routes change.
    Directory,
}

struct Cached {
    snapshot: Arc<Snapshot>,
    etag: String,
    /// Monotonic time of the last read that confirmed this snapshot.
    confirmed: u64,
}

/// A replica's disposable cache of the control state.
pub(super) struct Poller {
    objects: Objects,
    s_ctl_ms: u64,
    cache: Mutex<Option<Cached>>,
}

impl Poller {
    pub(super) fn new(objects: Objects, s_ctl_ms: u64) -> Result<Self, &'static str> {
        if s_ctl_ms == 0 {
            return Err("S_ctl must be positive");
        }
        Ok(Self {
            objects,
            s_ctl_ms,
            cache: Mutex::new(None),
        })
    }

    /// Reads the object at monotonic time `now` (ms) and returns the cached
    /// generation. A failed read leaves the cache and its age unchanged.
    pub(super) fn poll(&self, now: u64) -> Result<u64, ApiError> {
        let (snapshot, etag) = self.objects.read()?;
        let mut cache = self.cache.lock().unwrap_or_else(PoisonError::into_inner);
        match cache.as_mut() {
            Some(cached) if cached.etag == etag => cached.confirmed = cached.confirmed.max(now),
            Some(cached) if snapshot.generation <= cached.snapshot.generation => {
                return Err(unavailable(
                    "control_state_regressed",
                    "control state generation went backwards",
                ));
            }
            _ => {
                *cache = Some(Cached {
                    snapshot: Arc::new(snapshot),
                    etag,
                    confirmed: now,
                })
            }
        }
        Ok(cache
            .as_ref()
            .map_or(0, |cached| cached.snapshot.generation))
    }

    /// The snapshot to authorize with at monotonic time `now`, or 503 when
    /// none was read or it was last confirmed more than S_ctl ago.
    pub(super) fn current(&self, now: u64) -> Result<Arc<Snapshot>, ApiError> {
        let cache = self.cache.lock().unwrap_or_else(PoisonError::into_inner);
        let cached = cache.as_ref().ok_or_else(|| {
            unavailable(
                "control_state_unavailable",
                "control state has not been read",
            )
        })?;
        match now.checked_sub(cached.confirmed) {
            Some(age) if age <= self.s_ctl_ms => Ok(cached.snapshot.clone()),
            _ => Err(unavailable(
                "control_state_stale",
                "control state is older than its staleness bound",
            )),
        }
    }
}

/// An object name below the target's coordination prefix: 1..=256 of
/// `[a-z0-9._/-]`, relative, no empty, `.` or `..` segment, and never one
/// of the set protocol's own objects.
pub(super) fn valid_object_name(name: &str) -> bool {
    (1..=256).contains(&name.len())
        && name.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'/' | b'-')
        })
        && name
            .split('/')
            .all(|segment| !matches!(segment, "" | "." | ".."))
        && !matches!(name, "owner.json" | "catalog.json")
        && !name.starts_with("sets/")
}

/// Where a process reads (and the CLI writes) the control state: a cloud
/// backup target file (account, containers, managed identity or loopback
/// emulator key) plus an object name under its coordination prefix.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Source {
    pub(super) target_file: PathBuf,
    pub(super) object: String,
    #[serde(default)]
    pub(super) shared_key_file: Option<PathBuf>,
}

impl Source {
    /// Checks the object name; touches no file or store.
    pub(super) fn validate(&self) -> Result<(), &'static str> {
        if valid_object_name(&self.object) {
            Ok(())
        } else {
            Err(
                "control_state.object must be a relative [a-z0-9._/-] name outside the set protocol",
            )
        }
    }

    /// Opens the store (blocking client: call off the async runtime).
    pub(super) fn connect(&self) -> Result<Objects, cloud_recovery::RecoveryError> {
        let (store, prefix) = self.coordination()?;
        Ok(Objects::new(store, format!("{prefix}{}", self.object)))
    }

    /// The coordination store and its prefix (blocking, as `connect`).
    pub(super) fn coordination(
        &self,
    ) -> Result<(Arc<dyn ObjectStore>, String), cloud_recovery::RecoveryError> {
        let sets = cloud_recovery::connect(&self.target_file, self.shared_key_file.as_deref())?;
        let (store, prefix) = sets.coordination();
        Ok((store, prefix.to_owned()))
    }
}

/// Bound for a `--directory-file`: the snapshot's own size bound.
pub(super) fn read_directory_file(path: &std::path::Path) -> Result<Vec<Route>, ApiError> {
    let invalid = || {
        ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "directory_invalid",
            "directory file must be a JSON array of directory records of at most 1 MiB",
        )
    };
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .and_then(|file| file.take(MAX_BYTES + 1).read_to_end(&mut bytes))
        .map_err(|_| invalid())?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err(invalid());
    }
    serde_json::from_slice(&bytes).map_err(|_| invalid())
}

/// A named background thread running `tick` every interval until stopped:
/// the poll loop of a `Feed` and of a tenant projection `Reader`.
#[derive(Default)]
pub(in crate::server) struct PollThread(
    Mutex<Option<(mpsc::Sender<()>, std::thread::JoinHandle<()>)>>,
);

impl PollThread {
    pub(in crate::server) fn start(
        &self,
        name: &str,
        interval: Duration,
        tick: impl Fn() + Send + 'static,
    ) -> std::io::Result<()> {
        let (stop, stopped) = mpsc::channel::<()>();
        let thread = std::thread::Builder::new()
            .name(name.to_owned())
            .spawn(move || {
                while let Err(mpsc::RecvTimeoutError::Timeout) = stopped.recv_timeout(interval) {
                    tick();
                }
            })?;
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = Some((stop, thread));
        Ok(())
    }

    /// Stops and joins the thread (at most one in-flight tick).
    pub(in crate::server) fn stop(&self) {
        if let Some(thread) = self.begin_stop() {
            let _ = thread.join();
        }
    }

    /// Tells the thread to stop without waiting for it: it ends after any
    /// tick in progress. Returns the thread to join, once.
    pub(in crate::server) fn begin_stop(&self) -> Option<std::thread::JoinHandle<()>> {
        let (stop, thread) = self
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()?;
        drop(stop);
        Some(thread)
    }
}

/// A serving process's live control state: one confirmed read before it
/// serves, then a background thread re-reading every poll interval until
/// stopped. Every failed poll is logged (`control_state_poll_failed`, with
/// its code), never swallowed; the cached snapshot is refused once it was
/// last confirmed more than S_ctl ago.
pub(super) struct Feed {
    poller: Poller,
    base: Instant,
    interval: Duration,
    worker: PollThread,
}

impl Feed {
    pub(super) fn new(
        objects: Objects,
        s_ctl_ms: u64,
        poll_interval_ms: u64,
    ) -> Result<Self, &'static str> {
        if poll_interval_ms == 0 || poll_interval_ms >= s_ctl_ms {
            return Err("poll interval must be positive and below S_ctl");
        }
        Ok(Self {
            poller: Poller::new(objects, s_ctl_ms)?,
            base: Instant::now(),
            interval: Duration::from_millis(poll_interval_ms),
            worker: PollThread::default(),
        })
    }

    fn now(&self) -> u64 {
        u64::try_from(self.base.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    /// The snapshot to authorize with now, or the fail-closed 503.
    pub(super) fn current(&self) -> Result<Arc<Snapshot>, ApiError> {
        self.poller.current(self.now())
    }

    /// One blocking read; a failure is logged and returned.
    pub(super) fn poll_once(&self) -> Result<u64, ApiError> {
        let result = self.poller.poll(self.now());
        if let Err(error) = &result {
            log_stderr(
                "warn",
                "control_state",
                json!({"event":"control_state_poll_failed","code":error.code}).to_string(),
            );
        }
        result
    }

    /// Starts the poll thread; `on_poll` sees each confirmed generation.
    pub(super) fn start(
        self: &Arc<Self>,
        on_poll: impl Fn(u64) + Send + 'static,
    ) -> std::io::Result<()> {
        let feed = self.clone();
        self.worker
            .start("control-state-poll", self.interval, move || {
                if let Ok(generation) = feed.poll_once() {
                    on_poll(generation);
                }
            })
    }

    /// Stops and joins the poll thread (at most one in-flight read).
    pub(super) fn stop(&self) {
        self.worker.stop();
    }

    /// Tells the poll thread to stop without waiting for it: it ends after
    /// any read in progress. Returns the thread to join, once.
    pub(super) fn begin_stop(&self) -> Option<std::thread::JoinHandle<()>> {
        self.worker.begin_stop()
    }
}

#[cfg(test)]
mod tests {
    use super::super::cloud_backup::fake::{Fault, Memory, Op};
    use super::super::principal_envelope::{self as envelope, tests as env};
    use super::*;

    const T1: &str = "0123456789abcdef0123456789abcdef";
    const T2: &str = "fedcba9876543210fedcba9876543210";
    const NAME: &str = "control/state.json";

    struct World {
        memory: Arc<Memory>,
        credentials: credentials::Store,
        tenants: tenants::Store,
        tokens: Vec<String>,
    }

    fn world() -> World {
        let (credentials, tokens) = credentials::tests::issued(
            &[
                ("aaaaaaaaaaaa", Some(T1)),
                ("bbbbbbbbbbbb", Some(T2)),
                ("cccccccccccc", None),
            ],
            0x77,
        );
        World {
            memory: Arc::new(Memory::default()),
            credentials,
            tenants: tenants::Store::for_test(vec![
                tenants::Tenant::for_test(T1, None),
                tenants::Tenant::for_test(T2, None),
            ]),
            tokens,
        }
    }

    impl World {
        fn objects(&self) -> Objects {
            Objects::new(self.memory.clone(), NAME)
        }
        fn import(&self) -> Snapshot {
            Snapshot::import(&self.credentials, Some(&self.tenants)).unwrap()
        }
    }

    #[test]
    fn authority_off_dedicated_route_is_valid_without_epoch_or_reader() {
        let w = world();
        let mut route = serde_json::to_value(Route::dedicated_for_test(T1, 1)).unwrap();
        route["authority_mode"] = json!("off");
        route.as_object_mut().unwrap().remove("epoch");
        route.as_object_mut().unwrap().remove("reader");
        let route: Route = serde_json::from_value(route).unwrap();
        assert!(route.valid(&w.import().tenants));
    }

    #[test]
    fn authority_off_with_epoch_or_reader_or_shared_is_rejected() {
        let w = world();
        let mut base = serde_json::to_value(Route::shared_for_test(T1)).unwrap();
        base["kind"] = json!("dedicated");
        base["authority_mode"] = json!("off");
        for (field, value) in [
            ("epoch", json!(1)),
            ("reader", json!("https://reader.internal")),
            ("kind", json!("shared-pool")),
            ("read_routing", json!("readers")),
        ] {
            let mut raw = base.clone();
            raw[field] = value;
            let route: Route = serde_json::from_value(raw).unwrap();
            assert!(!route.valid(&w.import().tenants));
        }
    }

    #[test]
    fn legacy_dedicated_route_still_requires_positive_epoch() {
        let w = world();
        for epoch in [None, Some(0), Some(1)] {
            let mut route = Route::dedicated_for_test(T1, 1);
            route.epoch = epoch;
            assert_eq!(route.valid(&w.import().tenants), epoch == Some(1));
        }
    }

    #[test]
    fn legacy_snapshot_without_migrations_or_authority_mode_round_trips_unchanged() {
        let w = world();
        w.objects()
            .create(
                w.import()
                    .with_directory(vec![Route::dedicated_for_test(T1, 7)]),
                1,
            )
            .unwrap();
        let bytes = w.memory.raw(NAME).unwrap();
        let raw = String::from_utf8(bytes.clone()).unwrap();
        assert!(!raw.contains("migrations") && !raw.contains("authority_mode"));
        assert_eq!(
            bytes,
            serde_json::to_vec(&Snapshot::parse(&bytes).unwrap()).unwrap()
        );
    }

    #[test]
    fn overlapping_migrations_for_one_tenant_are_rejected() {
        let w = world();
        let source = Route::shared_for_test(T1);
        w.objects()
            .create(w.import().with_directory(vec![source.clone()]), 1)
            .unwrap();
        let mut target = serde_json::to_value(&source).unwrap();
        target["kind"] = json!("dedicated");
        target["authority_mode"] = json!("off");
        target["lineage"] = json!(0x5678);
        let migration = json!({
            "id": "move-one", "tenant": T1, "phase": "prepared",
            "source": source, "target": target,
            "source_lineage": 0x1234, "target_lineage": 0x5678, "updated_unix_ms": 1
        });
        let mut raw: Value = serde_json::from_slice(&w.memory.raw(NAME).unwrap()).unwrap();
        raw["migrations"] = json!([migration]);
        assert!(Snapshot::parse(&serde_json::to_vec(&raw).unwrap()).is_ok());
        let mut second = raw["migrations"][0].clone();
        second["id"] = json!("move-two");
        raw["migrations"].as_array_mut().unwrap().push(second);
        assert!(Snapshot::parse(&serde_json::to_vec(&raw).unwrap()).is_err());
        raw["migrations"][1]["phase"] = json!("rolled_back");
        assert!(Snapshot::parse(&serde_json::to_vec(&raw).unwrap()).is_ok());
    }

    #[test]
    fn malformed_migration_identity_routes_lineages_and_unknown_fields_are_rejected() {
        let w = world();
        let source = Route::shared_for_test(T1);
        w.objects()
            .create(w.import().with_directory(vec![source.clone()]), 1)
            .unwrap();
        let mut target = source.clone();
        target.kind = Kind::Dedicated;
        target.authority_mode = Some(AuthorityMode::Off);
        target.lineage += 1;
        let migration = Migration {
            id: "move-one".into(),
            tenant: T1.into(),
            phase: Phase::Prepared,
            source: source.clone(),
            target,
            source_lineage: source.lineage,
            target_lineage: source.lineage + 1,
            updated_unix_ms: 1,
        };
        let mut good: Value = serde_json::from_slice(&w.memory.raw(NAME).unwrap()).unwrap();
        good["migrations"] = json!([migration]);
        assert!(Snapshot::parse(&serde_json::to_vec(&good).unwrap()).is_ok());
        for (field, value) in [
            ("id", json!("")),
            ("id", json!("A")),
            ("id", json!("x".repeat(65))),
            ("id", json!("move_one")),
            ("tenant", json!(T2)),
            ("source_lineage", json!(0)),
            ("source_lineage", json!(source.lineage + 1)),
            ("target_lineage", json!(0)),
            ("target_lineage", json!(source.lineage)),
            ("phase", json!("unknown")),
            ("extra", json!(true)),
        ] {
            let mut raw = good.clone();
            raw["migrations"][0][field] = value;
            assert!(
                Snapshot::parse(&serde_json::to_vec(&raw).unwrap()).is_err(),
                "{field}"
            );
        }
        for (route, field, value) in [
            ("source", "kind", json!("dedicated")),
            ("target", "kind", json!("shared-pool")),
            ("target", "authority_mode", Value::Null),
            ("target", "epoch", json!(1)),
            ("target", "tenant", json!(T2)),
            ("target", "lineage", json!(source.lineage + 2)),
            ("source", "audience", json!("")),
        ] {
            let mut raw = good.clone();
            raw["migrations"][0][route][field] = value;
            assert!(
                Snapshot::parse(&serde_json::to_vec(&raw).unwrap()).is_err(),
                "{route}.{field}"
            );
        }
        let mut raw = good.clone();
        raw["migrations"][0]["phase"] = json!("released");
        let duplicate = raw["migrations"][0].clone();
        raw["migrations"].as_array_mut().unwrap().push(duplicate);
        assert!(Snapshot::parse(&serde_json::to_vec(&raw).unwrap()).is_err());
        let mut raw = good;
        raw["directory"] = json!([]);
        assert!(Snapshot::parse(&serde_json::to_vec(&raw).unwrap()).is_err());
    }

    #[test]
    fn import_carries_hashes_and_state_but_no_plaintext_secret() {
        let w = world();
        credentials::tests::revoke(&w.credentials, "cccccccccccc", 5);
        let published = w.objects().create(w.import(), 10).unwrap();
        assert_eq!(published.generation(), 1);
        let raw = String::from_utf8(w.memory.raw(NAME).unwrap()).unwrap();
        for token in &w.tokens {
            let secret = token.rsplit('_').next().unwrap();
            assert!(!raw.contains(secret), "plaintext secret in control state");
        }
        assert!(raw.contains("secret_sha256") && raw.contains(SCHEMA));
        let (read, _) = w.objects().read().unwrap();
        // Same rules as standalone: active works, revoked is refused, a
        // tenant credential is admitted only while its tenant is active.
        let grant = read.authenticate(&w.tokens[0], 20).unwrap();
        assert_eq!(grant.tenant_id.as_deref(), Some(T1));
        assert_eq!(grant.lineage, 0x77);
        assert_eq!(
            read.authenticate(&w.tokens[2], 20).err().unwrap().status,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            read.authenticate(
                "vlk_aaaaaaaaaaaa_wrongwrongwrongwrongwrongwrongwrongwrongwro",
                20
            )
            .err()
            .unwrap()
            .status,
            StatusCode::UNAUTHORIZED
        );
    }

    #[test]
    fn tenanted_credentials_need_their_tenant_in_the_snapshot() {
        let w = world();
        let alone = Snapshot::import(&w.credentials, None).unwrap();
        assert_eq!(
            alone.authenticate(&w.tokens[0], 1).err().unwrap().code,
            "tenant_unavailable"
        );
        assert!(alone.authenticate(&w.tokens[2], 1).is_ok());
    }

    #[test]
    fn parse_refuses_unknown_schema_fields_and_invalid_content() {
        let w = world();
        w.objects().create(w.import(), 1).unwrap();
        let good: Value = serde_json::from_slice(&w.memory.raw(NAME).unwrap()).unwrap();
        let mut cases = Vec::new();
        let mut v = good.clone();
        v["schema"] = json!("vlite.control-state/2");
        cases.push(v);
        let mut v = good.clone();
        v["signing_key"] = json!("00");
        cases.push(v);
        let mut v = good.clone();
        v["generation"] = json!(0);
        cases.push(v);
        let mut v = good.clone();
        v["credentials"][0]["secret_sha256"] = json!("not-a-hash");
        cases.push(v);
        let mut v = good.clone();
        v["credentials"][1] = v["credentials"][0].clone();
        cases.push(v);
        let mut v = good.clone();
        v["tenants"][1] = v["tenants"][0].clone();
        cases.push(v);
        // A recovery inventory entry has no hash: rekey-only, never imported.
        let mut v = good.clone();
        v["credentials"][0] = json!({"id":"aaaaaaaaaaaa","name":"svc","tenant_id":null,
            "collections":["*"],"operations":["read"],"state":"active","expires_unix_ms":1});
        cases.push(v);
        for case in cases {
            w.memory.set_raw(NAME, &serde_json::to_vec(&case).unwrap());
            let error = w.objects().read().err().expect("refused");
            assert_eq!(error.code, "control_state_invalid");
        }
        w.memory.set_raw(NAME, b"{");
        assert_eq!(
            w.objects().read().err().unwrap().code,
            "control_state_invalid"
        );
    }

    #[test]
    fn create_is_exclusive_and_update_advances_generation_by_cas() {
        let w = world();
        w.objects().create(w.import(), 1).unwrap();
        let again = w.objects().create(w.import(), 2).err().unwrap();
        assert_eq!(again.status, StatusCode::CONFLICT);
        let next = w.objects().update(3, |_| Ok(w.import())).unwrap();
        assert_eq!(next.generation(), 2);
        assert_eq!(w.objects().read().unwrap().0.generation(), 2);
    }

    #[test]
    fn update_retries_a_lost_race_against_the_new_state() {
        let w = world();
        w.objects().create(w.import(), 1).unwrap();
        let mut seen = Vec::new();
        let other = w.memory.clone();
        let mut first = true;
        let result = w
            .objects()
            .update(3, |current| {
                seen.push(current.generation());
                if first {
                    first = false;
                    // Concurrent writer publishes generation 3 first.
                    Objects::new(other.clone(), NAME)
                        .update(3, |s| Ok(s.clone()))
                        .unwrap();
                }
                Ok(current.clone())
            })
            .unwrap();
        assert_eq!(seen, vec![1, 2]);
        assert_eq!(result.generation(), 3);
    }

    #[test]
    fn update_reports_unknown_outcomes_and_exhausted_contention() {
        let w = world();
        w.objects().create(w.import(), 1).unwrap();
        w.memory.fault(Op::Put, NAME, Fault::After, 0, 1);
        let error = w.objects().update(2, |s| Ok(s.clone())).err().unwrap();
        assert_eq!(error.code, "control_state_indeterminate");
        assert_eq!(error.status, StatusCode::SERVICE_UNAVAILABLE);
        // Applied despite the unknown result; never re-applied blindly.
        assert_eq!(w.objects().read().unwrap().0.generation(), 2);

        let other = w.memory.clone();
        let error = w
            .objects()
            .update(3, |s| {
                Objects::new(other.clone(), NAME)
                    .update(3, |s| Ok(s.clone()))
                    .unwrap();
                Ok(s.clone())
            })
            .err()
            .unwrap();
        assert_eq!(error.code, "control_state_conflict");
        assert_eq!(error.status, StatusCode::CONFLICT);
    }

    #[test]
    fn poller_fails_closed_without_a_snapshot_and_after_s_ctl() {
        let w = world();
        let poller = Poller::new(w.objects(), 5_000).unwrap();
        assert!(Poller::new(w.objects(), 0).is_err());
        assert_eq!(
            poller.current(0).err().unwrap().code,
            "control_state_unavailable"
        );
        assert_eq!(poller.poll(0).err().unwrap().code, "control_state_missing");
        w.objects().create(w.import(), 1).unwrap();
        assert_eq!(poller.poll(100).unwrap(), 1);
        // TEST-014: served up to S_ctl after the last confirmation, then 503.
        assert!(poller.current(5_100).is_ok());
        let stale = poller.current(5_101).err().unwrap();
        assert_eq!(
            (stale.status, stale.code),
            (StatusCode::SERVICE_UNAVAILABLE, "control_state_stale")
        );
        // Unreachable store: the age is not refreshed, so it stays closed.
        w.memory.fault(Op::Get, NAME, Fault::Before, 0, 1);
        assert!(poller.poll(5_200).is_err());
        assert_eq!(
            poller.current(5_200).err().unwrap().code,
            "control_state_stale"
        );
        // A confirming read with the same ETag refreshes the age.
        let poller = Poller::new(w.objects(), 5_000).unwrap();
        assert_eq!(poller.poll(10).unwrap(), 1);
        assert_eq!(poller.poll(5_000).unwrap(), 1);
        assert!(poller.current(10_000).is_ok());
        // A clock that moves backwards is refused, not trusted.
        assert_eq!(
            poller.current(4_999).err().unwrap().code,
            "control_state_stale"
        );
    }

    #[test]
    fn poller_refuses_a_generation_regression() {
        let w = world();
        w.objects().create(w.import(), 1).unwrap();
        let old = w.memory.raw(NAME).unwrap();
        w.objects().update(2, |s| Ok(s.clone())).unwrap();
        let poller = Poller::new(w.objects(), 1_000).unwrap();
        assert_eq!(poller.poll(0).unwrap(), 2);
        w.memory.set_raw(NAME, &old);
        let error = poller.poll(500).err().unwrap();
        assert_eq!(error.code, "control_state_regressed");
        // Neither replaced nor refreshed by the regressed read.
        assert_eq!(poller.current(1_000).unwrap().generation(), 2);
        assert!(poller.current(1_001).is_err());
    }

    #[test]
    fn begin_stop_returns_at_once_while_a_read_is_in_progress() {
        let w = world();
        w.objects().create(w.import(), 1).unwrap();
        let feed = Arc::new(Feed::new(w.objects(), 60_000, 10).unwrap());
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        w.memory.hook(Op::Get, NAME, move || {
            entered_tx.send(()).unwrap();
            let _ = release_rx.recv();
        });
        feed.start(|_| {}).unwrap();
        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let thread = feed.begin_stop().expect("the poll thread is running");
        assert!(!thread.is_finished());
        assert!(feed.begin_stop().is_none());
        release_tx.send(()).unwrap();
        thread.join().unwrap();
    }

    /// TEST-004 (library level): a revocation and a suspension published as
    /// generation 2 reach a replica on its next poll; a backend told the new
    /// generation refuses envelopes minted from the old snapshot; beyond
    /// S_ctl with the store unreachable everything fails closed.
    #[test]
    fn revocation_and_suspension_propagate_and_old_generations_are_refused() {
        let w = world();
        let objects = w.objects();
        objects.create(w.import(), 1).unwrap();
        let stale_replica = Poller::new(w.objects(), 5_000).unwrap();
        let fresh_replica = Poller::new(w.objects(), 5_000).unwrap();
        stale_replica.poll(0).unwrap();
        fresh_replica.poll(0).unwrap();
        let before = stale_replica.current(0).unwrap();
        let grant = before.authenticate(&w.tokens[1], 1).unwrap();
        assert_eq!(grant.tenant_id.as_deref(), Some(T2));

        credentials::tests::revoke(&w.credentials, "aaaaaaaaaaaa", 2);
        w.tenants.suspend_for_test(T2, 2);
        let published = objects.update(3, |_| Ok(w.import())).unwrap();
        assert_eq!(published.generation(), 2);

        assert_eq!(fresh_replica.poll(1_000).unwrap(), 2);
        let after = fresh_replica.current(1_000).unwrap();
        assert_eq!(
            after.authenticate(&w.tokens[0], 4).err().unwrap().status,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            after.authenticate(&w.tokens[1], 4).err().unwrap().code,
            "tenant_suspended"
        );
        assert!(after.authenticate(&w.tokens[2], 4).is_ok());

        // A backend that learned A=2 refuses an envelope minted at A=1.
        let key = env::signer("k1", 1);
        let verifier =
            envelope::Verifier::new(vec![("k1".into(), key.public_key())], env::policy(Some(T2)))
                .unwrap();
        verifier.raise_minimum(published.generation());
        let old = envelope::Claims {
            aud: "backend-a".into(),
            iat: 10,
            exp: 20,
            generation: before.generation(),
            principal: credentials::Principal::of(&grant),
        };
        let token = key.sign(&old).unwrap();
        assert_eq!(
            verifier.verify(&token, 10).err(),
            Some(envelope::Refusal::Stale)
        );

        // The replica that cannot reach the store stops within S_ctl.
        w.memory.fault(Op::Get, NAME, Fault::Before, 0, 100);
        assert!(stale_replica.poll(1_000).is_err());
        assert!(stale_replica.current(5_000).is_ok());
        assert_eq!(
            stale_replica.current(5_001).err().unwrap().code,
            "control_state_stale"
        );
    }

    #[test]
    fn directory_round_trips_and_survives_a_reimport() {
        let w = world();
        // Objects written before the directory existed carry none and
        // still parse: every tenant is unrouted.
        w.objects().create(w.import(), 1).unwrap();
        assert!(
            !String::from_utf8(w.memory.raw(NAME).unwrap())
                .unwrap()
                .contains("directory")
        );
        assert_eq!(w.objects().read().unwrap().0.record_for(T1), None);

        let directory = vec![Route::dedicated_for_test(T1, 7), Route::shared_for_test(T2)];
        let next = w
            .objects()
            .update(2, |s| Ok(s.clone().with_directory(directory.clone())))
            .unwrap();
        assert_eq!(next.generation(), 2);
        let raw = String::from_utf8(w.memory.raw(NAME).unwrap()).unwrap();
        assert!(raw.contains(r#""kind":"shared-pool""#) && raw.contains("writer_only"));
        let (read, _) = w.objects().read().unwrap();
        assert_eq!(read.record_for(T1), Some(&directory[0]));
        assert_eq!(read.record_for(T2).unwrap().kind, Kind::SharedPool);
        assert_eq!(read.record_for(T2).unwrap().epoch, None);
        assert_eq!(read.record_for("00000000000000000000000000000000"), None);

        // A credential refresh keeps the routes; a bare import would drop them.
        credentials::tests::revoke(&w.credentials, "cccccccccccc", 3);
        let refreshed = w
            .objects()
            .update(4, |s| s.reimport(&w.credentials, Some(&w.tenants)))
            .unwrap();
        assert_eq!(refreshed.generation(), 3);
        assert_eq!(refreshed.record_for(T1), Some(&directory[0]));
        assert!(refreshed.authenticate(&w.tokens[2], 5).is_err());
    }

    #[test]
    fn invalid_directory_records_are_refused() {
        let w = world();
        let bad = Route {
            epoch: Some(0),
            ..Route::dedicated_for_test(T1, 1)
        };
        let error = w
            .objects()
            .create(w.import().with_directory(vec![bad]), 1)
            .err()
            .unwrap();
        assert_eq!(error.code, "control_state_invalid");
        assert!(w.memory.raw(NAME).is_none(), "nothing published");

        let directory = vec![Route::dedicated_for_test(T1, 7), Route::shared_for_test(T2)];
        w.objects()
            .create(w.import().with_directory(directory), 1)
            .unwrap();
        let good: Value = serde_json::from_slice(&w.memory.raw(NAME).unwrap()).unwrap();
        let mut cases = Vec::new();
        let mut v = good.clone();
        v["directory"][1] = v["directory"][0].clone();
        cases.push(v);
        let mut v = good.clone();
        v["directory"][0]["tenant"] = json!("00000000000000000000000000000000");
        cases.push(v);
        let mut v = good.clone();
        v["tenants"][0]["deleted"] = json!(true);
        cases.push(v);
        for (field, value) in [
            ("writer", json!("")),
            ("writer", json!("https://a\nb")),
            ("audience", json!("")),
            ("epoch", json!(0)),
            ("epoch", Value::Null),
            ("kind", json!("replica")),
            ("read_routing", json!("anywhere")),
            ("signing_key", json!("00")),
        ] {
            let mut v = good.clone();
            v["directory"][0][field] = value;
            cases.push(v);
        }
        // Opt-in reader routing needs a reader.
        let mut v = good.clone();
        v["directory"][0]["read_routing"] = json!("readers");
        v["directory"][0].as_object_mut().unwrap().remove("reader");
        cases.push(v);
        // `shared-pool` has nothing to fence and no reader.
        let mut v = good.clone();
        v["directory"][1]["epoch"] = json!(1);
        cases.push(v);
        let mut v = good.clone();
        v["directory"][1]["reader"] = json!("https://reader");
        cases.push(v);
        for case in cases {
            w.memory.set_raw(NAME, &serde_json::to_vec(&case).unwrap());
            let error = w.objects().read().err().expect("refused");
            assert_eq!(error.code, "control_state_invalid", "{case}");
        }
        let mut v = good.clone();
        v["directory"][0]["read_routing"] = json!("readers");
        w.memory.set_raw(NAME, &serde_json::to_vec(&v).unwrap());
        let (read, _) = w.objects().read().unwrap();
        assert_eq!(
            read.record_for(T1).unwrap().read_routing,
            ReadRouting::Readers
        );
    }

    fn revoke(id: &str) -> Edit<'static> {
        Edit::Revoke(id.to_owned())
    }

    fn suspend(id: &str, suspend: bool) -> Edit<'static> {
        Edit::Suspend(id.to_owned(), suspend)
    }

    #[test]
    fn edits_survive_each_other_and_a_stale_reimport() {
        // Security review, finding 1: each edit changes only the published
        // state, so the admin directory (`w`, which saw none of them) cannot
        // undo one by republishing its older copy.
        let w = world();
        let objects = w.objects();
        objects.create(w.import(), 1).unwrap();
        let unauthorized =
            |read: &Snapshot, i: usize| read.authenticate(&w.tokens[i], 9).err().unwrap().status;
        assert_eq!(
            objects
                .apply(2, &revoke("aaaaaaaaaaaa"), None, None)
                .unwrap()
                .generation(),
            2
        );
        assert_eq!(
            objects
                .apply(3, &suspend(T2, true), None, None)
                .unwrap()
                .generation(),
            3
        );
        let (read, _) = objects.read().unwrap();
        assert_eq!(unauthorized(&read, 0), StatusCode::UNAUTHORIZED);
        assert_eq!(
            read.authenticate(&w.tokens[1], 9).err().unwrap().code,
            "tenant_suspended"
        );

        // A newer credential reaches the published state only by re-import,
        // which keeps the published revocation and suspension.
        let (newer, tokens) = credentials::tests::issued(
            &[
                ("aaaaaaaaaaaa", Some(T1)),
                ("bbbbbbbbbbbb", Some(T2)),
                ("cccccccccccc", None),
                ("dddddddddddd", Some(T1)),
            ],
            0x77,
        );
        let edit = Edit::Reimport(&newer, &w.tenants);
        assert_eq!(
            objects.apply(4, &edit, None, Some(3)).unwrap().generation(),
            4
        );
        let (read, _) = objects.read().unwrap();
        assert_eq!(unauthorized(&read, 0), StatusCode::UNAUTHORIZED);
        assert_eq!(
            read.authenticate(&w.tokens[1], 9).err().unwrap().code,
            "tenant_suspended"
        );
        assert!(read.authenticate(&tokens[3], 9).is_ok());

        // A re-import that did not see the current generation publishes
        // nothing.
        let error = objects.apply(5, &edit, None, Some(3)).err().unwrap();
        assert_eq!(error.status, StatusCode::CONFLICT);
        assert_eq!(error.code, "control_state_generation_mismatch");
        assert_eq!(objects.read().unwrap().0.generation(), 4);

        // Resuming is explicit, and revoking again keeps the first time.
        let first: Value = serde_json::from_slice(&w.memory.raw(NAME).unwrap()).unwrap();
        objects.apply(6, &suspend(T2, false), None, None).unwrap();
        objects
            .apply(7, &revoke("aaaaaaaaaaaa"), None, None)
            .unwrap();
        let (read, _) = objects.read().unwrap();
        assert_eq!(read.generation(), 6);
        assert!(read.authenticate(&w.tokens[1], 9).is_ok());
        let last: Value = serde_json::from_slice(&w.memory.raw(NAME).unwrap()).unwrap();
        assert_eq!(first["credentials"][0], last["credentials"][0]);
    }

    #[test]
    fn edits_to_unknown_ids_publish_nothing() {
        let w = world();
        let objects = w.objects();
        objects.create(w.import(), 1).unwrap();
        let error = objects
            .apply(2, &revoke("dddddddddddd"), None, None)
            .err()
            .unwrap();
        assert_eq!(
            (error.status, error.code),
            (StatusCode::NOT_FOUND, "credential_not_found")
        );
        for state in [true, false] {
            let error = objects
                .apply(2, &suspend(&"0".repeat(32), state), None, None)
                .err()
                .unwrap();
            assert_eq!(
                (error.status, error.code),
                (StatusCode::NOT_FOUND, "tenant_not_found")
            );
        }
        assert_eq!(objects.read().unwrap().0.generation(), 1);
    }

    #[test]
    fn a_lost_race_reapplies_the_edit_to_the_winner() {
        let w = world();
        w.objects().create(w.import(), 1).unwrap();
        let other = w.memory.clone();
        w.memory.hook(Op::Put, NAME, move || {
            Objects::new(other, NAME)
                .apply(2, &revoke("bbbbbbbbbbbb"), None, None)
                .unwrap();
        });
        let next = w
            .objects()
            .apply(3, &revoke("aaaaaaaaaaaa"), None, None)
            .unwrap();
        assert_eq!(next.generation(), 3);
        let (read, _) = w.objects().read().unwrap();
        for token in &w.tokens[..2] {
            assert_eq!(
                read.authenticate(token, 9).err().unwrap().status,
                StatusCode::UNAUTHORIZED
            );
        }
        assert!(read.authenticate(&w.tokens[2], 9).is_ok());
    }

    #[test]
    fn a_reimport_never_revives_a_deleted_tenant() {
        let w = world();
        w.objects().create(w.import(), 1).unwrap();
        // T2 was deleted through another admin directory; `w` still holds it.
        let mut v: Value = serde_json::from_slice(&w.memory.raw(NAME).unwrap()).unwrap();
        let tenants = v["tenants"].as_array_mut().unwrap();
        let t2 = tenants.iter_mut().find(|t| t["id"] == T2).unwrap();
        t2["deleted"] = json!(true);
        w.memory.set_raw(NAME, &serde_json::to_vec(&v).unwrap());
        let edit = Edit::Reimport(&w.credentials, &w.tenants);
        w.objects().apply(2, &edit, None, Some(1)).unwrap();
        let (read, _) = w.objects().read().unwrap();
        assert_eq!(read.admit(T2).err().unwrap().code, "tenant_unavailable");
        assert_eq!(
            read.authenticate(&w.tokens[1], 9).err().unwrap().code,
            "tenant_unavailable"
        );
        assert!(read.authenticate(&w.tokens[0], 9).is_ok());
        // A deleted tenant cannot be resumed either.
        let error = w
            .objects()
            .apply(3, &suspend(T2, false), None, None)
            .err()
            .unwrap();
        assert_eq!(error.code, "tenant_not_found");
    }

    /// design-tenant-provisioning ITEM-007: publishes `directory`, then
    /// reserves T1 for a `kind` operation.
    fn reserved(w: &World, directory: Vec<Route>, kind: OperationKind) -> Objects {
        let objects = w.objects();
        objects
            .create(w.import().with_directory(directory), 1)
            .unwrap();
        objects
            .update(2, |current| {
                current.reserve(T1, &"1".repeat(32), kind, &"2".repeat(64))
            })
            .unwrap();
        objects
    }

    fn assert_reserved_refusal(objects: &Objects, result: Result<Snapshot, ApiError>) {
        let before = objects.read().unwrap().0.generation();
        let error = result.err().expect("refused");
        assert_eq!(
            (error.status, error.code),
            (StatusCode::CONFLICT, "lifecycle_reserved")
        );
        assert_eq!(objects.read().unwrap().0.generation(), before);
    }

    #[test]
    fn a_reserved_tenant_takes_only_restrictions_from_control_edits() {
        let w = world();
        let directory = vec![
            Route::dedicated_for_test(T1, 1),
            Route::dedicated_for_test(T2, 1),
        ];
        let objects = reserved(&w, directory.clone(), OperationKind::SetReaders);

        // The legacy directory replacement cannot move, change or drop the
        // reserved tenant's route.
        let mut moved = directory.clone();
        moved[0].epoch = Some(2);
        let mut readers = directory.clone();
        readers[0].read_routing = ReadRouting::Readers;
        for routes in [moved, readers, vec![directory[1].clone()]] {
            let result = objects.apply(3, &Edit::Directory, Some(&routes[..]), None);
            assert_reserved_refusal(&objects, result);
        }
        // A re-import cannot add a credential to it or delete it.
        let (newer, _) = credentials::tests::issued(
            &[
                ("aaaaaaaaaaaa", Some(T1)),
                ("bbbbbbbbbbbb", Some(T2)),
                ("cccccccccccc", None),
                ("dddddddddddd", Some(T1)),
            ],
            0x77,
        );
        let result = objects.apply(3, &Edit::Reimport(&newer, &w.tenants), None, None);
        assert_reserved_refusal(&objects, result);
        let only_t2 = tenants::Store::for_test(vec![tenants::Tenant::for_test(T2, None)]);
        let result = objects.apply(3, &Edit::Reimport(&w.credentials, &only_t2), None, None);
        assert_reserved_refusal(&objects, result);

        // Restrictions still apply at once, and only restrictions.
        objects
            .apply(3, &revoke("aaaaaaaaaaaa"), None, None)
            .unwrap();
        objects.apply(4, &suspend(T1, true), None, None).unwrap();
        let result = objects.apply(5, &suspend(T1, false), None, None);
        assert_reserved_refusal(&objects, result);
        // A re-import that keeps the reserved tenant (and its published
        // restrictions) as it is still publishes.
        objects
            .apply(5, &Edit::Reimport(&w.credentials, &w.tenants), None, None)
            .unwrap();

        // Unreserved tenants keep the legacy behaviour.
        let mut t2_moved = directory.clone();
        t2_moved[1].epoch = Some(2);
        objects
            .apply(6, &Edit::Directory, Some(&t2_moved[..]), None)
            .unwrap();
        objects.apply(7, &suspend(T2, true), None, None).unwrap();
        objects.apply(8, &suspend(T2, false), None, None).unwrap();
        let (newer_t2, _) = credentials::tests::issued(
            &[
                ("aaaaaaaaaaaa", Some(T1)),
                ("bbbbbbbbbbbb", Some(T2)),
                ("cccccccccccc", None),
                ("dddddddddddd", Some(T2)),
            ],
            0x77,
        );
        let read = objects
            .apply(9, &Edit::Reimport(&newer_t2, &w.tenants), None, None)
            .unwrap();
        assert_eq!(read.generation(), 9);
        assert!(read.record_for(T1).unwrap() == &directory[0]);
        assert!(read.reserved(T1));
        assert_eq!(read.admit(T1).err().unwrap().code, "tenant_suspended");
    }

    #[test]
    fn a_create_reservation_cannot_be_routed_or_deleted_by_a_control_edit() {
        let w = world();
        let t2 = Route::dedicated_for_test(T2, 1);
        let objects = reserved(&w, vec![t2.clone()], OperationKind::Create);
        // Routing the unrouted reserved tenant outside its operation would
        // bypass the operation's verification and activation receipt.
        let routes = [t2.clone(), Route::dedicated_for_test(T1, 1)];
        let result = objects.apply(3, &Edit::Directory, Some(&routes[..]), None);
        assert_reserved_refusal(&objects, result);
        let only_t2 = tenants::Store::for_test(vec![tenants::Tenant::for_test(T2, None)]);
        let result = objects.apply(3, &Edit::Reimport(&w.credentials, &only_t2), None, None);
        assert_reserved_refusal(&objects, result);
        // Nothing about T1 changed; T2's route may still be replaced.
        objects
            .apply(3, &Edit::Directory, Some(&[][..]), None)
            .unwrap();
        let (read, _) = objects.read().unwrap();
        assert!(read.record_for(T1).is_none() && read.reserved(T1));
    }
}
