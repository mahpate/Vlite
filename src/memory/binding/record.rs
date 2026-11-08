//! The persisted binding record (`binding:v1`): one crate-private graph node
//! per bound graph that stores the identity a restart must not change.
//! Design: `docs/projects/roadmap/m71-memory-followups.md` section 1.
//! Nothing here is a public API, wire route or public `Error` variant, and the
//! engine file format does not change: the record is an ordinary node.

use std::collections::BTreeSet;

use sha2::{Digest, Sha256};

use super::{Binding, BindingError, BindingState, Grant};
use crate::memory::{MAX_STRING_VALUE_BYTES, MemoryScope, hex};
use crate::property_graph::NodeView;
use crate::{Metadata, MetadataValue};

/// The one node key of the record in a bound graph.
pub(crate) const KEY: &str = "binding:v1";
/// The only label of the record node.
pub(crate) const LABEL: &str = "VliteMemoryBinding";
const RECORD_SCHEMA: &str = "memory-binding-record:v1";
const GRANTS_DOMAIN: &[u8] = b"vlite.memory.binding.grants.v1\0";
/// The domain of a grants digest when any grant has read scopes; without
/// them the bytes stay those of `GRANTS_DOMAIN`, so stored digests hold.
const GRANTS_DOMAIN_V2: &[u8] = b"vlite.memory.binding.grants.v2\0";
const MAX_RETIRED_AT: u64 = u64::MAX;

/// Properties every record has, and the two a record may have.
const REQUIRED: [&str; 11] = [
    "record_schema",
    "tenant",
    "binding",
    "generation",
    "state",
    "graph",
    "memory_schema",
    "ranking_version",
    "request_digest_version",
    "receipt_window_ms",
    "grants_digest",
];
const RETIRED_AT: &str = "retired_at_ms";
const EVIDENCE: &str = "evidence";

/// A node that claims to be a binding record and is not exactly one. It is
/// never repaired and carries no detail: the startup message names nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Corrupt;

/// What a record stores of one binding: its identity, rotation state and a
/// digest of its grants (the grants themselves are not stored).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Recorded {
    pub tenant: String,
    pub binding: String,
    pub generation: u64,
    pub state: BindingState,
    /// The file's graph name, not the tenant-qualified stored name.
    pub graph: String,
    pub evidence: Option<String>,
    pub receipt_window_ms: u64,
    pub grants_digest: String,
}

/// `v1:` and the SHA-256 of the grants as written (design note 1.1): the
/// order of grants and of scopes is part of the digest. When any grant has
/// read scopes, the domain is `GRANTS_DOMAIN_V2` and every grant also frames
/// its read scopes after its scopes.
pub(crate) fn grants_digest(grants: &[Grant]) -> String {
    let read_scoped = grants.iter().any(|entry| !entry.read_scopes.is_empty());
    let domain = if read_scoped {
        GRANTS_DOMAIN_V2
    } else {
        GRANTS_DOMAIN
    };
    let mut bytes = domain.to_vec();
    bytes.extend_from_slice(&(grants.len() as u32).to_be_bytes());
    for grant in grants {
        bytes.extend_from_slice(&(grant.principal.len() as u32).to_be_bytes());
        bytes.extend_from_slice(grant.principal.as_bytes());
        bytes.push(u8::from(grant.raw_access));
        bytes.extend_from_slice(&(grant.scopes.len() as u32).to_be_bytes());
        for scope in &grant.scopes {
            let encoded = scope.encode();
            bytes.extend_from_slice(&(encoded.len() as u32).to_be_bytes());
            bytes.extend_from_slice(&encoded);
        }
        if read_scoped {
            framed(&mut bytes, &grant.read_scopes);
        }
    }
    format!("v1:{}", hex(&Sha256::digest(bytes)))
}

/// Appends a read-scope count, then each read scope's length and encoding.
fn framed(out: &mut Vec<u8>, read_scopes: &[MemoryScope]) {
    out.extend_from_slice(&(read_scopes.len() as u32).to_be_bytes());
    for read_scope in read_scopes {
        let encoding = read_scope.encode();
        out.extend_from_slice(&(encoding.len() as u32).to_be_bytes());
        out.extend_from_slice(&encoding);
    }
}

/// Digits only, no sign, no leading zero, no whitespace, within `low..=high`.
fn canonical(value: &str, low: u64, high: u64) -> Option<u64> {
    let bytes = value.as_bytes();
    if bytes.is_empty()
        || bytes.len() > 20
        || !bytes.iter().all(u8::is_ascii_digit)
        || (bytes[0] == b'0' && bytes.len() > 1)
    {
        return None;
    }
    value
        .parse::<u64>()
        .ok()
        .filter(|number| (low..=high).contains(number))
}

