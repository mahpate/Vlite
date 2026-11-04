use super::*;

const HEX_TENANT: &str = "0123456789abcdef0123456789abcdef";
const DAY_MS: u64 = 86_400_000;

fn scope(tenant: &str, agent: &str) -> MemoryScope {
    MemoryScope {
        tenant: tenant.into(),
        agent: agent.into(),
        user: None,
        session: None,
    }
}

fn entry(binding: &str, graph: &str) -> Binding {
    Binding {
        binding: binding.into(),
        tenant: "@root".into(),
        generation: 1,
        state: BindingState::Active,
        graph: graph.into(),
        evidence_collection: None,
        receipt_window_ms: DAY_MS,
        grants: vec![Grant {
            principal: "key-id".into(),
            scopes: vec![scope("@root", "a")],
        }],
    }
}

fn set(bindings: Vec<Binding>) -> BindingSet {
    BindingSet { bindings }
}

fn invalid(binding: Binding) -> Result<(), BindingError> {
    set(vec![binding]).validate()
}

#[test]
fn field_rules_accept_their_boundaries() {
    let mut longest = entry(&format!("a{}", "z".repeat(62)), "g");
    longest.tenant = HEX_TENANT.into();
    longest.grants[0].scopes = vec![scope(HEX_TENANT, "a")];
    longest.generation = MAX_GENERATION;
    longest.receipt_window_ms = MAX_RECEIPT_WINDOW_MS;
    longest.evidence_collection = Some("evidence".into());
    let mut smallest = entry("n", "g2");
    smallest.receipt_window_ms = 1;
    smallest.grants.clear();
    let mut same_name_other_tenant = entry("n", "g3");
    same_name_other_tenant.tenant = HEX_TENANT.into();
    same_name_other_tenant.grants[0].scopes = vec![MemoryScope {
        tenant: HEX_TENANT.into(),
        agent: "a".into(),
        user: Some("u".into()),
        session: Some("s".into()),
    }];
    let mut retired = entry("r_9-x", "g4");
    retired.state = BindingState::Retired { at_ms: 0 };
    assert_eq!(
        set(vec![longest, smallest, same_name_other_tenant, retired]).validate(),
        Ok(())
    );
    assert_eq!(BindingSet::default().validate(), Ok(()));
}

#[test]
fn field_rules_reject_each_bad_field() {
    use BindingError::*;
    let mut cases: Vec<(Binding, BindingError)> = Vec::new();
    let too_long = format!("a{}", "z".repeat(63));
    for name in ["", "A", "9a", "_a", "a.b", "a b", "é", too_long.as_str()] {
        cases.push((entry(name, "g"), InvalidName));
    }
    for tenant in [
        "",
        "root",
        "@ROOT",
        "0123456789ABCDEF0123456789abcdef",
        "0123456789abcdef0123456789abcde",
        "0123456789abcdef0123456789abcdef0",
        "0123456789abcdef0123456789abcdeg",
    ] {
        let mut bad = entry("n", "g");
        bad.tenant = tenant.into();
        bad.grants[0].scopes = vec![scope(tenant, "a")];
        cases.push((bad, InvalidTenant));
    }
    for generation in [0, MAX_GENERATION + 1, u64::MAX] {
        let mut bad = entry("n", "g");
        bad.generation = generation;
        cases.push((bad, InvalidGeneration));
    }
    for window in [0, MAX_RECEIPT_WINDOW_MS + 1] {
        let mut bad = entry("n", "g");
        bad.receipt_window_ms = window;
        cases.push((bad, InvalidWindow));
    }
    // Storage names follow the stored graph and collection rule.
    let too_long_storage = format!("g{}", "z".repeat(63));
    for graph in ["", "G", "9g", "-g", "g.h", "g h", too_long_storage.as_str()] {
        cases.push((entry("n", graph), InvalidName));
    }
    for collection in ["", "E", "e.f", too_long_storage.as_str()] {
        let mut bad = entry("n", "g");
        bad.evidence_collection = Some(collection.into());
        cases.push((bad, InvalidName));
    }
    let grant_cases: Vec<Vec<Grant>> = vec![
        vec![Grant {
            principal: "p".repeat(257),
            scopes: vec![scope("@root", "a")],
        }],
        vec![Grant {
            principal: String::new(),
            scopes: vec![scope("@root", "a")],
        }],
        vec![Grant {
            principal: "p".into(),
            scopes: Vec::new(),
        }],
        vec![
            Grant {
                principal: "p".into(),
                scopes: vec![scope("@root", "a")],
            },
            Grant {
                principal: "p".into(),
                scopes: vec![scope("@root", "b")],
            },
        ],
        vec![Grant {
            principal: "p".into(),
            scopes: vec![scope("@root", "a"), scope("@root", "a")],
        }],
        vec![Grant {
            principal: "p".into(),
            scopes: vec![scope("@root", "")],
        }],
        vec![Grant {
            principal: "p".into(),
            scopes: vec![MemoryScope {
                tenant: "@root".into(),
                agent: "a".into(),
                user: None,
                session: Some("s".into()),
            }],
        }],
        // A scope must be in the entry's own tenant.
        vec![Grant {
            principal: "p".into(),
            scopes: vec![scope(HEX_TENANT, "a")],
        }],
    ];
    for grants in grant_cases {
        let mut bad = entry("n", "g");
        bad.grants = grants;
        cases.push((bad, InvalidGrant));
    }
    for (binding, expected) in cases {
        assert_eq!(invalid(binding.clone()), Err(expected), "{binding:?}");
    }
}

