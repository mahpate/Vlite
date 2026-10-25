"""Render architecture sources as local, theme-aware SVGs shared with README.

Requires Playwright and Chromium (see tests/requirements-dashboard.txt).
The pinned Mermaid renderer is fetched only at build time; published pages
and README images require neither JavaScript nor a third-party connection.
Usage: python scripts/build_site_diagrams.py
"""

import json
from html import unescape
from pathlib import Path
import re

from playwright.sync_api import sync_playwright


ROOT = Path(__file__).resolve().parents[1]
ASSETS = ROOT / "site" / "assets" / "diagrams"
RENDERER = "https://cdn.jsdelivr.net/npm/mermaid@11.12.0/dist/mermaid.esm.min.mjs"
# Explicit roles avoid assigning misleading meaning from a node's position.
DIAGRAMS = [
    ("layers", "VLite layers: interfaces call the Database core; search indexes are rebuildable; storage writes the authoritative file.",
     {"interface": "SDK", "storage": "DB STORE FILE", "search": "IDX"}),
    ("single-file", "The authoritative database rebuilds in-memory indexes and optional ANN sidecars; a matching snapshot can reload the cache.",
     {"storage": "F", "search": "A D M"}),
    ("formats", "Explicit file-format progression from base logs to collections, then expiry and extended vectors.",
     {"storage": "V3 V4 V5"}),
    ("commit", "Commit sequence: validate, encode, append, synchronize, publish indexes and acknowledge; failed writes roll back or return an error.", {}),
    ("recovery", "Recovery validates locking, headers and committed frames; corruption and incomplete tails return distinct errors; tail repair is explicit.",
     {"storage": "O AP TR RDY", "operations": "L H N", "search": "R", "error": "EL EC ET"}),
    ("concurrency", "A server owns an exclusive file lock; shared reader processes are an alternative mode, not concurrent with that writer.",
     {"interface": "H1 H2 G1 R1 R2", "operations": "AUTH MX LK SH", "storage": "CORE FILE RF WRITE", "search": "READ BG"}),
    ("records", "Records hold vectors, payload, metadata and expiry; vectors and metadata feed rebuildable indexes.",
     {"storage": "ID V P MD EX", "search": "MP TP HG QC"}),
    ("planner", "Query planning chooses matching ready HNSW or exact scans, then returns deterministic top-k results.",
     {"interface": "Q", "operations": "M I F", "search": "EX ANN CS RR TOP"}),
    ("quantization", "ScalarU8 accelerates candidate traversal; final ranking uses original full-precision vectors.",
     {"storage": "R", "interface": "Q", "search": "CB T C X K"}),
    ("hybrid", "Dense and BM25 rankings merge through weighted reciprocal-rank fusion into top-k results.",
     {"interface": "Q", "search": "D T F K"}),
    ("encryption", "Argon2id derives a key for encrypted frame bodies; plaintext ANN sidecars are refused; logical tenants share one file.",
     {"interface": "PW T1 T2", "storage": "K S F N1 N2", "error": "DA"}),
    ("changes", "Committed frames feed change capture, replication, webhooks, backups and point-in-time restore.",
     {"storage": "F SEG RF BK PITR", "operations": "CF WH"}),
    ("server", "REST and gRPC share authentication, authorization and a Database lock; operations surfaces expose logs and metrics.",
     {"interface": "CL R G UI", "operations": "L A RL AU OB LOG LG SUM", "storage": "DB"}),
    ("bindings", "Python and Node send handle-based requests through the C ABI to Database and release the handle on close.", {}),
]


def decorated(source, roles, palette, dark):
    if source.startswith("sequenceDiagram"):
        # Mermaid boxes group participants by the same roles as flowchart nodes.
        lines = source.splitlines()
        result = []
        for line in lines:
            if line.strip().startswith("participant "):
                role = "interface" if re.search(r"participant (App|L|C) as", line) else "storage"
                if "participant Idx " in line:
                    role = "search"
                color = palette[role]["dark" if dark else "light"]
                result.extend([f"  box {color} {palette[role]['label']}", line, "  end"])
            else:
                result.append(line)
        return "\n".join(result)
    for role, nodes in roles.items():
        values = palette[role]
        source += (
            f"\nclassDef {role} fill:{values['dark' if dark else 'light']},"
            f"stroke:{values['color']},stroke-width:2px,color:{'#e6e9f2' if dark else '#0f172a'};"
            f"\nclass {','.join(nodes.split())} {role};"
        )
    return source


