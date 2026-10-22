"""Real HTTPS proxy acceptance with a trusted local CA; no public DNS required."""

import http.client
import json
import os
import ssl
import subprocess
import tempfile
import time
import sys

sys.dont_write_bytecode = True
import rest_e2e as rest


def run(directory):
    token_path = os.path.join(directory, "token")
    with open(token_path, "w", encoding="ascii") as output:
        output.write(rest.TOKEN)
    os.chmod(token_path, 0o644)
    rest.ENV.update(
        COMPOSE_FILE="compose.yaml:compose.tls.yaml",
        COMPOSE_PROFILES="api",
        VLITE_TOKEN_FILE=token_path,
        VLITE_DOMAIN="localhost",
        VLITE_TLS_BIND="127.0.0.1",
        VLITE_TLS_HTTP_PORT="0",
        VLITE_TLS_HTTPS_PORT="0",
    )
    rest.compose("up", "--no-build" if os.environ.get("VLITE_E2E_NO_BUILD") else "--build",
                 "--detach", "api", "caddy")
    address = rest.compose("port", "caddy", "443").stdout.strip()
    port = int(address.rsplit(":", 1)[1])
    container = rest.compose("ps", "--quiet", "caddy").stdout.strip()
    root = os.path.join(directory, "root.crt")
    deadline = time.monotonic() + 120
    while True:
        copied = subprocess.run(
            ["docker", "cp", container + ":/data/caddy/pki/authorities/local/root.crt", root],
            capture_output=True, timeout=10,
        )
        if copied.returncode == 0:
            break
        if time.monotonic() >= deadline:
            raise AssertionError("Caddy local CA not ready")
        time.sleep(0.5)
    context = ssl.create_default_context(cafile=root)

    def request(method, path, body=None, authenticated=True):
        connection = http.client.HTTPSConnection("localhost", port, context=context, timeout=10)
        try:
            headers = {"Content-Type": "application/json"}
            if authenticated:
                headers["Authorization"] = "Bearer " + rest.TOKEN
            connection.request(method, path, None if body is None else json.dumps(body), headers)
            response = connection.getresponse()
            content = response.read()
            assert response.getheader("Strict-Transport-Security") == "max-age=31536000"
            assert response.getheader("X-Request-ID")
            return response.status, json.loads(content) if content else None
        finally:
            connection.close()

    deadline = time.monotonic() + 120
    while True:
        try:
            if request("GET", "/ready")[0] == 200:
                break
        except (OSError, http.client.HTTPException):
            pass
        if time.monotonic() >= deadline:
            raise AssertionError("TLS proxy readiness timed out")
        time.sleep(0.5)
    assert request("GET", "/v1/database", authenticated=False)[0] == 401
    assert request("PUT", "/v1/vectors/1", {"vector": [1, 0, 0]})[0] == 204
    assert request("GET", "/v1/vectors/1")[1]["vector"] == [1, 0, 0]
    assert request("GET", "/metrics")[0] == 200
    print("PASS HTTPS: verified certificate chain and hostname, bearer authentication, CRUD, metrics, HSTS")


if __name__ == "__main__":
    with tempfile.TemporaryDirectory(prefix="vlite-tls-") as directory:
        try:
            run(directory)
        except subprocess.CalledProcessError as error:
            print(error.stdout)
            print(error.stderr)
            raise
        finally:
            logs = rest.compose("logs", "--no-color", check=False)
            print(logs.stdout.replace(rest.TOKEN, "[REDACTED]"))
            rest.compose("down")
            assert rest.TOKEN not in logs.stdout and rest.TOKEN not in logs.stderr
            print(f"Retained TLS test volumes for project: {rest.PROJECT}")
