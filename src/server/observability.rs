//! F8 observability: JSON and Prometheus metrics plus the bounded slow-query
//! log. Scoped principals see only their granted collections; database-wide
//! counters are reserved for admin, read-only and `*` grants.
use super::*;
use std::fmt::Write as _;

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct MetricsQuery {
    format: Option<String>,
}

pub(super) fn record_slow(metrics: &Metrics, entry: Value) {
    if let Ok(mut slow) = metrics.slow.lock() {
        if slow.len() == SLOW_QUERY_CAPACITY {
            slow.pop_front();
        }
        slow.push_back(entry);
    }
}

/// A slow-query entry is visible to a scoped principal only under the same
/// binding as normal access: database lineage, collection ID and name. A
/// dropped and recreated collection (new ID) is never visible to an old grant.
fn visible(role: &admin::Role, item: &Value) -> bool {
    let admin::Role::Scoped(grant) = role else {
        return true;
    };
    let lineage = item["lineage"]
        .as_str()
        .and_then(|lineage| u64::from_str_radix(lineage, 16).ok());
    let collection = item["collection_id"]
        .as_u64()
        .and_then(|id| u16::try_from(id).ok());
    match (collection, item["collection"].as_str()) {
        (Some(collection), Some(name)) => grant.covers(lineage, collection, name),
        _ => false,
    }
}

fn global(role: &admin::Role) -> bool {
    match role {
        admin::Role::Scoped(grant) => grant.all && grant.tenant_id.is_none(),
        _ => true,
    }
}

fn collection_metrics(db: &Database, role: &admin::Role) -> Vec<Value> {
    db.collections
        .iter()
        .filter(|&(&id, state)| match role {
            admin::Role::Scoped(grant) => grant.covers(db.lineage(), id, &state.name),
            _ => true,
        })
        .map(|(&id, state)| {
            let index = state.index.as_ref();
            let (buffered_ids, buffer_bytes) = index.map_or((0, 0), |i| i.buffer_usage());
            let memory = index.map(|i| i.memory());
            json!({"id": id, "name": state.name, "live_records": state.records.len(),
                "live_body_bytes": state.live_body_bytes,
                "hnsw_ready": index.is_some(),
                "hnsw_graph_nodes": index.map_or(0, |i| i.graph_nodes()),
                "hnsw_stale_nodes": index.map_or(0, |i| i.stale_nodes()),
                "hnsw_buffered_ids": buffered_ids, "hnsw_buffer_vector_bytes": buffer_bytes,
                "hnsw_link_bytes": memory.map_or(0, |m| m.link_bytes),
                "hnsw_code_bytes": memory.map_or(0, |m| m.code_bytes),
                "resident_vector_bytes": state.records.len() as u64 * state.dimensions as u64 * 4})
        })
        .collect()
}

/// Full JSON metrics (admin view). Old keys are preserved unchanged.
#[cfg(test)]
pub(super) async fn metrics(State(service): State<Service>) -> Result<Json<Value>, ApiError> {
    snapshot(service, admin::Role::Admin).await.map(Json)
}

