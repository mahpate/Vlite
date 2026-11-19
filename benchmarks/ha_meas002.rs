use reqwest::blocking::Client;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    env, fs,
    io::{BufRead, BufReader, Read},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};
use vlite::{Database, FormatVersion};

type Result<T, E = String> = std::result::Result<T, E>;

struct Options {
    server: PathBuf,
    target_file: Option<PathBuf>,
    target: Value,
    shared_key_file: Option<PathBuf>,
    duration_secs: u64,
    rate: u64,
    archive_interval_secs: u64,
    staleness_bound_ms: u64,
    reader_poll_ms: u64,
    observe_ms: u64,
    work_dir: Option<PathBuf>,
}

impl Options {
    fn parse(
        args: Vec<String>,
        server_env: Option<String>,
        target_env: Option<String>,
    ) -> Result<Self> {
        let allowed = [
            "--server",
            "--target-file",
            "--shared-key-file",
            "--duration-secs",
            "--rate",
            "--archive-interval-secs",
            "--staleness-bound-ms",
            "--reader-poll-ms",
            "--observe-ms",
            "--work-dir",
        ];
        let mut flags = BTreeMap::new();
        let mut args = args.into_iter();
        while let Some(flag) = args.next() {
            if !allowed.contains(&flag.as_str()) {
                return Err("unknown argument".into());
            }
            let value = args.next().ok_or("argument requires a value")?;
            if value.is_empty() || value.starts_with("--") {
                return Err("argument requires a value".into());
            }
            if flags.insert(flag, value).is_some() {
                return Err("repeated argument".into());
            }
        }
        let server = flags
            .get("--server")
            .cloned()
            .or(server_env)
            .filter(|s| !s.is_empty())
            .ok_or("--server is required")?
            .into();
        let target_file = flags.get("--target-file").map(PathBuf::from);
        let target_text = match (&target_file, target_env) {
            (Some(path), None) => {
                fs::read_to_string(path).map_err(|_| "cannot read target file")?
            }
            (None, Some(text)) => text,
            _ => return Err("exactly one target source is required".into()),
        };
        let target: Value =
            serde_json::from_str(&target_text).map_err(|_| "invalid target JSON")?;
        let object = target.as_object().ok_or("target must be an object")?;
        if object.keys().any(|k| {
            ![
                "account",
                "container",
                "coordination_container",
                "prefix",
                "auth",
                "client_id",
                "endpoint",
            ]
            .contains(&k.as_str())
        }) {
            return Err("unknown target field".into());
        }
        for field in [
            "account",
            "container",
            "coordination_container",
            "prefix",
            "auth",
        ] {
            if !target[field].is_string() {
                return Err(format!("target requires string field {field}"));
            }
        }
        let shared_key_file = flags.get("--shared-key-file").map(PathBuf::from);
        match target["auth"].as_str() {
            Some("managed_identity") if shared_key_file.is_none() => {
                if let Some(endpoint) = target.get("endpoint") {
                    let public = format!(
                        "https://{}.blob.core.windows.net",
                        target["account"].as_str().unwrap()
                    );
                    if endpoint.as_str().map(|s| s.trim_end_matches('/')) != Some(public.as_str()) {
                        return Err("managed identity requires the public default endpoint".into());
                    }
                }
            }
            Some("emulator_shared_key") if shared_key_file.is_some() => {
                let endpoint = target["endpoint"]
                    .as_str()
                    .ok_or("emulator endpoint is required")?;
                let url = reqwest::Url::parse(endpoint).map_err(|_| "invalid emulator endpoint")?;
                let host = url.host_str().unwrap_or("").trim_matches(['[', ']']);
                if !(host.eq_ignore_ascii_case("localhost")
                    || host
                        .parse::<std::net::IpAddr>()
                        .is_ok_and(|ip| ip.is_loopback()))
                    || !matches!(url.scheme(), "http" | "https")
                    || url.query().is_some()
                    || url.fragment().is_some()
                    || !url.username().is_empty()
                    || url.password().is_some()
                    || target.get("client_id").is_some()
                {
                    return Err(
                        "emulator requires a credential-free loopback endpoint and no client id"
                            .into(),
                    );
                }
            }
            _ => return Err("auth and shared-key-file are incompatible".into()),
        }
        run_prefix(target["prefix"].as_str().unwrap(), "0123456789abcdef")?;
        let number = |flag: &str, default, low, high| -> Result<u64> {
            let value = match flags.get(flag) {
                Some(text) => text.parse().map_err(|_| format!("invalid {flag}"))?,
                None => default,
            };
            if !(low..=high).contains(&value) {
                return Err(format!("{flag} is out of range"));
            }
            Ok(value)
        };
        let options = Self {
            server,
            target_file,
            target,
            shared_key_file,
            duration_secs: number("--duration-secs", 360, 60, 3600)?,
            rate: number("--rate", 20, 1, 200)?,
            archive_interval_secs: number("--archive-interval-secs", 60, 60, 86400)?,
            staleness_bound_ms: number("--staleness-bound-ms", 300_000, 1000, 3_600_000)?,
            reader_poll_ms: number("--reader-poll-ms", 1000, 100, 60_000)?,
            observe_ms: number("--observe-ms", 100, 50, 5000)?,
            work_dir: flags.get("--work-dir").map(PathBuf::from),
        };
        if options.reader_poll_ms >= options.staleness_bound_ms {
            return Err("reader-poll-ms must be below staleness-bound-ms".into());
        }
        if options.work_dir.as_ref().is_some_and(|p| !p.is_absolute()) {
            return Err("work-dir must be absolute".into());
        }
        Ok(options)
    }

