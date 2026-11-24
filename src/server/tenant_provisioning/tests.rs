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
    // ITEM-007: the reservation guard refuses before snapshot validation.
    assert_eq!(error.code, "lifecycle_reserved");
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
    // A lifecycle reservation on T blocks a legacy migration of T. A control
    // edit cannot route T while a create reservation holds it (ITEM-007),
    // so T is routed first and then held by a reader update.
    w.objects
        .apply(
            NOW,
            &Edit::Directory,
            Some(&[Route::shared_for_test(OTHER), Route::shared_for_test(T)]),
            None,
        )
        .unwrap();
    w.objects
        .update(NOW, |current| {
            current.reserve(T, &"3".repeat(32), Kind::SetReaders, &"4".repeat(64))
        })
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

/// ITEM-008 assessment fixture on the real set protocol: a reader's
/// operation pin rewrites the whole set record, in the same coordination
/// store and prefix as the writer authority and the lifecycle guard. No
/// object-level grant can therefore allow the pin without also allowing
/// publication, deletion and verification rewrites, so reader fleets stay
/// denied. This proves the layout only, not Azure RBAC or mount enforcement.
#[test]
fn reader_pins_share_authority_scope_so_reader_fleets_stay_denied() {
    use crate::server::cloud_backup::PinKind;
    use std::collections::BTreeMap;
    let (env, authority, _claim) = crate::server::cloud_recovery::tests::setup_ha();
    guard::Guards::of(&authority).hold(T, OTHER).unwrap();
    let (_, prefix) = env.sets.coordination();
    let objects = || -> BTreeMap<String, Vec<u8>> {
        env.meta
            .names()
            .into_iter()
            .filter_map(|name| env.meta.raw(&name).map(|bytes| (name, bytes)))
            .collect()
    };
    let before = objects();
    let record = format!("{prefix}sets/{}/record.json", env.set_id);
    for control in [
        format!("{prefix}authority.json"),
        format!("{prefix}{}", guard::OBJECT),
        record.clone(),
    ] {
        assert!(before.contains_key(&control), "{control} shares the store");
    }
    env.sets
        .pin(
            &env.set_id,
            "reader-apply:replica-1",
            PinKind::Operation,
            NOW,
        )
        .unwrap();
    let after = objects();
    let changed: Vec<_> = after
        .iter()
        .filter(|(name, bytes)| before.get(*name) != Some(*bytes))
        .map(|(name, _)| name.clone())
        .collect();
    assert_eq!(changed, [record.as_str()], "a pin writes only the record");
    // The one object a pin needs also carries the set's lifecycle state.
    let written: Value = serde_json::from_slice(&after[&record]).unwrap();
    for field in ["state", "manifest_sha256", "pins", "delete_reason"] {
        assert!(written.get(field).is_some(), "{field}");
    }
    assert_eq!(written["pins"].as_array().unwrap().len(), 1);
    let refusal = pin_scope_unproven();
    assert_eq!(refusal.status, axum::http::StatusCode::CONFLICT);
    assert_eq!(refusal.code, "pin_scope_unproven");
}

// ------------------------------------------------------------- EPIC-004

mod epic_004 {
    //! design-tenant-provisioning EPIC-004 (ITEM-009..011) over a scripted
    //! in-memory ARM port. Nothing is dispatched to any cloud.
    use super::*;
    use crate::server::authority::{AuthorityStore, HeadRef};
    use aca::{
        Arm, ArmError, BudgetApproval, Call, Demand, Desired, Driver, Observed, Ownership, Poll,
        Preflighted, Profile, Qualification, QuotaProof, Revision, Revisions, Verb,
    };
    use std::collections::VecDeque;
    use std::sync::Mutex;

    const ENV: &str =
        "/subscriptions/s/resourceGroups/g/providers/Microsoft.App/managedEnvironments/e";
    const STORE: &str =
        "/subscriptions/s/resourceGroups/g/providers/Microsoft.Storage/storageAccounts/s";
    const APP1: &str = "/subscriptions/s/resourceGroups/g/providers/Microsoft.App/containerApps/w1";
    const TARGET: &str = "target/";
    const LINEAGE: u64 = 0x77;

    fn ours(tenant: &str) -> String {
        format!("{}:{tenant}", aca::OWNER_TAG)
    }

    fn app(owner: &str, config: &str, active: &[bool]) -> Observed {
        Observed {
            owner: Some(owner.into()),
            config_digest: config.into(),
            succeeded: true,
            revisions: Revisions::Complete(
                active
                    .iter()
                    .enumerate()
                    .map(|(i, active)| Revision {
                        name: format!("r{i}"),
                        active: *active,
                        config_digest: config.into(),
                    })
                    .collect(),
            ),
        }
    }

    fn plain(owner: Option<&str>, config: &str) -> Observed {
        Observed {
            owner: owner.map(str::to_owned),
            config_digest: config.into(),
            succeeded: true,
            revisions: Revisions::NotApplicable,
        }
    }

