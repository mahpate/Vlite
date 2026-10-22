"""TEST-010: black-box gRPC smoke test against a real `vlite-server --grpc-bind`.

Stubs are generated from proto/vlite/v1/vlite.proto into a temporary
directory with grpc_tools.protoc (no checked-in generated code).

    python -m pip install -r tests/requirements-grpc.txt
    VLITE_SERVER_BIN=target/release/vlite-server python tests/grpc_smoke.py

VLITE_SERVER_BIN defaults to target/debug/vlite-server[.exe] and must be
built with `--features grpc` (or `--all-features`).
"""

import http.client
import importlib
import json
import os
import secrets
import signal
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

import grpc
from grpc_tools import protoc

ROOT = Path(__file__).resolve().parent.parent
# The standard grpc.health.v1 schema (grpcio-health-checking is not required).
HEALTH_PROTO = """syntax = "proto3";
package grpc.health.v1;
message HealthCheckRequest { string service = 1; }
message HealthCheckResponse {
  enum ServingStatus { UNKNOWN = 0; SERVING = 1; NOT_SERVING = 2; SERVICE_UNKNOWN = 3; }
  ServingStatus status = 1;
}
service Health { rpc Check(HealthCheckRequest) returns (HealthCheckResponse); }
"""
TOKEN = secrets.token_urlsafe(32)


def server_binary():
    configured = os.environ.get("VLITE_SERVER_BIN")
    if configured:
        return Path(configured)
    name = "vlite-server.exe" if os.name == "nt" else "vlite-server"
    return ROOT / "target" / "debug" / name


def generate(out):
    include = Path(protoc.__file__).resolve().parent / "_proto"
    # Same wire package, different file path: never shadows the grpc library.
    health = Path(out) / "health_proto" / "smoke_health"
    health.mkdir(parents=True)
    (health / "health.proto").write_text(HEALTH_PROTO)
    status = protoc.main(
        [
            "grpc_tools.protoc",
            f"-I{ROOT / 'proto'}",
            f"-I{include}",
            f"-I{Path(out) / 'health_proto'}",
            f"--python_out={out}",
            f"--grpc_python_out={out}",
            "vlite/v1/vlite.proto",
            "smoke_health/health.proto",
        ]
    )
    assert status == 0, "protoc failed"
    for package in ["vlite", "vlite/v1", "smoke_health"]:
        package = Path(out) / package
        (package / "__init__.py").touch()
    sys.path.insert(0, str(out))
    return {
        name: importlib.import_module(module)
        for name, module in [
            ("pb", "vlite.v1.vlite_pb2"),
            ("rpc", "vlite.v1.vlite_pb2_grpc"),
            ("health_pb2", "smoke_health.health_pb2"),
            ("health_rpc", "smoke_health.health_pb2_grpc"),
        ]
    }


def start(directory):
    flags = 0
    if os.name == "nt":
        flags = subprocess.CREATE_NEW_PROCESS_GROUP
    process = subprocess.Popen(
        [
            str(server_binary()),
            str(Path(directory) / "smoke.vlite"),
            "--create-if-missing",
            "2",
            "--create-format",
            "4",
            "--bind",
            "127.0.0.1:0",
            "--grpc-bind",
            "127.0.0.1:0",
        ],
        env={
            **{k: v for k, v in os.environ.items() if not k.startswith("VLITE_")},
            "VLITE_AUTH_TOKEN": TOKEN,
        },
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        creationflags=flags,
    )
    # Drain stderr (request logs) so the server never blocks on a full pipe.
    stderr = []
    threading.Thread(target=lambda: stderr.extend(process.stderr), daemon=True).start()
    lines = [process.stdout.readline().strip() for _ in range(2)]
    if not lines[0].startswith("LISTENING http://"):
        process.kill()
        raise AssertionError(f"startup failed: {lines} {''.join(stderr)}")
    assert lines[1].startswith("LISTENING grpc://"), lines
    return process, lines[0].removeprefix("LISTENING http://"), lines[1].removeprefix(
        "LISTENING grpc://"
    ), stderr


def stop(process):
    if os.name == "nt":
        process.send_signal(signal.CTRL_BREAK_EVENT)
    else:
        process.send_signal(signal.SIGTERM)
    return process.wait(timeout=60)


def rest(address, method, path, body=None):
    host, port = address.rsplit(":", 1)
    connection = http.client.HTTPConnection(host, int(port), timeout=30)
    try:
        connection.request(
            method,
            path,
            body=None if body is None else json.dumps(body),
            headers={"Authorization": "Bearer " + TOKEN, "Content-Type": "application/json"},
        )
        response = connection.getresponse()
        content = response.read()
        assert 200 <= response.status < 300, (response.status, content)
        return json.loads(content) if content else None
    finally:
        connection.close()


def expect_error(call, code, stable):
    try:
        call()
    except grpc.RpcError as error:
        assert error.code() == code, (error.code(), error.details())
        trailers = dict(error.trailing_metadata() or ())
        assert trailers.get("vlite-error-code") == stable, trailers
        return
    raise AssertionError(f"expected {code}")


