//! design-tenant-provisioning EPIC-001 (ITEM-001..003): offline proofs over
//! the existing fake `ObjectStore`. No cloud dispatch happens here.
use super::*;
use crate::server::cloud_backup::fake::{Fault, Memory, Op};
use crate::server::control_state::{Edit, Route, Slot};
use crate::server::{credentials, tenants};
use serde_json::{Value, json};
use std::sync::Arc;

const CONTROL: &str = "control/state.json";
const PREFIX: &str = "provisioning";
const ADMIN: &str = "admin-aaaaaaaa";
const KEY: &str = "onboard-contoso-2026-11-22";
const T: &str = "0123456789abcdef0123456789abcdef";
const OTHER: &str = "fedcba9876543210fedcba9876543210";
const NOW: u64 = 1_700_000_000_000;
const PROFILE: &[u8] = br#"{"image":"vlite@sha256:0","cpu":"approved-by-profile"}"#;
const INIT: &[u8] = br#"{"dimension":"from-approved-profile"}"#;
const APP: &str = "/subscriptions/s/resourceGroups/g/providers/x/apps/w";

fn sha(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

fn create_payload(name: &str) -> Payload {
    Payload::Create {
        name: name.into(),
        max_vectors: Some(1000),
        writer_profile: sha(PROFILE),
        initialization: sha(INIT),
        resources: vec![APP.into()],
        secret_versions: vec!["archive-sas@3".into()],
    }
}

fn create_with(name: &str, profile: &str) -> Payload {
    let mut payload = create_payload(name);
    let Payload::Create { writer_profile, .. } = &mut payload else {
        unreachable!()
    };
    *writer_profile = profile.into();
    payload
}

fn contents() -> Vec<Vec<u8>> {
    vec![PROFILE.to_vec(), INIT.to_vec()]
}

struct World {
    memory: Arc<Memory>,
    ops: Operations,
    objects: Objects,
    credentials: credentials::Store,
    tenants: tenants::Store,
    tokens: Vec<String>,
}

/// Control state with tenants `T` and `OTHER`; `OTHER` is routed shared.
fn world() -> World {
    let (credentials, tokens) = credentials::tests::issued(
        &[("aaaaaaaaaaaa", Some(T)), ("bbbbbbbbbbbb", Some(OTHER))],
        0x77,
    );
    let tenants = tenants::Store::for_test(vec![
        tenants::Tenant::for_test(T, None),
        tenants::Tenant::for_test(OTHER, None),
    ]);
    let memory = Arc::new(Memory::default());
    let objects = Objects::new(memory.clone(), CONTROL);
    objects
        .create(
            Snapshot::import(&credentials, Some(&tenants))
                .unwrap()
                .with_directory(vec![Route::shared_for_test(OTHER)]),
            NOW,
        )
        .unwrap();
    World {
        ops: Operations::new(memory.clone(), PREFIX),
        objects,
        memory,
        credentials,
        tenants,
        tokens,
    }
}

impl World {
    fn accept(&self, key: &str, payload: &Payload) -> Result<Accepted, ApiError> {
        self.ops.accept(ADMIN, key, payload, &contents(), NOW)
    }

    /// A `create` intent retargeted at the existing tenant `T`: it stands in
    /// for EPIC-002's adapter, which creates tenant metadata from the
    /// persisted ID (out of EPIC-001 scope).
    fn intent_for_t(&self, key: &str) -> Intent {
        let mut intent = self.accept(key, &create_payload("contoso")).unwrap().intent;
        intent.tenant_id = T.into();
        intent
    }

    fn snapshot(&self) -> Snapshot {
        self.objects.read().unwrap().0
    }

    fn intent_raw(&self, operation_id: &str) -> Vec<u8> {
        self.memory
            .raw(&format!("{PREFIX}/operations/{operation_id}/intent.json"))
            .unwrap()
    }
}

fn writer_route(tenant: &str) -> Route {
    Route {
        reader: None,
        ..Route::dedicated_for_test(tenant, 1)
    }
}

fn prefixed(memory: &Memory) -> Vec<String> {
    memory
        .names()
        .into_iter()
        .filter(|name| name.starts_with(PREFIX))
        .collect()
}

// ------------------------------------------------------------- ITEM-001

#[test]
fn identical_retry_returns_the_same_operation_and_tenant() {
    let w = world();
    let first = w.accept(KEY, &create_payload("contoso")).unwrap();
    assert!(!first.duplicate);
    let again = w.accept(KEY, &create_payload("contoso")).unwrap();
    assert!(again.duplicate);
    assert_eq!(again.intent, first.intent);
    assert_eq!(first.intent.tenant_id.len(), 32);
    assert_eq!(first.intent.kind(), Kind::Create);
    let intents = prefixed(&w.memory)
        .into_iter()
        .filter(|name| name.ends_with("intent.json"))
        .count();
    assert_eq!(intents, 1);
}

#[test]
fn conflicting_retry_is_refused_and_changes_nothing() {
    let w = world();
    let first = w.accept(KEY, &create_payload("contoso")).unwrap();
    let before = w.memory.names();
    let raw = w.intent_raw(&first.intent.operation_id);
    let error = w.accept(KEY, &create_payload("fabrikam")).unwrap_err();
    assert_eq!(error.status, StatusCode::CONFLICT);
    assert_eq!(error.code, "operation_request_conflict");
    assert_eq!(w.memory.names(), before);
    assert_eq!(w.intent_raw(&first.intent.operation_id), raw);
}

#[test]
fn key_is_bound_per_principal_and_never_stored_in_plaintext() {
    let w = world();
    let mine = w.accept(KEY, &create_payload("contoso")).unwrap().intent;
    let theirs = w
        .ops
        .accept(
            "admin-bbbbbbbb",
            KEY,
            &create_payload("fabrikam"),
            &contents(),
            NOW,
        )
        .unwrap()
        .intent;
    assert_ne!(mine.operation_id, theirs.operation_id);
    assert_ne!(mine.tenant_id, theirs.tenant_id);
    let raw = String::from_utf8(w.intent_raw(&mine.operation_id)).unwrap();
    assert!(!raw.contains(KEY));
    assert!(raw.contains(&mine.idempotency_key_digest));
}

#[test]
fn concurrent_same_key_requests_allocate_one_tenant() {
    let w = world();
    let racer = Operations::new(w.memory.clone(), PREFIX);
    let won = Arc::new(std::sync::Mutex::new(None));
    let slot = won.clone();
    w.memory.hook(Op::Put, "intent.json", move || {
        let accepted = racer
            .accept(ADMIN, KEY, &create_payload("contoso"), &contents(), NOW)
            .unwrap();
        *slot.lock().unwrap() = Some(accepted.intent);
    });
    let lost = w.accept(KEY, &create_payload("contoso")).unwrap();
    let winner = won.lock().unwrap().clone().unwrap();
    assert!(lost.duplicate);
    assert_eq!(lost.intent, winner);
}

#[test]
fn concurrent_conflicting_request_loses_with_conflict() {
    let w = world();
    let racer = Operations::new(w.memory.clone(), PREFIX);
    w.memory.hook(Op::Put, "intent.json", move || {
        racer
            .accept(ADMIN, KEY, &create_payload("fabrikam"), &contents(), NOW)
            .unwrap();
    });
    let error = w.accept(KEY, &create_payload("contoso")).unwrap_err();
    assert_eq!(error.code, "operation_request_conflict");
}

#[test]
fn ambiguous_intent_write_is_resolved_by_readback_not_blind_retry() {
    // Applied but unacknowledged: the readback is the persisted intent.
    let w = world();
    w.memory.fault(Op::Put, "intent.json", Fault::After, 0, 1);
    let applied = w.accept(KEY, &create_payload("contoso")).unwrap();
    assert!(!applied.duplicate);
    assert_eq!(
        w.accept(KEY, &create_payload("contoso")).unwrap().intent,
        applied.intent
    );

    // Not applied: reported as indeterminate; a same-key retry is safe.
    let w = world();
    w.memory.fault(Op::Put, "intent.json", Fault::Before, 0, 1);
    let error = w.accept(KEY, &create_payload("contoso")).unwrap_err();
    assert_eq!(error.code, "operation_indeterminate");
    let retried = w.accept(KEY, &create_payload("contoso")).unwrap();
    assert!(!retried.duplicate);
}

#[test]
fn recovery_needs_only_the_operation_id_and_retained_content() {
    let w = world();
    let accepted = w.accept(KEY, &create_payload("contoso")).unwrap().intent;
    // A fresh process with no original request and no profile catalog.
    let restarted = Operations::new(w.memory.clone(), PREFIX);
    let recovered = restarted.load(&accepted.operation_id).unwrap();
    assert_eq!(recovered.intent, accepted);
    assert_eq!(recovered.contents[&sha(PROFILE)], PROFILE);
    assert_eq!(recovered.contents[&sha(INIT)], INIT);
    assert_eq!(
        restarted.load(&"0".repeat(32)).unwrap_err().code,
        "operation_not_found"
    );
}

#[test]
fn missing_or_corrupt_retained_input_blocks_recovery() {
    for corrupt in [false, true] {
        let w = world();
        let id = w
            .accept(KEY, &create_payload("contoso"))
            .unwrap()
            .intent
            .operation_id;
        let name = format!("{PREFIX}/contents/{}", sha(PROFILE));
        if corrupt {
            w.memory.set_raw(&name, b"tampered");
        } else {
            w.memory.remove(&name);
        }
        let error = w.ops.load(&id).unwrap_err();
        assert_eq!(
            error.code, "operation_input_unavailable",
            "corrupt={corrupt}"
        );
    }
}

#[test]
fn referenced_content_must_be_supplied_exactly() {
    let w = world();
    let missing = w
        .ops
        .accept(
            ADMIN,
            KEY,
            &create_payload("contoso"),
            &[PROFILE.to_vec()],
            NOW,
        )
        .unwrap_err();
    assert_eq!(missing.status, StatusCode::UNPROCESSABLE_ENTITY);
    let mut extra = contents();
    extra.push(b"unreferenced".to_vec());
    let extra = w
        .ops
        .accept(ADMIN, KEY, &create_payload("contoso"), &extra, NOW)
        .unwrap_err();
    assert_eq!(extra.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(prefixed(&w.memory).is_empty());
}

#[test]
fn oversized_request_content_is_refused_before_any_write() {
    let w = world();
    let big = vec![b'x'; MAX_BYTES as usize + 1];
    let payload = create_with("contoso", &sha(&big));
    let error = w
        .ops
        .accept(ADMIN, KEY, &payload, &[big, INIT.to_vec()], NOW)
        .unwrap_err();
    assert_eq!(error.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(prefixed(&w.memory).is_empty());
}

#[test]
fn payloads_are_strict_and_tagged() {
    let good = json!({"kind": "set_readers", "tenant_id": T, "fleet": {"state": "absent"}});
    assert!(serde_json::from_value::<Payload>(good).is_ok());
    let rejected = [
        json!({"kind": "set_readers", "tenant_id": T, "fleet": {"state": "absent", "min_replicas": 1}}),
        json!({"kind": "set_readers", "tenant_id": T, "fleet": {"state": "present"}}),
        json!({"kind": "set_readers", "tenant_id": T, "fleet": {"state": "absent"}, "extra": 1}),
        json!({"kind": "retire", "tenant_id": T, "expected_receipt": sha(b"r"), "reason": "x", "data_disposition": "delete"}),
        json!({"kind": "launch"}),
    ];
    for raw in rejected {
        assert!(
            serde_json::from_value::<Payload>(raw.clone()).is_err(),
            "{raw}"
        );
    }
    let invalid = [
        Payload::SetReaders {
            tenant_id: T.into(),
            fleet: Fleet::Present {
                min_replicas: 3,
                max_replicas: 2,
                reader_profile: sha(PROFILE),
                scaling_rule: sha(INIT),
                read_routing: control_state::ReadRouting::Readers,
            },
        },
        Payload::SetReaders {
            tenant_id: "not-a-tenant".into(),
            fleet: Fleet::Absent {},
        },
        create_payload(""),
        create_with("x", "not-a-digest"),
    ];
    for payload in invalid {
        assert!(payload.validate().is_err(), "{payload:?}");
    }
}

#[test]
fn stored_intent_with_unknown_fields_is_refused_on_recovery() {
    let w = world();
    let id = w
        .accept(KEY, &create_payload("contoso"))
        .unwrap()
        .intent
        .operation_id;
    let name = format!("{PREFIX}/operations/{id}/intent.json");
    let mut raw: Value = serde_json::from_slice(&w.memory.raw(&name).unwrap()).unwrap();
    raw["plaintext_token"] = json!("vlk_secret");
    w.memory.set_raw(&name, &serde_json::to_vec(&raw).unwrap());
    assert_eq!(w.ops.load(&id).unwrap_err().code, "operation_state_invalid");
}

// ------------------------------------------------------------- ITEM-002

#[test]
fn legacy_snapshot_bytes_are_unchanged_without_lifecycle() {
    let w = world();
    let raw = String::from_utf8(w.memory.raw(CONTROL).unwrap()).unwrap();
    assert!(!raw.contains("lifecycle"));
    let after = w
        .objects
        .apply(
            NOW + 1,
            &Edit::Reimport(&w.credentials, &w.tenants),
            None,
            None,
        )
        .unwrap();
    assert!(after.lifecycle(T).is_none());
    let raw = String::from_utf8(w.memory.raw(CONTROL).unwrap()).unwrap();
    assert!(!raw.contains("lifecycle"));
}

#[test]
fn old_consumers_refuse_snapshots_with_lifecycle_state() {
    // The field set of a pre-lifecycle control-state consumer (strict).
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    #[allow(dead_code)]
    struct Legacy {
        schema: String,
        generation: u64,
        written_unix_ms: u64,
        tenants: Value,
        credentials: Value,
        #[serde(default)]
        directory: Value,
        #[serde(default)]
        migrations: Value,
    }
    let w = world();
    assert!(serde_json::from_slice::<Legacy>(&w.memory.raw(CONTROL).unwrap()).is_ok());
    reserve(&w.ops, &w.objects, &w.intent_for_t(KEY), NOW).unwrap();
    assert!(serde_json::from_slice::<Legacy>(&w.memory.raw(CONTROL).unwrap()).is_err());
}

#[test]
fn reservation_serializes_operations_per_tenant() {
    let w = world();
    let first = w.intent_for_t(KEY);
    let second = w.intent_for_t("another-key");
    reserve(&w.ops, &w.objects, &first, NOW).unwrap();
    // Idempotent for the holder: no new generation.
    let generation = w.snapshot().generation();
    reserve(&w.ops, &w.objects, &first, NOW).unwrap();
    assert_eq!(w.snapshot().generation(), generation);
    assert_eq!(
        reserve(&w.ops, &w.objects, &second, NOW).unwrap_err().code,
        "lifecycle_reserved"
    );
    // An unrelated tenant's change advances `A` and keeps the slot.
    w.objects
        .apply(NOW, &Edit::Suspend(OTHER.into(), true), None, None)
        .unwrap();
    let entry = w.snapshot().lifecycle(T).cloned().unwrap();
    assert!(
        matches!(entry.slot, Slot::Reserved { ref operation_id, .. } if *operation_id == first.operation_id)
    );
    assert_eq!(entry.generation, 1);
    // A non-create needs a routed tenant; a create needs an unrouted one.
    let mut routed_create = w.intent_for_t("routed");
    routed_create.tenant_id = OTHER.into();
    assert_eq!(
        reserve(&w.ops, &w.objects, &routed_create, NOW)
            .unwrap_err()
            .code,
        "lifecycle_transition_unavailable"
    );
}

#[test]
fn activation_publishes_route_and_receipt_atomically_and_keeps_restrictions() {
    let w = world();
    let intent = w.intent_for_t(KEY);
    reserve(&w.ops, &w.objects, &intent, NOW).unwrap();
    assert!(
        w.snapshot().record_for(T).is_none(),
        "a reservation is not routable"
    );
    w.objects
        .apply(NOW, &Edit::Revoke("aaaaaaaaaaaa".into()), None, None)
        .unwrap();
    w.objects
        .apply(NOW, &Edit::Suspend(T.into(), true), None, None)
        .unwrap();
    let route = writer_route(T);
    let receipt = activate(&w.ops, &w.objects, &intent, &route, &sha(b"proof"), NOW).unwrap();
    let snapshot = w.snapshot();
    assert_eq!(snapshot.record_for(T), Some(&route));
    assert_eq!(receipt.generation, snapshot.generation());
    assert_eq!(receipt.route_digest, control_state::route_digest(&route));
    assert_eq!(receipt.epoch, 1);
    assert_eq!(snapshot.admit(T).unwrap_err().code, "tenant_suspended");
    assert!(snapshot.authenticate(&w.tokens[0], NOW).is_err());
    assert!(snapshot.authenticate(&w.tokens[1], NOW).is_ok());
}

#[test]
fn activation_refuses_unsafe_routes() {
    let w = world();
    let intent = w.intent_for_t(KEY);
    reserve(&w.ops, &w.objects, &intent, NOW).unwrap();
    let mut authority_off = writer_route(T);
    authority_off.epoch = None;
    authority_off.authority_mode = Some(control_state::AuthorityMode::Off);
    let with_reader = Route::dedicated_for_test(T, 1);
    let shared = Route::shared_for_test(T);
    for route in [authority_off, with_reader, shared, writer_route(OTHER)] {
        assert!(activate(&w.ops, &w.objects, &intent, &route, &sha(b"p"), NOW).is_err());
    }
    assert!(
        activate(
            &w.ops,
            &w.objects,
            &intent,
            &writer_route(T),
            "not-hex",
            NOW
        )
        .is_err()
    );
    assert!(w.snapshot().record_for(T).is_none());
    // Only the reserved operation can activate.
    let stranger = w.intent_for_t("stranger");
    assert_eq!(
        activate(
            &w.ops,
            &w.objects,
            &stranger,
            &writer_route(T),
            &sha(b"p"),
            NOW
        )
        .unwrap_err()
        .code,
        "lifecycle_not_reserved"
    );
}

#[test]
fn lost_activation_acknowledgement_is_recovered_at_a_later_generation() {
    let w = world();
    let intent = w.intent_for_t(KEY);
    reserve(&w.ops, &w.objects, &intent, NOW).unwrap();
    w.memory.fault(Op::Put, CONTROL, Fault::After, 0, 1);
    let receipt = activate(
        &w.ops,
        &w.objects,
        &intent,
        &writer_route(T),
        &sha(b"p"),
        NOW,
    )
    .unwrap();
    // Unrelated later generations, including a stale-directory reimport.
    w.objects
        .apply(NOW, &Edit::Suspend(OTHER.into(), true), None, None)
        .unwrap();
    w.objects
        .apply(NOW, &Edit::Reimport(&w.credentials, &w.tenants), None, None)
        .unwrap();
    let snapshot = w.snapshot();
    assert!(snapshot.generation() > receipt.generation);
    let entry = snapshot.lifecycle(T).unwrap();
    assert!(matches!(&entry.slot, Slot::Reserved { receipt: Some(r), .. } if *r == receipt));
    // A retry of the same activation reads back the same receipt.
    let generation = snapshot.generation();
    assert_eq!(
        activate(
            &w.ops,
            &w.objects,
            &intent,
            &writer_route(T),
            &sha(b"p"),
            NOW
        )
        .unwrap(),
        receipt
    );
    assert_eq!(w.snapshot().generation(), generation);
}

#[test]
fn directory_replacement_cannot_silently_drop_an_activated_route() {
    let w = world();
    let intent = w.intent_for_t(KEY);
    reserve(&w.ops, &w.objects, &intent, NOW).unwrap();
    activate(
        &w.ops,
        &w.objects,
        &intent,
        &writer_route(T),
        &sha(b"p"),
        NOW,
    )
    .unwrap();
    let error = w
        .objects
        .apply(
            NOW,
            &Edit::Directory,
            Some(&[Route::shared_for_test(OTHER)]),
            None,
        )
        .err()
        .unwrap();
    assert_eq!(error.code, "control_state_invalid");
    assert!(w.snapshot().record_for(T).is_some());
}

#[test]
fn terminal_receipt_is_sealed_before_the_slot_is_released_and_reused() {
    let w = world();
    let intent = w.intent_for_t(KEY);
    reserve(&w.ops, &w.objects, &intent, NOW).unwrap();
    // Not terminal yet: nothing is sealed or released.
    assert_eq!(
        w.ops.release(&w.objects, &intent, NOW).unwrap_err().code,
        "operation_not_terminal"
    );
    assert!(w.ops.terminal(&intent.operation_id).unwrap().is_none());
    let receipt = activate(
        &w.ops,
        &w.objects,
        &intent,
        &writer_route(T),
        &sha(b"p"),
        NOW,
    )
    .unwrap();
    let sealed = w.ops.release(&w.objects, &intent, NOW).unwrap();
    let terminal = w.ops.terminal(&intent.operation_id).unwrap().unwrap();
    assert_eq!(terminal.outcome, TerminalOutcome::Succeeded { receipt });
    match &w.snapshot().lifecycle(T).unwrap().slot {
        Slot::Released {
            operation_id,
            terminal_digest,
        } => {
            assert_eq!(*operation_id, intent.operation_id);
            assert_eq!(terminal_digest, sealed.digest());
        }
        other => panic!("{other:?}"),
    }
    // The route stays after the operation ends; a successor may reserve.
    assert!(w.snapshot().record_for(T).is_some());
    let successor = w
        .ops
        .accept(
            ADMIN,
            "successor",
            &Payload::SetReaders {
                tenant_id: T.into(),
                fleet: Fleet::Absent {},
            },
            &[],
            NOW,
        )
        .unwrap()
        .intent;
    reserve(&w.ops, &w.objects, &successor, NOW).unwrap();
    assert_eq!(w.snapshot().lifecycle(T).unwrap().generation, 2);
    // The prior outcome survives slot reuse in immutable history.
    assert!(w.ops.terminal(&intent.operation_id).unwrap().is_some());
}

#[test]
fn lost_terminal_acknowledgements_are_recovered_by_readback() {
    let w = world();
    let intent = w.intent_for_t(KEY);
    reserve(&w.ops, &w.objects, &intent, NOW).unwrap();
    activate(
        &w.ops,
        &w.objects,
        &intent,
        &writer_route(T),
        &sha(b"p"),
        NOW,
    )
    .unwrap();
    w.memory.fault(Op::Put, "terminal.json", Fault::After, 0, 1);
    w.memory.fault(Op::Put, CONTROL, Fault::After, 0, 1);
    let sealed = w.ops.release(&w.objects, &intent, NOW).unwrap();
    assert!(matches!(&w.snapshot().lifecycle(T).unwrap().slot,
        Slot::Released { terminal_digest, .. } if terminal_digest == sealed.digest()));
    // Idempotent afterwards.
    assert_eq!(
        w.ops.release(&w.objects, &intent, NOW).unwrap().digest(),
        sealed.digest()
    );
}

#[test]
fn unsealed_terminal_never_releases_the_slot() {
    let w = world();
    let intent = w.intent_for_t(KEY);
    reserve(&w.ops, &w.objects, &intent, NOW).unwrap();
    request_cancel(&w.objects, &intent, NOW).unwrap();
    w.memory
        .fault(Op::Put, "terminal.json", Fault::Before, 0, 1);
    assert_eq!(
        w.ops.release(&w.objects, &intent, NOW).unwrap_err().code,
        "operation_indeterminate"
    );
    assert!(matches!(
        w.snapshot().lifecycle(T).unwrap().slot,
        Slot::Reserved { .. }
    ));
    w.ops.release(&w.objects, &intent, NOW).unwrap();
    assert_eq!(
        w.ops
            .terminal(&intent.operation_id)
            .unwrap()
            .unwrap()
            .outcome,
        TerminalOutcome::Cancelled
    );
    assert!(w.snapshot().record_for(T).is_none());
}

#[test]
fn lifecycle_entry_is_compact_and_strict() {
    let w = world();
    let intent = w.intent_for_t(KEY);
    reserve(&w.ops, &w.objects, &intent, NOW).unwrap();
    activate(
        &w.ops,
        &w.objects,
        &intent,
        &writer_route(T),
        &sha(b"p"),
        NOW,
    )
    .unwrap();
    let raw: Value = serde_json::from_slice(&w.memory.raw(CONTROL).unwrap()).unwrap();
    // Largest form (reserved + receipt); 1024 tenants stay well below the
    // existing 1 MiB snapshot bound.
    let entry = &raw["lifecycle"][0];
    assert!(serde_json::to_vec(entry).unwrap().len() <= 640, "{entry}");
    let parse = |raw: &Value| {
        w.memory.set_raw(CONTROL, &serde_json::to_vec(raw).unwrap());
        w.objects.read().map(drop)
    };
    assert!(parse(&raw).is_ok());
    for (pointer, value) in [
        ("/lifecycle/0/extra", json!(1)),
        ("/lifecycle/0/slot/extra", json!(1)),
        ("/lifecycle/0/slot/receipt/extra", json!(1)),
        ("/lifecycle/0/slot/operation_id", json!("x".repeat(4096))),
        (
            "/lifecycle/0/slot/receipt/route_digest",
            json!(sha(b"other")),
        ),
        ("/lifecycle/0/slot/receipt/epoch", json!(9)),
        ("/lifecycle/0/slot/cancellation_requested", json!(true)),
        ("/lifecycle/0/generation", json!(0)),
    ] {
        let mut bad = raw.clone();
        let (parent, field) = pointer.rsplit_once('/').unwrap();
        bad.pointer_mut(parent).unwrap()[field] = value;
        assert!(parse(&bad).is_err(), "{pointer}");
    }
    let mut duplicate = raw.clone();
    let copy = duplicate["lifecycle"][0].clone();
    duplicate["lifecycle"].as_array_mut().unwrap().push(copy);
    assert!(parse(&duplicate).is_err());
}

#[test]
fn legacy_migration_and_lifecycle_reservation_exclude_each_other() {
    use crate::server::control_state::{AuthorityMode, Kind as RouteKind, Migration, Phase};
    use crate::server::tenant_migration;
    let w = world();
    // A legacy migration holds the shared-routed OTHER.
    let source = Route::shared_for_test(OTHER);
    let target = Route {
        kind: RouteKind::Dedicated,
        authority_mode: Some(AuthorityMode::Off),
        lineage: source.lineage + 1,
        ..source.clone()
    };
    let migration = Migration {
        id: "move-one".into(),
        tenant: OTHER.into(),
        phase: Phase::Prepared,
        source: source.clone(),
        target,
        source_lineage: source.lineage,
        target_lineage: source.lineage + 1,
        updated_unix_ms: NOW,
    };
    let generation = w.snapshot().generation();
    tenant_migration::prepare(&w.objects, "move-one", generation, &migration, NOW).unwrap();
    let mut held_by_migration = w.intent_for_t(KEY);
    held_by_migration.tenant_id = OTHER.into();
    assert_eq!(
        reserve(&w.ops, &w.objects, &held_by_migration, NOW)
            .unwrap_err()
            .code,
        "lifecycle_reserved"
    );
    // A lifecycle reservation on T blocks a legacy migration of T.
    reserve(&w.ops, &w.objects, &w.intent_for_t("held"), NOW).unwrap();
    w.objects
        .apply(
            NOW,
            &Edit::Directory,
            Some(&[Route::shared_for_test(OTHER), Route::shared_for_test(T)]),
            None,
        )
        .unwrap();
    let mut blocked = migration.clone();
    blocked.id = "move-two".into();
    blocked.tenant = T.into();
    blocked.source = Route::shared_for_test(T);
    blocked.target.tenant = T.into();
    let generation = w.snapshot().generation();
    assert_eq!(
        tenant_migration::prepare(&w.objects, "move-two", generation, &blocked, NOW)
            .err()
            .unwrap()
            .code,
        "tenant_migration_conflict"
    );
}

// ------------------------------------------------------------- ITEM-003

#[test]
fn cancel_wins_the_cas_race_and_activation_cannot_follow() {
    let w = world();
    let intent = w.intent_for_t(KEY);
    reserve(&w.ops, &w.objects, &intent, NOW).unwrap();
    let racer = Objects::new(w.memory.clone(), CONTROL);
    let cancelling = intent.clone();
    w.memory.hook(Op::Put, CONTROL, move || {
        request_cancel(&racer, &cancelling, NOW).unwrap();
    });
    let error = activate(
        &w.ops,
        &w.objects,
        &intent,
        &writer_route(T),
        &sha(b"p"),
        NOW,
    )
    .unwrap_err();
    assert_eq!(error.code, "operation_cancelled");
    let snapshot = w.snapshot();
    assert!(snapshot.record_for(T).is_none());
    assert!(matches!(
        snapshot.lifecycle(T).unwrap().slot,
        Slot::Reserved {
            cancellation_requested: true,
            receipt: None,
            ..
        }
    ));
}

#[test]
fn activation_wins_the_cas_race_and_cancel_requires_retirement() {
    let w = world();
    let intent = w.intent_for_t(KEY);
    reserve(&w.ops, &w.objects, &intent, NOW).unwrap();
    let racer = Objects::new(w.memory.clone(), CONTROL);
    let racer_ops = Operations::new(w.memory.clone(), PREFIX);
    let activating = intent.clone();
    w.memory.hook(Op::Put, CONTROL, move || {
        activate(
            &racer_ops,
            &racer,
            &activating,
            &writer_route(T),
            &sha(b"p"),
            NOW,
        )
        .unwrap();
    });
    let error = request_cancel(&w.objects, &intent, NOW).unwrap_err();
    assert_eq!(error.status, StatusCode::CONFLICT);
    assert_eq!(error.code, "retirement_required");
    let snapshot = w.snapshot();
    assert!(snapshot.record_for(T).is_some());
    assert!(matches!(
        snapshot.lifecycle(T).unwrap().slot,
        Slot::Reserved {
            cancellation_requested: false,
            receipt: Some(_),
            ..
        }
    ));
}

#[test]
fn indeterminate_cancel_is_reconciled_and_still_excludes_activation() {
    let w = world();
    let intent = w.intent_for_t(KEY);
    reserve(&w.ops, &w.objects, &intent, NOW).unwrap();
    w.memory.fault(Op::Put, CONTROL, Fault::After, 0, 1);
    request_cancel(&w.objects, &intent, NOW).unwrap();
    assert_eq!(
        activate(
            &w.ops,
            &w.objects,
            &intent,
            &writer_route(T),
            &sha(b"p"),
            NOW
        )
        .unwrap_err()
        .code,
        "operation_cancelled"
    );
}

fn action(operation: &str, destructive: bool) -> Action {
    Action {
        resource: APP.into(),
        operation: operation.into(),
        request_digest: sha(operation.as_bytes()),
        destructive,
    }
}

fn accepted_id(w: &World) -> String {
    w.accept(KEY, &create_payload("c"))
        .unwrap()
        .intent
        .operation_id
}

#[test]
fn step_is_persisted_before_dispatch_and_uncertain_until_settled() {
    let w = world();
    let id = accepted_id(&w);
    w.ops.claim(&id, "worker-a", None).unwrap();
    let create = action("create_app", false);
    assert_eq!(
        w.ops.record(&id, "worker-a", &create).unwrap(),
        Decision::Dispatch(1)
    );
    let journal = w.ops.journal(&id).unwrap();
    assert_eq!(journal.steps[0].outcome, StepOutcome::Uncertain {});
    // Re-recording the same action after a crash means "read back the exact
    // resource", never a second dispatch.
    assert_eq!(
        w.ops.record(&id, "worker-a", &create).unwrap(),
        Decision::Reconcile(1)
    );
    // A different action while the outcome is unknown is refused.
    let other = action("update_app", false);
    assert_eq!(
        w.ops.record(&id, "worker-a", &other).unwrap_err().code,
        "prior_action_unknown"
    );
    w.ops.settle(&id, "worker-a", 1, "revision-0001").unwrap();
    assert_eq!(
        w.ops.record(&id, "worker-a", &create).unwrap(),
        Decision::Settled(1)
    );
    assert_eq!(
        w.ops.record(&id, "worker-a", &other).unwrap(),
        Decision::Dispatch(2)
    );
}

#[test]
fn ambiguous_step_write_is_reconciled_not_redispatched() {
    let w = world();
    let id = accepted_id(&w);
    w.ops.claim(&id, "worker-a", None).unwrap();
    let create = action("create_app", false);
    w.memory.fault(Op::Put, "journal.json", Fault::After, 0, 1);
    assert_eq!(
        w.ops.record(&id, "worker-a", &create).unwrap_err().code,
        "operation_indeterminate"
    );
    // The step may exist; the retry finds it and must not dispatch again.
    assert_eq!(
        w.ops.record(&id, "worker-a", &create).unwrap(),
        Decision::Reconcile(1)
    );
}

#[test]
fn settled_steps_are_immutable() {
    let w = world();
    let id = accepted_id(&w);
    w.ops.claim(&id, "worker-a", None).unwrap();
    w.ops
        .record(&id, "worker-a", &action("create_app", false))
        .unwrap();
    w.ops.settle(&id, "worker-a", 1, "revision-0001").unwrap();
    w.ops.settle(&id, "worker-a", 1, "revision-0001").unwrap();
    assert_eq!(
        w.ops
            .settle(&id, "worker-a", 1, "revision-0002")
            .unwrap_err()
            .code,
        "operation_step_conflict"
    );
    assert_eq!(
        w.ops.settle(&id, "worker-a", 9, "x").unwrap_err().code,
        "operation_step_conflict"
    );
}

#[test]
fn blob_journal_ownership_alone_cannot_take_over_an_actuator() {
    let w = world();
    let id = accepted_id(&w);
    w.ops.claim(&id, "worker-a", None).unwrap();
    let create = action("create_app", false);
    w.ops.record(&id, "worker-a", &create).unwrap();
    // worker-b can read and CAS the journal, but cannot prove worker-a will
    // not still dispatch its in-flight call.
    assert_eq!(
        w.ops.claim(&id, "worker-b", None).unwrap_err().code,
        "actuator_fence_unproven"
    );
    let wrong = ActuatorStopped::proven_for_test("worker-z");
    assert_eq!(
        w.ops.claim(&id, "worker-b", Some(&wrong)).unwrap_err().code,
        "actuator_fence_unproven"
    );
    let stopped = ActuatorStopped::proven_for_test("worker-a");
    w.ops.claim(&id, "worker-b", Some(&stopped)).unwrap();
    // The unknown prior action still blocks conflicting and destructive work.
    for blocked in [action("update_app", false), action("delete_app", true)] {
        assert_eq!(
            w.ops.record(&id, "worker-b", &blocked).unwrap_err().code,
            "prior_action_unknown"
        );
    }
    assert_eq!(
        w.ops.record(&id, "worker-b", &create).unwrap(),
        Decision::Reconcile(1)
    );
    // The paused former owner is fenced out of the journal.
    assert_eq!(
        w.ops.record(&id, "worker-a", &create).unwrap_err().code,
        "actuator_not_owner"
    );
    assert_eq!(
        w.ops.settle(&id, "worker-a", 1, "rev").unwrap_err().code,
        "actuator_not_owner"
    );
    w.ops.settle(&id, "worker-b", 1, "revision-0001").unwrap();
    assert_eq!(
        w.ops
            .record(&id, "worker-b", &action("delete_app", true))
            .unwrap(),
        Decision::Dispatch(2)
    );
}

#[test]
fn concurrent_first_claims_have_one_owner() {
    let w = world();
    let id = accepted_id(&w);
    let racer = Operations::new(w.memory.clone(), PREFIX);
    let racing_id = id.clone();
    w.memory.hook(Op::Put, "journal.json", move || {
        racer.claim(&racing_id, "worker-b", None).unwrap();
    });
    assert_eq!(
        w.ops.claim(&id, "worker-a", None).unwrap_err().code,
        "actuator_fence_unproven"
    );
    assert_eq!(
        w.ops.journal(&id).unwrap().attempt.as_deref(),
        Some("worker-b")
    );
}

#[test]
fn unresolved_side_effect_blocks_terminal_release() {
    let w = world();
    let intent = w.intent_for_t(KEY);
    reserve(&w.ops, &w.objects, &intent, NOW).unwrap();
    w.ops.claim(&intent.operation_id, "worker-a", None).unwrap();
    w.ops
        .record(
            &intent.operation_id,
            "worker-a",
            &action("create_app", false),
        )
        .unwrap();
    request_cancel(&w.objects, &intent, NOW).unwrap();
    assert_eq!(
        w.ops.release(&w.objects, &intent, NOW).unwrap_err().code,
        "cleanup_pending"
    );
    assert!(w.ops.terminal(&intent.operation_id).unwrap().is_none());
    assert!(matches!(
        w.snapshot().lifecycle(T).unwrap().slot,
        Slot::Reserved { .. }
    ));
    w.ops
        .settle(&intent.operation_id, "worker-a", 1, "absent")
        .unwrap();
    w.ops.release(&w.objects, &intent, NOW).unwrap();
}

#[test]
fn an_ended_operation_can_never_reserve_or_activate_again() {
    let w = world();
    let intent = w.intent_for_t(KEY);
    reserve(&w.ops, &w.objects, &intent, NOW).unwrap();
    request_cancel(&w.objects, &intent, NOW).unwrap();
    w.ops.release(&w.objects, &intent, NOW).unwrap();
    // Recovery from the operation ID alone must not re-open the slot.
    assert_eq!(
        reserve(&w.ops, &w.objects, &intent, NOW).unwrap_err().code,
        "operation_terminal"
    );
    assert_eq!(
        activate(
            &w.ops,
            &w.objects,
            &intent,
            &writer_route(T),
            &sha(b"p"),
            NOW
        )
        .unwrap_err()
        .code,
        "operation_terminal"
    );
    // The snapshot refuses it too, independent of the terminal record.
    let error = w
        .snapshot()
        .reserve(
            T,
            &intent.operation_id,
            intent.kind(),
            &intent.request_digest,
        )
        .err()
        .unwrap();
    assert_eq!(error.code, "operation_terminal");
    assert!(w.snapshot().record_for(T).is_none());
}

#[test]
fn an_action_recorded_while_sealing_blocks_the_seal() {
    let w = world();
    let intent = w.intent_for_t(KEY);
    reserve(&w.ops, &w.objects, &intent, NOW).unwrap();
    w.ops.claim(&intent.operation_id, "worker-a", None).unwrap();
    request_cancel(&w.objects, &intent, NOW).unwrap();
    // The live owner records a step just before the sealing journal CAS.
    let racer = Operations::new(w.memory.clone(), PREFIX);
    let id = intent.operation_id.clone();
    w.memory.hook(Op::Put, "journal.json", move || {
        assert_eq!(
            racer
                .record(&id, "worker-a", &action("create_app", false))
                .unwrap(),
            Decision::Dispatch(1)
        );
    });
    assert_eq!(
        w.ops.release(&w.objects, &intent, NOW).unwrap_err().code,
        "cleanup_pending"
    );
    assert!(w.ops.terminal(&intent.operation_id).unwrap().is_none());
    assert!(matches!(
        w.snapshot().lifecycle(T).unwrap().slot,
        Slot::Reserved { .. }
    ));
    // Once settled, sealing closes the journal: no later action is recorded.
    w.ops
        .settle(&intent.operation_id, "worker-a", 1, "absent")
        .unwrap();
    w.ops.release(&w.objects, &intent, NOW).unwrap();
    assert!(w.ops.journal(&intent.operation_id).unwrap().closed);
    assert_eq!(
        w.ops
            .record(
                &intent.operation_id,
                "worker-a",
                &action("delete_app", true)
            )
            .unwrap_err()
            .code,
        "operation_terminal"
    );
}

#[test]
fn journal_is_strict() {
    let w = world();
    let id = accepted_id(&w);
    w.ops.claim(&id, "worker-a", None).unwrap();
    w.ops
        .record(&id, "worker-a", &action("create_app", false))
        .unwrap();
    let name = format!("{PREFIX}/operations/{id}/journal.json");
    let good: Value = serde_json::from_slice(&w.memory.raw(&name).unwrap()).unwrap();
    for pointer in [
        "/extra",
        "/steps/0/extra",
        "/steps/0/action/extra",
        "/steps/0/outcome/extra",
    ] {
        let mut bad = good.clone();
        let (parent, field) = pointer.rsplit_once('/').unwrap();
        bad.pointer_mut(parent).unwrap()[field] = json!(1);
        w.memory.set_raw(&name, &serde_json::to_vec(&bad).unwrap());
        assert_eq!(
            w.ops.journal(&id).unwrap_err().code,
            "operation_state_invalid",
            "{pointer}"
        );
    }
}