#[test]
fn set_rules_reject_duplicate_keys_and_shared_storage() {
    assert_eq!(
        set(vec![entry("n", "g1"), entry("n", "g2")]).validate(),
        Err(BindingError::DuplicateBinding)
    );
    assert_eq!(
        set(vec![entry("n", "g"), entry("m", "g")]).validate(),
        Err(BindingError::SharedStorage)
    );
    let mut first = entry("n", "g1");
    first.evidence_collection = Some("e".into());
    let mut second = entry("m", "g2");
    second.evidence_collection = Some("e".into());
    assert_eq!(
        set(vec![first.clone(), second]).validate(),
        Err(BindingError::SharedStorage)
    );
    // A retired entry still owns its storage.
    let mut retired = entry("m", "g1");
    retired.state = BindingState::Retired { at_ms: 5 };
    assert_eq!(
        set(vec![entry("n", "g1"), retired]).validate(),
        Err(BindingError::SharedStorage)
    );
    // Graph and evidence-collection names are separate namespaces.
    assert_eq!(set(vec![first, entry("m", "e")]).validate(), Ok(()));
    let found = set(vec![entry("n", "g1")]);
    assert_eq!(
        found.find("@root", "n").map(|b| b.graph.as_str()),
        Some("g1")
    );
    assert!(found.find(HEX_TENANT, "n").is_none());
    assert!(found.find("@root", "m").is_none());
}

fn rotate(old: Vec<Binding>, new: Vec<Binding>, now_ms: u64) -> Result<(), BindingError> {
    set(new).validate_rotation(&set(old), now_ms)
}

#[test]
fn rotation_keeps_identity_and_advances_generation_only_on_change() {
    use BindingError::*;
    let old = entry("n", "g");
    assert_eq!(rotate(vec![old.clone()], vec![old.clone()], 10), Ok(()));
    let mut bumped = old.clone();
    bumped.generation = 2;
    assert_eq!(
        rotate(vec![old.clone()], vec![bumped], 10),
        Err(GenerationChanged)
    );
    let mut regranted = old.clone();
    regranted.grants[0].principal = "other".into();
    assert_eq!(
        rotate(vec![old.clone()], vec![regranted.clone()], 10),
        Err(GenerationNotAdvanced)
    );
    regranted.generation = 3;
    assert_eq!(
        rotate(vec![old.clone()], vec![regranted.clone()], 10),
        Ok(())
    );
    let mut lowered = regranted.clone();
    lowered.generation = 2;
    lowered.grants.clear();
    assert_eq!(
        rotate(vec![regranted], vec![lowered], 10),
        Err(GenerationNotAdvanced)
    );
    let identity: [fn(&mut Binding); 3] = [
        |b| b.graph = "g2".into(),
        |b| b.evidence_collection = Some("e".into()),
        |b| b.receipt_window_ms += 1,
    ];
    for change in identity {
        let mut changed = old.clone();
        change(&mut changed);
        changed.generation = 2;
        assert_eq!(
            rotate(vec![old.clone()], vec![changed.clone()], 10),
            Err(IdentityChanged),
            "{changed:?}"
        );
    }
    // A new entry may be added with any valid generation.
    let mut added = entry("m", "g2");
    added.generation = 7;
    assert_eq!(rotate(vec![old.clone()], vec![old, added], 10), Ok(()));
    // Both sets are validated.
    assert_eq!(rotate(vec![], vec![entry("N", "g")], 10), Err(InvalidName));
    assert_eq!(rotate(vec![entry("N", "g")], vec![], 10), Err(InvalidName));
}

