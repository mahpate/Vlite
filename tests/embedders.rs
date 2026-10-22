use std::sync::Arc;
use vlite::embedding::{Embedder, EmbedderRegistry, HashEmbedder};
#[test]
fn deterministic_hash_bytes_text_registry_and_validation() {
    let h = HashEmbedder::new(64).unwrap();
    let a = h.embed_text("hello world").unwrap();
    assert_eq!(a, h.embed_bytes(b"hello world").unwrap());
    assert_eq!(a.len(), 64);
    assert!(a.iter().all(|v| v.is_finite()));
    let norm = a.iter().map(|v| f64::from(*v).powi(2)).sum::<f64>();
    assert!((norm - 1.0).abs() < 1e-6);
    assert_ne!(a, h.embed_text("different document").unwrap());
    assert_eq!(h.embed_bytes(b"").unwrap(), vec![0.0; 64]);
    assert!(HashEmbedder::new(0).is_err());
    let mut registry = EmbedderRegistry::default();
    registry.register("hash", Arc::new(h)).unwrap();
    assert_eq!(
        registry
            .get("hash")
            .unwrap()
            .embed_text("hello world")
            .unwrap(),
        a
    );
    assert!(registry.get("missing").is_none());
    assert!(
        registry
            .register("", Arc::new(HashEmbedder::new(2).unwrap()))
            .is_err()
    );
}
#[cfg(feature = "http-embedder")]
#[test]
fn openai_compatible_provider_uses_only_local_server() {
    use std::{
        io::{Read, Write},
        net::TcpListener,
    };
    use vlite::embedding::HttpEmbedder;
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(10)))
            .unwrap();
        let mut request = Vec::new();
        let mut buf = [0; 1024];
        loop {
            let n = stream.read(&mut buf).unwrap();
            assert!(n > 0);
            request.extend_from_slice(&buf[..n]);
            if let Some(end) = request.windows(4).position(|x| x == b"\r\n\r\n") {
                let headers = String::from_utf8_lossy(&request[..end]);
                let len = headers
                    .lines()
                    .find_map(|l| {
                        l.to_lowercase()
                            .strip_prefix("content-length:")
                            .map(|x| x.trim().parse::<usize>().unwrap())
                    })
                    .unwrap();
                if request.len() >= end + 4 + len {
                    break;
                }
            }
        }
        let request = String::from_utf8(request).unwrap();
        assert!(request.starts_with("POST /embeddings HTTP/1.1"));
        assert!(
            request
                .to_lowercase()
                .contains("authorization: bearer test-key")
        );
        assert!(request.contains("test-model"));
        let body = r#"{"data":[{"embedding":[0.25,0.75]}]}"#;
        write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",body.len(),body).unwrap();
    });
    let provider = HttpEmbedder::new(
        &format!("http://{addr}/embeddings"),
        "test-model",
        Some("test-key".into()),
    )
    .unwrap();
    assert_eq!(provider.embed_text("hello").unwrap(), [0.25, 0.75]);
    server.join().unwrap();
    assert!(provider.embed_bytes(&[255]).is_err());
    assert!(HttpEmbedder::new("file:///invalid", "x", None).is_err());
}
#[cfg(feature = "http-embedder")]
#[test]
fn malformed_provider_response_is_rejected() {
    use std::{
        io::{Read, Write},
        net::TcpListener,
    };
    use vlite::embedding::HttpEmbedder;
    for body in [
        r#"{"data":[{"embedding":[]}]}"#,
        r#"{"data":[{"embedding":[1e100]}]}"#,
        "not json",
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut b = [0; 4096];
            let n = stream.read(&mut b).unwrap();
            assert!(n > 0);
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
        });
        assert!(
            HttpEmbedder::new(&format!("http://{addr}/embeddings"), "m", None)
                .unwrap()
                .embed_text("x")
                .is_err()
        );
        server.join().unwrap();
    }
}
