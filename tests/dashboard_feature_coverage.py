"""Fail closed on REST inventory drift and disconnected dashboard request handlers.

Discovery is shared with the API reference (including constant/loop registrations,
chained method routers and OpenAPI cross-checks). The contracts below trace named
controls through *bounded* JavaScript scopes to the actual method/path call.
Comments, unrelated handlers and playground strings cannot satisfy a contract.
This is deliberately a small source-contract checker, not a JavaScript interpreter:
when a handler is refactored, update its explicit call chain and the browser tests.
No third-party packages or running server are required.
"""
import re
from dataclasses import dataclass
from pathlib import Path

from admin_route_coverage import INVENTORY as ADMIN_INVENTORY
from api_reference import rest_keys

ROOT = Path(__file__).resolve().parents[1]
COLLECTION = "/v1/collections/{collection}"
MEMORY = "/v1/memory/{binding}/"


def tokens(source):
    """Tokenize the JS subset used by contracts, preserving literals atomically."""
    result = []
    i = 0

    def quoted(start, quote):
        k = start + 1
        while k < len(source):
            if source[k] == "\\":
                k += 2
            elif source[k] == quote:
                return k + 1
            elif quote == "`" and source.startswith("${", k):
                depth, k = 1, k + 2
                while k < len(source) and depth:
                    if source[k] in "\"'`":
                        k = quoted(k, source[k])
                        continue
                    if source[k] == "{":
                        depth += 1
                    elif source[k] == "}":
                        depth -= 1
                    k += 1
            else:
                k += 1
        raise AssertionError("Unclosed JavaScript literal")

    while i < len(source):
        if source[i].isspace():
            i += 1
            continue
        if source.startswith("//", i):
            end = source.find("\n", i)
            i = len(source) if end < 0 else end + 1
            continue
        if source.startswith("/*", i):
            end = source.find("*/", i + 2)
            assert end >= 0, "Unclosed JavaScript comment"
            i = end + 2
            continue
        if source[i] in "\"'`":
            end = quoted(i, source[i])
        elif source[i] == "/" and (not result or result[-1] in (
                "(", "=", ":", ",", "!", "return", "=>", "[", "&&", "||", "?")):
            # Regex literals must not contribute brackets to scope balancing.
            end, character_class, closed = i + 1, False, False
            while end < len(source):
                char = source[end]
                if char == "\\":
                    end += 2
                    continue
                if char == "[":
                    character_class = True
                elif char == "]":
                    character_class = False
                elif char == "/" and not character_class:
                    closed = True
                    end += 1
                    while end < len(source) and source[end].isalpha():
                        end += 1
                    break
                end += 1
            assert closed, "Unclosed JavaScript regex"
        else:
            match = re.match(r"[A-Za-z_$][\w$]*|\d+(?:\.\d+)?|=>|&&|\|\||===|!==|==|!=|\?\.", source[i:])
            end = i + (len(match[0]) if match else 1)
        result.append(source[i:end])
        i = end
    return tuple(result)


def find(sequence, fragment):
    needle = tokens(fragment)
    return [i for i in range(len(sequence) - len(needle) + 1)
            if sequence[i:i + len(needle)] == needle]


def balanced(sequence, start):
    pairs = {"(": ")", "[": "]", "{": "}"}
    stack = []
    for i in range(start, len(sequence)):
        token = sequence[i]
        if token in pairs:
            stack.append(pairs[token])
        elif token in pairs.values():
            assert stack and token == stack.pop(), "Unbalanced JavaScript scope"
            if not stack:
                return sequence[start:i + 1]
    raise AssertionError("Unclosed JavaScript scope")


def scope(sequence, anchor):
    """A complete call or function body beginning at a unique explicit anchor."""
    matches = find(sequence, anchor)
    assert len(matches) == 1, f"Expected one scope {anchor!r}, found {len(matches)}"
    start = matches[0]
    opening = start + len(tokens(anchor))
    # Anchors for on()/addEventListener() end just before their opening '('.
    if sequence[opening] == "(":
        return balanced(sequence, opening)
    # Function anchors include the argument list; arrow anchors include '=>'.
    if sequence[opening] == "{":
        return balanced(sequence, opening)
    # Expression-bodied arrow functions are bounded by their declaration ';'.
    end = opening
    while end < len(sequence) and sequence[end] != ";":
        if sequence[end] in ("(", "[", "{"):
            part = balanced(sequence, end)
            end += len(part)
        else:
            end += 1
    assert end < len(sequence), f"Unbounded expression: {anchor}"
    return sequence[opening:end]


