# VLite embedded Node.js / TypeScript

Server-free VLite for Node.js 22.9+, using an N-API addon and the versioned VLite
C ABI. Native operations run in a dedicated worker thread rather than blocking
the JavaScript event loop. The ESM package includes TypeScript declarations.
This is distinct from the REST client at `clients/javascript`.

## Build and install

Build on the target OS/architecture with Rust 1.89+, Python, a C++ compiler,
Node.js 22.9+ and npm. Windows needs Visual Studio C++ build tools; Linux needs
make and a C++ toolchain; macOS needs Xcode command-line tools.

```text
npm --prefix bindings/node ci --ignore-scripts
npm --prefix bindings/node run build
npm --prefix bindings/node test
npm --prefix bindings/node run typecheck
```

Run `node bindings/node/pack.mjs <new-output-directory>` from the root to produce
a checksummed tarball containing only the host-native addon and Rust library,
with OS/architecture restrictions in its package metadata.
Verify it using `node tests/node_package.mjs <tarball-path>`.
Install that tarball in your application:
`npm install <path-to-tarball>`. Runtime users need Node and its normal OS
runtime, not Rust, a compiler, Docker, an HTTP service or credentials.
CI builds and verifies a tarball for each native runner. No npm publication,
automatic binary download or universal native package is implied.
Packages must match the OS/architecture and native runtime where they run.
Linux builds are host-specific, not manylinux-certified Node addons.
Windows x64 and Linux x64/Debian 12 packages have been built and installed
locally. CI also builds on the current macOS runner; other architectures and
libc/runtime combinations require their own builds and verification.
For the clean Linux build/install check:

```text
docker build --file bindings/node/Dockerfile --target artifacts --output type=local,dest=node-linux-packages .
```

The final verification stage installs and runs the package without network
access, Rust or a compiler. Release workflows include native tarballs and
checksums; adding that workflow does not itself publish a new release.

```javascript
import { NativeLibrary, EmbeddedError } from "@vlite/embedded";

const native = new NativeLibrary();
try {
  const db = await native.create("documents.vlite", 3);
  try {
    await db.put("18446744073709551615", [1, 0, 0], {
      payload: new Uint8Array([0, 255]),
      metadata: { category: { type: "string", value: "example" } },
    });
    console.log(await db.search([1, 0, 0], 5));
    console.log(await db.getMany(["0", "18446744073709551615"]));
    await db.buildIndex();
    console.log(await db.searchApproximateReady([1, 0, 0], 5));
  } finally {
    await db.close();
  }
} catch (error) {
  if (error instanceof EmbeddedError) console.error(error.code);
  throw error;
} finally {
  await native.close();
}
```

Use `native.open(path)` for an existing file; create never overwrites, and open
never silently creates or repairs. Records return padded `payload_base64`;
`put` accepts `Uint8Array`/Buffer for binary input. Batch operations use the
existing JSON `payload_base64` shape, not Buffers. Full-width IDs and i64
metadata remain strings. `getMany` retains order/duplicates and returns null
for missing IDs, with 100-ID/8-MiB bounds. Put replaces all record components.
Exact search accepts typed filters; ANN is ready-only with explicit builds.
See [ABI contracts and limitations](../README.md).

## Lifetime and failure behavior

Each `NativeLibrary` owns one worker. Always await `native.close()` in `finally`,
including after failed database creation. It drains accepted requests, closes
remaining database handles and stops the worker. Do not forcibly terminate a
worker during a write; a disconnect/crash may leave the mutation outcome unknown.
Database close rejects new database calls and drains previously admitted ones.
Explicit close is required; there is no finalizer that hides failures.

The queue admits at most 64 pending operations by default (`maxPending`: 1..1024);
`busy` means rejection before admission. Library shutdown bypasses that cap so
cleanup can always be requested. A database close rejected as `busy` retains its
handle; await admitted work before explicitly retrying close. No writes are
automatically retried. Requests are limited to 8 MiB. JSON encoding on the main
thread is still synchronous and bounded; native work runs in the worker.
This is asynchronous scheduling, not a query latency guarantee.
The internal worker does not inherit the application's `execArgv` flags/loaders.

`EmbeddedError.code` preserves engine codes, including `commit_indeterminate`,
`poisoned`, `locked`, `not_found` and `index_not_ready`. `worker_failed` and
`native_failure` mean a native outcome may be unknown; do not blindly retry a
mutation. Native process crashes cannot be contained by a worker thread.
The native ABI serializes all handles through one mutex; multiple workers do
not make the engine parallel. File ownership remains exclusive across
processes/REST/native bindings.

For advanced deployments, pass absolute `libraryPath` and `addonPath` to the
constructor. There is no PATH search or automatic native download. N-API v8
avoids binding to one Node major version, but the native platform must match.
This package is Node-only, not a browser or edge-runtime binding.
