use super::*;

fn expiring_b() -> TestGraph {
    let mut db = hand_graph();
    db.run(&[GraphMutation::SetNodeExpiry {
        key: "b".into(),
        expires_at_ms: Some(NOW),
        expected: Precondition::Any,
    }]);
    db
}

#[test]
fn due_nonseed_path_source_fails_before_seed_membership() {
    let db = expiring_b();
    let scope = scope(&["a"], Direction::Out);
    let algorithm = path("b", "c", None);
    assert_eq!(
        run_at(db.graph(), &scope, &algorithm, &hops(2), &CAPS, NOW),
        Err(AlgorithmError::Graph(GraphError::UnknownNodeKey(
            "b".into()
        )))
    );
    assert_eq!(
        run_at(db.graph(), &scope, &algorithm, &hops(2), &CAPS, NOW - 1),
        Err(AlgorithmError::Graph(GraphError::InvalidQuery(
            "the path source must be a seed"
        )))
    );
}

#[test]
fn due_nonseed_personalization_honours_the_expiry_clock() {
    let db = expiring_b();
    let scope = scope(&["a"], Direction::Out);
    let algorithm = Algorithm::PageRank {
        damping: 0.85,
        tolerance: 1e-10,
        personalization: Some(vec![(key("b"), 1.0)]),
        weight_property: None,
    };
    assert_eq!(
        run_at(db.graph(), &scope, &algorithm, &hops(2), &CAPS, NOW),
        Err(AlgorithmError::Graph(GraphError::UnknownNodeKey(
            "b".into()
        )))
    );
    assert_eq!(
        run_at(db.graph(), &scope, &algorithm, &hops(2), &CAPS, NOW - 1),
        Err(AlgorithmError::Graph(GraphError::InvalidQuery(
            "personalization nodes must be seeds"
        )))
    );
}

#[test]
fn due_nonseed_path_destination_is_rejected() {
    let db = expiring_b();
    let scope = scope(&["a"], Direction::Out);
    let algorithm = path("a", "b", None);
    assert_eq!(
        run_at(db.graph(), &scope, &algorithm, &hops(2), &CAPS, NOW),
        Err(AlgorithmError::Graph(GraphError::UnknownNodeKey(
            "b".into()
        )))
    );
    let before = run_at(db.graph(), &scope, &algorithm, &hops(2), &CAPS, NOW - 1).unwrap();
    assert_eq!(found(&before).nodes, vec![db.id("a"), db.id("b")]);
}

#[test]
fn due_seed_is_rejected_for_an_iterative_request() {
    let db = expiring_b();
    let scope = scope(&["b"], Direction::Out);
    let algorithm = Algorithm::PageRank {
        damping: 0.85,
        tolerance: 1e-10,
        personalization: None,
        weight_property: None,
    };
    assert_eq!(
        run_at(db.graph(), &scope, &algorithm, &hops(2), &CAPS, NOW),
        Err(AlgorithmError::Graph(GraphError::UnknownNodeKey(
            "b".into()
        )))
    );
    assert!(run_at(db.graph(), &scope, &algorithm, &hops(2), &CAPS, NOW - 1).is_ok());
}
