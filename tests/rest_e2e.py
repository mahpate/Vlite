"""Docker REST lifecycle test; Python standard library only. Keeps the data volume."""

import http.client
import json
import os
import subprocess
import time
import uuid


PROJECT = "vlite-rest-" + uuid.uuid4().hex[:10]
ENV = dict(os.environ, VLITE_PORT="0", VLITE_DIMENSIONS="3")


def compose(*args, check=True):
    return subprocess.run(
        ["docker", "compose", "-p", PROJECT, *args],
        env=ENV,
        check=check,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        timeout=600,
    )


def request(port, method, path, body=None, expected=200):
    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=10)
    try:
        connection.request(
            method,
            path,
            body=None if body is None else json.dumps(body),
            headers={"Content-Type": "application/json"},
        )
        response = connection.getresponse()
        content = response.read()
        assert response.status == expected, (response.status, content)
        return json.loads(content) if content else None
    finally:
        connection.close()


def ready(port):
    deadline = time.monotonic() + 30
    while time.monotonic() < deadline:
        try:
            request(port, "GET", "/ready")
            return
        except (OSError, http.client.HTTPException):
            time.sleep(0.2)
    raise AssertionError("REST readiness timed out")


def stopped_cleanly():
    compose("stop", "api")
    container = compose("ps", "--all", "--quiet", "api").stdout.strip()
    result = subprocess.run(
        ["docker", "inspect", "--format", "{{.State.ExitCode}}", container],
        text=True,
        capture_output=True,
        check=True,
        timeout=30,
    )
    assert result.stdout.strip() == "0", result.stdout


def published_port():
    address = compose("port", "api", "8080").stdout.strip()
    assert address.startswith("127.0.0.1:"), address
    return int(address.rsplit(":", 1)[1])


def run():
    compose("up", "--build", "--detach", "api")
    port = published_port()
    ready(port)
    assert request(port, "GET", "/v1/database") == {
        "dimensions": 3, "format_version": 3, "records": 0
    }
    request(port, "PUT", "/v1/vectors/18446744073709551615",
            {"vector": [1, 0, 0], "payload_base64": "AP8=", "metadata": {
                "category": {"type": "string", "value": "docs"},
                "count": {"type": "integer", "value": "9223372036854775807"}
            }}, expected=204)
    request(port, "POST", "/v1/batches", {"operations": [
        {"op": "put", "id": "1", "vector": [0, 1, 0]},
        {"op": "put", "id": "2", "vector": [0, 0, 1]},
        {"op": "delete", "id": "2"},
    ]}, expected=204)
    request(port, "POST", "/v1/batches", {"operations": [
        {"op": "delete", "id": "1"},
        {"op": "put", "id": "3", "vector": [1]},
    ]}, expected=422)
    request(port, "GET", "/v1/vectors/01", expected=400)
    assert request(port, "POST", "/v1/search", {"vector": [0, 1, 0], "k": 2})["hits"] == [
        {"id": "1", "distance": 0.0},
        {"id": "18446744073709551615", "distance": 2.0},
    ]
    approximate = {"vector": [0, 1, 0], "k": 2, "mode": "approximate", "ef_search": 100}
    assert request(port, "POST", "/v1/search", approximate)["hits"][0]["id"] == "1"
    filtered = {"vector": [0, 1, 0], "k": 1, "filter": [
        {"op": "eq", "field": "category", "value": {"type": "string", "value": "docs"}}
    ]}
    assert request(port, "POST", "/v1/search", filtered)["hits"][0]["id"] == "18446744073709551615"
    request(port, "POST", "/v1/search", dict(filtered, mode="approximate"), expected=422)
    assert request(port, "GET", "/openapi.json")["openapi"] == "3.1.0"
    stopped_cleanly()
    mismatch = compose(
        "run", "--rm", "--no-deps", "api", "/data/vectors.vlite",
        "--create-if-missing", "4", "--bind", "127.0.0.1:0", check=False
    )
    assert mismatch.returncode != 0 and "expected 4" in mismatch.stderr, mismatch
    compose("start", "api")
    port = published_port()
    ready(port)
    assert request(port, "GET", "/v1/vectors/18446744073709551615")["payload_base64"] == "AP8="
    assert request(port, "GET", "/v1/vectors/18446744073709551615")["metadata"]["count"]["value"] == "9223372036854775807"
    assert request(port, "POST", "/v1/search", filtered)["hits"][0]["id"] == "18446744073709551615"
    assert request(port, "GET", "/v1/database")["records"] == 2
    assert request(port, "POST", "/v1/search", approximate)["hits"][0]["id"] == "1"
    request(port, "GET", "/v1/vectors/2", expected=404)
    assert request(port, "DELETE", "/v1/vectors/1")["deleted"] is True
    assert request(port, "DELETE", "/v1/vectors/1")["deleted"] is False
    assert request(port, "POST", "/v1/search", approximate)["hits"][0]["id"] == "18446744073709551615"
    stopped_cleanly()
    compose("start", "api")
    port = published_port()
    ready(port)
    assert request(port, "GET", "/v1/database")["records"] == 1
    request(port, "GET", "/v1/vectors/1", expected=404)
    stopped_cleanly()
    print("PASS REST: CRUD, typed metadata, exact filters, atomic rejection, ANN filter rejection, contract, restart persistence, SIGTERM")


if __name__ == "__main__":
    try:
        run()
    except subprocess.CalledProcessError as error:
        print(error.stdout)
        print(error.stderr)
        raise
    finally:
        logs = compose("logs", "--no-color", "api", check=False)
        print(logs.stdout)
        compose("down")
        print(f"Retained Docker data volume: {PROJECT}_api-data")
