# Independent REST memory wire oracle

## Result and boundary

`memory_wire_oracle.py` implements an independent, standard-library-only Python
3.11-compatible logical/schema oracle. Nine static JSON files contain **163
exchanges and 386 structural cases (549 named cases)**. Both normal and optimized
self-tests pass: **322 rejected cases repaired, 111 orthogonal second defects
detected, 39/39 named mutants killed, zero survivors, zero equivalent mutants**.

This qualifies neither a running REST adapter nor Rust/storage parity. No Rust
source, Rust output, production digest, provider, network request, OpenAPI,
protobuf, client, or CI implementation was consulted or changed. Expectations
were authored independently from the serving contract and Python references;
the oracle never generates an expectation that it subsequently checks.

Covered structural components include every named JSON schema in serving-memory
contract section 8, all nine method/path/operation-ID combinations, canonical
decimal response strings and exact integer inputs, closed objects, duplicate
JSON keys, nullable-versus-absent fields, conditional unions, finite/binary32
vectors, typed metadata shapes, error/status mappings, and 429 Retry-After.
Byte/count boundaries include raw 8 MiB request sizes (including whitespace),
4,096-byte UTF-8 content, 256-byte opaque/source fields, 64 metadata fields,
4,096-byte metadata strings, 4,096/4,097 encoded provenance bytes, 4,096 items,
reference/scopes limits, and every MemoryLimits minimum/ceiling and adjacent
out-of-range value. Encoded metadata and reserved-field exclusions are OPEN
below; structural validation does not pretend those unspecified checks passed.

## Named exchanges per operation

Shared structural cases are stored with Remember; counts below are file-local.
Structural pending/fact successes describe schema shapes only, not executable
capabilities. No pending erasure execution success is invented.

| Operation | Exchanges | Success | Refusal/error | Structural |
|---|---:|---:|---:|---:|
| Remember | 78 | 25 | 53 | 361 |
| Recall | 44 | 18 | 26 | 3 |
| FindFacts | 1 | 0 | 1 | 3 |
| CorrectFacts | 1 | 0 | 1 | 7 |
| Consolidate | 12 | 4 | 8 | 2 |
| Forget | 16 | 2 | 14 | 4 |
| GetReceipt | 9 | 3 | 6 | 2 |
| GetErasure | 1 | 0 | 1 | 2 |
| ResumeErasure | 1 | 0 | 1 | 2 |
| Total | 163 | 52 | 111 | 386 |

Remember covers creation, exact dedup, source-kind independence, guarded merge,
typed replay, live replay preceding deadlines, retry-window endpoints, scope and
binding refusals, forward/retained references, and a final-item atomic refusal.
Receipt lookup pins operation/scope/key identity, expiry equality, read-only
access and preservation of deletion caveats.

Recall consumes an independently injected candidate/view vocabulary, rechecks
scope/kind/TTL and typed filters, and reuses the independent rational ranking
reference. Cases pin default/mixed/scoped/ID ties, graph-only R=0, independent
T/C, future refusal/clamp, marked partial reads, exact/vector-ID ties, nontrivial
MMR diversity and work preflight. BM25/RRF diagnostics are *injected leg results*,
not a fabricated independent analyzer or fusion algorithm (OPEN-G1W-08).
Provider-off and unsupported bounded/ANN/MMR plans have explicit refusals.

Forget covers small logical deletion, replay after deletion, missing targets,
revision/attachment/last-target atomic refusals, and notices. TTL owner visibility
at equality is covered in Recall; TTL request/response shapes are validated.
TTL mutation success revision mapping is deliberately excluded (OPEN-G1W-05).
Consolidation covers retained sources, replay, canonical source sets, stale
revision/expired-source failures, and the serving 64-input cap. The accepted
write and C1 Python planners are reused without changing their fixtures.

## Refusal inventory

