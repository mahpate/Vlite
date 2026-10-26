"""Check local page, asset and fragment links in an assembled website."""

from html.parser import HTMLParser
from pathlib import Path
from urllib.parse import unquote, urlsplit
import sys


class Page(HTMLParser):
    def __init__(self, text):
        super().__init__()
        self.ids = set()
        self.links = []
        self.primary_links = []
        self.in_primary = False
        self.text = []
        self.sections = []
        self.headings = []
        self.feed(text)

    def handle_data(self, data):
        self.text.append(data)

    def handle_starttag(self, tag, attrs):
        attrs = dict(attrs)
        if tag == "nav" and attrs.get("aria-label") == "Primary":
            self.in_primary = True
        if tag == "a" and self.in_primary and attrs.get("href"):
            self.primary_links.append(attrs["href"])
        if "id" in attrs:
            self.ids.add(attrs["id"])
        if tag == "section" and attrs.get("id"):
            self.sections.append(attrs["id"])
        if tag == "h2" and attrs.get("id"):
            self.headings.append(attrs["id"])
        for key in ("href", "src", "data-tour", "data-poster"):
            if attrs.get(key):
                self.links.append(attrs[key])

    def handle_endtag(self, tag):
        if tag == "nav":
            self.in_primary = False


def check(root):
    root = root.resolve()
    pages = {
        path.resolve(): Page(path.read_text(encoding="utf-8"))
        for path in root.rglob("*.html")
    }
    errors = []
    for path, page in pages.items():
        if path == root / "docs" / "TOOLS.html":
            expected = [
                "choose-a-tool",
                "build-and-install",
                "interactive-or-scripted-shell",
                "validated-atomic-or-resumable-imports",
                "search-quality-and-configurationdatabase-comparison",
                "index-tuning-advisor",
                "mcp-server-and-agent-skills",
                "graph-operations",
                "tests",
            ]
            if page.headings != expected:
                errors.append("docs/TOOLS.html: expected chooser, setup, shell, import, quality, tuning, MCP, graph, then tests")
        for link in page.primary_links:
            if urlsplit(link).fragment in ("features", "products", "quickstart"):
                errors.append(f"{path.relative_to(root)}: redundant homepage link in primary navigation: {link}")
        if path == root / "dashboard.html":
            if page.sections != ["screenshots", "start", "workflows"]:
                errors.append("dashboard.html: expected tours, setup, then workflows")
            for name in ("current-overview-light.png",):
                if f"assets/dashboard/{name}" not in [urlsplit(link).path for link in page.links]:
                    errors.append(f"dashboard.html: missing latest screenshot {name}")
            if "Earlier release tours" in " ".join(page.text):
                errors.append("dashboard.html: remove historical dashboard tours")
        if path == root / "features.html":
            expected = [
                "storage", "search", "indexes", "vectors", "graph", "interfaces",
                "security", "operations", "dashboard", "tools", "requirements",
            ]
            if page.sections != expected:
                errors.append("features.html: missing or reordered feature categories")
            for category in expected:
                if f"#{category}" not in page.links:
                    errors.append(f"features.html: missing category navigation {category}")
            for guide in (
                "CAPABILITIES", "VECTORS", "STORAGE-FEATURES", "SECURITY",
                "TOOLS", "CONSOLE", "CONSOLE-ADMIN", "GRPC", "INSTALL",
            ):
                if f"docs/{guide}.html" not in page.links:
                    errors.append(f"features.html: missing feature reference {guide}")
            if "docs/GRAPH.html" not in page.links:
                errors.append("features.html: missing property graph guide link")
        if path == root / "index.html":
            for guide in ("docs/MCP.html", "docs/AGENT-SKILLS.html"):
                if guide not in page.links:
                    errors.append(f"index.html: agent links must open their own guide: {guide}")
            for mode in ("embedded", "self-hosted"):
                if f"assets/diagrams/deployment-{mode}-light.svg" not in page.links:
                    errors.append(f"index.html: missing {mode} deployment diagram")
            if "deployment-options" not in page.ids:
                errors.append("index.html: missing deployment-options anchor")
            if "assets/dashboard/current-overview-light.png" not in [urlsplit(link).path for link in page.links]:
                errors.append("index.html: dashboard preview must show the current interface")
            dashboard = pages.get(root / "dashboard.html")
            if dashboard:
                normalize = lambda link: link.removeprefix("./")
                if [normalize(link) for link in page.primary_links] != [
                    normalize(link) for link in dashboard.primary_links
                ]:
                    errors.append("index.html: primary links must match the dashboard navigation")
            for required in ("quickstart", "dashboard-preview", "products", "features", "data-safety"):
                if required not in page.sections:
                    errors.append(f"index.html: missing landing section {required}")
            if all(name in page.sections for name in ("quickstart", "features")):
                if page.sections.index("quickstart") > page.sections.index("features"):
                    errors.append("index.html: quickstart must precede the feature inventory")
            for anchor in ("products", "features", "capabilities", "quickstart"):
                if anchor not in page.ids:
                    errors.append(f"index.html: missing legacy anchor {anchor}")
        if path.parent == root:
            visible = " ".join(" ".join(page.text).lower().split())
            for stale in (
                "unreleased source",
                "next source-only",
                "current-source optimization work",
                "these source changes are not included in previously published packages",
            ):
                if stale in visible:
                    errors.append(f"{path.name}: stale released-feature label {stale!r}")
            for required in ("features.html", "dashboard.html", "docs/TOOLS.html"):
                if required not in page.primary_links:
                    errors.append(f"{path.name}: missing primary navigation link {required}")
            features = pages.get(root / "features.html")
            if features and page.primary_links != features.primary_links:
                errors.append(f"{path.name}: primary links must match Features navigation")
        for link in page.links:
            url = urlsplit(link)
            if url.scheme or url.netloc:
                continue
            target = (path.parent / unquote(url.path)).resolve() if url.path else path
            if url.path.startswith("/"):
                errors.append(f"{path.relative_to(root)}: root-relative URL {link}")
                continue
            if target.is_dir():
                target /= "index.html"
            if not target.is_file():
                errors.append(f"{path.relative_to(root)}: missing {link}")
            elif url.fragment and target in pages and unquote(url.fragment) not in pages[target].ids:
                errors.append(f"{path.relative_to(root)}: missing fragment {link}")
    if errors:
        raise SystemExit("\n".join(errors))
    print(f"Checked local links and fragments in {len(pages)} HTML pages.")


if __name__ == "__main__":
    check(Path(sys.argv[1]))