    enum Begin {
        /// Takes effect at once.
        Apply,
        /// Accepted; takes effect only when the test applies it.
        Pending(&'static str),
        /// Lost acknowledgement before any ID was returned.
        Unknown,
        Refused(&'static str),
        /// Refused with no effect, after `Observed` appeared at the name
        /// (another actor won the race since the pre-dispatch read).
        RefuseAfter(&'static str, Observed),
        /// Someone else's resource appears at the name.
        Collide(Observed),
        /// Takes effect at once, but its operation keeps running (it may
        /// still act afterwards).
        ApplyRunning(&'static str),
    }

    #[derive(Default)]
    struct MockArm {
        resources: Mutex<BTreeMap<String, Observed>>,
        calls: Mutex<Vec<Call>>,
        begins: Mutex<VecDeque<Begin>>,
        polls: Mutex<VecDeque<Result<Poll, ArmError>>>,
        polled: Mutex<Vec<String>>,
        read_fails: Mutex<bool>,
        on_read: Mutex<Option<ReadHook>>,
    }

    /// Runs once, just before the next resource readback (a deterministic
    /// interleaving point for another executor).
    type ReadHook = Box<dyn FnOnce() + Send>;

    impl MockArm {
        fn on_read(&self, f: impl FnOnce() + Send + 'static) {
            *self.on_read.lock().unwrap() = Some(Box::new(f));
        }

        fn effect(&self, call: &Call) {
            let mut resources = self.resources.lock().unwrap();
            match &call.verb {
                Verb::Put { config_digest } => {
                    let found = if call.resource.contains("/containerApps/") {
                        app(&call.owner, config_digest, &[true])
                    } else {
                        plain(Some(&call.owner), config_digest)
                    };
                    resources.insert(call.resource.clone(), found);
                }
                // EPIC-005: an in-place update rolls a new active revision.
                Verb::Update { config_digest } => {
                    if let Some(found) = resources.get_mut(&call.resource)
                        && let Revisions::Complete(revisions) = &mut found.revisions
                    {
                        revisions.iter_mut().for_each(|r| r.active = false);
                        revisions.push(Revision {
                            name: format!("r{}", revisions.len()),
                            active: true,
                            config_digest: config_digest.clone(),
                        });
                        found.config_digest = config_digest.clone();
                    }
                }
                Verb::Delete => {
                    resources.remove(&call.resource);
                }
                Verb::Stop => {
                    if let Some(Observed {
                        revisions: Revisions::Complete(revisions),
                        ..
                    }) = resources.get_mut(&call.resource)
                    {
                        revisions.iter_mut().for_each(|r| r.active = false);
                    }
                }
            }
        }

        fn put(&self, resource: &str, found: Observed) {
            self.resources
                .lock()
                .unwrap()
                .insert(resource.into(), found);
        }

        fn get(&self, resource: &str) -> Option<Observed> {
            self.resources.lock().unwrap().get(resource).cloned()
        }

        fn script(&self, begin: Begin) {
            self.begins.lock().unwrap().push_back(begin);
        }

        fn polls(&self, polls: impl IntoIterator<Item = Result<Poll, ArmError>>) {
            self.polls.lock().unwrap().extend(polls);
        }

        fn calls(&self) -> Vec<Call> {
            self.calls.lock().unwrap().clone()
        }

        fn count(&self, verb: &Verb) -> usize {
            self.calls().iter().filter(|c| c.verb == *verb).count()
        }
    }

    impl Arm for MockArm {
        fn begin(&self, call: &Call) -> Result<String, ArmError> {
            let mut calls = self.calls.lock().unwrap();
            calls.push(call.clone());
            let id = format!("arm-op-{}", calls.len());
            drop(calls);
            let next = self.begins.lock().unwrap().pop_front();
            match next.unwrap_or(Begin::Apply) {
                Begin::Apply => {
                    self.effect(call);
                    Ok(id)
                }
                Begin::Pending(id) => Ok(id.into()),
                Begin::Unknown => Err(ArmError::Unknown),
                Begin::Refused(code) => Err(ArmError::Refused(code.into())),
                Begin::RefuseAfter(code, found) => {
                    self.put(&call.resource, found);
                    Err(ArmError::Refused(code.into()))
                }
                Begin::Collide(found) => {
                    self.put(&call.resource, found);
                    Ok(id)
                }
                Begin::ApplyRunning(id) => {
                    self.effect(call);
                    Ok(id.into())
                }
            }
        }

        fn poll(&self, operation: &str) -> Result<Poll, ArmError> {
            self.polled.lock().unwrap().push(operation.into());
            self.polls
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(Ok(Poll::Succeeded))
        }

        fn read(&self, resource: &str) -> Result<Option<Observed>, ArmError> {
            let hook = self.on_read.lock().unwrap().take();
            if let Some(f) = hook {
                f();
            }
            if *self.read_fails.lock().unwrap() {
                return Err(ArmError::Unknown);
            }
            Ok(self.get(resource))
        }
    }

    fn desired(resource: &str, app: bool, ownership: Ownership) -> Desired {
        Desired {
            resource: resource.into(),
            config_digest: sha(resource.as_bytes()),
            app,
            ownership,
        }
    }

    const PEAK: Demand = Demand {
        apps: 2,
        cores_milli: 2000,
        memory_mib: 4096,
    };

    fn ready(resources: Vec<Desired>) -> Preflighted {
        aca::preflight(
            Profile::for_test(resources, PEAK),
            Some(&Qualification::for_test()),
            Some(&QuotaProof::for_test(PEAK)),
            Some(&BudgetApproval::for_test()),
        )
        .unwrap()
    }

    /// ENV pre-exists (verified); STORE and APP1 are created.
    fn standard() -> Preflighted {
        ready(vec![
            desired(ENV, false, Ownership::Verified),
            desired(STORE, false, Ownership::Owned),
            desired(APP1, true, Ownership::Owned),
        ])
    }

    fn app_only() -> Preflighted {
        ready(vec![desired(APP1, true, Ownership::Owned)])
    }

    fn arm_with_env() -> MockArm {
        let arm = MockArm::default();
        arm.put(ENV, plain(None, &sha(ENV.as_bytes())));
        arm
    }

    fn reserved(w: &World) -> Intent {
        let intent = w.intent_for_t(KEY);
        reserve(&w.ops, &w.objects, &intent, NOW).unwrap();
        intent
    }

    fn driver<'a>(w: &'a World, arm: &'a MockArm, intent: &'a Intent) -> Driver<'a> {
        Driver::new(&w.ops, arm, intent, "worker-a", None, 3).unwrap()
    }

    fn steps(w: &World, intent: &Intent) -> Vec<(String, String, StepOutcome)> {
        w.ops
            .journal(&intent.operation_id)
            .unwrap()
            .steps
            .into_iter()
            .map(|s| (s.action.resource, s.action.operation, s.outcome))
            .collect()
    }

    fn settled(observed: &str) -> StepOutcome {
        StepOutcome::Settled {
            observed: observed.into(),
        }
    }

    fn running(polls: usize) -> Vec<Result<Poll, ArmError>> {
        vec![Ok(Poll::Running); polls]
    }

    // --------------------------------------------------------- ITEM-009

    #[test]
    fn effects_need_the_actuator_claim_before_any_arm_call() {
        let w = world();
        let arm = arm_with_env();
        let intent = reserved(&w);
        w.ops.claim(&intent.operation_id, "worker-a", None).unwrap();
        let refused = Driver::new(&w.ops, &arm, &intent, "worker-b", None, 3);
        assert_eq!(refused.err().unwrap().code, "actuator_fence_unproven");
        assert!(arm.calls().is_empty());
        assert!(steps(&w, &intent).is_empty());
    }

    #[test]
    fn owned_resources_are_created_once_and_settled_from_exact_readback() {
        let w = world();
        let arm = arm_with_env();
        let intent = reserved(&w);
        let d = driver(&w, &arm, &intent);
        d.provision(&w.objects, &standard()).unwrap();
        assert_eq!(
            steps(&w, &intent),
            [
                (ENV.into(), "verify".into(), settled("verified")),
                (STORE.into(), "create".into(), settled("created")),
                (APP1.into(), "create-app".into(), settled("created")),
            ]
        );
        let journal = w.ops.journal(&intent.operation_id).unwrap();
        assert_eq!(journal.steps[0].arm_operation, None);
        assert_eq!(journal.steps[2].arm_operation.as_deref(), Some("arm-op-2"));
        let calls = arm.calls();
        assert_eq!(calls.len(), 2, "the verified prerequisite is only read");
        assert!(
            calls
                .iter()
                .all(|c| c.owner == ours(T) && matches!(c.verb, Verb::Put { .. }))
        );
        // An identical rerun dispatches nothing.
        d.provision(&w.objects, &standard()).unwrap();
        assert_eq!(arm.calls().len(), 2);
        // No route, reader or client admission follows from provisioning.
        assert!(w.snapshot().record_for(T).is_none());
    }

    #[test]
    fn unknown_outcome_is_reconciled_through_the_persisted_operation_never_redispatched() {
        let w = world();
        let arm = arm_with_env();
        let intent = reserved(&w);
        let d = driver(&w, &arm, &intent);
        arm.script(Begin::Pending("arm-op-slow"));
        arm.polls([Ok(Poll::Running), Err(ArmError::Unknown), Ok(Poll::Running)]);
        let error = d.provision(&w.objects, &app_only()).unwrap_err();
        assert_eq!(
            (error.status, error.code),
            (StatusCode::SERVICE_UNAVAILABLE, "cloud_outcome_unknown")
        );
        let journal = w.ops.journal(&intent.operation_id).unwrap();
        assert_eq!(journal.steps[0].outcome, StepOutcome::Uncertain {});
        assert_eq!(
            journal.steps[0].arm_operation.as_deref(),
            Some("arm-op-slow")
        );
        // Even a proven takeover resumes by polling, not by a new PUT.
        let stopped = ActuatorStopped::proven_for_test("worker-a");
        let resumed = Driver::new(&w.ops, &arm, &intent, "worker-b", Some(&stopped), 3).unwrap();
        arm.effect(&arm.calls()[0]);
        resumed.provision(&w.objects, &app_only()).unwrap();
        assert_eq!(arm.calls().len(), 1);
        assert_eq!(arm.polled.lock().unwrap().last().unwrap(), "arm-op-slow");
        assert_eq!(steps(&w, &intent)[0].2, settled("created"));
    }

    #[test]
    fn lost_dispatch_without_an_operation_id_stays_unknown() {
        let w = world();
        let arm = arm_with_env();
        let intent = reserved(&w);
        let d = driver(&w, &arm, &intent);
        arm.script(Begin::Unknown);
        for _ in 0..2 {
            assert_eq!(
                d.provision(&w.objects, &app_only()).unwrap_err().code,
                "cloud_outcome_unknown"
            );
        }
        // Absence is no proof a lost PUT will never land: no redispatch.
        assert_eq!(arm.calls().len(), 1);
        assert_eq!(steps(&w, &intent)[0].2, StepOutcome::Uncertain {});
    }

    #[test]
    fn pre_existing_resources_are_refused_never_adopted() {
        for owner in ["unmanaged".to_owned(), ours(T), ours(OTHER)] {
            let w = world();
            let arm = arm_with_env();
            arm.put(APP1, app(&owner, &sha(APP1.as_bytes()), &[true]));
            let intent = reserved(&w);
            let d = driver(&w, &arm, &intent);
            assert_eq!(
                d.provision(&w.objects, &app_only()).unwrap_err().code,
                "resource_ownership_conflict"
            );
            assert!(arm.calls().is_empty());
            assert!(steps(&w, &intent).is_empty());
        }
    }

    #[test]
    fn collision_with_a_foreign_resource_settles_conflict() {
        let w = world();
        let arm = arm_with_env();
        let intent = reserved(&w);
        let d = driver(&w, &arm, &intent);
        arm.script(Begin::Collide(app(
            &ours(OTHER),
            &sha(APP1.as_bytes()),
            &[true],
        )));
        assert_eq!(
            d.provision(&w.objects, &app_only()).unwrap_err().code,
            "resource_ownership_conflict"
        );
        assert_eq!(steps(&w, &intent)[0].2, settled("conflict"));
        assert_eq!(
            d.provision(&w.objects, &app_only()).unwrap_err().code,
            "resource_ownership_conflict"
        );
        assert_eq!(arm.calls().len(), 1);
    }

    #[test]
    fn revision_overlap_and_incomplete_inventory_are_never_proof() {
        let w = world();
        let arm = arm_with_env();
        let intent = reserved(&w);
        let d = driver(&w, &arm, &intent);
        let config = sha(APP1.as_bytes());
        arm.script(Begin::Collide(app(&ours(T), &config, &[true, true])));
        assert_eq!(
            d.provision(&w.objects, &app_only()).unwrap_err().code,
            "revision_overlap"
        );
        let mut partial = app(&ours(T), &config, &[true]);
        partial.revisions = Revisions::Incomplete;
        arm.put(APP1, partial);
        assert_eq!(
            d.provision(&w.objects, &app_only()).unwrap_err().code,
            "revision_inventory_incomplete"
        );
        // The active revision runs another configuration.
        let mut drifted = app(&ours(T), &sha(b"other"), &[true]);
        drifted.config_digest.clone_from(&config);
        arm.put(APP1, drifted);
        assert_eq!(
            d.provision(&w.objects, &app_only()).unwrap_err().code,
            "cloud_outcome_unknown"
        );
        assert_eq!(steps(&w, &intent)[0].2, StepOutcome::Uncertain {});
        arm.put(APP1, app(&ours(T), &config, &[false, true]));
        d.provision(&w.objects, &app_only()).unwrap();
        assert_eq!(arm.calls().len(), 1);
    }

    #[test]
    fn refused_create_with_absent_readback_settles_failed() {
        let w = world();
        let arm = arm_with_env();
        let intent = reserved(&w);
        let d = driver(&w, &arm, &intent);
        let profile = ready(vec![desired(STORE, false, Ownership::Owned)]);
        arm.script(Begin::Refused("Quota Exceeded!"));
        assert_eq!(
            d.provision(&w.objects, &profile).unwrap_err().code,
            "cloud_action_failed"
        );
        assert_eq!(steps(&w, &intent)[0].2, settled("failed:QuotaExceeded"));
        // A settled failure is final for this operation: no retry dispatch.
        assert_eq!(
            d.provision(&w.objects, &profile).unwrap_err().code,
            "cloud_action_failed"
        );
        assert_eq!(arm.calls().len(), 1);
    }

    #[test]
    fn polled_failure_with_absent_readback_settles_failed() {
        let w = world();
        let arm = arm_with_env();
        let intent = reserved(&w);
        let d = driver(&w, &arm, &intent);
        arm.script(Begin::Pending("arm-op-bad"));
        arm.polls([Ok(Poll::Failed("Bad Request".into()))]);
        assert_eq!(
            d.provision(&w.objects, &app_only()).unwrap_err().code,
            "cloud_action_failed"
        );
        assert_eq!(steps(&w, &intent)[0].2, settled("failed:BadRequest"));
    }

    #[test]
    fn verified_prerequisites_are_only_read() {
        let config = sha(ENV.as_bytes());
        let cases = [
            (None, "prerequisite_missing"),
            (Some(plain(None, &sha(b"drift"))), "prerequisite_mismatch"),
            (
                Some(plain(Some(&ours(OTHER)), &config)),
                "resource_ownership_conflict",
            ),
        ];
        for (found, code) in cases {
            let w = world();
            let arm = MockArm::default();
            if let Some(found) = found {
                arm.put(ENV, found);
            }
            let intent = reserved(&w);
            let d = driver(&w, &arm, &intent);
            assert_eq!(d.provision(&w.objects, &standard()).unwrap_err().code, code);
            assert!(arm.calls().is_empty());
            assert!(steps(&w, &intent).is_empty());
        }
        let w = world();
        let arm = arm_with_env();
        *arm.read_fails.lock().unwrap() = true;
        let intent = reserved(&w);
        let d = driver(&w, &arm, &intent);
        assert_eq!(
            d.provision(&w.objects, &standard()).unwrap_err().code,
            "cloud_outcome_unknown"
        );
        assert!(arm.calls().is_empty());
    }

    #[test]
    fn provisioning_needs_an_uncancelled_reservation() {
        let w = world();
        let arm = arm_with_env();
        let intent = w.intent_for_t(KEY);
        let d = driver(&w, &arm, &intent);
        assert_eq!(
            d.provision(&w.objects, &standard()).unwrap_err().code,
            "operation_not_provisionable"
        );
        reserve(&w.ops, &w.objects, &intent, NOW).unwrap();
        request_cancel(&w.objects, &intent, NOW).unwrap();
        assert_eq!(
            d.provision(&w.objects, &standard()).unwrap_err().code,
            "operation_not_provisionable"
        );
        assert!(arm.calls().is_empty());
    }

    // --------------------------------------------------------- ITEM-011

    #[test]
    fn preflight_refuses_every_missing_proof_and_no_profile_schema_exists() {
        let w = world();
        let intent = w.intent_for_t(KEY);
        let loaded = w.ops.load(&intent.operation_id).unwrap();
        assert_eq!(
            Profile::resolve(&loaded).unwrap_err().code,
            "profile_unsupported"
        );
        let profile = || Profile::for_test(vec![desired(APP1, true, Ownership::Owned)], PEAK);
        let (q, b) = (Qualification::for_test(), BudgetApproval::for_test());
        let small = QuotaProof::for_test(Demand {
            cores_milli: 1000,
            ..PEAK
        });
        let room = QuotaProof::for_test(PEAK);
        let code = |r: Result<Preflighted, ApiError>| r.err().unwrap().code;
        let cases = [
            (None, Some(&room), Some(&b), "qualification_missing"),
            (Some(&q), None, Some(&b), "quota_unproven"),
            (Some(&q), Some(&small), Some(&b), "quota_exceeded"),
            (Some(&q), Some(&room), None, "budget_unapproved"),
        ];
        for (q, quota, b, expected) in cases {
            assert_eq!(code(aca::preflight(profile(), q, quota, b)), expected);
        }
        let owned = |resource| desired(resource, true, Ownership::Owned);
        for invalid in [
            Profile::for_test(vec![owned(APP1), owned(APP1)], PEAK),
            Profile::for_test(vec![owned(APP1), owned(ENV), owned(STORE)], PEAK),
            Profile::for_test(Vec::new(), PEAK),
        ] {
            let error = aca::preflight(invalid, Some(&q), Some(&room), Some(&b))
                .err()
                .unwrap();
            assert_eq!(error.status, StatusCode::UNPROCESSABLE_ENTITY);
        }
        assert!(aca::preflight(profile(), Some(&q), Some(&room), Some(&b)).is_ok());
    }

    #[test]
    fn cancel_cleanup_deletes_only_created_apps_and_retains_everything_else() {
        let w = world();
        let arm = arm_with_env();
        let intent = reserved(&w);
        let d = driver(&w, &arm, &intent);
        assert_eq!(
            d.cleanup(&w.objects, &Guards::new(w.memory.clone(), TARGET))
                .unwrap_err()
                .code,
            "operation_not_cancelled"
        );
        d.provision(&w.objects, &standard()).unwrap();
        request_cancel(&w.objects, &intent, NOW).unwrap();
        assert_eq!(
            w.ops.release(&w.objects, &intent, NOW).unwrap_err().code,
            "cleanup_pending"
        );
        d.cleanup(&w.objects, &Guards::new(w.memory.clone(), TARGET))
            .unwrap();
        d.cleanup(&w.objects, &Guards::new(w.memory.clone(), TARGET))
            .unwrap();
        let deletes: Vec<_> = arm
            .calls()
            .into_iter()
            .filter(|c| c.verb == Verb::Delete)
            .map(|c| c.resource)
            .collect();
        assert_eq!(deletes, [APP1]);
        assert!(arm.get(APP1).is_none());
        assert!(arm.get(STORE).is_some() && arm.get(ENV).is_some());
        let sealed = w.ops.release(&w.objects, &intent, NOW).unwrap();
        assert_eq!(sealed.outcome(), &TerminalOutcome::Cancelled);
    }

    #[test]
    fn cleanup_resolves_unknown_creates_by_owner_tag_before_deleting() {
        let w = world();
        let arm = arm_with_env();
        let intent = reserved(&w);
        let d = driver(&w, &arm, &intent);
        arm.script(Begin::Pending("arm-op-slow"));
        arm.polls(running(3));
        d.provision(&w.objects, &app_only()).unwrap_err();
        request_cancel(&w.objects, &intent, NOW).unwrap();
        arm.polls(running(3));
        assert_eq!(
            d.cleanup(&w.objects, &Guards::new(w.memory.clone(), TARGET))
                .unwrap_err()
                .code,
            "cloud_outcome_unknown"
        );
        assert_eq!(
            w.ops.release(&w.objects, &intent, NOW).unwrap_err().code,
            "cleanup_pending"
        );
        // The late PUT lands; cleanup proves it ours, then deletes it.
        arm.effect(&arm.calls()[0]);
        d.cleanup(&w.objects, &Guards::new(w.memory.clone(), TARGET))
            .unwrap();
        assert!(arm.get(APP1).is_none());
        let outcomes: Vec<_> = steps(&w, &intent).into_iter().map(|s| (s.1, s.2)).collect();
        assert_eq!(
            outcomes,
            [
                ("create-app".to_owned(), settled("created")),
                ("delete-app".to_owned(), settled("deleted")),
            ]
        );
        w.ops.release(&w.objects, &intent, NOW).unwrap();
    }

    /// Creates APP1 for `T`, activates its route and seals the create.
    fn activated(w: &World, arm: &MockArm) -> (Intent, String) {
        let intent = reserved(w);
        driver(w, arm, &intent)
            .provision(&w.objects, &app_only())
            .unwrap();
        let receipt = activate(
            &w.ops,
            &w.objects,
            &intent,
            &writer_route(T),
            &sha(b"p"),
            NOW,
        )
        .unwrap();
        w.ops.release(&w.objects, &intent, NOW).unwrap();
        (intent, admin::receipt_digest(&receipt))
    }

    fn retirement(w: &World, expected_receipt: &str) -> Intent {
        w.ops
            .accept(
                ADMIN,
                "retire-t",
                &Payload::Retire {
                    tenant_id: T.into(),
                    expected_receipt: expected_receipt.into(),
                    reason: "offboarded".into(),
                    data_disposition: DataDisposition::Retain,
                },
                &[],
                NOW,
            )
            .unwrap()
            .intent
    }

    #[test]
    fn retirement_removes_the_route_stops_apps_and_retains_data() {
        let w = world();
        let arm = MockArm::default();
        let (_, receipt) = activated(&w, &arm);
        let credential = w.snapshot().live_credential("aaaaaaaaaaaa", NOW);
        let intent = retirement(&w, &receipt);
        let d = driver(&w, &arm, &intent);
        let guards = Guards::new(w.memory.clone(), TARGET);
        assert_eq!(
            d.retire(&w.objects, &guards, NOW).unwrap_err().code,
            "retirement_unverified"
        );
        d.reserve_retirement(&w.objects, NOW).unwrap();
        d.reserve_retirement(&w.objects, NOW).unwrap();
        let sealed = d.retire(&w.objects, &guards, NOW).unwrap();
        assert_eq!(sealed.outcome(), &TerminalOutcome::Retired);
        assert!(w.snapshot().record_for(T).is_none());
        assert!(matches!(
            w.snapshot().lifecycle(T).unwrap().slot,
            Slot::Released { .. }
        ));
        // The app and its data remain; no revision is active.
        assert_eq!(
            arm.get(APP1).unwrap(),
            app(&ours(T), &sha(APP1.as_bytes()), &[false])
        );
        assert_eq!(arm.count(&Verb::Delete), 0);
        assert_eq!(
            w.snapshot().live_credential("aaaaaaaaaaaa", NOW),
            credential
        );
        // No writer claim or failover after retirement.
        assert_eq!(guards.current().unwrap().unwrap().state, guard::State::Held);
        // Idempotent afterwards.
        assert_eq!(
            d.retire(&w.objects, &guards, NOW).unwrap().digest(),
            sealed.digest()
        );
        assert_eq!(arm.count(&Verb::Stop), 1);
    }

    #[test]
    fn retirement_refuses_unverified_receipts() {
        let w = world();
        let arm = MockArm::default();
        // Before any activation there is nothing to verify against.
        let early = retirement(&w, &sha(b"receipt"));
        assert_eq!(
            driver(&w, &arm, &early)
                .reserve_retirement(&w.objects, NOW)
                .unwrap_err()
                .code,
            "retirement_receipt_mismatch"
        );
        let w = world();
        activated(&w, &arm);
        let wrong = retirement(&w, &sha(b"receipt"));
        assert_eq!(
            driver(&w, &arm, &wrong)
                .reserve_retirement(&w.objects, NOW)
                .unwrap_err()
                .code,
            "retirement_receipt_mismatch"
        );
        assert!(w.snapshot().record_for(T).is_some());
        assert!(matches!(
            w.snapshot().lifecycle(T).unwrap().slot,
            Slot::Released { .. }
        ));
    }

    #[test]
    fn retirement_resumes_after_an_unknown_stop_and_cannot_be_cancelled_back() {
        let w = world();
        let arm = MockArm::default();
        let (_, receipt) = activated(&w, &arm);
        let intent = retirement(&w, &receipt);
        let d = driver(&w, &arm, &intent);
        let guards = Guards::new(w.memory.clone(), TARGET);
        d.reserve_retirement(&w.objects, NOW).unwrap();
        let journal = w.ops.journal(&intent.operation_id).unwrap();
        assert_eq!(
            aca::terminal_ready(&journal, &intent.payload, &TerminalOutcome::Retired)
                .unwrap_err()
                .code,
            "retirement_incomplete"
        );
        arm.script(Begin::Pending("arm-op-stop"));
        arm.polls(running(3));
        assert_eq!(
            d.retire(&w.objects, &guards, NOW).unwrap_err().code,
            "cloud_outcome_unknown"
        );
        assert!(w.snapshot().record_for(T).is_none());
        assert_eq!(
            request_cancel(&w.objects, &intent, NOW).unwrap_err().code,
            "retirement_in_progress"
        );
        assert_eq!(
            w.ops.release(&w.objects, &intent, NOW).unwrap_err().code,
            "cleanup_pending"
        );
        let stop = arm
            .calls()
            .into_iter()
            .find(|c| c.verb == Verb::Stop)
            .unwrap();
        arm.effect(&stop);
        let sealed = d.retire(&w.objects, &guards, NOW).unwrap();
        assert_eq!(sealed.outcome(), &TerminalOutcome::Retired);
        assert_eq!(arm.count(&Verb::Stop), 1);
    }

    #[test]
    fn lost_route_removal_acknowledgement_is_resolved_by_readback() {
        let w = world();
        let arm = MockArm::default();
        let (_, receipt) = activated(&w, &arm);
        let intent = retirement(&w, &receipt);
        let d = driver(&w, &arm, &intent);
        let guards = Guards::new(w.memory.clone(), TARGET);
        d.reserve_retirement(&w.objects, NOW).unwrap();
        w.memory.fault(Op::Put, CONTROL, Fault::After, 0, 1);
        let sealed = d.retire(&w.objects, &guards, NOW).unwrap();
        assert_eq!(sealed.outcome(), &TerminalOutcome::Retired);
    }

    #[test]
    fn cancelled_retirement_keeps_the_route() {
        let w = world();
        let arm = MockArm::default();
        let (_, receipt) = activated(&w, &arm);
        let intent = retirement(&w, &receipt);
        let d = driver(&w, &arm, &intent);
        let guards = Guards::new(w.memory.clone(), TARGET);
        d.reserve_retirement(&w.objects, NOW).unwrap();
        request_cancel(&w.objects, &intent, NOW).unwrap();
        assert_eq!(
            d.retire(&w.objects, &guards, NOW).unwrap_err().code,
            "operation_cancelled"
        );
        assert!(w.snapshot().record_for(T).is_some());
        assert_eq!(arm.count(&Verb::Stop), 0);
        assert!(guards.current().unwrap().is_none_or(
            |g| g.operation_id != intent.operation_id || g.state == guard::State::Released
        ));
        assert_eq!(
            w.ops.release(&w.objects, &intent, NOW).unwrap().outcome(),
            &TerminalOutcome::Cancelled
        );
    }

    // --------------------------------------------------------- ITEM-010

    fn seed() -> Seed {
        Seed {
            lineage: LINEAGE,
            digest: sha(b"seed"),
        }
    }

    #[test]
    fn seed_is_recorded_once_and_never_rekeyed() {
        let w = world();
        let intent = reserved(&w);
        let id = &intent.operation_id;
        w.ops.claim(id, "worker-a", None).unwrap();
        assert!(recorded_seed(&w.ops.journal(id).unwrap()).is_none());
        w.ops.record_seed(id, "worker-a", &seed()).unwrap();
        w.ops.record_seed(id, "worker-a", &seed()).unwrap();
        assert_eq!(recorded_seed(&w.ops.journal(id).unwrap()), Some(seed()));
        for other in [
            Seed {
                lineage: 0x78,
                ..seed()
            },
            Seed {
                digest: sha(b"other"),
                ..seed()
            },
        ] {
            assert_eq!(
                w.ops.record_seed(id, "worker-a", &other).unwrap_err().code,
                "seed_mismatch"
            );
        }
        let zero = Seed {
            lineage: 0,
            ..seed()
        };
        assert_eq!(
            w.ops.record_seed(id, "worker-a", &zero).unwrap_err().status,
            StatusCode::UNPROCESSABLE_ENTITY
        );
    }

    fn authority(w: &World) -> AuthorityStore {
        AuthorityStore::new(w.memory.clone(), TARGET.into())
    }

    fn establish(
        w: &World,
        store: &AuthorityStore,
        intent: &Intent,
    ) -> Result<crate::server::authority::Observed, ApiError> {
        let guards = Guards::new(w.memory.clone(), TARGET);
        establish_authority(
            &w.ops,
            &w.objects,
            store,
            &guards,
            intent,
            "install-1",
            "scope-1",
        )
    }

    fn seeded(w: &World) -> Intent {
        let intent = reserved(w);
        let id = &intent.operation_id;
        w.ops.claim(id, "worker-a", None).unwrap();
        w.ops.record_seed(id, "worker-a", &seed()).unwrap();
        intent
    }

    #[test]
    fn authority_is_created_for_the_seed_and_never_reset() {
        let w = world();
        let intent = reserved(&w);
        let store = authority(&w);
        let guards = Guards::new(w.memory.clone(), TARGET);
        w.ops.claim(&intent.operation_id, "worker-a", None).unwrap();
        assert_eq!(
            establish(&w, &store, &intent).unwrap_err().code,
            "seed_unrecorded"
        );
        assert!(guards.current().unwrap().is_none());
        w.ops
            .record_seed(&intent.operation_id, "worker-a", &seed())
            .unwrap();
        let observed = establish(&w, &store, &intent).unwrap();
        assert_eq!(observed.record.lineage, format!("{LINEAGE:016x}"));
        assert_eq!(
            guards.current().unwrap().unwrap().state,
            guard::State::WriteEnabled
        );
        assert_eq!(
            establish(&w, &store, &intent).unwrap().record,
            observed.record
        );
        // The route stays absent: no client write is admitted yet.
        assert!(w.snapshot().record_for(T).is_none());

        // An authority of any other identity is refused, never reset.
        let w = world();
        let intent = seeded(&w);
        let store = authority(&w);
        let guards = Guards::new(w.memory.clone(), TARGET);
        store
            .create("install-1", "scope-1", "0000000000000078")
            .unwrap();
        let before = store.read().unwrap();
        assert_eq!(
            establish(&w, &store, &intent).unwrap_err().code,
            "authority_identity_mismatch"
        );
        assert_eq!(store.read().unwrap(), before);
        assert_eq!(guards.current().unwrap().unwrap().state, guard::State::Held);
    }

    const MANIFEST: &str = "1111111111111111111111111111111111111111111111111111111111111111";

    /// A seeded, guarded, serving authority with a first checkpoint.
    fn serving(w: &World) -> (Intent, AuthorityStore) {
        let intent = seeded(w);
        let store = authority(w);
        let prepared = establish(w, &store, &intent).unwrap();
        let (claim, observed) = store.claim(&prepared, NOW).unwrap();
        let head = HeadRef {
            epoch: claim.epoch,
            set_id: "20240101T000000Z-0123456789abcdef".into(),
            manifest_sha256: MANIFEST.into(),
            index_sha256: None,
            sequence: 1,
            chain_sha256: "2".repeat(64),
        };
        store.advance_head(&claim, &observed, head).unwrap();
        (intent, store)
    }

    fn readiness(tenant: &str) -> Readiness {
        Readiness {
            archive_manifest_sha256: MANIFEST.into(),
            writer: app(&ours(tenant), &sha(APP1.as_bytes()), &[false, true]),
            policy_generation_required: 3,
            policy_generation_observed: 3,
            route: Route {
                tenant: tenant.into(),
                ..route()
            },
        }
    }

    #[test]
    fn readiness_needs_clock_authority_archive_writer_and_policy_proof() {
        let w = world();
        let (intent, store) = serving(&w);
        let clock = projection::ClockProof::for_test(50);
        let error = prove(&w.ops, &store, &intent, &readiness(T), None).unwrap_err();
        assert_eq!(
            (error.status, error.code),
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "projection_freshness_unproven"
            )
        );
        let config = sha(APP1.as_bytes());
        let mut incomplete = readiness(T);
        incomplete.writer.revisions = Revisions::Incomplete;
        let cases = [
            Readiness {
                archive_manifest_sha256: "3".repeat(64),
                ..readiness(T)
            },
            readiness(OTHER),
            Readiness {
                writer: app(&ours(T), &config, &[true, true]),
                ..readiness(T)
            },
            Readiness {
                policy_generation_observed: 2,
                ..readiness(T)
            },
            Readiness {
                policy_generation_required: 0,
                policy_generation_observed: 0,
                ..readiness(T)
            },
            incomplete,
        ];
        for case in &cases {
            assert_eq!(
                prove(&w.ops, &store, &intent, case, Some(&clock))
                    .unwrap_err()
                    .code,
                "readiness_unproven"
            );
        }
        let proof = prove(&w.ops, &store, &intent, &readiness(T), Some(&clock)).unwrap();
        assert_eq!((proof.epoch, proof.lineage), (1, LINEAGE));
        // A recovering authority (no first checkpoint) proves nothing.
        let w = world();
        let intent = seeded(&w);
        let store = authority(&w);
        establish(&w, &store, &intent).unwrap();
        assert_eq!(
            prove(&w.ops, &store, &intent, &readiness(T), Some(&clock))
                .unwrap_err()
                .code,
            "readiness_unproven"
        );
    }

