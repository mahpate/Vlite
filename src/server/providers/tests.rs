use super::*;
use std::{
    io::{BufRead, BufReader, Write},
    net::TcpListener,
    sync::{
        Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread::JoinHandle,
};

const SECRET: &str = "sentinel-secret-9f31";
const TEXT: &str = "sentinel-private-text-7ab2";
const VECTOR: f32 = 987654.25;

fn config(endpoint: &str, kind: &str) -> Value {
    let url = reqwest::Url::parse(endpoint).unwrap();
    json!({
        "aliases":[{
            "alias":"test", "kind":kind, "endpoint":endpoint,
            "model":"pinned-model", "version":"2026-01",
            "dimensions": if kind == "embedding" { json!(2) } else { Value::Null },
            "connect_timeout_ms":2000, "total_timeout_ms":10000,
            "concurrency":2, "tenant_concurrency":1,
            "replay_safe":false, "tls_required":false, "allow_local_http":true,
            "endpoint_allowlist":{"schemes":["http"],"hosts":[url.host_str().unwrap()],"addresses":["127.0.0.1"]}
        }],
        "bindings":[{"lineage":42,"collection_id":0,"tenant":"tenant-a","alias":"test","payload_fields":[]}]
    })
}

fn registry(value: &Value) -> ProviderRegistry {
    ProviderRegistry::parse(&serde_json::to_vec(value).unwrap()).unwrap()
}

fn key() -> BindingKey {
    (42, 0, "tenant-a".into(), "test".into())
}

struct Fake {
    calls: AtomicUsize,
    status: u16,
    bytes: Vec<u8>,
    error: Option<ProviderError>,
    bodies: Mutex<Vec<Value>>,
}

impl Fake {
    fn new(status: u16, body: &str) -> Self {
        Self {
            calls: AtomicUsize::new(0),
            status,
            bytes: body.as_bytes().to_vec(),
            error: None,
            bodies: Mutex::default(),
        }
    }
}

impl Transport for Fake {
    fn send<'a>(&'a self, request: TransportRequest<'a>) -> TransportFuture<'a> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.bodies
            .lock()
            .unwrap()
            .push(serde_json::from_slice(request.body).unwrap());
        Box::pin(async move {
            if let Some(error) = self.error {
                return Err(TransportFailure {
                    error,
                    retryable: error == ProviderError::Unavailable,
                });
            }
            Ok(TransportResponse {
                status: self.status,
                body: self.bytes.clone(),
                retry_after: None,
            })
        })
    }
}

struct Loopback {
    endpoint: String,
    calls: Arc<AtomicUsize>,
    bodies: Arc<Mutex<Vec<Value>>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Loopback {
    fn new(responses: Vec<(String, Duration)>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let endpoint = format!("http://{}/inference", listener.local_addr().unwrap());
        let calls = Arc::new(AtomicUsize::new(0));
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let (thread_calls, thread_bodies, thread_stop) =
            (calls.clone(), bodies.clone(), stop.clone());
        let thread = std::thread::spawn(move || {
            for (response, delay) in responses {
                let mut stream = loop {
                    if thread_stop.load(Ordering::SeqCst) {
                        return;
                    }
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(1))
                        }
                        Err(e) => panic!("loopback accept: {e}"),
                    }
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                thread_calls.fetch_add(1, Ordering::SeqCst);
                let mut reader = BufReader::new(&mut stream);
                let mut size = 0;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).is_err() {
                        return;
                    }
                    if line == "\r\n" || line.is_empty() {
                        break;
                    }
                    if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        size = value.trim().parse::<usize>().unwrap();
                    }
                }
                let mut body = vec![0; size];
                if reader.read_exact(&mut body).is_err() {
                    return;
                }
                thread_bodies.lock().unwrap().push(if body.is_empty() {
                    Value::Null
                } else {
                    serde_json::from_slice(&body).unwrap()
                });
                std::thread::sleep(delay);
                // Deadline/cap tests intentionally disconnect while the server writes.
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.shutdown(std::net::Shutdown::Write);
                let mut drain = [0; 1024];
                while matches!(stream.read(&mut drain), Ok(n) if n != 0) {}
            }
        });
        Self {
            endpoint,
            calls,
            bodies,
            stop,
            thread: Some(thread),
        }
    }
    fn json(status: u16, body: &str) -> String {
        format!(
            "HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }
    fn one(body: &str) -> Self {
        Self::new(vec![(Self::json(200, body), Duration::ZERO)])
    }
}

impl Drop for Loopback {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.thread.take().unwrap().join().unwrap();
    }
}

