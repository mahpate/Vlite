# Agent skills

VLite ships four portable [Agent Skills](https://agentskills.io/specification)
with focused instructions, on-demand references and a runnable ingestion example:

- [vlite-rag](../skills/vlite-rag/SKILL.md): grounded retrieval with embedded Python
  or the Python REST client, typed metadata, filters and cited payloads.
- [vlite-mcp](../skills/vlite-mcp/SKILL.md): use stdio `vlite-mcp`, discover
  capabilities first, retrieve bounded context and keep mutations opt-in.
- [vlite-ops](../skills/vlite-ops/SKILL.md): exclusive writers/shared readers,
  backups, verification, copy-out maintenance, recovery and monitoring.
- [vlite-index-tuning](../skills/vlite-index-tuning/SKILL.md): exact/HNSW selection,
  metadata indexes/quantization and recall, latency and memory measurement.

See [installation and example commands](../skills/README.md). Copy each complete
skill folder to `.github/skills/` for repository Copilot skills or
`~/.copilot/skills/` for personal use; Claude Code uses `.claude/skills/` or
`~/.claude/skills/`. Installing skills does not start a server, grant write
permissions or run scripts.

The RAG example uses real APIs with illustrative vectors and refuses overwrite.
Production applications must supply compatible document/query embeddings.
The MCP surface is evolving: consult [MCP](MCP.md), installed schemas and binary
`--help`. Operational details remain in [offline tools](TOOLS.md),
[storage features](STORAGE-FEATURES.md), [ANN snapshots](ANN-SNAPSHOTS.md)
and [dashboard](CONSOLE.md) documentation.