    #[test]
    fn credential_delivery_needs_acknowledgement_and_replacement_revokes() {
        let w = world();
        let id = &w.intent_for_t(KEY).operation_id;
        let unacknowledged = |w: &World| w.ops.delivered(id).unwrap_err().code;
        assert_eq!(unacknowledged(&w), "credential_delivery_unacknowledged");
        w.ops.issue_candidate(id, "cand-a").unwrap();
        assert_eq!(w.ops.issue_candidate(id, "cand-a").unwrap().generation, 1);
        assert_eq!(
            w.ops.issue_candidate(id, "cand-b").unwrap_err().code,
            "delivery_candidate_exists"
        );
        assert_eq!(unacknowledged(&w), "credential_delivery_unacknowledged");
        let replaced = w.ops.replace_candidate(id, 1, "cand-b").unwrap();
        assert_eq!(replaced.generation, 2);
        assert_eq!(replaced.revoked, ["cand-a"]);
        assert_eq!(w.ops.replace_candidate(id, 1, "cand-b").unwrap(), replaced);
        let stale = |r: Result<Delivery, ApiError>| r.unwrap_err().code;
        assert_eq!(
            stale(w.ops.replace_candidate(id, 2, "cand-a")),
            "delivery_stale"
        );
        assert_eq!(
            stale(w.ops.acknowledge_delivery(id, 1, "cand-a")),
            "delivery_stale"
        );
        w.ops.acknowledge_delivery(id, 2, "cand-b").unwrap();
        w.ops.acknowledge_delivery(id, 2, "cand-b").unwrap();
        assert_eq!(
            stale(w.ops.replace_candidate(id, 2, "cand-c")),
            "delivery_stale"
        );
        w.ops.delivered(id).unwrap();
        // A lost write acknowledgement is resolved by the same call.
        let w = world();
        let id = &w.intent_for_t(KEY).operation_id;
        w.memory.fault(Op::Put, "delivery.json", Fault::After, 0, 1);
        assert_eq!(
            w.ops.issue_candidate(id, "cand-a").unwrap_err().code,
            "operation_indeterminate"
        );
        assert_eq!(w.ops.issue_candidate(id, "cand-a").unwrap().generation, 1);
    }

    fn proven(w: &World) -> (Intent, Proof) {
        let (intent, store) = serving(w);
        let clock = projection::ClockProof::for_test(50);
        let proof = prove(&w.ops, &store, &intent, &readiness(T), Some(&clock)).unwrap();
        (intent, proof)
    }

    fn route() -> Route {
        Route {
            lineage: LINEAGE,
            ..writer_route(T)
        }
    }

    #[test]
    fn activation_requires_the_proven_route_and_revoked_candidates() {
        let w = world();
        let (intent, proof) = proven(&w);
        let guards = Guards::new(w.memory.clone(), TARGET);
        let id = &intent.operation_id;
        // `bbbbbbbbbbbb` stays live in the published view: replacing it is
        // not enough until the view confirms its revocation.
        w.ops.issue_candidate(id, "bbbbbbbbbbbb").unwrap();
        w.ops.replace_candidate(id, 1, "aaaaaaaaaaaa").unwrap();
        w.ops.acknowledge_delivery(id, 2, "aaaaaaaaaaaa").unwrap();
        let delivered = w.ops.delivered(id).unwrap();
        let activate = |route: &Route| {
            activate_create(
                &w.ops, &w.objects, &guards, &intent, route, &proof, &delivered, NOW,
            )
        };
        for bad in [
            Route {
                lineage: 0x1234,
                ..route()
            },
            Route {
                epoch: Some(2),
                ..route()
            },
            Route {
                reader: Some("https://reader".into()),
                ..route()
            },
            Route {
                read_routing: control_state::ReadRouting::Readers,
                ..route()
            },
            Route {
                tenant: OTHER.into(),
                ..route()
            },
        ] {
            assert_eq!(activate(&bad).unwrap_err().code, "activation_route_invalid");
        }
        assert_eq!(
            activate(&route()).unwrap_err().code,
            "candidate_revocation_unconfirmed"
        );
        assert!(w.snapshot().record_for(T).is_none());
        assert_eq!(
            guards.current().unwrap().unwrap().state,
            guard::State::WriteEnabled
        );
    }

    #[test]
    fn unpublished_credential_blocks_activation() {
        let w = world();
        let (intent, proof) = proven(&w);
        let guards = Guards::new(w.memory.clone(), TARGET);
        let id = &intent.operation_id;
        // The production case for a new tenant (OD-004): acknowledged, but
        // not in the published view.
        w.ops.issue_candidate(id, "cccccccccccc").unwrap();
        w.ops.acknowledge_delivery(id, 1, "cccccccccccc").unwrap();
        let delivered = w.ops.delivered(id).unwrap();
        assert_eq!(
            activate_create(
                &w.ops,
                &w.objects,
                &guards,
                &intent,
                &route(),
                &proof,
                &delivered,
                NOW,
            )
            .unwrap_err()
            .code,
            "credential_view_unpublished"
        );
        assert!(w.snapshot().record_for(T).is_none());
    }

    #[test]
    fn activation_publishes_the_writer_route_and_seals_succeeded() {
        let w = world();
        let (intent, proof) = proven(&w);
        let guards = Guards::new(w.memory.clone(), TARGET);
        let id = &intent.operation_id;
        w.ops.issue_candidate(id, "aaaaaaaaaaaa").unwrap();
        w.ops.acknowledge_delivery(id, 1, "aaaaaaaaaaaa").unwrap();
        let delivered = w.ops.delivered(id).unwrap();
        let run = || {
            activate_create(
                &w.ops,
                &w.objects,
                &guards,
                &intent,
                &route(),
                &proof,
                &delivered,
                NOW,
            )
        };
        let sealed = run().unwrap();
        let TerminalOutcome::Succeeded { receipt } = sealed.outcome() else {
            panic!("{:?}", sealed.outcome());
        };
        assert_eq!((receipt.lineage, receipt.epoch), (LINEAGE, 1));
        assert_eq!(receipt.proof_digest, proof.digest);
        let published = w.snapshot().record_for(T).unwrap().clone();
        assert_eq!(published, route());
        assert!(published.reader.is_none());
        assert_eq!(
            guards.current().unwrap().unwrap().state,
            guard::State::Released
        );
        assert_eq!(run().unwrap().digest(), sealed.digest());
    }

    // ------------------------------------- review corrections (a913ebbe)

    #[test]
    fn a_running_create_is_never_proven_by_presence_alone() {
        // Finding 1 (create path): the resource is there, but the recorded
        // operation still runs; settling `created` would let a later cleanup
        // delete it while the create can still act.
        let w = world();
        let arm = arm_with_env();
        let intent = reserved(&w);
        let d = driver(&w, &arm, &intent);
        arm.script(Begin::Collide(app(
            &ours(T),
            &sha(APP1.as_bytes()),
            &[true],
        )));
        arm.polls(running(3));
        assert_eq!(
            d.provision(&w.objects, &app_only()).unwrap_err().code,
            "cloud_outcome_unknown"
        );
        assert_eq!(steps(&w, &intent)[0].2, StepOutcome::Uncertain {});
        d.provision(&w.objects, &app_only()).unwrap();
        assert_eq!(steps(&w, &intent)[0].2, settled("created"));
    }

    #[test]
    fn cleanup_waits_until_a_running_create_can_no_longer_act() {
        // Finding 1 (cleanup path).
        let w = world();
        let arm = arm_with_env();
        let intent = reserved(&w);
        let d = driver(&w, &arm, &intent);
        arm.script(Begin::Pending("arm-op-slow"));
        arm.polls(running(3));
        d.provision(&w.objects, &app_only()).unwrap_err();
        request_cancel(&w.objects, &intent, NOW).unwrap();
        arm.effect(&arm.calls()[0]);
        arm.polls(running(3));
        assert_eq!(
            d.cleanup(&w.objects, &Guards::new(w.memory.clone(), TARGET))
                .unwrap_err()
                .code,
            "cloud_outcome_unknown"
        );
        assert_eq!(arm.count(&Verb::Delete), 0);
        assert!(arm.get(APP1).is_some());
        assert_eq!(
            w.ops.release(&w.objects, &intent, NOW).unwrap_err().code,
            "cleanup_pending"
        );
        // Once the create is terminal, cleanup deletes it.
        d.cleanup(&w.objects, &Guards::new(w.memory.clone(), TARGET))
            .unwrap();
        assert!(arm.get(APP1).is_none());
        w.ops.release(&w.objects, &intent, NOW).unwrap();
    }

    #[test]
    fn cleanup_does_not_delete_an_unproven_create_without_an_operation_id() {
        // Finding 1: a lost dispatch has no operation to wait for, so its
        // resource cannot be shown to be finished; it is never deleted.
        let w = world();
        let arm = arm_with_env();
        let intent = reserved(&w);
        let d = driver(&w, &arm, &intent);
        arm.script(Begin::Unknown);
        d.provision(&w.objects, &app_only()).unwrap_err();
        request_cancel(&w.objects, &intent, NOW).unwrap();
        arm.effect(&arm.calls()[0]);
        assert_eq!(
            d.cleanup(&w.objects, &Guards::new(w.memory.clone(), TARGET))
                .unwrap_err()
                .code,
            "cloud_outcome_unknown"
        );
        assert_eq!(arm.count(&Verb::Delete), 0);
    }

    #[test]
    fn a_refused_dispatch_never_records_a_create() {
        // Finding 2: the port refused, so this operation created nothing,
        // even though a matching resource of the same tenant now exists.
        let w = world();
        let arm = arm_with_env();
        let intent = reserved(&w);
        let d = driver(&w, &arm, &intent);
        let exact = app(&ours(T), &sha(APP1.as_bytes()), &[true]);
        arm.script(Begin::RefuseAfter("AlreadyExists", exact.clone()));
        assert_eq!(
            d.provision(&w.objects, &app_only()).unwrap_err().code,
            "cloud_action_failed"
        );
        assert_eq!(steps(&w, &intent)[0].2, settled("failed:AlreadyExists"));
        request_cancel(&w.objects, &intent, NOW).unwrap();
        d.cleanup(&w.objects, &Guards::new(w.memory.clone(), TARGET))
            .unwrap();
        assert_eq!(arm.count(&Verb::Delete), 0);
        assert_eq!(arm.get(APP1), Some(exact));
        w.ops.release(&w.objects, &intent, NOW).unwrap();
    }

    fn delivered_for(w: &World, intent: &Intent, id: &str) -> Delivered {
        let op = &intent.operation_id;
        w.ops.issue_candidate(op, id).unwrap();
        w.ops.acknowledge_delivery(op, 1, id).unwrap();
        w.ops.delivered(op).unwrap()
    }

    #[test]
    fn activation_refuses_a_route_substituted_after_readiness() {
        // Finding 3.
        let w = world();
        let (intent, proof) = proven(&w);
        let guards = Guards::new(w.memory.clone(), TARGET);
        let delivered = delivered_for(&w, &intent, "aaaaaaaaaaaa");
        for substituted in [
            Route {
                writer: "https://attacker.invalid".into(),
                ..route()
            },
            Route {
                audience: "other-audience".into(),
                ..route()
            },
            Route {
                binding_generation: route().binding_generation + 1,
                ..route()
            },
        ] {
            let error = activate_create(
                &w.ops,
                &w.objects,
                &guards,
                &intent,
                &substituted,
                &proof,
                &delivered,
                NOW,
            )
            .unwrap_err();
            assert_eq!(error.code, "activation_route_invalid");
        }
        assert!(w.snapshot().record_for(T).is_none());
    }

    #[test]
    fn a_committed_activation_is_recovered_after_a_crash_before_the_seal() {
        // Finding 4: the route CAS committed, then the worker crashed. A
        // rerun whose readiness digest changed (a later checkpoint) and whose
        // credential is no longer published still finishes from the receipt.
        let w = world();
        let (intent, proof) = proven(&w);
        let guards = Guards::new(w.memory.clone(), TARGET);
        let committed =
            activate(&w.ops, &w.objects, &intent, &route(), &proof.digest, NOW).unwrap();
        let later = Proof {
            digest: "e".repeat(64),
            ..proof
        };
        let unpublished = delivered_for(&w, &intent, "cccccccccccc");
        let sealed = activate_create(
            &w.ops,
            &w.objects,
            &guards,
            &intent,
            &route(),
            &later,
            &unpublished,
            NOW,
        )
        .unwrap();
        assert_eq!(
            sealed.outcome(),
            &TerminalOutcome::Succeeded { receipt: committed }
        );
        assert_eq!(
            guards.current().unwrap().unwrap().state,
            guard::State::Released
        );
    }

