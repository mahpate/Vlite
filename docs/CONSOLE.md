# VLite testing console

The console is a single-database development tool with collection management,
not an embedding generator or production observability system. Open the standalone
Compose UI (default `http://localhost:8081`), enter a bearer token and connect.
The embedded console remains available at the API's `/dashboard` route.
The dashboard/observability features are included in the v0.4.1 server and
container images. The dashboard itself is not bundled in the Python packages.
Install the separate Python embedded or REST package with
`python -m pip install vlite-embedded` or `python -m pip install vlite-client`.
The dashboard uses focused primary views instead of one long tools page. On a
narrow screen, open the navigation drawer; Escape or the backdrop closes it and
returns focus to the navigation control. Collection work is grouped into
Records, Write, Search, Index and Settings tabs. Write separates the record
editor from JSON documents; Search separates the playground, advanced
retrieval and exploration. The selected-record inspector stays available from
Records without leaving the collection workflow, with Payload, Metadata, Vector
and Raw JSON tabs. Navigation reveals the selected view and any containing
advanced panel. The Developer Console remains the dedicated `#requests` view
for the request inspector, REST playground and read-only scenarios; its
inspector shows the latest request only.
The usual Create → Write → Query path is: review/create an empty collection in
Settings, select it as active, write with the record editor or JSON documents,
then query from Search.

## Studio workflow improvements

VLite provides a create → write → query quick start, theme-aware presentation,
onboarding and SDK-code generation. It uses its own REST schema, decimal u64
IDs and typed metadata.

- **Appearance:** Light, Dark or System, with live system-theme changes. The
  existing density selector remains. Appearance persistence is opt-in: check
  **Remember appearance only** to save it in `vlite-appearance`.
  Storage-disabled browsers still apply changes. Neither records nor tokens are
  saved. Existing light/dark preferences are preserved.
- **Quick tour:** a dismissible first-visit orientation highlights Connect →
  Collections → Write JSON → Search → Observability (Diagnostics on an older
  dashboard). It reveals each focused view before scrolling, and sends no
  requests or writes. Dismissal is remembered separately as
  `vlite-studio-tour`; **Quick tour** reopens it at any time.
- **Collection creation:** define a name and dimensions, choose a metric and
  optionally declare metadata indexes as `[{"field":"category","kind":"eq"}]`.
  Supported kinds are `eq`, `range`, `eq_range`. Advanced settings remain JSON;
  explicit metric/index fields override the same advanced settings. **Review
  collection (step 2)** validates and displays the complete request before the
  existing confirmation/create action. Any edit requires a new review. Creating
  does not switch the active collection.
- **Write JSON:** paste an array of 1..10000 documents, at most 4 MiB, with quoted
  decimal string IDs, a finite f32 vector matching the active collection, and
  optional typed metadata / standard-base64 `payload` (or `payload_base64`, not
  both). Example for a three-dimensional collection:

  ```json
  [{"id":"1","vector":[1,0,0],"metadata":{"category":{"type":"string","value":"demo"}},"payload":""}]
  ```

  **Validate / preview** checks every document and duplicate IDs, reports counts
  and the first 20 row errors, and shows sample IDs. All errors must be corrected
  before upserting. Any draft change or collection switch invalidates the preview.
  Confirmation names the target collection. Writes use existing batch routes in
  chunks of 20; progress counts acknowledged records. **Pause after current batch**
  stops before another batch, not during a commit. Each batch is atomic, the whole
  operation is not. Earlier batches remain committed if another fails. Unknown
  outcomes are never retried automatically. The preview is consumed on commit;
  inspect the data before preparing remaining records or manually retrying. Upserts
  replace vector, payload, metadata and TTL, rather than merging fields.
- **Copy as code:** generate from the current search form (including ANN, filter,
  hybrid and rerank parameters), editor upsert, or JSON write draft. Choose POSIX
  curl, Python `vlite-client` (`vlite_client.Client`), or the repository's JavaScript
  `@vlite/client` SDK (`clients/javascript`, currently private / not npm-published).
  Set `VLITE_BASE` to the API origin and `VLITE_TOKEN` locally. SDK examples read
  them from the environment; curl uses a masked authorization placeholder. No
  connected token is passed to the generator. Known credential-shaped fields
  and values are redacted; review the output before execution or sharing because
  vector/metadata content may still be private.
  Write examples retain 20-operation batching; none automatically retries. Editor
  examples deliberately use unconditional upserts, **not** the editor's If-Match
  constraint. If browser clipboard access is unavailable, copy the text manually.
