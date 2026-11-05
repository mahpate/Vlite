use super::*;
use crate::memory::binding::{BindingState, Grant};
use crate::memory::{
    ForgetRequest, ForgetTarget, MemoryItem, MemoryKind, MemoryScope, Provenance, RememberRequest,
};
use crate::{
    CollectionSettings, CollectionSpec, CreateOptions, FormatVersion, Metadata, MetadataValue,
};

const DAY_MS: u64 = 86_400_000;
const NOW: u64 = 1_000_000;
const PRINCIPAL: &str = "abcdefghij27";
const TENANT: &str = "0123456789abcdef0123456789abcdef";

fn scope(tenant: &str, agent: &str) -> MemoryScope {
    MemoryScope {
        tenant: tenant.into(),
        agent: agent.into(),
        user: None,
        session: None,
    }
}

fn binding(name: &str, graph: &str, evidence: Option<&str>) -> Binding {
    Binding {
        binding: name.into(),
        tenant: "@root".into(),
        generation: 1,
        state: BindingState::Active,
        graph: graph.into(),
        evidence_collection: evidence.map(Into::into),
        receipt_window_ms: DAY_MS,
        grants: vec![Grant {
            principal: PRINCIPAL.into(),
            scopes: vec![scope("@root", "a")],
            raw_access: false,
        }],
    }
}

fn set(bindings: Vec<Binding>) -> BindingSet {
    BindingSet { bindings }
}

fn database() -> Database {
    let mut db = Database::create_in_memory(
        2,
        CreateOptions {
            format: Some(FormatVersion::V5),
            ..Default::default()
        },
    )
    .unwrap();
    let tenant_graph = format!("t{TENANT}__tg");
    for graph in ["g1", "g2", "g3", tenant_graph.as_str()] {
        db.create_graph(graph).unwrap();
    }
    let tenant_evidence = format!("t{TENANT}__tev");
    for collection in ["ev1", "ev2", tenant_evidence.as_str()] {
        db.create_collection(
            collection,
            CollectionSpec {
                dimensions: 2,
                settings: CollectionSettings::default(),
            },
        )
        .unwrap();
    }
    db
}

/// Runs the checks and then the writes a writable primary performs.
fn start(db: &mut Database, file: &BindingSet, now_ms: u64) -> Result<Plan, Refusal> {
    let planned = plan(db, file, now_ms)?;
    for index in &planned.writes {
        let binding = &file.bindings[*index];
        let graph = binding.stored(&binding.graph);
        let evidence = binding
            .evidence_collection
            .as_deref()
            .map(|name| binding.stored(name));
        MemoryMut::bind(db, &graph, evidence.as_deref())
            .unwrap()
            .write_binding_record(binding)
            .unwrap();
    }
    Ok(planned)
}

fn refusal(db: &Database, file: &BindingSet, now_ms: u64) -> Refusal {
    plan(db, file, now_ms).expect_err("the start must be refused")
}

fn record_node(db: &Database, graph: &str) -> Option<NodeView> {
    db.graph(graph)
        .unwrap()
        .get_node(RECORD_KEY)
        .unwrap()
        .output
}

fn stored_record(db: &Database, graph: &str) -> Recorded {
    Recorded::decode(&record_node(db, graph).unwrap(), graph).unwrap()
}

fn create(db: &mut Database, graph: &str, key: &str, labels: &[&str], properties: Metadata) {
    db.graph_mut(graph)
        .unwrap()
        .write_batch(&[GraphOp::CreateNode {
            key: key.into(),
            labels: labels.iter().map(|label| (*label).into()).collect(),
            properties,
        }])
        .unwrap();
}

fn provision(db: &mut Database, graph: &str, tenant: &str, agent: &str) {
    MemoryMut::bind(db, graph, None)
        .unwrap()
        .provision_scope(&scope(tenant, agent))
        .unwrap();
}

fn item(id: &str) -> MemoryItem {
    MemoryItem {
        id: id.into(),
        kind: MemoryKind::Entity,
        content: format!("content {id}"),
        source: "s".into(),
        importance: None,
        observed_at_ms: Some(1),
        provenance: vec![Provenance {
            source: "s".into(),
            kind: Some("caller".into()),
            uri: None,
            transformation: None,
        }],
        references: Vec::new(),
        merge_into: None,
        expected_revision: None,
        expiry_ms: None,
        expiry_valid: true,
        evidence: Vec::new(),
    }
}

fn remember(db: &mut Database, graph: &str, id: &str, key: &str) {
    MemoryMut::bind(db, graph, None)
        .unwrap()
        .remember(&RememberRequest {
            scope: scope("@root", "a"),
            idempotency_key: key.into(),
            receipt_window_ms: DAY_MS,
            now_ms: NOW,
            deadline_ms: None,
            allow_future_observations: false,
            items: vec![item(id)],
        })
        .unwrap();
}

