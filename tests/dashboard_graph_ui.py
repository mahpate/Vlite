"""Graph editor, scope, confirmation, 3D and appearance tests without a backend."""

from playwright.sync_api import expect, sync_playwright

from dashboard_e2e import MOCK_ORIGIN, FakeApi, confirm, control, mocked_page


class GraphApi(FakeApi):
    def __init__(self, writable=True):
        super().__init__(format_version=5)
        self.caps["features"]["graph"] = True
        if not writable:
            self.caps.update(role="scoped", scope={
                "collections": ["*"], "graphs": ["g"], "operations": ["read", "search", "graph_read"]})
        self.conflict = False
        self.graphs = ["g", "h"]

    def api(self, method, path, query, body, headers):
        if path == "/v1/graphs":
            if method == "POST":
                self.graphs.append(body["graph"])
                return 201, {"graph": {"name": body["graph"]}}, {}
            return 200, {"graphs": [{"name": name, "node_count": 2, "edge_count": 1} for name in self.graphs]}, {}
        if path == "/v1/graphs/g/nodes/find":
            return 200, {"nodes": [
                {"id": "18446744073709551615", "key": "n", "labels": ["Doc"], "properties": {}, "revision": "7"},
                {"id": "2", "key": "m", "labels": [], "properties": {}, "revision": "3"}],
                "edges": [{"id": "1", "key": "e", "source": "18446744073709551615", "target": "2", "relationship": "LINKS", "revision": "5"},
                          {"id": "2", "key": "loop", "source": "2", "target": "2", "relationship": "SELF", "revision": "5"}],
                "complete": True}, {}
        if path == "/v1/graphs/g/batch":
            if self.conflict:
                return self.error(409, "graph_revision_conflict")
            return 200, {"revision": "8", "results": []}, {}
        if method == "DELETE" and path in ["/v1/graphs/" + name for name in self.graphs]:
            self.graphs.remove(path.rsplit("/", 1)[1])
            return 204, None, {}
        return super().api(method, path, query, body, headers)


def batches(fake):
    return [entry for entry in fake.log if entry["path"].endswith("/batch")]


