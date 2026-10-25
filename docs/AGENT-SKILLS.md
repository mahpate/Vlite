# Agent skills

VLite ships four portable [Agent Skills](https://agentskills.io/specification)
with focused instructions, on-demand references and a runnable ingestion example.
They are instructions your coding assistant loads when relevant, not a plugin
that runs a database or installs packages automatically.

## Supported skills

| Skill | What it helps with | Try asking |
| --- | --- | --- |
| [vlite-rag](../skills/vlite-rag/SKILL.md) | Ingest document embeddings, retrieve/filter chunks and cite payloads using embedded Python or the REST client. | "Build a RAG pipeline with VLite." |
| [vlite-mcp](../skills/vlite-mcp/SKILL.md) | Discover MCP tools, retrieve bounded context and keep mutations opt-in. | "Use VLite MCP to inspect my collections without writing." |
| [vlite-ops](../skills/vlite-ops/SKILL.md) | Locking, backups, verification, copy-out maintenance, recovery and monitoring. | "Back up my VLite database safely." |
| [vlite-index-tuning](../skills/vlite-index-tuning/SKILL.md) | Choose exact/HNSW, tune metadata indexes/quantization and measure recall, latency and memory. | "Tune VLite HNSW recall for my workload." |

## Install for GitHub Copilot

Requires Git and a host that supports Agent Skills. Clone the repository
once, then copy the **complete folders**, including references and scripts.
No Rust or Python is needed just to copy/install the skills.

In PowerShell:

```powershell
git clone https://github.com/mahpate/Vlite
cd Vlite
$destination = Join-Path $HOME '.copilot\skills'
$names = 'vlite-rag', 'vlite-mcp', 'vlite-ops', 'vlite-index-tuning'
foreach ($name in $names) {
    if (Test-Path (Join-Path $destination $name)) {
        throw "Skill already exists: $name. Review before replacing."
    }
}
New-Item -ItemType Directory -Force $destination | Out-Null
foreach ($name in $names) {
    Copy-Item -Recurse (Join-Path 'skills' $name) $destination
}
```

On Linux/macOS, from the checkout:

```sh
destination="$HOME/.copilot/skills"
for name in vlite-rag vlite-mcp vlite-ops vlite-index-tuning; do
  if [ -e "$destination/$name" ]; then
    printf 'Skill already exists: %s; review before replacing.\n' "$name" >&2
    exit 1
  fi
done
mkdir -p "$destination"
for name in vlite-rag vlite-mcp vlite-ops vlite-index-tuning; do
  cp -R "skills/$name" "$destination/"
done
```

For one repository instead of personal use, set the destination to
`.github/skills` in that repository. If it is not the VLite checkout, use the
absolute path to the cloned VLite `skills` directory as the copy source.
Review skills before committing them to a shared repository.

## Install for Claude Code

Use the same folders/copy commands, changing the personal destination to
`~/.claude/skills` (PowerShell: `Join-Path $HOME '.claude\skills'`) or
the project destination to `.claude/skills`. Do not copy the parent `skills`
directory as one skill or copy only `SKILL.md`.

## Use and verify

Start/reload your agent session after installation, then use an example prompt
from the table. Claude Code also supports `/vlite-rag`, `/vlite-mcp`,
`/vlite-ops` and `/vlite-index-tuning`. Skill discovery/invocation depends on
your host; if nothing loads, check its supported discovery directory and
confirm each installed folder contains `SKILL.md`.

Installing a skill does not start a server, install a driver/model, grant write
permissions or execute scripts. To use the MCP skill, separately
[install/configure the MCP server](MCP.md#install-and-launch).
The RAG example needs Python 3.11+ and `python -m pip install vlite-embedded`;
production retrieval also needs your own compatible embeddings.
See [runnable example commands](../skills/README.md#run-the-ingestion-example).

The RAG example uses real APIs with illustrative vectors and refuses overwrite.
Production applications must supply compatible document/query embeddings.
For MCP tool details, consult [MCP](MCP.md), installed schemas and binary
`--help`. Operational details are in [offline tools](TOOLS.md),
[storage features](STORAGE-FEATURES.md), [ANN snapshots](ANN-SNAPSHOTS.md)
and [dashboard](CONSOLE.md) documentation.
