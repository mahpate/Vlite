//! Crate-private memory binding configuration (`memory-bindings-v1`).
//! Contract: `docs/projects/roadmap/gap1-adapter-mapping.md` section 4.
//! Nothing here is a public API, wire route or public `Error` variant.

use super::mapping::{WireCode, WireOperation, unavailable};
use std::collections::BTreeSet;

use super::MemoryScope;
use crate::format::valid_collection_name;
use crate::property_graph::engine::model::validate_graph_name;

pub(crate) const ROOT_TENANT: &str = "@root";
pub(crate) const VERSION: &str = "memory-bindings-v1";
pub(crate) const SCHEMA_VERSION: &str = "memory-schema-v1";
pub(crate) const RANKING_VERSION: &str = "memory-ranking-v1";
pub(crate) const DIGEST_VERSION: &str = "memory-request-v1";
const MAX_RECEIPT_WINDOW_MS: u64 = 2_592_000_000;
const MAX_GENERATION: u64 = i64::MAX as u64;
/// PROVISIONAL caps (mapping section 4).
pub(crate) const MAX_FILE_BYTES: usize = 1024 * 1024;
const MAX_BINDINGS: usize = 256;
const MAX_GRANTS: usize = 1024;
const MAX_SCOPES: usize = 256;
/// Credential IDs are 12 characters of this alphabet (`server::credentials`).
const PRINCIPAL_ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz234567";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BindingState {
    Active,
    Retired { at_ms: u64 },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Grant {
    /// A credential ID, never a token.
    pub principal: String,
    /// Exact scopes; each scope's tenant is the entry's tenant.
    pub scopes: Vec<MemoryScope>,
    /// Raw access to the reserved resources (mapping section 6). Never a
    /// memory scope grant; the underlying grants are still required.
    pub raw_access: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Binding {
    pub binding: String,
    pub tenant: String,
    pub generation: u64,
    pub state: BindingState,
    pub graph: String,
    pub evidence_collection: Option<String>,
    pub receipt_window_ms: u64,
    pub grants: Vec<Grant>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct BindingSet {
    pub bindings: Vec<Binding>,
}

/// Why a binding set (or a rotation to it) is rejected as a whole.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BindingError {
    /// Not strict `memory-bindings-v1` JSON (syntax, unknown or duplicate
    /// key, null, missing field, wrong type or unsupported version value).
    Malformed,
    /// Over a section 4 cap (file bytes, bindings, grants or scopes).
    TooLarge,
    InvalidName,
    InvalidTenant,
    InvalidGeneration,
    InvalidWindow,
    InvalidState,
    InvalidGrant,
    DuplicateBinding,
    SharedStorage,
    IdentityChanged,
    GenerationNotAdvanced,
    GenerationChanged,
    Reactivated,
    RetirementChanged,
    BackdatedRetirement,
    EarlyRemoval,
    StorageReused,
}

fn valid_principal(principal: &str) -> bool {
    principal.len() == 12 && principal.bytes().all(|b| PRINCIPAL_ALPHABET.contains(&b))
}

fn valid_tenant(tenant: &str) -> bool {
    tenant == ROOT_TENANT
        || (tenant.len() == 32
            && tenant
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)))
}

fn tenant_qualified(name: &str) -> bool {
    name.strip_prefix('t')
        .and_then(|name| name.split_once("__"))
        .is_some_and(|(tenant, local)| {
            tenant.len() == 32
                && tenant
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                && !local.is_empty()
        })
}

impl Binding {
    fn validate(&self) -> Result<(), BindingError> {
        if !valid_collection_name(&self.binding) {
            return Err(BindingError::InvalidName);
        }
        if !valid_tenant(&self.tenant) {
            return Err(BindingError::InvalidTenant);
        }
        if !(1..=MAX_GENERATION).contains(&self.generation) {
            return Err(BindingError::InvalidGeneration);
        }
        if !(1..=MAX_RECEIPT_WINDOW_MS).contains(&self.receipt_window_ms) {
            return Err(BindingError::InvalidWindow);
        }
        if self.state == (BindingState::Retired { at_ms: 0 }) {
            return Err(BindingError::InvalidState);
        }
        if self.grants.len() > MAX_GRANTS
            || self
                .grants
                .iter()
                .any(|grant| grant.scopes.len() > MAX_SCOPES)
        {
            return Err(BindingError::TooLarge);
        }
        if validate_graph_name(&self.graph).is_err()
            || tenant_qualified(&self.graph)
            || self
                .evidence_collection
                .as_deref()
                .is_some_and(|name| !valid_collection_name(name) || tenant_qualified(name))
        {
            return Err(BindingError::InvalidName);
        }
        let mut principals = BTreeSet::new();
        for grant in &self.grants {
            if !valid_principal(&grant.principal)
                || !principals.insert(grant.principal.as_str())
                || grant.scopes.is_empty()
            {
                return Err(BindingError::InvalidGrant);
            }
            let mut scopes = BTreeSet::new();
            for scope in &grant.scopes {
                if scope.validate().is_err()
                    || scope.tenant != self.tenant
                    || !scopes.insert(scope.encode())
                {
                    return Err(BindingError::InvalidGrant);
                }
            }
        }
        Ok(())
    }

