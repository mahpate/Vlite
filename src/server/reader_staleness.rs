//! REST freshness admission and response metadata for archive-following readers.

use axum::{
    extract::{Request, State},
    http::{HeaderValue, StatusCode},
    middleware::Next,
    response::Response,
};
use serde_json::{Value, json};

use super::{ApiError, ServeMode, Service, serve_mode::unix_ms};

impl Service {
    pub(super) fn reader_stale(&self, now: u64) -> bool {
        self.serve.mode == ServeMode::Reader
            && self.reader.as_ref().is_some_and(|progress| {
                let bound = progress.bound();
                bound > 0 && progress.age_ms(now).is_none_or(|age| age > bound)
            })
    }
}

pub(super) fn stale() -> ApiError {
    ApiError::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "reader_stale",
        "this reader has not confirmed fresh data within its staleness bound; send reads to the writer",
    )
}

pub(super) fn report(service: &Service, hide: bool, now: u64) -> Option<Value> {
    service.reader.as_ref().map(|progress| {
        json!({
            "applied_sequence": if hide { None } else { Some(progress.applied().0) },
            "age_ms": progress.age_ms(now),
            "bound_ms": progress.bound(),
            "stale": service.reader_stale(now),
            "failed": progress.failure().is_some(),
        })
    })
}

/// Marks a response to a request that authenticated, so reader identity
/// and freshness headers never reach anonymous or rejected callers.
#[derive(Clone, Copy)]
pub(super) struct Authenticated;