    fn safe_config(&self, work_dir: &Path, prefix: &str) -> Value {
        json!({
            "server": self.server, "target_file": self.target_file,
            "target_source": if self.target_file.is_some() { "file" } else { "environment" },
            "shared_key_file": self.shared_key_file,
            "auth": self.target["auth"], "prefix": prefix,
            "target": {
                "account": self.target["account"], "container": self.target["container"],
                "coordination_container": self.target["coordination_container"],
                "endpoint": self.target["endpoint"], "prefix": prefix, "auth": self.target["auth"]
            },
            "duration_secs": self.duration_secs, "rate": self.rate,
            "archive_interval_secs": self.archive_interval_secs,
            "staleness_bound_ms": self.staleness_bound_ms, "reader_poll_ms": self.reader_poll_ms,
            "observe_ms": self.observe_ms, "work_dir": work_dir,
            "capacity_bytes": 1_073_741_824_u64
        })
    }
}

fn run_prefix(prefix: &str, run_id: &str) -> Result<String> {
    let result = if prefix.is_empty() || prefix.ends_with('/') {
        format!("{prefix}{run_id}/")
    } else {
        format!("{prefix}/{run_id}/")
    };
    if result.len() > 200
        || result.starts_with('/')
        || result.contains("//")
        || !result
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._/-".contains(&b))
        || result
            .trim_end_matches('/')
            .split('/')
            .any(|p| p == "." || p == "..")
    {
        return Err("invalid run prefix".into());
    }
    Ok(result)
}

fn random_hex(bytes: usize) -> Result<String> {
    let mut buffer = vec![0; bytes];
    getrandom::fill(&mut buffer).map_err(|_| "random generation failed")?;
    Ok(buffer.iter().map(|b| format!("{b:02x}")).collect())
}

fn error_code(value: &Value) -> Option<&str> {
    let text = value.get("error")?.as_str()?;
    let code = text.split(':').next()?;
    if code.is_empty()
        || !code
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
    {
        return None;
    }
    Some(code)
}

fn authority_error(stderr: &[u8]) -> String {
    // The operator emits structured diagnostic lines before its final error JSON.
    stderr
        .split(|b| *b == b'\n')
        .rev()
        .filter_map(|line| serde_json::from_slice::<Value>(line).ok())
        .find_map(|value| error_code(&value).map(str::to_owned))
        .unwrap_or_else(|| "malformed_authority_error".into())
}

fn status_key(status: u16, body: &Value) -> String {
    match error_code(body) {
        Some(code) => format!("{status}:{code}"),
        None => status.to_string(),
    }
}

fn summary(samples: &[f64]) -> Value {
    if samples.is_empty() {
        return Value::Null;
    }
    let mut sorted = samples.to_vec();
    sorted.sort_by(f64::total_cmp);
    let rank = |p: usize| sorted[(p * sorted.len()).div_ceil(100) - 1];
    json!({
        "count":sorted.len(), "min":sorted[0], "p50":rank(50), "p90":rank(90),
        "p95":rank(95), "p99":rank(99), "max":sorted[sorted.len()-1],
        "mean": sorted.iter().sum::<f64>() / sorted.len() as f64
    })
}

struct Process(Child);

struct StopOnDrop<'a>(&'a AtomicBool);

impl Drop for StopOnDrop<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn command(server: &Path) -> Command {
    let mut command = Command::new(server);
    for (name, _) in env::vars_os() {
        if name
            .to_string_lossy()
            .to_ascii_uppercase()
            .starts_with("VLITE_")
        {
            command.env_remove(name);
        }
    }
    command
}

struct Operator<'a> {
    options: &'a Options,
    target: &'a Path,
}

