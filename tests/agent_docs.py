"""Keep agent installation guides tied to the shipped skills and MCP tools."""

from pathlib import Path
import re

ROOT = Path(__file__).resolve().parents[1]
mcp = (ROOT / "docs" / "MCP.md").read_text(encoding="utf-8")
source = (ROOT / "src" / "bin" / "vlite-mcp.rs").read_text(encoding="utf-8")
tools = set(re.findall(r'\{"name":"(vlite_[a-z_]+)"', source))
documented = set(re.findall(r"^\| `(vlite_[a-z_]+)`", mcp, re.MULTILINE))
assert tools == documented, (tools, documented)
assert len(tools) == 9
kv_source = (ROOT / "src/bin/tool_support/kv_mcp.rs").read_text(encoding="utf-8")
kv_tools = set(re.findall(r'const \w+: &str = "(vlite_kv_[a-z_]+)";', kv_source))
kv_section = mcp.split("## Local key-value tools\n", 1)[1].split("\n## ", 1)[0]
assert kv_tools == {"vlite_kv_get", "vlite_kv_put", "vlite_kv_delete", "vlite_kv_list",
                    "vlite_kv_get_many", "vlite_kv_batch"}
assert kv_tools == set(re.findall(r"^- `(vlite_kv_[a-z_]+)`:", kv_section, re.MULTILINE))
memory_source = (ROOT / "src/bin/tool_support/memory_mcp.rs").read_text(encoding="utf-8")
memory_tools = set(re.findall(r'name: "(vlite_memory_[a-z_]+)"', memory_source))
memory_section = mcp.split("## Server-backed agent memory\n", 1)[1].split("\n## ", 1)[0]
assert len(memory_tools) == 9
assert memory_tools == set(re.findall(r"^- `(vlite_memory_[a-z_]+)`:", memory_section, re.MULTILINE))
for text in ("--server URL", "--writable", "UNKNOWN", "vlite_memory_receipt", "isError:true"):
    assert text in memory_section
tool_docs = (ROOT / "docs/TOOLS.md").read_text(encoding="utf-8")
shell_section = tool_docs.split("## Local key-value shell commands\n", 1)[1].split("\n## ", 1)[0]
for command in ("kv-get", "kv-put", "kv-delete", "kv-list", "kv-get-many", "kv-batch"):
    assert f"`{command}`" in shell_section
assert "cargo install --locked --path . --features mcp --bin vlite-mcp" in mcp
for heading in ("## What you get", "### Option 1: prebuilt binary", "### Choose a database",
                "### Check that the connection works"):
    assert heading in mcp

skills = (ROOT / "docs" / "AGENT-SKILLS.md").read_text(encoding="utf-8")
folders = {path.parent.name for path in (ROOT / "skills").glob("*/SKILL.md")}
listed = set(re.findall(r"^\| \[(vlite-[a-z-]+)\]", skills, re.MULTILINE))
assert folders == listed and len(folders) == 4
for heading in ("## Install for GitHub Copilot", "## Install for Claude Code", "## Use and verify"):
    assert heading in skills
for path in (".copilot\\skills", ".github/skills", ".claude/skills"):
    assert path in skills
assert "Copy-Item -Recurse" in skills and 'cp -R "skills/$name"' in skills
print("Agent guides cover nine core MCP tools, six local KV tools, nine remote memory tools, four skills, installation and verification.")