    #[test]
    fn a_cancelled_retirement_releases_its_guard_and_keeps_the_route() {
        // Finding 5: the guard was held, then cancellation won the route CAS
        // (modelled by a failed CAS followed by the cancel).
        let w = world();
        let arm = MockArm::default();
        let (_, receipt) = activated(&w, &arm);
        let route = w.snapshot().record_for(T).cloned();
        let intent = retirement(&w, &receipt);
        let d = driver(&w, &arm, &intent);
        let guards = Guards::new(w.memory.clone(), TARGET);
        d.reserve_retirement(&w.objects, NOW).unwrap();
        w.memory.fault(Op::Put, CONTROL, Fault::Before, 0, 1);
        d.retire(&w.objects, &guards, NOW).unwrap_err();
        assert_eq!(guards.current().unwrap().unwrap().state, guard::State::Held);
        request_cancel(&w.objects, &intent, NOW).unwrap();
        // The seal waits for the guard to be reconciled.
        assert_eq!(
            w.ops.release(&w.objects, &intent, NOW).unwrap_err().code,
            "cleanup_pending"
        );
        assert_eq!(
            d.retire(&w.objects, &guards, NOW).unwrap_err().code,
            "operation_cancelled"
        );
        assert_eq!(
            guards.current().unwrap().unwrap().state,
            guard::State::Released
        );
        assert_eq!(
            w.ops.release(&w.objects, &intent, NOW).unwrap().outcome(),
            &TerminalOutcome::Cancelled
        );
        assert_eq!(w.snapshot().record_for(T).cloned(), route);
        assert_eq!(arm.count(&Verb::Stop), 0);
    }

    #[test]
    fn a_retirement_cancelled_before_it_started_never_holds_the_guard() {
        // Finding 5: cancellation is noticed before the hold.
        let w = world();
        let arm = MockArm::default();
        let (_, receipt) = activated(&w, &arm);
        let intent = retirement(&w, &receipt);
        let d = driver(&w, &arm, &intent);
        let guards = Guards::new(w.memory.clone(), TARGET);
        d.reserve_retirement(&w.objects, NOW).unwrap();
        request_cancel(&w.objects, &intent, NOW).unwrap();
        assert_eq!(
            d.retire(&w.objects, &guards, NOW).unwrap_err().code,
            "operation_cancelled"
        );
        assert!(
            guards
                .current()
                .unwrap()
                .is_none_or(|g| g.operation_id != intent.operation_id)
        );
        w.ops.release(&w.objects, &intent, NOW).unwrap();
    }

    fn retirement_keyed(w: &World, key: &str, expected_receipt: &str) -> Intent {
        w.ops
            .accept(
                ADMIN,
                key,
                &Payload::Retire {
                    tenant_id: T.into(),
                    expected_receipt: expected_receipt.into(),
                    reason: "offboarded".into(),
                    data_disposition: DataDisposition::Retain,
                },
                &[],
                NOW,
            )
            .unwrap()
            .intent
    }

    #[test]
    fn a_retirement_after_a_cancelled_one_verifies_the_original_receipt() {
        // Finding 6.
        let w = world();
        let arm = MockArm::default();
        let (_, receipt) = activated(&w, &arm);
        let first = retirement_keyed(&w, "retire-1", &receipt);
        let d = driver(&w, &arm, &first);
        let guards = Guards::new(w.memory.clone(), TARGET);
        d.reserve_retirement(&w.objects, NOW).unwrap();
        request_cancel(&w.objects, &first, NOW).unwrap();
        d.retire(&w.objects, &guards, NOW).unwrap_err();
        w.ops.release(&w.objects, &first, NOW).unwrap();
        // An unrelated receipt is still refused.
        let wrong = retirement_keyed(&w, "retire-2", &sha(b"receipt"));
        assert_eq!(
            driver(&w, &arm, &wrong)
                .reserve_retirement(&w.objects, NOW)
                .unwrap_err()
                .code,
            "retirement_receipt_mismatch"
        );
        let second = retirement_keyed(&w, "retire-3", &receipt);
        let d = driver(&w, &arm, &second);
        d.reserve_retirement(&w.objects, NOW).unwrap();
        let sealed = d.retire(&w.objects, &guards, NOW).unwrap();
        assert_eq!(sealed.outcome(), &TerminalOutcome::Retired);
        assert!(w.snapshot().record_for(T).is_none());
    }

    // ------------------------------- review corrections, round 2 (f047bb9a)

    #[test]
    fn a_running_create_is_not_settled_by_a_foreign_owner_readback() {
        // A: a foreign resource at the name is no proof the recorded create
        // can no longer act; the cancelled seal waits for a terminal poll.
        let w = world();
        let arm = arm_with_env();
        let intent = reserved(&w);
        let d = driver(&w, &arm, &intent);
        arm.script(Begin::Collide(app(&ours(OTHER), "x", &[true])));
        arm.polls(running(3));
        assert_eq!(
            d.provision(&w.objects, &app_only()).unwrap_err().code,
            "cloud_outcome_unknown"
        );
        assert_eq!(steps(&w, &intent)[0].2, StepOutcome::Uncertain {});
        request_cancel(&w.objects, &intent, NOW).unwrap();
        let guards = Guards::new(w.memory.clone(), TARGET);
        arm.polls(running(3));
        assert_eq!(
            d.cleanup(&w.objects, &guards).unwrap_err().code,
            "cloud_outcome_unknown"
        );
        assert_eq!(
            w.ops.release(&w.objects, &intent, NOW).unwrap_err().code,
            "cleanup_pending"
        );
        // A terminal failure settles the conflict (never `created`).
        arm.polls([Ok(Poll::Failed("Conflict".into()))]);
        d.cleanup(&w.objects, &guards).unwrap();
        assert_eq!(steps(&w, &intent)[0].2, settled("conflict"));
        assert_eq!(arm.count(&Verb::Delete), 0);
        assert_eq!(
            w.ops.release(&w.objects, &intent, NOW).unwrap().outcome(),
            &TerminalOutcome::Cancelled
        );
    }

    #[test]
    fn a_running_delete_is_not_settled_by_an_absent_readback() {
        // A: the delete's effect is visible, but its operation still runs.
        let w = world();
        let arm = arm_with_env();
        let intent = reserved(&w);
        let d = driver(&w, &arm, &intent);
        let guards = Guards::new(w.memory.clone(), TARGET);
        d.provision(&w.objects, &app_only()).unwrap();
        request_cancel(&w.objects, &intent, NOW).unwrap();
        arm.script(Begin::ApplyRunning("arm-op-delete"));
        arm.polls(running(3));
        assert_eq!(
            d.cleanup(&w.objects, &guards).unwrap_err().code,
            "cloud_outcome_unknown"
        );
        assert!(arm.get(APP1).is_none());
        assert_eq!(
            w.ops.release(&w.objects, &intent, NOW).unwrap_err().code,
            "cleanup_pending"
        );
        d.cleanup(&w.objects, &guards).unwrap();
        assert_eq!(steps(&w, &intent)[1].2, settled("deleted"));
        assert_eq!(arm.count(&Verb::Delete), 1);
        assert_eq!(
            w.ops.release(&w.objects, &intent, NOW).unwrap().outcome(),
            &TerminalOutcome::Cancelled
        );
    }

    #[test]
    fn a_failed_delete_is_never_success_even_when_the_app_is_gone() {
        let w = world();
        let arm = arm_with_env();
        let intent = reserved(&w);
        let d = driver(&w, &arm, &intent);
        let guards = Guards::new(w.memory.clone(), TARGET);
        d.provision(&w.objects, &app_only()).unwrap();
        request_cancel(&w.objects, &intent, NOW).unwrap();
        arm.script(Begin::ApplyRunning("arm-op-delete"));
        arm.polls([Ok(Poll::Failed("InternalError".into()))]);
        assert_eq!(
            d.cleanup(&w.objects, &guards).unwrap_err().code,
            "cloud_action_failed"
        );
        assert_eq!(steps(&w, &intent)[1].2, settled("failed:InternalError"));
        assert_eq!(
            w.ops.release(&w.objects, &intent, NOW).unwrap_err().code,
            "cleanup_pending"
        );
    }

    #[test]
    fn a_running_stop_is_not_settled_by_an_inactive_readback() {
        // A: the reservation is not released while the stop can still act.
        let w = world();
        let arm = MockArm::default();
        let (_, receipt) = activated(&w, &arm);
        let intent = retirement(&w, &receipt);
        let d = driver(&w, &arm, &intent);
        let guards = Guards::new(w.memory.clone(), TARGET);
        d.reserve_retirement(&w.objects, NOW).unwrap();
        arm.script(Begin::ApplyRunning("arm-op-stop"));
        arm.polls(running(3));
        assert_eq!(
            d.retire(&w.objects, &guards, NOW).unwrap_err().code,
            "cloud_outcome_unknown"
        );
        assert_eq!(
            arm.get(APP1).unwrap(),
            app(&ours(T), &sha(APP1.as_bytes()), &[false])
        );
        assert_eq!(
            w.ops.release(&w.objects, &intent, NOW).unwrap_err().code,
            "cleanup_pending"
        );
        assert!(matches!(
            w.snapshot().lifecycle(T).unwrap().slot,
            Slot::Reserved { .. }
        ));
        let sealed = d.retire(&w.objects, &guards, NOW).unwrap();
        assert_eq!(sealed.outcome(), &TerminalOutcome::Retired);
        assert_eq!(arm.count(&Verb::Stop), 1);
    }

    /// A parked guard write: name, bytes and its condition's etag (`None` for
    /// create-only).
    type Parked = (String, Vec<u8>, Option<String>);

    /// Delegates to `Memory`, except that guard writes can be parked: the
    /// caller sees an unknown outcome and the conditional write lands only
    /// when the test calls [`Delayed::land`].
    struct Delayed {
        inner: Arc<Memory>,
        park: Mutex<usize>,
        parked: Mutex<Vec<Parked>>,
    }

    impl Delayed {
        fn new(inner: Arc<Memory>) -> Arc<Self> {
            Arc::new(Self {
                inner,
                park: Mutex::new(0),
                parked: Mutex::new(Vec::new()),
            })
        }

        fn park_next(&self, writes: usize) {
            *self.park.lock().unwrap() = writes;
        }

        /// Lands every parked write, in order, with its original condition.
        fn land(&self) -> Vec<Result<String, StoreError>> {
            let parked = std::mem::take(&mut *self.parked.lock().unwrap());
            parked
                .into_iter()
                .map(|(name, bytes, etag)| {
                    let cond = match &etag {
                        Some(etag) => Cond::Match(etag),
                        None => Cond::Create,
                    };
                    self.inner.put(&name, &bytes, cond)
                })
                .collect()
        }
    }

    impl ObjectStore for Delayed {
        fn get(
            &self,
            name: &str,
            max: u64,
        ) -> Result<crate::server::cloud_backup::Blob, StoreError> {
            self.inner.get(name, max)
        }

        fn download(
            &self,
            name: &str,
            out: &mut dyn std::io::Write,
            max: u64,
        ) -> Result<u64, StoreError> {
            self.inner.download(name, out, max)
        }

        fn put(&self, name: &str, bytes: &[u8], cond: Cond<'_>) -> Result<String, StoreError> {
            let mut park = self.park.lock().unwrap();
            if *park > 0 && name.ends_with(guard::OBJECT) {
                *park -= 1;
                let etag = match cond {
                    Cond::Match(etag) => Some(etag.to_owned()),
                    Cond::Create => None,
                };
                self.parked
                    .lock()
                    .unwrap()
                    .push((name.into(), bytes.to_vec(), etag));
                return Err(StoreError::Unknown("parked".into()));
            }
            drop(park);
            self.inner.put(name, bytes, cond)
        }

        fn put_file(
            &self,
            name: &str,
            path: &std::path::Path,
            cond: Cond<'_>,
        ) -> Result<String, StoreError> {
            self.inner.put_file(name, path, cond)
        }

        fn delete(&self, name: &str) -> Result<(), StoreError> {
            self.inner.delete(name)
        }

        fn list(&self, prefix: &str) -> Result<Vec<String>, StoreError> {
            self.inner.list(prefix)
        }
    }

    /// Parks the retirement's conditional hold, cancels it, and runs the
    /// cleanup; then lands the delayed hold. No late `held` may survive.
    fn delayed_hold_after_cancel(predecessor: Option<&str>) {
        let w = world();
        let arm = MockArm::default();
        let (_, receipt) = activated(&w, &arm);
        let store = Delayed::new(w.memory.clone());
        let guards = Guards::new(store.clone(), TARGET);
        if let Some(predecessor) = predecessor {
            guards.hold(T, predecessor).unwrap();
            guards.release(T, predecessor).unwrap();
        }
        let before = guards.current().unwrap();
        let intent = retirement(&w, &receipt);
        let d = driver(&w, &arm, &intent);
        d.reserve_retirement(&w.objects, NOW).unwrap();
        store.park_next(1);
        assert_eq!(
            d.retire(&w.objects, &guards, NOW).unwrap_err().code,
            "lifecycle_guard_indeterminate"
        );
        // The hold has not landed yet: the guard reads as before.
        assert_eq!(guards.current().unwrap(), before);
        request_cancel(&w.objects, &intent, NOW).unwrap();
        // A fencing write whose own outcome is unknown proves nothing: the
        // seal stays blocked.
        store.park_next(1);
        assert!(d.cleanup(&w.objects, &guards).is_err());
        assert_eq!(
            w.ops.release(&w.objects, &intent, NOW).unwrap_err().code,
            "cleanup_pending"
        );
        d.cleanup(&w.objects, &guards).unwrap();
        assert_eq!(
            w.ops.release(&w.objects, &intent, NOW).unwrap().outcome(),
            &TerminalOutcome::Cancelled
        );
        // The delayed conditional writes land only now; each is fenced.
        for landed in store.land() {
            assert_eq!(landed, Err(StoreError::Conflict));
        }
        let after = guards.current().unwrap().unwrap();
        assert_eq!(after.state, guard::State::Released);
        assert!(guards.check_transfer().is_ok());
        assert!(w.snapshot().record_for(T).is_some());
    }

    #[test]
    fn a_delayed_hold_cannot_land_after_cleanup_when_no_guard_existed() {
        delayed_hold_after_cancel(None);
    }

    #[test]
    fn a_delayed_hold_cannot_land_after_cleanup_over_a_released_predecessor() {
        delayed_hold_after_cancel(Some("fedcba9876543210fedcba9876543210"));
    }

    // ------------------------------- review corrections, round 3 (277c6f77)

    #[test]
    fn an_overlapping_cleanup_cannot_settle_a_running_delete_as_absent() {
        // A: cleanup A finds no delete step and pauses before its readback;
        // cleanup B of the same attempt records and dispatches the delete,
        // whose effect is visible while it still runs. A's later record
        // reconciles B's step: absence is no proof, only a terminal poll is.
        let w = Arc::new(world());
        let arm = Arc::new(arm_with_env());
        let intent = reserved(&w);
        let guards = Guards::new(w.memory.clone(), TARGET);
        driver(&w, &arm, &intent)
            .provision(&w.objects, &app_only())
            .unwrap();
        request_cancel(&w.objects, &intent, NOW).unwrap();
        let (bw, barm, bintent) = (w.clone(), arm.clone(), intent.clone());
        arm.on_read(move || {
            let guards = Guards::new(bw.memory.clone(), TARGET);
            barm.script(Begin::ApplyRunning("arm-op-delete"));
            barm.polls(running(3));
            assert_eq!(
                driver(&bw, &barm, &bintent)
                    .cleanup(&bw.objects, &guards)
                    .unwrap_err()
                    .code,
                "cloud_outcome_unknown"
            );
            assert!(barm.get(APP1).is_none());
        });
        arm.polls(running(3));
        assert_eq!(
            driver(&w, &arm, &intent)
                .cleanup(&w.objects, &guards)
                .unwrap_err()
                .code,
            "cloud_outcome_unknown"
        );
        assert_eq!(steps(&w, &intent)[1].2, StepOutcome::Uncertain {});
        assert_eq!(
            w.ops.release(&w.objects, &intent, NOW).unwrap_err().code,
            "cleanup_pending"
        );
        // Only the terminal poll of B's recorded operation settles it.
        driver(&w, &arm, &intent)
            .cleanup(&w.objects, &guards)
            .unwrap();
        assert_eq!(steps(&w, &intent)[1].2, settled("deleted"));
        assert_eq!(arm.count(&Verb::Delete), 1);
        assert_eq!(
            w.ops.release(&w.objects, &intent, NOW).unwrap().outcome(),
            &TerminalOutcome::Cancelled
        );
    }

    #[test]
    fn an_overlapping_cleanup_accepts_a_delete_another_executor_settled() {
        // A: when B settles its delete in between, A's record finds it
        // settled and reports B's outcome (never a separate absence).
        let w = Arc::new(world());
        let arm = Arc::new(arm_with_env());
        let intent = reserved(&w);
        let guards = Guards::new(w.memory.clone(), TARGET);
        driver(&w, &arm, &intent)
            .provision(&w.objects, &app_only())
            .unwrap();
        request_cancel(&w.objects, &intent, NOW).unwrap();
        let (bw, barm, bintent) = (w.clone(), arm.clone(), intent.clone());
        arm.on_read(move || {
            let guards = Guards::new(bw.memory.clone(), TARGET);
            driver(&bw, &barm, &bintent)
                .cleanup(&bw.objects, &guards)
                .unwrap();
        });
        driver(&w, &arm, &intent)
            .cleanup(&w.objects, &guards)
            .unwrap();
        assert_eq!(steps(&w, &intent)[1].2, settled("deleted"));
        assert_eq!(arm.count(&Verb::Delete), 1);
        assert_eq!(
            w.ops.release(&w.objects, &intent, NOW).unwrap().outcome(),
            &TerminalOutcome::Cancelled
        );
    }

