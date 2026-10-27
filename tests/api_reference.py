#!/usr/bin/env python3
"""Check that site/api.html documents every public API item, and nothing stale.

Every documented item on the page carries a `data-api` key. This script reads
each public surface straight from the sources and compares the two sets:
a key missing from the page is an undocumented API, and a page key that no
source defines is stale documentation.

Keys and their sources:
  py:Class, py:Class.member      bindings/python/vlite_embedded/__init__.py
  node:Class, node:Class.member  bindings/node/index.d.ts (non-private members)
  rust:Type::method              public methods of public types in the vlite crate
  rust:Trait::method             methods of public traits
  rust:fn:name                   public free functions (src/bin and src/server excluded)
  c:op                           bindings/c/src/lib.rs requests
  c:batch-op/op                  batch operations
  c:features/op, c:graph/op      requests wrapped by the `features` and `graph` ops
  c:graph-op/op                  graph batch operations
  c-fn:name                      functions in bindings/c/vlite.h
  wasm:name, wasm:Class.name     bindings/wasm/src/lib.rs (JavaScript names)
  rest:METHOD /path              registered routers in src/server.rs and src/server/
  grpc:Rpc                       proto/vlite/v1/vlite.proto
  mcp:tool                       src/bin/vlite-mcp.rs and src/bin/tool_support/graph_mcp.rs

Rust visibility follows the module tree: an item is public when it is declared
`pub` in a module reachable through `pub mod` declarations, or re-exported
with `pub use` from such a module (under its exported name).

Stdlib only. Usage: python tests/api_reference.py [--list] [PAGE]
"""
from __future__ import annotations

import ast
import html
import json
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
PAGE = ROOT / "site" / "api.html"


# --------------------------------------------------------------------------- shared
def _skip_literal(text: str, k: int) -> int:
    """Index after a string, char literal or comment starting at k, else k."""
    if text.startswith("//", k):
        nl = text.find("\n", k)
        return len(text) if nl < 0 else nl
    if text.startswith("/*", k):
        return text.index("*/", k + 2) + 2
    raw = re.match(r'b?r(#*)"', text[k:k + 12])
    if raw and (k == 0 or not (text[k - 1].isalnum() or text[k - 1] == "_")):
        end = '"' + raw.group(1)
        return text.index(end, k + len(raw.group(0))) + len(end)
    c = text[k]
    if c == '"':
        j = k + 1
        while text[j] != '"':
            j += 2 if text[j] == "\\" else 1
        return j + 1
    if c == "'":
        if text[k + 1] == "\\":
            return text.index("'", k + 2) + 1
        if k + 2 < len(text) and text[k + 2] == "'":
            return k + 3
    return k


def block_end(text: str, start: int) -> int:
    """`start` is just after an opening brace; return the index of its closing brace."""
    depth = 1
    k = start
    while True:
        j = _skip_literal(text, k)
        if j != k:
            k = j
            continue
        if text[k] == "{":
            depth += 1
        elif text[k] == "}":
            depth -= 1
            if depth == 0:
                return k
        k += 1


def _snake(name: str) -> str:
    return re.sub(r"(?<!^)(?=[A-Z])", "_", name).lower()


# --------------------------------------------------------------------------- Python
def python_keys(root: Path) -> set[str]:
    tree = ast.parse((root / "bindings/python/vlite_embedded/__init__.py").read_text(encoding="utf-8"))
    keys: set[str] = set()
    for node in tree.body:
        if isinstance(node, ast.ClassDef) and not node.name.startswith("_"):
            keys.add(f"py:{node.name}")
            for item in node.body:
                if isinstance(item, (ast.FunctionDef, ast.AsyncFunctionDef)) and (
                    not item.name.startswith("_") or item.name == "__init__"
                ):
                    keys.add(f"py:{node.name}.{item.name}")
        elif isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)) and not node.name.startswith("_"):
            keys.add(f"py:{node.name}")
    return keys


# --------------------------------------------------------------------------- Node
NODE_MEMBER_RE = re.compile(r"((?:(?:static|readonly|public|private|get|set|async)\s+)*)([A-Za-z_$][\w$]*)")


