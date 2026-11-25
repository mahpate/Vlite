#[path = "ha_gateway_support.rs"]
mod support;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};
use support::*;
use tokio::{sync::Semaphore, task::JoinSet, time::Instant};
use vlite::server::gateway_measure::EnvelopeIssuer;

fn rates() -> Vec<u64> {
    vec![50, 200, 800, 1600]
}
fn duration() -> u64 {
    180
}
fn warmup() -> u64 {
    30
}
fn concurrency() -> u64 {
    256
}
fn timeout() -> u64 {
    5000
}
fn recovery() -> u64 {
    180
}
const MAX_FAILURE_DIAGNOSTIC_GROUPS: usize = 16;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Config {
    gateway_config: PathBuf,
    token_file: PathBuf,
    expected_body_file: PathBuf,
    gateway_url: String,
    direct_url: String,
    #[serde(default = "rates")]
    rates: Vec<u64>,
    #[serde(default = "duration")]
    duration_secs: u64,
    #[serde(default = "warmup")]
    warmup_secs: u64,
    #[serde(default = "recovery")]
    recovery_secs: u64,
    #[serde(default = "concurrency")]
    max_in_flight: u64,
    #[serde(default = "timeout")]
    timeout_ms: u64,
    /// Opaque resource/topology labels only, not hostnames, IDs or credentials.
    requested: BTreeMap<String, String>,
}

impl Config {
    fn validate(&self) -> Result<()> {
        origin(&self.gateway_url)?;
        origin(&self.direct_url)?;
        bounded("duration_secs", self.duration_secs, 1, 180)?;
        bounded("warmup_secs", self.warmup_secs, 0, 30)?;
        bounded("recovery_secs", self.recovery_secs, 0, 180)?;
        bounded("max_in_flight", self.max_in_flight, 1, 4096)?;
        bounded("timeout_ms", self.timeout_ms, 10, 10000)?;
        if self.rates.is_empty()
            || self.rates.len() > 4
            || self.rates.iter().collect::<BTreeSet<_>>().len() != self.rates.len()
            || self.rates.iter().any(|r| !(1..=10_000).contains(r))
            || self.requested.len() > 32
            || self.requested.iter().any(|(k, v)| !label(k) || !label(v))
        {
            return Err("settings_invalid".into());
        }
        let windows = 4 * self.duration_secs + 2 * self.warmup_secs;
        let count = self.rates.iter().sum::<u64>() * windows + self.rates[0] * self.recovery_secs;
        let drains = (6 * self.rates.len() as u64 + 1) * self.timeout_ms;
        if count > 3_000_000
            || (windows * self.rates.len() as u64 + self.recovery_secs) * 1000 + drains > 3_540_000
        {
            return Err("command_budget_exceeded".into());
        }
        Ok(())
    }
}

struct Outcome {
    status: Option<u16>,
    error: Option<&'static str>,
    semantic_match: bool,
    elapsed_ms: f64,
    schedule_delay_ms: f64,
    diagnostic: Option<ResponseDiagnostic>,
}

struct ResponseDiagnostic {
    content_type: &'static str,
    body_bytes: usize,
    body_sha256: Option<String>,
    body_truncated: bool,
}

#[derive(Clone, Eq, Ord, PartialEq, PartialOrd)]
struct FailureDiagnosticKey {
    status: Option<u16>,
    error: Option<&'static str>,
    content_type: Option<&'static str>,
    body_bytes: Option<usize>,
    body_sha256: Option<String>,
    body_truncated: bool,
}

fn content_type_class(headers: &reqwest::header::HeaderMap) -> &'static str {
    let Some(value) = headers
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
    else {
        return "missing_or_invalid";
    };
    let media_type = value.split(';').next().unwrap_or("").trim();
    if media_type.eq_ignore_ascii_case("application/json")
        || media_type.to_ascii_lowercase().ends_with("+json")
    {
        "application_json"
    } else if media_type.eq_ignore_ascii_case("text/plain") {
        "text_plain"
    } else if media_type.eq_ignore_ascii_case("text/html") {
        "text_html"
    } else {
        "other"
    }
}