@dataclass(frozen=True)
class Contract:
    script: str
    control: str
    # Each step names the bounded scope and the executable call/expression in it.
    steps: tuple


INVENTORY = {}


def add(method, path, control, expression, *, script="console-tools.js",
        anchor=None, steps=(), call="ctx.api"):
    key = f"rest:{method} {path}"
    assert key not in INVENTORY, f"Duplicate dashboard contract: {key}"
    anchor = anchor or f'on("{control}",'
    INVENTORY[key] = Contract(script, control, (
        (anchor, f'{call}("{method}", {expression}'), *steps))


def chain(method, path, control, steps, script="console-tools.js"):
    key = f"rest:{method} {path}"
    assert key not in INVENTORY, f"Duplicate dashboard contract: {key}"
    INVENTORY[key] = Contract(script, control, tuple(steps))


def on(control):
    return f'on("{control}",'


def event(control, kind="click"):
    return f'$("{control}").addEventListener("{kind}",'


# Advanced retrieval's shared POST helper must receive the intended suffix.
for suffix, control in (
    ("/search/grouped", "group-search"), ("/search/named", "named-search"),
    ("/search/sparse", "sparse-search"), ("/search/dense-sparse", "dense-sparse-search"),
    ("/search/geo", "geo-search"), ("/recommend", "recommend-run"),
    ("/facets", "facets-run"), ("/stats/numeric", "stats-run"),
    ("/query", "language-run"), ("/search/disk-ann", "disk-probe"),
):
    chain("POST", COLLECTION + suffix, control, (
        (on(control), f'read("{suffix}",'),
        ("async function read(suffix, body, result)", 'ctx.api("POST", route(suffix), body'),
    ))
for method, suffix, control, expression in (
    ("POST", "/vectors/configure", "vector-configure", 'route("/vectors/configure")'),
    ("PUT", "/vectors/{id}/named", "vector-values-save", 'route(`/vectors/${recordId}/named`)'),
    ("POST", "/index/disk-ann", "disk-build", 'route("/index/disk-ann")'),
    ("POST", "/embed", "embed-generate", '`/v1/collections/${collectionName(ctx.collection())}/embed`'),
):
    add(method, COLLECTION + suffix, control, expression)
chain("GET", "/v1/changes", "feed-load", (
    (on("feed-load"), 'loadFeed($("feed-cursor").value.trim())'),
    ("async function loadFeed(cursor)", 'ctx.api("GET", feedPath(cursor, $("feed-limit").value))'),
    ("function feedPath(cursor, limit)", 'return `/v1/changes?${query}`'),
))

# Keep the established admin control inventory, but require request linkage here,
# rather than counting a method/path string elsewhere in the script as evidence.
ADMIN_SPECIAL = {
    "list-backups", "cred-list", "cred-table", "backup-verify", "backup-drill",
    "tenant-save-settings", "tenant-suspend", "tenant-resume", "replication-export",
    "obs-refresh", "webhook-list", "tenant-list",
}
ADMIN_DYNAMIC = {
    "/v1/admin/tenants/{id}": '`/v1/admin/tenants/${value}`',
    "/v1/admin/tenants/{id}/usage": '`/v1/admin/tenants/${id}/usage`',
    "/v1/admin/users/{id}": '`/v1/admin/users/${id}`',
    "/v1/admin/users/{id}/password": '`/v1/admin/users/${id}/password`',
    "/v1/admin/webhooks/{id}": '`/v1/admin/webhooks/${webhookId()}`',
    "/v1/admin/audit/persistent": '`/v1/admin/audit/persistent?limit=${limit}`',
}
for (method, path), (script, control) in ADMIN_INVENTORY.items():
    if control in ADMIN_SPECIAL:
        continue
    expression = ADMIN_DYNAMIC.get(path, f'"{path}"')
    if method == "DELETE" and path == "/v1/admin/webhooks/{id}":
        expression = '`/v1/admin/webhooks/${value}`'
    add(method, path, control, expression, script=script)
