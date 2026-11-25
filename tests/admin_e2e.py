"""Exercise production admin UI against deterministic API responses in Chromium.

Run on a hosted runner: python tests/admin_e2e.py.
No Docker, real credentials, server, or database is used.
"""

import hashlib
import json
import re
import time
from urllib.parse import quote

from playwright.sync_api import expect, sync_playwright

from dashboard_e2e import (MOCK_ORIGIN, FakeApi, confirm, control, expect_signed_out, mocked_page,
                           sign_in_with_token, sign_out, visible_state)


TENANT = "a" * 32
ADMIN_FEATURES = ("tenants", "tenant_lifecycle", "native_tls", "tls_reload", "credential_rate_limits",
                  "credential_vector_quotas")
EXPORT = "/v1/admin/replication/export"
LIFECYCLE = ("tenant-usage", "tenant-save-settings", "tenant-suspend", "tenant-resume")
USER_CONTROLS = ("user-list", "user-create", "user-save", "user-reset-password", "user-delete")
ANN = "d" * 32
BOB = "e" * 32
FIRST = "correct horse battery staple"
SECOND = "violet harbor lantern quietly"
ROLE_LABELS = {"super_admin": "Super admin", "editor": "Editor", "viewer": "Viewer"}
CREATED = 1767225600000


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


class PasswordApi(AdminApi):
    """Password sessions for "ann", a super admin. Server-side state stands in for the HttpOnly cookie."""

    def __init__(self, config=None):
        super().__init__()
        self.caps["features"].update({"accounts": True, "password_login": True})
        self.config = {"mode": "protected", "password_login": True, "reason": None, "setup_required": False, **(config or {})}
        self.password = FIRST
        self.must_change = True
        self.active = False
        self.throttled = False
        self.users = {ANN: self.account(ANN, "ann", "super_admin")}

    @staticmethod
    def account(user_id, username, role, **fields):
        return {"id": user_id, "username": username, "role": role, "role_label": ROLE_LABELS[role], "tenant_id": None,
                "tenant_name": None, "disabled": False, "must_change_password": False,
                "created_unix_ms": CREATED, "updated_unix_ms": CREATED, **fields}

    def signed_in(self):
        return {"user": {**self.users[ANN], "must_change_password": self.must_change},
                "must_change_password": self.must_change, "idle_timeout_seconds": 1800, "expires_unix_ms": CREATED + 43200000}

    def credentialed(self, headers):
        # A session counts once its password is settled; a temporary one is answered password_change_required.
        return (self.active and not self.must_change) or super().credentialed(headers)

    def api(self, method, path, query, body, headers):
        if path == "/v1/auth/config":
            return 200, self.config, {}
        if path.startswith("/v1/auth/") and method != "GET" and headers.get("x-vlite-request") != "1":
            return self.error(403, "csrf_rejected")
        if path == "/v1/auth/login" and method == "POST":
            if self.throttled:
                return 429, {"error": {"code": "too_many_attempts", "message": "too many attempts"}}, {"retry-after": "30"}
            if body != {"username": "ann", "password": self.password}:
                return self.error(401, "invalid_credentials")
            self.active = True
            return 200, self.signed_in(), {}
        if path == "/v1/auth/logout" and method == "POST":
            self.active = False
            return 200, {"signed_out": True}, {}
        if "authorization" in headers:
            return super().api(method, path, query, body, headers)
        if not self.active:
            return self.error(401, "unauthorized")
        if path == "/v1/auth/session":
            return 200, {"method": "password", "role": "admin", "accounts": True, "setup_required": False, **self.signed_in()}, {}
        if path == "/v1/auth/password" and method == "POST":
            if body["current_password"] != self.password:
                return self.error(403, "wrong_password")
            self.password, self.must_change = body["new_password"], False
            return 200, self.signed_in(), {}
        if self.must_change and path != "/v1/capabilities":
            return self.error(403, "password_change_required")
        if path == "/v1/capabilities":
            return 200, {**self.caps, "identity": {
                "kind": "user", "label": "ann · Super admin", "user_id": ANN, "username": "ann", "account_role": "super_admin",
                "tenant_id": None, "tenant_name": None, "must_change_password": self.must_change}}, {}
        if path == "/v1/admin/users":
            if method == "POST":
                self.users[BOB] = self.account(BOB, body["username"], body["role"], tenant_id=body.get("tenant_id"),
                                               disabled=body["disabled"], must_change_password=body["must_change_password"])
                return 201, {"user": self.users[BOB]}, {}
            return 200, {"items": list(self.users.values()), "capacity": 1024}, {}
        if path.startswith("/v1/admin/users/"):
            user_id = path.split("/")[4]
            if user_id == ANN:
                return self.error(409, "cannot_modify_self")
            user = self.users.get(user_id)
            if user is None:
                return self.error(404, "user_not_found")
            if method == "PUT":
                user.update(body)
                user["role_label"] = ROLE_LABELS[user["role"]]
                return 200, {"user": user, "changed": sorted(body)}, {}
            if method == "POST" and path.endswith("/password"):
                user["must_change_password"] = body["must_change_password"]
                return 200, {"user": user}, {}
            if method == "DELETE":
                del self.users[user_id]
                return 200, {"deleted": True}, {}
        # Past the session checks, the token-mode fake serves every other route.
        return super().api(method, path, query, body, {**headers, "authorization": "Bearer session-cookie"})


def calls(fake, path, method):
    return [item for item in fake.log if item["path"] == path and item["method"] == method]


def session_page(browser, fake):
    """Load the dashboard without connecting, like someone about to sign in."""
    context = browser.new_context(viewport={"width": 1280, "height": 900})
    context.route(MOCK_ORIGIN + "/**", fake.handle)
    page = context.new_page()
    page.set_default_timeout(30000)
    errors = []
    page.on("pageerror", lambda error: errors.append(str(error)))
    # The page-load session probe answers 401 when nobody is signed in; Chromium logs it as a failed resource.
    page.on("console", lambda message: errors.append(message.text)
            if message.type == "error" and "Failed to load resource" not in message.text else None)
    page.goto(MOCK_ORIGIN + "/dashboard")
    return context, page, errors


def sign_in(page, username, password):
    control(page, "#username").fill(username)
    control(page, "#password").fill(password)
    control(page, "#sign-in").get_by_role("button", name="Sign in").click()


def change_password(page, current, new, repeat):
    control(page, "#current-password").fill(current)
    control(page, "#new-password").fill(new)
    control(page, "#repeat-password").fill(repeat)
    control(page, "#change-password").get_by_role("button", name="Change password").click()


def token_form_open(page):
    return page.locator("#token-sign-in").evaluate("details => details.open")


def open_user_menu(page):
    """Opens the account menu from its button in the top bar."""
    menu = page.locator("#user-menu")
    if not menu.evaluate("details => details.open"):
        menu.locator("summary").click()
    expect(menu.locator(".user-menu-panel")).to_be_visible()


def close_user_menu(page):
    page.locator("#user-menu summary").press("Escape")
    expect(page.locator("#user-menu .user-menu-panel")).to_be_hidden()
    expect(page.locator("#user-menu summary")).to_be_focused()


def expect_gate_message(page, text):
    """The sign-in screen repeats the status line while the dashboard is hidden."""
    expect(page.locator("#gate-message")).to_have_text(text)
    expect(page.locator("#gate-message")).to_be_visible()


