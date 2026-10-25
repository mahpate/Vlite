"""Reject stale release versions in current product pages and package READMEs.

Historical benchmark reports, release notes and compatibility specifications
retain their original provenance and are intentionally outside this check.
"""

from pathlib import Path
import re
import tomllib


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
        # Exclude numeric fragments inside IP addresses and larger dotted values.
        for match in re.finditer(r"(?<![\w.])v?(0\.\d+\.\d+)(?![\d.])", text):
            found = tuple(map(int, match.group(1).split(".")))
            if found < current:
                line = text.count("\n", 0, match.start()) + 1
                errors.append(f"{path.relative_to(root)}:{line}: stale version {match.group(0)}; current release {version}")
    if errors:
        raise SystemExit("\n".join(errors))
    print(f"Current-release references checked in {len(paths)} product pages and READMEs ({version}).")


if __name__ == "__main__":
    check(Path(__file__).resolve().parents[1])
