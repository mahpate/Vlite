# Executable gateway measurement contract

These are opt-in proposal-evidence tools, **not qualification/deployment gates**.
No automatic failover, deployment approval, production S_ctl, capacity limit or
CPU-only overhead follows from their output. `writer_only` remains unchanged.
No tool provisions Azure resources. The following PowerShell harness owns
temporary resource creation, role-specific secret projections, replica inventories,
clock calibration, evidence transport and deletion. Do not invoke `deploy.ps1`
or weaken its qualification checks to run these tools.

## Binaries, entrypoints and output

Native build (no new dependencies; Cargo.lock must remain unchanged):

```powershell
cargo build --locked --release --features server --bin vlite-server --example ha-meas004 --example ha-meas005
$Server = (Resolve-Path .\target\release\vlite-server.exe).Path
$Load = (Resolve-Path .\target\release\examples\ha-meas004.exe).Path
$Control = (Resolve-Path .\target\release\examples\ha-meas005.exe).Path
```

Linux container build: `docker build --target gateway-measure -t vlite-gateway-measure .`.
That **separate** target contains `/usr/local/bin/vlite-server`,
`/usr/local/bin/ha-meas004`, `/usr/local/bin/ha-meas005`, `python3` and
`/usr/local/bin/measure-gateway-entrypoint.py`, runs as UID/GID 10001,
and has writable work directory `/measurement` (0700).
It does not change the production `api` target. Docker COPY/context allowlists
include all three benchmark sources. No examples are installed in the normal
API image.

Every invocation requires a file, never inline secret arguments:

```powershell
& $Server gateway-measure bootstrap --config $BootstrapConfig
& $Server gateway-measure mint --gateway-config $GatewayConfig --token-file $TokenFile --output $NewPrivateEnvelopeFile
& $Load --config $LoadConfig
& $Control --config $ControlConfig
```

Success prints exactly one **compact JSON** line to stdout. Capture stdout and
stderr separately, never combine them. Errors contain static codes, not native
HTTP/Blob errors, credentials, signing seeds, envelopes, endpoints or config
contents. Bootstrap's local API may also log request metadata to stderr (not
tokens). Exit 1 means invalid input/operational error; stdout is normally empty.
MEAS-005 exit 2 preserves one JSON line of **incomplete evidence** plus static
`measurement_incomplete` on stderr: retain that JSON and do not call it a pass.
Example errors have shape `{"error":{"code":"CODE"}}`; the server helper follows
the existing server-tool shape `{"error":"CODE: detail"}` (usage detail is static).
Exit 0 for the mutation stage does not mean replica propagation was measured:
its `complete` is deliberately false until analysis. HTTP refusal/overload,
unsent work and timeouts in a completed load window are observations, not a
reason to discard that window or retry it.

Set these **on driver jobs and every observer app** from the source build:

* `VLITE_RESOURCE_SOURCE_REVISION`: source revision.
* `VLITE_RESOURCE_SOURCE_DIRTY_PATHS`: newline-separated dirty repository paths;
  empty string means clean. Missing/oversized provenance becomes JSON null,
  never an invented clean revision.

Configs are UTF-8 JSON, at most 65536 bytes, with unknown fields refused.
Use absolute paths to existing mounted files, no shell expansion in JSON.
The JSON below uses **container paths**. Bootstrap writes absolute paths into
`gateway.json`, `trust.json` and the manifest, so the one real bootstrap runs on a
Linux/WSL operator host at the container path `/measurement/bootstrap` (see
"Exact shared fixture" below). Windows-path bootstrap output is not
distributable to the containers.
Opaque `requested` label maps accept at most 32 entries with 1..128 characters
from `[A-Za-z0-9._-]` in keys and values. Put only nonsecret resource/settings
labels there, never tokens or resource URLs. Endpoint origins accept only HTTP(S),
no userinfo, query, fragment or non-root path. TLS verification stays enabled,
HTTP redirects/proxies/retries are disabled in load/probe clients.

## 1. Offline fixture/bootstrap