def exercise_password(browser):
    fake = PasswordApi()
    context, page, errors = session_page(browser, fake)
    expect_signed_out(page)
    expect(page.locator("#sign-in")).to_be_visible()
    expect(page.locator("#token-sign-in > summary")).to_be_visible()
    assert token_form_open(page) is False, "With password sign-in available, the token form starts collapsed"
    expect(page.locator("#sign-in-setup")).to_be_hidden()
    expect(page.locator("#gate-retry")).to_be_hidden()
    expect(page.locator("#gate-sign-out")).to_be_hidden()
    expect(page.locator("#identity-badge")).to_have_text("Not signed in")
    fake.throttled = True
    sign_in(page, "ann", FIRST)
    expect(control(page, "#status")).to_have_text("Too many sign-in attempts. Try again in 30 seconds.")
    expect_gate_message(page, "Too many sign-in attempts. Try again in 30 seconds.")
    expect(control(page, "#error-help")).to_have_text("")
    expect(page.locator("#gate-help")).to_be_hidden()
    expect(control(page, "#password")).to_have_value("")
    fake.throttled = False
    sign_in(page, "ann", "not the right password")
    expect(control(page, "#status")).to_have_text("Wrong username or password.")
    expect_gate_message(page, "Wrong username or password.")
    expect(page.locator("#app-shell")).to_be_hidden()
    login = calls(fake, "/v1/auth/login", "POST")[-1]
    assert login["body"] == {"username": "ann", "password": "not the right password"}, login["body"]
    assert login["headers"].get("x-vlite-request") == "1" and "authorization" not in login["headers"], login["headers"]

    # A temporary password signs in but only allows choosing a new one; the dashboard stays hidden.
    sign_in(page, "ann", FIRST)
    expect(control(page, "#status")).to_have_text("Signed in as ann. Choose a new password to continue.")
    expect(page.get_by_role("heading", name="Choose a new password", exact=True)).to_be_visible()
    expect(page.locator("#gate-intro")).to_be_hidden()
    expect(page.locator("#app-shell")).to_be_hidden()
    expect(page.locator("#connection-badge")).to_have_text("Password change needed")
    expect(page.locator("#identity-badge")).to_have_text("Signed in as ann")
    expect(control(page, "#change-password")).to_be_visible()
    expect(control(page, "#change-password-reason")).to_contain_text("temporary password")
    expect(control(page, "#change-password-cancel")).to_be_hidden()
    expect(control(page, "#current-password")).to_be_focused()
    expect(control(page, "#sign-in")).to_be_hidden()
    expect(page.locator("#token-sign-in")).to_be_hidden()
    expect(page.locator("#gate-sign-out")).to_be_visible()
    expect(page.locator("#gate-retry")).to_be_hidden()
    assert not calls(fake, "/v1/capabilities", "GET"), "A temporary password must not load data"
    change_password(page, FIRST, SECOND, SECOND + "!")
    expect(control(page, "#status")).to_have_text("The new passwords do not match. Nothing changed.")
    expect_gate_message(page, "The new passwords do not match. Nothing changed.")
    expect(control(page, "#new-password")).to_have_value("")
    assert not calls(fake, "/v1/auth/password", "POST")
    change_password(page, "wrong current password", SECOND, SECOND)
    expect(control(page, "#status")).to_have_text("Your current password is not correct. Nothing changed.")
    expect(control(page, "#error-help")).to_have_text("")
    change_password(page, FIRST, SECOND, SECOND)
    expect(control(page, "#status")).to_have_text("Password changed. Signed in as ann; overview refreshed")
    assert calls(fake, "/v1/auth/password", "POST")[-1]["body"] == {"current_password": FIRST, "new_password": SECOND}
    expect(page.locator("#sign-in-gate")).to_be_hidden()
    expect(page.locator("#app-shell")).to_be_visible()
    expect(page.locator("#connection-badge")).to_have_text("Connected")
    expect(page.locator("#identity-badge")).to_have_text("Signed in as ann · Super admin")
    expect(page.locator("#identity-detail")).to_contain_text("can manage users")
    expect(page.locator("#change-password")).to_be_hidden()
    expect(page.locator("#session-refresh")).to_be_visible()
    data = [item for item in fake.log if not item["path"].startswith("/v1/auth/")]
    assert data and all("authorization" not in item["headers"] and item["headers"].get("x-vlite-request") == "1"
                        for item in data), "Password sessions use the cookie plus the X-VLite-Request header"
    assert SECOND not in visible_state(page) and FIRST not in visible_state(page), "Passwords must not stay on the page"
    # Changing a password by choice opens the same screen, which can be cancelled.
    open_user_menu(page)
    expect(page.locator("#identity-name")).to_have_text("Signed in as ann · Super admin")
    page.locator("#change-password-open").click()
    expect(page.get_by_role("heading", name="Choose a new password", exact=True)).to_be_visible()
    expect(page.locator("#app-shell")).to_be_hidden()
    expect(page.locator("#change-password-reason")).to_have_text("Enter your current password, then choose a new one.")
    expect(page.locator("#current-password")).to_be_focused()
    expect(page.locator("#gate-sign-out")).to_be_hidden()
    page.locator("#change-password-cancel").click()
    expect(page.locator("#change-password")).to_be_hidden()
    expect(page.locator("#app-shell")).to_be_visible()
    expect(page.locator("#workspace-title")).to_be_focused()
    open_user_menu(page)
    expect(page.locator("#change-password-open")).to_be_visible()
    close_user_menu(page)

    # A super-admin account manages other accounts but not its own.
    control(page, "#user-list").click()
    expect(control(page, "#user-result")).to_contain_text('"count": 1')
    expect(control(page, "#user-result")).to_contain_text('"access": "whole server"')
    control(page, "#user-username").fill("Bob")
    control(page, "#user-password").fill(FIRST)
    control(page, "#user-role").select_option("editor")
    control(page, "#user-create").click()
    confirm(page, 'Add editor "bob" with access to every collection? They must choose a new password at their first sign-in.', accept=False)
    assert not calls(fake, "/v1/admin/users", "POST")
    control(page, "#user-create").click()
    confirm(page, "Share the password with them privately.")
    expect(control(page, "#user-result")).to_contain_text('"added"')
    expect(control(page, "#user-password")).to_have_value("")
    expect(control(page, "#user-id")).to_have_value(BOB)
    assert calls(fake, "/v1/admin/users", "POST")[-1]["body"] == {
        "username": "bob", "password": FIRST, "role": "editor", "must_change_password": True, "disabled": False}
    control(page, "#user-role").select_option("viewer")
    control(page, "#user-disabled").check()
    control(page, "#user-save").click()
    confirm(page, f'Update "bob" ({BOB}): role → Viewer; account disabled → yes?')
    expect(control(page, "#user-result")).to_contain_text("disabled (cannot sign in)")
    assert calls(fake, f"/v1/admin/users/{BOB}", "PUT")[-1]["body"] == {"role": "viewer", "disabled": True}
    control(page, "#user-password").fill(SECOND)
    control(page, "#user-must-change").uncheck()
    control(page, "#user-reset-password").click()
    confirm(page, f'Set a new password for "bob" ({BOB})? Their open sessions end. Share')
    expect(control(page, "#user-result")).to_contain_text('"password_set"')
    expect(control(page, "#user-password")).to_have_value("")
    assert calls(fake, f"/v1/admin/users/{BOB}/password", "POST")[-1]["body"] == {"password": SECOND, "must_change_password": False}
    control(page, "#user-id").fill(ANN)
    expect(control(page, "#user-username")).to_have_value("ann")
    control(page, "#user-delete").click()
    confirm(page, "cannot undo this")
    expect(control(page, "#user-result")).to_contain_text("Use Change password")
    control(page, "#user-id").fill(BOB)
    control(page, "#user-delete").click()
    confirm(page, f'Delete user "bob" ({BOB})?')
    expect(control(page, "#user-result")).to_contain_text('"deleted": true')
    assert BOB not in fake.users

    # Reload keeps the session; a session ended on the server asks for a new sign-in.
    page.reload()
    expect(control(page, "#status")).to_have_text("Signed in as ann; overview refreshed")
    expect(control(page, "#user-result")).to_have_text("")
    assert not calls(fake, "/v1/auth/logout", "POST"), "Leaving the page must not sign out"
    fake.active = False
    control(page, "#session-refresh").click()
    expect(control(page, "#status")).to_have_text("Your session ended. Sign in again.")
    expect(control(page, "#error-help")).to_contain_text("Sessions end after a period of inactivity")
    expect_signed_out(page)
    expect_gate_message(page, "Your session ended. Sign in again.")
    expect(page.locator("#gate-help")).to_contain_text("Sessions end after a period of inactivity")
    expect(page.locator("#sign-in")).to_be_visible()
    expect(page.locator("#username")).to_be_focused()
    sign_in(page, "ann", SECOND)
    expect(control(page, "#status")).to_have_text("Signed in as ann; overview refreshed")
    for width in (900, 390, 320):
        page.set_viewport_size({"width": width, "height": 1000})
        for section in ("database", "users"):
            page.evaluate("id => window.VLiteNavigation.reveal(id)", section)
            assert page.evaluate("document.documentElement.scrollWidth <= innerWidth"), (width, section)
    page.set_viewport_size({"width": 1280, "height": 900})
    control(page, "#user-list").click()
    expect(control(page, "#user-result")).to_contain_text('"count": 1')
    page.evaluate("window.VLiteNavigation.reveal('database')")
    sign_out(page)
    expect(page.locator("#status")).to_have_text("Signed out. Displayed results were cleared.")
    expect_gate_message(page, "Signed out. Displayed results were cleared.")
    expect(page.locator("#user-result")).to_have_text("")
    expect(page.locator("#sign-in")).to_be_visible()
    deadline = time.monotonic() + 10
    while not calls(fake, "/v1/auth/logout", "POST") and time.monotonic() < deadline:
        page.wait_for_timeout(50)
    assert calls(fake, "/v1/auth/logout", "POST") and not fake.active, "Sign out must end the server session"

    # Access tokens remain available as the alternative.
    assert token_form_open(page) is False, "After signing out, the token form is a collapsed alternative again"
    sign_in_with_token(page, "admin-" + "t" * 32)
    expect(page.locator("#identity-badge")).to_have_text("Signed in as Super admin (bootstrap admin token)")
    expect(page.locator("#sign-in")).to_be_hidden()
    open_user_menu(page)
    expect(page.locator("#change-password-open")).to_be_hidden()
    close_user_menu(page)
    sign_out(page)
    expect(page.locator("#status")).to_have_text("Signed out. The access token and displayed results were cleared.")
    assert token_form_open(page) is False
    assert not errors, errors
    context.close()

    hints = (({"setup_required": True},
              "No user accounts yet. Sign in with the admin token below, then add people in Administration > Users & access."),
             ({"password_login": False, "reason": "insecure_transport"},
              "Password sign-in is off because the server cannot confirm this connection uses HTTPS. Use an access token."))
    for config, hint in hints:
        fake = PasswordApi(config)
        context, page, errors = session_page(browser, fake)
        expect_signed_out(page)
        expect(control(page, "#sign-in-setup")).to_have_text(hint)
        expect(control(page, "#sign-in-setup")).to_be_visible()
        expect(control(page, "#sign-in")).to_be_hidden()
        expect(page.locator("#token-sign-in > summary")).to_be_hidden()
        assert token_form_open(page) is True
        expect(page.get_by_role("button", name="Sign in with token", exact=True)).to_be_visible()
        assert not errors, errors
        context.close()


