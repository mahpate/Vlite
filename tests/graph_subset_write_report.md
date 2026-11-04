# Independent write oracle: completed reference package

The Python-only `evaluate_write` reference covers CREATE, keyed MERGE,
SET/REMOVE, DELETE/DETACH, schema enforcement, captured time, transactional
planning and actual body/operation limits. It imports the accepted Python
front end and value/read primitives unchanged. No subset Rust source was read;
no Rust was run or transliterated. Expected graphs, operations and body counts
are hand-authored from the design and storage contract.

## OPEN-W01 RESOLVED: pinned body bytes

R-HP1-01 authorizes existing storage-format facts, not subset code. The byte
contract is DO-003, `docs/projects/feature-agent-graph/decisions.md:85-145`,
as superseded for reclaims by `:1090-1116`. All integers are little-endian.
The cap is body-only: 16,777,216 bytes, not prefix, encryption or footer bytes.

Hand derivation (M is encoded metadata; L a label list; U an unset list):

| Structure | Bytes | Document |
|---|---|---|
| Body header | 4 | decisions.md:105-112 |
| Entry envelope | 8 | decisions.md:114 |
| M | 4 + sum(2 + UTF8(name) + tag/payload) | decisions.md:116-125 |
| L | 1 + sum(1 + ASCII(label)) | decisions.md:132-133 |
| U | 1 + sum(2 + UTF8(name)) | decisions.md:133,136,143 |
| PutNode | 8 + 8 + 2 + UTF8(key) + L + 4 + M | decisions.md:132 |
| PatchNode | 8 + 8 + L(add) + L(remove) + 4 + M(set) + U | decisions.md:133 |
| PutEdge | 8 + 24 + 1 + ASCII(type) + 2 + UTF8(key or empty) + 4 + M | decisions.md:135 |
| PatchEdge | 8 + 8 + 4 + M(set) + U | decisions.md:136 |
| DeleteNode / DeleteEdge | 8 + 8 | decisions.md:134,137 |
| Reclaim | 8 + 8 (clock) | decisions.md:1104-1106; C1 contract:322 |

Tag/payload: String 1+4+UTF8(value); Integer/Float 1+8; Boolean 1+1;
GeoPoint 1+16 (`decisions.md:125`). Empty metadata still costs 4.
Independent worked precedent: `gap1-c1-consolidation-contract.md:316-323`.

Cross-check **after hand derivation**: `engine/codec.rs:38-40,370-387,508-567`
and `engine/properties.rs:8-11,57-80` agree. Reclaim's u64 clock:
`codec.rs:156-164,473`; lowering: `engine/mutate.rs:301-317,374-379,412-423,
433-440,556-565`. Node deletion drops attachments without extra Detach entries.
One opening reclaim costs 16 regardless of how many due entities it removes.
`engine/limits.rs:31-35` gives 4,096 counted operations and the body cap;
the obsolete all-entry wording is superseded by the Gap 3 frame rule.
No mismatch or new OPEN byte item was found.

## Delivered semantics and milestones

Milestone 1 passed 36 scenarios under normal and optimized Python after
infrastructure, byte pinning, CREATE/MERGE/SET/REMOVE and this report's first
version. Milestone 2 passed 67 after DELETE/DETACH and binding lifetime rules.
Milestone 3 passed 110 after schema, time and the real byte/operation limits.
Milestone 4 passed 160 with 32 killed switches and 38 named kills. The final
164 include three explicit execution-clock NOT_MODELED scenarios and the
storage commit-sequence-exhaustion boundary.

All modeled failures return the entire original graph, no operations and
zero frames. Successful results compare complete graphs including schema,
vector records, attachments, IDs, high-water marks, expiry and revisions.
Staged operations are reference-normalized plain dicts with resolved IDs,
not a proposed Rust/wire serialization. CREATE records include the allocated
ID and frame revision; patch/delete records include the captured revision,
then `any` for subsequent operations under the same conceptual exclusive guard.
Each changing statement has exactly one frame. A create-then-delete still
commits its high-water advance. MERGE hits and identical edge retries do not.

The absent-member REMOVE is a successful typed patch: one operation and a new
revision, not an error. Thus its frame changes revision even if member contents
are equal. MERGE hits and identical keyed CREATE edge retries emit no operations.

MATCH is materialized once, then clauses run in textual order over canonical
rows. A fixture actually removes MATCH's required label before the next clause;
the rematerialize-MATCH mutant dies. CREATE acts per row; patches/deletes dedup
entities within a clause. Binding identity remains the ID even if its key is
recreated. RETURN and its caps are validated before the frame boundary.

DETACH enumerates physical retained incident edges lazily, charging each
examination and bounding the discovery set before insertion. It explicitly
deletes ascending edge IDs before ascending node IDs, including self-loops
and closed temporal series. Node deletion drops attachments, not vector records.
Schema checks run after each operation, not merely on the final state.

The captured C is fixed through MATCH, cleanup, clauses and commit planning.
Node/edge expiry at C-1/C/C+1, temporal `[from,to)`, expired-key reuse,
reclaim-only no-frame, cascade cleanup, cleanup rollback, temporal patch/MERGE
refusal and explicit whole-series deletion have witnesses.