#[test]
fn rotation_retires_one_way_and_never_backdates() {
    use BindingError::*;
    let active = entry("n", "g");
    let mut retired = active.clone();
    retired.generation = 2;
    retired.state = BindingState::Retired { at_ms: 10 };
    assert_eq!(
        rotate(vec![active.clone()], vec![retired.clone()], 10),
        Ok(())
    );
    assert_eq!(
        rotate(vec![active.clone()], vec![retired.clone()], 11),
        Err(BackdatedRetirement)
    );
    let mut unbumped = retired.clone();
    unbumped.generation = 1;
    assert_eq!(
        rotate(vec![active], vec![unbumped], 10),
        Err(GenerationNotAdvanced)
    );
    let mut reactivated = retired.clone();
    reactivated.generation = 3;
    reactivated.state = BindingState::Active;
    assert_eq!(
        rotate(vec![retired.clone()], vec![reactivated], 20),
        Err(Reactivated)
    );
    for at_ms in [9, 11] {
        let mut moved = retired.clone();
        moved.generation = 3;
        moved.state = BindingState::Retired { at_ms };
        assert_eq!(
            rotate(vec![retired.clone()], vec![moved], 20),
            Err(RetirementChanged)
        );
    }
    // A retired entry may still change grants (generation advances).
    let mut regranted = retired.clone();
    regranted.generation = 3;
    regranted.grants.clear();
    assert_eq!(rotate(vec![retired], vec![regranted], 20), Ok(()));
}

#[test]
fn rotation_removes_only_after_the_last_receipt_window() {
    use BindingError::*;
    let mut retired = entry("n", "g");
    retired.state = BindingState::Retired { at_ms: 100 };
    retired.receipt_window_ms = 50;
    assert_eq!(
        rotate(vec![entry("n", "g")], vec![], u64::MAX),
        Err(EarlyRemoval)
    );
    assert_eq!(
        rotate(vec![retired.clone()], vec![], 149),
        Err(EarlyRemoval)
    );
    assert_eq!(rotate(vec![retired.clone()], vec![], 150), Ok(()));
    let mut far = retired.clone();
    far.state = BindingState::Retired {
        at_ms: u64::MAX - 10,
    };
    assert_eq!(rotate(vec![far], vec![], u64::MAX), Err(EarlyRemoval));
    // Storage of a removed entry may not be reused by another key at once.
    assert_eq!(
        rotate(vec![retired.clone()], vec![entry("m", "g")], 150),
        Err(StorageReused)
    );
    let mut with_evidence = retired.clone();
    with_evidence.evidence_collection = Some("e".into());
    let mut reuser = entry("m", "g2");
    reuser.evidence_collection = Some("e".into());
    assert_eq!(
        rotate(vec![with_evidence], vec![reuser], 150),
        Err(StorageReused)
    );
    // The same name in another tenant is a different key, so also refused.
    let mut other_tenant = entry("n", "g");
    other_tenant.tenant = HEX_TENANT.into();
    other_tenant.grants[0].scopes = vec![scope(HEX_TENANT, "a")];
    assert_eq!(
        rotate(vec![retired], vec![other_tenant], 150),
        Err(StorageReused)
    );
}

#[test]
fn retired_bindings_serve_receipt_reads_only() {
    use WireOperation::*;
    let active = entry("n", "g");
    for operation in [Remember, Recall, Consolidate, Forget, GetReceipt] {
        assert_eq!(active.refusal(operation), None, "{operation:?}");
    }
    assert_eq!(active.refusal(FindFacts), Some(unavailable("facts")));
    let mut retired = active;
    retired.state = BindingState::Retired { at_ms: 1 };
    assert_eq!(retired.refusal(GetReceipt), None);
    assert_eq!(
        retired.refusal(GetErasure),
        Some(unavailable("erasure_jobs"))
    );
    for operation in [
        Remember,
        Recall,
        Consolidate,
        Forget,
        FindFacts,
        CorrectFacts,
        ResumeErasure,
    ] {
        assert_eq!(
            retired.refusal(operation),
            Some(unavailable("binding_retired")),
            "{operation:?}"
        );
    }
}

#[cfg(feature = "json")]
mod json {
    use super::*;

