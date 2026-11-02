use super::*;

const EMBEDDING: &str = r#"{"data":[{"embedding":[1,2]}]}"#;

struct Parked {
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

impl Parked {
    fn new() -> Self {
        Self {
            entered: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
        }
    }
}

impl Transport for Parked {
    fn send<'a>(&'a self, _: TransportRequest<'a>) -> TransportFuture<'a> {
        Box::pin(async move {
            self.entered.notify_one();
            self.release.notified().await;
            Ok(TransportResponse {
                status: 200,
                body: EMBEDDING.as_bytes().to_vec(),
                retry_after: None,
            })
        })
    }
}

struct Pending;

impl Transport for Pending {
    fn send<'a>(&'a self, _: TransportRequest<'a>) -> TransportFuture<'a> {
        Box::pin(std::future::pending())
    }
}

struct BlockingReply {
    delay: Duration,
    body: &'static str,
}

impl Transport for BlockingReply {
    fn send<'a>(&'a self, _: TransportRequest<'a>) -> TransportFuture<'a> {
        Box::pin(async move {
            std::thread::sleep(self.delay);
            Ok(TransportResponse {
                status: 200,
                body: self.body.as_bytes().to_vec(),
                retry_after: None,
            })
        })
    }
}

fn timed_registry(total_ms: u64) -> ProviderRegistry {
    let mut value = config("http://127.0.0.1:1234/inference", "embedding");
    value["aliases"][0]["connect_timeout_ms"] = json!(total_ms.min(2000));
    value["aliases"][0]["total_timeout_ms"] = json!(total_ms);
    registry(&value)
}

#[test]
fn exhausted_deadline_is_timeout() {
    let registry = timed_registry(10000);
    assert_eq!(
        check_deadline(
            Instant::now().checked_sub(Duration::from_secs(60)).unwrap(),
            &registry.providers["test"]
        ),
        Err(ProviderError::Timeout)
    );
}

#[tokio::test]
async fn pending_transport_is_cancelled_by_the_total_deadline() {
    let registry = timed_registry(50);
    let binding_key = key();
    let operation = registry.embed(&binding_key, TEXT, &Pending);
    tokio::pin!(operation);
    let mut context = std::task::Context::from_waker(std::task::Waker::noop());
    assert!(operation.as_mut().poll(&mut context).is_pending());
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        matches!(
            operation.as_mut().poll(&mut context),
            std::task::Poll::Ready(Err(ProviderError::Timeout))
        ),
        "pending transport must be ready with Timeout after its total deadline"
    );
}

#[tokio::test]
async fn late_ready_invalid_body_is_timeout_before_parsing() {
    let registry = timed_registry(1);
    let reply = BlockingReply {
        delay: Duration::from_millis(50),
        body: "invalid",
    };
    assert_eq!(
        registry.embed(&key(), TEXT, &reply).await.unwrap_err(),
        ProviderError::Timeout
    );
}

struct RetryClock {
    calls: Mutex<Vec<Instant>>,
    delay: Duration,
}

impl Transport for RetryClock {
    fn send<'a>(&'a self, _: TransportRequest<'a>) -> TransportFuture<'a> {
        Box::pin(async move {
            self.calls.lock().unwrap().push(Instant::now());
            Ok(TransportResponse {
                status: 503,
                body: Vec::new(),
                retry_after: Some(self.delay),
            })
        })
    }
}

#[tokio::test]
async fn retry_delay_must_fit_and_includes_additive_jitter() {
    let mut registry = timed_registry(1005);
    registry
        .providers
        .get_mut("test")
        .unwrap()
        .definition
        .replay_safe = true;
    let retry = RetryClock {
        calls: Mutex::default(),
        delay: Duration::from_secs(1),
    };
    let binding_key = key();
    {
        let operation = registry.embed(&binding_key, TEXT, &retry);
        tokio::pin!(operation);
        let mut context = std::task::Context::from_waker(std::task::Waker::noop());
        assert!(
            matches!(
                operation.as_mut().poll(&mut context),
                std::task::Poll::Ready(Err(ProviderError::Timeout))
            ),
            "retry must reject a delay that does not fit without yielding to a sleep"
        );
    }
    assert_eq!(retry.calls.lock().unwrap().len(), 1);
    registry
        .providers
        .get_mut("test")
        .unwrap()
        .definition
        .total_timeout_ms = 10000;
    retry.calls.lock().unwrap().clear();
    assert_eq!(
        registry.embed(&key(), TEXT, &retry).await.unwrap_err(),
        ProviderError::Unavailable
    );
    let calls = retry.calls.lock().unwrap();
    assert_eq!(calls.len(), 2);
    assert!(calls[1].duration_since(calls[0]) >= Duration::from_millis(1010));
}

