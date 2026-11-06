# VLite 0.7.0: release handover and 0.7.1 plan

Status: written at the 0.7.0 freeze. Read it with [RELEASE-0.7.0.md](../../RELEASE-0.7.0.md) (what changed) and
[next-major.md](next-major.md) (the roadmap). This page records how the release was qualified, which limits it ships with and
what 0.7.1 takes up first. It holds no credentials, subscription identifiers or private infrastructure names.

Update after the release: 0.7.1 shipped on 2026-11-05 with the request guard (G71), the agent-memory follow-ups (M71), the
key-value, input and audit follow-ups (K71) and the dashboard, and without any compact work ([handover-0.7.1.md](handover-0.7.1.md)
is its handover). The owner moved the compact lanes of the plan below (R0, B4, A4, E4 and C4) to 0.7.2. What this page
records for items 1 and 2 is carried on the integration tree T (`mahpate-int072-c1` at `2d405125`; [seam
freeze](p3b-seam-freeze.md#1-baseline)).

## What 0.7.0 contains

Key-value store, agent memory (five of nine routes served), the directory store (opt-in), graph expiry and change feeds, bounded
graph algorithms, managed embedding providers and custom ranking, exact-search kernels and the opt-in background index rebuild.
The size work (P3b engine integration, P3c, P4, P5 writers and compression) is parked: nothing compact is active, defaults are
unchanged and no size claim is made.

## How the release was qualified

- **Candidate discipline.** One candidate branch received only accepted work. Each lane branch was integrated after its own diff
  review, gates and hand-mutation campaign, then the whole candidate was reviewed again by independent reviewers.
- **Hand-mutation campaigns.** Every behavior the contract states had to be killed by a test that derives its expectation from
  the contract, not from the code. Counts recorded by the lanes at the freeze: key-value 82 of 82 non-equivalent mutants killed
  plus 10 follow-up mutants; reserved-resource guards 117 hand mutants over the rounds (40, 34, 19, 22 and 2), all killed, with
  the lists in `tests/reserved_resource_mutants.json` and the commit messages; agent-memory routes 352 independent mutants, of
  which 202 were killed on the first run and the other 150 were closed with new tests. A survivors-only rerun on the release head
  confirmed it: 149 of the 150 are killed and one is proven equivalent (the adapter captures the clock once per request, and
  receipt nodes carry no deadline, so recapturing it cannot change a response). Mutant lists with their anchors, the runner and the
  results live on the throwaway `ci-mutants-*` branches until they are deleted; the counts above are the durable record.
- **Independent reviews.** Conformance against the contract (found and fixed an empty-evidence refusal), two security reviews
  (no High or Medium findings), a key-value live review (one wrong error code, fixed), and two rounds of differential reviews of the
  reserved-resource fix. The first round found five answers that differed from a missing resource and one older tenant search
  defect (fixed; see the release notes); the second round re-ran every principal against every route family on the fixed head.
  A later inventory of every handler that resolves a collection name found a second, older tenant defect in the metadata update
  route (also fixed; see the release notes).
- **Live checks.** `scripts/memory_live_e2e.py` (98 checks over real HTTP, 3 documented skips), `scripts/kv_concurrency_check.py`
  (60 s on 8 threads, 0 violations) and `scripts/rest_differential_check.py` (931 requests per server against 0.6.1). The one
  response difference with a cause is the tenant metadata update fix: for a tenant whose own collection has no such record, 0.6.1
  answered `500` after it had written to the root collection of the same name, and this release answers `412`. That extra write
  leaves every later revision, ETag and sequence number on the 0.6.1 server one higher and its file 144 bytes larger, which the
  check reports as 32 follow-on differences. The only OpenAPI difference flagged is the documented `rerank.lambda` tightening.
- **CI.** The full workflow, the release workflow as a dry run and the Pages checks ran on the exact head that was published.
- **Benchmarks.** One DO-005 campaign ran for this major release (see [RESULTS.md](../../../benchmarks/RESULTS.md)). Benchmarks run
  for major releases only; the quiet-host criteria (background CPU mean at most 10%, at most 50% per half-second sample) are
  unchanged.

## Decisions in force

- Startup flags `--providers-file` and `--memory-bindings-file` (environment `VLITE_PROVIDERS_FILE`,
  `VLITE_MEMORY_BINDINGS_FILE`). gRPC `ABORTED` for `provider_snapshot_conflict`.
- Agent memory first slice: remember, recall, consolidate, forget by id and receipt lookup are served; facts query, facts
  correct, erasure get and erasure resume answer 409 `memory_capability_unavailable`; a `remember` receipt cannot be looked up
  (409 `remember_receipt_lookup`); no response size cap on REST, requests up to 8 MiB.
- Reserved memory resources answer like missing ones on reads and like occupied ones on creates, for every caller without
  `raw_access` on the owning binding. `raw_access` lives only in the bindings file; provision raw holders with `collections: ["*"]`.
- Size goals S1 to S6 are best effort; the release reports them and never waits for them. Must-not-regress items still block.
- The Azure Container App stays on 0.6.1 until the owner asks for a deployment.

## Limits shipped in 0.7.0

The table is the state at 0.7.0; [the 0.7.1 handover](handover-0.7.1.md) says what 0.7.1 closed and what it left open, and the
plan below is the one written for it at 0.7.0. The shared commit sequence (F2 and the tenants row) and the import job
numbering describe design properties, so they need an owner decision before anything changes.

| Item | What it is |
|---|---|
| F-01 | Binding identity and receipt window are read from the bindings file at each start and not checked against the database |
| F-03 | Recall with `exact` or `mmr` is vector only; hybrid is refused |
| F-04 | Recall in approximate mode answers 422 `memory_bounded_plan_unsupported` |
| Recall distance | Vector recall measures squared Euclidean distance, not cosine: `exact` orders by it and every selection reports it as `scores.distance`; the contract describes cosine distance for `mmr`, and three goldens that the replay cannot seed expect it |
| F2 | Database-wide sequence counters reveal hidden write activity |
| F3 | Idempotency keys are scoped to binding and memory scope, not to the credential |
| F5 | One node with the `VliteMemoryScope` label satisfies the startup adoption check |
| F6 | Removing a binding ends its reservation at the next start |
| F8, F9 | Collection list ids show gaps where reserved collections sit; the administrator log shows reserved names |
| Quotas and audit | Live vector quotas count reserved records; an attempt on a reserved name is not counted as a credential denial |
| Attachments | A raw credential can attach ordinary records to nodes of its reserved graph (and reserved records to an ordinary graph); nobody without raw access can then delete or detach them until the holder does |
| Provisioning and lifecycle | A tenant that owns a binding cannot be deleted (409 `tenant_not_empty`); a credential created before a binding whose grant names a reserved collection answers 404 where a missing name answers 403; subscriptions and credential graph lists that name a reserved resource begin to match it when the binding is removed; backup counts include reserved resources; a memory agent credential created after the bindings file is in place needs `["*"]` because naming the evidence collection answers 404 |
| Tenants | Import job IDs are allocated for the whole file (another tenant's job answers 403, an unused ID 404) and a first chunk waits behind earlier reserved jobs of any tenant; scoped metrics and import responses show stored names (`t<id>__name`); the commit sequence is shared by all tenants |
| gRPC disk ANN | `BuildDiskAnn` and `SearchDiskAnn` take a client-chosen sidecar path, so a credential that holds the `index` or `search` operation can write or read a sidecar anywhere the server process can; [GRPC.md](../../GRPC.md) already says to restrict these operations to administrators, and the REST routes derive the path on the server. 0.7.1 confines scoped credentials to the admin directory |
| KV-3, KV-4, KV-5 | Key-value list sorts every matching key before the limit; a writer can fill a key's 16 slots; the ordinary record route `PUT /v1/collections/{collection}/vectors/{id}` answers 500 to an already due `expires_at_ms` or a zero `ttl_ms` (present since 0.6.1; the key-value route accepts a due expiry by contract) |
| Guards | A request guard type that makes a raw lookup without the reservation check impossible to write (see below) |

Open readings recorded by the mutation work, none pinned by a test: whether consolidate needs the evidence-collection grant
(it touches no evidence in the first slice), whether `allow_partial: true` with an exhausted budget may return a marked partial
answer, `details.field` for a bad consolidate output (`items`), the order of the stale-incarnation refusal against `read_only`,
and whether an evidence alias is the binding name or the collection name.

## 0.7.1

Order and gates come from the storage readiness inventory. Defaults stay unchanged and nothing compact becomes active.
0.7.1 shipped K71 (item 3) and G71 and M71 (item 7). It shipped none of the compact work: R0, B4, A4 and E4 (items 1 and 2) were
built and are carried to 0.7.2 for re-qualification on an integration tree, C4 (item 3) moved to 0.7.2 not started, and items 4
to 6 were not started in 0.7.1 (the [0.7.1 handover](handover-0.7.1.md) names no release for them).

1. **R0** baseline and seam freeze from the 0.7.0 tag, reconciled progress text and 91 hand mutants of the read seam, of
   which 28 had no replay evidence on `v0.7.0` (12 are reached through the seam's calls, the test-only mapped copy or an
   input that no writer produces; 16 need stored bytes or the in-memory directory to be edited) and are now killed by 16
   of the 19 added tests (each of the other three kills a mutant of its own statement): [seam
   freeze](p3b-seam-freeze.md), [survivor ledgers](p3b-survivor-ledgers.md) and
   [`tests/compact_seam_mutants.json`](../../../tests/compact_seam_mutants.json). Slice 3 adds the ordered cursors that the
   bounded exact scan needs (`base_rows`, `affected_ids` and `ordered_ids`, crate-private and test-gated, `effective_ids`
   unchanged) with 7 tests and 34 more hand mutants, all killed
   ([`tests/compact_ordered_cursors_mutants.json`](../../../tests/compact_ordered_cursors_mutants.json)). The final-tree
   replays ran on the lane's machine, and every mutant that counts on a final tree (35 of the 91, all 34 of slice 3) was
   replayed again on separate machines on R0's integrated tree (`faa3b32e`), all killed. On T the 52 mutants of the two ledgers
   that mutate `records.rs` or `lib.rs` were replayed again, on the lane's machine and on separate machines, all killed, and
   `verify --strict` accepts that replay ([seam freeze, 7.3](p3b-seam-freeze.md#73-the-mutants-of-recordsrs-and-librs-on-the-integration-tree),
   [`tests/compact_r0_integrated_mutants.json`](../../../tests/compact_r0_integrated_mutants.json)).
2. **B4** finished lane B by cherry-picking `250a98bc` (never by merge). Its 18 replay obligations are replayed on B4's own tree: the seven
   that mutate the read seam are in R0's ledger and the eleven for consumers, settings and rerank are in B4's, each gap
   measured on `v0.7.0` before the tests that kill it. B4 added the five survivors that lane B's own report names (four
   are killed by new tests, one is accepted as an equivalent mutant whose invariant a test pins) and three gaps that its
   wider campaign found in the lane's consumer tests, all closed by tests: 48 mutants in `tests/compact_b4_mutants.json`,
   replayed and checked with `tests/compact_mutant_replay.py`. **A4** finished lane A: it cherry-picked lane A's four patches
   (never by merge, the key-value hunks last), rebuilt the key-value group on K71's bounded visitor and added 68 consumer
   tests in nine modules and two resident graph tests, so that key-value reads,
   hybrid search, the query language, grouped search and facets, the record iterator and browse, named vectors and
   re-embedding and graph evidence read compact records under the test gate. Lane A's 47 mutants, 26 of them survivors, are
   replayed and 170 more added: 217 in `tests/compact_a4_mutants.json`, all killed and all accepted by `verify` of B4's final
   tool on A4's head ([lane A4](p3b-record-consumers.md)). A few changes are visible to a resident file and ship in a release
   build (one clock for the exact and text legs of the hybrid search and for the pivot and evidence reads of graph retrieval,
   among them; the approximate leg still reads the wall clock), and `scripts/shadow_copy_check.py` checks that the shipped
   code is that of `v0.7.0` apart from the named exceptions. A ledger run is tied to the file it ran on, so on T the recorded
   replays of B4's and A4's mutants in the files that changed since are stale until they are replayed there (A4's, in
   `records.rs`, `kv.rs`, `lib.rs` and `property_graph`), and two of A4's anchors, `a4-cursorgroup-07` and
   `pa-kv-store-resident-guard-removed`, match twice and need more
   context first ([the ledgers on T](p3b-survivor-ledgers.md#the-ledgers-on-the-integration-tree)). **E4** moved the exact
   scan of lane A onto the lazy `ordered_ids` cursor (one line of `src/lib.rs`, under the test gate) and added five tests and
   24 mutants in `tests/compact_e4_mutants.json`; its recorded runs on T are E4's to add ([E4's page](p3b-exact-scan.md)).
3. **C4** server and MCP consumers and accounting (moved to 0.7.2); **K71** bounded key-value, input and audit follow-ups
   (shipped in 0.7.1).
4. **L5** effective-source lifecycle and generation retirement after A4, B4, E4 and C4.
5. **P4-71** compact graph integration, **ANN71** compact approximate search, **P5-71** persisted summaries then compression.
6. **F4 and F5** measurements of the actual changed behavior after correctness lands.
7. The limits above: **G71** (guards) and **M71** (memory), both shipped in 0.7.1.

Each lane carries its own hand-mutation minimum (12 to 24 effective mutants, more for the codec and durability lanes), an
independent review and the shared-file serialization map in the inventory. No benchmark campaign runs for 0.7.1.

Structural fix for the reserved-resource class: one guard type per request that returns handles with private fields
(collection, graph, graph for write, create), delete the free `open()` and its optional bindings argument, filter reserved names
in the core `resolve`, generate the differential test from the OpenAPI route inventory including body-named collections, make the
`GraphOp` match exhaustive and ban `db.resolve`, `db.collection` and `db.graph` calls in `src/server` outside the guard module
with a source check. Also assert after every mutating request that the reserved resources and the root namesakes of a tenant's
collection names are unchanged: comparing status codes alone missed the metadata update route, which answered normally while it
wrote to the wrong collection.

## Habits that paid off

- Derive expectations from the contract and keep the answer for a reserved name byte-identical to the answer for a missing one,
  including header names, body bytes and the order of validation errors. Compare with principals whose grants cover both names:
  the grant check runs first.
- After a guard resolves a name, use the identity it returned. `db.collection(target.name())` after a guard reads the unqualified
  root name for a tenant caller.
- Deadlines in tests: the server's default budget is 250 ms of real time. Tests that go through the whole stack either inject
  the `Clock` seam or set `deadline_ms` (recall: `limits.deadline_ms`) to 5,000 so a loaded runner cannot return 504.
- Mutation lists: a command must not start with `--offline`, a mutant that targets a function a later fix touched needs its
  `old` anchor re-based before a rerun, and a command that runs only library tests cannot confirm a kill made by an integration
  test.
- Reading the skipped goldens: replays that inject candidate ranks cannot catch value differences such as the recall distance.
- CI: the `docker` and `native (ubuntu-latest, 1.89.0)` jobs can run out of disk; rerun the failed job after the run finishes.