/// The first adoption of a named binding, as a writable primary performs it.
fn adopted(db: &mut Database, bindings: Vec<Binding>) -> BindingSet {
    let file = set(bindings);
    start(db, &file, NOW).unwrap();
    file
}

// ---------------------------------------------------------------- first start

#[test]
fn a_fresh_graph_is_adopted_and_gets_its_record() {
    let mut db = database();
    let notes = binding("notes", "g1", Some("ev1"));
    let file = set(vec![notes.clone()]);
    assert_eq!(
        plan(&db, &file, NOW),
        Ok(Plan {
            writes: vec![0],
            orphans: Vec::new()
        })
    );
    assert!(record_node(&db, "g1").is_none(), "planning writes nothing");
    start(&mut db, &file, NOW).unwrap();
    let node = record_node(&db, "g1").unwrap();
    assert_eq!(node.properties, Recorded::of(&notes).properties());
    assert_eq!(node.labels.iter().collect::<Vec<_>>(), [RECORD_LABEL]);
    assert_eq!(stored_record(&db, "g1"), Recorded::of(&notes));
    assert!(
        record_node(&db, "g2").is_none(),
        "other graphs are untouched"
    );
}

#[test]
fn the_first_start_of_two_bindings_records_both() {
    let mut db = database();
    let mut tenant = binding("team", "tg", Some("tev"));
    tenant.tenant = TENANT.into();
    tenant.grants[0].scopes = vec![scope(TENANT, "a")];
    let file = set(vec![binding("notes", "g1", Some("ev1")), tenant.clone()]);
    let planned = start(&mut db, &file, NOW).unwrap();
    assert_eq!(planned.writes, [0, 1]);
    assert_eq!(stored_record(&db, "g1"), Recorded::of(&file.bindings[0]));
    let tenant_graph = format!("t{TENANT}__tg");
    assert_eq!(stored_record(&db, &tenant_graph), Recorded::of(&tenant));
}

#[test]
fn a_restart_with_the_same_file_adds_no_commit() {
    let mut db = database();
    let notes = binding("notes", "g1", Some("ev1"));
    let file = adopted(&mut db, vec![notes.clone()]);
    let before = record_node(&db, "g1").unwrap();
    let sequence = db.sequence;
    assert_eq!(
        plan(&db, &file, NOW + 1),
        Ok(Plan {
            writes: Vec::new(),
            orphans: Vec::new()
        })
    );
    let outcome = MemoryMut::bind(&mut db, "g1", Some("ev1"))
        .unwrap()
        .write_binding_record(&notes)
        .unwrap();
    assert_eq!(outcome, RecordWrite::Unchanged);
    assert_eq!(record_node(&db, "g1").unwrap(), before);
    assert_eq!(db.sequence, sequence);
}

#[test]
fn a_binding_whose_graph_holds_a_record_is_adopted_as_recorded() {
    let mut db = database();
    let notes = binding("notes", "g1", Some("ev1"));
    adopted(&mut db, vec![notes.clone()]);
    let stored = stored_record(&db, "g1");
    let memory = MemoryRef::bind(&db, "g1", Some("ev1")).unwrap();
    assert_eq!(
        memory.adoption(&notes, Some(&stored)).unwrap(),
        Some(Adoption::Recorded)
    );
    let mut other = notes.clone();
    other.binding = "other".into();
    assert_eq!(memory.adoption(&other, Some(&stored)).unwrap(), None);
    let mut elsewhere = notes.clone();
    elsewhere.tenant = TENANT.into();
    assert_eq!(memory.adoption(&elsewhere, Some(&stored)).unwrap(), None);
}

#[test]
fn a_fresh_graph_with_ordinary_evidence_records_is_refused() {
    let mut db = database();
    db.collection_mut("ev1")
        .unwrap()
        .put(1, &[1.0, 0.0], b"")
        .unwrap();
    assert_eq!(
        refusal(&db, &set(vec![binding("notes", "g1", Some("ev1"))]), NOW),
        Refusal::InUse
    );
    // Without an evidence collection the same graph is fresh.
    assert!(plan(&db, &set(vec![binding("notes", "g1", None)]), NOW).is_ok());
}

#[test]
fn a_fresh_graph_is_adopted_with_no_evidence_collection() {
    let mut db = database();
    let notes = binding("notes", "g1", None);
    let planned = start(&mut db, &set(vec![notes.clone()]), NOW).unwrap();
    assert_eq!(planned.writes, [0]);
    assert_eq!(stored_record(&db, "g1").evidence, None);
    assert!(
        !record_node(&db, "g1")
            .unwrap()
            .properties
            .contains_key("evidence")
    );
}

// ---------------------------------------------------------------- legacy (c)

