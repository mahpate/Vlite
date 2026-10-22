"""CI helper: own a disposable server and run an opt-in Go or .NET live suite."""
import argparse
import os
from pathlib import Path
import secrets
import shutil
import socket
import subprocess
import time
from urllib.request import build_opener, ProxyHandler


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("language", choices=["go", "dotnet"])
    parser.add_argument("--server", default="target/debug/vlite-server" + (".exe" if os.name == "nt" else ""))
    args = parser.parse_args()
    root = Path(__file__).resolve().parents[2]
    work = root / "clients" / "go" / ".live-test" / secrets.token_hex(8)
    work.mkdir(parents=True)
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        port = sock.getsockname()[1]
    # Never inherit an admin directory/restore selection, token files or schedules.
    env = {key: value for key, value in os.environ.items() if not key.startswith("VLITE_")}
    env.update(VLITE_AUTH_TOKEN=secrets.token_urlsafe(32), VLITE_URL=f"http://127.0.0.1:{port}")
    opener = build_opener(ProxyHandler({}))
    process = None
    try:
        with (work / "server.log").open("w+") as log:
            process = subprocess.Popen([str((root / args.server).resolve()), str(work / "vectors.vlite"),
                "--create-if-missing", "3", "--create-format", "4", "--bind", f"127.0.0.1:{port}"],
                env=env, stdout=log, stderr=subprocess.STDOUT, cwd=root)
            for _ in range(200):
                if process.poll() is not None:
                    log.seek(0)
                    raise RuntimeError("server exited: " + log.read())
                try:
                    with opener.open(env["VLITE_URL"] + "/health", timeout=1) as response:
                        if response.status == 200:
                            break
                except OSError:
                    time.sleep(0.1)
            else:
                raise RuntimeError("server did not become healthy")
            if args.language == "go":
                command, cwd = ["go", "test", "./...", "-count=1", "-v"], root / "clients" / "go"
            else:
                command, cwd = ["dotnet", "run", "--project", "clients/dotnet/VLite.Client.Tests"], root
            subprocess.run(command, cwd=cwd, env=env, check=True)
    finally:
        if process is not None and process.poll() is None:
            process.terminate()
            try:
                process.wait(timeout=15)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()
        shutil.rmtree(work)


if __name__ == "__main__":
    main()
