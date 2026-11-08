//! Agent-memory REST adapter (serving contract section 3/8, mapping
//! `docs/projects/roadmap/gap1-adapter-mapping.md`). Startup checks every
//! bound resource and provisions the granted scopes before any listener
//! serves, and the nine routes are mounted only when a bindings file names at
//! least one binding: without a file every `/v1/memory/...` path is a plain
//! 404.

use std::collections::BTreeSet;

use axum::Router;

use super::{Service, reserved};
use crate::Database;
use crate::memory::binding::{Binding, BindingError, BindingState, Recorded};
use crate::memory::startup::{Refusal, plan};
use crate::memory::{MemoryMut, MemoryScope};

/// Startup refusal messages; they never name a binding, resource or scope.
pub(super) const GRAPH_DISABLED: &str = "memory bindings require a graph-enabled database";
pub(super) const GRAPH_MISSING: &str = "a memory binding graph does not exist";
pub(super) const EVIDENCE_MISSING: &str = "a memory binding evidence collection does not exist";
pub(super) const IN_USE: &str = "a memory binding resource already holds ordinary data";
pub(super) const PROVISION_FAILED: &str = "a memory binding scope could not be provisioned";
pub(super) const RECORDS_INVALID: &str = "a memory binding record in the database is not valid";
pub(super) const RECORDS_UNREADABLE: &str = "the memory binding records could not be read";
pub(super) const RECORD_FAILED: &str = "a memory binding record could not be written";

/// The message of a refused start: fixed text, one per class.
fn refusal_message(refusal: Refusal) -> &'static str {
    match refusal {
        Refusal::Records => RECORDS_INVALID,
        Refusal::Unreadable => RECORDS_UNREADABLE,
        Refusal::InUse => IN_USE,
        Refusal::Rotation(error) => rotation_message(error),
    }
}

fn rotation_message(error: BindingError) -> &'static str {
    use BindingError::*;
    match error {
        IdentityChanged => {
            "a memory binding changed its graph, evidence collection or receipt window"
        }
        GenerationNotAdvanced => "a memory binding changed without a higher generation",
        GenerationChanged => "a memory binding changed its generation without another change",
        Reactivated => "a retired memory binding cannot become active again",
        RetirementChanged => "a retired memory binding changed its retirement time",
        BackdatedRetirement => "a memory binding retirement time precedes this start",
        EarlyRemoval => {
            "a memory binding was removed before it was retired and its receipt window ended"
        }
        StorageReused => "a removed memory binding's resources are used by another binding",
        Malformed | TooLarge | InvalidName | InvalidTenant | InvalidGeneration | InvalidWindow
        | InvalidState | InvalidGrant | DuplicateBinding | SharedStorage => {
            "memory bindings are inconsistent with the database records"
        }
    }
}

/// Checks every binding's resources against the stored binding records,
/// writes the records and idempotently provisions the granted scopes of
/// active bindings (mapping section 6, B2; 0.7.1 design note section 1). A
/// read-only database is checked but never written: its memory writes are
/// refused. Resources are created by the existing admin routes, never here.
/// The bindings validator rejects the legacy `default` evidence collection.
/// Returns the records that the file does not name: their resources stay
/// reserved with no grants, so a database that holds memory is never served
/// raw because its file was removed or emptied.
pub(super) fn startup(service: &Service) -> std::io::Result<Vec<Recorded>> {
    start(service, crate::current_time_ms())
}

/// [`startup`] at an injected validation clock: tests never wait on real time.
#[cfg(test)]
pub(super) fn startup_at(service: &Service, now_ms: u64) -> std::io::Result<Vec<Recorded>> {
    start(service, now_ms)
}

/// The binding records alone, as a started server whose scopes were never
/// provisioned would hold them.
#[cfg(test)]
pub(super) fn write_records(service: &Service) {
    let mut owner = service.database.write().unwrap();
    let db = owner.as_mut().unwrap();
    for binding in service.memory_bindings.iter() {
        let (graph, evidence) = resources(binding);
        MemoryMut::bind(db, &graph, evidence.as_deref())
            .and_then(|mut memory| memory.write_binding_record(binding))
            .unwrap();
    }
}

