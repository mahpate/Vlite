import {
  Client, Collection, ImportRejectedError, Memory, UnsupportedError, VLiteError,
  type AnnQuality, type BatchOperation, type CollectionIndexStatus, type CollectionInfo, type Condition, type Credential,
  type ImportChunkReport, type ImportStatus, type IssuedCredential, type MemoryConsolidateResponse, type MemoryEffectiveScope,
  type MemoryErasure, type MemoryErasureNotice, type MemoryFactsRequest, type MemoryFeed, type MemoryForgetRequest, type MemoryItem,
  type MemoryMutationResponse, type MemoryReadMeta, type MemoryReceipt, type MemoryReceiptRequest, type MemoryRecallRequest,
  type MemoryResumeRequest, type MemoryScalar, type MemoryTarget, type MemoryView, type Metadata, type SearchResponse,
} from "./index.js";
import type { Expected, KvEntry, KvPage, KvPutResult } from "./index.js";

// Supply the token at runtime; never hard-code real credentials.
export async function example(baseUrl: string, token: string): Promise<void> {
  const client = new Client(baseUrl, token);
  const metadata: Metadata = { category: { type: "string", value: "example" } };
  await client.put("10", { vector: [1, 0, 0], metadata });
  console.log(await client.search({ vector: [1, 0, 0], k: 5 }));
  console.log(await client.search({ vector: [1, 0, 0], k: 5, metric: "cosine" }));
  await client.buildIndex({ metric: "cosine" });
  console.log(await client.search({ vector: [1, 0, 0], k: 5, metric: "cosine", mode: "approximate_ready" }));
  // The same typed AND filter works in exact and approximate modes. Filtered ANN
  // returns at most k matching records (possibly fewer); there is no exact fallback.
  const filter: Condition[] = [
    { op: "eq", field: "category", value: { type: "string", value: "example" } },
    { op: "range", field: "rank", min: { type: "integer", value: "10" }, max: null },
  ];
  console.log(await client.search({ vector: [1, 0, 0], k: 5, filter }));
  console.log(await client.search({ vector: [1, 0, 0], k: 5, metric: "cosine", mode: "approximate_ready", filter, ef_search: 200 }));
  console.log(await client.listVectors({ limit: 25 }));
  // Optimistic concurrency: a 412 means the database changed (or the service
  // restarted) since the read. Refetch and decide; the client never retries.
  const { record, etag } = await client.getVersioned("10");
  try {
    await client.put("10", { vector: [0, 1, 0], payload_base64: record.payload_base64, metadata: record.metadata }, { ifMatch: etag });
  } catch (error) {
    if (!(error instanceof VLiteError && error.status === 412)) throw error;
  }
  try {
    console.log(await client.get("10"));
  } catch (error) {
    if (error instanceof VLiteError) console.error(error.status, error.code, error.requestId);
    throw error;
  }
}

