"""First-class Key-value, MemoryAI and provider controls against explicit REST contracts.

Runs without Docker: python tests/dashboard_data_ui.py
"""
import base64
import json
from urllib.parse import parse_qs

from playwright.sync_api import expect, sync_playwright
from dashboard_e2e import FakeApi, mocked_page, control, confirm, sign_out, assert_no_stored_credentials

MAX_U64 = "18446744073709551615"


class WorkspaceApi(FakeApi):
    def __init__(self, role="scoped"):
        super().__init__(role=role, format_version=5, scope={
            "collections": ["*"], "graphs": ["*"],
            "operations": ["read", "write", "search", "collections", "graph_read", "graph_write"],
            "expires_unix_ms": 2000000000000,
        } if role == "scoped" else None)
        self.collections["cache"] = {**self.collections["docs"], "id": 2, "name": "cache", "dimensions": 1}
        self.caps["features"].update(named_vectors=True, sparse_vectors=True, kv=True)
        self.caps["routes"] = [
            "/v1/collections/{collection}/kv/entry", "/v1/collections/{collection}/kv/entries",
            "/v1/collections/{collection}/query",
        ]
        self.values = {
            "key&one": {"key": "key&one", "version": MAX_U64, "expires_at_ms": MAX_U64, "value_base64": "AP8="},
            "key/two": {"key": "key/two", "version": "2", "expires_at_ms": None, "value_base64": "aGVsbG8="},
        }
        self.fail_write = False
        self.fail_memory = False
        self.fail_vectors = False

    def api(self, method, path, query, body, headers):
        if path == "/v1/capabilities":
            status, value, response_headers = super().api(method, path, query, body, headers)
            return status, value, {**response_headers, "Date": "Thu, 05 Nov 2026 22:00:00 GMT"}
        if "/kv/" in path:
            if self.fail_write and method != "GET":
                return self.error(412, "precondition_failed")
            params = parse_qs(query, keep_blank_values=True)
            if path.endswith("/entries"):
                keys = list(self.values)
                selected = [self.values[key] for key in keys if key > params.get("after", [""])[0]]
                limit = int(params["limit"][0])
                return 200, {"items": selected[:limit], "next_after": selected[limit - 1]["key"] if len(selected) > limit else None}, {}
            key = body["key"] if method == "PUT" else params["key"][0]
            if method == "PUT":
                self.values[key] = {"key": key, "version": "3", "expires_at_ms": body.get("expires_at_ms"), "value_base64": body["value_base64"]}
                return 200, self.values[key], {}
            if method == "DELETE":
                self.values.pop(key, None)
                return 200, {"deleted": True}, {}
            if key not in self.values:
                return self.error(404, "kv_not_found")
            return 200, self.values[key], {}
        if path.startswith("/v1/memory/"):
            if self.fail_memory:
                return self.error(503, "busy")
            if body["scope"] != {"agent": "assistant", "user": "owner"}:
                return self.error(403, "forbidden_scope")
            if path.endswith("/recall"):
                return 200, {"memories": [{"id": "<entity>", "revision": MAX_U64, "kind": "entity", "content": "<script>private memory</script>", "source": {"source_id": "test"}}],
                             "truncation": ["candidate_limit"], "stats": {"candidates": 1}}, {}
            return 200, {"receipt": {"operation": path.rsplit("/", 1)[1], "revision": MAX_U64, "retry_until_ms": MAX_U64}, "outcomes": []}, {}
        if path.endswith("/embed"):
            return 200, {"vector": [1, 0, 0], "dimensions": 3, "provider": body["provider"]}, {}
        if path.endswith("/vectors/configure"):
            return 200, {"configured": body["name"], **body}, {}
        if path.endswith("/named") and method == "PUT":
            if self.fail_vectors:
                return self.error(409, "vector_conflict")
            return 200, {"updated": True}, {}
        if path.endswith("/search/sparse"):
            return 200, {"hits": [{"id": "1", "distance": -.5}]}, {}
        if path.endswith("/search") and body.get("rerank", {}).get("mode") == "custom":
            return 200, {"hits": [{"id": "1", "distance": .5, "rerank_score": .9}],
                         "ranking": {"provider": body["rerank"]["provider"], "order": "score_desc_id_asc"}}, {}
        return super().api(method, path, query, body, headers)


