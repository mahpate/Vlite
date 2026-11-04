//! E1 erasure barrier: exact replay of every `erasure_golden.json` case
//! (`tests/memory_erasure_oracle.py`, never edited) plus dedicated checks.
//!
//! Accepted replay mapping (coordinator ruling on the ratified contract):
//!
//! * (a) Graph revisions and sequences are model-relative. The setup end maps
//!   to `setup.sequence` (default 1) and each scope node to its seed revision
//!   (default 1); every fresh begin, and every provision that creates a scope,
//!   is the next model revision. Harness writes and allowed ordinary writes
//!   keep the current model sequence, so order and equalities are preserved.
//!   A model `expected_scope_revision` v becomes `actual + v - model` (0 stays
//!   0). Literal bytes (`begin_hex`, `digest`, digest probes) come only from
//!   the pure encoder with the model values; the persisted job and receipt
//!   `begin_digest` is checked against the encoder at the actual revision.
//!   Entry, outcome and frame byte counts are measured from the real frame.
//! * (b) An ordinary receipt at a revision equal to the barrier revision is
//!   unreachable (the begin frame contains only E1 nodes); such a receipt is
//!   rewritten unchanged after the barrier, at a newer revision, and must give
//!   DeletionPending. The `>=` versus `>` revision comparison mutant is
//!   therefore equivalent: no reachable state has equal revisions.
//! * (c) `decoder-fault:<op>:<stage>` expectations are abstract
//!   differentials: the same operation and request run before any barrier on
//!   an identical setup (same receipt state) must return the same existing
//!   decoder error, and that error is never DeletionPending.
//!   `fixture:PreappendRefused` is the injected pre-append write failure
//!   (`Storage`), checked to append nothing and leave no job.
//! * (d) A corruption rewrites the job, outcome and receipt in one raw frame
//!   so their revisions stay equal; only the receipt-revision corruption
//!   rewrites the receipt alone.
//!
//! Ordinary receipts are made by real remember/forget/consolidate calls at
//! C=1 with window `expires - 1`; their flags are raw property faults
//! (scope, digest property, stored digest, outcome chunk, expiry). A request
//! `digest` other than "same" changes a hashed field of that operation.
//! "allowed" is a successful real operation (S1 status: success or
//! MissingMemory), i.e. barrier permission. Two rejection goldens whose clock
//! is not a `u64` (`clock-negative`, `clock-bool`) cannot be expressed by the
//! typed request and are skipped by name, never silently passed.

use std::{collections::BTreeMap, path::PathBuf};

use serde_json::{Value, json};

use super::super::{
    FAIL_NEXT_WRITE_BATCH, ForgetRequest, ForgetResult, ForgetTarget, LABEL_SCOPE, MemoryError,
    MemoryEvidence, MemoryItem, MemoryKind, MemoryMut, MemoryRef, MemoryScope, RememberRequest,
    RememberResult,
    consolidate::{ConsolidateInput, ConsolidateRequest, ConsolidateResult, ConsolidateSummary},
    evidence_record_id, forget_receipt_key, hex, memory_key, operation_receipt_key,
    ranking::RankingPolicy,
    recall::{RecallError, RecallLimits, RecallRequest, RecallSeed, Selection},
    receipt_key, receipt_outcome_key,
    stale::SummaryStatusRequest,
};
use super::{
    EraseScopeRequest, ErasureStatus, ErasureStatusRequest, begin_bytes, begin_digest,
    begin_receipt_key, job_key,
};
use crate::{
    Database, Expected, Metadata, MetadataValue, Mutation,
    hybrid::HybridQuery,
    property_graph::{
        GraphLimits, GraphOp, NodeView,
        engine::{GraphBody, GraphEntry, decode_body, encode_body},
    },
};

const GOLDEN: &str = include_str!("../../../tests/fixtures/agent_memory/erasure_golden.json");
const GRAPH: &str = "memory";
/// Goldens whose `now_ms` is not representable by the typed `u64` request.
const UNREPRESENTABLE: [&str; 2] = ["clock-negative", "clock-bool"];

fn golden() -> Value {
    let fixture: Value = serde_json::from_str(GOLDEN).unwrap();
    assert_eq!(fixture["schema"], "fixture.memory-erasure.v1");
    fixture
}

fn merge(base: &Value, update: &Value) -> Value {
    let mut value = base.clone();
    if let (Some(target), Some(update)) = (value.as_object_mut(), update.as_object()) {
        for (key, item) in update {
            let merged = match target.get(key) {
                Some(existing) if existing.is_object() && item.is_object() => merge(existing, item),
                _ => item.clone(),
            };
            target.insert(key.clone(), merged);
        }
        return value;
    }
    update.clone()
}

/// Fixture-only `{"$text", "count"}` input descriptors.
fn expand(value: &Value) -> Value {
    match value {
        Value::Object(map) if map.len() == 2 && map.contains_key("$text") => {
            let count = map["count"].as_u64().unwrap() as usize;
            Value::String(map["$text"].as_str().unwrap().repeat(count))
        }
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, item)| (key.clone(), expand(item)))
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(expand).collect()),
        other => other.clone(),
    }
}

/// The oracle's `cases()`: goldens, rejections, matrix and live corruptions.
fn cases(fixture: &Value) -> Vec<Value> {
    let e1 = &fixture["groups"]["E1"];
    let mut result = e1["cases"].as_array().unwrap().clone();
    for item in e1["rejections"].as_array().unwrap() {
        result.push(json!({
            "name": item["name"],
            "setup": item.get("setup").cloned().unwrap_or(json!({})),
            "steps": [{"request": item["request"], "expect": {"error": item["error"]}}],
        }));
    }
    for item in e1["matrix"].as_array().unwrap() {
        let Some(ops) = item.get("ops") else {
            result.push(item.clone());
            continue;
        };
        for op in ops.as_array().unwrap() {
            let mut value = item.clone();
            let name = format!(
                "{}-{}",
                item["name"].as_str().unwrap(),
                op.as_str().unwrap()
            );
            value["name"] = json!(name);
            value.as_object_mut().unwrap().remove("ops");
            value["steps"][1]["op"] = op.clone();
            result.push(value);
        }
    }
    for item in e1["corruptions"].as_array().unwrap() {
        result.push(json!({
            "name": format!("{}-live", item["name"].as_str().unwrap()),
            "steps": [
                {"expect": {"replayed": false}},
                {"request": {"now_ms": 11},
                 "corrupt": {"enabled": true, "node": item["node"], "change": item["change"]},
                 "expect": {"error": item.get("error").cloned().unwrap_or(json!("InvalidBundle"))}},
            ],
        }));
    }
    result
}

fn project(actual: &Value, expected: &Value) -> Value {
    match expected.as_object() {
        Some(expected) => Value::Object(
            expected
                .iter()
                .map(|(key, value)| {
                    let item = actual.get(key).unwrap_or(&Value::Null);
                    (key.clone(), project(item, value))
                })
                .collect(),
        ),
        None => actual.clone(),
    }
}

fn scope_of(value: &Value) -> MemoryScope {
    let part = |name: &str| value.get(name).and_then(Value::as_str).map(str::to_owned);
    MemoryScope {
        tenant: part("tenant").unwrap(),
        agent: part("agent").unwrap(),
        user: part("user"),
        session: part("session"),
    }
}