Create a new private parent directory owned by the measurement user (for a
distributable run: `/measurement` on the Linux operator host). Unix staging
directories are 0700, explicit secret outputs 0600. A Windows bootstrap is only
for local smoke and must restrict its ACL first. The output directory **must not exist**. Partial
bootstrap contents are removed on failure. Existing directories/files are not
replaced. The successful directory persists for the harness to distribute and
must be deleted in harness `finally`.

`bootstrap-config.json`:

```json
{
  "output_dir": "/measurement/bootstrap",
  "control_state": {
    "target_file": "/measurement/control-target.json",
    "object": "gateway/control.json"
  },
  "writer_url": "https://WRITER-INTERNAL-FQDN",
  "audience": "measurement-writer",
  "poll_interval_ms": 1000,
  "max_replicas": 2,
  "revocations": 10
}
```

`poll_interval_ms` defaults to 1000 (1..5000); `max_replicas` to 2 (1..64);
`revocations` to 10 (1..100). The wrapper's local `/ready` observer interval is
independent. An optional `control_state.shared_key_file` is only for loopback
Azurite; omit it for managed identity. Bootstrap is **offline**: it does not
connect to Blob, create containers, publish control or initialize authority.
`control_state.target_file` must resolve identically in every consuming process.

Example managed-identity `control-target.json` (existing target-file schema):

```json
{
  "account": "TEMPORARYACCOUNT",
  "container": "control-data",
  "coordination_container": "control-coordination",
  "prefix": "isolated-run/",
  "auth": "managed_identity"
}
```

For local smoke only, use `"auth":"emulator_shared_key"`, add
`"endpoint":"http://127.0.0.1:10000/devstoreaccount1"` and
`control_state.shared_key_file` pointing to the emulator's private key file.
Both containers must already exist. Never pass an Azure account key/SAS.

Generated artifacts:

| File | Consumer / exact content |
|---|---|
| `database.vlite` | Synthetic v4, dimensions 2; two isolated tenants each have `probe`, vector ID 1, vector `[1,0]`. Copy to each backend's **private writable** directory before starting it; never run two servers on one file. |
| `admin/` | Temporary issuance/audit/control CLI directory; driver only. Not a serving admin directory. |
| `backend.token` | Random startup token; backend `--token-file` only, not a data-plane auth bypass. |
| `gateway.json` | Ready-to-run gateway config referencing the final bootstrap paths, bind `0.0.0.0:8080`, S_ctl 30000 ms, configured polling/max replicas, 60000 ms envelopes, 4096 in flight, 5000 ms upstream timeout. |
| `trust.json` | Ready-to-run backend trust, same audience/control/public bundle, max envelope lifetime 60000 ms, skew 1000 ms, S_ctl 30000 ms. |
| `signing.key`, `public.json` | Normal production-format generated Ed25519 seed/bundle, key ID `measurement`, bundle generation 1. Only gateway and signing driver get the seed. |
| `directory.json` | One route per tenant; dedicated, `authority_mode:"off"`, no epoch, `writer_only`, same synthetic database lineage. |
| `load-0.token` | Unaffected tenant credential used for load and behavioral control. |
| `load-1.token` .. `load-10.token` | Sacrificial revocation credentials (count configurable). |
| `suspension-0.token` | Separate tenant's suspend/resume credential. |
| `load.expected.json`, `suspension.expected.json` | Complete expected JSON bodies for `GET /v1/collections/probe/vectors/1`, generated through the real scoped router. |
| `manifest.json` | Private shape below. It contains paths/IDs, never plaintext bearer values. |

```json
{
  "schema": "vlite-gateway-measure-bootstrap/1",
  "lineage": 123,
  "writer_url": "https://WRITER-INTERNAL-FQDN",
  "path": "/v1/collections/probe/vectors/1",
  "credentials": [
    {"purpose":"load","id":"GENERATED","tenant":"GENERATED","token_file":"/measurement/bootstrap/load-0.token"},
    {"purpose":"revocation","id":"GENERATED","tenant":"GENERATED","token_file":"/measurement/bootstrap/load-1.token"},
    {"purpose":"suspension","id":"GENERATED","tenant":"GENERATED","token_file":"/measurement/bootstrap/suspension-0.token"}
  ]
}
```