- **Vector projection:** the existing Explore / compare tool already provides
  an original approximate two-axis PCA SVG for the first 32 sampled records
  (at most 256 dimensions, 4-MiB working set). Select a point or its keyboard
  accessible ID button to load the record editor. The pure helper additionally
  rejects more than 2000 points or invalid/mismatched vectors. This is a bounded,
  non-snapshot view, not UMAP/t-SNE or a guarantee of neighbor preservation.

## Observability: live metrics and logs

Connect with the bootstrap admin token and open **Observability**. The panel
shows request rate, error percentage, request p50/p95, search rate/latency,
vector/collection counts and in-flight requests, with bounded sparklines. Rates
need two samples; `—` means unavailable, not zero. Sort the per-route table by
count, errors or p50/p95/p99. Metrics use `GET /v1/admin/metrics/summary`; older
servers returning 404 fall back to Prometheus `/metrics`, with lifetime
histogram estimates rather than the summary's recent-window percentiles.
Summary percentiles use the latest 1024 completed samples per route.

The live log tail uses `GET /v1/admin/logs` with a `since` sequence cursor,
minimum `level` (`trace`, `debug`, `info`, `warn`, `error`), case-insensitive `q`
(message/target text), and `limit` (1..1000, default 200). Responses contain
`entries`, `next_seq`, `dropped`, and `capacity`. Use severity/text filters,
target chips, auto-scroll, pause/resume tail and JSONL export of the filtered
browser buffer. The UI retains at most 5000 entries and displays the newest
500 matching rows. An independent monitoring pause stops refreshes; background
polling also stops when the tab/panel is hidden or disconnected. Slow queries
and audit entries appear alongside the tail when the credential permits them.
Unauthorized/unsupported endpoints show an explanation rather than fake data.

The server's structured ring is **memory-only**, default 2000 entries; set
`vlite-server ... --log-buffer N` (1..100000) to change it. Old entries are
counted as dropped when overwritten, and all entries disappear on restart.
It is not a replacement for the persistent administrative audit log or a
production monitoring/log collector. Sanitized HTTP access events record route
templates, status and duration without raw URLs, tokens, vectors or payloads.
Exports can still contain operational details: review them before sharing.
These admin endpoints require a bootstrap admin credential; a read-only token
or a collection-scoped token does not grant global log/summary access.

## Testing workflows

- **Guided demo:** prepare three dimension-matched vectors, inspect the preview,
  then explicitly confirm replacement of IDs `1`, `2`, `3`. Use a dedicated test
  database; demo records are not isolated from existing records.
- **Import:** select a JSON array or CSV with headers `id,vector` and optional
  `payload_base64,metadata`. CSV vector/metadata fields contain JSON and must be
  quoted/escaped according to CSV rules. IDs must be decimal strings, including
  in JSON. Typed integer metadata uses decimal strings, not JavaScript numbers.
  Preview validates dimensions, f32 range, IDs, metadata and base64 before writing.
  Limits: 100 records, 4 MiB, 20 operations per atomic batch.
  The whole import is **not** atomic. If a later batch fails, earlier batches remain
  committed; if a response is lost, the last batch's outcome can be unknown.
  The preview is consumed when committing so it cannot be blindly replayed.
- **Export:** export the currently loaded browse page as full-record JSON or CSV,
  or export search hits as JSON. Full-record exports fetch at most 100 records
  sequentially, stop above a 4-MiB working set, and are not snapshots. Prefer JSON for exact machine round trips;
  CSV prefixes spreadsheet formula-leading cells with an apostrophe for safety.