def exercise_password_memory_mode(browser):
    """The incident: a super admin signs in with a password while agent memory is on. The mock refuses the
    database-wide routes to session requests exactly as it does to tokens, so sign-in must survive the refusal."""
    fake = PasswordApi()
    fake.memory_mode_enabled = True
    fake.must_change = False
    context, page, errors = session_page(browser, fake)
    sign_in(page, "ann", FIRST)
    expect(page.locator("#app-shell")).to_be_visible()
    expect(page.locator("#status")).to_have_text("Signed in as ann; overview refreshed")
    expect(page.locator("#connection-badge")).to_have_text("Connected")
    expect(control(page, "#metrics-note")).to_have_text(
        "Server metrics are off while agent memory is enabled. Database size shows Unavailable.")
    metrics = calls(fake, "/metrics", "GET")
    assert metrics and all("authorization" not in item["headers"] for item in metrics), "The refused request is a session request"
    for selector in ("#status", "#error-help", "#gate-message", "#metrics-note"):
        expect(page.locator(selector)).not_to_contain_text("403")
        expect(page.locator(selector)).not_to_contain_text("forbidden_scope")
    assert not errors, errors
    context.close()


def exercise(browser):
    fake = AdminApi()
    context, page, errors = mocked_page(browser, fake)
    expect(page.locator("#identity-badge")).to_have_text("Signed in as Super admin (bootstrap admin token)")
    expect(page.locator("#identity-badge")).to_have_attribute("data-identity", "super_admin")
    expect(page.locator("#identity-detail")).to_contain_text("Full administrator")
    assert not calls(fake, "/v1/admin/tenants", "GET"), "Admin actions must not run on connection"
    for name in USER_CONTROLS:
        expect(control(page, "#" + name)).to_be_disabled()
    expect(control(page, "#user-result")).to_contain_text("VLITE_ADMIN_DIR")
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
        for section in ("tenants", "users", "transport-security", "replication", "admin-backups",
                        "admin-auto-maintenance", "admin-backup-schedule"):
            page.evaluate("id => window.VLiteNavigation.reveal(id)", section)
            assert page.evaluate("document.documentElement.scrollWidth <= innerWidth"), (width, section)
    page.evaluate("window.VLiteNavigation.reveal('database')")
    sign_out(page)
    expect(page.locator("#status")).to_have_text("Signed out. The access token and displayed results were cleared.")
    expect(page.locator("#tenant-result")).to_have_text("")
    expect(page.locator("#user-result")).to_have_text("")
    expect(page.locator("#tls-result")).to_have_text("")
    expect(page.locator("#replication-result")).to_have_text("")
    expect(page.locator("#replication-download")).to_be_disabled()
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
        for name in ("tenant-list", "tenant-create", "tenant-delete", "tls-reload", *LIFECYCLE, *USER_CONTROLS):
            expect(control(page, "#" + name)).to_be_disabled()
        expect(control(page, "#tenant-result")).to_contain_text("bootstrap admin")
        expect(control(page, "#user-result")).to_contain_text("User management requires a super admin")
        if role == "scoped":
            expect(page.locator("#identity-badge")).to_have_text('Signed in as API key "ci" (' + "c" * 32 + ') · tenant "Support"')
            expect(page.locator("#identity-detail")).to_contain_text(f'inside tenant "Support" ({TENANT})')
        else:
            expect(page.locator("#identity-badge")).to_contain_text("(this server does not report identity details)")
            expect(page.locator("#identity-badge")).to_have_attribute("data-identity", "read_only")
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


CLOUD = "/v1/admin/cloud-backup"
CLOUD_AT = 1794659400000  # 2026-11-14 12:30 UTC
CLOUD_POLICY = {"target": {"account": "vlitebackups", "container": "backups", "coordination_container": "backups-lease",
                           "prefix": "vlite/prod/", "auth": "managed_identity"},
                "enabled": True, "interval_secs": 3600, "retention_enabled": True, "keep_sets": 5, "keep_days": 45,
                "orphan_grace_hours": 12, "drill_every": 6}


class CloudApi(AdminApi):
    """The cloud backup REST API with its documented response shapes; nothing reaches Azure."""

    def __init__(self, role="admin"):
        super().__init__(role=role)
        self.caps["features"]["cloud_backup"] = True
        self.policy = None
        self.busy = False
        self.pins = [{"id": "p1", "reason": "restore", "created_unix_ms": CLOUD_AT, "kind": "operation"}]

    def state(self):
        return {"configured": self.policy is not None, "policy": self.policy, "policy_uncertain": False,
                "installation_id": "inst-1", "running": False, "next_run_unix_ms": CLOUD_AT + 3600000,
                "status": {"runs": 1, "published": 1, "failures": 0, "skipped_busy": 0,
                           "last_success_unix_ms": CLOUD_AT, "last_set": "set-1"}}

    def sets(self):
        record = {"state": "published", "created_unix_ms": CLOUD_AT, "verified_unix_ms": CLOUD_AT,
                  "last_drill": {"ok": True, "unix_ms": CLOUD_AT}, "pins": self.pins}
        return {"installation_id": "inst-1", "tombstoned": [], "sets": [
            {"set_id": "set-1", "data_objects": 4, "record": record},
            {"set_id": "set-2", "data_objects": 1, "record": {**record, "state": "staging", "pins": []}}]}

    def api(self, method, path, query, body, headers):
        if path == CLOUD:
            if method == "PUT":
                self.policy = body
            return 200, self.state(), {}
        if path == CLOUD + "/run" and method == "POST":
            if self.busy:
                return self.error(409, "cloud_backup_busy")
            return 200, {"set_id": "set-3", "drill": {"ok": True, "unix_ms": CLOUD_AT}}, {}
        if path == CLOUD + "/sets" and method == "GET":
            return 200, self.sets(), {}
        prefix = CLOUD + "/sets/set-1"
        if path == prefix + "/restore" and method == "POST":
            return 200, {"name": "restore-set-1.vlite", "set_id": "set-1", "bundle": {"credentials": 2},
                         "pin_released": True}, {}
        if path == prefix + "/pins" and method == "POST":
            pin = {"id": "p2", "reason": body["reason"], "created_unix_ms": CLOUD_AT, "kind": "operator"}
            self.pins = [*self.pins, pin]
            return 200, {"set_id": "set-1", "pin": pin}, {}
        if path.startswith(prefix + "/pins/") and method == "DELETE":
            pin = path.rsplit("/", 1)[1]
            self.pins = [item for item in self.pins if item["id"] != pin]
            return 200, {"set_id": "set-1", "pin_id": pin, "forced": query == "force=true"}, {}
        return super().api(method, path, query, body, headers)


