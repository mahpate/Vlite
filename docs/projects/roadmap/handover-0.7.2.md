# VLite 0.7.2: release handover and what comes next

Status: written when 0.7.2 was tagged. Read it with [RELEASE-0.7.2.md](../../RELEASE-0.7.2.md) (what changed),
[handover-0.7.1.md](handover-0.7.1.md) (the table of open items this release works through) and
[next-major.md](next-major.md) (the roadmap). This page records how the release was qualified, what it deliberately leaves
out and what is parked. It holds no credentials, subscription identifiers or private infrastructure names.

## What 0.7.2 contains

- **Key-value batch and multi-get.** An atomic batch of up to 1,000 puts and deletes in one collection and a read of up to
  1,000 keys, over REST, gRPC, the MCP tools, the shell, the Rust API and the Python, JavaScript, .NET and Go clients (lane
  F2; contract in [kv-batch-contract.md](kv-batch-contract.md)).
- **Key-value in the embedded libraries.** Get, put, delete and list through the C library, Python, Node.js and WebAssembly;
  no batch, multi-get or lease there (lane F3).
- **A gRPC fix for library embedders.** A gRPC adapter built after a configured REST router now applies that router's memory
  reservations, as it already applied its tokens. `vlite-server` was never exposed because it refuses gRPC while memory is
  configured (lane F3; record in [f3-memory-grpc-proposal.md](f3-memory-grpc-proposal.md)).
- **A dashboard that signs in while agent memory is enabled.** The 0.7.1 dashboard read the 403 that the database-wide routes
  answer in that mode as a failed sign-in (lane H1). The server and the security model did not change.
- **Hygiene** (lane H2): the `ttl_ms` entry of the differential allowlist, the rustdoc warnings of `src/property_graph`, an
  explicit deadline for one recall test, headroom for two provider-latency tests and the missing `204` and error entries of
  the collection batch route in the OpenAPI document.
- **Guides.** Forget and expiry are logical deletion; the client guides say how to settle a mutation after a timeout.