pub(super) async fn headers(
    State(service): State<Service>,
    request: Request,
    next: Next,
) -> Response {
    let mut response = next.run(request).await;
    if service.serve.mode == ServeMode::Reader
        && response.extensions().get::<Authenticated>().is_some()
        && let Some(progress) = &service.reader
    {
        let values = [
            (
                "x-vlite-lineage",
                service.lineage.map(|lineage| format!("{lineage:016x}")),
            ),
            (
                "x-vlite-binding-generation",
                Some(service.binding_generation().to_string()),
            ),
            (
                "x-vlite-applied-age-ms",
                progress.age_ms(unix_ms()).map(|age| age.to_string()),
            ),
            (
                "x-vlite-applied-sequence",
                (!service.metrics_blocked_by_memory()).then(|| progress.applied().0.to_string()),
            ),
        ];
        for (name, value) in values {
            if let Some(value) = value {
                response.headers_mut().insert(
                    name,
                    HeaderValue::from_str(&value).expect("numeric reader response header"),
                );
            }
        }
    }
    response
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::{
        body::{Body, to_bytes},
        http::{Request, StatusCode},
        response::Response,
    };
    use serde_json::{Value, json};
    use tower::ServiceExt;

    use crate::{
        Database,
        server::{
            BearerToken, ServeMode, Service, authenticated_router, reader_apply::Progress,
            reserved, router,
        },
    };

    fn reader() -> (tempfile::TempDir, Service, Arc<Progress>) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("reader.vlite");
        let mut db = Database::create(&path, 1).unwrap();
        db.put(1, &[1.0], b"").unwrap();
        db.close().unwrap();
        let mut service = Service::new(Database::open_read_only(&path).unwrap())
            .with_serve_mode(ServeMode::Reader)
            .unwrap();
        let progress = Arc::new(Progress::default());
        service.reader = Some(progress.clone());
        (dir, service, progress)
    }

    async fn request(service: &Service, uri: &str) -> Response {
        router(service.clone())
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap()
    }

    async fn body(response: Response) -> Value {
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap()
    }

    #[tokio::test]
    async fn stale_reader_refuses_readiness_and_reads_but_reports_status() {
        let (_dir, service, progress) = reader();
        progress.set_bound(60_000);
        progress.confirm(7, super::unix_ms());
        for uri in ["/ready", "/v1/vectors/1"] {
            assert_eq!(request(&service, uri).await.status(), StatusCode::OK);
        }
        progress.confirm(7, super::unix_ms().saturating_sub(120_000));
        for uri in ["/ready", "/v1/vectors/1"] {
            let response = request(&service, uri).await;
            assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
            assert_eq!(body(response).await["error"]["code"], "reader_stale");
        }
        let response = request(&service, "/v1/admin/serve-status").await;
        assert_eq!(response.status(), StatusCode::OK);
        let status = body(response).await;
        assert_eq!(status["binding_generation"], 0);
        assert_eq!(status["reader"]["applied_sequence"], 7);
        assert!(status["reader"]["age_ms"].as_u64().unwrap() >= 120_000);
        assert_eq!(status["reader"]["bound_ms"], 60_000);
        assert_eq!(status["reader"]["stale"], true);
        assert_eq!(status["reader"]["failed"], false);
        assert!(status["epoch"].is_null());
        progress.fail("position 999 cannot be applied".into());
        let status = body(request(&service, "/v1/admin/serve-status").await).await;
        assert_eq!(status["reader"]["failed"], true);
        assert!(!status.to_string().contains("999"));
        let response = request(&service, "/ready").await;
        assert_eq!(body(response).await["error"]["code"], "reader_apply_failed");
    }

    #[tokio::test]
    async fn unconfirmed_reader_needs_a_bound_and_equality_is_fresh() {
        let (_dir, service, progress) = reader();
        assert!(!service.reader_stale(10_000));
        assert_eq!(request(&service, "/ready").await.status(), StatusCode::OK);
        let response = request(&service, "/ready").await;
        assert!(!response.headers().contains_key("x-vlite-applied-age-ms"));
        progress.set_bound(100);
        assert!(service.reader_stale(10_000));
        for uri in ["/ready", "/v1/vectors/1"] {
            assert_eq!(
                request(&service, uri).await.status(),
                StatusCode::SERVICE_UNAVAILABLE
            );
        }
        progress.confirm(1, 1_000);
        assert!(!service.reader_stale(1_100));
        assert!(service.reader_stale(1_101));
        progress.set_bound(0);
        assert!(!service.reader_stale(u64::MAX));
        assert_eq!(
            request(&service, "/v1/vectors/1").await.status(),
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn reader_headers_cover_success_and_errors_and_hide_memory_positions() {
        let (_dir, mut service, progress) = reader();
        progress.confirm(7, super::unix_ms());
        for uri in ["/v1/vectors/1", "/v1/vectors/99", "/health"] {
            let response = request(&service, uri).await;
            let headers = response.headers();
            assert_eq!(
                headers["x-vlite-lineage"],
                format!("{:016x}", service.lineage.unwrap())
            );
            assert_eq!(headers["x-vlite-binding-generation"], "0");
            assert_eq!(headers["x-vlite-applied-sequence"], "7");
            assert!(
                headers["x-vlite-applied-age-ms"]
                    .to_str()
                    .unwrap()
                    .parse::<u64>()
                    .is_ok()
            );
        }
        let set = crate::memory::binding::parse(
            &serde_json::to_vec(&json!({
                "version": "memory-bindings-v1",
                "bindings": [{
                    "binding": "memory", "tenant": "@root", "generation": 1,
                    "state": "active", "graph": "memory_graph", "receipt_window_ms": 1,
                    "evidence_collection": null, "schema_version": "memory-schema-v1",
                    "ranking_version": "memory-ranking-v1", "digest_version": "memory-request-v1",
                    "grants": []
                }]
            }))
            .unwrap(),
        )
        .unwrap();
        service.memory_bindings = Arc::new(reserved::Served::new(
            Arc::new(reserved::Bindings::new(set)),
            None,
        ));
        let response = request(&service, "/v1/admin/serve-status").await;
        assert!(!response.headers().contains_key("x-vlite-applied-sequence"));
        assert_eq!(
            response.headers()["x-vlite-binding-generation"],
            service.binding_generation().to_string()
        );
        let status = body(response).await;
        assert!(status["reader"]["applied_sequence"].is_null());
        assert!(status["reader"]["age_ms"].is_u64());
        service.lineage = None;
        let response = request(&service, "/health").await;
        assert!(!response.headers().contains_key("x-vlite-lineage"));
    }

    #[tokio::test]
    async fn reader_headers_go_only_to_authenticated_callers() {
        const TOKEN: &str = "reader_token_reader_token_reader_tok_1";
        let (_dir, service, progress) = reader();
        progress.confirm(7, super::unix_ms());
        let app = authenticated_router(service, BearerToken::new(TOKEN).unwrap());
        for (uri, token, status) in [
            ("/health", None, StatusCode::OK),
            ("/v1/vectors/1", None, StatusCode::UNAUTHORIZED),
            ("/v1/vectors/1", Some("wrong"), StatusCode::UNAUTHORIZED),
            ("/health", Some(TOKEN), StatusCode::OK),
            ("/v1/vectors/1", Some(TOKEN), StatusCode::OK),
        ] {
            let mut request = Request::builder().uri(uri);
            if let Some(token) = token {
                request = request.header("authorization", format!("Bearer {token}"));
            }
            let response = app
                .clone()
                .oneshot(request.body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), status, "{uri}");
            let tagged = response
                .headers()
                .keys()
                .any(|key| key.as_str().starts_with("x-vlite-"));
            assert_eq!(tagged, token == Some(TOKEN), "{uri} {token:?}");
        }
    }

    #[tokio::test]
    async fn non_followers_have_no_reader_headers_or_status_and_are_not_stale() {
        for mode in [ServeMode::Standalone, ServeMode::Writer] {
            let service = Service::new(Database::create_in_memory(1, Default::default()).unwrap())
                .with_serve_mode(mode)
                .unwrap();
            assert!(!service.reader_stale(u64::MAX));
            let response = request(&service, "/v1/admin/serve-status").await;
            assert_eq!(response.status(), StatusCode::OK);
            assert!(
                !response
                    .headers()
                    .keys()
                    .any(|key| key.as_str().starts_with("x-vlite-"))
            );
            let status = body(response).await;
            assert_eq!(status["binding_generation"], 0);
            assert!(status["reader"].is_null());
            assert_eq!(request(&service, "/ready").await.status(), StatusCode::OK);
        }
        let (_dir, mut service, _) = reader();
        service.reader = None;
        assert!(!service.reader_stale(u64::MAX));
        let response = request(&service, "/v1/admin/serve-status").await;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            !response
                .headers()
                .keys()
                .any(|key| key.as_str().starts_with("x-vlite-"))
        );
        let status = body(response).await;
        assert_eq!(status["binding_generation"], 0);
        assert!(status["reader"].is_null());
    }
}