- **Filter builder:** add equality or inclusive numeric lower/upper range
  conditions joined by AND. The generated JSON stays editable. Filters use exact
  and filter-aware ANN search. The builder preserves the selected mode. ANN
  retains matching candidates during traversal and uses nonmatching nodes as
  graph bridges, not a final filter on unfiltered top-k. It can return fewer than
  k matches; no hidden exact-search fallback is performed. Selective/no-match
  filters can traverse many or all reachable nodes; ef is not a visit/time cap.
- **Search comparison:** run exact search followed by matching ready-only ANN
  with the same filter. The output includes exact/ANN result counts, ID overlap
  and separate browser-observed times, not engine-only timings or a general
  recall claim. Overlap is null when exact returns no matches. The reads are
  not a snapshot. No index is built implicitly by the comparison.
- **Batch editing:** enter unique IDs from the loaded page for deletion or
  replacement of entire metadata maps. Operations are confirmed and divided
  into batches of 20. Metadata replacement first reads full records, then
  rewrites them; it is not compare-and-swap and can overwrite concurrent edits.
- **REST playground:** submit allowlisted same-server requests; mutations require
  confirmation. Generated curl, Python and Node examples use environment variable
  placeholders, never the connected token. No external origins are accepted.
- **Scenarios:** download/load token-free JSON test definitions with 1..20
  read-only steps. Each step is a record GET or search with expected ordered IDs.
  Replay shows expected and actual results and pass/fail. Definitions cannot
  contain authorization headers or credential fields. Data and vectors may still
  be private: share scenario files accordingly.

## Record exploration

The record browser offers selectable typed metadata columns and ascending or
descending sorting of the **current loaded page only**, never a database-wide
sort. IDs and integer metadata retain full-width precision. Missing fields sort
last; mixed nonnumeric types group by type. Selecting a row loads a bounded
inline vector/payload preview and the editor without leaving the browse area.

The visual metadata editor supports string, integer, float and boolean fields.
Loading a record replaces its visual draft. After editing raw JSON, use
**Load JSON into fields** before making visual edits. **Apply fields to JSON
draft** validates names, duplicate fields, typed values and size limits, but does
not write to the database. **Put / replace** remains a separate confirmed write.

Named query files preserve the vector, exact filter, metric, search mode, k,
search ef and HNSW configuration. **Download query** exports a bounded versioned
JSON file; **Load query into form** validates the entire file before replacing
the form, with no request sent. Use **Search** to replay it. Files contain query
data that may be private, never authentication credentials; no query history is
persisted in browser storage. Loading a different-dimensional query is rejected.

“Find similar to saved ID” fetches the committed vector and runs an exact,
unfiltered search. Selecting a browse/search/sample ID loads the editor.
Record comparison shows both complete records and squared L2, cosine and negative
inner product. Cosine is unavailable for a zero vector.

Payload previews decode valid UTF-8 and optionally format JSON, always as text.
Binary payloads stay base64. Vector slices expose 32 components at a chosen
offset; the original full vector remains editable.

Sample analysis reads the first 32 records (up to a 4-MiB working set), reports zero vectors, exact duplicate
vectors, metadata types, mixed types and sampled value counts. This is a live,
non-snapshot sample, **not a whole-database audit**. The PCA plot estimates two
principal components using centered power iteration for samples with at least
two records and at most 256 dimensions. Coordinates are independently scaled
to fit; visual distances are not original metric distances. Each plotted record
has a keyboard-accessible inspection button.

Squared L2 sums squared component differences. Cosine distance is one minus
normalized similarity. Inner product returns negative dot product. Lower is
better for every distance; none is a universal confidence percentage. Hybrid
search instead returns a fused **score** where higher is better (see below).

## Safety, accessibility and preferences

Tokens remain in memory only. Disconnect/reload removes the credential; disconnect
clears data panels, samples, previews and generated examples. Appearance can be
changed without persistence. Only checking **Remember appearance only** writes
theme/density to local storage; no token, vector, payload or scenario is stored.
Unchecking removes that preference.

Sidebar anchors and the skip link support keyboard navigation. Operations and
errors use live status announcements; search/sample inspection moves focus to
the editor, while browse selections retain their place and show an inline preview.
Light/dark themes, compact/comfortable density and responsive tables are available.