for control, path, helper in (
    ("list-backups", "/v1/admin/backups", "listBackups"),
    ("cred-list", "/v1/admin/credentials", "listCredentials"),
):
    chain("GET", path, control, (
        (on(control), f'{helper}()'),
        (f'async function {helper}()', f'ctx.api("GET", "{path}"'),
    ))
chain("GET", "/v1/admin/webhooks", "webhook-list", (
    (on("webhook-list"), '"webhook-list", list'),
    ('const list = async () =>', 'ctx.api("GET", "/v1/admin/webhooks"'),
))
chain("GET", "/v1/admin/tenants", "tenant-list", (
    (on("tenant-list"), "listTenants(mark)"),
    ("const listTenants = async (mark) =>", 'ctx.api("GET", "/v1/admin/tenants"'),
), "admin-tools.js")
for action in ("rotate", "revoke"):
    chain("POST", f"/v1/admin/credentials/{{id}}/{action}", "cred-table", (
        ("function credentialRows(items)", 'button.addEventListener("click", () => ctx.run(() => credentialAction(item, verb)))'),
        ("function credentialRows(items)", f'"{action}"'),
        ("async function credentialAction(item, verb)", f'ctx.api("POST", `/v1/admin/credentials/${{item.id}}/{action}`'),
    ))
for action, control in (("verify", "backup-verify"), ("drill", "backup-drill")):
    loop = 'for (const [name, verb] of [["backup-verify", "verify"], ["backup-drill", "drill"]])'
    chain("POST", f"/v1/admin/backups/{{name}}/{action}", control, (
        (loop, 'on(name, async () =>'),
        (loop, 'ctx.api("POST", `/v1/admin/backups/${backup}/${verb}`'),
    ))
for method, suffix, control in (
    ("PUT", "settings", "tenant-save-settings"),
    ("POST", "suspend", "tenant-suspend"), ("POST", "resume", "tenant-resume"),
):
    chain(method, f"/v1/admin/tenants/{{id}}/{suffix}", control, (
        (on(control), f'lifecycle(mark, "{method}", id, "{suffix}",'),
        ("const lifecycle = async (mark, method, id, suffix, body, note) =>",
         'ctx.api(method, `/v1/admin/tenants/${id}/${suffix}`, body'),
    ), "admin-tools.js")
chain("GET", "/v1/admin/replication/export", "replication-export", (
    (on("replication-export"), 'const path = exportPath('),
    (on("replication-export"), 'ctx.api("GET", path,'),
    ("function exportPath(after, maxBytes)", 'return query ? `${EXPORT_ROUTE}?${query}` : EXPORT_ROUTE'),
    ("const EXPORT_ROUTE =", '"/v1/admin/replication/export"'),
), "admin-tools.js")

# Collection management, resumable import and index lifecycle.
for method, suffix, control in (
    ("GET", "/index/hnsw", "index-memory"),
    ("POST", "/index/hnsw/quality", "quality-run"),
    ("GET", "/index/hnsw/rebuild", "status-rebuild"),
    ("POST", "/index/hnsw/rebuild", "start-rebuild"),
    ("POST", "/index/hnsw/rebuild/poll", "poll-rebuild"),
    ("POST", "/index/hnsw/rebuild/cancel", "cancel-rebuild"),
    ("GET", "/index/hnsw/maintenance", "read-maintenance"),
    ("POST", "/index/hnsw/maintenance/tick", "tick-maintenance"),
):
    add(method, COLLECTION + suffix, control, f'indexPath("{suffix}")')
    # The same control deliberately supports servers predating collections.
    add(method, "/v1" + suffix, control, f'indexPath("{suffix}")')