#[tokio::test]
async fn inflight_permits_are_held_and_returned_across_collections() {
    for (provider_limit, tenant_limit) in [(1, 1), (2, 1)] {
        let mut value = config("http://127.0.0.1:1234/inference", "embedding");
        value["aliases"][0]["concurrency"] = json!(provider_limit);
        value["aliases"][0]["tenant_concurrency"] = json!(tenant_limit);
        let mut other = value["bindings"][0].clone();
        other["collection_id"] = json!(1);
        let other_tenant = if provider_limit == 1 {
            "tenant-b"
        } else {
            "tenant-a"
        };
        other["tenant"] = json!(other_tenant);
        value["bindings"].as_array_mut().unwrap().push(other);
        let registry = registry(&value);
        let parked = Parked::new();
        let first_key = key();
        let first = registry.embed(&first_key, TEXT, &parked);
        tokio::pin!(first);
        tokio::select! {
            _ = &mut first => panic!("parked call unexpectedly finished"),
            _ = parked.entered.notified() => {}
        }
        let fake = Fake::new(200, EMBEDDING);
        assert_eq!(
            registry
                .embed(&(42, 1, other_tenant.into(), "test".into()), TEXT, &fake)
                .await
                .unwrap_err(),
            ProviderError::Busy
        );
        assert_eq!(fake.calls.load(Ordering::SeqCst), 0);
        parked.release.notify_one();
        assert_eq!(first.await.unwrap(), vec![1.0, 2.0]);
        assert_eq!(
            registry.embed(&key(), TEXT, &fake).await.unwrap(),
            vec![1.0, 2.0]
        );
    }
}

#[tokio::test]
async fn fake_response_caps_and_success_edges_are_strict() {
    let registry = timed_registry(10000);
    for status in [199, 300] {
        let fake = Fake::new(status, EMBEDDING);
        assert_eq!(
            registry.embed(&key(), TEXT, &fake).await.unwrap_err(),
            ProviderError::Unavailable
        );
        assert_eq!(fake.calls.load(Ordering::SeqCst), 1);
    }
    let oversized = format!("{EMBEDDING}{}", " ".repeat(4_194_305 - EMBEDDING.len()));
    assert_eq!(
        registry
            .embed(&key(), TEXT, &Fake::new(200, &oversized))
            .await
            .unwrap_err(),
        ProviderError::InvalidResponse
    );
}

