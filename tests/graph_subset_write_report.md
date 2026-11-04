# Independent write reference: blocked before implementation

Status: WIP; no executable write reference, write fixtures, or qualification
claim. The accepted read reference is unchanged.

## OPEN-W01: exact operation-body encoding is not specified in permitted inputs

The design requires a limit of 16,777,216 **encoded body bytes**, including
reclaims, not the size of the query, parameter map, user operations, or JSON:
`docs/projects/roadmap/gap6-cypher-subset.md:32-33,382-384,435`.
It also requires complete validation before append
(`docs/projects/roadmap/gap6-cypher-subset.md:372`).

The permitted ground truth establishes:

- `src/property_graph/engine/limits.rs:31-35` fixes the operation and byte caps,
  but supplies no body serialization grammar.
- `src/property_graph/mod.rs:107` imports `BODY_HEADER_BYTES` and
  `ENTRY_HEADER_BYTES` from the codec; it does not define their values.
- `src/property_graph/mod.rs:635-661` gives a lower-bound frame-prefix check.
  Its reclaim clock is eight bytes and its expiry payload is sixteen bytes,
  but this is not a complete body-size function. In particular it does not
  settle the serialized sizes of node/edge creation, patches, deletion,
  attachments removed with nodes, or variable-length keys and label lists.
- `src/property_graph/mod.rs:777-788` validates the state, delegates body
  encoding to `engine::encode_body`, then appends the encoded body.
- `src/property_graph/engine/state.rs:835-839` explicitly delegates enforcement
  of the encoded-byte limit to `super::codec::encode_body`.

The accepted metadata encoding in design section 12(l) and
`tests/fixtures/graph_subset/README.md:135-141` settles parameter/property
count/name/tag/value sizes. It does **not** settle the surrounding graph body
or the lowering from a GraphOp to persisted entries. Substituting it for the
body would silently omit required overhead. A minimum-size inequality cannot
establish equality at the cap.

**Conservative disposition:** leave the write model unimplemented. Do not
invent header constants, count JSON bytes, use arbitrary operation-size
estimates, omit reclaim overhead, or report successful modeled writes without
the mandatory byte check. There is no adopted byte ruling or boundary case.
This is an unresolved evidence boundary, not a finding of a Rust defect or
a contradiction in the approved design.

**Proposed resolution, not ratified:** provide an independent, frozen
operation-to-entry and entry-to-body byte contract, including all fixed
headers, length prefixes, optional fields, property encoding, reclaim
entries, and deletion/attachment effects. Alternatively explicitly authorize
the additional codec/frame/planner evidence needed to establish that contract;
this would require a renewed independence review before implementation.
No additional implementation source was read to bypass the boundary.

**Cases to author after resolution:** `body-at-cap` (16,777,216 bytes),
`body-over-cap` (16,777,217), and reclaim-inclusive counterparts. Each needs
hand-reviewed byte arithmetic and a named omitted-overhead mutant kill.
These are proposed case IDs only, not delivered or passing scenarios.

## Delivered and excluded scope

Only this investigation is delivered. There is no `evaluate_write` function;
no CREATE, MERGE, SET, REMOVE, DELETE, DETACH, schema, staged-operation,
counter, full-state, or atomicity campaign is claimed. Zero write scenarios
and zero write switches were implemented; killed/survived/equivalent counts
are all zero because no write campaign ran. The 150-scenario target is unmet.

Execution time, writer-guard hold time, scratch execution cost, cancellation,
I/O, reopen, indeterminate commits, and durability are not modeled.
In particular there is no replacement oracle returning invented success for
these exclusions, and no placeholder self-test that could falsely qualify
missing write behavior.

## Baseline checks

From the isolated repository root, both commands exited zero:

```text
python -B tests\graph_subset_oracle.py --self-test
python -O -B tests\graph_subset_oracle.py --self-test
```

Each reported 303 parse cases, 218 read cases, 63 parser mutant switches
killed, 70 read mutant switches killed, and 79 named read kills.
Other counts per run: 36 parse malformed trials, 14 parse boundaries,
800 fuzz trials, 38 read malformed trials, 75 read boundaries, and five
shuffle seeds in each suite. These are existing read/front-end evidence,
not write coverage.

The requested new write self-test commands, normal and `-O -B`, each exited
2 because `tests\graph_subset_write_oracle.py` does not exist. This is an
explicit incomplete-delivery failure, not a passing test or skipped success.

Python 3.13.17 and Git 2.56.0.windows.4 were available. Rust tools were not
on PATH; no Rust build, installation, or workaround was attempted. Rust is
not needed for the requested Python checks.

## Proposed integration checks, after completion only

Do not add these to CI for this WIP:

```text
python -B tests/graph_subset_write_oracle.py --self-test
python -O -B tests/graph_subset_write_oracle.py --self-test
```

Proposed oracle name for `tests/ci_workflows.py`:
`graph_subset_write_oracle.py`. No workflow, status, public documentation,
implementation, or accepted fixture file was changed.