// Ten-capability surface (servers advertising the matching capabilities.features flags).
export async function capabilities(baseUrl: string, token: string, ndjsonPath: string): Promise<void> {
  const client = new Client(baseUrl, token);
  const caps = await client.capabilities();
  if (caps.features.collections !== true) return;
  const created: CollectionInfo = await client.createCollection("docs", 3, {
    metric: "cosine", quantization: "scalar_u8", rerank_multiplier: 4,
    metadata_indexes: [{ field: "category", kind: "eq" }], text_fields: ["title"],
  });
  console.log(created.id, created.settings.hnsw.seed);
  const docs: Collection = client.collection("docs");
  await docs.put("1", { vector: [1, 0, 0], metadata: { title: { type: "string", value: "red apple" } } });
  const { record, etag } = await docs.getVersioned("1");
  console.log(record.revision);
  // Per-record tags: unrelated writes do not invalidate this tag.
  await docs.put("1", { vector: [0, 1, 0], metadata: record.metadata }, { ifMatch: etag });
  const auto: SearchResponse = await docs.searchDetailed({ vector: [1, 0, 0], k: 5, mode: "auto", explain: true });
  console.log(auto.plan?.strategy, auto.stats?.distance_evaluations);
  const hybrid = await docs.search({ mode: "hybrid", text: "apple", k: 5, fusion: { rrf_k: 60 }, vector_leg: "exact" });
  for (const hit of hybrid) if ("score" in hit) console.log(hit.id, hit.score, hit.bm25);
  await docs.buildIndex();
  const status: CollectionIndexStatus = await docs.indexStatus();
  console.log(status.settings_ready, status.memory?.code_bytes);
  const quality: AnnQuality = await docs.annQuality({ samples: 16, k: 5 });
  console.log(quality.recall_at_k, quality.skipped_reason);
  const updated = await client.setCollectionSettings("docs", { planner: { exact_max_candidates: 1000 } });
  console.log(updated.persisted);

  // Imports: a replayed chunk reports already_committed; a rejected chunk throws, never retried.
  const job = await client.beginImport("docs");
  const ops: BatchOperation[] = [{ op: "put", id: "2", vector: [0, 0, 1] }];
  const report: ImportChunkReport = await client.importChunk(job.job, "1", ops);
  console.log(report.outcome, (await client.importChunk(job.job, "1", ops)).outcome);
  try {
    await client.importChunk(job.job, "2", [{ op: "put", id: "3", vector: [1] }]);
  } catch (error) {
    if (!(error instanceof ImportRejectedError)) throw error;
    console.log(error.report.invalid);
  }
  const finished: ImportStatus = await client.finishImport(job.job);
  console.log(finished.state, await client.importNdjson(ndjsonPath, "docs", { chunkSize: 512 }));

  // Admin operations.
  await client.setAutoMaintenance({ enabled: true, interval_secs: 60 });
  await client.setBackupSchedule({ enabled: true, interval_secs: 3600, retain_last: 7 });
  const backup = await client.createBackup();
  console.log((await client.verifyBackup(backup.name)).verified, (await client.drillBackup(backup.name)).drill.status);
  console.log((await client.autoMaintenance()).config.interval_ms, await client.tickAutoMaintenance(), await client.runBackupSchedule());
  console.log((await client.slowQueries()).items.length, (await client.prometheusMetrics()).length);
  // The token is shown once: hand it to the consumer, never log or persist it.
  const issued: IssuedCredential = await client.createCredential({
    name: "reader", collections: ["docs"], operations: ["read", "search"], expires_unix_ms: Date.now() + 86_400_000,
  });
  const scoped = new Client(baseUrl, issued.token);
  console.log((await scoped.capabilities()).scope?.operations);
  const rotated = await client.rotateCredential(issued.credential.id, { graceSeconds: 60 });
  await client.revokeCredential(rotated.previous.id);
  console.log((await client.listCredentials()).items.map((c: Credential) => c.state), await client.persistentAudit({ limit: 10 }));
}

// Key-value: the dedicated one-dimensional collection must already exist. Versions and deadlines are bigint, never Number.
// 404 kv_not_found and 412 precondition_failed are ordinary VLiteErrors; UnsupportedError means the server has no KV routes.
export async function kvExample(baseUrl: string, token: string): Promise<void> {
  const client = new Client(baseUrl, token);
  const address = { collection: "cache", namespace: "app", key: "hello" };
  try {
    const stored: KvPutResult = await client.kvPut({ ...address, value: new TextEncoder().encode("world"),
      expected: "absent", expiresAtMs: BigInt(Date.now() + 60_000) });
    const entry: KvEntry = await client.kvGet(address);
    console.log(new TextDecoder().decode(entry.value), entry.version, entry.expires_at_ms);
    // Compare-and-set: the write succeeds only while the entry still has the version you read, otherwise 412.
    const unchanged: Expected = { revision: stored.version };
    await client.kvPut({ ...address, value: new TextEncoder().encode("again"), expected: unchanged });
    const page: KvPage = await client.kvList({ collection: "cache", namespace: "app", prefix: "he", limit: 100 });
    console.log(page.items.map((item) => item.key), page.next_after);
    // A cancelled or timed-out mutation may have committed: read the entry back before deciding. Nothing is retried.
    console.log(await client.kvDelete({ ...address, expected: "any", signal: AbortSignal.timeout(5_000) }));
  } catch (error) {
    if (error instanceof UnsupportedError) return;
    if (error instanceof VLiteError) console.error(error.status, error.code, error.requestId);
    throw error;
  }
}

