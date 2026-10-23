# Install a versioned VLite release

Download packages from [GitHub releases](https://github.com/mahpate/Vlite/releases).
There is no need to clone the repository or install Rust when using a prebuilt
SDK. Packages are not published to PyPI or npm; do not assume an unqualified
package-registry name belongs to this project.

## Verify before installing

Download `SHA256SUMS`, `release-manifest.json`, and the desired assets from the
same release. The manifest records their sizes, hashes and default source
commit. An asset's own `source_commit` records its provenance when a corrective
packaging build differs from the original tagged source.
On Linux, check downloaded files with:

```sh
sha256sum --ignore-missing --check SHA256SUMS
```

On macOS, use `shasum -a 256 <downloaded-file>` and compare the hash with
its entry in `SHA256SUMS`.

On Windows, calculate a file's hash and compare it with its entry in
`SHA256SUMS`:

```powershell
Get-FileHash .\vlite-embedded-0.3.0-win32-x64.tgz -Algorithm SHA256
```

Checksums detect altered or incomplete downloads; they are not a publisher
signature. Obtain both packages and hashes from the intended GitHub release.

## Embedded Python

Requires Python 3.11+. Choose a `vlite_embedded-*.whl` matching your OS and
architecture, then install the downloaded filename:

```text
python -m pip install ./vlite_embedded-0.3.0-py3-none-win_amd64.whl
```

That example is for Windows x64. Linux x86_64/glibc 2.28+ should use the
`manylinux_2_28_x86_64` wheel. Do not use a host-native `linux_x86_64` wheel
unless its build baseline matches your runtime. macOS wheels include the
architecture and minimum OS version in their names; use a matching wheel.
There is no universal, musl/Alpine or general ARM support claim.

The initial v0.1.0 `universal2` macOS Python wheel was withdrawn after final
inspection found it contained only ARM64 code. Do not install cached copies.
Use the corrected `macosx_11_0_arm64` wheel on Apple Silicon; Intel macOS
requires a source build. The release manifest records the corrective wheel's
packaging source commit. The macOS Node package and native archive are
unaffected. Builds now verify the actual Mach-O architecture against the
wheel tag.

```python
from vlite_embedded import NativeLibrary

with NativeLibrary().create("example.vlite", dimensions=3) as db:
    db.put("1", [1, 0, 0])
    print(db.search([1, 0, 0], 1))
```

The wheel includes the native library. No Rust compiler or server is required.

Version v0.1.2+ supports cosine ANN with an explicit matching index:

```python
with NativeLibrary().open("example.vlite") as db:
    db.build_index(metric="cosine")
    print(db.search_approximate_ready([1, 0, 0], 5, metric="cosine"))
```

The default remains squared L2. Each owner keeps one in-memory graph:
building another metric replaces it, and ready-only queries reject a mismatched
metric without rebuilding. Reopen loses the index. Inner-product search remains
exact-only. No file format migration is required; back up before any upgrade.

## Embedded Node.js / TypeScript

Requires Node.js 22.9+. Choose a host-matching tarball:

```text
npm install ./vlite-embedded-0.3.0-win32-x64.tgz
```

Use `linux-x64` or the macOS `darwin-<architecture>` asset instead on those
platforms. Linux Node packages are host-native builds, not manylinux-certified;
the Python wheel's portability policy does not apply to them. Native OS runtime
dependencies still apply. Browser/edge runtimes are not supported.

```javascript
import { NativeLibrary } from "@vlite/embedded";

const native = new NativeLibrary();
try {
  const db = await native.create("example.vlite", 3);
  await db.put("1", [1, 0, 0]);
  console.log(await db.search([1, 0, 0], 1));
} finally {
  await native.close();
}
```

The tarball includes the addon, native library and TypeScript types. Installation
does not compile or download native code. See [ownership and failure contracts](../bindings/node/README.md).

## REST clients

These packages are portable clients, not embedded engines. Deploy an
authenticated VLite server separately and provide its URL and token.
Python 3.11+:

```text
python -m pip install ./vlite_client-0.3.0-py3-none-any.whl
```

Node.js 22+:

```text
npm install ./vlite-client-0.3.0.tgz
```

Import `Client` from `vlite_client` in Python or `@vlite/client` in JavaScript.
See [REST client examples](../clients/README.md). Any other HTTP-capable language
can use the [OpenAPI contract](../src/openapi.json).

## Native tools and C ABI

Extract the `vlite-<Rust-target>.zip` matching your platform. It contains the
server, maintenance tools, native library, C header, documentation and MIT
license. The Linux arm64 archive contains command-line binaries, contracts and
documentation, not a C shared library or embedded SDK. Python and Node consumers should prefer the self-contained packages
above rather than copying native libraries manually.

One process owns a database file at a time. Use the service when applications
need shared access. The live dataset must fit in RAM. Back up before upgrading;
v0.1.x releases do not claim that a 1.0 stability contract
or physical power-loss certification has been achieved.

## Additional 0.3.0 packages

See [0.3.0 upgrade and rollback guidance](RELEASE-0.3.0.md) before selecting a
new binary or database format. The native archives also ship the offline shell,
import, tuning and quality tools. Go/.NET clients, the Helm chart and WASM
wrapper sources are included in desktop/x64 archives; see the
[client guide](../clients/README.md), [Helm guide](../deploy/helm/vlite/README.md)
and [WASM build instructions](../bindings/wasm/README.md). Generated WASM bundles
and NuGet packages are not prebuilt release assets.

## Registry publication

PyPI/npm trusted publishing is not configured by a repository workflow alone.
It requires control of the destination project/scope and an explicitly
configured trusted publisher at that registry. Native npm packages also need
platform-specific package names and a public loader to avoid publishing
different binaries under one name/version. Until those identities and packaging
contracts are established, install the GitHub assets above.