def main():
    path = ROOT / "site" / "architecture.html"
    html = path.read_text(encoding="utf-8")
    sources = re.findall(r'<pre class="mermaid">\s*(.*?)\s*</pre>', html, re.S)
    if len(sources) != len(DIAGRAMS):
        raise ValueError(f"Expected {len(DIAGRAMS)} architecture sources; found {len(sources)}")
    palette = json.loads((ASSETS / "palette.json").read_text(encoding="utf-8"))
    with sync_playwright() as p:
        browser = p.chromium.launch()
        page = browser.new_page()
        page.goto("https://cdn.jsdelivr.net")
        page.evaluate("""async url => { window.diagramRenderer = (await import(url)).default; }""", RENDERER)
        for dark in (False, True):
            text = "#e6e9f2" if dark else "#0f172a"
            background = "#121729" if dark else "#ffffff"
            for (name, description, roles), source in zip(DIAGRAMS, sources):
                config = {
                    "startOnLoad": False, "securityLevel": "strict", "theme": "base", "htmlLabels": False,
                    "deterministicIds": True, "deterministicIDSeed": name,
                    "fontFamily": "ui-sans-serif, system-ui, sans-serif", "fontSize": 16,
                    "themeCSS": ".node rect, .cluster rect { rx: 12px; ry: 12px; } .nodeLabel { font-weight: 500; }",
                    "flowchart": {"htmlLabels": False, "curve": "basis", "nodeSpacing": 28, "rankSpacing": 45},
                    "sequence": {"useMaxWidth": True, "wrap": True},
                    "themeVariables": {
                        "primaryColor": palette["interface"]["dark" if dark else "light"],
                        "primaryTextColor": text, "primaryBorderColor": palette["interface"]["color"],
                        "lineColor": "#98a2b8" if dark else "#64748b", "textColor": text,
                        "clusterBkg": "#0f1322" if dark else "#f6f7fb",
                        "clusterBorder": "#475569" if dark else "#cbd5e1",
                        "edgeLabelBackground": background, "actorBkg": background,
                        "actorTextColor": text, "actorBorder": palette["interface"]["color"],
                        "actorLineColor": "#64748b", "signalColor": text, "signalTextColor": text,
                        "labelTextColor": text, "labelBoxBkgColor": background,
                        "labelBoxBorderColor": palette["operations"]["color"],
                        "loopTextColor": text, "noteTextColor": text,
                        "noteBkgColor": palette["operations"]["dark" if dark else "light"],
                    },
                }
                svg = page.evaluate("""async ({config, source, id}) => {
                    window.diagramRenderer.initialize(config);
                    return (await window.diagramRenderer.render(id, source)).svg;
                }""", {"config": config, "source": decorated(unescape(source), roles, palette, dark), "id": f"vlite-{name}"})
                # Standalone accessible images; no foreignObject, scripts or fonts.
                if "<foreignObject" in svg or "<script" in svg:
                    raise ValueError(f"{name}: renderer produced non-portable SVG")
                svg = svg.replace("</svg>", "</svg>\n")
                (ASSETS / f"{name}-{'dark' if dark else 'light'}.svg").write_text(svg, encoding="utf-8")
        browser.close()
    position = 0

    def figure(match):
        nonlocal position
        name, description, _ = DIAGRAMS[position]
        source = sources[position]
        position += 1
        caption = re.search(r"<figcaption>.*?</figcaption>", match.group(0), re.S)
        return (
            f'<figure class="diagram" data-diagram="{name}">\n'
            f'      <picture><source srcset="assets/diagrams/{name}-dark.svg" media="(prefers-color-scheme: dark)">'
            f'<img src="assets/diagrams/{name}-light.svg" data-light="assets/diagrams/{name}-light.svg" '
            f'data-dark="assets/diagrams/{name}-dark.svg" alt="{description}" loading="lazy"></picture>\n'
            f'      <a class="diagram-full" href="assets/diagrams/{name}-light.svg" '
            f'data-light="assets/diagrams/{name}-light.svg" data-dark="assets/diagrams/{name}-dark.svg">Open full-size diagram</a>\n'
            f'      <details class="diagram-source"><summary>Text diagram</summary>\n'
            f'<pre class="mermaid">\n{source}\n</pre></details>\n'
            + (f'      {caption.group(0)}\n' if caption else "")
            + '    </figure>'
        )

    html = re.sub(r'<figure class="diagram"[^>]*>.*?</figure>', figure, html, flags=re.S)
    path.write_text(html, encoding="utf-8")
    css = ":root {\n" + "".join(f"  --diagram-{role}: {value['color']};\n" for role, value in palette.items()) + "}\n"
    (ASSETS / "palette.css").write_text(css, encoding="utf-8")
    print(f"Rendered {len(DIAGRAMS)} diagrams in two themes; refreshed architecture images and shared palette.")


if __name__ == "__main__":
    main()
