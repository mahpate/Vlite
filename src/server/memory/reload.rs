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
    // The rest runs in a task of its own, which a client that goes away does
    // not cancel: the apply, the drain and the report all happen, once.
    tokio::spawn(complete(service.clone(), read, reloading, drain))
        .await
        .map_err(internal)?
}

/// Applies `read`, waits at most `drain` for the requests that consulted the
/// grants it superseded, then logs and audits the counts. The reload lock is
/// released last.
async fn complete(
    service: Service,
    read: reserved::Bindings,
    reloading: tokio::sync::OwnedMutexGuard<()>,
    drain: Duration,
) -> Result<Json<Value>, ApiError> {
    let applying = service.clone();
    // The reload lock comes back with the result, so it is held while the
    // apply runs, and then through the drain and the report.
    let ((bindings, changed), reloading) = service
        .run(move |db| {
            let applied = apply(db, &applying, read, crate::current_time_ms())?;
            Ok((applied, reloading))
        })
        .await?;
    let drained = service.memory_bindings.drain(drain).await;
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
    drop(reloading);
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
    let Checked {
        new,
        changed,
        writes,
    } = check(db, service, &read, now_ms)?;
    // A read-only database is checked and serves the new grants; its records
    // advance through the primary, as at startup.
    if !db.is_read_only() {
        provision(db, &new)?;
        for index in &writes {
            let binding = &new.set().bindings[*index];
            let (graph, evidence) = resources(binding);
            MemoryMut::bind(db, &graph, evidence.as_deref())
                .and_then(|mut memory| memory.write_binding_record(binding))
                .map_err(|error| refused(error, RECORD_FAILED))?;
        }
    }
    if changed > 0 {
        service.memory_bindings.install(new);
    }
    Ok((read.set().bindings.len(), changed))
}

/// What [`check`] found: the view a reload would install, how many bindings
/// changed, and which binding records it would write.
pub(super) struct Checked {
    new: reserved::Bindings,
    changed: usize,
    writes: Vec<usize>,
}

/// Every check of a reload of `read`, in reload's order, against `db`: the
/// gRPC adoption, the grants-only delta, the resources, the records plan and
/// the coverage both ways. It writes nothing, installs nothing and takes no
/// lock beyond the caller's, so a preflight runs it under the shared lock.
pub(super) fn check(
    db: &Database,
    service: &Service,
    read: &reserved::Bindings,
    now_ms: u64,
) -> Result<Checked, ApiError> {
    if service.grpc_built.load(Ordering::SeqCst) {
        return Err(reload_unavailable(AFTER_GRPC));
    }
    let old = service.memory_bindings.current();
    let set = read.set();
    if !grants_only(old.set(), set) {
        return Err(restart_required());
    }
    let changed = changed(old.set(), set);
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
    Ok(Checked {
        new,
        changed,
        writes: planned.writes,
    })
}

/// How many entries of `new` differ from the entry of the same tenant and
/// binding in `old`, grants and their order included. A reload counts its
/// `changed` with it, and status its agreement with the file.
fn changed(old: &BindingSet, new: &BindingSet) -> usize {
    new.bindings
        .iter()
        .filter(|entry| old.find(&entry.tenant, &entry.binding) != Some(*entry))
        .count()
}

/// `POST /v1/admin/memory/bindings/preflight`: what a reload of the file as it
/// is now would decide, with reload's own code and message, changing nothing.
/// It never takes the reload lock, so it cannot make a reload answer
/// `memory_reload_in_progress`; it runs [`check`] under the shared database
/// lock, and neither logs nor audits. The verdict is advisory: a reload
/// checks again.
pub(in crate::server) async fn preflight_bindings(
    State(service): State<Service>,
) -> Result<Json<Value>, ApiError> {
    let served = &service.memory_bindings;
    let source = match served.source() {
        Some(source) if served.is_configured() => source.to_path_buf(),
        _ => {
            return Ok(verdict(
                "unavailable",
                None,
                &reload_unavailable(UNCONFIGURED),
            ));
        }
    };
    let read = match tokio::task::spawn_blocking(move || admin::read_memory_bindings(&source))
        .await
        .map_err(internal)?
    {
        Ok(read) => read,
        Err(error) => {
            let invalid = ApiError::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "invalid_input",
                error.to_string(),
            );
            return Ok(verdict("invalid", None, &invalid));
        }
    };
    let bindings = read.set().bindings.len();
    let checking = service.clone();
    let checked = service
        .run_read(move |db| {
            let checked = check(db, &checking, &read, crate::current_time_ms());
            // A read-only database or replica writes no record on reload.
            let records = |checked: &Checked| {
                if db.is_read_only() {
                    0
                } else {
                    checked.writes.len()
                }
            };
            Ok(checked.map(|checked| (checked.changed, records(&checked))))
        })
        .await?;
    match checked {
        Ok((changed, records)) => Ok(Json(json!({
            "outcome": if changed > 0 { "reload" } else { "unchanged" },
            "bindings": bindings,
            "changed": changed,
            "records": records,
            "error": null,
        }))),
        Err(error) => match error.code {
            "memory_reload_unavailable" => Ok(verdict("unavailable", None, &error)),
            "memory_restart_required" => Ok(verdict("restart_required", Some(bindings), &error)),
            "memory_bindings_conflict" => Ok(verdict("refused", Some(bindings), &error)),
            _ => Err(error),
        },
    }
}

/// A preflight verdict that reload would refuse: the counts reload never
/// reached are `null`, and `error` is reload's own code and message.
fn verdict(outcome: &str, bindings: Option<usize>, error: &ApiError) -> Json<Value> {
    Json(json!({
        "outcome": outcome,
        "bindings": bindings,
        "changed": null,
        "records": null,
        "error": {"code": error.code, "message": error.message},
    }))
}

/// `GET /v1/admin/memory/bindings/status`: whether the served grants agree
/// with the configured file, and whether the requests that consulted
/// superseded grants have finished. It takes neither the database lock nor
/// the reload lock and changes nothing; it may lag a concurrent reload by one
/// call.
pub(in crate::server) async fn bindings_status(
    State(service): State<Service>,
) -> Result<Json<Value>, ApiError> {
    let served = &service.memory_bindings;
    let current = served.current();
    let file = match served.source() {
        None => "none",
        Some(source) => {
            let source = source.to_path_buf();
            match tokio::task::spawn_blocking(move || admin::read_memory_bindings(&source))
                .await
                .map_err(internal)?
            {
                Err(_) => "unreadable",
                Ok(read)
                    if read.set().bindings.len() == current.set().bindings.len()
                        && changed(current.set(), read.set()) == 0 =>
                {
                    "agrees"
                }
                Ok(_) => "differs",
            }
        }
    };
    Ok(Json(json!({
        "configured": current.is_configured(),
        "reloadable": admin::reloadable(&service),
        "bindings": current.set().bindings.len(),
        "file": file,
        "drained": served.is_drained(),
    })))
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
