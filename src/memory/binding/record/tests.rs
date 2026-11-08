use super::*;
use crate::memory::MemoryScope;
use crate::property_graph::GraphOp;
use crate::{CreateOptions, Database, Expected, FormatVersion};
use serde_json::Value;

/// Literal expectations derived from the design note by an independent
/// reference (`tests/memory_binding_record_oracle.py`), not from this code.
const GOLDEN: &str =
    include_str!("../../../../tests/fixtures/agent_memory/binding_record_golden.json");

/// SHA-256 of the domain, a zero grant count and nothing else, from the
/// reference implementation.
const EMPTY_GRANTS_DIGEST: &str =
    "v1:94c287f855405a8ba5cbb357529deb0b1bfeb17a3af994d23adafeb849c11272";

fn golden() -> Value {
    serde_json::from_str(GOLDEN).unwrap()
}

fn text(value: &Value) -> String {
    value.as_str().unwrap().to_owned()
}

fn binding_of(entry: &Value) -> Binding {
    let tenant = text(&entry["tenant"]);
    Binding {
        binding: text(&entry["binding"]),
        tenant: tenant.clone(),
        generation: entry["generation"].as_u64().unwrap(),
        state: match entry["state"].as_str().unwrap() {
            "active" => BindingState::Active,
            "retired" => BindingState::Retired {
                at_ms: entry["retired_at_ms"].as_u64().unwrap(),
            },
            other => panic!("unknown state {other}"),
        },
        graph: text(&entry["graph"]),
        evidence_collection: entry["evidence_collection"].as_str().map(str::to_owned),
        receipt_window_ms: entry["receipt_window_ms"].as_u64().unwrap(),
        grants: entry["grants"]
            .as_array()
            .unwrap()
            .iter()
            .map(|grant| Grant {
                principal: text(&grant["principal"]),
                raw_access: grant
                    .get("raw_access")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                scopes: grant["scopes"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|scope| MemoryScope {
                        tenant: tenant.clone(),
                        agent: text(&scope["agent"]),
                        user: scope.get("user").map(text),
                        session: scope.get("session").map(text),
                    })
                    .collect(),
                read_scopes: grant
                    .get("read_scopes")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .map(|scope| MemoryScope {
                        tenant: tenant.clone(),
                        agent: text(&scope["agent"]),
                        user: scope.get("user").map(text),
                        session: scope.get("session").map(text),
                    })
                    .collect(),
            })
            .collect(),
    }
}

fn properties_of(value: &Value) -> Metadata {
    value
        .as_object()
        .unwrap()
        .iter()
        .map(|(name, value)| {
            let value = match value {
                Value::String(text) => MetadataValue::String(text.clone()),
                Value::Number(number) => MetadataValue::Integer(number.as_i64().unwrap()),
                Value::Bool(flag) => MetadataValue::Boolean(*flag),
                other => panic!("unsupported fixture value {other}"),
            };
            (name.clone(), value)
        })
        .collect()
}

fn database(holder: &str) -> Database {
    let mut db = Database::create_in_memory(
        2,
        CreateOptions {
            format: Some(FormatVersion::V5),
            ..Default::default()
        },
    )
    .unwrap();
    db.create_graph(holder).unwrap();
    db
}

/// Writes one node under the record key through the real graph write path
/// and reads it back, so labels and expiry are as the engine reports them.
fn stored_node(
    holder: &str,
    labels: Vec<String>,
    properties: Metadata,
    expires_at_ms: Option<u64>,
) -> NodeView {
    let mut db = database(holder);
    let mut ops = vec![GraphOp::CreateNode {
        key: KEY.into(),
        labels,
        properties,
    }];
    if let Some(at) = expires_at_ms {
        ops.push(GraphOp::SetNodeExpiry {
            key: KEY.into(),
            expires_at_ms: Some(at),
            expected: Expected::Any,
        });
    }
    db.graph_mut(holder).unwrap().write_batch(&ops).unwrap();
    db.graph(holder)
        .unwrap()
        .with_clock(1)
        .get_node(KEY)
        .unwrap()
        .output
        .unwrap()
}

#[test]
fn the_record_key_and_label_are_pinned() {
    let golden = golden();
    assert_eq!(golden["key"], KEY);
    assert_eq!(golden["label"], LABEL);
    assert_eq!(KEY, "binding:v1");
    assert_eq!(LABEL, "VliteMemoryBinding");
}

#[test]
fn golden_records_encode_exactly_as_designed() {
    let golden = golden();
    let records = golden["records"].as_array().unwrap();
    assert!(records.len() >= 10);
    for case in records {
        let name = case["name"].as_str().unwrap();
        let binding = binding_of(&case["entry"]);
        assert_eq!(
            Recorded::of(&binding).properties(),
            properties_of(&case["expected"]["properties"]),
            "{name}"
        );
        assert_eq!(
            Recorded::of(&binding).stored_graph(),
            text(&case["expected"]["holder"]),
            "{name}"
        );
    }
}

