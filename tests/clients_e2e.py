"""Run both clients against isolated authenticated native servers."""
import os
from pathlib import Path
import queue
import re
import secrets
import subprocess
import sys
import tempfile
import threading

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "clients" / "python"))
from vlite_client import Client, VLiteError  # noqa: E402


def python_test(base, token):
    client = Client(base, token)
    assert client.health()["status"] == "ok"
    assert client.ready()["status"] == "ready"
    assert client.database()["dimensions"] == 3
    assert not client.index_status()["ready"]
    client.put("0", {"vector": [0, 0, 1]})
    try:
        client.search({"vector": [1, 0, 0], "k": 1, "mode": "approximate_ready"})
        raise AssertionError("Expected index_not_ready")
    except VLiteError as error:
        assert error.code == "index_not_ready" and error.status == 409
    record = {"vector": [1, 0, 0], "payload_base64": "AP8=",
              "metadata": {"n": {"type": "integer", "value": "-9223372036854775808"}}}
    client.put("18446744073709551615", record)
    assert client.get("18446744073709551615") == dict(id="18446744073709551615", **record)
    assert client.search({"vector": [1, 0, 0], "k": 1, "metric": "cosine"})[0]["distance"] == 0
    assert client.search({"vector": [1, 0, 0], "k": 1, "metric": "inner_product"})[0]["distance"] == -1
    client.batch([{"op": "put", "id": "2", "vector": [0, 1, 0]}, {"op": "delete", "id": "0"}])
    first = client.list_vectors(limit=1)
    assert first["items"][0]["id"] == "2" and first["next_after"] == "2"
    last = client.list_vectors(after=first["next_after"], limit=1)
    assert last["items"][0]["id"] == "18446744073709551615" and last["next_after"] is None
    assert client.search({"vector": [1, 0, 0], "k": 2, "filter": [
        {"op": "eq", "field": "n", "value": record["metadata"]["n"]},
    ]})[0]["id"] == "18446744073709551615"
    client.build_index()
    assert client.index_status()["ready"]
    for mode in ["approximate", "approximate_ready"]:
        assert client.search({"vector": [1, 0, 0], "k": 1, "mode": mode})[0]["distance"] == 0
    client.build_index(metric="cosine")
    assert client.index_status()["metric"] == "cosine"
    assert client.search({"vector": [1,0,0], "k": 1, "mode": "approximate_ready", "metric": "cosine"})[0]["distance"] == 0
    try:
        client.search({"vector": [1,0,0], "k": 1, "mode": "approximate_ready"})
        raise AssertionError("Expected metric mismatch")
    except VLiteError as error:
        assert error.code == "index_not_ready"
    assert client.metrics()["database_healthy"]
    for action, status in [
        (lambda: Client(base, "b" * 32).database(), 401),
        (lambda: client.batch([{"op": "put", "id": "3", "vector": [1, 2]}]), 422),
    ]:
        try:
            action()
            raise AssertionError("Expected structured error")
        except VLiteError as error:
            assert error.status == status and error.request_id
    assert client.database()["records"] == 2
    assert client.delete("2") and not client.delete("2")
    try:
        client.get("2")
        raise AssertionError("Expected not found")
    except VLiteError as error:
        assert error.status == 404
    conditional_test(client)
    print("PASS Python live REST client")


def expect_412(action):
    try:
        action()
    except VLiteError as error:
        assert error.status == 412 and error.code == "precondition_failed" and error.request_id, error
        return
    raise AssertionError("Expected 412 precondition_failed")