add("PUT", "/v1/index/hnsw/maintenance", "save-maintenance", '"/v1/index/hnsw/maintenance"')
for method, path, control, expression in (
    ("POST", "/v1/collections", "collection-create", '"/v1/collections"'),
    ("DELETE", COLLECTION, "collection-drop", '`/v1/collections/${name}`'),
    ("GET", COLLECTION, "settings-load", 'ctx.settingsPath("")'),
    ("PUT", COLLECTION + "/settings", "settings-save", 'ctx.settingsPath("/settings")'),
    ("POST", COLLECTION + "/batches", "write-commit", 'collectionPath(draft.owner, "/batches")'),
    ("POST", "/v1/batches", "write-commit", 'collectionPath(draft.owner, "/batches")'),
    ("POST", COLLECTION + "/imports", "job-start", '`/v1/collections/${owner}/imports`'),
):
    add(method, path, control, expression)
chain("GET", "/v1/imports/{job}", "job-status", (
    (on("job-status"), 'readJob()'),
    ("async function readJob()", 'ctx.api("GET", `/v1/imports/${value}`'),
))
chain("PUT", "/v1/imports/{job}/chunks/{chunk}", "job-start", (
    (on("job-start"), 'sendChunks(1)'),
    ("async function sendChunks(from)", 'ctx.api("PUT", `/v1/imports/${job.id}/chunks/${n}`'),
))
for action in ("finish", "abort"):
    loop = 'for (const [name, verb] of [["job-finish", "finish"], ["job-abort", "abort"]])'
    chain("POST", f"/v1/imports/{{job}}/{action}", f"job-{action}", (
        (loop, 'on(name, async () =>'),
        (loop, 'ctx.api("POST", `/v1/imports/${status.job}/${verb}`'),
    ))

# KV and memory actions share handlers; check the binding/verb sources as well.
for suffix, control, helper in (("entry", "kv-get", "getValue"), ("entries", "kv-list", "list")):
    chain("GET", COLLECTION + "/kv/" + suffix, control, (
        (on(control), f'{helper}('),
        ("async function getValue()" if suffix == "entry" else "async function list(next)",
         f'ctx.api("GET", `${{base}}/{suffix}?${{query}}`'),
        ("const checkKV = operation =>", 'return kvAddress(ctx.collection(), $("kv-namespace").value)'),
        ("function kvAddress(collection, namespace, key)", 'return `/v1/collections/${collection}/kv`'),
    ))
for method, action in (("PUT", "save"), ("DELETE", "delete")):
    chain(method, COLLECTION + "/kv/entry", f"kv-{action}", (
        ('for (const action of ["save", "delete"])', 'on(`kv-${action}`,'),
        ('on(`kv-${action}`,', 'deleting = action === "delete"'),
        ('on(`kv-${action}`,', 'ctx.api(deleting ? "DELETE" : "PUT", `${base}/entry${deleting ? `?${query}` : ""}`, body)'),
        ("const checkKV = operation =>", 'return kvAddress(ctx.collection(), $("kv-namespace").value)'),
        ("function kvAddress(collection, namespace, key)", 'return `/v1/collections/${collection}/kv`'),
    ))
for action in ("remember", "recall", "consolidate", "forget", "receipts/lookup"):
    chain("POST", MEMORY + action, "memory-run", (
        (on("memory-run"), 'const action = $("memory-action").value'),
        (on("memory-run"), 'memoryRequest(action, input, ctx.serverNow())'),
        (on("memory-run"), 'ctx.api("POST", `/v1/memory/${encodeURIComponent(binding)}/${action}`, body'),
    ))

# Main app controls use the same path builders as the exported tools context.
for method, suffix, control, expression, kind in (
    ("GET", "/vectors", "browse-form", 'path(`/vectors?limit=${requestedSize}${after === null ? "" : `&after=${after}`}`)', "submit"),
    ("POST", "/search", "search-form", 'settingsPath("/search")', "submit"),
    ("POST", "/index/hnsw", "build-index", 'settingsPath("/index/hnsw")', "click"),
):
    steps = [(event(control, kind), 'browse(')] if control == "browse-form" else []
    anchor = "async function browse(after)" if control == "browse-form" else event(control, kind)
    if control == "search-form":
        steps.append((event(control, kind), "run(search)"))
        anchor = "async function search()"
    steps.append((anchor, f'api("{method}", {expression}'))
    chain(method, COLLECTION + suffix, control, steps, "app.js")
    chain(method, "/v1" + suffix, control, steps, "app.js")