    /// Rule 1: the fields hashed into or naming retained receipts.
    fn identity(&self) -> (&str, Option<&str>, u64) {
        (
            &self.graph,
            self.evidence_collection.as_deref(),
            self.receipt_window_ms,
        )
    }

    /// The stored name of a caller-visible graph or collection name: the
    /// tenant-qualified `t{tenant}__{name}` exactly as the existing routes
    /// qualify it, unchanged for `@root`.
    pub(crate) fn stored(&self, name: &str) -> String {
        if self.tenant == ROOT_TENANT {
            name.to_owned()
        } else {
            format!("t{}__{name}", self.tenant)
        }
    }

    /// Stored names, `true` for the graph and `false` for the collection.
    pub(crate) fn storage(&self) -> impl Iterator<Item = (bool, String)> + '_ {
        std::iter::once((true, self.stored(&self.graph))).chain(
            self.evidence_collection
                .as_deref()
                .map(|name| (false, self.stored(name))),
        )
    }

    /// Rule 4: a retired binding serves GetReceipt (and later GetErasure)
    /// only; the first-slice refusals of `WireOperation::refusal` still apply.
    pub(crate) fn refusal(&self, operation: WireOperation) -> Option<WireCode> {
        match (self.state, operation) {
            (BindingState::Active, _)
            | (
                BindingState::Retired { .. },
                WireOperation::GetReceipt | WireOperation::GetErasure,
            ) => operation.refusal(),
            (BindingState::Retired { .. }, _) => Some(unavailable("binding_retired")),
        }
    }
}

impl BindingSet {
    /// Validates one set on its own (section 4 field rules).
    pub(crate) fn validate(&self) -> Result<(), BindingError> {
        if self.bindings.len() > MAX_BINDINGS {
            return Err(BindingError::TooLarge);
        }
        let mut keys = BTreeSet::new();
        let mut storage = BTreeSet::new();
        for binding in &self.bindings {
            binding.validate()?;
            if !keys.insert((binding.tenant.as_str(), binding.binding.as_str())) {
                return Err(BindingError::DuplicateBinding);
            }
            for name in binding.storage() {
                if !storage.insert(name) {
                    return Err(BindingError::SharedStorage);
                }
            }
        }
        Ok(())
    }

    /// Validates `self` as the replacement of the running set `old` at the
    /// validation clock `now_ms` (section 4 generation rotation rule).
    pub(crate) fn validate_rotation(&self, old: &Self, now_ms: u64) -> Result<(), BindingError> {
        old.validate()?;
        self.validate()?;
        let mut removed = BTreeSet::new();
        for after in &self.bindings {
            // Anti-backdating: an entry first seen already retired starts its
            // removal window no earlier than this change (PROVISIONAL).
            if let BindingState::Retired { at_ms } = after.state
                && old.find(&after.tenant, &after.binding).is_none()
                && at_ms < now_ms
            {
                return Err(BindingError::BackdatedRetirement);
            }
        }
        for before in &old.bindings {
            let Some(after) = self.find(&before.tenant, &before.binding) else {
                // Rule 5.
                let BindingState::Retired { at_ms } = before.state else {
                    return Err(BindingError::EarlyRemoval);
                };
                if at_ms
                    .checked_add(before.receipt_window_ms)
                    .is_none_or(|end| now_ms < end)
                {
                    return Err(BindingError::EarlyRemoval);
                }
                removed.extend(before.storage());
                continue;
            };
            if after.identity() != before.identity() {
                return Err(BindingError::IdentityChanged);
            }
            match (before.state, after.state) {
                (BindingState::Retired { .. }, BindingState::Active) => {
                    return Err(BindingError::Reactivated);
                }
                (BindingState::Retired { at_ms: was }, BindingState::Retired { at_ms })
                    if was != at_ms =>
                {
                    return Err(BindingError::RetirementChanged);
                }
                // PROVISIONAL: rule 5 counts the window from `retired_at_ms`,
                // so it may not precede the rotation that retires the entry.
                (BindingState::Active, BindingState::Retired { at_ms }) if at_ms < now_ms => {
                    return Err(BindingError::BackdatedRetirement);
                }
                _ => {}
            }
            let changed = after.state != before.state || after.grants != before.grants;
            if changed && after.generation <= before.generation {
                return Err(BindingError::GenerationNotAdvanced);
            }
            if !changed && after.generation != before.generation {
                return Err(BindingError::GenerationChanged);
            }
        }
        // Rule 6: kept entries cannot rename storage (rule 1), so any user is
        // a different key. Stored names are tenant-qualified.
        if self
            .bindings
            .iter()
            .flat_map(Binding::storage)
            .any(|name| removed.contains(&name))
        {
            return Err(BindingError::StorageReused);
        }
        Ok(())
    }

