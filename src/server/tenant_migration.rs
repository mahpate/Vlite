//! Sanitized tenant migration state and credential rekey (HA EPIC-007, ITEM-028).
//!
//! Implemented test-first against
//! `docs/projects/feature-aca-ha/feature-aca-ha.prd.md` ITEM-028.
//!
//! The offline coordinator supplies sidecar/import observations to `select`,
//! verifies projected resources, then obtains `TargetState` with `for_target`.
//! Rekey returns sensitive candidate snapshots and a separate one-time token
//! output, not a live update. Control-state transitions below atomically
//! publish generation-fenced routes and credential views while retaining
//! current restrictions. Live admission and read-only-window guards remain
//! unwired.

use super::control_state::{Migration, Objects, Phase, Snapshot};
use super::reserved::Bindings;
use super::{ApiError, StatusCode, credentials, recovery_bundle, tenants, users};
use crate::memory::binding::{self, BindingSet, BindingState, Recorded};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

const SCHEMA: u32 = 1;

#[derive(Debug, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))]
pub(super) enum MigrationError {
    UnknownTenant,
    DeletedTenant,
    UnknownSidecar,
    OpenImport,
    CrossTenantBinding,
    CorruptOrphan,
    Invalid(&'static str),
    StaleGrant {
        principal_id: String,
        stored_name: String,
    },
    Unavailable,
}

impl std::fmt::Display for MigrationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownTenant => f.write_str("migration tenant not found"),
            Self::DeletedTenant => f.write_str("deleted tenants cannot migrate"),
            Self::UnknownSidecar => f.write_str("unrecognized or symlinked migration sidecar"),
            Self::OpenImport => f.write_str("selected tenant has an open import job"),
            Self::CrossTenantBinding => f.write_str("migration binding crosses tenant boundaries"),
            Self::CorruptOrphan => f.write_str("corrupt or conflicting migration orphan record"),
            Self::Invalid(message) => f.write_str(message),
            Self::StaleGrant {
                principal_id,
                stored_name,
            } => {
                write!(
                    f,
                    "principal {principal_id} has a stale grant to {stored_name}"
                )
            }
            Self::Unavailable => f.write_str("migration state or randomness is unavailable"),
        }
    }
}

impl std::error::Error for MigrationError {}

/// Explicit observations supplied by the offline coordinator, not filesystem
/// authority inferred from package paths. No webhook contents enter this API.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(not(test), allow(dead_code))]
pub(super) struct SidecarEntry {
    pub name: String,
    pub directory: bool,
    pub symlink: bool,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(not(test), allow(dead_code))]
pub(super) struct ImportJob {
    pub stored_collection: String,
    pub open: bool,
}

#[cfg_attr(not(test), allow(dead_code))]
pub(super) struct Selection<'a> {
    pub tenants: &'a [tenants::Tenant],
    pub accounts: Option<&'a users::Accounts>,
    pub credentials: &'a credentials::Snapshot,
    pub bindings: &'a BindingSet,
    pub orphans: &'a [Recorded],
    pub sidecars: &'a [SidecarEntry],
    pub imports: &'a [ImportJob],
    pub source_lineage: u64,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct TenantState {
    schema: u32,
    pub tenant: tenants::Tenant,
    pub source_lineage: u64,
    pub captured_unix_ms: u64,
    pub users: Vec<users::RecoveryAccount>,
    pub credentials: Vec<credentials::MigrationCredential>,
    #[serde(with = "binding_json")]
    pub bindings: BindingSet,
    pub orphans: Vec<Orphan>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Orphan {
    tenant: String,
    binding: String,
    generation: u64,
    retired_at_ms: Option<u64>,
    graph: String,
    evidence: Option<String>,
    receipt_window_ms: u64,
    grants_digest: String,
}

impl Orphan {
    fn of(record: &Recorded) -> Self {
        Self {
            tenant: record.tenant.clone(),
            binding: record.binding.clone(),
            generation: record.generation,
            retired_at_ms: match record.state {
                BindingState::Active => None,
                BindingState::Retired { at_ms } => Some(at_ms),
            },
            graph: record.graph.clone(),
            evidence: record.evidence.clone(),
            receipt_window_ms: record.receipt_window_ms,
            grants_digest: record.grants_digest.clone(),
        }
    }

    fn recorded(&self) -> Result<Recorded, MigrationError> {
        let record =
            Recorded {
                tenant: self.tenant.clone(),
                binding: self.binding.clone(),
                generation: self.generation,
                state: self.retired_at_ms.map_or(BindingState::Active, |at_ms| {
                    BindingState::Retired { at_ms }
                }),
                graph: self.graph.clone(),
                evidence: self.evidence.clone(),
                receipt_window_ms: self.receipt_window_ms,
                grants_digest: self.grants_digest.clone(),
            };
        let node = crate::property_graph::NodeView {
            id: 1,
            key: binding::RECORD_KEY.into(),
            labels: BTreeSet::from([binding::RECORD_LABEL.into()]),
            properties: record.properties(),
            revision: 1,
            expires_at_ms: None,
        };
        Recorded::decode(&node, &record.stored_graph()).map_err(|_| MigrationError::CorruptOrphan)
    }
}

mod binding_json {
    use super::*;

    pub(super) fn serialize<S: serde::Serializer>(
        set: &BindingSet,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        binding::to_json(set).serialize(serializer)
    }

    pub(super) fn deserialize<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> Result<BindingSet, D::Error> {
        let value = serde_json::Value::deserialize(deserializer)?;
        let bytes = serde_json::to_vec(&value).map_err(serde::de::Error::custom)?;
        binding::parse(&bytes).map_err(|_| serde::de::Error::custom("invalid migration bindings"))
    }
}

#[cfg_attr(not(test), allow(dead_code))]
pub(super) fn select(
    tenant_id: &str,
    input: Selection<'_>,
    now: u64,
) -> Result<TenantState, MigrationError> {
    if input.sidecars.iter().any(|entry| {
        !recovery_bundle::migration_sidecar_known(&entry.name, entry.directory, entry.symlink)
    }) {
        return Err(MigrationError::UnknownSidecar);
    }
    let tenant = input
        .tenants
        .iter()
        .find(|tenant| tenant.id == tenant_id)
        .ok_or(MigrationError::UnknownTenant)?;
    if !tenants::routable(std::slice::from_ref(tenant), tenant_id) {
        return Err(MigrationError::DeletedTenant);
    }
    let prefix = format!("t{tenant_id}__");
    if input
        .imports
        .iter()
        .any(|job| job.open && job.stored_collection.starts_with(&prefix))
    {
        return Err(MigrationError::OpenImport);
    }
    let state = TenantState {
        schema: SCHEMA,
        tenant: tenant.clone(),
        source_lineage: input.source_lineage,
        captured_unix_ms: now,
        users: input
            .accounts
            .map(|accounts| accounts.migration_inventory(tenant_id))
            .transpose()
            .map_err(|_| MigrationError::Unavailable)?
            .unwrap_or_default(),
        credentials: input
            .credentials
            .migration_inventory(tenant_id, input.source_lineage, now)
            .map_err(MigrationError::Invalid)?,
        bindings: BindingSet {
            bindings: input
                .bindings
                .bindings
                .iter()
                .filter(|binding| binding.tenant == tenant_id)
                .cloned()
                .collect(),
        },
        orphans: input
            .orphans
            .iter()
            .filter(|record| record.tenant == tenant_id)
            .map(Orphan::of)
            .collect(),
    };
    state.validate()?;
    Ok(state)
}

#[cfg_attr(not(test), allow(dead_code))]
impl TenantState {
    /// Validate again after deserialization, before any import or issuance.
    pub(super) fn validate(&self) -> Result<(), MigrationError> {
        let id = &self.tenant.id;
        if self.schema != SCHEMA
            || self.source_lineage == 0
            || !tenants::valid_store(std::slice::from_ref(&self.tenant))
        {
            return Err(MigrationError::Invalid(
                "invalid migration schema, lineage or tenant",
            ));
        }
        if !tenants::routable(std::slice::from_ref(&self.tenant), id) {
            return Err(MigrationError::DeletedTenant);
        }
        let mut names = BTreeSet::new();
        if self.users.iter().any(|user| {
            user.tenant_id.as_ref() != Some(id)
                || user.check().is_err()
                || !names.insert(&user.username)
        }) {
            return Err(MigrationError::Invalid(
                "invalid or cross-tenant migration account",
            ));
        }
        let mut principals = BTreeSet::new();
        if self.credentials.len() > credentials::CAPACITY
            || self.credentials.iter().any(|credential| {
                credential.check(id).is_err() || !principals.insert(&credential.inventory.id)
            })
        {
            return Err(MigrationError::Invalid(
                "invalid or duplicate migration credential",
            ));
        }
        if self.bindings.bindings.iter().any(|binding| {
            binding.tenant != *id
                || binding.grants.iter().any(|grant| {
                    grant
                        .scopes
                        .iter()
                        .chain(&grant.read_scopes)
                        .any(|scope| scope.tenant != *id)
                })
        }) {
            return Err(MigrationError::CrossTenantBinding);
        }
        self.bindings
            .validate()
            .map_err(|_| MigrationError::Invalid("invalid migration bindings"))?;
        if self
            .bindings
            .bindings
            .iter()
            .flat_map(|binding| &binding.grants)
            .any(|grant| !principals.contains(&grant.principal))
        {
            return Err(MigrationError::Invalid(
                "a binding names an unselected principal",
            ));
        }
        let mut records: Vec<_> = self.bindings.bindings.iter().map(Recorded::of).collect();
        for orphan in &self.orphans {
            if orphan.tenant != *id {
                return Err(MigrationError::CorruptOrphan);
            }
            records.push(orphan.recorded()?);
        }
        binding::validate_all(&records).map_err(|_| MigrationError::CorruptOrphan)?;
        Ok(())
    }

    pub(super) fn reservations(&self) -> Result<Bindings, MigrationError> {
        self.validate()?;
        let orphans = self
            .orphans
            .iter()
            .map(Orphan::recorded)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Bindings::new(self.bindings.clone()).with_orphans(&orphans))
    }

    /// A target check is required to obtain the input accepted by rekey.
    pub(super) fn for_target(
        &self,
        collections: &[(String, u16)],
    ) -> Result<TargetState<'_>, MigrationError> {
        self.validate()?;
        let mut names = BTreeSet::new();
        let mut ids = BTreeSet::new();
        if collections
            .iter()
            .any(|(name, id)| !names.insert(name) || !ids.insert(id))
        {
            return Err(MigrationError::Invalid(
                "duplicate target collection identity",
            ));
        }
        for credential in self
            .credentials
            .iter()
            .filter(|credential| credential.active(self.captured_unix_ms))
        {
            for scope in &credential.source_collections {
                if !collections
                    .iter()
                    .any(|(name, id)| name == &scope.stored_name && *id == scope.id)
                {
                    return Err(MigrationError::StaleGrant {
                        principal_id: credential.inventory.id.clone(),
                        stored_name: scope.stored_name.clone(),
                    });
                }
            }
        }
        Ok(TargetState { state: self })
    }

    /// Typed data is converted only after selection; field names and scalar
    /// bytes are length framed, object keys sorted, array order preserved.
    pub(super) fn canonical_digest(&self) -> Result<String, MigrationError> {
        self.validate()?;
        let value = serde_json::to_value(self)
            .map_err(|_| MigrationError::Invalid("cannot encode migration state"))?;
        let mut digest = Sha256::new();
        frame(&mut digest, b"vlite.tenant-migration.state.v1");
        canonical(&mut digest, &value);
        Ok(super::hex(&digest.finalize()))
    }
}

