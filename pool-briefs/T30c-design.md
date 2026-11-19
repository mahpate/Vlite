# T30c: temporary-ACA design for MEAS-004 and MEAS-005

**Status: INCOMPLETE for runtime/ACA validation; repository-only design delivered.**
No MEAS-004/005 measurement was run, no number below is a measured result, and no
SLO, replica limit, S_ctl, deployment approval or qualification gate is established.
All numeric experimental settings below are **proposals, unmeasured** unless
explicitly identified as an existing code constraint or a calculated quantity.

Inspected checkout: `3059f397d6a759c173e1c4142d5c04136bc6dc08`.
This assignment permits repository reads and one report only; no Azure or other
network calls were made. T30a/T30b are contracts for concurrent work, not evidence
that their implementations are present or have passed.

## 1. Conclusions and evidence boundary

1. **MEAS-005 does not need a new generation endpoint.** Gateway `/ready` already
   returns `generation`; trusted-backend `/ready` returns both `generation` and
   `minimum_generation`. The actual gaps are identifying every ACA replica,
   observing it independently of ingress balancing, bracketing the successful CAS,
   clock uncertainty, and distinguishing logical polls from physical Blob requests.
   Evidence: `src/server/gateway_mode.rs:362-386`,
   `src/server/gateway_trust.rs:268-275`,
   `src/server/control_state.rs:649-663,666-677,970-983`.
2. **MEAS-004 must compare two authenticated paths to the same backend.** A direct
   bearer is not a valid baseline for a trusted backend: it authenticates a
   principal envelope, while the gateway accepts a tenant bearer and mints that
   envelope. Reuse the private signer/mint helpers in a measurement-only server
   command rather than bypass backend auth or duplicate the signature format.
   Evidence: `src/server/gateway_trust.rs:222-254`,
   `src/server/gateway_mode.rs:415-425`,
   `src/server/gateway.rs:470-492`,
   `src/server/principal_envelope.rs:92-113`.
3. **Do not call the HA deploy script as a shortcut.** It unconditionally throws
   at its qualification verifier. Reuse its pure templates/configuration patterns
   within a separately validated, disposable measurement path; do not remove or
   override the verifier outside its existing recording-mock tests.
   Evidence: `deploy/azure-container-apps/ha/deploy.ps1:319-325`,
   `deploy/azure-container-apps/ha/templates.ps1:76-112,363-409`,
   `deploy/azure-container-apps/ha/test.ps1:637-641`.
4. **Archive fault paths already exist locally.** MEAS-003 is not blocked by a
   complete absence of fault injection. There are test-only fake-store faults,
   a process-level partial-transfer proxy, an interrupted-archive drill and a
   failover loss-report drill. What is missing is an approved, managed-identity,
   real-Blob fault mechanism and an evidence-producing temporary-ACA measurement
   driver. Evidence: `src/server/cloud_backup.rs:2191-2226`,
   `tests/ha_chaos_azurite.rs:102-117,1213-1275,1862-1937`.

The PRD makes MEAS-004 input to OD-006 and MEAS-005 input to OD-005, requires
unmeasured settings to be labelled, and explicitly separates proposal evidence
from production approval (`docs/projects/feature-aca-ha/feature-aca-ha.prd.md:318-319,409-410,482,517-518`).
ITEM-033 authorizes separate temporary tagged resources and sequences MEAS-003
after archive fault paths, not after a made-up result
(`docs/projects/feature-aca-ha/feature-aca-ha.prd.md:903`).
Keep `writer_only`, dashboard work and EPIC-010 out of this implementation.
Writer-only is the current template restriction
(`deploy/azure-container-apps/ha/templates.ps1:291-292`).

### Source conflicts to preserve, not silently resolve

The test plan's older remaining-gaps text says no epoch stamp/check exists
(`docs/projects/feature-aca-ha/gateway-test-plan.md:152-154`).
Current code checks `x-vlite-epoch` and the real-process test explicitly includes
an authority-backed writer (`src/server/gateway.rs:565-566`,
`src/server/gateway_mode.rs:337-359`, `tests/gateway_azurite.rs:14-18`).
Treat those particular older statements as stale, not as implementation gaps.
The same document's blanket MEAS-007-open sentence is older than ITEM-033's
recorded MEAS-007 development observations
(`docs/projects/feature-aca-ha/gateway-test-plan.md:151`,
`docs/projects/feature-aca-ha/feature-aca-ha.prd.md:903`).
Neither conflict turns MEAS-004/005 into executed measurements.

## 2. Shared temporary resource topology and safeguards

### Proposed topology

Use a **new resource group for each measurement/configuration/repetition**, with
fresh opaque run ID, ownership and expiry tags. Do not reuse a MEAS-002 group.
Within it:

| Resource | Proposed configuration and purpose |
|---|---|
| ACR | Basic registry, admin disabled; immutable recorded image digest, remote build or import. |
| ACA environment | New Consumption environment with its own Log Analytics workspace; same region for all resources. Zone behavior is an explicit run label, not inferred. |
| Storage account | New HTTPS-only account, public Blob access disabled; use Standard_ZRS for target-like control measurements if supported. Any Standard_LRS pilot is labelled non-target-tier. |
| Blob containers | Four isolated containers: `control-data`, `control-coordination`, `archive`, `archive-coordination`. A random per-run prefix further prevents accidental cross-run state, but is not RBAC isolation. |
| Gateway app | Actual ACA gateway app; MEAS-005 fixed two replicas; MEAS-004 fixed two for overhead, then min two/max four for scale-out. One active revision per trial, no sticky sessions. |
| Writer app | One fixed replica, trusted backend, v4 synthetic database, full durability, authority and archive enabled. Internal ingress only. |
| Reader app | One fixed trusted reader for MEAS-005, private follower directory and archive enrollment; not a load target. Omit for MEAS-004 to reduce interference. |
| Driver job | Manual ACA job in the **same environment**, one completion, one parallel replica, zero execution retries, timeout 3600 s. |
| Identities | Separate pull/driver, gateway, writer and reader identities. Driver alone can mutate temporary control state; gateway/backends read control. Archive/pin rights are role-specific and container-scoped. |

These are measurement proposals, not the existing harness's capabilities.
The current harness creates ACR/identity/environment/jobs only, and accepts only
MEAS-007 (`deploy/azure-container-apps/ha/measure.ps1:45-47,130-178`).
T30b proposes a separate MEAS-002 mode with StorageV2 Standard_LRS, two containers
and a job identity grant; it does not provision this gateway topology
(`pool-briefs/T30b.md:32-60`).