#[test]
fn a_graph_adopted_by_the_first_release_is_adopted_once_and_recorded() {
    let mut db = database();
    provision(&mut db, "g1", "@root", "a");
    remember(&mut db, "g1", "m1", "k1");
    let notes = binding("notes", "g1", Some("ev1"));
    let file = set(vec![notes.clone()]);
    let memory = MemoryRef::bind(&db, "g1", Some("ev1")).unwrap();
    assert_eq!(
        memory.adoption(&notes, None).unwrap(),
        Some(Adoption::Legacy)
    );
    assert_eq!(plan(&db, &file, NOW).unwrap().writes, [0]);
    start(&mut db, &file, NOW).unwrap();
    assert_eq!(stored_record(&db, "g1"), Recorded::of(&notes));
    // The next start finds the record and adopts as recorded.
    assert_eq!(
        plan(&db, &file, NOW + 1).unwrap().writes,
        Vec::<usize>::new()
    );
}

#[test]
fn raw_loaded_evidence_does_not_stop_a_legacy_upgrade() {
    // 0.7.0 let an operator load evidence before attaching it; the upgrade
    // check looks at the graph, never at the evidence collection.
    let mut db = database();
    provision(&mut db, "g1", "@root", "a");
    db.collection_mut("ev1")
        .unwrap()
        .put(7, &[1.0, 0.0], b"loaded before it was attached")
        .unwrap();
    db.collection_mut("ev1")
        .unwrap()
        .put(8, &[0.0, 1.0], b"another")
        .unwrap();
    let notes = binding("notes", "g1", Some("ev1"));
    let file = set(vec![notes.clone()]);
    let memory = MemoryRef::bind(&db, "g1", Some("ev1")).unwrap();
    assert_eq!(
        memory.adoption(&notes, None).unwrap(),
        Some(Adoption::Legacy)
    );
    assert_eq!(plan(&db, &file, NOW).unwrap().writes, [0]);
}

#[test]
fn one_forged_scope_label_on_an_ordinary_graph_is_refused() {
    let mut db = database();
    create(&mut db, "g1", "ordinary", &["Person"], Metadata::new());
    create(
        &mut db,
        "g1",
        "scope:v1:forged",
        &["VliteMemoryScope"],
        Metadata::new(),
    );
    assert_eq!(
        refusal(&db, &set(vec![binding("notes", "g1", None)]), NOW),
        Refusal::InUse
    );
}

#[test]
fn a_graph_with_ordinary_nodes_is_refused_even_beside_provisioned_scopes() {
    let mut db = database();
    provision(&mut db, "g1", "@root", "a");
    assert!(plan(&db, &set(vec![binding("notes", "g1", None)]), NOW).is_ok());
    create(&mut db, "g1", "ordinary", &["Person"], Metadata::new());
    assert_eq!(
        refusal(&db, &set(vec![binding("notes", "g1", None)]), NOW),
        Refusal::InUse
    );
}

#[test]
fn legacy_adoption_needs_each_node_to_pair_its_label_with_its_key_prefix() {
    let wrong: [(&str, &[&str]); 9] = [
        ("mem:v1:x", &["VliteMemoryScope"]),
        ("scope:v1:x", &["VliteMemory"]),
        ("receipt:v1:x", &["VliteMemoryContent"]),
        ("receipt-outcome:v1:x", &["VliteMemoryReceipt"]),
        ("content:v1:x", &["VliteMemoryReceiptOutcome"]),
        ("scope:v1:x", &["VliteMemoryScope", "VliteMemory"]),
        ("scope:v1:x", &[]),
        ("scope:v1:x", &["VliteMemoryFuture"]),
        ("other:v1:x", &["VliteMemoryScope"]),
    ];
    for (key, labels) in wrong {
        let mut db = database();
        provision(&mut db, "g1", "@root", "a");
        create(&mut db, "g1", key, labels, Metadata::new());
        assert_eq!(
            refusal(&db, &set(vec![binding("notes", "g1", None)]), NOW),
            Refusal::InUse,
            "{key} {labels:?}"
        );
    }
    let right: [(&str, &str); 6] = [
        ("mem:v1:x", "VliteMemory"),
        ("scope:v1:x", "VliteMemoryScope"),
        ("receipt:v1:x", "VliteMemoryReceipt"),
        ("receipt:v1:y", "VliteMemoryErasureJob"),
        ("receipt-outcome:v1:x", "VliteMemoryReceiptOutcome"),
        ("content:v1:x", "VliteMemoryContent"),
    ];
    for (key, label) in right {
        let mut db = database();
        create(&mut db, "g1", key, &[label], Metadata::new());
        assert!(
            plan(&db, &set(vec![binding("notes", "g1", None)]), NOW).is_ok(),
            "{key} {label}"
        );
    }
}

