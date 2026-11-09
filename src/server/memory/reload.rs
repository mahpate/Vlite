//! `POST /v1/admin/memory/bindings/reload`: applies a grants-only change of
//! the configured bindings file to the running server with the checks of a
//! restart. The file must name the same bindings with the same resources;
//! only grants and generations may differ. Every scope is provisioned and
//! every record written before the new view is installed, all under the
//! exclusive database write lock, and the answer then waits for the requests
//! that consulted superseded grants. No answer, log line or audit event names
//! a binding, tenant, principal, scope or path.

use std::collections::BTreeSet;
use std::sync::{PoisonError, atomic::Ordering};
use std::time::Duration;

use axum::{Json, extract::State, http::StatusCode};
use serde_json::{Value, json};

use super::super::{ApiError, Service, admin, internal, logs, reserved};
use super::{
    EVIDENCE_MISSING, GRAPH_DISABLED, GRAPH_MISSING, PROVISION_FAILED, RECORD_FAILED,
    refusal_message, resources, scope_tuple,
};
use crate::Database;
use crate::memory::binding::{BindingSet, BindingState};
use crate::memory::startup::plan;
use crate::memory::{MemoryError, MemoryMut, MemoryScope};

const UNCONFIGURED: &str = "memory bindings reload is unavailable; it needs a configured bindings file that names a binding";
const AFTER_GRPC: &str =
    "memory bindings reload is unavailable after a gRPC router was built; restart instead";
const IN_PROGRESS: &str = "a memory bindings reload is already running";
const RESTART_REQUIRED: &str = "a memory bindings reload may change only grants and generations; restart to apply other changes";

pub(in crate::server) async fn reload_bindings(
    State(service): State<Service>,
) -> Result<Json<Value>, ApiError> {
    reload(&service, reserved::DRAIN_TIMEOUT).await
}

/// The reload, waiting at most `drain` for the requests that consulted
/// superseded grants (tests wait less than [`reserved::DRAIN_TIMEOUT`]).
pub(super) async fn reload(service: &Service, drain: Duration) -> Result<Json<Value>, ApiError> {
    let served = &service.memory_bindings;
    let source = match served.source() {
        Some(source) if served.is_configured() => source.to_path_buf(),
        _ => return Err(reload_unavailable(UNCONFIGURED)),
    };
    let reloading = served.try_reload().ok_or_else(|| {
        ApiError::new(
            StatusCode::CONFLICT,
            "memory_reload_in_progress",
            IN_PROGRESS,
        )
    })?;
    let read = tokio::task::spawn_blocking(move || admin::read_memory_bindings(&source))
        .await
        .map_err(internal)?
        .map_err(|error| {
            ApiError::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "invalid_input",
                error.to_string(),
            )
        })?;
    let applying = service.clone();
    // The reload lock comes back with the result, so it is held while the
    // apply runs even if the client goes away, and then through the drain.
    let ((bindings, changed), reloading) = service
        .run(move |db| {
            let applied = apply(db, &applying, read, crate::current_time_ms())?;
            Ok((applied, reloading))
        })
        .await?;
    let drained = served.drain(drain).await;
    drop(reloading);
    let event = json!({
        "event": "memory_bindings_reloaded",
        "bindings": bindings,
        "changed": changed,
        "drained": drained,
    });
    logs::stderr("info", "memory", event.to_string());
    if let Some(log) = &service.admin.audit_log {
        let mut entry = event;
        entry["outcome"] = json!("ok");
        if log.append(entry, false).is_err() {
            service
                .metrics
                .audit_failures
                .fetch_add(1, Ordering::Relaxed);
        }
    }
    Ok(Json(
        json!({"bindings": bindings, "changed": changed, "drained": drained}),
    ))
}

