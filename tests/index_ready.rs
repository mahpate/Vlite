use vlite::{Database, Error, HnswConfig, MAX_HNSW_BUFFER_IDS};

#[test]
fn ready_search_never_builds_and_requires_explicit_maintenance_after_overflow_or_reopen() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("ready.vlite");
    let mut db = Database::create(&path, 1).unwrap();
    let config = HnswConfig::default();
    assert!(
        db.search_approximate_ready(&[0.0], 10, 100, config)
            .unwrap()
            .is_empty()
    );
    db.put(0, &[0.0], b"").unwrap();
    for _ in 0..2 {
        assert!(matches!(
            db.search_approximate_ready(&[0.0], 1, 100, config),
            Err(Error::IndexNotReady)
        ));
    }
    assert!(
        db.search_approximate_ready(&[0.0], 0, 100, config)
            .unwrap()
            .is_empty()
    );
    db.build_hnsw(config).unwrap();
    let other = HnswConfig {
        seed: 123,
        ..config
    };
    assert!(matches!(
        db.search_approximate_ready(&[0.0], 1, 100, other),
        Err(Error::IndexNotReady)
    ));
    assert_eq!(
        db.search_approximate_ready(&[0.0], 1, 100, config).unwrap(),
        db.search(&[0.0], 1).unwrap()
    );
    for id in 0..MAX_HNSW_BUFFER_IDS as u64 {
        db.put(id, &[id as f32], b"").unwrap();
    }
    assert_eq!(
        db.search_approximate_ready(&[255.0], 1, 100, config)
            .unwrap()[0]
            .id,
        255
    );
    db.put(256, &[256.0], b"").unwrap();
    assert!(matches!(
        db.search_approximate_ready(&[256.0], 1, 100, config),
        Err(Error::IndexNotReady)
    ));
    assert_eq!(db.search(&[256.0], 1).unwrap()[0].id, 256);
    db.close().unwrap();
    let bytes = std::fs::read(&path).unwrap();
    let mut db = Database::open(&path).unwrap();
    db.build_hnsw(config).unwrap();
    db.close().unwrap();
    assert_eq!(bytes, std::fs::read(&path).unwrap());
    let mut db = Database::open(&path).unwrap();
    db.build_hnsw(config).unwrap();
    assert_eq!(
        db.search_approximate_ready(&[256.0], 1, 300, config)
            .unwrap()[0]
            .id,
        256
    );
    db.delete(256).unwrap();
    assert_eq!(
        db.search_approximate_ready(&[256.0], 1, 300, config)
            .unwrap()[0]
            .id,
        255
    );
    db.close().unwrap();
    let db = Database::open(&path).unwrap();
    assert!(matches!(
        db.search_approximate_ready(&[0.0], 1, 100, config),
        Err(Error::IndexNotReady)
    ));
}

#[test]
fn ready_queries_validate_before_checking_readiness_even_for_zero_k() {
    let directory = tempfile::tempdir().unwrap();
    let mut db = Database::create(directory.path().join("validation.vlite"), 1).unwrap();
    let config = HnswConfig::default();
    for populated in [false, true] {
        if populated {
            db.put(1, &[0.0], b"").unwrap();
        }
        assert!(matches!(
            db.search_approximate_ready(&[], 0, 100, config),
            Err(Error::DimensionMismatch { .. })
        ));
        assert!(matches!(
            db.search_approximate_ready(&[f32::NAN], 0, 100, config),
            Err(Error::NonFiniteVector)
        ));
        assert!(matches!(
            db.search_approximate_ready(&[0.0], 0, 0, config),
            Err(Error::InvalidEfSearch)
        ));
        assert!(matches!(
            db.search_approximate_ready(&[0.0], 0, 100, HnswConfig { m: 0, ..config }),
            Err(Error::InvalidHnswConfig)
        ));
    }
}

#[cfg(feature = "server")]
mod rest {
    use super::*;
    use axum::{Router, body::Body, http::Request};
    use http_body_util::BodyExt;
    use serde_json::{Value, json};
    use tower::ServiceExt;
    use vlite::server::{BearerToken, Service, authenticated_router};

