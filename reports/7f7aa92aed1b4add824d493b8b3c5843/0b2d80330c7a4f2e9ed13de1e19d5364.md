# T4: ACA limits and opt-in HA provisioning design

**Status: INCOMPLETE for numeric-limit verification and Azure platform qualification; design/report delivered.**

Research date: 2026-11-18. Inspected checkout: `7f00be4f70054a58432a53f8096ac7029cdbae59`. This is a research/design deliverable, not implemented IaC, a deployment approval, a passed ITEM-031 deployment check, or a passed HA gate. No Azure resource was queried or changed. Only this report was created.

The current Microsoft Learn quota page explains quota scopes and lookup/increase procedures but does **not** publish universal numeric app-count, environment-count, or environment-core defaults. The current secret documentation/reference also does not specify a numeric secret-count or value-size ceiling. Those requested numbers remain unresolved, not silently replaced with remembered historical limits. A numeric app-instance termination-grace maximum is likewise not established by the fetched sources. Consequently ITEM-032 cannot honestly be declared fully verified. These gaps do not prevent proposing a fail-closed deployment design.

## 1. Findings and controlling constraints

1. Use a **new HA-only directory and a separate, owned ACA environment**, retaining the existing PowerShell 7.2+ / Azure CLI / generated JSON passed to `--yaml` style. Do not edit or invoke the parent deployment script as an HA deployment. Its one-writer stop-before-update behavior and original app, environment, share, account settings, keys and token must remain untouched.
2. No Key Vault, database service, queue, Redis, Dapr sidecar, Terraform, Bicep module, new Rust/Python package, or additional logging service is needed. Use existing Blob storage, platform managed identity supported by the existing connector, and native ACA app secrets projected into files. Managed identity is already an implemented authentication mechanism, not a proposed new service.
3. **Internal ingress alone is insufficient.** Current Microsoft Learn explicitly says an environment HTTP route may expose an app with `external: false` [S5, S6]. Use an HA environment with **zero environment HTTP route configurations**, verified before mutation and after provisioning, plus internal-only backend ingress. This stricter default makes target aliases, labels and revisions irrelevant to the isolation verdict.
4. Gateway min=2 is an unmeasured starting setting, not proof of availability. `GatewayMaxReplicas` must be required with **no default**, matching the latest OD-006 decision. Its value must be identical to gateway JSON `max_replicas`. ACA temporarily exceeding configured replica counts is documented [S3]; static rate shares can therefore temporarily overshoot.
5. Writer min=max=1 is a **configured replica target**, never an at-most-one-process guarantee. Revision replacement and platform maintenance can overlap replicas [S3, S9]. Only the EPIC-006 authority/fencing protocol makes that tolerable. Manual stop/all-revisions-zero/authority-transfer ordering remains required for controlled writer updates.
6. This base is **not deployable as the complete HA topology**. `vlite-server.rs` has no `--reader-apply-file` or authority enrollment option; library reader primitives are not a running enrollment loop. A trusted writer still rejects `VLITE_ADMIN_DIR`, while archive scheduling currently belongs to that runtime. IaC must refuse an unqualified image rather than deploy a bare read-only file and call it a synchronized reader.

### Repository evidence

Paths and line numbers below refer to the inspected checkout, not a live system.

| Evidence | Design consequence |
|---|---|
| `docs/projects/feature-aca-ha/feature-aca-ha.prd.md:155-202,277-287,317-324,348,867-874` | Separate stateless gateway / one writer / reader fleet; SEC-003, RISK-011, ITEM-031/032; no invented availability or cost measurements. |
| PRD `:491-523` | Latest OD-005/006 require explicit unmeasured settings; OD-009 mandates ACA app secrets, no Key Vault; OD-010 uses ZRS, no geo failover; OD-011 keeps shared-pool as the default until dedicated cost is measured. |
| `pool-briefs/epic006-design-report.md` sections 1-3 | Stable, combined authority/head object at the coordination prefix; immutable epoch-bound indexes; no separate mutable HA head; initialization, claiming, transfer and ambiguous CAS outcomes fail closed. |
| Same report, section 6 (`:389-445`) | Gateway verifies backend epoch response stamps before exposing headers/body. Deploying a new app cannot substitute for that runtime behavior. |
| `deploy/azure-container-apps/deploy.ps1:1-97` | PowerShell parameter validation, checked CLI wrapper, private local state, generated template, secretRefs, startup/readiness probes, grace period. Reuse style, not the existing singleton template. |
| Same script `:100-130,165-309`; parent `README.md`, especially Storage support boundary and Updates and failures | Stop every revision, wait for every replica, verify ownership/subscription/storage, no automatic writer rollback; Azure Files remains evaluation-only. |
| `deploy/azure-container-apps/test.ps1` | Existing tests mock `Invoke-VLiteAz`, use private temporary state, assert mutation ordering and cleanup. HA tests should follow this dependency-free pattern. |
| `src/server/gateway_mode.rs:48-151,161-245,338-360` | Real gateway CLI and exact required JSON fields; DB-free start, snapshot confirmation, `/health` and `/ready`, bounded drain. |
| `src/server/gateway_trust.rs:45-131` | Exact trust-file fields and public-bundle loading; config/public bundle limit 64 KiB is a **VLite parser limit**, not an ACA secret-size limit. |
| `src/server/control_state.rs:534-562` | `control_state` names a target file plus relative object in the target coordination prefix; optional shared-key file is for the emulator. |
| `src/server/cloud_backup.rs:175-209,248-277`; `src/server/cloud_backup/azure.rs:97-141` | Production Blob auth is managed identity; shared keys are accepted only for loopback emulation. Target supports separate data and coordination containers. |
| `src/bin/vlite-server.rs:330-373,530-548` and no reader-apply/authority flag matches in this file | Reader refuses creation, explicit durability, group commit, ANN snapshots, gRPC and admin dir. Trust rejects admin dir and durability-none. Current source does **not** reject trust+reader in that block, despite stale usage text/older EPIC-006 report suggesting otherwise; this does not supply reader enrollment. |

## 2. Current Microsoft evidence and page dates

All sources were fetched from Microsoft during this task. Dates below distinguish page `ms.date` metadata from `updated_at` metadata; these are page dates, not promises that every statement was individually revalidated on that date. Full URLs are included so the report is usable independently of the tool transcript.