#[tokio::test]
async fn kind_mismatch_precedes_transport() {
    for kind in ["embedding", "rerank"] {
        let registry = registry(&config("http://127.0.0.1:1234/inference", kind));
        let fake = Fake::new(200, EMBEDDING);
        let result = if kind == "rerank" {
            registry.embed(&key(), TEXT, &fake).await.map(|_| ())
        } else {
            registry
                .rerank(&key(), &[1.0, 2.0], &[candidate(1)], 1, &fake)
                .await
                .map(|_| ())
        };
        assert_eq!(result, Err(ProviderError::InvalidInput));
        assert_eq!(fake.calls.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn rerank_request_fields_and_ascending_cost_order_are_exact() {
    let registry = registry(&config("http://127.0.0.1:1234/inference", "rerank"));
    let fake = Fake::new(
        200,
        r#"{"results":[{"id":1,"cost":0.9},{"id":2,"cost":0.1}]}"#,
    );
    let candidates = [candidate(1), candidate(2)];
    let result = registry
        .rerank(&key(), &[1.0, 2.0], &candidates, 2, &fake)
        .await
        .unwrap();
    assert_eq!(
        result.iter().map(|rank| rank.id).collect::<Vec<_>>(),
        vec![2, 1]
    );
    let fake = Fake::new(200, r#"{"results":[{"id":2,"cost":0.1}]}"#);
    registry
        .rerank(&key(), &[1.0, 2.0], &candidates, 1, &fake)
        .await
        .unwrap();
    let bodies = fake.bodies.lock().unwrap();
    assert_eq!(bodies[0]["k"], 1);
    assert_eq!(bodies[0]["model"], "pinned-model");
    assert_eq!(bodies[0]["version"], "2026-01");
    assert_eq!(bodies[0]["query"], json!([1.0, 2.0]));
}

#[tokio::test]
async fn log_fingerprints_errors_duration_and_rerank_batch_are_exact() {
    use sha2::{Digest, Sha256};
    let alias = "survivor-accounting-only";
    let mut value = config("http://127.0.0.1:1234/inference", "rerank");
    value["aliases"][0]["alias"] = json!(alias);
    value["bindings"][0]["alias"] = json!(alias);
    value["bindings"][0]["tenant"] = json!("accounting-tenant-a");
    let mut other = value["bindings"][0].clone();
    other["tenant"] = json!("accounting-tenant-b");
    value["bindings"].as_array_mut().unwrap().push(other);
    let registry = registry(&value);
    for tenant in ["accounting-tenant-a", "accounting-tenant-b"] {
        let key = (42, 0, tenant.into(), alias.into());
        let slow = BlockingReply {
            delay: Duration::from_millis(50),
            body: r#"{"results":[{"id":1,"cost":0.1}]}"#,
        };
        registry
            .rerank(
                &key,
                &[1.0, 2.0],
                &[candidate(1), candidate(2), candidate(3)],
                1,
                &slow,
            )
            .await
            .unwrap();
        assert_eq!(
            registry
                .rerank(&key, &[1.0, 2.0], &[candidate(1)], 1, &Fake::new(401, ""))
                .await
                .unwrap_err(),
            ProviderError::Unavailable
        );
    }
    let entries: Vec<_> = super::super::super::logs::buffer()
        .snapshot()
        .0
        .into_iter()
        .filter(|entry| entry.target == "providers" && entry.fields["alias"] == alias)
        .collect();
    assert_eq!(entries.len(), 4);
    for (index, tenant) in ["accounting-tenant-a", "accounting-tenant-b"]
        .into_iter()
        .enumerate()
    {
        let expected = super::super::super::hex(&Sha256::digest(tenant.as_bytes()));
        for entry in &entries[index * 2..index * 2 + 2] {
            assert_eq!(entry.fields["tenant_fingerprint"], expected);
        }
        assert_eq!(entries[index * 2].fields["batch_count"], 3);
        let duration = entries[index * 2].fields["duration_ms"].as_u64().unwrap();
        assert!((50..5000).contains(&duration), "duration_ms={duration}");
        assert_eq!(
            entries[index * 2 + 1].fields["outcome"],
            "provider_unavailable"
        );
    }
    assert_ne!(
        entries[0].fields["tenant_fingerprint"],
        entries[2].fields["tenant_fingerprint"]
    );
}

#[test]
fn error_display_is_the_public_mapping_code() {
    for error in [
        ProviderError::NotConfigured,
        ProviderError::InvalidInput,
        ProviderError::InvalidResponse,
        ProviderError::Unavailable,
        ProviderError::Timeout,
        ProviderError::Busy,
    ] {
        assert_eq!(error.to_string(), error.mapping().1);
    }
}

#[test]
fn literal_registry_count_boundaries_are_fixed() {
    for count in [63, 64, 65] {
        let mut value = config("http://127.0.0.1:1234/inference", "embedding");
        let template = value["aliases"][0].clone();
        value["aliases"] = Value::Array(
            (0..count)
                .map(|i| {
                    let mut alias = template.clone();
                    alias["alias"] = json!(format!("alias-{i}"));
                    alias
                })
                .collect(),
        );
        value["bindings"] = json!([]);
        if count <= 64 {
            registry(&value);
        } else {
            assert_eq!(failure(&value), ConfigReason::AliasCount);
        }
    }
    for count in [4095, 4096, 4097] {
        let mut value = config("http://127.0.0.1:1234/inference", "embedding");
        let template = value["bindings"][0].clone();
        value["bindings"] = Value::Array(
            (0..count)
                .map(|i| {
                    let mut binding = template.clone();
                    binding["lineage"] = json!(i);
                    binding
                })
                .collect(),
        );
        if count <= 4096 {
            registry(&value);
        } else {
            assert_eq!(failure(&value), ConfigReason::BindingCount);
        }
    }
}

#[test]
fn literal_secret_bytes_charset_trim_and_modes_are_fixed() {
    for length in [4095, 4096] {
        assert_eq!(
            validate_secret_bytes(&vec![b'x'; length]).unwrap().len(),
            length
        );
    }
    for invalid in [
        vec![b'x'; 4097],
        b"abc\x7f".to_vec(),
        "\u{e9}".as_bytes().to_vec(),
        vec![255],
    ] {
        assert_eq!(
            validate_secret_bytes(&invalid),
            Err(SecretReason::InvalidValue)
        );
    }
    assert_eq!(trim_secret_file(b"SECRET \n").unwrap(), b"SECRET ");
    for bytes in [b"SECRET\n\n\n".as_slice(), b"SECRET\r\n\r\n"] {
        assert_eq!(trim_secret_file(bytes).unwrap(), b"SECRET");
    }
    assert!(mode_is_private(0o400));
    for mode in [0o610, 0o601] {
        assert!(!mode_is_private(mode));
    }
    assert_eq!(
        load_secret(&SecretSource::File("relative-secret".into())),
        Err(SecretReason::InvalidPath)
    );
}

#[test]
fn config_file_exact_size_and_errors_are_sanitized() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join(SECRET);
    assert_eq!(
        ProviderRegistry::from_file(&path).err().unwrap(),
        "provider configuration file unavailable"
    );
    let prefix = br#"{"aliases":[],"bindings":[]}"#;
    for size in [1_048_576, 1_048_577] {
        let mut bytes = prefix.to_vec();
        bytes.resize(size, b' ');
        std::fs::write(&path, bytes).unwrap();
        let result = ProviderRegistry::from_file(&path);
        if size == 1_048_576 {
            assert!(result.is_ok());
        } else {
            assert_eq!(
                result.err().unwrap(),
                "provider configuration exceeds 1048576 bytes"
            );
        }
    }
    std::fs::write(&path, br#"{"aliases":[],"bindings":[],"unexpected":true}"#).unwrap();
    assert_eq!(
        ProviderRegistry::from_file(&path).err().unwrap(),
        CONFIG_ERROR
    );
}

#[test]
fn config_names_shapes_and_minimum_timeout_are_strict() {
    for bytes in [
        br#"{"aliases":[],"bindings":[]} x"#.as_slice(),
        br#"{"aliases":[],"bindings":[]} {"aliases":[],"bindings":[]}"#,
        br#"{"aliases":[],"aliases":[],"bindings":[]}"#,
    ] {
        assert_eq!(
            ProviderRegistry::parse_detailed(bytes).err().unwrap().0,
            ConfigReason::JsonShape
        );
    }
    for invalid in ["", "bad name!", &"a".repeat(257)] {
        let mut value = config("http://127.0.0.1:1234/inference", "embedding");
        value["aliases"][0]["version"] = json!(invalid);
        assert_eq!(failure(&value), ConfigReason::Version);
        value["aliases"][0]["version"] = json!("2026.01");
        value["bindings"][0]["tenant"] = json!(invalid);
        assert_eq!(failure(&value), ConfigReason::BindingFields);
    }
    for text in ["/", "\\", ":", "\"", "$", "\u{e9}", "\n", "\0"] {
        assert!(!name(&format!("a{text}b")));
    }
    for text in [".", "a.b", "A-z_0.9"] {
        assert!(name(text));
    }
    let mut value = config("http://127.0.0.1:1234/inference", "embedding");
    value["aliases"][0]["model"] = json!("text.embed-v2");
    value["aliases"][0]["version"] = json!("2026.01.2");
    value["bindings"][0]["tenant"] = json!("t.1");
    value["aliases"][0]["connect_timeout_ms"] = json!(1);
    value["aliases"][0]["total_timeout_ms"] = json!(1);
    registry(&value);
    let mut rerank = config("http://127.0.0.1:1234/inference", "rerank");
    rerank["aliases"][0]["dimensions"] = json!(2);
    assert_eq!(failure(&rerank), ConfigReason::Dimensions);
}

#[tokio::test]
async fn dns_slots_match_concurrency_and_return_after_success_error_and_timeout() {
    let server = Loopback::one(EMBEDDING);
    let socket = reqwest::Url::parse(&server.endpoint)
        .unwrap()
        .socket_addrs(|| None)
        .unwrap()[0];
    let mut registry = hostname_config(
        &format!("http://dns-lifecycle.invalid:{}/inference", socket.port()),
        "dns-lifecycle.invalid",
        socket.ip(),
    );
    assert_eq!(registry.providers["test"].dns_slots.available_permits(), 2);
    registry.providers.get_mut("test").unwrap().resolver =
        Arc::new(QueueResolver::new([vec![socket], vec![]]));
    assert_eq!(
        registry.embed(&key(), TEXT, &HttpTransport).await.unwrap(),
        vec![1.0, 2.0]
    );
    assert_eq!(registry.providers["test"].dns_slots.available_permits(), 2);
    registry.providers.get_mut("test").unwrap().resolver = Arc::new(PortResolver {
        ports: Mutex::default(),
    });
    assert_eq!(
        registry
            .embed(&key(), TEXT, &HttpTransport)
            .await
            .unwrap_err(),
        ProviderError::Unavailable
    );
    assert_eq!(registry.providers["test"].dns_slots.available_permits(), 2);
    let resolver = Arc::new(BlockingResolver {
        state: Arc::new((Mutex::new(false), std::sync::Condvar::new())),
        entered: AtomicBool::new(false),
    });
    let release = BlockingReleaseGuard(resolver.state.clone());
    let provider = registry.providers.get_mut("test").unwrap();
    provider.resolver = resolver;
    provider.definition.connect_timeout_ms = 50;
    provider.definition.total_timeout_ms = 10000;
    let started = Instant::now();
    assert_eq!(
        registry
            .embed(&key(), TEXT, &HttpTransport)
            .await
            .unwrap_err(),
        ProviderError::Timeout
    );
    let elapsed = started.elapsed();
    drop(release);
    let deadline = Instant::now() + Duration::from_secs(5);
    while registry.providers["test"].dns_slots.available_permits() != 2 && Instant::now() < deadline
    {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(registry.providers["test"].dns_slots.available_permits(), 2);
    assert!(
        elapsed < Duration::from_millis(500),
        "DNS used the total instead of connect budget: {elapsed:?}"
    );
}

#[tokio::test]
async fn response_latency_may_exceed_connect_but_not_total_budget() {
    let server = Loopback::new(vec![(
        Loopback::json(200, EMBEDDING),
        Duration::from_millis(500),
    )]);
    let mut value = config(&server.endpoint, "embedding");
    value["aliases"][0]["connect_timeout_ms"] = json!(50);
    value["aliases"][0]["total_timeout_ms"] = json!(10000);
    assert_eq!(
        registry(&value)
            .embed(&key(), TEXT, &HttpTransport)
            .await
            .unwrap(),
        vec![1.0, 2.0]
    );
}

struct PortResolver {
    ports: Mutex<Vec<u16>>,
}

impl Resolver for PortResolver {
    fn resolve(&self, _: &str, port: u16) -> Result<Vec<SocketAddr>, ProviderError> {
        self.ports.lock().unwrap().push(port);
        Err(ProviderError::Unavailable)
    }
}

#[tokio::test]
async fn literal_ips_bypass_dns_and_https_uses_implicit_port_443() {
    let server = Loopback::one(EMBEDDING);
    let mut registry = registry(&config(&server.endpoint, "embedding"));
    let resolver = Arc::new(PortResolver {
        ports: Mutex::default(),
    });
    registry.providers.get_mut("test").unwrap().resolver = resolver.clone();
    assert_eq!(
        registry.embed(&key(), TEXT, &HttpTransport).await.unwrap(),
        vec![1.0, 2.0]
    );
    assert!(resolver.ports.lock().unwrap().is_empty());
    registry.providers.get_mut("test").unwrap().endpoint =
        reqwest::Url::parse("https://implicit-port.invalid/inference").unwrap();
    assert_eq!(
        registry
            .embed(&key(), TEXT, &HttpTransport)
            .await
            .unwrap_err(),
        ProviderError::Unavailable
    );
    assert_eq!(*resolver.ports.lock().unwrap(), vec![443]);
}

#[cfg(unix)]
#[tokio::test]
async fn every_checked_address_is_pinned_for_connection_failover() {
    let server = Loopback::one(EMBEDDING);
    let socket = reqwest::Url::parse(&server.endpoint)
        .unwrap()
        .socket_addrs(|| None)
        .unwrap()[0];
    let mut registry = hostname_config(
        &format!("http://failover.invalid:{}/inference", socket.port()),
        "failover.invalid",
        socket.ip(),
    );
    let provider = registry.providers.get_mut("test").unwrap();
    provider
        .definition
        .endpoint_allowlist
        .addresses
        .push("127.0.0.2".parse().unwrap());
    provider.resolver = Arc::new(QueueResolver::new([vec![
        SocketAddr::new("127.0.0.2".parse().unwrap(), socket.port()),
        socket,
    ]]));
    assert_eq!(
        registry.embed(&key(), TEXT, &HttpTransport).await.unwrap(),
        vec![1.0, 2.0]
    );
    assert_eq!(server.calls.load(Ordering::SeqCst), 1);
}

// This fixture sends headers immediately, then holds the body open until teardown.
struct HeaderHold {
    endpoint: String,
    stop: Arc<AtomicBool>,
    release: Option<std::sync::mpsc::Sender<()>>,
    body: Arc<Mutex<Vec<u8>>>,
    probed: std::sync::mpsc::Receiver<()>,
    thread: Option<JoinHandle<()>>,
}

impl HeaderHold {
    fn new(headers: String) -> Self {
        Self::with_read_probe(headers, false)
    }

    fn with_read_probe(headers: String, probe: bool) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let endpoint = format!("http://{}/inference", listener.local_addr().unwrap());
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let (release, released) = std::sync::mpsc::channel();
        let (probe_complete, probed) = std::sync::mpsc::channel();
        let body = Arc::new(Mutex::new(Vec::new()));
        let request_body = body.clone();
        let thread = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(30);
            while !thread_stop.load(Ordering::SeqCst) && Instant::now() < deadline {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream.set_nonblocking(false).unwrap();
                        if probe {
                            stream
                                .set_read_timeout(Some(Duration::from_millis(100)))
                                .unwrap();
                            let error = stream
                                .peek(&mut [0])
                                .expect_err("probe client sends no data");
                            if cfg!(windows) {
                                assert_eq!(
                                    error.kind(),
                                    std::io::ErrorKind::TimedOut,
                                    "accepted stream must block; WouldBlock means nonblocking was inherited"
                                );
                            } else {
                                assert!(matches!(
                                    error.kind(),
                                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                                ));
                            }
                            probe_complete.send(()).unwrap();
                        }
                        stream
                            .set_read_timeout(Some(Duration::from_secs(30)))
                            .unwrap();
                        stream
                            .set_write_timeout(Some(Duration::from_secs(3)))
                            .unwrap();
                        let mut reader = BufReader::new(&mut stream);
                        let mut length = 0;
                        loop {
                            let mut line = String::new();
                            assert_ne!(
                                reader.read_line(&mut line).unwrap(),
                                0,
                                "client closed before request headers"
                            );
                            if line == "\r\n" {
                                break;
                            }
                            if let Some(size) =
                                line.to_ascii_lowercase().strip_prefix("content-length:")
                            {
                                length = size.trim().parse::<usize>().unwrap();
                            }
                        }
                        let mut bytes = vec![0; length];
                        reader.read_exact(&mut bytes).unwrap();
                        *request_body.lock().unwrap() = bytes;
                        stream.write_all(headers.as_bytes()).unwrap();
                        released
                            .recv_timeout(Duration::from_secs(30))
                            .expect("test did not release held-open response");
                        return;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    Err(error) => panic!("header-hold accept failed: {error}"),
                }
            }
        });
        Self {
            endpoint,
            stop,
            release: Some(release),
            body,
            probed,
            thread: Some(thread),
        }
    }
}

#[test]
fn held_open_fixture_blocks_and_consumes_request_before_responding() {
    let server = HeaderHold::with_read_probe(
        "HTTP/1.1 200 OK\r\nContent-Length: 100\r\nConnection: close\r\n\r\n".into(),
        true,
    );
    let address = reqwest::Url::parse(&server.endpoint)
        .unwrap()
        .socket_addrs(|| None)
        .unwrap()[0];
    let mut client = TcpStream::connect_timeout(&address, Duration::from_secs(2)).unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    client
        .set_write_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    server
        .probed
        .recv_timeout(Duration::from_secs(5))
        .expect("fixture did not complete its blocking-mode probe");
    client
        .write_all(b"POST /inference HTTP/1.1\r\nHost: localhost\r\nContent-Length: 4\r\n\r\n{}\n ")
        .unwrap();
    let mut response = BufReader::new(&client);
    let mut status = String::new();
    response.read_line(&mut status).unwrap();
    assert!(status.starts_with("HTTP/1.1 200"));
    assert_eq!(*server.body.lock().unwrap(), b"{}\n ");
}

impl Drop for HeaderHold {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = self.release.take().unwrap().send(());
        let result = self.thread.take().unwrap().join();
        if !std::thread::panicking() {
            assert!(result.is_ok(), "held-open fixture thread failed");
        }
    }
}