Template prior art: writer min=max=1, reader EmptyDir, gateway without a database
volume, role-specific secret projections, internal backends, disabled sticky
sessions, HTTP concurrency rule and explicit CPU/RAM pairs
(`deploy/azure-container-apps/ha/templates.ps1:380-411,414-426,453-525`).
The HA deploy path additionally requires preexisting infrastructure and currently
blocks rollout; it is not a resource provisioner for these runs
(`deploy/azure-container-apps/ha/README.md:3-25,46-76`,
`deploy/azure-container-apps/ha/deploy.ps1:319-325`).

**Temporary writer storage trade-off:** prefer private EmptyDir and a tiny
nonsecret v4 fixture generated into the measurement image by the proposed driver.
Both job and writer can read its lineage manifest. Copy that fixture into the
writer's fresh writable directory, initialize authority create-only from the
driver, publish the control directory, then start the writer. Never seed a
production path. A writer restart invalidates the run; stop and record it rather
than silently creating an empty replacement. This deliberately does **not** copy
the HA template's Azure Files choice and supplies no writer-restart, storage
durability or HA evidence. The template's actual AzureFile mount and writer flags
are at `deploy/azure-container-apps/ha/templates.ps1:395-403,462-470`.
T30a describes v4 creation, lineage capture, authority initialization and writer
archive startup to reuse (`pool-briefs/T30a.md:8-15,74-92`).
Do not put the database itself into an ACA secret; use secrets only for small
configuration/key files and respect measured platform limits.

**Assumptions requiring platform preflight:** selected region/SKU availability,
environment/replica quota, identity propagation, secret mount readability,
internal FQDN reachability from jobs, CLI JSON shape and replica-specific exec/log
support. No platform property was verified over the network in this task.

### Isolation, secrets and cleanup contract

- Refuse `vlite-app`, `vlite-env`, `vlite-data`, `asendme-rg`, production storage,
  any preexisting resource group, and any resource ID outside the newly created
  run's group/environment. Extend validation to every new app/container/account
  input; a naming prefix alone is insufficient ownership proof. Existing prior
  art refuses protected names and existing groups
  (`deploy/azure-container-apps/ha/measure.ps1:22-26,106-112,335-337`).
- Preserve tags `vliteMeasurement`, `vliteMeasurementOwner=ha-harness`,
  `vliteMeasurementExpires`; expiry is only an aid, not a deletion service
  (`deploy/azure-container-apps/ha/measure.ps1:130-132`,
  `deploy/azure-container-apps/ha/measure.md:102-107,138-140`).
- Mount immutable generation-suffixed native ACA secrets, not Key Vault. Admin
  files and seed exist only in the temporary driver's private directory/secret
  projection; serving gateway/backends get no admin token or admin directory.
  Existing role filtering and private manifest behavior are the pattern
  (`deploy/azure-container-apps/ha/templates.ps1:414-426`,
  `deploy/azure-container-apps/ha/README.md:123-171`).
- Use managed identity, never an account key or SAS. Gateway/backend read grants
  must not be widened merely because T30b's single job uses Contributor.
  Existing deployment target validation requires managed identity and separate
  control/archive containers
  (`deploy/azure-container-apps/ha/deploy.ps1:253-265`).
- Always request group deletion in `finally`, await confirmed boolean absence,
  and record measurement failure separately from deletion failure. Reject
  `KeepOnFailure` for these new modes to satisfy the owner's delete-afterward
  requirement; do not change legacy MEAS-007 behavior. The old harness has an
  explicit retention exception, so inheriting it unchanged would be insufficient
  (`deploy/azure-container-apps/ha/measure.ps1:407-435`).
- Preserve provenance, bounded framed gzip/base64 evidence, SHA-256 verification,
  and external empty/non-symlink OutDir. Native diagnostics must not be copied
  into evidence. Existing behavior:
  `deploy/azure-container-apps/ha/measure.ps1:84-104,310-343,389-400`,
  `deploy/azure-container-apps/ha/measure.dockerfile:27-43`,
  `deploy/azure-container-apps/ha/measure.md:77-100`.

No dollars are estimated. Cost drivers are Consumption app/job active and idle
vCPU-seconds and GiB-seconds, HTTP request meter where applicable, ACR Basic
registry time/storage and build compute, Log Analytics ingestion/retention,
Blob read/write/list operations and stored bytes, ZRS versus LRS storage, and
any applicable network transfer. These meter names are a budgeting checklist,
**not verified current Azure billing terms or prices**. Actual pricing, free
grants, regional rates and cross-zone charges remain assumptions pending an
owner-side pricing review. Keep build/setup/deletion wall time separate from
the job's 3600 s execution budget; existing build/deletion bounds are documented
at `deploy/azure-container-apps/ha/measure.md:95-107`.

## 3. MEAS-005: definition, observation and read-cost model

### Exact target and honest observable

The target is, for each successful mutation generation A and every expected
live process i:

`L_i(A) = time process i installs/enforces A - time the successful Blob CAS commits A`.

For a gateway, observation means its cached snapshot is A. For a trusted backend,
require **both** snapshot A and verifier floor at least A; a cache update alone
is not proof that the floor has advanced. The backend raises the floor after a
successful poll (`src/server/gateway_trust.rs:149-166`).
The CAS reads the old snapshot, increments A, and conditionally writes; conflicts
can cause bounded re-read/retry. Unknown write outcome is an explicit error
(`src/server/control_state.rs:666-677,695-718`).

Neither CLI return nor snapshot publication timestamp is the exact server-side
commit instant. Today the CLI captures `now` before the mutation and returns only
generation/object after it (`src/server/gateway_mode.rs:1097-1127`).
Therefore report **interval-censored commit-to-observation latency**, not an
invented exact timestamp:

- Instrument only the winning conditional PUT's local send/start and successful
  acknowledgement boundaries; the true commit is within that bracket.
- If observation time is also bracketed, report
  `[max(0, observe_lower - commit_upper), max(0, observe_upper - commit_lower)]`,
  after conservatively expanding both brackets for clock-offset uncertainty.
- A replica may observe the new object before the mutator receives its PUT
  acknowledgement. Do not discard those samples or call negative ack-relative
  duration a propagation failure.