## Exact cap witnesses

`ops-4095`, `ops-4096`, `ops-4097` use real statements over that many node rows.
`ops-4096-plus-reclaim` keeps all 4,096 user operations with an additional
computed reclaim. DETACH separately accepts 4,095 edges + one node, and
refuses 4,096 edges + one node during discovery, before an oversized set.

For `body-at-cap` / `body-over-cap`, each of 4,000 edge patches sets a
66-byte field to a 4,096-byte String:
`8+8+4+(4+2+66+1+4+4096)+1 = 4194`.
The shared source is patched once with a one-byte field and a 1,177/1,178-byte
String: `8+8+1+1+4+(4+2+1+1+4+length)+1 = 35+length`.
Thus `4 + 4000*4194 + 1212/1213 = 16777216/16777217`.
These statements stage 4,001 user operations, within the operation cap.

The reclaim-inclusive pair uses an unrelated due node and a 1,161/1,162-byte
tail: `4 + 16 + 4000*4194 + 1196/1197 = 16777216/16777217`.
No reduced test cap or fabricated size replaces the actual body check.
Omit-envelope and omit-reclaim switches make named over-cap statements succeed
incorrectly; both are killed. Expected arithmetic is stored in each citation.

## Inventory and qualification

164 local scenarios: create 12, merge 13, patch 16, delete 11, detach 6,
binding 19, schema 13, time 21, limits 34, atomicity 7, validation 8,
explicit NOT_MODELED 4.

The hand-built campaign has 32 switches, 38 named kills, zero survivors and
zero equivalent mutants. Every mapped case executes; exceptions never count
as kills. Switch restoration is protected by `finally`, followed by rerunning
36 distinct mapped baseline cases. The map pins every switch and real case ID.

Other checks: 46 malformed container/map/raw-JSON trials; 14 independent
metadata/field/key/label/unset/signed-zero boundaries; three rehashed,
structurally valid but wrong expected goldens; five seeded case/entity/parameter
order shuffles on small graphs; AST-input parity; actual input/parameter/control
immutability; both canonical content hashes. Large arrays use only bounded
literal templates with decimal-index substitution, never evaluator output.
Full-state equality distinguishes Float/GeoPoint signed-zero bits.

## Explicit exclusions and remaining questions

OPEN-W01 is RESOLVED. No new blocking OPEN item or document/codec mismatch
was found. Existing read-reference uncertainties are not newly frozen here:
exact competing-work/sort policy, scratch layout, and the public error kind
for default C-to-i64 overflow remain inherited reference policies rather than
newly approved API decisions. This reference follows the read module's
conservative InvalidQuery without a reason for that checked-conversion failure.

Work accounting is a deterministic reference ledger: scanned nodes, key
resolutions, predicates, fixed-hop canonical comparisons, projections,
adjacencies and staged operations charge shared work; DETACH never resets it.
Body/operation limits and exposed counters are normative contract witnesses,
not a claim that every Rust allocation or sorting schedule has been specified.
`binding_rows` counts materialized rows retained after WHERE; insertion limits
also count those later filtered. `detach_edges_examined` includes repeated
examinations at two selected endpoints. Reclaim counters/bytes are zero when
no frame commits, even if tentative cleanup found due entities.

Deadline/scratch values are validated, but execution cost, cancellation, guard
hold, append/sync faults, read-only/poisoned storage, durability and reopen are
NOT_MODELED. Supplying fake elapsed/cancellation controls returns NOT_MODELED
with unchanged graph, never a success-shaped simulated write. These require
separate implementation/fault qualification. Storage commit-sequence exhaustion
also returns explicit NOT_MODELED rather than inventing a public storage error
or a successful graph with a revision outside u64. No public API, serving, real
database, exactly-once, TCK-conformance or release claim is made.

## Final evidence and proposed CI

All commands below exited 0 on Python 3.15.0 / Git 2.56.0.windows.4.
Python 3.11 syntax was independently parsed with `ast.parse(feature_version=(3,11))`;
a Python 3.11 runtime was not available for a separate execution.

```text
python -B tests\graph_subset_write_oracle.py --self-test
python -O -B tests\graph_subset_write_oracle.py --self-test
python -B tests\graph_subset_oracle.py --self-test
python -O -B tests\graph_subset_oracle.py --self-test
git diff --check
```

Each write run reports the counts above. Each accepted oracle run still has
303 parse / 218 read cases, 63 parser / 70 read switches killed, 79 named read
kills, 36/38 malformed trials, 14/75 boundaries, 800 fuzz trials and five seeds.
All six deliverable files have zero CR bytes; no other path was changed.
Rust/build/differential/reopen checks were not run: this is a Python-only
independent-reference package, not the executor qualification.

Propose exactly these oracle-list lines, without applying them:

```text
python -B tests/graph_subset_write_oracle.py --self-test
python -O -B tests/graph_subset_write_oracle.py --self-test
```

Proposed name for `tests/ci_workflows.py`: `graph_subset_write_oracle.py`.