def node_keys(root: Path) -> set[str]:
    text = (root / "bindings/node/index.d.ts").read_text(encoding="utf-8")
    keys = {f"node:{n}" for n in re.findall(r"(?m)^export (?:declare )?function (\w+)", text)}
    for m in re.finditer(r"(?m)^export (?:declare )?class (\w+)\b[^{]*\{\n", text):
        cls = m.group(1)
        keys.add(f"node:{cls}")
        depth = 0
        in_doc = False
        for line in text[m.end():block_end(text, m.end())].splitlines():
            s = line.strip()
            if in_doc:
                in_doc = not s.endswith("*/")
                continue
            if s.startswith("/*"):
                in_doc = not s.endswith("*/")
                continue
            if not s or s.startswith("//"):
                continue
            if depth == 0 and line.startswith("  ") and not line.startswith("   "):
                mm = NODE_MEMBER_RE.match(s)
                if mm and "private" not in mm.group(1).split():
                    keys.add(f"node:{cls}.{mm.group(2)}")
            depth += sum(s.count(c) for c in "({[<") - sum(s.count(c) for c in ")}]>") + s.count("=>")
    return keys


# --------------------------------------------------------------------------- Rust
FN_DECL_RE = re.compile(r"(?m)^pub (?:(?:const|async|unsafe) )*fn (\w+)")
TYPE_DECL_RE = re.compile(r"(?m)^pub (struct|enum|trait|union) (\w+)")
ANY_TYPE_DECL_RE = re.compile(r"(?m)^(?:pub(?:\([^)]*\))? )?(?:struct|enum|trait|union) (\w+)")
MOD_RE = re.compile(r"(?m)^(pub(?:\([^)]*\))? )?mod (\w+);")
USE_RE = re.compile(r"(?ms)^pub use ([^;]+);")
IMPL_RE = re.compile(
    r"(?m)^((?:#\[[^\n]*\]\n)*)impl(?:<[^>]*>)? (?:(?:crate|super|self)::(?:\w+::)*)?([A-Za-z_]\w*)(?:<[^>{]*>)? \{"
)
TRAIT_RE = re.compile(r"(?m)^pub trait (\w+)[^{;]*\{")
METHOD_RE = re.compile(
    r"(?m)^((?:    #\[[^\n]*\]\n|    //[^\n]*\n)*)    pub (?:(?:const|async|unsafe) )*fn (\w+)"
)
TRAIT_METHOD_RE = re.compile(r"(?m)^((?:    #\[[^\n]*\]\n|    //[^\n]*\n)*)    (?:(?:async|unsafe) )*fn (\w+)")


def _module_path(rel: str) -> tuple[str, ...]:
    parts = rel[:-3].split("/")
    if parts == ["lib"]:
        return ()
    if parts[-1] == "mod":
        parts = parts[:-1]
    return tuple(parts)


def _use_targets(module: tuple[str, ...], body: str) -> list[tuple[tuple[str, ...], str, str]]:
    """Resolve one `pub use` body to (target module, item name, exported name) triples."""
    body = " ".join(body.split())
    if "{" in body:
        prefix, inner = body.split("{", 1)
        inner = inner.rsplit("}", 1)[0]
        if "{" in inner:
            raise SystemExit(f"api_reference: nested `pub use` lists are not supported: {body}")
        prefix = prefix.rstrip(": ")
        items = [i.strip() for i in inner.split(",") if i.strip()]
    else:
        prefix, items = "", [body]
    out = []
    for item in items:
        path, _, alias = item.partition(" as ")
        segments = (prefix.split("::") if prefix else []) + path.strip().split("::")
        name, segments = segments[-1], segments[:-1]
        base = module
        if segments and segments[0] == "crate":
            base, segments = (), segments[1:]
        elif segments and segments[0] == "self":
            segments = segments[1:]
        while segments and segments[0] == "super":
            base, segments = base[:-1], segments[1:]
        out.append((base + tuple(segments), name, (alias or name).strip()))
    return out


