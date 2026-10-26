"""Exercise production admin UI against deterministic API responses in Chromium.

Run on a hosted runner: python tests/admin_e2e.py.
No Docker, real credentials, server, or database is used.
"""

import time

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


if __name__ == "__main__":
    with sync_playwright() as playwright:
        browser = playwright.chromium.launch()
        try:
            exercise(browser)
            exercise_password(browser)
        finally:
            browser.close()
    print("PASS browser admin flows: identity badge, confirmations/cancel, tenant CRUD/usage/settings/"
          "suspend/resume/errors, TLS reload/errors, replication download/paging, sign-out clearing, "
          "role/configuration/older-server gates and mobile navigation.")
    print("PASS password sign-in: sign-in screen, errors and throttling, forced first password change, cookie-mode "
          "headers, user add/save/reset/delete with self-protection, reload restore, ended session, account-menu "
          "password change and sign-out, token fallback and setup/HTTPS hints.")