#[tokio::test]
async fn declared_oversize_is_rejected_before_body_and_chunk_errors_are_classified() {
    for (length, expected) in [
        (4_194_305, ProviderError::InvalidResponse),
        (100, ProviderError::Timeout),
    ] {
        let server = HeaderHold::new(format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n"
        ));
        let registry = registry(&config(&server.endpoint, "embedding"));
        let request = TransportRequest {
            provider: &registry.providers["test"],
            body: b"{}",
            remaining: Duration::from_secs(2),
        };
        let error = HttpTransport.send(request).await.err().unwrap();
        assert_eq!(error.error, expected);
    }
    let server = Loopback::new(vec![(
        "HTTP/1.1 200 OK\r\nContent-Length: 100\r\nConnection: close\r\n\r\nshort".into(),
        Duration::ZERO,
    )]);
    let registry = registry(&config(&server.endpoint, "embedding"));
    let error = HttpTransport
        .send(TransportRequest {
            provider: &registry.providers["test"],
            body: b"{}",
            remaining: Duration::from_secs(10),
        })
        .await
        .err()
        .unwrap();
    assert_eq!(error.error, ProviderError::InvalidResponse);
}

#[tokio::test]
async fn replay_safe_does_not_retry_a_failure_after_connect() {
    let server = Loopback::new(vec![
        ("not an HTTP response\r\n\r\n".into(), Duration::ZERO),
        (Loopback::json(200, EMBEDDING), Duration::ZERO),
    ]);
    let mut value = config(&server.endpoint, "embedding");
    value["aliases"][0]["replay_safe"] = json!(true);
    assert_eq!(
        registry(&value)
            .embed(&key(), TEXT, &HttpTransport)
            .await
            .unwrap_err(),
        ProviderError::Unavailable
    );
    assert_eq!(server.calls.load(Ordering::SeqCst), 1);
}
#[test]
fn oversized_vectors_and_ranked_answers_are_rejected() {
    assert_eq!(
        check_embedding(&[1.0, 2.0, 3.0], 2),
        Err(ProviderError::InvalidResponse)
    );
    let results = [Ranked { id: 1, cost: 0.1 }, Ranked { id: 2, cost: 0.2 }];
    assert_eq!(
        check_ranked(&results, 1, &BTreeSet::from([1, 2])),
        Err(ProviderError::InvalidResponse)
    );
}