- If precise instrumentation is not yet available, use whole-command start/end
  as the wider commit bracket. Label it `command_bracket`, including local file
  work/process overhead; never label it exact commit timing.
- On conflict, associate timestamps with the winning A only. On
  `control_state_indeterminate`, record the failure, reconcile read-only, and
  exclude from successful-commit distributions; do not retry an uncertain
  mutation blindly.

### Per-replica observation without `/metrics`

Preferred design: a measurement-only wrapper process in each app polls its own
`http://127.0.0.1:8080/ready` every **50 ms** and writes a bounded, structured event
on generation/floor changes and readiness failures. Also emit one startup and one
final coverage record. It records local send/receive times, UTC and monotonic
elapsed time, boot ID, role, app/revision/replica identity, generation,
minimum_generation where applicable, and status/error code. No token, key,
principal envelope, account identifier or full error message is logged.

The wrapper launches/owns the serving child, forwards termination, and fails if
the child dies. Do not turn observation into a second serving proxy. Both
`/ready` implementations already expose the necessary fields
(`src/server/gateway_mode.rs:362-386`,
`src/server/gateway_trust.rs:268-275`).
Readiness sampling does not request Blob: it reads the current cache
(`src/server/control_state.rs:951-954,834-847`).
It is a sampled observation with up to the **observed** sampling gap and request
latency, not a 50 ms precision guarantee.

The harness inventories all replicas/revisions before and after each mutation
and collects each replica's observer log. Ingress-wide repeated requests are
**not** proof every replica was observed. Missing identity, missing samples,
restart/new boot ID, revision overlap or a replica-set change invalidates that
fixed-set trial; preserve partial evidence, not a success-shaped percentile.

Assumption: ACA exposes usable replica identity metadata to the process or its
console logs. Verify against `replica list`; if absent, add no fake fallback ID.
Use replica-specific `az containerapp exec` to invoke the proposed helper's
`clock-sample` operation before/after each short trial. Harness send/receive
brackets and remote UTC bracket offset; record uncertainty and host clock jumps.
This CLI mechanism/precision is **unverified**. If offset drift cannot be bounded,
mark cross-host timing incomplete, while retaining generation coverage and
logical read-cost evidence. Do not infer clock error from normal request RTT to
a load-balanced gateway.

Cross-check behavior after generation observations:

- Revoked sacrificial credential: gateway returns 401.
- Suspended sacrificial tenant: record `tenant_suspended`; resume must restore
  its expected authorized response.
- Stale/no control snapshot: readiness fails and data request returns the
  applicable 503, not successful propagation.
- Direct old-generation envelope: backend refusal after its minimum rises.

These outcomes have local precedent, not ACA timing evidence
(`tests/gateway_azurite.rs:971-1013`,
`src/server/control_state.rs:834-847`,
`src/server/gateway_trust.rs:238-254`).
Keep an unaffected observation tenant/credential so suspension does not prevent
the driver from checking normal traffic. Freshness/lineage/epoch headers describe
other state, not control generation: the gateway parses `x-vlite-epoch`, and
reader test headers include lineage, binding generation, age and sequence
(`src/server/gateway.rs:565-566`,
`src/server/gateway_mode.rs:1172-1178`).
Do not substitute any of those for A or invent an existing replica-ID header.

### Sweep and mutation protocol

**Proposed poll intervals:** 100, 250, 500, 1000, 2000, 5000 ms.
Run a separate fresh trial per interval, keeping gateway and trust poll settings
equal. Initial **S_ctl=30000 ms** is only an experimental guard, not a conclusion;
every interval must remain below S_ctl, as required in runtime config
(`src/server/gateway_mode.rs:114-118`,
`src/server/gateway_trust.rs:96-103`).

Use two gateway replicas, one writer and one reader: **four independent control
feeds**, not just the two gateways. Driver/admin reads are separate.
Hold scale fixed and keep the reader warm before starting.
Run 20 sequential mutations: ten sacrificial credential revocations and ten
alternating suspend/resume operations on a separate tenant. Use no other control
writer. Wait for every expected replica/floor or a 30 s deadline before the next
mutation; insert seeded jitter in `[0,poll_interval]` to avoid phase-locking.
Create all sacrificial credentials before the measured window.
Repeat the matrix under a separately declared modest fixed request rate only
after the idle sweep; record that rate rather than conflating both workloads.

Twenty samples per replica are a pilot, not strong p99 evidence. Publish sample
counts, maxima, censored/missing samples and individual bounded mutation records;
repeat independently before proposing a bound. Do not choose S_ctl simply as the
largest observed sample. A proposal must budget observed propagation,
poll/read tail, scheduling pauses, observation uncertainty, clock error, and
unmeasured outage behavior. No finite empirical run proves a universal bound.

### Blob reads: what is counted and what is only calculated

One logical feed poll invokes `Objects::read`, which performs one bounded
`ObjectStore::get`; ETag equality refreshes confirmation time **after reading**.
There is no HEAD or conditional GET in this path
(`src/server/control_state.rs:649-663,808-829`).
The poll worker waits an interval **after** the previous poll finishes; it is
not a fixed-rate timer (`src/server/control_state.rs:979-983`).
Startup performs one confirmed poll before serving
(`src/server/gateway_mode.rs:191-200`,
`src/server/gateway_trust.rs:149-166`).

For zero-duration successful reads, the **calculated ideal steady-state**
per-process upper reference is `60000 / p_ms` logical GETs/minute:

| Poll ms | GETs/process/min, calculated | Four-feed total/min, calculated |
|---:|---:|---:|
| 100 | 600 | 2400 |
| 250 | 240 | 960 |
| 500 | 120 | 480 |
| 1000 | 60 | 240 |
| 2000 | 30 | 120 |
| 5000 | 12 | 48 |

Exclude startup from the steady-state denominator and report it separately.
Actual successful cadence is approximately `60000 / (p_ms + read_ms)`, subject
to scheduler delay; measure window boundaries instead of rounding theoretical
counts into observations. Same-generation polls still download the snapshot.
Snapshot byte size is a required cost dimension; test fixture size and credential
count must be recorded.

At the Azure adapter, `get` sends GET inside a retry loop, with at most three
attempts for `Unknown` errors. Authentication can fail before an HTTP request is
sent, and managed-identity token requests have their own endpoint/cache.
Consequently one logical poll is not always one billed Blob request
(`src/server/cloud_backup/azure.rs:30,284-331,401-411,526-549,574-586`).
The HTTP client also has 120 s total/10 s connect timeout, so slow failure trials
cannot assume the normal poll interval bounds read completion
(`src/server/cloud_backup/azure.rs:144-148`).
Freshness uses poll-start monotonic time; a long read can make its returned cache
immediately stale (`src/server/control_state.rs:808-824,842-847,957-958`).

