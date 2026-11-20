//! Opt-in, disposable gateway measurement tooling. No serving authentication
//! exception is installed: direct requests use the production signer and grant.
use super::{control_state, gateway, gateway_mode, gateway_trust::read_bounded, *};
use crate::FormatVersion;
use serde::de::DeserializeOwned;
use std::{
    ffi::OsString,
    path::{Path, PathBuf},
};
use tower::ServiceExt;

const MAX_FILE: u64 = 65_536;

fn read<T: DeserializeOwned>(path: &Path) -> Result<T, String> {
    serde_json::from_slice(&read_bounded(path, MAX_FILE, "measurement config")?)
        .map_err(|_| "invalid_measurement_config".into())
}

fn write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path).map_err(|_| "measurement_write_failed")?;
    let result = file.write_all(bytes).and_then(|()| file.sync_all());
    drop(file);
    if result.is_err() {
        return match std::fs::remove_file(path) {
            Ok(()) => Err("measurement_write_failed".into()),
            Err(_) => Err("measurement_write_and_cleanup_failed".into()),
        };
    }
    Ok(())
}

fn write_json(path: &Path, value: &Value) -> Result<(), String> {
    write(path, value.to_string().as_bytes())
}

/// A short-lived direct request credential. Never serialize or log `envelope`.
pub struct PreparedEnvelope {
    pub envelope: String,
    pub generation: u64,
    pub expires_unix_ms: u64,
    pub endpoint: String,
}

/// A fixed-generation, authenticated measurement grant, not an auth bypass.
/// Construct once before a load window; refreshing only signs a new lifetime.
pub struct EnvelopeIssuer {
    signer: principal_envelope::Signer,
    target: gateway::Target,
    grant: Arc<credentials::Grant>,
    generation: u64,
    lifetime: u64,
    max_replicas: u32,
}

impl EnvelopeIssuer {
    pub fn issue(&self) -> Result<PreparedEnvelope, String> {
        let now = admin::now_ms() as u64;
        if self.grant.expires <= now + self.lifetime {
            return Err("measurement_bearer_expiring".into());
        }
        let envelope = gateway::mint(
            &self.signer,
            &self.target,
            &self.grant,
            self.generation,
            now,
            self.lifetime,
        )
        .map_err(|_| "measurement_sign_failed")?;
        Ok(PreparedEnvelope {
            envelope,
            generation: self.generation,
            expires_unix_ms: now + self.lifetime,
            endpoint: self.target.endpoint.clone(),
        })
    }

    pub fn read(gateway_config: &Path, token_file: &Path) -> Result<Self, String> {
        issuer(gateway_config, token_file)
    }

    /// Nonsecret settings needed to interpret the authenticated comparison.
    pub fn policy_evidence(&self) -> Value {
        json!({"envelope_lifetime_ms":self.lifetime,
            "credential_expires_unix_ms":self.grant.expires,
            "credential_requests_per_second":self.grant.requests_per_second,
            "credential_max_vectors":self.grant.max_vectors,
            "credential_max_vectors_per_collection":self.grant.max_vectors_per_collection,
            "credential_all_collections":self.grant.all,
            "gateway_max_replicas":self.max_replicas,
            "tenant_aggregate_quota":"not_exported"})
    }
}

/// Authenticates a real bearer against a freshly read shared snapshot, applies
/// the same route/scoping checks as the gateway, then uses its private signer.
/// Blocking store I/O: invoke outside an async runtime.
pub fn prepare_envelope(
    gateway_config: &Path,
    token_file: &Path,
) -> Result<PreparedEnvelope, String> {
    EnvelopeIssuer::read(gateway_config, token_file)?.issue()
}