for method, suffix, control, kind in (
    ("PUT", "", "vector-form", "submit"),
    ("PATCH", "/metadata", "patch-metadata", "click"),
    ("DELETE", "", "delete-vector", "click"),
):
    expression = '`${target}/metadata`' if suffix else "target"
    steps = ((event(control, kind), 'const target = recordPath()'),
             (event(control, kind), f'api("{method}", {expression}'))
    chain(method, COLLECTION + "/vectors/{id}" + suffix, control, steps, "app.js")
    chain(method, "/v1/vectors/{id}" + suffix, control, steps, "app.js")
for prefix in (COLLECTION, "/v1"):
    chain("GET", prefix + "/vectors/{id}", "get-vector", (
        (event("get-vector"), 'loadRecord(recordPath())'),
        ("async function loadRecord(recordPath)", 'api("GET", recordPath,'),
    ), "app.js")
for method, path, control, anchor, expression in (
    ("GET", "/v1/capabilities", "connection", "async function connect()", '"/v1/capabilities"'),
    ("GET", "/v1/database", "session-refresh", "async function connect()", '"/v1/database"'),
    ("GET", "/metrics", "session-refresh", "async function connect()", '"/metrics"'),
    ("GET", "/v1/collections", "connection", "async function connect()", '"/v1/collections"'),
):
    chain(method, path, control, (
        (event(control, "submit") if control == "connection" else event(control),
         "await connect()" if control == "connection" else "run(connect)"),
        (anchor, f'api("{method}", {expression}'),
    ), "app.js")
for method, path, control, anchor in (
    ("POST", "/v1/auth/login", "sign-in", event("sign-in", "submit")),
    ("POST", "/v1/auth/password", "change-password", event("change-password", "submit")),
    ("POST", "/v1/auth/logout", "disconnect", "function signOut(quiet = false)"),
    ("GET", "/v1/auth/config", "connection", "async function restoreSession()"),
    ("GET", "/v1/auth/session", "connection", "async function restoreSession()"),
):
    steps = [(anchor, f'authRequest("{method}", "{path}"')]
    if path == "/v1/auth/logout":
        steps.insert(0, (event("disconnect"), "disconnect()"))
    chain(method, path, control, steps, "app.js")

# Graph UI is dynamically built. Its read operation table is checked per route,
# and its request object must actually flow into send(), not merely be exported.
GRAPH = "/v1/graphs/{graph}"
for operation, suffix in (
    ("get-node", "nodes/get"), ("get-edge", "edges/get"),
    ("find-nodes", "nodes/find"), ("neighbors", "neighbors"),
    ("traverse", "traverse"), ("shortest-path", "shortest-path"),
):
    operation_key = f'"{operation}"' if "-" in operation else operation
    chain("POST", GRAPH + "/" + suffix, "graph-run", (
        (event("graph-run"), 'run(false)'),
        ("const run = async (explain) =>", 'call = request(operation, $("graph-name").value, form(), explain)'),
        ("const run = async (explain) =>", 'const operation = $("graph-operation").value'),
        ("const run = async (explain) =>", 'send(call.method, call.path, call.body)'),
        ("function request(operation, graph, form, explain = false)",
         'method: "POST", path: graphPath(graph, OPERATIONS[operation].path)'),
        ("const OPERATIONS =", f'{operation_key}: {{ path: "{suffix}"'),
    ), "graph.js")