fn scope_json(scope: &MemoryScope) -> Value {
    let mut value = json!({"tenant": scope.tenant, "agent": scope.agent});
    if let Some(user) = &scope.user {
        value["user"] = json!(user);
    }
    if let Some(session) = &scope.session {
        value["session"] = json!(session);
    }
    value
}

fn name(error: &MemoryError) -> String {
    match error {
        MemoryError::Storage(_) => "Storage".into(),
        other => format!("{other:?}"),
    }
}

fn binding_of(value: Option<&Value>) -> Option<String> {
    match value {
        None => Some("c".into()),
        Some(Value::Null) => None,
        Some(value) => Some(value.as_str().unwrap().to_owned()),
    }
}

fn item(id: &str, content: &str) -> MemoryItem {
    MemoryItem {
        id: id.into(),
        kind: MemoryKind::Entity,
        content: content.into(),
        source: "source".into(),
        importance: Some(0.5),
        observed_at_ms: Some(0),
        provenance: Vec::new(),
        references: Vec::new(),
        merge_into: None,
        expected_revision: None,
        expiry_ms: None,
        expiry_valid: true,
        evidence: Vec::new(),
    }
}

/// One ordinary operation request. `changed` alters a hashed field.
struct Ordinary<'a> {
    scope: &'a MemoryScope,
    key: &'a str,
    now: u64,
    window: u64,
    deadline: Option<u64>,
    changed: bool,
    /// A pre-barrier ordinary receipt of this operation exists for the key.
    old: bool,
}

impl Ordinary<'_> {
    fn remember(&self) -> RememberRequest {
        let (id, content) = match (self.old, self.changed) {
            (true, false) => ("old-remember", "old content"),
            (true, true) => ("old-remember", "changed content"),
            (false, _) => ("gate", "gate content"),
        };
        RememberRequest {
            scope: self.scope.clone(),
            idempotency_key: self.key.into(),
            receipt_window_ms: self.window,
            now_ms: self.now,
            deadline_ms: self.deadline,
            allow_future_observations: false,
            items: vec![item(id, content)],
        }
    }

    fn forget(&self) -> ForgetRequest {
        ForgetRequest {
            scope: self.scope.clone(),
            idempotency_key: self.key.into(),
            // The forget digest includes the receipt window.
            receipt_window_ms: self.window + u64::from(self.changed),
            now_ms: self.now,
            deadline_ms: self.deadline,
            targets: vec![ForgetTarget {
                id: "f".into(),
                expected_revision: 1,
            }],
        }
    }

    fn consolidate(&self) -> ConsolidateRequest {
        ConsolidateRequest {
            scope: self.scope.clone(),
            idempotency_key: self.key.into(),
            now_ms: self.now,
            clock_valid: true,
            receipt_window_ms: self.window,
            deadline_ms: self.deadline,
            input_budget: None,
            inputs: vec![ConsolidateInput {
                id: "m".into(),
                expected_memory_revision: Some(1),
                retain: true,
            }],
            summary: ConsolidateSummary {
                id: "summary".into(),
                content: if self.changed { "changed" } else { "summary" }.into(),
                source: "s".into(),
                observed_at_ms: Some(1),
                importance: None,
                expiry_ms: None,
                expiry_valid: true,
                provenance: Vec::new(),
            },
            transformation: "summarize".into(),
            evidence: None,
            allow_future_observations: false,
        }
    }
}

fn remembered(result: &RememberResult) -> Value {
    json!({"receipt": result.receipt_revision, "actions": format!("{:?}", result.actions)})
}

fn forgotten(result: &ForgetResult) -> Value {
    json!({"receipt": result.receipt_revision, "deleted": result.deleted})
}

fn consolidated(result: &ConsolidateResult) -> Value {
    json!({"receipt": result.receipt_revision, "summary": result.summary_id,
           "digest": result.source_digest, "sources": result.source_count})
}

struct OldReceipt {
    op: String,
    scope: MemoryScope,
    key: String,
    window: u64,
    original: Value,
}

struct Harness {
    _dir: tempfile::TempDir,
    path: PathBuf,
    db: Option<Database>,
    base: Value,
    /// Model scopes by key: JSON scope and handle binding.
    scopes: BTreeMap<String, (Value, Option<String>)>,
    /// Scope key -> (actual, model) provisioning revision.
    scope_revisions: BTreeMap<String, (u64, u64)>,
    /// Harness-only scopes (a cross-scope ordinary receipt's own scope).
    extra: BTreeMap<String, Option<String>>,
    model_sequence: u64,
    /// Actual job/receipt revision -> model revision.
    revisions: BTreeMap<u64, u64>,
    old: Vec<OldReceipt>,
    /// Ordinary receipts (key, `old` index) to rewrite after the first
    /// barrier, rule (b).
    rewrites: Vec<(String, usize)>,
}