fn record_failure_diagnostic(
    groups: &mut BTreeMap<FailureDiagnosticKey, u64>,
    unlisted_failures: &mut u64,
    key: FailureDiagnosticKey,
) {
    if let Some(count) = groups.get_mut(&key) {
        *count += 1;
    } else if groups.len() < MAX_FAILURE_DIAGNOSTIC_GROUPS {
        groups.insert(key, 1);
    } else {
        *unlisted_failures += 1;
    }
}

async fn send(
    client: reqwest::Client,
    url: String,
    header: &'static str,
    secret: String,
    expected: Arc<Value>,
    scheduled: Option<Instant>,
) -> Outcome {
    let start = Instant::now();
    let mut outcome = Outcome {
        status: None,
        error: None,
        semantic_match: false,
        elapsed_ms: 0.,
        schedule_delay_ms: scheduled.map_or(0., |deadline| {
            start.saturating_duration_since(deadline).as_secs_f64() * 1000.
        }),
        diagnostic: None,
    };
    let mut response_content_type = None;
    let mut response_body_bytes = None;
    let mut response_body_truncated = false;
    let mut diagnostic_body = None;
    let result = async {
        let mut response = client
            .get(url)
            .header(header, secret)
            .send()
            .await
            .map_err(|_| "transport_error")?;
        outcome.status = Some(response.status().as_u16());
        response_content_type = Some(content_type_class(response.headers()));
        let mut bytes = Vec::new();
        loop {
            let chunk = match response.chunk().await {
                Ok(Some(chunk)) => chunk,
                Ok(None) => break,
                Err(_) => {
                    response_body_bytes = Some(bytes.len());
                    diagnostic_body = Some(bytes);
                    return Err("body_read_error");
                }
            };
            let remaining = BODY_LIMIT - bytes.len();
            if chunk.len() > remaining {
                bytes.extend_from_slice(&chunk[..remaining]);
                response_body_bytes = Some(BODY_LIMIT + 1);
                response_body_truncated = true;
                diagnostic_body = Some(bytes);
                return Err("body_too_large");
            }
            bytes.extend_from_slice(&chunk);
        }
        response_body_bytes = Some(bytes.len());
        let body: Value = match serde_json::from_slice(&bytes) {
            Ok(body) => body,
            Err(_) => {
                diagnostic_body = Some(bytes);
                return Err("body_invalid_json");
            }
        };
        outcome.semantic_match = body == *expected;
        if outcome.status != Some(200) || !outcome.semantic_match {
            diagnostic_body = Some(bytes);
        }
        Ok::<_, &'static str>(())
    }
    .await;
    outcome.error = result.err();
    outcome.elapsed_ms = start.elapsed().as_secs_f64() * 1000.;
    if outcome.status != Some(200) || outcome.error.is_some() || !outcome.semantic_match {
        outcome.diagnostic = response_content_type.map(|content_type| ResponseDiagnostic {
            content_type,
            body_bytes: response_body_bytes.unwrap_or(0),
            body_sha256: diagnostic_body
                .as_deref()
                .map(|body| format!("{:x}", Sha256::digest(body))),
            body_truncated: response_body_truncated,
        });
    }
    outcome
}

struct Window<'a> {
    client: &'a reqwest::Client,
    issuer: &'a EnvelopeIssuer,
    bearer: &'a str,
    expected: Arc<Value>,
    config: &'a Config,
}

fn wait_for_request(deadline: Instant, period: Duration) -> bool {
    // The load loop runs on Runtime::block_on's caller, not a runtime worker.
    // Tokio timers round to milliseconds, exceeding the 1600-rps slot (625us).
    // Sleep this dedicated caller instead; HTTP tasks keep running on workers.
    std::thread::sleep(deadline.saturating_duration_since(Instant::now()));
    Instant::now().saturating_duration_since(deadline) < period
}

