"""Generate deployment diagrams using the website's shared semantic palette."""

from html import escape
import json
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
ASSETS = ROOT / "site" / "assets" / "diagrams"


def render(mode, theme, palette):
    dark = theme == "dark"
    text = "#e6e9f2" if dark else "#0f172a"
    muted = "#b4bfd2" if dark else "#475569"
    background = "#121729" if dark else "#ffffff"
    parts = [
        '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 520 690" role="img" aria-labelledby="title desc">',
        f'<title id="title">{"Embedded" if mode == "embedded" else "Self-hosted"} VLite deployment</title>',
        '<desc id="desc">' + (
            'Your application calls the VLite library in the same process. The library reads and writes '
            'a local database file. No API service or dashboard is required.'
            if mode == "embedded" else
            'Your clients and optional dashboard call an authenticated API service on your machine or server. '
            'The service contains the VLite engine and owns the database file on persistent storage. '
            'The dashboard can be served by the API or a separate frontend; it does not store the database.'
        ) + '</desc>',
        f'<rect width="520" height="690" rx="16" fill="{background}"/>',
        f'<style>text{{font-family:ui-sans-serif,system-ui,sans-serif;fill:{text};text-anchor:middle}}'
        f'.label{{font-size:17px;font-weight:650}}.note{{font-size:14px;fill:{muted}}}</style>',
        f'<defs><marker id="arrow" viewBox="0 0 10 10" refX="9" refY="5" markerWidth="7" markerHeight="7" orient="auto-start-reverse">'
        f'<path d="M0 0 10 5 0 10z" fill="{muted}"/></marker></defs>',
    ]

    def label(x, y, value, kind="label"):
        parts.append(f'<text x="{x}" y="{y}" class="{kind}">{escape(value)}</text>')

    def box(x, y, width, height, role, title, subtitle=None):
        values = palette[role]
        parts.append(f'<rect x="{x}" y="{y}" width="{width}" height="{height}" rx="12" '
                     f'fill="{values[theme]}" stroke="{values["color"]}" stroke-width="2"/>')
        label(x + width / 2, y + (height / 2 + 6 if not subtitle else 30), title)
        if subtitle:
            label(x + width / 2, y + 54, subtitle, "note")

    def arrow(y1, y2, value):
        parts.append(f'<path d="M260 {y1} V{y2}" fill="none" stroke="{muted}" stroke-width="2" marker-end="url(#arrow)"/>')
        if value:
            label(350, (y1 + y2) / 2 + 5, value, "note")

    def storage(y):
        values = palette["storage"]
        parts.append(f'<path d="M90 {y+16} V{y+72} C90 {y+94} 430 {y+94} 430 {y+72} V{y+16}" '
                     f'fill="{values[theme]}" stroke="{values["color"]}" stroke-width="2"/>')
        parts.append(f'<ellipse cx="260" cy="{y+16}" rx="170" ry="16" fill="{values[theme]}" stroke="{values["color"]}" stroke-width="2"/>')
        label(260, y + 51, "vectors.vlite")
        label(260, y + 73, "Vectors, metadata and payloads", "note")

    if mode == "embedded":
        label(260, 36, "YOUR DEVICE / APPLICATION HOST")
        parts.append(f'<rect x="30" y="65" width="460" height="335" rx="16" fill="none" stroke="{muted}" stroke-dasharray="6 5"/>')
        label(260, 98, "One application process", "note")
        box(70, 125, 380, 80, "interface", "Your application", "Rust, Python, Node.js or C")
        arrow(205, 270, "Local calls")
        box(70, 270, 380, 95, "storage", "VLite library", "Storage engine + search indexes")
        arrow(365, 455, "File I/O")
        storage(455)
        label(260, 588, "You manage the file and its backups.")
        label(260, 620, "No database server to run.", "note")
        label(260, 646, "No network API or dashboard required.", "note")
    else:
        label(260, 36, "CLIENTS / YOUR BROWSER")
        box(30, 60, 220, 80, "interface", "Your applications", "REST / gRPC clients")
        box(270, 60, 220, 80, "interface", "Dashboard (optional)", "Browser UI over REST")
        parts.append(f'<path d="M140 140 V178 H260 M380 140 V178 H260 V220" fill="none" stroke="{muted}" stroke-width="2" marker-end="url(#arrow)"/>')
        label(260, 204, "Authenticated requests", "note")
        parts.append(f'<rect x="25" y="230" width="470" height="410" rx="16" fill="none" stroke="{muted}" stroke-dasharray="6 5"/>')
        label(260, 260, "YOUR MACHINE / SERVER")
        box(55, 280, 410, 80, "interface", "vlite-server", "REST + optional gRPC + auth")
        box(90, 380, 340, 75, "storage", "VLite engine in the server", "Same engine as embedded")
        arrow(360, 380, "")
        arrow(455, 495, "File I/O")
        storage(495)
        label(260, 610, "Your disk or persistent Docker volume", "note")
        label(260, 670, "You operate the service, storage and backups.", "note")
    parts.append("</svg>")
    return "\n".join(parts) + "\n"


def main():
    palette = json.loads((ASSETS / "palette.json").read_text(encoding="utf-8"))
    for mode in ("embedded", "self-hosted"):
        for theme in ("light", "dark"):
            (ASSETS / f"deployment-{mode}-{theme}.svg").write_text(
                render(mode, theme, palette), encoding="utf-8")
    print("Generated four deployment diagrams using the shared palette.")


if __name__ == "__main__":
    main()