impl Harness {
    fn new(setup: &Value, base: &Value, reclaim: bool) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("e1.vlite");
        let mut db = Database::create(&path, 2).unwrap();
        db.create_graph(GRAPH).unwrap();
        db.create_collection(
            "c",
            crate::CollectionSpec {
                dimensions: 2,
                settings: crate::CollectionSettings::default(),
            },
        )
        .unwrap();
        let mut harness = Self {
            _dir: dir,
            path,
            db: Some(db),
            base: base.clone(),
            scopes: BTreeMap::new(),
            scope_revisions: BTreeMap::new(),
            extra: BTreeMap::new(),
            model_sequence: setup
                .get("sequence")
                .map_or(1, |value| value.as_u64().unwrap()),
            revisions: BTreeMap::new(),
            old: Vec::new(),
            rewrites: Vec::new(),
        };
        let seeds = setup
            .get("scopes")
            .cloned()
            .unwrap_or_else(|| json!([{"scope": base["scope"]}]));
        let seeds = seeds.as_array().unwrap();
        for seed in seeds {
            let scope = scope_of(&seed["scope"]);
            harness.provision(&scope);
            match seed.get("items") {
                // The oracle's literal ID catalog: recall must discover these.
                Some(items) => {
                    let ids: Vec<String> = items
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|id| id.as_str().unwrap().to_owned())
                        .collect();
                    harness.catalog_items(&scope, &ids);
                    harness.remember_items(&scope, &["f".into()]);
                }
                None => harness.remember_items(&scope, &["m".into(), "f".into()]),
            }
            harness.scopes.insert(
                scope.key(),
                (seed["scope"].clone(), binding_of(seed.get("collection"))),
            );
        }
        let begin_revision = harness.model_sequence + 1;
        for receipt in setup
            .get("ordinary_receipts")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            harness.ordinary_receipt(receipt, begin_revision);
        }
        for seed in seeds {
            if let Some(tuple) = seed.get("tuple") {
                let key = scope_of(&seed["scope"]).key();
                let mut properties = Metadata::new();
                properties.insert("tuple".into(), MetadataValue::String(tuple_str(tuple)));
                harness.raw(vec![GraphOp::UpsertNode {
                    key,
                    labels: vec![LABEL_SCOPE.into()],
                    properties,
                    expected: Expected::Any,
                }]);
            }
        }
        for seed in seeds {
            let key = scope_of(&seed["scope"]).key();
            let actual = harness.node(&key).unwrap().revision;
            let model = seed
                .get("revision")
                .map_or(1, |value| value.as_u64().unwrap());
            harness.scope_revisions.insert(key, (actual, model));
        }
        if reclaim {
            // One real opening reclaim in the next commit (C1 harness pattern).
            let due = crate::current_time_ms() + 60_000;
            harness.raw(vec![
                GraphOp::CreateNode {
                    key: "fixture-reclaim".into(),
                    labels: vec!["FixtureReclaim".into()],
                    properties: Metadata::new(),
                },
                GraphOp::SetNodeExpiry {
                    key: "fixture-reclaim".into(),
                    expires_at_ms: Some(due),
                    expected: Expected::Any,
                },
            ]);
            let db = harness.db_mut();
            db.graph_clock = db.graph_clock.max(due);
        }
        harness
    }

    fn db(&self) -> &Database {
        self.db.as_ref().unwrap()
    }

    fn db_mut(&mut self) -> &mut Database {
        self.db.as_mut().unwrap()
    }

    fn state(&self) -> (u64, u64) {
        (self.db().sequence, self.db().end)
    }

    fn handle(&mut self, binding: Option<String>) -> MemoryMut<'_> {
        MemoryMut {
            db: self.db.as_mut().unwrap(),
            graph: GRAPH.into(),
            evidence_collection: binding,
        }
    }

    fn reader(&self, binding: Option<String>) -> MemoryRef<'_> {
        MemoryRef {
            db: self.db(),
            graph: GRAPH.into(),
            evidence_collection: binding,
        }
    }

    fn binding(&self, scope: &MemoryScope) -> Option<String> {
        let key = scope.key();
        match (self.scopes.get(&key), self.extra.get(&key)) {
            (Some((_, binding)), _) | (None, Some(binding)) => binding.clone(),
            (None, None) => Some("c".into()),
        }
    }

    fn node(&self, key: &str) -> Option<NodeView> {
        self.db()
            .graph(GRAPH)
            .unwrap()
            .get_node(key)
            .unwrap()
            .output
    }

    fn raw(&mut self, ops: Vec<GraphOp>) {
        self.db_mut()
            .graph_mut(GRAPH)
            .unwrap()
            .write_batch(&ops)
            .unwrap();
    }

    fn provision(&mut self, scope: &MemoryScope) {
        self.handle(None).provision_scope(scope).unwrap();
    }

    /// Catalog items carry one evidence record (vector `[1, 0]`) in `c`, so
    /// the recall query discovers every catalog item of an unbarred scope.
    fn catalog_items(&mut self, scope: &MemoryScope, ids: &[String]) {
        let ops = ids
            .iter()
            .map(|id| GraphOp::Vector {
                collection: "c".into(),
                mutation: Mutation::PutWithMetadata {
                    id: evidence_record_id(id),
                    vector: vec![1.0, 0.0],
                    payload: id.as_bytes().to_vec(),
                    metadata: Metadata::new(),
                },
            })
            .collect();
        self.raw(ops);
        let request = RememberRequest {
            scope: scope.clone(),
            idempotency_key: "catalog".into(),
            receipt_window_ms: 99,
            now_ms: 1,
            deadline_ms: None,
            allow_future_observations: false,
            items: ids
                .iter()
                .map(|id| MemoryItem {
                    evidence: vec![MemoryEvidence {
                        id: id.clone(),
                        scope: scope.clone(),
                    }],
                    ..item(id, &format!("content {id}"))
                })
                .collect(),
        };
        self.handle(Some("c".into())).remember(&request).unwrap();
    }

    fn remember_items(&mut self, scope: &MemoryScope, ids: &[String]) {
        let request = RememberRequest {
            scope: scope.clone(),
            idempotency_key: "setup".into(),
            receipt_window_ms: 99,
            now_ms: 1,
            deadline_ms: None,
            allow_future_observations: false,
            items: ids
                .iter()
                .map(|id| item(id, &format!("content {id}")))
                .collect(),
        };
        self.handle(None).remember(&request).unwrap();
    }

    /// Creates one ordinary receipt by a real operation, then injects its
    /// raw faults.
    fn ordinary_receipt(&mut self, raw: &Value, begin_revision: u64) {
        let field = |name: &str| raw.get(name);
        let flag = |name: &str| field(name).is_some_and(|value| value.as_bool().unwrap());
        let op = field("op").unwrap().as_str().unwrap().to_owned();
        let base_scope = self.base["scope"].clone();
        let scope = scope_of(field("scope").unwrap_or(&base_scope));
        let key = field("key").map_or("old", |value| value.as_str().unwrap());
        let expires = field("expires").map_or(100, |value| value.as_u64().unwrap());
        let revision = field("revision").map_or(1, |value| value.as_u64().unwrap());
        let expiry_valid = field("expiry_valid").is_none_or(|value| value.as_bool().unwrap());
        let digest = field("digest").map_or("same", |value| value.as_str().unwrap());
        let scope_key = scope.key();
        if !self.scopes.contains_key(&scope_key) && !self.extra.contains_key(&scope_key) {
            self.provision(&scope);
            self.remember_items(&scope, &["m".into(), "f".into()]);
            self.extra.insert(scope_key.clone(), Some("c".into()));
        }
        let window = expires - 1;
        let ordinary = Ordinary {
            scope: &scope,
            key,
            now: 1,
            window,
            deadline: None,
            changed: false,
            old: true,
        };
        let binding = self.binding(&scope);
        let (original, receipt) = match op.as_str() {
            "remember" => (
                remembered(&self.handle(None).remember(&ordinary.remember()).unwrap()),
                receipt_key(&scope_key, key),
            ),
            "forget" => (
                forgotten(&self.handle(None).forget(&ordinary.forget()).unwrap()),
                forget_receipt_key(&scope_key, key),
            ),
            "consolidate" => (
                consolidated(
                    &self
                        .handle(binding)
                        .consolidate(&ordinary.consolidate())
                        .unwrap(),
                ),
                operation_receipt_key("consolidate", &scope_key, key),
            ),
            other => panic!("ordinary op {other}"),
        };
        let consolidate = op == "consolidate";
        let mut node = self.node(&receipt).unwrap();
        let mut faulted = false;
        if flag("malformed_scope") {
            let (name, value) = if consolidate {
                ("c1_scope_tuple", "00")
            } else {
                ("scope", "wrong")
            };
            node.properties
                .insert(name.into(), MetadataValue::String(value.into()));
            faulted = true;
        }
        if flag("digest_property") {
            node.properties.remove("digest");
            faulted = true;
        }
        if digest != "same" {
            node.properties.insert(
                "digest".into(),
                MetadataValue::String(format!("v1:{}", "0".repeat(64))),
            );
            faulted = true;
        }
        if !expiry_valid {
            node.properties.insert(
                "expires_at_ms".into(),
                MetadataValue::String("invalid".into()),
            );
            faulted = true;
        }
        let mut ops = Vec::new();
        if faulted {
            ops.push(upsert(&node));
        }
        if flag("malformed_outcome") {
            let mut outcome = self.node(&receipt_outcome_key(&receipt, 0)).unwrap();
            let (name, value) = if consolidate {
                ("outcome", MetadataValue::String("garbage".into()))
            } else {
                ("index", MetadataValue::Integer(7))
            };
            outcome.properties.insert(name.into(), value);
            ops.push(upsert(&outcome));
        }
        if !ops.is_empty() {
            self.raw(ops);
        }
        let mut original = original;
        if faulted {
            // A faulted receipt is rewritten; a replay reports its new revision.
            original["receipt"] = json!(self.node(&receipt).unwrap().revision);
        }
        if revision >= begin_revision {
            self.rewrites.push((receipt, self.old.len()));
        }
        self.old.push(OldReceipt {
            op,
            scope,
            key: key.into(),
            window,
            original,
        });
    }

    fn status_json(&self, status: &ErasureStatus) -> Value {
        json!({
            "job_key": status.job_key,
            "revision": self
                .revisions
                .get(&status.revision)
                .map_or_else(|| json!(format!("unmapped:{}", status.revision)), |model| json!(model)),
            "generation": status.generation,
            "state": format!("{:?}", status.state).to_lowercase(),
            "logical_visibility": format!("{:?}", status.logical_visibility).to_lowercase(),
            "deleted_count": status.deleted_count,
            "remaining": status.remaining,
            "resume_token": status.resume_token,
        })
    }

    fn execute(&mut self, step: &Value) -> Value {
        for change in step
            .get("edits")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            self.edit(change);
        }
        if let Some(corrupt) = step.get("corrupt")
            && corrupt["enabled"].as_bool().unwrap()
        {
            self.corrupt(corrupt);
        }
        let request = expand(&merge(
            &self.base,
            step.get("request").unwrap_or(&json!({})),
        ));
        let op = step.get("op").map_or("begin", |op| op.as_str().unwrap());
        let before = self.state();
        let result = match op {
            "begin" => self.begin(&request, step.get("fault") == Some(&json!(true))),
            "inspect" => Ok(self.inspect()),
            "reopen" => {
                let path = self.path.clone();
                drop(self.db.take());
                self.db = Some(Database::open(&path).unwrap());
                assert_eq!(self.state(), before, "reopen changed state");
                Ok(json!({"reopened": true}))
            }
            "recall" => self.recall(step, &request),
            other => self.gate(other, &request),
        };
        match result {
            Ok(value) => value,
            Err(error) => {
                assert_eq!(self.state(), before, "refusal {error} appended");
                json!({"error": error})
            }
        }
    }

    fn edit(&mut self, change: &Value) {
        let path: Vec<&str> = change["path"]
            .as_array()
            .unwrap()
            .iter()
            .map(|part| part.as_str().unwrap())
            .collect();
        match path.as_slice() {
            ["scopes"] => {
                assert_eq!(change["set"], json!({}));
                let ops = self
                    .scopes
                    .keys()
                    .map(|key| GraphOp::DeleteNode {
                        key: key.clone(),
                        expected: Expected::Any,
                    })
                    .collect();
                self.raw(ops);
                self.scopes.clear();
                self.scope_revisions.clear();
            }
            ["scopes", "$only", field] => {
                assert_eq!(self.scopes.len(), 1, "$only needs one entry");
                let key = self.scopes.keys().next().unwrap().clone();
                match *field {
                    "collection" => {
                        self.scopes.get_mut(&key).unwrap().1 = binding_of(Some(&change["set"]));
                    }
                    "tuple" => {
                        let mut properties = Metadata::new();
                        properties.insert(
                            "tuple".into(),
                            MetadataValue::String(tuple_str(&change["set"])),
                        );
                        self.raw(vec![GraphOp::UpsertNode {
                            key,
                            labels: vec![LABEL_SCOPE.into()],
                            properties,
                            expected: Expected::Any,
                        }]);
                    }
                    other => panic!("unsupported scope edit {other}"),
                }
            }
            other => panic!("unsupported edit {other:?}"),
        }
    }

    /// Rule (d): rewrite job, outcome and receipt in one frame.
    fn corrupt(&mut self, corrupt: &Value) {
        let scope_key = scope_of(&self.base["scope"]).key();
        let receipt = begin_receipt_key(&scope_key, self.base["idempotency_key"].as_str().unwrap());
        let keys = [
            ("job", job_key(&scope_key)),
            ("outcome", receipt_outcome_key(&receipt, 0)),
            ("receipt", receipt.clone()),
        ];
        let target = corrupt["node"].as_str().unwrap();
        let change = &corrupt["change"];
        let path: Vec<String> = change["path"]
            .as_array()
            .unwrap()
            .iter()
            .map(|part| match part {
                Value::String(part) => part.clone(),
                other => other.to_string(),
            })
            .collect();
        if path == ["revision"] {
            assert_eq!(target, "receipt");
            let node = self.node(&receipt).unwrap();
            self.raw(vec![upsert(&node)]);
            return;
        }
        let mut ops = Vec::new();
        if path == ["expires_at_ms"] {
            // A graph TTL on one node, in the same frame (rule d).
            let mut expiry = None;
            for (node_name, key) in keys {
                ops.push(upsert(&self.node(&key).unwrap()));
                if node_name == target {
                    expiry = Some(GraphOp::SetNodeExpiry {
                        key,
                        expires_at_ms: Some(change["set"].as_u64().unwrap()),
                        expected: Expected::Any,
                    });
                }
            }
            ops.push(expiry.expect("unknown TTL target"));
            self.raw(ops);
            return;
        }
        for (node_name, key) in keys {
            let mut node = self.node(&key).unwrap();
            if node_name == target {
                match (path[0].as_str(), path[1].as_str()) {
                    ("properties", property) => {
                        if change.get("delete").is_some() {
                            node.properties.remove(property);
                        } else {
                            node.properties
                                .insert(property.into(), metadata(&change["set"]));
                        }
                    }
                    ("labels", "0") => {
                        node.labels = [change["set"].as_str().unwrap().to_owned()]
                            .into_iter()
                            .collect();
                    }
                    other => panic!("unsupported corruption {other:?}"),
                }
            }
            ops.push(upsert(&node));
        }
        self.raw(ops);
    }

    fn begin(&mut self, request: &Value, fault: bool) -> Result<Value, String> {
        let scope = scope_of(&request["scope"]);
        let scope_key = scope.key();
        let key = request["idempotency_key"].as_str().unwrap().to_owned();
        let model_expected = request["expected_scope_revision"].as_u64().unwrap();
        let expected = match (model_expected, self.scope_revisions.get(&scope_key)) {
            (0, _) | (_, None) => model_expected,
            (value, Some(&(actual, model))) => {
                u64::try_from((i128::from(actual) + i128::from(value) - i128::from(model)).max(1))
                    .unwrap()
            }
        };
        let now = request["now_ms"].as_u64().unwrap();
        let window = request["receipt_window_ms"].as_u64().unwrap();
        let deadline = request.get("deadline_ms").and_then(Value::as_u64);
        let binding = self.binding(&scope);
        let erase = EraseScopeRequest {
            scope: scope.clone(),
            idempotency_key: key.clone(),
            expected_scope_revision: expected,
            now_ms: now,
            receipt_window_ms: window,
            deadline_ms: deadline,
        };
        let before = self.state();
        if fault {
            FAIL_NEXT_WRITE_BATCH.with(|fail| fail.set(true));
        }
        let result = self.handle(binding.clone()).begin_erasure(&erase);
        let unconsumed = FAIL_NEXT_WRITE_BATCH.with(|fail| fail.replace(false));
        let result = match result {
            Err(error) => {
                let error = name(&error);
                if fault && error == "Storage" {
                    assert!(!unconsumed, "fault injected past the write");
                    assert!(
                        self.node(&job_key(&scope_key)).is_none(),
                        "fault left a job"
                    );
                    return Err("fixture:PreappendRefused".into());
                }
                return Err(error);
            }
            Ok(result) => result,
        };
        if result.replayed {
            assert_eq!(self.state(), before, "replay appended");
            return Ok(json!({"status": self.status_json(&result.status), "replayed": true}));
        }
        self.model_sequence += 1;
        self.revisions
            .insert(result.status.revision, self.model_sequence);
        let after = self.state();

        // The persisted digest is the encoder at the actual revision; the
        // projected literal bytes use the model revision (rule a).
        let actual_digest =
            begin_digest(&begin_bytes(&scope, binding.as_deref(), expected, window));
        let job = self.node(&job_key(&scope_key)).unwrap();
        let receipt_key = begin_receipt_key(&scope_key, &key);
        let receipt = self.node(&receipt_key).unwrap();
        assert_eq!(text(&job, "begin_digest"), actual_digest);
        assert_eq!(text(&receipt, "digest"), actual_digest);
        assert_eq!(job.revision, result.status.revision);
        assert_eq!(receipt.revision, result.status.revision);
        let outcome_key = receipt_outcome_key(&receipt_key, 0);
        let outcome_node = self.node(&outcome_key).unwrap();
        assert_eq!(outcome_node.revision, result.status.revision);
        let outcome = text(&outcome_node, "outcome").to_owned();
        let bytes = begin_bytes(&scope, binding.as_deref(), model_expected, window);
        let (entry_bytes, reclaim_bytes, payload) = self.frame(before.1, after.1);

        let mut value = json!({
            "status": self.status_json(&result.status),
            "replayed": false,
            "begin_hex": hex(&bytes),
            "digest": begin_digest(&bytes),
            "receipt_key": receipt_key,
            "outcome_key": outcome_key,
            "outcome_bytes": outcome.len(),
            "outcome": outcome,
            "physical_ops": entry_bytes.len(),
            "entry_bytes": entry_bytes,
            "frame_bytes": payload - reclaim_bytes,
        });
        if reclaim_bytes > 0 {
            value["frame_bytes_with_reclaim"] = json!(payload);
        }
        for (rewrite, index) in std::mem::take(&mut self.rewrites) {
            let node = self.node(&rewrite).unwrap();
            self.raw(vec![upsert(&node)]);
            let revision = self.node(&rewrite).unwrap().revision;
            assert!(revision > result.status.revision);
            // A (wrongly) allowed replay would report the rewritten revision.
            self.old[index].original["receipt"] = json!(revision);
        }
        Ok(value)
    }

    /// Reads the one begin frame: per-entry bytes of the E1 entries, total
    /// reclaim entry bytes and the body payload length.
    fn frame(&self, start: u64, end: u64) -> (Vec<usize>, usize, usize) {
        // The database holds a byte-range lock on Windows; read through it.
        let mut file = self.db().file.try_clone().unwrap();
        let mut frame = vec![0; (end - start) as usize];
        std::io::Seek::seek(&mut file, std::io::SeekFrom::Start(start)).unwrap();
        std::io::Read::read_exact(&mut file, &mut frame).unwrap();
        assert_eq!(&frame[..4], b"VREC");
        assert_eq!(frame[4], 12, "a graph frame");
        let count = u32::from_le_bytes(frame[24..28].try_into().unwrap());
        let payload = u32::from_le_bytes(frame[28..32].try_into().unwrap()) as usize;
        assert_eq!(frame.len(), 36 + payload + 16, "one frame per commit");
        let body = decode_body(&frame[36..36 + payload], count).unwrap();
        let mut entries = Vec::new();
        let mut reclaim = 0;
        for entry in &body.entries {
            let single = GraphBody {
                graph_id: body.graph_id,
                entries: vec![entry.clone()],
            };
            let size = encode_body(&single).unwrap().len() - 4;
            match entry {
                GraphEntry::Reclaim { .. } => reclaim += size,
                GraphEntry::PutNode { .. } => entries.push(size),
                other => panic!("unexpected begin entry {other:?}"),
            }
        }
        (entries, reclaim, payload)
    }

    fn inspect(&self) -> Value {
        let scope_key = scope_of(&self.base["scope"]).key();
        let receipt = begin_receipt_key(&scope_key, self.base["idempotency_key"].as_str().unwrap());
        let mut nodes = serde_json::Map::new();
        for key in [
            job_key(&scope_key),
            receipt_outcome_key(&receipt, 0),
            receipt.clone(),
        ] {
            if self.node(&key).is_some() {
                nodes.insert(key, json!(true));
            }
        }
        let mut scopes = serde_json::Map::new();
        for (key, (scope, binding)) in &self.scopes {
            let node = self.node(key).unwrap();
            let items: Vec<&str> = ["m", "secret", "public"]
                .into_iter()
                .filter(|id| self.node(&memory_key(key, id)).is_some())
                .collect();
            scopes.insert(
                key.clone(),
                json!({
                    "scope": scope,
                    "tuple": text(&node, "tuple"),
                    "revision": self.scope_revisions[key].1,
                    "collection": binding,
                    "items": items,
                }),
            );
        }
        json!({"sequence": self.model_sequence, "nodes": nodes, "scopes": scopes})
    }

    fn recall(&mut self, step: &Value, request: &Value) -> Result<Value, String> {
        let scopes: Vec<MemoryScope> = step
            .get("scopes")
            .map(|scopes| scopes.as_array().unwrap().iter().map(scope_of).collect())
            .unwrap_or_else(|| vec![scope_of(&request["scope"])]);
        let seeds = step
            .get("seeds")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|seed| RecallSeed {
                scope: scope_of(&seed["scope"]),
                id: seed["id"].as_str().unwrap().to_owned(),
            })
            .collect();
        let recall = RecallRequest {
            scopes: scopes.clone(),
            kinds: vec![MemoryKind::Entity],
            filter: Vec::new(),
            query: Some(HybridQuery::new(Some(vec![1.0, 0.0]), "", 20)),
            seeds,
            as_of_ms: None,
            clock_ms: request["now_ms"].as_u64().unwrap(),
            policy: RankingPolicy::default(),
            selection: Selection::Ranked,
            limits: RecallLimits {
                graph: GraphLimits {
                    deadline_ms: 5_000,
                    ..GraphLimits::default()
                },
                ..RecallLimits::default()
            },
            expand: false,
            allow_partial: false,
            snippet_bytes: 512,
            cancel: None,
        };
        let output =
            self.reader(Some("c".into()))
                .recall(&recall)
                .map_err(|error| match error {
                    RecallError::Memory(error) => name(&error),
                    other => format!("{other:?}"),
                })?;
        // Whole-request preflight stays charged for barred scopes.
        let bound = self.db().graph(GRAPH).unwrap().memory_scan_bound();
        let collection = self.db().collection("c").unwrap().len();
        assert!(output.stats.source_records_charged >= bound * scopes.len() + collection);
        Ok(json!({
            "hits": output.hits.iter().map(|hit| json!({
                "scope": hit.ranking.candidate.scope.key(),
                "id": hit.ranking.candidate.id,
            })).collect::<Vec<_>>(),
            "deletion_barriers": output.deletion_barriers.iter().map(scope_json).collect::<Vec<_>>(),
            "partial": !output.complete,
            "preflight_scope_count": scopes.len(),
        }))
    }

    fn gate(&mut self, op: &str, request: &Value) -> Result<Value, String> {
        let scope = scope_of(&request["scope"]);
        let scope_key = scope.key();
        let key = request
            .get("key")
            .map_or("old", |key| key.as_str().unwrap())
            .to_owned();
        let now = request["now_ms"].as_u64().unwrap();
        let old = self
            .old
            .iter()
            .position(|old| old.op == op && old.scope == scope && old.key == key);
        let window = old.map_or(99, |index| self.old[index].window);
        let ordinary = Ordinary {
            scope: &scope,
            key: &key,
            now,
            window,
            deadline: request.get("deadline_ms").and_then(Value::as_u64),
            changed: request.get("digest").is_some_and(|digest| digest != "same"),
            old: old.is_some(),
        };
        let binding = self.binding(&scope);
        let before = self.state();
        let fingerprint = match op {
            "remember" => {
                let result = self
                    .handle(None)
                    .remember(&ordinary.remember())
                    .map_err(|error| name(&error))?;
                (remembered(&result), result.replayed)
            }
            "forget" => {
                let result = self
                    .handle(None)
                    .forget(&ordinary.forget())
                    .map_err(|error| name(&error))?;
                (forgotten(&result), result.replayed)
            }
            "consolidate" => {
                let result = self
                    .handle(binding)
                    .consolidate(&ordinary.consolidate())
                    .map_err(|error| name(&error))?;
                (consolidated(&result), result.replayed)
            }
            "summary_status" => {
                let status = self.reader(binding).summary_status(&SummaryStatusRequest {
                    scope: scope.clone(),
                    summary_id: "summary".into(),
                    now_ms: now,
                });
                assert_eq!(self.state(), before, "S1 status appended");
                return match status {
                    Ok(_) | Err(MemoryError::MissingMemory) => Ok(json!({"status": "allowed"})),
                    Err(error) => Err(name(&error)),
                };
            }
            "provision_scope" => {
                let created = self.node(&scope_key).is_none();
                self.handle(None)
                    .provision_scope(&scope)
                    .map_err(|error| name(&error))?;
                if created {
                    self.model_sequence += 1;
                    let actual = self.node(&scope_key).unwrap().revision;
                    self.scope_revisions
                        .insert(scope_key.clone(), (actual, self.model_sequence));
                    self.scopes
                        .insert(scope_key, (scope_json(&scope), Some("c".into())));
                } else {
                    assert_eq!(self.state(), before, "existing provision appended");
                }
                return Ok(json!({"status": "allowed"}));
            }
            "erasure_status" => {
                let status = self
                    .reader(binding)
                    .erasure_status(&ErasureStatusRequest { scope })
                    .map_err(|error| name(&error));
                assert_eq!(self.state(), before, "erasure status appended");
                return Ok(json!({"status": self.status_json(&status?)}));
            }
            "scope_revision" => {
                let actual = self
                    .reader(binding)
                    .scope_revision(&scope)
                    .map_err(|error| name(&error))?;
                assert_eq!(self.state(), before, "scope revision appended");
                let (mapped, model) = self.scope_revisions[&scope_key];
                assert_eq!(actual, mapped);
                return Ok(json!({"revision": model}));
            }
            other => panic!("unknown op {other}"),
        };
        let (fingerprint, replayed) = fingerprint;
        if !replayed {
            return Ok(json!({"status": "allowed"}));
        }
        let index = old.expect("replay without an ordinary receipt");
        assert_eq!(fingerprint, self.old[index].original, "replay differs");
        assert_eq!(self.state(), before, "replay appended");
        Ok(json!({"status": "replayed", "result": "original"}))
    }
}

