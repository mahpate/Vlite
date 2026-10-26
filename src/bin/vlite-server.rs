use std::{
    error::Error,
    net::SocketAddr,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use serde_json::json;
use tokio::net::TcpListener;
use vlite::{
    AnnSnapshotLoad, Database, FormatVersion,
    server::{
        BearerToken, GroupCommitConfig, ServerOptions, Service, configured_admin_router,
        read_token_file, selected_database,
    },
};

const USAGE: &str = "usage: vlite-server <database-path> [--create-if-missing <dimensions> [--create-format 3|4|5]] [--bind <ip:port>] [--tls-cert <pem> --tls-key <pem>] [--otlp-endpoint <url>] [--grpc-bind <ip:port> [--grpc-tls-cert <pem> --grpc-tls-key <pem> [--grpc-tls-client-ca <pem>]]] [--allow-remote] [--ann-snapshot] [--lock-held-reads] [--group-commit [--group-commit-requests <1..16>] [--group-commit-bytes <1024..268435456>] [--group-commit-delay-ms <0..100>]] [--encryption-key-file <path> | VLITE_ENCRYPTION_KEY | VLITE_ENCRYPTION_KEY_HEX] [--token-file <path> | --insecure-local]; admin: VLITE_AUTH_TOKEN or VLITE_AUTH_TOKEN_FILE; optional read-only: VLITE_READ_ONLY_TOKEN or VLITE_READ_ONLY_TOKEN_FILE; backups/credentials/persistent audit: VLITE_ADMIN_DIR; off-by-default schedules: VLITE_AUTO_MAINTENANCE_INTERVAL_SECS, VLITE_BACKUP_INTERVAL_SECS, VLITE_BACKUP_RETENTION_ENABLED=1, VLITE_BACKUP_RETAIN_LAST, VLITE_BACKUP_RETAIN_DAYS, VLITE_BACKUP_DRILL_EVERY, VLITE_BACKUP_DRILL_MAX_BYTES; VLITE_SLOW_QUERY_MS; dashboard sign-in (needs VLITE_ADMIN_DIR and an admin token): VLITE_TRUSTED_PROXY=1 behind a proxy that sets X-Forwarded-Proto/For, VLITE_SESSION_IDLE_MINUTES (default 30), VLITE_SESSION_MAX_HOURS (default 8), optional first super admin: VLITE_ADMIN_USERNAME with VLITE_ADMIN_PASSWORD_FILE; [--log-buffer <n>] (default 2000, maximum 100000)";

struct Config {
    lock_held_reads: bool,
    group_commit: Option<GroupCommitConfig>,
    otlp_endpoint: Option<String>,
    path: PathBuf,
    dimensions: Option<usize>,
    format: FormatVersion,
    bind: SocketAddr,
    /// Optional second listener for the gRPC transport (feature `grpc`).
    grpc_bind: Option<SocketAddr>,
    /// Native TLS identity for the REST listener.
    rest_tls: Option<vlite::server::tls::TlsIdentity>,
    #[cfg(feature = "grpc")]
    grpc_tls: Option<vlite::server::tls::TlsIdentity>,
    /// Load `<active>.ann` after open; save it after both listeners drain.
    ann_snapshot: bool,
    encryption_key: Option<vlite::EncryptionKey>,
    token: Option<BearerToken>,
    token_file: Option<PathBuf>,
    read_only_token: Option<BearerToken>,
    read_only_file: Option<PathBuf>,
    admin_directory: Option<PathBuf>,
    log_buffer_capacity: usize,
    schedule: Schedule,
    accounts: AccountSettings,
}

/// Dashboard sign-in settings from the environment.
#[derive(Default)]
struct AccountSettings {
    trusted_proxy: bool,
    session_idle: Option<Duration>,
    session_lifetime: Option<Duration>,
    admin_username: Option<String>,
    admin_password_file: Option<PathBuf>,
}

fn env_flag(name: &str) -> Result<bool, Box<dyn Error>> {
    match std::env::var(name).as_deref() {
        Err(std::env::VarError::NotPresent) | Ok("0" | "false") => Ok(false),
        Ok("1" | "true") => Ok(true),
        _ => Err(format!("{name} must be 0, 1, true or false").into()),
    }
}

fn account_settings() -> Result<AccountSettings, Box<dyn Error>> {
    let admin_username = match std::env::var("VLITE_ADMIN_USERNAME") {
        Err(std::env::VarError::NotPresent) => None,
        Ok(name) => Some(name),
        Err(_) => return Err("VLITE_ADMIN_USERNAME must be valid UTF-8".into()),
    };
    Ok(AccountSettings {
        trusted_proxy: env_flag("VLITE_TRUSTED_PROXY")?,
        session_idle: env_number::<u64>("VLITE_SESSION_IDLE_MINUTES")?
            .map(|minutes| Duration::from_secs(minutes.saturating_mul(60))),
        session_lifetime: env_number::<u64>("VLITE_SESSION_MAX_HOURS")?
            .map(|hours| Duration::from_secs(hours.saturating_mul(3600))),
        admin_username,
        admin_password_file: std::env::var_os("VLITE_ADMIN_PASSWORD_FILE").map(PathBuf::from),
    })
}

/// Off-by-default operational settings from the environment.
#[derive(Default)]
struct Schedule {
    auto_maintenance_interval: Option<Duration>,
    backup_interval: Option<Duration>,
    backup_retention_enabled: bool,
    backup_retain_last: Option<usize>,
    backup_retain_days: Option<u64>,
    backup_drill_every: Option<u64>,
    backup_drill_max_bytes: Option<u64>,
    slow_query_threshold: Option<Duration>,
}

fn env_number<T: std::str::FromStr>(name: &str) -> Result<Option<T>, Box<dyn Error>> {
    match std::env::var_os(name) {
        None => Ok(None),
        Some(value) => value
            .to_str()
            .and_then(|text| text.parse().ok())
            .map(Some)
            .ok_or_else(|| format!("{name} must be a non-negative integer").into()),
    }
}

fn schedule() -> Result<Schedule, Box<dyn Error>> {
    let retention = match std::env::var("VLITE_BACKUP_RETENTION_ENABLED").as_deref() {
        Err(std::env::VarError::NotPresent) | Ok("0" | "false") => false,
        Ok("1" | "true") => true,
        _ => return Err("VLITE_BACKUP_RETENTION_ENABLED must be 0, 1, true or false".into()),
    };
    Ok(Schedule {
        auto_maintenance_interval: env_number("VLITE_AUTO_MAINTENANCE_INTERVAL_SECS")?
            .map(Duration::from_secs),
        backup_interval: env_number("VLITE_BACKUP_INTERVAL_SECS")?.map(Duration::from_secs),
        backup_retention_enabled: retention,
        backup_retain_last: env_number("VLITE_BACKUP_RETAIN_LAST")?,
        backup_retain_days: env_number("VLITE_BACKUP_RETAIN_DAYS")?,
        backup_drill_every: env_number("VLITE_BACKUP_DRILL_EVERY")?,
        backup_drill_max_bytes: env_number("VLITE_BACKUP_DRILL_MAX_BYTES")?,
        slow_query_threshold: env_number("VLITE_SLOW_QUERY_MS")?.map(Duration::from_millis),
    })
}

fn options(config: &Config, database_path: Option<PathBuf>) -> ServerOptions {
    let s = &config.schedule;
    ServerOptions {
        admin_token: config.token.clone(),
        read_only_token: config.read_only_token.clone(),
        admin_token_file: config.token_file.clone(),
        read_only_token_file: config.read_only_file.clone(),
        admin_directory: config.admin_directory.clone(),
        auto_maintenance_interval: s.auto_maintenance_interval,
        backup_interval: s.backup_interval,
        backup_retention_enabled: s.backup_retention_enabled,
        backup_retain_last: s.backup_retain_last,
        backup_retain_days: s.backup_retain_days,
        backup_drill_every: s.backup_drill_every,
        backup_drill_max_bytes: s.backup_drill_max_bytes,
        slow_query_threshold: s.slow_query_threshold,
        log_buffer_capacity: Some(config.log_buffer_capacity),
        database_path,
        https: config.rest_tls.is_some(),
        loopback_only: config.bind.ip().is_loopback(),
        trusted_proxy: config.accounts.trusted_proxy,
        session_idle: config.accounts.session_idle,
        session_lifetime: config.accounts.session_lifetime,
        admin_username: config.accounts.admin_username.clone(),
        admin_password_file: config.accounts.admin_password_file.clone(),
    }
}

fn decode_hex_key(value: &str) -> Result<[u8; 32], Box<dyn Error>> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(
            "VLITE_ENCRYPTION_KEY_HEX must contain exactly 64 hexadecimal characters".into(),
        );
    }
    let mut key = [0u8; 32];
    for (index, byte) in key.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&value[index * 2..index * 2 + 2], 16)?;
    }
    Ok(key)
}