#[test]
fn golden_records_survive_a_write_and_a_strict_decode() {
    let golden = golden();
    for case in golden["records"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let binding = binding_of(&case["entry"]);
        let holder = text(&case["expected"]["holder"]);
        let node = stored_node(
            &holder,
            vec![LABEL.into()],
            properties_of(&case["expected"]["properties"]),
            None,
        );
        assert_eq!(
            Recorded::decode(&node, &holder),
            Ok(Recorded::of(&binding)),
            "{name}"
        );
    }
}

#[test]
fn every_malformed_golden_node_is_corrupt() {
    let golden = golden();
    let cases = golden["malformed"].as_array().unwrap();
    assert!(cases.len() >= 40);
    for case in cases {
        let name = case["name"].as_str().unwrap();
        let holder = text(&case["holder"]);
        let node = &case["node"];
        let labels = node["labels"]
            .as_array()
            .unwrap()
            .iter()
            .map(text)
            .collect();
        let expires = node["expires_at_ms"].as_u64();
        let stored = stored_node(&holder, labels, properties_of(&node["properties"]), expires);
        assert_eq!(Recorded::decode(&stored, &holder), Err(Corrupt), "{name}");
    }
}

#[test]
fn a_node_with_another_key_is_never_a_record() {
    let golden = golden();
    let case = &golden["records"][0];
    let holder = text(&case["expected"]["holder"]);
    let mut db = database(&holder);
    db.graph_mut(&holder)
        .unwrap()
        .write_batch(&[GraphOp::CreateNode {
            key: "binding:v2".into(),
            labels: vec![LABEL.into()],
            properties: properties_of(&case["expected"]["properties"]),
        }])
        .unwrap();
    let node = db
        .graph(&holder)
        .unwrap()
        .get_node("binding:v2")
        .unwrap()
        .output
        .unwrap();
    assert_eq!(Recorded::decode(&node, &holder), Err(Corrupt));
}

#[test]
fn the_grants_digest_covers_everything_the_design_names() {
    assert_eq!(grants_digest(&[]), EMPTY_GRANTS_DIGEST);
    let golden = golden();
    let base = binding_of(&golden["records"][0]["entry"]).grants;
    assert_eq!(base.len(), 1);
    let digest = grants_digest(&base);
    let mut changed = Vec::new();
    let mut raw = base.clone();
    raw[0].raw_access = true;
    changed.push(raw);
    let mut principal = base.clone();
    principal[0].principal = "bcdefghij234".into();
    changed.push(principal);
    let mut agent = base.clone();
    agent[0].scopes[0].agent = "b".into();
    changed.push(agent);
    let mut user = base.clone();
    user[0].scopes[0].user = Some("u".into());
    changed.push(user);
    let mut tenant = base.clone();
    tenant[0].scopes[0].tenant = "0123456789abcdef0123456789abcdef".into();
    changed.push(tenant);
    let mut more = base.clone();
    let extra = more[0].scopes[0].clone();
    more[0].scopes.push(extra);
    changed.push(more);
    let mut twice = base.clone();
    twice.push(base[0].clone());
    changed.push(twice);
    changed.push(Vec::new());
    // A read scope added, and the one scope moved to the read list.
    let mut read = base.clone();
    let mut other = base[0].scopes[0].clone();
    other.agent = "b".into();
    read[0].read_scopes.push(other);
    changed.push(read);
    let mut moved = base.clone();
    moved[0].read_scopes = std::mem::take(&mut moved[0].scopes);
    changed.push(moved);
    let mut seen = std::collections::BTreeSet::new();
    seen.insert(digest);
    for grants in changed {
        assert!(seen.insert(grants_digest(&grants)), "{grants:?}");
    }
}

#[test]
fn a_user_and_a_session_are_not_interchangeable_in_the_digest() {
    let scope = |user: Option<&str>, session: Option<&str>| Grant {
        principal: "abcdefghij27".into(),
        raw_access: false,
        read_scopes: Vec::new(),
        scopes: vec![MemoryScope {
            tenant: "@root".into(),
            agent: "a".into(),
            user: user.map(str::to_owned),
            session: session.map(str::to_owned),
        }],
    };
    assert_ne!(
        grants_digest(&[scope(Some("x"), None)]),
        grants_digest(&[scope(Some("x"), Some("y"))])
    );
    assert_ne!(
        grants_digest(&[scope(Some("xy"), None)]),
        grants_digest(&[scope(Some("x"), Some("y"))])
    );
}

