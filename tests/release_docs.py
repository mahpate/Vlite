"""Check current release references and keep product copy free of version clutter.

Historical benchmark reports, release notes and compatibility specifications
retain their original provenance and are intentionally outside this check.
"""

from pathlib import Path
from html.parser import HTMLParser
import re
import tomllib


class ProductCopy(HTMLParser):
    def __init__(self, text):
        super().__init__()
        self.hidden_depth = 0
        self.prose = []
        self.feed(text)

    def handle_starttag(self, tag, attrs):
        if tag in ("pre", "code", "details", "script", "style"):
            self.hidden_depth += 1

    def handle_endtag(self, tag):
        if tag in ("pre", "code", "details", "script", "style"):
            self.hidden_depth -= 1

    def handle_data(self, data):
        if not self.hidden_depth:
            self.prose.append(data)


def check(root):
    version = tomllib.loads((root / "Cargo.toml").read_text(encoding="utf-8"))["package"]["version"]
    current = tuple(map(int, version.split(".")))
    paths = [root / "README.md", *sorted((root / "site").glob("*.html"))]
    for directory in ("bindings", "clients"):
        for relative in ("README.md", "python/README.md", "node/README.md"):
            path = root / directory / relative
            if path.is_file():
                paths.append(path)
    errors = []
    for path in paths:
        text = path.read_text(encoding="utf-8")
        if path.suffix == ".html":
            prose = " ".join(" ".join(ProductCopy(text).prose).split())
            if re.search(r"\bv?0\.\d+\.\d+\b", prose):
                errors.append(f"{path.relative_to(root)}: keep product versions in install commands or expandable measurement details")
            for clutter in ("Self-hosted dashboard; this page is not a live console.",
                            "captured October", "Measured October", "Actual current-source UI"):
                if clutter in prose:
                    errors.append(f"{path.relative_to(root)}: unnecessary product copy: {clutter}")
        # Exclude numeric fragments inside IP addresses and larger dotted values.
        for match in re.finditer(r"(?<![\w.])v?(0\.\d+\.\d+)(?![\d.])", text):
            found = tuple(map(int, match.group(1).split(".")))
            if found < current:
                line = text.count("\n", 0, match.start()) + 1
                errors.append(f"{path.relative_to(root)}:{line}: stale version {match.group(0)}; current release {version}")
    if errors:
        raise SystemExit("\n".join(errors))
    print(f"Product copy and current-release references checked in {len(paths)} pages and READMEs ({version}).")


if __name__ == "__main__":
    check(Path(__file__).resolve().parents[1])
