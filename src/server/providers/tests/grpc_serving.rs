use super::*;
use crate::server::grpc::{self as transport, ERROR_CODE_TRAILER, proto};
use tonic::{Code, Request, Status};

fn authorized<T>(value: T, token: &str) -> Request<T> {
    let mut request = Request::new(value);
    request
        .metadata_mut()
        .insert("authorization", format!("Bearer {token}").parse().unwrap());
    request
}

fn embed(name: &str, alias: &str) -> proto::EmbedTextRequest {
    proto::EmbedTextRequest {
        collection: name.into(),
        text: "hello".into(),
        provider: alias.into(),
    }
}

fn query(name: &str) -> proto::SearchRequest {
    proto::SearchRequest {
        collection: name.into(),
        vector: vec![1.0, 0.0],
        k: 1,
        rerank_mode: proto::RerankMode::Custom as i32,
        rerank_provider: Some("test".into()),
        rerank_candidates: Some(2),
        ..Default::default()
    }
}

fn error(status: &Status, expected: Code, code: &str) {
    assert_eq!(status.code(), expected, "{status}");
    assert_eq!(status.metadata().get(ERROR_CODE_TRAILER).unwrap(), code);
}

#[tokio::test]
async fn grpc_managed_embedding_and_custom_ranking_share_bound_workflow() {
    let f = fixture(
        "embedding",
        200,
        r#"{"data":[{"embedding":[1.0,0.0]}]}"#,
        &[],
    );
    let result =
        transport::test_embed(f.service.clone(), authorized(embed(&f.name, "test"), ADMIN))
            .await
            .unwrap()
            .into_inner();
    assert_eq!(result.vector, vec![1.0, 0.0]);
    assert_eq!(result.dimensions, 2);
    assert_eq!(result.provider, "test");
    let f = fixture(
        "rerank",
        200,
        r#"{"results":[{"id":2,"cost":-0.8}]}"#,
        &["public"],
    );
    let response = transport::test_search(f.service.clone(), authorized(query(&f.name), ADMIN))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        response.hits,
        vec![proto::Hit {
            id: 2,
            distance: 2.0,
            rerank_score: Some(f64::from(0.8_f32))
        }]
    );
    assert_eq!(
        response.ranking,
        Some(proto::Ranking {
            provider: "test".into(),
            order: "score_desc_id_asc".into(),
        })
    );
    let bodies = f.transport.bodies.lock().unwrap();
    assert_eq!(bodies[0]["candidates"].as_array().unwrap().len(), 2);
    assert!(!bodies[0].to_string().contains("sentinel-secret"));
    assert_eq!(bodies[0]["candidates"][0]["vector"], json!([1.0, 0.0]));
}