| Code | Named exchanges |
|---|---:|
| invalid_input | 50 |
| memory_capability_unavailable | 18 |
| forbidden_scope | 9 |
| precondition_failed | 5 |
| receipt_not_found | 5 |
| memory_not_configured | 4 |
| deadline_exceeded | 2 |
| invalid_json | 2 |
| memory_bounded_plan_unsupported | 2 |
| memory_feed_unsupported | 2 |
| memory_not_found | 2 |
| memory_unsupported_ranking | 2 |
| idempotency_conflict | 1 |
| index_not_ready | 1 |
| memory_attachment_conflict | 1 |
| provider_not_configured | 1 |
| read_only | 1 |
| tenant_suspended | 1 |
| tenant_unavailable | 1 |
| unauthorized | 1 |

The 18 absent-capability exchanges are: FindFacts, CorrectFacts, GetErasure and
ResumeErasure (one each); Remember/Recall fact/relation (four); Forget
scope/session/entity/series and fact/relation IDs (six); Consolidate fact/relation
inputs and source deletion (three); and gated temporal expansion (one).
GetReceipt for absent correct_facts/resume_erasure identities is 404
receipt_not_found, rather than inventing a capability error for receipt lookup.

Each rejection has exactly one declared defect and repair. Seventeen absent-core
cases use an explicit **gate-only repair**: enabling the capability demonstrates
that the gate is repaired, without inventing an unaccepted successful operation.
The retention request's repaired shape is checked structurally only because its
success mapping is OPEN. All other exchange repairs must execute successfully.
Every repaired exchange also receives an orthogonal unknown-field defect and
must reject it. Error message strings are validated but their English wording
is not compared; code/status/envelope/body structure are compared.

## Mutation evidence

The `MUTATIONS` registry names each deliberate defect and its exact killing
golden/specimen. Kills must occur at that named case; crashes and unrelated
exceptions fail the test, not count as kills. Mutants cover unknown/required/
duplicate fields; numeric outputs, decimal canonicalization and overflow;
UTF-8, content, metadata count/string caps; booleans as integers; deadlines,
retention, replay expiry/flag/conflict; scope widening and owner TTL equality;
feed/provider fallbacks; candidate default/ceiling and output relationships;
weights/decay; method/status/error envelopes; erasure notices; historical and
retained-state limitations; MMR hybrid refusal; and consolidation caps.

Normal and optimized results are identical: **39 killed, 0 survived**. There are
no excluded/equivalent mutants. Baseline expected responses and actual responses
are both validated independently of the mutation under test.

## OPEN items (not guessed expectations)

**OPEN-G1W-01 - Additional accepted fields and digest identity.** Section 3:
"Fields A1 does not hash (vectors, metadata, evidence, confidence, validity,
policies, limits, erasure targets/tokens) join the digest only in the core slice
that accepts them, under a new version with byte goldens published before adapter
code". Section 8 currently permits only `memory-request-v1`. Shapes are tested,
but no new-field success/replay/digest or evidence-attachment wire mapping is
fabricated. Requests requiring those mappings report an explicit unmodeled error.

**OPEN-G1W-02 - Reserved metadata names.** Section 4: "the core slice that first
accepts caller metadata publishes the reserved names (A1 accepts none)".
There is no published caller-metadata collision set here. No name is guessed
from stored properties; reserved-name collision goldens remain blocked.

**OPEN-G1W-03 - Exact metadata property accounting.** Section 4 requires
"65,536 encoded property bytes" and "whole reserved+caller object must fit".
Section 8 defines typed JSON scalars, but neither a wire-to-stored-property
encoding nor the reserved object's field set/size. The erasure reference's
reserved-node encoder is not a general caller-metadata encoding specification.
Consequently 65,536/65,537 *whole-object* metadata byte goldens are deliberately
absent. Count, scalar type and decoded string-byte boundaries are covered.

**OPEN-G1W-04 - Receipt accessors and configuration rotation.** Section 3:
"Their exact encoding, generation handling and response fixtures still require
implementation acceptance"; Core alignment: "A1 returns no receipt ID".
IDs, sequence/view and window are explicit fixture injections, never predicted
production keys/hashes. No binding-generation or rotation success is claimed.