chain("POST", GRAPH + "/explain", "graph-explain", (
    (event("graph-explain"), 'run(true)'),
    ("const run = async (explain) =>", 'request(operation, $("graph-name").value, form(), explain)'),
    ("const run = async (explain) =>", 'send(call.method, call.path, call.body)'),
    ("function request(operation, graph, form, explain = false)", 'if (explain) return { method: "POST", path: graphPath(graph, "explain")'),
), "graph.js")
for method, path, control, expression, helper in (
    ("GET", "/v1/graphs", "graph-list", '"/v1/graphs"', "const probe = async () =>"),
    ("POST", "/v1/graphs", "graph-create-save", '"/v1/graphs"', None),
    ("DELETE", GRAPH, "graph-drop", "graphPath(graph)", None),
    ("POST", GRAPH + "/batch", "graph-edit-save", 'graphPath(graph, "batch")', "const saveEdit = async () =>"),
    ("GET", "/v1/admin/graph/stats", "graph-stats", "STATS_ROUTE", "const loadStats = async () =>"),
):
    callback = {"graph-list": "probe", "graph-edit-save": "saveEdit", "graph-stats": "loadStats"}
    steps = [(event(control), f'"click", {callback[control]}')] if helper else []
    steps.append((helper or event(control), f'send("{method}", {expression}'))
    chain(method, path, control, steps, "graph.js")
for path, loader, expression in (
    ("/v1/admin/metrics/summary", "metrics", '"/v1/admin/metrics/summary"'),
    ("/v1/admin/logs", "logs", '`/v1/admin/logs?${query}`'),
):
    chain("GET", path, "obs-refresh", (
        (event("obs-refresh"), 'refresh(true)'),
        ("async function refresh(manual = false)", f'{loader}('),
        (f"async function {loader}(epoch)", f'get({expression}'),
        ("async function get(path, text = false)", 'connection().request(path, { text })'),
    ), "observability.js")

# Explicit exclusions, never inferred from what happens to be registered today.
EXCLUDED = {
    "rest:GET /health": "Load-balancer liveness probe, not an operator action.",
    "rest:GET /ready": "Load-balancer readiness probe, not an operator action.",
    "rest:GET /openapi.json": "Machine-readable schema used by graph capability probing.",
    "rest:GET " + GRAPH: "Graph metadata API; UI uses graph listing and query results.",
    "rest:GET " + GRAPH + "/schema": "SDK schema inspection; no dedicated dashboard schema editor.",
    "rest:PUT " + GRAPH + "/schema": "SDK schema administration; no dedicated dashboard schema editor.",
    "rest:POST " + GRAPH + "/query": "Tagged graph-query multiplexor; UI uses explicit operation routes.",
    "rest:POST " + GRAPH + "/retrieve": "Composite SDK retrieval pipeline; not a dedicated dashboard operation.",
}
for asset in (
    "", "/admin-tools.js", "/ambience.js", "/app.js", "/console-tools.js",
    "/graph.css", "/graph.js", "/graph3d.js", "/navigation.js", "/observability.js",
    "/style.css", "/theme.css", "/three.LICENSE", "/three.min.js",
    "/fonts/OFL-JetBrainsMono.txt", "/fonts/OFL-Outfit.txt",
    "/fonts/jetbrains-mono-latin-wght-normal.woff2",
    "/fonts/outfit-latin-wght-normal.woff2",
):
    EXCLUDED["rest:GET /dashboard" + asset] = "Dashboard document/static asset, not a REST action."
UNSUPPORTED = {
    "facts/query": "facts", "facts/correct": "facts",
    "erasures/get": "erasure_jobs", "erasures/resume": "erasure_jobs",
}
for action, feature in UNSUPPORTED.items():
    EXCLUDED["rest:POST " + MEMORY + action] = f"Server explicitly refuses {feature}; checked below."


def bounded_scope(sequence, anchor):
    # Partial call anchors include a literal first argument. Balance from the
    # call's opening parenthesis, retaining the entire callback and no siblings.
    if anchor.startswith(("on(", "$(", "on(name,", "on(`")):
        matches = find(sequence, anchor)
        assert len(matches) == 1, f"Expected one handler {anchor!r}, found {len(matches)}"
        start = matches[0]
        # addEventListener is a second call after $(control).
        if ".addEventListener" in anchor:
            start += tokens(anchor).index("addEventListener")
        opening = next(i for i in range(start, len(sequence)) if sequence[i] == "(")
        return balanced(sequence, opening)
    if anchor.startswith("for ("):
        matches = find(sequence, anchor)
        assert len(matches) == 1, f"Expected one loop {anchor!r}, found {len(matches)}"
        start = matches[0] + 1
        end = start + len(balanced(sequence, start))
        if sequence[end] == "{":
            end += len(balanced(sequence, end))
        else:
            while end < len(sequence) and sequence[end] != ";":
                if sequence[end] in ("(", "[", "{"):
                    end += len(balanced(sequence, end))
                else:
                    end += 1
        return sequence[start:end]
    return scope(sequence, anchor)