def exercise_cloud_backup(browser):
    fake = CloudApi()
    context, page, errors = mocked_page(browser, fake)
    card = control(page, "#admin-cloud-backup")
    expect(card.locator('[data-admin-availability="cloud_backup"]')).to_have_text(
        "Supported by this server. Select Refresh status to see whether it is set up.")
    assert not [item for item in fake.log if item["path"].startswith(CLOUD)], "Cloud backup must not be read on connection"
    expect(page.locator("#cloud-backup-run")).to_be_disabled()
    control(page, "#cloud-backup-refresh").click()
    expect(page.locator("#cloud-backup-status")).to_contain_text("Not set up yet")
    expect(page.locator("#cloud-backup-interval")).to_have_value("60")

    # Invalid settings are marked in place and nothing is sent.
    page.locator("#cloud-backup-account").fill("Bad_Account")
    page.locator("#cloud-backup-prefix").fill("vlite")
    page.locator("#cloud-backup-save").click()
    expect(page.locator("#cloud-backup-result")).to_have_text("Fix the highlighted settings; nothing was saved.")
    expect(page.locator("#cloud-backup-account-error")).to_have_text("Enter 3 to 24 lowercase letters or digits.")
    expect(page.locator("#cloud-backup-account")).to_have_attribute("aria-invalid", "true")
    expect(page.locator("#cloud-backup-account")).to_be_focused()
    expect(page.locator("#cloud-backup-prefix-error")).to_be_visible()
    assert not calls(fake, CLOUD, "PUT")
    for selector, value in (("#cloud-backup-account", "vlitebackups"), ("#cloud-backup-container", "backups"),
                            ("#cloud-backup-coordination", "backups-lease"), ("#cloud-backup-prefix", "vlite/prod/"),
                            ("#cloud-backup-keep-sets", "5"), ("#cloud-backup-keep-days", "45"),
                            ("#cloud-backup-grace", "12"), ("#cloud-backup-drill-every", "6")):
        page.locator(selector).fill(value)
    expect(page.locator("#cloud-backup-account-error")).to_be_hidden()
    page.locator("#cloud-backup-enabled").check()
    page.locator("#cloud-backup-retention").check()
    page.locator("#cloud-backup-save").click()
    confirm(page, "Save cloud backup settings (account vlitebackups", accept=False)
    assert not calls(fake, CLOUD, "PUT")
    page.locator("#cloud-backup-save").click()
    confirm(page, "keeping the newest 5 and any younger than 45 days")
    expect(page.locator("#cloud-backup-result")).to_have_text("Saved. Automatic backups are on.")
    assert calls(fake, CLOUD, "PUT")[-1]["body"] == CLOUD_POLICY, calls(fake, CLOUD, "PUT")[-1]["body"]
    expect(page.locator("#cloud-backup-status")).to_have_text("On: a backup is made every 1 hour.")
    expect(page.locator("#cloud-backup-facts")).to_contain_text("vlite/prod/")

    page.locator("#cloud-backup-run").click()
    confirm(page, "Back up to account vlitebackups now?")
    expect(page.locator("#cloud-backup-result")).to_contain_text("Backup set-3 was uploaded. Restore test: Passed")
    assert calls(fake, CLOUD + "/run", "POST")
    fake.busy = True
    page.locator("#cloud-backup-run").click()
    confirm(page, "Back up to account vlitebackups now?")
    expect(page.locator("#cloud-backup-result")).to_contain_text("Another cloud backup operation is running")

    # Stored backups: an unfinished upload cannot be restored or held; a complete one can.
    page.locator("#cloud-backup-sets").click()
    expect(page.locator("#cloud-backup-result")).to_contain_text("2 stored backups found, newest first.")
    page.locator("#cloud-backup-set-id").select_option("set-2")
    for selector in ("#cloud-backup-drill", "#cloud-backup-restore", "#cloud-backup-pin"):
        expect(page.locator(selector)).to_be_disabled()
    page.locator("#cloud-backup-set-id").select_option("set-1")
    expect(page.locator("#cloud-backup-set-facts")).to_contain_text("Stored files")
    expect(page.locator("#cloud-backup-restore")).to_be_enabled()
    page.locator("#cloud-backup-restore").click()
    confirm(page, "The live database is not changed")
    expect(page.locator("#cloud-backup-result")).to_contain_text("restored as a new copy named restore-set-1.vlite")
    expect(page.locator("#cloud-backup-result")).to_contain_text("2 credentials must be issued again")
    page.locator("#cloud-backup-pin").click()
    expect(page.locator("#cloud-backup-pin-reason-error")).to_have_text("Enter a reason.")
    expect(page.locator("#cloud-backup-pin-reason")).to_be_focused()
    assert not calls(fake, CLOUD + "/sets/set-1/pins", "POST")
    page.locator("#cloud-backup-pin-reason").fill("legal hold")
    page.locator("#cloud-backup-pin").click()
    expect(page.locator("#cloud-backup-result")).to_contain_text("Backup set-1 is held (hold p2)")
    assert calls(fake, CLOUD + "/sets/set-1/pins", "POST")[-1]["body"] == {"reason": "legal hold"}
    expect(page.locator("#cloud-backup-pin-id option")).to_have_count(2)
    page.locator("#cloud-backup-pin-id").select_option("p1")
    page.locator("#cloud-backup-force").check()
    page.locator("#cloud-backup-unpin").click()
    confirm(page, "no longer running")
    expect(page.locator("#cloud-backup-result")).to_contain_text("Hold p1 released.")
    assert calls(fake, CLOUD + "/sets/set-1/pins/p1", "DELETE")[-1]["query"] == "force=true"
    expect(page.locator("#cloud-backup-force")).not_to_be_checked()

    for width, height in ((1280, 900), (768, 1000), (390, 844)):
        page.set_viewport_size({"width": width, "height": height})
        control(page, "#admin-cloud-backup")
        overflow = page.evaluate("""() => {
            const card = document.querySelector('#admin-cloud-backup');
            return { page: document.documentElement.scrollWidth - document.documentElement.clientWidth,
                     card: card.scrollWidth - card.clientWidth };
        }""")
        assert overflow["page"] <= 0 and overflow["card"] <= 0, (width, overflow)
    page.set_viewport_size({"width": 1280, "height": 900})
    sign_out(page)
    expect(page.locator("#cloud-backup-result")).to_have_text("")
    expect(page.locator("#cloud-backup-status")).to_have_text("Cloud backup status not loaded.")
    assert not errors, errors
    context.close()

    fake = CloudApi("read_only")
    context, page, errors = mocked_page(browser, fake)
    control(page, "#admin-cloud-backup")
    expect(page.locator('[data-admin-availability="cloud_backup"]')).to_contain_text("Needs a super admin")
    for selector in ("#cloud-backup-refresh", "#cloud-backup-save", "#cloud-backup-run", "#cloud-backup-sets"):
        expect(page.locator(selector)).to_be_disabled()
    assert not errors, errors
    context.close()
    fake = CloudApi()
    fake.caps["features"]["cloud_backup"] = False
    context, page, errors = mocked_page(browser, fake)
    control(page, "#admin-cloud-backup")
    expect(page.locator('[data-admin-availability="cloud_backup"]')).to_contain_text("Not available on this server.")
    expect(page.locator("#cloud-backup-refresh")).to_be_disabled()
    assert not errors, errors
    context.close()


AUTO = "/v1/admin/auto-failover"


class AutoFailoverApi(AdminApi):
    """The automatic failover switch with its documented response shape; no supervisor or HA store exists."""

    def __init__(self, role="admin", available=True):
        super().__init__(role=role)
        self.available = available
        self.enabled = False
        self.revision = 0
        self.conflict = False

    def state(self):
        stored = self.available and self.revision > 0
        return {"available": self.available, "enabled": self.enabled,
                "revision": f"r{self.revision}" if stored else None,
                "updated_unix_ms": CLOUD_AT if stored else None, "source": "rest:admin" if stored else None,
                "invalid": False, "qualified": False, "gate": "GATE-005",
                "warning": "automatic failover is unqualified (GATE-005 open)"}

    def api(self, method, path, query, body, headers):
        if path == AUTO:
            if method == "GET":
                return 200, self.state(), {}
            if method == "PUT":
                if not self.available:
                    return self.error(409, "auto_failover_not_configured")
                if body.get("enabled") and self.conflict:
                    return self.error(409, "auto_failover_conflict")
                self.enabled = body["enabled"]
                self.revision += 1
                return 200, self.state(), {}
        return super().api(method, path, query, body, headers)