Selecting a file does not send it to the API. Import preview and resumable-file
validation parse locally; loading a saved query validates and fills the form
without searching. Loading a scenario file validates it locally; its read-only
requests run only after **Run scenario**. Imports require an explicit preview
and confirmed commit/start action. Database workflows are serialized by the UI;
this is not a concurrent-request runner. Navigation, appearance, help,
cancellation, disconnect and existing pause controls remain available while a
workflow is running. Search keeps the previous results and plan until a refresh
succeeds, while its status reports that refresh or a failure.

**Stop waiting** aborts the browser's fetch and stops waiting for its response;
it does not cancel work already accepted by the server. The UI warns that server
work may continue. Ordinary requests time out after 60 seconds, index operations
after one hour. A disconnected writer can still finish. Verify mutation
outcomes before any manual retry. Error help covers authorization, dimensions,
index readiness, overload and unknown outcomes; writes are never retried
automatically.

Health refresh is opt-in every 10 seconds only while the page is visible,
connected and idle. It reads existing metrics and index status with a timestamp.
Overview counters are snapshots and invalidated after mutations or indeterminate
write failures. Compatibility
diagnostics show the connected server's version and advertised capabilities.

See [server administration](CONSOLE-ADMIN.md) for credential roles, reload,
audit, maintenance and backup/restore behavior, and
[Docker operations](DOCKER-OPERATIONS.md) for versioned deployment and rollback.

## Collections and newer server features