fn text<'a>(node: &'a NodeView, name: &str) -> &'a str {
    match node.properties.get(name) {
        Some(MetadataValue::String(value)) => value,
        other => panic!("{name}: {other:?}"),
    }
}

fn tuple_str(value: &Value) -> String {
    value.as_str().unwrap().to_owned()
}

fn metadata(value: &Value) -> MetadataValue {
    match value {
        Value::String(value) => MetadataValue::String(value.clone()),
        Value::Bool(value) => MetadataValue::Boolean(*value),
        Value::Number(number) => MetadataValue::Integer(number.as_i64().unwrap()),
        other => panic!("unsupported corruption value {other}"),
    }
}

fn upsert(node: &NodeView) -> GraphOp {
    GraphOp::UpsertNode {
        key: node.key.clone(),
        labels: node.labels.iter().cloned().collect(),
        properties: node.properties.clone(),
        expected: Expected::Any,
    }
}

/// Runs one case; `decoder-fault:` steps are differential (rule c).
fn run_case(case: &Value, base: &Value) -> Vec<Value> {
    let label = case["name"].as_str().unwrap();
    eprintln!("case {label}");
    let setup = expand(case.get("setup").unwrap_or(&json!({})));
    let steps = case["steps"].as_array().unwrap();
    let reclaim = steps[0]["expect"].get("frame_bytes_with_reclaim").is_some();
    let mut harness = Harness::new(&setup, base, reclaim);
    let mut results = Vec::new();
    for (index, step) in steps.iter().enumerate() {
        let actual = harness.execute(step);
        let expect = &step["expect"];
        match expect.get("error").and_then(Value::as_str) {
            Some(fault) if fault.starts_with("decoder-fault:") => {
                let error = actual["error"]
                    .as_str()
                    .unwrap_or_else(|| panic!("{label} step {index}: got {actual}"));
                assert_ne!(error, "DeletionPending", "{label} step {index}");
                let mut twin = Harness::new(&setup, base, false);
                let unbarred = twin.execute(step);
                assert_eq!(
                    unbarred,
                    json!({"error": error}),
                    "{label} step {index}: {fault} differs before the barrier"
                );
            }
            _ => {
                if expect.get("nodes") == Some(&json!({})) {
                    assert_eq!(actual["nodes"], json!({}), "{label} step {index}");
                }
                assert_eq!(
                    project(&actual, expect),
                    *expect,
                    "{label} step {index}: got {actual}"
                );
            }
        }
        results.push(actual);
    }
    results
}