def exercise_auto_failover(browser):
    fake = AutoFailoverApi()
    context, page, errors = mocked_page(browser, fake)
    control(page, "#admin-auto-failover")
    expect(page.locator("#auto-failover-status")).to_have_text("Automatic failover status not loaded.")
    assert not calls(fake, AUTO, "GET"), "Automatic failover must not be read on connection"
    expect(page.locator("#auto-failover-toggle")).to_be_disabled()
    expect(page.locator("#auto-failover-help")).to_have_text("Select Refresh status first.")
    control(page, "#auto-failover-refresh").click()
    expect(page.locator("#auto-failover-status")).to_contain_text("Off: failover happens only when a person runs it")
    expect(page.locator("#auto-failover-facts")).to_contain_text("GATE-005 has not passed")
    toggle = page.locator("#auto-failover-toggle")
    expect(toggle).to_have_text("Turn on automatic failover")
    expect(toggle).to_be_enabled()

    # Cancelling the warning sends nothing; accepting sends the acknowledged, revision-guarded change.
    toggle.click()
    confirm(page, "NOT yet proven safe (GATE-005 has not passed)", accept=False)
    assert not calls(fake, AUTO, "PUT")
    toggle.click()
    confirm(page, "can be lost")
    expect(page.locator("#auto-failover-result")).to_have_text("Automatic failover is on (not yet proven safe).")
    assert calls(fake, AUTO, "PUT")[-1]["body"] == {"enabled": True, "acknowledge_unqualified": True}
    expect(page.locator("#auto-failover-status")).to_contain_text("On (not yet proven safe)")
    expect(page.locator("#auto-failover-facts")).to_contain_text("rest:admin")
    expect(toggle).to_have_text("Turn off automatic failover")

    # Turning off is the emergency stop: no revision guard.
    toggle.click()
    confirm(page, "Turn off automatic failover?")
    expect(page.locator("#auto-failover-result")).to_have_text("Automatic failover is off.")
    assert calls(fake, AUTO, "PUT")[-1]["body"] == {"enabled": False}
    expect(toggle).to_have_text("Turn on automatic failover")

    fake.conflict = True
    toggle.click()
    confirm(page, "can be lost")
    expect(page.locator("#auto-failover-result")).to_contain_text("Someone else changed automatic failover first")
    assert calls(fake, AUTO, "PUT")[-1]["body"]["expected_revision"] == "r2"
    expect(page.locator("#auto-failover-status")).to_have_text("Automatic failover status not loaded.")
    expect(toggle).to_be_disabled()
    expect(page.locator("#auto-failover-help")).to_have_text("Select Refresh status first.")

    for width, height in ((1280, 900), (768, 1000), (390, 844)):
        page.set_viewport_size({"width": width, "height": height})
        control(page, "#admin-auto-failover")
        overflow = page.evaluate("""() => {
            const card = document.querySelector('#admin-auto-failover');
            return { page: document.documentElement.scrollWidth - document.documentElement.clientWidth,
                     card: card.scrollWidth - card.clientWidth };
        }""")
        assert overflow["page"] <= 0 and overflow["card"] <= 0, (width, overflow)
    page.set_viewport_size({"width": 1280, "height": 900})
    sign_out(page)
    expect(page.locator("#auto-failover-result")).to_have_text("")
    expect(page.locator("#auto-failover-status")).to_have_text("Automatic failover status not loaded.")
    assert not errors, errors
    context.close()

    fake = AutoFailoverApi(available=False)
    context, page, errors = mocked_page(browser, fake)
    control(page, "#auto-failover-refresh").click()
    expect(page.locator("#auto-failover-status")).to_contain_text("Not set up")
    expect(page.locator("#auto-failover-toggle")).to_be_disabled()
    expect(page.locator("#auto-failover-help")).to_contain_text("cannot switch automatic failover")
    assert not errors, errors
    context.close()

    fake = AutoFailoverApi("read_only")
    context, page, errors = mocked_page(browser, fake)
    control(page, "#admin-auto-failover")
    for selector in ("#auto-failover-refresh", "#auto-failover-toggle"):
        expect(page.locator(selector)).to_be_disabled()
    expect(page.locator("#auto-failover-help")).to_contain_text("Automatic failover requires a super admin")
    assert not calls(fake, AUTO, "GET") and not calls(fake, AUTO, "PUT")
    assert not errors, errors
    context.close()


DEPLOY_OPS = "/v1/admin/tenant-deployment-operations"
HOSTILE = "<img src=x onerror=window.__xss=1>"


def deployment(index, **extra):
    operation = {
        "operation_id": f"{index:032x}", "kind": "create", "tenant_id": "c" * 32, "phase": "reserved",
        "next_step": "cloud execution is blocked (OD-002); cancel to release the reservation",
        "operation_generation": 1, "lifecycle_generation": 1, "control_generation": 3, "receipt_digest": None,
        "created_unix_ms": CREATED,
        "desired": {"kind": "create", "name": f"tenant-{index}-" + "x" * 50, "max_vectors": None,
                    "writer_profile": "d" * 64, "initialization": "e" * 64, "resources": [], "secret_versions": 1},
        "observed": {"reserved": True, "routed": False, "cancellation_requested": False,
                     "journal_steps": 2, "uncertain_steps": 0, "terminal": False},
    }
    return {**operation, **extra}


class TenantDeploymentApi(AdminApi):
    """The read-only operation list with its documented page shape and refusals; nothing is executed."""

    def __init__(self, role="admin", refusal=None):
        super().__init__(role=role)
        self.refusal = refusal
        self.operations = [deployment(1, next_step=HOSTILE)] + [deployment(i) for i in range(2, 23)] + [
            deployment(23, kind="retire", phase="retired", desired={"kind": "retire"})]

    def api(self, method, path, query, body, headers):
        if path == DEPLOY_OPS and method == "GET":
            if self.refusal:
                return self.error(*self.refusal)
            params = dict(item.split("=", 1) for item in query.split("&") if item)
            limit = int(params.get("limit", "50"))
            after = params.get("after")
            items = [item for item in self.operations if after is None or item["operation_id"] > after][:limit]
            more = items and items[-1] is not self.operations[-1]
            return 200, {"operations": items, "next_after": items[-1]["operation_id"] if more else None}, {}
        return super().api(method, path, query, body, headers)


def exercise_tenant_deployments(browser):
    fake = TenantDeploymentApi()
    context, page, errors = mocked_page(browser, fake)
    control(page, "#admin-tenant-deployments")
    status = page.locator("#tenant-deployment-status")
    expect(status).to_have_text("Tenant deployment operations not loaded.")
    assert not calls(fake, DEPLOY_OPS, "GET"), "Tenant deployments must not be read on connection"
    expect(page.locator("#tenant-deployment-next")).to_be_disabled()
    expect(page.locator("#tenant-deployment-help")).to_contain_text("Nothing loads automatically")

    control(page, "#tenant-deployment-refresh").click()
    expect(status).to_have_text("Page 1: 20 operations; more on the next page. Listed in operation ID order, not by time.")
    items = page.locator("#tenant-deployment-list > li")
    expect(items).to_have_count(20)
    expect(items.first).to_contain_text("Create tenant: Reserved, waiting for cloud execution")
    expect(items.first).not_to_contain_text("not enabled yet")
    expect(items.first).to_contain_text(HOSTILE)
    assert page.evaluate("() => window.__xss === undefined && !document.querySelector('#tenant-deployment-list img')")
    assert calls(fake, DEPLOY_OPS, "GET")[-1]["query"] == "limit=20"

    for width, height in ((1280, 900), (768, 1000), (390, 844)):
        page.set_viewport_size({"width": width, "height": height})
        control(page, "#admin-tenant-deployments")
        overflow = page.evaluate("""() => {
            const card = document.querySelector('#admin-tenant-deployments');
            return { page: document.documentElement.scrollWidth - document.documentElement.clientWidth,
                     card: card.scrollWidth - card.clientWidth };
        }""")
        assert overflow["page"] <= 0 and overflow["card"] <= 0, (width, overflow)
    page.set_viewport_size({"width": 1280, "height": 900})

    control(page, "#tenant-deployment-next").click()
    expect(status).to_have_text("Page 2: 3 operations. Listed in operation ID order, not by time.")
    assert calls(fake, DEPLOY_OPS, "GET")[-1]["query"] == f"limit=20&after={20:032x}"
    expect(items).to_have_count(3)
    expect(items.last).to_contain_text("Retire tenant: Retired: route removed, data kept")
    expect(page.locator("#tenant-deployment-next")).to_be_disabled()

    sign_out(page)
    expect(page.locator("#tenant-deployment-result")).to_have_text("")
    expect(status).to_have_text("Tenant deployment operations not loaded.")
    assert not errors, errors
    context.close()

    # Not enabled on this server: a calm explanation, not a failure.
    fake = TenantDeploymentApi(refusal=(409, "tenant_deployments_not_configured"))
    context, page, errors = mocked_page(browser, fake)
    control(page, "#tenant-deployment-refresh").click()
    status = page.locator("#tenant-deployment-status")
    expect(status).to_contain_text("--tenant-deployments-file")
    expect(status).to_have_attribute("data-tone", "blocked")
    expect(page.locator("#tenant-deployment-next")).to_be_disabled()
    assert not errors, errors
    context.close()

    # Reader sign-in: locked with the reason, and nothing is requested.
    fake = TenantDeploymentApi("read_only")
    context, page, errors = mocked_page(browser, fake)
    control(page, "#admin-tenant-deployments")
    for selector in ("#tenant-deployment-refresh", "#tenant-deployment-next", "#tenant-deployment-check",
                     "#tenant-deployment-cancel", "#tenant-deployment-create", "#tenant-deployment-readers",
                     "#tenant-deployment-retire", "#tenant-deployment-name", "#tenant-deployment-writer-file"):
        expect(page.locator(selector)).to_be_disabled()
    expect(page.locator("#tenant-deployment-help")).to_contain_text("requires a super admin")
    expect(page.locator("#tenant-deployment-operation-help")).to_contain_text("requires a super admin")
    assert not calls(fake, DEPLOY_OPS, "GET")
    assert not errors, errors
    context.close()


DEPLOY_SUBMIT = "/v1/admin/tenant-deployments"
# A chosen file is sent and hashed byte for byte (CRLF and non-ASCII kept); pasted text is sent as typed.
WRITER = '{\r\n  "apps": ["writer"],\r\n  "note": "caf\u00e9"\r\n}\r\n'
INIT = '{"start": "empty"}\n'
READER = '{"apps": ["reader"]}\n'
SCALING = '{"rule": "cpu above 70 percent"}'
RECEIPT = "f" * 64
UUID4 = re.compile(r"[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}")


def sha(text):
    return hashlib.sha256(text.encode("utf-8")).hexdigest()