    #[test]
    fn an_overlapping_create_never_adopts_or_settles_another_executors_create() {
        // A (audit): provision A finds no create step and pauses before its
        // pre-dispatch readback; B dispatches the create, still running. A
        // refuses without settling anything, and the cancelled seal waits
        // for B's recorded operation.
        let w = Arc::new(world());
        let arm = Arc::new(arm_with_env());
        let intent = reserved(&w);
        let guards = Guards::new(w.memory.clone(), TARGET);
        let (bw, barm, bintent) = (w.clone(), arm.clone(), intent.clone());
        arm.on_read(move || {
            barm.script(Begin::ApplyRunning("arm-op-create"));
            barm.polls(running(3));
            assert_eq!(
                driver(&bw, &barm, &bintent)
                    .provision(&bw.objects, &app_only())
                    .unwrap_err()
                    .code,
                "cloud_outcome_unknown"
            );
        });
        assert!(
            driver(&w, &arm, &intent)
                .provision(&w.objects, &app_only())
                .is_err()
        );
        assert_eq!(steps(&w, &intent)[0].2, StepOutcome::Uncertain {});
        assert_eq!(
            arm.count(&Verb::Put {
                config_digest: sha(APP1.as_bytes())
            }),
            1
        );
        request_cancel(&w.objects, &intent, NOW).unwrap();
        arm.polls(running(3));
        assert_eq!(
            driver(&w, &arm, &intent)
                .cleanup(&w.objects, &guards)
                .unwrap_err()
                .code,
            "cloud_outcome_unknown"
        );
        assert_eq!(
            w.ops.release(&w.objects, &intent, NOW).unwrap_err().code,
            "cleanup_pending"
        );
        driver(&w, &arm, &intent)
            .cleanup(&w.objects, &guards)
            .unwrap();
        assert_eq!(steps(&w, &intent)[0].2, settled("created"));
        assert!(arm.get(APP1).is_none());
        w.ops.release(&w.objects, &intent, NOW).unwrap();
    }

    /// Cancels, cleans up and seals retirement `a` as a second executor of
    /// the same attempt, then runs successor retirement B through a hold,
    /// a cancellation and its cleanup. Returns B's operation ID.
    fn cancel_a_then_cycle_b(w: &World, arm: &MockArm, a: &Intent, receipt: &str) -> String {
        let guards = Guards::new(w.memory.clone(), TARGET);
        request_cancel(&w.objects, a, NOW).unwrap();
        driver(w, arm, a).cleanup(&w.objects, &guards).unwrap();
        assert_eq!(
            w.ops.release(&w.objects, a, NOW).unwrap().outcome(),
            &TerminalOutcome::Cancelled
        );
        let b = retirement_keyed(w, "retire-b", receipt);
        let db = driver(w, arm, &b);
        db.reserve_retirement(&w.objects, NOW).unwrap();
        w.memory.fault(Op::Put, CONTROL, Fault::Before, 0, 1);
        db.retire(&w.objects, &guards, NOW).unwrap_err();
        let held = guards.current().unwrap().unwrap();
        assert_eq!(
            (held.operation_id.as_str(), held.state),
            (b.operation_id.as_str(), guard::State::Held)
        );
        request_cancel(&w.objects, &b, NOW).unwrap();
        db.cleanup(&w.objects, &guards).unwrap();
        assert_eq!(
            w.ops.release(&w.objects, &b, NOW).unwrap().outcome(),
            &TerminalOutcome::Cancelled
        );
        b.operation_id
    }

    /// B: retirement A journals its hold and pauses at `pause` (just before
    /// its guard read, or just before its conditional guard write). A
    /// second executor of A cancels, fences and seals it; successor B holds
    /// and is cancelled and cleaned up. When A resumes it must not acquire
    /// B's (or any later) guard version.
    fn late_hold_after_successor(pause: Op) {
        let w = Arc::new(world());
        let arm = Arc::new(MockArm::default());
        let (_, receipt) = activated(&w, &arm);
        let a = retirement_keyed(&w, "retire-a", &receipt);
        let guards = Guards::new(w.memory.clone(), TARGET);
        driver(&w, &arm, &a)
            .reserve_retirement(&w.objects, NOW)
            .unwrap();
        let route = w.snapshot().record_for(T).cloned();
        let b = Arc::new(Mutex::new(String::new()));
        let (hw, harm, ha, hb) = (w.clone(), arm.clone(), a.clone(), b.clone());
        w.memory.hook(pause, guard::OBJECT, move || {
            *hb.lock().unwrap() = cancel_a_then_cycle_b(&hw, &harm, &ha, &receipt);
        });
        assert!(
            driver(&w, &arm, &a)
                .retire(&w.objects, &guards, NOW)
                .is_err()
        );
        let after = guards.current().unwrap().unwrap();
        assert_eq!(
            (after.operation_id.as_str(), after.state),
            (b.lock().unwrap().as_str(), guard::State::Released)
        );
        assert!(guards.check_transfer().is_ok());
        assert_eq!(w.snapshot().record_for(T).cloned(), route);
        // A stays terminal: a rerun cannot hold either.
        let _ = driver(&w, &arm, &a).retire(&w.objects, &guards, NOW);
        assert_eq!(guards.current().unwrap().unwrap(), after);
        assert!(guards.check_transfer().is_ok());
        assert_eq!(arm.count(&Verb::Stop), 0);
    }

    #[test]
    fn a_hold_paused_before_its_guard_read_cannot_acquire_a_successor_version() {
        late_hold_after_successor(Op::Get);
    }

    #[test]
    fn a_hold_paused_before_its_guard_write_cannot_acquire_a_successor_version() {
        late_hold_after_successor(Op::Put);
    }

    #[test]
    fn a_hold_interrupted_before_binding_resumes_or_is_fenced() {
        // B: a crash after the hold was journaled but before its version
        // was bound; a retry binds and holds, a cancellation fences it.
        for cancel in [false, true] {
            let w = world();
            let arm = MockArm::default();
            let (_, receipt) = activated(&w, &arm);
            let intent = retirement(&w, &receipt);
            let d = driver(&w, &arm, &intent);
            let guards = Guards::new(w.memory.clone(), TARGET);
            d.reserve_retirement(&w.objects, NOW).unwrap();
            w.memory.fault(Op::Get, guard::OBJECT, Fault::Refuse, 0, 1);
            assert_eq!(
                d.retire(&w.objects, &guards, NOW).unwrap_err().code,
                "lifecycle_guard_unavailable"
            );
            assert!(guards.current().unwrap().is_none());
            if cancel {
                request_cancel(&w.objects, &intent, NOW).unwrap();
                d.cleanup(&w.objects, &guards).unwrap();
                w.ops.release(&w.objects, &intent, NOW).unwrap();
                assert!(guards.check_transfer().is_ok());
                assert!(w.snapshot().record_for(T).is_some());
            } else {
                let sealed = d.retire(&w.objects, &guards, NOW).unwrap();
                assert_eq!(sealed.outcome(), &TerminalOutcome::Retired);
                assert_eq!(guards.current().unwrap().unwrap().state, guard::State::Held);
            }
        }
    }

    #[test]
    fn an_unknown_hold_write_is_retried_against_its_bound_version() {
        // B: the hold landed but its acknowledgement was lost; a retry
        // recognises its own hold, and a retry of an unapplied write reuses
        // the bound version (never a refreshed one).
        for fault in [Fault::After, Fault::Before] {
            let w = world();
            let arm = MockArm::default();
            let (_, receipt) = activated(&w, &arm);
            let intent = retirement(&w, &receipt);
            let d = driver(&w, &arm, &intent);
            let guards = Guards::new(w.memory.clone(), TARGET);
            d.reserve_retirement(&w.objects, NOW).unwrap();
            w.memory.fault(Op::Put, guard::OBJECT, fault, 0, 1);
            assert_eq!(
                d.retire(&w.objects, &guards, NOW).unwrap_err().code,
                "lifecycle_guard_indeterminate"
            );
            let sealed = d.retire(&w.objects, &guards, NOW).unwrap();
            assert_eq!(sealed.outcome(), &TerminalOutcome::Retired);
            let held = guards.current().unwrap().unwrap();
            assert_eq!(
                (held.operation_id.as_str(), held.state),
                (intent.operation_id.as_str(), guard::State::Held)
            );
        }
    }

    // ----------------------------------------------------------- EPIC-005

    mod epic_005 {
        //! design-tenant-provisioning EPIC-005 (ITEM-012/013) over the same
        //! scripted ARM port, local fixtures and test-only proofs.
        use super::*;
        use crate::server::authority::{Phase, Record as Authority};
        use crate::server::cloud_backup::{MAX_PINS, PinKind};
        use crate::server::control_state::ReadRouting;
        use aca::{FleetObservation, PinScope, ReplicaStatus, ReplicaStopped};

        const READER: &str =
            "/subscriptions/s/resourceGroups/g/providers/Microsoft.App/containerApps/r1";
        const ENDPOINT: &str = "https://reader-t.internal";
        const RPROFILE: &[u8] = br#"{"reader":"profile"}"#;
        const RULE: &[u8] = br#"{"scale":"rule"}"#;
        const HEAD: &str = "20260101T000000Z-0123456789abcdef";
        const BASE: &str = "20251231T000000Z-fedcba9876543210";

        fn present(min_replicas: u32, max_replicas: u32) -> Fleet {
            Fleet::Present {
                min_replicas,
                max_replicas,
                reader_profile: sha(RPROFILE),
                scaling_rule: sha(RULE),
                read_routing: ReadRouting::WriterOnly,
            }
        }

        fn readers_for(w: &World, tenant: &str, key: &str, fleet: Fleet) -> Intent {
            let contents = match fleet {
                Fleet::Present { .. } => vec![RPROFILE.to_vec(), RULE.to_vec()],
                Fleet::Absent {} => Vec::new(),
            };
            let payload = Payload::SetReaders {
                tenant_id: tenant.into(),
                fleet,
            };
            w.ops
                .accept(ADMIN, key, &payload, &contents, NOW)
                .unwrap()
                .intent
        }

        fn readers(w: &World, key: &str, fleet: Fleet) -> Intent {
            readers_for(w, T, key, fleet)
        }

        fn reader_app(config: &str) -> Preflighted {
            ready(vec![Desired {
                resource: READER.into(),
                config_digest: sha(config.as_bytes()),
                app: true,
                ownership: Ownership::Owned,
            }])
        }

        fn authority(head: &str, epoch: u64) -> Authority {
            Authority {
                schema: "vlite.writer-authority/1".into(),
                installation_id: "install".into(),
                scope_id: "scope".into(),
                lineage: format!("{:016x}", 0x1234u64),
                writer_id: Some("a".repeat(32)),
                epoch,
                revision: "b".repeat(32),
                phase: Phase::Serving,
                base: None,
                head: Some(HeadRef {
                    epoch,
                    set_id: head.into(),
                    manifest_sha256: "c".repeat(64),
                    index_sha256: None,
                    sequence: 1,
                    chain_sha256: "d".repeat(64),
                }),
                claimed_unix_ms: Some(NOW),
            }
        }

        fn replica(name: &str) -> ReplicaStatus {
            ReplicaStatus {
                replica: name.into(),
                app: READER.into(),
                revision: "r0".into(),
                set_id: HEAD.into(),
                lineage: 0x1234,
                binding_generation: 3,
                epoch: 1,
                age_ms: 100,
                bound_ms: 1_000,
                failed: false,
            }
        }

        fn fleet(replicas: usize) -> FleetObservation {
            fleet_at(replicas, "r0")
        }

        /// `replicas` replicas serving the reader app's revision `revision`
        /// (r0 once created, r{n} after the n-th in-place update).
        fn fleet_at(replicas: usize, revision: &str) -> FleetObservation {
            FleetObservation::for_test(
                authority(HEAD, 1),
                (0..replicas)
                    .map(|i| ReplicaStatus {
                        revision: revision.into(),
                        ..replica(&format!("replica-{i}"))
                    })
                    .collect(),
            )
        }

        /// This tenant's reader replica `name`, proven stopped at NOW.
        fn stopped(name: &str) -> ReplicaStopped {
            ReplicaStopped::for_test(T, READER, name, NOW)
        }

        fn route(w: &World) -> Route {
            w.snapshot().record_for(T).unwrap().clone()
        }

        fn digest(w: &World) -> String {
            crate::server::control_state::route_digest(&route(w))
        }

        fn held_receipt(w: &World) -> bool {
            matches!(
                w.snapshot().lifecycle(T).map(|e| &e.slot),
                Some(Slot::Reserved {
                    receipt: Some(_),
                    ..
                })
            )
        }

        fn receipt_of(sealed: &Sealed) -> String {
            match sealed.outcome() {
                TerminalOutcome::Succeeded { receipt } => admin::receipt_digest(receipt),
                other => panic!("not succeeded: {other:?}"),
            }
        }

        fn add(
            w: &World,
            arm: &MockArm,
            intent: &Intent,
            config: &str,
            observed: &FleetObservation,
        ) -> Result<Sealed, ApiError> {
            let d = driver(w, arm, intent);
            d.reserve_readers(&w.objects, Some(ENDPOINT), NOW)?;
            d.set_readers(
                &w.objects,
                Some(&reader_app(config)),
                Some(&PinScope::for_test()),
                Some(observed),
                NOW,
            )
        }

        /// The writer activated (EPIC-004), then a first reader fleet
        /// (1..=2) added: the activation's and the readers' receipts.
        fn with_reader(w: &World, arm: &MockArm) -> (String, String) {
            let (_, activation) = activated(w, arm);
            let intent = readers(w, "readers-1", present(1, 2));
            let sealed = add(w, arm, &intent, "v1", &fleet(1)).unwrap();
            (activation, receipt_of(&sealed))
        }

        /// The route keeps the writer's identity and stays writer-only.
        fn assert_writer_unchanged(w: &World, reader: Option<&str>) {
            let route = route(w);
            let writer = writer_route(T);
            assert_eq!(
                (
                    &route.writer,
                    route.lineage,
                    route.epoch,
                    route.binding_generation
                ),
                (
                    &writer.writer,
                    writer.lineage,
                    writer.epoch,
                    writer.binding_generation
                )
            );
            assert_eq!(route.reader.as_deref(), reader);
            assert_eq!(route.read_routing, ReadRouting::WriterOnly);
        }

        fn writer_calls(arm: &MockArm) -> usize {
            arm.calls().iter().filter(|c| c.resource == APP1).count()
        }

        // ----------------------------------------------------- ITEM-012

        #[test]
        fn a_first_reader_is_created_privately_and_its_endpoint_published_writer_only() {
            let w = world();
            let arm = MockArm::default();
            let (_, activation) = activated(&w, &arm);
            let before = digest(&w);
            let intent = readers(&w, "readers-1", present(1, 2));
            let sealed = add(&w, &arm, &intent, "v1", &fleet(2)).unwrap();
            assert_ne!(receipt_of(&sealed), activation);
            assert_writer_unchanged(&w, Some(ENDPOINT));
            assert_ne!(digest(&w), before);
            // Only the reader app was created; the writer app is untouched.
            assert_eq!(writer_calls(&arm), 1);
            let reader_puts: Vec<_> = arm
                .calls()
                .into_iter()
                .filter(|c| c.resource == READER)
                .collect();
            assert_eq!(reader_puts.len(), 1);
            assert!(matches!(reader_puts[0].verb, Verb::Put { .. }));
            assert!(steps(&w, &intent).contains(&(
                format!("fleet/{READER}"),
                "eligible".into(),
                settled("eligible")
            )));
            assert!(matches!(
                w.snapshot().lifecycle(T).map(|e| &e.slot),
                Some(Slot::Released { .. })
            ));
            // A retry returns the same sealed outcome with no new effect.
            let calls = arm.calls().len();
            let again = add(&w, &arm, &intent, "v1", &fleet(2)).unwrap_err();
            assert_eq!(again.code, "operation_terminal");
            assert_eq!(arm.calls().len(), calls);
        }

        #[test]
        fn a_same_endpoint_scale_updates_the_reader_in_place_and_keeps_the_route() {
            let w = world();
            let arm = MockArm::default();
            let (_, first) = with_reader(&w, &arm);
            let before = digest(&w);
            let intent = readers(&w, "readers-2", present(2, 4));
            let sealed = add(&w, &arm, &intent, "v2", &fleet_at(3, "r1")).unwrap();
            assert_ne!(receipt_of(&sealed), first);
            assert_eq!(digest(&w), before);
            assert_writer_unchanged(&w, Some(ENDPOINT));
            let update = Verb::Update {
                config_digest: sha(b"v2"),
            };
            assert_eq!(arm.count(&update), 1);
            assert_eq!(
                arm.calls()
                    .iter()
                    .filter(|c| matches!(c.verb, Verb::Put { .. }))
                    .count(),
                2,
                "the writer and the first reader only"
            );
            assert_eq!(arm.get(READER).unwrap().config_digest, sha(b"v2"));
            assert_eq!(writer_calls(&arm), 1);
        }