// Agent memory: one binding-local facade. Responses are whole objects; u64/i64 outputs are decimal strings.
// A server without memory routes answers with UnsupportedError (404 not_found); nothing is retried or embedded.
export async function memoryExample(baseUrl: string, token: string): Promise<void> {
  const memory: Memory = new Client(baseUrl, token).memory("notes");
  const scope = { agent: "assistant", user: "ada" };
  try {
    // The idempotency key is yours. After an uncertain write, repeat a remember with the same key and body inside the
    // receipt window; getReceipt looks up a consolidate or forget (a remember receipt cannot be looked up).
    const remembered = await memory.remember({
      scope, idempotency_key: "remember-1",
      items: [{ id: "ada", kind: "entity", content: "Ada", observed_at_ms: 1700000000000n,
        source: { source_id: "chat", source_kind: "caller" } }],
    });
    console.log(remembered.receipt.id, remembered.receipt.sequence, remembered.outcomes[0]?.revision);
    // Text and vector recall find only memories that hold an evidence record; entity_seeds reads back what you saved.
    const found = await memory.recall({ scope, entity_seeds: ["ada"], limit: 5 });
    console.log(found.memories.map((hit) => hit.id), found.meta.complete, found.meta.limitations);
    const revision = remembered.outcomes[0]?.revision ?? 1n;
    const consolidated: MemoryConsolidateResponse = await memory.consolidate({ scope, idempotency_key: "c-1",
      inputs: [{ id: "ada", revision }],
      output: { id: "s", kind: "episodic", content: "Summary", observed_at_ms: "1", source: { source_id: "s", source_kind: "caller", transformation_id: "v1" } } });
    if (!consolidated.dry_run) console.log(consolidated.receipt.id);
    const receipt = await memory.getReceipt({ scope, operation: "consolidate", idempotency_key: "c-1" });
    console.log(receipt.result.replayed);
    const forgotten = await memory.forget({ scope, idempotency_key: "forget-1", mode: "logical_delete",
      target: { ids: [{ id: "ada", revision }] } });
    // HTTP 202 is an ordinary result: the logical deletion is accepted and the job continues.
    if (forgotten.state === "pending") console.log(forgotten.erasure?.resume_token);
  } catch (error) {
    if (error instanceof UnsupportedError) return;
    if (error instanceof VLiteError) console.error(error.code, error.status, error.requestId, error.details?.feature);
    throw error;
  }
}

// Compile-time contract assertions; never executed.
function typeChecks(client: Client): void {
  // @ts-expect-error IDs must remain strings, not lossy JavaScript numbers.
  client.get(18446744073709551615);
  // @ts-expect-error Integer metadata must preserve i64 precision as a string.
  client.put("1", { vector: [1], metadata: { n: { type: "integer", value: 1 } } });
  // @ts-expect-error Conditional options accept only ifMatch.
  client.delete("1", { ifmatch: '"x"' });
  // @ts-expect-error Entity tags are strings.
  client.put("1", { vector: [1] }, { ifMatch: 1 });
  // @ts-expect-error Range bounds must be numeric metadata values.
  client.search({ vector: [1], k: 1, mode: "approximate", filter: [{ op: "range", field: "n", min: { type: "string", value: "a" } }] });
  // @ts-expect-error Filter values are typed metadata, not raw JSON scalars.
  client.search({ vector: [1], k: 1, mode: "approximate_ready", filter: [{ op: "eq", field: "n", value: 1 }] });
  // @ts-expect-error The vector is optional only in hybrid mode.
  client.search({ k: 1, mode: "auto" });
  // @ts-expect-error Text and fusion apply only to hybrid search.
  client.search({ vector: [1], k: 1, mode: "exact", text: "apple" });
  // @ts-expect-error Quantization is none or scalar_u8.
  client.collection("docs").buildIndex({ quantization: "pq" });
  // @ts-expect-error Credential operations are a closed set.
  client.createCredential({ collections: ["*"], operations: ["root"], expires_unix_ms: 1 });
  // @ts-expect-error Credential expiry is mandatory.
  client.createCredential({ collections: ["*"], operations: ["read"] });
  // @ts-expect-error Import chunk numbers are decimal strings.
  client.importChunk("1", 1, []);
  // @ts-expect-error Import policy is reject_chunk or skip_invalid.
  client.importChunk("1", "1", [], { policy: "ignore" });
}
void typeChecks;