def rust_items(root: Path) -> list[dict]:
    """Public methods, trait methods and free functions of the vlite crate.

    Each item: key, owner (public type or trait name, or None for a free
    function), name, rel (file under src/), module, offset (start of the
    declaration in the file), attrs (attribute and doc lines above it) and
    impl_attrs (attributes above the impl block).
    """
    src = root / "src"
    files: dict[tuple[str, ...], tuple[str, str]] = {}
    for path in sorted(src.rglob("*.rs")):
        rel = path.relative_to(src).as_posix()
        if rel.startswith(("bin/", "server")) or rel.endswith("tests.rs") or "/tests/" in rel:
            continue
        files[_module_path(rel)] = (rel, path.read_text(encoding="utf-8"))

    pub_mod: dict[tuple[str, ...], bool] = {}
    for module, (_, text) in files.items():
        for m in MOD_RE.finditer(text):
            pub_mod[module + (m.group(2),)] = m.group(1) == "pub "

    def reachable(module: tuple[str, ...]) -> bool:
        return all(pub_mod.get(module[:i + 1], False) for i in range(len(module)))

    # (module, name) -> the name a crate user sees, for items public through `pub use`.
    exported: dict[tuple[tuple[str, ...], str], str] = {}
    edges: dict[tuple[tuple[str, ...], str], set[tuple[tuple[str, ...], str]]] = {}
    for module, (_, text) in files.items():
        for m in USE_RE.finditer(text):
            for target, name, alias in _use_targets(module, m.group(1)):
                edges.setdefault((module, alias), set()).add((target, name))
                if reachable(module):
                    exported.setdefault((module, alias), alias)
    pending = list(exported)
    while pending:
        node = pending.pop()
        for target in edges.get(node, ()):
            if target not in exported:
                exported[target] = exported[node]
                pending.append(target)

    def public_name(module: tuple[str, ...], name: str) -> str | None:
        return name if reachable(module) else exported.get((module, name))

    # Declared type name -> public name, for impl blocks outside the declaring file.
    types: dict[str, str] = {}
    pub_types: dict[tuple[str, ...], set[str]] = {}
    for module, (rel, text) in files.items():
        pub_types[module] = {name for _, name in TYPE_DECL_RE.findall(text)}
        for name in sorted(pub_types[module]):
            shown = public_name(module, name)
            if shown is None:
                continue
            if name in types or shown in types.values():
                raise SystemExit(f"api_reference: two public types are named {shown}; give their keys a module path")
            types[name] = shown

    items: list[dict] = []
    seen_fns: set[str] = set()
    for module, (rel, text) in files.items():
        local_types = set(ANY_TYPE_DECL_RE.findall(text))
        for m in FN_DECL_RE.finditer(text):
            shown = public_name(module, m.group(1))
            if shown is None:
                continue
            if shown in seen_fns:
                raise SystemExit(f"api_reference: two public functions are named {shown}")
            seen_fns.add(shown)
            start = text.rfind("\n\n", 0, m.start()) + 1
            items.append({"key": f"rust:fn:{shown}", "owner": None, "name": shown, "rel": rel,
                          "module": module, "offset": m.start(), "attrs": text[start:m.start()],
                          "impl_attrs": ""})
        for m in IMPL_RE.finditer(text):
            if "cfg(test)" in m.group(1):
                continue
            declared = m.group(2)
            if declared in local_types:
                owner = public_name(module, declared) if declared in pub_types[module] else None
            else:
                owner = types.get(declared)
            if owner is None:
                continue
            body_start = m.end()
            body = text[body_start:block_end(text, body_start)]
            for mm in METHOD_RE.finditer(body):
                if "cfg(test)" in mm.group(1) or "doc(hidden)" in mm.group(1):
                    continue
                items.append({"key": f"rust:{owner}::{mm.group(2)}", "owner": owner, "name": mm.group(2),
                              "rel": rel, "module": module, "offset": body_start + mm.end(1) + 4,
                              "attrs": mm.group(1), "impl_attrs": m.group(1)})
        for m in TRAIT_RE.finditer(text):
            trait = public_name(module, m.group(1))
            if trait is None:
                continue
            body_start = m.end()
            body = text[body_start:block_end(text, body_start)]
            for mm in TRAIT_METHOD_RE.finditer(body):
                items.append({"key": f"rust:{trait}::{mm.group(2)}", "owner": trait,
                              "name": mm.group(2), "rel": rel, "module": module,
                              "offset": body_start + mm.end(1) + 4, "attrs": mm.group(1), "impl_attrs": ""})
    return items


def rust_keys(root: Path) -> set[str]:
    return {item["key"] for item in rust_items(root)}


# --------------------------------------------------------------------------- C ABI
C_PREFIXES = {
    ("lib", "Request"): "c:",
    ("lib", "Operation"): "c:batch-op/",
    ("features", "Request"): "c:features/",
    ("graph", "Request"): "c:graph/",
    ("graph", "OpIn"): "c:graph-op/",
}
C_ENUM_RE = re.compile(
    r'#\[serde\(([^\]]*tag = "op"[^\]]*)\)\]\s*\n(?:\s*#\[[^\n]*\]\s*\n)*\s*(?:pub(?:\([^)]*\))? )?enum (\w+)(?:<[^>]*>)? \{'
)
RENAME_RE = re.compile(r'\brename\s*=\s*"([^"]+)"')