        #[test]
        fn reader_routing_and_endpoint_changes_are_refused_before_any_effect() {
            let w = world();
            let arm = MockArm::default();
            with_reader(&w, &arm);
            let calls = arm.calls().len();
            let routed = readers(
                &w,
                "routed",
                Fleet::Present {
                    min_replicas: 1,
                    max_replicas: 2,
                    reader_profile: sha(RPROFILE),
                    scaling_rule: sha(RULE),
                    read_routing: ReadRouting::Readers,
                },
            );
            let refused = driver(&w, &arm, &routed)
                .reserve_readers(&w.objects, Some(ENDPOINT), NOW)
                .unwrap_err();
            assert_eq!(refused.code, "reader_routing_unqualified");
            let moved = readers(&w, "moved", present(1, 2));
            let refused = driver(&w, &arm, &moved)
                .reserve_readers(&w.objects, Some("https://elsewhere.internal"), NOW)
                .unwrap_err();
            assert_eq!(refused.code, "reader_endpoint_changed");
            let unnamed = readers(&w, "unnamed", present(1, 2));
            assert!(
                driver(&w, &arm, &unnamed)
                    .reserve_readers(&w.objects, None, NOW)
                    .is_err()
            );
            let absent = readers(&w, "absent", Fleet::Absent {});
            assert!(
                driver(&w, &arm, &absent)
                    .reserve_readers(&w.objects, Some(ENDPOINT), NOW)
                    .is_err()
            );
            assert_eq!(arm.calls().len(), calls);
            assert!(matches!(
                w.snapshot().lifecycle(T).map(|e| &e.slot),
                Some(Slot::Released { .. })
            ));
        }

        #[test]
        fn a_route_not_published_by_a_sealed_lifecycle_operation_is_unanchored() {
            let w = world();
            let intent = readers_for(&w, OTHER, "other", present(1, 2));
            let arm = MockArm::default();
            let refused = driver(&w, &arm, &intent)
                .reserve_readers(&w.objects, Some(ENDPOINT), NOW)
                .unwrap_err();
            assert_eq!(refused.code, "readers_unanchored");
            assert!(w.snapshot().lifecycle(OTHER).is_none());
        }

        #[test]
        fn missing_pin_scope_profile_or_observation_is_refused_and_never_published() {
            let w = world();
            let arm = MockArm::default();
            activated(&w, &arm);
            let before = digest(&w);
            let calls = arm.calls().len();
            let intent = readers(&w, "readers-1", present(1, 2));
            let d = driver(&w, &arm, &intent);
            d.reserve_readers(&w.objects, Some(ENDPOINT), NOW).unwrap();
            let profile = reader_app("v1");
            let scope = PinScope::for_test();
            let observed = fleet(1);
            let refused = d
                .set_readers(&w.objects, Some(&profile), None, Some(&observed), NOW)
                .unwrap_err();
            assert_eq!(refused.code, "pin_scope_unproven");
            assert_eq!(refused.status, axum::http::StatusCode::CONFLICT);
            assert_eq!(arm.calls().len(), calls, "no ARM call without pin scope");
            let refused = d
                .set_readers(&w.objects, None, Some(&scope), Some(&observed), NOW)
                .unwrap_err();
            assert_eq!(refused.code, "profile_unsupported");
            assert_eq!(arm.calls().len(), calls);
            let refused = d
                .set_readers(&w.objects, Some(&profile), Some(&scope), None, NOW)
                .unwrap_err();
            assert_eq!(refused.code, "fleet_observation_unavailable");
            assert_eq!(digest(&w), before);
            assert!(!held_receipt(&w));
            // The created reader stays this operation's; a later proof finishes.
            d.set_readers(
                &w.objects,
                Some(&profile),
                Some(&scope),
                Some(&observed),
                NOW,
            )
            .unwrap();
            assert_writer_unchanged(&w, Some(ENDPOINT));
        }

        #[test]
        fn ineligible_fleets_never_publish_the_endpoint_or_a_receipt() {
            let w = world();
            let arm = MockArm::default();
            activated(&w, &arm);
            let before = digest(&w);
            let intent = readers(&w, "readers-1", present(1, 2));
            let d = driver(&w, &arm, &intent);
            d.reserve_readers(&w.objects, Some(ENDPOINT), NOW).unwrap();
            let (profile, scope) = (reader_app("v1"), PinScope::for_test());
            let with = |edit: &dyn Fn(&mut ReplicaStatus)| {
                let mut one = replica("replica-0");
                edit(&mut one);
                FleetObservation::for_test(authority(HEAD, 1), vec![one])
            };
            let mut recovering = authority(HEAD, 1);
            recovering.phase = Phase::Recovering;
            recovering.head = None;
            let cases = [
                (
                    FleetObservation::for_test(recovering, vec![replica("replica-0")]),
                    "reader_base_transition",
                ),
                (
                    FleetObservation::for_test(authority(HEAD, 2), vec![replica("replica-0")]),
                    "replica_ineligible",
                ),
                // A replica still on a frozen base / replaced head.
                (with(&|r| r.set_id = BASE.into()), "replica_ineligible"),
                (with(&|r| r.lineage = 0x99), "replica_ineligible"),
                (with(&|r| r.binding_generation = 4), "replica_ineligible"),
                (with(&|r| r.epoch = 2), "replica_ineligible"),
                (with(&|r| r.age_ms = 1_001), "replica_ineligible"),
                (with(&|r| r.bound_ms = 0), "replica_ineligible"),
                (with(&|r| r.failed = true), "replica_ineligible"),
                // A replica of another app (the writer's) or of a revision
                // that is not the reader app's single active one.
                (with(&|r| r.app = APP1.into()), "replica_ineligible"),
                (with(&|r| r.revision = "r1".into()), "replica_ineligible"),
                (
                    FleetObservation::for_test(
                        authority(HEAD, 1),
                        vec![replica("replica-0"), replica("replica-0")],
                    ),
                    "replica_identity_conflict",
                ),
                (fleet(0), "reader_fleet_below_min"),
                (fleet(3), "reader_fleet_not_reduced"),
            ];
            for (observed, code) in cases {
                let refused = d
                    .set_readers(
                        &w.objects,
                        Some(&profile),
                        Some(&scope),
                        Some(&observed),
                        NOW,
                    )
                    .unwrap_err();
                assert_eq!(refused.code, code);
                assert_eq!(digest(&w), before, "{code}");
                assert!(!held_receipt(&w), "{code}");
            }
            d.set_readers(
                &w.objects,
                Some(&profile),
                Some(&scope),
                Some(&fleet(2)),
                NOW,
            )
            .unwrap();
            assert_writer_unchanged(&w, Some(ENDPOINT));
        }

        // ----------------------------------------------------- ITEM-013

        /// After an in-place update, only replicas of the reader app's new
        /// single active revision count.
        #[test]
        fn replicas_still_on_the_replaced_revision_are_not_eligible() {
            let w = world();
            let arm = MockArm::default();
            with_reader(&w, &arm);
            let intent = readers(&w, "readers-2", present(1, 2));
            // The update made r1 the single active revision; r0 replicas
            // are the replaced configuration.
            let refused = add(&w, &arm, &intent, "v2", &fleet_at(1, "r0")).unwrap_err();
            assert_eq!(refused.code, "replica_ineligible");
            assert!(!held_receipt(&w));
            add(&w, &arm, &intent, "v2", &fleet_at(1, "r1")).unwrap();
        }

        /// A Present reduction updates the reader app in place while its
        /// endpoint stays published (nothing is withdrawn first); it seals
        /// only once the reduced fleet is observed. Reads stay on the writer
        /// only because reader routing is disabled (`WriterOnly`), not
        /// because the reduction is withdrawn: withdraw-before-reduction is
        /// an ITEM-021 prerequisite.
        #[test]
        fn a_present_scale_in_updates_in_place_and_seals_only_once_reduced_while_routing_is_disabled()
         {
            let w = world();
            let arm = MockArm::default();
            with_reader(&w, &arm);
            let intent = readers(&w, "readers-2", present(1, 1));
            let refused = add(&w, &arm, &intent, "v1", &fleet_at(2, "r1")).unwrap_err();
            assert_eq!(refused.code, "reader_fleet_not_reduced");
            // The endpoint is still published while the app was updated.
            assert_writer_unchanged(&w, Some(ENDPOINT));
            assert_eq!(route(&w).read_routing, ReadRouting::WriterOnly);
            assert!(!held_receipt(&w));
            add(&w, &arm, &intent, "v1", &fleet_at(1, "r1")).unwrap();
            assert_writer_unchanged(&w, Some(ENDPOINT));
        }

        #[test]
        fn removing_readers_unpublishes_the_endpoint_before_stopping_and_cannot_be_cancelled() {
            let w = world();
            let arm = MockArm::default();
            with_reader(&w, &arm);
            let intent = readers(&w, "readers-off", Fleet::Absent {});
            let d = driver(&w, &arm, &intent);
            d.reserve_readers(&w.objects, None, NOW).unwrap();
            arm.script(Begin::ApplyRunning("arm-op-stop"));
            arm.polls(running(3));
            let refused = d
                .set_readers(&w.objects, None, None, None, NOW)
                .unwrap_err();
            assert_eq!(refused.code, "cloud_outcome_unknown");
            // The endpoint left the route before the stop was dispatched.
            assert_writer_unchanged(&w, None);
            assert!(!held_receipt(&w));
            assert_eq!(
                request_cancel(&w.objects, &intent, NOW).unwrap_err().code,
                "readers_in_progress"
            );
            assert_eq!(
                w.ops.release(&w.objects, &intent, NOW).unwrap_err().code,
                "operation_not_terminal"
            );
            let sealed = d.set_readers(&w.objects, None, None, None, NOW).unwrap();
            receipt_of(&sealed);
            assert_writer_unchanged(&w, None);
            assert_eq!(arm.count(&Verb::Stop), 1);
            assert_eq!(writer_calls(&arm), 1, "the writer is never stopped");
            assert!(matches!(
                arm.get(READER).unwrap().revisions,
                Revisions::Complete(r) if r.iter().all(|r| !r.active)
            ));
        }

        #[test]
        fn a_cancelled_reader_add_deletes_only_its_reader_and_the_next_one_anchors() {
            let w = world();
            let arm = MockArm::default();
            activated(&w, &arm);
            let before = digest(&w);
            let intent = readers(&w, "readers-1", present(1, 2));
            let d = driver(&w, &arm, &intent);
            d.reserve_readers(&w.objects, Some(ENDPOINT), NOW).unwrap();
            let (profile, scope) = (reader_app("v1"), PinScope::for_test());
            d.set_readers(&w.objects, Some(&profile), Some(&scope), None, NOW)
                .unwrap_err();
            request_cancel(&w.objects, &intent, NOW).unwrap();
            let guards = Guards::new(w.memory.clone(), TARGET);
            d.cleanup(&w.objects, &guards).unwrap();
            let sealed = w.ops.release(&w.objects, &intent, NOW).unwrap();
            assert_eq!(sealed.outcome(), &TerminalOutcome::Cancelled);
            assert!(arm.get(READER).is_none());
            assert_eq!(digest(&w), before);
            assert_eq!(writer_calls(&arm), 1);
            let next = readers(&w, "readers-2", present(1, 2));
            add(&w, &arm, &next, "v1", &fleet(1)).unwrap();
            assert_writer_unchanged(&w, Some(ENDPOINT));
        }

        fn retire_with(w: &World, key: &str, expected: &str) -> Intent {
            w.ops
                .accept(
                    ADMIN,
                    key,
                    &Payload::Retire {
                        tenant_id: T.into(),
                        expected_receipt: expected.into(),
                        reason: "offboarded".into(),
                        data_disposition: DataDisposition::Retain,
                    },
                    &[],
                    NOW,
                )
                .unwrap()
                .intent
        }

        #[test]
        fn retirement_after_readers_names_the_latest_receipt_and_stops_both_apps() {
            let w = world();
            let arm = MockArm::default();
            let (activation, latest) = with_reader(&w, &arm);
            let stale = retire_with(&w, "retire-stale", &activation);
            assert_eq!(
                driver(&w, &arm, &stale)
                    .reserve_retirement(&w.objects, NOW)
                    .unwrap_err()
                    .code,
                "retirement_receipt_mismatch"
            );
            let intent = retire_with(&w, "retire-t", &latest);
            let d = driver(&w, &arm, &intent);
            d.reserve_retirement(&w.objects, NOW).unwrap();
            let guards = Guards::new(w.memory.clone(), TARGET);
            let sealed = d.retire(&w.objects, &guards, NOW).unwrap();
            assert_eq!(sealed.outcome(), &TerminalOutcome::Retired);
            assert_eq!(arm.count(&Verb::Stop), 2);
            for app in [APP1, READER] {
                assert!(matches!(
                    arm.get(app).unwrap().revisions,
                    Revisions::Complete(r) if r.iter().all(|r| !r.active)
                ));
            }
        }

        #[test]
        fn an_overlapping_same_attempt_removal_stops_the_reader_once() {
            let w = Arc::new(world());
            let arm = Arc::new(MockArm::default());
            with_reader(&w, &arm);
            let intent = readers(&w, "readers-off", Fleet::Absent {});
            driver(&w, &arm, &intent)
                .reserve_readers(&w.objects, None, NOW)
                .unwrap();
            let (bw, barm, bintent) = (w.clone(), arm.clone(), intent.clone());
            arm.on_read(move || {
                let sealed = driver(&bw, &barm, &bintent)
                    .set_readers(&bw.objects, None, None, None, NOW)
                    .unwrap();
                receipt_of(&sealed);
            });
            let sealed = driver(&w, &arm, &intent)
                .set_readers(&w.objects, None, None, None, NOW)
                .unwrap();
            receipt_of(&sealed);
            assert_eq!(arm.count(&Verb::Stop), 1);
            assert_writer_unchanged(&w, None);
        }

        // ------------------------------------------- abandoned reader pins

        #[test]
        fn abandoned_pins_are_released_only_for_the_proven_stopped_replica() {
            let env = crate::server::cloud_recovery::tests::setup();
            let w = world();
            let arm = MockArm::default();
            with_reader(&w, &arm);
            let intent = readers(&w, "readers-off", Fleet::Absent {});
            let d = driver(&w, &arm, &intent);
            d.reserve_readers(&w.objects, None, NOW).unwrap();
            let pin = |reason: &str, kind, at| env.sets.pin(&env.set_id, reason, kind, at).unwrap();
            let gone = pin("reader-apply:replica-0", PinKind::Operation, NOW - 10);
            let newer = pin("reader-apply:replica-0", PinKind::Operation, NOW + 10);
            let other = pin("reader-apply:replica-1", PinKind::Operation, NOW - 10);
            let operator = pin("reader-apply:replica-0", PinKind::Operator, NOW - 10);
            let scope = PinScope::for_test();
            let stopped = stopped("replica-0");
            // A stop proof for another tenant or another app frees nothing.
            for foreign in [
                ReplicaStopped::for_test(OTHER, READER, "replica-0", NOW),
                ReplicaStopped::for_test(T, APP1, "replica-0", NOW),
            ] {
                assert_eq!(
                    d.recover_pins(&w.objects, &env.sets, Some(&scope), Some(&foreign), NOW)
                        .unwrap_err()
                        .code,
                    "replica_stop_unproven"
                );
            }
            assert_eq!(
                d.recover_pins(&w.objects, &env.sets, None, Some(&stopped), NOW)
                    .unwrap_err()
                    .code,
                "pin_scope_unproven"
            );
            assert_eq!(
                d.recover_pins(&w.objects, &env.sets, Some(&scope), None, NOW)
                    .unwrap_err()
                    .code,
                "replica_stop_unproven"
            );
            assert_eq!(
                d.recover_pins(&w.objects, &env.sets, Some(&scope), Some(&stopped), NOW)
                    .unwrap(),
                1
            );
            assert_eq!(
                d.recover_pins(&w.objects, &env.sets, Some(&scope), Some(&stopped), NOW)
                    .unwrap(),
                0
            );
            let left: Vec<String> = env
                .sets
                .list()
                .unwrap()
                .sets
                .into_iter()
                .filter_map(|s| s.record)
                .flat_map(|r| r.pins)
                .map(|p| p.id)
                .collect();
            assert!(!left.contains(&gone.id));
            for kept in [newer, other, operator] {
                assert!(left.contains(&kept.id));
            }
        }

        #[test]
        fn abandoned_reader_pin_objects_are_released_for_the_proven_stopped_replica() {
            // design-tenant-provisioning ITEM-008: isolated reader pins are
            // recovered with the same proofs as record pins.
            let env = crate::server::cloud_recovery::tests::setup();
            let w = world();
            let arm = MockArm::default();
            with_reader(&w, &arm);
            let intent = readers(&w, "readers-off", Fleet::Absent {});
            let d = driver(&w, &arm, &intent);
            d.reserve_readers(&w.objects, None, NOW).unwrap();
            let pin = |reason: &str, at| env.sets.pin_reader(&env.set_id, reason, at).unwrap();
            let gone = pin("reader-apply:replica-0", NOW - 10);
            let newer = pin("reader-apply:replica-0", NOW + 10);
            let other = pin("reader-apply:replica-1", NOW - 10);
            let legacy = env
                .sets
                .pin(
                    &env.set_id,
                    "reader-apply:replica-0",
                    PinKind::Operation,
                    NOW - 10,
                )
                .unwrap();
            let scope = PinScope::for_test();
            let stopped = stopped("replica-0");
            assert_eq!(
                d.recover_pins(&w.objects, &env.sets, None, Some(&stopped), NOW)
                    .unwrap_err()
                    .code,
                "pin_scope_unproven"
            );
            assert_eq!(env.sets.reader_pins().unwrap().len(), 3);
            assert_eq!(
                d.recover_pins(&w.objects, &env.sets, Some(&scope), Some(&stopped), NOW)
                    .unwrap(),
                2
            );
            assert_eq!(
                d.recover_pins(&w.objects, &env.sets, Some(&scope), Some(&stopped), NOW)
                    .unwrap(),
                0
            );
            let left: Vec<String> = env
                .sets
                .reader_pins()
                .unwrap()
                .into_iter()
                .map(|(_, pin)| pin.id)
                .collect();
            assert!(!left.contains(&gone.id));
            assert!(left.contains(&newer.id) && left.contains(&other.id));
            assert!(
                env.sets.list().unwrap().sets[0]
                    .record
                    .as_ref()
                    .unwrap()
                    .pins
                    .iter()
                    .all(|p| p.id != legacy.id)
            );
        }