// Compile-time key-value contract assertions; never executed.
async function kvTypeChecks(client: Client): Promise<void> {
  const address = { collection: "c", namespace: "n", key: "k" };
  const value = new Uint8Array();
  // @ts-expect-error A revision is a bigint, never a lossy JavaScript Number.
  client.kvDelete({ ...address, expected: { revision: 1 } });
  // @ts-expect-error Expected is "any", "absent" or { revision }.
  client.kvPut({ ...address, value, expected: "present" });
  // @ts-expect-error Values are bytes, not strings.
  client.kvPut({ ...address, value: "text" });
  // @ts-expect-error A deadline is a bigint or null, never a Number.
  client.kvPut({ ...address, value, expiresAtMs: 1 });
  // @ts-expect-error The collection is explicit; the client never defaults one.
  client.kvGet({ namespace: "n", key: "k" });
  // @ts-expect-error A list takes an exclusive after, never a key.
  client.kvList({ collection: "c", namespace: "n", key: "k" });
  // @ts-expect-error A signal must be an AbortSignal.
  client.kvGet({ ...address, signal: "stop" });

  // Outputs keep 64-bit values lossless and nullable members nullable.
  const entry = await client.kvGet(address);
  const version: bigint = entry.version;
  const expiry: bigint | null = entry.expires_at_ms;
  const bytes: Uint8Array = entry.value;
  // @ts-expect-error A version is a bigint, never a Number.
  const asNumber: number = entry.version;
  const stored = await client.kvPut({ ...address, value, expiresAtMs: null, signal: AbortSignal.abort() });
  const page = await client.kvList({ collection: "c", namespace: "n", after: "a", limit: 10, signal: AbortSignal.abort() });
  const next: string | null = page.next_after;
  const deleted: boolean = await client.kvDelete({ ...address, expected: { revision: stored.version } });
  void [version, expiry, bytes, asNumber, next, deleted];
}
void kvTypeChecks;

