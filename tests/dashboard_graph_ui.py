"""Graph editor, scope, confirmation, 3D and appearance tests without a backend."""

import re

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
        self.nonempty = {"g", "h"}

    def api(self, method, path, query, body, headers):
        if path == "/v1/graphs":
            if method == "POST":
                self.graphs.append(body["graph"])
                return 201, {"graph": {"name": body["graph"]}}, {}
            return 200, {"graphs": [{"name": name, "node_count": 2 if name in self.nonempty else 0,
                                     "edge_count": 1 if name in self.nonempty else 0} for name in self.graphs]}, {}
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
            if path.rsplit("/", 1)[1] in self.nonempty:
                return self.error(409, "graph_not_empty")
            self.graphs.remove(path.rsplit("/", 1)[1])
            return 200, {"dropped": path.rsplit("/", 1)[1]}, {}
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
    diagram = page.locator("#graph-view")
    diagram.scroll_into_view_if_needed()
    node = diagram.locator('[data-pick-node="18446744073709551615"]')
    circle = node.locator("circle")
    original_x = float(circle.get_attribute("cx"))
    bounds = circle.bounding_box()
    page.mouse.move(bounds["x"] + bounds["width"] / 2, bounds["y"] + bounds["height"] / 2)
    page.mouse.down()
    page.mouse.move(bounds["x"] + bounds["width"] / 2 + 38, bounds["y"] + bounds["height"] / 2 + 24, steps=4)
    page.mouse.up()
    page.wait_for_function("(old) => document.querySelector('[data-pick-node=\"18446744073709551615\"] circle').getAttribute('cx') !== old", arg=str(original_x))
    scene = page.locator("#graph-scene")
    diagram_box = diagram.bounding_box()
    pan_before = scene.get_attribute("transform")
    page.mouse.move(diagram_box["x"] + 12, diagram_box["y"] + 12)
    page.mouse.down()
    page.mouse.move(diagram_box["x"] + 44, diagram_box["y"] + 34, steps=3)
    page.mouse.up()
    expect(scene).not_to_have_attribute("transform", pan_before)
    page.mouse.move(diagram_box["x"] + diagram_box["width"] / 2, diagram_box["y"] + diagram_box["height"] / 2)
    before_wheel = scene.get_attribute("transform")
    page.mouse.wheel(0, -180)
    page.wait_for_timeout(150)
    expect(scene).to_have_attribute("transform", before_wheel)
    diagram.scroll_into_view_if_needed()
    diagram_box = diagram.bounding_box()
    page.mouse.move(diagram_box["x"] + diagram_box["width"] / 2, diagram_box["y"] + diagram_box["height"] / 2)
    page.keyboard.down("Control")
    page.mouse.wheel(0, -180)
    page.keyboard.up("Control")
    expect(page.locator("#graph-scene")).to_have_attribute("transform", re.compile(r"scale\(1\.[0-9]+\)"))
    assert diagram.evaluate("el => getComputedStyle(el).touchAction") == "none"
    page.get_by_role("button", name="Fit / reset view").click()
    expect(page.locator("#graph-scene")).to_have_attribute("transform", "translate(0 0) scale(1)")
    diagram.focus()
    diagram.press("ArrowRight")
    expect(page.locator("#graph-scene")).not_to_have_attribute("transform", "translate(0 0) scale(1)")
    diagram.press("0")
    expect(page.locator("#graph-scene")).to_have_attribute("transform", "translate(0 0) scale(1)")
    node.click()
    expect(node).to_have_class(re.compile(r"is-selected"))
    expect(diagram.locator('[data-pick-edge="1"]')).to_have_class(re.compile(r"is-neighbor"))
    assert diagram.locator("text").evaluate_all("""labels => labels.every(label => {
        for (let node = label; node && node !== label.ownerSVGElement; node = node.parentElement) {
            if (getComputedStyle(node).opacity !== "1") return false;
        }
        return true;
    })""")
    assert len(batches(fake)) == 1, "Graph gestures and selection must not write"
    page.locator("#graph-view-mode").select_option("3d")
    expect(page.locator("#graph-figure-caption")).to_contain_text("not an embedding")
    stage = page.locator("#graph-three-stage")
    stage.scroll_into_view_if_needed()
    expect(stage).to_have_attribute("data-renderer", "three")
    page.wait_for_function("() => Number(document.querySelector('#graph-three-stage').dataset.frames) > 0")
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
    expect(stage).to_have_attribute("data-settled", "true")
    settled = stage.get_attribute("data-frames")
    page.wait_for_timeout(200)
    assert stage.get_attribute("data-frames") == settled, "The settle animation must stop after its bounded duration"
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
    canvas = stage.locator("canvas")
    node_label = stage.locator('[data-node-id="18446744073709551615"]')
    before_drag = node_label.evaluate("node => ({x: node.dataset.pickX, y: node.dataset.pickY})")
    box = stage.bounding_box()
    drag_point = {"x": box["x"] + float(before_drag["x"]), "y": box["y"] + float(before_drag["y"])}
    page.mouse.move(drag_point["x"], drag_point["y"])
    page.mouse.down()
    page.mouse.move(drag_point["x"] + 32, drag_point["y"] + 18, steps=4)
    page.mouse.up()
    page.wait_for_function("(before) => { const n = document.querySelector('[data-node-id=\"18446744073709551615\"]'); return n.dataset.pickX !== before.x || n.dataset.pickY !== before.y; }", arg=before_drag)
    rotated_before = node_label.get_attribute("style")
    page.mouse.move(box["x"] + 18, box["y"] + 18)
    page.mouse.down()
    page.mouse.move(box["x"] + 48, box["y"] + 34, steps=3)
    page.mouse.up()
    page.wait_for_function("(before) => document.querySelector('[data-node-id=\"18446744073709551615\"]').getAttribute('style') !== before", arg=rotated_before)
    zoom_before = node_label.get_attribute("style")
    page.mouse.move(box["x"] + box["width"] / 2, box["y"] + box["height"] / 2)
    assert canvas.evaluate("""el => {
        const event = new WheelEvent("wheel", {deltaY: -150, cancelable: true});
        el.dispatchEvent(event); return !event.defaultPrevented;
    }""")
    expect(node_label).to_have_attribute("style", zoom_before)
    page.keyboard.down("Control")
    page.mouse.wheel(0, -150)
    page.keyboard.up("Control")
    page.wait_for_function("(before) => document.querySelector('[data-node-id=\"18446744073709551615\"]').getAttribute('style') !== before", arg=zoom_before)
    assert len(batches(fake)) == 1, "3D gestures must not write"
    point = node_label.evaluate("node => ({x: Number(node.dataset.pickX), y: Number(node.dataset.pickY)})")
    canvas.click(position=point)
    expect(node_label).to_have_class(re.compile(r"is-selected"))
    assert canvas.evaluate("el => getComputedStyle(el).touchAction") == "none"
    for label in stage.locator(".graph-three-node, .graph-three-edge").all():
        assert label.evaluate("el => getComputedStyle(el).opacity") == "1"
    expect(stage.locator('[data-edge-id="1"]')).to_have_class(re.compile(r"is-neighbor"))
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
    expect(page.locator("#graph-drop-result")).to_contain_text("graph_not_empty")
    assert fake.graphs == ["g", "h"]
    assert len([entry for entry in fake.log if entry["method"] == "DELETE"]) == 1, "Nonempty graphs must not retry"
    fake.nonempty.remove("g")
    page.locator("#graph-drop").click()
    confirm(page, 'Permanently delete empty graph "g"')
    expect(page.locator("#graph-drop-result")).to_contain_text('Deleted graph "g"')
    assert fake.graphs == ["h"]
    assert len([entry for entry in fake.log if entry["method"] == "DELETE"]) == 2
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