async fn snapshot(service: Service, role: admin::Role) -> Result<Value, ApiError> {
    let metrics = service.metrics.clone();
    let ops = service.ops.metrics_json();
    let format_version = service.format_version;
    service
        .run_read(move |db| {
            let healthy = !db.poisoned;
            let collections = if healthy {
                collection_metrics(db, &role)
            } else {
                Vec::new()
            };
            if !global(&role) {
                return Ok(json!({"scoped": true, "collections": collections}));
            }
            let database_bytes = if healthy {
                Some(db.file.0.metadata().map_err(crate::Error::from)?.len())
            } else {
                None
            };
            let (buffered_ids, buffer_bytes) = db
                .collections
                .get(&0)
                .and_then(|c| c.index.as_ref())
                .map_or((0, 0), |index| index.buffer_usage());
            let live_bytes: u64 = db.collections.values().map(|c| c.live_body_bytes).sum();
            let load = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
            let last_build = load(&metrics.last_rebuild_ns);
            let mut value = json!({
                "requests_total": load(&metrics.requests),
                "request_failures_total": load(&metrics.failures),
                "unauthorized_total": load(&metrics.unauthorized),
                "request_duration_seconds_total": load(&metrics.duration_ns) as f64 / 1e9,
                "grpc_requests_total": load(&metrics.grpc_requests),
                "grpc_request_failures_total": load(&metrics.grpc_failures),
                "grpc_stream_messages_total": load(&metrics.grpc_stream_messages),
                "commit_failures_total": load(&metrics.commit_failures),
                "group_commit_groups_total": load(&metrics.group_commit_groups),
                "group_commit_requests_total": load(&metrics.group_commit_requests),
                "hnsw_rebuilds_total": load(&metrics.rebuilds),
                "hnsw_buffered_ids": buffered_ids,
                "hnsw_buffer_vector_bytes": buffer_bytes,
                "hnsw_rebuild_and_search_seconds_total": load(&metrics.rebuild_search_ns) as f64 / 1e9,
                "hnsw_last_build_seconds": (last_build > 0).then(|| (last_build - 1) as f64 / 1e9),
                "hnsw_background_rebuilds_started_total": load(&metrics.background_started),
                "hnsw_background_rebuilds_published_total": load(&metrics.background_published),
                "hnsw_rebuild_pending": db.rebuild.is_some(),
                "search_graph_nodes_visited_total": load(&metrics.graph_nodes_visited),
                "search_distance_evaluations_total": load(&metrics.distance_evaluations),
                "search_quantized_distance_evaluations_total": load(&metrics.quantized_distance_evaluations),
                "search_reranked_candidates_total": load(&metrics.reranked),
                "slow_queries_total": load(&metrics.slow_queries),
                "slow_query_threshold_seconds": load(&metrics.slow_threshold_ns) as f64 / 1e9,
                "import_chunks_total": load(&metrics.import_chunks),
                "import_rows_skipped_total": load(&metrics.import_rows_skipped),
                "import_open_jobs": if healthy { db.imports.open_jobs().count() } else { 0 },
                "credential_denials_total": load(&metrics.credential_denials),
                "rate_limited_total": load(&metrics.rate_limited),
                "quota_exceeded_total": load(&metrics.quota_exceeded),
                "audit_write_failures_total": load(&metrics.audit_failures),
                "ann_quality_last_recall": metrics.ann_quality_last_recall.lock().ok().and_then(|r| *r),
                "database_healthy": healthy,
                "database_bytes": database_bytes,
                "database_valid_bytes": healthy.then_some(db.end),
                "reclaimable_bytes_estimate": healthy.then(|| db.end.saturating_sub(live_bytes)),
                "live_records": healthy.then(|| db.len()),
                "format_version": format_version,
                "collections_total": db.collections.len(),
                "searches_by_strategy": metrics.by_strategy.lock().map(|m| m.clone()).unwrap_or_default(),
                "collections": collections,
            });
            if let (Value::Object(target), Value::Object(extra)) = (&mut value, ops) {
                target.extend(extra);
            }
            Ok(value)
        })
        .await
}

/// `GET /metrics[?format=json|prometheus]`.
pub(super) async fn metrics_route(
    State(service): State<Service>,
    Extension(role): Extension<admin::Role>,
    query: Result<Query<MetricsQuery>, QueryRejection>,
) -> Result<Response, ApiError> {
    let Query(query) = query.map_err(|error| {
        ApiError::new(StatusCode::BAD_REQUEST, "invalid_query", error.body_text())
    })?;
    let value = snapshot(service, role).await?;
    match query.format.as_deref() {
        None | Some("json") => Ok(Json(value).into_response()),
        Some("prometheus") => Ok((
            [(
                header::CONTENT_TYPE,
                "text/plain; version=0.0.4; charset=utf-8",
            )],
            prometheus(&value),
        )
            .into_response()),
        Some(_) => Err(unprocessable("format must be json or prometheus")),
    }
}

