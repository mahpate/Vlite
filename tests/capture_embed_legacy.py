"""Capture raw embed responses from an independently built v0.6.1 server."""

import argparse
import http.client
import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import time


def request(port, path, body):
    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=5)
    try:
        connection.request(
            "POST", path, json.dumps(body).encode(), {"Content-Type": "application/json"}
        )
        response = connection.getresponse()
        return response.status, response.read()
    finally:
        connection.close()


def capture(binary, output, check=False):
    if not check:
        output.mkdir(parents=True, exist_ok=True)
    expected_cases = {
        case["file"]: case
        for case in json.loads((output / "cases.json").read_text(encoding="utf-8"))
    } if check else {}
    cases = []
    failures = []
    for dimensions in (3, 7, 16):
        with tempfile.TemporaryDirectory() as directory:
            with socket.socket() as reservation:
                reservation.bind(("127.0.0.1", 0))
                port = reservation.getsockname()[1]
            env = {k: v for k, v in os.environ.items() if not k.startswith("VLITE_")}
            command = [
                str(binary),
                str(Path(directory) / "db.vlite"),
                "--create-if-missing", str(dimensions),
                "--durability", "none",
                "--insecure-local",
                "--bind", f"127.0.0.1:{port}",
            ]
            process = subprocess.Popen(
                command, env=env, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE
            )
            try:
                for _ in range(200):
                    if process.poll() is not None:
                        raise RuntimeError(process.stderr.read().decode())
                    try:
                        with socket.create_connection(("127.0.0.1", port), timeout=0.1):
                            break
                    except OSError:
                        time.sleep(0.05)
                else:
                    raise TimeoutError("server did not start")
                requests = [
                    ("default", {"text": "alpha beta gamma delta epsilon"}),
                    ("hashing", {"text": "rust vector database repeated repeated", "provider": "hashing"}),
                    ("empty_text", {"text": ""}),
                    ("unicode", {"text": "caf\u00e9 \u6771\u4eac vectors", "provider": None}),
                ]
                if dimensions == 3:
                    requests += [
                        ("provider_empty", {"text": "hello", "provider": ""}),
                        ("provider_malformed", {"text": "hello", "provider": "bad/name"}),
                        ("provider_whitespace", {"text": "hello", "provider": " "}),
                        ("provider_unknown", {"text": "hello", "provider": "unknown"}),
                        ("provider_unsupported", {"text": "hello", "provider": "openai"}),
                        ("provider_overlong", {"text": "hello", "provider": "a" * 257}),
                        ("provider_long_text", {"text": "x" * 16385, "provider": "unknown"}),
                    ]
                for name, body in requests:
                    for route, path in (
                        ("collection", "/v1/collections/default/embed"),
                        ("legacy", "/v1/embed"),
                    ):
                        filename = f"d{dimensions}_{name}_{route}.json"
                        try:
                            status, response = request(port, path, body)
                        except http.client.HTTPException:
                            process.terminate()
                            _, errors = process.communicate(timeout=10)
                            raise RuntimeError(
                                f"{filename}: {errors.decode()}"
                            ) from None
                        case = {
                            "file": filename,
                            "dimensions": dimensions,
                            "path": path,
                            "request": body,
                            "status": status,
                        }
                        if check:
                            if (
                                case != expected_cases[filename]
                                or response != (output / filename).read_bytes()
                            ):
                                failures.append(
                                    f"{filename}: expected {expected_cases[filename]['status']}, "
                                    f"got {status}: {response.decode()}"
                                )
                        else:
                            (output / filename).write_bytes(response)
                        cases.append(case)
            finally:
                process.terminate()
                process.communicate(timeout=10)
    if check:
        if failures:
            raise AssertionError("\n".join(failures))
        print(f"Verified {len(cases)} raw responses from {binary}")
    else:
        (output / "cases.json").write_text(
            json.dumps(cases, indent=2, ensure_ascii=True) + "\n", encoding="utf-8"
        )
        print(f"Captured {len(cases)} responses from {binary}")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    parser.add_argument("--check", action="store_true")
    parser.add_argument(
        "--output", type=Path,
        default=Path(__file__).parent / "fixtures" / "embed_legacy",
    )
    arguments = parser.parse_args()
    capture(arguments.binary.resolve(), arguments.output, arguments.check)