def check(browser):
    fake = GraphApi()
    context, page, errors = mocked_page(browser, fake)
    page.get_by_role("link", name="Graph", exact=True).click()
    expect(page.locator("#workspace-description")).to_contain_text("confirmed edits")
    expect(page.locator("#graph-editor")).to_be_visible()
    page.locator("#graph-create-name").fill("draft_graph")
    page.locator("#graph-create-save").click()
    confirm(page, "Create empty graph", accept=False)
    assert fake.graphs == ["g", "h"]
    page.locator("#graph-create-save").click()
    confirm(page, "Create empty graph")
    expect(page.locator("#graph-name")).to_have_value("draft_graph")
    assert fake.graphs == ["g", "h", "draft_graph"]
    page.locator("#graph-name").select_option("g")
    page.locator("#graph-editor summary").click()
    page.locator("#graph-edit-key").fill("draft")
    page.locator("#graph-add-property").click()
    row = page.locator(".graph-property-row")
    row.get_by_label("Name", exact=True).fill("rank")
    row.get_by_role("combobox", name="Type", exact=True).select_option("integer")
    row.get_by_label("Value", exact=True).fill("9007199254740993")
    page.locator("#graph-edit-save").click()
    confirm(page, "Add node", accept=False)
    assert not batches(fake)
    page.locator("#graph-edit-save").click()
    confirm(page, "9007199254740993")
    expect(page.locator("#graph-edit-result")).to_contain_text("Edit committed")
    assert batches(fake)[0]["body"] == {"ops": [{"op": "create_node", "key": "draft", "labels": [],
                                               "properties": {"rank": {"type": "integer", "value": "9007199254740993"}}}]}
    page.locator("#graph-editor summary").click()
    page.locator("#graph-operation").select_option("find-nodes")
    page.locator("#graph-run").click()
    expect(page.locator("#graph-nodes tbody tr")).to_have_count(2)
    expect(page.locator("#graph-view .graph-edge")).to_have_count(2)
    expect(page.locator("#graph-view .graph-edge path")).to_have_count(1)
    page.locator("#graph-view-mode").select_option("3d")
    expect(page.locator("#graph-figure-caption")).to_contain_text("not an embedding")
    stage = page.locator("#graph-three-stage")
    stage.scroll_into_view_if_needed()
    expect(stage).to_have_attribute("data-renderer", "three")
    page.wait_for_function("Number(document.querySelector('#graph-three-stage').dataset.frames) > 0")
    expect(stage.locator(".graph-three-node")).to_have_count(2)
    expect(stage.locator(".graph-three-edge")).to_have_count(2)
    before = stage.locator(".graph-three-node").first.get_attribute("style")
    reads = len(fake.log)
    page.locator("#graph-yaw").focus()
    page.locator("#graph-yaw").press("ArrowRight")
    page.wait_for_function("(old) => document.querySelector('.graph-three-node').getAttribute('style') !== old", arg=before)
    assert len(fake.log) == reads, "Rotating must not query or write"
    stage.scroll_into_view_if_needed()
    start = int(stage.get_attribute("data-frames"))
    page.wait_for_timeout(1100)
    assert 1 <= int(stage.get_attribute("data-frames")) - start <= 28, "Rendering must be bounded to 24 fps"
    page.get_by_role("link", name="Overview", exact=True).click()
    expect(stage).to_have_attribute("data-paused", "true")
    paused = stage.get_attribute("data-frames")
    page.wait_for_timeout(200)
    assert stage.get_attribute("data-frames") == paused, "Hidden workspaces must not render"
    page.get_by_role("link", name="Graph", exact=True).click()
    stage.scroll_into_view_if_needed()
    expect(stage).to_have_attribute("data-paused", "false")
    page.emulate_media(reduced_motion="reduce")
    page.wait_for_timeout(200)
    stage.scroll_into_view_if_needed()
    page.wait_for_timeout(100)
    stationary = stage.get_attribute("data-frames")
    page.wait_for_timeout(200)
    assert stage.get_attribute("data-frames") == stationary
    painted = stage.screenshot()
    stage.locator("canvas").evaluate("canvas => canvas.style.visibility = 'hidden'")
    assert stage.screenshot() != painted, "three.js must contribute visible pixels, not just an empty canvas"
    stage.locator("canvas").evaluate("canvas => canvas.style.visibility = ''")
    point = stage.locator('[data-node-id="18446744073709551615"]').evaluate(
        "node => ({x: Number(node.dataset.pickX), y: Number(node.dataset.pickY)})")
    stage.locator("canvas").click(position=point)
    expect(page.locator("#graph-edit-key")).to_have_value("n")
    expect(page.locator("#graph-edit-revision")).to_have_value("7")
    # Picking supplies exact revision-checked editor values; it never writes.
    stage.locator('[data-edge-id="1"]').click()
    expect(page.locator("#graph-edit-action")).to_have_value("patch_edge")
    expect(page.locator("#graph-edit-key")).to_have_value("e")
    expect(page.locator("#graph-edit-revision")).to_have_value("5")
    page.locator("#graph-selected-delete").click()
    expect(page.locator("#graph-edit-action")).to_have_value("delete_edge")
    expect(page.locator("#graph-edit-revision")).to_have_value("5")
    page.locator("#graph-connect-nodes").click()
    stage.locator('[data-node-id="18446744073709551615"]').click()
    expect(page.locator("#graph-pick-note")).to_contain_text('Source "n"')
    stage.locator('[data-node-id="2"]').click()
    expect(page.locator("#graph-edit-action")).to_have_value("create_edge")
    expect(page.locator("#graph-edit-source")).to_have_value("n")
    expect(page.locator("#graph-edit-destination")).to_have_value("m")
    assert len(batches(fake)) == 1, "Selecting nodes, edges and endpoints must not mutate"
    for theme in ("light", "dark"):
        page.evaluate("(theme) => document.documentElement.dataset.theme = theme", theme)
        for width in (1280, 768, 390):
            page.set_viewport_size({"width": width, "height": 900})
            assert page.evaluate("document.documentElement.scrollWidth <= innerWidth"), (theme, width)
    page.set_viewport_size({"width": 1280, "height": 900})
    page.locator("#graph-nodes").get_by_role("button", name="Edit", exact=True).first.click()
    expect(page.locator("#graph-edit-key")).to_have_value("n")
    expect(page.locator("#graph-edit-revision")).to_have_value("7")
    fake.conflict = True
    page.locator("#graph-edit-save").click()
    confirm(page, '"revision": "7"')
    expect(page.locator("#graph-edit-result")).to_contain_text("graph_revision_conflict")
    assert len(batches(fake)) == 2, "Conflict must not retry"
    expect(page.locator("#graph-edit-revision")).to_have_value("7")
    # A graph change during review must invalidate the captured write.
    page.locator("#graph-edit-save").click()
    expect(page.locator("#confirmation")).to_be_visible()
    page.evaluate("""() => {
        const graph = document.getElementById('graph-name');
        graph.value = 'h'; graph.dispatchEvent(new Event('change'));
    }""")
    confirm(page, "graph", accept=True)
    expect(page.locator("#graph-edit-result")).to_contain_text("no edit sent")
    assert len(batches(fake)) == 2
    # Both surfaces consume the same theme key; cross-tab changes update the live dashboard.
    page.evaluate("""() => window.dispatchEvent(new StorageEvent('storage',
        {key: 'vlite-theme', newValue: 'dark'}))""")
    expect(page.locator("html")).to_have_attribute("data-theme", "dark")
    expect(page.locator("#theme")).to_have_value("dark")
    page.evaluate("document.fonts.ready")
    assert page.evaluate("document.fonts.check('14px Outfit')")
    assert page.evaluate("document.fonts.check('13px \"JetBrains Mono\"')")
    control(page, "#remember-preferences").check()
    control(page, "#theme").select_option("light")
    assert page.evaluate("localStorage.getItem('vlite-theme')") == "light"
    control(page, "#theme").select_option("system")
    assert page.evaluate("localStorage.getItem('vlite-theme')") is None
    assert not errors, errors
    context.close()

    # Explicit graph deletion is permission-gated, cancellable and narrowly routed.
    fake = GraphApi()
    context, page, errors = mocked_page(browser, fake)
    page.get_by_role("link", name="Graph", exact=True).click()
    expect(page.locator("#graph-drop")).to_be_visible()
    page.locator("#graph-drop").click()
    confirm(page, "Permanently delete", accept=False)
    assert fake.graphs == ["g", "h"]
    page.locator("#graph-drop").click()
    confirm(page, "no revision precondition")
    expect(page.locator("#graph-drop-result")).to_contain_text('Deleted graph "g"')
    assert fake.graphs == ["h"]
    assert len([entry for entry in fake.log if entry["method"] == "DELETE"]) == 1
    response = page.evaluate("window.VLiteGraphConnection.request('DELETE', '/v1/graphs/h/batch')")
    assert response["value"]["error"]["code"] == "unsupported_route"
    assert not errors, errors
    context.close()

    # A pending mutation locks its draft and invalidates reads admitted before its commit.
    fake = GraphApi()
    context, page, errors = mocked_page(browser, fake)
    page.get_by_role("link", name="Graph", exact=True).click()
    page.locator("#graph-operation").select_option("find-nodes")
    page.locator("#graph-run").click()
    expect(page.locator("#graph-nodes tbody tr")).to_have_count(2)
    reads, writes = [], []
    context.route(MOCK_ORIGIN + "/v1/graphs/g/nodes/find", lambda route: reads.append(route))
    context.route(MOCK_ORIGIN + "/v1/graphs/g/batch", lambda route: writes.append(route))
    page.locator("#graph-run").click()
    page.locator("#graph-nodes").get_by_role("button", name="Edit", exact=True).first.click()
    page.locator("#graph-edit-save").click()
    confirm(page, '"revision": "7"')
    expect(page.locator("#graph-editor")).to_have_attribute("inert", "")
    expect(page.locator("#graph-create-save")).to_be_disabled()
    assert len(writes) == len(reads) == 1
    writes[0].fulfill(status=200, json={"revision": "8"})
    expect(page.locator("#graph-edit-result")).to_contain_text("Edit committed")
    reads[0].fulfill(status=200, json={"nodes": [{"id": "old", "key": "stale"}], "complete": True})
    expect(page.locator("#graph-run")).to_be_enabled()
    expect(page.locator("#graph-summary")).to_contain_text("Run a fresh query")
    expect(page.locator('[data-graph-table="graph-nodes"]')).to_be_hidden()
    assert not page.locator("#graph-editor").evaluate("e => e.inert")
    assert not errors, errors
    context.close()

    context, page, errors = mocked_page(browser, GraphApi(writable=False))
    page.get_by_role("link", name="Graph", exact=True).click()
    expect(page.locator("#graph-controls")).to_be_visible()
    expect(page.locator("#graph-editor")).to_be_hidden()
    expect(page.locator("#graph-create")).to_be_hidden()
    expect(page.locator("#graph-drop")).to_be_hidden()
    page.locator("#graph-operation").select_option("find-nodes")
    page.locator("#graph-run").click()
    expect(page.locator("#graph-nodes").get_by_role("button", name="Edit", exact=True).first).to_be_disabled()
    assert not errors, errors
    context.close()

    # Progressive fallback for both no WebGL and Save-Data keeps the real diagram/tables.
    for fallback in ("no-webgl", "save-data"):
        context, page, errors = mocked_page(browser, GraphApi())
        if fallback == "no-webgl":
            page.evaluate("""() => {
                const original = HTMLCanvasElement.prototype.getContext;
                HTMLCanvasElement.prototype.getContext = function(kind, ...args) {
                    return kind === 'webgl2' ? null : original.call(this, kind, ...args);
                };
            }""")
        else:
            page.evaluate("Object.defineProperty(navigator, 'connection', {value: {saveData: true}, configurable: true})")
        page.get_by_role("link", name="Graph", exact=True).click()
        page.locator("#graph-operation").select_option("find-nodes")
        page.locator("#graph-run").click()
        page.locator("#graph-view-mode").select_option("3d")
        expect(page.locator("#graph-figure-caption")).to_contain_text("3D unavailable")
        expect(page.locator("#graph-view")).to_be_visible()
        expect(page.locator("#graph-nodes tbody tr")).to_have_count(2)
        assert not errors, errors
        context.close()


if __name__ == "__main__":
    with sync_playwright() as p:
        browser = p.chromium.launch(args=["--enable-webgl", "--use-gl=angle", "--use-angle=swiftshader", "--enable-unsafe-swiftshader"])
        try:
            check(browser)
        finally:
            browser.close()
    print("Graph editing, revision conflicts, read-only scope, confirmation invalidation and responsive 3D passed.")
