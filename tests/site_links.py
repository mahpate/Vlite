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
        for key in ("href", "src"):
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
            for required in ("dashboard.html", "docs/TOOLS.html"):
                if required not in page.primary_links:
                    errors.append(f"{path.name}: missing primary navigation link {required}")
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
