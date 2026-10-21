# VLite testing console

The console is a single-database development tool, not a collections manager,
embedding generator or production observability system. Open the standalone
Compose UI (default `http://localhost:8081`), enter a bearer token and connect.
The embedded console remains available at the API's `/dashboard` route.

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
  search; no silent ANN post-filtering.
- **Search comparison:** run an unfiltered exact search followed by matching
  ready-only ANN. The output includes ID overlap and separate browser-observed
  times, not engine-only timings or a general recall claim. No index is built
  implicitly by the comparison.
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
better in all cases; none is a universal confidence percentage.

## Safety, accessibility and preferences

Tokens remain in memory only. Disconnect/reload removes the credential; disconnect
clears data panels, samples, previews and generated examples. Appearance can be
changed without persistence. Only checking **Remember appearance only** writes
theme/density to local storage; no token, vector, payload or scenario is stored.
Unchecking removes that preference.

Sidebar anchors and the skip link support keyboard navigation. Operations and
errors use live status announcements; record inspection moves focus to the editor.
Light/dark themes, compact/comfortable density and responsive tables are available.

**Stop waiting** aborts the browser fetch, not necessarily server work. Ordinary
requests time out after 60 seconds, index operations after one hour. A disconnected
writer can still finish. Verify mutation outcomes before any manual retry. Error
help covers authorization, dimensions, index readiness, overload and unknown
outcomes; it never automatically retries writes.

Health refresh is opt-in every 10 seconds only while the page is visible,
connected and idle. It reads existing metrics and index status with a timestamp.
Overview counters are snapshots and invalidated after mutations or indeterminate
write failures. Compatibility
diagnostics show the connected server's version and advertised capabilities.

See [server administration](CONSOLE-ADMIN.md) for credential roles, reload,
audit, maintenance and backup/restore behavior, and
[Docker operations](DOCKER-OPERATIONS.md) for versioned deployment and rollback.