def check_graph_only_tenant(browser):
    """A tenant with graphs but no vector collections signs in, lands on Graph and never calls a collection route."""
    for role in ("scoped", "admin"):
        fake = GraphApi(writable=role == "admin")
        if role == "scoped":
            fake.caps.update(role="scoped", scope={
                "collections": ["*"], "graphs": ["g"], "operations": ["read", "search", "graph_read"]})
        fake.collections = {}
        context, page, errors = mocked_page(browser, fake)
        expect(page.locator("#workspace-description")).to_contain_text("confirmed edits")
        expect(page.locator("#graph-controls")).to_be_visible()
        assert page.evaluate("location.hash") == "#graph"
        expect(page.locator("#collection-badge")).to_have_text("No vector collections")
        expect(page.locator("#collection")).to_be_disabled()
        expect(page.locator("#collection-note")).to_contain_text("No vector collections in this tenant yet")
        page.locator("#graph-operation").select_option("find-nodes")
        page.locator("#graph-run").click()
        expect(page.locator("#graph-nodes tbody tr")).to_have_count(2)
        for section in ("search", "browse", "editor", "index"):
            page.evaluate(f"window.VLiteNavigation.reveal('{section}')")
            note = page.locator(f"#{section} [data-vector-empty]")
            expect(note).to_be_visible()
            expect(note).to_contain_text("No vector collections in this tenant yet")
            enabled = page.locator(f"#{section}").evaluate(
                "s => [...s.querySelectorAll('button, input, select, textarea')].filter(e => !e.disabled).map(e => e.id || e.name)")
            assert enabled == [], (section, enabled)
            for width in (1280, 768, 390):
                page.set_viewport_size({"width": width, "height": 900})
                assert page.evaluate("document.documentElement.scrollWidth <= innerWidth"), (section, width)
            page.set_viewport_size({"width": 1280, "height": 900})
        page.evaluate("window.VLiteNavigation.reveal('database')")
        expect(page.locator("#stat-records")).to_have_text("0")
        page.locator("#session-refresh").click()
        expect(page.locator("#status")).to_have_text("Connected; overview refreshed")
        assert not any(entry["path"].startswith("/v1/collections/") for entry in fake.log), \
            [entry["path"] for entry in fake.log if entry["path"].startswith("/v1/collections/")]

        # Once a collection exists, Refresh restores the vector views.
        fake.collections = {"docs": {"id": 1, "name": "docs", "dimensions": 2, "live_records": 1,
                                     "settings": GraphApi().collections["docs"]["settings"], "settings_persisted": True}}
        page.locator("#session-refresh").click()
        expect(page.locator("#status")).to_have_text("Connected; overview refreshed")
        expect(page.locator("#collection-badge")).to_have_text("Collection: docs")
        page.evaluate("window.VLiteNavigation.reveal('search')")
        expect(page.locator("#search [data-vector-empty]")).to_be_hidden()
        expect(page.locator("#query")).to_be_enabled()
        assert not errors, errors
        context.close()