fn frame(digest: &mut Sha256, bytes: &[u8]) {
    digest.update((bytes.len() as u64).to_be_bytes());
    digest.update(bytes);
}

fn canonical(digest: &mut Sha256, value: &serde_json::Value) {
    use serde_json::Value;
    match value {
        Value::Null => frame(digest, b"null"),
        Value::Bool(value) => {
            frame(digest, b"bool");
            frame(digest, if *value { b"true" } else { b"false" });
        }
        Value::Number(value) => {
            frame(digest, b"number");
            frame(digest, value.to_string().as_bytes());
        }
        Value::String(value) => {
            frame(digest, b"string");
            frame(digest, value.as_bytes());
        }
        Value::Array(values) => {
            frame(digest, b"array");
            frame(digest, &(values.len() as u64).to_be_bytes());
            for value in values {
                canonical(digest, value);
            }
        }
        Value::Object(values) => {
            frame(digest, b"object");
            frame(digest, &(values.len() as u64).to_be_bytes());
            let sorted: std::collections::BTreeMap<_, _> = values.iter().collect();
            for (key, value) in sorted {
                frame(digest, key.as_bytes());
                canonical(digest, value);
            }
        }
    }
}

#[cfg_attr(not(test), allow(dead_code))]
pub(super) struct TargetState<'a> {
    state: &'a TenantState,
}

#[cfg_attr(not(test), allow(dead_code))]
pub(super) struct CredentialViews {
    pub source: credentials::Snapshot,
    pub target: credentials::Snapshot,
}

pub(super) struct MigrationTokens(Vec<String>);

impl std::fmt::Debug for MigrationTokens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MigrationTokens([REDACTED])")
    }
}

#[cfg_attr(not(test), allow(dead_code))]
impl MigrationTokens {
    /// Consume the one-time output for an operator-private token file.
    pub(super) fn into_tokens(self) -> Vec<String> {
        self.0
    }
}

#[cfg_attr(not(test), allow(dead_code))]
pub(super) fn rekey_for_migration(
    ready: &TargetState<'_>,
    target_lineage: u64,
    now: u64,
) -> Result<(CredentialViews, MigrationTokens), MigrationError> {
    let state = ready.state;
    if now < state.captured_unix_ms {
        return Err(MigrationError::Invalid(
            "migration issuance predates inventory",
        ));
    }
    if target_lineage == 0 || target_lineage == state.source_lineage {
        return Err(MigrationError::Invalid(
            "migration requires distinct nonzero lineages",
        ));
    }
    let (source, target, tokens) = credentials::migration_rekey(
        &state.tenant.id,
        &state.credentials,
        state.source_lineage,
        target_lineage,
        now,
    )
    .map_err(|_| MigrationError::Unavailable)?;
    Ok((CredentialViews { source, target }, MigrationTokens(tokens)))
}

/// Isolated user import retains the existing fresh-admin/collision/password
/// rules verbatim; source account IDs, hashes and sessions never enter it.
#[cfg_attr(not(test), allow(dead_code))]
pub(super) fn import_users(
    state: &TenantState,
    directory: &Path,
    fresh_admin: (&str, &str),
    created: &mut Vec<PathBuf>,
) -> std::io::Result<usize> {
    state.validate().map_err(std::io::Error::other)?;
    users::rekey_import(directory, &state.users, fresh_admin, created)
}

pub(super) fn conflict() -> ApiError {
    ApiError::new(
        StatusCode::CONFLICT,
        "tenant_migration_conflict",
        "migration generation, phase or identity does not match; nothing was published",
    )
}