#[test]
fn every_erasure_golden_replays_exactly() {
    let fixture = golden();
    let base = &fixture["base_request"];
    let all = cases(&fixture);
    assert_eq!(all.len(), 88);
    let steps: usize = all
        .iter()
        .map(|case| case["steps"].as_array().unwrap().len())
        .sum();
    assert_eq!(steps, 182);
    let only = std::env::var("E1_CASE").ok();
    let mut skipped = Vec::new();
    let mut replayed = 0;
    for case in &all {
        let name = case["name"].as_str().unwrap();
        if UNREPRESENTABLE.contains(&name) {
            assert!(case["steps"][0]["request"]["now_ms"].as_u64().is_none());
            skipped.push(name);
            continue;
        }
        if only.as_deref().is_some_and(|only| only != name) {
            continue;
        }
        run_case(case, base);
        replayed += 1;
    }
    assert_eq!(skipped, UNREPRESENTABLE);
    if only.is_none() {
        assert_eq!(replayed, 86);
    } else {
        assert_eq!(replayed, 1, "E1_CASE names no replayable case");
    }
}

#[test]
fn begin_nodes_match_the_static_manifests() {
    let fixture = golden();
    let base = &fixture["base_request"];
    let mut harness = Harness::new(&json!({}), base, false);
    assert_eq!(harness.execute(&json!({}))["replayed"], false);
    let scope_key = scope_of(&base["scope"]).key();
    let receipt = begin_receipt_key(&scope_key, "k");
    let keys = [
        job_key(&scope_key),
        receipt_outcome_key(&receipt, 0),
        receipt,
    ];
    let manifests = fixture["groups"]["E1"]["node_manifests"]
        .as_array()
        .unwrap();
    // Rule (a): the literal digest is a pure encoder vector at model revision
    // 1; the stored digest is the checked encoder at the actual revision.
    let scope = scope_of(&base["scope"]);
    let model_digest = begin_digest(&begin_bytes(&scope, Some("c"), 1, 100));
    assert_eq!(json!(model_digest), manifests[0]["values"]["begin_digest"]);
    let (actual, model) = harness.scope_revisions[&scope_key];
    assert_eq!(model, 1);
    let actual_digest = json!(begin_digest(&begin_bytes(&scope, Some("c"), actual, 100)));
    for (key, manifest) in keys.iter().zip(manifests) {
        let node = harness.node(key).unwrap();
        assert_eq!(node.expires_at_ms, None, "no graph TTL on E1 nodes");
        let labels: Vec<&str> = node.labels.iter().map(String::as_str).collect();
        assert_eq!(json!(labels), manifest["labels"]);
        let mut types = serde_json::Map::new();
        for (name, value) in &node.properties {
            let kind = match value {
                MetadataValue::String(_) => "String",
                MetadataValue::Integer(_) => "Integer",
                other => panic!("{other:?}"),
            };
            types.insert(name.clone(), json!(kind));
        }
        assert_eq!(Value::Object(types), manifest["types"]);
        for (name, expected) in manifest["values"].as_object().unwrap() {
            let actual = match &node.properties[name] {
                MetadataValue::String(value) => json!(value),
                MetadataValue::Integer(value) => json!(value),
                other => panic!("{other:?}"),
            };
            let expected = if name == "begin_digest" || name == "digest" {
                &actual_digest
            } else {
                expected
            };
            assert_eq!(&actual, expected, "{key} {name}");
        }
    }
}