class TenantDeploymentWriteApi(TenantDeploymentApi):
    """Records create, readers and retire requests and cancels them as the documented contract says: the digests
    must name exactly the posted contents, a repeated key with the same request returns the same operation, and
    cancel must quote the operation's current generations. Nothing is executed."""

    def __init__(self):
        super().__init__()
        self.operations = []
        self.keys = {}
        self.fail = []  # (status, code, recorded first) for the next submissions and cancels
        self.bump_on_read = False  # the operation changes right after its status is read
        self.actions = {}  # (operation, key) -> the stored cancel and its result
        self.release_fails = False  # the next cancel seals the operation but fails to release its reservation
        self.older_server = False  # refuses a new cancel for an ended operation, as servers before that recovery do
        self.rejected_token = None  # an access token refused with a non-JSON 401, as a proxy in front might answer

    def failure(self):
        return self.fail.pop(0) if self.fail else None

    def submit(self, path, body):
        assert set(body) == {"idempotency_key", "request", "contents"}, body
        request, contents = body["request"], body["contents"]
        fleet = request.get("fleet", {})
        referenced = ({request["writer_profile"], request["initialization"]} if request["kind"] == "create"
                      else {fleet["reader_profile"], fleet["scaling_rule"]} if fleet.get("state") == "present" else set())
        digests = [sha(text) for text in contents]
        tenant = request.get("tenant_id")
        expected_path = DEPLOY_SUBMIT if request["kind"] == "create" else (
            f"{DEPLOY_SUBMIT}/{tenant}/{'readers' if request['kind'] == 'set_readers' else 'retire'}")
        if len(set(digests)) != len(digests) or set(digests) != referenced or path != expected_path:
            return self.error(400, "operation_request_invalid")
        fingerprint = (path, repr(request), tuple(sorted(contents)))
        if body["idempotency_key"] in self.keys:
            known, operation = self.keys[body["idempotency_key"]]
            if known != fingerprint:
                return self.error(409, "operation_request_conflict")
            return 200, {"duplicate": True, "operation": operation}, {}
        failure = self.failure()
        if failure and not failure[2]:
            return self.error(*failure[:2])
        index = len(self.operations) + 1
        operation = deployment(index, kind=request["kind"], tenant_id=tenant or "c" * 31 + f"{index:x}",
                               phase="reserved" if request["kind"] == "create" else "accepted", desired=request,
                               next_step=HOSTILE)
        self.operations.append(operation)
        self.keys[body["idempotency_key"]] = (fingerprint, operation)
        return self.error(*failure[:2]) if failure else (202, {"duplicate": False, "operation": operation}, {})

    def act(self, method, rest, body):
        operation = next((item for item in self.operations if item["operation_id"] == rest[0]), None)
        if operation is None:
            return self.error(404, "operation_not_found")
        if method == "GET" and len(rest) == 1:
            snapshot = {**operation, "observed": dict(operation["observed"])}
            if self.bump_on_read:
                self.bump_on_read = False
                operation["operation_generation"] += 1
            return 200, {"operation": snapshot}, {}
        if method != "POST" or rest[1:] != ["cancel"]:
            return self.error(404, "tenant_deployment_action_unknown")
        assert set(body) <= {"idempotency_key", "expected_operation_generation", "expected_lifecycle_generation",
                             "expected_receipt"}, body
        # As admin.rs `act`: the action is stored per key before it is applied; an exact replay returns the stored
        # result, or applies again (finishing a release) without checking tokens when no result was stored.
        stored = self.actions.get((rest[0], body["idempotency_key"]))
        if stored is not None:
            if stored["body"] != body:
                return self.error(409, "action_request_conflict")
            if stored["result"] is not None:
                return 200, stored["result"], {}
        else:
            if (body["expected_operation_generation"], body["expected_lifecycle_generation"]) != (
                    operation["operation_generation"], operation["lifecycle_generation"]):
                return self.error(409, "operation_generation_stale")
            # As admin.rs `available`: once ended, only a sealed cancel still holding its reservation takes a new
            # cancel (it finishes the release); a server older than that recovery refuses it as well.
            if operation["observed"]["terminal"] and (self.older_server or not (
                    operation["phase"] == "cancelled" and operation["observed"]["reserved"])):
                return self.error(409, "operation_terminal")
            stored = self.actions[(rest[0], body["idempotency_key"])] = {"body": dict(body), "result": None}
        if not operation["observed"]["terminal"]:
            # The terminal record is sealed first; releasing the reservation is a separate control-state write.
            operation.update(phase="cancelled", next_step="none: cancelled",
                             operation_generation=operation["operation_generation"] + 1,
                             observed={**operation["observed"], "cancellation_requested": True, "terminal": True})
        if self.release_fails:
            self.release_fails = False
            return self.error(503, "control_state_unavailable")
        operation["observed"]["reserved"] = False
        stored["result"] = {"action": "cancel", "operation": {**operation, "observed": dict(operation["observed"])}}
        return 200, stored["result"], {}

    def api(self, method, path, query, body, headers):
        if self.rejected_token is not None and headers.get("authorization") == "Bearer " + self.rejected_token:
            return 401, "<html><body>401 Authorization Required</body></html>", {}
        if method == "POST" and (path == DEPLOY_SUBMIT or path.startswith(DEPLOY_SUBMIT + "/")):
            return self.submit(path, body)
        if path.startswith(DEPLOY_OPS + "/"):
            return self.act(method, path[len(DEPLOY_OPS) + 1:].split("/"), body)
        return super().api(method, path, query, body, headers)


def confirm_all(page, *expected, accept=True):
    for text in expected[:-1]:
        expect(control(page, "#confirmation-text")).to_contain_text(text)
    confirm(page, expected[-1], accept=accept)


def deployment_overflow(page):
    for width, height in ((1280, 900), (768, 1000), (390, 844)):
        page.set_viewport_size({"width": width, "height": height})
        control(page, "#admin-tenant-deployments")
        overflow = page.evaluate("""() => {
            const card = document.querySelector('#admin-tenant-deployments');
            return { page: document.documentElement.scrollWidth - document.documentElement.clientWidth,
                     card: card.scrollWidth - card.clientWidth };
        }""")
        assert overflow["page"] <= 0 and overflow["card"] <= 0, (width, overflow)
    page.set_viewport_size({"width": 1280, "height": 900})