def exercise_kv_bytes(browser):
    fake = WorkspaceApi()
    fixtures = {"cr": b"a\rb", "crlf": b"a\r\nb", "bom": b"\xef\xbb\xbfhello", "line\nbreak": b'{"value":1}'}
    fake.values = {key: {"key": key, "version": "2", "expires_at_ms": None,
                         "value_base64": base64.b64encode(value).decode()}
                   for key, value in fixtures.items()}
    context, page, errors = mocked_page(browser, fake)
    page.locator("#collection").select_option("cache")
    control(page, "#kv-list")
    for key, value in fixtures.items():
        page.locator("#kv-list").click()
        page.locator("#kv-entries").get_by_role("button", name=key, exact=True).click()
        expect(page.locator("#kv-key")).to_have_value(key)
        if key != "line\nbreak":
            expect(page.locator("#kv-format")).to_have_value("base64")
        page.locator("#kv-expiry").fill(MAX_U64)
        page.locator("#kv-save").click()
        confirm(page, "Save key")
        expect(page.locator("#kv-outcome")).to_contain_text("Server confirmed")
        put = next(item for item in reversed(fake.log) if item["method"] == "PUT")
        assert put["body"]["key"] == key
        assert base64.b64decode(put["body"]["value_base64"]) == value
        expect(page.locator("#kv-version")).to_have_value("3")
    # A display format change converts bytes; it never reinterprets base64 as text.
    page.locator("#kv-format").select_option("base64")
    expect(page.locator("#kv-value")).to_have_value(base64.b64encode(fixtures["line\nbreak"]).decode())
    page.locator("#kv-format").select_option("json")
    expect(page.locator("#kv-value")).to_have_value('{"value":1}')
    page.locator("#kv-value").fill('{"value":2}')
    page.locator("#kv-namespace").fill("another")
    expect(page.locator("#kv-value")).to_have_value('{"value":2}')
    expect(page.locator("#kv-key")).to_have_value("line\nbreak")
    expect(page.locator("#kv-version")).to_have_value("")
    expect(page.locator("#kv-condition")).to_have_value("absent")
    page.locator("#kv-save").click()
    confirm(page, "Save key")
    expect(page.locator("#kv-outcome")).to_contain_text("Server confirmed")
    put = next(item for item in reversed(fake.log) if item["method"] == "PUT")
    assert put["body"]["namespace"] == "another" and put["body"]["expected"] == "absent"
    assert base64.b64decode(put["body"]["value_base64"]) == b'{"value":2}'
    assert not errors, errors
    context.close()
    fake = WorkspaceApi()
    fake.caps["features"]["kv"] = False
    context, page, errors = mocked_page(browser, fake)
    page.locator("#collection").select_option("cache")
    control(page, "#kv-list")
    for name in ("list", "get", "save", "delete"):
        expect(page.locator("#kv-" + name)).to_be_disabled()
    assert not errors, errors
    context.close()