#[test]
fn the_legacy_scan_pages_through_the_whole_graph() {
    let nodes = |count: usize| {
        (0..count)
            .map(|index| GraphOp::CreateNode {
                key: format!("scope:v1:{index:05}"),
                labels: vec!["VliteMemoryScope".into()],
                properties: Metadata::new(),
            })
            .collect::<Vec<_>>()
    };
    let mut db = database();
    db.graph_mut("g1")
        .unwrap()
        .write_batch(&nodes(2_500))
        .unwrap();
    let file = set(vec![binding("notes", "g1", None)]);
    assert_eq!(plan(&db, &file, NOW).unwrap().writes, [0]);
    // A foreign node created last is on the last page.
    create(&mut db, "g1", "zz-last", &["Person"], Metadata::new());
    assert_eq!(refusal(&db, &file, NOW), Refusal::InUse);
}

#[test]
fn the_legacy_scan_handles_the_page_boundaries() {
    let mut db = database();
    let batch: Vec<GraphOp> = (0..1_000)
        .map(|index| GraphOp::CreateNode {
            key: format!("scope:v1:{index:05}"),
            labels: vec!["VliteMemoryScope".into()],
            properties: Metadata::new(),
        })
        .collect();
    db.graph_mut("g1").unwrap().write_batch(&batch).unwrap();
    let memory = MemoryRef::bind(&db, "g1", None).unwrap();
    assert!(memory.holds_only_memory().unwrap());
    create(
        &mut db,
        "g1",
        "scope:v1:extra",
        &["VliteMemoryScope"],
        Metadata::new(),
    );
    let memory = MemoryRef::bind(&db, "g1", None).unwrap();
    assert!(memory.holds_only_memory().unwrap());
    create(&mut db, "g1", "plain", &[], Metadata::new());
    let memory = MemoryRef::bind(&db, "g1", None).unwrap();
    assert!(!memory.holds_only_memory().unwrap());
}

// ---------------------------------------------------------------- rotation

fn recorded_file() -> (Database, Binding, BindingSet) {
    let mut db = database();
    let notes = binding("notes", "g1", Some("ev1"));
    let file = adopted(&mut db, vec![notes.clone()]);
    (db, notes, file)
}

fn rotation(db: &Database, change: impl FnOnce(&mut Binding), now_ms: u64) -> Refusal {
    let mut changed = binding("notes", "g1", Some("ev1"));
    change(&mut changed);
    refusal(db, &set(vec![changed]), now_ms)
}

#[test]
fn rule_one_identity_never_changes() {
    let (db, ..) = recorded_file();
    let identity = Refusal::Rotation(BindingError::IdentityChanged);
    assert_eq!(rotation(&db, |b| b.receipt_window_ms += 1, NOW), identity);
    assert_eq!(rotation(&db, |b| b.receipt_window_ms -= 1, NOW), identity);
    assert_eq!(rotation(&db, |b| b.graph = "g2".into(), NOW), identity);
    assert_eq!(
        rotation(&db, |b| b.evidence_collection = Some("ev2".into()), NOW),
        identity
    );
    assert_eq!(
        rotation(&db, |b| b.evidence_collection = None, NOW),
        identity
    );
}

#[test]
fn rule_one_also_holds_when_the_entry_was_added_with_evidence() {
    let mut db = database();
    let no_evidence = binding("notes", "g1", None);
    adopted(&mut db, vec![no_evidence]);
    assert_eq!(
        refusal(&db, &set(vec![binding("notes", "g1", Some("ev1"))]), NOW),
        Refusal::Rotation(BindingError::IdentityChanged)
    );
}

#[test]
fn rule_two_a_change_needs_a_higher_generation_and_no_change_keeps_it() {
    let (db, ..) = recorded_file();
    let not_advanced = Refusal::Rotation(BindingError::GenerationNotAdvanced);
    let moved = Refusal::Rotation(BindingError::GenerationChanged);
    let grant = |agent: &str| Grant {
        principal: PRINCIPAL.into(),
        scopes: vec![scope("@root", agent)],
        raw_access: false,
    };
    assert_eq!(
        rotation(&db, |b| b.grants = vec![grant("b")], NOW),
        not_advanced
    );
    assert_eq!(
        rotation(&db, |b| b.grants[0].raw_access = true, NOW),
        not_advanced
    );
    assert_eq!(rotation(&db, |b| b.grants.clear(), NOW), not_advanced);
    assert_eq!(
        rotation(
            &db,
            |b| {
                b.grants = vec![grant("b")];
                b.generation = 1;
            },
            NOW
        ),
        not_advanced
    );
    assert_eq!(rotation(&db, |b| b.generation = 2, NOW), moved);
    assert_eq!(rotation(&db, |b| b.generation = u64::MAX >> 1, NOW), moved);
}

#[test]
fn rule_two_generations_never_decrease() {
    let mut db = database();
    let mut notes = binding("notes", "g1", Some("ev1"));
    notes.generation = 5;
    adopted(&mut db, vec![notes]);
    let not_advanced = Refusal::Rotation(BindingError::GenerationNotAdvanced);
    let moved = Refusal::Rotation(BindingError::GenerationChanged);
    assert_eq!(rotation(&db, |b| b.generation = 4, NOW), moved);
    assert_eq!(
        rotation(
            &db,
            |b| {
                b.generation = 4;
                b.grants.clear();
            },
            NOW
        ),
        not_advanced
    );
    assert_eq!(
        rotation(
            &db,
            |b| {
                b.generation = 5;
                b.grants.clear();
            },
            NOW
        ),
        not_advanced
    );
}