Minimum deliverable: explicitly labelled **code-derived request model**, measured
replica lifetime and logical poll counts. Preferred small instrumentation:

- Per feed: startup/steady attempts, successes, failures, generation advances,
  total read elapsed time and snapshot bytes; count failure once even if retried.
- At control-object Azure send boundary only: physical GET attempts and response
  statuses/bytes, scoped to the exact temporary control container/object.
  Distinguish auth failures before send, transport-unknown outcomes and read-body
  failures; no counter should pretend to be authoritative Azure billing.
- Emit bounded window summaries rather than a log per unchanged poll, to avoid
  turning log ingestion/observer overhead into the measured cost.
- Keep operator mutation GET/PUT, archive GET/PUT/list, authority rechecks,
  reader apply/pin operations and identity calls in separate categories.

No new monitoring service or storage diagnostic setting is required. If existing
Storage metrics are readable, save a bounded query of transaction counts by
API/time window as a cross-check, with its aggregation delay and contamination
noted. Assumption: suitable dimensions/permissions exist; not verified here.
Do not report an account aggregate as per-replica cost, or substitute unavailable
metrics with zero. Logical counts alone do not validate actual billed cost.

### MEAS-005 sizes and budget

Proposed requested sizes: each gateway **1 vCPU/2 GiB**; writer **1/2 GiB**;
reader **0.5/1 GiB**; driver **1/2 GiB**. Observer runs inside each role's stated
allocation, not as an unaccounted extra replica. Record requested resources and
host signals separately; ITEM-033 explicitly warns visible cgroup quota was not
proof of effective ACA entitlement
(`docs/projects/feature-aca-ha/feature-aca-ha.prd.md:903`).

Per-interval job budget, proposed maximum:
`600 identity/store preflight + 180 backend/reader readiness + 120 warmup
+ 20*(30 observation deadline + 5 jitter) + 120 flush = 1720 s < 3600 s`.
Initialization and clock calibration must fit those bounded phases; otherwise
abort, not extend invisibly. Six intervals mean six separately bounded jobs,
not six intervals crammed into one replica timeout. Build/group setup and final
deletion are additional wall time, not excluded resource lifetime cost.

## 4. MEAS-004: gateway overhead and scale-out

### Isolating the path

Run the load generator in one manual ACA job in the same new environment as
gateway and internal writer. Use a fixed tiny read-only fixture and predeclared
operations, e.g. `GET /v1/database` and a representative small search recorded
as separate request classes; verify the exact supported search payload before
implementation. **Assumption:** this synthetic mix is useful for initial gateway
overhead; it is not a representative tenant workload.

Compare:

`job -> gateway ingress -> gateway auth/classifier/signing -> writer ingress -> trusted writer`

with

`same job -> same writer ingress -> same trusted writer`.

Use the same database, tenant, policy, lineage, generation, request bodies,
payload sizes, keepalive configuration, TLS verification, client pool limits,
HTTP version and backend size. Direct requests carry valid short-lived envelopes
for that writer audience; gateway requests use the tenant bearer. Prepare direct
envelopes outside the timed request path and record their generation/lifetime.
Replenish safely before expiry; never print them. No control mutation or key
rotation during the load window. Private `gateway::mint` already includes the
target audience, A and scoped grant
(`src/server/gateway.rs:470-492`).

A small server-side measurement module can reuse these private helpers without
exporting signing internals as a general library API. Existing modules are private
and only selected gateway commands are re-exported
(`src/server.rs:47-49,66,94`).
Add an opt-in operator CLI command rather than unsafe standalone auth exceptions.

Use alternating **ABBA windows** at each offered rate, with independent warmed
client pools, full response-body consumption and matching status/body digests.
Do not send full-rate direct and gateway floods simultaneously at the same
single writer and mistake shared-backend contention for gateway cost.
Report per-path latency distributions, throughput, schedule delays, status/error
counts and semantic mismatches. The difference of independently measured p95s
is a **difference of percentiles**, not a p95 distribution of per-request overhead.
Report paired-window mean/median deltas separately with repeat variability.

This comparison includes an extra ingress/network hop, gateway auth/signing and
edge policy. It does **not** isolate pure gateway CPU cost. Record that limitation
instead of calling every delta cryptographic overhead. Normalize gateway
throughput by observed ready replica-seconds only for stable windows; distinguish
aggregate capacity from per-replica scaling efficiency.

Ensure credentials are unlimited or their configured per-replica shares are well
above offered rates; record exact policy. Never silently omit 429s. Runtime divides
limits by configured `max_replicas`, so lower live counts can under-serve a limited
credential (`docs/projects/feature-aca-ha/gateway.md:64,75`).
Gateway also rejects excess in-flight requests with 503 `overloaded`
(`src/server/gateway_mode.rs:388-393`).
Backend saturation, rate-limit saturation and load-generator saturation must be
separate outcomes, not successful gateway-capacity measurements.

### Open-loop profile and scale timing

**Proposed `concurrentRequests` sweep:** 10, 25, 50, 100.
Use one fresh configuration/run per threshold; retain identical gateway
CPU/memory, fixed min=2/max=4, and runtime `max_replicas=4` throughout scale trials.
For overhead-only trials use fixed min=max=2 and runtime `max_replicas=2`.
Never revise scale max without matching runtime max. The template uses the HTTP
metadata setting and runtime config takes its maximum explicitly
(`deploy/azure-container-apps/ha/templates.ps1:88-101,453-458,503`).
No threshold in this report is a recommended production setting.

Proposed open-loop pilot stages: **50, 200, 800, 1600 requests/s**, each with
30 s transition/warmup and 180 s measured hold, followed by 180 s low-load
recovery. Schedule sends from monotonic absolute deadlines, not response
completion; bound in-flight memory/tasks. When the client bound is hit, count
unsent/dropped/later-than-deadline work explicitly. No catch-up burst, adaptive
success-only rate or automatic request retries. Save intended rate, actually
started rate, completion rate and lateness distribution for each stage.