#[test]
fn strict_config_roundtrip_and_invalid_shapes() {
    let base = config("http://127.0.0.1:1234/inference", "embedding");
    let parsed: Config = serde_json::from_value(base.clone()).unwrap();
    assert!(ProviderRegistry::parse(&serde_json::to_vec(&parsed).unwrap()).is_ok());
    let mut cases = vec![
        json!({}),
        json!([]),
        json!({"aliases":[]}),
        json!({"aliases":[],"bindings":[],"secret":SECRET}),
    ];
    for field in base["aliases"][0].as_object().unwrap().keys() {
        let mut bad = base.clone();
        bad["aliases"][0].as_object_mut().unwrap().remove(field);
        cases.push(bad);
    }
    for (field, value) in [
        ("unexpected", json!(SECRET)),
        ("alias", json!("")),
        ("kind", json!("unknown")),
        ("dimensions", json!(0)),
        ("dimensions", json!(crate::MAX_DIMENSIONS + 1)),
        ("dimensions", json!("2")),
        ("model", json!("")),
        ("version", json!("latest")),
        ("endpoint", json!("not a URL")),
        (
            "endpoint",
            json!(format!("http://user:{SECRET}@127.0.0.1/")),
        ),
        ("endpoint", json!(format!("http://127.0.0.1/?key={SECRET}"))),
        ("endpoint", json!("http://example.invalid/")),
        ("secret", json!(SECRET)),
        ("secret", json!({"literal":SECRET})),
        (
            "secret",
            json!({"env":"VLITE_TEST_MISSING_SECRET_7AB24F31"}),
        ),
        ("secret", json!({"file":"relative-secret"})),
        ("connect_timeout_ms", json!(0)),
        ("connect_timeout_ms", json!(2001)),
        ("total_timeout_ms", json!(0)),
        ("total_timeout_ms", json!(10001)),
        ("total_timeout_ms", json!(50)),
        ("concurrency", json!(0)),
        ("concurrency", json!(65)),
        ("tenant_concurrency", json!(0)),
        ("tenant_concurrency", json!(3)),
        ("tls_required", json!(true)),
        ("allow_local_http", json!(false)),
        (
            "endpoint_allowlist",
            json!({"schemes":["http"],"hosts":["outside.invalid"],"addresses":["127.0.0.1"]}),
        ),
        (
            "endpoint_allowlist",
            json!({"schemes":["http"],"hosts":["127.0.0.1"],"addresses":["127.0.0.2"]}),
        ),
        (
            "endpoint_allowlist",
            json!({"schemes":["http"],"hosts":["127.0.0.1"],"addresses":["127.0.0.1"],"unknown":true}),
        ),
    ] {
        let mut bad = base.clone();
        bad["aliases"][0][field] = value;
        cases.push(bad);
    }
    let mut duplicate = base.clone();
    duplicate["aliases"]
        .as_array_mut()
        .unwrap()
        .push(base["aliases"][0].clone());
    cases.push(duplicate);
    let mut unknown_binding = base.clone();
    unknown_binding["bindings"][0]["unexpected"] = json!(SECRET);
    cases.push(unknown_binding);
    let mut unknown_alias = base.clone();
    unknown_alias["bindings"][0]["alias"] = json!("absent");
    cases.push(unknown_alias);
    let mut duplicate_binding = base.clone();
    duplicate_binding["bindings"]
        .as_array_mut()
        .unwrap()
        .push(base["bindings"][0].clone());
    cases.push(duplicate_binding);
    let mut invalid_fields = base.clone();
    invalid_fields["bindings"][0]["payload_fields"] = json!(["body"]);
    cases.push(invalid_fields);
    let mut too_many = base.clone();
    too_many["aliases"] = json!(vec![base["aliases"][0].clone(); 65]);
    cases.push(too_many);
    let mut too_many_bindings = base.clone();
    too_many_bindings["bindings"] = json!(vec![base["bindings"][0].clone(); 4097]);
    cases.push(too_many_bindings);
    for bad in cases {
        let error = match ProviderRegistry::parse(&serde_json::to_vec(&bad).unwrap()) {
            Err(error) => error,
            Ok(_) => panic!("accepted invalid shape"),
        };
        assert!(!error.contains(SECRET), "{error}");
        assert!(!error.contains(TEXT), "{error}");
    }
}