pub(super) async fn logs(
    query: Result<Query<super::logs::Query>, QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    let Query(query) = query.map_err(|error| {
        ApiError::new(StatusCode::BAD_REQUEST, "invalid_query", error.body_text())
    })?;
    super::logs::query(query)
        .map(Json)
        .map_err(|(code, message)| ApiError::new(StatusCode::BAD_REQUEST, code, message))
}

/// Compact UI snapshot. Route latency percentiles use the most recent 1024
/// completed observations per route; search totals use the existing strategy
/// counters, and database fields come from a shared database read lock.
pub(super) async fn summary(State(service): State<Service>) -> Result<Json<Value>, ApiError> {
    let metrics = service.metrics.clone();
    let route_snapshot = metrics
        .by_route
        .lock()
        .map_err(internal)?
        .iter()
        .map(|((method, route), stats)| {
            (
                method.clone(),
                route.clone(),
                stats.count,
                stats.errors,
                stats.durations_ms.iter().copied().collect::<Vec<_>>(),
            )
        })
        .collect::<Vec<_>>();
    let mut all_durations = route_snapshot
        .iter()
        .flat_map(|(_, _, _, _, durations)| durations.iter().copied())
        .collect::<Vec<_>>();
    let mut search_durations = route_snapshot
        .iter()
        .filter(|(_, route, _, _, _)| route.contains("/search"))
        .flat_map(|(_, _, _, _, durations)| durations.iter().copied())
        .collect::<Vec<_>>();
    let route_stats = route_snapshot
        .iter()
        .map(|(method, route, count, errors, durations)| {
            json!({"method":method,"route":route,"count":count,"errors":errors,
            "p50_ms":percentile(durations, 0.50),"p95_ms":percentile(durations, 0.95),
            "p99_ms":percentile(durations, 0.99)})
        })
        .collect::<Vec<_>>();
    let strategies = metrics.by_strategy.lock().map_err(internal)?.clone();
    let search_total = strategies.values().copied().sum::<u64>();
    let search_mean = (!search_durations.is_empty())
        .then(|| search_durations.iter().sum::<u64>() as f64 / search_durations.len() as f64);
    let (log_dropped, log_capacity, last_seq) = super::logs::buffer().stats();
    let ts_ms = admin::now_ms() as u64;
    let uptime_s = service.started.elapsed().as_secs_f64();
    all_durations.sort_unstable();
    search_durations.sort_unstable();
    let snapshot = json!({
        "ts_ms":ts_ms,
        "uptime_s":uptime_s,
        "requests":{"total":metrics.requests.load(Ordering::Relaxed),
            "errors":metrics.failures.load(Ordering::Relaxed),
            "in_flight":metrics.in_flight.load(Ordering::Relaxed),
            "p50_ms":percentile_sorted(&all_durations, 0.50),
            "p95_ms":percentile_sorted(&all_durations, 0.95),
            "by_route":route_stats},
        "search":{"total":search_total,"p95_ms":percentile_sorted(&search_durations, 0.95),
            "mean_ms":search_mean,"by_strategy":strategies,
            "graph_nodes_visited":metrics.graph_nodes_visited.load(Ordering::Relaxed),
            "distance_evaluations":metrics.distance_evaluations.load(Ordering::Relaxed),
            "quantized_distance_evaluations":metrics.quantized_distance_evaluations.load(Ordering::Relaxed),
            "reranked_candidates":metrics.reranked.load(Ordering::Relaxed)},
        "logs":{"capacity":log_capacity,"dropped":log_dropped,"last_seq":last_seq}
    });
    service
        .run_read(move |db| {
            let file_bytes = db.file.0.metadata().ok().map(|metadata| metadata.len());
            let mut snapshot = snapshot;
            let vectors = db
                .collections
                .values()
                .map(|collection| collection.records.len())
                .sum::<usize>();
            snapshot["database"] = json!({"vectors":vectors,"collections":db.collections.len(),
            "file_bytes":file_bytes,"read_only":db.is_read_only()});
            Ok(Json(snapshot))
        })
        .await
}