        #[test]
        fn full_reader_pins_are_freed_by_control_owned_recovery() {
            let env = crate::server::cloud_recovery::tests::setup();
            let w = world();
            let arm = MockArm::default();
            with_reader(&w, &arm);
            let intent = readers(&w, "readers-2", present(1, 2));
            let d = driver(&w, &arm, &intent);
            d.reserve_readers(&w.objects, Some(ENDPOINT), NOW).unwrap();
            let held = env.sets.list().unwrap().sets[0]
                .record
                .as_ref()
                .unwrap()
                .pins
                .len();
            for _ in held..MAX_PINS {
                env.sets
                    .pin(
                        &env.set_id,
                        "reader-apply:replica-0",
                        PinKind::Operation,
                        NOW - 1,
                    )
                    .unwrap();
            }
            assert_eq!(
                env.sets
                    .pin(
                        &env.set_id,
                        "reader-apply:replica-1",
                        PinKind::Operation,
                        NOW
                    )
                    .unwrap_err()
                    .code,
                "cloud_pins_full"
            );
            let freed = d
                .recover_pins(
                    &w.objects,
                    &env.sets,
                    Some(&PinScope::for_test()),
                    Some(&stopped("replica-0")),
                    NOW,
                )
                .unwrap();
            assert_eq!(freed, MAX_PINS - held);
            env.sets
                .pin(
                    &env.set_id,
                    "reader-apply:replica-1",
                    PinKind::Operation,
                    NOW,
                )
                .unwrap();
        }

        #[test]
        fn pin_recovery_needs_a_held_reader_reservation() {
            let env = crate::server::cloud_recovery::tests::setup();
            let w = world();
            let arm = MockArm::default();
            with_reader(&w, &arm);
            let intent = readers(&w, "readers-2", present(1, 2));
            let refused = driver(&w, &arm, &intent)
                .recover_pins(
                    &w.objects,
                    &env.sets,
                    Some(&PinScope::for_test()),
                    Some(&stopped("replica-0")),
                    NOW,
                )
                .unwrap_err();
            assert_eq!(refused.code, "operation_not_reserved");
        }

        // ------------------------- 2e53839d review: stale settled plans (H1)

        fn released_by(w: &World) -> Option<String> {
            match w.snapshot().lifecycle(T).map(|e| e.slot.clone()) {
                Some(Slot::Released { operation_id, .. }) => Some(operation_id),
                _ => None,
            }
        }

        /// Plans `intent` against the current anchor, but its reservation
        /// write fails: a settled plan with no slot (a same-attempt
        /// executor that may retry later).
        fn plan_only(w: &World, arm: &MockArm, intent: &Intent, endpoint: Option<&str>) {
            let released = released_by(w);
            w.memory.fault(Op::Put, CONTROL, Fault::Before, 0, 1);
            let d = driver(w, arm, intent);
            match intent.kind() {
                Kind::Retire => d.reserve_retirement(&w.objects, NOW).unwrap_err(),
                _ => d.reserve_readers(&w.objects, endpoint, NOW).unwrap_err(),
            };
            assert_eq!(released_by(w), released, "no slot was taken");
        }

        /// A stale plan's retry is refused with no slot and no effect.
        fn assert_stale_refused(
            w: &World,
            arm: &MockArm,
            intent: &Intent,
            endpoint: Option<&str>,
            code: &str,
        ) {
            let (calls, released, route) = (arm.calls().len(), released_by(w), digest(w));
            let d = driver(w, arm, intent);
            let refused = match intent.kind() {
                Kind::Retire => d.reserve_retirement(&w.objects, NOW).unwrap_err(),
                _ => d.reserve_readers(&w.objects, endpoint, NOW).unwrap_err(),
            };
            assert_eq!(refused.code, code);
            assert_eq!(released_by(w), released, "the stale plan took no slot");
            assert_eq!(digest(w), route);
            assert_eq!(arm.calls().len(), calls);
            if intent.kind() == Kind::SetReaders {
                d.set_readers(
                    &w.objects,
                    Some(&reader_app("v9")),
                    Some(&PinScope::for_test()),
                    Some(&fleet(1)),
                    NOW,
                )
                .unwrap_err();
                assert_eq!(arm.calls().len(), calls, "no reader app effect");
            }
        }

        #[test]
        fn a_stale_reader_plan_never_reserves_after_another_change_sealed() {
            let w = world();
            let arm = MockArm::default();
            activated(&w, &arm);
            let a = readers(&w, "readers-a", present(1, 2));
            plan_only(&w, &arm, &a, Some(ENDPOINT));
            let b = readers(&w, "readers-b", present(1, 2));
            add(&w, &arm, &b, "v1", &fleet(1)).unwrap();
            assert_stale_refused(&w, &arm, &a, Some(ENDPOINT), "readers_unanchored");
            // A fresh change anchored at B's receipt still reserves and seals.
            let c = readers(&w, "readers-c", present(1, 3));
            add(&w, &arm, &c, "v2", &fleet_at(2, "r1")).unwrap();
            assert_eq!(released_by(&w).as_deref(), Some(c.operation_id.as_str()));
        }

        #[test]
        fn a_stale_reader_plan_is_refused_even_when_the_route_is_unchanged() {
            let w = world();
            let arm = MockArm::default();
            with_reader(&w, &arm);
            let a = readers(&w, "readers-a", present(2, 4));
            plan_only(&w, &arm, &a, Some(ENDPOINT));
            let route = digest(&w);
            let b = readers(&w, "readers-b", present(1, 3));
            let rb = receipt_of(&add(&w, &arm, &b, "v2", &fleet_at(2, "r1")).unwrap());
            assert_eq!(digest(&w), route, "a same-endpoint change keeps the route");
            assert_stale_refused(&w, &arm, &a, Some(ENDPOINT), "readers_unanchored");
            let c = readers(&w, "readers-c", present(2, 4));
            add(&w, &arm, &c, "v3", &fleet_at(2, "r2")).unwrap();
            // And a retirement naming B's receipt is stale only after C.
            let stale = retire_with(&w, "retire-b", &rb);
            assert_stale_refused(&w, &arm, &stale, None, "retirement_receipt_mismatch");
        }

        #[test]
        fn a_reader_reservation_racing_a_sealed_change_is_refused_atomically() {
            let w = Arc::new(world());
            let arm = Arc::new(MockArm::default());
            activated(&w, &arm);
            let a = readers(&w, "readers-a", present(1, 2));
            let b = readers(&w, "readers-b", present(1, 2));
            let (bw, barm) = (w.clone(), arm.clone());
            // A's checks pass; B reserves and seals before A's slot write.
            w.memory.hook(Op::Put, CONTROL, move || {
                add(&bw, &barm, &b, "v1", &fleet(1)).unwrap();
            });
            let refused = driver(&w, &arm, &a)
                .reserve_readers(&w.objects, Some(ENDPOINT), NOW)
                .unwrap_err();
            assert_eq!(refused.code, "readers_unanchored");
            assert!(matches!(
                w.snapshot().lifecycle(T).map(|e| &e.slot),
                Some(Slot::Released { .. })
            ));
            assert_stale_refused(&w, &arm, &a, Some(ENDPOINT), "readers_unanchored");
        }

        #[test]
        fn a_stale_retirement_plan_never_reserves_after_a_reader_change() {
            for same_route in [false, true] {
                let w = world();
                let arm = MockArm::default();
                let expected = if same_route {
                    with_reader(&w, &arm).1
                } else {
                    activated(&w, &arm).1
                };
                let a = retire_with(&w, "retire-a", &expected);
                plan_only(&w, &arm, &a, None);
                let route = digest(&w);
                let b = readers(&w, "readers-b", present(1, 3));
                let (config, observed) = if same_route {
                    ("v2", fleet_at(1, "r1"))
                } else {
                    ("v1", fleet(1))
                };
                let rb = receipt_of(&add(&w, &arm, &b, config, &observed).unwrap());
                assert_eq!(digest(&w) == route, same_route);
                assert_stale_refused(&w, &arm, &a, None, "retirement_receipt_mismatch");
                // A retirement naming the latest receipt still retires.
                let c = retire_with(&w, "retire-c", &rb);
                let d = driver(&w, &arm, &c);
                d.reserve_retirement(&w.objects, NOW).unwrap();
                let guards = Guards::new(w.memory.clone(), TARGET);
                let sealed = d.retire(&w.objects, &guards, NOW).unwrap();
                assert_eq!(sealed.outcome(), &TerminalOutcome::Retired);
            }
        }

        #[test]
        fn a_retirement_reservation_racing_a_reader_change_is_refused_atomically() {
            let w = Arc::new(world());
            let arm = Arc::new(MockArm::default());
            let (_, latest) = with_reader(&w, &arm);
            let a = retire_with(&w, "retire-a", &latest);
            let b = readers(&w, "readers-b", present(1, 3));
            let (bw, barm) = (w.clone(), arm.clone());
            w.memory.hook(Op::Put, CONTROL, move || {
                add(&bw, &barm, &b, "v2", &fleet_at(1, "r1")).unwrap();
            });
            let refused = driver(&w, &arm, &a)
                .reserve_retirement(&w.objects, NOW)
                .unwrap_err();
            assert_eq!(refused.code, "retirement_receipt_mismatch");
            assert!(matches!(
                w.snapshot().lifecycle(T).map(|e| &e.slot),
                Some(Slot::Released { .. })
            ));
        }

        // ----------------- 2e53839d review: fresh eligibility on retry (M1)

        #[test]
        fn a_retried_publication_needs_a_fresh_eligible_observation() {
            let w = world();
            let arm = MockArm::default();
            activated(&w, &arm);
            let before = digest(&w);
            let intent = readers(&w, "readers-1", present(1, 2));
            let d = driver(&w, &arm, &intent);
            d.reserve_readers(&w.objects, Some(ENDPOINT), NOW).unwrap();
            let (profile, scope) = (reader_app("v1"), PinScope::for_test());
            // Eligibility settles; the publication write then fails.
            w.memory.fault(Op::Put, CONTROL, Fault::Before, 0, 1);
            d.set_readers(
                &w.objects,
                Some(&profile),
                Some(&scope),
                Some(&fleet(1)),
                NOW,
            )
            .unwrap_err();
            assert!(steps(&w, &intent).contains(&(
                format!("fleet/{READER}"),
                "eligible".into(),
                settled("eligible")
            )));
            let mut stale = replica("replica-0");
            stale.set_id = BASE.into();
            let cases = [
                (None, "fleet_observation_unavailable"),
                (Some(fleet(3)), "reader_fleet_not_reduced"),
                (
                    Some(FleetObservation::for_test(authority(HEAD, 1), vec![stale])),
                    "replica_ineligible",
                ),
            ];
            for (observed, code) in cases {
                let refused = d
                    .set_readers(
                        &w.objects,
                        Some(&profile),
                        Some(&scope),
                        observed.as_ref(),
                        NOW,
                    )
                    .unwrap_err();
                assert_eq!(refused.code, code);
                assert_eq!(digest(&w), before, "{code}");
                assert!(!held_receipt(&w), "{code}");
            }
            d.set_readers(
                &w.objects,
                Some(&profile),
                Some(&scope),
                Some(&fleet(1)),
                NOW,
            )
            .unwrap();
            assert_writer_unchanged(&w, Some(ENDPOINT));
        }

        #[test]
        fn a_committed_publication_is_recovered_without_a_fresh_observation() {
            let w = world();
            let arm = MockArm::default();
            activated(&w, &arm);
            let intent = readers(&w, "readers-1", present(1, 2));
            let d = driver(&w, &arm, &intent);
            d.reserve_readers(&w.objects, Some(ENDPOINT), NOW).unwrap();
            let (profile, scope) = (reader_app("v1"), PinScope::for_test());
            // The endpoint and receipt commit; the seal is lost.
            w.memory
                .fault(Op::Put, "terminal.json", Fault::Before, 0, 1);
            d.set_readers(
                &w.objects,
                Some(&profile),
                Some(&scope),
                Some(&fleet(1)),
                NOW,
            )
            .unwrap_err();
            assert!(held_receipt(&w));
            let route = digest(&w);
            let calls = arm.calls().len();
            let sealed = d.set_readers(&w.objects, None, None, None, NOW).unwrap();
            receipt_of(&sealed);
            assert_eq!(digest(&w), route);
            assert_eq!(arm.calls().len(), calls);
            assert_writer_unchanged(&w, Some(ENDPOINT));
        }

        // ------------------------ 2e53839d review: path regressions (M5)

        #[test]
        fn a_cancelled_in_place_update_keeps_the_route_and_is_not_rolled_back() {
            let w = world();
            let arm = MockArm::default();
            with_reader(&w, &arm);
            let route = digest(&w);
            let intent = readers(&w, "readers-2", present(2, 4));
            let d = driver(&w, &arm, &intent);
            d.reserve_readers(&w.objects, Some(ENDPOINT), NOW).unwrap();
            let (profile, scope) = (reader_app("v2"), PinScope::for_test());
            let refused = d
                .set_readers(&w.objects, Some(&profile), Some(&scope), None, NOW)
                .unwrap_err();
            assert_eq!(refused.code, "fleet_observation_unavailable");
            request_cancel(&w.objects, &intent, NOW).unwrap();
            let guards = Guards::new(w.memory.clone(), TARGET);
            d.cleanup(&w.objects, &guards).unwrap();
            let sealed = w.ops.release(&w.objects, &intent, NOW).unwrap();
            assert_eq!(sealed.outcome(), &TerminalOutcome::Cancelled);
            assert_eq!(digest(&w), route);
            assert_writer_unchanged(&w, Some(ENDPOINT));
            // The tenant's reader app is kept, not deleted, and the update
            // is NOT rolled back (an ITEM-021 routing prerequisite).
            assert_eq!(arm.count(&Verb::Delete), 0);
            assert_eq!(arm.get(READER).unwrap().config_digest, sha(b"v2"));
            // The next change anchors through the cancellation and restores.
            let next = readers(&w, "readers-3", present(1, 2));
            add(&w, &arm, &next, "v1", &fleet_at(1, "r2")).unwrap();
            assert_eq!(arm.get(READER).unwrap().config_digest, sha(b"v1"));
            assert_eq!(digest(&w), route);
        }

        #[test]
        fn an_unknown_reader_update_stays_unknown_and_is_never_resent() {
            let w = world();
            let arm = MockArm::default();
            with_reader(&w, &arm);
            let route = digest(&w);
            let intent = readers(&w, "readers-2", present(2, 4));
            let d = driver(&w, &arm, &intent);
            d.reserve_readers(&w.objects, Some(ENDPOINT), NOW).unwrap();
            let (profile, scope) = (reader_app("v2"), PinScope::for_test());
            let update = Verb::Update {
                config_digest: sha(b"v2"),
            };
            arm.script(Begin::Unknown);
            for _ in 0..3 {
                let refused = d
                    .set_readers(
                        &w.objects,
                        Some(&profile),
                        Some(&scope),
                        Some(&fleet_at(2, "r1")),
                        NOW,
                    )
                    .unwrap_err();
                assert_eq!(refused.code, "cloud_outcome_unknown");
                assert_eq!(arm.count(&update), 1, "never resent");
                assert_eq!(digest(&w), route);
                assert!(!held_receipt(&w));
            }
        }

        #[test]
        fn a_running_reader_update_is_polled_never_resent() {
            let w = world();
            let arm = MockArm::default();
            with_reader(&w, &arm);
            let intent = readers(&w, "readers-2", present(2, 4));
            let d = driver(&w, &arm, &intent);
            d.reserve_readers(&w.objects, Some(ENDPOINT), NOW).unwrap();
            let (profile, scope) = (reader_app("v2"), PinScope::for_test());
            let update = Verb::Update {
                config_digest: sha(b"v2"),
            };
            arm.script(Begin::ApplyRunning("arm-op-update"));
            arm.polls(running(3));
            let observed = fleet_at(2, "r1");
            let refused = d
                .set_readers(
                    &w.objects,
                    Some(&profile),
                    Some(&scope),
                    Some(&observed),
                    NOW,
                )
                .unwrap_err();
            assert_eq!(refused.code, "cloud_outcome_unknown");
            assert!(!held_receipt(&w));
            let sealed = d
                .set_readers(
                    &w.objects,
                    Some(&profile),
                    Some(&scope),
                    Some(&observed),
                    NOW,
                )
                .unwrap();
            receipt_of(&sealed);
            assert_eq!(arm.count(&update), 1);
        }

