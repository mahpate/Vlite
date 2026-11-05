# VLite 0.7.0: release handover and 0.7.1 plan

Status: written at the 0.7.0 freeze. Read it with [RELEASE-0.7.0.md](../../RELEASE-0.7.0.md) (what changed) and
[next-major.md](next-major.md) (the roadmap). This page records how the release was qualified, which limits it ships with and
what 0.7.1 takes up first. It holds no credentials, subscription identifiers or private infrastructure names.

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
  (60 s on 8 threads, 0 violations) and `scripts/rest_differential_check.py` (931 requests per server against 0.6.1, 0 response
  differences, only the documented `rerank.lambda` tightening flagged).
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

## Limits shipped in 0.7.0 and fixed in 0.7.1

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
| KV-3, KV-4, KV-5 | Key-value list sorts every matching key before the limit; a writer can fill a key's 16 slots; the ordinary record route `PUT /v1/collections/{collection}/vectors/{id}` answers 500 to an already due `expires_at_ms` or a zero `ttl_ms` (present since 0.6.1; the key-value route accepts a due expiry by contract) |
| Guards | A request guard type that makes a raw lookup without the reservation check impossible to write (see below) |

Open readings recorded by the mutation work, none pinned by a test: whether consolidate needs the evidence-collection grant
(it touches no evidence in the first slice), whether `allow_partial: true` with an exhausted budget may return a marked partial
answer, `details.field` for a bad consolidate output (`items`), the order of the stale-incarnation refusal against `read_only`,
and whether an evidence alias is the binding name or the collection name.

## 0.7.1

Order and gates come from the storage readiness inventory. Defaults stay unchanged and nothing compact becomes active.

1. **R0** baseline and seam freeze from the 0.7.0 tag; reconcile stale progress text.
2. **B4** finish lane B (18 replay obligations, 10 still open); **A4** record consumers; both before **E4**, the bounded exact scan.
3. **C4** server and MCP consumers and accounting; **K71** bounded key-value, input and audit follow-ups (beside B4).
4. **L5** effective-source lifecycle and generation retirement after A4, B4, E4 and C4.
5. **P4-71** compact graph integration, **ANN71** compact approximate search, **P5-71** persisted summaries then compression.
6. **F4 and F5** measurements of the actual changed behavior after correctness lands.
7. The limits above: **G71** (guards) and **M71** (memory).

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