def exercise_tenant_deployment_requests(browser):
    fake = TenantDeploymentWriteApi()
    context, page, errors = mocked_page(browser, fake)
    control(page, "#admin-tenant-deployments")
    expect(page.locator("#tenant-deployment-operation-help")).to_contain_text("Check status reads")
    posts = lambda path: calls(fake, path, "POST")  # noqa: E731

    # Create: a declined confirmation sends nothing.
    control(page, "#tenant-deployment-name").fill("Contoso EU")
    control(page, "#tenant-deployment-max-vectors").fill("1000")
    control(page, "#tenant-deployment-writer-file").set_input_files(
        files=[{"name": "writer.json", "mimeType": "application/json", "buffer": WRITER.encode("utf-8")}])
    expect(page.locator("#tenant-deployment-writer-note")).to_contain_text("writer.json")
    control(page, "#tenant-deployment-init-text").fill(INIT)
    control(page, "#tenant-deployment-secrets").fill("archive-sas@3\n")
    control(page, "#tenant-deployment-create").click()
    confirm_all(page, 'Record a request to create tenant "Contoso EU" (vector limit 1000)?',
                "Cloud execution is switched off (OD-002), so nothing is deployed, routed, started or stopped in Azure",
                f"Initialization settings: {len(INIT)} bytes, SHA-256 {sha(INIT)}",
                f"Writer profile: {len(WRITER.encode('utf-8'))} bytes, SHA-256 {sha(WRITER)}", accept=False)
    message = page.locator("#tenant-deployment-create-message")
    expect(message).to_have_text("Nothing was sent.")
    assert not posts(DEPLOY_SUBMIT)

    # The server records it but the answer is lost: the same button repeats the same key and is told it was recorded.
    fake.fail = [(503, "operation_indeterminate", True)]
    control(page, "#tenant-deployment-create").click()
    confirm(page, "only records the request on this server")
    expect(message).to_contain_text("Select the same button again without changing anything")
    inspector = json.loads(page.locator("#inspector").text_content())
    assert inspector["request"]["body"] == {
        "documents": 2, "resources": 0, "secret_versions": 1, "profile_digests": [sha(WRITER), sha(INIT)]}
    assert "archive-sas@3" not in json.dumps(inspector)
    control(page, "#tenant-deployment-create").click()
    confirm(page, "repeats your earlier request whose outcome is unknown, with the same request key")
    expect(message).to_contain_text("This was already recorded earlier with the same request key; nothing new was added.")
    expect(message).to_contain_text("Create tenant: Reserved, waiting for cloud execution")
    expect(message).to_contain_text(HOSTILE)
    first, second = posts(DEPLOY_SUBMIT)
    assert first["body"] == second["body"] and UUID4.fullmatch(first["body"]["idempotency_key"])
    assert first["body"]["request"] == {
        "kind": "create", "name": "Contoso EU", "max_vectors": 1000, "writer_profile": sha(WRITER),
        "initialization": sha(INIT), "resources": [], "secret_versions": ["archive-sas@3"]}
    assert first["body"]["contents"] == [WRITER, INIT]
    assert len(fake.operations) == 1
    # The form is cleared, the list reloads from page 1 and the operation box holds the new operation.
    expect(control(page, "#tenant-deployment-name")).to_have_value("")
    assert page.locator("#tenant-deployment-writer-file").evaluate("input => input.files.length") == 0
    expect(page.locator("#tenant-deployment-status")).to_have_text(
        "Page 1: 1 operation. Listed in operation ID order, not by time.")
    assert calls(fake, DEPLOY_OPS, "GET")[-1]["query"] == "limit=20"
    operation_id, tenant = fake.operations[0]["operation_id"], fake.operations[0]["tenant_id"]
    expect(control(page, "#tenant-deployment-operation")).to_have_value(operation_id)
    expect(page.locator("#tenant-deployment-operation-status")).to_have_text(
        "Create tenant: Reserved, waiting for cloud execution")
    assert page.evaluate("() => window.__xss === undefined && !document.querySelector('#admin-tenant-deployments img')")
    deployment_overflow(page)

    # Cancel reads the status first and quotes its generations; a change in between is explained, then retried.
    cancel_path = f"{DEPLOY_OPS}/{operation_id}/cancel"
    fake.bump_on_read = True
    control(page, "#tenant-deployment-cancel").click()
    confirm_all(page, f"Cancel operation {operation_id}?",
                f"It is: Create tenant for tenant {tenant}, now Reserved, waiting for cloud execution.",
                "Cloud execution is switched off (OD-002), so nothing is deployed, routed, started or stopped in Azure.")
    operation_message = page.locator("#tenant-deployment-operation-message")
    expect(operation_message).to_contain_text("The operation changed after its status was read, so nothing was recorded.")
    assert [(item["method"], item["path"]) for item in fake.log if item["path"].startswith(DEPLOY_OPS + "/")][-2:] == [
        ("GET", f"{DEPLOY_OPS}/{operation_id}"), ("POST", cancel_path)]
    stale = posts(cancel_path)[-1]["body"]
    assert stale["expected_operation_generation"] == 1 and stale["expected_lifecycle_generation"] == 1
    control(page, "#tenant-deployment-cancel").click()
    confirm(page, f"Cancel operation {operation_id}?")
    expect(operation_message).to_contain_text("Cancelled. Cloud execution is switched off (OD-002), so nothing in Azure was touched.")
    fresh = posts(cancel_path)[-1]["body"]
    assert fresh["expected_operation_generation"] == 2 and fresh["idempotency_key"] != stale["idempotency_key"]
    assert len(calls(fake, f"{DEPLOY_OPS}/{operation_id}", "GET")) == 2
    expect(page.locator("#tenant-deployment-cancel")).to_be_disabled()
    expect(page.locator("#tenant-deployment-operation-help")).to_contain_text("has ended")
    expect(page.locator("#tenant-deployment-list > li").first).to_contain_text("Create tenant: Cancelled")

    # Readers: Use this operation fills the tenant; profiles from a file and a paste.
    page.locator("#tenant-deployment-list > li").first.get_by_role("button", name=f"Use operation {operation_id}").click()
    expect(control(page, "#tenant-deployment-readers-tenant")).to_have_value(tenant)
    expect(control(page, "#tenant-deployment-retire-tenant")).to_have_value(tenant)
    control(page, "#tenant-deployment-readers-min").fill("0")
    control(page, "#tenant-deployment-readers-max").fill("2")
    control(page, "#tenant-deployment-reader-file").set_input_files(
        files=[{"name": "reader.json", "mimeType": "application/json", "buffer": READER.encode("utf-8")}])
    control(page, "#tenant-deployment-scaling-text").fill(SCALING)
    control(page, "#tenant-deployment-readers").click()
    confirm_all(page, f"Record a request for 0 to 2 reader replicas on tenant {tenant}", "ITEM-008",
                "only records the request on this server")
    expect(page.locator("#tenant-deployment-readers-message")).to_contain_text("Recorded. Nothing was deployed (cloud execution is switched off, OD-002).")
    readers = posts(f"{DEPLOY_SUBMIT}/{tenant}/readers")[-1]["body"]
    assert readers["request"] == {"kind": "set_readers", "tenant_id": tenant, "fleet": {
        "state": "present", "min_replicas": 0, "max_replicas": 2, "reader_profile": sha(READER),
        "scaling_rule": sha(SCALING), "read_routing": "writer_only"}}
    assert readers["contents"] == [READER, SCALING] and UUID4.fullmatch(readers["idempotency_key"])

    # Retire: the answer is lost before anything was recorded; editing the form first makes it a new request.
    control(page, "#tenant-deployment-retire-receipt").fill(RECEIPT)
    control(page, "#tenant-deployment-retire-reason").fill("Customer left")
    fake.fail = [(503, "operation_store_unavailable", False)]
    control(page, "#tenant-deployment-retire").click()
    confirm(page, "All data is kept.")
    retire_message = page.locator("#tenant-deployment-retire-message")
    expect(retire_message).to_contain_text("Select the same button again without changing anything")
    control(page, "#tenant-deployment-retire-reason").fill("Customer left the service")
    control(page, "#tenant-deployment-retire").click()
    expect(control(page, "#confirmation-text")).to_contain_text("Reason: Customer left the service")
    expect(control(page, "#confirmation-text")).not_to_contain_text("repeats your earlier request")
    confirm(page, "Reason: Customer left the service")
    expect(retire_message).to_contain_text("Recorded. Nothing was deployed (cloud execution is switched off, OD-002).")
    expect(retire_message).to_contain_text("Retire tenant: Recorded, nothing reserved yet")
    lost, retire = (item["body"] for item in posts(f"{DEPLOY_SUBMIT}/{tenant}/retire"))
    assert lost["idempotency_key"] != retire["idempotency_key"]
    assert retire["request"] == {"kind": "retire", "tenant_id": tenant, "expected_receipt": RECEIPT,
                                 "reason": "Customer left the service", "data_disposition": "retain"}
    assert retire["contents"] == []
    deployment_overflow(page)

    # Cancel records the outcome, then the release fails: status says cancelled but still reserved. The saved cancel
    # survives a reload in this tab, and resending it exactly (same key and body) finishes the release.
    def create_second(name):
        control(page, "#tenant-deployment-name").fill(name)
        control(page, "#tenant-deployment-writer-text").fill(WRITER)
        control(page, "#tenant-deployment-init-text").fill(INIT)
        control(page, "#tenant-deployment-create").click()
        confirm(page, "only records the request on this server")
        expect(control(page, "#tenant-deployment-name")).to_have_value("")
        operation = fake.operations[-1]["operation_id"]
        expect(control(page, "#tenant-deployment-operation")).to_have_value(operation)
        return operation

    pending_id = create_second("Fabrikam")
    pending_path = f"{DEPLOY_OPS}/{pending_id}/cancel"
    fake.release_fails = True
    control(page, "#tenant-deployment-cancel").click()
    confirm(page, f"Cancel operation {pending_id}?")
    expect(operation_message).to_contain_text("Select the same button again without changing anything")
    assert fake.operations[-1]["phase"] == "cancelled" and fake.operations[-1]["observed"]["reserved"] is True
    # The saved cancel is filed under the API address this dashboard sends requests to (here the site root), then the
    # signed-in account (the bootstrap admin token is one account for everyone holding it). It holds only the key and
    # quoted generations, never the token.
    saved = page.evaluate("() => Object.entries(sessionStorage)")
    cancel_key = lambda operation: f"vlite.tenant-deployment.cancel.{quote(MOCK_ORIGIN, safe='')}/admin_token.{operation}"
    assert [key for key, _ in saved if pending_id in key] == [cancel_key(pending_id)]
    assert not [item for item in saved if "t" * 32 in item[0] or "t" * 32 in item[1]]
    page.reload()
    sign_in_with_token(page, "admin-" + "t" * 32)
    control(page, "#admin-tenant-deployments")
    control(page, "#tenant-deployment-operation").fill(pending_id)
    control(page, "#tenant-deployment-check").click()
    expect(operation_message).to_contain_text("has not released the tenant's reservation yet")
    expect(operation_message).to_contain_text("Select Cancel to resend your earlier cancel exactly")
    expect(operation_message).to_contain_text("Everyone signed in with the bootstrap admin token counts as one account")
    expect(page.locator("#tenant-deployment-cancel")).to_be_enabled()
    control(page, "#tenant-deployment-cancel").click()
    confirm_all(page, f"Finish cancelling operation {pending_id}?", "resends your earlier cancel exactly")
    expect(operation_message).to_contain_text("Cancelled. Cloud execution is switched off (OD-002)")
    sealed, resent = (item["body"] for item in posts(pending_path))
    assert sealed == resent and fake.operations[-1]["observed"]["reserved"] is False
    expect(page.locator("#tenant-deployment-cancel")).to_be_disabled()
    assert not [key for key in page.evaluate("() => Object.keys(sessionStorage)") if pending_id in key]

    # Without the saved cancel (another tab, cleared storage) the dashboard cannot resend it and says so; Cancel then
    # sends a new cancel quoting the status read first, which finishes the release. Declining it sends nothing.
    lost_id = create_second("Northwind")
    lost_path = f"{DEPLOY_OPS}/{lost_id}/cancel"
    fake.release_fails = True
    control(page, "#tenant-deployment-cancel").click()
    confirm(page, f"Cancel operation {lost_id}?")
    expect(operation_message).to_contain_text("Select the same button again without changing anything")
    page.evaluate("() => sessionStorage.clear()")
    page.reload()
    sign_in_with_token(page, "admin-" + "t" * 32)
    control(page, "#admin-tenant-deployments")
    control(page, "#tenant-deployment-operation").fill(lost_id)
    control(page, "#tenant-deployment-check").click()
    expect(operation_message).to_contain_text("has no saved copy of the earlier cancel")
    expect(operation_message).to_contain_text("Select Cancel to send a new cancel instead")
    expect(page.locator("#tenant-deployment-cancel")).to_be_enabled()
    control(page, "#tenant-deployment-cancel").click()
    confirm_all(page, f"Finish cancelling operation {lost_id}?", "sends a new cancel that quotes the status just read",
                accept=False)
    expect(operation_message).to_have_text("Nothing was sent.")
    assert len(posts(lost_path)) == 1
    control(page, "#tenant-deployment-cancel").click()
    confirm_all(page, f"Finish cancelling operation {lost_id}?", "sends a new cancel that quotes the status just read")
    expect(operation_message).to_contain_text("Cancelled. Cloud execution is switched off (OD-002)")
    sealed, fresh = (item["body"] for item in posts(lost_path))
    lost_operation = next(item for item in fake.operations if item["operation_id"] == lost_id)
    assert fresh["idempotency_key"] != sealed["idempotency_key"]
    assert (fresh["expected_operation_generation"], fresh["expected_lifecycle_generation"]) == (
        lost_operation["operation_generation"], lost_operation["lifecycle_generation"])
    assert lost_operation["observed"]["reserved"] is False
    expect(page.locator("#tenant-deployment-cancel")).to_be_disabled()
    assert not [key for key in page.evaluate("() => Object.keys(sessionStorage)") if lost_id in key]

    # A token refused on a fresh page (here with a non-JSON 401) deletes the bootstrap admin token's saved cancels on
    # this API address, even though the dashboard never learned who was signed in: the right token can no longer resend.
    # A server older than the dashboard refuses the new cancel, and the dashboard says what to do instead.
    refused_id = create_second("Litware")
    fake.release_fails = True
    control(page, "#tenant-deployment-cancel").click()
    confirm(page, f"Cancel operation {refused_id}?")
    expect(operation_message).to_contain_text("Select the same button again without changing anything")
    page.reload()
    assert cancel_key(refused_id) in page.evaluate("() => Object.keys(sessionStorage)")
    fake.rejected_token = "wrong-" + "t" * 32
    sign_in_with_token(page, fake.rejected_token, expected=None)
    page.wait_for_function("key => sessionStorage.getItem(key) === null", arg=cancel_key(refused_id))
    expect_signed_out(page)
    fake.rejected_token = None
    sign_in_with_token(page, "admin-" + "t" * 32)
    control(page, "#admin-tenant-deployments")
    control(page, "#tenant-deployment-operation").fill(refused_id)
    control(page, "#tenant-deployment-check").click()
    expect(operation_message).to_contain_text("has no saved copy of the earlier cancel")
    fake.older_server = True
    control(page, "#tenant-deployment-cancel").click()
    confirm_all(page, f"Finish cancelling operation {refused_id}?", "sends a new cancel that quotes the status just read")
    expect(operation_message).to_contain_text("The server refused a new cancel for this operation")
    expect(operation_message).to_contain_text("report the operation ID to the server operator")
    fake.older_server = False
    assert len(posts(f"{DEPLOY_OPS}/{refused_id}/cancel")) == 2
    assert not [key for key in page.evaluate("() => Object.keys(sessionStorage)") if refused_id in key]

    # Signing out clears every form, result and kept request, and deletes this account's saved cancels: signed in
    # again with the same token, the dashboard can no longer resend that cancel.
    held_id = create_second("Contoso")
    fake.release_fails = True
    control(page, "#tenant-deployment-cancel").click()
    confirm(page, f"Cancel operation {held_id}?")
    expect(operation_message).to_contain_text("Select the same button again without changing anything")
    assert [key for key in page.evaluate("() => Object.keys(sessionStorage)") if held_id in key]
    control(page, "#tenant-deployment-name").fill("draft")
    sign_out(page)
    assert not [key for key in page.evaluate("() => Object.keys(sessionStorage)") if held_id in key]
    sign_in_with_token(page, "admin-" + "t" * 32)
    control(page, "#admin-tenant-deployments")
    for selector in ("#tenant-deployment-name", "#tenant-deployment-operation", "#tenant-deployment-readers-tenant",
                     "#tenant-deployment-retire-receipt", "#tenant-deployment-scaling-text"):
        expect(page.locator(selector)).to_have_value("")
    for selector in ("#tenant-deployment-create-message", "#tenant-deployment-readers-message",
                     "#tenant-deployment-retire-message", "#tenant-deployment-operation-message"):
        expect(page.locator(selector)).to_have_text("")
    expect(page.locator("#tenant-deployment-operation-status")).to_have_text("No operation selected.")
    expect(page.locator("#tenant-deployment-list > li")).to_have_count(0)
    control(page, "#tenant-deployment-operation").fill(held_id)
    control(page, "#tenant-deployment-check").click()
    expect(operation_message).to_contain_text("has no saved copy of the earlier cancel")
    expect(page.locator("#tenant-deployment-cancel")).to_be_enabled()
    assert len(posts(f"{DEPLOY_OPS}/{held_id}/cancel")) == 1
    # Losing authentication during a real form submission leaves no inspector contents in the hidden dashboard.
    control(page, "#tenant-deployment-name").fill("Session boundary")
    control(page, "#tenant-deployment-writer-text").fill(WRITER)
    control(page, "#tenant-deployment-init-text").fill(INIT)
    control(page, "#tenant-deployment-secrets").fill("archive-sas@7")
    fake.fail = [(401, "unauthorized", False)]
    control(page, "#tenant-deployment-create").click()
    confirm(page, "only records the request on this server")
    expect(page.locator("#app-shell")).to_be_hidden()
    expect(page.locator("#inspector")).to_have_text("")
    expect(page.locator("#tenant-deployment-writer-text")).to_have_value("")
    expect(page.locator("#tenant-deployment-secrets")).to_have_value("")
    assert not [item for item in fake.log if "/migrations" in item["path"]]
    assert not errors, errors
    context.close()


