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
use crate::memory::binding::{Binding, BindingState};
use crate::memory::{MemoryMut, MemoryRef, MemoryScope};

/// Startup refusal messages; they never name a binding, resource or scope.
pub(super) const GRAPH_DISABLED: &str = "memory bindings require a graph-enabled database";
pub(super) const GRAPH_MISSING: &str = "a memory binding graph does not exist";
pub(super) const EVIDENCE_MISSING: &str = "a memory binding evidence collection does not exist";
pub(super) const IN_USE: &str = "a memory binding resource already holds ordinary data";
pub(super) const PROVISION_FAILED: &str = "a memory binding scope could not be provisioned";

/// Checks every binding's resources and idempotently provisions the
/// granted scopes of active bindings (mapping section 6, B2). A read-only
/// database is checked but never written: its memory writes are refused.
/// Resources are created by the existing admin routes, never here.
/// **PROVISIONAL**: no persisted binding record yet, so rotation rules are
/// checked against the file alone (mapping section 4).
pub(super) fn startup(service: &Service) -> std::io::Result<()> {
    if service.memory_bindings.is_empty() {
        return Ok(());
    }
    let mut owner = service
        .database
        .write()
        .map_err(|_| std::io::Error::other(PROVISION_FAILED))?;
    let db = owner
        .as_mut()
        .ok_or_else(|| std::io::Error::other(PROVISION_FAILED))?;
    prepare(db, &service.memory_bindings).map_err(std::io::Error::other)
}

fn prepare(db: &mut Database, bindings: &reserved::Bindings) -> Result<(), &'static str> {
    if !db.is_graph_enabled() {
        return Err(GRAPH_DISABLED);
    }
    // Every binding is checked before the first write.
    for binding in bindings.iter() {
        let (graph, evidence) = resources(binding);
        db.graph(&graph).map_err(|_| GRAPH_MISSING)?;
        if let Some(evidence) = &evidence {
            db.collection(evidence).map_err(|_| EVIDENCE_MISSING)?;
        }
        if !MemoryRef::bind(db, &graph, evidence.as_deref())
            .and_then(|memory| memory.adoptable())
            .map_err(|_| IN_USE)?
        {
            return Err(IN_USE);
        }
    }
    if db.is_read_only() {
        return Ok(());
    }
    for binding in bindings.iter() {
        if binding.state != BindingState::Active {
            continue;
        }
        let (graph, evidence) = resources(binding);
        let scopes: BTreeSet<_> = binding
            .grants
            .iter()
            .flat_map(|grant| grant.scopes.iter().map(scope_tuple))
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
    Ok(())
}

/// The memory routes: none (plain 404) without bindings (mapping section 6).
pub(super) fn routes(service: &Service) -> Router<Service> {
    if service.memory_bindings.is_empty() {
        Router::new()
    } else {
        routes::routes()
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
mod pins_consolidate;
#[cfg(test)]
mod replay;
mod routes;
mod wire;

#[cfg(test)]
mod tests;