| ID | Source URL | `ms.date`; `updated_at` (UTC date) |
|---|---|---|
| S1 | https://learn.microsoft.com/en-us/azure/container-apps/quotas | 2026-10-09; 2026-10-16 |
| S2 | https://learn.microsoft.com/en-us/azure/container-apps/quota-requests | 2026-09-21; 2026-09-24 |
| S3 | https://learn.microsoft.com/en-us/azure/container-apps/scale-app | 2026-05-19; 2026-05-20 |
| S4 | https://learn.microsoft.com/en-us/azure/container-apps/containers | 2025-05-15; 2026-03-25 |
| S5 | https://learn.microsoft.com/en-us/azure/container-apps/ingress-overview | 2026-10-09; 2026-10-22 |
| S6 | https://learn.microsoft.com/en-us/azure/container-apps/rule-based-routing | 2025-04-16; 2026-08-05 |
| S7 | https://learn.microsoft.com/en-us/azure/container-apps/ingress-environment-configuration | 2025-05-12; 2026-03-25 |
| S8 | https://learn.microsoft.com/en-us/azure/container-apps/storage-mounts | 2026-10-13; 2026-10-26 |
| S9 | https://learn.microsoft.com/en-us/azure/container-apps/revisions | 2025-10-27; 2026-02-26 |
| S10 | https://learn.microsoft.com/en-us/azure/container-apps/manage-secrets | 2026-09-04; 2026-09-11 |
| S11 | https://learn.microsoft.com/en-us/azure/container-apps/workload-profiles-overview | 2025-11-04; 2026-06-26 |
| S12 | https://learn.microsoft.com/en-us/azure/container-apps/health-probes | 2025-11-06; 2026-03-25 |
| S13 | https://learn.microsoft.com/en-us/azure/templates/microsoft.app/2026-07-01/containerapps | 2024-12-09; 2026-09-21 |
| S14 | https://learn.microsoft.com/en-us/azure/templates/microsoft.app/2026-07-01/managedenvironments/httprouteconfigs | 2024-12-09; 2026-09-21 |
| S15 | https://learn.microsoft.com/en-us/cli/azure/containerapp/env/http-route-config?view=azure-cli-latest | 2026-11-09; 2026-11-16 |
| S16 | https://learn.microsoft.com/en-us/cli/azure/containerapp?view=azure-cli-latest | 2026-09-01; 2026-06-02 (metadata dates are out of order on the fetched page) |
| S17 | https://learn.microsoft.com/en-us/azure/azure-resource-manager/management/azure-subscription-service-limits#azure-container-apps-limits | 2026-08-04; 2026-09-29 |
| S18 | https://learn.microsoft.com/en-us/azure/container-apps/environment | 2026-02-26; 2026-02-27 |
| S19 | https://learn.microsoft.com/en-us/azure/container-apps/application-lifecycle-management | 2025-11-07; 2026-02-26 |
| S20 | https://learn.microsoft.com/en-us/azure/container-apps/billing | 2025-12-09; 2026-03-25 |
| S21 | https://azure.microsoft.com/en-us/pricing/details/container-apps/ | No page last-updated date exposed in fetched content |
| S22 | https://learn.microsoft.com/en-us/azure/reliability/reliability-container-apps | 2025-10-23; 2026-11-18 |

The old-looking `ms.date` on generated template references does not indicate the API version's release date. S13 is the current stable `2026-07-01` reference consulted; `2025-07-01` was also fetched and agrees about the absence of an app-grace upper bound. S17's ACA section now redirects quota questions to S1; it is not an independent numeric app/environment quota table.

## 3. Limits, confidence and HA sizing impact

**Classification:** **Documented** means the fetched source explicitly states the value/scope. **Inferred** means a design calculation or conservative deployment policy, not an Azure guarantee. **Unknown** means no defensible numeric value was found in the cited current sources; it is neither an inferred number nor an unlimited quota. Every source ID below links to the full URL in section 2.