fn config() -> Result<Config, Box<dyn Error>> {
    let mut args = std::env::args_os().skip(1);
    let path = args.next().ok_or(USAGE)?;
    let mut otlp_endpoint = std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT").ok();
    let mut dimensions = None;
    let mut format = None;
    let mut bind: SocketAddr = "127.0.0.1:8080".parse()?;
    let mut allow_remote = false;
    let mut seen_bind = false;
    let mut token_file = std::env::var_os("VLITE_AUTH_TOKEN_FILE");
    let mut insecure_local = false;
    let mut grpc_bind: Option<SocketAddr> = None;
    let mut ann_snapshot = false;
    let mut encryption_key_file: Option<PathBuf> = None;
    let mut tls_cert: Option<PathBuf> = None;
    let mut tls_key: Option<PathBuf> = None;
    let mut grpc_tls_client_ca: Option<PathBuf> = None;
    let mut grpc_tls_cert: Option<PathBuf> = None;
    let mut grpc_tls_key: Option<PathBuf> = None;
    let mut log_buffer_capacity = 2_000usize;
    let mut seen_log_buffer = false;
    let mut group_commit = None;
    let mut lock_held_reads = false;
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--lock-held-reads") if !lock_held_reads => lock_held_reads = true,
            Some("--group-commit") if group_commit.is_none() => {
                group_commit = Some(GroupCommitConfig::default());
            }
            Some(
                "--group-commit-requests" | "--group-commit-bytes" | "--group-commit-delay-ms",
            ) => {
                let config = group_commit
                    .as_mut()
                    .ok_or("group commit limits must follow --group-commit")?;
                let value = args.next().ok_or(USAGE)?;
                let value = value.to_str().ok_or(USAGE)?;
                match arg.to_str() {
                    Some("--group-commit-requests") => config.max_requests = value.parse()?,
                    Some("--group-commit-bytes") => config.max_bytes = value.parse()?,
                    Some("--group-commit-delay-ms") => {
                        config.max_delay = Duration::from_millis(value.parse()?)
                    }
                    _ => unreachable!(),
                }
            }
            Some("--otlp-endpoint") => {
                otlp_endpoint = Some(args.next().ok_or(USAGE)?.to_str().ok_or(USAGE)?.to_owned());
            }
            Some("--create-if-missing") if dimensions.is_none() => {
                let value = args.next().ok_or(USAGE)?;
                let count: usize = value.to_str().ok_or(USAGE)?.parse()?;
                if !(1..=vlite::MAX_DIMENSIONS).contains(&count) {
                    return Err(vlite::Error::InvalidDimensions(count).into());
                }
                dimensions = Some(count);
            }
            Some("--create-format") if format.is_none() => {
                format = Some(match args.next().ok_or(USAGE)?.to_str() {
                    Some("3") => FormatVersion::V3,
                    Some("4") => FormatVersion::V4,
                    Some("5") => FormatVersion::V5,
                    _ => return Err("--create-format must be 3, 4 or 5".into()),
                });
            }
            Some("--bind") if !seen_bind => {
                bind = args.next().ok_or(USAGE)?.to_str().ok_or(USAGE)?.parse()?;
                seen_bind = true;
            }
            Some("--grpc-bind") if grpc_bind.is_none() => {
                grpc_bind = Some(args.next().ok_or(USAGE)?.to_str().ok_or(USAGE)?.parse()?);
            }
            Some("--tls-cert") if tls_cert.is_none() => {
                tls_cert = Some(args.next().ok_or(USAGE)?.into());
            }
            Some("--tls-key") if tls_key.is_none() => {
                tls_key = Some(args.next().ok_or(USAGE)?.into());
            }
            Some("--grpc-tls-client-ca") if grpc_tls_client_ca.is_none() => {
                grpc_tls_client_ca = Some(args.next().ok_or(USAGE)?.into());
            }
            Some("--grpc-tls-cert") if grpc_tls_cert.is_none() => {
                grpc_tls_cert = Some(args.next().ok_or(USAGE)?.into());
            }
            Some("--grpc-tls-key") if grpc_tls_key.is_none() => {
                grpc_tls_key = Some(args.next().ok_or(USAGE)?.into());
            }
            Some("--log-buffer") if !seen_log_buffer => {
                log_buffer_capacity = args.next().ok_or(USAGE)?.to_str().ok_or(USAGE)?.parse()?;
                if !(1..=100_000).contains(&log_buffer_capacity) {
                    return Err("--log-buffer must be 1..100000".into());
                }
                seen_log_buffer = true;
            }
            Some("--ann-snapshot") if !ann_snapshot => ann_snapshot = true,
            Some("--encryption-key-file") if encryption_key_file.is_none() => {
                encryption_key_file = Some(args.next().ok_or(USAGE)?.into());
            }
            Some("--allow-remote") if !allow_remote => allow_remote = true,
            Some("--token-file") if token_file.is_none() => {
                token_file = Some(args.next().ok_or(USAGE)?);
            }
            Some("--insecure-local") if !insecure_local => insecure_local = true,
            _ => return Err(USAGE.into()),
        }
    }
    if otlp_endpoint.is_some() && !cfg!(feature = "otel") {
        return Err(
            "--otlp-endpoint / OTEL_EXPORTER_OTLP_ENDPOINT requires the otel feature".into(),
        );
    }
    if format.is_some() && dimensions.is_none() {
        return Err("--create-format requires --create-if-missing".into());
    }
    if grpc_bind.is_some() && !cfg!(feature = "grpc") {
        return Err("--grpc-bind requires a build with the grpc feature".into());
    }
    if tls_cert.is_some() != tls_key.is_some() {
        return Err("--tls-cert and --tls-key must be given together".into());
    }
    if grpc_tls_client_ca.is_some() && grpc_tls_cert.is_none() {
        return Err("--grpc-tls-client-ca requires --grpc-tls-cert and --grpc-tls-key".into());
    }
    let rest_tls = match (&tls_cert, &tls_key) {
        (Some(cert), Some(key)) => Some(
            vlite::server::tls::TlsIdentity::from_files(cert, key, None)
                .map_err(|error| format!("invalid REST TLS identity: {error}"))?,
        ),
        _ => None,
    };
    if grpc_tls_cert.is_some() != grpc_tls_key.is_some() {
        return Err("--grpc-tls-cert and --grpc-tls-key must be given together".into());
    }
    if grpc_tls_cert.is_some() && grpc_bind.is_none() {
        return Err("--grpc-tls-cert requires --grpc-bind".into());
    }
    if let Some(grpc) = grpc_bind {
        // Port 0 asks for two distinct ephemeral ports; anything else must differ.
        if grpc == bind && grpc.port() != 0 {
            return Err("--grpc-bind must differ from --bind".into());
        }
        if !grpc.ip().is_loopback() && !allow_remote {
            return Err(
                "non-loopback --grpc-bind requires --allow-remote and bearer authentication; use --grpc-tls-cert/--grpc-tls-key or terminate TLS at a proxy"
                    .into(),
            );
        }
    }
    if !bind.ip().is_loopback() && !allow_remote {
        return Err(
            "non-loopback bind requires --allow-remote and bearer authentication; use --tls-cert/--tls-key or terminate TLS at a proxy"
                .into(),
        );
    }
    let token_env = std::env::var_os("VLITE_AUTH_TOKEN");
    if usize::from(token_file.is_some())
        + usize::from(token_env.is_some())
        + usize::from(insecure_local)
        != 1
    {
        return Err(
            "select exactly one of token file, VLITE_AUTH_TOKEN, or --insecure-local".into(),
        );
    }
    if insecure_local && grpc_bind.is_some_and(|grpc| !grpc.ip().is_loopback()) {
        return Err(
            "--insecure-local requires a loopback --grpc-bind, even with --allow-remote".into(),
        );
    }
    if insecure_local && !bind.ip().is_loopback() {
        return Err("--insecure-local requires a loopback bind, even with --allow-remote".into());
    }
    let token = if insecure_local {
        None
    } else if let Some(ref path) = token_file {
        read_token_file(std::path::Path::new(path), false)?
    } else {
        let text = token_env
            .expect("validated token source")
            .into_string()
            .map_err(|_| "token must be valid ASCII")?;
        Some(BearerToken::new(&text)?)
    };
    let read_only_file = std::env::var_os("VLITE_READ_ONLY_TOKEN_FILE").map(PathBuf::from);
    let read_only_env = std::env::var_os("VLITE_READ_ONLY_TOKEN");
    if read_only_file.is_some() && read_only_env.is_some() {
        return Err("select one read-only token source".into());
    }
    let read_only_token = if let Some(file) = &read_only_file {
        read_token_file(file, true)?
    } else {
        read_only_env
            .map(|value| {
                let text = value
                    .into_string()
                    .map_err(|_| "read-only token must be ASCII")?;
                BearerToken::new(&text)
            })
            .transpose()?
    };
    #[cfg(feature = "grpc")]
    let grpc_tls = match (&grpc_tls_cert, &grpc_tls_key) {
        (Some(cert), Some(key)) => Some(
            vlite::server::tls::TlsIdentity::from_files(cert, key, grpc_tls_client_ca.as_deref())
                .map_err(|error| format!("invalid gRPC TLS identity (cannot read --grpc-tls-cert/--grpc-tls-key/--grpc-tls-client-ca or invalid PEM): {error}"))?,
        ),
        _ => None,
    };
    #[cfg(not(feature = "grpc"))]
    let _ = (grpc_tls_cert, grpc_tls_key, grpc_tls_client_ca);
    if insecure_local && (read_only_token.is_some() || read_only_file.is_some()) {
        return Err("read-only credentials require admin bearer authentication".into());
    }
    let env_key = std::env::var_os("VLITE_ENCRYPTION_KEY");
    let env_hex = std::env::var_os("VLITE_ENCRYPTION_KEY_HEX");
    if usize::from(encryption_key_file.is_some())
        + usize::from(env_key.is_some())
        + usize::from(env_hex.is_some())
        > 1
    {
        return Err("select only one encryption key source".into());
    }
    let encryption_key = if let Some(path) = encryption_key_file {
        let bytes = std::fs::read(path)?;
        Some(if bytes.len() == 32 {
            vlite::EncryptionKey::from_bytes(bytes.as_slice().try_into().expect("32-byte key"))
        } else {
            let end = bytes
                .iter()
                .rposition(|byte| !matches!(byte, b'\r' | b'\n'))
                .map_or(0, |n| n + 1);
            vlite::EncryptionKey::from_passphrase(&bytes[..end])?
        })
    } else if let Some(hex) = env_hex {
        let text = hex
            .into_string()
            .map_err(|_| "encryption key hex must be ASCII")?;
        let raw = decode_hex_key(&text)?;
        Some(vlite::EncryptionKey::from_bytes(raw))
    } else if let Some(passphrase) = env_key {
        Some(vlite::EncryptionKey::from_passphrase(
            passphrase.to_string_lossy().as_bytes(),
        )?)
    } else {
        None
    };
    if encryption_key.is_some()
        && dimensions.is_some()
        && format.is_some_and(|f| f != FormatVersion::V5)
    {
        return Err("encryption requires --create-format 5".into());
    }
    let config = Config {
        lock_held_reads,
        group_commit,
        otlp_endpoint,
        path: path.into(),
        dimensions,
        format: format.unwrap_or(if encryption_key.is_some() {
            FormatVersion::V5
        } else {
            FormatVersion::V3
        }),
        bind,
        grpc_bind,
        rest_tls,
        #[cfg(feature = "grpc")]
        grpc_tls,
        ann_snapshot,
        encryption_key,
        token,
        token_file: token_file.map(PathBuf::from),
        read_only_token,
        read_only_file,
        admin_directory: std::env::var_os("VLITE_ADMIN_DIR").map(PathBuf::from),
        log_buffer_capacity,
        schedule: schedule()?,
        accounts: account_settings()?,
    };
    // Validate every option before creating or opening a database.
    options(&config, None).validate()?;
    if let Some(group) = config.group_commit {
        group.validate()?;
    }
    Ok(config)
}

