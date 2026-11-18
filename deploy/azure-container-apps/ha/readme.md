# Opt-in Container Apps HA scaffolding (blocked, experimental)

**This is not a production deployment or a zero-downtime guarantee.**
The pure templates and mocked reconciliation are implemented, but actual rollout
always stops at `Assert-VLiteHaQualification`, before creating private state,
deactivating revisions or applying apps. The existing runtime has gateway/trust
configuration and reader progress support, but no executable
`--reader-apply-file` enrollment/configuration contract. The reader JSON/template
is a future contract, not a claim that the current binary accepts it.

There is no Boolean qualification bypass. The missing verifier must establish
digest-bound `linux/amd64` capabilities, complete route enumeration, effective
container-scoped identity grants, quota/storage headroom, writer-only control
routing, verified recovery, quiesce/flush/loss reporting, create-only authority
initialization, and expected-epoch CAS handoff after all old writer replicas stop.
Reader release additionally requires enrollment, two private follower
generations, whole-set retention pins, capacity and staleness enforcement,
binding generation/current authority checks and readiness integration.
Do not remove the gate merely because template tests pass.

## This does not change the singleton deployment

The parent deployment scripts, `vlite-app`, `vlite-env`, `vlite-data` and the
singleton's original `admin-token` remain unchanged. The HA script does not
dot-source the parent script, discover singleton resources or adopt existing
unowned resources. Protected singleton names are refused for HA apps,
environment, share and storage binding, even with HA approval. HA app names
must be distinct. Existing apps require exact `vliteHaDeployment=<DeploymentId>`
and `vliteHaRole=gateway|writer|reader`; the preexisting HA environment requires
the deployment tag, and the share requires matching ownership metadata.

`-EnableHa` is mandatory on the script command line, including when `-Config`
contains `EnableHa=true`. Omission refuses before any Azure call. Explicit
`-AllowUnqualifiedWriterStorage` is also required for isolated evaluation.
The selected subscription must match `SubscriptionId`; it is never switched.
Approval does not bypass the currently blocked capabilities.

## Prerequisites (provisioned and approved separately)

Use PowerShell 7.2+, and a pinned/recorded Azure CLI and Container Apps extension
whose JSON manifests, revision inventories and complete environment HTTP-route
listing have been qualified. The script does not install or upgrade tooling.
Have an existing colocated resource group and owned, ready, **zone-redundant
workload-profile environment**, with a `Consumption` profile and approved
existing delegated infrastructure subnet in the selected subscription.
Workload-profile subnet minimum is `/27`; actual zone/scale headroom must be
sized independently. Zone redundancy cannot be retrofitted into the singleton
environment. Dedicated profiles are refused pending measured approval.

Provide an existing colocated HTTPS-only ZRS storage account; isolated control
data/coordination and tenant data/coordination Blob containers; an owned,
separate writer Azure Files share; and an existing ReadWrite HA environment
storage binding to that exact account/share. No new Azure services, implicit
networking, storage-key lookup/renewal, account-setting changes or authority
object upload are performed. Control containers must not overlap tenant
containers. Prefixes are **not RBAC isolation**.

Provide existing user-assigned gateway/writer/reader identities in the selected
subscription with effective least-privilege container-scoped grants: gateway
read-only control; backends read-only control; writer immutable archive upload
and required coordination CAS; reader data read plus only the coordination
writes needed for retention pins. Broad container Contributor grants can exceed
authority-only requirements: isolate containers/use approved custom grants.
Unavailable scoped grants block deployment; shared-key target auth is refused.
The target client IDs must initially match gateway (control) and writer
(archive); backend projections use their own assigned client IDs.

**Azure Files remains experimental, not power-loss qualified.** The opt-in
does not certify actual-mount primitives, cross-host locking, crash durability,
recovery or latency. Actual-mount probing and qualification are still release
blockers; the HA template does not copy the singleton's Python init probe.
All generated apps are marked nonproduction/unqualified in tags and any
eventual successful evaluation emits a warning.

## Accepted parameters and configuration

`parameters.example.json` contains only deliberately invalid, nonsecret
placeholders. It is not runnable and proposes **no production defaults**.
Replace numeric placeholders with JSON numbers, not quoted numbers; measure and
approve every timing/resource input. Keep private material outside the checkout.
Load the edited file with `ConvertFrom-Json -AsHashtable`, then supply it through
`-Config`; individual named script parameters override corresponding keys.
Supply approval switches explicitly, not as JSON strings.