    const EXAMPLE: &str = r#"{"version":"memory-bindings-v1","bindings":[{"binding":"notes","tenant":"@root",
 "generation":1,"state":"active","graph":"notes_graph","evidence_collection":null,
 "schema_version":"memory-schema-v1","ranking_version":"memory-ranking-v1",
 "digest_version":"memory-request-v1","receipt_window_ms":86400000,
 "grants":[{"principal":"key-id","scopes":[{"agent":"a"}]}]}]}"#;

    fn edit(from: &str, to: &str) -> String {
        assert!(EXAMPLE.contains(from), "{from}");
        EXAMPLE.replace(from, to)
    }

    #[test]
    fn the_documented_example_parses_exactly() {
        let mut expected = entry("notes", "notes_graph");
        expected.receipt_window_ms = 86_400_000;
        assert_eq!(parse(EXAMPLE.as_bytes()), Ok(set(vec![expected])));
        let retired = EXAMPLE
            .replace(
                r#""state":"active""#,
                r#""state":"retired","retired_at_ms":5"#,
            )
            .replace(
                r#"{"agent":"a"}"#,
                r#"{"agent":"a","user":"u","session":"s"}"#,
            )
            .replace(
                r#""evidence_collection":null"#,
                r#""evidence_collection":"ev""#,
            );
        let parsed = parse(retired.as_bytes()).unwrap();
        let entry = &parsed.bindings[0];
        assert_eq!(entry.state, BindingState::Retired { at_ms: 5 });
        assert_eq!(entry.evidence_collection.as_deref(), Some("ev"));
        assert_eq!(
            entry.grants[0].scopes,
            vec![MemoryScope {
                tenant: "@root".into(),
                agent: "a".into(),
                user: Some("u".into()),
                session: Some("s".into()),
            }]
        );
    }

    #[test]
    fn strict_parsing_rejects_every_fallback() {
        use BindingError::*;
        let state = r#""state":"active""#;
        let window = r#""receipt_window_ms":86400000"#;
        let cases: Vec<(String, BindingError)> = vec![
            (edit("memory-bindings-v1", "memory-bindings-v2"), Malformed),
            (edit("memory-schema-v1", "memory-schema-v2"), Malformed),
            (edit("memory-ranking-v1", "memory-ranking-v0"), Malformed),
            (edit("memory-request-v1", "memory-request-v2"), Malformed),
            (edit(r#""version""#, r#""extra":1,"version""#), Malformed),
            (
                edit(r#""binding":"notes""#, r#""binding":"notes","x":1"#),
                Malformed,
            ),
            (
                edit(r#""binding":"notes""#, r#""binding":"notes","binding":"n""#),
                Malformed,
            ),
            (
                edit(r#""graph":"notes_graph""#, r#""graph":null"#),
                Malformed,
            ),
            (edit(r#""evidence_collection":null,"#, ""), Malformed),
            (edit(r#""receipt_window_ms":86400000,"#, ""), Malformed),
            (edit(window, r#""receipt_window_ms":"86400000""#), Malformed),
            (
                edit(
                    r#""receipt_window_ms":86400000"#,
                    r#""receipt_window_ms":2592000001"#,
                ),
                InvalidWindow,
            ),
            (
                edit(r#""generation":1"#, r#""generation":9223372036854775808"#),
                InvalidGeneration,
            ),
            (
                edit(r#""generation":1"#, r#""generation":18446744073709551616"#),
                Malformed,
            ),
            (
                edit(r#"{"agent":"a"}"#, r#"{"agent":"a","agent":"b"}"#),
                Malformed,
            ),
            (
                edit(
                    r#""grants":["#,
                    r#""grants":[{"principal":"p","scopes":[],"x":1},"#,
                ),
                Malformed,
            ),
            (edit(r#""tenant":"@root""#, r#""tenant":null"#), Malformed),
            (edit(r#""grants":["#, r#""grants":null,"g":["#), Malformed),
            (
                edit(r#""graph":"notes_graph""#, r#""graph":"Notes""#),
                InvalidName,
            ),
            (edit(r#""generation":1"#, r#""generation":-1"#), Malformed),
            (edit(r#""generation":1"#, r#""generation":1.0"#), Malformed),
            (edit(state, r#""state":"paused""#), Malformed),
            (
                edit(r#"{"agent":"a"}"#, r#"{"agent":"a","user":null}"#),
                Malformed,
            ),
            (
                edit(r#"{"agent":"a"}"#, r#"{"agent":"a","tenant":"@root"}"#),
                Malformed,
            ),
            (
                edit(state, r#""state":"active","retired_at_ms":null"#),
                Malformed,
            ),
            (format!("{EXAMPLE} {{}}"), Malformed),
            (String::new(), Malformed),
            ("null".into(), Malformed),
            (edit(state, r#""state":"retired""#), InvalidState),
            (
                edit(state, r#""state":"active","retired_at_ms":5"#),
                InvalidState,
            ),
            (
                edit(r#""binding":"notes""#, r#""binding":"Notes""#),
                InvalidName,
            ),
            (edit(window, r#""receipt_window_ms":0"#), InvalidWindow),
        ];
        for (text, expected) in cases {
            assert_eq!(parse(text.as_bytes()), Err(expected), "{text}");
        }
    }
}
