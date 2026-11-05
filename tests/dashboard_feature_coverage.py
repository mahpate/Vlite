"""Fail when supported feature routes lack named dashboard controls.

The generic REST playground never counts as feature coverage. This complements
the admin/graph inventory and browser contract tests; it does not replace them.
"""
import re
from pathlib import Path

from admin_route_coverage import admin_routes

ROOT = Path(__file__).resolve().parents[1]
COLLECTION = "/v1/collections/{collection}"
MEMORY = "/v1/memory/{binding}/"
INVENTORY = {
    ("GET", "/v1/changes"): "feed-load",
    ("POST", COLLECTION + "/search/grouped"): "group-search",
    ("POST", COLLECTION + "/recommend"): "recommend-run",
    ("POST", COLLECTION + "/facets"): "facets-run",
    ("POST", COLLECTION + "/stats/numeric"): "stats-run",
    ("POST", COLLECTION + "/search/geo"): "geo-search",
    ("POST", COLLECTION + "/vectors/configure"): "vector-configure",
    ("PUT", COLLECTION + "/vectors/{id}/named"): "vector-values-save",
    ("POST", COLLECTION + "/search/named"): "named-search",
    ("POST", COLLECTION + "/search/sparse"): "sparse-search",
    ("POST", COLLECTION + "/search/dense-sparse"): "dense-sparse-search",
    ("POST", COLLECTION + "/query"): "language-run",
    ("POST", COLLECTION + "/embed"): "embed-generate",
    ("POST", COLLECTION + "/index/disk-ann"): "disk-build",
    ("POST", COLLECTION + "/search/disk-ann"): "disk-probe",
    ("GET", "/v1/admin/replication/export"): "replication-export",
    ("GET", COLLECTION + "/kv/entry"): "kv-get",
    ("PUT", COLLECTION + "/kv/entry"): "kv-save",
    ("DELETE", COLLECTION + "/kv/entry"): "kv-delete",
    ("GET", COLLECTION + "/kv/entries"): "kv-list",
    **{("POST", MEMORY + action): "memory-run" for action in (
        "remember", "recall", "consolidate", "forget", "receipts/lookup")},
}
UNSUPPORTED = {
    ("POST", MEMORY + "facts/query"): "facts",
    ("POST", MEMORY + "facts/correct"): "facts",
    ("POST", MEMORY + "erasures/get"): "erasure_jobs",
    ("POST", MEMORY + "erasures/resume"): "erasure_jobs",
}


def main():
    source = ROOT / "src" / "server"
    registered = set()
    for name in ("features.rs", "kv.rs"):
        registered.update(admin_routes((source / name).read_text(encoding="utf-8"), prefix="/v1/"))
    memory = (source / "memory" / "routes.rs").read_text(encoding="utf-8")
    constants = dict(re.findall(r'const\s+(\w+):\s*&str\s*=\s*"([^"]+)"', memory))
    for constant, method in re.findall(r"\.route\(\s*(\w+),\s*(post|get|put|delete)\(", memory):
        registered.add((method.upper(), constants[constant]))
    assert registered == set(INVENTORY) | set(UNSUPPORTED), (
        f"Update feature coverage: missing={registered - set(INVENTORY) - set(UNSUPPORTED)}, "
        f"stale={(set(INVENTORY) | set(UNSUPPORTED)) - registered}")
    html = (ROOT / "src" / "dashboard" / "index.html").read_text(encoding="utf-8")
    handlers = "\n".join((ROOT / "src" / "dashboard" / name).read_text(encoding="utf-8")
                         for name in ("console-tools.js", "admin-tools.js"))
    for route, control in INVENTORY.items():
        assert f'id="{control}"' in html, f"Missing named control for {route}"
        if control == "memory-run":
            action = route[1].removeprefix(MEMORY)
            assert f'value="{action}"' in html, f"Missing memory action {action}"
        elif control in ("kv-save", "kv-delete"):
            assert 'for (const action of ["save", "delete"]) on(`kv-${action}`' in handlers
        else:
            assert re.search(r'\bon\("' + re.escape(control) + r'"', handlers), (
                f"Missing event handler for {route}: {control}")
    for route, feature in UNSUPPORTED.items():
        constant = next(name for name, path in constants.items() if path == route[1])
        assert re.search(r"\.route\(" + constant + r", post\(refused::", memory), (
            f"{route} is no longer refused; implement its dashboard controls")
        refused = (source / "memory" / "routes" / "refused.rs").read_text(encoding="utf-8")
        assert "bound.refusal.map(Fail::wire)" in refused
        assert {"facts": "fact correction", "erasure_jobs": "erasure jobs"}[feature] in html.lower()
    for phrase in ("graph expiry", "graph algorithms", "checkpoint", "Compact storage"):
        assert phrase in html, f"Missing explicit backend-only disclosure: {phrase}"
    print(f"PASS dashboard feature coverage: {len(INVENTORY)} supported actions have named controls; "
          f"{len(UNSUPPORTED)} refused memory routes and Rust-only operations are explicitly excluded")


if __name__ == "__main__":
    main()