#[test]
fn endpoint_scheme_host_count_ipv6_and_mixed_http_allowlist_are_rejected() {
    let mut value = config("ftp://127.0.0.1:21/inference", "embedding");
    value["aliases"][0]["endpoint_allowlist"]["schemes"] = json!(["ftp"]);
    assert_eq!(failure(&value), ConfigReason::EndpointAllowlist);
    let mut value = config("https://pinned.invalid/inference", "embedding");
    value["aliases"][0]["tls_required"] = json!(true);
    value["aliases"][0]["endpoint_allowlist"]["schemes"] = json!(["https"]);
    value["aliases"][0]["endpoint_allowlist"]["hosts"] = json!(["pinned.invalid", "other.invalid"]);
    assert_eq!(failure(&value), ConfigReason::EndpointAllowlist);
    value["aliases"][0]["endpoint_allowlist"]["hosts"] = json!(["pinned.invalid"]);
    value["aliases"][0]["endpoint_allowlist"]["addresses"] = json!([]);
    assert_eq!(failure(&value), ConfigReason::EndpointAllowlist);
    let mut value = config("http://[::1]:1234/inference", "embedding");
    assert_eq!(failure(&value), ConfigReason::EndpointAllowlist);
    value["aliases"][0]["endpoint"] = json!("http://127.0.0.1:1234/inference");
    value["aliases"][0]["endpoint_allowlist"]["hosts"] = json!(["127.0.0.1"]);
    value["aliases"][0]["endpoint_allowlist"]["addresses"] = json!(["127.0.0.1", "192.0.2.1"]);
    assert_eq!(failure(&value), ConfigReason::TlsPolicy);
}