        #[test]
        fn an_overlapping_same_attempt_update_dispatches_once() {
            let w = Arc::new(world());
            let arm = Arc::new(MockArm::default());
            with_reader(&w, &arm);
            let intent = readers(&w, "readers-2", present(2, 4));
            driver(&w, &arm, &intent)
                .reserve_readers(&w.objects, Some(ENDPOINT), NOW)
                .unwrap();
            let (bw, barm, bintent) = (w.clone(), arm.clone(), intent.clone());
            // B runs the whole change while A is about to dispatch.
            arm.on_read(move || {
                let sealed = driver(&bw, &barm, &bintent)
                    .set_readers(
                        &bw.objects,
                        Some(&reader_app("v2")),
                        Some(&PinScope::for_test()),
                        Some(&fleet_at(2, "r1")),
                        NOW,
                    )
                    .unwrap();
                receipt_of(&sealed);
            });
            let sealed = driver(&w, &arm, &intent)
                .set_readers(
                    &w.objects,
                    Some(&reader_app("v2")),
                    Some(&PinScope::for_test()),
                    Some(&fleet_at(2, "r1")),
                    NOW,
                )
                .unwrap();
            receipt_of(&sealed);
            let update = Verb::Update {
                config_digest: sha(b"v2"),
            };
            assert_eq!(arm.count(&update), 1);
            assert_writer_unchanged(&w, Some(ENDPOINT));
        }

        #[test]
        fn re_adding_readers_after_removal_reuses_the_stopped_reader_app() {
            let w = world();
            let arm = MockArm::default();
            with_reader(&w, &arm);
            let off = readers(&w, "readers-off", Fleet::Absent {});
            let d = driver(&w, &arm, &off);
            d.reserve_readers(&w.objects, None, NOW).unwrap();
            d.set_readers(&w.objects, None, None, None, NOW).unwrap();
            assert_writer_unchanged(&w, None);
            let again = readers(&w, "readers-again", present(1, 2));
            add(&w, &arm, &again, "v1", &fleet_at(1, "r1")).unwrap();
            assert_writer_unchanged(&w, Some(ENDPOINT));
            let reader_puts = arm
                .calls()
                .iter()
                .filter(|c| c.resource == READER && matches!(c.verb, Verb::Put { .. }))
                .count();
            assert_eq!(reader_puts, 1, "the stopped app is reused, not recreated");
            assert_eq!(
                arm.count(&Verb::Update {
                    config_digest: sha(b"v1")
                }),
                1
            );
            assert!(matches!(
                arm.get(READER).unwrap().revisions,
                Revisions::Complete(r) if r.iter().filter(|r| r.active).count() == 1
            ));
        }

        // ------------- EPIC-007 ITEM-018: local runbook regression gaps

        /// A failed replica keeps the change unsealed until an all-healthy
        /// fleet is observed on the same active revision. The fake update is
        /// dispatched once; this does not prove ACA rolls a new revision or
        /// replaces a replica when the desired configuration is unchanged.
        #[test]
        fn a_failed_replica_update_dispatches_once_and_waits_for_a_healthy_fleet() {
            let w = world();
            let arm = MockArm::default();
            with_reader(&w, &arm);
            let before = digest(&w);
            let intent = readers(&w, "replace-1", present(1, 2));
            let observed = |replicas: &[(&str, bool)]| {
                FleetObservation::for_test(
                    authority(HEAD, 1),
                    replicas
                        .iter()
                        .map(|(name, failed)| ReplicaStatus {
                            revision: "r1".into(),
                            failed: *failed,
                            ..replica(name)
                        })
                        .collect(),
                )
            };
            for fleet in [
                observed(&[("replica-0", true)]),
                observed(&[("replica-0", true), ("replica-1", false)]),
            ] {
                let refused = add(&w, &arm, &intent, "v1", &fleet).unwrap_err();
                assert_eq!(refused.code, "replica_ineligible");
                assert!(!held_receipt(&w));
                assert_eq!(digest(&w), before);
            }
            add(&w, &arm, &intent, "v1", &observed(&[("replica-9", false)])).unwrap();
            assert_eq!(digest(&w), before);
            assert_writer_unchanged(&w, Some(ENDPOINT));
            assert_eq!(
                arm.count(&Verb::Update {
                    config_digest: sha(b"v1")
                }),
                1,
                "one in-place update, never resent"
            );
            assert_eq!(writer_calls(&arm), 1);
            assert_eq!(arm.count(&Verb::Stop) + arm.count(&Verb::Delete), 0);
        }

        /// Control-side route drift: a route change published outside the
        /// lifecycle (a binding-generation advance through a legacy directory
        /// edit once the slot is released) matches no receipt, so later reader
        /// changes and retirement are refused with no slot and no effect.
        /// Replica-side lag is `replica_ineligible` (above). Only a route equal
        /// to the latest receipt's re-anchors; restoring it here isolates that
        /// check, not an operator procedure for rolling back a deliberate move.
        #[test]
        fn a_route_change_outside_the_lifecycle_refuses_reader_changes_and_retirement() {
            let w = world();
            let arm = MockArm::default();
            let (_, latest) = with_reader(&w, &arm);
            let published = route(&w);
            let publish = |route: Route| {
                w.objects
                    .apply(
                        NOW,
                        &Edit::Directory,
                        Some(&[Route::shared_for_test(OTHER), route]),
                        None,
                    )
                    .unwrap();
            };
            publish(Route {
                binding_generation: published.binding_generation + 1,
                ..published.clone()
            });
            let change = readers(&w, "after-drift", present(1, 2));
            assert_stale_refused(&w, &arm, &change, Some(ENDPOINT), "readers_unanchored");
            let stale = retire_with(&w, "retire-after-drift", &latest);
            assert_stale_refused(&w, &arm, &stale, None, "retirement_receipt_mismatch");
            publish(published);
            let intent = retire_with(&w, "retire-restored", &latest);
            let d = driver(&w, &arm, &intent);
            d.reserve_retirement(&w.objects, NOW).unwrap();
            let guards = Guards::new(w.memory.clone(), TARGET);
            let sealed = d.retire(&w.objects, &guards, NOW).unwrap();
            assert_eq!(sealed.outcome(), &TerminalOutcome::Retired);
        }

        #[test]
        fn pin_recovery_refuses_while_another_action_is_unknown() {
            let env = crate::server::cloud_recovery::tests::setup();
            let w = world();
            let arm = MockArm::default();
            with_reader(&w, &arm);
            let intent = readers(&w, "readers-off", Fleet::Absent {});
            let d = driver(&w, &arm, &intent);
            d.reserve_readers(&w.objects, None, NOW).unwrap();
            arm.script(Begin::ApplyRunning("arm-op-stop"));
            arm.polls(running(3));
            d.set_readers(&w.objects, None, None, None, NOW)
                .unwrap_err();
            let pin = env
                .sets
                .pin(
                    &env.set_id,
                    "reader-apply:replica-0",
                    PinKind::Operation,
                    NOW - 1,
                )
                .unwrap();
            let refused = d
                .recover_pins(
                    &w.objects,
                    &env.sets,
                    Some(&PinScope::for_test()),
                    Some(&stopped("replica-0")),
                    NOW,
                )
                .unwrap_err();
            assert_eq!(refused.code, "prior_action_unknown");
            assert!(pins(&env).contains(&pin.id));
        }

        #[test]
        fn an_uncertain_pin_release_is_replayed_before_anything_else() {
            let env = crate::server::cloud_recovery::tests::setup();
            let w = world();
            let arm = MockArm::default();
            with_reader(&w, &arm);
            let intent = readers(&w, "readers-off", Fleet::Absent {});
            let d = driver(&w, &arm, &intent);
            d.reserve_readers(&w.objects, None, NOW).unwrap();
            let pin = env
                .sets
                .pin(
                    &env.set_id,
                    "reader-apply:replica-0",
                    PinKind::Operation,
                    NOW - 1,
                )
                .unwrap();
            let scope = PinScope::for_test();
            // Every bounded attempt of the release write is lost.
            env.meta.fault(Op::Put, "record.json", Fault::Before, 0, 4);
            d.recover_pins(
                &w.objects,
                &env.sets,
                Some(&scope),
                Some(&stopped("replica-0")),
                NOW,
            )
            .unwrap_err();
            assert!(pins(&env).contains(&pin.id));
            let resource = format!("pin/{}/{}", env.set_id, pin.id);
            assert!(steps(&w, &intent).contains(&(
                resource.clone(),
                "release-pin".into(),
                StepOutcome::Uncertain {}
            )));
            // The replay settles it, then nothing is left to release.
            assert_eq!(
                d.recover_pins(
                    &w.objects,
                    &env.sets,
                    Some(&scope),
                    Some(&stopped("replica-0")),
                    NOW,
                )
                .unwrap(),
                0
            );
            assert!(!pins(&env).contains(&pin.id));
            assert!(steps(&w, &intent).contains(&(
                resource,
                "release-pin".into(),
                settled("released")
            )));
        }

        // ------- e36403e1 review: admin submissions never hold a slot (R1)

        fn cli(w: &World, command: admin::Command) -> Result<serde_json::Value, ApiError> {
            let deployments = admin::Deployments::new(w.memory.clone(), CONTROL, PREFIX);
            admin::run(&deployments, &w.tenants, None, command, NOW)
        }

        /// `vlite-server tenant-deployment readers|retire`: the operation.
        fn cli_submit(w: &World, key: &str, payload: &Payload) -> serde_json::Value {
            let contents: Vec<String> = match payload {
                Payload::SetReaders {
                    fleet: Fleet::Present { .. },
                    ..
                } => [RPROFILE, RULE]
                    .iter()
                    .map(|c| String::from_utf8(c.to_vec()).unwrap())
                    .collect(),
                _ => Vec::new(),
            };
            let request = serde_json::json!({
                "idempotency_key": key, "request": payload, "contents": contents,
            });
            let command = admin::Command::Submit {
                kind: payload.kind(),
                tenant: Some(T.into()),
                request: request.to_string().into_bytes(),
            };
            cli(w, command).unwrap()["operation"].clone()
        }

        /// `vlite-server tenant-deployment cancel` quoting `operation`'s
        /// current tokens.
        fn cli_cancel(w: &World, id: &str, key: &str) -> Result<serde_json::Value, ApiError> {
            let status = cli(
                w,
                admin::Command::Status {
                    operation: id.into(),
                },
            )
            .unwrap()["operation"]
                .clone();
            let request = serde_json::json!({
                "idempotency_key": key,
                "expected_operation_generation": status["operation_generation"],
                "expected_lifecycle_generation": status["lifecycle_generation"],
                "expected_receipt": status["receipt_digest"],
            });
            cli(
                w,
                admin::Command::Act {
                    operation: id.into(),
                    verb: "cancel".into(),
                    request: request.to_string().into_bytes(),
                },
            )
        }

        fn loaded(w: &World, operation: &serde_json::Value) -> Intent {
            w.ops
                .load(operation["operation_id"].as_str().unwrap())
                .unwrap()
                .intent
        }

        /// A fresh reader change and then a retirement naming its receipt
        /// still anchor, reserve and complete.
        fn lifecycle_still_anchors(w: &World, arm: &MockArm, config: &str, revision: &str) {
            let fresh = readers(w, "fresh", present(1, 4));
            let sealed = add(w, arm, &fresh, config, &fleet_at(1, revision)).unwrap();
            let retire = retire_with(w, "fresh-retire", &receipt_of(&sealed));
            let d = driver(w, arm, &retire);
            d.reserve_retirement(&w.objects, NOW).unwrap();
            let guards = Guards::new(w.memory.clone(), TARGET);
            let sealed = d.retire(&w.objects, &guards, NOW).unwrap();
            assert_eq!(sealed.outcome(), &TerminalOutcome::Retired);
        }

        #[test]
        fn admin_reader_and_retirement_submissions_hold_no_slot_and_never_poison_anchoring() {
            for retirement in [false, true] {
                let w = world();
                let arm = MockArm::default();
                let (_, latest) = with_reader(&w, &arm);
                let anchor = released_by(&w);
                let payload = if retirement {
                    Payload::Retire {
                        tenant_id: T.into(),
                        expected_receipt: latest.clone(),
                        reason: "offboarded".into(),
                        data_disposition: DataDisposition::Retain,
                    }
                } else {
                    Payload::SetReaders {
                        tenant_id: T.into(),
                        fleet: present(2, 4),
                    }
                };
                let submitted = cli_submit(&w, "cli-a", &payload);
                assert_eq!(submitted["phase"], "accepted", "{submitted}");
                assert_eq!(submitted["observed"]["reserved"], false);
                // A duplicate submission reserves nothing either.
                let again = cli_submit(&w, "cli-a", &payload);
                assert_eq!(again["operation_id"], submitted["operation_id"]);
                assert_eq!(released_by(&w), anchor, "no planless slot");
                // Nothing is held, so there is nothing to cancel.
                let id = submitted["operation_id"].as_str().unwrap();
                let refused = cli_cancel(&w, id, "cancel-a").unwrap_err();
                assert_eq!(refused.code, "transition_unavailable");
                assert_eq!(released_by(&w), anchor);
                lifecycle_still_anchors(&w, &arm, "v2", "r1");
            }
        }

        #[test]
        fn an_admin_submission_reserved_by_its_planned_actuator_cancels_through_its_plan() {
            let w = world();
            let arm = MockArm::default();
            with_reader(&w, &arm);
            let payload = Payload::SetReaders {
                tenant_id: T.into(),
                fleet: present(2, 4),
            };
            let submitted = cli_submit(&w, "cli-a", &payload);
            let a = loaded(&w, &submitted);
            let d = driver(&w, &arm, &a);
            d.reserve_readers(&w.objects, Some(ENDPOINT), NOW).unwrap();
            // A legitimate same-operation retry keeps its bound slot.
            d.reserve_readers(&w.objects, Some(ENDPOINT), NOW).unwrap();
            let id = a.operation_id.as_str();
            let status = cli(
                &w,
                admin::Command::Status {
                    operation: id.into(),
                },
            )
            .unwrap();
            assert_eq!(status["operation"]["phase"], "reserved");
            let cancelled = cli_cancel(&w, id, "cancel-a").unwrap();
            assert_eq!(cancelled["operation"]["phase"], "cancelled");
            assert_eq!(released_by(&w).as_deref(), Some(id));
            // The cancelled anchor hops through A's plan: no poisoning.
            lifecycle_still_anchors(&w, &arm, "v2", "r1");
        }

        #[test]
        fn a_duplicate_admin_submission_of_a_stale_plan_takes_no_slot() {
            for same_route in [false, true] {
                let w = world();
                let arm = MockArm::default();
                if same_route {
                    with_reader(&w, &arm);
                } else {
                    activated(&w, &arm);
                }
                let payload = Payload::SetReaders {
                    tenant_id: T.into(),
                    fleet: present(2, 4),
                };
                let a = loaded(&w, &cli_submit(&w, "cli-a", &payload));
                plan_only(&w, &arm, &a, Some(ENDPOINT));
                let route = digest(&w);
                let b = readers(&w, "readers-b", present(1, 3));
                let observed = if same_route {
                    fleet_at(1, "r1")
                } else {
                    fleet(1)
                };
                add(&w, &arm, &b, "v2", &observed).unwrap();
                assert_eq!(digest(&w) == route, same_route);
                let again = cli_submit(&w, "cli-a", &payload);
                assert_eq!(again["observed"]["reserved"], false, "{again}");
                assert_stale_refused(&w, &arm, &a, Some(ENDPOINT), "readers_unanchored");
                assert_eq!(
                    cli_cancel(&w, &a.operation_id, "cancel-a")
                        .unwrap_err()
                        .code,
                    "transition_unavailable"
                );
                let revision = if same_route { "r2" } else { "r1" };
                lifecycle_still_anchors(&w, &arm, "v3", revision);
            }
        }

        /// A slot held without plan binding (only a test fixture or state
        /// persisted by EPIC-002 through e36403e1 admin submissions can hold one)
        /// never passes as this operation's planned reservation. Its cancel
        /// leaves no plan to hop through, so later changes stay refused:
        /// fail closed, never repaired.
        #[test]
        fn a_slot_held_without_its_plan_route_is_refused_and_fails_closed() {
            let w = world();
            let arm = MockArm::default();
            activated(&w, &arm);
            let a = readers(&w, "readers-a", present(1, 2));
            plan_only(&w, &arm, &a, Some(ENDPOINT));
            let b = readers(&w, "readers-b", present(1, 2));
            add(&w, &arm, &b, "v1", &fleet(1)).unwrap();
            reserve(&w.ops, &w.objects, &a, NOW).unwrap();
            let calls = arm.calls().len();
            let d = driver(&w, &arm, &a);
            assert_eq!(
                d.reserve_readers(&w.objects, Some(ENDPOINT), NOW)
                    .unwrap_err()
                    .code,
                "readers_unanchored"
            );
            d.set_readers(
                &w.objects,
                Some(&reader_app("v9")),
                Some(&PinScope::for_test()),
                Some(&fleet(1)),
                NOW,
            )
            .unwrap_err();
            assert_eq!(arm.calls().len(), calls, "no reader app effect");
            request_cancel(&w.objects, &a, NOW).unwrap();
            w.ops.release(&w.objects, &a, NOW).unwrap();
            let fresh = readers(&w, "fresh", present(1, 3));
            assert_eq!(
                driver(&w, &arm, &fresh)
                    .reserve_readers(&w.objects, Some(ENDPOINT), NOW)
                    .unwrap_err()
                    .code,
                "readers_unanchored"
            );
        }

        fn pins(env: &crate::server::cloud_recovery::tests::Env) -> Vec<String> {
            env.sets
                .list()
                .unwrap()
                .sets
                .into_iter()
                .filter_map(|s| s.record)
                .flat_map(|r| r.pins)
                .map(|p| p.id)
                .collect()
        }
    }
}
