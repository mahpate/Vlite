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