All fixture credentials expire two hours after issuance and have unlimited RPS/
vector quotas, collections `["*"]`, operations `read,write,collections`. The latter
two are used to create the fixture, not in the timed GET workload. Start the
measurement promptly; do not extend stale credentials silently. The generated
route's **explicit authority-off synthetic topology** measures no authority,
epoch fencing, failover, archive/apply, reader freshness or durable writer
restart. It does not relax any deployment gate. A harness using real authority
must provision it with the existing authority tools and replace the directory
before publication; preserve that difference in requested labels.

### Exact shared fixture (Linux operator host)

Every role must use the **same** bootstrap result: role bundles carry the files,
and backends carry that result's own `database.vlite`. A separately bootstrapped
database has different tenant IDs and lineage and is invalid. Prepare one run
on Linux/WSL with Python 3, the source-revision Linux `vlite-server`, and an
**empty** `/measurement` (0700, owned by the operator, not a symlink):

```sh
export VLITE_RESOURCE_SOURCE_REVISION=... VLITE_RESOURCE_SOURCE_DIRTY_PATHS=...
python3 deploy/azure-container-apps/ha/measure-gateway-fixture.py prepare --plan /private/plan.json
python3 deploy/azure-container-apps/ha/measure-gateway-fixture.py cleanup
```

The plan names `server`, `writer_url` (the run's internal writer origin, so the
writer app environment must exist first), `audience`, `control_target`
(account/containers/prefix; written as managed identity, no `client_id`),
`control_object`, `generation`, `registry`, `base_image`
(`REGISTRY.azurecr.io/REPO@sha256:DIGEST` of the `gateway-measure` image) and
`fixture_repository`. `prepare` then does the following:

1. It runs the entrypoint `bootstrap` once, at `/measurement/bootstrap`, with
   `/measurement/control-target.json`. It never retries.
2. It exports `driver`, `gateway`, `writer` and `reader` to
   `/measurement/exports/ROLE.json`.
3. It decodes each export and checks that it matches the bootstrap files byte
   for byte: exactly the role-allowed set, with no `database.vlite`. It also
   checks the schema, role, generation, source, the `bootstrap_dir`, and the
   receipt's digest and size. Finally it checks that the `measurement.json`
   fixture digest and lineage match the bootstrap fixture and manifest.
4. It refuses a fixture that contains any generated token or signing seed.
5. It writes a private image context, `/measurement/image/`, containing only
   `database.vlite` (digest-verified) and this Dockerfile:
   `FROM <base_image>` plus `COPY --chown=10001:10001 --chmod=0400 database.vlite
   /opt/vlite-measure/fixture/database.vlite`.

Its stdout is one public JSON summary. The summary holds the fixture digest,
the export digests and paths, the conservative credential deadline (bootstrap
start plus 2 h) and a **planned, not executed** `az acr build` into the run's
private ACR, tagged `GENERATION-DIGEST12`. Any failure prints only
`VLITE-MEAS FAILED 1` and a static code, then removes everything under
`/measurement`. A non-empty root is refused and left untouched.

Distribute the parts as follows:

- Each role's projection goes only to that role's ACA secrets. Each role then
  runs `restore` to `/measurement/bootstrap`.
- Each role gets its own `control-target.json`, with its own `client_id`.
- Writer and reader run `copy_fixture` from
  `/opt/vlite-measure/fixture/database.vlite`, with `fixture_sha256` set to the
  summary digest.
- `serve` independently rechecks that digest against the restored
  `measurement.json`.

Credentials expire 2 h after issuance, so the build, the secrets, publish and
the whole trial must finish before `credentials_expire_not_before_unix_ms`. Do
not reuse a run. Run `cleanup` once secrets are written, and again in harness
`finally`. Delete the fixture image with the run's ACR/resource group. Never
push it to a public registry or copy it into evidence.

Publish exactly once, **after** fixture/config distribution and **before** apps:

```powershell
& $Server control-state publish --target-file $Target --object gateway/control.json --admin-dir $AdminDir --directory-file $Directory
# Local Azurite only: append --shared-key-file $EmulatorKeyFile
```

Do not blindly republish/retry unknown writes. The existing conditional control
publication/refusal semantics are unchanged.