    async fn request(
        app: &Router,
        method: &str,
        uri: &str,
        value: Value,
        authorized: bool,
    ) -> (u16, Value) {
        let mut request = Request::builder()
            .method(method)
            .uri(uri)
            .header("content-type", "application/json");
        if authorized {
            request = request.header("authorization", format!("Bearer {}", "a".repeat(32)));
        }
        let response = app
            .clone()
            .oneshot(request.body(Body::from(value.to_string())).unwrap())
            .await
            .unwrap();
        let status = response.status().as_u16();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            if bytes.is_empty() {
                Value::Null
            } else {
                serde_json::from_slice(&bytes).unwrap()
            },
        )
    }

    #[tokio::test]
    async fn authenticated_explicit_build_and_ready_mode_survive_buffering_not_overflow() {
        let directory = tempfile::tempdir().unwrap();
        let service =
            Service::new(Database::create(directory.path().join("rest-ready.vlite"), 1).unwrap());
        let app = authenticated_router(service.clone(), BearerToken::new(&"a".repeat(32)).unwrap());
        let search = json!({"vector":[0.0],"k":1,"mode":"approximate_ready"});
        assert_eq!(
            request(&app, "POST", "/v1/index/hnsw", json!({}), false)
                .await
                .0,
            401
        );
        assert_eq!(
            request(&app, "PUT", "/v1/vectors/0", json!({"vector":[0.0]}), true)
                .await
                .0,
            204
        );
        let (status, error) = request(&app, "POST", "/v1/search", search.clone(), true).await;
        assert_eq!(status, 409);
        assert_eq!(error["error"]["code"], "index_not_ready");
        assert_eq!(
            request(&app, "GET", "/metrics", Value::Null, true).await.1["hnsw_rebuilds_total"],
            0
        );
        let contract = request(&app, "GET", "/openapi.json", Value::Null, true)
            .await
            .1;
        assert!(contract["paths"]["/v1/index/hnsw"]["post"]["responses"]["204"].is_object());
        assert!(contract["paths"]["/v1/search"]["post"]["responses"]["409"].is_object());
        let mut schema = &contract["paths"]["/v1/search"]["post"]["requestBody"]["content"]["application/json"]
            ["schema"];
        if let Some(name) = schema["$ref"]
            .as_str()
            .and_then(|r| r.strip_prefix("#/components/schemas/"))
        {
            schema = &contract["components"]["schemas"][name];
        }
        assert!(
            schema["properties"]["mode"]["enum"]
                .as_array()
                .unwrap()
                .contains(&json!("approximate_ready"))
        );
        for body in [json!({"m":8}), Value::Null, json!([])] {
            assert_eq!(
                request(&app, "POST", "/v1/index/hnsw", body, true).await.0,
                422
            );
        }
        assert_eq!(
            request(&app, "POST", "/v1/index/hnsw", json!({}), true)
                .await
                .0,
            204
        );
        assert_eq!(
            request(&app, "POST", "/v1/search", search.clone(), true)
                .await
                .1["hits"][0]["id"],
            "0"
        );
        let operations: Vec<_> = (0..257)
            .map(|id| json!({"op":"put","id":id.to_string(),"vector":[id]}))
            .collect();
        assert_eq!(
            request(
                &app,
                "POST",
                "/v1/batches",
                json!({"operations":operations}),
                true
            )
            .await
            .0,
            204
        );
        assert_eq!(
            request(&app, "POST", "/v1/search", search.clone(), true)
                .await
                .0,
            409
        );
        let metrics = request(&app, "GET", "/metrics", Value::Null, true).await.1;
        assert_eq!(metrics["hnsw_rebuilds_total"], 1);
        assert_eq!(metrics["hnsw_buffered_ids"], 0);
        assert_eq!(
            request(&app, "GET", "/v1/index/hnsw", Value::Null, true)
                .await
                .1["ready"],
            false
        );
        assert_eq!(
            request(&app, "POST", "/v1/index/hnsw", json!({}), true)
                .await
                .0,
            204
        );
        assert_eq!(
            request(&app, "DELETE", "/v1/vectors/0", Value::Null, true)
                .await
                .0,
            200
        );
        assert_eq!(
            request(&app, "POST", "/v1/search", search.clone(), true)
                .await
                .1["hits"][0]["id"],
            "1"
        );
        for body in [
            json!({"vector":[0.0],"k":1,"mode":"approximate_ready","filter":[{"op":"range","field":"x"}]}),
            json!({"vector":[0.0],"k":1,"mode":"approximate_ready","ef_search":0}),
            json!({"vector":[],"k":0,"mode":"approximate_ready"}),
        ] {
            assert_eq!(request(&app, "POST", "/v1/search", body, true).await.0, 422);
        }
        let metrics = request(&app, "GET", "/metrics", Value::Null, true).await.1;
        assert_eq!(metrics["hnsw_rebuilds_total"], 2);
        assert_eq!(metrics["hnsw_buffered_ids"], 1);
        assert_eq!(
            request(&app, "GET", "/v1/index/hnsw", Value::Null, true)
                .await
                .1["ready"],
            true
        );
        assert!(
            metrics["hnsw_rebuild_and_search_seconds_total"]
                .as_f64()
                .unwrap()
                > 0.0
        );
        service.close().await.unwrap();
        assert_eq!(
            request(&app, "POST", "/v1/index/hnsw", json!({}), true)
                .await
                .0,
            503
        );
    }
}
