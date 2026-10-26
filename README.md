# VLite

VLite is a single-file vector and graph database. Embed it in your
application with Rust, Python, Node.js or C, or run it as a self-hosted server
with REST, gRPC and a browser dashboard. Vectors, payloads, typed metadata and
property graphs live in one durable file, so there is no separate database
service to operate.

**Website:** [mahpate.github.io/Vlite](https://mahpate.github.io/Vlite/) ·
[Features](https://mahpate.github.io/Vlite/features.html) ·
[Dashboard](https://mahpate.github.io/Vlite/dashboard.html) ·
[Benchmarks](https://mahpate.github.io/Vlite/compare.html) ·
[Docs](https://mahpate.github.io/Vlite/docs/)

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="site/assets/diagrams/layers-dark.svg">
  <img src="site/assets/diagrams/layers-light.svg" alt="VLite architecture: interfaces call the Database core; search indexes are rebuildable; storage writes one authoritative file.">
</picture>

## Highlights

- **One durable file.** Each write is a checksummed commit, synced within 60 s
  by default or before acknowledgment with full durability. Corruption fails
  closed and recovery is explicit.
- **Exact and approximate search.** Exact search with squared L2, cosine or
  inner product. HNSW for L2 and cosine. Typed metadata filters apply first.
- **Property graphs.** Built in; opt-in per file: nodes, typed edges and
  links to vector records, with bounded traversal and graph retrieval.
- **Atomic batches.** Write or delete up to 4,096 records in one commit.
- **Advanced retrieval.** Opt-in collections, record expiry, named and sparse
  vectors, hybrid search, facets, geo filters and recommendations.
- **Server.** Authenticated REST with OpenAPI, optional gRPC and TLS,
  Prometheus metrics, non-root Docker images and a Helm chart.
- **Dashboard.** Browse, search, write, import, index, monitor and query
  graphs, with user accounts, roles and tenants.
- **Storage options.** Encryption at rest, point-in-time restore, a change
  feed, file-shipped read replicas and a disk-based ANN index.
- **Tools.** Interactive shell, validated imports, offline maintenance, index
  tuning, a graph CLI, an MCP server and agent skills.

## Key terms

| Term | What it means in VLite |
|---|---|
| Database | One `.vlite` file with your records, collections and graphs. Embed it, or serve it with `vlite-server`, one file per server. |
| Collection | A named set of records, like a table, with its own vector size and settings. Every database has `default`. Named collections need format v4 or newer; create server files with `--create-format 4` or `5`. |
| Graph | A named property graph in the same file: nodes, typed edges and links to records. Available in graph-enabled files. |
| Tenant | A private space for one team or customer in the same file. Its users and API keys see only its collections and graphs. |
| User | A person who signs in to the dashboard as super admin, editor or viewer. Apps use API keys. |

Tenants and users exist only on a server; their settings live in its admin
directory.

## Quick start

### Docker with the dashboard

Requires Docker Engine with Compose v2 and Python 3. From a checkout:

```sh
python3 -c "import pathlib,secrets; p=pathlib.Path('.secrets'); p.mkdir(mode=0o700,exist_ok=True); (p/'api-token').write_text(secrets.token_urlsafe(32),encoding='ascii')"
docker compose --profile api up --build -d api
```

Open **http://127.0.0.1:18084/dashboard**. The first time, the sign-in screen
asks for an access token: paste the token from `.secrets/api-token`, then
create your super admin account in **Administration → Users & access**. There
is no default password. Data persists in a Docker volume.

To serve the dashboard from its own container on port 8081, run
`docker compose --profile dashboard up --build -d dashboard`. Stop everything
with `docker compose --profile dashboard stop dashboard api`.

On Windows, `scripts/devbox.ps1` sets up Docker in WSL and serves the
dashboard at http://127.0.0.1:8082. On Ubuntu, `sudo sh scripts/docker-dashboard.sh`
installs Docker and starts the published images. See
[Docker operations](docs/DOCKER-OPERATIONS.md#first-deployment).

Azure Container Apps: [experimental deployment](deploy/azure-container-apps/README.md)
with a separate Azure Files share in ASENDME's existing storage account.

### Embedded Rust

Requires Rust 1.89 or later. Run the example from a checkout:

```sh
cargo run --locked --example basic -- demo.vlite
```

Or add VLite to your project:

```toml
[dependencies]
vlite = { path = "../VLite" }
```

```rust
use vlite::Database;

fn main() -> vlite::Result<()> {
    let mut db = Database::create("vectors.vlite", 3)?;
    db.put(10, &[1.0, 0.0, 0.0], b"document A")?;
    db.put(20, &[0.0, 1.0, 0.0], b"document B")?;

    let hits = db.search(&[0.9, 0.1, 0.0], 5)?;
    assert_eq!(hits[0].id, 10);
    db.close()
}
```

`create` never overwrites a file. Use `Database::open` to reopen one.

### Standalone server

```sh
cargo run --release --locked --features server --bin vlite-server -- vectors.vlite --create-if-missing 3 --insecure-local
```

This starts an open server on http://127.0.0.1:8080, with the dashboard at
`/dashboard`. Open local mode accepts loopback connections only and has no
sign-in. For shared use, run in protected mode with an admin token and an admin
directory. See [server setup](docs/REFERENCE.md#rest-service-optional) and
[security](docs/SECURITY.md).

## Sign-in and tenants

People sign in to the dashboard with a username and password. Apps and scripts
use API keys, and the admin token is kept for automation and recovery.

| Role | Can |
|---|---|
| Super admin | Manage the server, tenants, users and API keys |
| Editor | Read, search, write, import, index and manage collections |
| Viewer | Read and search |

Editors and viewers see either one tenant's collections or all collections.
Super admins can create, rename, limit, suspend, resume and delete tenants.
Sign-in requires protected mode. See the [dashboard guide](docs/CONSOLE.md) and
[accounts and sessions](docs/SECURITY.md#dashboard-sign-in-and-accounts).

## Clients and integrations

- **Python, embedded:** `python -m pip install vlite-embedded` (Python 3.11+). [Guide](bindings/python/README.md)
- **Node.js, embedded:** `@vlite/embedded` native package (Node.js 22.9+). [Guide](bindings/node/README.md)
- **C, C++ and other FFI:** versioned C ABI and shared library. [Guide](bindings/README.md)
- **Python over REST:** `python -m pip install vlite-client`. [Guide](clients/README.md#python)
- **TypeScript and JavaScript over REST:** typed ESM client. [Guide](clients/README.md#typescript--javascript)
- **Go and .NET over REST:** [Go](clients/README.md#go) · [.NET](clients/README.md#net)
- **LangChain and LlamaIndex:** [adapters](clients/README.md#langchain-and-llamaindex-integrations)
- **Any language:** [REST API](docs/API.md), [OpenAPI](src/openapi.json) or [gRPC](docs/GRPC.md)
- **AI agents:** [MCP server](docs/MCP.md) and [agent skills](docs/AGENT-SKILLS.md)
- **Command line:** [shell, import, maintenance, tuning and graph tools](docs/TOOLS.md)
- **Kubernetes:** [Helm chart](deploy/helm/vlite/README.md)

Prebuilt packages: [PyPI](docs/INSTALL.md#pypi) and [release assets](docs/INSTALL.md).

## Documentation

| Topic | Guide |
|---|---|
| Install packages and release assets | [INSTALL](docs/INSTALL.md) |
| Docker deployment, upgrades and rollback | [DOCKER-OPERATIONS](docs/DOCKER-OPERATIONS.md) |
| Dashboard tasks | [CONSOLE](docs/CONSOLE.md) |
| Server administration | [CONSOLE-ADMIN](docs/CONSOLE-ADMIN.md) |
| Sign-in, roles, tenants and TLS | [SECURITY](docs/SECURITY.md) |
| REST endpoints | [API](docs/API.md) |
| Contracts, limits and durability | [REFERENCE](docs/REFERENCE.md) |
| Collections, hybrid search and imports | [CAPABILITIES](docs/CAPABILITIES.md) |
| Named and sparse vectors | [VECTORS](docs/VECTORS.md) |
| Encryption, restore, change feed and replicas | [STORAGE-FEATURES](docs/STORAGE-FEATURES.md) |
| Command-line tools and tests | [TOOLS](docs/TOOLS.md) |
| gRPC, MCP and agent skills | [GRPC](docs/GRPC.md), [MCP](docs/MCP.md), [AGENT-SKILLS](docs/AGENT-SKILLS.md) |
| Webhooks | [WEBHOOKS](docs/WEBHOOKS.md) |
| Property graphs (opt-in per file) | [GRAPH](docs/GRAPH.md) |
| Benchmarks | [Methodology](benchmarks/README.md), [results](benchmarks/RESULTS.md) |
| Upgrade notes | [Release notes](docs/RELEASE-0.5.1.md) |

## Status and limitations

VLite is pre-1.0, so file formats and APIs may still change. Packages and images
are on the [releases page](https://github.com/mahpate/Vlite/releases).

- The live dataset is held in memory and the file is replayed on open.
- One process writes a file at a time. Read-only handles can share it.
- HNSW is approximate and supports L2 and cosine only.
- Replicas are file-shipped. There is no networked replication.
- Deleted records use disk space until you compact to a new file.
- Durability depends on the OS and storage honoring sync. It is not a
  power-loss guarantee. The default [periodic durability](docs/REFERENCE.md#periodic-lazy-durability)
  syncs every 60 s, so an OS crash or power loss can lose up to about 60 s of
  acknowledged writes (a process crash loses nothing). `--durability full`
  syncs every commit; `--durability none` keeps data in memory only and loses
  it on restart.

## License

VLite is licensed under the [MIT License](LICENSE).