impl Operator<'_> {
    fn call(
        &self,
        verb: &str,
        extra: &[&str],
        timeout: Duration,
    ) -> Result<(bool, Vec<u8>, Vec<u8>)> {
        // Files prevent pipe backpressure while keeping every subprocess under a kill-on-drop guard.
        let mut stdout = tempfile::tempfile().map_err(|_| "cannot create operator stdout")?;
        let mut stderr = tempfile::tempfile().map_err(|_| "cannot create operator stderr")?;
        let mut cmd = command(&self.options.server);
        cmd.args(["authority", verb, "--target-file"])
            .arg(self.target)
            .args(extra)
            .stdout(
                stdout
                    .try_clone()
                    .map_err(|_| "cannot clone operator stdout")?,
            )
            .stderr(
                stderr
                    .try_clone()
                    .map_err(|_| "cannot clone operator stderr")?,
            );
        if let Some(key) = &self.options.shared_key_file {
            cmd.arg("--shared-key-file").arg(key);
        }
        let mut process = Process(cmd.spawn().map_err(|_| "cannot spawn authority command")?);
        let deadline = Instant::now() + timeout;
        let status = loop {
            if let Some(status) = process
                .0
                .try_wait()
                .map_err(|_| "cannot wait for authority command")?
            {
                break status;
            }
            if Instant::now() >= deadline {
                return Err("authority_command_timeout".into());
            }
            thread::sleep(Duration::from_millis(25));
        };
        use std::io::{Seek, SeekFrom};
        stdout
            .seek(SeekFrom::Start(0))
            .map_err(|_| "cannot seek operator stdout")?;
        stderr
            .seek(SeekFrom::Start(0))
            .map_err(|_| "cannot seek operator stderr")?;
        let mut out = Vec::new();
        let mut err = Vec::new();
        stdout
            .take(1_048_577)
            .read_to_end(&mut out)
            .map_err(|_| "cannot read operator stdout")?;
        stderr
            .take(1_048_577)
            .read_to_end(&mut err)
            .map_err(|_| "cannot read operator stderr")?;
        if out.len() > 1_048_576 || err.len() > 1_048_576 {
            return Err("authority_output_too_large".into());
        }
        Ok((status.success(), out, err))
    }

    fn preflight(&self) -> Result<(u64, f64)> {
        let start = Instant::now();
        let deadline = start + Duration::from_secs(600);
        let mut attempts = 0;
        loop {
            let next = Instant::now() + Duration::from_secs(15);
            attempts += 1;
            let remaining = deadline.saturating_duration_since(Instant::now());
            let result = self.call("show", &[], remaining.min(Duration::from_secs(60)));
            let code = match result {
                Ok((true, _, _)) => return Err("preflight_prefix_not_fresh".into()),
                Ok((false, _, stderr)) => authority_error(&stderr),
                Err(code) => code,
            };
            if code == "writer_authority_missing" {
                return Ok((attempts, start.elapsed().as_secs_f64() * 1000.0));
            }
            if Instant::now() >= deadline {
                return Err(format!("preflight_failed: {code}"));
            }
            eprintln!("preflight attempt {attempts}: {code}");
            thread::sleep(next.min(deadline).saturating_duration_since(Instant::now()));
            if Instant::now() >= deadline {
                return Err(format!("preflight_failed: {code}"));
            }
        }
    }
}

struct Server {
    _process: Process,
    url: String,
}

impl Server {
    fn spawn(mut command: Command) -> Result<Self> {
        let mut process = Process(
            command
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .spawn()
                .map_err(|_| "cannot start server")?,
        );
        let stdout = process.0.stdout.take().ok_or("server stdout missing")?;
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            let mut first = String::new();
            let result = reader
                .by_ref()
                .take(8192)
                .read_line(&mut first)
                .map(|_| first);
            let _ = sender.send(result);
            let _ = std::io::copy(&mut reader, &mut std::io::sink());
        });
        let line = receiver
            .recv_timeout(Duration::from_secs(60))
            .map_err(|_| "server startup timed out")?
            .map_err(|_| "cannot read server startup")?;
        let url = line
            .trim()
            .strip_prefix("LISTENING ")
            .ok_or("server did not announce LISTENING")?
            .to_owned();
        let parsed = reqwest::Url::parse(&url).map_err(|_| "invalid server listening URL")?;
        if parsed.scheme() != "http" || parsed.host_str() != Some("127.0.0.1") {
            return Err("server did not bind loopback".into());
        }
        Ok(Self {
            _process: process,
            url,
        })
    }
}

#[derive(Default)]
struct Measurements {
    commits: Vec<Commit>,
    statuses: BTreeMap<String, u64>,
    ages: Vec<f64>,
    observations: Vec<(u64, f64)>,
    heads: Vec<(u64, f64)>,
    last_sequence: Option<u64>,
    regressions: u64,
    matched: usize,
    lateness_max: f64,
    fatal: Option<String>,
}

