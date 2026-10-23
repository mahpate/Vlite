# VLite agent skills

Reusable [Agent Skills](https://agentskills.io/specification) for retrieval and
safe operation. These are instructions/examples, not an MCP server, embedding
model, database driver, or hosted service.

- [vlite-rag](vlite-rag/SKILL.md): grounded retrieval with embedded Python or the Python REST client.
- [vlite-mcp](vlite-mcp/SKILL.md): bounded, least-privilege retrieval over stdio MCP.
- [vlite-ops](vlite-ops/SKILL.md): locking, backups, verification, recovery and observability.
- [vlite-index-tuning](vlite-index-tuning/SKILL.md): exact/HNSW, filters, quantization and recall measurement.

## Install

Review instructions and scripts first. Copy **each selected skill folder**,
including its `references` and `scripts`, into an agent discovery directory.
Do not copy only `SKILL.md`, rename folders, or copy this parent `skills` folder
as a single skill. Keeping these sources here does not install them automatically.

GitHub Copilot supports project skills in `.github/skills/`, `.claude/skills/`
or `.agents/skills/`, and personal skills in `~/.copilot/skills/` or
`~/.agents/skills/`. Claude Code supports project skills in `.claude/skills/`
and personal skills in `~/.claude/skills/`. Follow your host's documented location;
discovery and invocation can differ by host/version.

From the VLite checkout, install all four for the current Copilot user:

```powershell
$destination = Join-Path $HOME '.copilot\skills'
New-Item -ItemType Directory -Force $destination | Out-Null
$names = 'vlite-rag', 'vlite-mcp', 'vlite-ops', 'vlite-index-tuning'
foreach ($name in $names) {
    $target = Join-Path $destination $name
    if (Test-Path $target) { throw "Already installed: $target; review before replacing" }
    Copy-Item -Recurse (Join-Path 'skills' $name) $destination
}
```

For project scope, set `$destination = '.github\skills'` (Copilot) or
`$destination = '.claude\skills'` (Claude Code). For personal Claude Code use
`Join-Path $HOME '.claude\skills'`. On Unix-like systems, for example:

```sh
mkdir -p ~/.copilot/skills
# Review any existing target before copying.
cp -R skills/vlite-rag ~/.copilot/skills/
```

Start/reload the agent session if needed, then ask "build a RAG pipeline with
VLite", "use VLite MCP", "back up VLite safely", or "tune VLite HNSW recall".
Claude Code also supports `/vlite-rag`, `/vlite-mcp`, `/vlite-ops`, and
`/vlite-index-tuning`. Installation does not execute scripts. No broad tool
pre-approvals are bundled; normal host permissions still apply.

## Run the ingestion example

Python 3.11+ and `vlite-embedded` are required. A platform wheel bundles the
native library (`pip install vlite-embedded`). This uses three-dimensional toy
vectors, not a production text embedder, and refuses to overwrite a file:

```powershell
python skills\vlite-rag\scripts\ingest_example.py --db rag-example.vlite --ann
```

Use local sources without downloading a wheel, from the repository root:

```powershell
$env:Path = "$env:USERPROFILE\.cargo\bin;$env:Path"
cargo build -p vlite-ffi --release
$env:PYTHONPATH = (Resolve-Path bindings\python).Path
python skills\vlite-rag\scripts\ingest_example.py --db rag-example.vlite --library target\release\vlite_ffi.dll --ann
```

Use the corresponding `libvlite_ffi.so` (Linux) or `libvlite_ffi.dylib` (macOS)
on those platforms. `NativeLibrary(path)` explicitly selects the library; no
production library-path environment variable is required. Remove the example
database only after you no longer need it.

## Format and maintenance

Each skill has standard `name`/`description` YAML frontmatter, focused
`SKILL.md`, and on-demand references. The RAG script has no dependencies beyond
the binding. Match APIs to your installed release; capabilities and local
`--help` take precedence, especially for evolving MCP/observability surfaces.
Repository documentation remains under `docs/` in the VLite checkout; it is not
copied into installed skill folders.

Conventions verified against official documentation:

- [Agent Skills specification](https://agentskills.io/specification)
- [GitHub Copilot skills](https://docs.github.com/en/copilot/how-tos/copilot-on-github/customize-copilot/customize-cloud-agent/add-skills)
- [Claude Code skills](https://code.claude.com/docs/en/skills)