These rates may fail to cross a threshold for a fast endpoint or may saturate the
writer first. Either is an informative **non-triggered/bottlenecked pilot**, not
a scale pass. A separate preregistered second run can choose a higher range from
the pilot; retain the first run. Do not add an artificial slow production route
just to force scaling. ACA's precise concurrency aggregation/window and reaction
behavior remain **unverified assumptions**; do not calculate the true scaler
trigger from client rate times mean latency alone.

Harness signal: poll `az containerapp replica list` every **5 s**, retaining
revision-scoped replica names and readiness/container status, including all
active revisions. CLI errors/null/malformed responses are unknown samples, not
zero replicas. Reconcile revision inventory to avoid mistaking rollout overlap
for autoscale. Existing deployment readiness code already uses revision-specific
replica enumeration (`deploy/azure-container-apps/ha/deploy.ps1:378-398`).

Record:

- Driver phase boundaries and ready replica count immediately before load.
- `t_first_present`: first observation with total replicas above baseline.
- `t_first_ready`: first observation with an additional ready replica.
- `t_target_ready`: first observation reaching the configured max, if reached.
- First successful request on each new replica only if real identity evidence
  exists; do not infer it from total responses or an epoch header.
- Stable-throughput/latency recovery after the increase and any later scale-down.

Primary timing is **load-step-to-observed-replica reaction**, with intervals
between the last no-change and first changed inventory samples, plus CLI request
brackets and driver/harness clock calibration uncertainty. It is not exact
threshold-crossing-to-scaler-decision time: that internal event is unavailable.
Use a bounded job-log phase marker carrying driver UTC/monotonic offset to align
inventory; log ingestion time is not the event time.

There is no replica identity field in the current gateway readiness JSON
(`src/server/gateway_mode.rs:367`). Use harness inventory for the required scale
signal; identity-stamped ordinary responses are optional future instrumentation,
not a prerequisite or an existing feature. Without them, explicitly leave
per-replica served-request counts/first-served time unknown.

### MEAS-004 sizes and budget

Proposed requested sizes: gateway **1 vCPU/2 GiB per replica**, writer **1/2 GiB**,
driver **1/2 GiB**; no reader in the primary load experiment. If the driver cannot
meet the schedule, invalidate that capacity run and repeat with **2/4 GiB**,
recording both attempts. Do not silently reinterpret its missed sends as server
throughput. Backend headroom must be evidenced by the direct-path run; if not,
the result is backend-limited and cannot establish gateway maximum capacity.

Separate jobs under the same bounded trial controller:

- **Fixed-replica overhead job:** 600 s preflight, 120 s ready/warm, four rates
  each with four ABBA windows of 15 s warm + 45 s measured, 60 s flush:
  `600 + 120 + 4*4*60 + 60 = 1740 s`.
- **One-threshold scale job:** `600 preflight + 120 warm + 4*(30+180)
  + 180 recovery + 60 flush = 1800 s`.

Each is below the existing job timeout, not a promise the workload will finish.
Use a separately bounded provisioning/ready phase between configurations.
Stop the full trial if quotas/identity/readiness never settle. Preserve the
requested max as a cap, not evidence the environment can reach it.
Current harness sets 3600 s timeout, retry zero, parallelism/completion one
(`deploy/azure-container-apps/ha/measure.ps1:164-170`).

## 5. Proposed plan steps in measure.ps1 style

The following are **design argv sketches, not commands executed or currently
accepted MEAS-004/005 options**. Append `--subscription <subscription>` to every
Azure step, as the existing step helper does
(`deploy/azure-container-apps/ha/measure.ps1:116-128`).
Actual ID values/client IDs/secret contents must never enter a printed plan.

```text
CheckGroup       az group exists --name <new-run-rg>
CreateGroup      az group create --name <new-run-rg> --location <location>
                 --tags vliteMeasurement=<run> vliteMeasurementOwner=ha-harness
                        vliteMeasurementExpires=<utc-expiry>
CreateRegistry   az acr create --name <new-acr> --resource-group <new-run-rg>
                 --sku Basic --admin-enabled false
BuildOrImport    az acr build ... --file <measurement-dockerfile> --no-logs
                 OR az acr import ... --source <immutable-source-digest>
CreateIdentities az identity create --name <role-identity> --resource-group <new-run-rg>
GrantPull        az role assignment create --assignee-object-id <role-principal-id>
                 --assignee-principal-type ServicePrincipal --role AcrPull
                 --scope <temporary-acr-id>
CreateStorage    az storage account create --name <new-account>
                 --resource-group <new-run-rg> --location <location>
                 --sku Standard_ZRS --kind StorageV2
                 --allow-shared-key-access false --allow-blob-public-access false
                 --min-tls-version TLS1_2 --https-only true
CreateContainers az storage container-rm create --storage-account <new-account>
                 --resource-group <new-run-rg> --name <each-of-four-containers>
GrantDataRoles   az role assignment create ... --scope <exact-temp-container-id>
                 # Read control for serving roles; isolated write/CAS for driver.
CreateEnv        az containerapp env create --name <new-env>
                 --resource-group <new-run-rg> --location <location>
                 --enable-workload-profiles true
PrepareJob       az containerapp job create ... --environment <new-env>
                 --trigger-type Manual --replica-timeout 3600
                 --replica-retry-limit 0 --parallelism 1 --replica-completion-count 1
                 --cpu <cpu> --memory <memory> --yaml <private-job-manifest>
Bootstrap        start one bounded driver/bootstrap execution; collect fixture
                 lineage; authority init once; create admin fixtures; publish A
CreateWriter     az containerapp create --name <writer> --resource-group <new-run-rg>
                 --yaml <private-internal-writer-manifest>
CreateReader005  az containerapp create ... --yaml <private-internal-reader-manifest>
CreateGateway    az containerapp create ... --yaml <private-gateway-manifest>
VerifyReady      revision inventory, every replica /ready observer, role grants,
                 internal ingress/empty environment route inventory, fixture parity
StartDriver      az containerapp job start --name <job> --resource-group <new-run-rg>
PollExecution    az containerapp job execution show ... --job-execution-name <execution>
Inventory004     az containerapp replica list --name <gateway> --revision <revision>
                 --resource-group <new-run-rg>       # repeated concurrently with job
Clock005         replica-specific exec of measurement clock-sample helper
ReadLogs         az containerapp job logs show ... --execution <execution>
                 plus bounded per-replica observer logs for MEAS-005
QueryLogs        existing workspace fallback, scoped by execution/run/replica
VerifyEvidence   check complete schema, frame sequence/hash, replica coverage,
                 timeline joins, censoring, provenance; write external evidence
DeleteGroup      az group delete --name <new-run-rg> --yes --no-wait
WaitDeleted      az group exists --name <new-run-rg>  # repeat; require false + exit 0
```

