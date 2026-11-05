# Dashboard guide

VLite's browser console serves one database at `/dashboard`; Docker Compose
also serves it on port 8081.

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="../site/assets/dashboard/current-overview-dark.png?v=connected">
  <img src="../site/assets/dashboard/current-overview-light.png?v=connected" data-light="../site/assets/dashboard/current-overview-light.png?v=connected" data-dark="../site/assets/dashboard/current-overview-dark.png?v=connected" alt="Connected dashboard with sample records, navigation and database snapshot cards." width="1440" height="1100">
</picture>

[Dashboard tour](https://mahpate.github.io/Vlite/dashboard.html#screenshots)

## Sign in

Sign in with a username and password; a session cookie keeps you signed in.

![Sign-in screen with username and password fields and the access token option](../site/assets/dashboard/current-sign-in.png?v=connected)

- **First account:** there is no default password. Use the admin token, then
  create a super admin in **Administration → Users & access**.
- **Temporary password:** a password set by an admin must be changed at first
  sign-in. Passwords are 15–128 characters.
- **Forgot your password:** ask a super admin to reset it.
- **Sessions** end when you sign out, when they expire and when the server
  restarts. Sign in again to continue.
- **HTTPS or a loopback-only server:** sign-in is accepted only over HTTPS or
  when the server listens on loopback only. When a TLS-terminating or local
  reverse proxy fronts the API, the operator sets `VLITE_TRUSTED_PROXY=1`.
  Docker Compose sets it.
- **Open local mode** (`--insecure-local`) has no sign-in.

See [dashboard sign-in and accounts](SECURITY.md#dashboard-sign-in-and-accounts)
for the full model.

What you can do depends on your role:

| Role | Can |
|---|---|
| Super admin | Everything, including the server, tenants, users and API keys. |
| Editor | Read, search, write, import, index and manage collections. |
| Viewer | Read and search. |

### Use an access token

Choose **Use an access token instead** and paste the value without `Bearer`.
Find the admin token here:

- **Docker Compose:** `.secrets/api-token` in the checkout that started the
  server. PowerShell: `Get-Content .secrets\api-token -Raw`.
- **Windows devbox:** `.\scripts\devbox.ps1 -Token` copies it to the clipboard.
- **Direct server:** the file passed with `--token-file`.
- **Shared server:** ask its administrator for an account.

Treat tokens as secrets. Keep them out of screenshots and logs.

## Find your way around

The sidebar groups nine workspaces in 0.7.1 source builds. Tabs inside each
workspace hold its views:

| Group | Workspace | Inside |
|---|---|---|
| | **Overview** | Database size, health and shortcuts |
| Data | **Collections** | Records, Add & edit, Import & export, Index, Manage |
| Data | **Search** | Vector search, Advanced retrieval, Analyze & compare |
| Data | **Key-value** | Namespaced keys, text/JSON/bytes, conditional writes and expiry |
| Data | **MemoryAI** | Scoped remember, recall, consolidation, logical deletion and receipts |
| Data | **Graph** | Query, edit and rotate graphs in 2D or 3D |
| Server | **Observability** | Live monitor, Diagnostics, Change feed |
| Server | **Administration** | Health, Users & access, Backup & recovery, Server settings |
| Tools | **Developer Console** | Requests and responses this dashboard sends |

Everyone sees every workspace; unavailable controls explain why. On narrow
screens, open the sidebar with **Menu**.

The account menu shows your identity, password, theme, density, **Quick tour**
and **Sign out**. Appearance persists only when you opt in.

![Account menu with the signed-in identity, appearance controls and sign out](../site/assets/dashboard/current-preferences.png?v=connected)

## Create a collection and add records

1. **Create.** In **Collections → Manage**, enter name, dimensions, metric and
   optional metadata indexes (`eq`, `range`, `eq_range`). Review before confirming.
2. **Select.** Choose the collection in the sidebar picker. Every browse,
   write, search and index action uses the active collection. Switching lists
   any unsaved work it will discard and asks first.
3. **Write.** In **Add & edit**, use **One record**, or **Bulk JSON** for up
   to 10,000 documents (4 MiB):

   ```json
   [{"id":"1","vector":[1,0,0],"metadata":{"category":{"type":"string","value":"demo"}},"payload":""}]
   ```

   **Validate / preview** checks every document before anything is written.
   Writes go in atomic batches of 20, but the whole upload is not atomic: if a
   batch fails, earlier batches stay. An upsert replaces the vector, payload,
   metadata and expiry.
4. **Import.** In **Import & export**, load a JSON or CSV file of up to 100
   records, or use **Large file import** for big JSON, NDJSON or CSV files.
   Each chunk commits atomically, and **Resume** continues from the last
   committed chunk.

Creating collections needs a format v4 file. The dashboard never upgrades a
file; that is an offline operator step. You can drop only an empty collection.

![Add & edit view with validated records awaiting confirmation](../site/assets/dashboard/current-write.png?v=connected)

## Browse and edit records

**Collections → Records** pages through records with selectable metadata
columns. Sorting applies to the loaded page only. Select a row to open the
inspector with **Payload**, **Metadata**, **Vector** and **Raw JSON** tabs.

- **Only if unchanged** protects edits. If the record changed meanwhile, nothing
  is written; reload it and reapply your edit.
- **Save metadata only** replaces the metadata and keeps the vector and payload.
- **Export** saves the loaded page as JSON or CSV.
- **Delete selected** and **Replace selected metadata** act on IDs from the
  loaded page.

## Search

Open **Search** and enter a query vector, `k`, a metric and an optional filter.
The filter builder joins equality and numeric range conditions with AND.

| Mode | What it does |
|---|---|
| Exact | Scans every record. The reference answer. |
| Approximate | Uses the HNSW index and builds it if missing. |
| Approximate (ready) | Uses an existing index and never builds one. |
| Auto | Lets the server pick a plan. Never builds an index. |
| Hybrid | Fuses vector results with text matches. Higher scores are better. |

Distances are lower-is-better. **Explain** shows plans; **Compare** compares exact
and ANN. **Analyze & compare** samples records and plots PCA. **Advanced retrieval**
offers grouping, geo, recommendations, facets and [query text](VECTORS.md).
Declare named vectors, replace complete named/sparse maps for existing records,
and search named, sparse-only or fused vectors. Replacement clears omitted maps;
there is no compare-and-set/readback. Retain an authoritative copy.
Saved queries and **Copy as code** never include credentials.

![Search workspace with a vector query and ranked neighbors](../site/assets/dashboard/current-search.png?v=connected)

### Managed embeddings and ranking

**Generate a query vector from text** confirms disclosure without searching.
Use `hashing` (nonsemantic), `http` or an authorized alias.
Aliases use 1–256 ASCII letters, digits, dots, underscores or hyphens.
Operators configure models/secrets offline.
**Managed ranking provider** separates higher-is-better scores from distances.
Saved queries retain aliases, never secrets. Failures never fall back silently.

## Key-value

Select a one-dimensional collection and browse keys by namespace/prefix.
Namespaces are not authorization boundaries; pages are live.

New keys default to create-only; fetched/saved keys require the returned version.
Versions/absolute expiry stay decimal strings. Blank expiry **clears TTL**.
After conflicts/timeouts, fetch before retrying. Namespace changes retain drafts
but reset to create-only. Keys retain line breaks. Expiry-only edits preserve
bytes. CR/CRLF, BOM and binary values load as base64. Format switching converts
values, refusing lossy text conversions.

![Key-value workspace browsing synthetic preference keys](../site/assets/dashboard/current-key-value.png?v=connected)

## MemoryAI

MemoryAI serves agent memory through an operator-provided binding, exact
agent/user/session scope and **scoped API token**, not administrator sign-in.
Recall accepts seeds, indexed text or vectors. Weighted ranking combines squared
Euclidean vector distance with other signals. From 0.7.1, exact/MMR use cosine distance
without entity seeds. Exact permits text; MMR does not. Inspect revisions and truncation.

Remember supports entity/episodic memories; consolidation retains originals.
Forget pins ID/revision pairs: **not secure erasure**. Confirm mutations with
idempotency keys. Download requests after failures and responses before sign-out.
Unchanged-draft replay preserves timestamps. Observation time uses the server's
Date header or `meta.ttl_clock_ms`, not your device clock. For unavailable/future
times, refresh and ask your operator to check server time and proxy Date headers.
Consolidation permits at most 256 wire references; server budgets may be lower.
Receipt lookup supports forget/consolidate only. Fact correction, erasure jobs
and automatic chat import are unavailable. See [memory serving](API.md).

## Explore graphs

**Graph** explores [property graphs](GRAPH.md) in **2D** or force-directed **3D**:
up to 40 nodes/400 edges; tables retain everything. Select nodes/edges to edit or
**Connect two nodes**. Confirm writes; edits reject stale revisions. Only empty
graphs can be deleted. Requery after writes.
Drag nodes; pan/rotate in 2D/3D; Ctrl/Cmd + scroll zooms.
Ordinary scrolling/vertical touch scrolls the page. Selection highlights neighbors,
not dimming labels. Fit/reset and keyboard controls remain available.
Settling respects reduced motion and pauses offscreen.

![Graph traversal drawn as a node-link view](../site/assets/dashboard/current-graph.png?v=connected)

![Revision-checked graph editor](../site/assets/dashboard/current-graph-edit.png?v=connected)

## Build an index

**Collections → Index** shows HNSW readiness, memory and sampled recall.
Build or rebuild in the background; the current index keeps serving and
writes stay searchable until the replacement is published.

## Monitor the server

Super admins open **Observability → Live monitor** for request rates, errors,
latency, a per-route table and a live log tail with level and text filters and
JSONL export. The server keeps the log in memory only, so it clears on
restart. **Diagnostics** lists slow queries. **Change feed** lists committed
events across the database and needs read access to all collections. Server
health is under **Administration → Health**.

## Administer users and tenants

Super admins manage users, API keys, tenants and audit logs in **Administration**.

Create users with temporary passwords and tenant/graph access. Issue scoped
API keys with 1–365 day expiry; secrets appear once. Tenant suspension blocks
sign-ins and keys but retains data. Drop collections and graphs before deleting
a tenant; deletion also removes its accounts.

The [administration walkthrough](CONSOLE-ADMIN.md#dashboard-administration-walkthrough)
covers every view, including backups, webhooks, TLS and replication.

![Tenants view with vector limits, state and usage](../site/assets/dashboard/current-tenants.png?v=connected)

## Safety

- Every write asks for confirmation and names the collection. Nothing retries
  automatically.
- **Stop waiting** stops the browser from waiting, but the server may still
  finish the work. Check the result before you retry.
- Requests time out after 60 seconds; index operations after one hour.
- Choosing a file sends nothing until you confirm.
- Besides the HttpOnly session cookie, the browser stores only your appearance
  choice, if you opt in, and whether you closed the quick tour. Records,
  queries and tokens are never stored.
