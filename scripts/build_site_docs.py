"""Render docs/*.md into themed HTML pages for the GitHub Pages site.

Usage: python scripts/build_site_docs.py <site-dir> <output-dir>
Reuses <site-dir>/about.html's header and footer so the docs share the site's
theme, navigation and dark mode. Requires the `markdown` package.
"""
import html
import pathlib
import re
import sys

import markdown

ROOT = pathlib.Path(__file__).resolve().parents[1]
DOCS = ROOT / "docs"
REPO = "https://github.com/mahpate/Vlite"

GROUPS = [
    ("Getting started", ["INSTALL", "REFERENCE"]),
    ("Interfaces", ["API", "GRPC", "CAPABILITIES", "VECTORS"]),
    ("Storage & indexes", ["STORAGE-FEATURES", "ANN-SNAPSHOTS"]),
    ("Operations", ["DOCKER-OPERATIONS", "SECURITY", "WEBHOOKS", "TOOLS"]),
    ("Console", ["CONSOLE", "CONSOLE-ADMIN"]),
    ("Releases", ["RELEASE-0.3.0", "RELEASE-0.2.0"]),
]


def slugify(value, separator="-"):
    value = re.sub(r"<[^>]+>", "", value)
    value = re.sub(r"[^\w\s-]", "", value).strip().lower()
    return re.sub(r"[\s_-]+", separator, value)


def title_and_summary(text):
    title = re.search(r"^#\s+(.+)$", text, re.M).group(1).strip()
    body = text.split("\n", 1)[1].strip().split("\n\n", 1)[0]
    body = re.sub(r"\[([^\]]+)\]\([^)]+\)", r"\1", body)
    body = re.sub(r"[`*_]", "", " ".join(body.split()))
    return title, (body[:180].rsplit(" ", 1)[0] + "…") if len(body) > 180 else body


def rewrite_link(match, known):
    target = match.group(2)
    if re.match(r"^(https?:|mailto:|#)", target):
        return match.group(0)
    path, _, anchor = target.partition("#")
    anchor = "#" + anchor if anchor else ""
    stem = pathlib.PurePosixPath(path).stem
    if path.endswith(".md") and "/" not in path and stem in known:
        return f'{match.group(1)}"{stem}.html{anchor}"'
    resolved = (DOCS / path).resolve()
    try:
        relative = resolved.relative_to(ROOT).as_posix()
    except ValueError:
        return match.group(0)
    kind = "tree" if resolved.is_dir() else "blob"
    return f'{match.group(1)}"{REPO}/{kind}/main/{relative}{anchor}"'


def chrome(site):
    about = (site / "about.html").read_text(encoding="utf-8")
    header = re.search(r'<header class="nav">.*?</header>', about, re.S).group(0)
    footer = re.search(r"<footer>.*?</footer>", about, re.S).group(0)

    def relocate(block):
        def fix(m):
            attr, value = m.group(1), m.group(2)
            if re.match(r"^(https?:|mailto:|#|/)", value):
                return m.group(0)
            value = "../" if value in ("./", "") else "../" + value.removeprefix("./")
            return f'{attr}="{value}"'
        return re.sub(r'(href|src)="([^"]*)"', fix, block)

    header, footer = relocate(header), relocate(footer)
    header = header.replace(' aria-current="page"', "")
    header = re.sub(r'<a href="[^"]*docs/REFERENCE\.md">Docs</a>|<a href="\.\./docs/">Docs</a>',
                    '<a href="./" aria-current="page">Docs</a>', header)
    return header, footer


def page(title, description, header, footer, body):
    return f"""<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>{html.escape(title)} | VLite docs</title>
  <meta name="description" content="{html.escape(description)}">
  <link rel="icon" href="../assets/logo.svg" type="image/svg+xml">
  <link rel="stylesheet" href="../assets/style.css">
  <script src="../assets/site.js"></script>
</head>
<body>
{header}
{body}
{footer}
</body>
</html>
"""


