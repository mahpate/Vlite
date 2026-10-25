"""Exercise production admin UI against deterministic API responses in Chromium.

Run on a hosted runner: python tests/admin_e2e.py.
No Docker, real credentials, server, or database is used.
"""

from playwright.sync_api import expect, sync_playwright

from dashboard_e2e import FakeApi, confirm, control, mocked_page


TENANT = "a" * 32
ADMIN_FEATURES = ("tenants", "tenant_lifecycle", "native_tls", "tls_reload", "credential_rate_limits",
                  "credential_vector_quotas")
EXPORT = "/v1/admin/replication/export"
LIFECYCLE = ("tenant-usage", "tenant-save-settings", "tenant-suspend", "tenant-resume")


class AdminApi(FakeApi):
    def __init__(self, role="admin"):
        super().__init__(role=role)
        self.caps["features"].update({name: True for name in ADMIN_FEATURES})
        self.caps["routes"] = [EXPORT]
        if role == "admin":
            self.caps["identity"] = {"kind": "super_admin", "label": "Super admin (bootstrap admin token)"}
        self.tenants = []
        self.nonempty = False
        self.tls_error = False

    def tenant(self):
        return next((item for item in self.tenants if item["id"] == TENANT), None)

    def api(self, method, path, query, body, headers):
        if path == "/v1/admin/tenants":
            if method == "POST":
                tenant = {"id": TENANT, "state": "active", "deleted": False, **body}
                self.tenants.append(tenant)
                return 201, {"tenant": tenant}, {}
            return 200, {"items": [{**item, "collections": 1, "vectors": 3} for item in self.tenants]}, {}
        prefix = f"/v1/admin/tenants/{TENANT}"
        if path.startswith("/v1/admin/tenants/") and (self.tenant() is None or not path.startswith(prefix)):
            return self.error(404, "tenant_not_found")
        if path == prefix + "/usage" and method == "GET":
            tenant = self.tenant()
            limit = tenant.get("max_vectors")
            return 200, {"tenant": tenant, "collections": [
                {"name": "docs", "physical_name": f"t{TENANT}__docs", "vectors": 3}],
                "vectors": 3, "max_vectors": limit,
                "remaining_vectors": None if limit is None else max(limit - 3, 0),
                "over_quota": limit is not None and 3 > limit,
                "credentials": {"active": 2, "expired": 1, "revoked": 0}}, {}
        if path == prefix + "/settings" and method == "PUT":
            self.tenant().update(body)
            return 200, {"tenant": self.tenant()}, {}
        if path in (prefix + "/suspend", prefix + "/resume") and method == "POST":
            suspended = path.endswith("/suspend")
            self.tenant().update({"state": "suspended" if suspended else "active"})
            if suspended:
                self.tenant()["suspended_unix_ms"] = 1767225600000
            else:
                self.tenant().pop("suspended_unix_ms", None)
            return 200, {"tenant": self.tenant()}, {}
        if path == prefix and method == "DELETE":
            if self.nonempty:
                return self.error(409, "tenant_not_empty")
            self.tenants = []
            return 200, {"deleted": TENANT}, {}
        if path == "/v1/admin/tls/reload":
            return self.error(422, "tls_reload_failed") if self.tls_error else (200, {"reloaded": True}, {})
        if path == EXPORT:
            return 200, {"segment_base64": "AQID", "lineage": "demo",
                         "base_sequence": "0", "last_sequence": "5", "head_sequence": "9"}, {}
        return super().api(method, path, query, body, headers)


def calls(fake, path, method):
    return [item for item in fake.log if item["path"] == path and item["method"] == method]


