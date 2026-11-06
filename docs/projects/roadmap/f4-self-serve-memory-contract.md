# Self-serve tenant memory (lane F4): contract

Status: **S0 design, parked for ratification (2026-11-06, owner pause).** No Rust, no worker plan and no slice exists. The four independent reviews of this contract (skeptic, advocate, architect, security) were **not run** before the pause: they are the first step on resume, before any code. Written on `origin/main` at `0eb2bd81` (0.7.1 plus the site card); every `file:line` below is that tree.
Constraints: no public Rust API, no `Error` variant, no engine `FormatVersion`, no proto message and no tenth memory data route (decisions 67 and 76 of the [serving contract](serving-memory-contract.md)). Everything is crate-private and every failure is a server `ApiError` code.
Authority: the [serving contract](serving-memory-contract.md), the [adapter mapping](gap1-adapter-mapping.md), the [0.7.1 follow-ups](m71-memory-followups.md) (binding record), [MEMORY](../../MEMORY.md), [API](../../API.md#start-the-server-with-memory), [SECURITY](../../SECURITY.md).

## 1. Outcome and laws

**Outcome.** An administrator turns memory on for a tenant with one call (later one switch) and nothing restarts. Every editor and viewer of that tenant creates their own memory token, pastes a short MCP configuration into GitHub Copilot CLI or agency, and uses one shared memory (`{agent: team}`) plus a private memory per assistant (`{agent: copilot, user: <label>}`) that the server derives from who they are. Plain `remember` is searchable by words.

| Law | Rule |
|---|---|
| L1 | **One registry.** REST and gRPC read one atomically swappable registry. A change is installed only while the database write lock is held. |
| L2 | **State first.** A managed change is written to the admin directory (S), then to the database binding record (R), then installed in the registry (N). Startup reconciles R and N to S. |
| L3 | **Files keep working.** A deployment that only uses `VLITE_MEMORY_BINDINGS_FILE` behaves exactly as 0.7.1 and can adopt its bindings later without a restart. |
| L4 | **The server derives.** The tenant comes from the token and the personal scope from the token owner's pinned label. A request never names either. |
| L5 | **Least privilege.** A memory token reaches memory routes only: never raw routes, never admin routes, never `raw_access`, and it never mints another token. |
| L6 | **No oracle.** Every refusal on a memory data route is the 0.7.1 403: unknown, ungranted, other-tenant and out-of-scope answers are byte-identical. |
| L7 | **No restart.** Enable, change, retire, purge, member and token changes never restart the server. |
| L8 | **Secrets once.** A token is shown once, stored as SHA-256 and never enters audit, logs, files or error text. |
| L9 | **Reservations never lift early.** A resource stays reserved until its data is deleted and it is dropped. |
| L10 | **Gates stay.** No gate is lowered; a slice that needs one lowered is a decision for the owner. |

## 2. Findings that shaped the design (0.7.1 tree)

| # | Finding | Evidence | Consequence |
|---|---|---|---|
| F1 | The registry is an immutable `Arc<reserved::Bindings>` cloned at request start. The nine routes are mounted only when the file names a binding; otherwise the fallback answers 404 `route not found`. | `server.rs:200,304-311,783`, `guard.rs:87`, `memory.rs:167-173` | A swappable handle, and an always-mounted router behind a gate that answers that same 404 while the registry is empty. |
| F2 | A handler takes its guard before it takes the write lock, so a swap can land between decision and write. `recheck` compares only the binding record. | `collections.rs:272`, `routes.rs:272-292` | Decisions read the registry inside the lock, every install runs under the lock, and a mutation re-authorizes under the lock (3.2). |
| F3 | Startup order: read the file, `with_bindings`, refuse gRPC and automatic maintenance, `memory::startup` (check, write records, provision), orphans, then the admin directory stores. | `admin.rs:228-262`, `memory.rs:97-166` | S loads before `memory::startup`; `prepare(db, &bindings, now)` is reused at runtime for the merged set. |
| F4 | While anything is reserved, 16 blanket `!memory_bindings.is_empty()` checks in six files refuse everyone, the administrator included. Removing the file does not lift them (records keep the reservation). | `observability.rs:190,226`, `ops.rs:906,930,1026`, `webhooks.rs:269,295`, `collections.rs:497,594,606,658,692,787,800`, `graph.rs:260`, `features.rs:801`, `admin.rs:240-262` | Runtime enable flips them: an acknowledgement flag, and the options in section 9. |
| F5 | The record stores only a digest of the explicit grants. Rotation: a changed digest or state needs a higher generation; an unchanged entry must keep it. | `src/memory/binding/record.rs` (`grants_digest`), `binding.rs:303,306`, `memory.rs:42-62` | Tenant-wide policy lives outside the digest. Only explicit grants and state raise the generation; member and token changes never do. |
| F6 | `credentials.json` holds 256 entries and 1 MiB, caps a lifetime at 365 days and rejects unknown fields. The rate limiter holds 512 buckets. | `credentials.rs:18-20,215`, `server.rs:3647-3650` | A separate token store and limiter. No owner field in `credentials.json` (it would stop older builds from loading the file). |
| F7 | A signed-in editor or viewer is a `Role::Scoped` grant with id `user:<account id>` and `api_credential: false`. Binding grants name 12-character credential ids, so no session matches a grant. | `users.rs:1159-1170`, `server.rs:1012-1017`, `routes.rs:222-246` | Sessions have no memory access today. Derived access needs an identity, which a token carries. |
| F8 | `authenticate` accepts the admin token, the read-only token and `vlk_` tokens, and REST and gRPC share it. `authorize_need` is the one scope gate and it compares the lineage. | `server.rs:842-870,1058-1098` | `vlm_` plugs into `authenticate`; the memory-only rule is one arm in `authorize_need`. |
| F9 | `Attach` needs a live record without expiry. A put and its attach may share one batch. | `property_graph/mod.rs:320-326,1066-1082` | A server-managed text record cannot carry the memory's expiry: a sweeper (section 7). |
| F10 | Recall's text leg searches only evidence records attached to the in-scope memories. The dense leg skips only empty vectors, and a put needs the full dimension. | `memory/recall.rs:470-551`, `lib.rs:1196-1205` | A text-only evidence collection (1 dimension), or a core skip (section 7). |
| F11 | `remember` and `forget` need the scope node to exist. `provision_scope` is idempotent and commits once per new scope. | `memory.rs:335-378` | Derived personal scopes are provisioned before first use. |
| F12 | A database file holds at most 64 graphs. | `property_graph/engine/limits.rs:12` | About 60 tenants can have memory per file; status shows the room left. |
| F13 | The memory surface is nine operations (decision 67). | serving contract | Discovery lives under `/v1/self/memory`, not a tenth data route. |
| F14 | Tenant delete is refused while a binding, collection or graph of the tenant exists. Accounts go before the tenant. | `tenants.rs:274-330` | Unchanged; the check reads the registry. Members and tokens go with the accounts. |
| F15 | The dashboard already has a MemoryAI page that works only with a scoped token; a password sign-in is told it has none. | `index.html:362-365`, `console-tools.js:1143-1146` | S5 extends that page instead of adding a second one. |
| F16 | `permission()` maps anything unlisted to admin-only and memory routes to `graph_read` or `graph_write`. | `server.rs:891-1003` | New `/v1/admin/memory/*` routes are admin-only by default; self routes are `Need::Any` plus a handler check. |
| F17 | Password change verifies the old password and counts failures per user. | `users.rs:392,1478-1525` | Minting a token reuses the verifier and the lockout (step-up). |

## 3. Model

### 3.1 Managed state (S)

S is one file, `<VLITE_ADMIN_DIR>/memory.json`, replaced atomically with the protocol of `credentials.json` (`credentials.rs:538-608`): a synced audit event, a temp file, fsync, rename, directory fsync. A failure before the rename changes nothing (503 `memory_state_unavailable`). A failure after it is in effect but unconfirmed (503 `memory_state_commit_indeterminate`, never a secret in the body). Managed memory needs an admin token and an admin directory, like credentials; without them the admin routes answer 409 `memory_managed_disabled` and the file model works as before. **No new flag or environment variable.**

```json
{
  "schema": "memory-state-v1",
  "revision": 17,
  "limits": {"token_default_days": 90, "token_max_days": 365,
             "tokens_per_user": 3, "token_requests_per_second": 20},
  "bindings": [{
    "origin": "managed",
    "phase": "active",
    "entry": {"binding": "team", "tenant": "<32 hex or @root>", "generation": 3,
              "state": "active", "...": "the memory-bindings-v1 entry, verbatim"},
    "access": {"audience": "members", "shared": [{"agent": "team"}],
               "personal_agents": ["copilot", "agency"]},
    "created_unix_ms": 0, "updated_unix_ms": 0
  }],
  "members": [{"tenant": "<32 hex or @root>", "user_id": "<32 hex>", "label": "alice",
               "state": "active", "created_unix_ms": 0}],
  "tokens": [{"id": "<12 chars a-z2-7>", "tenant": "<32 hex or @root>", "user_id": "<32 hex>",
              "name": "laptop", "lineage": "<hex>", "created_unix_ms": 0,
              "expires_unix_ms": 0, "revoked_unix_ms": null, "secret_sha256": "<64 hex>"}]
}
```

An adopted binding also carries `"adopted_digest": "v1:<64 hex>"`, the digest of the file entry at adoption. The envelope rejects unknown fields; `entry` keeps unknown keys.

**Entry rule.** `entry` is the `memory-bindings-v1` entry exactly as a file would hold it. A managed change edits only `generation`, `state`, `retired_at_ms` and `grants` inside that JSON object. Every other key (for example F1's optional `slots`) is carried as raw JSON and validated by the compiled-in binding parser, never re-serialized from a typed copy.

| Phase | Meaning | Served | Reserved |
|---|---|---|---|
| `provisioning` | S written; graph or collection may still be missing | no | yes |
| `active` | normal | yes | yes |
| `retired` | state `retired`, generation raised; `receipts/lookup` still answers | receipts only | yes |
| `purging` | data being deleted | no | yes, until dropped |

| Limit | Value |
|---|---|
| File | 4 MiB |
| Bindings | 64 (the graph cap, F12) |
| Members | 1,024 |
| Token rows | 1,024; when full, inactive rows older than 30 days are pruned, as `admit` does for credentials |
| Active tokens per user | 3 by default, 1 to 10 by limits |

### 3.2 Registry (N)

A crate-private `Arc<Registry>` in `Service` replaces the `Arc<Bindings>` field and is published to the other transports as the handle itself, not as a snapshot. Existing call sites keep their shape (`service.memory.is_empty()`, `.resolve(tenant, name)`, `.iter()`) and read the current snapshot at call time. A snapshot holds the bindings (set, reservations, orphans), the access policies, the members, the token digests, the limits and an epoch.

| Rule | Text |
|---|---|
| R1 | `load()` is a read lock and an `Arc` clone: no I/O. |
| R2 | `install` runs only inside `Service::run`, with the closure's `&mut Database` as witness that the write lock is held, and replaces the whole snapshot. |
| R3 | `RequestScope` holds the registry, not a snapshot, and reads it at each decision, inside the lock closure. |
| R4 | A memory mutation re-resolves and re-authorizes against `load()` after it takes the lock and before `recheck`: a revoked token or removed grant fails with the uniform 403, a changed generation with 409 `binding_generation` (nothing written). A read authorizes once at its start: a change is effective for every request that starts after `install` returns, and an earlier read may finish within its request deadline. |
| R5 | Lock order is the database write lock, then the state store, then the registry. No store lock is held while another store's lock is taken (tenants, users and state are taken one after another). |

### 3.3 Startup merge

Admin directory and S load first, then:

| In the file | In S | Result |
|---|---|---|
| yes | no | `origin: file`: served, read-only to the managed routes, adoptable |
| no | yes | managed or adopted entry, served |
| yes | adopted, and the file entry's digest equals `adopted_digest` | S wins, the file entry is shadowed and shown in status |
| yes | adopted, digest differs | **refuse to start** (`memory_origin_conflict`, fixed message): an edited file never silently does nothing |
| yes | managed | refuse to start (same message) |
| neither, but a record in the database | | orphan: reserved with no owner, as today |

The union is rendered as a `memory-bindings-v1` document and parsed by the existing parser, so there is no second validator. Then `prepare` runs (checks, records, scopes). Creating a missing graph or collection is allowed only for a `provisioning` entry; an `active` entry with a missing resource refuses to start, and a `purging` entry resumes. An entry whose record generation is higher than S's refuses to start (an older S over a newer database). `grpc_enabled` or automatic maintenance with any binding keeps today's refusal until F3 lifts it.

### 3.4 Access policy, members and personal scopes

```json
"access": {"audience": "explicit", "shared": [], "personal_agents": []}
```

`audience` is `explicit` (grants only: 0.7.1 semantics) or `members`. `shared` holds at most 16 agent-only scopes; `personal_agents` at most 8 agent names that do not appear in `shared`. The policy sits outside the record digest, so changing it never changes the generation.

- **Member.** An account with role editor or viewer, not disabled, whose tenant equals the binding's tenant (`@root` for no tenant). It becomes a member row at its first token or by an admin `PUT`.
- **Label.** The scope `user` value of the member. Grammar of a username: 3 to 64 characters of `a-z 0-9 . _ @ -`, starting with a letter or digit (`users.rs:468-485`); a valid scope part (1 to 256 bytes). It defaults to the normalized username at enrollment, is **pinned** (a rename keeps it), is unique per tenant among active and tombstoned rows, and is tombstoned when the account is deleted. A tombstoned label is reused only by an explicit admin call (`take_over`).
- **Allowed scopes of a memory token.** When the binding's `audience` is `members` and the token's owner is an active member: `shared` plus `{agent: a, user: label}` for each `a` in `personal_agents`. Authorization stays **exact-scope** (no parent, child or sibling fallback, no `session` part in v1). Nothing is read from the request except the scope it asks for.
- **Operations.** Recomputed on every request from the owner's live account: editor reads and writes, viewer reads (`users.rs:203-215`: role operations filtered by graph level). An account whose graph level is none has no memory operation; its `graphs` list also applies (a binding whose graph is not on the list is not usable).
- **Explicit grants** keep their 0.7.1 meaning for `vlk_` principals. A memory token never matches an explicit grant (its principal is `memtoken:<id>`, never 12 characters).
- **Provisioning.** Personal scope nodes are created eagerly (F11) at enrollment and when `personal_agents` grows, in chunks of at most 64 per lock hold, idempotently, and again by the startup reconcile. A crate-private batch helper from F1 would cut the commits (D16).

### 3.5 Memory tokens

- **Format.** `vlm_<12 chars a-z2-7 id>_<43 chars base64url secret>`. Only the SHA-256 of the secret is stored. The id is the principal `memtoken:<id>` in audit.
- **Authentication** (`authenticate`, one new `vlm_` branch): hash the presented secret and compare in constant time (an unknown id compares against a dummy digest, `credentials.rs:445-470`). Valid means: secret matches, not revoked, not expired, `lineage` equals the database lineage, the owner account exists and is enabled with role editor or viewer, its tenant equals the token's tenant, the tenant is not suspended (the check `server.rs:858-864` makes for credentials), and the member row is active. Otherwise 401 (`credential_expired` for an expired one; `memory_token_stale` only after a matching secret with another lineage, so only the holder learns it).
- **Role.** A memory token is **not** a `Role::Scoped` grant. Preferred: a new `Role::Memory` variant, so the compiler forces a decision at each of the ~40 `Role::Scoped` sites (`audit::actor`, `raw_credential`, `authorize_need`, guard). Fallback: a `memory_only` flag on `Grant` plus one arm in `authorize_need`, pinned by a route inventory test. Either way it passes only the nine data routes, `GET /v1/self/memory`, `GET /v1/capabilities`, `/ready` and `/openapi.json`; every other route is 403 `forbidden_scope`.
- **Lifetime.** Default 90 days, maximum 365 (`credentials.rs:20`), both lowerable by an admin. `last_used` is kept in memory only (reset at restart) and shown as approximate.
- **Rate.** A dedicated limiter table sized to the token cap, 20 requests per second per token by default; mint is limited to 10 per user per hour.
- **Minting** needs a cookie session of an editor or viewer (not an admin session, not a token), the same-origin check, the account password (the verifier and per-user lockout of password change, F17) and a tenant with an active `members` binding. Order: checks, enroll the member (label, scopes), persist S, return the secret once.
- **Hooks**, in the same `Service::run` closure after the account store commits: disabling an account, deleting it, changing its tenant or making it a super admin revokes its tokens; deleting a tenant removes its members and tokens after its accounts. Startup revokes tokens whose owner is missing, disabled, in another tenant or not editor or viewer, and marks tokens of another lineage `stale`. Validity is also recomputed on every request, so a missed hook never grants access.

## 4. Routes

### 4.1 Admin

Caller: the admin token or a super-admin session (`Need::Admin`); `GET` also the read-only token. Scoped credentials, editors, viewers and memory tokens get 403. Tenant is `@root` or a 32-hex id; `@` needs no escaping in a path.

| Route | Body (required in bold) and result |
|---|---|
| `GET /v1/admin/memory` | Status: `available`, `managed`, `revision`, capacity (`graphs_free`, members, tokens), `blocked` (features that forbid enabling now), `reserving`, `aggregate_refusals`, `bindings` (`tenant`, `binding`, `origin`, `phase`, `generation`, `shadowed`), `orphans`, `limits`. |
| `POST /v1/admin/memory/bindings` | Enable. **`tenant`**, `binding` (default `memory`), `graph` (default the binding name), `evidence` (default `null`: no evidence collection, so no recall by words; or `{collection (default memory_ev), dimensions (required, 1 to 65,536), text_fields}`), `receipt_window_ms` (default 86,400,000), `access` (default `explicit`, empty), `grants` (default none), `adopt_existing_resources` (default false: adopts an existing empty graph and collection, or an orphan whose record matches), `acknowledge_server_limits` (must be `true` when nothing is reserved yet). Tenant-local names are at most 28 bytes (63 minus the 35-byte `t<tenant>__` prefix; `format.rs:125`, `limits.rs:14`). 201 with the view; 200 for the identical request repeated; 409 `memory_binding_exists` for a different body. |
| `POST /v1/admin/memory/adopt` | **`tenant`**, **`binding`**: copies a file entry into S as `adopted`. No record write, no generation change, no restart. |
| `GET /v1/admin/memory/bindings/{tenant}/{binding}` | View: `entry` fields, `grants`, `access`, `origin`, `phase`, member count, timestamps. |
| `PUT /v1/admin/memory/bindings/{tenant}/{binding}/access` | **`audience`**, `shared`, `personal_agents`. Generation unchanged. |
| `PUT /v1/admin/memory/bindings/{tenant}/{binding}/grants` | **`grants`**: replaces the explicit list. Generation +1 when the digest changes. |
| `POST /v1/admin/memory/bindings/{tenant}/{binding}/retire` | `{}`. State `retired`, generation +1. Idempotent. |
| `POST /v1/admin/memory/bindings/{tenant}/{binding}/purge` | **`confirm`** (`<tenant>/<binding>`), `force` (default false). Only when retired and `retired_at_ms + receipt_window_ms` has passed, unless `force`. |
| `GET` and `PUT /v1/admin/memory/limits` | `token_default_days`, `token_max_days`, `tokens_per_user`, `token_requests_per_second` (until S4 the defaults of 3.1 are fixed). |
| `GET /v1/admin/memory/tenants/{tenant}/members` | Members and their labels. |
| `PUT /v1/admin/memory/tenants/{tenant}/members/{user_id}` | **`label`**, `take_over` (default false). Creates the member row of an eligible account. |
| `GET /v1/admin/memory/tokens` | Query `tenant`, `user_id`. Ids, names, owners, times and state; no secret. |
| `POST /v1/admin/memory/tokens/{id}/revoke` | Revokes one token. |
| `POST /v1/admin/memory/tokens/revoke` | **`user_id`**: revokes every token of one user. |
| `GET /v1/admin/memory/export` | The current explicit-grant state as a `memory-bindings-v1` file, with current generations (rollback, section 8). |

### 4.2 Self-service

Caller: a signed-in editor or viewer. A super admin or the admin token gets 403 (no tenant).

| Route | Caller | Body and result |
|---|---|---|
| `GET /v1/self/memory` | session or memory token | `enabled`, `bindings` (name, state, usable scopes with plain labels, whether it can write, `recall` modes), `limits`; with a session also `tokens` (id, name, created, expires, last used). No server URL: the page builds it from `location.origin`. |
| `POST /v1/self/memory/tokens` | session only | **`name`** (1 to 64 printable characters), `expires_in_days`, **`password`**. 201 `{token, info, mcp: {env_var: "VLITE_TOKEN", writable, bindings}}`; the token appears here once. |
| `POST /v1/self/memory/tokens/{id}/revoke` | session only | Revokes the caller's own token; idempotent; another user's id is `memory_token_not_found`. |

Mint and revoke are cookie-only (same-origin check). The ready configuration is `vlite-mcp --server <url> --token-env VLITE_TOKEN`, plus `--writable` only for an editor (`docs/MCP.md`, Server-backed agent memory). The token is never embedded in a file the page produces.

### 4.3 Errors

| Code | Status | When |
|---|---|---|
| `memory_managed_disabled` | 409 | no admin token or admin directory |
| `memory_state_unavailable` | 503 | S could not be written; nothing changed |
| `memory_state_commit_indeterminate` | 503 | the change is in effect; durability unconfirmed |
| `memory_reconcile_pending` | 503 | a step after the S write failed; repeat the same call to finish |
| `memory_binding_exists` | 409 | enable with a different body for an existing key |
| `memory_binding_not_found` | 404 | no managed, adopted or file entry |
| `memory_resource_in_use` | 409 | the graph or collection exists and is not adoptable, or another binding owns it |
| `memory_acknowledgement_required` | 409 | first enable without `acknowledge_server_limits: true` |
| `memory_runtime_blocked` | 409 | `details.feature` is `grpc_listener`, `auto_maintenance` or `imports_open` |
| `memory_origin_conflict` | 409 | adopt of a managed or adopted entry (fatal at startup for an edited adopted entry) |
| `memory_not_retired` | 409 | purge of an active binding |
| `memory_receipt_window_open` | 409 | purge before the window ended without `force` |
| `memory_label_taken` | 409 | label held by a member or a tombstone (`take_over` for a tombstone) |
| `memory_member_not_found` | 404 | member route for an account that is not eligible |
| `memory_token_limit` | 409 | active tokens per user reached |
| `memory_token_not_found` | 404 | unknown id, another user's id |
| `memory_not_enabled` | 404 | the caller's tenant has no active `members` binding |
| `memory_not_permitted` | 403 | the account has no graph access |
| `memory_token_stale` | 401 | right secret, issued for another database lineage |

Reused: `rate_limited` (429), `invalid_credentials` and `too_many_attempts` (the password step-up), `credential_expired` (401), `forbidden_scope` (403), the engine's own graph-limit and unsupported-graph errors.

### 4.4 Audit

One synced event before each state write (and a `_failed` or `_durability_unknown` companion, as `credentials.rs:560-600` writes): `memory_binding_enabled`, `_adopted`, `_access_updated`, `_grants_updated`, `_retired`, `_purge_started`, `_purged`, `memory_state_reconciled`, `memory_limits_updated`, `memory_member_labeled`, `memory_token_created`, `memory_token_revoked`, `memory_token_swept`. Fields are actor, tenant, binding, generation, token id and owner id. Never a token, secret, hash, password or memory content. The existing `memory_request` event carries `memtoken:<id>` as principal and actor.

### 4.5 Capabilities and the gate

`GET /v1/capabilities` gains `features.memory_managed` (add-only). For a memory token it lists exactly the data routes its derived operations allow, plus `/v1/self/memory`, with `role: "memory"` (S5 adapts `console-tools.js:1144`, which accepts only `scoped`). **Gate:** the nine routes are always mounted on a graph-enabled database. While the registry has no binding, every request to them is answered exactly as 0.7.1 answers without a bindings file, for every principal and method (S1 pins this with answers recorded on the 0.7.1 tree).

### 4.6 Words for the dashboard (S5)

| Where | Label | Help |
|---|---|---|
| Administration | **Agent memory** | Lets assistants such as GitHub Copilot and agency remember things between sessions. |
| Workspace row | **Off**, **On**, **Paused** | |
| Button | **Turn on memory** | |
| Field | **Shared memory name** (`team`) | Everyone in this workspace reads and writes it. |
| Field | **Private memory for each person** (`copilot`, `agency`) | Each person gets a private memory per assistant listed. Nobody else can read it. |
| Fixed line | **Who can use it** | Everyone in this workspace. Editors read and write; viewers only read. |
| Switch | **Search memories by words** (on) | Lets an assistant find a memory by the words in it. |
| Checkbox, first time | **I understand what switches off** | Text follows decision D2. |
| Buttons | **Pause memory**, **Delete memory data** | Pause keeps the data and stops use. Delete removes it for good (type `workspace/name`). |
| MemoryAI page, signed-in user | **My memory**, **Your tokens** | Name, created, expires, last used, revoke. |
| Button | **Create a token for GitHub Copilot or agency** | Asks for a name, a lifetime (90 days) and your password. |
| After creating | **Copy your token now. It is shown only once.** | Three steps: set `VLITE_TOKEN`; add the server to the assistant's MCP settings; press **Test it here**. |

## 5. Lifecycles and crash consistency

Every operation runs in one `Service::run` closure (database write lock held). S is always written first.

| Operation | Steps | If the process dies after step | At the next start |
|---|---|---|---|
| Enable | 1 check (blocked features, names, capacity, acknowledgement); 2 S: `provisioning`; 3 create graph and collection; 4 `prepare`: write R, provision explicit scopes; 5 S: `active`; 6 install | 2 or 3 | the entry is `provisioning`: create what is missing, `prepare`, set `active` |
| | | 4 | R exists and matches: set `active` |
| | | 5 | N is built from S |
| Grants, retire | S (generation +1) then `prepare` (R) then install | S written | `prepare` writes R: a higher generation is allowed |
| Access, limits, member, token | S only, then install | S written | N is built from S |
| Purge | S: `purging`; delete nodes (record last) and evidence records in chunks, releasing the lock between chunks; drop both; remove the entry from S; install | any | `purging` resumes; resources already gone count as done; the entry is removed |
| Adopt | S only | S written | N is built from S |

A `commit_indeterminate` S write leaves the change in effect and the later steps are still run, as `credentials.rs` does. In-flight mutations that straddle a generation change answer 409 `binding_generation` (nothing written) and are retried by the client. Purging the last reserved resource ends every reservation and restores the aggregate routes and the 404 gate.

## 6. Threat model

| # | Threat | Control | Pinned by (test to write) |
|---|---|---|---|
| T1 | A token of tenant A reads or writes tenant B, including a same-named binding. | The tenant comes from the token; resolution is by tenant and name; the body never names a tenant; every miss is one 403 (L6). | two tenants, one binding name, all nine data routes, both directions |
| T2 | Impersonation: a new account named like an earlier one reads its private memory after a rename or delete. | Pinned labels, unique per tenant, kept across renames, tombstoned on delete, taken over only by an explicit admin call. | label tests: rename, delete, re-create, take over |
| T3 | A caller names another person's personal scope. | Exact match against the derived set; the label is never read from the request. | derivation tests for every scope shape, including a `session` part |
| T4 | A viewer writes. | Operations are derived on each request from the live role and graph level; a downgrade applies to the next request. | viewer and downgrade tests on the five write routes |
| T5 | A memory token reaches a raw, admin or auth route, or mints a token. | L5 and 3.5: the memory-only role; mint and revoke need a cookie session. | inventory: every OpenAPI route and method against a memory token |
| T6 | A token reaches the memory graph or evidence through a raw path. | A token never has `raw_access`; item evidence references stay refused; the bound resources stay reserved. | the reserved-as-missing suite plus token rows |
| T7 | Token theft. | 256-bit secret, hash at rest, shown once; 90 days by default, 365 at most; 3 active per user; revoke by owner or admin; tied to tenant and lineage; `vlite-mcp` refuses non-loopback HTTP. | lifetime, cap, revoke and lineage tests |
| T8 | A stolen session or XSS mints a token. | Cookie session, same-origin check and the account password with the lockout; 10 mints per user per hour. | no password, wrong password, lockout and rate tests |
| T9 | Guessing and timing. | Unknown ids compare against a dummy digest; every failure is one 401. | structure test and uniform-answer test |
| T10 | Floods, table growth and storage. | Limiter sized to the token cap, 20 requests per second per token; caps of 1,024 tokens and members; the graph cap is shown in status; managed text records count against the tenant `max_vectors` quota (S3). | limiter, cap and quota tests |
| T11 | Suspended tenant. | Token authentication calls the tenants store as for credentials; the owner account check does the same for sessions. | suspend test |
| T12 | Deleted or disabled account, changed tenant, promotion to super admin. | Validity is recomputed per request; hooks revoke persistently; startup revokes stale owners. | one test per hook and for the startup revoke |
| T13 | Tenant delete orphaning state. | The existing refusal while a binding, collection or graph exists; members and tokens removed after the accounts. | tenant delete test |
| T14 | Backup and restore split S, the database and `credentials.json`. | S is admin-directory state like `credentials.json`. Start refuses (fixed message) when an `active` entry's resources are missing or a record's generation exceeds S; `provisioning` completes; a changed lineage makes tokens `stale`. Procedure in section 8. | restore matrix: older S, older database, new lineage |
| T15 | Audit gaps or leaks. | 4.4: one event before each write; nothing secret; if the audit cannot be written the answer is 503 and nothing changes. | audit test per event, secret scan |
| T16 | Crash inconsistency between S, R and N. | Section 5. | a failpoint after each step |
| T17 | Stale snapshot between a decision and its write. | R2 to R4. | barrier test: a revoke lands between authorization and the lock |
| T18 | Enabling memory switches off metrics, webhook status and imports, and breaks the 0.7.1 dashboard sign-in. | Acknowledgement flag; status lists the refusals; section 9. | acknowledgement and status tests |
| T19 | gRPC bypass. | Enable refused while a gRPC listener or automatic maintenance is configured or imports are open, until F3 lands; gRPC adopts the registry handle. | blocked-feature tests; the gRPC leg of the differential |
| T20 | Error oracles. | Data routes: one 403. Self routes state facts about the caller's own tenant only. Enable names a taken resource only to the administrator. | reserved-as-missing rows and the namesake sweep |
| T21 | Shared-scope poisoning: any editor writes what everyone reads. | Accepted in v1 (D6); every write is audited with the token id. | none (documented) |
| T22 | Peers replay each other's keys and read receipts on a shared scope (`MEMORY.md:266`). | Documented; tenant-wide scopes make every member a peer. | none (documented) |
| T23 | Filesystem insider. | Same posture as `credentials.json`: private directory, hashes only. | none |
| T24 | Downgrade to a build without F4. | Old builds ignore S and `vlm_`; managed records stay and keep their graphs reserved (fail closed); `export` renders a file for the old model. | downgrade check at the end of S1 |

## 7. Searchable remember

**Facts.** Recall by words finds only memories that hold an attached evidence record (F10). A record attached to a memory must have no expiry (F9), so it cannot expire with the memory. A put needs a full-dimension vector, and the dense leg skips only empty vectors (F10).

| Option | Design | Cost |
|---|---|---|
| **T1 (recommended)** | The binding's evidence collection is text-only (`dimensions` 1, `text_fields: ["text"]`). When `remember` creates a new memory from an item that names no evidence, the same batch puts one record `{vector [1.0], text: the content, vlite_managed: "text:v1"}` and attaches it. A repeat writes nothing. `forget` already deletes attached evidence. | A crate-private hook in `src/memory/**` (F1's tree), an optional additive entry field `text_index: "managed"` next to `slots`, and a server sweeper. No vector recall on that binding. |
| T2 | The same record in a vector-bearing collection. | Also needs the dense leg to skip marked records (`recall.rs:520`); later. |
| T3 | Status quo: a helper that uses a `raw_access` ingest credential. | No server change, but users need raw access: it defeats the goal. |
| T4 | A text index over the graph node content. | A new core feature. |

**Sweeper (T1).** Runs at start, every 10 minutes and on demand, per binding. It scans at most 4,096 records and deletes at most 256 per pass, in one batch: marked records that are unattached or attached to a memory that is gone or expired. It audits the count (`memory_text_swept`). An expired memory is hidden at once, and its record is invisible to recall because the text leg only searches records of in-scope memories; the sweeper reclaims the storage.
**Existing memories are not backfilled** (a backfill is a merge per memory and raises its revision). Managed records count against the tenant `max_vectors` quota (`quota_exceeded` 429 before the write). To verify in S3: an empty text-only collection reports its text index as ready, and F1's scope erasure removes attached managed records.

## 8. Migration, rollback and rehearsal

**Live app.** Memory is off since 2026-11-06 00:05 PT and returns with 0.7.2 from the static file: binding `team` in each of two tenants, graph `memory`, evidence `memory_ev` (`text_fields: ["text"]`), generation 1, explicit grants, one raw-access ingest credential per tenant. Nothing in S1 changes a file-only deployment (L3).

**Path A: file first, then adopt** (after S1, with the image upgrade as the only restart; stop old revisions and confirm zero replicas first, as `deploy/azure-container-apps/README.md` requires):
1. The file still serves (`origin: file`).
2. `POST /v1/admin/memory/adopt` for `team` in each tenant. No record write, no generation change, no restart. The file entry is shadowed.
3. After S2: `PUT .../access` (shared `{agent: team}`, personal agents `copilot` and `agency`, audience `members`).
4. `PUT .../members/{user_id}` pins labels equal to the existing `user` values, so existing personal data stays reachable.
5. Each user mints a token. The old explicit grants keep working until `PUT .../grants` with an empty list retires them.
6. The file and `VLITE_MEMORY_BINDINGS_FILE` can go at leisure. While the file entry equals the adopted digest it is shadowed; an edit refuses to start (`memory_origin_conflict`).

**Path B: no file** (after S2): `POST /v1/admin/memory/bindings` per tenant, with `adopt_existing_resources: true` when the graphs already exist.

**Rollback.** `GET /v1/admin/memory/export` renders the explicit-grant state with current generations; an older build started with it runs the file model (tokens are unknown to it and tenant-wide access is not exported). A build without F4 started with an old file after any managed generation change refuses to start ("changed without a higher generation"): export first.

**Restore.** Restore the database and the admin directory from the same moment. A newer database with an older `memory.json` refuses to start (3.3). A restored `memory.json` over a database that lacks an `active` entry's resources refuses to start. A new lineage leaves tokens `stale`: mint again.

**Rehearsal.** Before S1 ships, rehearse on a production-shaped copy (ask the coordinator for it, not on the live app): upgrade, adopt, set access, mint, retire the explicit grants, export, downgrade. Docker only inside WSL. A terminated replica can leave stale SMB handles on `vectors.vlite` (the next revision fails with `database I/O error: Permission denied`): every restart is a risk on Azure Files, which is one more reason for hot reload.

## 9. Aggregate refusals: options

While anything is reserved, `/metrics`, `/v1/admin/metrics/summary`, webhook list and status, every import route, the replication export and the automatic-maintenance routes answer 403 `forbidden_scope` to everyone (F4), the gRPC listener refuses to start, and a removed file does not lift it. The 0.7.1 dashboard fails super-admin sign-in because it calls `/metrics`; lane H1 fixes that side. Without a decision, enabling memory for **one** tenant switches these off for the whole server.

| Option | Change | Effect | Cost |
|---|---|---|---|
| A. Keep | none | Server metrics, webhook status, imports and export are off server-wide until the last binding is purged. | None. The acknowledgement flag and the status list warn the administrator first. |
| **B. Admin-only aggregates (recommended; its own lane, H2)** | The administrator (admin token, super-admin session) gets metrics, summary and webhook list and status. Scoped credentials, tenant users and the read-only token stay refused. | Operations keep metrics. Totals include reserved sizes and counts, never names or content. | Flips about four checks (`observability.rs:190,226`, `webhooks.rs:269,295`; H2 to confirm), rewrites their pins and the differential, and needs the owner to accept that the administrator sees totals that include reserved resources. |
| C. Per-tenant aggregates | Every caller gets numbers computed without reserved resources. | Everyone keeps metrics, but database-wide numbers (bytes, record counts) no longer match the file. | A second counter set and the largest test surface. |

Replication export and automatic maintenance stay refused in every option (they read or rewrite reserved data wholesale). Imports stay refused until the owner decides otherwise.

## 10. Decisions

Owner-level decisions go to the owner through the coordinator; the rest the coordinator ratifies.

| # | Decision | Options | Recommendation | Who |
|---|---|---|---|---|
| D1 | Who may turn memory on or off for a tenant | (a) super admin only; (b) also a tenant editor, behind a server switch; (c) a new tenant-admin role first | (a): enabling spends shared resources (64 graphs, the aggregate refusals) | Owner |
| D2 | Aggregate refusals while memory is on | A, B or C (section 9) | B as lane H2; until then A with the acknowledgement flag | Owner |
| D3 | Token lifetime | (a) default 90 days, maximum 365; (b) default 30 days; (c) no expiry | (a), lowerable by the admin; (c) is refused (the credential maximum is 365 days) | Owner |
| D4 | Password to create a token | (a) required; (b) the session alone | (a): a stolen cookie or an XSS cannot mint a long-lived key | Owner |
| D5 | Who receives the tenant-wide scopes | (a) memory tokens of members; (b) also every `vlk_` credential of the tenant (shared scopes only); (c) also dashboard sessions on the data routes | (a): (b) widens existing credentials when a policy flips; (c) puts memory behind the cookie | Owner |
| D6 | Who may write the shared scope | (a) every editor; (b) writers chosen by the admin; (c) server-stamped attribution on each memory | (a) in v1 with audit; (c) later, through the core | Owner |
| D7 | Personal labels | (a) pinned username at first enrollment, admin override, tombstones; (b) opaque account id; (c) the admin types every label | (a): readable and stable across renames | Owner |
| D8 | Admin-minted tokens for another user | (a) never; (b) allowed and audited | (a): an operator must not hold a person's private-scope key; service identities keep `vlk_` credentials | Owner |
| D9 | Default receipt window of a managed binding | 24 h, 1 h, 7 d | 24 h (what the live bindings use) | Owner |
| D10 | Search by words for plain `remember` | T1, T2, T3, T4 (section 7) | T1 | Owner (behavior), coordinator (F1 touchpoint) |
| D11 | Where managed state lives | (a) the admin-directory file `memory.json`; (b) the database (graph nodes); (c) a sidecar database | (a): the protocol and trust of `credentials.json`. (b) puts token hashes into graph backups, lets a lineage change wipe them and splits state across bindings | Coordinator |
| D12 | Token kind | (a) a new `vlm_` kind and store; (b) an `owner` field in `credentials.json` | (a): (b) shares the 256 cap and stops older builds from loading the file | Coordinator |
| D13 | How gRPC sees hot changes | (a) one shared `Arc<Registry>` handle; (b) a snapshot per transport (F3 S1 as written) | (a): (b) cannot see a hot change | Coordinator with F3 |
| D14 | In-flight requests after a hot change | (a) mutations re-authorize under the lock, reads once; (b) refuse in-flight with 409 on any epoch change; (c) restart only | (a) | Coordinator |
| D15 | Slice order | the given order; or self-service mint moves from S4 into S2 | Move it: derived scopes cannot be tested without an owner identity; S4 shrinks to limits, onboarding data and snippets | Coordinator |
| D16 | F1 touchpoints | an optional additive `text_index` entry field; a managed-record hook in `remember`; a batch `provision_scopes` helper | Ask F1 for all three; touch nothing else in `src/memory/**` | Coordinator with F1 |
| D17 | Release order | H1 first, then S1 with D2 decided; or S1 with the acknowledgement as the only guard | H1 first | Coordinator |
| D18 | Agency MCP configuration | assume the `mcpServers` object of Copilot CLI (`~/.copilot/mcp-config.json`); or ask | Ask the owner for the file location and shape before S5 | Owner |
| D19 | Enable while a gRPC listener, automatic maintenance or open imports exist | refuse; or allow | Refuse, until F3 S2 lifts the gRPC refusal | Coordinator |

## 11. Slices

Standing rules for every slice: `cargo fmt`, strict clippy, the relevant suites, hand-written mutants killed by named tests and replayed on a second machine, three independent deep reviews on `claude-opus-5.5` (S2 adds a security review), no benchmarks, gates never lowered, decisions to the coordinator. Each slice lands its own OpenAPI entries, `site/api.html` rows (`data-api` keys) and `server_memory_reserved_as_missing.rs` CASES rows, so `tests/api_reference.py` stays green. Hot files get add-only commits.

| Slice | Delivers | Size | Needs | Mostly touches |
|---|---|---|---|---|
| S1 | Registry handle and gate (a behavior-neutral first commit, proven by recorded 0.7.1 answers), managed state, hot enable, adopt, access (stored, enforced from S2), grants, retire, purge, status, export, `features.memory_managed`, tenant-delete check | L | F3 S1 merged; H1 order (D17) | new `src/server/memory/{state,registry,manage}.rs`; add-only in `server.rs`, `admin.rs`, `guard.rs`, `memory.rs`, `tenants.rs` |
| S2 | Members and labels, `vlm_` store and authentication, the memory-only role, tenant-wide access, derivation, self routes, admin member and token routes, account and tenant hooks (**security critical**) | L | S1 | `memory/{members,tokens}.rs`, `server.rs` (`authenticate`, `authorize_need`), `users.rs`, `routes.rs` (`resolve`) |
| S3 | Searchable remember: managed record in `remember`, sweeper, quota, capability | M | S1, F1 agreement (D16) | `src/memory/**` hook (F1), `server/memory/sweep.rs` |
| S4 | Operator controls: limits routes, onboarding data, MCP snippets | S | S2 | `manage.rs`, docs |
| S5 | Dashboard: enablement page, My memory on the MemoryAI page, one tour recapture | M | S2, H1, coordinator sequencing | `src/dashboard/*`, `site/assets/dashboard/connected-captures.json` |
| S6 | Docs, clients, MCP, parity (`MEMORY.md`, `API.md`, `SECURITY.md`, `MCP.md`, `CONSOLE.md` within its word budget, README and site) | S | S1 to S5 | docs, site |

Size means changed lines with tests: S under 600, M 1,000 to 1,500, L 3,000 to 4,000.

**Mutant seeds** (each needs a named killing test; replayed on a second machine):

| Slice | Mutants |
|---|---|
| S1 | M1 enable writes R before S. M2 `install` without the write lock or before `prepare`. M3 `RequestScope` keeps a request-start snapshot. M4 the gate answers other than the 0.7.1 404. M5 adopt writes a record or raises the generation. M6 an edited file entry is ignored. M7 the entry round trip drops an unknown key. M8 a policy change raises the generation (or a grants change does not). M9 purge ignores the window or `force`. M10 purge leaves the record node or the collection. M11 an `active` entry with a missing graph is recreated. M12 enable allowed with gRPC, automatic maintenance or open imports. M13 tenant delete ignores managed entries. M14 the acknowledgement is ignored. M15 `commit_indeterminate` reported as success or as no change. M16 reservations lift before the drop. |
| S2 | N1 the personal scope read from the request. N2 the label looked up from the current username. N3 label uniqueness or tombstones dropped. N4 operations cached at mint. N5 a token valid after disable, tenant change, delete or suspension. N6 the memory-only rule dropped. N7 mint accepted with a token or an admin session. N8 expiry or the lifetime cap ignored. N9 non-constant-time compare or an early exit on an unknown id. N10 shared scopes on an `explicit` binding. N11 tenant-wide scopes applied to `vlk_` credentials. N12 a secret stored, logged or returned twice. N13 the per-user cap not enforced or counting revoked rows. N14 a binding resolved in the wrong tenant. N15 the lineage check dropped. N16 the password step-up skipped. |
| S3 | `forget` leaves the managed record. The sweeper deletes an attached record. The marker is missing. A repeat writes a record. The quota is skipped. The hook runs for an item that names its own evidence. |

## 12. Coordination and open questions

- **F3.** Replace `published_bindings: Arc<Mutex<Option<Arc<reserved::Bindings>>>>` (their S1) with the registry handle, so `adopt_published` shares one registry. Register the gRPC `Memory` service whenever the database is graph-enabled, behind the same gate as REST, not "only when a file names a binding" (their D4): otherwise managed enable never exposes gRPC. S1 starts after F3 S1 merges, and a sequence with F3 S2 (which edits `admin.rs` and `routes.rs`) is the coordinator's.
- **F1.** Three requests (D16). Managed bindings round-trip F1's optional fields because `entry` is raw JSON (3.1). F1's temporal activation changes the lineage: memory tokens become `stale` and a re-mint recovers, with no F1 work.
- **F2.** Only `features` in `/v1/capabilities` (add-only).
- **H1.** S1 does not ship before H1, or before D2 is decided. S5 edits listed dashboard assets after H1's `app.js` change, with one tour recapture the coordinator sequences.
- **Rehearsal.** A production-shaped copy is needed before S1 ships.
- **Open questions.** Agency's MCP configuration location and shape (D18). Whether recall of an authorized scope that has no node should answer empty instead of an error (verify in S2). Whether an empty text-only collection reports its text index ready (verify in S3).