#[test]
fn named_environment_secret_is_loaded_without_disclosure() {
    const VARIABLE: &str = "VLITE_PROVIDERS_FAKE_TEST_SECRET";
    if std::env::var_os("VLITE_PROVIDERS_TEST_CHILD").is_some() {
        assert_eq!(
            load_secret(&SecretSource::Env(VARIABLE.into())).unwrap(),
            SECRET
        );
        let mut value = config("http://127.0.0.1:1234/inference", "embedding");
        value["aliases"][0]["secret"] = json!({"env":VARIABLE});
        let registry = registry(&value);
        assert_eq!(registry.providers["test"].secret.as_deref(), Some(SECRET));
        return;
    }
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "server::providers::tests::named_environment_secret_is_loaded_without_disclosure",
            "--nocapture",
        ])
        .env("VLITE_PROVIDERS_TEST_CHILD", "1")
        .env(VARIABLE, SECRET)
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
    assert!(!String::from_utf8_lossy(&output.stdout).contains(SECRET));
    assert!(!String::from_utf8_lossy(&output.stderr).contains(SECRET));
}

#[tokio::test]
async fn every_provoked_error_and_captured_log_is_sanitized() {
    let mut registry = registry(&config("http://127.0.0.1:1234/inference", "embedding"));
    registry.providers.get_mut("test").unwrap().secret = Some(SECRET.into());
    let body = format!(r#"{{"secret":"{SECRET}","text":"{TEXT}","vector":[{VECTOR}]}}"#);
    let fake = Fake::new(200, &body);
    let missing = registry
        .embed(&(99, 0, "tenant-a".into(), "test".into()), TEXT, &fake)
        .await
        .unwrap_err();
    let invalid = registry
        .embed(&key(), &format!("{TEXT}{}", "x".repeat(16384)), &fake)
        .await
        .unwrap_err();
    let response = registry.embed(&key(), TEXT, &fake).await.unwrap_err();
    let unavailable = registry
        .embed(&key(), TEXT, &Fake::new(401, &body))
        .await
        .unwrap_err();
    let mut timeout = Fake::new(200, &body);
    timeout.error = Some(ProviderError::Timeout);
    let timeout = registry.embed(&key(), TEXT, &timeout).await.unwrap_err();
    let permit = registry.providers["test"]
        .slots
        .acquire_many(2)
        .await
        .unwrap();
    let busy = registry.embed(&key(), TEXT, &fake).await.unwrap_err();
    drop(permit);
    assert_eq!(
        [missing, invalid, response, unavailable, timeout, busy],
        [
            ProviderError::NotConfigured,
            ProviderError::InvalidInput,
            ProviderError::InvalidResponse,
            ProviderError::Unavailable,
            ProviderError::Timeout,
            ProviderError::Busy,
        ]
    );
    let captured = serde_json::to_string(&super::super::logs::buffer().snapshot().0).unwrap();
    for error in [missing, invalid, response, unavailable, timeout, busy] {
        for sentinel in [SECRET.to_owned(), TEXT.to_owned(), VECTOR.to_string()] {
            assert!(!error.to_string().contains(&sentinel));
            assert!(!captured.contains(&sentinel));
        }
    }
}

#[tokio::test]
async fn encoded_request_exact_byte_boundary_and_dimension_boundaries() {
    let registry = registry(&config("http://127.0.0.1:1234/inference", "embedding"));
    let (provider, bound) = registry.bound(&key(), Kind::Embedding).unwrap();
    let fake = Fake::new(200, "{}");
    let overhead = serde_json::to_vec(&json!({"pad":""})).unwrap().len();
    for (bytes, valid) in [(REQUEST_BYTES, true), (REQUEST_BYTES + 1, false)] {
        let body = json!({"pad":"x".repeat(bytes-overhead)});
        assert_eq!(serde_json::to_vec(&body).unwrap().len(), bytes);
        let result = call(provider, bound, body, Instant::now(), &fake).await;
        if valid {
            assert!(result.is_ok());
        } else {
            assert_eq!(result.unwrap_err(), ProviderError::InvalidInput);
        }
    }
    assert_eq!(fake.calls.load(Ordering::SeqCst), 1);
    for dimension in [1, crate::MAX_DIMENSIONS] {
        let mut value = config("http://127.0.0.1:1234/inference", "embedding");
        value["aliases"][0]["dimensions"] = json!(dimension);
        let fake = Fake::new(
            200,
            &json!({"data":[{"embedding":vec![1.0;dimension]}]}).to_string(),
        );
        assert_eq!(
            super::ProviderRegistry::parse(&serde_json::to_vec(&value).unwrap())
                .unwrap()
                .embed(&key(), TEXT, &fake)
                .await
                .unwrap()
                .len(),
            dimension
        );
    }
}
#[test]
fn private_secret_files_fail_closed() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("private");
    std::fs::write(&path, SECRET).unwrap();
    let absolute = std::fs::canonicalize(&path).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(load_secret(&SecretSource::File(absolute.clone())).is_err());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(
            load_secret(&SecretSource::File(absolute.clone())).unwrap(),
            SECRET
        );
        std::fs::write(&path, format!("{SECRET}{}", "\n".repeat(4097))).unwrap();
        assert!(load_secret(&SecretSource::File(absolute.clone())).is_err());
        std::fs::write(&path, SECRET).unwrap();
        let link = directory.path().join("link");
        std::os::unix::fs::symlink(&absolute, &link).unwrap();
        assert!(load_secret(&SecretSource::File(link)).is_err());
    }
    #[cfg(not(unix))]
    assert!(
        load_secret(&SecretSource::File(absolute))
            .unwrap_err()
            .contains("unsupported")
    );
}

