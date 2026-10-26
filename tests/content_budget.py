"""Keep the README, website pages and dashboard guides short.

Usage: python tests/content_budget.py

Measures:
- lines: physical lines in the file.
- visible words: words a reader sees when a website page loads. Markup, the
  page head, scripts, styles, inline SVG, templates, elements marked hidden
  and the bodies of collapsed <details> blocks (their summaries still count)
  are excluded. asendme.html is intentionally not budgeted.
- words: whitespace-separated tokens in the Markdown source.

api.html is the complete API reference, so it has no word budget;
tests/api_reference.py checks instead that it documents every public API item
and nothing stale. features.html is the full feature catalogue, so its budget
covers every capability; keep each row to one or two sentences.
architecture.html also carries the on-disk format reference (file header,
frames, opcodes, format versions, graph frames and sidecar files), so its
budget is larger; keep the remaining sections brief.

Collapsed and hidden content is excluded on purpose: architecture.html keeps
the Mermaid sources of its diagrams in collapsed blocks (build_site_diagrams.py
reads them), compare.html keeps archived benchmark tables collapsed, and
index.html shows one install tab at a time. The "All text" column reports every
word except scripts and styles for reference; it is not budgeted, but keep
collapsed content short too.

A word is any token that contains a letter or digit, so separators such as
"·" or "|" are not counted.
"""

from html.parser import HTMLParser
from pathlib import Path
import re
import sys


ROOT = Path(__file__).resolve().parents[1]

BUDGETS = [
    ("README.md", "lines", 250),
    ("site/compare.html", "visible words", 900),
    ("site/architecture.html", "visible words", 2300),
    ("site/features.html", "visible words", 2800),
    ("site/index.html", "visible words", 500),
    ("site/dashboard.html", "visible words", 400),
    ("site/about.html", "visible words", 250),
    ("docs/CONSOLE.md", "words", 1500),
    ("docs/CONSOLE-ADMIN.md", "words", 2500),
]

VOID = {
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link",
    "meta", "param", "source", "track", "wbr",
}
SKIPPED = {"head", "script", "style", "svg", "template", "noscript"}
WORD = re.compile(r"\w")


def count_words(text):
    return sum(1 for token in text.split() if WORD.search(token))


class VisibleText(HTMLParser):
    """Collect text that is rendered when the page first loads."""

    def __init__(self, text):
        super().__init__(convert_charrefs=True)
        # Each entry: tag, whether its children are hidden, inherited state.
        self.stack = []
        self.chunks = []
        self.feed(text)
        self.close()

    def hidden(self):
        return self.stack[-1][1] if self.stack else False

    def handle_starttag(self, tag, attrs):
        if tag in VOID:
            return
        attrs = dict(attrs)
        inherited = self.hidden()
        own = tag in SKIPPED or "hidden" in attrs
        if tag == "summary" and self.stack and self.stack[-1][0] == "details-closed":
            # A collapsed <details> still shows its summary.
            inherited = self.stack[-1][2]
        if tag == "details" and "open" not in attrs:
            self.stack.append(("details-closed", True, inherited or own))
            return
        self.stack.append((tag, inherited or own, inherited))

    def handle_startendtag(self, tag, attrs):
        pass

    def handle_endtag(self, tag):
        name = "details-closed" if tag == "details" else tag
        for index in range(len(self.stack) - 1, -1, -1):
            entry = self.stack[index][0]
            if entry == name or (tag == "details" and entry == "details"):
                del self.stack[index:]
                return

    def handle_data(self, data):
        if not self.hidden():
            self.chunks.append(data)

    @property
    def text(self):
        return " ".join(self.chunks)


class AllText(HTMLParser):
    """Collect every text node except scripts and styles."""

    def __init__(self, text):
        super().__init__(convert_charrefs=True)
        self.skipping = 0
        self.chunks = []
        self.feed(text)
        self.close()

    def handle_starttag(self, tag, attrs):
        if tag in ("script", "style"):
            self.skipping += 1

    def handle_endtag(self, tag):
        if tag in ("script", "style") and self.skipping:
            self.skipping -= 1

    def handle_data(self, data):
        if not self.skipping:
            self.chunks.append(data)


def measure(path, kind):
    text = path.read_text(encoding="utf-8")
    if kind == "lines":
        return len(text.splitlines())
    if kind == "visible words":
        return count_words(VisibleText(text).text)
    return count_words(text)


def all_text_words(path):
    return count_words(" ".join(AllText(path.read_text(encoding="utf-8")).chunks))


def main():
    rows = []
    failures = []
    for relative, kind, budget in BUDGETS:
        path = ROOT / relative
        if not path.is_file():
            rows.append((relative, kind, "missing", budget, "FAIL", ""))
            failures.append(f"{relative}: file is missing")
            continue
        value = measure(path, kind)
        status = "ok" if value <= budget else "FAIL"
        reference = str(all_text_words(path)) if kind == "visible words" else ""
        rows.append((relative, kind, str(value), budget, status, reference))
        if value > budget:
            failures.append(f"{relative}: {value} {kind}; budget is {budget}")
    headers = ("File", "Measure", "Count", "Budget", "Status", "All text")
    widths = [max(len(str(row[i])) for row in [headers, *rows]) for i in range(len(headers))]
    line = lambda row: "  ".join(
        str(cell).rjust(width) if index in (2, 3, 5) else str(cell).ljust(width)
        for index, (cell, width) in enumerate(zip(row, widths))
    ).rstrip()
    print(line(headers))
    print(line(["-" * width for width in widths]))
    for row in rows:
        print(line(row))
    print("\nAll text: every word except scripts and styles, including collapsed and hidden content (not budgeted).")
    if failures:
        print("\nContent budget exceeded:\n" + "\n".join(f"- {failure}" for failure in failures))
        sys.exit(1)
    print(f"\nAll {len(rows)} content budgets met.")


if __name__ == "__main__":
    main()