struct Commit {
    sent_ms: f64,
    ack_ms: Option<f64>,
    visible_ms: Option<f64>,
}

impl Measurements {
    fn observe(&mut self, records: usize, sequence: u64, at_ms: f64) {
        if self.last_sequence.is_some_and(|last| sequence < last) {
            self.regressions += 1;
        }
        self.last_sequence = Some(sequence);
        if self
            .observations
            .last()
            .is_none_or(|(last, _)| sequence > *last)
        {
            self.observations.push((sequence, at_ms));
        }
        let end = records.min(self.commits.len());
        for commit in &mut self.commits[self.matched..end.max(self.matched)] {
            commit.visible_ms = Some(at_ms);
        }
        self.matched = self.matched.max(end);
    }

    fn all_visible(&self) -> bool {
        self.commits
            .iter()
            .all(|c| c.ack_ms.is_none() || c.visible_ms.is_some())
    }
}

fn ms(start: Instant) -> f64 {
    start.elapsed().as_secs_f64() * 1000.0
}

fn sleep_until(deadline: Instant, stop: &AtomicBool) {
    while !stop.load(Ordering::Relaxed) {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        thread::sleep(remaining.min(Duration::from_millis(50)));
    }
}

fn writer(
    client: &Client,
    url: &str,
    options: &Options,
    start: Instant,
    state: &Mutex<Measurements>,
    stop: &AtomicBool,
) {
    let phase = Instant::now();
    let end = phase + Duration::from_secs(options.duration_secs);
    for index in 0..options.duration_secs * options.rate {
        let scheduled = phase + Duration::from_secs_f64(index as f64 / options.rate as f64);
        sleep_until(scheduled, stop);
        if Instant::now() >= end || stop.load(Ordering::Relaxed) {
            break;
        }
        let sent = Instant::now();
        let id = {
            let mut state = state.lock().unwrap();
            state.lateness_max = state
                .lateness_max
                .max(sent.saturating_duration_since(scheduled).as_secs_f64() * 1000.0);
            state.commits.push(Commit {
                sent_ms: ms(start),
                ack_ms: None,
                visible_ms: None,
            });
            state.commits.len()
        };
        let result = client
            .put(format!("{url}/v1/vectors/{id}"))
            .json(&json!({"vector":[id,1]}))
            .send()
            .and_then(|response| response.error_for_status());
        match result {
            Ok(_) => state.lock().unwrap().commits[id - 1].ack_ms = Some(ms(start)),
            Err(error) => eprintln!(
                "write failed: {}",
                error
                    .status()
                    .map_or_else(|| "transport".into(), |s| s.as_u16().to_string())
            ),
        }
    }
    sleep_until(end, stop);
}

fn observer(
    client: &Client,
    url: &str,
    token: &str,
    options: &Options,
    start: Instant,
    state: &Mutex<Measurements>,
    stop: &AtomicBool,
) -> Result<()> {
    while !stop.load(Ordering::Relaxed) {
        let next = Instant::now() + Duration::from_millis(options.observe_ms);
        match client
            .get(format!("{url}/v1/database"))
            .bearer_auth(token)
            .send()
        {
            Ok(response) => {
                let status = response.status();
                let sequence = header_number(&response, "x-vlite-applied-sequence");
                let age = header_number(&response, "x-vlite-applied-age-ms");
                let body: Value = response
                    .json()
                    .map_err(|_| "invalid reader observation JSON")?;
                let at_ms = ms(start);
                let mut state = state.lock().unwrap();
                *state
                    .statuses
                    .entry(status_key(status.as_u16(), &body))
                    .or_default() += 1;
                if let Some(age) = age {
                    state.ages.push(age as f64);
                }
                if status.is_success() {
                    let records = body["records"].as_u64().ok_or("reader records missing")?;
                    let records =
                        usize::try_from(records).map_err(|_| "reader records overflow")?;
                    state.observe(
                        records,
                        sequence.ok_or("reader applied sequence missing")?,
                        at_ms,
                    );
                    if age.is_none() {
                        return Err("reader applied age missing".into());
                    }
                } else if let Some(sequence) = sequence {
                    if state.last_sequence.is_some_and(|last| sequence < last) {
                        state.regressions += 1;
                    }
                    state.last_sequence = Some(sequence);
                }
            }
            Err(_) => {
                eprintln!("reader observation transport error");
            }
        }
        sleep_until(next, stop);
    }
    Ok(())
}