fn transition(
    objects: &Objects,
    now: u64,
    change: impl FnMut(&Snapshot) -> Result<Snapshot, ApiError>,
) -> Result<Snapshot, ApiError> {
    objects
        .update(now, change)
        .map_err(|error| match error.code {
            "control_state_indeterminate" => ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "tenant_migration_indeterminate",
                "migration write outcome is unknown; reconcile before retrying",
            ),
            "control_state_conflict" => conflict(),
            _ => error,
        })
}

/// All phase changes carry the expected generation and the immutable
/// tenant/source/target identity captured by preparation. A lost CAS race
/// rechecks that generation against the winner and cannot replay a change.
#[cfg_attr(not(test), allow(dead_code))]
pub(super) fn prepare(
    objects: &Objects,
    migration_id: &str,
    expected_generation: u64,
    migration: &Migration,
    now: u64,
) -> Result<Snapshot, ApiError> {
    transition(objects, now, |current| {
        current.prepare(expected_generation, migration_id, migration, now)
    })
}

#[cfg_attr(not(test), allow(dead_code))]
pub(super) fn freeze(
    objects: &Objects,
    migration_id: &str,
    expected_generation: u64,
    identity: &Migration,
    now: u64,
) -> Result<Snapshot, ApiError> {
    transition(objects, now, |current| {
        current.freeze(expected_generation, migration_id, identity, now)
    })
}

#[cfg_attr(not(test), allow(dead_code))]
pub(super) fn target_verified(
    objects: &Objects,
    migration_id: &str,
    expected_generation: u64,
    identity: &Migration,
    now: u64,
) -> Result<Snapshot, ApiError> {
    transition(objects, now, |current| {
        current.target_verified(expected_generation, migration_id, identity, now)
    })
}

#[cfg_attr(not(test), allow(dead_code))]
pub(super) fn cutover(
    objects: &Objects,
    migration_id: &str,
    expected_generation: u64,
    identity: &Migration,
    views: &CredentialViews,
    now: u64,
) -> Result<Snapshot, ApiError> {
    transition(objects, now, |current| {
        current.cutover(expected_generation, migration_id, identity, views, now)
    })
}

#[cfg_attr(not(test), allow(dead_code))]
pub(super) fn rollback(
    objects: &Objects,
    migration_id: &str,
    expected_generation: u64,
    identity: &Migration,
    views: &CredentialViews,
    target_acked_mutations: bool,
    now: u64,
) -> Result<Snapshot, ApiError> {
    transition(objects, now, |current| {
        current.rollback(
            expected_generation,
            migration_id,
            identity,
            views,
            target_acked_mutations,
            now,
        )
    })
}

#[cfg_attr(not(test), allow(dead_code))]
pub(super) fn release(
    objects: &Objects,
    migration_id: &str,
    expected_generation: u64,
    identity: &Migration,
    ack_irreversible: bool,
    now: u64,
) -> Result<Snapshot, ApiError> {
    transition(objects, now, |current| {
        current.release(
            expected_generation,
            migration_id,
            identity,
            ack_irreversible,
            now,
        )
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))]
pub(super) enum Reconciled {
    Applied,
    NotApplied,
}

