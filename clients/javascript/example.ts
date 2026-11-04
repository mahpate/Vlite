import {
  Client, Collection, ImportRejectedError, Memory, UnsupportedError, VLiteError,
  type AnnQuality, type BatchOperation, type CollectionIndexStatus, type CollectionInfo, type Condition, type Credential,
  type ImportChunkReport, type ImportStatus, type IssuedCredential, type MemoryConsolidateResponse, type MemoryScalar,
  type MemoryTarget, type Metadata, type SearchResponse,
} from "./index.js";

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

// Agent memory: one binding-local facade. Responses are whole objects; u64/i64 outputs are decimal strings.
// A server without memory routes answers with UnsupportedError (404 not_found); nothing is retried or embedded.
export async function memoryExample(baseUrl: string, token: string): Promise<void> {
  const memory: Memory = new Client(baseUrl, token).memory("notes");
  const scope = { agent: "assistant", user: "ada" };
  try {
    // The idempotency key is yours: reuse it to replay a mutation whose outcome is unknown, then getReceipt.
    const remembered = await memory.remember({
      scope, idempotency_key: "remember-1",
      items: [{ id: "ada", kind: "entity", content: "Ada", observed_at_ms: 1700000000000n,
        source: { source_id: "chat", source_kind: "caller" } }],
    });
    console.log(remembered.receipt.id, remembered.receipt.sequence, remembered.outcomes[0]?.revision);
    const found = await memory.recall({ scope, text: "Ada", limit: 5 });
    console.log(found.memories.map((hit) => hit.id), found.meta.complete, found.meta.limitations);
    const receipt = await memory.getReceipt({ scope, operation: "remember", idempotency_key: "remember-1" });
    console.log(receipt.result.replayed);
    const forgotten = await memory.forget({ scope, idempotency_key: "forget-1", mode: "logical_delete",
      target: { ids: [{ id: "ada", revision: remembered.outcomes[0]?.revision ?? 1n }] } });
    // HTTP 202 is an ordinary result: the logical deletion is accepted and the job continues.
    if (forgotten.state === "pending") console.log(forgotten.erasure?.resume_token);
    const plan: MemoryConsolidateResponse = await memory.consolidate({ scope, idempotency_key: "c-1", dry_run: true,
      inputs: [{ id: "ada", revision: 1 }],
      output: { id: "s", kind: "episodic", content: "Summary", observed_at_ms: "1", source: { source_id: "s", source_kind: "caller", transformation_id: "v1" } } });
    console.log(plan.dry_run ? plan.user_operations : plan.receipt.id);
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