#[test]
fn digest_probes_change_exactly_their_hashed_fields() {
    let fixture = golden();
    let base = &fixture["base_request"];
    let vectors = |step: &Value| {
        let request = expand(&merge(base, step.get("request").unwrap_or(&json!({}))));
        let scope = scope_of(&request["scope"]);
        let collection = binding_of(step.get("collection"));
        let bytes = begin_bytes(
            &scope,
            collection.as_deref(),
            request["expected_scope_revision"].as_u64().unwrap(),
            request["receipt_window_ms"].as_u64().unwrap(),
        );
        json!({
            "digest": begin_digest(&bytes),
            "job_key": job_key(&scope.key()),
            "receipt_key": begin_receipt_key(
                &scope.key(), request["idempotency_key"].as_str().unwrap()),
        })
    };
    let probes = fixture["groups"]["E1"]["digest_probes"].as_array().unwrap();
    assert_eq!(probes.len(), 14);
    for probe in probes {
        let baseline = vectors(probe.get("baseline").unwrap_or(&json!({})));
        let actual = vectors(&probe["step"]);
        for field in ["digest", "job_key", "receipt_key"] {
            assert_eq!(
                actual[field] == baseline[field],
                probe["equal"][field].as_bool().unwrap(),
                "probe {} {field}",
                probe["name"]
            );
        }
    }
}