Implementation must resolve the app/job manifest invocation syntax against the
pinned CLI **offline mocks first, platform preflight later**; the sketch is not
a verified `az ... --yaml`/flag combination. Bootstrap and main driver are
separate manual executions so app startup need not depend on a long-running
bootstrap lease. Grant only the driver permission to publish run metadata.
Main driver reloads/checks the exact immutable run manifest and rejects missing,
foreign or changed state. Role-propagation retries must not repeat create-only
authority initialization.

MEAS-005 repeats the full bounded trial for each poll interval.
MEAS-004 completes overhead first, then one bounded trial per concurrency
threshold with the same image/fixture and recorded settings. Dedicated scale
inventory must run while the job runs, not after the existing blocking
`PollExecution` finishes. The current harness is sequential per tier
(`deploy/azure-container-apps/ha/measure.ps1:152-178,388-402`).
Add this concurrency explicitly; otherwise scale timing cannot be recovered.

## 6. Evidence JSON contracts

These are proposed shapes. Angle-bracket values are schema placeholders, not
sample results. Driver success prints exactly one compact JSON object; diagnostics
go to stderr, failures exit nonzero without success JSON, mirroring T30a
(`pool-briefs/T30a.md:70-71,107-121`).
The wrapper preserves the existing `VLITE-MEAS BEGIN/chunk/END/FAILED` transport
(`deploy/azure-container-apps/ha/measure.dockerfile:31-43`).

```json
{
  "schema": "vlite-meas005/1",
  "platform": "azure_container_apps_job",
  "source": {"revision": "<sha>", "dirty_paths": "<porcelain-or-empty>"},
  "run_id": "<opaque>",
  "config": {
    "poll_interval_ms": "<integer>", "s_ctl_ms": "<integer>",
    "observe_ms": "<integer>", "expected_roles": "<role-count-map>",
    "requested_resources": "<per-role-cpu-memory>", "snapshot_bytes": "<integer>",
    "storage_redundancy": "<observed-sku>", "fixture": "<nonsecret-description>"
  },
  "coverage": {
    "expected_replicas": "<bounded-role-revision-replica-boot-list>",
    "observed_replicas": "<bounded-list>", "missing": "<bounded-list>",
    "replica_set_changes": "<integer>", "clock_calibration": "<bounded-intervals>"
  },
  "mutations": [{
    "operation": "<revoke|suspend|resume>", "generation": "<integer>",
    "commit_clock": "<clock-id>", "commit_bracket_ms": ["<lower>", "<upper>"],
    "commit_basis": "<put_bracket|command_bracket>",
    "replicas": [{
      "replica_id": "<opaque>", "boot_id": "<opaque>",
      "generation": "<integer>", "minimum_generation": "<integer-or-null>",
      "observe_bracket_ms": ["<lower>", "<upper>"],
      "propagation_interval_ms": ["<lower>", "<upper>"],
      "status": "<observed|deadline|not_ready|missing|clock_unbounded>",
      "status_counts": "<status-or-status:error-code-map>"
    }]
  }],
  "blob_reads": {
    "basis": "<code_model|instrumented_attempts|storage_metrics>",
    "windows": "<bounded-per-replica-count-duration-byte-records>",
    "startup": "<separate-counts>", "mutation_io": "<separate-counts>",
    "logical_gets_per_replica_min": "<calculated-and-observed-separately>",
    "physical_get_attempts": "<integer-or-null>",
    "billing_validation": "not_established"
  },
  "summary": "<counts-and-lower-upper-latency-summaries>",
  "limitations": "<bounded-list>", "elapsed_ms": "<integer>",
  "qualification": "proposal_evidence_only"
}
```

Nearest-rank summaries use `{count,min,p50,p90,p95,p99,max,mean}`, null when empty,
and must say whether values summarize lower endpoints, upper endpoints, or a
clearly labelled estimator. Keep missing/censored counts outside the successful
distribution; a timeout is not a 30 s latency sample.
This summary convention reuses T30a's contract, not its not-yet-built code
(`pool-briefs/T30a.md:115-121`).

```json
{
  "schema": "vlite-meas004/1",
  "platform": "azure_container_apps_job",
  "source": "<revision-and-dirty-paths>", "run_id": "<opaque>",
  "config": {
    "mode": "<overhead|scale>", "concurrent_requests": "<integer>",
    "gateway_min": "<integer>", "gateway_max": "<integer>",
    "runtime_max_replicas": "<integer>", "requested_resources": "<role-map>",
    "fixture": "<description-and-digest>", "http_client": "<effective-options>",
    "credential_policy": "<nonsecret-effective-limits>",
    "envelope_lifetime_ms": "<integer>"
  },
  "phases": [{
    "id": "<phase>", "path": "<gateway|direct>", "offered_rps": "<integer>",
    "start_elapsed_ms": "<integer>", "duration_ms": "<integer>",
    "scheduled": "<integer>", "started": "<integer>", "completed": "<integer>",
    "unsent": "<integer>", "transport_failed": "<integer>",
    "status_counts": "<status:error-map>", "body_mismatches": "<integer>",
    "latency_ms": "<summary>", "schedule_lateness_ms": "<summary>",
    "max_in_flight_observed": "<integer>"
  }],
  "overhead": "<paired-window-deltas-not-a-fake-p95-delta-distribution>",
  "phase_clock": "<utc-monotonic-calibration-and-uncertainty>",
  "limitations": "<bounded-list>", "elapsed_ms": "<integer>",
  "qualification": "proposal_evidence_only"
}
```

The harness, not the unprivileged driver, writes a separate joined
`MEAS-004-scale-evidence.json`: verified driver payload digest, bounded
`replica_timeline` entries `{request_start,request_end,revision,replicas,errors}`,
clock alignment intervals, first-present/ready reaction intervals, maximum
observed ready count, and right-censoring flags. The driver cannot truthfully
print ARM replica counts it never observed. Keep raw valid driver JSON unchanged.
Likewise the harness joins MEAS-005 per-app logs to the driver's mutation output,
verifies complete replica coverage, and writes a derived evidence object without
pretending the driver alone directly observed every host.