fn header_number(response: &reqwest::blocking::Response, name: &str) -> Option<u64> {
    response.headers().get(name)?.to_str().ok()?.parse().ok()
}

fn heads(
    operator: &Operator<'_>,
    start: Instant,
    state: &Mutex<Measurements>,
    stop: &AtomicBool,
) -> Result<()> {
    let mut last = 0;
    while !stop.load(Ordering::Relaxed) {
        let next = Instant::now() + Duration::from_secs(1);
        let (success, stdout, stderr) = operator.call("show", &[], Duration::from_secs(60))?;
        if !success {
            return Err(format!(
                "head_observation_failed: {}",
                authority_error(&stderr)
            ));
        }
        let value: Value =
            serde_json::from_slice(&stdout).map_err(|_| "invalid authority show JSON")?;
        if !value["head"].is_null() {
            let sequence = value["head"]["sequence"]
                .as_u64()
                .ok_or("authority head sequence missing")?;
            if sequence > last {
                state.lock().unwrap().heads.push((sequence, ms(start)));
                last = sequence;
            }
        }
        sleep_until(next, stop);
    }
    Ok(())
}

fn measure(
    options: &Options,
    operator: &Operator<'_>,
    writer_url: &str,
    reader_url: &str,
    token: &str,
    start: Instant,
    drain: Duration,
) -> Result<Measurements> {
    let client = Client::builder()
        .timeout(Duration::from_secs(10))
        .no_proxy()
        .build()
        .map_err(|_| "cannot create HTTP client")?;
    let state = Mutex::new(Measurements::default());
    let stop = AtomicBool::new(false);
    thread::scope(|scope| -> Result<()> {
        let _stop_on_drop = StopOnDrop(&stop);
        let observation = scope.spawn(|| {
            let _stop_on_drop = StopOnDrop(&stop);
            let result = observer(&client, reader_url, token, options, start, &state, &stop);
            if let Err(error) = &result {
                state.lock().unwrap().fatal = Some(error.clone());
            }
            result
        });
        let head = scope.spawn(|| {
            let _stop_on_drop = StopOnDrop(&stop);
            let result = heads(operator, start, &state, &stop);
            if let Err(error) = &result {
                state.lock().unwrap().fatal = Some(error.clone());
            }
            result
        });
        // Always signal the other threads even if the writer panics.
        let write = scope.spawn(|| writer(&client, writer_url, options, start, &state, &stop));
        let written = write.join();
        if written.is_ok() {
            let deadline = Instant::now() + drain;
            loop {
                let state = state.lock().unwrap();
                if stop.load(Ordering::Relaxed)
                    || state.fatal.is_some()
                    || state.all_visible()
                    || Instant::now() >= deadline
                {
                    break;
                }
                drop(state);
                thread::sleep(Duration::from_millis(50));
            }
        }
        stop.store(true, Ordering::Relaxed);
        let observed = observation.join();
        let headed = head.join();
        written.map_err(|_| "writer thread panicked")?;
        observed.map_err(|_| "observer thread panicked")??;
        headed.map_err(|_| "head thread panicked")??;
        Ok(())
    })?;
    state
        .into_inner()
        .map_err(|_| "measurement state poisoned".into())
}