Backend app entrypoint: command `/usr/local/bin/vlite-server`; args
`["/measurement/writer/database.vlite","--serve-mode","writer","--durability","full","--bind","0.0.0.0:8080","--allow-remote","--token-file","/measurement/bootstrap/backend.token","--gateway-trust-file","/measurement/bootstrap/trust.json"]`.
No `VLITE_ADMIN_DIR`, no `--insecure-local`, no admin token in gateway requests.
A control-only reader observer may instead own an existing private fixture copy
with `--serve-mode reader` and **without** `--durability`; that is not reader
archive/apply evidence. Gateways/backends get read-only control permissions;
only the temporary mutation driver gets control write rights and `admin/`.

Gateway app entrypoint: command `/usr/local/bin/vlite-server`; args
`["gateway","--config","/measurement/bootstrap/gateway.json"]`.
The helper does not create an alternate serving endpoint or replica header.
Direct requests still require a correctly signed audience/lineage/scoped
principal; a tenant bearer alone cannot authenticate a trusted backend.

`gateway-measure mint` authenticates the real bearer against a **fresh** shared
snapshot and reuses the gateway signer/mint logic. Its output file must be new;
stdout contains only generation/expiry, not the envelope. Lifetimes are
`min(gateway.envelope_lifetime_ms,60000)`, and credential expiry must cover them.
The load driver keeps envelopes only in memory and refreshes signatures outside
HTTP timers without changing its initial generation or rereading control.
Do not mutate control or rotate keys during MEAS-004.

## 2. MEAS-004 load job

Job command `/usr/local/bin/ha-meas004`, args
`["--config","/measurement/meas004.json"]`; execution retries **zero**, parallelism
one, timeout 3600 seconds. `meas004.json`:

```json
{
  "gateway_config": "/measurement/bootstrap/gateway.json",
  "token_file": "/measurement/bootstrap/load-0.token",
  "expected_body_file": "/measurement/bootstrap/load.expected.json",
  "gateway_url": "https://GATEWAY-FQDN",
  "direct_url": "https://WRITER-INTERNAL-FQDN",
  "rates": [50,200,800,1600],
  "duration_secs": 180,
  "warmup_secs": 30,
  "recovery_secs": 180,
  "max_in_flight": 256,
  "timeout_ms": 5000,
  "requested": {"gateway_cpu":"1","gateway_memory":"2Gi","runtime_max_replicas":"2","routing":"writer_only","trial":"fixed-two"}
}
```

All numeric fields shown are proposed full defaults, **not measured settings**.
Pilot overrides: `rates:[5]`, `duration_secs:1`, `warmup_secs:0`,
`recovery_secs:0`, `max_in_flight:4`, `timeout_ms:1000`.
Bounds: 1..4 distinct rates, each 1..10000 RPS; duration 1..180 s; warmup 0..30 s;
recovery 0..180 s; in flight 1..4096; timeout 10..10000 ms. Combined scheduled
requests <=3000000 and configured schedule + maximum drains <=3540 s.
Defaults occupy at most 3425 s plus startup/preflight/local work, so use a
small pilot before any expensive full run. There is no automatic rate search.

Each rate runs A=direct, B=gateway, B=gateway, A=direct. The first A and first B
each get a separate counted warmup phase; both independent HTTP/1 pools persist.
The final recovery runs gateway at the first offered rate. GET preflight
requires HTTP 200 and complete semantic JSON equality on **both** paths.
The direct origin must match the authenticated control-directory route.
Responses are consumed completely up to 65536 bytes; invalid/truncated/oversize
bodies are explicit errors. No request is retried.

Absolute monotonic open-loop slots do not wait for responses. A slot one period
late is counted `unsent_late`, not caught up in a burst; a full semaphore is
`unsent_capacity`. Every scheduled slot is started or in exactly one unsent
bucket. Every started task is joined and counted; all response statuses,
transport/body errors, semantic mismatches and schedule lateness are retained.
Schedule lateness is measured when the async request task actually begins, not
just when it is queued. Successful latency distributions include **only** HTTP 200, valid semantically
matching bodies, using nearest rank; zero samples means null percentiles.
Throughput fields divide by offered-window duration; the separately recorded
`elapsed_with_drain_secs` makes timeout drains explicit.