Cap mutation arrays, identities, log bytes and timelines; e.g. the proposed
20 mutations and 5 s inventory naturally bound records, but enforce byte/record
limits and fail on truncation. No credentials, full target JSON, subscription ID,
identity client ID, signing seed, principal token or raw native errors belong in
any JSON. Driver success, evidence completeness and cleanup success are distinct
statuses; a complete driver run with failed deletion fails the overall command.

## 7. Smallest implementation gaps, file by file

No changes listed here were made in this report-only assignment.

| Proposed file(s) | Smallest purpose/change | Interface and scope consequence |
|---|---|---|
| `src/server/gateway_measure.rs` (new) | One bounded opt-in helper: fixture preparation, bootstrap/run, child wrapper/readiness observer and clock sample. Reuse private signer/mint/control-state helpers, existing HTTP/JSON/runtime crates and admin files. | New experimental operator CLI functionality, not a new REST route or public signing library. No dashboard assets. |
| `src/server.rs`, `src/bin/vlite-server.rs` | Register/export only the measurement command entry point and dispatch it; preserve all existing defaults. | CLI help/operator docs update required. Existing command dispatch is at `src/bin/vlite-server.rs:1448-1506`. |
| `src/server/control_state.rs` | Opt-in trace of successful mutation PUT bracket, winning generation, logical poll window counters. Do not change CAS retries, snapshot format or freshness clock semantics. | No REST API or storage schema change. Trace disabled by default; no key/object content logging. |
| `src/server/cloud_backup/azure.rs` | If exact request attempts are required, opt-in counters at the control-object GET send/retry boundary; separate pre-send auth failure and HTTP attempt/response. | No REST change; no new dependencies/services. Otherwise explicitly deliver only a code-derived cost model. |
| `deploy/azure-container-apps/ha/measure-gateway.ps1` (new) | Pure mode validation/manifests/step construction and bounded concurrent replica/log collector. Dot-source existing pure template helpers where applicable. | New measurement-only module; must not invoke/bypass `Assert-VLiteHaQualification`. |
| `deploy/azure-container-apps/ha/measure.ps1` | After T30b integration, accept exactly one new measurement at a time; dispatch to the module; preserve legacy MEAS-002/007 plans, framing, redaction and cleanup. Reject retention for new modes. | Additional opt-in harness modes; no deployment default change. |
| `deploy/azure-container-apps/ha/measure.dockerfile` | Build helper with the existing server binary, generate nonsecret tiny fixture in image, choose mode safely, keep non-root user and framed evidence. | No new image service/dependency; no secret baked into image. Existing image only builds reader probe at `deploy/azure-container-apps/ha/measure.dockerfile:23-25`. |
| `deploy/azure-container-apps/ha/measure-gateway.test.ps1` (new), `measure.test.ps1` | Offline pure-plan, inventory, framing, redaction and all cleanup/failure paths; keep old assertions. | Fixture tests only, not ACA evidence. |
| `tests/gateway_measure.rs` (new), helper module unit tests | Contracts, interval math, clock jumps, no invented identities, bounded open-loop scheduling, envelope validity, child cleanup. | Loopback/fake transport tests must be labelled separately from real Blob. |
| `docs/API.md`, `deploy/azure-container-apps/ha/measure.md` | Document experimental CLI, schemas, unmet preconditions and measured-vs-modelled distinction. | Operator documentation only; no `site/**`, dashboard or REST schema edits needed for this no-new-route design. |

Do **not** add `/metrics` reliance or a new `/generation` route: both are
unnecessary given existing `/ready`. No response replica header is needed for
the primary scale experiment. If later chosen, it is an additive public HTTP
API change requiring normal API/OpenAPI/convention updates; it must not be
smuggled into this measurement task. The PRD documents those route conventions
at `docs/projects/feature-aca-ha/feature-aca-ha.prd.md:317`.

Existing dependency availability, not permission to add more:
`server` already enables reqwest/base64/tokio/ring; reqwest includes blocking,
JSON and rustls features (`Cargo.toml:22,44,73`). Preserve `Cargo.lock`.
Because private helpers already exist, the in-server module is smaller and
safer than a separate crate or handwritten envelope implementation.

## 8. MEAS-003 readiness

**Present today, by static inspection:**

- `#[cfg(test)]` memory-store faults before/after ambiguous application and
  definite refusal, selectable by operation
  (`src/server/cloud_backup.rs:2191-2226`).
- HA archive tests for head-CAS success with lost acknowledgement, refusal
  before commit, mismatching readback and unresolved readback
  (`src/server/cloud_backup/archive.rs:1696-1748`).
- A loopback process proxy that gates partial request bodies/response transfer
  and can refuse matching authority reads
  (`tests/ha_chaos_azurite.rs:102-117,309-362`).
- D3 kills/cuts a writer during archive upload, checks failures, restarts and
  verifies readers converge without a gap
  (`tests/ha_chaos_azurite.rs:1213-1275`).
- D8 compares an authority-backed report's loss count with observed writer
  sequence and missing restored writes
  (`tests/ha_chaos_azurite.rs:1862-1937`).
- `failover report` accepts a paired observed sequence/time and optional archive
  status; its output explicitly represents unknowns rather than a configured
  archive interval as loss (`src/server/failover.rs:51-57,86-99,119-124,195-244`).
  Archive status also has checkpoint sequence/time/failures/last error
  (`src/server/serve_mode.rs:177-195`).

**Missing before a real-ACA MEAS-003 implementation/run:**

1. A permitted fault mechanism targeting only temporary writer/archive traffic:
   dropped upload before commit, lost acknowledgement after commit, stalled
   reads, authority-CAS conflict and writer termination at an evidenced phase.
   Existing proxy is test-only/loopback-oriented and does not establish a
   managed-identity HTTPS fault mechanism for Azure. Do not redirect production
   endpoints, disable TLS validation, add an external proxy service or weaken
   managed-identity endpoint validation to reuse it.
2. A durable external-to-writer acknowledgement ledger with sequence mapping,
   last verified head/base reference, fault trigger/event timestamps and
   uncertainty. Counters/last archive time alone do not prove actual lost
   acknowledged data.
3. A verified isolated restore/replacement protocol and zero-old-writer proof,
   with failure/unknown outcomes preserved; then compare restored effects to
   ledger and exact loss report, not only record-count estimates.
4. A scoped temporary-ACA driver/harness mode, deterministic phase-trigger
   controls, cleanup guarantees and approval for specific faults. The current
   harness refuses MEAS-003 (`deploy/azure-container-apps/ha/measure.ps1:45-47`).