def validate_contract(key, scripts, html):
    contract = INVENTORY[key]
    source = scripts[contract.script]
    assert f'id="{contract.control}"' in html or (
        contract.script == "graph.js" and find(source, f'id: "{contract.control}"')
    ), f"{key}: missing named control #{contract.control}"
    for anchor, expression in contract.steps:
        body = bounded_scope(source, anchor)
        assert find(body, expression), (
            f"{key}: #{contract.control} no longer links {anchor!r} to {expression!r}")


def checker_regressions(sources, scripts, html):
    """Negative controls: disconnected/misdirected handlers must not pass."""
    fake = tokens('on("a", () => { /* ctx.api("POST", "/target") */ '
                  'const example = \'ctx.api("POST", "/target")\'; }); '
                  'on("b", () => ctx.api("POST", "/target"));')
    assert not find(bounded_scope(fake, on("a")), 'ctx.api("POST", "/target"')
    for key, before, after in (
        ("rest:POST " + COLLECTION + "/search/grouped",
         'ctx.api("POST", route(suffix)', 'ctx.api("GET", route(suffix)'),
        ("rest:POST " + COLLECTION + "/search/grouped",
         'read("/search/grouped",', 'read("/facets",'),
        ("rest:GET /v1/admin/tenants", "await listTenants(mark)", "await unrelated(mark)"),
        ("rest:DELETE " + COLLECTION + "/kv/entry",
         'deleting ? "DELETE" : "PUT"', 'deleting ? "PUT" : "DELETE"'),
        ("rest:POST " + GRAPH + "/nodes/get",
         '"get-node": { path: "nodes/get"', '"get-node": { path: "edges/get"'),
    ):
        script = INVENTORY[key].script
        assert sources[script].count(before) == 1, f"Update mutation fixture: {before}"
        changed = dict(scripts)
        changed[script] = tokens(sources[script].replace(before, after, 1))
        try:
            validate_contract(key, changed, html)
        except AssertionError:
            continue
        raise AssertionError(f"Coverage checker accepted misdirected handler: {key}: {after}")