Schema `vlite-meas004/1` includes qualification `proposal_evidence_only`, source,
requested settings/opaque resource labels, limits, uncertainty, per-window
counts/distributions and ABBA deltas, including two paired-window mean/median
differences and their small-sample variability summaries. A difference of window percentiles is
**not** a per-request overhead percentile. This workload includes an extra
ingress/network hop; it does not isolate cryptographic CPU. It is one synthetic
GET class, not a representative search mix.
`authenticated_policy` records the actual credential RPS/vector limits, expiry,
collection-scope kind, short envelope lifetime and gateway runtime replica maximum
read before the run. Tenant aggregate quota is explicitly not exported.

For scale pilots, the harness configures the separately declared HTTP concurrency
threshold (proposed sweep 10/25/50/100), fixed min 2/max 4 and matching runtime
`max_replicas:4`, with the same driver. It must collect independently timestamped
replica inventories/readiness outside this tool. `scale_out` stays **unknown**,
`replica_seconds` null: the driver neither invents a replica header nor infers
all replicas/capacity from ingress samples. Do not label non-triggered or
backend/driver-saturated profiles scale successes. The fixed-two overhead trial
uses runtime/platform max 2, not 4.

## 3. MEAS-005 process observers

Each gateway/backend app must use the **observer as its entrypoint**, not start
a second unrelated sidecar and call that process coverage. Command
`/usr/local/bin/ha-meas005`, args `["--config","/measurement/observe.json"]`.
One observer owns exactly one serving child for a finite window, polls only its
own loopback `/ready`, and kills/reaps that exact child at completion/error or
handled termination. Child output is discarded, never copied into evidence.
This intentionally bounded disposable supervisor is not a production launcher
or a durability/restart test. Do not let an app replacement silently resume the
same trial. SIGINT/SIGTERM (Windows Ctrl+C/Ctrl+Break) produces interrupted
evidence; abrupt host kill cannot guarantee an observer report and means
missing/incomplete coverage.

Gateway `observe.json`:

```json
{
  "mode": "observe",
  "server": {
    "program": "/usr/local/bin/vlite-server",
    "args": ["gateway","--config","/measurement/bootstrap/gateway.json"]
  },
  "ready_url": "http://127.0.0.1:8080/ready",
  "role": "gateway",
  "interval_ms": 50,
  "duration_secs": 1200,
  "timeout_ms": 1000
}
```

Backend: same shape, `role:"backend"` and `server.args` exactly the backend
argument array above. Writer and reader are both backend verifiers. No remote
readiness URL is allowed. Bounds: interval 10..5000 ms, duration 1..3540 s,
request timeout 10..1000 ms, 2048 transition records, 65536 bytes per body.
Child program must be absolute; <=64 args, <=2048 characters each.
For local smoke **only**, add `"local_replica":"local-gateway-1"` or another
unique label and choose e.g. duration 20 s. On ACA this field is forbidden:
actual `CONTAINER_APP_REPLICA_NAME` is mandatory, and `CONTAINER_APP_NAME` /
`CONTAINER_APP_REVISION` are retained when present. Missing identity is an error,
not a randomly invented replica name.

One compact `vlite-meas005-observer/1` report is printed at completion. It contains
actual replica/role/app/revision, unique observer boot ID, owned-child lifecycle,
UTC and monotonic send/receive brackets at **state changes**, lifetime,
readiness poll/failure counts, actual maximum sample-start gap and observed clock
jumps. Backend transitions include **actual generation AND minimum_generation**.
Unchanged samples update the last-old-ready bound in memory but are not logged.
Install time is conservatively bounded by the previous successful readiness
sample's start and the first matching new sample's end, not just HTTP latency.
Readiness polls are not Blob GETs. Logical feed polls and physical Blob requests
are explicitly null; only the code-derived one-full-snapshot-logical-GET per feed
poll plus startup model is supplied. Adapter retries/identity traffic/billed
transactions remain unknown. Observer overhead is inside each app's allocation.