/// The oracle's explicit multi-defect precedence probes: 14 named plus one
/// expired-corruption probe per corruption. `E1_PROBE=<name>` runs one.
#[test]
fn explicit_precedence_probes_hold() {
    let fixture = golden();
    let base = &fixture["base_request"];
    let only = std::env::var("E1_PROBE").ok();
    let mut count = 0;
    let mut run = |name: &str, setup: Value, steps: Vec<Value>| {
        if only.as_deref().is_some_and(|only| only != name) {
            return;
        }
        let case = json!({"name": name, "setup": setup, "steps": steps});
        run_case(&case, base);
        count += 1;
    };
    let begin = || json!({"expect": {"replayed": false}});
    let error = |value: &str| json!({"error": value});
    for op in ["remember", "forget"] {
        run(
            &format!("digest-before-outcome-{op}"),
            json!({"ordinary_receipts": [{"op": op, "digest": "altered", "malformed_outcome": true}]}),
            vec![
                begin(),
                json!({"op": op, "expect": error("DeletionPending")}),
            ],
        );
    }
    run(
        "job-conflict-before-deadline",
        json!({}),
        vec![
            begin(),
            json!({"request": {"now_ms": 110, "deadline_ms": 0}, "expect": error("JobConflict")}),
        ],
    );
    run(
        "revision-after-deadline",
        json!({}),
        vec![
            json!({"request": {"deadline_ms": 0, "expected_scope_revision": 2},
                    "expect": error("DeadlineExceeded")}),
        ],
    );
    let missing = json!([{"path": ["scopes"], "set": {}}]);
    let collision = json!([{"path": ["scopes", "$only", "tuple"], "set": "00"}]);
    for (name, op, edits, expected) in [
        (
            "missing-scope-before-barrier",
            "remember",
            &missing,
            "InvalidScope",
        ),
        (
            "tuple-collision-before-barrier",
            "remember",
            &collision,
            "DigestCollision",
        ),
        (
            "begin-missing-scope-before-barrier",
            "begin",
            &missing,
            "InvalidScope",
        ),
        (
            "begin-tuple-collision-before-barrier",
            "begin",
            &collision,
            "DigestCollision",
        ),
        (
            "recall-missing-scope-before-barrier",
            "recall",
            &missing,
            "InvalidScope",
        ),
        (
            "recall-tuple-collision-before-barrier",
            "recall",
            &collision,
            "DigestCollision",
        ),
        (
            "provision-tuple-collision-before-barrier",
            "provision_scope",
            &collision,
            "DigestCollision",
        ),
        // An orphan job never lets provision recreate its scope.
        (
            "provision-missing-scope-under-barrier",
            "provision_scope",
            &missing,
            "InvalidBundle",
        ),
        (
            "summary-status-missing-scope-before-barrier",
            "summary_status",
            &missing,
            "InvalidScope",
        ),
        (
            "summary-status-tuple-collision-before-barrier",
            "summary_status",
            &collision,
            "DigestCollision",
        ),
    ] {
        run(
            name,
            json!({}),
            vec![
                begin(),
                json!({"op": op, "edits": edits, "expect": error(expected)}),
            ],
        );
    }
    run(
        "recall-barred-seed-after-scope-loop",
        json!({}),
        vec![
            begin(),
            json!({"op": "recall",
                   "scopes": [base["scope"], {"tenant": "missing", "agent": "a"}],
                   "seeds": [{"scope": base["scope"], "id": "secret"}],
                   "expect": error("InvalidScope")}),
        ],
    );
    for op in ["remember", "forget", "consolidate"] {
        let expected = if op == "consolidate" {
            "decoder-fault:consolidate:scope"
        } else {
            "DeletionPending"
        };
        run(
            &format!("expired-malformed-{op}"),
            json!({"ordinary_receipts": [{"op": op, "expires": 10, "malformed_scope": true}]}),
            vec![
                begin(),
                json!({"op": op, "request": {"now_ms": 10}, "expect": error(expected)}),
            ],
        );
    }
    for corruption in fixture["groups"]["E1"]["corruptions"].as_array().unwrap() {
        let name = corruption["name"].as_str().unwrap();
        let expected = corruption
            .get("error")
            .cloned()
            .unwrap_or(json!("InvalidBundle"));
        run(
            name,
            json!({}),
            vec![
                begin(),
                json!({"request": {"now_ms": 110},
                       "corrupt": {"enabled": true, "node": corruption["node"],
                                   "change": corruption["change"]},
                       "expect": {"error": expected}}),
            ],
        );
    }
    if only.is_none() {
        assert_eq!(count, 38);
        // Without corruption the expired valid receipt still keeps the job.
        run_case(
            &json!({"name": "expired-valid", "steps": [
                begin(), {"request": {"now_ms": 110}, "expect": error("JobConflict")}]}),
            base,
        );
    } else {
        assert_eq!(count, 1, "E1_PROBE names no probe");
    }
}