def c_keys(root: Path) -> set[str]:
    keys = {
        f"c-fn:{name}"
        for name in re.findall(r"(?m)^VLITE_API\b[^;(]*?\b(vlite_\w+)\s*\(",
                               (root / "bindings/c/vlite.h").read_text(encoding="utf-8"))
    }
    for path in sorted((root / "bindings/c/src").glob("*.rs")):
        text = path.read_text(encoding="utf-8")
        for m in C_ENUM_RE.finditer(text):
            prefix = C_PREFIXES.get((path.stem, m.group(2)))
            if prefix is None:
                raise SystemExit(
                    f"api_reference: new C request enum {path.stem}::{m.group(2)}; add it to C_PREFIXES"
                )
            snake = "snake_case" in m.group(1)
            depth = 0
            attrs: list[str] = []
            for line in text[m.end():block_end(text, m.end())].splitlines():
                s = line.strip()
                if s.startswith("//") or not s:
                    continue
                if depth == 0:
                    if s.startswith("#["):
                        attrs.append(s)
                        continue
                    vm = re.match(r"([A-Z]\w*)\b", s)
                    if vm:
                        rename = RENAME_RE.search(" ".join(attrs))
                        op = rename.group(1) if rename else (_snake(vm.group(1)) if snake else vm.group(1))
                        keys.add(prefix + op)
                    attrs = []
                depth += s.count("{") + s.count("(") - s.count("}") - s.count(")")
    return keys


# --------------------------------------------------------------------------- WASM
WASM_FN_RE = re.compile(r"(?m)^((?:[ ]*#\[[^\n]*\]\n|[ ]*///[^\n]*\n)*)([ ]*)pub fn (\w+)")


def wasm_keys(root: Path) -> set[str]:
    text = (root / "bindings/wasm/src/lib.rs").read_text(encoding="utf-8")

    def js_name(attrs: str, name: str) -> str:
        m = re.search(r'js_name\s*=\s*"?(\w+)', attrs)
        return m.group(1) if m else name

    keys: set[str] = set()
    for m in WASM_FN_RE.finditer(text):
        if not m.group(2) and "wasm_bindgen" in m.group(1):
            keys.add("wasm:" + js_name(m.group(1), m.group(3)))
    for m in re.finditer(r"(?m)^#\[wasm_bindgen[^\n]*\]\nimpl (\w+) \{", text):
        body = text[m.end():block_end(text, m.end())]
        for mm in WASM_FN_RE.finditer(body):
            if mm.group(2) == "    ":
                keys.add(f"wasm:{m.group(1)}." + js_name(mm.group(1), mm.group(3)))
    return keys


# --------------------------------------------------------------------------- servers
RPC_RE = re.compile(r"(?m)^\s*rpc\s+(\w+)\s*\(")
CORE_TOOL_RE = re.compile(r'"name":"(vlite_\w+)","description"')
GRAPH_TOOL_RE = re.compile(r'tool\(\s*(\w+|"[a-z_]+"),')
GRAPH_CONST_RE = re.compile(r'const (\w+_TOOL): &str = "(\w+)";')


def route_keys(source: str) -> set[str]:
    """Extract explicit Axum methods, failing closed on unfamiliar route builders."""
    constants = dict(re.findall(r'const (\w+): &str = "([^"]+)";', source))
    keys: set[str] = set()
    matches, offset = [], 0
    while offset < len(source):
        end = _skip_literal(source, offset)
        if end != offset:
            offset = end
            continue
        if re.match(r"\.(nest|route_service)\s*\(", source[offset:offset + 30]):
            raise AssertionError("Nested/service routing needs explicit API inventory support")
        if re.match(r"\.route\s*\(", source[offset:offset + 20]):
            match = re.compile(r'\.route\s*\(\s*("[^"]+"|\w+)\s*,').match(source, offset)
            assert match, "Unknown route expression; update route extraction"
            matches.append(match)
        offset += 1
    for match in matches:
        raw = match.group(1)
        if raw == "route" and "for (route, _, kind) in QUERIES" in source:
            queries = re.search(r"const QUERIES:.*?= \[(.*?)\];", source, re.S)
            assert queries and re.search(r"fn query_route\b.*?\bpost\(", source, re.S)
            keys.update(f"rest:POST {path}" for path in re.findall(r'"(/[^"]+)"', queries.group(1)))
            continue
        path = raw.strip('"') if raw.startswith('"') else constants.get(raw)
        assert path, f"Unknown route constant {raw}; update route extraction"
        depth, k, methods = 0, match.end(), set()
        while k < len(source):
            end = _skip_literal(source, k)
            if end != k:
                k = end
                continue
            if source[k] == ")" and depth == 0:
                break
            if depth == 0:
                method = re.match(r"(?:axum::routing::)?(get|post|put|patch|delete|head|options|trace)\s*\(", source[k:])
                if method and (k == 0 or not (source[k - 1].isalnum() or source[k - 1] == "_")):
                    methods.add(method.group(1).upper())
            if source[k] in "([{":
                depth += 1
            elif source[k] in ")]}":
                depth -= 1
            k += 1
        assert methods and k < len(source), f"Unknown method router for {path}; update route extraction"
        keys.update(f"rest:{method} {path}" for method in methods)
    return keys