def sidebar(meta, current):
    parts = ['<aside class="docs-sidebar" aria-label="Documentation"><details class="docs-nav" open>'
             '<summary>Browse docs</summary>',
             '<div class="docs-group"><h4>Overview</h4>'
             f'<a href="./"{" aria-current=\"page\"" if current == "index" else ""}>All docs</a>'
             '<a href="../architecture.html">Architecture</a>'
             '<a href="../api.html">API reference</a></div>']
    for group, names in GROUPS:
        parts.append(f'<div class="docs-group"><h4>{html.escape(group)}</h4>')
        for name in names:
            if name in meta:
                cur = ' aria-current="page"' if name == current else ""
                parts.append(f'<a href="{name}.html"{cur}>{html.escape(meta[name][0])}</a>')
        parts.append("</div>")
    parts.append("</details></aside>"
                 "<script>if (matchMedia('(max-width: 800px)').matches)"
                 " document.querySelector('.docs-nav').open = false;</script>")
    return "\n".join(parts)


def toc_html(tokens):
    items = []
    for token in tokens:
        for child in token.get("children", []):
            items.append(f'<a href="#{child["id"]}">{child["name"]}</a>')
            for grandchild in child.get("children", []):
                items.append(f'<a class="sub" href="#{grandchild["id"]}">{grandchild["name"]}</a>')
    if not items:
        return ""
    return '<nav class="docs-toc" aria-label="On this page"><h4>On this page</h4>' + "".join(items) + "</nav>"


def main():
    site, output = pathlib.Path(sys.argv[1]), pathlib.Path(sys.argv[2])
    output.mkdir(parents=True, exist_ok=True)
    header, footer = chrome(site)
    sources = {path.stem: path.read_text(encoding="utf-8") for path in sorted(DOCS.glob("*.md"))}
    meta = {name: title_and_summary(text) for name, text in sources.items()}
    grouped = {name for _, names in GROUPS for name in names}
    extra = sorted(set(meta) - grouped)
    if extra:
        GROUPS.append(("More", extra))

    for name, text in sources.items():
        md = markdown.Markdown(extensions=["fenced_code", "tables", "sane_lists", "toc"],
                               extension_configs={"toc": {"slugify": slugify, "toc_depth": "2-3"}})
        body = md.convert(text)
        body = re.sub(r'(href=)"([^"]+)"', lambda m: rewrite_link(m, meta), body)
        body = body.replace("<pre><code", '<pre class="code"><code')
        body = re.sub(r"<table>", '<div class="table-scroll"><table>', body).replace("</table>", "</table></div>")
        title, summary = meta[name]
        main_html = (f'<div class="docs-layout">\n{sidebar(meta, name)}\n'
                     f'<main class="docs-main prose">\n<p class="kicker">Documentation</p>\n{body}\n'
                     f'<p class="doc-edit"><a href="{REPO}/blob/main/docs/{name}.md">View or edit this page on GitHub ↗</a></p>\n'
                     f'</main>\n{toc_html(md.toc_tokens)}\n</div>')
        (output / f"{name}.html").write_text(page(title, summary, header, footer, main_html), encoding="utf-8")

    cards = []
    for group, names in GROUPS:
        cards.append(f'<h2 id="{slugify(group)}">{html.escape(group)}</h2><div class="grid cols-2 docs-cards">')
        for name in names:
            if name in meta:
                title, summary = meta[name]
                cards.append(f'<a class="card" href="{name}.html"><span class="label">{name}.md</span>'
                             f'<h3>{html.escape(title)}</h3><p>{html.escape(summary)}</p></a>')
        cards.append("</div>")
    index = (f'<div class="docs-layout">\n{sidebar(meta, "index")}\n<main class="docs-main prose">\n'
             '<p class="kicker">Documentation</p><h1>VLite documentation</h1>\n'
             '<p class="lead">Guides and references for the embedded engine, the REST and gRPC server, storage features and operations. '
             'Start with <a href="INSTALL.html">Install</a>, read the <a href="../architecture.html">architecture deep dive</a>, '
             'or jump to the <a href="../api.html">API reference</a>.</p>\n'
             + "\n".join(cards) + "\n</main>\n</div>")
    (output / "index.html").write_text(page("Documentation", "VLite documentation", header, footer, index), encoding="utf-8")
    print(f"rendered {len(sources)} docs into {output}")


if __name__ == "__main__":
    main()
