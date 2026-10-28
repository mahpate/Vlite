# Install VLite

Python packages are published on PyPI: [vlite-embedded](https://pypi.org/project/vlite-embedded/)
for in-process use and [vlite-client](https://pypi.org/project/vlite-client/)
for the REST API. Install them
directly with pip; no repository clone or Rust toolchain is needed. Other
prebuilt release assets are available from
[GitHub releases](https://github.com/mahpate/Vlite/releases). npm packages are
not published.

See [upgrade and rollback guidance](RELEASE-0.6.1.md) before upgrading an
existing deployment. Release binaries, the C library, wheels and Node release
tarballs include the [property graph](GRAPH.md); new files are graph-enabled
and existing files are never converted. The Docker image's API server (`vlite-server`)
has graph; `vlite-graph-cli` is not in the image.

## API and dashboard: first-time setup

Use the [guided Docker setup](DOCKER-OPERATIONS.md#first-deployment) for the
API and dashboard. On Windows, it checks/installs WSL 2, Ubuntu 24.04 and Docker
after confirmation, and explains when to restart and rerun. On Ubuntu
22.04/24.04, the bootstrap installs missing Docker Engine/Compose packages.
Both use published images; no Rust toolchain or local image build is needed.
Existing Docker Desktop WSL integration can be reused.

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
Get-FileHash .\vlite-embedded-0.6.1-win32-x64.tgz -Algorithm SHA256
```

Checksums detect altered or incomplete downloads; they are not a publisher
signature. Obtain both packages and hashes from the intended GitHub release.

## Embedded Python

Requires Python 3.11+. The recommended end-user installation is:

```text
python -m pip install vlite-embedded
```

PyPI provides Windows x64, macOS arm64 and Linux x86_64/glibc 2.28+
wheels. For supported platforms, pip selects the matching wheel and
`NativeLibrary()` loads its bundled native library. If you need a release asset
instead, choose a `vlite_embedded-*.whl` matching your OS and architecture and
install its downloaded filename:

```text
python -m pip install ./vlite_embedded-0.6.1-py3-none-win_amd64.whl
```

That example is for Windows x64. Linux x86_64/glibc 2.28+ should use the
`manylinux_2_28_x86_64` wheel. Do not use a host-native `linux_x86_64` wheel
unless its build baseline matches your runtime. macOS wheels are arm64 only
and include the minimum OS version in their names; Intel macOS requires a
source build. An early `universal2` macOS wheel was withdrawn because it
contained only ARM64 code; do not install cached copies. There is no
universal, musl/Alpine or general ARM support claim.

```python
from vlite_embedded import NativeLibrary

with NativeLibrary().create("example.vlite", dimensions=3) as db:
    db.put("1", [1, 0, 0])
    print(db.search([1, 0, 0], 1))
```

The wheel includes the native library. No Rust compiler or server is required.

For cosine ANN, build an index with a matching metric:

```python
with NativeLibrary().open("example.vlite") as db:
    db.build_index(metric="cosine")
    print(db.search_approximate_ready([1, 0, 0], 5, metric="cosine"))
```

The default is squared L2. Each owner keeps one in-memory graph: building
another metric replaces it, and ready-only queries reject a mismatched metric
without rebuilding. Reopen loses the index. Inner-product search is exact-only.

## Embedded Node.js / TypeScript

Requires Node.js 22.9+. Choose a host-matching tarball:

```text
npm install ./vlite-embedded-0.6.1-win32-x64.tgz
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
python -m pip install vlite-client
```

Node.js 22+:

```text
npm install ./vlite-client-0.6.1.tgz
```

Import `Client` from `vlite_client` in Python or `@vlite/client` in JavaScript.
See [REST client examples](../clients/README.md). Any other HTTP-capable language
can use the [OpenAPI contract](../src/openapi.json).

## Native tools and C ABI

Extract the `vlite-<Rust-target>.zip` matching your platform. It contains the
server, maintenance tools, native library, C header, documentation and MIT
license. The Linux arm64 archive contains command-line binaries, contracts and
documentation, not a C shared library or embedded SDK. Python and Node users
should prefer the self-contained packages above to copying native libraries.

One process owns a database file at a time. Use the service when applications
need shared access. The live dataset must fit in RAM. Back up before upgrading.
VLite does not yet offer a 1.0 stability contract or physical power-loss
certification.

## Additional packages

See [upgrade and rollback guidance](RELEASE-0.6.1.md) before selecting a
new binary or database format. The native archives also ship the offline shell,
import, tuning and quality tools. Go/.NET clients, the Helm chart and WASM
wrapper sources are included in desktop/x64 archives; see the
[client guide](../clients/README.md), [Helm guide](../deploy/helm/vlite/README.md)
and [WASM build instructions](../bindings/wasm/README.md). Generated WASM bundles
and NuGet packages are not prebuilt release assets.

## PyPI publication details for maintainers

### PyPI

[`pypi.yml`](../.github/workflows/pypi.yml) runs after a successful tagged
release (or manually with an existing tag). It downloads that release's wheels,
verifies them against `SHA256SUMS`, skips the bare `linux_x86_64` wheel (PyPI
only accepts `manylinux` Linux tags), and uploads with PyPI trusted publishing
(OIDC; no API token is stored). The distributions are published at
[vlite-embedded](https://pypi.org/project/vlite-embedded/) and
[vlite-client](https://pypi.org/project/vlite-client/). End users should use
the pip commands above.

```sh
python -m pip install vlite-embedded   # in-process engine (Windows x64, macOS arm64, Linux x86_64 glibc 2.28+)
python -m pip install vlite-client     # REST client
```

Platforms without a wheel (for example Linux arm64 or Intel macOS) must use a
release asset or build from source with `bindings/build_wheel.py`.

One-time publisher configuration (only if setting up a new publisher):

1. Sign in to <https://pypi.org> (create an account and enable 2FA if needed).
2. Under **Your projects → Publishing → Add a new pending publisher**, add one
   per project with owner `mahpate`, repository `Vlite` and workflow `pypi.yml`:
   environment `pypi` for `vlite-client` and environment `pypi-embedded` for
   `vlite-embedded`. The environments must differ: PyPI matches an upload token
   to one publisher identity, so two projects sharing one identity cannot both
   be created by the workflow.
3. In GitHub, create the `pypi` and `pypi-embedded` environments
   (Settings → Environments), ideally with a required reviewer.
4. Run **Actions → Publish to PyPI → Run workflow** with an existing release
   tag, or push the next release tag.

### npm

Native npm packages need platform-specific package names and a public loader to
avoid publishing different binaries under one name/version, plus control of the
`@vlite` scope. Until then, install the GitHub tarballs above.