def exercise(browser):
    fake = WorkspaceApi()
    context, page, errors = mocked_page(browser, fake)
    control(page, "#kv-list")
    expect(page.locator("#kv-list")).to_be_disabled()
    page.locator("#collection").select_option("cache")
    expect(page.locator("#kv-list")).to_be_enabled()
    page.locator("#kv-namespace").fill("n&space")
    page.locator("#kv-limit").fill("1")
    page.locator("#kv-list").click()
    expect(page.locator("#kv-entries tbody tr")).to_have_count(1)
    page.locator("#kv-entries button").click()
    expect(page.locator("#kv-format")).to_have_value("base64")
    expect(page.locator("#kv-value")).to_have_value("AP8=")
    expect(page.locator("#kv-version")).to_have_value(MAX_U64)
    expect(page.locator("#kv-expiry")).to_have_value(MAX_U64)
    page.locator("#kv-value").fill("aGVsbG8=")
    page.locator("#kv-expiry").fill("")
    page.locator("#kv-save").click()
    confirm(page, "Save key")
    expect(page.locator("#kv-outcome")).to_contain_text("Server confirmed")
    request = next(item for item in reversed(fake.log) if item["method"] == "PUT")
    assert request["query"] == ""
    assert request["body"]["expected"] == {"revision": MAX_U64}
    assert request["body"]["namespace"] == "n&space"
    assert "expires_at_ms" not in request["body"]
    expect(page.locator("#kv-version")).to_have_value("3")
    page.locator("#kv-save").click()
    confirm(page, "Save key")
    expect(page.locator("#kv-outcome")).to_contain_text("Server confirmed")
    request = next(item for item in reversed(fake.log) if item["method"] == "PUT")
    assert request["body"]["expected"] == {"revision": "3"}
    page.locator("#kv-list").click()
    expect(page.locator("#kv-next")).to_be_enabled()
    page.locator("#kv-next").click()
    expect(page.locator("#kv-entries")).to_contain_text("key/two")
    page.locator("#kv-entries button").click()
    fake.fail_write = True
    before = len([item for item in fake.log if item["method"] == "PUT"])
    page.locator("#kv-save").click()
    confirm(page, "Save key")
    expect(page.locator("#kv-outcome")).to_contain_text("precondition_failed")
    expect(page.locator("#kv-value")).to_have_value("hello")
    assert len([item for item in fake.log if item["method"] == "PUT"]) == before + 1
    page.locator("#kv-key").fill("new-key")
    expect(page.locator("#kv-condition")).to_have_value("absent")
    expect(page.locator("#kv-version")).to_have_value("")
    # Discard the unsaved KV draft when switching collections.
    page.locator("#collection").select_option("default")
    confirm(page, "Key-value")
    control(page, "#memory-binding").fill("assistant-memory")
    page.locator("#memory-agent").fill("assistant")
    page.locator("#memory-user").fill("owner")
    expect(page.locator("#memory-availability")).to_contain_text("not listed")
    page.locator("#memory-seeds").fill("<entity>")
    page.locator("#memory-run").click()
    expect(page.locator("#memory-results")).to_contain_text("<script>private memory</script>")
    assert page.locator("#memory-results script").count() == 0
    expect(page.locator("#memory-result")).to_contain_text("candidate_limit")
    page.locator("#memory-ranking").select_option("mmr")
    page.locator("#memory-vector").fill("[2,0,0]")
    before = len(fake.log)
    page.locator("#memory-run").click()
    expect(page.locator("#status")).to_contain_text("vector alone")
    assert not any(item["path"].endswith("/recall") for item in fake.log[before:])
    page.locator("#memory-seeds").fill("")
    page.locator("#memory-limit").fill("7")
    page.locator("#memory-lambda").fill("0.25")
    page.locator("#memory-run").click()
    expect(page.locator("#memory-outcome")).to_contain_text("server confirmed")
    recalled = next(item for item in reversed(fake.log) if item["path"].endswith("/recall"))
    assert recalled["body"]["ranking"] == {"selection": "mmr", "weights": [1, 0, 0], "lambda": .25}
    assert recalled["body"]["limit"] == 7
    page.locator("#memory-action").select_option("remember")
    for name, value in [("id", "new-memory"), ("content", "Synthetic memory"), ("source", "test"), ("idempotency", "once")]:
        page.locator("#memory-" + name).fill(value)
    page.evaluate("window.originalDateNow = Date.now; Date.now = () => 4102444800000")
    page.locator("#memory-run").click()
    confirm(page, "Idempotency key: once")
    expect(page.locator("#memory-result")).to_contain_text('"operation": "remember"')
    remembered = next(item for item in reversed(fake.log) if item["path"].endswith("/remember"))
    assert remembered["body"]["items"][0]["observed_at_ms"] == "1793916000000"
    page.evaluate("Date.now = window.originalDateNow")
    with page.expect_download() as downloaded:
        page.locator("#memory-download").click()
    saved = json.loads(open(downloaded.value.path(), encoding="utf-8").read())
    assert saved["request"]["idempotency_key"] == "once"
    assert saved["response"]["receipt"]["revision"] == MAX_U64
    assert "token" not in saved
    fake.fail_memory = True
    page.locator("#memory-idempotency").fill("uncertain")
    page.locator("#memory-run").click()
    confirm(page, "Idempotency key: uncertain")
    expect(page.locator("#memory-outcome")).to_contain_text("Outcome may be unknown")
    attempts = [item for item in fake.log if item["path"].endswith("/remember")]
    assert len(attempts) == 2, "Failed memory mutations must not be automatically retried"
    original = attempts[-1]["body"]
    with page.expect_download() as downloaded:
        page.locator("#memory-download-request").click()
    submitted = json.loads(open(downloaded.value.path(), encoding="utf-8").read())
    assert submitted["request"] == original
    fake.fail_memory = False
    page.locator("#memory-run").click()
    confirm(page, "Idempotency key: uncertain")
    expect(page.locator("#memory-outcome")).to_contain_text("server confirmed")
    replay = next(item for item in reversed(fake.log) if item["path"].endswith("/remember"))
    assert replay["body"] == original, "Explicit replay of an unchanged draft must preserve its timestamp"
    page.locator("#memory-action").select_option("consolidate")
    page.locator("#memory-transformation").fill("summary-v1")
    page.locator("#memory-references").fill(json.dumps([{"id": "<entity>", "revision": MAX_U64}]))
    page.locator("#memory-idempotency").fill("summary-once")
    page.locator("#memory-run").click()
    confirm(page, "retain originals")
    expect(page.locator("#memory-result")).to_contain_text('"operation": "consolidate"')
    request = next(item for item in reversed(fake.log) if item["path"].endswith("/consolidate"))
    assert request["body"]["output"]["kind"] == "episodic"
    assert request["body"]["inputs"][0]["revision"] == MAX_U64
    page.locator("#memory-action").select_option("receipts/lookup")
    page.locator("#memory-receipt-operation").select_option("consolidate")
    page.locator("#memory-run").click()
    expect(page.locator("#memory-result")).to_contain_text('"receipt"')
    request = next(item for item in reversed(fake.log) if item["path"].endswith("/receipts/lookup"))
    assert request["body"]["operation"] == "consolidate"
    page.locator("#memory-action").select_option("forget")
    page.locator("#memory-references").fill(json.dumps([{"id": "<entity>", "revision": MAX_U64}]))
    page.locator("#memory-idempotency").fill("delete-once")
    page.locator("#memory-run").click()
    confirm(page, "NOT secure erasure")
    expect(page.locator("#memory-result")).to_contain_text('"operation": "forget"')
    request = next(item for item in reversed(fake.log) if item["path"].endswith("/forget"))
    assert request["body"]["mode"] == "logical_delete"
    assert request["body"]["target"]["ids"][0]["revision"] == MAX_U64
    page.locator("#memory-user").fill("someone-else")
    expect(page.locator("#memory-result")).to_have_text("")
    expect(page.locator("#memory-download")).to_be_disabled()
    page.locator("#memory-action").select_option("recall")
    page.locator("#memory-ranking").select_option("weighted")
    page.locator("#memory-run").click()
    expect(page.locator("#memory-outcome")).to_contain_text("forbidden_scope")
    control(page, "#embed-text").fill("Synthetic embedding input")
    page.locator("#embed-provider").fill("Managed.Semantic_V2-1")
    page.locator("#embed-generate").click()
    confirm(page, "Text may leave this server")
    expect(page.locator("#embed-result")).to_contain_text("Query vector ready")
    expect(page.locator("#query")).to_have_value("[1,0,0]")
    control(page, "#rerank-mode").select_option("custom")
    page.locator("#rerank-provider").fill("my-ranker")
    page.locator("#search-form button[type=submit]").click()
    expect(page.locator("#search-summary")).to_contain_text("my-ranker")
    expect(page.locator("#hits")).to_contain_text("Provider score (higher is better)")
    expect(page.locator("#hits caption")).to_contain_text("higher provider score")
    assert ".." not in page.locator("#search-summary").inner_text()
    expect(page.locator("#hits")).to_contain_text("0.9")
    control(page, "#vector-spec-name").fill("text")
    page.locator("#vector-spec-version").fill("v1")
    page.locator("#vector-configure").click()
    confirm(page, "Declare named vector", accept=False)
    assert not any(item["path"].endswith("/vectors/configure") for item in fake.log)
    page.locator("#vector-configure").click()
    confirm(page, "Declare named vector")
    expect(page.locator("#vector-write-result")).to_contain_text('"configured": "text"')
    page.locator("#vector-values-id").fill(MAX_U64)
    values = {"dense": {"text": [{"version": "v1", "vector": [1, 0, 0]}]}, "sparse": {"terms": [[1, .5]]}}
    page.locator("#vector-values").fill('{"dense":[]}')
    before = len(fake.log)
    page.locator("#vector-values-save").click()
    expect(page.locator("#status")).to_contain_text("dense must be a map")
    assert len(fake.log) == before
    page.locator("#vector-values").fill(json.dumps(values))
    page.locator("#vector-values-save").click()
    confirm(page, "Replace ALL named and sparse values", accept=False)
    assert len(fake.log) == before
    page.locator("#vector-values-save").click()
    confirm(page, "Replace ALL named and sparse values")
    expect(page.locator("#vector-write-result")).to_contain_text('"updated": true')
    request = next(item for item in reversed(fake.log) if item["method"] == "PUT" and item["path"].endswith("/named"))
    assert request["path"] == f"/v1/collections/default/vectors/{MAX_U64}/named"
    assert request["body"] == values
    fake.fail_vectors = True
    before = len(fake.log)
    page.locator("#vector-values-save").click()
    confirm(page, "Replace ALL named and sparse values")
    expect(page.locator("#vector-write-result")).to_contain_text("vector_conflict")
    expect(page.locator("#vector-write-result")).to_contain_text("do not retry blindly")
    assert sum(item["method"] == "PUT" for item in fake.log[before:]) == 1
    expect(page.locator("#vector-values")).to_have_value(json.dumps(values))
    fake.fail_vectors = False
    page.locator("#sparse-query").fill("[[1,0.5]]")
    page.locator("#sparse-search").click()
    expect(page.locator("#named-result")).to_contain_text('"distance": -0.5')
    request = next(item for item in reversed(fake.log) if item["path"].endswith("/search/sparse"))
    assert request["body"] == {"name": "terms", "vector": [[1, .5]], "k": 10}
    page.locator("#collection").select_option("cache")
    confirm(page, "advanced retrieval draft", accept=False)
    expect(page.locator("#collection")).to_have_value("default")
    expect(page.locator("#vector-values")).to_have_value(json.dumps(values))
    page.locator("#collection").select_option("cache")
    confirm(page, "advanced retrieval draft")
    expect(page.locator("#vector-values")).to_have_value('{"dense":{},"sparse":{}}')
    expect(page.locator("#vector-write-result")).to_have_text("")
    control(page, "#vector-values").fill(json.dumps(values))
    for width in (1280, 768, 390):
        page.set_viewport_size({"width": width, "height": 900})
        for target in ("key-value", "memory"):
            page.evaluate("id => window.VLiteNavigation.reveal(id)", target)
            assert page.evaluate("document.documentElement.scrollWidth <= innerWidth"), (width, target)
    sign_out(page)
    expect(page.locator("#memory-content")).to_have_value("")
    expect(page.locator("#kv-value")).to_have_value("")
    expect(page.locator("#vector-values")).to_have_value('{"dense":{},"sparse":{}}')
    assert_no_stored_credentials(page, "admin-" + "t" * 32)
    assert not errors, errors
    context.close()
    for role in ("admin", "read_only"):
        fake = WorkspaceApi(role)
        context, page, errors = mocked_page(browser, fake)
        control(page, "#memory-run")
        expect(page.locator("#memory-run")).to_be_disabled()
        if role == "read_only":
            page.locator("#collection").select_option("cache")
            control(page, "#kv-save")
            expect(page.locator("#kv-save")).to_be_disabled()
            expect(page.locator("#kv-get")).to_be_enabled()
            control(page, "#vector-configure")
            expect(page.locator("#vector-configure")).to_be_disabled()
            expect(page.locator("#vector-values-save")).to_be_disabled()
            expect(page.locator("#sparse-search")).to_be_enabled()
            page.locator("#collection").select_option("default")
            control(page, "#embed-text").fill("Read-only query")
            page.locator("#embed-generate").click()
            confirm(page, "Text may leave this server")
            expect(page.locator("#embed-result")).to_contain_text("Query vector ready")
        assert not errors, errors
        context.close()


if __name__ == "__main__":
    with sync_playwright() as p:
        browser = p.chromium.launch()
        exercise_kv_bytes(browser)
        exercise(browser)
        browser.close()
    print("PASS Key-value, MemoryAI and managed providers: contracts, exact revisions, conflict safety, scope gates, receipts, sign-out and responsive layout")
