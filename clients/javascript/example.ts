import { Client, VLiteError, type Condition, type Metadata } from "./index.js";

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
}
void typeChecks;
