import { Client, VLiteError, type Metadata } from "./index.js";

// Supply the token at runtime; never hard-code real credentials.
export async function example(baseUrl: string, token: string): Promise<void> {
  const client = new Client(baseUrl, token);
  const metadata: Metadata = { category: { type: "string", value: "example" } };
  await client.put("10", { vector: [1, 0, 0], metadata });
  console.log(await client.search({ vector: [1, 0, 0], k: 5 }));
  console.log(await client.listVectors({ limit: 25 }));
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
}
void typeChecks;