fn issuer(gateway_config: &Path, token_file: &Path) -> Result<EnvelopeIssuer, String> {
    let settings = gateway_mode::read_config(gateway_config)
        .map_err(|_| "measurement_gateway_config_invalid")?;
    let token = read_bounded(token_file, 1024, "measurement bearer")
        .map_err(|_| "measurement_bearer_invalid")?;
    let token = std::str::from_utf8(&token)
        .map_err(|_| "measurement_bearer_invalid")?
        .trim();
    let (snapshot, _) = settings
        .file
        .control_state
        .connect()
        .map_err(|_| "measurement_control_read_failed")?
        .read()
        .map_err(|_| "measurement_control_read_failed")?;
    let now = admin::now_ms() as u64;
    let grant = snapshot
        .authenticate(token, now)
        .map_err(|_| "measurement_bearer_refused")?;
    let classification = gateway::classify(&Method::GET, "/v1/collections/probe/vectors/1")
        .map_err(|_| "measurement_route_refused")?;
    let target = gateway::route_target(classification, &grant, &snapshot)
        .map_err(|_| "measurement_route_refused")?;
    if target.fallback.is_some() {
        return Err("measurement_requires_writer_only".into());
    }
    authorize_scoped(
        &grant,
        Some(target.lineage),
        classification.need,
        false,
        false,
    )
    .map_err(|_| "measurement_scope_refused")?;
    // Keep direct envelopes short even when a general gateway allows longer ones.
    let lifetime = settings.file.envelope_lifetime_ms.min(60_000);
    Ok(EnvelopeIssuer {
        signer: settings.signer,
        target,
        grant,
        generation: snapshot.generation(),
        lifetime,
        max_replicas: settings.file.max_replicas,
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Bootstrap {
    output_dir: PathBuf,
    control_state: control_state::Source,
    writer_url: String,
    audience: String,
    #[serde(default = "poll_default")]
    poll_interval_ms: u64,
    #[serde(default = "replicas_default")]
    max_replicas: u32,
    #[serde(default = "revocations_default")]
    revocations: usize,
}
fn poll_default() -> u64 {
    1000
}
fn replicas_default() -> u32 {
    2
}
fn revocations_default() -> usize {
    10
}

async fn call(
    router: &Router,
    token: &str,
    method: Method,
    path: &str,
    body: Value,
) -> Result<Value, String> {
    let request = axum::http::Request::builder()
        .method(method)
        .uri(path)
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .body(axum::body::Body::from(body.to_string()))
        .map_err(|_| "measurement_bootstrap_request_failed")?;
    let response = router
        .clone()
        .oneshot(request)
        .await
        .map_err(|_| "measurement_bootstrap_request_failed")?;
    if !response.status().is_success() {
        return Err("measurement_bootstrap_api_refused".into());
    }
    let bytes = axum::body::to_bytes(response.into_body(), MAX_FILE as usize)
        .await
        .map_err(|_| "measurement_bootstrap_body_failed")?;
    if bytes.is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_slice(&bytes).map_err(|_| "measurement_bootstrap_body_invalid".into())
}

fn bootstrap(config: Bootstrap) -> Result<Value, String> {
    config
        .control_state
        .validate()
        .map_err(|_| "measurement_source_invalid")?;
    let url = reqwest::Url::parse(&config.writer_url).map_err(|_| "measurement_writer_invalid")?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path() != "/"
        || config.audience.is_empty()
        || config.audience.len() > 96
        || config.audience.chars().any(char::is_control)
        || !config.control_state.target_file.is_absolute()
        || config
            .control_state
            .shared_key_file
            .as_ref()
            .is_some_and(|p| !p.is_absolute())
        || !(1..=5000).contains(&config.poll_interval_ms)
        || !(1..=64).contains(&config.max_replicas)
        || !(1..=100).contains(&config.revocations)
        || !config.output_dir.is_absolute()
        || std::fs::symlink_metadata(&config.output_dir).is_ok()
    {
        return Err("measurement_bootstrap_invalid".into());
    }
    let parent = config
        .output_dir
        .parent()
        .ok_or("measurement_output_invalid")?;
    let stage = tempfile::tempdir_in(parent).map_err(|_| "measurement_output_invalid")?;
    let root = &config.output_dir;
    let local = stage.path();
    std::fs::create_dir(local.join("admin")).map_err(|_| "measurement_write_failed")?;
    let db = Database::create_with_format(local.join("database.vlite"), 2, FormatVersion::V4)
        .map_err(|_| "measurement_fixture_failed")?;
    let lineage = db.lineage().ok_or("measurement_fixture_failed")?;
    let mut random = [0u8; 32];
    getrandom::fill(&mut random).map_err(|_| "measurement_random_failed")?;
    let admin_token = hex(&random);
    getrandom::fill(&mut random).map_err(|_| "measurement_random_failed")?;
    write(&local.join("backend.token"), hex(&random).as_bytes())?;
    random.fill(0);
    let service = Service::try_new(db).map_err(|_| "measurement_fixture_failed")?;
    let (service, router) = admin::configured_service(
        service,
        ServerOptions {
            admin_token: Some(
                BearerToken::new(&admin_token).map_err(|_| "measurement_fixture_failed")?,
            ),
            admin_directory: Some(local.join("admin")),
            ..Default::default()
        },
        None,
    )
    .map_err(|_| "measurement_admin_failed")?;
    let runtime = tokio::runtime::Runtime::new().map_err(|_| "measurement_runtime_failed")?;
    let manifest = runtime.block_on(async {
        let mut routes = Vec::new();
        let mut credentials = Vec::new();
        for (index, name) in ["load", "suspension"].iter().enumerate() {
            let tenant = call(&router, &admin_token, Method::POST, "/v1/admin/tenants",
                json!({"name":name})).await?["tenant"]["id"]
                .as_str().ok_or("measurement_tenant_failed")?.to_owned();
            routes.push(json!({"tenant":tenant,"kind":"dedicated","authority_mode":"off",
                "writer":config.writer_url,"audience":config.audience,"lineage":lineage,
                "read_routing":"writer_only"}));
            let count = if index == 0 { config.revocations + 1 } else { 1 };
            for credential in 0..count {
                let issued = call(&router, &admin_token, Method::POST, "/v1/admin/credentials",
                    json!({"name":format!("{name}-{credential}"),"collections":["*"],
                        "operations":["read","write","collections"],"tenant_id":tenant,
                        "expires_unix_ms":admin::now_ms() as u64 + 7_200_000})).await?;
                let token = issued["token"].as_str().ok_or("measurement_issue_failed")?;
                let file = format!("{name}-{credential}.token");
                write(&local.join(&file), token.as_bytes())?;
                if credential == 0 {
                    call(&router, token, Method::POST, "/v1/collections",
                        json!({"name":"probe","dimensions":2})).await?;
                    call(&router, token, Method::PUT, "/v1/collections/probe/vectors/1",
                        json!({"vector":[1.0,0.0]})).await?;
                    let expected = call(&router, token, Method::GET,
                        "/v1/collections/probe/vectors/1", Value::Null).await?;
                    write_json(&local.join(format!("{name}.expected.json")), &expected)?;
                }
                credentials.push(json!({"purpose":if index == 1 {"suspension"} else if credential == 0 {"load"} else {"revocation"},
                    "id":issued["credential"]["id"],"tenant":tenant,"token_file":root.join(file)}));
            }
        }
        service.run(|db| Ok(db.sync()?)).await.map_err(|_| "measurement_fixture_sync_failed")?;
        write_json(&local.join("directory.json"), &Value::Array(routes))?;
        Ok::<_, String>(json!({"schema":"vlite-gateway-measure-bootstrap/1","lineage":lineage,
            "writer_url":config.writer_url,
            "path":"/v1/collections/probe/vectors/1","credentials":credentials}))
    });
    let closed = runtime
        .block_on(service.close())
        .map_err(|_| "measurement_fixture_close_failed");
    drop(router);
    drop(service);
    drop(runtime);
    closed?;
    let manifest = manifest?;
    let args: Vec<OsString> = vec![
        "--kid".into(),
        "measurement".into(),
        "--generation".into(),
        "1".into(),
        "--signing-key-out".into(),
        local.join("signing.key").into_os_string(),
        "--public-bundle-out".into(),
        local.join("public.json").into_os_string(),
    ];
    gateway_mode::gateway_keygen(&args).map_err(|_| "measurement_keygen_failed")?;
    let source = json!({"target_file":config.control_state.target_file,
        "object":config.control_state.object,"shared_key_file":config.control_state.shared_key_file});
    write_json(
        &local.join("gateway.json"),
        &json!({
            "bind":"0.0.0.0:8080","allow_remote":true,"control_state":source,
            "s_ctl_ms":30000,"poll_interval_ms":config.poll_interval_ms,"max_replicas":config.max_replicas,
            "envelope_lifetime_ms":60000,"public_key_bundle_file":root.join("public.json"),
            "signing_key_file":root.join("signing.key"),"active_kid":"measurement",
            "readiness_propagation_ms":100,"drain_timeout_ms":5000,"max_request_bytes":65536,
            "max_in_flight":4096,"upstream_timeout_ms":5000,"max_buckets":1024
        }),
    )?;
    write_json(
        &local.join("trust.json"),
        &json!({
            "audience":config.audience,"control_state":source,
            "public_key_bundle_file":root.join("public.json"),"s_ctl_ms":30000,
            "poll_interval_ms":config.poll_interval_ms,"skew_ms":1000,
            "max_lifetime_ms":60000,"readiness_propagation_ms":100
        }),
    )?;
    write_json(&local.join("manifest.json"), &manifest)?;
    // Rename only a completed bootstrap. TempDir removes all partial secret files.
    std::fs::rename(local, root).map_err(|_| "measurement_output_publish_failed")?;
    Ok(
        json!({"schema":"vlite-gateway-measure-bootstrap/1","qualification":"proposal_evidence_only",
        "output_dir":root,"lineage":lineage,"control_state_published":false,
        "authority_mode":"off","read_routing":"writer_only"}),
    )
}

/// Offline bootstrap and private envelope export. No secret is printed.
pub fn command(args: &[OsString]) -> Result<Value, String> {
    match args {
        [verb, flag, path] if verb == "bootstrap" && flag == "--config" =>
            bootstrap(read(Path::new(path))?),
        [verb, config_flag, config, token_flag, token, out_flag, output]
            if verb == "mint" && config_flag == "--gateway-config"
                && token_flag == "--token-file" && out_flag == "--output" => {
            let prepared = prepare_envelope(Path::new(config), Path::new(token))?;
            write(Path::new(output), prepared.envelope.as_bytes())?;
            Ok(json!({"generation":prepared.generation,"expires_unix_ms":prepared.expires_unix_ms,
                "qualification":"proposal_evidence_only"}))
        }
        _ => Err("invalid_arguments: gateway-measure bootstrap --config <json> | mint --gateway-config <json> --token-file <path> --output <new-private-file>".into()),
    }
}