fn digest_shaped(value: &str) -> bool {
    value.len() == 67
        && value.starts_with("v1:")
        && value[3..]
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

impl Recorded {
    /// The record the file entry `binding` stands for.
    pub(crate) fn of(binding: &Binding) -> Self {
        Self {
            tenant: binding.tenant.clone(),
            binding: binding.binding.clone(),
            generation: binding.generation,
            state: binding.state,
            graph: binding.graph.clone(),
            evidence: binding.evidence_collection.clone(),
            receipt_window_ms: binding.receipt_window_ms,
            grants_digest: grants_digest(&binding.grants),
        }
    }

    /// The node properties (design note 1.1), all strings.
    pub(crate) fn properties(&self) -> Metadata {
        let mut properties = Metadata::new();
        let mut put = |name: &str, value: String| {
            properties.insert(name.into(), MetadataValue::String(value));
        };
        put("record_schema", RECORD_SCHEMA.into());
        put("tenant", self.tenant.clone());
        put("binding", self.binding.clone());
        put("generation", self.generation.to_string());
        put(
            "state",
            match self.state {
                BindingState::Active => "active",
                BindingState::Retired { .. } => "retired",
            }
            .into(),
        );
        if let BindingState::Retired { at_ms } = self.state {
            put(RETIRED_AT, at_ms.to_string());
        }
        put("graph", self.graph.clone());
        if let Some(evidence) = &self.evidence {
            put(EVIDENCE, evidence.clone());
        }
        put("memory_schema", super::SCHEMA_VERSION.into());
        put("ranking_version", super::RANKING_VERSION.into());
        put("request_digest_version", super::DIGEST_VERSION.into());
        put("receipt_window_ms", self.receipt_window_ms.to_string());
        put("grants_digest", self.grants_digest.clone());
        properties
    }

    /// The file's field rules applied to the stored fields (the grants are
    /// not stored, so none are checked).
    fn shell(&self) -> Binding {
        Binding {
            binding: self.binding.clone(),
            tenant: self.tenant.clone(),
            generation: self.generation,
            state: self.state,
            graph: self.graph.clone(),
            evidence_collection: self.evidence.clone(),
            receipt_window_ms: self.receipt_window_ms,
            grants: Vec::new(),
        }
    }

    /// The stored graph name, `(true, graph)`, then the stored evidence
    /// collection name, `(false, collection)`; see [`Binding::storage`].
    pub(crate) fn storage(&self) -> impl Iterator<Item = (bool, String)> {
        let shell = self.shell();
        shell.storage().collect::<Vec<_>>().into_iter()
    }

    /// The stored name of the graph this record belongs in.
    pub(crate) fn stored_graph(&self) -> String {
        self.shell().stored(&self.graph)
    }

    /// Rule 1: the fields hashed into or naming retained receipts.
    pub(super) fn identity(&self) -> (&str, Option<&str>, u64) {
        (
            &self.graph,
            self.evidence.as_deref(),
            self.receipt_window_ms,
        )
    }

    /// Decodes `node` strictly. `holder` is the stored name of the graph that
    /// holds it: a record copied into another graph is corrupt.
    pub(crate) fn decode(node: &NodeView, holder: &str) -> Result<Self, Corrupt> {
        if node.key != KEY
            || node.labels.len() != 1
            || !node.labels.contains(LABEL)
            || node.expires_at_ms.is_some()
        {
            return Err(Corrupt);
        }
        let mut text = std::collections::BTreeMap::new();
        for (name, value) in &node.properties {
            match value {
                MetadataValue::String(value) if value.len() <= MAX_STRING_VALUE_BYTES => {
                    text.insert(name.as_str(), value.as_str());
                }
                _ => return Err(Corrupt),
            }
        }
        if text
            .keys()
            .any(|name| !REQUIRED.contains(name) && *name != RETIRED_AT && *name != EVIDENCE)
        {
            return Err(Corrupt);
        }
        let get = |name: &str| text.get(name).copied().ok_or(Corrupt);
        if get("record_schema")? != RECORD_SCHEMA
            || get("memory_schema")? != super::SCHEMA_VERSION
            || get("ranking_version")? != super::RANKING_VERSION
            || get("request_digest_version")? != super::DIGEST_VERSION
        {
            return Err(Corrupt);
        }
        let generation = canonical(get("generation")?, 1, super::MAX_GENERATION).ok_or(Corrupt)?;
        let receipt_window_ms =
            canonical(get("receipt_window_ms")?, 1, super::MAX_RECEIPT_WINDOW_MS).ok_or(Corrupt)?;
        let state = match (get("state")?, text.get(RETIRED_AT)) {
            ("active", None) => BindingState::Active,
            ("retired", Some(at_ms)) => BindingState::Retired {
                at_ms: canonical(at_ms, 1, MAX_RETIRED_AT).ok_or(Corrupt)?,
            },
            _ => return Err(Corrupt),
        };
        let grants_digest = get("grants_digest")?;
        if !digest_shaped(grants_digest) {
            return Err(Corrupt);
        }
        let recorded = Self {
            tenant: get("tenant")?.to_owned(),
            binding: get("binding")?.to_owned(),
            generation,
            state,
            graph: get("graph")?.to_owned(),
            evidence: text.get(EVIDENCE).map(|name| (*name).to_owned()),
            receipt_window_ms,
            grants_digest: grants_digest.to_owned(),
        };
        if recorded.shell().validate().is_err() || recorded.stored_graph() != holder {
            return Err(Corrupt);
        }
        Ok(recorded)
    }

    /// Whether `tenant` and `binding` are this record's key.
    pub(crate) fn is(&self, tenant: &str, binding: &str) -> bool {
        self.tenant == tenant && self.binding == binding
    }
}

/// The previous state read from the database must be self-consistent before
/// the rotation rules run on it: one record per `(tenant, binding)` and no
/// stored name owned twice (the file's own rules, applied to records).
pub(crate) fn validate_all(previous: &[Recorded]) -> Result<(), BindingError> {
    let mut keys = BTreeSet::new();
    let mut storage = BTreeSet::new();
    for record in previous {
        if !keys.insert((record.tenant.as_str(), record.binding.as_str())) {
            return Err(BindingError::DuplicateBinding);
        }
        for name in record.storage() {
            if !storage.insert(name) {
                return Err(BindingError::SharedStorage);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