def canonical_rest_key(key: str) -> str:
    # Older OpenAPI paths spell the collection placeholder {c}.
    return key.replace("{c}", "{collection}").replace("{job_id}", "{job}").replace("{n}", "{chunk}")


def rest_keys(root: Path) -> set[str]:
    sources = [root / "src/server.rs", *sorted((root / "src/server").glob("*.rs"))]
    keys = set().union(*(route_keys(path.read_text(encoding="utf-8")) for path in sources))
    declared = set()
    for spec in ("src/openapi.json", "src/openapi_graph.json"):
        doc = json.loads((root / spec).read_text(encoding="utf-8"))
        for path, item in doc["paths"].items():
            for method in ("get", "post", "put", "patch", "delete"):
                if method in item:
                    declared.add(canonical_rest_key(f"rest:{method.upper()} {path}"))
    assert declared <= keys, f"OpenAPI routes not registered by server: {sorted(declared - keys)}"
    return keys


def grpc_keys(root: Path) -> set[str]:
    return {f"grpc:{n}" for n in RPC_RE.findall((root / "proto/vlite/v1/vlite.proto").read_text(encoding="utf-8"))}


def mcp_keys(root: Path) -> set[str]:
    keys = {f"mcp:{n}" for n in CORE_TOOL_RE.findall((root / "src/bin/vlite-mcp.rs").read_text(encoding="utf-8"))}
    graph = (root / "src/bin/tool_support/graph_mcp.rs").read_text(encoding="utf-8")
    consts = dict(GRAPH_CONST_RE.findall(graph))
    for raw in GRAPH_TOOL_RE.findall(graph):
        name = raw.strip('"') if raw.startswith('"') else consts.get(raw)
        if name and name.startswith("vlite_"):
            keys.add(f"mcp:{name}")
    return keys


# --------------------------------------------------------------------------- check
SURFACES = {
    "Python": python_keys,
    "Node.js": node_keys,
    "Rust": rust_keys,
    "C ABI": c_keys,
    "WASM": wasm_keys,
    "REST": rest_keys,
    "gRPC": grpc_keys,
    "MCP": mcp_keys,
}


def source_keys(root: Path = ROOT) -> dict[str, set[str]]:
    out = {}
    for surface, extract in SURFACES.items():
        keys = extract(root)
        if not keys:
            raise SystemExit(f"api_reference: found no {surface} items; the extractor needs updating")
        out[surface] = keys
    return out


def page_keys(text: str) -> set[str]:
    return {canonical_rest_key(html.unescape(k)) for k in re.findall(r'\bdata-api="([^"]+)"', text)}


def main(argv: list[str]) -> int:
    args = [a for a in argv if not a.startswith("--")]
    surfaces = source_keys(ROOT)
    if "--list" in argv:
        for surface, keys in surfaces.items():
            for key in sorted(keys):
                print(key)
        return 0
    page = Path(args[0]) if args else PAGE
    documented = page_keys(page.read_text(encoding="utf-8"))
    expected = set().union(*surfaces.values())
    missing = sorted(expected - documented)
    stale = sorted(documented - expected)
    for title, keys in (("Undocumented API items (add them to the page)", missing),
                        ("Stale page entries (the sources no longer define them)", stale)):
        if keys:
            print(f"{title}: {len(keys)}")
            for key in keys:
                print(f"  {key}")
    if missing or stale:
        print(f"FAIL: {page.name} must document exactly the public API (see the data-api keys).")
        return 1
    counts = ", ".join(f"{surface} {len(keys)}" for surface, keys in surfaces.items())
    print(f"OK: {page.name} documents all {len(expected)} public API items ({counts}).")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