| Group | Accepted inputs and constraints |
|---|---|
| Approval | `EnableHa`, `AllowUnqualifiedWriterStorage`; optional `EnableReaders`, `InitializeTenant`. `Rotate` is accepted only to refuse automatic rotation. |
| Target | Required `SubscriptionId`, `ResourceGroup`, `Location`, `DeploymentId`, `Environment`, `SubnetId`, `GatewayName`, `WriterName`, `StorageAccount`, `ShareName`, `WriterStorageName`; `ReaderName` when readers enabled. Use short opaque lowercase identifiers; ACA names 3..32 characters, no consecutive/trailing hyphens. The orchestration currently limits share/binding names to 3..32 too. |
| Release | Required digest-pinned `Image` (`...@sha256:<64 hex>`); `WorkloadProfileName=Consumption`. No default image. Digest syntax alone is not capability evidence. |
| Identity/targets | Required `GatewayIdentityId`, `WriterIdentityId`, `ControlTarget`, `ArchiveTarget`; `ReaderIdentityId` when enabled. Targets contain exactly `account`, `container`, `coordination_container`, `prefix`, `auth=managed_identity`, `client_id`. |
| Key material | Positive `SecretGeneration`, lowercase `ActiveKid` (1..64 ASCII letters/digits/underscore/hyphen), exact `WriterAudience`; `ReaderAudience` when enabled. First initialization needs private offline `gateway-keygen` files via `SigningKeySourceFile`, `PublicKeyBundleSourceFile`. No implicit key generation. |
| Scaling | Required `GatewayMinReplicas` >=2, mandatory `GatewayMaxReplicas` >=min and <=1000, positive `GatewayHttpConcurrency`. Gateway config `max_replicas` must match scale max. Writer is fixed min=max=1 with no scale rules. |
| Resources | Required `GatewayCpu`, `GatewayMemory`, `WriterCpu`, `WriterMemory`; `ReaderCpu`, `ReaderMemory` when enabled. CPU 0.25..2 in 0.25 increments; explicit RAM is twice CPU in `Gi`. This validates supported pairs, not measured headroom. |
| Reader | When enabled: `ReaderMinReplicas` >=1, mandatory `ReaderMaxReplicas` >=min and <=1000, positive `ReaderHttpConcurrency`, `ReaderCapacityBytes`, `ReaderStalenessBoundMs`, `ReaderPollIntervalMs`. Reader poll must be below both `SCtlMs` and staleness bound. Disabled by default; enabling does not qualify reader routing. |
| Control/envelopes | Required positive `SCtlMs`, `PollIntervalMs`, `EnvelopeLifetimeMs`, `MaxLifetimeMs`, `UpstreamTimeoutMs`; explicit nonnegative `SkewMs`, `ReadinessPropagationMs`. Runtime timing bounds <=3,600,000 ms; poll < S_ctl; envelope lifetime <= backend max lifetime. Upstream timeout is per phase, not a total stream deadline. |
| Drain/startup | Required positive `DrainTimeoutMs`, explicit nonnegative `SafetyMarginMs`; required per-enabled-role `Gateway/Writer/ReaderTerminationGracePeriodSeconds` (1..3600) and `Gateway/Writer/ReaderStartupBudgetSeconds` (10..100, multiples of 10). Required `WriterStopBudgetMs`, plus `ReaderStopBudgetMs` if enabled (1..3,600,000). Every grace_ms must be strictly greater than readiness propagation + drain + margin; backend grace must also exceed readiness propagation + measured stop budget + margin. |
| Limits/wait | Required `MaxRequestBytes` 1..8,388,608; `MaxInFlight` 1..65,536; `MaxBuckets` 1..1,000,000; positive `TimeoutSeconds` for rollout waits. |
| Initialization only | With `InitializeTenant`, explicit `Dimensions` 1..65,536 and `CreateFormat` 3, 4 or 5. Omit both otherwise. Initialization requires brand-new apps, empty-scope/create-only authority proof and no previous initialization attempt. It is never a missing-database recovery strategy. |

`-Config` is a hashtable of these inputs. The pure template API additionally uses
fixed runtime paths: `ControlTargetFile=/run/vlite/control-target.json`,
`ControlObject=control.json`, `SigningKeyFile=/run/vlite/signing-key`,
`PublicKeyBundleFile=/run/vlite/public-keys.json`,
`ReaderTargetFile=/run/vlite/archive-target.json`, `FollowerDir=/data`.
Deployment sets these internally; they are not local key-source paths or
named script parameters. Trust builders receive role-specific `Audience`.
The optional Config-only `ReadRouting` must be `writer_only`;
`ReaderSharedKeyFile` is refused. No authority/epoch/lineage or capability
attestation parameter is invented where an operational protocol is missing.

Startup/readiness probes use `/ready`, liveness `/health`, port 8080.
Probe periods are fixed at 10/5/30 seconds, timeouts at 3 seconds, success
thresholds at 1; readiness/liveness failure thresholds are 3. Startup failure
threshold is the explicit startup budget divided by 10. A measured budget
outside the supported range blocks rather than silently extending it.

## Private material and intended reconciliation