#[test]
fn a_higher_generation_with_changed_grants_is_accepted_and_recorded() {
    let (mut db, ..) = recorded_file();
    let mut rotated = binding("notes", "g1", Some("ev1"));
    rotated.generation = 2;
    rotated.grants[0].raw_access = true;
    let file = set(vec![rotated.clone()]);
    let planned = start(&mut db, &file, NOW).unwrap();
    assert_eq!(planned.writes, [0]);
    assert_eq!(stored_record(&db, "g1"), Recorded::of(&rotated));
    assert_eq!(stored_record(&db, "g1").generation, 2);
    assert!(plan(&db, &file, NOW + 1).unwrap().writes.is_empty());
}

#[test]
fn rule_three_a_retired_entry_never_returns_and_keeps_its_time() {
    let mut db = database();
    let mut retired = binding("notes", "g1", Some("ev1"));
    retired.state = BindingState::Retired { at_ms: NOW };
    adopted(&mut db, vec![retired]);
    let stored = stored_record(&db, "g1");
    assert_eq!(stored.state, BindingState::Retired { at_ms: NOW });
    assert_eq!(
        rotation(&db, |b| b.generation = 2, NOW + 5),
        Refusal::Rotation(BindingError::Reactivated)
    );
    assert_eq!(
        rotation(
            &db,
            |b| {
                b.generation = 2;
                b.state = BindingState::Retired { at_ms: NOW + 1 };
            },
            NOW + 5
        ),
        Refusal::Rotation(BindingError::RetirementChanged)
    );
    let same = {
        let mut same = binding("notes", "g1", Some("ev1"));
        same.state = BindingState::Retired { at_ms: NOW };
        same
    };
    // Unchanged and later: no backdating check applies to a stored time.
    assert!(plan(&db, &set(vec![same]), NOW + DAY_MS * 9).is_ok());
}

#[test]
fn rule_three_retirement_may_not_precede_the_change_that_makes_it() {
    let (db, ..) = recorded_file();
    let backdated = Refusal::Rotation(BindingError::BackdatedRetirement);
    let retire = |at_ms: u64| {
        move |b: &mut Binding| {
            b.generation = 2;
            b.state = BindingState::Retired { at_ms };
        }
    };
    assert_eq!(rotation(&db, retire(NOW + 9 - 1), NOW + 9), backdated);
    let file = {
        let mut retired = binding("notes", "g1", Some("ev1"));
        retire(NOW + 9)(&mut retired);
        set(vec![retired])
    };
    assert!(plan(&db, &file, NOW + 9).is_ok());
}

#[test]
fn rule_three_an_entry_first_seen_retired_may_not_be_backdated() {
    let mut db = database();
    let mut retired = binding("old", "g2", None);
    retired.state = BindingState::Retired { at_ms: NOW - 1 };
    let file = set(vec![retired.clone()]);
    assert_eq!(
        refusal(&db, &file, NOW),
        Refusal::Rotation(BindingError::BackdatedRetirement)
    );
    retired.state = BindingState::Retired { at_ms: NOW };
    let planned = start(&mut db, &set(vec![retired.clone()]), NOW).unwrap();
    assert_eq!(planned.writes, [0]);
    assert_eq!(
        stored_record(&db, "g2").state,
        BindingState::Retired { at_ms: NOW }
    );
}

#[test]
fn rule_five_an_active_entry_cannot_be_removed() {
    let (db, ..) = recorded_file();
    let other = binding("other", "g2", None);
    assert_eq!(
        refusal(&db, &set(vec![other]), NOW + DAY_MS * 400),
        Refusal::Rotation(BindingError::EarlyRemoval)
    );
}

#[test]
fn rule_five_a_retired_entry_may_go_only_when_its_window_has_ended() {
    let mut db = database();
    let mut retired = binding("old", "g2", None);
    retired.state = BindingState::Retired { at_ms: NOW };
    let live = binding("live", "g1", Some("ev1"));
    adopted(&mut db, vec![live.clone(), retired.clone()]);
    let without = set(vec![live.clone()]);
    let end = NOW + DAY_MS;
    assert_eq!(
        refusal(&db, &without, end - 1),
        Refusal::Rotation(BindingError::EarlyRemoval)
    );
    let planned = plan(&db, &without, end).unwrap();
    assert!(planned.writes.is_empty());
    assert_eq!(planned.orphans, [stored_record(&db, "g2")]);
    // The record of a removed entry is kept, untouched.
    let kept = record_node(&db, "g2").unwrap();
    start(&mut db, &without, end).unwrap();
    assert_eq!(record_node(&db, "g2").unwrap(), kept);
    // Still an orphan at every later start.
    assert_eq!(plan(&db, &without, end + DAY_MS).unwrap().orphans.len(), 1);
}

