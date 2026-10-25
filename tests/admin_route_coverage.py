"""Static route/control inventory, not a substitute for authenticated API/browser tests."""

import re
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]


def admin_routes(source):
    """Read balanced .route calls, including chained .get/.post method routers."""
    for match in re.finditer(r'\.route\(\s*"(/v1/admin/[^"]+)"\s*,', source):
        start, depth = match.end(), 1
        quoted = escaped = False
        end = start
        for end in range(start, len(source)):
            char = source[end]
            if quoted:
                if escaped:
                    escaped = False
                elif char == "\\":
                    escaped = True
                elif char == '"':
                    quoted = False
            elif char == '"':
                quoted = True
            elif char == "(":
                depth += 1
            elif char == ")":
                depth -= 1
                if depth == 0:
                    break
        assert depth == 0, f"Unbalanced route: {match.group(1)}"
        for method in re.findall(r"\b(get|post|put|delete|patch|head)\s*\(", source[start:end]):
            yield method.upper(), match.group(1)


# A named usable control per action; the generic request playground never counts.
# Handler checks include loop-bound controls and observability's shared refresh.
INVENTORY = {
    ("GET", "/v1/admin/tokens"): ("console-tools.js", "token-status"),
    ("POST", "/v1/admin/tokens/reload"): ("console-tools.js", "token-reload"),
    ("POST", "/v1/admin/tokens/revoke-read-only"): ("console-tools.js", "token-revoke"),
    ("GET", "/v1/admin/audit"): ("console-tools.js", "read-audit"),
    ("GET", "/v1/admin/audit/persistent"): ("console-tools.js", "persistent-audit"),
    ("GET", "/v1/admin/backups"): ("console-tools.js", "list-backups"),
    ("POST", "/v1/admin/backups"): ("console-tools.js", "create-backup"),
    ("POST", "/v1/admin/restores"): ("console-tools.js", "stage-restore"),
    ("POST", "/v1/admin/restores/select"): ("console-tools.js", "select-restore"),
    ("GET", "/v1/admin/maintenance/auto"): ("console-tools.js", "auto-maintenance-read"),
    ("PUT", "/v1/admin/maintenance/auto"): ("console-tools.js", "auto-maintenance-save"),
    ("POST", "/v1/admin/maintenance/auto/tick"): ("console-tools.js", "auto-maintenance-tick"),
    ("GET", "/v1/admin/backups/schedule"): ("console-tools.js", "backup-schedule-read"),
    ("PUT", "/v1/admin/backups/schedule"): ("console-tools.js", "backup-schedule-save"),
    ("POST", "/v1/admin/backups/schedule/run"): ("console-tools.js", "backup-schedule-run"),
    ("POST", "/v1/admin/backups/{name}/verify"): ("console-tools.js", "backup-verify"),
    ("POST", "/v1/admin/backups/{name}/drill"): ("console-tools.js", "backup-drill"),
    ("GET", "/v1/admin/credentials"): ("console-tools.js", "cred-list"),
    ("POST", "/v1/admin/credentials"): ("console-tools.js", "cred-create"),
    ("POST", "/v1/admin/credentials/{id}/rotate"): ("console-tools.js", "cred-table"),
    ("POST", "/v1/admin/credentials/{id}/revoke"): ("console-tools.js", "cred-table"),
    ("GET", "/v1/admin/webhooks"): ("console-tools.js", "webhook-list"),
    ("POST", "/v1/admin/webhooks"): ("console-tools.js", "webhook-create"),
    ("GET", "/v1/admin/webhooks/{id}"): ("console-tools.js", "webhook-status"),
    ("DELETE", "/v1/admin/webhooks/{id}"): ("console-tools.js", "webhook-delete"),
    ("GET", "/v1/admin/diagnostics/slow-queries"): ("console-tools.js", "slow-queries"),
    ("GET", "/v1/admin/logs"): ("observability.js", "obs-refresh"),
    ("GET", "/v1/admin/metrics/summary"): ("observability.js", "obs-refresh"),
    ("GET", "/v1/admin/tenants"): ("admin-tools.js", "tenant-list"),
    ("POST", "/v1/admin/tenants"): ("admin-tools.js", "tenant-create"),
    ("DELETE", "/v1/admin/tenants/{id}"): ("admin-tools.js", "tenant-delete"),
    ("GET", "/v1/admin/tenants/{id}/usage"): ("admin-tools.js", "tenant-usage"),
    ("PUT", "/v1/admin/tenants/{id}/settings"): ("admin-tools.js", "tenant-save-settings"),
    ("POST", "/v1/admin/tenants/{id}/suspend"): ("admin-tools.js", "tenant-suspend"),
    ("POST", "/v1/admin/tenants/{id}/resume"): ("admin-tools.js", "tenant-resume"),
    ("POST", "/v1/admin/tls/reload"): ("admin-tools.js", "tls-reload"),
    ("GET", "/v1/admin/replication/export"): ("admin-tools.js", "replication-export"),
}


def main():
    sources = [ROOT / "src" / "server.rs", *sorted((ROOT / "src" / "server").glob("*.rs"))]
    registered = {route for path in sources for route in admin_routes(path.read_text(encoding="utf-8"))}
    assert registered == set(INVENTORY), (
        f"Admin route inventory drift: missing controls={registered - set(INVENTORY)}; "
        f"removed server routes={set(INVENTORY) - registered}"
    )
    dashboard = ROOT / "src" / "dashboard"
    markup = (dashboard / "index.html").read_text(encoding="utf-8")
    scripts = {name: (dashboard / name).read_text(encoding="utf-8") for name, _ in INVENTORY.values()}
    for (method, route), (script, control) in INVENTORY.items():
        assert f'id="{control}"' in markup, f"{method} {route}: missing UI #{control}"
        assert f'"{control}"' in scripts[script], f"{method} {route}: missing handler #{control}"
        prefix = route.split("{", 1)[0]
        assert prefix in scripts[script], f"{method} {route}: missing request path in {script}"
    console = scripts["console-tools.js"]
    for control in ["pitr-run", "cred-tenant", "cred-rpm", "cred-max-vectors", "cred-max-vectors-per-collection"]:
        assert f'id="{control}"' in markup and f'"{control}"' in console, f"Missing admin input/control #{control}"
    for verb in ["rotate", "revoke"]:
        assert f'"{verb}"' in console and f"/{verb}`" in console, f"Missing credential {verb} action"
    print(f"PASS static admin route inventory: {len(registered)} method/actions across "
          f"{len({route for _, route in registered})} paths; named controls, not playground")


if __name__ == "__main__":
    main()