fn percentile(samples: &[u64], quantile: f64) -> Option<u64> {
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    percentile_sorted(&sorted, quantile)
}

fn percentile_sorted(sorted: &[u64], quantile: f64) -> Option<u64> {
    if sorted.is_empty() {
        None
    } else {
        let index = ((sorted.len() as f64 * quantile).ceil() as usize).saturating_sub(1);
        sorted.get(index).copied()
    }
}

fn number(value: &Value) -> Option<f64> {
    match value {
        Value::Bool(flag) => Some(f64::from(u8::from(*flag))),
        Value::Number(n) => n.as_f64(),
        _ => None,
    }
}

fn escape(label: &str) -> String {
    label
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

fn sample(out: &mut String, name: &str, kind: &str, rows: &[(String, f64)]) {
    if rows.is_empty() {
        return;
    }
    let _ = writeln!(out, "# TYPE vlite_{name} {kind}");
    for (labels, value) in rows {
        let _ = writeln!(out, "vlite_{name}{labels} {value}");
    }
}

/// Prometheus text exposition 0.0.4. Scalar keys become `vlite_<key>`;
/// `*_total` keys are counters; per-collection and per-strategy series are
/// labelled. Nulls (unknown) are omitted rather than reported as zero.
pub(super) fn prometheus(value: &Value) -> String {
    let mut out = String::new();
    let Some(object) = value.as_object() else {
        return out;
    };
    for (key, item) in object {
        if let Some(n) = number(item) {
            let kind = if key.ends_with("_total") {
                "counter"
            } else {
                "gauge"
            };
            sample(&mut out, key, kind, &[(String::new(), n)]);
        }
    }
    if let Some(strategies) = object
        .get("searches_by_strategy")
        .and_then(Value::as_object)
    {
        let rows = strategies
            .iter()
            .filter_map(|(s, n)| Some((format!("{{strategy=\"{}\"}}", escape(s)), number(n)?)))
            .collect::<Vec<_>>();
        sample(&mut out, "searches_total", "counter", &rows);
    }
    let collections = object
        .get("collections")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut series = BTreeMap::<String, Vec<(String, f64)>>::new();
    for collection in &collections {
        let name = collection["name"].as_str().unwrap_or_default();
        let labels = format!("{{collection=\"{}\"}}", escape(name));
        for (key, item) in collection.as_object().into_iter().flatten() {
            if key == "id" {
                continue;
            }
            if let Some(n) = number(item) {
                series
                    .entry(format!("collection_{key}"))
                    .or_default()
                    .push((labels.clone(), n));
            }
        }
    }
    for (name, rows) in series {
        sample(&mut out, &name, "gauge", &rows);
    }
    out
}

/// `GET /v1/admin/diagnostics/slow-queries`, newest last. Entries hold only
/// field names and counters, never vectors, text or filter values.
pub(super) async fn slow_queries(
    State(service): State<Service>,
    Extension(role): Extension<admin::Role>,
) -> Result<Json<Value>, ApiError> {
    let items = service
        .metrics
        .slow
        .lock()
        .map_err(internal)?
        .iter()
        .filter(|item| visible(&role, item))
        .cloned()
        .collect::<Vec<_>>();
    let threshold = service.metrics.slow_threshold_ns.load(Ordering::Relaxed);
    Ok(Json(json!({"threshold_ms": threshold as f64 / 1e6,
        "capacity": SLOW_QUERY_CAPACITY, "items": items})))
}