**OPEN-G1W-05 - TTL outcome revisions.** Section 5 requires an "`expired` outcome
and the committed deadline"; section 3 Core alignment says per-item "`deleted`,
`expired`, `retained_shared` outcome values are not core outcomes yet".
Section 8 requires an outcome revision but supplies no retention-set exchange
or TTL-to-logical-revision rule. Logical deletion follows the explicit section 8
deleted exchange. TTL mutation successes and shared-record outcome projections
await a specified revision/result mapping; their shapes and TTL visibility are
tested without inventing that mapping.

**OPEN-G1W-06 - C1 planner/stale-cascade mapping.** Section 8's dry-run numbers are
"an injected planner fixture ... not measured/estimated frame sizes".
The memory model states C1 is "one retained-source Episodic summary" and the
remainder, including "stale-source reporting", "is not implemented".
No dry-run physical count/byte estimator, output-reference/merge planner, or
stale-cascade completion result is fabricated. Stale source revision and expiry
checks use the accepted C1 reference; dry-run responses are structural fixtures.

**OPEN-G1W-07 - Exact selection with hybrid input.** Serving section 4 says exact
selection preserves "text-only candidates"; accepted private B says:
"Vector-only exact/MMR rejects text, seeds, expansion and non-relevance ranking
weights". Successful and refused exact-hybrid executions are excluded pending
alignment. Vector-only exact and explicitly refused hybrid MMR remain covered.

**OPEN-G1W-08 - BM25/RRF discovery math.** The memory model says "existing indexed
BM25 and RRF"; the ranking reference explicitly models "an already-bounded
eligible set", not retrieval. The serving text specifies neither tokenizer,
BM25 constants/index statistics nor the RRF fusion constant. Leg scores/ranks
are injected observations for wire projection, not independently computed
retrieval scores. No production-output-derived golden fills this gap.

**OPEN-G1W-09 - Unspecified HTTP details.** Section 4 defines unary POSTs and
"no memory DELETE/GET/HEAD alias", but does not choose the rejection status/body
for an unsupported method/path. Section 3 says "No tenant field, tenant header"
is accepted without enumerating header spellings or an HTTP error mapping.
Section 6 requires `{error:{code,message}}` but does not prescribe message text.
Wrong methods/paths are structural failures, not invented HTTP error exchanges;
headers never supply tenant/scope/idempotency. Provider-off uses the explicitly
inherited `provider_not_configured` row in serving-contract.md's Gap 8 table.

**OPEN-G1W-10 - Zero-norm cosine.** Section 4 selects "vector-only cosine
selection" and existing reranker ties but specifies no zero-norm convention;
the existing ranking Python reference excludes MMR/distance recomputation.
No zero-norm vector result or refusal is guessed. Nonzero unit-vector exact/
MMR fixtures use hand-checkable distances and diversity selection.

## Checks and proposed CI

Passed on available **Python 3.15.0**, with bytecode writes disabled:

```text
python -B tests/memory_wire_oracle.py --self-test
python -O -B tests/memory_wire_oracle.py --self-test
python -B tests/agent_memory_oracle.py --self-test
python -B tests/memory_ranking_oracle.py --self-test
python -B tests/memory_erasure_oracle.py --self-test
python -B tests/c1_consolidation_oracle.py --self-test
git diff --check
```

Python's `ast.parse(..., feature_version=(3,11))` also passes. An actual Python
3.11 interpreter is not installed, so a Python 3.11 runtime check could not run.
Rust/cargo, live REST/provider/storage/concurrency tests and CI execution are
outside this Python-only scope and were not run. Logical rollback and existing
reference regressions do not establish physical atomicity or durability.

Proposed CI additions are the first two commands above and registration of
`memory_wire_oracle` in `tests/ci_workflows.py`. Neither that file nor ci.yml
is edited.

Test artifacts added: `tests/memory_wire_oracle.py`, this report,
`tests/fixtures/agent_memory/wire/README`, and nine JSON families: remember,
recall, facts, correct_facts, consolidate, forget, receipt, erasure,
resume_erasure. Existing oracles, fixtures and contracts are unchanged.
