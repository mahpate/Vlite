import {
  Client, Collection, ImportRejectedError, VLiteError,
  type AnnQuality, type BatchOperation, type CollectionIndexStatus, type CollectionInfo, type Condition, type Credential,
  type ImportChunkReport, type ImportStatus, type IssuedCredential, type Metadata, type SearchResponse,
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