fn run() -> Result<Value> {
    let start = Instant::now();
    let mut options = Options::parse(
        env::args().skip(1).collect(),
        env::var("VLITE_MEAS002_SERVER").ok(),
        env::var("VLITE_MEAS002_TARGET").ok(),
    )?;
    let temp;
    let work_dir = if let Some(path) = &options.work_dir {
        fs::create_dir_all(path).map_err(|_| "cannot create work directory")?;
        if fs::read_dir(path)
            .map_err(|_| "cannot read work directory")?
            .next()
            .is_some()
        {
            return Err("work directory must be empty".into());
        }
        path.clone()
    } else {
        temp = tempfile::tempdir().map_err(|_| "cannot create temporary work directory")?;
        temp.path().to_owned()
    };
    let prefix = run_prefix(options.target["prefix"].as_str().unwrap(), &random_hex(8)?)?;
    options.target["prefix"] = json!(prefix);
    let safe_config = options.safe_config(&work_dir, &prefix);
    let database = work_dir.join("writer.vlite");
    let db = Database::create_with_format(&database, 2, FormatVersion::V4)
        .map_err(|_| "cannot create v4 database")?;
    let lineage = format!(
        "{:016x}",
        db.lineage().ok_or("cannot read database lineage")?
    );
    db.close().map_err(|_| "cannot close database")?;
    let target = work_dir.join("target.json");
    let policy = work_dir.join("policy.json");
    fs::write(&target, options.target.to_string()).map_err(|_| "cannot write target")?;
    fs::write(
        &policy,
        json!({
            "target":options.target, "enabled":true, "interval_secs":604800, "drill_every":0,
            "archive":{"enabled":true, "interval_secs":options.archive_interval_secs}
        })
        .to_string(),
    )
    .map_err(|_| "cannot write policy")?;
    let operator = Operator {
        options: &options,
        target: &target,
    };
    let (preflight_attempts, preflight_ms) = operator.preflight()?;
    let installation = random_hex(8)?;
    let (success, _, stderr) = operator.call(
        "init",
        &[
            "--installation-id",
            &installation,
            "--scope-id",
            "meas002",
            "--lineage",
            &lineage,
        ],
        Duration::from_secs(120),
    )?;
    if !success {
        return Err(format!(
            "authority_init_failed: {}",
            authority_error(&stderr)
        ));
    }
    let mut writer_command = command(&options.server);
    writer_command
        .arg(&database)
        .args([
            "--serve-mode",
            "writer",
            "--durability",
            "full",
            "--insecure-local",
            "--bind",
            "127.0.0.1:0",
            "--authority-target-file",
        ])
        .arg(&target)
        .arg("--archive-policy-file")
        .arg(&policy);
    if let Some(key) = &options.shared_key_file {
        writer_command
            .arg("--authority-shared-key-file")
            .arg(key)
            .env("VLITE_CLOUD_BACKUP_SHARED_KEY_FILE", key);
    }
    let writer = Server::spawn(writer_command)?;
    let token = random_hex(32)?;
    let follower = work_dir.join("follower");
    let reader_config = work_dir.join("reader.json");
    fs::write(&reader_config, json!({
        "target_file":target, "shared_key_file":options.shared_key_file, "follower_dir":follower,
        "capacity_bytes":1_073_741_824_u64, "staleness_bound_ms":options.staleness_bound_ms,
        "poll_interval_ms":options.reader_poll_ms
    }).to_string()).map_err(|_| "cannot write reader configuration")?;
    let mut reader_command = command(&options.server);
    reader_command
        .arg(follower.join("reader-a").join("database.vlite"))
        .args(["--serve-mode", "reader", "--reader-apply-file"])
        .arg(&reader_config)
        .args(["--bind", "127.0.0.1:0"])
        .env("VLITE_AUTH_TOKEN", &token);
    let reader = Server::spawn(reader_command)?;
    let drain = Duration::from_secs(2 * options.archive_interval_secs + 120);
    let client = Client::builder()
        .timeout(Duration::from_secs(5))
        .no_proxy()
        .build()
        .map_err(|_| "cannot create readiness client")?;
    let deadline = Instant::now() + drain;
    loop {
        if client
            .get(format!("{}/ready", reader.url))
            .bearer_auth(&token)
            .send()
            .is_ok_and(|r| r.status().as_u16() == 200)
        {
            break;
        }
        if Instant::now() >= deadline {
            return Err("reader_readiness_timeout".into());
        }
        thread::sleep(Duration::from_millis(250));
    }
    let measurements = measure(
        &options,
        &operator,
        &writer.url,
        &reader.url,
        &token,
        start,
        drain,
    )?;
    drop(reader);
    drop(writer);
    let acked: Vec<_> = measurements
        .commits
        .iter()
        .filter(|c| c.ack_ms.is_some())
        .collect();
    let ack_latencies: Vec<_> = acked
        .iter()
        .map(|c| c.ack_ms.unwrap() - c.sent_ms)
        .collect();
    let visible_latencies: Vec<_> = acked
        .iter()
        .filter_map(|c| c.visible_ms.map(|v| v - c.ack_ms.unwrap()))
        .collect();
    // Both endpoints are observation instants, not the actual publish/apply times.
    // Reader visibility may precede the 1 Hz head poll, yielding a negative observed delta.
    let publish_latencies: Vec<_> = measurements
        .heads
        .iter()
        .filter_map(|(sequence, published)| {
            measurements
                .observations
                .iter()
                .find(|(applied, _)| applied >= sequence)
                .map(|(_, visible)| visible - published)
        })
        .collect();
    let head_advances: Vec<_> = measurements
        .heads
        .iter()
        .take(240)
        .map(|(sequence, at)| json!({"at_ms":at,"sequence":sequence}))
        .collect();
    let platform = if env::var_os("CONTAINER_APP_JOB_NAME").is_some()
        && env::var_os("CONTAINER_APP_JOB_EXECUTION_NAME").is_some()
    {
        "azure_container_apps_job"
    } else {
        "local"
    };
    let source = match (
        env::var("VLITE_RESOURCE_SOURCE_REVISION"),
        env::var("VLITE_RESOURCE_SOURCE_DIRTY_PATHS"),
    ) {
        (Ok(revision), Ok(dirty_paths)) => {
            json!({"revision":revision,"dirty_paths":dirty_paths.lines().collect::<Vec<_>>()})
        }
        _ => Value::Null,
    };
    Ok(json!({
        "schema":"vlite-meas002/1", "platform":platform, "source":source,
        "config":safe_config,
        "host":{"available_parallelism":thread::available_parallelism().map_err(|_| "cannot read parallelism")?.get(),
            "os":env::consts::OS},
        "preflight_attempts":preflight_attempts, "preflight_ms":preflight_ms,
        "commits":{"attempted":measurements.commits.len(), "acked":acked.len(),
            "failed":measurements.commits.len()-acked.len(), "visible":visible_latencies.len(),
            "not_visible_by_deadline":acked.len()-visible_latencies.len()},
        "ack_latency_ms":summary(&ack_latencies), "ack_to_visible_ms":summary(&visible_latencies),
        "publish_to_visible_ms":summary(&publish_latencies), "applied_age_ms":summary(&measurements.ages),
        "reader_status_counts":measurements.statuses,
        "applied_sequence_regressions":measurements.regressions,
        "schedule_lateness_ms_max":measurements.lateness_max, "head_advances":head_advances,
        "elapsed_ms":ms(start)
    }))
}