Collect every observer report by **replica-specific** console/file transport;
do not use an ingress request or only revision-level log sampling as coverage.
These reports can exceed a console-line limit. The harness must redirect to
private files and transport them using its own bounded framed encoding/checksum,
or otherwise verify complete single-report retrieval. Do not truncate and parse
the tail as valid evidence. A valid report is <=4 MiB. Platform inventory before
and after must establish the same replica/revision set, no replacement boots.

## 4. MEAS-005 mutation job

Wait for the declared fixed set (proposal: two gateways, writer and reader) to
be ready, collect before-inventory and clock samples, then run one job, zero
retries, command `/usr/local/bin/ha-meas005`,
args `["--config","/measurement/meas005-run.json"]`, timeout 3600 s:

```json
{
  "mode": "run",
  "server": "/usr/local/bin/vlite-server",
  "bootstrap_dir": "/measurement/bootstrap",
  "target_file": "/measurement/control-target.json",
  "object": "gateway/control.json",
  "gateway_url": "https://GATEWAY-FQDN",
  "poll_interval_ms": 1000,
  "observation_deadline_ms": 30000,
  "mutations": 20,
  "seed": 1,
  "requested": {"gateway_replicas":"2","writer_replicas":"1","reader_replicas":"1","gateway_cpu":"1","gateway_memory":"2Gi","writer_cpu":"1","writer_memory":"2Gi","reader_cpu":"0.5","reader_memory":"1Gi","driver_cpu":"1","driver_memory":"2Gi","routing":"writer_only","load":"idle"}
}
```

Optional `shared_key_file` is again loopback Azurite only. Bounds: poll 1..5000 ms,
deadline 100..30000 ms, mutations 3..100, seed any u64. Worst-case mutation
command time (120000 ms each) + envelope preparation (15000 ms each) +
observation windows + at most 3000 ms final behavioral-request overrun per
mutation + maximum jitter must be
<=3540000 ms. Bootstrap must supply at least floor(mutations/2) sacrificial
credentials. Pilot: bootstrap revocations 1, poll 100 ms; mutations 3,
observation deadline 1000 ms, observer duration at least 20 s.
The full proposed idle sweep is a **separate fresh trial per interval**:
100, 250, 500, 1000, 2000, 5000 ms. Set bootstrap polling and this run's requested
poll/jitter interval identically for each trial, retaining S_ctl 30000 ms and
the 50 ms observer. Resource labels above are proposals, not measured entitlement.

The first floor(N/2) mutations revoke distinct sacrificial credentials, followed
by alternating suspension/resumption on the separate tenant. Seeded SplitMix64
jitter is uniform modulo in `[0,poll_interval_ms]` and occurs before each
mutation. There is one control writer and no adaptive retry.
Before each mutation, a separately bounded `gateway-measure mint` child
authenticates against the fresh shared state. Its private envelope file and
captured command output are deleted after the iteration. The direct behavioral
target comes from the private bootstrap manifest's `writer_url`; do not reroute
the fixture mid-trial. Preparation failure stops before attempting that mutation,
with explicit incomplete evidence. Each existing `control-state update` child is bracketed as **command_bracket**
(includes command startup, file/audit work, CAS retries and acknowledgement).
Unknown/nonzero/timed-out writes stop the sequence as
`unknown_do_not_retry`; retain evidence and reconcile read-only externally.
Successful generations must advance by exactly one from the prior authenticated
snapshot/command; a competing writer makes the mutation stage incomplete.

Behavior checks, separately labelled **ingress behavior only**, require
revocation HTTP 401, suspension HTTP 403 with `tenant_suspended`, and resumption
HTTP 200 with the expected semantic body. Every operation also requires the
unaffected tenant's expected HTTP 200 body and a pre-mutation direct envelope's
HTTP 401 `envelope_stale`. Expired/malformed envelopes do not count as stale-floor
enforcement. Retain all check statuses/errors/counts. Successful behavior does
not end a replica window early: the driver holds the full declared observation
window after each command. No mutator-generated per-replica latency is fabricated.
Failure of a behavioral check or detection of a competing generation writer
stops the remaining mutation sequence and preserves the partial records.

The `vlite-meas005/1` mutation report contains requested settings, source, limits,
uncertainty, generation/whole-command UTC+monotonic brackets, jitter, bounded
behavior records and `mutation_and_ingress_checks_complete`. `complete:false`
is intentional until observer analysis.