#[test]
fn tls_and_loopback_policy_are_validated_without_dns() {
    let mut value = config("http://127.0.0.1:1234/inference", "embedding");
    value["aliases"][0]["endpoint"] = json!("http://example.invalid/inference");
    value["aliases"][0]["endpoint_allowlist"]["hosts"] = json!(["example.invalid"]);
    value["aliases"][0]["endpoint_allowlist"]["addresses"] = json!(["192.0.2.1"]);
    assert!(ProviderRegistry::parse(&serde_json::to_vec(&value).unwrap()).is_err());
    value["aliases"][0]["endpoint"] = json!("https://example.invalid/inference");
    value["aliases"][0]["endpoint_allowlist"]["schemes"] = json!(["https"]);
    assert!(ProviderRegistry::parse(&serde_json::to_vec(&value).unwrap()).is_err());
    value["aliases"][0]["tls_required"] = json!(true);
    value["aliases"][0]["allow_local_http"] = json!(false);
    assert!(ProviderRegistry::parse(&serde_json::to_vec(&value).unwrap()).is_ok());
    let mut ipv6 = config("http://[::1]:1234/inference", "embedding");
    ipv6["aliases"][0]["endpoint_allowlist"]["addresses"] = json!(["::1"]);
    assert!(ProviderRegistry::parse(&serde_json::to_vec(&ipv6).unwrap()).is_ok());
    ipv6["aliases"][0]["endpoint"] = json!("http://localhost:1234/inference");
    ipv6["aliases"][0]["endpoint_allowlist"]["hosts"] = json!(["localhost"]);
    assert!(ProviderRegistry::parse(&serde_json::to_vec(&ipv6).unwrap()).is_err());
}