impl Window<'_> {
    async fn run(&self, path: &str, phase: &str, rate: u64, seconds: u64) -> Result<Value> {
        let mut envelope = self.issuer.issue()?;
        let generation = envelope.generation;
        let mut refresh_at = envelope.expires_unix_ms.saturating_sub(20_000);
        let mut minted = 1u64;
        let url = format!(
            "{}{}",
            if path == "direct" {
                &self.config.direct_url
            } else {
                &self.config.gateway_url
            }
            .trim_end_matches('/'),
            PATH
        );
        let in_flight = Arc::new(Semaphore::new(self.config.max_in_flight as usize));
        let mut jobs = JoinSet::new();
        let mut outcomes = Vec::new();
        let mut unsent_capacity = 0u64;
        let mut unsent_late = 0u64;
        let mut lateness = Vec::new();
        let start = Instant::now();
        let scheduled = rate * seconds;
        let period = Duration::from_nanos(1_000_000_000 / rate);
        for ordinal in 0..scheduled {
            while let Some(result) = jobs.try_join_next() {
                outcomes.push(result.map_err(|_| "load_task_failed")?);
            }
            let deadline = start + Duration::from_nanos(ordinal * 1_000_000_000 / rate);
            if !wait_for_request(deadline, period) {
                unsent_late += 1;
                continue;
            }
            let Ok(permit) = in_flight.clone().try_acquire_owned() else {
                unsent_capacity += 1;
                continue;
            };
            // Signing is outside every timed HTTP request. A is fixed for the
            // entire run; no control mutation or automatic request retry occurs.
            if now_ms() >= refresh_at {
                envelope = self.issuer.issue()?;
                refresh_at = envelope.expires_unix_ms.saturating_sub(20_000);
                minted += 1;
            }
            let (header, secret) = if path == "direct" {
                ("x-vlite-principal", envelope.envelope.clone())
            } else {
                ("authorization", format!("Bearer {}", self.bearer))
            };
            let client = self.client.clone();
            let url = url.clone();
            let expected = self.expected.clone();
            jobs.spawn(async move {
                let outcome = send(client, url, header, secret, expected, Some(deadline)).await;
                drop(permit);
                outcome
            });
        }
        tokio::time::sleep_until(start + Duration::from_secs(seconds)).await;
        while let Some(result) = jobs.join_next().await {
            outcomes.push(result.map_err(|_| "load_task_failed")?);
        }
        let mut statuses = BTreeMap::<u16, u64>::new();
        let mut errors = BTreeMap::<&str, u64>::new();
        let mut failure_diagnostics = BTreeMap::new();
        let mut unlisted_failure_diagnostics = 0u64;
        let mut successful = Vec::new();
        let mut mismatches = 0u64;
        for outcome in &outcomes {
            lateness.push(outcome.schedule_delay_ms);
            if let Some(status) = outcome.status {
                *statuses.entry(status).or_default() += 1;
            }
            if let Some(error) = outcome.error {
                *errors.entry(error).or_default() += 1;
            }
            if outcome.status != Some(200) || outcome.error.is_some() || !outcome.semantic_match {
                let diagnostic = outcome.diagnostic.as_ref();
                record_failure_diagnostic(
                    &mut failure_diagnostics,
                    &mut unlisted_failure_diagnostics,
                    FailureDiagnosticKey {
                        status: outcome.status,
                        error: outcome.error,
                        content_type: diagnostic.map(|detail| detail.content_type),
                        body_bytes: diagnostic.map(|detail| detail.body_bytes),
                        body_sha256: diagnostic.and_then(|detail| detail.body_sha256.clone()),
                        body_truncated: diagnostic.is_some_and(|detail| detail.body_truncated),
                    },
                );
            }
            if outcome.error.is_none() && !outcome.semantic_match {
                mismatches += 1;
            }
            if outcome.status == Some(200) && outcome.error.is_none() && outcome.semantic_match {
                successful.push(outcome.elapsed_ms);
            }
        }
        let failure_diagnostics = failure_diagnostics
            .into_iter()
            .map(|(key, count)| {
                json!({"status":key.status,"error":key.error,"content_type":key.content_type,
                    "body_bytes":key.body_bytes,"body_sha256":key.body_sha256,
                    "body_truncated":key.body_truncated,"count":count})
            })
            .collect::<Vec<_>>();
        let elapsed = start.elapsed().as_secs_f64();
        Ok(
            json!({"path":path,"phase":phase,"offered_rate":rate,"duration_secs":seconds,
            "elapsed_with_drain_secs":elapsed,"scheduled":scheduled,"started":outcomes.len(),
            "completed":outcomes.len(),"unsent_capacity":unsent_capacity,"unsent_late":unsent_late,
            "statuses":statuses,"errors":errors,"semantic_mismatches":mismatches,
            "failure_diagnostics":{"max_groups":MAX_FAILURE_DIAGNOSTIC_GROUPS,
                "groups":failure_diagnostics,"unlisted_failures":unlisted_failure_diagnostics},
            "successful":successful.len(),"started_per_sec":outcomes.len() as f64 / seconds.max(1) as f64,
            "successful_per_sec":successful.len() as f64 / seconds.max(1) as f64,
            "successful_latency_ms":summary(&successful),"schedule_lateness_ms":summary(&lateness),
            "envelopes_minted_outside_http_timer":minted,"generation":generation}),
        )
    }
}