fn start(service: &Service, now_ms: u64) -> std::io::Result<Vec<Recorded>> {
    let mut owner = service
        .database
        .write()
        .map_err(|_| std::io::Error::other(PROVISION_FAILED))?;
    let Some(db) = owner.as_mut() else {
        return if service.memory_bindings.is_configured() {
            Err(std::io::Error::other(PROVISION_FAILED))
        } else {
            Ok(Vec::new())
        };
    };
    prepare(db, &service.memory_bindings, now_ms).map_err(std::io::Error::other)
}

fn prepare(
    db: &mut Database,
    bindings: &reserved::Bindings,
    now_ms: u64,
) -> Result<Vec<Recorded>, &'static str> {
    if !db.is_graph_enabled() {
        return if bindings.is_configured() {
            Err(GRAPH_DISABLED)
        } else {
            Ok(Vec::new())
        };
    }
    // Every binding is checked before the first write.
    for binding in bindings.iter() {
        let (graph, evidence) = resources(binding);
        db.graph(&graph).map_err(|_| GRAPH_MISSING)?;
        if let Some(evidence) = &evidence {
            db.collection(evidence).map_err(|_| EVIDENCE_MISSING)?;
        }
    }
    let planned = plan(db, bindings.set(), now_ms).map_err(refusal_message)?;
    if db.is_read_only() {
        return Ok(planned.orphans);
    }
    for index in &planned.writes {
        let binding = &bindings.set().bindings[*index];
        let (graph, evidence) = resources(binding);
        MemoryMut::bind(db, &graph, evidence.as_deref())
            .and_then(|mut memory| memory.write_binding_record(binding))
            .map_err(|_| RECORD_FAILED)?;
    }
    for binding in bindings.iter() {
        if binding.state != BindingState::Active {
            continue;
        }
        let (graph, evidence) = resources(binding);
        let scopes: BTreeSet<_> = binding
            .grants
            .iter()
            .flat_map(|grant| {
                grant
                    .scopes
                    .iter()
                    .chain(&grant.read_scopes)
                    .map(scope_tuple)
            })
            .collect();
        let mut memory =
            MemoryMut::bind(db, &graph, evidence.as_deref()).map_err(|_| PROVISION_FAILED)?;
        for (tenant, agent, user, session) in scopes {
            memory
                .provision_scope(&MemoryScope {
                    tenant,
                    agent,
                    user,
                    session,
                })
                .map_err(|_| PROVISION_FAILED)?;
        }
    }
    Ok(planned.orphans)
}

/// The memory routes: none (plain 404) unless the file names a binding
/// (mapping section 6).
pub(super) fn routes(service: &Service) -> Router<Service> {
    if service.memory_bindings.is_configured() {
        routes::routes()
    } else {
        Router::new()
    }
}

/// Read routes (`graph_read`; read-only roles may reach them).
pub(super) fn read_route(route: &str) -> bool {
    matches!(
        route,
        routes::RECALL | routes::RECEIPT | routes::FIND_FACTS | routes::GET_ERASURE
    )
}

/// Write routes (`graph_write`).
pub(super) fn write_route(route: &str) -> bool {
    matches!(
        route,
        routes::REMEMBER
            | routes::CONSOLIDATE
            | routes::FORGET
            | routes::CORRECT_FACTS
            | routes::RESUME_ERASURE
    )
}

fn resources(binding: &Binding) -> (String, Option<String>) {
    let evidence = binding
        .evidence_collection
        .as_deref()
        .map(|name| binding.stored(name));
    (binding.stored(&binding.graph), evidence)
}

type ScopeTuple = (String, String, Option<String>, Option<String>);

fn scope_tuple(scope: &MemoryScope) -> ScopeTuple {
    (
        scope.tenant.clone(),
        scope.agent.clone(),
        scope.user.clone(),
        scope.session.clone(),
    )
}

#[cfg(test)]
mod pins_access;
#[cfg(test)]
mod pins_binding;
#[cfg(test)]
mod pins_consolidate;
#[cfg(test)]
mod pins_evidence;
#[cfg(test)]
mod pins_forget;
#[cfg(test)]
mod pins_receipts;
#[cfg(test)]
mod pins_remember;
#[cfg(test)]
mod replay;
mod routes;
mod wire;

#[cfg(test)]
mod tests;

#[cfg(test)]
mod pins_recall;