#[test]
fn rule_five_the_window_end_cannot_overflow() {
    let mut db = database();
    let mut retired = binding("old", "g2", None);
    retired.receipt_window_ms = 2_592_000_000;
    retired.state = BindingState::Retired {
        at_ms: u64::MAX - 5,
    };
    // Backdating is not at issue here: the record is written directly.
    MemoryMut::bind(&mut db, "g2", None)
        .unwrap()
        .write_binding_record(&retired)
        .unwrap();
    assert_eq!(
        refusal(&db, &set(vec![binding("live", "g1", None)]), u64::MAX),
        Refusal::Rotation(BindingError::EarlyRemoval)
    );
}

#[test]
fn rule_six_removed_storage_is_not_reused_by_another_binding() {
    let mut db = database();
    let mut retired = binding("old", "g2", Some("ev2"));
    retired.state = BindingState::Retired { at_ms: NOW };
    adopted(&mut db, vec![retired]);
    let end = NOW + DAY_MS;
    let reuse = Refusal::Rotation(BindingError::StorageReused);
    assert_eq!(
        refusal(&db, &set(vec![binding("new", "g2", None)]), end),
        reuse
    );
    assert_eq!(
        refusal(&db, &set(vec![binding("new", "g3", Some("ev2"))]), end),
        reuse
    );
    assert!(plan(&db, &set(vec![binding("new", "g3", Some("ev1"))]), end).is_ok());
    // Another tenant's stored name is another name.
    let mut tenant = binding("new", "tg", Some("tev"));
    tenant.tenant = TENANT.into();
    tenant.grants[0].scopes = vec![scope(TENANT, "a")];
    assert!(plan(&db, &set(vec![tenant]), end).is_ok());
}

#[test]
fn naming_a_removed_entrys_graph_again_as_the_same_binding_is_a_kept_entry() {
    let mut db = database();
    let mut retired = binding("old", "g2", None);
    retired.state = BindingState::Retired { at_ms: NOW };
    let live = binding("live", "g1", None);
    adopted(&mut db, vec![live.clone(), retired.clone()]);
    let end = NOW + DAY_MS * 3;
    // Re-adding the same retired entry with a raw grant needs a new generation.
    let mut again = retired.clone();
    again.grants[0].raw_access = true;
    assert_eq!(
        refusal(&db, &set(vec![live.clone(), again.clone()]), end),
        Refusal::Rotation(BindingError::GenerationNotAdvanced)
    );
    again.generation = 2;
    let planned = start(&mut db, &set(vec![live, again.clone()]), end).unwrap();
    assert_eq!(planned.writes, [1]);
    assert_eq!(stored_record(&db, "g2"), Recorded::of(&again));
}

#[test]
fn retired_entries_are_recorded_like_active_ones() {
    let mut db = database();
    let mut retired = binding("old", "g2", None);
    retired.state = BindingState::Retired { at_ms: NOW };
    let file = set(vec![retired.clone()]);
    assert_eq!(start(&mut db, &file, NOW).unwrap().writes, [0]);
    assert_eq!(stored_record(&db, "g2"), Recorded::of(&retired));
    assert!(plan(&db, &file, NOW + 1).unwrap().writes.is_empty());
}

// ---------------------------------------------------------------- records

fn corrupt_record(
    db: &mut Database,
    graph: &str,
    binding: &Binding,
    change: impl FnOnce(&mut Metadata),
) {
    let mut properties = Recorded::of(binding).properties();
    change(&mut properties);
    create(db, graph, RECORD_KEY, &[RECORD_LABEL], properties);
}

#[test]
fn a_corrupt_record_is_refused_with_or_without_bindings() {
    let mut db = database();
    let notes = binding("notes", "g1", None);
    corrupt_record(&mut db, "g1", &notes, |properties| {
        properties.remove("generation");
    });
    assert_eq!(refusal(&db, &set(Vec::new()), NOW), Refusal::Records);
    assert_eq!(refusal(&db, &set(vec![notes]), NOW), Refusal::Records);
    // The graph the record sits in does not have to be named.
    let other = binding("other", "g2", None);
    assert_eq!(refusal(&db, &set(vec![other]), NOW), Refusal::Records);
}

#[test]
fn a_record_held_by_another_graph_is_corrupt() {
    let mut db = database();
    let notes = binding("notes", "g1", None);
    corrupt_record(&mut db, "g2", &notes, |_| {});
    assert_eq!(refusal(&db, &set(Vec::new()), NOW), Refusal::Records);
}

