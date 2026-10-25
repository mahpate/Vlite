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
print("Agent guides cover the shipped nine MCP tools, four skills, installation and verification.")