fn open(config: &Config) -> vlite::Result<(Database, PathBuf)> {
    let selected = config
        .admin_directory
        .as_deref()
        .map(selected_database)
        .transpose()?
        .flatten();
    let path = selected.as_deref().unwrap_or(&config.path);
    let db = match config.encryption_key.as_ref().map_or_else(
        || Database::open(path),
        |key| Database::open_encrypted(path, key),
    ) {
        Ok(db) => db,
        Err(vlite::Error::Io(error))
            if error.kind() == std::io::ErrorKind::NotFound
                && config.dimensions.is_some()
                && selected.is_none() =>
        {
            if let Some(key) = &config.encryption_key {
                Database::create_with_options(
                    &config.path,
                    config.dimensions.expect("guarded dimensions"),
                    vlite::CreateOptions {
                        format: Some(config.format),
                        encryption: Some(key.clone()),
                        ..Default::default()
                    },
                )?
            } else {
                Database::create_with_format(
                    &config.path,
                    config.dimensions.expect("guarded dimensions"),
                    config.format,
                )?
            }
        }
        Err(error) => return Err(error),
    };
    if let Some(expected) = config.dimensions
        && db.dimensions() != expected
    {
        return Err(vlite::Error::DimensionMismatch {
            expected,
            actual: db.dimensions(),
        });
    }
    Ok((db, path.to_path_buf()))
}