/// Applies `read` under the exclusive database write lock that `db` comes
/// from: the delta and restart checks, the scopes, the records, then the
/// install of a changed view. Answers the number of bindings and how many
/// changed.
pub(super) fn apply(
    db: &mut Database,
    service: &Service,
    read: reserved::Bindings,
    now_ms: u64,
) -> Result<(usize, usize), ApiError> {
    // Held until the install, so no gRPC router adopts the cell in between.
    // Lock order: database write lock, `published_bindings`, the cell.
    let _published = service
        .published_bindings
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    if service.grpc_built.load(Ordering::SeqCst) {
        return Err(reload_unavailable(AFTER_GRPC));
    }
    let served = &service.memory_bindings;
    let old = served.current();
    let set = read.set();
    if !grants_only(old.set(), set) {
        return Err(restart_required());
    }
    let changed = set
        .bindings
        .iter()
        .filter(|entry| old.set().find(&entry.tenant, &entry.binding) != Some(*entry))
        .count();
    if !db.is_graph_enabled() {
        return Err(conflict(GRAPH_DISABLED));
    }
    for binding in &set.bindings {
        let (graph, evidence) = resources(binding);
        db.graph(&graph).map_err(|_| conflict(GRAPH_MISSING))?;
        if let Some(evidence) = &evidence {
            db.collection(evidence)
                .map_err(|_| conflict(EVIDENCE_MISSING))?;
        }
    }
    let planned = plan(db, set, now_ms).map_err(|refusal| conflict(refusal_message(refusal)))?;
    let new = read.with_orphans(&planned.orphans);
    if !new.covers(&old) || !old.covers(&new) {
        return Err(restart_required());
    }
    // A read-only database is checked and serves the new grants; its records
    // advance through the primary, as at startup.
    if !db.is_read_only() {
        provision(db, &new)?;
        for index in &planned.writes {
            let binding = &new.set().bindings[*index];
            let (graph, evidence) = resources(binding);
            MemoryMut::bind(db, &graph, evidence.as_deref())
                .and_then(|mut memory| memory.write_binding_record(binding))
                .map_err(|error| refused(error, RECORD_FAILED))?;
        }
    }
    if changed > 0 {
        served.install(new);
    }
    Ok((set.bindings.len(), changed))
}

/// Whether `new` names exactly the bindings of `old`, in any order, each with
/// the same state, resources and receipt window. Both sets are validated, so
/// no (tenant, binding) key repeats in either.
fn grants_only(old: &BindingSet, new: &BindingSet) -> bool {
    old.bindings.len() == new.bindings.len()
        && new.bindings.iter().all(|entry| {
            old.find(&entry.tenant, &entry.binding)
                .is_some_and(|served| {
                    served.state == entry.state
                        && served.graph == entry.graph
                        && served.evidence_collection == entry.evidence_collection
                        && served.receipt_window_ms == entry.receipt_window_ms
                })
        })
}

/// Provisions every scope and read scope of every active binding, as startup
/// does: an existing scope costs one lookup and no commit.
fn provision(db: &mut Database, bindings: &reserved::Bindings) -> Result<(), ApiError> {
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
        let mut memory = MemoryMut::bind(db, &graph, evidence.as_deref())
            .map_err(|error| refused(error, PROVISION_FAILED))?;
        for (tenant, agent, user, session) in scopes {
            memory
                .provision_scope(&MemoryScope {
                    tenant,
                    agent,
                    user,
                    session,
                })
                .map_err(|error| refused(error, PROVISION_FAILED))?;
        }
    }
    Ok(())
}

/// A provisioning or record failure: a storage error keeps its 5xx answer
/// (`commit_indeterminate`, `poisoned`); anything else is the startup refusal.
fn refused(error: MemoryError, message: &'static str) -> ApiError {
    if let MemoryError::Storage(error) = error {
        let answer = ApiError::from(error);
        if answer.status.is_server_error() {
            return answer;
        }
    }
    conflict(message)
}

fn reload_unavailable(message: &'static str) -> ApiError {
    ApiError::new(StatusCode::CONFLICT, "memory_reload_unavailable", message)
}

fn restart_required() -> ApiError {
    ApiError::new(
        StatusCode::CONFLICT,
        "memory_restart_required",
        RESTART_REQUIRED,
    )
}

fn conflict(message: &'static str) -> ApiError {
    ApiError::new(StatusCode::CONFLICT, "memory_bindings_conflict", message)
}