fn main() {
    match run() {
        Ok(value) => println!("{value}"),
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(auth: &str) -> String {
        json!({
            "account":"devstoreaccount1", "container":"data", "coordination_container":"coord",
            "prefix":"measure/", "auth":auth
        })
        .to_string()
    }

    fn parse(flags: &[&str], env_target: Option<String>) -> Result<Options, String> {
        Options::parse(
            flags.iter().map(|s| (*s).to_owned()).collect(),
            Some("server".into()),
            env_target,
        )
    }

    #[test]
    fn argument_defaults_and_target_sources() {
        let options = parse(&[], Some(target("managed_identity"))).unwrap();
        assert_eq!(options.duration_secs, 360);
        assert_eq!(options.rate, 20);
        assert_eq!(options.archive_interval_secs, 60);
        assert_eq!(options.staleness_bound_ms, 300_000);
        assert_eq!(options.reader_poll_ms, 1000);
        assert_eq!(options.observe_ms, 100);
        assert!(parse(&[], None).is_err());
        assert!(
            parse(
                &["--target-file", "unused"],
                Some(target("managed_identity"))
            )
            .is_err()
        );
        assert!(Options::parse(vec![], None, Some(target("managed_identity"))).is_err());
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("target.json");
        fs::write(&file, target("managed_identity")).unwrap();
        let file = file.to_str().unwrap();
        assert!(parse(&["--target-file", file, "--server", "explicit"], None).is_ok());
    }

    #[test]
    fn argument_bounds_and_invalid_flags() {
        for (flag, low, high) in [
            ("--duration-secs", 60, 3600),
            ("--rate", 1, 200),
            ("--archive-interval-secs", 60, 86400),
            ("--staleness-bound-ms", 1000, 3_600_000),
            ("--reader-poll-ms", 100, 60_000),
            ("--observe-ms", 50, 5000),
        ] {
            for value in [low - 1, high + 1] {
                assert!(
                    parse(
                        &[flag, &value.to_string()],
                        Some(target("managed_identity"))
                    )
                    .is_err()
                );
            }
            for value in [low, high] {
                let mut flags = vec![flag.to_owned(), value.to_string()];
                if flag != "--reader-poll-ms" {
                    flags.extend(["--reader-poll-ms".into(), "100".into()]);
                }
                assert!(
                    Options::parse(
                        flags,
                        Some("server".into()),
                        Some(target("managed_identity"))
                    )
                    .is_ok()
                );
            }
        }
        for flags in [
            vec!["--unknown", "1"],
            vec!["--rate", "1", "--rate", "2"],
            vec!["--rate"],
            vec!["--rate", "bad"],
            vec!["--reader-poll-ms", "1000", "--staleness-bound-ms", "1000"],
            vec!["--work-dir", "relative"],
        ] {
            assert!(parse(&flags, Some(target("managed_identity"))).is_err());
        }
    }

    #[test]
    fn shared_key_auth_rules() {
        assert!(
            parse(
                &["--shared-key-file", "secret"],
                Some(target("managed_identity"))
            )
            .is_err()
        );
        assert!(parse(&[], Some(target("emulator_shared_key"))).is_err());
        let mut emulator: Value = serde_json::from_str(&target("emulator_shared_key")).unwrap();
        emulator["endpoint"] = json!("http://127.0.0.1:10000/devstoreaccount1");
        assert!(parse(&["--shared-key-file", "secret"], Some(emulator.to_string())).is_ok());
        emulator["endpoint"] = json!("https://example.com");
        assert!(parse(&["--shared-key-file", "secret"], Some(emulator.to_string())).is_err());
    }

    #[test]
    fn nearest_rank_summary() {
        assert_eq!(summary(&[]), Value::Null);
        let one = summary(&[7.0]);
        assert_eq!(one["count"], 1);
        assert_eq!(one["p99"], 7.0);
        let two = summary(&[9.0, 1.0]);
        assert_eq!(two["p50"], 1.0);
        assert_eq!(two["p90"], 9.0);
        assert_eq!(two["mean"], 5.0);
        let hundred = summary(&(1..=100).map(f64::from).collect::<Vec<_>>());
        for percentile in [50, 90, 95, 99] {
            assert_eq!(hundred[format!("p{percentile}")], f64::from(percentile));
        }
        assert_eq!(hundred["min"], 1.0);
        assert_eq!(hundred["max"], 100.0);
    }

    #[test]
    fn records_jump_matches_multiple_commits_and_sequence_regressions() {
        let mut state = Measurements::default();
        for i in 0..4 {
            state.commits.push(Commit {
                sent_ms: f64::from(i),
                ack_ms: Some(f64::from(i) + 1.0),
                visible_ms: None,
            });
        }
        state.observe(3, 10, 20.0);
        state.observe(4, 9, 25.0);
        assert_eq!(state.regressions, 1);
        assert_eq!(state.commits[0].visible_ms, Some(20.0));
        assert_eq!(state.commits[2].visible_ms, Some(20.0));
        assert_eq!(state.commits[3].visible_ms, Some(25.0));
        state.observe(4, 11, 30.0);
        assert_eq!(state.commits[0].visible_ms, Some(20.0));
    }

    #[test]
    fn status_keys_and_authority_error_codes() {
        assert_eq!(status_key(200, &json!({})), "200");
        assert_eq!(
            status_key(503, &json!({"error":"reader_stale: details"})),
            "503:reader_stale"
        );
        assert_eq!(
            authority_error(br#"{"error":"writer_authority_missing: no record"}"#),
            "writer_authority_missing"
        );
        assert_eq!(
            authority_error(br#"{"error":"writer_authority_unavailable: forbidden"}"#),
            "writer_authority_unavailable"
        );
        assert_eq!(
            authority_error(
                b"{\"event\":\"authority_command\",\"outcome\":\"writer_authority_missing\"}\n\
                {\"error\":\"writer_authority_missing: no record\"}\n"
            ),
            "writer_authority_missing"
        );
        for malformed in [b"not json".as_slice(), b"{}", br#"{"error":"secret text"}"#] {
            assert_eq!(authority_error(malformed), "malformed_authority_error");
        }
    }

    #[test]
    fn prefix_suffix_and_safe_config() {
        let run = "0123456789abcdef";
        assert_eq!(run_prefix("", run).unwrap(), format!("{run}/"));
        assert_eq!(run_prefix("base/", run).unwrap(), format!("base/{run}/"));
        assert_eq!(run_prefix("base", run).unwrap(), format!("base/{run}/"));
        for invalid in ["/base", "a//b", "..", "UPPER"] {
            assert!(run_prefix(invalid, run).is_err());
        }
        assert!(run_prefix(&"a".repeat(184), run).is_err());
        let mut value: Value = serde_json::from_str(&target("managed_identity")).unwrap();
        value["client_id"] = json!("01234567-89ab-cdef-0123-456789abcdef");
        let options = parse(&[], Some(value.to_string())).unwrap();
        let config = options.safe_config(Path::new("work"), "base/run/");
        let serialized = config.to_string();
        for secret in [
            "01234567-89ab-cdef-0123-456789abcdef",
            "client_id",
            "token",
            "account_key",
        ] {
            assert!(!serialized.contains(secret));
        }
        assert_eq!(config["auth"], "managed_identity");
        let directory = tempfile::tempdir().unwrap();
        let key_file = directory.path().join("emulator.key");
        let key = "test-key-content-not-for-output";
        fs::write(&key_file, key).unwrap();
        let mut emulator: Value = serde_json::from_str(&target("emulator_shared_key")).unwrap();
        emulator["endpoint"] = json!("http://127.0.0.1:10000/devstoreaccount1");
        let options = parse(
            &["--shared-key-file", key_file.to_str().unwrap()],
            Some(emulator.to_string()),
        )
        .unwrap();
        let serialized = options
            .safe_config(Path::new("work"), "base/run/")
            .to_string();
        assert!(!serialized.contains(key));
        assert!(!serialized.contains("test-reader-token-not-for-output"));
    }
}