#[tokio::test]
async fn tls_connect_timeout_is_not_retryable() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = stop.clone();
    let (release, released) = std::sync::mpsc::channel();
    let thread = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(60);
        while !thread_stop.load(Ordering::SeqCst) && Instant::now() < deadline {
            match listener.accept() {
                Ok((stream, _)) => {
                    stream.set_nonblocking(false).unwrap();
                    released
                        .recv_timeout(Duration::from_secs(60))
                        .expect("TLS stall was not released");
                    return;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(error) => panic!("TLS stall accept: {error}"),
            }
        }
    });
    let result = reqwest::Client::builder()
        .no_proxy()
        .connect_timeout(Duration::from_millis(100))
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap()
        .get(format!("https://{address}/"))
        .send()
        .await;
    stop.store(true, Ordering::SeqCst);
    let _ = release.send(());
    thread.join().unwrap();
    let error = result.expect_err("TLS handshake must time out");
    assert!(error.is_timeout(), "{error:?}");
    assert!(error.is_connect(), "{error:?}");
    let failure = http_error(error);
    assert_eq!(failure.error, ProviderError::Timeout);
    assert!(!failure.retryable, "a connect timeout must not be retried");
}

#[cfg(unix)]
#[test]
fn unix_secret_file_raw_bounds_privacy_and_line_endings() {
    use std::os::unix::fs::PermissionsExt;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("fake-secret");
    for bytes in [b"SECRET\r\n\r\n".to_vec(), vec![b'x'; 4096]] {
        std::fs::write(&path, &bytes).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(
            load_secret(&SecretSource::File(path.clone())).unwrap(),
            String::from_utf8(trim_secret_file(&bytes).unwrap().to_vec()).unwrap()
        );
    }
    std::fs::write(&path, vec![b'x'; 4097]).unwrap();
    assert_eq!(
        load_secret(&SecretSource::File(path.clone())),
        Err(SecretReason::TooLarge)
    );
    for mode in [0o610, 0o601] {
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        assert_eq!(
            load_secret(&SecretSource::File(path.clone())),
            Err(SecretReason::FileNotPrivate)
        );
    }
}