/// Readback never writes or retries a transition. Only the exact successor
/// generation with the intended phase proves an ambiguous CAS applied.
#[cfg_attr(not(test), allow(dead_code))]
pub(super) fn reconcile(
    objects: &Objects,
    migration_id: &str,
    expected_generation: u64,
    intended_phase: Phase,
) -> Result<Reconciled, ApiError> {
    let (current, _) = objects.read()?;
    let phase = current
        .migration(migration_id)
        .map(|migration| migration.phase);
    if expected_generation.checked_add(1) == Some(current.generation())
        && phase == Some(intended_phase)
    {
        return Ok(Reconciled::Applied);
    }
    if current.generation() == expected_generation
        && (phase.is_some_and(|phase| intended_phase.can_follow(phase))
            || (intended_phase == Phase::Prepared && phase.is_none()))
    {
        return Ok(Reconciled::NotApplied);
    }
    Err(conflict())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::MemoryScope;
    use crate::memory::binding::{Binding, BindingState, Grant};
    use serde_json::{Value, json};

    const T: &str = "0123456789abcdef0123456789abcdef";
    const OTHER: &str = "0123456789abcdef0123456789abcdee";
    const ID: &str = "abcdefghijkl";
    const NOW: u64 = 100;
    const PASSWORD: &str = "Quartz!7-mango-River-29";

    use super::super::cloud_backup::fake::{Fault, Memory, Op};
    use super::super::control_state::{self as control, AuthorityMode, Migration, Phase, Route};
    use std::sync::Arc;

    const CONTROL: &str = "control/state.json";

    struct World {
        memory: Arc<Memory>,
        objects: control::Objects,
        migration: Migration,
        views: CredentialViews,
        old_tokens: Vec<String>,
        fresh_tokens: Vec<String>,
        credentials: credentials::Store,
        tenants: tenants::Store,
    }

    impl World {
        fn new() -> Self {
            let (credentials, old_tokens) =
                credentials::tests::issued(&[(ID, Some(T)), ("bbbbbbbbbbbb", Some(OTHER))], 0x77);
            let tenants = tenants::Store::for_test(vec![
                tenants::Tenant::for_test(T, None),
                tenants::Tenant::for_test(OTHER, None),
            ]);
            let source = Route {
                lineage: 0x77,
                ..Route::shared_for_test(T)
            };
            let target = Route {
                kind: control::Kind::Dedicated,
                writer: "https://target.internal".into(),
                audience: "target".into(),
                lineage: 0x88,
                authority_mode: Some(AuthorityMode::Off),
                ..source.clone()
            };
            let migration = Migration {
                id: "move-one".into(),
                tenant: T.into(),
                phase: Phase::Prepared,
                source: source.clone(),
                target,
                source_lineage: 0x77,
                target_lineage: 0x88,
                updated_unix_ms: NOW,
            };
            let snapshot = credentials.snapshot().unwrap();
            let inventory = snapshot.migration_inventory(T, 0x77, NOW).unwrap();
            let (source_view, target_view, fresh_tokens) =
                credentials::migration_rekey(T, &inventory, 0x77, 0x88, NOW).unwrap();
            let memory = Arc::new(Memory::default());
            let objects = control::Objects::new(memory.clone(), CONTROL);
            objects
                .create(
                    control::Snapshot::import(&credentials, Some(&tenants))
                        .unwrap()
                        .with_directory(vec![source, Route::shared_for_test(OTHER)]),
                    NOW,
                )
                .unwrap();
            Self {
                memory,
                objects,
                migration,
                views: CredentialViews {
                    source: source_view,
                    target: target_view,
                },
                old_tokens,
                fresh_tokens,
                credentials,
                tenants,
            }
        }

        fn prepare(&self) -> control::Snapshot {
            prepare(&self.objects, "move-one", 1, &self.migration, NOW).unwrap()
        }

        fn verified(&self) {
            self.prepare();
            freeze(&self.objects, "move-one", 2, &self.migration, NOW).unwrap();
            target_verified(&self.objects, "move-one", 3, &self.migration, NOW).unwrap();
        }

        fn cutover(&self, generation: u64) -> Result<control::Snapshot, super::super::ApiError> {
            cutover(
                &self.objects,
                "move-one",
                generation,
                &self.migration,
                &self.views,
                NOW,
            )
        }

        fn validating(&self) {
            self.verified();
            self.cutover(4).unwrap();
        }

        fn rollback(
            &self,
            generation: u64,
            writes: bool,
        ) -> Result<control::Snapshot, super::super::ApiError> {
            rollback(
                &self.objects,
                "move-one",
                generation,
                &self.migration,
                &self.views,
                writes,
                NOW,
            )
        }

        fn release(
            &self,
            generation: u64,
            ack: bool,
        ) -> Result<control::Snapshot, super::super::ApiError> {
            release(
                &self.objects,
                "move-one",
                generation,
                &self.migration,
                ack,
                NOW,
            )
        }

        fn raw(&self) -> Value {
            serde_json::from_slice(&self.memory.raw(CONTROL).unwrap()).unwrap()
        }
    }

    #[test]
    fn migration_happy_path_advances_generation_once_per_phase() {
        let w = World::new();
        assert_eq!(w.prepare().generation(), 2);
        assert_eq!(
            freeze(&w.objects, "move-one", 2, &w.migration, NOW)
                .unwrap()
                .generation(),
            3
        );
        assert_eq!(
            target_verified(&w.objects, "move-one", 3, &w.migration, NOW)
                .unwrap()
                .generation(),
            4
        );
        assert_eq!(w.cutover(4).unwrap().generation(), 5);
        assert_eq!(w.release(5, true).unwrap().generation(), 6);
        assert_eq!(w.raw()["migrations"][0]["phase"], "released");
    }

    #[test]
    fn cutover_switches_route_and_credentials_atomically() {
        let w = World::new();
        w.verified();
        let before = w.raw();
        let after = w.cutover(4).unwrap();
        assert_eq!(after.record_for(T), Some(&w.migration.target));
        assert!(after.authenticate(&w.old_tokens[0], NOW).is_err());
        assert_eq!(
            after.authenticate(&w.fresh_tokens[0], NOW).unwrap().lineage,
            0x88
        );
        assert_eq!(before["directory"][1], w.raw()["directory"][1]);
        let other = |raw: Value| {
            raw["credentials"]
                .as_array()
                .unwrap()
                .iter()
                .find(|entry| entry["tenant_id"] == OTHER)
                .unwrap()
                .clone()
        };
        assert_eq!(
            serde_json::to_vec(&other(before)).unwrap(),
            serde_json::to_vec(&other(w.raw())).unwrap()
        );
        assert!(after.authenticate(&w.old_tokens[1], NOW).is_ok());
    }

    #[test]
    fn cutover_preserves_revocation_expiry_and_suspension() {
        let w = World::new();
        w.verified();
        w.objects
            .update(NOW, |current| {
                let mut raw = serde_json::to_value(current).unwrap();
                raw["credentials"][0]["expires_unix_ms"] = json!(NOW + 1);
                serde_json::from_value::<control::Snapshot>(raw)
                    .unwrap()
                    .with_revoked(ID, NOW)?
                    .with_suspended(T, true, NOW)
            })
            .unwrap();
        let after = w.cutover(5).unwrap();
        assert_eq!(after.admit(T).err().unwrap().code, "tenant_suspended");
        let raw = w.raw();
        let entry = raw["credentials"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["id"] == ID)
            .unwrap();
        assert_eq!(entry["expires_unix_ms"], NOW + 1);
        assert_eq!(entry["revoked_unix_ms"], NOW);
        assert!(after.authenticate(&w.fresh_tokens[0], NOW).is_err());
        let refreshed = w
            .objects
            .update(NOW, |s| s.reimport(&w.credentials, Some(&w.tenants)))
            .unwrap();
        assert!(refreshed.authenticate(&w.old_tokens[0], NOW).is_err());
        assert_eq!(refreshed.record_for(T), Some(&w.migration.target));
        assert_eq!(w.raw()["migrations"][0]["phase"], "validating");
        w.rollback(7, false).unwrap();
        let raw = w.raw();
        let entry = raw["credentials"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["id"] == ID)
            .unwrap();
        assert_eq!(entry["expires_unix_ms"], NOW + 1);
        assert_eq!(entry["revoked_unix_ms"], NOW);
    }

    #[test]
    fn rollback_restores_shared_route_with_the_same_fresh_credentials() {
        let w = World::new();
        w.validating();
        let before = w.raw()["credentials"].clone();
        let after = w.rollback(5, false).unwrap();
        assert_eq!(after.record_for(T), Some(&w.migration.source));
        assert_eq!(
            after.authenticate(&w.fresh_tokens[0], NOW).unwrap().lineage,
            0x77
        );
        assert!(after.authenticate(&w.old_tokens[0], NOW).is_err());
        let mut restored = w.raw()["credentials"].clone();
        for entry in restored.as_array_mut().unwrap() {
            if entry["tenant_id"] == T {
                entry["lineage"] = json!("0000000000000088");
            }
        }
        assert_eq!(before, restored);
    }

    #[test]
    fn rollback_with_target_writes_requires_reconciliation() {
        for phase in [Phase::Frozen, Phase::TargetVerified, Phase::Validating] {
            let w = World::new();
            w.prepare();
            freeze(&w.objects, "move-one", 2, &w.migration, NOW).unwrap();
            let mut generation = 3;
            if phase != Phase::Frozen {
                target_verified(&w.objects, "move-one", 3, &w.migration, NOW).unwrap();
                generation = 4;
            }
            if phase == Phase::Validating {
                w.cutover(4).unwrap();
                generation = 5;
            }
            let before = w.memory.raw(CONTROL).unwrap();
            let error = w.rollback(generation, true).err().unwrap();
            assert_eq!(error.code, "rollback_requires_reconciliation");
            assert_eq!(error.status, axum::http::StatusCode::CONFLICT);
            assert_eq!(before, w.memory.raw(CONTROL).unwrap());
            w.rollback(generation, false).unwrap();
            if phase != Phase::Validating {
                let after = w.objects.read().unwrap().0;
                assert!(after.authenticate(&w.old_tokens[0], NOW).is_ok());
                assert_eq!(after.record_for(T), Some(&w.migration.source));
            }
        }
    }

    #[test]
    fn rollback_after_release_is_migration_source_released() {
        let w = World::new();
        w.validating();
        w.release(5, true).unwrap();
        assert_eq!(
            w.rollback(6, false).err().unwrap().code,
            "migration_source_released"
        );
    }

    #[test]
    fn release_requires_validating_and_ack() {
        let w = World::new();
        w.prepare();
        assert_eq!(
            w.release(2, true).err().unwrap().code,
            "tenant_migration_conflict"
        );
        let w = World::new();
        w.validating();
        let error = w.release(5, false).err().unwrap();
        assert_eq!(error.code, "tenant_migration_ack_required");
        assert_eq!(error.status, axum::http::StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(w.objects.read().unwrap().0.generation(), 5);
    }

    #[test]
    fn release_and_rollback_race_has_one_winner() {
        let w = World::new();
        w.validating();
        let barrier = std::sync::Barrier::new(2);
        let results = std::thread::scope(|scope| {
            let release = scope.spawn(|| {
                barrier.wait();
                w.release(5, true)
            });
            let rollback = scope.spawn(|| {
                barrier.wait();
                w.rollback(5, false)
            });
            [release.join().unwrap(), rollback.join().unwrap()]
        });
        assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
        assert_eq!(
            results.into_iter().find_map(Result::err).unwrap().code,
            "tenant_migration_conflict"
        );
        assert_eq!(w.objects.read().unwrap().0.generation(), 6);
    }

    #[test]
    fn stale_expected_generation_is_conflict_without_write() {
        let w = World::new();
        w.verified();
        let before = w.memory.raw(CONTROL).unwrap();
        w.memory.fault(Op::Put, CONTROL, Fault::After, 0, 1);
        assert_eq!(
            w.cutover(3).err().unwrap().code,
            "tenant_migration_conflict"
        );
        assert_eq!(before, w.memory.raw(CONTROL).unwrap());
        assert_eq!(
            w.cutover(4).err().unwrap().code,
            "tenant_migration_indeterminate"
        );
    }

    #[test]
    fn unknown_write_outcome_is_indeterminate_and_reconcile_reads_exactly() {
        for (fault, outcome) in [
            (Fault::Before, Reconciled::NotApplied),
            (Fault::After, Reconciled::Applied),
        ] {
            let w = World::new();
            w.verified();
            w.memory.fault(Op::Put, CONTROL, fault, 0, 1);
            let error = w.cutover(4).err().unwrap();
            assert_eq!(error.code, "tenant_migration_indeterminate");
            assert_eq!(error.status, axum::http::StatusCode::SERVICE_UNAVAILABLE);
            assert_eq!(
                reconcile(&w.objects, "move-one", 4, Phase::Validating).unwrap(),
                outcome
            );
            assert_eq!(
                w.objects.read().unwrap().0.generation(),
                if outcome == Reconciled::Applied { 5 } else { 4 }
            );
            assert_eq!(
                reconcile(&w.objects, "move-one", 3, Phase::Released)
                    .err()
                    .unwrap()
                    .code,
                "tenant_migration_conflict"
            );
            w.memory.fault(Op::Put, CONTROL, Fault::After, 0, 1);
            reconcile(&w.objects, "move-one", 4, Phase::Validating).unwrap();
            assert_eq!(
                w.objects.update(NOW, |s| Ok(s.clone())).err().unwrap().code,
                "control_state_indeterminate"
            );
        }
    }

    #[test]
    fn generation_never_decreases_across_rollback() {
        let w = World::new();
        w.validating();
        let after = w.rollback(5, false).unwrap();
        assert_eq!(after.generation(), 6);
        assert_eq!(w.raw()["migrations"][0]["phase"], "rolled_back");
        let refreshed = w
            .objects
            .update(NOW, |s| s.reimport(&w.credentials, Some(&w.tenants)))
            .unwrap();
        assert_eq!(refreshed.generation(), 7);
        assert!(refreshed.authenticate(&w.old_tokens[0], NOW).is_err());
        assert_eq!(
            refreshed
                .authenticate(&w.fresh_tokens[0], NOW)
                .unwrap()
                .lineage,
            0x77
        );
    }

    #[test]
    fn rollback_refuses_different_rekeyed_credentials() {
        let mut w = World::new();
        w.validating();
        let before = w.memory.raw(CONTROL).unwrap();
        for view in [&mut w.views.source, &mut w.views.target] {
            let mut raw = serde_json::to_value(&*view).unwrap();
            raw[0]["secret_sha256"] = json!("f".repeat(64));
            *view = serde_json::from_value(raw).unwrap();
        }
        assert_eq!(
            w.rollback(5, false).err().unwrap().code,
            "tenant_migration_conflict"
        );
        assert_eq!(before, w.memory.raw(CONTROL).unwrap());
    }

    #[test]
    fn reimport_accepts_new_restrictions_without_restoring_old_hashes() {
        let w = World::new();
        w.validating();
        credentials::tests::revoke(&w.credentials, ID, NOW);
        let after = w
            .objects
            .update(NOW, |s| s.reimport(&w.credentials, Some(&w.tenants)))
            .unwrap();
        assert!(after.authenticate(&w.fresh_tokens[0], NOW).is_err());
        assert!(after.authenticate(&w.old_tokens[0], NOW).is_err());
        let raw = w.raw();
        let entry = raw["credentials"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["id"] == ID)
            .unwrap();
        assert_eq!(entry["lineage"], "0000000000000088");
        assert_eq!(entry["revoked_unix_ms"], NOW);
    }

    #[test]
    fn migration_lost_cas_race_refuses_to_reapply_to_the_winner() {
        let w = World::new();
        w.verified();
        let memory = w.memory.clone();
        w.memory.hook(Op::Put, CONTROL, move || {
            control::Objects::new(memory, CONTROL)
                .update(NOW, |s| s.clone().with_revoked(ID, NOW))
                .unwrap();
        });
        assert_eq!(
            w.cutover(4).err().unwrap().code,
            "tenant_migration_conflict"
        );
        assert_eq!(w.raw()["migrations"][0]["phase"], "target_verified");
        assert_eq!(w.objects.read().unwrap().0.generation(), 5);
        assert!(
            w.objects
                .read()
                .unwrap()
                .0
                .authenticate(&w.old_tokens[0], NOW)
                .is_err()
        );
        let next = w.cutover(5).unwrap();
        assert_eq!(next.generation(), 6);
        assert!(next.authenticate(&w.fresh_tokens[0], NOW).is_err());
    }

    #[test]
    fn wrong_phase_missing_migration_and_changed_identity_are_conflicts_without_writes() {
        let w = World::new();
        w.verified();
        let before = w.memory.raw(CONTROL).unwrap();
        let mut identities = vec![w.migration.clone(); 5];
        identities[0].id = "missing".into();
        identities[1].tenant = OTHER.into();
        identities[2].source.binding_generation += 1;
        identities[3].target.audience = "different".into();
        identities[4].target_lineage += 1;
        for identity in identities {
            assert_eq!(
                cutover(&w.objects, "move-one", 4, &identity, &w.views, NOW)
                    .err()
                    .unwrap()
                    .code,
                "tenant_migration_conflict",
            );
        }
        assert_eq!(
            cutover(&w.objects, "missing", 4, &w.migration, &w.views, NOW)
                .err()
                .unwrap()
                .code,
            "tenant_migration_conflict",
        );
        assert_eq!(
            freeze(&w.objects, "move-one", 4, &w.migration, NOW)
                .err()
                .unwrap()
                .code,
            "tenant_migration_conflict"
        );
        assert_eq!(before, w.memory.raw(CONTROL).unwrap());
    }

    #[test]
    fn invalid_cross_tenant_or_wrong_lineage_views_cannot_cutover() {
        for (field, value) in [
            ("tenant_id", json!(OTHER)),
            ("lineage", json!("0000000000000077")),
            ("operations", json!(["read", "write"])),
        ] {
            let mut w = World::new();
            w.verified();
            let before = w.memory.raw(CONTROL).unwrap();
            let mut raw = serde_json::to_value(&w.views.target).unwrap();
            raw[0][field] = value;
            w.views.target = serde_json::from_value(raw).unwrap();
            assert_eq!(
                w.cutover(4).err().unwrap().code,
                "tenant_migration_conflict"
            );
            assert_eq!(before, w.memory.raw(CONTROL).unwrap());
        }
    }

    #[test]
    fn prepare_refuses_overlaps_and_source_route_changes() {
        let w = World::new();
        w.prepare();
        let before = w.memory.raw(CONTROL).unwrap();
        let mut second = w.migration.clone();
        second.id = "move-two".into();
        assert_eq!(
            prepare(&w.objects, "move-two", 2, &second, NOW)
                .err()
                .unwrap()
                .code,
            "tenant_migration_conflict"
        );
        assert_eq!(before, w.memory.raw(CONTROL).unwrap());
        let w = World::new();
        let mut changed = w.migration.clone();
        changed.source.binding_generation += 1;
        assert_eq!(
            prepare(&w.objects, "move-one", 1, &changed, NOW)
                .err()
                .unwrap()
                .code,
            "tenant_migration_conflict"
        );
        assert_eq!(w.objects.read().unwrap().0.generation(), 1);
    }

    #[test]
    fn reconcile_requires_exact_phase_and_generation_and_handles_prepare() {
        for (fault, outcome) in [
            (Fault::Before, Reconciled::NotApplied),
            (Fault::After, Reconciled::Applied),
        ] {
            let w = World::new();
            w.memory.fault(Op::Put, CONTROL, fault, 0, 1);
            assert_eq!(
                prepare(&w.objects, "move-one", 1, &w.migration, NOW)
                    .err()
                    .unwrap()
                    .code,
                "tenant_migration_indeterminate"
            );
            assert_eq!(
                reconcile(&w.objects, "move-one", 1, Phase::Prepared).unwrap(),
                outcome
            );
        }
        let w = World::new();
        w.validating();
        assert_eq!(
            reconcile(&w.objects, "move-one", 4, Phase::Released)
                .err()
                .unwrap()
                .code,
            "tenant_migration_conflict"
        );
        assert_eq!(
            reconcile(&w.objects, "move-one", 5, Phase::Validating)
                .err()
                .unwrap()
                .code,
            "tenant_migration_conflict"
        );
        w.objects.update(NOW, |s| Ok(s.clone())).unwrap();
        assert_eq!(
            reconcile(&w.objects, "move-one", 4, Phase::Validating)
                .err()
                .unwrap()
                .code,
            "tenant_migration_conflict"
        );
    }

    fn binding(tenant: &str) -> Binding {
        Binding {
            binding: "memory".into(),
            tenant: tenant.into(),
            generation: 7,
            state: BindingState::Active,
            graph: "mem".into(),
            evidence_collection: Some("evidence".into()),
            receipt_window_ms: 1000,
            grants: vec![Grant {
                principal: ID.into(),
                scopes: vec![MemoryScope {
                    tenant: tenant.into(),
                    agent: "agent".into(),
                    user: None,
                    session: None,
                }],
                read_scopes: vec![],
                raw_access: true,
            }],
        }
    }

    fn account(tenant: &str, name: &str) -> users::RecoveryAccount {
        users::RecoveryAccount {
            username: name.into(),
            role: users::AccountRole::Editor,
            tenant_id: Some(tenant.into()),
            disabled: false,
            graphs: Some(vec!["mem".into()]),
            graph_access: Some(users::GraphAccess::Read),
        }
    }

    struct Fixture {
        directory: tempfile::TempDir,
        accounts: users::Accounts,
        credentials: credentials::Snapshot,
        tenants: Vec<tenants::Tenant>,
        bindings: BindingSet,
        orphans: Vec<Recorded>,
        sidecars: Vec<SidecarEntry>,
        imports: Vec<ImportJob>,
    }

    impl Fixture {
        fn new() -> Self {
            let directory = tempfile::tempdir().unwrap();
            users::rekey_import(
                directory.path(),
                &[account(T, "alice"), account(OTHER, "bob")],
                ("root", PASSWORD),
                &mut vec![],
            )
            .unwrap();
            let users_path = directory.path().join("users.json");
            let mut raw_users: Value =
                serde_json::from_slice(&std::fs::read(&users_path).unwrap()).unwrap();
            raw_users["users"][0]["disabled"] = json!(false);
            std::fs::write(&users_path, serde_json::to_vec(&raw_users).unwrap()).unwrap();
            let accounts = users::Accounts::load(
                directory.path(),
                &users::Settings {
                    idle: users::DEFAULT_IDLE,
                    lifetime: users::DEFAULT_LIFETIME,
                    https: false,
                    loopback_only: true,
                    trusted_proxy: false,
                },
            )
            .unwrap();
            let (store, _) = credentials::tests::issued(
                &[
                    (ID, Some(T)),
                    ("bbbbbbbbbbbb", Some(OTHER)),
                    ("cccccccccccc", None),
                ],
                11,
            );
            let mut raw = serde_json::to_value(store.snapshot().unwrap()).unwrap();
            raw[0]["all"] = json!(false);
            raw[0]["collections"] = json!([{"name":format!("t{T}__docs"),"id":4}]);
            raw[0]["operations"] = json!(["read", "write", "graph_read"]);
            raw[0]["graphs"] = json!(["mem"]);
            raw[0]["requests_per_second"] = json!(25);
            raw[0]["max_vectors"] = json!(500);
            raw[0]["max_vectors_per_collection"] = json!(100);
            raw[0]["expires_unix_ms"] = json!(1000);
            let mut tenant = tenants::Tenant::for_test(T, Some(50));
            tenant.max_vectors = Some(900);
            Self {
                directory,
                accounts,
                credentials: serde_json::from_value(raw).unwrap(),
                tenants: vec![tenant, tenants::Tenant::for_test(OTHER, None)],
                bindings: BindingSet {
                    bindings: vec![binding(T), binding(OTHER)],
                },
                orphans: vec![],
                sidecars: vec![],
                imports: vec![],
            }
        }

        fn select(&self) -> Result<TenantState, MigrationError> {
            select(
                T,
                Selection {
                    tenants: &self.tenants,
                    accounts: Some(&self.accounts),
                    credentials: &self.credentials,
                    bindings: &self.bindings,
                    orphans: &self.orphans,
                    sidecars: &self.sidecars,
                    imports: &self.imports,
                    source_lineage: 11,
                },
                NOW,
            )
        }

        fn edit_credential(&mut self, field: &str, value: Value) {
            let mut raw = serde_json::to_value(&self.credentials).unwrap();
            raw[0][field] = value;
            self.credentials = serde_json::from_value(raw).unwrap();
        }
    }

    fn target() -> Vec<(String, u16)> {
        vec![(format!("t{T}__docs"), 4)]
    }

    #[test]
    fn filter_selects_only_tenant_users_credentials_bindings() {
        let mut f = Fixture::new();
        f.orphans.push(Recorded::of(&binding(OTHER)));
        let state = f.select().unwrap();
        assert_eq!(state.users.len(), 1);
        assert_eq!(state.users[0].username, "alice");
        assert!(!state.users[0].disabled);
        assert_eq!(state.tenant.max_vectors, Some(900));
        assert_eq!(state.credentials.len(), 1);
        assert_eq!(state.credentials[0].inventory.id, ID);
        assert_eq!(state.bindings.bindings, vec![binding(T)]);
        assert!(state.orphans.is_empty());
        assert_eq!(
            serde_json::to_value(&state.tenant).unwrap()["suspended_unix_ms"],
            50
        );
    }

    #[test]
    fn no_hash_password_session_or_secret_in_serialized_state() {
        let f = Fixture::new();
        let session = f.accounts.migration_test_session();
        let state = f.select().unwrap();
        let bytes = serde_json::to_string(&state).unwrap();
        assert!(!bytes.contains(&session));
        assert!(!bytes.contains(&super::super::hex(&Sha256::digest(session.as_bytes()))));
        let source: Value =
            serde_json::from_slice(&std::fs::read(f.directory.path().join("users.json")).unwrap())
                .unwrap();
        for user in source["users"].as_array().unwrap() {
            assert!(!bytes.contains(user["password"].as_str().unwrap()));
            assert!(!bytes.contains(user["id"].as_str().unwrap()));
        }
        for (i, credential) in serde_json::to_value(&f.credentials)
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .enumerate()
        {
            assert!(!bytes.contains(credential["secret_sha256"].as_str().unwrap()));
            let secret = format!("{i:0>43}");
            assert!(!bytes.contains(&secret));
            assert!(!bytes.contains(&format!(
                "vlk_{}_{secret}",
                credential["id"].as_str().unwrap()
            )));
        }
        let selected: Value = serde_json::from_str(&bytes).unwrap();
        for user in selected["users"].as_array().unwrap() {
            for forbidden in ["id", "password", "version", "session"] {
                assert!(user.get(forbidden).is_none());
            }
        }
        for forbidden in ["password", "secret_sha256", "rotated_from", "webhook"] {
            assert!(!bytes.contains(forbidden), "{forbidden}");
        }
        let decoded: TenantState = serde_json::from_str(&bytes).unwrap();
        assert_eq!(
            state.canonical_digest().unwrap(),
            decoded.canonical_digest().unwrap()
        );
    }

    #[test]
    fn rekey_keeps_principal_id_new_secret() {
        let f = Fixture::new();
        let state = f.select().unwrap();
        let ready = state.for_target(&target()).unwrap();
        let (views, tokens) = rekey_for_migration(&ready, 22, NOW).unwrap();
        let token = &tokens.into_tokens()[0];
        assert!(token.starts_with(&format!("vlk_{ID}_")));
        assert!(!token.ends_with(&format!("{:0>43}", 0)));
        assert!(views.target.authenticate(token, NOW).is_ok());
        assert!(f.credentials.authenticate(token, NOW).is_err());
        assert!(
            views
                .target
                .authenticate(&format!("vlk_{ID}_{:0>43}", 0), NOW)
                .is_err()
        );
    }

    #[test]
    fn rekey_preserves_ops_scopes_limits_rate_and_absolute_expiry() {
        let f = Fixture::new();
        let state = f.select().unwrap();
        let ready = state.for_target(&target()).unwrap();
        let (views, tokens) = rekey_for_migration(&ready, 22, NOW).unwrap();
        let token = &tokens.into_tokens()[0];
        let grant = views.target.authenticate(token, NOW).unwrap();
        assert_eq!(grant.id, ID);
        assert_eq!(grant.tenant_id.as_deref(), Some(T));
        assert_eq!(grant.expires, 1000);
        assert_eq!(grant.requests_per_second, Some(25));
        assert_eq!(grant.max_vectors, Some(500));
        assert_eq!(grant.max_vectors_per_collection, Some(100));
        assert_eq!(grant.graphs, vec!["mem"]);
        assert!(grant.allows(credentials::Op::Write));
        assert!(grant.allows(credentials::Op::GraphRead));
        assert!(!grant.allows(credentials::Op::Backup));
        assert_eq!(grant.collections, target());
        assert!(views.target.authenticate(token, 1000).is_err());
    }

    #[test]
    fn expired_and_revoked_never_mint() {
        for (field, value) in [
            ("expires_unix_ms", json!(NOW)),
            ("revoked_unix_ms", json!(1)),
        ] {
            let mut f = Fixture::new();
            f.edit_credential(field, value);
            let state = f.select().unwrap();
            let ready = state.for_target(&[]).unwrap();
            let (views, tokens) = rekey_for_migration(&ready, 22, NOW).unwrap();
            assert!(tokens.into_tokens().is_empty());
            assert_eq!(serde_json::to_value(views.target).unwrap(), json!([]));
        }
        let f = Fixture::new();
        let state = f.select().unwrap();
        let ready = state.for_target(&target()).unwrap();
        assert!(
            rekey_for_migration(&ready, 22, 1000)
                .unwrap()
                .1
                .into_tokens()
                .is_empty()
        );
    }

    #[test]
    fn two_views_share_hash_differ_in_lineage() {
        let f = Fixture::new();
        let state = f.select().unwrap();
        let ready = state.for_target(&target()).unwrap();
        let (views, tokens) = rekey_for_migration(&ready, 22, NOW).unwrap();
        assert_eq!(format!("{tokens:?}"), "MigrationTokens([REDACTED])");
        let mut source = serde_json::to_value(views.source).unwrap();
        let target = serde_json::to_value(views.target).unwrap();
        assert_eq!(source[0]["lineage"], "000000000000000b");
        assert_eq!(target[0]["lineage"], "0000000000000016");
        source[0]["lineage"] = target[0]["lineage"].clone();
        assert_eq!(source, target);
    }

    #[test]
    fn stale_explicit_grant_to_recreated_collection_refused() {
        let state = Fixture::new().select().unwrap();
        assert!(matches!(
            state.for_target(&[(format!("t{T}__docs"), 5)]),
            Err(MigrationError::StaleGrant { principal_id, stored_name })
                if principal_id == ID && stored_name == format!("t{T}__docs")
        ));
        assert!(matches!(
            state.for_target(&[]),
            Err(MigrationError::StaleGrant { .. })
        ));
    }

    #[test]
    fn wildcard_grant_stays_within_tenant() {
        let mut f = Fixture::new();
        f.edit_credential("all", json!(true));
        f.edit_credential("collections", json!([]));
        let state = f.select().unwrap();
        let ready = state.for_target(&[]).unwrap();
        let (views, tokens) = rekey_for_migration(&ready, 22, NOW).unwrap();
        let grant = views
            .target
            .authenticate(&tokens.into_tokens()[0], NOW)
            .unwrap();
        assert!(grant.covers(Some(22), 99, &format!("t{T}__new")));
        assert!(!grant.covers(Some(22), 99, &format!("t{OTHER}__new")));
        assert!(!grant.covers(Some(22), 0, "default"));
    }

    #[test]
    fn imported_users_disabled_with_fresh_ids() {
        let f = Fixture::new();
        let state = f.select().unwrap();
        let directory = tempfile::tempdir().unwrap();
        import_users(&state, directory.path(), ("fresh", PASSWORD), &mut vec![]).unwrap();
        let imported: Value =
            serde_json::from_slice(&std::fs::read(directory.path().join("users.json")).unwrap())
                .unwrap();
        let original: Value =
            serde_json::from_slice(&std::fs::read(f.directory.path().join("users.json")).unwrap())
                .unwrap();
        let alice = &imported["users"][0];
        assert_eq!(alice["disabled"], true);
        assert_eq!(alice["must_change_password"], true);
        assert_ne!(alice["id"], original["users"][0]["id"]);
        assert_ne!(alice["password"], original["users"][0]["password"]);
        assert_eq!(alice["graphs"], original["users"][0]["graphs"]);
        assert_eq!(alice["graph_access"], original["users"][0]["graph_access"]);
        assert_eq!(imported["users"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn fresh_admin_name_collision_refused() {
        let state = Fixture::new().select().unwrap();
        let directory = tempfile::tempdir().unwrap();
        assert!(import_users(&state, directory.path(), ("ALICE", PASSWORD), &mut vec![]).is_err());
        assert!(!directory.path().join("users.json").exists());
    }

    #[test]
    fn orphan_only_set_has_zero_b_and_nonempty_digest() {
        let mut f = Fixture::new();
        f.orphans = vec![Recorded::of(&binding(T))];
        f.bindings.bindings.clear();
        let state = f.select().unwrap();
        let reservations = state.reservations().unwrap();
        assert_eq!(reservations.binding_generation(), 0);
        assert!(!reservations.is_empty());
        assert_ne!(
            reservations.reservation_digest(),
            Bindings::default().reservation_digest()
        );
        assert_eq!(reservations.reservation_digest().len(), 64);
        assert!(!reservations.reserved().raw_allowed(
            Some(ID),
            super::super::reserved::Resource::Graph,
            &format!("t{T}__mem")
        ));
        assert_eq!(state.orphans.len(), 1);
    }

    #[test]
    fn retired_binding_preserved() {
        let mut f = Fixture::new();
        f.bindings.bindings[0].state = BindingState::Retired { at_ms: 17 };
        assert_eq!(
            f.select().unwrap().bindings.bindings[0],
            f.bindings.bindings[0]
        );
    }

    #[test]
    fn binding_generation_not_bumped() {
        let f = Fixture::new();
        let state = f.select().unwrap();
        assert_eq!(state.bindings.bindings[0].generation, 7);
        let expected = Bindings::new(BindingSet {
            bindings: vec![binding(T)],
        });
        assert_eq!(
            state.reservations().unwrap().binding_generation(),
            expected.binding_generation()
        );
    }

    #[test]
    fn unknown_sidecar_field_refused() {
        let mut f = Fixture::new();
        let mut raw = serde_json::to_value(f.select().unwrap()).unwrap();
        raw["surprise"] = json!(true);
        assert!(serde_json::from_value::<TenantState>(raw).is_err());
        f.sidecars.push(SidecarEntry {
            name: "unrecognized.json".into(),
            directory: false,
            symlink: false,
        });
        assert!(matches!(f.select(), Err(MigrationError::UnknownSidecar)));
        f.sidecars[0].name = "users.json".into();
        f.sidecars[0].symlink = true;
        assert!(matches!(f.select(), Err(MigrationError::UnknownSidecar)));
    }

    #[test]
    fn nested_unknown_fields_and_wrong_schema_refused() {
        let state = Fixture::new().select().unwrap();
        for path in [
            "/tenant",
            "/users/0",
            "/credentials/0",
            "/credentials/0/inventory",
            "/credentials/0/source_collections/0",
            "/bindings",
            "/bindings/bindings/0",
            "/bindings/bindings/0/grants/0",
            "/bindings/bindings/0/grants/0/scopes/0",
        ] {
            let mut raw = serde_json::to_value(&state).unwrap();
            raw.pointer_mut(path).unwrap()["unknown"] = json!(true);
            assert!(
                serde_json::from_value::<TenantState>(raw).is_err(),
                "{path}"
            );
        }
        let mut raw = serde_json::to_value(&state).unwrap();
        raw["schema"] = json!(2);
        assert!(
            serde_json::from_value::<TenantState>(raw)
                .unwrap()
                .validate()
                .is_err()
        );
        let mut f = Fixture::new();
        f.bindings.bindings.clear();
        f.orphans.push(Recorded::of(&binding(T)));
        let mut raw = serde_json::to_value(f.select().unwrap()).unwrap();
        raw["orphans"][0]["unknown"] = json!(true);
        assert!(serde_json::from_value::<TenantState>(raw).is_err());
    }

    #[test]
    fn digest_frames_fields_preserves_order_and_ignores_json_key_order() {
        let state = Fixture::new().select().unwrap();
        let expected = state.canonical_digest().unwrap();
        assert_eq!(expected.len(), 64);
        let bytes = serde_json::to_string_pretty(&state).unwrap();
        let decoded: TenantState = serde_json::from_str(&bytes).unwrap();
        assert_eq!(expected, decoded.canonical_digest().unwrap());
        let mut changed = state.clone();
        changed.tenant.max_vectors = Some(901);
        assert_ne!(expected, changed.canonical_digest().unwrap());
        let mut changed = state.clone();
        changed.credentials[0].inventory.operations.reverse();
        assert_ne!(expected, changed.canonical_digest().unwrap());
        let mut first = Sha256::new();
        frame(&mut first, b"ab");
        frame(&mut first, b"c");
        let mut second = Sha256::new();
        frame(&mut second, b"a");
        frame(&mut second, b"bc");
        assert_ne!(first.finalize(), second.finalize());
    }

    #[test]
    fn binding_read_scopes_order_and_scope_bytes_preserved() {
        let mut f = Fixture::new();
        let grant = &mut f.bindings.bindings[0].grants[0];
        grant.raw_access = false;
        grant.read_scopes = vec![MemoryScope {
            tenant: T.into(),
            agent: "read-agent".into(),
            user: Some("user".into()),
            session: Some("memory-scope-not-a-login-session".into()),
        }];
        let state = f.select().unwrap();
        let bytes = serde_json::to_vec(&state).unwrap();
        let decoded: TenantState = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(state.bindings, decoded.bindings);
        assert_eq!(state.bindings.bindings[0], f.bindings.bindings[0]);
    }

    #[test]
    fn deleted_tenant_refused() {
        let mut f = Fixture::new();
        let mut raw = serde_json::to_value(&f.tenants[0]).unwrap();
        raw["deleted"] = json!(true);
        f.tenants[0] = serde_json::from_value(raw).unwrap();
        assert!(matches!(f.select(), Err(MigrationError::DeletedTenant)));
    }

    #[test]
    fn open_import_cross_tenant_binding_and_corrupt_orphan_refused() {
        let mut f = Fixture::new();
        f.imports.push(ImportJob {
            stored_collection: format!("t{T}__docs"),
            open: true,
        });
        assert!(matches!(f.select(), Err(MigrationError::OpenImport)));
        f.imports[0].stored_collection = format!("t{OTHER}__docs");
        assert!(f.select().is_ok());
        f.bindings.bindings[0].grants[0].scopes[0].tenant = OTHER.into();
        assert!(f.select().is_err());
        f.bindings.bindings.clear();
        let mut orphan = Recorded::of(&binding(T));
        orphan.grants_digest = "corrupt".into();
        f.orphans.push(orphan);
        assert!(matches!(f.select(), Err(MigrationError::CorruptOrphan)));
    }

    #[tokio::test]
    async fn ordinary_issue_and_rotate_unchanged() {
        use axum::{body::Body, http::Request};
        use http_body_util::BodyExt;
        use tower::ServiceExt;
        let directory = tempfile::tempdir().unwrap();
        let service = super::super::Service::new(
            crate::Database::create_with_format(
                directory.path().join("data.vlite"),
                2,
                crate::FormatVersion::V4,
            )
            .unwrap(),
        );
        let admin = "migration_test_admin_0123456789_ABCDEFGHIJKLMNOPQRSTUVWXYZ";
        let app = super::super::configured_admin_router(
            service,
            super::super::ServerOptions {
                admin_token: Some(super::super::BearerToken::new(admin).unwrap()),
                admin_directory: Some(directory.path().into()),
                ..Default::default()
            },
        )
        .unwrap();
        let call = |path: String, input: Value| {
            let app = app.clone();
            async move {
                let response = app
                    .oneshot(
                        Request::builder()
                            .method("POST")
                            .uri(path)
                            .header("authorization", format!("Bearer {admin}"))
                            .header("content-type", "application/json")
                            .body(Body::from(input.to_string()))
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                assert_eq!(response.status(), axum::http::StatusCode::CREATED);
                serde_json::from_slice::<Value>(
                    &response.into_body().collect().await.unwrap().to_bytes(),
                )
                .unwrap()
            }
        };
        let spec = json!({"name":"svc","collections":["*"],"operations":["read"],
            "expires_unix_ms":super::super::admin::now_ms() as u64+60_000});
        let first = call("/v1/admin/credentials".into(), spec.clone()).await;
        let second = call("/v1/admin/credentials".into(), spec).await;
        let rotated = call(
            format!(
                "/v1/admin/credentials/{}/rotate",
                first["credential"]["id"].as_str().unwrap()
            ),
            json!({"grace_seconds":0}),
        )
        .await;
        assert_ne!(first["credential"]["id"], second["credential"]["id"]);
        assert_ne!(first["credential"]["id"], rotated["credential"]["id"]);
        assert_ne!(first["token"], rotated["token"]);
    }
}
