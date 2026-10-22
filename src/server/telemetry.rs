//! Opt-in OTLP tracing. Only an explicit attribute allowlist is exported.
use opentelemetry::{
    KeyValue,
    trace::{Span, Tracer},
};
use opentelemetry_otlp::WithExportConfig;

/// Configure the OTLP/gRPC exporter; keep the provider alive until shutdown.
/// An absent endpoint leaves tracing disabled, even when compiled in.
pub fn configure(
    endpoint: Option<&str>,
) -> Result<Option<opentelemetry_sdk::trace::SdkTracerProvider>, Box<dyn std::error::Error>> {
    let Some(endpoint) = endpoint else {
        return Ok(None);
    };
    if !endpoint.starts_with("http://") && !endpoint.starts_with("https://") {
        return Err("OTLP endpoint must be an http:// or https:// URL".into());
    }
    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_tonic()
        .with_endpoint(endpoint)
        .build()?;
    let provider = opentelemetry_sdk::trace::SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .build();
    opentelemetry::global::set_tracer_provider(provider.clone());
    Ok(Some(provider))
}

pub(super) fn request(
    request_id: &str,
    route: &str,
    collection: &str,
) -> opentelemetry::global::BoxedSpan {
    let mut span = opentelemetry::global::tracer("vlite-server").start("vlite.request");
    span.set_attributes([
        KeyValue::new("request_id", request_id.to_owned()),
        KeyValue::new("route", route.to_owned()),
        KeyValue::new("collection", collection.to_owned()),
    ]);
    span
}
pub(super) fn finish(mut span: opentelemetry::global::BoxedSpan, status: u16) {
    span.set_attribute(KeyValue::new("http.response.status_code", status as i64));
    span.end();
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry_sdk::trace::{SpanData, SpanExporter};
    use std::sync::{Arc, Mutex};
    #[derive(Debug, Clone)]
    struct Capture(Arc<Mutex<Vec<SpanData>>>);
    impl SpanExporter for Capture {
        async fn export(&self, batch: Vec<SpanData>) -> opentelemetry_sdk::error::OTelSdkResult {
            self.0.lock().unwrap().extend(batch);
            Ok(())
        }
    }
    #[test]
    fn request_spans_export_only_the_attribute_allowlist() {
        let capture = Capture(Arc::default());
        let provider = opentelemetry_sdk::trace::SdkTracerProvider::builder()
            .with_simple_exporter(capture.clone())
            .build();
        opentelemetry::global::set_tracer_provider(provider.clone());
        finish(
            request(
                "security-unit-request",
                "/v1/collections/{collection}/search",
                "docs",
            ),
            200,
        );
        provider.force_flush().unwrap();
        let spans = capture.0.lock().unwrap();
        let span = spans
            .iter()
            .find(|span| {
                span.attributes.iter().any(|a| {
                    a.key.as_str() == "request_id" && a.value.as_str() == "security-unit-request"
                })
            })
            .unwrap();
        let mut keys = span
            .attributes
            .iter()
            .map(|a| a.key.as_str())
            .collect::<Vec<_>>();
        keys.sort();
        assert_eq!(
            keys,
            [
                "collection",
                "http.response.status_code",
                "request_id",
                "route"
            ]
        );
        drop(spans);
        provider.shutdown().unwrap();
    }
    #[test]
    fn endpoint_is_optional_and_invalid_endpoint_is_rejected() {
        assert!(configure(None).unwrap().is_none());
        assert!(configure(Some("file:///secret")).is_err());
    }
}