#[test]
fn read_scopes_change_the_digest_only_when_present() {
    let scope = |agent: &str| MemoryScope {
        tenant: "@root".into(),
        agent: agent.into(),
        user: None,
        session: None,
    };
    let grant = |principal: &str, scopes: &[&str], read: &[&str]| Grant {
        principal: principal.into(),
        raw_access: false,
        scopes: scopes.iter().map(|agent| scope(agent)).collect(),
        read_scopes: read.iter().map(|agent| scope(agent)).collect(),
    };
    // `[]` is the legacy encoding: the golden `grants-plain` digest.
    let golden = golden();
    let plain = golden["records"]
        .as_array()
        .unwrap()
        .iter()
        .find(|case| case["name"] == "grants-plain")
        .unwrap();
    let plain_digest = text(&plain["expected"]["properties"]["grants_digest"]);
    assert_eq!(
        plain_digest,
        "v1:f567d9dd44e7c3eb2027f756c63e94a174eddf15ad38822df9931d28f11e1ee7"
    );
    assert_eq!(
        grants_digest(&[grant("abcdefghij27", &["c"], &[])]),
        plain_digest
    );
    assert_eq!(grants_digest(&[]), EMPTY_GRANTS_DIGEST);
    // Which grant holds the read scope is part of the digest.
    assert_ne!(
        grants_digest(&[
            grant("abcdefghij27", &["a"], &["b"]),
            grant("bcdefghij234", &["c"], &[]),
        ]),
        grants_digest(&[
            grant("abcdefghij27", &["a"], &[]),
            grant("bcdefghij234", &["c"], &["b"]),
        ])
    );
    // A read scope is not a write scope.
    assert_ne!(
        grants_digest(&[grant("abcdefghij27", &["a"], &["b"])]),
        grants_digest(&[grant("abcdefghij27", &["a", "b"], &[])])
    );
    assert_ne!(
        grants_digest(&[grant("abcdefghij27", &[], &["a"])]),
        grants_digest(&[grant("abcdefghij27", &["a"], &[])])
    );
    // With read scopes anywhere, every grant gains a read-scope count.
    assert_ne!(
        grants_digest(&[
            grant("abcdefghij27", &["a"], &[]),
            grant("bcdefghij234", &[], &["b"]),
        ]),
        grants_digest(&[
            grant("abcdefghij27", &["a"], &["b"]),
            grant("bcdefghij234", &[], &[]),
        ])
    );
    let digest = grants_digest(&[grant("abcdefghij27", &["a"], &["b"])]);
    assert!(digest_shaped(&digest), "{digest}");
}

#[test]
fn previous_state_must_be_self_consistent() {
    let golden = golden();
    let first = Recorded::of(&binding_of(&golden["records"][0]["entry"]));
    let mut other = first.clone();
    other.binding = "other".into();
    other.graph = "other_graph".into();
    assert_eq!(validate_all(&[first.clone(), other.clone()]), Ok(()));
    assert_eq!(validate_all(&[]), Ok(()));
    let mut same_key = other.clone();
    same_key.graph = "third_graph".into();
    same_key.binding = first.binding.clone();
    assert_eq!(
        validate_all(&[first.clone(), same_key]),
        Err(BindingError::DuplicateBinding)
    );
    let mut same_graph = other.clone();
    same_graph.graph = first.graph.clone();
    assert_eq!(
        validate_all(&[first.clone(), same_graph]),
        Err(BindingError::SharedStorage)
    );
    let mut same_evidence = other.clone();
    same_evidence.evidence = Some("shared_ev".into());
    let mut with_evidence = first.clone();
    with_evidence.evidence = Some("shared_ev".into());
    assert_eq!(
        validate_all(&[with_evidence.clone(), same_evidence.clone()]),
        Err(BindingError::SharedStorage)
    );
    // The same names under another tenant are other stored names.
    same_evidence.tenant = "0123456789abcdef0123456789abcdef".into();
    assert_eq!(validate_all(&[with_evidence, same_evidence]), Ok(()));
}

#[test]
fn storage_names_follow_the_tenant() {
    let golden = golden();
    for case in golden["records"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let binding = binding_of(&case["entry"]);
        let recorded = Recorded::of(&binding);
        assert_eq!(
            recorded.storage().collect::<Vec<_>>(),
            binding.storage().collect::<Vec<_>>(),
            "{name}"
        );
        assert_eq!(
            recorded.stored_graph(),
            binding.stored(&binding.graph),
            "{name}"
        );
        assert!(recorded.is(&binding.tenant, &binding.binding), "{name}");
        assert!(!recorded.is(&binding.tenant, "elsewhere"), "{name}");
        assert!(
            !recorded.is("0123456789abcdef0123456789abcdee", &binding.binding),
            "{name}"
        );
    }
}