async function memoryTypeChecks(memory: Memory): Promise<void> {
  const scope = { agent: "a" };
  const item = { id: "e", kind: "entity", content: "c", observed_at_ms: "1", source: { source_id: "s", source_kind: "k" } } as const;
  const slot = { subject_id: "e", predicate: "p", slot_key: "k" };
  // @ts-expect-error The idempotency key is required on every mutation; the client never invents one.
  memory.remember({ scope, items: [item] });
  // @ts-expect-error MemoryExpected is {absent: true} or {revision}, never both.
  memory.remember({ scope, idempotency_key: "k", items: [{ ...item, expected: { absent: true, revision: "1" } }] });
  // @ts-expect-error absent can only be true.
  memory.remember({ scope, idempotency_key: "k", items: [{ ...item, expected: { absent: false } }] });
  // @ts-expect-error A forget target selects exactly one variant.
  memory.forget({ scope, idempotency_key: "k", mode: "logical_delete", target: { scope: true, session: true } });
  // @ts-expect-error session and scope targets are the literal true.
  memory.forget({ scope, idempotency_key: "k", mode: "logical_delete", target: { session: false } });
  // @ts-expect-error A scalar value must match its tag.
  memory.recall({ scope, text: "x", filter: { all: [{ field: "n", op: "eq", value: { type: "integer", value: true } }] } });
  // @ts-expect-error Float scalars are numbers.
  memory.recall({ scope, text: "x", filter: { all: [{ field: "n", op: "eq", value: { type: "float", value: "1" } }] } });
  memory.correctFacts({ scope, idempotency_key: "k", slot, series: { id: "f", revision: "1" }, slot_revision: "1",
    // @ts-expect-error The old validity names are not part of the contract.
    interval: { from_ms: "0", to_ms: null }, action: "close", at_ms: "1" });
  memory.correctFacts({ scope, idempotency_key: "k", slot, series: { id: "f", revision: "1" }, slot_revision: "1",
    // @ts-expect-error Bounds are required: null is an explicit unbounded end, omission is not.
    interval: { valid_from_ms: "0" }, action: "close", at_ms: "1" });
  // @ts-expect-error Requests are closed objects: a client tenant is never accepted.
  memory.recall({ scope, text: "x", tenant: "t" });
  // @ts-expect-error A signal belongs to the options, not to the request.
  memory.recall({ scope, text: "x", signal: AbortSignal.abort() });
  // @ts-expect-error Options carry only a signal.
  memory.recall({ scope, text: "x" }, { timeoutMs: 1 });

  // Exact inputs: decimal string, safe integer or bigint for every u64 and i64 position.
  await memory.resumeErasure({ scope, idempotency_key: "k", job_id: "j", revision: 18446744073709551615n, generation: "1", resume_token: "t" });
  await memory.recall({ scope, text: "x", as_of_ms: -9223372036854775808n });
  // @ts-expect-error A boolean is not an integer.
  memory.recall({ scope, text: "x", as_of_ms: true });

  // Outputs are canonical strings, never numbers or bigints.
  const received = await memory.getReceipt({ scope, operation: "remember", idempotency_key: "k" });
  const sequence: string = received.result.receipt.sequence;
  // @ts-expect-error A u64 output is not a number.
  const asNumber: number = received.result.receipt.sequence;
  // @ts-expect-error A u64 output is not a bigint.
  const asBigint: bigint = received.result.receipt.sequence;
  void [sequence, asNumber, asBigint];

  // MemoryConsolidateResponse is discriminated by dry_run.
  const consolidated: MemoryConsolidateResponse = await memory.consolidate({ scope, idempotency_key: "k", inputs: [], output: item });
  if (consolidated.dry_run) {
    const operations: string = consolidated.user_operations;
    // @ts-expect-error A plan has no receipt.
    consolidated.receipt;
    void operations;
  } else {
    const receiptId: string = consolidated.receipt.id;
    void receiptId;
  }

  // MemoryTarget narrows by its key.
  const target: MemoryTarget = (await memory.getErasure({ scope, job_id: "j" })).erasure.target;
  if (target.ids) {
    const first: string | undefined = target.ids[0]?.revision;
    void first;
  } else if (target.series) {
    const series: string = target.series.id;
    void series;
  } else if (target.session) {
    const session: true = target.session;
    void session;
  }

  // MemoryScalar narrows by its tag.
  const view = (await memory.recall({ scope, text: "x" })).memories[0];
  const scalar: MemoryScalar | undefined = view?.metadata?.["m"];
  switch (scalar?.type) {
    case "integer": { const value: string = scalar.value; void value; break; }
    case "float": { const value: number = scalar.value; void value; break; }
    case "boolean": { const value: boolean = scalar.value; void value; break; }
    case "string": { const value: string = scalar.value; void value; break; }
    default: break;
  }

  // Responses keep nullable and optional members as the contract defines them.
  const meta = (await memory.findFacts({ scope, slot })).meta;
  const remaining: string | null = (await memory.forget({ scope, idempotency_key: "k", mode: "logical_delete", target: { scope: true } })).remaining_work;
  const distance: number | undefined = view?.scores.distance;
  const decay: number | null | undefined = view?.scores.D;
  void [meta.complete, remaining, distance, decay];

  // Error details are optional and typed.
  try { await memory.recall({ scope, text: "x" }); } catch (error) {
    if (error instanceof VLiteError) {
      const feature: string | undefined = error.details?.feature;
      const limit: string | undefined = error.details?.limit;
      void [feature, limit];
    }
  }
}
void memoryTypeChecks;