Thus a preliminary MEAS-003 design can reuse existing local paths **now**, but
its real-ACA fault execution remains blocked. No new fault hook is necessary
for MEAS-004/005, and this work must not introduce automatic failover/EPIC-010.
No archive/chaos tests were executed in this task.

## 9. Proposed implementation split and validation

This is a **future implementation plan**, not a request to split or dispatch this
report. Keep T30a's driver scope and T30b's harness scope intact; integrate their
immutable results first. T30a owns its example/Cargo/build inputs and T30b owns
`measure.*` (`pool-briefs/T30a.md:123-135`,
`pool-briefs/T30b.md:108-113`). Do not run overlapping writers on those paths.

| Task | Dependencies and exclusive writes | Required tests/checks before handoff |
|---|---|---|
| W1: runtime measurement helper | Own `src/server/gateway_measure.rs`, `src/server.rs`, `src/bin/vlite-server.rs`, `src/server/control_state.rs`, `src/server/cloud_backup/azure.rs`, `tests/gateway_measure.rs`, `docs/API.md`. Freeze CLI/log/schema contract first. | Unit/fixture tests for CLI bounds/duplicates, success-only stdout, secret redaction, winning-CAS bracket/conflicts/unknowns, poll-vs-HTTP counts, clocks, readiness/floor, child cleanup and open-loop misses. `cargo fmt --all -- --check`; `cargo clippy --locked --features server --bin vlite-server --lib --test gateway_measure -- -D warnings`; `cargo test --locked --features server --lib gateway_measure`; `cargo test --locked --features server --test gateway_measure`; targeted `control_state` and `gateway_trust` library tests; `cargo build --locked --features server --bin vlite-server`; `git diff --exit-code -- Cargo.lock`. |
| W2: pure gateway measurement module | After W1 contract; own new `deploy/azure-container-apps/ha/measure-gateway.ps1` and `measure-gateway.test.ps1` only. No `deploy.ps1`/`templates.ps1` edits. | Mock every Azure call, check role/container scope and protected names, no adoption, min/max match, writer-only routing, resource bounds, per-job budget rejection, clock/replica set changes, malformed inventories and no zero-on-error. `pwsh -NoProfile -File deploy/azure-container-apps/ha/measure-gateway.test.ps1`. |
| W3: serialized harness integration/package | After W1/W2 and integrated T30a/T30b; exclusive `measure.ps1`, `measure.test.ps1`, `measure.dockerfile`, `measure.md` under `deploy/azure-container-apps/ha/`. | Regression MEAS-002/007 plans; new one-kind-only dispatch; no-new-mode retention; framed failure/truncation/hash tests; inventory runs during job; cleanup failures and redaction. `pwsh -NoProfile -File deploy/azure-container-apps/ha/measure.test.ps1`; new module test; `pwsh -NoProfile -File deploy/azure-container-apps/ha/test.ps1`; `python tests/docker_build_inputs.py`. |
| W4: authorized isolated run/evidence | After W1-W3 pass; own only an agreed report/evidence path, no source changes. Separate owner-side authorization/tools needed; not authorized here. | `-Plan` with explicit source revision and external scratch OutDir first; review exact temporary IDs/grants. Run one low-rate pilot, then the matrix. Confirm per-replica evidence and real deletion. Label every skipped/failed stage; never promote mock success to measurement success. |

Commands above are proposed for future implementing workers and were **not run
here**, except the two existing PowerShell suites listed below. Use task-specific
external `CARGO_TARGET_DIR`, temporary fixture directories, output and caches for
future builds/tests; no generated source-tree artifacts. If dependencies are
unavailable, use approved offline caches or report blocked rather than fetching
without permission. Emulator tests are conditional on separate permission:
this assignment forbids all network calls, including starting a loopback probe.

Recommended additional regression commands once implementation exists:
`cargo test --locked --features server --lib control_state`,
`cargo test --locked --features server --lib gateway_trust`,
`cargo test --locked --features server --test server_cli`,
and the existing ignored `gateway_azurite` test only in an explicitly permitted
loopback environment. Its local-only limits are explicit
(`tests/gateway_azurite.rs:1-19,567-568`).
Any future emulator run remains fixture evidence, not ACA scaling, managed
identity or GATE-003 qualification.

## 10. Checks actually executed

| Check | Result and boundary |
|---|---|
| `git --no-pager status --short` before work | Exit 0; empty output, clean initial checkout. |
| `git --no-pager rev-parse HEAD` | Exit 0; revision recorded above. |
| Repository file/range reads and scoped searches cited in this report | Executed; static inspection only, not behavioral validation. |
| `pwsh -NoProfile -File deploy\azure-container-apps\ha\measure.test.ps1` | **Exit 0**, printed `Measurement harness offline tests passed.` Expected injected-failure/retry warnings were emitted. These were recording mocks; no actual resource group was retained or deleted. |
| `pwsh -NoProfile -File deploy\azure-container-apps\ha\test.ps1` | **Exit 0**, printed `HA native assertions and recording-mock deployment safety tests passed; no Azure access.` Expected nonproduction warnings were emitted by mocked flows. |

Both suites put scratch material under OS temporary directories and clean it in
`finally`; the HA suite overrides its private state root to that scratch
(`deploy/azure-container-apps/ha/measure.test.ps1:15-16,282-284`,
`deploy/azure-container-apps/ha/test.ps1:438-442,637-639`).
Only the assigned report is intended to persist in this checkout.

**Not run / cannot be claimed:** Azure plans against a real CLI session,
provisioning, real Blob read/cost counts, ACA jobs/apps, replica identity/clock
calibration, concurrency scaling, MEAS-004/005 timing, Storage metrics, remote
image build, Azurite/HTTP probes, fault injection, restore or production checks.
They are prohibited by the brief's network/Azure restriction or depend on
implementation not present. No Rust build/test was necessary to change this
report, and none was run. Existing mock passes validate only the existing
harness/template behavior, not this proposed implementation.

Final delivery checks **passed**: strict UTF-8 decoding, nonempty content,
size below 1,048,576 bytes, fewer than 900 lines, and 89 existing-file/in-range
path/line citations. `git diff --check` exited 0; final
`git status --short --untracked-files=all` listed only the permitted report.
Citation-range checking verifies references exist, not the truth of conclusions.
**ACA/runtime validation remains INCOMPLETE regardless of report delivery.**