PHONE = {"width": 390, "height": 844}
FINGER = 120  # CSS pixels a finger travels: far past the touch slop yet still inside the canvas
FIRST_NODE, SECOND_NODE = "18446744073709551615", "2"


class PhoneBrowser:
    """Lets mocked_page open a phone-sized touch context without changing its signature."""

    def __init__(self, browser):
        self.browser = browser

    def new_context(self, **options):
        return self.browser.new_context(**{**options, "viewport": PHONE, "has_touch": True, "is_mobile": True})


def finger_drag(page, cdp, start, dy, steps=10):
    """One finger goes down at start, travels dy pixels down in small steps and lifts.

    Playwright has no touch drag, so the events go through CDP into Chromium's real input
    pipeline: touch-action decides whether the page scrolls or the page's pointer handlers
    keep the gesture, exactly as for a finger on a phone."""
    def touch(kind, y=None):
        cdp.send("Input.dispatchTouchEvent", {"type": kind, "touchPoints": [] if y is None else [{"x": start[0], "y": y, "id": 1}]})
    touch("touchStart", start[1])
    for step in range(1, steps + 1):
        touch("touchMove", start[1] + dy * step / steps)
        page.wait_for_timeout(20)
    page.wait_for_timeout(150)  # resting before the lift avoids momentum scrolling
    touch("touchEnd")


def settled_scroll(page):
    """Page and graph-section scroll offsets once any smooth or momentum scrolling has stopped."""
    return page.evaluate("""() => new Promise(resolve => {
        const section = document.getElementById("graph");
        const read = () => [scrollX, scrollY, section.scrollLeft, section.scrollTop];
        let last = read().join(), calm = 0;
        const poll = () => {
            const now = read().join();
            calm = now === last ? calm + 1 : 0;
            last = now;
            if (calm >= 4) resolve(read()); else setTimeout(poll, 50);
        };
        setTimeout(poll, 50);
    })""")