fn run() -> Result<Value> {
    let config: Config = read(&config_arg()?)?;
    config.validate()?;
    let issuer = EnvelopeIssuer::read(&config.gateway_config, &config.token_file)?;
    let prepared = issuer.issue()?;
    if origin(&prepared.endpoint)? != origin(&config.direct_url)? {
        return Err("direct_origin_differs_from_authenticated_route".into());
    }
    let bearer = token(&config.token_file)?;
    let expected: Arc<Value> = Arc::new(read(&config.expected_body_file)?);
    let runtime = tokio::runtime::Runtime::new().map_err(|_| "runtime_failed")?;
    runtime.block_on(async {
        let build_client = || reqwest::Client::builder().no_proxy()
            .redirect(reqwest::redirect::Policy::none()).retry(reqwest::retry::never())
            .http1_only().pool_max_idle_per_host(config.max_in_flight as usize)
            .timeout(Duration::from_millis(config.timeout_ms))
            .connect_timeout(Duration::from_millis(config.timeout_ms))
            .build().map_err(|_| "http_client_failed".to_owned());
        let direct = build_client()?;
        let gateway = build_client()?;
        // A bad fixture/auth configuration is not an overhead measurement.
        for (client, path, header, secret) in [
            (&direct, &config.direct_url, "x-vlite-principal", prepared.envelope),
            (&gateway, &config.gateway_url, "authorization", format!("Bearer {bearer}")),
        ] {
            let outcome = send(client.clone(), format!("{}{PATH}", path.trim_end_matches('/')),
                header, secret, expected.clone(), None).await;
            if outcome.status != Some(200) || outcome.error.is_some() || !outcome.semantic_match {
                return Err("preflight_auth_or_fixture_failed".into());
            }
        }
        let mut windows = Vec::new();
        for &rate in &config.rates {
            for (index, path) in ["direct","gateway","gateway","direct"].into_iter().enumerate() {
                let window = Window { client:if path == "direct" { &direct } else { &gateway },
                    issuer:&issuer, bearer:&bearer, expected:expected.clone(), config:&config };
                if config.warmup_secs > 0 && index < 2 {
                    windows.push(window.run(path, "warmup", rate, config.warmup_secs).await?);
                }
                windows.push(window.run(path, "measure", rate, config.duration_secs).await?);
            }
        }
        if config.recovery_secs > 0 {
            windows.push(Window { client:&gateway, issuer:&issuer, bearer:&bearer,
                expected:expected.clone(), config:&config }.run("gateway", "recovery", config.rates[0], config.recovery_secs).await?);
        }
        let mut deltas = Vec::new();
        for &rate in &config.rates {
            let select = |path| windows.iter().filter(|w| w["phase"] == "measure" && w["path"] == path && w["offered_rate"] == rate).collect::<Vec<_>>();
            let a = select("direct");
            let b = select("gateway");
            let pair_delta = |pair: usize, field: &str| -> Option<f64> {
                Some(b[pair]["successful_latency_ms"][field].as_f64()?
                    - a[pair]["successful_latency_ms"][field].as_f64()?)
            };
            let mean_pairs = (0..2).filter_map(|pair| pair_delta(pair,"mean")).collect::<Vec<_>>();
            let median_pairs = (0..2).filter_map(|pair| pair_delta(pair,"p50")).collect::<Vec<_>>();
            let delta = |field: &str| -> Option<f64> {
                let average = |items: &[&Value]| -> Option<f64> {
                    let values = items.iter().map(|w| w["successful_latency_ms"][field].as_f64()).collect::<Option<Vec<_>>>()?;
                    Some(values.iter().sum::<f64>() / values.len() as f64)
                };
                Some(average(&b)? - average(&a)?)
            };
            deltas.push(json!({"rate":rate,"difference_of_window_mean_ms":delta("mean"),
                "difference_of_window_p50_ms":delta("p50"),"difference_of_window_p95_ms":delta("p95"),
                "paired_window_mean_deltas_ms":summary(&mean_pairs),
                "paired_window_median_deltas_ms":summary(&median_pairs),
                "not_a_per_request_overhead_distribution":true}));
        }
        Ok(json!({"schema":"vlite-meas004/1","qualification":"proposal_evidence_only",
            "source":provenance(),"requested":{"rates":config.rates,"duration_secs":config.duration_secs,
                "warmup_secs":config.warmup_secs,"recovery_secs":config.recovery_secs,
                "max_in_flight":config.max_in_flight,"timeout_ms":config.timeout_ms,"labels":config.requested},
            "limits":{"requests":3000000,"body_bytes":BODY_LIMIT,"max_in_flight":4096,
                "command_with_drain_secs":3540,"max_output_windows":25,"retries":0},
            "uncertainty":{"pure_cpu_overhead":"not_isolated_extra_network_hop_included",
                "scale_out":"unknown_external_replica_inventory_and_timing_required",
                "replica_seconds":null,"effective_cpu_entitlement":"unknown",
                "control_mutation_during_load":"prohibited_not_independently_observed"},
            "order":"ABBA","request_class":"GET_probe_vector","generation":prepared.generation,
            "authenticated_policy":issuer.policy_evidence(),
            "windows":windows,"deltas":deltas}))
    })
}
fn main() {
    finish(run());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn late_slots_are_not_replayed_as_catch_up_requests() {
        let rate = 1600;
        let start = Instant::now() - Duration::from_secs(2);
        let period = Duration::from_nanos(1_000_000_000 / rate);
        for ordinal in 0..rate {
            let deadline = start + Duration::from_nanos(ordinal * 1_000_000_000 / rate);
            assert!(!wait_for_request(deadline, period));
        }
    }

    #[test]
    fn bounded_pilot_and_proposed_defaults() {
        let mut value = json!({"gateway_config":"g","token_file":"t","expected_body_file":"e",
            "gateway_url":"http://localhost:1","direct_url":"http://localhost:2","requested":{}});
        let config: Config = serde_json::from_value(value.clone()).unwrap();
        config.validate().unwrap();
        value["rates"] = json!([10000, 10000, 10000, 10000]);
        assert!(
            serde_json::from_value::<Config>(value)
                .unwrap()
                .validate()
                .is_err()
        );
    }

    #[tokio::test]
    async fn responses_preserve_status_errors_and_semantic_mismatches() {
        use axum::{Json, Router, http::StatusCode, routing::get};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new()
            .route("/ok", get(|| async { Json(json!({"vector":[1,0]})) }))
            .route("/mismatch", get(|| async { Json(json!({"vector":[0,1]})) }))
            .route("/bad-json", get(|| async { "invalid" }))
            .route("/large", get(|| async { "x".repeat(BODY_LIMIT + 1) }))
            .route(
                "/refusal",
                get(|| async {
                    (
                        StatusCode::TOO_MANY_REQUESTS,
                        Json(json!({"error":"limited"})),
                    )
                }),
            );
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let client = reqwest::Client::builder()
            .no_proxy()
            .retry(reqwest::retry::never())
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap();
        let expected = Arc::new(json!({"vector":[1,0]}));
        for (path, status, error, matches, content_type, truncated) in [
            ("/ok", 200, None, true, "application_json", false),
            ("/mismatch", 200, None, false, "application_json", false),
            (
                "/bad-json",
                200,
                Some("body_invalid_json"),
                false,
                "text_plain",
                false,
            ),
            (
                "/large",
                200,
                Some("body_too_large"),
                false,
                "text_plain",
                true,
            ),
            ("/refusal", 429, None, false, "application_json", false),
        ] {
            let reply = send(
                client.clone(),
                format!("{origin}{path}"),
                "authorization",
                "Bearer private-test-value".into(),
                expected.clone(),
                Some(Instant::now()),
            )
            .await;
            assert_eq!(reply.status, Some(status));
            assert_eq!(reply.error, error);
            assert_eq!(reply.semantic_match, matches);
            assert!(reply.schedule_delay_ms >= 0.);
            if status == 200 && matches {
                assert!(reply.diagnostic.is_none());
            } else {
                let diagnostic = reply.diagnostic.as_ref().unwrap();
                assert_eq!(diagnostic.content_type, content_type);
                assert!(diagnostic.body_bytes > 0);
                assert!(
                    diagnostic
                        .body_sha256
                        .as_ref()
                        .is_some_and(|hash| hash.len() == 64)
                );
                assert_eq!(diagnostic.body_truncated, truncated);
                if truncated {
                    assert_eq!(diagnostic.body_bytes, BODY_LIMIT + 1);
                    assert_eq!(
                        diagnostic.body_sha256.as_deref(),
                        Some(format!("{:x}", Sha256::digest(vec![b'x'; BODY_LIMIT])).as_str())
                    );
                }
            }
        }
        server.abort();
    }

    #[test]
    fn failure_diagnostic_groups_are_bounded_and_count_overflow() {
        let mut groups = BTreeMap::new();
        let mut unlisted = 0;
        let key = |body_sha256: String| FailureDiagnosticKey {
            status: Some(503),
            error: None,
            content_type: Some("application_json"),
            body_bytes: Some(16),
            body_sha256: Some(body_sha256),
            body_truncated: false,
        };
        let repeated = key("same".into());
        record_failure_diagnostic(&mut groups, &mut unlisted, repeated.clone());
        record_failure_diagnostic(&mut groups, &mut unlisted, repeated);
        for index in 0..MAX_FAILURE_DIAGNOSTIC_GROUPS {
            record_failure_diagnostic(&mut groups, &mut unlisted, key(index.to_string()));
        }
        assert_eq!(groups.len(), MAX_FAILURE_DIAGNOSTIC_GROUPS);
        assert_eq!(
            groups.values().sum::<u64>(),
            MAX_FAILURE_DIAGNOSTIC_GROUPS as u64 + 1
        );
        assert_eq!(unlisted, 1);
    }
}