/// Graceful-stop signals, registered before any listener binds: Ctrl-C
/// everywhere, SIGTERM on Unix (container stop) and Ctrl-Break on Windows
/// (the console event a supervisor can send to one process group).
struct Signals {
    #[cfg(unix)]
    terminate: tokio::signal::unix::Signal,
    #[cfg(windows)]
    ctrl_break: tokio::signal::windows::CtrlBreak,
}

impl Signals {
    fn new() -> std::io::Result<Self> {
        Ok(Self {
            #[cfg(unix)]
            terminate: tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?,
            #[cfg(windows)]
            ctrl_break: tokio::signal::windows::ctrl_break()?,
        })
    }

    async fn recv(mut self) -> std::io::Result<()> {
        #[cfg(unix)]
        let result = tokio::select! {
            result = tokio::signal::ctrl_c() => result,
            _ = self.terminate.recv() => Ok(()),
        };
        #[cfg(windows)]
        let result = tokio::select! {
            result = tokio::signal::ctrl_c() => result,
            _ = self.ctrl_break.recv() => Ok(()),
        };
        #[cfg(not(any(unix, windows)))]
        let result = tokio::signal::ctrl_c().await;
        result
    }
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// Loads `<active>.ann`. Every outcome is one stderr JSON line; no outcome
/// fails startup (the sidecar is a disposable cache).
fn load_ann_snapshot(database: &mut Database, path: &Path) {
    let started = Instant::now();
    let line = match database.load_ann_snapshot(path) {
        Ok(outcome) => {
            let (indexes, reason) = match outcome {
                AnnSnapshotLoad::Loaded { indexes } => (indexes, None),
                AnnSnapshotLoad::Rejected(rejection) => (0, Some(rejection.to_string())),
                _ => (0, None),
            };
            json!({"event":"ann_snapshot_load","outcome":outcome.code(),"reason":reason,
                "indexes":indexes,"duration_ms":elapsed_ms(started)})
        }
        Err(error) => json!({"event":"ann_snapshot_load","outcome":"error",
            "reason":error.to_string(),"indexes":0,"duration_ms":elapsed_ms(started)}),
    };
    vlite::server::log_stderr("info", "maintenance", line.to_string());
}

/// Saves after both listeners drained. A failed save is logged and never
/// blocks `close` or changes the exit code (the prior sidecar stays intact).
async fn save_ann_snapshot(service: &Service, path: &Path) {
    let started = Instant::now();
    let line = match service.save_ann_snapshot(path).await {
        Ok(report) => json!({"event":"ann_snapshot_save","outcome":"saved","reason":null,
            "indexes":report.indexes,"bytes":report.bytes,"duration_ms":elapsed_ms(started)}),
        Err(error) => json!({"event":"ann_snapshot_save","outcome":"failed",
            "reason":error.to_string(),"indexes":0,"bytes":0,"duration_ms":elapsed_ms(started)}),
    };
    vlite::server::log_stderr("info", "maintenance", line.to_string());
}

#[cfg(feature = "grpc")]
type GrpcHealth = vlite::server::grpc::GrpcHealth;
/// Uninhabited: without the feature there is never a gRPC listener.
#[cfg(not(feature = "grpc"))]
#[derive(Clone)]
enum GrpcHealth {}

/// Resolves once the shared stop flag is raised, after reporting gRPC
/// health `NOT_SERVING`, so health flips before either listener drains.
async fn stopped(mut stop: tokio::sync::watch::Receiver<bool>, health: Option<GrpcHealth>) {
    let _ = stop.wait_for(|stop| *stop).await;
    #[cfg(feature = "grpc")]
    if let Some(health) = health {
        health.set_not_serving().await;
    }
    #[cfg(not(feature = "grpc"))]
    let _ = health;
}

async fn run() -> Result<(), Box<dyn Error>> {
    let config = config()?;
    #[cfg(feature = "otel")]
    let telemetry = vlite::server::telemetry::configure(config.otlp_endpoint.as_deref())?;
    #[cfg(not(feature = "otel"))]
    let _ = &config.otlp_endpoint;
    let signals = Signals::new()?;
    // Bind both listeners before opening, so a port conflict never opens
    // (or creates) the database.
    let listener = TcpListener::bind(config.bind).await?;
    let address = listener.local_addr()?;
    let grpc_listener = match config.grpc_bind {
        Some(bind) => Some(TcpListener::bind(bind).await?),
        None => None,
    };
    let (mut database, active) = open(&config)?;
    let snapshot = config
        .ann_snapshot
        .then(|| Database::ann_snapshot_path(&active));
    if let Some(path) = &snapshot {
        load_ann_snapshot(&mut database, path);
    }
    let service = match config.encryption_key.clone() {
        Some(key) => Service::try_new_with_encryption_key(database, key)?,
        None => Service::try_new(database)?,
    };
    let service = match config.group_commit {
        Some(group) => service.with_group_commit(group)?,
        None => service,
    };
    let service = service.with_snapshot_reads(!config.lock_held_reads)?;
    if let Some(tls) = &config.rest_tls {
        service.register_tls(tls.clone())?;
    }
    #[cfg(feature = "grpc")]
    if let Some(tls) = &config.grpc_tls {
        service.register_tls(tls.clone())?;
    }
    let router = match configured_admin_router(service.clone(), options(&config, Some(active))) {
        Ok(router) => router,
        Err(error) => {
            // Never leave a scheduler or the database open on failed startup.
            let _ = service.close().await;
            return Err(error.into());
        }
    };
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let signal = tokio::spawn({
        let stop_tx = stop_tx.clone();
        async move {
            let result = signals.recv().await;
            let _ = stop_tx.send(true);
            result
        }
    });

    #[cfg(feature = "grpc")]
    let grpc = match grpc_listener {
        // Built after the REST router so it shares its tokens and audit log.
        Some(listener) => {
            let server = vlite::server::grpc::grpc_router(service.clone()).await;
            let server = match config.grpc_tls.clone() {
                Some(tls) => server.with_reloadable_tls(tls),
                None => server,
            };
            Some((server, listener))
        }
        None => None,
    };
    #[cfg(not(feature = "grpc"))]
    let grpc: Option<((), TcpListener)> = {
        // `config` rejects --grpc-bind without the feature.
        debug_assert!(grpc_listener.is_none());
        None
    };
    #[cfg(feature = "grpc")]
    let health = grpc.as_ref().map(|(server, _)| server.health());
    #[cfg(not(feature = "grpc"))]
    let health: Option<GrpcHealth> = None;

    let scheme = if config.rest_tls.is_some() {
        "https"
    } else {
        "http"
    };
    vlite::server::log_stderr(
        "info",
        "server",
        json!({"event":"server_start","address":address.to_string()}).to_string(),
    );
    println!("LISTENING {scheme}://{address}");
    if let Some((_, listener)) = &grpc {
        #[cfg(feature = "grpc")]
        let scheme = if config.grpc_tls.is_some() {
            "grpcs"
        } else {
            "grpc"
        };
        #[cfg(not(feature = "grpc"))]
        let scheme = "grpc";
        println!("LISTENING {scheme}://{}", listener.local_addr()?);
    }
    let rest = {
        let stop_tx = stop_tx.clone();
        let shutdown = stopped(stop_rx.clone(), health.clone());
        async move {
            let result = if let Some(tls) = config.rest_tls {
                axum::serve(vlite::server::tls::TlsListener::new(listener, tls)?, router)
                    .with_graceful_shutdown(shutdown)
                    .await
            } else {
                axum::serve(listener, router)
                    .with_graceful_shutdown(shutdown)
                    .await
            };
            // Either listener failing stops the other one.
            let _ = stop_tx.send(true);
            result.map_err(|error| -> Box<dyn Error> { error.into() })
        }
    };
    let grpc = async move {
        let Some((server, listener)) = grpc else {
            return Ok(());
        };
        #[cfg(feature = "grpc")]
        let result = server
            .serve_with_shutdown(listener, stopped(stop_rx, health))
            .await
            .map_err(|error| -> Box<dyn Error> { error.into() });
        #[cfg(not(feature = "grpc"))]
        let result: Result<(), Box<dyn Error>> = {
            let _ = (server, listener, stop_rx, health);
            Ok(())
        };
        let _ = stop_tx.send(true);
        result
    };
    let (rest, grpc) = tokio::join!(rest, grpc);
    if let Some(path) = &snapshot {
        save_ann_snapshot(&service, path).await;
    }
    let closing = service.close().await;
    vlite::server::log_stderr(
        "info",
        "server",
        json!({"event":"server_shutdown"}).to_string(),
    );
    rest?;
    grpc?;
    closing?;
    #[cfg(feature = "otel")]
    if let Some(provider) = telemetry {
        provider.shutdown()?;
    }
    if signal.is_finished() {
        signal.await??;
    } else {
        signal.abort();
    }
    Ok(())
}
#[tokio::main]
async fn main() -> std::process::ExitCode {
    match run().await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            vlite::server::log_stderr("error", "server", serde_json::json!({"event":"startup_or_shutdown_failure","message":error.to_string()}).to_string());
            std::process::ExitCode::FAILURE
        }
    }
}