The size work is not in 0.7.2: nothing compact is active, defaults are unchanged and no size claim is made. The compact lanes
were re-qualified on an integration branch (see [What is parked](#what-is-parked)).

## How the release was qualified

- **Candidate discipline.** One release branch received only accepted work. Each lane was gated, hand-mutated and reviewed by
  independent reviewers before integration (F2 in three rounds, F3 in six, H1 four reviews, H2 one). Three more reviewers
  (skeptic, advocate and architect) then read the integrated candidate and the release notes; all three approved with fixes,
  and the fixes were applied before the tag.
- **Hand-mutation campaigns.** Key-value batch and multi-get: 72 mutants, all killed (`tests/f2_mutants.json`). Surface parity:
  66 mutants, all killed (`tests/f3_mutants.json`); six mutants of the C library survived an earlier head and were killed by
  tests added afterwards.
- **Suites.** The full workspace run on the final F2 head: 3,975 tests passed, none failed (7 ignored, as in CI). Lane F3's
  gates ran 1,583 library tests and 456 tests in 45 integration targets, all passing.
- **Live checks on the candidate.** An upgrade from 0.7.1 to 0.7.2 and back to 0.7.1: 51 checks. A test on a copy of the
  hosted app's database: 26 checks, copy deleted afterwards. `scripts/kv_concurrency_check.py`: about 33,600 operations, no
  violations. `scripts/memory_live_e2e.py`: every section passes except the documented skips; the .NET section could not run
  on the test host and is covered by the CI clients job. `scripts/rest_differential_check.py` against 0.7.1: 1,022 requests,
  20 differences, all of them the four dashboard asset routes under five credential kinds (the dashboard fix); no difference in
  an API answer.
- **CI.** The full workflow passed on the tagged commit in all 24 jobs. One job (macOS, Rust 1.89) failed once in an
  allocation-accounting check of the `graph_memory` example and passed when re-run; the example and the code it measures are
  not changed by this release. The release workflow as a dry run, the Dashboard UI workflow and the Pages checks (on a copy of
  the tagged tree plus one workflow file) passed as well. The tag sits on exactly the tested commit.
- **Benchmarks.** None; benchmarks run for major releases only.

## Decisions in force

- **Publication order.** Tag first; release workflow, PyPI and images verified; then `main` fast-forwarded to the tagged
  commit. Pages deploys on every push to `main` and its download links need the release to exist, so the older order (both at
  once) left a window of dead links.
- **Batch errors name the operation in the message.** There is no operation-index field. The embedded libraries carry
  single-key operations only in 0.7.2.
- **Memory mode and the dashboard.** The database-wide routes (`/metrics` and the others listed in
  [Memory-bound resources](../../SECURITY.md#memory-bound-resources)) still answer 403 to every credential while agent memory is
  enabled. The dashboard treats that refusal as "off while memory is enabled" only when the credential could otherwise use the
  route.
- **gRPC and memory.** `vlite-server` still refuses `--grpc-bind` and automatic index maintenance while memory is configured or
  reserved records remain.
- **Upgrades of a hosted app** deactivate every old revision, confirm zero replicas and change only the image. The dashboard
  image is separate from the API image and is upgraded with it.
- Size goals S1 to S6 stay best effort; must-not-regress items still block a release.

## Known limits and open items

Closed from the 0.7.1 table: the recall test deadline, the `ttl_ms` allowlist entry, the rustdoc warnings (two private-item
links in `src/property_graph/temporal.rs` and `temporal/activate.rs` remain), the batch route's OpenAPI entry, the dashboard clock
hint (narrowed: another 422 of `remember` or `consolidate` that names no field can still show it) and the vertical touch drag
(handled by the graph; a drag that starts on a label of the 3D graph does nothing). The test and evidence commits that missed
the 0.7.1 tag are on `main`.

| Item | What it is | Suggested release |
|---|---|---|
| Hidden-attempt telemetry, `reserved_hidden_attempt` actor | Attempts are audited but not exported as a metric, and the event has no `actor` field. | next |
| Scanner hardening | The source scan for lookups outside the guard is a text scan; the gRPC scanner fails open for cores not named `*_core`. | next |
| Retrieve evidence names | Names in `evidence.collections` are hidden but not checked against the grant. | next |
| Duplicated code | The key-value page builder exists in the server and in the MCP and shell tools, and the key-value dialect is written in four places (C library, WebAssembly, tool layer, server). A shared-core slice should precede more embedded key-value work. | next |
| Over-long MCP line answer | It is answered with an `id` of null. | next |
| gRPC publication | Sharing the router's tokens and reservations is an interim mechanism until an explicit handle replaces it, and the lane record lists the limits of the change (sections 5.4 and 9 of [f3-memory-grpc-proposal.md](f3-memory-grpc-proposal.md)). Fix them before the gRPC refusal for memory is lifted. | with the gRPC memory service |
| Dashboard with memory on | A tenant editor still sees the raw 403 on the import routes, and the wording says "enabled" when only binding records remain. | next |
| Unmatched routes | A request to a route that does not exist answers 403 to a non-admin credential and 404 to the admin token. | next |
| Batch cost | Batch and multi-get performance is not measured. | next major |
| Release guard | `release.yml` does not require a green `ci.yml` for its commit. A change that does is an optional commit on the CI-speed branch. | next |
| Housekeeping | A `server,grpc` baseline for `scripts/api_surface.py`, a versioning policy, per-release labels in the guides and the Go client's module tag. | next |
| Content budgets | `README.md`, `site/index.html`, `site/features.html`, `docs/CONSOLE.md`, `docs/CONSOLE-ADMIN.md` and `docs/architecture.md` sit within a few lines or words of their budgets (`tests/content_budget.py`): make room before you add text. | any |

## What is parked

Nothing below is active, and none of it is merged. The branches stay on the remote.

- **Temporal graph, memory facts and erasure** (branch `mahpate-f1-temporal-memory`). Contract documents and a partial
  independent oracle for resuming erasure jobs. The rulings ES1 to ES14 for serving erasure jobs over REST, MCP and the command
  line wait for a decision ([gap1-adapter-mapping.md](gap1-adapter-mapping.md)).
- **Self-serve tenant memory** (branch `mahpate-f4-memory-self-serve`). A contract draft with decisions D1 to D19 that wait
  for ratification.
- **Compact storage** (branch `mahpate-int072-final`). Every compact mutant ledger replayed on the integrated tree with the
  strict verifier, all green. Merge it only together with a decision to activate or ship size work.
- **Faster CI and mutation replay** (branch `mahpate-h3-faster-ci`). Workflow changes only; its timing measurements are not
  done. Two optional gate cuts (the minimum-Rust-version jobs on Linux only, and a release that skips its native test re-run
  when CI is green for the same commit) are separate commits and need an explicit decision.
- **Leases and fencing tokens** ([gap2-lease-ledger.md](gap2-lease-ledger.md)) and the gRPC memory service are not started.
