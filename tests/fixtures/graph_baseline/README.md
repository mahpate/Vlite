# Graph-off golden baseline (feature-agent-graph EPIC-001 ITEM-003)

Generated ONLY from baseline commit `347b01dcb437051fd02816443f8f0bbad082b866` by

    python scripts/graph_baseline.py fixtures

which extracts that commit with `git archive` into a temporary directory and
builds everything offline there; the working tree is never used as the
baseline. `python scripts/graph_baseline.py fixtures --check` regenerates and
compares without writing.

| File | Content | Consumer |
|------|---------|----------|
| `<variant>.vlite.hex` | file bytes after `scenario::populate` + close | REG-003 (V3 byte-exact; V4/V5 lineage/CRC and encrypted salt/nonce/ciphertext masked), REG-004 |
| `<variant>.ops.txt` | `{:?}` of every populate result/error | REG-006 |
| `<variant>.report.txt` | reopened state, searches, parse, change feed, replica bootstrap | REG-005, REG-007 |
| `<variant>.state.txt` | state + searches only | REG-004 |
| `<variant>.maintenance.txt` | backup/restore/compact/verify results and each output's header version and state | REG-004 |
| `openapi.json` | served `/openapi.json` bytes (LF) | REG-008 |
| `mcp_requests.jsonl` | the fixed MCP request transcript | REG-008 |
| `mcp_read_only.json`, `mcp_writable.json` | `vlite-mcp` responses to that transcript on a 3-record default-format file | REG-008 |

Variants: `v3`, `v4`, `v5_plain`, `v5_encrypted`, `v5_timestamps` (see
`tests/graph_baseline/scenario.rs`). `.hex` text is whitespace-insensitive;
`MANIFEST.sha256` hashes decoded bytes and LF-normalized text.