    /// The entry for `(tenant, binding)`, if any.
    pub(crate) fn find(&self, tenant: &str, binding: &str) -> Option<&Binding> {
        self.bindings
            .iter()
            .find(|entry| entry.tenant == tenant && entry.binding == binding)
    }
}

#[cfg(feature = "json")]
mod json {
    use serde::{Deserialize, Deserializer};

    use super::*;

    /// Required and nullable: a missing key is an error, not `None`.
    fn nullable<'de, D: Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
        Option::deserialize(d)
    }

    /// Optional and non-null: present values must not be `null`.
    fn present<'de, D: Deserializer<'de>, T: Deserialize<'de>>(
        d: D,
    ) -> Result<Option<T>, D::Error> {
        T::deserialize(d).map(Some)
    }

    fn exact<'de, D: Deserializer<'de>>(d: D, expected: &str) -> Result<(), D::Error> {
        let value = String::deserialize(d)?;
        if value == expected {
            Ok(())
        } else {
            Err(serde::de::Error::custom("unsupported version"))
        }
    }

    fn version<'de, D: Deserializer<'de>>(d: D) -> Result<(), D::Error> {
        exact(d, VERSION)
    }
    fn schema<'de, D: Deserializer<'de>>(d: D) -> Result<(), D::Error> {
        exact(d, SCHEMA_VERSION)
    }
    fn ranking<'de, D: Deserializer<'de>>(d: D) -> Result<(), D::Error> {
        exact(d, RANKING_VERSION)
    }
    fn digest<'de, D: Deserializer<'de>>(d: D) -> Result<(), D::Error> {
        exact(d, DIGEST_VERSION)
    }

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Document {
        #[serde(deserialize_with = "version")]
        #[allow(dead_code)]
        version: (),
        bindings: Vec<Entry>,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "lowercase")]
    enum State {
        Active,
        Retired,
    }

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Entry {
        binding: String,
        tenant: String,
        generation: u64,
        state: State,
        #[serde(default, deserialize_with = "present")]
        retired_at_ms: Option<u64>,
        graph: String,
        #[serde(deserialize_with = "nullable")]
        evidence_collection: Option<String>,
        #[serde(deserialize_with = "schema")]
        #[allow(dead_code)]
        schema_version: (),
        #[serde(deserialize_with = "ranking")]
        #[allow(dead_code)]
        ranking_version: (),
        #[serde(deserialize_with = "digest")]
        #[allow(dead_code)]
        digest_version: (),
        receipt_window_ms: u64,
        grants: Vec<GrantEntry>,
    }

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct GrantEntry {
        principal: String,
        scopes: Vec<ScopeEntry>,
        #[serde(default)]
        raw_access: bool,
    }

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct ScopeEntry {
        agent: String,
        #[serde(default, deserialize_with = "present")]
        user: Option<String>,
        #[serde(default, deserialize_with = "present")]
        session: Option<String>,
    }

    pub(crate) fn parse(bytes: &[u8]) -> Result<BindingSet, BindingError> {
        if bytes.len() > MAX_FILE_BYTES {
            return Err(BindingError::TooLarge);
        }
        let document: Document =
            serde_json::from_slice(bytes).map_err(|_| BindingError::Malformed)?;
        let mut bindings = Vec::with_capacity(document.bindings.len());
        for entry in document.bindings {
            let state = match (entry.state, entry.retired_at_ms) {
                (State::Active, None) => BindingState::Active,
                (State::Retired, Some(at_ms)) => BindingState::Retired { at_ms },
                _ => return Err(BindingError::InvalidState),
            };
            let tenant = entry.tenant;
            let grants = entry
                .grants
                .into_iter()
                .map(|grant| Grant {
                    principal: grant.principal,
                    raw_access: grant.raw_access,
                    scopes: grant
                        .scopes
                        .into_iter()
                        .map(|scope| MemoryScope {
                            tenant: tenant.clone(),
                            agent: scope.agent,
                            user: scope.user,
                            session: scope.session,
                        })
                        .collect(),
                })
                .collect();
            bindings.push(Binding {
                binding: entry.binding,
                tenant,
                generation: entry.generation,
                state,
                graph: entry.graph,
                evidence_collection: entry.evidence_collection,
                receipt_window_ms: entry.receipt_window_ms,
                grants,
            });
        }
        let set = BindingSet { bindings };
        set.validate()?;
        Ok(set)
    }
}

#[cfg(feature = "json")]
#[cfg_attr(
    not(feature = "server"),
    allow(unused_imports, reason = "only the server reads a bindings file")
)]
pub(crate) use json::parse;

#[cfg(test)]
mod tests;
