import { NativeLibrary, EmbeddedError } from "./index.js";

export async function example(path: string): Promise<void> {
  const native = new NativeLibrary();
  try {
    const db = await native.create(path, 3);
    try {
      await db.put("18446744073709551615", [1, 0, 0], {
        payload: new Uint8Array([0, 255]),
        metadata: { category: { type: "string", value: "example" } },
      });
      console.log(await db.search([1, 0, 0], 5));
      console.log(await db.search([1, 0, 0], 5, { metric: "cosine" }));
      await db.buildIndex({ metric: "cosine" });
      console.log(await db.searchApproximateReady([1, 0, 0], 5, { metric: "cosine" }));
      console.log(await db.getMany(["0", "18446744073709551615"]));
    } finally {
      await db.close();
    }
  } catch (error) {
    if (error instanceof EmbeddedError) console.error(error.code);
    throw error;
  } finally {
    await native.close();
  }
}

function checkTypes(db: Awaited<ReturnType<NativeLibrary["open"]>>) {
  // @ts-expect-error IDs must remain decimal strings.
  db.get(1);
  // @ts-expect-error i64 metadata must not use lossy JS numbers.
  db.put("1", [1], { metadata: { n: { type: "integer", value: 1 } } });
}
void checkTypes;

export async function collections(native: NativeLibrary, path: string): Promise<void> {
  const db = await native.create(path, 2, { format: 4 });
  try {
    await db.createCollection("docs", 2, { textFields: ["title"], metadataIndexes: [{ field: "tag", kind: "eq" }] });
    const revision: string = await db.put("1", [1, 0], { collection: "docs", expectedRevision: "absent" });
    const unconditional: void = await db.put("2", [0, 1], { collection: "docs" });
    void unconditional;
    await db.put("1", [0, 1], { collection: "docs", expectedRevision: revision });
    const explained = await db.search([1, 0], 1, { collection: "docs", explain: true });
    console.log(explained.stats.records_scanned);
    const planned = await db.searchAuto([1, 0], 1, { collection: "docs", explain: true });
    console.log(planned.plan?.strategy);
    console.log((await db.searchHybrid("apple", 2, { collection: "docs", vector: [1, 0] })).hits[0]?.bm25);
    const job = await db.nextImportJobId();
    const report = await db.importChunk(job, "1", [{ op: "put", id: "3", vector: [1, 1] }], { collection: "docs" });
    console.log(report.outcome, (await db.importFinish(job, { collection: "docs" })).state);
    // @ts-expect-error import jobs and chunks are decimal strings.
    await db.importStatus(1);
  } finally {
    await db.close();
  }
}