## 5. Clock samples and offline MEAS-005 analysis

`clock.json` is exactly `{"mode":"clock_sample"}`. Invoke the same binary:
`& $Control --config $ClockConfig`.
Run it in the mutator job/host and via **replica-specific exec** on each observer
replica before and after each trial. It returns `vlite-meas005-clock/1` with
remote UTC start/end and monotonic elapsed. Bracket remote exec locally.
Derive conservative **observer UTC minus mutator UTC** offset intervals, including
both command RTT brackets, their reference-host calibration, rounding, drift and
any before/after disagreement. The implementation does not claim that ACA exec,
NTP, or cross-host drift has a prevalidated precision. If no defensible bound is
available, use nulls, preserve generation/floor coverage, and expect incomplete
timing. Do not estimate host offset from load-balanced HTTP RTT.

After observers finish, collect all reports, after-inventory and calibration.
`analyze.json`:

```json
{
  "mode": "analyze",
  "mutation_evidence_file": "/measurement/evidence/mutations.json",
  "fixed_inventory_confirmed": true,
  "replicas": [
    {
      "replica": "ACTUAL-GATEWAY-REPLICA-NAME",
      "role": "gateway",
      "report_file": "/measurement/evidence/gateway-1.json",
      "clock_offset_ms": null,
      "clock_uncertainty_ms": null
    },
    {
      "replica": "ACTUAL-WRITER-REPLICA-NAME",
      "role": "backend",
      "report_file": "/measurement/evidence/writer.json",
      "clock_offset_ms": null,
      "clock_uncertainty_ms": null
    }
  ]
}
```

The real four-process trial must list **both** gateways, writer **and** reader,
not just the illustrative two entries. Maximum 64 distinct replica entries;
each report <=4 MiB. Identity/role/source must match, every expected boot must
own its child without early termination/interruption, and at least one gateway
and backend must be present. `fixed_inventory_confirmed:true` is the harness's
explicit assertion that the list is exhaustive, not the analyzer discovering
all replicas. Never set it from ingress sampling.

Invoke `& $Control --config $AnalyzeConfig`. Calibration values must be finite,
uncertainty 0..60000 ms. The analyzer additionally expands bounds by observed
observer clock drift plus a declared 10 ms mutator drift allowance. A >10 ms
observer clock jump or command wall/monotonic mismatch makes timing incomplete.
It requires observed lifetime to conservatively cover each whole command and
the entire observation deadline (`command_end + observation_deadline_ms`),
even when the target is observed earlier; the target generation exactly,
backend floor >= target, and first
observation by the declared deadline. A skipped target generation is not a
successful sample. For calibrated brackets:

`lower=max(0, last_old_ready_send - command_end - clock_uncertainty)`

`upper=max(0, first_new_ready_receive - command_start + clock_uncertainty)`

Analysis also requires non-null matching source provenance and the requested
mutation count. Observation before command acknowledgement is legitimate, not negative latency.
Missing clocks, boot/lifetime/identity gaps, changed inventory, missing generation
or floor and deadline misses stay incomplete/censored/unknown, never zero.
Analysis distributions summarize **successful bounded samples only**; they
include count, min, nearest-rank p50/p95/p99, max and mean, separately for lower
and upper bounds. Empty distributions have null values. Preserve individual
samples and missing counts; 20 observations are a pilot, not strong p99 evidence.

## Cleanup and harness obligations

Use fresh isolated temporary resources, reject protected/preexisting resources,
preserve ownership/expiry tags and role-specific managed identity rights.
Never mount driver admin/signing files into backend apps. Never copy credentials,
native diagnostics or private configs into evidence. Neither Rust driver retries
requests, mutates scale policy, provisions resources nor deletes resource groups.
The observer reaps only its own child; temporary command outputs are removed by
RAII. The harness must always remove private bootstrap/token/envelope/observer
files and request resource-group deletion in `finally`, await confirmed absence,
and record deletion failure separately from measurement failure. No keep-on-fail
exception is part of this interface. A restart, expiry, lost report or failed
deletion cannot become qualification or a success-shaped zero.