Native ACA app secrets have immutable generation-suffixed names (for example
`gw-seed-g7`, `verify-g7`, `gateway-config-g7`, `backend-trust-g7`).
Explicit `storageType: Secret` `secretRef`/`path` entries project only the
role's files under `/run/vlite`; never mount all secrets. Gateway receives seed,
public bundle, gateway config and control target only, **never an admin token**.
Backends receive trust/control/public bundle; reader additionally receives
archive target and reader config. Writer archive/authority runtime wiring
remains blocked rather than inventing flags. Reader `/data` is private
`EmptyDir`, never the writer share; gateway has no database volume.

After qualification, state would live privately under
`$HOME\.vlite\azure-ha\<subscription>\<resource-group>\<deployment>`.
`deployment.json` pins bindings/initialization and `generation-gN.json` pins
original material and exact configuration. Back up these files securely.
Missing originals on rerun cause hard refusal, not regeneration or rotation.
Changing generation/configuration under immutable names is refused.
Manifests are CreateNew private temporary JSON files serialized at depth 30
and removed in `finally`, including failed CLI requests and writer-stop errors.
Only their paths reach `containerapp create|update --yaml`; secrets are never
argv, tags or normal output. Disable transcripts, debug/verbose native tracing
and payload capture. Verify secret projection UID 10001 readability/permissions
on the actual platform; do not assume hot-reload behavior or unknown ACA limits.
Config/public-bundle UTF-8 limits (64 KiB) and seed limit (1 KiB) are runtime
bounds, not proof of ACA secret-count/size or overlapping-generation capacity.

Intended order is **writer -> enabled readers -> gateway last**, with exact
revision/container readiness and isolation checks between each stage. Existing
owned apps use update; new apps use create, internally from their first manifest.
Writer update enumerates/deactivates **all active revisions**, checks every
revision (including inactive ones) has zero replicas, and re-enumerates for
drift before the separate authority handoff gate and update. Timeout or failed
handoff leaves old revisions stopped: no update, automatic rollback or old
revision reactivation. Single mode and replicas=1 are not fencing.

The environment HTTP-route inventory must be a complete JSON array of length
zero. Null/malformed/error/nonempty results refuse. Expected backends must be
owned in the exact environment, explicitly internal, HTTPS-only HTTP on 8080
with no extra ports; unknown tagged backends refuse. These point-in-time checks
are not a network kill switch. Restrict ingress/route-write RBAC, repeat drift
checks and serialize deployments across machines: the local lock is not a
distributed mutex. Never automatically delete someone else's routes.

## Manual staged overlap rotation (not automated)

`-Rotate` refuses, and changing `SecretGeneration` on a rerun is not a rotation
implementation. The following is the required reviewed operational sequence,
**not an executable workaround for the qualification gates**. A future approved
runbook must preserve all original generations and exact ready revision IDs.

1. Generate new material privately with the real `gateway-keygen`, validate
   bundle/key IDs and peak overlap secret byte/count capacity. Use fresh,
   immutable app-secret names; retain old pinned material. Never blindly PUT
   the combined `authority.json`/head object.
2. Roll backends with public bundles accepting old+new verification keys.
   Complete writer quiesce/recovery/expected-epoch CAS and reader qualification
   separately; verify exact ready revisions and replicas before proceeding.
3. Roll gateway to the new active signing key and corresponding bundle/config.
   Explicitly verify readiness; changing an app-scoped secret alone is not a
   rollout. Bundles are read at process start.
4. Confirm **all old signing replicas are stopped**, then wait for the maximum
   old envelope lifetime + skew + applicable in-flight/drain allowance. Only
   then roll verification-only bundles dropping the old key.
5. Deactivate every revision referencing retired material and verify no
   references/replicas remain before deleting obsolete app secrets. Retain
   private audit/recovery material under the approved retention policy.

A failed stage stops later stages; never remove old verification keys midway.
Emergency compromise revocation can deliberately interrupt requests.
Audience/lineage/binding changes require coordinated control-directory CAS.

## Availability boundary and offline checks

Keep control-directory routing **writer-only** until replication qualification.
One warm reader still has an outage window on zone loss, reseed or staleness;
writer fallback/manual recovery must preserve readiness, authority and auth.
Writer replacement deliberately sacrifices write availability. There is no
zero-downtime claim, automatic geo-failover or production durability guarantee.

These commands use synthetic fixtures and replace `Invoke-VLiteAz` with a
recording mock that rejects unexpected calls; they do not access subscriptions:

```powershell
pwsh -NoProfile -File deploy\azure-container-apps\ha\test.ps1
pwsh -NoProfile -File deploy\azure-container-apps\test.ps1
```

HA orchestration tests bypass the blocked verifier only inside the test process
to exercise ordering/cleanup. They do not qualify an image, Azure storage,
effective grants, platform manifests or live routing.