// Required and optional members of the section 8 types, in both directions; never executed.
function memoryShapeChecks(): void {
  const scope: MemoryEffectiveScope = { tenant: "@root", agent: "a" };
  const feed: MemoryFeed = { mode: "ordinary_changes", temporal_complete: false, ttl_complete: false, consumer_purge_acknowledged: false };
  // retry_until_ms and expires_at_ms are optional.
  const receipt: MemoryReceipt = { id: "r1", operation: "remember", scope, sequence: "7", digest_version: "memory-request-v1" };
  const mutation: MemoryMutationResponse = { receipt, replayed: false, outcomes: [{ id: "e1", action: "created", revision: "1" }], feed, remaining_work: null };
  // @ts-expect-error The feed block is always present.
  const noFeed: MemoryMutationResponse = { receipt, replayed: false, outcomes: [], remaining_work: "0" };
  // @ts-expect-error remaining_work is nullable but never omitted.
  const noWork: MemoryMutationResponse = { receipt, replayed: false, outcomes: [], feed };
  // @ts-expect-error The receipt always carries its digest version.
  const noDigest: MemoryReceipt = { id: "r1", operation: "remember", scope, sequence: "7" };
  // resume_token is absent once an erasure finished; remaining_count is null when unknown, never omitted.
  const erasure: MemoryErasure = { id: "j", scope, target: { scope: true }, revision: "8", generation: "1", state: "pending",
    logical_visibility: "hidden", deleted_count: "0", remaining_count: null };
  // @ts-expect-error remaining_count is nullable but never omitted.
  const noRemaining: MemoryErasure = { id: "j", scope, target: { scope: true }, revision: "8", generation: "1", state: "pending", logical_visibility: "hidden", deleted_count: "0" };
  // manifest_id is absent for retention expiry.
  const notice: MemoryErasureNotice = { live_state_only: true, secure_erasure: false, external_copies: "operator_managed",
    restore_requires_manifest: true, backup_history_purge: "not_verified" };
  // @ts-expect-error The notice never claims secure erasure.
  const secure: MemoryErasureNotice = { ...notice, secure_erasure: true };
  const view: MemoryView = { id: "a", revision: "1", scope, kind: "entity", content: "a", observed_at_ms: "90", created_at_ms: "100",
    updated_at_ms: "100", importance: 0.5, status: "accepted", stale: false, segment: null, evidence: [], scores: { R: 1, D: null, I: 0.5, S: 1 } };
  // @ts-expect-error segment is nullable but never omitted.
  const noSegment: MemoryView = { ...view, segment: undefined };
  // @ts-expect-error D is null when decay is off, never omitted.
  const noDecay: MemoryView = { ...view, scores: { R: 1, I: 0.5, S: 1 } };
  const meta: MemoryReadMeta = { scopes: [scope], as_of_ms: "100", ttl_clock_ms: "100", sequence: "6", complete: true, truncation: [],
    limitations: ["retained_state_only"], mode: "exact_enumeration", candidates_examined: "0", edges_charged: "0", vector_components: "0",
    mmr_components: "0", evidence_bytes: "0" };
  // @ts-expect-error Read metadata counters are strings.
  const numeric: MemoryReadMeta = { ...meta, candidates_examined: 0 };
  void [mutation, noFeed, noWork, noDigest, erasure, noRemaining, notice, secure, view, noSegment, noDecay, meta, numeric];

  // Requests: only the contract's required members are required.
  const source = { source_id: "s", source_kind: "k" };
  const item: MemoryItem = { id: "e", kind: "entity", content: "c", observed_at_ms: 1, source };
  // @ts-expect-error observed_at_ms is required.
  const noObserved: MemoryItem = { id: "e", kind: "entity", content: "c", source };
  const recall: MemoryRecallRequest = { scope: { agent: "a" } };
  // @ts-expect-error The scope is required.
  const noScope: MemoryRecallRequest = { text: "x" };
  const facts: MemoryFactsRequest = { scope: { agent: "a" }, slot: { subject_id: "e", predicate: "p", slot_key: "k" } };
  // @ts-expect-error The slot is required.
  const noSlot: MemoryFactsRequest = { scope: { agent: "a" } };
  // @ts-expect-error A forget request needs its mode.
  const noMode: MemoryForgetRequest = { scope: { agent: "a" }, idempotency_key: "k", target: { scope: true } };
  // @ts-expect-error A resume request needs its token.
  const noToken: MemoryResumeRequest = { scope: { agent: "a" }, idempotency_key: "k", job_id: "j", revision: "1", generation: "1" };
  // @ts-expect-error A receipt lookup needs its operation.
  const noOperation: MemoryReceiptRequest = { scope: { agent: "a" }, idempotency_key: "k" };
  void [item, noObserved, recall, noScope, facts, noSlot, noMode, noToken, noOperation];
}
void memoryShapeChecks;