def main():
    registered = rest_keys(ROOT)
    classified = set(INVENTORY) | set(EXCLUDED)
    assert not set(INVENTORY) & set(EXCLUDED), "Covered routes cannot also be excluded"
    assert registered == classified, (
        f"Update dashboard REST inventory: unclassified={sorted(registered - classified)}; "
        f"stale={sorted(classified - registered)}")
    dashboard = ROOT / "src" / "dashboard"
    html = (dashboard / "index.html").read_text(encoding="utf-8")
    sources = {name: (dashboard / name).read_text(encoding="utf-8")
               for name in {contract.script for contract in INVENTORY.values()}}
    scripts = {name: tokens(source) for name, source in sources.items()}
    for name in sources:
        assert f'src="/dashboard/{name}"' in html, f"Dashboard does not load {name}"
    assert find(scripts["app.js"], 'if (window.location) restoreSession()'), (
        "Session/config preflight is no longer run on page load")
    for key in INVENTORY:
        validate_contract(key, scripts, html)
    checker_regressions(sources, scripts, html)
    # Shared builders/transports are part of the proof, not trusted stubs.
    helpers = {
        "console-tools.js": (
            ("const route = (suffix) =>", '`/v1/collections/${collectionName(ctx.collection())}${suffix}`'),
            ("const indexPath = (suffix) =>", 'ctx.settingsPath(suffix)'),
            ("function collectionPath(collection, suffix)", 'return collection === "default" ? `/v1${suffix}` : `/v1/collections/${collection}${suffix}`'),
            ("const on = (name, action) =>", 'element.addEventListener("click", () => ctx.run(action))'),
            ("function init(ctx)", "advancedTools(ctx, on, get)"),
            ("function init(ctx)", "dataWorkspaces(ctx, on)"),
            ("function init(ctx)", "studioTools(ctx, on, get)"),
        ),
        "app.js": (
            ("const settingsPath = (suffix) =>", 'return extended() ? `/v1/collections/${utils().collectionName(collection)}${suffix}` : path(suffix)'),
            ("const path = (suffix) =>", 'return utils().collectionPath(collection, suffix)'),
            ("const recordPath = () =>", 'return path(`/vectors/${id}`)'),
            ("async function api(method, path, body, options = {})", 'fetch(`${apiBase}${path}`, {'),
            ("async function api(method, path, body, options = {})", 'method, cache: "no-store"'),
            ("async function authRequest(method, route, body)", 'fetch(`${apiBase}${route}`, { method,'),
            ("async function request(route, options = {})", 'fetch(`${apiBase}${route}`, { method: "GET",'),
            ("async function request(method, path, body)", 'const verb = String(method || "GET").toUpperCase()'),
            ("async function request(method, path, body)", 'const route = String(path || "")'),
            ("async function request(method, path, body)", 'fetch(`${apiBase}${route}`, { method: verb,'),
            ("function disconnect(options = {})", "signOut()"),
            ("const toolsContext =", "api, run, status,"),
            ("const toolsContext =", "path, settingsPath,"),
        ),
        "admin-tools.js": (
            ("const on = (name, area, action) =>", '$(name).addEventListener("click", () => ctx.run('),
            ("const on = (name, area, action) =>", 'await action(mark)'),
        ),
        "observability.js": (
            ("const connection = () =>", "window.VLiteObservabilityConnection"),
        ),
        "graph.js": (
            ("function graphPath(name, suffix)", '`/v1/graphs/${encodeURIComponent(graph)}${suffix ? `/${suffix}` : ""}`'),
            ("const STATS_ROUTE =", '"/v1/admin/graph/stats"'),
            ("const send = async (method, path, payload) =>", "const bridge = win.VLiteGraphConnection"),
            ("const send = async (method, path, payload) =>", "bridge.request(method, path, payload)"),
            ("const send = async (method, path, payload) =>", "cookieRequest(win, method, path, payload)"),
            ("async function cookieRequest(win, method, path, payload)", 'const init = { method,'),
            ("async function cookieRequest(win, method, path, payload)", 'win.fetch(path, init)'),
            ("function init(win)", "section.replaceChildren(...markup(doc))"),
            ("function markup(doc)", "const operationNames = Object.keys(OPERATIONS)"),
            ("function markup(doc)", 'options(doc, "graph-operation", operationNames,'),
        ),
    }
    for script, checks in helpers.items():
        for anchor, expression in checks:
            assert find(bounded_scope(scripts[script], anchor), expression), (
                f"{script}: shared request builder/transport changed: {anchor}")
    for action in ("remember", "recall", "consolidate", "forget", "receipts/lookup"):
        assert f'value="{action}"' in html, f"Missing memory action {action}"
    memory = (ROOT / "src" / "server" / "memory" / "routes.rs").read_text(encoding="utf-8")
    constants = dict(re.findall(r'const\s+(\w+):\s*&str\s*=\s*"([^"]+)"', memory))
    for action, feature in UNSUPPORTED.items():
        constant = next(name for name, path in constants.items() if path == MEMORY + action)
        assert re.search(r"\.route\(\s*" + constant + r"\s*,\s*post\(refused::", memory), (
            f"{action} is no longer refused; implement dashboard controls")
        assert {"facts": "fact correction", "erasure_jobs": "erasure jobs"}[feature] in html.lower()
    refused = (ROOT / "src" / "server" / "memory" / "routes" / "refused.rs").read_text(encoding="utf-8")
    assert "bound.refusal.map(Fail::wire)" in refused
    for phrase in ("graph expiry", "graph algorithms", "checkpoint", "Compact storage"):
        assert phrase in html, f"Missing explicit backend-only disclosure: {phrase}"
    print(f"PASS dashboard REST coverage: {len(registered)} registered method/routes; "
          f"{len(INVENTORY)} linked to named controls; {len(EXCLUDED)} explicit exclusions")


if __name__ == "__main__":
    main()