def main():
    with tempfile.TemporaryDirectory() as directory:
        modules = generate(Path(directory) / "stubs")
        process, http_address, grpc_address, stderr = start(directory)
        try:
            run(modules, http_address, grpc_address)
        except BaseException:
            process.kill()
            process.wait()
            print("".join(stderr), file=sys.stderr)
            raise
        status = stop(process)
        assert status == 0, (status, "".join(stderr))
        assert all(TOKEN not in line for line in stderr), "token logged"
    print("grpc smoke: ok")


def run(modules, http_address, grpc_address):
    pb, rpc = modules["pb"], modules["rpc"]
    health_pb2 = modules["health_pb2"]
    channel = grpc.insecure_channel(grpc_address)
    grpc.channel_ready_future(channel).result(timeout=30)
    health = modules["health_rpc"].HealthStub(channel)
    for service in ["", "vlite.v1.VLite"]:
        response = health.Check(health_pb2.HealthCheckRequest(service=service), timeout=10)
        assert response.status == health_pb2.HealthCheckResponse.SERVING, response

    stub = rpc.VLiteStub(channel)
    auth = (("authorization", "Bearer " + TOKEN),)
    expect_error(
        lambda: stub.ListCollections(pb.ListCollectionsRequest(), timeout=10),
        grpc.StatusCode.UNAUTHENTICATED,
        "unauthorized",
    )
    expect_error(
        lambda: stub.ListCollections(
            pb.ListCollectionsRequest(), metadata=(("authorization", "Bearer wrong"),), timeout=10
        ),
        grpc.StatusCode.UNAUTHENTICATED,
        "unauthorized",
    )

    created = stub.CreateCollection(
        pb.CreateCollectionRequest(name="docs", dimensions=2), metadata=auth, timeout=10
    )
    assert created.name == "docs" and created.dimensions == 2, created
    written = stub.PutVector(
        pb.PutVectorRequest(
            collection="docs",
            id=1,
            vector=[1.0, 0.0],
            payload=b"\x00\xff",
            metadata={"tag": pb.MetadataValue(string_value="a")},
            expected=pb.Expected(absent=pb.Expected.Absent()),
        ),
        metadata=auth,
        timeout=10,
    )
    assert written.created, written
    vector = stub.GetVector(pb.GetVectorRequest(collection="docs", id=1), metadata=auth, timeout=10)
    assert list(vector.vector) == [1.0, 0.0] and vector.payload == b"\x00\xff", vector
    assert vector.metadata["tag"].string_value == "a", vector
    assert vector.revision == written.revision, (vector, written)
    expect_error(
        lambda: stub.PutVector(
            pb.PutVectorRequest(
                collection="docs",
                id=1,
                vector=[0.0, 1.0],
                expected=pb.Expected(absent=pb.Expected.Absent()),
            ),
            metadata=auth,
            timeout=10,
        ),
        grpc.StatusCode.ABORTED,
        "precondition_failed",
    )

    operations = [
        pb.Operation(put=pb.Operation.Put(id=i, vector=[float(i), 1.0])) for i in range(2, 66)
    ]
    stub.WriteBatch(pb.WriteBatchRequest(collection="docs", operations=operations), metadata=auth, timeout=30)

    exact = stub.Search(
        pb.SearchRequest(collection="docs", vector=[3.0, 1.0], k=3, mode=pb.SEARCH_MODE_EXACT),
        metadata=auth,
        timeout=10,
    )
    assert exact.hits[0].id == 3 and exact.hits[0].distance == 0.0, exact
    ready = pb.SearchRequest(
        collection="docs", vector=[3.0, 1.0], k=3, mode=pb.SEARCH_MODE_APPROXIMATE_READY
    )
    expect_error(
        lambda: stub.Search(ready, metadata=auth, timeout=10),
        grpc.StatusCode.FAILED_PRECONDITION,
        "index_not_ready",
    )
    rest(http_address, "POST", "/v1/collections/docs/index/hnsw", {})
    approximate = stub.Search(ready, metadata=auth, timeout=10)
    assert approximate.hits[0].id == 3, approximate

    deleted = stub.DeleteVector(
        pb.DeleteVectorRequest(
            collection="docs", id=1, expected=pb.Expected(revision=written.revision)
        ),
        metadata=auth,
        timeout=10,
    )
    assert deleted.deleted
    stub.WriteBatch(
        pb.WriteBatchRequest(
            collection="docs",
            operations=[pb.Operation(delete=pb.Operation.Delete(id=i)) for i in range(2, 66)],
        ),
        metadata=auth,
        timeout=30,
    )
    dropped = stub.DropCollection(pb.DropCollectionRequest(name="docs"), metadata=auth, timeout=10)
    assert dropped.deleted
    expect_error(
        lambda: stub.GetCollection(pb.GetCollectionRequest(name="docs"), metadata=auth, timeout=10),
        grpc.StatusCode.NOT_FOUND,
        "collection_not_found",
    )
    channel.close()


if __name__ == "__main__":
    main()