| Limit | Value | Applicable plan/scope | Evidence / classification | Impact on VLite HA |
|---|---|---|---|---|
| Apps per environment | **Numeric ceiling unknown** in current fetched quota/environment/limits pages; do not assert historical 100, 200 or unlimited | Consumption-only and workload-profile environments | [S1](https://learn.microsoft.com/en-us/azure/container-apps/quotas), [S18](https://learn.microsoft.com/en-us/azure/container-apps/environment), [S17](https://learn.microsoft.com/en-us/azure/azure-resource-manager/management/azure-subscription-service-limits#azure-container-apps-limits); **Unknown** | One shared gateway app + two apps per dedicated tenant. App-count-derived tenant maximum cannot be numerically certified. Obtain subscription/environment-specific confirmation before fleet admission. |
| Environments per subscription/region | Adjustable **Managed Environment Count**, per subscription **and region**; default varies with subscription age/type/usage; no universal numeric default published here | Both environment types | [S1](https://learn.microsoft.com/en-us/azure/container-apps/quotas), [S2](https://learn.microsoft.com/en-us/azure/container-apps/quota-requests); scope/process **Documented**, numeric **Unknown** | A fresh HA environment consumes additional regional quota; do not reuse the existing live environment to evade it. |
| Minimum replicas | Default 0; configurable 0..1,000 | **Per revision**, subject to quotas and capacity | [S3](https://learn.microsoft.com/en-us/azure/container-apps/scale-app); **Documented** | Explicit gateway min 2, writer 1, reader proposed 1. A minimum is not guaranteed uninterrupted healthy capacity. |
| Maximum replicas | Default 10; configurable 1..1,000; temporary platform over-replication possible | Per revision, subject to quotas/capacity | [S3](https://learn.microsoft.com/en-us/azure/container-apps/scale-app); **Documented** | Require explicit gateway/reader max. Sum overlapping revisions, not just one app's max, when reserving headroom. |
| Revision history | Default 100 inactive revisions; threshold customizable using `--max-inactive-revisions` (preview in S9); no reliable active-revision numeric ceiling established here | Both plans | [S9](https://learn.microsoft.com/en-us/azure/container-apps/revisions); default **Documented**, other ceilings **Unknown** | Do not call 100 the simultaneous-active-revision limit. Use Single mode; still account for replacement overlap and prohibit reactivation of old writers. |
| Consumption replica CPU/RAM | Total allocation across containers: 0.25/0.5 GiB through 4 vCPU/8 GiB, in 0.25-vCPU / 0.5-GiB increments | Consumption profile in workload-profile environment | [S4](https://learn.microsoft.com/en-us/azure/container-apps/containers), [S11](https://learn.microsoft.com/en-us/azure/container-apps/workload-profiles-overview); **Documented** | Sidecars consume the same envelope; avoid them. Select a CPU pair large enough for DB, indexes, replay peaks and local space. |
| Legacy Consumption-only replica CPU/RAM | Maximum 2 vCPU/4 GiB | Legacy Consumption-only environment | [S4](https://learn.microsoft.com/en-us/azure/container-apps/containers); **Documented** | Do not assume the parent script's 4-vCPU option is valid in a legacy environment; proposed HA uses a new workload-profile environment. No parent-file change proposed. |
| Dedicated node sizes | D4/D8/D16/D32: 4/8/16/32 vCPU and 16/32/64/128 GiB. E4/E8/E16/E32: 4/8/16/32 vCPU and 32/64/128/256 GiB | Dedicated D/E profiles, **per node**, not per replica | [S11](https://learn.microsoft.com/en-us/azure/container-apps/workload-profiles-overview); **Documented** | Replica resources must fit an available node after runtime reservations. Do not present E32's 256 GiB as a guaranteed allocatable replica size. |
| Dedicated per-replica maximum | Exact usable CPU/RAM ceiling not numerically established by S11; platform reserves part of each node | Dedicated D/E profiles | [S11](https://learn.microsoft.com/en-us/azure/container-apps/workload-profiles-overview); reservation **Documented**, replica maximum **Unknown** | Qualify exact allocation and placement before choosing Dedicated for oversized data. |
| Environment Consumption cores | Sum of requested cores for **every active replica across all apps**; adjustable environment quota; actual numeric limit must be retrieved | Consumption in either environment type | [S1](https://learn.microsoft.com/en-us/azure/container-apps/quotas); definition **Documented**, numeric **Unknown** | Evaluate at steady floor, all configured maxima and revision/maintenance overlap. A replica maximum of 1,000 does not grant 1,000 cores. |
| Environment Dedicated CPU/RAM | Separate General Purpose and Memory Optimized core quotas aggregate D/E profiles per environment; numeric allocations unknown. RAM derives from provisioned node types minus reservations, not a universal documented environment-RAM allowance | Dedicated workload profiles | [S1](https://learn.microsoft.com/en-us/azure/container-apps/quotas), [S11](https://learn.microsoft.com/en-us/azure/container-apps/workload-profiles-overview); definitions **Documented**, totals **Unknown** | Need both quota and real node/bin-packing capacity; spare aggregate cores do not guarantee a large replica fits. |
| HTTP ingress visibility | External publishes app at environment boundary; internal app FQDN reachable directly only inside same environment. Environment routes can still target internal apps | Both plans; per app + environment | [S5](https://learn.microsoft.com/en-us/azure/container-apps/ingress-overview), [S6](https://learn.microsoft.com/en-us/azure/container-apps/rule-based-routing); **Documented** | Only gateway external; writer/reader internal; require ITEM-031's separate route inventory check. Internal environment and internal app ingress are different settings. |
| HTTP ingress ports / extra ports | Built-in HTTP 80/443; target port chosen per app. At most 5 additional TCP ports/app; support request for more. External custom TCP ports unique per environment; theoretical capacity-planning maximum 290 externally exposed custom TCP ports/environment | Both plans; external TCP requires VNet | [S5](https://learn.microsoft.com/en-us/azure/container-apps/ingress-overview); **Documented**, 290 explicitly theoretical | Use HTTP target 8080 and **no extra ports**. 290 is not an HTTP-app-count limit. Reserved port 36985 must not be used. |
| Default environment ingress capacity | 2 ingress-proxy instances, autoscaling to 10, each up to 1 vCPU and 2 **GB** RAM; default ingress proxy resources not billed | Environment default ingress mode | [S7](https://learn.microsoft.com/en-us/azure/container-apps/ingress-environment-configuration); **Documented** | This is shared ingress capacity, not a documented per-app RPS promise or tenant count. Measure bottlenecks before considering premium ingress. |
| Environment HTTP routing limits | Route configurations are environment child resources; rules contain routes and targets including app/label/revision. Numeric configs/rules/targets caps not specified in fetched S6/S14 | Environment-level HTTP routes | [S6](https://learn.microsoft.com/en-us/azure/container-apps/rule-based-routing), [S14](https://learn.microsoft.com/en-us/azure/templates/microsoft.app/2026-07-01/managedenvironments/httprouteconfigs); model **Documented**, numeric **Unknown** | Proposed zero-route policy is **Inferred design policy**, not a platform limit; no need to consume or guess route quotas. |
| HTTP request timeout | 240 seconds in ingress overview | Default HTTP ingress | [S5](https://learn.microsoft.com/en-us/azure/container-apps/ingress-overview); **Documented** | Long requests/streams can fail ambiguously; gateway must not replay sent writes. Application per-phase timeouts do not extend ACA's limit. |
| Premium ingress settings | Idle request timeout 4..30 minutes, default 4; ingress-proxy termination grace 0..3,600 seconds, default 500; dedicated ingress profile, at least 2 nodes | Premium environment ingress | [S7](https://learn.microsoft.com/en-us/azure/container-apps/ingress-environment-configuration); **Documented** | These are **not** proof that app replica termination grace has a 3,600-second maximum, nor a general promise of 30-minute request duration. Premium is not the default recommendation. |
| Application replica termination grace | `template.terminationGracePeriodSeconds`: nonnegative integer, default 30 seconds; 0 means immediate kill. **No numeric upper bound established** by fetched stable references | App replica, both plans | [S13](https://learn.microsoft.com/en-us/azure/templates/microsoft.app/2026-07-01/containerapps), [S19](https://learn.microsoft.com/en-us/azure/container-apps/application-lifecycle-management); default/minimum **Documented**, maximum **Unknown** | Require positive explicit app grace and validate against propagation+drain+margin; actual chosen value needs ACA qualification. S19's simplified 30-second shutdown text is not an overriding maximum. |
| Ephemeral storage | Total container+replica storage: <=0.25 vCPU 1 GiB; <=0.5 2 GiB; <=1 4 GiB; >1 8 GiB | Per replica | [S8](https://learn.microsoft.com/en-us/azure/container-apps/storage-mounts); **Documented** | Reader needs two follower files plus baseline/download/verification overhead. >1 vCPU can buy local-space headroom, but 8 GiB is not 8 GiB of database capacity. |
| Azure Files / volume mounts | Classic account-backed SMB `AzureFile` and NFS `NfsAzureFile` supported; multiple replicas/revisions/apps may mount a share; multiple volumes supported. NFS requires custom VNet and compatible network/security settings; NFS transit encryption unsupported. Blob/NetApp file-share mounts unsupported | Both plans, with documented network restrictions | [S8](https://learn.microsoft.com/en-us/azure/container-apps/storage-mounts); features **Documented**; numeric mount/share-count cap **Unknown** | Never mount Blob as a DB, and never let readers share writer DB files. Existing Azure Files durability remains unqualified; do not equate mount support with VLite storage correctness. |
| Secrets per app | App-scoped secrets supported, shared by referencing revisions; **numeric count ceiling unknown** in fetched docs/reference | Per app, both plans | [S10](https://learn.microsoft.com/en-us/azure/container-apps/manage-secrets), [S13](https://learn.microsoft.com/en-us/azure/templates/microsoft.app/2026-07-01/containerapps); semantics **Documented**, count **Unknown** | Keep a small, explicit per-generation set; reserve overlapping generations for rotation. Confirm the effective service ceiling before admission/rotation. |
| Secret value size | **Numeric bytes/characters ceiling unknown** in fetched docs/reference; string type alone does not establish a size limit | Per secret/app | [S10](https://learn.microsoft.com/en-us/azure/container-apps/manage-secrets), [S13](https://learn.microsoft.com/en-us/azure/templates/microsoft.app/2026-07-01/containerapps); **Unknown** | Do not call VLite's 64-KiB parser cap, Kubernetes' secret cap, or Key Vault's cap an ACA limit. Use compact seeds/config files; do not put the control-state snapshot into app secrets. |
| Secret change propagation | App-secret change creates no revision and does not automatically update existing revisions; deploy a new revision or restart. Specific secrets can be projected to named files | Both plans | [S10](https://learn.microsoft.com/en-us/azure/container-apps/manage-secrets); **Documented** | Use immutable secret names per generation and an explicit rollout. Do not rely on hot reload or Key Vault's separate automatic refresh behavior. |
| HTTP scale rule | `concurrentRequests` default 10, minimum 1, no numeric upper bound stated; evaluation every 15 seconds using requests in previous 15s divided by 15. Any rule can trigger scale-out | Per revision; HTTP apps, not jobs | [S3](https://learn.microsoft.com/en-us/azure/container-apps/scale-app); **Documented** | Require explicit thresholds; not a hard in-flight limit, rate limiter, or staleness-aware reader admission mechanism. Polling/apply work can remain busy without HTTP load. |
| Health probes | One each Startup/Readiness/Liveness per container; HTTP success 200..399; no exec or gRPC probes | Both plans | [S12](https://learn.microsoft.com/en-us/azure/container-apps/health-probes); **Documented** | Use `/ready`, not TCP or `/health`, for freshness/trust admission. Do not depend on `/metrics`. |
| Quota increase | Subscription/region requests via Azure Quota Management System; per-environment requests via that environment's Quota page after creation; may be automatic or reviewed by Support | Both plans; quota-specific | [S1](https://learn.microsoft.com/en-us/azure/container-apps/quotas), [S2](https://learn.microsoft.com/en-us/azure/container-apps/quota-requests); **Documented** | Preflight approved capacity. Quota approval does not fix a regional capacity shortage or turn undocumented ceilings into adjustable quotas. |

### Tenant-capacity calculation: useful, but not a verified number

For `T` dedicated tenants with one writer app and one reader app each, `S` other apps, app-count allowance `A`, gateway maximum `G`, gateway CPU `c_g`, writer CPU `c_w`, reader maximum `R` and reader CPU `c_r`:

```text
app_count                = 1 + S + 2*T
steady_floor_cores       = 2*c_g + T*(c_w + reader_min*c_r) + other_floor
configured_peak_cores    = G*c_g + T*(c_w + R*c_r) + other_peak
app-count tenant bound   = floor((A - 1 - S)/2)
core-derived bound       = floor((Q - G*c_g - other_peak - H)/(c_w + R*c_r))
```

These are **inferred calculations**, not documented platform limits. `Q` is the observed Consumption environment quota and `H` is an explicit operational reserve for rollout/maintenance, not a guessed Azure constant. Plan at least for old/new gateway or reader revisions overlapping; writer fixed-at-one does not remove its overlap risk. Dedicated profiles require a separate node-placement calculation, not substitution into a Consumption formula. Also account for subnet/IP availability, memory, local disk, Blob throughput, startup bursts, and approved scale-down/reseed time.

No value for `A`, `Q`, or a numerical maximum tenant count is justified by this investigation. For a hypothetical **operator-confirmed** `A=100`, no other apps and one gateway, app count alone would yield 49 tenants; this is merely arithmetic, **not a statement that ACA's current app limit is 100**.

Future authorized preflight should run `az containerapp env list-usages --name $Environment --resource-group $ResourceGroup --output json` and inspect actual names/limits/current usage; do not assume every unknown limit appears there. The regional environment-count allocation is available through QMS. If app-count/secret/grace caps are absent, obtain Microsoft Support confirmation and retain dated evidence. Until then refuse fleet-capacity/rotation claims, rather than invent defaults.

## 4. Proposed IaC organization and inputs

Proposed future files only; **none of these files are created by this task**:

```text
deploy\azure-container-apps\ha\
  README.md                 opt-in boundary, prerequisites, release gates, operations
  deploy.ps1                parameter validation, preflight, ordered reconciliation
  templates.ps1             pure gateway/writer/reader hashtable builders
  test.ps1                  native PowerShell assertions and mocked Azure CLI
  parameters.example.json   nonsecret schema example; no live names/tokens
```

No framework migration is warranted. Dot-source only `ha\templates.ps1` from HA scripts; do not import the parent deployment's mutable singleton flow. PowerShell's `ConvertTo-Json` avoids a YAML-parser dependency. As the existing script does, serialize generated JSON to a private temporary manifest accepted by `az containerapp ... --yaml`, then remove it in `finally`. S16 states other configuration flags are ignored with `--yaml`: the manifest, not an extra `--ingress` flag, must contain every security-relevant setting.

### Inputs and invariants

| Input group | Required parameters / proposed behavior |
|---|---|
| Explicit opt-in and target | Separate deployment approval plus `-EnableHa`, explicit `SubscriptionId`, `ResourceGroup`, `Location`, HA deployment ID and new environment name. Check selected subscription matches; never switch it. Reject parent singleton app/environment/share names and unowned resources. Names use short opaque tenant IDs and remain within ACA naming limits. |
| Infrastructure | Existing approved storage account, data/coordination containers, control-state target/prefix, per-tenant targets/prefixes, existing infrastructure subnet ID, existing supported identities and their container-scoped grants. No storage-account key renewal or account-setting changes. Require ZRS posture verification; reject geo-failover automation. |
| Image/profile | Required verified `linux/amd64` image digest with explicit release capability evidence; **do not default to the parent's v0.6.1 image**. Workload-profile environment with `Consumption` initially; Dedicated allowed only after measured need and capacity approval. |
| Gateway scale | `GatewayMinReplicas=2` proposed floor, validate >=2; **mandatory `GatewayMaxReplicas` with no default**, >=minimum and <=1,000. Must equal gateway runtime `max_replicas`. `GatewayHttpConcurrency` required positive integer, measured later. |
| Reader scale | `ReaderMinReplicas=1` proposed warm floor; required `ReaderMaxReplicas` and `ReaderHttpConcurrency`; reader capacity ceiling and max staleness required, no defaults. Keep reader routes writer-only until replication qualification passes. |
| Resources | Explicit gateway/writer/reader CPU and RAM; initially Consumption-compatible pairs. Verify image/scratch/DB/index memory and storage headroom; refuse unsupported pairs or insufficient capacity. |
| Control/trust | Required `s_ctl_ms`, `poll_interval_ms`, envelope lifetime/max lifetime, skew, audiences, active key ID, key generation, and exact file paths. Poll interval positive and below S_ctl; match code's bounds. No guessed production values. |
| Drain/probes | Explicit readiness propagation, drain timeout, app termination grace and startup budget. Enforce `grace_seconds*1000 > propagation_ms + drain_timeout_ms + configured_safety_margin_ms` for gateway, including any shutdown overhead. Backend grace must cover its measured flush/stop protocol, not simply copy gateway values. |
| Tenant directory | Tenant ID, lineage, binding generation, writer/reader audiences/origins, routing policy, epoch. Obtain epoch/authority from approved operations; never derive E from a timestamp, revision suffix or deployment count. |
| Local state | Private HA-specific directory under existing `$HOME\.vlite\azure` mechanism keyed by subscription/resource-group/deployment; separate from singleton token directory. Files contain secrets only locally, never in the repo. Missing original material on rerun is a hard refusal, not implicit regeneration. |

S22 recommends zone redundancy at environment creation, a supported existing VNet subnet (workload-profile minimum `/27`, with actual HA headroom sized separately) and min two replicas to distribute across zones. Use these as prerequisites for an **HA-qualified** environment; do not create a VNet/NAT/logging service implicitly. If approved existing network capacity is unavailable under the no-new-service constraint, provisioning is blocked, not downgraded silently. Zone redundancy cannot be enabled later on the existing environment. One warm reader still has an outage window on its zone loss; writer fallback/manual recovery must be explicit, not advertised as zero downtime.

### Storage, identities and authority

Use the existing storage **service/account**, but separate logical containers when permissions must differ:

```text
existing account
  control data + control coordination containers
    {control_prefix}control.json
  per-tenant data container (or preapproved equivalent isolated scope)
    {tenant_prefix}sets/{set_id}/...
  per-tenant coordination container
    {tenant_prefix}authority.json
    {tenant_prefix}sets/{set_id}/...pins/catalog/HA indexes as applicable
```

Container creation, if later approved, is within the existing Blob service, not a new service. Prefixes alone are **not RBAC isolation**. Do not grant a compromised tenant writer broad write access to shared control state or other tenants' authorities. Prefer preprovisioned, least-privilege app identities: gateway read-only control; backend read-only control; writer immutable data upload + required coordination CAS; reader data read plus only the coordination writes needed for whole-set retention pins. The existing reader pin protocol means a blanket read-only data-plane identity may be insufficient. Standard container-wide Contributor is broader than an authority-only write grant: document that exposure and use distinct containers/custom scoped permissions where existing mechanisms permit. If those grants/identities are unavailable, block rather than introduce production shared-key auth that the connector rejects.

Targets follow the **existing** schema, for example:

```json
{
  "account": "<existing-account>",
  "container": "<tenant-data-container>",
  "coordination_container": "<tenant-coordination-container>",
  "prefix": "ha/<deployment>/<tenant>/",
  "auth": "managed_identity",
  "client_id": "<existing-user-assigned-identity-client-id>"
}
```

Gateway/control targets use their own container/prefix and identity. The runtime control-state source is `{"target_file":"/run/vlite/control-target.json","object":"control.json"}`. This object remains in Blob, not in an ACA secret; the snapshot includes sensitive credential hashes and should not be printed.

The authority object is the **combined authority + authoritative head**, stable at the coordination prefix, as EPIC-006 sections 1-3 specify. IaC provisions addresses/credentials and verifies capability, but does not blindly PUT `authority.json` on every rerun. First initialization needs explicit empty-scope approval with create-only CAS. Updates/transfers use the operational authority command, expected E/revision, loss acknowledgement, all-old-replicas-zero and CAS; no general-purpose unconditional `az storage blob upload --overwrite` path. Writer process-incarnation IDs are runtime-generated, never copied from app secrets. Gateways neither possess nor mutate writer authority.

**Disk posture decision:** readers use private `EmptyDir` for their two follower generations and staging; gateway has no DB volume. Writer disk choice is a release blocker, not something IaC can conceal. Reusing an approved per-tenant Azure Files share keeps the existing storage mechanism but remains experimental and requires its actual-mount probe plus cross-host/durability qualification. Never mount that share in the reader or reuse the singleton share. A Blob-backed, ephemeral writer can be considered only after EPIC-006 restart/recovery and operator-loss-acknowledgement behavior are implemented and qualified; it must not automatically discard unarchived acknowledged data on routine replacement. Default: **refuse production HA provisioning while this choice is unqualified**; isolated evaluation can explicitly opt into the existing Azure Files mechanism. Do not introduce managed disks/AKS to bypass the no-new-service constraint.

## 5. Secrets, file mapping and rotation

All material below uses **native ACA app secrets**, with immutable generation-suffixed names. Public config can be delivered through the same file projection without making it a secret cryptographically. Never use Key Vault references; never put the signing seed in Blob control state, command-line literals, logs, status output or repo parameter files. Avoid the CLI's mount-all-secrets shortcut; explicitly project only required files.

| ACA secret name (illustrative) | Contents | Apps / projection |
|---|---|---|
| `gw-seed-g7` | Existing gateway-keygen's Ed25519 seed file encoding, not a PEM/JSON replacement | Gateway only: `/run/vlite/signing-key` |
| `verify-g7` | Versioned public verification bundle with old+new key overlap | Gateway and authorized backends: `/run/vlite/public-keys.json`; contains **no private key** |
| `gateway-config-g7` | Compact exact gateway configuration JSON | Gateway only: `/run/vlite/gateway.json` |
| `control-target-g7` | Existing Blob Target JSON; account/container/prefix/identity, no production account key | Every app needing control feed: `/run/vlite/control-target.json` |
| `backend-trust-g7` | Per-backend trust JSON with its exact audience, timing policy and public-bundle/control-target file paths | Writer or reader app: `/run/vlite/trust.json` |
| `archive-target-g7` | Per-tenant archive/coordination Target JSON | That tenant's writer/reader: `/run/vlite/archive-target.json` |
| `reader-config-g7`, `authority-config-g7` | Future versioned enrollment/authority settings **only after real schemas/CLI exist** | Tenant reader/writer, projected files; not claimed executable at this base |
| `bindings-g7` (only when needed) | Matching startup-only memory bindings file | Writer/reader via `--memory-bindings-file /run/vlite/bindings.json`; roll both and update binding generation |
| `admin-token` (only if an eligible management runtime needs it) | Existing-style bootstrap/admin bearer | `VLITE_AUTH_TOKEN` via `secretRef`, or file via `VLITE_AUTH_TOKEN_FILE`; **not** a gateway credential or bypass for trusted data routes. Do not invent a new management app. |

The existing singleton continues holding its own original `admin-token` unchanged. The gateway authenticates tenant credentials from Blob control state and must **not** receive the singleton admin token. A backend can parse a token while trust overrides data authorization, but setting `VLITE_ADMIN_DIR` on a trusted backend still fails startup here; secrets do not solve the unresolved management/archive-runtime boundary.

Example generated fragment (placeholders only):

```powershell
configuration.secrets = @(
  @{ name = 'gw-seed-g7'; value = $PrivateSeedText },
  @{ name = 'verify-g7'; value = $PublicBundleText },
  @{ name = 'gateway-config-g7'; value = $GatewayConfigJson },
  @{ name = 'control-target-g7'; value = $ControlTargetJson }
)
template.volumes = @(@{
  name = 'vlite-config'; storageType = 'Secret'
  secrets = @(
    @{ secretRef = 'gw-seed-g7'; path = 'signing-key' },
    @{ secretRef = 'verify-g7'; path = 'public-keys.json' },
    @{ secretRef = 'gateway-config-g7'; path = 'gateway.json' },
    @{ secretRef = 'control-target-g7'; path = 'control-target.json' }
  )
})
template.containers[0].volumeMounts = @(
  @{ volumeName = 'vlite-config'; mountPath = '/run/vlite' }
)
```

This is property-oriented pseudocode, not runnable PowerShell dotted assignments. S10/S13 document `storageType: Secret` and `secretRef/path`; S10's statement that volume mounts are unsupported in "PowerShell" refers to Azure PowerShell cmdlets, **not** PowerShell launching `az ... --yaml`. Read-only projection, file permissions and actual UID 10001 readability are platform gates to test; no hot file-update assumption is required.

**Rotation order:** generate material in private local state using the existing `gateway-keygen`; preserve a pinned copy of every referenced generation. First roll backend public bundles accepting old+new keys and confirm the exact ready revisions/replicas. Roll gateway revisions to the new active key and corresponding bundle/config. Wait until all old signing replicas are stopped and all old envelopes have expired (maximum envelope lifetime + skew, plus relevant in-flight/drain allowance), then roll verification-only bundles dropping the old key. Deactivate all revisions referencing retired material, then remove obsolete app secrets. A failed stage stops subsequent stages; do not revoke old verification keys halfway through an incomplete rollout. Emergency compromise rotation deliberately revokes faster and can interrupt requests; say so.

Secrets are app-scoped, not revision-versioned [S10]; immutable names are the versioning convention. A secret update alone is not a successful rollout. Bundles are read at process start in this base. Always roll/restart explicitly, verify generation/key IDs without logging seed values, and retain old names until no old revision references them. Do not claim that readers or writers can be restarted without their separate recovery/authority protocol. A fresh backend audience or lineage requires coordinated control-directory CAS rather than silently accepting a new origin.

Unknown ACA count/size caps must be resolved before claiming rotation capacity. Keep files compact, validate exact UTF-8 byte lengths and peak overlapping secret count, and make any local conservative budget clearly a deployment policy rather than an ACA limit. VLite's parser caps still apply independently.

## 6. Per-app templates and exact Azure CLI invocation shape

Everything in this section is **unexecuted pseudocode for later approved implementation**. Symbols such as `$GatewayManifest` name private generated files outside the repo.

### Common app template

Each complete manifest must specify name/location/tags, `properties.managedEnvironmentId` (or the chosen CLI-supported environment field), `workloadProfileName`, identity assignment, ingress, explicit containers, explicit scale, probes, secret projections, and `template.terminationGracePeriodSeconds`. Use ownership tags such as `vliteHaDeployment=<id>` and `vliteHaRole=gateway|writer|reader`; check exact ownership before adoption/update.

```text
configuration:
  activeRevisionsMode: Single
  ingress:
    external: <true gateway; false writer/reader>
    targetPort: 8080
    transport: http
    allowInsecure: false
    additionalPortMappings: []
    stickySessions: { affinity: none }
template:
  revisionSuffix: <unique release/generation suffix, not epoch>
  terminationGracePeriodSeconds: <explicit validated app grace>
  scale:
    minReplicas: <role minimum>
    maxReplicas: <role maximum>
    rules: <role rules below>
```

Use the manifest version supported by the pinned CLI/extension, verified against S13/S16. Pin and record CLI/extension versions as operational tooling; do not auto-upgrade shared machine tools during deployment. S16 includes `--termination-grace-period` today, but writing this field in the complete manifest matches existing deployment style and avoids a second partially applied update.

### Gateway

```text
command = ["/usr/local/bin/vlite-server"]
args    = ["gateway", "--config", "/run/vlite/gateway.json"]
scale   = { minReplicas: 2, maxReplicas: GatewayMaxReplicas,
            rules: [{ name: "gateway-http",
                      http: { metadata: { concurrentRequests:
                                           "<GatewayHttpConcurrency>" } } }] }
ingress.external = true
DB volumes = none
```

Gateway JSON includes **all real required fields** from `gateway_mode.rs`: `bind:"0.0.0.0:8080"`, `allow_remote:true`, `control_state`, `s_ctl_ms`, `poll_interval_ms`, `max_replicas`, `envelope_lifetime_ms`, `public_key_bundle_file`, `signing_key_file`, `active_kid`, `readiness_propagation_ms`, `drain_timeout_ms`, `max_request_bytes`, `max_in_flight`, `upstream_timeout_ms`, `max_buckets`. No silent configuration defaults. Validate parser limits, including poll<S_ctl and positive max replicas; upstream timeout is per phase, not a total stream deadline.

Proposed **unmeasured** starting probes: Startup HTTP `/ready:8080`, period 10s, timeout 3s, failure threshold 10; Readiness HTTP `/ready:8080`, period 5s, timeout 3s, failure threshold 3; Liveness HTTP `/health:8080`, period 30s, timeout 3s, failure threshold 3. Explicit success threshold 1. Startup failure budget must cover observed control-feed startup; backend enrollment may need a different, longer budget. These values stay within conservative generated-reference probe ranges; the health guide's larger default thresholds and generated reference bounds are not perfectly aligned, so do not simply copy the parent's threshold 120 into a supposedly validated HA template.

### Dedicated writer

```text
command = ["/usr/local/bin/vlite-server"]
args    = ["/data/vectors.vlite", "--serve-mode", "writer",
           "--bind", "0.0.0.0:8080", "--allow-remote",
           "--gateway-trust-file", "/run/vlite/trust.json",
           "--durability", "full"]
scale   = { minReplicas: 1, maxReplicas: 1, rules: [] }
ingress.external = false
```

Those are real current data-serving flags, **not a complete fenced/archive-enabled writer invocation**. `full` is a recommended local durability choice for acknowledged data, not a promise of synchronous Blob replication or qualified Azure Files durability. No automatic `--create-if-missing` on replacement: a missing database is a recovery/failure condition, not permission to start empty. A separately approved brand-new tenant initialization can explicitly use `--create-if-missing <dimensions> --create-format <qualified-format>`; it must not leak into restart manifests.

Add archive/authority configuration only through the actual implementation delivered by EPIC-006; **there is no current authority flag to print here**. Do not set `VLITE_ADMIN_DIR` in this trusted template until the runtime boundary is implemented. Startup/readiness must remain false until storage primitives, verified recovery, authority claim and fresh control feed succeed. Use `/ready` (not bare `/health`) and gate the image on these semantics.

Writer rollout pseudocode: remove from readiness/routing as required; quiesce/flush and obtain the loss/head report; enumerate **all** revisions; deactivate active ones; wait for every revision's replica list to become empty; prepare/transfer authority with expected-E CAS and loss acknowledgement; only then start a replacement that claims that prepared record and verifies recovery. Do not reactivate an old revision as automatic rollback. This is a deliberate write-availability cost; fixed replicas and Single revision mode alone do not fence.

### Dedicated reader

```text
command = ["/usr/local/bin/vlite-server"]
args    = ["/data/served/database.vlite", "--serve-mode", "reader",
           "--bind", "0.0.0.0:8080", "--allow-remote",
           "--gateway-trust-file", "/run/vlite/trust.json"]
scale   = { minReplicas: ReaderMinReplicas, maxReplicas: ReaderMaxReplicas,
            rules: [{ name: "reader-http",
                      http: { metadata: { concurrentRequests:
                                           "<ReaderHttpConcurrency>" } } }] }
ingress.external = false
volume  = private EmptyDir mounted at /data, never writer's share
```

These real flags only open an **existing read-only file** at this base. The PRD-proposed `--reader-apply-file /run/vlite/reader.json` is **not implemented in the inspected binary** and is deliberately excluded from the runnable-flag list. Later IaC must require an image with actual reader enrollment, dual followers, whole-set pins, capacity/staleness enforcement, current epoch verification and readiness integration. Do not replace missing functionality with an init script that downloads one stale file forever. Reader flags must not include create/durability/group-commit/ANN-snapshot/gRPC/admin-dir options.

Reader `/ready` must combine enrollment, apply verification, staleness bound, binding generation, authority epoch and control freshness, then remain false during re-seed/drain. Until demonstrated, keep routing `writer_only` and mark reader deployment blocked. HTTP scaling does not measure archive lag; retain warm capacity and qualify reseed costs, especially during bursts. If all readers are stale, gateway fallback to the writer must preserve routing/epoch/auth rules, not send traffic to a not-ready reader.

### Exact CLI calls to apply those complete manifests

```powershell
# Future approved deploy only. Config, image, ingress, replicas, probes,
# grace, secrets and identity are INSIDE each complete generated manifest.
# Invoke-VLiteAz checks native exit code and parses bounded JSON.
Invoke-VLiteAz @('containerapp', 'create', '--name', $GatewayName,
  '--resource-group', $ResourceGroup, '--yaml', $GatewayManifest)
Invoke-VLiteAz @('containerapp', 'create', '--name', $WriterName,
  '--resource-group', $ResourceGroup, '--yaml', $WriterManifest)
Invoke-VLiteAz @('containerapp', 'create', '--name', $ReaderName,
  '--resource-group', $ResourceGroup, '--yaml', $ReaderManifest)

# Existing, same-owned HA apps only, with role-specific rollout gates above:
Invoke-VLiteAz @('containerapp', 'update', '--name', $GatewayName,
  '--resource-group', $ResourceGroup, '--yaml', $GatewayManifest)
Invoke-VLiteAz @('containerapp', 'update', '--name', $WriterName,
  '--resource-group', $ResourceGroup, '--yaml', $WriterManifest)
Invoke-VLiteAz @('containerapp', 'update', '--name', $ReaderName,
  '--resource-group', $ResourceGroup, '--yaml', $ReaderManifest)
# Wrapper appends --only-show-errors --output json. No raw secrets in argv.
```

These are three role-specific operations, **not** instructions to run all six sequentially. In actual reconciliation the verb is selected by existing owned resource state. Backends and their checks must complete before publishing gateway routes/control directory; writer update must complete its fencing steps before its `update` call. Private generated manifests are removed on success and failure. Do not use `containerapp up` or partially create a public backend before flipping ingress internal.

For auditing the template-to-CLI mapping, the equivalent **non-YAML** settings are:

```text
gateway: --ingress external --target-port 8080 --transport http
         --allow-insecure false --revisions-mode single
         --min-replicas 2 --max-replicas <G>
         --scale-rule-name gateway-http --scale-rule-type http
         --scale-rule-http-concurrency <Cg>
         --termination-grace-period <Tg>
writer:  --ingress internal --target-port 8080 --transport http
         --allow-insecure false --revisions-mode single
         --min-replicas 1 --max-replicas 1
         --termination-grace-period <Tw>
reader:  --ingress internal --target-port 8080 --transport http
         --allow-insecure false --revisions-mode single
         --min-replicas <Rmin> --max-replicas <Rmax>
         --scale-rule-name reader-http --scale-rule-type http
         --scale-rule-http-concurrency <Cr>
         --termination-grace-period <Tr>
all:     --environment <owned-HA-environment> --image <verified-digest>
         --cpu <role-cpu> --memory <role-memory>
         --workload-profile-name Consumption
         --command /usr/local/bin/vlite-server --args <role args above>
```

Do **not** append these flags to the `--yaml` calls expecting them to override a manifest. Probes and selective secret-volume file mappings require the manifest. Do not put secret values in `--secrets` argv. Prefer existing explicitly assigned identities in the manifest; attachment and container RBAC readiness precede listener readiness.

## 7. ITEM-031: deploy-time proof, fail behavior and limitations

### Required read-only queries (proposed, not executed against Azure)

Run preflight before any mutations, again after every backend app reconciliation, and immediately before publishing a gateway/control-directory change and reporting success. Specify the selected subscription and full expected environment ID; no default RG/environment inference.

```powershell
# Full inventory is authoritative. Check exit status before parsing.
az containerapp list --subscription $SubscriptionId `
  --resource-group $ResourceGroup --only-show-errors --output json

# Fetch exact app objects for all expected writer/reader names.
az containerapp show --subscription $SubscriptionId `
  --resource-group $ResourceGroup --name $BackendName `
  --only-show-errors --output json

# Human/audit projection; not sufficient alone because filters can hide nulls.
az containerapp show --subscription $SubscriptionId `
  --resource-group $ResourceGroup --name $BackendName `
  --query "{id:id,environment:properties.managedEnvironmentId,ingress:properties.configuration.ingress}" `
  --only-show-errors --output json

# List ALL environment-level configs, not only routes managed by this script.
az containerapp env http-route-config list --subscription $SubscriptionId `
  --resource-group $ResourceGroup --name $Environment `
  --only-show-errors --output json

# Audit projection when investigating a nonempty result (S14 target structure).
az containerapp env http-route-config list --subscription $SubscriptionId `
  --resource-group $ResourceGroup --name $Environment `
  --query "[].{name:name,targets:properties.rules[].targets[].{app:containerApp,label:label,revision:revision}}" `
  --only-show-errors --output json
```

S15 documents the list command; S6/S14 place targets under **`properties.rules[].targets[]`**, not under `routes[].action` or ingress traffic. A labeled/specific-revision target still targets the backend app and violates SEC-003. If a pinned CLI supports only a newer environment field, normalize from a verified schema and fail conflicting IDs; do not interpret a missing environment ID as matching.

**Default enforcement:** require the route-list response to be a valid JSON **array of length zero**. This proves no environment route targets any backend at that observation, without attempting to safely allow arbitrary route shapes. A CLI/permission/unsupported-command/network/parse failure, null result, unhandled continuation/pagination, or unrecognized response shape is a hard refusal, never treated as an empty list. Validate that the selected CLI list implementation retrieves the complete list; if that cannot be established, fail. A future raw REST implementation must exhaust `nextLink`.

For each expected backend require exactly one owned app in the expected environment, explicit Boolean `external:false`, explicit `allowInsecure:false`, `transport:http`, `targetPort:8080`, and no additional port mappings. Check all expected tenant writer/reader apps and every app tagged with this HA deployment's backend roles; reject ownership/inventory drift, unknown backend additions or missing apps. Do not identify backends only by a `vlite-` prefix. Disabled/null ingress is not accepted as proof of the required working internal ingress configuration.

The pure validator below captures the strict ingress/zero-route core. **This function was executed only against synthetic in-memory fixtures**, not Azure. Ownership/tenant inventory reconciliation, CLI error handling, pagination and rollout sequencing are additional required implementation checks, not covered by this fixture.

```powershell
function Assert-HaIsolationSnapshot($Apps, $Routes, [string]$EnvId,
                                    [string[]]$BackendNames) {
    if ($Apps -isnot [array] -or $Routes -isnot [array]) {
        throw 'Malformed or missing inventory'
    }
    foreach ($name in $BackendNames) {
        $found = @($Apps | Where-Object { $_.name -ceq $name })
        if ($found.Count -ne 1) { throw 'Missing or duplicate backend' }
        $app = $found[0]
        if (-not [string]::Equals($app.properties.managedEnvironmentId,
                 $EnvId, [StringComparison]::OrdinalIgnoreCase)) {
            throw 'Wrong environment'
        }
        $ingress = $app.properties.configuration.ingress
        if ($ingress.external -isnot [bool] -or
            $ingress.external -ne $false) {
            throw 'Backend is not explicitly internal'
        }
        if ($ingress.allowInsecure -isnot [bool] -or
            $ingress.allowInsecure -ne $false) {
            throw 'Insecure ingress'
        }
        if ($ingress.transport -ne 'http' -or
            $ingress.targetPort -ne 8080) {
            throw 'Unexpected ingress protocol or port'
        }
        if (@($ingress.additionalPortMappings).Where({
                $null -ne $_
            }).Count -ne 0) {
            throw 'Additional ports prohibited'
        }
    }
    if ($Routes.Count -ne 0) {
        throw 'Environment HTTP routes prohibited in dedicated HA environment'
    }
}
```

Parse native CLI JSON with array shape preserved (PowerShell `ConvertFrom-Json -AsHashtable -NoEnumerate`), rather than pipeline-unrolling `[]` into null. The existing wrapper's lack of output for `[]` must not become an accidental pass or false missing-inventory result.

**Failure behavior:** fail before first mutation on preflight drift. A post-provisioning failure stops reconciliation, emits a nonsecret error containing app/route IDs and the failed invariant, withholds success URLs and directory publication, and never automatically modifies/deletes someone else's HTTP routes. Existing traffic may already exist: a detect-only gate is **not** a network kill switch. An approved incident runbook must isolate an already-exposed backend while retaining its data and writer authority; do not casually auto-deactivate a writer or bump epoch as cleanup. New backend creation must start internal from its initial manifest, never via a public intermediate resource.

**Optional future relaxation, not the default:** in a shared route-bearing HA environment, parse every rule/target from every config, normalize supported app names/resource IDs case-insensitively, reject every backend app/label/revision target, allow only explicitly approved gateway targets, and fail unknown shapes. This has more parser/allowlist risk than the zero-route default and should require distinct tests and approval.

**Limits of this proof:** a check is a point-in-time control-plane observation, not an immutable network policy. Environment-level Contributors can create route configs without permissions on the target app [S6]. Serialize deployments, restrict `Microsoft.App/managedEnvironments/httpRouteConfigs/write` and app ingress mutation rights using existing RBAC administration, repeat drift checks, and retain independent envelope verification. A local file lock only prevents concurrent deployment on one machine; it is not a cross-machine deployment mutex. No new watcher/service is proposed.

## 8. Cost per dedicated tenant, with gateway shared

**Current numeric list-price estimate: UNKNOWN.** The current official pricing page [S21] was fetched, but resource and request prices render as `$-` in the retrieved content and no region/currency was supplied. No defensible current regional dollar rate was obtained. Do not publish remembered historical prices as current list prices. This satisfies the brief's instruction to report unknown pricing rather than invent it.

The documented billing model [S20, S21] still supports a concrete budgeting formula:

```text
dedicated tenant, on Consumption:
  one always-running writer + at least one warm reader
  + extra reader replica-seconds and replacement/startup overlap
  + Blob data/coordination capacity, operations, pins and archive retention
  + approved Azure Files capacity/operations if used
  + existing logging/network charges and shared gateway allocation

compute = sum_over_writer_and_reader_replicas(
            active_seconds * (vCPU*active_CPU_rate + GiB*active_RAM_rate)
          + eligible_idle_seconds * (vCPU*idle_CPU_rate + GiB*idle_RAM_rate))
```

For a **purely illustrative unqualified sizing** of 1 vCPU/2 GiB writer and 1 vCPU/2 GiB reader, running 730 hours each, the baseline is 5,256,000 allocated vCPU-seconds and 10,512,000 GiB-seconds per tenant-month before overlaps/scale-out. Dollar cost remains unknown until region/currency rates and active/idle fractions are obtained. Replication, polling and indexing may prevent the idle condition even with no client requests. The example is not an assertion that 1 vCPU has enough RAM/local disk for a tenant.

Idle requires minimum replicas>0, revision at its minimum, no HTTP requests, CPU below 0.01 cores and inbound network below 1,000 bytes/s; above-minimum replicas bill active [S20]. The free 180,000 vCPU-seconds, 360,000 GiB-seconds and 2 million HTTP requests/month apply **per subscription**, not per tenant; do not subtract them separately for every tenant. Only HTTP requests originating **outside** the environment are billable and probes are excluded [S20], so gateway-to-backend hops within the environment are not another public request charge.

Dedicated workload profiles are charged by provisioned nodes plus the environment's management fee, not a standalone per-tenant replica rate. Tenant marginal cost can be low until it forces a new node, then step up; distribute shared headroom/management costs explicitly. Premium ingress adds dedicated ingress-node costs; it is not a free performance setting. No new logging workspace/private endpoint/planned-maintenance service is proposed, and any such future change needs separate approval/costing.

Until pricing and capacity are measured, keep `shared-pool` the default and dedicate by demonstrated need (OD-011), with an operator budget ceiling for each tenant's maximum reader fleet and overlap allowance.

## 9. Risks, open decisions and recommended defaults

| Risk / unresolved decision | Recommended default / completion evidence |
|---|---|
| Universal numeric quotas absent from current docs | Keep ITEM-032 partially unresolved; capture actual subscription/environment quotas and Microsoft confirmation for app/secret/grace caps before admission. Do not label unknown unlimited. |
| Reader enrollment and writer authority/archive integration missing at this base | Refuse HA rollout with current image. Require tested runtime schemas, synchronized-reader readiness and epoch stamps/checks, including EPIC-006's stable authority/head CAS. |
| Historical report/CLI help does not fully match source | Treat inspected parser as present behavior and disclose mismatch; test image startup compatibility rather than rely on old notes. No source correction within this task. |
| Writer persistence and unarchived acknowledged data | No production default until recovery/durability qualification; evaluation-only opt-in Azure Files with separate per-tenant share and actual mount tests. Never silently recreate empty DB on replacement. |
| Fixed writer replicas mistaken for fencing | Always require startup claim, per-batch E/ID verification, CAS publication, manual transfer and all-revisions-zero infrastructure fence. No auto failover/auto old-revision rollback. |
| Gateway min/max, thresholds, S_ctl and draining not measured | Min2 proposed; explicit max and all timing/threshold inputs, no runtime invented defaults. Measure latency/throughput/scale reaction, revocation lag and SIGTERM propagation before claims. |
| Gateway rollout/maintenance exceeds configured max | Reserve capacity and disclose rate-share overshoot. Keep backend rate limits; do not introduce a shared counter service. |
| Same-environment network trust / route drift | Separate HA-owned environment, zero environment routes, exact internal ingress validation, least-privilege existing RBAC; signed envelopes remain mandatory. |
| Cross-tenant authority/control tampering | Separate containers/grants; prefixes are naming, not authorization. Resolve reader pin write permissions without granting gateway/writers broad control-state mutation. |
| Secret delivery, UID readability, stale old replicas | Version-pinned native app secrets, explicit file selection, staged overlap rotation; verify effective mounted permissions/generations and old-replica shutdown on ACA before production. |
| Reader local storage and memory, full rebuild/reseed cost | Required ceiling <= existing 1-GiB baseline cap, measured transient headroom and free-space checks; fail enrollment before download. Never use total 8-GiB disk as a DB-size promise. |
| Region/zone outages and correlated dependency | ZRS/no geo failover, zone-redundant new environment on approved subnet, min2 gateway. Writer/one-reader loss still has recovery windows; no VLite RPO/RTO guarantee from ACA/Blob alone. |
| Request timeout/streaming and write retries | Default HTTP 240-second boundary, explicit application budgets and drain; no retries of sent/ambiguous writes. Keep platform retry-policy changes out of initial IaC pending specific qualification. |
| Multi-machine provisioning and out-of-band updates | Explicit serialized operator pipeline and ownership checks; local private lock is insufficient across hosts. Recheck invariants after changes; never claim atomic deployment from sequential CLI calls. |
| Price/tenant economics unknown | Budget using current regional quote/calculator evidence, active/idle measurements and max-reader/overlap cost; shared-pool stays default. |

## 10. Checks actually executed versus unexecuted qualification

### Executed

| Check | Result and boundary |
|---|---|
| Repository reads/searches and `git rev-parse HEAD` / status | Base recorded above; worktree initially clean. Static evidence only; not a code-path execution or Azure test. |
| Current Microsoft documentation HTTP fetches | S1-S22 fetched during task, including targeted extraction of ACA section and latest template fields. Verifies what those pages currently say, not live quotas. An initial obsolete `container-apps/availability-zones` URL returned 404; the canonical reliability page S22 was then fetched successfully. |
| `pwsh -NoLogo -NoProfile -File .\deploy\azure-container-apps\test.ps1` | Exit 0: existing deployment guard/create/update/cleanup checks passed with **mocked Azure and HTTP**. Created private OS-temp fixture state and cleaned it in its `finally`. This is singleton regression-fixture coverage, not new HA IaC validation. |
| `$env:PYTHONDONTWRITEBYTECODE='1'; python -B .\tests\azure_container_apps.py` | Exit 0: 6 tests discovered, **1 executed, 5 skipped** on Windows because they require Linux filesystem primitives. The passing test checks missing-mount refusal with mocked `fcntl`; no Azure share or real SMB/NFS durability/locking was tested. Bytecode writes disabled. |
| In-memory `Assert-HaIsolationSnapshot` PowerShell fixtures | **10/10** outcomes matched: safe internal backends/zero routes accepted; public backend, missing external flag, string `"false"`, wrong environment, missing backend, extra port, route to backend revision/label, unrelated route and missing route inventory all refused. Synthetic objects only; no Azure CLI/resource calls. |
| Delivery validation | Final report checked for nonempty strict UTF-8 Markdown and <=1,048,576 bytes; final Git status/diff scope checked separately. No committed source/test/deploy changes. |

### Not executed / could not qualify in this assignment

- **No live Azure commands**, including read-only resource/quota inventory, `az containerapp show`, route listing, `env list-usages`, app creation, deployment, secret writes or role changes: live-resource access was prohibited. ITEM-031 control-plane isolation and current subscription numeric allocations therefore remain **NOT RUN**.
- No HA template apply/dry-run or deployed image startup test: no HA scripts were implemented here and required runtime surfaces are missing. Pseudocode/fixture success is not validation of a runnable complete deployment.
- Five Linux-specific filesystem cases were skipped on this Windows worker. Real Azure Files cross-host lock exclusion, crash durability, account/share performance and power-loss behavior remain unqualified.
- ACA app-secret count/size ceilings, secret projection read-only behavior/UID permissions, rotation overlap, real readiness propagation, SIGTERM grace enforcement, quota saturation, HTTP scaling reaction, reader catch-up/reseed resource peaks, and epoch/zombie failure tests were not run.
- No current numeric regional pricing quote was obtained; the fetched official pricing table had placeholders. Cost remains unknown.
- No Rust build/full test suite was run: this report-only task changes no Rust code; running it would not prove the missing platform/runtime integration and was unnecessary for the research deliverable.

**Completion recommendation:** accept this report as design evidence, not a fully passed ITEM-032 or EPIC-008 validation. Resolve the specific documentation/quota gaps, deliver the runtime prerequisites, and execute isolated separately approved ACA qualification before implementing/enabling the HA deployment. Preserve the existing single-ACA setup throughout.