#[tokio::test]
async fn custom_candidate_ceiling_is_inclusive() {
    let registry = registry(&config("http://127.0.0.1:1234/inference", "rerank"));
    let fake = Fake::new(200, r#"{"results":[{"id":1,"cost":0.1}]}"#);
    let candidates: Vec<_> = (1..=1000).map(candidate).collect();
    assert_eq!(
        registry
            .rerank(&key(), &[VECTOR, 1.0], &candidates, 1, &fake)
            .await
            .unwrap()[0]
            .id,
        1
    );
    assert_eq!(fake.calls.load(Ordering::SeqCst), 1);
}
#[test]
fn file_read_errors_and_size_are_sanitized() {
    let directory = tempfile::tempdir().unwrap();
    assert!(
        ProviderRegistry::from_file(&directory.path().join(SECRET))
            .err()
            .unwrap()
            .contains("unavailable")
    );
    let path = directory.path().join("config.json");
    std::fs::write(&path, vec![b' '; CONFIG_BYTES + 1]).unwrap();
    assert!(
        ProviderRegistry::from_file(&path)
            .err()
            .unwrap()
            .contains("exceeds")
    );
    std::fs::write(&path, format!("{{\"secret\":\"{SECRET}\"}}")).unwrap();
    assert!(
        !ProviderRegistry::from_file(&path)
            .err()
            .unwrap()
            .contains(SECRET)
    );
}

#[test]
fn error_mapping_is_exact_and_sanitized() {
    for (error, expected) in [
        (
            ProviderError::NotConfigured,
            (409, "provider_not_configured", "FAILED_PRECONDITION"),
        ),
        (
            ProviderError::InvalidInput,
            (422, "invalid_provider_input", "INVALID_ARGUMENT"),
        ),
        (
            ProviderError::InvalidResponse,
            (502, "provider_invalid_response", "UNAVAILABLE"),
        ),
        (
            ProviderError::Unavailable,
            (503, "provider_unavailable", "UNAVAILABLE"),
        ),
        (
            ProviderError::Timeout,
            (504, "provider_timeout", "DEADLINE_EXCEEDED"),
        ),
        (
            ProviderError::Busy,
            (429, "provider_busy", "RESOURCE_EXHAUSTED"),
        ),
    ] {
        assert_eq!(error.mapping(), expected);
        for sentinel in [SECRET.to_owned(), TEXT.to_owned(), VECTOR.to_string()] {
            assert!(!error.to_string().contains(&sentinel));
            assert!(!format!("{error:?}").contains(&sentinel));
        }
    }
}

#[tokio::test]
async fn zero_outbound_without_config_or_bindings_including_existing_handlers() {
    use axum::{body::Body, http::Request};
    use tower::ServiceExt;
    for value in [json!({"aliases":[],"bindings":[]}), {
        let mut value = config("http://127.0.0.1:1234/inference", "embedding");
        value["bindings"] = json!([]);
        value
    }] {
        let fake = Arc::new(Fake::new(200, r#"{"data":[{"embedding":[1,2]}]}"#));
        let mut registry = registry(&value);
        registry.transport = fake.clone();
        assert_eq!(
            registry.embed_bound(&key(), TEXT).await.unwrap_err(),
            ProviderError::NotConfigured
        );
        let db = crate::Database::create_in_memory(2, Default::default()).unwrap();
        let service = super::super::Service::new(db).with_providers(registry);
        assert!(service.providers().bindings.is_empty());
        let app = super::super::router(service);
        for (route, body) in [
            ("/v1/search", json!({"vector":[1,0],"k":1})),
            ("/v1/collections/default/embed", json!({"text":TEXT})),
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::post(route)
                        .header("content-type", "application/json")
                        .body(Body::from(body.to_string()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert!(response.status().is_success(), "{}", response.status());
        }
        assert_eq!(fake.calls.load(Ordering::SeqCst), 0);
    }
    assert!(ProviderRegistry::default().providers.is_empty());
}

#[tokio::test]
async fn loopback_embedding_success_and_bad_response_shapes() {
    for (body, expected) in [
        (r#"{"data":[{"embedding":[1,2]}]}"#, None),
        ("malformed", Some(ProviderError::InvalidResponse)),
        (
            r#"{"data":[{"embedding":[NaN,2]}]}"#,
            Some(ProviderError::InvalidResponse),
        ),
        (
            r#"{"data":[{"embedding":[Infinity,2]}]}"#,
            Some(ProviderError::InvalidResponse),
        ),
        (
            r#"{"data":[{"embedding":[1e100,2]}]}"#,
            Some(ProviderError::InvalidResponse),
        ),
        (
            r#"{"data":[{"embedding":[1]}]}"#,
            Some(ProviderError::InvalidResponse),
        ),
        (r#"{"data":[]}"#, Some(ProviderError::InvalidResponse)),
        (
            r#"{"data":[{"embedding":[1,2]},{"embedding":[1,2]}]}"#,
            Some(ProviderError::InvalidResponse),
        ),
    ] {
        let server = Loopback::one(body);
        let result = registry(&config(&server.endpoint, "embedding"))
            .embed(&key(), TEXT, &HttpTransport)
            .await;
        if let Some(error) = expected {
            assert_eq!(result.unwrap_err(), error);
        } else {
            assert_eq!(result.unwrap(), vec![1.0, 2.0]);
        }
        assert_eq!(server.calls.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn redirects_are_not_followed() {
    let target = Loopback::one(r#"{"data":[{"embedding":[1,2]}]}"#);
    let response = format!(
        "HTTP/1.1 302 Found\r\nLocation: {}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        target.endpoint
    );
    let source = Loopback::new(vec![(response, Duration::ZERO)]);
    let result = registry(&config(&source.endpoint, "embedding"))
        .embed(&key(), TEXT, &HttpTransport)
        .await;
    assert_eq!(result.unwrap_err(), ProviderError::Unavailable);
    assert_eq!(target.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn response_cap_checks_declared_and_chunked_lengths() {
    let oversize = "x".repeat(RESPONSE_BYTES + 1);
    for response in [
        Loopback::json(200, &oversize),
        format!(
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:x}\r\n{}\r\n0\r\n\r\n",
            oversize.len(),
            oversize
        ),
    ] {
        let server = Loopback::new(vec![(response, Duration::ZERO)]);
        let registry = registry(&config(&server.endpoint, "embedding"));
        let (provider, _) = registry.bound(&key(), Kind::Embedding).unwrap();
        let response = HttpTransport
            .send(TransportRequest {
                provider,
                body: b"{}",
                remaining: Duration::from_secs(10),
            })
            .await;
        assert!(matches!(
            response,
            Err(TransportFailure {
                error: ProviderError::InvalidResponse,
                ..
            })
        ));
    }
}

#[tokio::test]
async fn total_deadline_includes_slow_response_and_retry_after() {
    let server = Loopback::new(vec![(
        Loopback::json(200, r#"{"data":[{"embedding":[1,2]}]}"#),
        Duration::from_millis(500),
    )]);
    let mut value = config(&server.endpoint, "embedding");
    value["aliases"][0]["connect_timeout_ms"] = json!(100);
    value["aliases"][0]["total_timeout_ms"] = json!(200);
    let started = Instant::now();
    assert_eq!(
        registry(&value)
            .embed(&key(), TEXT, &HttpTransport)
            .await
            .unwrap_err(),
        ProviderError::Timeout
    );
    assert!(started.elapsed() < Duration::from_millis(400));
    let server = Loopback::new(vec![(
        "HTTP/1.1 503 Busy\r\nRetry-After: 20\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            .into(),
        Duration::ZERO,
    )]);
    value["aliases"][0]["endpoint"] = json!(server.endpoint);
    value["aliases"][0]["replay_safe"] = json!(true);
    value["aliases"][0]["connect_timeout_ms"] = json!(2000);
    value["aliases"][0]["total_timeout_ms"] = json!(10000);
    assert_eq!(
        registry(&value)
            .embed(&key(), TEXT, &HttpTransport)
            .await
            .unwrap_err(),
        ProviderError::Timeout
    );
    assert_eq!(server.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn replay_safe_retry_is_at_most_once_and_only_for_transients() {
    let server = Loopback::new(vec![
        (Loopback::json(503, SECRET), Duration::ZERO),
        (
            Loopback::json(200, r#"{"data":[{"embedding":[1,2]}]}"#),
            Duration::ZERO,
        ),
    ]);
    let mut value = config(&server.endpoint, "embedding");
    value["aliases"][0]["replay_safe"] = json!(true);
    assert_eq!(
        registry(&value)
            .embed(&key(), TEXT, &HttpTransport)
            .await
            .unwrap(),
        vec![1.0, 2.0]
    );
    assert_eq!(server.calls.load(Ordering::SeqCst), 2);
    {
        let bodies = server.bodies.lock().unwrap();
        assert_eq!(bodies[0], bodies[1]);
        assert_eq!(bodies[0]["version"], "2026-01");
    }
    for safe in [false, true] {
        for status in [302, 401, 403, 429, 500, 503] {
            let responses = vec![(Loopback::json(status, SECRET), Duration::ZERO); 2];
            let server = Loopback::new(responses);
            let mut value = config(&server.endpoint, "embedding");
            value["aliases"][0]["replay_safe"] = json!(safe);
            let error = registry(&value)
                .embed(&key(), TEXT, &HttpTransport)
                .await
                .unwrap_err();
            assert_eq!(
                error,
                ProviderError::Unavailable,
                "replay_safe={safe}, HTTP {status}"
            );
            assert!(!error.to_string().contains(SECRET));
            assert_eq!(
                server.calls.load(Ordering::SeqCst),
                if safe && matches!(status, 429 | 503) {
                    2
                } else {
                    1
                }
            );
        }
        for error in [
            ProviderError::Unavailable,
            ProviderError::InvalidResponse,
            ProviderError::Timeout,
        ] {
            let mut value = config("http://127.0.0.1:1234/inference", "embedding");
            value["aliases"][0]["replay_safe"] = json!(safe);
            let mut fake = Fake::new(200, "");
            fake.error = Some(error);
            assert_eq!(
                registry(&value)
                    .embed(&key(), TEXT, &fake)
                    .await
                    .unwrap_err(),
                error
            );
            assert_eq!(
                fake.calls.load(Ordering::SeqCst),
                if safe && error == ProviderError::Unavailable {
                    2
                } else {
                    1
                }
            );
        }
    }
}

fn candidate(id: u64) -> Candidate {
    Candidate {
        id,
        vector: vec![VECTOR, 1.0],
        payload: BTreeMap::from([
            ("allowed".into(), json!("approved")),
            ("private".into(), json!(SECRET)),
        ]),
    }
}

#[tokio::test]
async fn rerank_validates_ids_costs_cardinality_and_payload_allowlist() {
    for (body, valid) in [
        (
            r#"{"results":[{"id":2,"cost":0.1},{"id":1,"cost":0.1}]}"#,
            true,
        ),
        (
            r#"{"results":[{"id":1,"cost":NaN},{"id":2,"cost":0.2}]}"#,
            false,
        ),
        (
            r#"{"results":[{"id":1,"cost":Infinity},{"id":2,"cost":0.2}]}"#,
            false,
        ),
        (
            r#"{"results":[{"id":1,"cost":1e100},{"id":2,"cost":0.2}]}"#,
            false,
        ),
        (
            r#"{"results":[{"id":1,"cost":0.1},{"id":1,"cost":0.2}]}"#,
            false,
        ),
        (
            r#"{"results":[{"id":999,"cost":0.1},{"id":2,"cost":0.2}]}"#,
            false,
        ),
        (r#"{"results":[{"id":1,"cost":0.1}]}"#, false),
        (r#"{"results":[]}"#, false),
    ] {
        let server = Loopback::one(body);
        let mut value = config(&server.endpoint, "rerank");
        value["bindings"][0]["payload_fields"] = json!(["allowed"]);
        let result = registry(&value)
            .rerank(
                &key(),
                &[VECTOR, 1.0],
                &[candidate(1), candidate(2)],
                2,
                &HttpTransport,
            )
            .await;
        if valid {
            assert_eq!(
                result.unwrap().iter().map(|r| r.id).collect::<Vec<_>>(),
                vec![1, 2]
            );
        } else {
            assert_eq!(result.unwrap_err(), ProviderError::InvalidResponse);
        }
        let bodies = server.bodies.lock().unwrap();
        assert_eq!(
            bodies[0]["candidates"][0]["payload"],
            json!({"allowed":"approved"})
        );
        assert!(!bodies[0].to_string().contains(SECRET));
    }
}

#[tokio::test]
async fn request_byte_text_candidate_and_vector_limits_precede_transport() {
    let fake = Fake::new(200, r#"{"data":[{"embedding":[1,2]}]}"#);
    let embedding = registry(&config("http://127.0.0.1:1234/inference", "embedding"));
    assert!(
        embedding
            .embed(&key(), &"a".repeat(16384), &fake)
            .await
            .is_ok()
    );
    assert_eq!(fake.calls.load(Ordering::SeqCst), 1);
    for input in ["a".repeat(16385), "é".repeat(8193)] {
        assert_eq!(
            embedding.embed(&key(), &input, &fake).await.unwrap_err(),
            ProviderError::InvalidInput
        );
    }
    let mut value = config("http://127.0.0.1:1234/inference", "rerank");
    value["bindings"][0]["payload_fields"] = json!(["allowed"]);
    let rerank = registry(&value);
    let fake = Fake::new(200, r#"{"results":[{"id":1,"cost":0.1}]}"#);
    let mut huge = candidate(1);
    huge.payload
        .insert("allowed".into(), json!("x".repeat(REQUEST_BYTES)));
    assert_eq!(
        rerank
            .rerank(&key(), &[VECTOR, 1.0], &[huge], 1, &fake)
            .await
            .unwrap_err(),
        ProviderError::InvalidInput
    );
    for (candidates, k, query) in [
        (vec![candidate(1)], 0, vec![VECTOR, 1.0]),
        (vec![candidate(1)], 2, vec![VECTOR, 1.0]),
        ((0..1001).map(candidate).collect(), 1, vec![VECTOR, 1.0]),
        (vec![candidate(1), candidate(1)], 1, vec![VECTOR, 1.0]),
        (vec![candidate(1)], 1, vec![f32::NAN, 1.0]),
        (vec![candidate(1)], 1, vec![VECTOR]),
    ] {
        assert_eq!(
            rerank
                .rerank(&key(), &query, &candidates, k, &fake)
                .await
                .unwrap_err(),
            ProviderError::InvalidInput
        );
    }
    assert_eq!(fake.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn provider_and_tenant_admission_is_immediate_and_shared_across_collections() {
    let mut value = config("http://127.0.0.1:1234/inference", "embedding");
    let mut second = value["bindings"][0].clone();
    second["collection_id"] = json!(1);
    value["bindings"].as_array_mut().unwrap().push(second);
    let registry = registry(&value);
    let fake = Fake::new(200, r#"{"data":[{"embedding":[1,2]}]}"#);
    let provider = registry.providers.get("test").unwrap();
    let provider_permit = provider.slots.acquire_many(2).await.unwrap();
    let started = Instant::now();
    assert_eq!(
        registry.embed(&key(), TEXT, &fake).await.unwrap_err(),
        ProviderError::Busy
    );
    assert!(started.elapsed() < Duration::from_millis(100));
    drop(provider_permit);
    let tenant_permit = registry
        .bindings
        .get(&key())
        .unwrap()
        .slots
        .acquire()
        .await
        .unwrap();
    assert_eq!(
        registry
            .embed(&(42, 1, "tenant-a".into(), "test".into()), TEXT, &fake)
            .await
            .unwrap_err(),
        ProviderError::Busy
    );
    drop(tenant_permit);
    for wrong in [
        (41, 0, "tenant-a".into(), "test".into()),
        (42, 1, "tenant-b".into(), "test".into()),
        (42, 2, "tenant-a".into(), "test".into()),
    ] {
        assert_eq!(
            registry.embed(&wrong, TEXT, &fake).await.unwrap_err(),
            ProviderError::NotConfigured
        );
    }
    assert_eq!(fake.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn address_allowlist_is_checked_before_connecting() {
    let server = Loopback::one(SECRET);
    let value = config(&server.endpoint, "embedding");
    let mut registry = registry(&value);
    registry
        .providers
        .get_mut("test")
        .unwrap()
        .definition
        .endpoint_allowlist
        .addresses = vec!["127.0.0.2".parse().unwrap()];
    assert_eq!(
        registry
            .embed(&key(), TEXT, &HttpTransport)
            .await
            .unwrap_err(),
        ProviderError::Unavailable
    );
    assert_eq!(server.calls.load(Ordering::SeqCst), 0);
}