def exercise(browser):
    fake = AdminApi()
    context, page, errors = mocked_page(browser, fake)
    expect(control(page, "#identity-badge")).to_have_text("Signed in as Super admin (bootstrap admin token)")
    expect(control(page, "#identity-badge")).to_have_attribute("data-identity", "super_admin")
    expect(control(page, "#identity-detail")).to_contain_text("Full administrator")
    assert not calls(fake, "/v1/admin/tenants", "GET"), "Admin actions must not run on connection"
    control(page, "#tenant-list").click()
    expect(control(page, "#tenant-result")).to_contain_text('"count": 0')
    control(page, "#tenant-name").fill("Support")
    control(page, "#tenant-max-vectors").fill("100")
    control(page, "#tenant-create").click()
    confirm(page, "Create tenant", accept=False)
    assert not calls(fake, "/v1/admin/tenants", "POST")
    control(page, "#tenant-create").click()
    confirm(page, "aggregate limit of 100")
    expect(control(page, "#tenant-result")).to_contain_text(TENANT)
    assert calls(fake, "/v1/admin/tenants", "POST")[-1]["body"] == {
        "name": "Support", "max_vectors": 100}
    expect(control(page, "#tenant-id")).to_have_value(TENANT)
    control(page, "#tenant-list").click()
    expect(control(page, "#tenant-result")).to_contain_text('"vectors": 3')
    control(page, "#tenant-usage").click()
    expect(control(page, "#tenant-result")).to_contain_text('"remaining_vectors": 97')
    expect(control(page, "#tenant-result")).to_contain_text('"active": 2')
    expect(control(page, "#tenant-result")).to_contain_text(f"t{TENANT}__docs")
    settings = f"/v1/admin/tenants/{TENANT}/settings"
    control(page, "#tenant-new-name").fill("Support EU")
    control(page, "#tenant-limit-mode").select_option("set")
    control(page, "#tenant-new-limit").fill("2")
    control(page, "#tenant-save-settings").click()
    confirm(page, 'rename it to "Support EU" and set its vector limit to 2', accept=False)
    assert not calls(fake, settings, "PUT")
    control(page, "#tenant-save-settings").click()
    confirm(page, "keeps existing vectors")
    expect(control(page, "#tenant-result")).to_contain_text('"name": "Support EU"')
    assert calls(fake, settings, "PUT")[-1]["body"] == {"name": "Support EU", "max_vectors": 2}
    expect(control(page, "#tenant-new-name")).to_have_value("")
    expect(control(page, "#tenant-limit-mode")).to_have_value("keep")
    expect(control(page, "#tenant-new-limit")).to_have_value("")
    control(page, "#tenant-save-settings").click()
    expect(control(page, "#tenant-result")).to_contain_text("Nothing to change")
    assert len(calls(fake, settings, "PUT")) == 1
    control(page, "#tenant-usage").click()
    expect(control(page, "#tenant-result")).to_contain_text("yes: new vectors are refused")
    suspend = f"/v1/admin/tenants/{TENANT}/suspend"
    control(page, "#tenant-suspend").click()
    confirm(page, "Every API key bound to it is refused", accept=False)
    assert not calls(fake, suspend, "POST")
    control(page, "#tenant-suspend").click()
    confirm(page, "Every API key bound to it is refused")
    expect(control(page, "#tenant-result")).to_contain_text("suspended (API keys blocked)")
    expect(control(page, "#tenant-result")).to_contain_text('"suspended_since"')
    control(page, "#tenant-resume").click()
    confirm(page, "work again immediately")
    expect(control(page, "#tenant-result")).to_contain_text('"state": "active"')
    assert calls(fake, f"/v1/admin/tenants/{TENANT}/resume", "POST")
    control(page, "#tenant-id").fill("b" * 32)
    control(page, "#tenant-usage").click()
    expect(control(page, "#tenant-result")).to_contain_text("No live tenant has this ID")
    control(page, "#tenant-id").fill(TENANT)
    fake.nonempty = True
    control(page, "#tenant-delete").click()
    confirm(page, "cannot undo")
    expect(control(page, "#tenant-result")).to_contain_text("still owns collections")
    fake.nonempty = False
    control(page, "#tenant-delete").click()
    confirm(page, "cannot undo")
    expect(control(page, "#tenant-result")).to_contain_text('"deleted"')
    control(page, "#tls-reload").click()
    confirm(page, "Nothing is uploaded", accept=False)
    assert not calls(fake, "/v1/admin/tls/reload", "POST")
    control(page, "#tls-reload").click()
    confirm(page, "Existing connections")
    expect(control(page, "#tls-result")).to_contain_text('"reloaded": true')
    fake.tls_error = True
    control(page, "#tls-reload").click()
    confirm(page, "Existing connections")
    expect(control(page, "#tls-result")).to_contain_text("previous certificates remain active")
    control(page, "#replication-max-bytes").fill("1024")
    control(page, "#replication-export").click()
    expect(control(page, "#replication-result")).to_contain_text('"more_available": true')
    expect(control(page, "#replication-after")).to_have_value("5")
    with page.expect_download() as download:
        control(page, "#replication-download").click()
    assert download.value.suggested_filename == "vlite-replication-demo-0-5.json"
    assert calls(fake, EXPORT, "GET")[-1]["query"] == "max_bytes=1024"
    for width in (1440, 900, 390, 320):
        page.set_viewport_size({"width": width, "height": 1000})
        for section in ("tenants", "transport-security", "replication", "admin-backups",
                        "admin-auto-maintenance", "admin-backup-schedule"):
            page.evaluate("id => window.VLiteNavigation.reveal(id)", section)
            assert page.evaluate("document.documentElement.scrollWidth <= innerWidth"), (width, section)
    page.evaluate("window.VLiteNavigation.reveal('database')")
    control(page, "#disconnect").click()
    expect(control(page, "#tenant-result")).to_have_text("")
    expect(control(page, "#tls-result")).to_have_text("")
    expect(control(page, "#replication-result")).to_have_text("")
    expect(control(page, "#replication-download")).to_be_disabled()
    assert not errors, errors
    context.close()

    for role in ("read_only", "scoped"):
        fake = AdminApi(role)
        if role == "scoped":
            fake.caps["scope"] = {"collections": ["docs"], "operations": ["read", "search"],
                                 "expires_unix_ms": 9999999999999, "tenant_id": TENANT}
            fake.caps["identity"] = {"kind": "scoped", "label": f'API key "ci" ({"c" * 32}) · tenant "Support"',
                                    "credential_id": "c" * 32, "credential_name": "ci",
                                    "tenant_id": TENANT, "tenant_name": "Support"}
        context, page, errors = mocked_page(browser, fake)
        for name in ("tenant-list", "tenant-create", "tenant-delete", "tls-reload", *LIFECYCLE):
            expect(control(page, "#" + name)).to_be_disabled()
        expect(control(page, "#tenant-result")).to_contain_text("bootstrap admin")
        if role == "scoped":
            expect(control(page, "#identity-badge")).to_have_text('Signed in as API key "ci" (' + "c" * 32 + ') · tenant "Support"')
            expect(control(page, "#identity-detail")).to_contain_text(f'inside tenant "Support" ({TENANT})')
        else:
            expect(control(page, "#identity-badge")).to_contain_text("(this server does not report identity details)")
            expect(control(page, "#identity-badge")).to_have_attribute("data-identity", "read_only")
        assert not errors, errors
        context.close()
    fake = AdminApi()
    fake.caps["features"]["tenant_lifecycle"] = False
    context, page, errors = mocked_page(browser, fake)
    expect(control(page, "#tenant-list")).to_be_enabled()
    for name in LIFECYCLE:
        expect(control(page, "#" + name)).to_be_disabled()
    assert not errors, errors
    context.close()
    fake = AdminApi()
    fake.caps["features"]["tenants"] = False
    fake.caps["features"]["tls_reload"] = False
    context, page, errors = mocked_page(browser, fake)
    expect(control(page, "#tenant-list")).to_be_disabled()
    for name in LIFECYCLE:
        expect(control(page, "#" + name)).to_be_disabled()
    expect(control(page, "#tls-reload")).to_be_disabled()
    expect(control(page, "#tenant-result")).to_contain_text("VLITE_ADMIN_DIR")
    expect(control(page, "#tls-result")).to_contain_text("file-backed TLS")
    assert not errors, errors
    context.close()


if __name__ == "__main__":
    with sync_playwright() as playwright:
        browser = playwright.chromium.launch()
        try:
            exercise(browser)
        finally:
            browser.close()
    print("PASS browser admin flows: identity badge, confirmations/cancel, tenant CRUD/usage/settings/"
          "suspend/resume/errors, TLS reload/errors, replication download/paging, disconnect clearing, "
          "role/configuration/older-server gates and mobile navigation.")
