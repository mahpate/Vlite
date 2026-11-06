# F3 decision brief: memory in the embedded bindings

Status: **decided on 2026-11-05: option R (through the coordinator, on the owner's delegation). No public embedded memory API for 0.7.x; memory stays on the REST clients, MCP, CLI and gRPC. Revisit for the next major after lane F1 settles the core types.** Nothing in this brief is implemented. Lane F3, branch `mahpate-f3-surface-parity`, base `b4673670`. It asked one question: should the embedded bindings (Python `vlite-embedded`, Node, WASM, C ABI) expose agent memory, and through which public Rust surface? Goal (b), the key-value API in the same bindings, is separate; the note at the end records that decision.

## 1. Facts

- **The memory core is crate-private.** `src/memory.rs` and its submodules export nothing: `MemoryRef`, `MemoryMut`, `RememberRequest`, `ForgetRequest`, `RecallRequest`, `MemoryError` and the rest are `pub(crate)`. The only consumer is the REST adapter (`src/server/memory/**`). `vlite-mcp` and `vlite-memory-cli` reach memory through an authenticated server, never by opening a file.
- **There is no embedded authorization.** Tenant, principal, binding grant, exact scope and `raw_access` exist only in the server layer. The core takes a `MemoryScope` as trusted input. Whoever opens the file can already read and write the memory graph and evidence collection directly. The contract says so: "Embedded file access and operator backups remain outside server scope enforcement", and the embedded, C and WASM bindings "must not claim server tenant enforcement".
- **Decision 76** (memory contract section 11): crate-private accessors only; no public Rust API and no new `Error` variant without a separate owner decision. `gap1-agent-memory.md` section 2 proposes borrowed handles (`memory_mut.remember(...)`, `memory.recall(...)`) and marks the types and error names as needing public API review. That review has not happened.
- **A server owns the binding record.** Each bound graph holds a `binding:v1` node with name, tenant, generation, state and receipt window. Startup adopts a graph only when it is empty, holds the binding's record, or holds nothing but memory nodes, and refuses a binding whose evidence collection already holds ordinary data. A database that embedded code filled with memory could be adopted by a later server only if the embedded API follows the same conventions (record, generation, evidence ownership), and each of those is a rule the server would enforce on the next start.
- **A one-way effect exists.** A memory that names an existing evidence record (item evidence references, 0.7.1) makes a 0.7.0 server answer 500 for that scope. An embedded API that exposed evidence references would carry the same effect into files written by embedded code.
- **Gates.** `scripts/api_surface.py check` compares the public item list of the default build with a baseline and fails on any addition outside `vlite::property_graph`, so a public `vlite::memory` module changes the baseline. Every public item also needs a `site/api.html` row with a `data-api` key (`tests/api_reference.py`).
- **Lane F1 is changing the core now.** Facts and erasure add and change core types and error cases. A public API frozen while they move would freeze half-settled shapes.
- **What exists today for a non-Rust embedded user:** the REST clients (Python, JavaScript, .NET, Go) against a local or remote `vlite-server`, the MCP tools and `vlite-memory-cli`, and, after goal (a), the gRPC `Memory` service.

## 2. Options

| | What ships | New public surface | Work | Main risks |
|---|---|---|---|---|
| **R** REST clients only (and MCP, CLI, gRPC) | Nothing new for the embedded bindings. Their docs say memory is served over the network adapters and name the clients. | None. | None beyond documentation. | An in-process agent must run or reach a server; there is no memory without a process boundary. |
| **J** JSON dialect through one entry point | One handle type and one `execute`-style method: JSON in, JSON out, the same request and response JSON as REST. The C ABI gets an `{"op":"memory",...}` envelope on `vlite_execute` (no new symbol, like key-value), and Python, Node and WASM get a thin `memory(binding)` object. | A handful of items under a new public module, plus the binding-provisioning call. | Large. The strict JSON parsing and rendering now living in `src/server/memory/{wire.rs,routes/*}` (axum and server types included) moves into the core crate behind the `json` feature, and the server calls it. The same code then serves REST and embedded callers, and the wire goldens in `tests/fixtures/agent_memory/wire/` replay against every binding. | A refactor of the most heavily mutation-tested code; decision 76 and an API review first; embedded trust model to write down; binding record and adoption rules; F1 churn. |
| **P** typed public Rust API plus per-language wrappers | `GraphRef`-style handles with typed requests and results, an error type, and the four wrappers each mapping every type. | Dozens of public items (requests, results, enums, errors) and a public error taxonomy. | Largest. Every language maps typed shapes (TypedDict, `.d.ts`, wasm-bindgen types) and every shape needs docs rows. | Same as J, plus a public error taxonomy that decision 76 withholds, and typed shapes that F1 changes. |
| **U** unstable feature flag | Option P or J behind a Cargo feature excluded from the default baseline and from semver. | Same items, outside the default API baseline. | As J or P, plus the flag plumbing in four packages. | An "unstable" label that users build on; `api_surface` treatment of feature-gated items is not defined; the same F1 churn. |

## 3. Recommendation

**Option R for the 0.7.x cycle.** Revisit **J** for the next major, after F1 has settled the core types and the owner has decided the embedded trust model.

- No option removes the need for decision 76; J and P also need an error taxonomy that nobody has reviewed.
- The memory features that matter most (grants, exact scopes, receipts shared by several callers, reserved resources) are server features. An embedded memory API would offer the operations without the protections, and the contract forbids implying the protections exist.
- F1 is rewriting parts of the core. Shapes frozen before F1 lands would be revised once F1 lands.
- Every language already has a path: the REST clients now, gRPC after goal (a).
- R costs nothing, changes no file, and leaves J open. J is the right shape if the owner wants it later: one dialect for REST, MCP, CLI and the bindings, validated by the same goldens.

## 4. Before any code under J or P

1. An owner decision on decision 76 (public Rust API and `Error` surface), with the F1 core types settled.
2. A written embedded trust model: scopes are labels, not boundaries; no tenant or `raw_access` claim.
3. Provisioning rules: who creates the binding record and evidence collection, and which generation and receipt window an embedded binding carries so a later server can adopt it.
4. A decision on item evidence references (omit them from the embedded API, or accept the one-way effect and document it).
5. For J, the extraction of the parser and renderer behind the `json` feature with the REST tests as the safety net, as its own reviewed slice before any binding work.

## 5. Goal (b) note: the key-value bindings

Key-value in the four bindings follows `serving-kv-ttl-contract.md` section 9, except that decision (i) of its section 12 (an open proposal, not a ratified choice: "C ABI graph TTL only; WASM explicitly unsupported; embedded KV and leases wait for the Rust slices") predates the owner's request for all four bindings. **Decided on 2026-11-05 (through the coordinator): the owner's request ("memory and KV in the Python, Node, WASM and C bindings") supersedes decision (i) for get, put, delete and list in these four bindings; the C ABI version stays 1 and no C symbol is added; the note is recorded in the contract.** The embedded Rust key-value API (`Database::kv_store`, `kv_view`) is already public, so the four existing operations need no new Rust slice; leases do, and stay out. The four operations use the JSON dialect of the local MCP and shell key-value tools (`{"op":"kv","request":{...}}` through `vlite_execute` for the C ABI). Memory is not part of this supersession: it stays out of the embedded bindings (option R above). Batch and multi-get (lane F2) and leases follow, each as one match arm and one thin wrapper per binding.