def center_on(page, locator):
    """Scroll an element to mid-screen so the page can scroll a long way both up and down."""
    locator.evaluate("el => el.scrollIntoView({block: 'center'})")
    offset = settled_scroll(page)[1]
    room = page.evaluate("document.scrollingElement.scrollHeight - innerHeight")
    assert 2 * FINGER < offset < room - 2 * FINGER, (offset, room)


def center_of(locator):
    box = locator.bounding_box()
    return box["x"] + box["width"] / 2, box["y"] + box["height"] / 2


def hits(page, point, selector):
    """Whether a touch at the point lands on an element that matches the selector, or inside one."""
    return page.evaluate("([x, y, selector]) => !!document.elementFromPoint(x, y)?.closest(selector)", [*point, selector])


def drag_keeps_page(page, cdp, start, dy, what):
    """A finger drag that starts on the graph must not scroll the page."""
    before = settled_scroll(page)
    finger_drag(page, cdp, start, dy)
    after = settled_scroll(page)
    assert after == before, f"A vertical finger drag that starts on {what} scrolled the page: {before} -> {after}"
    return after


def touch_graph(browser, view):
    """A phone-sized touch session showing the mocked graph result as the "2d" diagram or the "3d" graph."""
    context, page, errors = mocked_page(PhoneBrowser(browser), GraphApi())
    page.evaluate("window.VLiteNavigation.reveal('graph')")
    page.locator("#graph-operation").select_option("find-nodes")
    page.locator("#graph-run").click()
    expect(page.locator("#graph-nodes tbody tr")).to_have_count(2)
    if view == "3d":
        page.locator("#graph-view-mode").select_option("3d")
        stage = page.locator("#graph-three-stage")
        expect(stage).to_have_attribute("data-renderer", "three")
        center_on(page, stage)
        expect(stage).to_have_attribute("data-settled", "true")
    return context, page, context.new_cdp_session(page), errors