def conditional_test(client):
    """Separate records; leaves the record count as it found it."""
    before = client.database()["records"]
    wide, other, gone = "18446744073709551614", "100", "101"
    client.put(wide, {"vector": [1, 0, 0]})
    first = client.get_versioned(wide)
    assert first["record"] == client.get(wide) == {"id": wide, "vector": [1, 0, 0],
                                                   "payload_base64": "", "metadata": {}}
    assert re.fullmatch(r'"[\x21\x23-\x7e]+"', first["etag"]), "strong opaque tag"
    assert client.get_versioned(wide)["etag"] == first["etag"], "stable without writes"

    client.put(wide, {"vector": [0, 1, 0]}, if_match=first["etag"])
    assert client.get(wide)["vector"] == [0, 1, 0]
    expect_412(lambda: client.put(wide, {"vector": [0, 0, 1]}, if_match=first["etag"]))
    expect_412(lambda: client.delete(wide, if_match=first["etag"]))
    assert client.get(wide)["vector"] == [0, 1, 0]

    current = client.get_versioned(wide)["etag"]
    client.put(other, {"vector": [1, 1, 0]})  # unrelated write invalidates every tag
    expect_412(lambda: client.delete(wide, if_match=current))
    current = client.get_versioned(wide)["etag"]
    client.batch([{"op": "put", "id": gone, "vector": [1, 0, 1]}])
    expect_412(lambda: client.put(wide, {"vector": [0, 0, 1]}, if_match=current))
    assert client.get(wide)["vector"] == [0, 1, 0]

    stale = client.get_versioned(gone)["etag"]
    assert client.delete(gone)
    expect_412(lambda: client.put(gone, {"vector": [1, 0, 0]}, if_match=stale))
    try:
        client.get(gone)
        raise AssertionError("conditional put must not recreate a deleted record")
    except VLiteError as error:
        assert error.status == 404

    def race(calls):
        barrier, results = threading.Barrier(len(calls)), [None] * len(calls)

        def run(index):
            barrier.wait()
            try:
                results[index] = ("ok", calls[index]())
            except VLiteError as error:
                results[index] = ("err", error.status, error.code)
        threads = [threading.Thread(target=run, args=(i,)) for i in range(len(calls))]
        for thread in threads:
            thread.start()
        for thread in threads:
            thread.join(30)
        assert all(r is not None for r in results), results
        winners = [i for i, r in enumerate(results) if r[0] == "ok"]
        assert len(winners) == 1, results
        assert all(r == ("err", 412, "precondition_failed") for i, r in enumerate(results) if i != winners[0]), results
        return winners[0], results[winners[0]][1]

    writers = 8
    tag = client.get_versioned(wide)["etag"]
    winner, _ = race([lambda i=i: client.put(wide, {"vector": [i + 2, 0, 0]}, if_match=tag) for i in range(writers)])
    assert client.get(wide)["vector"] == [winner + 2, 0, 0]
    tag = client.get_versioned(wide)["etag"]
    _, deleted = race([lambda: client.delete(wide, if_match=tag) for _ in range(writers)])
    assert deleted is True
    expect_412(lambda: client.delete(wide, if_match=tag))

    tag = client.get_versioned(other)["etag"]
    assert client.delete(other, if_match=tag) is True
    assert client.database()["records"] == before
    print("PASS Python live conditional writes")


def main():
    binary = Path(sys.argv[1]).resolve()
    for language in ["python", "javascript"]:
        with tempfile.TemporaryDirectory(prefix="vlite-client-") as directory:
            token = secrets.token_urlsafe(32)
            env = dict(os.environ, VLITE_AUTH_TOKEN=token)
            env.pop("VLITE_AUTH_TOKEN_FILE", None)
            process = subprocess.Popen(
                [str(binary), str(Path(directory) / "data.vlite"), "--create-if-missing", "3",
                 "--bind", "127.0.0.1:0"],
                stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True, env=env)
            try:
                lines = queue.Queue()
                threading.Thread(target=lambda: lines.put(process.stdout.readline()), daemon=True).start()
                line = lines.get(timeout=30).strip()
                assert line.startswith("LISTENING http://"), "Server did not start"
                base = line.removeprefix("LISTENING ")
                if language == "python":
                    python_test(base, token)
                else:
                    subprocess.run(["node", str(ROOT / "clients" / "javascript" / "e2e.mjs")],
                                   env=dict(env, VLITE_URL=base), check=True, timeout=60)
            finally:
                if process.poll() is None:
                    process.terminate()
                process.wait(timeout=15)
                process.stdout.close()


if __name__ == "__main__":
    main()