#[test]
fn live_corruption_probes_beyond_the_golden() {
    let fixture = golden();
    let base = &fixture["base_request"];
    // Far beyond the wall clock: the TTL never reclaims during the test, and
    // the live read at C=11 still sees the node.
    let ttl = json!(4_000_000_000_000u64);
    let probes = [
        (
            "job-visibility",
            "job",
            json!(["properties", "logical_visibility"]),
            json!("visible"),
        ),
        (
            "job-deleted-count",
            "job",
            json!(["properties", "deleted_count"]),
            json!("1"),
        ),
        (
            "job-schema",
            "job",
            json!(["properties", "e1_schema"]),
            json!("erase-scope:v2"),
        ),
        (
            "receipt-schema",
            "receipt",
            json!(["properties", "e1_schema"]),
            json!("erase-scope:v2"),
        ),
        (
            "receipt-chunks-two",
            "receipt",
            json!(["properties", "outcome_chunks"]),
            json!(2),
        ),
        (
            "outcome-receipt-key",
            "outcome",
            json!(["properties", "receipt_key"]),
            json!("wrong"),
        ),
        ("job-ttl", "job", json!(["expires_at_ms"]), ttl.clone()),
        (
            "receipt-ttl",
            "receipt",
            json!(["expires_at_ms"]),
            ttl.clone(),
        ),
        ("outcome-ttl", "outcome", json!(["expires_at_ms"]), ttl),
    ];
    for (name, node, path, set) in probes {
        for (suffix, now) in [("live", 11), ("expired", 110)] {
            let case = json!({"name": format!("{name}-{suffix}"), "steps": [
                {"expect": {"replayed": false}},
                {"request": {"now_ms": now},
                 "corrupt": {"enabled": true, "node": node,
                             "change": {"path": path, "set": set}},
                 "expect": {"error": "InvalidBundle"}},
            ]});
            run_case(&case, base);
        }
    }
}

#[test]
fn recall_keeps_unbarred_hits_and_reports_barriers_in_request_order() {
    let fixture = golden();
    let case = json!({
        "name": "unbarred-hits",
        "setup": {"scopes": [{"scope": {"tenant": "t", "agent": "a"}, "items": ["secret"]},
                             {"scope": {"tenant": "other", "agent": "a"}, "items": ["public"]}]},
        "steps": [
            {"expect": {"replayed": false}},
            {"op": "recall",
             "scopes": [{"tenant": "other", "agent": "a"}, {"tenant": "t", "agent": "a"}],
             "seeds": [{"scope": {"tenant": "other", "agent": "a"}, "id": "public"}],
             "expect": {"partial": false, "deletion_barriers": [{"tenant": "t", "agent": "a"}]}},
        ],
    });
    let results = run_case(&case, &fixture["base_request"]);
    let other = scope_of(&json!({"tenant": "other", "agent": "a"})).key();
    let hits = results[1]["hits"].as_array().unwrap();
    assert!(
        hits.contains(&json!({"scope": other, "id": "public"})),
        "{hits:?}"
    );
    assert!(
        hits.iter().all(|hit| hit["scope"] == json!(other)),
        "{hits:?}"
    );
}

#[test]
fn barrier_survives_reopen_on_all_backends() {
    let dir = tempfile::tempdir().unwrap();
    let encryption_key = crate::EncryptionKey::from_bytes([9; 32]);
    let scope = MemoryScope {
        tenant: "t".into(),
        agent: "a".into(),
        user: None,
        session: None,
    };
    for (label, directory, encrypted) in [
        ("plain.vlite", false, false),
        ("encrypted.vlite", false, true),
        ("plain-directory", true, false),
        ("encrypted-directory", true, true),
    ] {
        let path = dir.path().join(label);
        let options = crate::CreateOptions {
            encryption: encrypted.then(|| encryption_key.clone()),
            ..crate::CreateOptions::default()
        };
        let mut db = if directory {
            Database::create_directory(&path, 2, options).unwrap()
        } else {
            Database::create_with_options(&path, 2, options).unwrap()
        };
        db.create_graph(GRAPH).unwrap();
        MemoryMut::bind(&mut db, GRAPH, None)
            .unwrap()
            .provision_scope(&scope)
            .unwrap();
        let request = EraseScopeRequest {
            scope: scope.clone(),
            idempotency_key: "k".into(),
            expected_scope_revision: MemoryRef::bind(&db, GRAPH, None)
                .unwrap()
                .scope_revision(&scope)
                .unwrap(),
            now_ms: crate::current_time_ms() + 30_000,
            receipt_window_ms: 60_000,
            deadline_ms: None,
        };
        let begun = MemoryMut::bind(&mut db, GRAPH, None)
            .unwrap()
            .begin_erasure(&request)
            .unwrap();
        assert!(!begun.replayed, "{label}");
        drop(db);

        let mut db = if encrypted {
            Database::open_encrypted(&path, &encryption_key).unwrap()
        } else {
            Database::open(&path).unwrap()
        };
        let before = db.sequence;
        let status = MemoryRef::bind(&db, GRAPH, None)
            .unwrap()
            .erasure_status(&ErasureStatusRequest {
                scope: scope.clone(),
            })
            .unwrap();
        assert_eq!(status, begun.status, "{label}");
        let replay = MemoryMut::bind(&mut db, GRAPH, None)
            .unwrap()
            .begin_erasure(&request)
            .unwrap();
        assert!(replay.replayed, "{label}");
        assert_eq!(replay.status, begun.status, "{label}");
        let ordinary = Ordinary {
            scope: &scope,
            key: "write",
            now: request.now_ms,
            window: 99,
            deadline: None,
            changed: false,
            old: false,
        };
        let refused = MemoryMut::bind(&mut db, GRAPH, None)
            .unwrap()
            .remember(&ordinary.remember());
        assert!(
            matches!(refused, Err(MemoryError::DeletionPending)),
            "{label}"
        );
        let refused = MemoryMut::bind(&mut db, GRAPH, None)
            .unwrap()
            .provision_scope(&scope);
        assert!(
            matches!(refused, Err(MemoryError::DeletionPending)),
            "{label}"
        );
        assert_eq!(db.sequence, before, "{label}");
    }
}