def check_touch_2d(browser):
    """A finger drag that starts on the 2D diagram moves a node or pans the view and never scrolls the page."""
    context, page, cdp, errors = touch_graph(browser, "2d")
    diagram = page.locator("#graph-view")
    first = diagram.locator(f'[data-pick-node="{FIRST_NODE}"] circle')
    second = diagram.locator(f'[data-pick-node="{SECOND_NODE}"] circle')
    expect(page.locator(".graph-gesture-help")).to_contain_text("on touch screens, drag outside the graph to scroll the page")
    center_on(page, diagram)

    # Starting on a node drags that node and leaves the rest of the diagram where it was.
    first_at, second_at = center_of(first), center_of(second)
    assert hits(page, first_at, f'[data-pick-node="{FIRST_NODE}"]')
    scroll = drag_keeps_page(page, cdp, first_at, FINGER // 2, "a node")
    moved = center_of(first)
    assert abs(moved[0] - first_at[0]) <= 3 and abs(moved[1] - first_at[1] - FINGER // 2) <= 3, ("The node must follow the finger", first_at, moved)
    assert max(abs(a - b) for a, b in zip(center_of(second), second_at)) <= 1, "Only the touched node may move"

    # Starting on empty canvas pans the whole diagram with the finger.
    box = diagram.bounding_box()
    empty = (box["x"] + 30, box["y"] + 100)
    assert hits(page, empty, "#graph-view") and not hits(page, empty, "[data-pick-node], [data-pick-edge]")
    before = center_of(second)
    drag_keeps_page(page, cdp, empty, FINGER, "the canvas")
    after = center_of(second)
    assert abs(after[0] - before[0]) <= 3 and abs(after[1] - before[1] - FINGER) <= 3, ("The diagram must follow the finger", before, after)

    # Control: the same drag that starts outside the canvas scrolls the page and does not touch the graph.
    outside = center_of(page.locator("#graph-figure-caption"))
    assert hits(page, outside, "#graph-figure-caption")
    drawing = diagram.evaluate("el => el.innerHTML")
    finger_drag(page, cdp, outside, FINGER)
    scrolled = settled_scroll(page)
    assert scrolled[1] <= scroll[1] - FINGER // 2, ("A finger drag that starts outside the canvas must scroll the page", scroll, scrolled)
    assert diagram.evaluate("el => el.innerHTML") == drawing, "Scrolling the page must not move the graph"

    # Over the canvas the mouse wheel still scrolls the page and does not zoom.
    center_on(page, diagram)
    scroll = settled_scroll(page)
    box = diagram.bounding_box()
    page.mouse.move(box["x"] + 30, box["y"] + 100)
    page.mouse.wheel(0, FINGER)
    page.wait_for_timeout(300)
    assert settled_scroll(page)[1] >= scroll[1] + FINGER // 2, "The wheel must scroll the page over the canvas"
    assert diagram.evaluate("el => el.innerHTML") == drawing, "A plain wheel must not zoom or pan the diagram"
    assert not errors, errors
    context.close()


def check_touch_3d(browser):
    """A finger drag that starts on the 3D canvas moves a node or turns the graph and never scrolls the page."""
    context, page, cdp, errors = touch_graph(browser, "3d")
    stage = page.locator("#graph-three-stage")
    labels = stage.locator(".graph-three-labels")
    node = stage.locator(f'[data-node-id="{FIRST_NODE}"]')

    def node_at():
        return node.evaluate("el => [Number(el.dataset.pickX), Number(el.dataset.pickY)]")

    def turn():
        return [int(page.locator(f"#graph-{name}").input_value()) for name in ("yaw", "pitch")]

    # Starting on a node drags that node, which follows the finger.
    box = stage.bounding_box()
    node_before = node_at()
    start = (box["x"] + node_before[0], box["y"] + node_before[1])
    assert hits(page, start, "#graph-three-stage canvas")
    scroll = drag_keeps_page(page, cdp, start, FINGER // 2, "a node")
    page.wait_for_function("([id, y]) => Math.abs(Number(document.querySelector(`[data-node-id='${id}']`).dataset.pickY) - y) <= 4",
                           arg=[FIRST_NODE, node_before[1] + FINGER // 2], timeout=5000)
    assert abs(node_at()[0] - node_before[0]) <= 4, ("The node must follow the finger", node_before, node_at())

    # Starting on empty canvas turns the graph, as a mouse drag does.
    empty = (box["x"] + 18, box["y"] + 18)
    assert hits(page, empty, "#graph-three-stage canvas")
    yaw, pitch = turn()
    drag_keeps_page(page, cdp, empty, FINGER, "the canvas")
    assert turn()[1] - pitch >= 40 and turn()[0] == yaw, ("The graph must turn with the finger", (yaw, pitch), turn())

    # Control: the same drag that starts outside the canvas scrolls the page and does not touch the graph.
    outside = center_of(page.locator("#graph-figure-caption"))
    assert hits(page, outside, "#graph-figure-caption")
    turned, drawing = turn(), labels.evaluate("el => el.innerHTML")
    finger_drag(page, cdp, outside, FINGER)
    scrolled = settled_scroll(page)
    assert scrolled[1] <= scroll[1] - FINGER // 2, ("A finger drag that starts outside the canvas must scroll the page", scroll, scrolled)
    assert turn() == turned and labels.evaluate("el => el.innerHTML") == drawing, "Scrolling the page must not turn the graph"

    # Over the canvas the mouse wheel still scrolls the page and does not zoom.
    center_on(page, stage)
    scroll = settled_scroll(page)
    box = stage.bounding_box()
    page.mouse.move(box["x"] + 18, box["y"] + 18)
    page.mouse.wheel(0, FINGER)
    page.wait_for_timeout(300)
    assert settled_scroll(page)[1] >= scroll[1] + FINGER // 2, "The wheel must scroll the page over the canvas"
    assert labels.evaluate("el => el.innerHTML") == drawing, "A plain wheel must not zoom the graph"
    assert not errors, errors
    context.close()


if __name__ == "__main__":
    with sync_playwright() as p:
        browser = p.chromium.launch(args=["--enable-webgl", "--use-gl=angle", "--use-angle=swiftshader", "--enable-unsafe-swiftshader"])
        try:
            check(browser)
            check_graph_only_tenant(browser)
            check_touch_2d(browser)
            check_touch_3d(browser)
        finally:
            browser.close()
    print("Graph editing, revision conflicts, read-only scope, confirmation invalidation, responsive 3D, touch drags and graph-only tenants passed.")