#[test]
fn a_node_with_the_record_key_and_no_record_label_is_ordinary_data() {
    let mut db = database();
    create(&mut db, "g1", RECORD_KEY, &["Person"], Metadata::new());
    let planned = plan(&db, &set(Vec::new()), NOW).unwrap();
    assert!(planned.orphans.is_empty());
    assert_eq!(
        refusal(&db, &set(vec![binding("notes", "g1", None)]), NOW),
        Refusal::InUse
    );
}

#[test]
fn two_graphs_claiming_one_binding_contradict_each_other() {
    let mut db = database();
    let first = binding("notes", "g1", None);
    let second = binding("notes", "g2", None);
    create(
        &mut db,
        "g1",
        RECORD_KEY,
        &[RECORD_LABEL],
        Recorded::of(&first).properties(),
    );
    create(
        &mut db,
        "g2",
        RECORD_KEY,
        &[RECORD_LABEL],
        Recorded::of(&second).properties(),
    );
    assert_eq!(refusal(&db, &set(Vec::new()), NOW), Refusal::Records);
}

#[test]
fn two_records_claiming_one_evidence_collection_contradict_each_other() {
    let mut db = database();
    let first = binding("first", "g1", Some("ev1"));
    let second = binding("second", "g2", Some("ev1"));
    for (graph, binding) in [("g1", &first), ("g2", &second)] {
        create(
            &mut db,
            graph,
            RECORD_KEY,
            &[RECORD_LABEL],
            Recorded::of(binding).properties(),
        );
    }
    assert_eq!(refusal(&db, &set(Vec::new()), NOW), Refusal::Records);
}

#[test]
fn without_bindings_every_record_is_an_orphan_and_no_rotation_rule_runs() {
    let mut db = database();
    let notes = binding("notes", "g1", Some("ev1"));
    let mut tenant = binding("team", "tg", None);
    tenant.tenant = TENANT.into();
    tenant.grants[0].scopes = vec![scope(TENANT, "a")];
    adopted(&mut db, vec![notes.clone(), tenant.clone()]);
    let planned = plan(&db, &set(Vec::new()), NOW + DAY_MS * 900).unwrap();
    assert!(planned.writes.is_empty());
    let mut orphans = planned.orphans;
    orphans.sort_by(|a, b| a.binding.cmp(&b.binding));
    assert_eq!(orphans, [Recorded::of(&notes), Recorded::of(&tenant)]);
}

#[test]
fn a_database_without_records_has_no_orphans() {
    let db = database();
    assert_eq!(
        plan(&db, &set(Vec::new()), NOW),
        Ok(Plan {
            writes: Vec::new(),
            orphans: Vec::new()
        })
    );
}

#[test]
fn an_unnamed_record_is_an_orphan_beside_named_entries() {
    let mut db = database();
    let mut retired = binding("old", "g2", Some("ev2"));
    retired.state = BindingState::Retired { at_ms: NOW };
    let live = binding("live", "g1", None);
    adopted(&mut db, vec![live.clone(), retired.clone()]);
    let planned = plan(&db, &set(vec![live]), NOW + DAY_MS).unwrap();
    assert_eq!(planned.orphans, [Recorded::of(&retired)]);
}

// ---------------------------------------------------------------- recheck

#[test]
fn the_recheck_accepts_only_the_recorded_generation_of_the_served_binding() {
    let mut db = database();
    let mut notes = binding("notes", "g1", Some("ev1"));
    notes.generation = 3;
    adopted(&mut db, vec![notes]);
    let check = |tenant: &str, name: &str, generation: u64| {
        MemoryRef::bind(&db, "g1", Some("ev1"))
            .unwrap()
            .check_binding(tenant, name, generation)
    };
    assert!(check("@root", "notes", 3).is_ok());
    for (tenant, name, generation) in [
        ("@root", "notes", 2),
        ("@root", "notes", 4),
        ("@root", "other", 3),
        (TENANT, "notes", 3),
    ] {
        assert!(
            matches!(
                check(tenant, name, generation),
                Err(MemoryError::BindingGeneration)
            ),
            "{tenant} {name} {generation}"
        );
    }
}

#[test]
fn the_recheck_refuses_a_missing_or_corrupt_record() {
    let mut db = database();
    let notes = binding("notes", "g1", None);
    assert!(matches!(
        MemoryRef::bind(&db, "g1", None)
            .unwrap()
            .check_binding("@root", "notes", 1),
        Err(MemoryError::BindingGeneration)
    ));
    adopted(&mut db, vec![notes]);
    let node = record_node(&db, "g1").unwrap();
    let mut properties = node.properties.clone();
    properties.insert("extra".into(), MetadataValue::String("x".into()));
    db.graph_mut("g1")
        .unwrap()
        .write_batch(&[GraphOp::UpsertNode {
            key: RECORD_KEY.into(),
            labels: vec![RECORD_LABEL.into()],
            properties,
            expected: Expected::Revision(node.revision),
        }])
        .unwrap();
    assert!(matches!(
        MemoryRef::bind(&db, "g1", None)
            .unwrap()
            .check_binding("@root", "notes", 1),
        Err(MemoryError::BindingGeneration)
    ));
    let node = record_node(&db, "g1").unwrap();
    db.graph_mut("g1")
        .unwrap()
        .delete_node(RECORD_KEY, Expected::Revision(node.revision))
        .unwrap();
    assert!(matches!(
        MemoryRef::bind(&db, "g1", None)
            .unwrap()
            .check_binding("@root", "notes", 1),
        Err(MemoryError::BindingGeneration)
    ));
}