Every newer surface is enabled only when `GET /v1/capabilities` advertises its
feature flag (and the credential's role or scope allows the operation). Older
servers keep the original console; unavailable controls stay disabled and the
Administration note lists which features are and are not available. Nothing is
inferred from a version number.

| Surface | Flag(s) | Routes used |
|---|---|---|
| Collection picker, list, create/drop empty, settings | `collections` (picker, list also work on format v3 with only `default`) | `GET/POST /v1/collections`, `GET/DELETE /v1/collections/{c}`, `PUT /v1/collections/{c}/settings` |
| Record revisions / If-Match | `per_record_etags` | `ETag` header + `revision` on GET; `If-Match` on PUT/DELETE |
| Auto search, Explain, Hybrid | `search_auto`, `search_explain`, `search_hybrid` | `POST .../search` (`mode`, `explain`, `text`, `fusion`, `vector_leg`) |
| Quantization / rerank | `quantization` | search, build, rebuild and quality bodies |
| Index memory and recall estimate | format v4/v3 collection routes + `index` permission | `GET .../index/hnsw` (`memory`), `POST .../index/hnsw/quality` |
| Resumable import | `imports` | `POST /v1/collections/{c}/imports`, `PUT /v1/imports/{job}/chunks/{n}`, `GET /v1/imports/{job}`, `POST .../finish`, `POST .../abort` |
| Automatic maintenance | `auto_maintenance` | `GET/PUT /v1/admin/maintenance/auto`, `POST .../auto/tick` |
| Backup schedule, verify, drill | `backup_schedule`, `backup_verify_drill` | `GET/PUT /v1/admin/backups/schedule`, `POST /v1/admin/backups/{name}/verify`, `.../drill` |
| Slow queries, Prometheus text | `slow_query_log`, `prometheus_metrics` | `GET /v1/admin/diagnostics/slow-queries`, `GET /metrics?format=prometheus` |
| Scoped credentials, persistent audit | `scoped_credentials`, `persistent_audit` | `GET/POST /v1/admin/credentials`, `POST .../{id}/rotate`, `.../revoke`, `GET /v1/admin/audit/persistent` |

**Active collection.** The sidebar picker selects one collection for every
browse, editor, search, saved-query, import, index, compare and scenario action.
`default` keeps the legacy `/v1/...` routes; any other collection uses
`/v1/collections/{name}/...` and the server never falls back to `default`.
Settings-sensitive actions (search, index status/build/rebuild, maintenance
read/tick, quality, memory, compare, health) use `/v1/collections/{name}/...`
for every collection, including `default`, whenever the server reports
`format_version`, because the legacy routes apply historical defaults
(squared L2, core HNSW, no quantization) and ignore stored settings. The
console also sends metric/config (and quantization/rerank for ANN modes)
explicitly, pre-filled from the selected collection. Older servers keep the
legacy routes. The maintenance policy save (`PUT /v1/index/hnsw/maintenance`)
exists only on the legacy route: it is enabled only with `default` selected,
its thresholds are global, and named collections always use their own stored
settings as index identity.
Switching collections first lists exactly what would be discarded (unsaved
editor changes, loaded record and revision, search results and query, import
previews, prepared import files, metadata drafts, batch IDs) and asks for
confirmation; Cancel keeps the current collection. After confirmation the
console re-reads the target collection, clears all collection-bound state and
pre-fills metric/HNSW/quantization from its settings, and the record/query
example vectors for its dimensions; a vector whose length does not match the
selected collection is rejected before sending. If a ready index was built
with an explicit identity that differs from the stored settings, the form
follows the ready index (so ready-only and auto searches use it) and the index
status says so. A result that arrives
after a switch is discarded, and every write re-checks that the collection did
not change between confirmation and send. Confirmations always name the
collection and route.

**Create, drop and settings.** Create makes an *empty* collection with fixed
dimensions and optional partial settings. Drop only succeeds for an empty
non-default collection. Settings saves are partial merges; on a format v3 file
`default` settings are memory-only (`persisted: false` is shown). A v3 database
is never upgraded by the console: collection creation returns
`requires_format_v4`, and the console explains that upgrading is an explicit,
offline operator step (back up first; no automatic migration).

**Search modes.** Exact, approximate and ready-only approximate return
distances (lower is better). **Auto** lets the server planner choose exact
scan, exact over indexed candidates or ready (filtered) ANN; it never builds an
index. **Explain** shows the plan (auto only) and work counters. **Hybrid**
fuses BM25 over the collection's declared `text_fields` with the vector leg by
weighted reciprocal rank fusion. Its table shows score (higher is better),
distance, BM25 and both ranks; `--` marks a leg that did not return the record.
Read-only and scoped credentials may use every mode except lazily built
`approximate` (which needs admin or the `search_build` operation).

**Revisions.** When the server returns an ETag, **Only if unchanged** (on by
default) sends `If-Match` with the tag of the exact record last loaded from the
same collection. A 412 means nothing was written; Get the record again and
reapply the edit.

**Index memory and quality.** Memory shows only the component byte counts the
server reports plus their sum; it is not process memory. Recall@k is a bounded,
sampled estimate (1..32 samples, k 1..100, distance-evaluation budget) against a
ready index; it may be skipped for budget or an empty collection.

**Resumable import.** Validate/split parses JSON, NDJSON or CSV locally and
splits it deterministically into chunks (1..4096 rows). Start reserves a job
and sends chunks in order; each chunk commits atomically. A rejected chunk
(`reject_chunk` policy) writes nothing and shows the invalid-row report;
`skip_invalid` commits valid rows and reports the skipped ones. Unknown
outcomes (network failure, timeout) stop the import: there are no blind
retries. **Resume** reads the durable checkpoint, replays the last committed
chunk with the same file and chunk size (it must report `already_committed`),
then continues. Stop pauses after the current chunk. Finish closes the job;
Abort stops it. In every case previously committed chunks are retained; the
confirmations say so. A job reserved without a committed chunk is not durable.

**Schedulers.** Automatic maintenance and scheduled backups are off by default.
Saves are partial merges, memory-only (restart restores the environment
configuration) and always confirmed. **Run tick now** (`POST
/v1/admin/maintenance/auto/tick`) and **Run schedule now** (`POST
/v1/admin/backups/schedule/run`) run exactly one confirmed cycle (admin only). Read the backup schedule first: the
confirmation describes the merged result and calls out retention deletion
explicitly when retention is enabled. Verify checks a backup manifest; a drill
restores a temporary copy in the admin directory, queries it and deletes it.

**Diagnostics.** The slow-query log shows request metadata, plan and counters
only (no vectors, text or filter values) and is process-local. Prometheus text
is shown truncated to 64 KiB. Scoped credentials need the `diagnostics`
operation.

**Scoped credentials.** Admins create credentials for named collections (or
`*`) with selected operations and a mandatory expiry (1..365 days). The new or
rotated secret is shown once, masked, in memory only: **Copy secret** writes it
to the clipboard (HTTPS or localhost), **Clear secret** removes it, and
disconnect clears it. Secrets are never written to the request inspector,
status text, history, saved queries, scenarios, local/session storage or the
URL; create/rotate responses are withheld from the inspector and the REST
playground refuses credential creation/rotation. Authorization headers and any
`token`-like field are redacted from inspector output. Grants are bound to the
collection identity and database lineage, so a dropped/recreated collection or
restored database makes the credential fail closed (403 `forbidden_scope`).
A 503 `credentials_commit_indeterminate` means the change IS in effect but
may not survive a crash and no secret was returned: the console says so, re-reads
the list and leaves revoking or re-issuing to the admin.

Page exports and batch metadata replacement use only the writable record fields
(`id`, `vector`, `metadata`, `payload_base64`); the server-owned `revision`
is dropped, so exports remain importable.

## Advanced retrieval, expiry and change feed

The V5 controls use the existing bearer token, active collection, request inspector
and scope gates. They do not persist drafts or credentials in browser storage.
Unsupported feature controls remain disabled. The assets are embedded with
`include_str!` by the Rust server and are also used by the standalone dashboard;
there are no additional scripts, dependencies or external connections.

- **TTL on put:** enter a nonnegative integer in milliseconds. Blank omits expiry
  on replacement; zero expires immediately. Expiry requires a V5 database. Get
  clears the TTL draft: a relative lifetime is never silently copied/restarted.
- **Save metadata only:** replaces the entire typed metadata map using PATCH,
  retaining vector, payload, expiry and additional vectors. The same optional
  If-Match from Get applies; a 412 requires reloading before another conditional
  edit. Other editor drafts are not saved by this action.
- **Candidate reranking:** exact f32 or MMR on ordinary vector search. Candidates
  must be at least k and at most 1,000. MMR lambda ranges from 0 (diversity) to 1
  (relevance); it requires a nonzero query. It is not available for lexical hybrid.
  Version 2 saved queries preserve optional reranking and still load older files.
- **Group by:** grouped search uses the playground vector, metric and filter,
  with independent group and per-group limits. Values and hits are displayed as
  safe JSON, not flattened into the ordinary neighbor list.
- **Geo radius:** uses the playground vector/metric/filter and a location field,
  latitude, longitude, radius in meters and vector/geographic sort selection.
- **Named vectors:** enter a declared dense name, optional model version and query
  vector. Named dimensions are independent of the base collection dimensions.
  **Dense + sparse** adds a sparse name and sorted unique `[u32_index, f32_value]`
  pairs. Fusion uses equal weights, RRF k=60 and 100 candidates per leg. Higher
  fused scores are better; named distances are lower-is-better. These routes do
  not accept the playground metadata filter or reranking; no filter is silently
  sent or applied. Configure/store additional vectors through the API or SDK.
- **Recommend:** positive/negative decimal-string IDs and/or vector examples;
  uses the playground k, metric and filter. A positive example is required.
- **Facets / numeric stats:** aggregates matching collection records with the
  playground filter, not a browser sample or only the current search results.
- **Query language:** a bounded read-only SEARCH statement and JSON vector
  parameter map. See [VECTORS.md](VECTORS.md) for grammar, typing and limits;
  this is not SQL or a script engine. Read-only credentials can use these POST
  retrieval routes; scoped credentials require search for the active collection.
- **Change feed:** database-wide, not selected-collection events. Requires read
  across all collections (a scoped credential must include `*`). Load a supplied
  opaque cursor or begin retained history with a blank cursor; Next page follows
  the returned cursor while `has_more` is true. The cursor is encoded as a query
  parameter without interpretation. Limits are approximate because commits are
  not split. To check for later events, Load the last next cursor explicitly.
  Compacted-history errors require consumer resynchronization. Expiry emits no
  independent event. Disconnect clears displayed events and cursor state.

Advanced results are text-only JSON, cleared on collection switches; the global
feed and admin views remain database-wide until disconnect/reconnect. Disk ANN,
PITR, webhooks and the encryption badge are described in
[CONSOLE-ADMIN.md](CONSOLE-ADMIN.md).