if __name__ == "__main__":
    with sync_playwright() as playwright:
        browser = playwright.chromium.launch()
        try:
            exercise(browser)
            exercise_password(browser)
            exercise_password_memory_mode(browser)
            exercise_cloud_backup(browser)
            exercise_auto_failover(browser)
            exercise_tenant_deployments(browser)
            exercise_tenant_deployment_requests(browser)
        finally:
            browser.close()
    print("PASS browser admin flows: identity badge, confirmations/cancel, tenant CRUD/usage/settings/"
          "suspend/resume/errors, TLS reload/errors, replication download/paging, sign-out clearing, "
          "role/configuration/older-server gates and mobile navigation.")
    print("PASS password sign-in: sign-in screen, errors and throttling, forced first password change, cookie-mode "
          "headers, user add/save/reset/delete with self-protection, reload restore, ended session, account-menu "
          "password change and sign-out, token fallback and setup/HTTPS hints.")
    print("PASS password sign-in with agent memory enabled: the session's refused /metrics leaves sign-in working and says so.")
    print("PASS cloud backup card: no read on connect, refresh, inline validation with focus, confirmed save with the "
          "exact policy, run/busy, stored backups, restore as a copy, hold/forced release, no overflow, sign-out "
          "clearing and role/feature gates.")
    print("PASS automatic failover card: no read on connect, refresh, cancelled and confirmed turn-on with the "
          "acknowledgement and revision, unguarded turn-off, conflict message, no overflow, sign-out clearing and "
          "unavailable/role gates.")
    print("PASS tenant deployments panel: no read on connect, paged list with plain-language status, untrusted text "
          "kept literal, no overflow at 1280/768/390, sign-out clearing, not-configured explanation and reader gate.")
    print("PASS tenant deployment requests: create/readers/retire with browser SHA-256 digests of the exact file or "
          "pasted text, declined confirmation sends nothing, unknown outcome retried with the same key and shown as "
          "already recorded, edits get a new key, cancel reads fresh tokens and explains a stale one, ended operations "
          "cannot be cancelled, a cancel whose release failed is resent exactly (even after a reload) and clears the "
          "reservation, a lost saved cancel is reported and replaced by a new cancel that finishes the release (an "
          "older server's refusal explained), list refresh, no overflow and sign-out clearing.")