#[test]
fn writing_a_record_never_repairs_a_corrupt_one() {
    let mut db = database();
    let notes = binding("notes", "g1", None);
    corrupt_record(&mut db, "g1", &notes, |properties| {
        properties.insert("state".into(), MetadataValue::String("paused".into()));
    });
    let before = record_node(&db, "g1").unwrap();
    let outcome = MemoryMut::bind(&mut db, "g1", None)
        .unwrap()
        .write_binding_record(&notes);
    assert!(matches!(outcome, Err(MemoryError::InvalidBundle)));
    assert_eq!(record_node(&db, "g1").unwrap(), before);
}

#[test]
fn writing_a_record_reports_what_it_did() {
    let mut db = database();
    let mut notes = binding("notes", "g1", None);
    let write = |db: &mut Database, notes: &Binding| {
        MemoryMut::bind(db, "g1", None)
            .unwrap()
            .write_binding_record(notes)
            .unwrap()
    };
    assert_eq!(write(&mut db, &notes), RecordWrite::Created);
    assert_eq!(write(&mut db, &notes), RecordWrite::Unchanged);
    notes.generation = 2;
    notes.grants.clear();
    assert_eq!(write(&mut db, &notes), RecordWrite::Updated);
    assert_eq!(stored_record(&db, "g1"), Recorded::of(&notes));
    assert_eq!(write(&mut db, &notes), RecordWrite::Unchanged);
}

// ---------------------------------------------------------------- rollback

/// The adoption rule 0.7.0 ran at startup (its `memory/startup.rs`): kept here
/// so the tests pin what a rollback to that release sees in a recorded graph.
fn released_adoptable(db: &Database, graph: &str, evidence: Option<&str>) -> bool {
    let graph = db.graph(graph).unwrap();
    if graph.info().node_count == 0 {
        return evidence.is_none_or(|name| db.collection(name).unwrap().is_empty());
    }
    !graph
        .find_nodes(
            &FindNodes {
                label: Some("VliteMemoryScope".into()),
                limit: Some(1),
                ..FindNodes::default()
            },
            &ReadOptions::default(),
        )
        .unwrap()
        .output
        .is_empty()
}

#[test]
fn the_released_adoption_rule_accepts_a_recorded_graph_that_holds_a_scope() {
    let mut db = database();
    let notes = binding("notes", "g1", Some("ev1"));
    adopted(&mut db, vec![notes]);
    provision(&mut db, "g1", "@root", "a");
    assert!(record_node(&db, "g1").is_some());
    assert!(released_adoptable(&db, "g1", Some("ev1")));
}

#[test]
fn the_released_adoption_rule_refuses_a_graph_that_holds_only_the_record() {
    // The documented rollback limit: a binding that never had a scope.
    let mut db = database();
    let mut notes = binding("notes", "g1", None);
    notes.grants.clear();
    adopted(&mut db, vec![notes]);
    assert!(record_node(&db, "g1").is_some());
    assert!(!released_adoptable(&db, "g1", None));
}

#[test]
fn memory_operations_ignore_the_record() {
    let mut db = database();
    let notes = binding("notes", "g1", None);
    adopted(&mut db, vec![notes.clone()]);
    provision(&mut db, "g1", "@root", "a");
    remember(&mut db, "g1", "m1", "k1");
    remember(&mut db, "g1", "m2", "k2");
    let forgotten = MemoryMut::bind(&mut db, "g1", None)
        .unwrap()
        .forget(&ForgetRequest {
            scope: scope("@root", "a"),
            idempotency_key: "f1".into(),
            receipt_window_ms: DAY_MS,
            now_ms: NOW,
            deadline_ms: None,
            targets: vec![ForgetTarget {
                id: "m1".into(),
                expected_revision: 1,
            }],
        })
        .unwrap();
    assert_eq!(forgotten.deleted, ["m1"]);
    assert_eq!(stored_record(&db, "g1"), Recorded::of(&notes));
    // The record is not a memory scan candidate: one live memory remains.
    let scan = db
        .graph("g1")
        .unwrap()
        .with_clock(NOW)
        .find_nodes(
            &FindNodes {
                label: Some("VliteMemory".into()),
                ..FindNodes::default()
            },
            &ReadOptions::default(),
        )
        .unwrap();
    assert_eq!(scan.output.len(), 1);
}