#[tokio::test]
async fn grpc_auth_and_tenant_fail_before_alias_or_provider_work() {
    let f = fixture(
        "embedding",
        200,
        r#"{"data":[{"embedding":[1.0,0.0]}]}"#,
        &[],
    );
    let missing =
        transport::test_embed(f.service.clone(), Request::new(embed(&f.name, "bad alias")))
            .await
            .unwrap_err();
    error(&missing, Code::Unauthenticated, "unauthorized");
    let read = credential(&f, TENANT, &["read"]).await;
    let denied = transport::test_embed(
        f.service.clone(),
        authorized(embed("items", "bad alias"), &read),
    )
    .await
    .unwrap_err();
    error(&denied, Code::PermissionDenied, "forbidden_scope");
    let other = credential(&f, OTHER, &["search"]).await;
    let denied = transport::test_embed(f.service.clone(), authorized(embed("items", ""), &read))
        .await
        .unwrap_err();
    error(&denied, Code::PermissionDenied, "forbidden_scope");
    let denied = transport::test_embed(
        f.service.clone(),
        authorized(embed(&f.name, "test"), &other),
    )
    .await
    .unwrap_err();
    assert_eq!(denied.code(), Code::PermissionDenied);
    let unbound = transport::test_embed(
        f.service.clone(),
        authorized(embed("items", "test"), &other),
    )
    .await
    .unwrap_err();
    error(
        &unbound,
        Code::FailedPrecondition,
        "provider_not_configured",
    );
    let missing = transport::test_search(f.service.clone(), Request::new(query(&f.name)))
        .await
        .unwrap_err();
    error(&missing, Code::Unauthenticated, "unauthorized");
    let denied = transport::test_search(f.service.clone(), authorized(query("items"), &read))
        .await
        .unwrap_err();
    error(&denied, Code::PermissionDenied, "forbidden_scope");
    let denied = transport::test_search(f.service.clone(), authorized(query(&f.name), &other))
        .await
        .unwrap_err();
    error(&denied, Code::PermissionDenied, "forbidden_scope");
    assert_eq!(f.transport.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn grpc_zero_configuration_and_legacy_modes_preserve_optional_absence() {
    let mut f = fixture(
        "embedding",
        200,
        r#"{"data":[{"embedding":[1.0,0.0]}]}"#,
        &[],
    );
    f.service.providers = Arc::new(ProviderRegistry {
        transport: f.transport.clone(),
        ..Default::default()
    });
    let response = transport::test_embed(f.service.clone(), authorized(embed(&f.name, ""), ADMIN))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(response.provider, "hashing");
    assert_eq!(response.dimensions, 2);
    let managed =
        transport::test_embed(f.service.clone(), authorized(embed(&f.name, "test"), ADMIN))
            .await
            .unwrap_err();
    error(
        &managed,
        Code::FailedPrecondition,
        "provider_not_configured",
    );
    for mode in [
        proto::RerankMode::None,
        proto::RerankMode::Exact,
        proto::RerankMode::Mmr,
    ] {
        let mut input = query(&f.name);
        input.rerank_mode = mode as i32;
        input.rerank_provider = None;
        input.rerank_candidates = (mode != proto::RerankMode::None).then_some(2);
        let response = transport::test_search(f.service.clone(), authorized(input, ADMIN))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(response.hits.len(), 1);
        assert!(response.ranking.is_none());
        assert!(response.hits.iter().all(|hit| hit.rerank_score.is_none()));
    }
    assert_eq!(f.transport.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn grpc_custom_limits_and_builtin_provider_options_fail_closed() {
    let f = fixture("rerank", 200, r#"{"results":[{"id":1,"cost":0.0}]}"#, &[]);
    for (k, candidates) in [(0, Some(2)), (1, Some(0)), (1, Some(1001)), (1001, None)] {
        let mut input = query(&f.name);
        input.k = k;
        input.rerank_candidates = candidates;
        let status = transport::test_search(f.service.clone(), authorized(input, ADMIN))
            .await
            .unwrap_err();
        error(&status, Code::InvalidArgument, "invalid_provider_input");
    }
    for mode in [
        proto::RerankMode::None,
        proto::RerankMode::Exact,
        proto::RerankMode::Mmr,
    ] {
        let mut input = query(&f.name);
        input.rerank_mode = mode as i32;
        let status = transport::test_search(f.service.clone(), authorized(input, ADMIN))
            .await
            .unwrap_err();
        error(&status, Code::InvalidArgument, "invalid_provider_input");
    }
    assert_eq!(f.transport.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn grpc_provider_failures_have_explicit_codes_and_sanitized_trailers() {
    for (status, response, expected, code) in [
        (
            401,
            "private-secret",
            Code::Unavailable,
            "provider_unavailable",
        ),
        (
            200,
            r#"{"data":[{"embedding":[1.0]}]}"#,
            Code::Unavailable,
            "provider_invalid_response",
        ),
    ] {
        let f = fixture("embedding", status, response, &[]);
        let result =
            transport::test_embed(f.service.clone(), authorized(embed(&f.name, "test"), ADMIN))
                .await
                .unwrap_err();
        error(&result, expected, code);
        assert!(!result.message().contains("private-secret"));
        assert_eq!(f.transport.calls.load(Ordering::SeqCst), 1);
    }
    let f = fixture(
        "embedding",
        200,
        r#"{"data":[{"embedding":[1.0,0.0]}]}"#,
        &[],
    );
    let slots = f.service.providers.providers["test"].slots.clone();
    let _held = slots.try_acquire_many(2).unwrap();
    let result =
        transport::test_embed(f.service.clone(), authorized(embed(&f.name, "test"), ADMIN))
            .await
            .unwrap_err();
    error(&result, Code::ResourceExhausted, "provider_busy");
    assert_eq!(result.metadata().get("retry-after").unwrap(), "1");
    assert_eq!(f.transport.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn grpc_snapshot_conflict_is_aborted_without_retry_hint() {
    for embedding in [true, false] {
        let f = if embedding {
            fixture(
                "embedding",
                200,
                r#"{"data":[{"embedding":[1.0,0.0]}]}"#,
                &[],
            )
        } else {
            fixture("rerank", 200, r#"{"results":[{"id":1,"cost":0.0}]}"#, &[])
        };
        let database = f.service.database.clone();
        let name = f.name.clone();
        *f.transport.action.lock().unwrap() = Some(Box::new(move || {
            let mut owner = database
                .try_write()
                .expect("inference releases database guard");
            let db = owner.as_mut().unwrap();
            let mut collection = db.collection_mut(&name).unwrap();
            collection.delete(1).unwrap();
            collection.delete(2).unwrap();
            db.drop_collection(&name).unwrap();
            db.create_collection(
                &name,
                CollectionSpec {
                    dimensions: 2,
                    settings: Default::default(),
                },
            )
            .unwrap();
        }));
        let result = if embedding {
            transport::test_embed(f.service.clone(), authorized(embed(&f.name, "test"), ADMIN))
                .await
                .unwrap_err()
        } else {
            transport::test_search(f.service.clone(), authorized(query(&f.name), ADMIN))
                .await
                .unwrap_err()
        };
        error(&result, Code::Aborted, "provider_snapshot_conflict");
        assert_eq!(result.message(), "provider request snapshot changed");
        assert!(result.metadata().get("retry-after").is_none());
    }
}

#[tokio::test]
async fn grpc_custom_ranking_orders_ties_and_normalizes_zero_score() {
    let f = fixture(
        "rerank",
        200,
        r#"{"results":[{"id":2,"cost":0.0},{"id":1,"cost":-0.0}]}"#,
        &[],
    );
    let mut input = query(&f.name);
    input.k = 2;
    let result = transport::test_search(f.service.clone(), authorized(input, ADMIN))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        result.hits.iter().map(|hit| hit.id).collect::<Vec<_>>(),
        vec![1, 2]
    );
    assert_eq!(result.hits[0].distance, 0.0);
    assert_eq!(result.hits[1].distance, 2.0);
    for hit in result.hits {
        assert_eq!(hit.rerank_score.unwrap().to_bits(), 0.0_f64.to_bits());
    }
}

#[tokio::test]
async fn grpc_custom_payload_and_cardinality_fail_closed() {
    for response in [
        r#"{"results":[]}"#,
        r#"{"results":[{"id":99,"cost":0.0}]}"#,
        r#"{"results":[{"id":1,"cost":0.0},{"id":1,"cost":0.0}]}"#,
        r#"{"results":[{"id":1,"cost":1e999}]}"#,
    ] {
        let f = fixture("rerank", 200, response, &[]);
        let result = transport::test_search(f.service.clone(), authorized(query(&f.name), ADMIN))
            .await
            .unwrap_err();
        error(&result, Code::Unavailable, "provider_invalid_response");
    }
    let f = fixture(
        "rerank",
        200,
        r#"{"results":[{"id":1,"cost":0.0}]}"#,
        &["public"],
    );
    f.service
        .run({
            let name = f.name.clone();
            move |db| {
                db.collection_mut(&name)?.put(1, &[1., 0.], b"not-json")?;
                Ok(())
            }
        })
        .await
        .unwrap();
    let result = transport::test_search(f.service.clone(), authorized(query(&f.name), ADMIN))
        .await
        .unwrap_err();
    error(&result, Code::InvalidArgument, "invalid_provider_input");
    assert_eq!(f.transport.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn grpc_custom_conversion_and_finite_vector_validation_are_provider_errors() {
    let f = fixture("rerank", 200, r#"{"results":[{"id":1,"cost":0.0}]}"#, &[]);
    for case in 0..5 {
        let mut input = query(&f.name);
        match case {
            0 => input.metric = 999,
            1 => input.vector = vec![f32::INFINITY, 0.0],
            2 => input.vector = vec![1.0],
            3 => input.mode = proto::SearchMode::Hybrid as i32,
            _ => input.mmr_lambda = Some(0.5),
        }
        let result = transport::test_search(f.service.clone(), authorized(input, ADMIN))
            .await
            .unwrap_err();
        error(&result, Code::InvalidArgument, "invalid_provider_input");
    }
    assert_eq!(f.transport.calls.load(Ordering::SeqCst), 0);
}

fn with_timeouts(mut f: Fixture, connect: u64, total: u64) -> Fixture {
    let key = f.service.providers.bindings.keys().next().unwrap();
    let mut value = config("http://127.0.0.1:9", "embedding");
    value["aliases"][0]["connect_timeout_ms"] = json!(connect);
    value["aliases"][0]["total_timeout_ms"] = json!(total);
    value["bindings"][0]["lineage"] = json!(key.0);
    value["bindings"][0]["collection_id"] = json!(key.1);
    value["bindings"][0]["tenant"] = json!(key.2);
    let mut registry = registry(&value);
    registry.transport = f.transport.clone();
    f.service.providers = Arc::new(registry);
    f
}

#[tokio::test]
async fn grpc_original_total_deadline_maps_to_deadline_exceeded() {
    let f = with_timeouts(
        fixture(
            "embedding",
            200,
            r#"{"data":[{"embedding":[1.0,0.0]}]}"#,
            &[],
        ),
        20,
        100,
    );
    *f.transport.gate.lock().unwrap() = Some(Arc::new((
        tokio::sync::Notify::new(),
        tokio::sync::Notify::new(),
    )));
    let result =
        transport::test_embed(f.service.clone(), authorized(embed(&f.name, "test"), ADMIN))
            .await
            .unwrap_err();
    error(&result, Code::DeadlineExceeded, "provider_timeout");
    assert_eq!(f.transport.calls.load(Ordering::SeqCst), 1);
    assert!(result.metadata().get("retry-after").is_none());
}

#[tokio::test]
async fn grpc_rechecks_authorization_after_provider_call() {
    let f = fixture(
        "embedding",
        200,
        r#"{"data":[{"embedding":[1.0,0.0]}]}"#,
        &[],
    );
    let token = credential(&f, TENANT, &["search"]).await;
    let gate = Arc::new((tokio::sync::Notify::new(), tokio::sync::Notify::new()));
    *f.transport.gate.lock().unwrap() = Some(gate.clone());
    let pending = tokio::spawn(transport::test_embed(
        f.service.clone(),
        authorized(embed("items", "test"), &token),
    ));
    tokio::time::timeout(Duration::from_secs(10), gate.0.notified())
        .await
        .unwrap();
    let (status, body, _) = request(
        &f.app,
        &format!("/v1/admin/tenants/{TENANT}/suspend"),
        Some(ADMIN),
        json!({}),
    )
    .await;
    gate.1.notify_one();
    assert_eq!(status, 200, "{body}");
    let result = pending.await.unwrap().unwrap_err();
    error(&result, Code::PermissionDenied, "tenant_suspended");
}

#[tokio::test]
async fn grpc_custom_default_is_fifty_candidates_not_legacy_four_k() {
    let f = fixture("rerank", 200, r#"{"results":[{"id":1,"cost":0.0}]}"#, &[]);
    let name = f.name.clone();
    f.service
        .run(move |db| {
            let mut collection = db.collection_mut(&name)?;
            for id in 3..=60 {
                collection.put(id, &[1., 0.], b"")?;
            }
            Ok(())
        })
        .await
        .unwrap();
    let mut input = query(&f.name);
    input.rerank_candidates = None;
    let result = transport::test_search(f.service.clone(), authorized(input, ADMIN))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(result.hits.len(), 1);
    let bodies = f.transport.bodies.lock().unwrap();
    assert_eq!(bodies[0]["candidates"].as_array().unwrap().len(), 50);
}

#[tokio::test]
async fn grpc_managed_embedding_input_bounds_and_alias_shape_precede_dispatch() {
    let f = fixture(
        "embedding",
        200,
        r#"{"data":[{"embedding":[1.0,0.0]}]}"#,
        &[],
    );
    for alias in ["bad alias".to_owned(), "x".repeat(257)] {
        let result =
            transport::test_embed(f.service.clone(), authorized(embed(&f.name, &alias), ADMIN))
                .await
                .unwrap_err();
        error(&result, Code::InvalidArgument, "invalid_provider_input");
    }
    let mut input = embed(&f.name, "test");
    input.text = "x".repeat(16_385);
    let result = transport::test_embed(f.service.clone(), authorized(input, ADMIN))
        .await
        .unwrap_err();
    error(&result, Code::InvalidArgument, "invalid_provider_input");
    assert_eq!(f.transport.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn grpc_short_client_deadline_cancels_both_managed_operations() {
    for embedding in [true, false] {
        let f = if embedding {
            fixture(
                "embedding",
                200,
                r#"{"data":[{"embedding":[1.0,0.0]}]}"#,
                &[],
            )
        } else {
            fixture("rerank", 200, r#"{"results":[{"id":1,"cost":0.0}]}"#, &[])
        };
        *f.transport.gate.lock().unwrap() = Some(Arc::new((
            tokio::sync::Notify::new(),
            tokio::sync::Notify::new(),
        )));
        let result = if embedding {
            let mut input = authorized(embed(&f.name, "test"), ADMIN);
            input.set_timeout(Duration::from_millis(100));
            tokio::time::timeout(
                Duration::from_secs(2),
                transport::test_embed(f.service.clone(), input),
            )
            .await
            .unwrap()
            .unwrap_err()
        } else {
            let mut input = authorized(query(&f.name), ADMIN);
            input.set_timeout(Duration::from_millis(100));
            tokio::time::timeout(
                Duration::from_secs(2),
                transport::test_search(f.service.clone(), input),
            )
            .await
            .unwrap()
            .unwrap_err()
        };
        error(&result, Code::DeadlineExceeded, "provider_timeout");
        let provider = &f.service.providers.providers["test"];
        assert_eq!(provider.slots.available_permits(), 2);
        assert_eq!(f.transport.calls.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn grpc_expired_client_deadline_prevents_dispatch() {
    let f = fixture(
        "embedding",
        200,
        r#"{"data":[{"embedding":[1.0,0.0]}]}"#,
        &[],
    );
    let mut input = authorized(embed(&f.name, "test"), ADMIN);
    input.set_timeout(Duration::ZERO);
    let result = transport::test_embed(f.service.clone(), input)
        .await
        .unwrap_err();
    error(&result, Code::DeadlineExceeded, "provider_timeout");
    assert_eq!(f.transport.calls.load(Ordering::SeqCst), 0);
}
