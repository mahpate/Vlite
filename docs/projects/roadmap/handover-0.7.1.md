# VLite 0.7.1: release handover and 0.7.2 plan

Status: written when 0.7.1 was tagged. Read it with [RELEASE-0.7.1.md](../../RELEASE-0.7.1.md) (what changed) and
[next-major.md](next-major.md) (the roadmap). This page records how the release was qualified, what it deliberately leaves
out and what 0.7.2 takes up first. It holds no credentials, subscription identifiers or private infrastructure names.

## What 0.7.1 contains

- **A request guard.** One per-request guard for every REST, gRPC and webhook path that names a collection, graph, import
  job or tenant resource, so reserved agent-memory resources stay hidden from callers without raw access (lane G71).
- **Agent-memory follow-ups.** Cosine distance for `exact` and `mmr` recall, `exact` with text, item evidence references,
  the raw graph batch guard for memory's reserved names and the binding record stored in the database (lane M71).
- **Key-value and tool bounds, audit actors and record expiry.** Bounded key-value listing, input line caps for `vlite-mcp`,
  `vlite-shell` and `vlite-import`, a stable audit `actor`, the due-expiry refusal and the `kv_slots_exhausted_total` counter
  (lane K71).
- **A dashboard with Key-value and MemoryAI workspaces,** interactive 2D and 3D graphs, provider controls and named and sparse
  vector management, plus two new guides, [agent memory](../../MEMORY.md) and [key-value store](../../KEY-VALUE.md).

The size work (compact storage, the P3b indexes and consumers, the bounded exact scan) is not in 0.7.1: nothing compact is
active, defaults are unchanged and no size claim is made. Those lanes stay parked until 0.7.2.

## How the release was qualified

- **Candidate discipline.** One release branch received only accepted work. Each lane was reviewed on its own diff by three
  independent reviewers (skeptic, advocate and architect), gated and hand-mutated before it was integrated; then the
  integrated candidate, the release notes and the new guides were reviewed again, and the guides' examples were executed
  against a real server (33 of 38 worked as written; the rest were corrected).
- **Hand-mutation campaigns.** Request guard: 59 mutants, all killed (`tests/reserved_resource_mutants.json`; 46 of them keep
  results recorded on earlier heads, flagged `ran_on`, and are re-run after the release). Agent-memory follow-ups: 265 mutants,
  259 killed and 6 proven equivalent, each with the code that repeats its check (`tests/memory_followup_mutants.json`).
  Key-value, input and audit follow-ups: 200 list entries (198 distinct mutants), none survived and one is proven equivalent
  (`tests/k71_mutants.json`, replay with `tests/k71_mutants.py`).
- **Live checks on the candidate.** `scripts/memory_live_e2e.py` (116 checks pass, 3 documented skips),
  `scripts/memory_evidence_downgrade_check.py` against the 0.7.0 binary, `scripts/kv_concurrency_check.py` (no violations over
  more than 100,000 operations) and `scripts/rest_differential_check.py` against 0.7.0: 29 response differences, every one
  intended (dashboard assets, the new metric, the audit actor), three OpenAPI changes (the `ttl_ms` minimum of 1) and two additions
  (the new metric); the 13 coverage gaps of the harness (nine memory routes without a bindings file, four key-value routes with
  unsuitable generated requests) are unchanged. A key-value list and get comparison against 0.7.0 over 1,719 requests found no
  difference.
- **Upgrade and rollback.** 0.7.0 to 0.7.1 and back: 36 checks on live-like data (tenants, credentials, a revoked
  credential, accounts, graphs) and, in the key-value lane, 630 checks over three runs on a database with key-value entries and an audit log. 0.7.0 starts on every
  file 0.7.1 wrote, apart from the item evidence reference exception in the release notes.
- **CI.** The full workflow, the release workflow as a dry run, the Pages checks and the Dashboard UI workflow ran on the
  candidate; the release was tagged on a commit that differs from the fully tested one by documentation files only (checked by
  diff), after the Pages checks and the Dashboard UI workflow had run on the tagged tree.
- **Benchmarks.** None; benchmarks run for major releases only.

## Decisions in force

- The raw graph batch refuses memory's reserved keys and labels on every graph for every caller; the check lives in the server,
  not in the engine, so tools that open the file directly are not covered.
- Backups are the administrator's alone while any memory resource is reserved. The symbolic-link refusal for the bindings
  file was dropped again (Kubernetes mounts Secret keys as links); the file is followed, as in 0.7.0.
- A due record expiry is refused (REST 422, gRPC `INVALID_ARGUMENT`); key-value routes still accept it by contract.
- The audit `actor` of collection events changed from `admin` to `admin_token` or `user:<account id>`; the release notes say so.
- A hosted app is upgraded by deactivating every old revision, confirming zero replicas and changing only the image. A
  deploy script that replaces the whole container template must not be used on an app whose arguments or environment differ.
- Size goals S1 to S6 stay best effort. Must-not-regress items still block a release.

## Known limits and open items

Unchanged limits: the shared commit sequence, the import job numbering of tenants, the stored names of tenant resources.

| Item | What it is | Suggested release |
|---|---|---|
| Hidden-attempt telemetry | Attempts are audited but not exported as a metric. | 0.7.2 |
| Scanner hardening | The source scan that finds lookups outside the guard is a text scan; make it structural. | 0.7.2 |
| Retrieve evidence names | Names in `evidence.collections` are hidden but not checked against the grant (older behavior). | 0.7.2 |
| Duplicated validation | The record put refusal repeats the engine's check order; move it into an engine option. | 0.7.2 |
| Duplicated page builder | The key-value page builder exists in the server and in the MCP and shell tools. | 0.7.2 |
| `reserved_hidden_attempt` actor | The event has no `actor` field. | 0.7.2 |
| Over-long MCP line answer | It is answered with an `id` of null; a client cannot match it to its request. | 0.7.2 |
| Dashboard | The clock hint also matches any invalid memory field; vertical touch drags on the graph scroll the page. | 0.7.2 |
| Test hygiene | One key-value test and the final mutant list were not in the tag; one test (`recall_authorization_is_the_intersection_of_every_grant`) relies on the 250 ms default deadline and can fail on a cold CI runner; five rustdoc warnings in `src/property_graph`. | 0.7.2 |
| Differential harness | The allowlist of reviewed OpenAPI tightenings does not yet list the `ttl_ms` minimum. | 0.7.2 |

## 0.7.2 plan

1. Pick the test and evidence commits that missed the tag (the key-value mutant test and list, the request guard's re-run record).
2. Re-qualify the parked size lanes on the integrated tree: replay their mutant ledgers, re-point the shadow-copy gate at the
   final key-value visitor commit and run it, then decide what is activated.
3. Work through the table above.
4. Next feature, one at a time: key-value batch and multi-get, then leases and fencing tokens, then memory facts and erasure.
