"""Install a REST wheel offline and smoke-test only the installed package."""
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import venv

wheel = Path(sys.argv[1]).resolve(strict=True)
with tempfile.TemporaryDirectory(prefix="vlite-client-wheel-") as temporary:
    directory = Path(temporary)
    venv.EnvBuilder(with_pip=True).create(directory / "venv")
    python = directory / "venv" / ("Scripts/python.exe" if os.name == "nt" else "bin/python")
    subprocess.run([str(python), "-m", "pip", "install", "--no-deps", "--no-index", str(wheel)], check=True, timeout=120)
    subprocess.run([str(python), "-I", "-c", """
from unittest.mock import patch
from vlite_client import Client, VLiteError
client = Client("http://127.0.0.1:8080", "a" * 32)
with patch.object(client, "_request", return_value={"id":"18446744073709551615"}) as request:
    assert client.get("18446744073709551615")["id"] == "18446744073709551615"
    assert request.call_args.args[:2] == ("GET", "/v1/vectors/18446744073709551615")
assert issubclass(VLiteError, Exception)
print("PASS offline installed REST Python wheel")
"""], cwd=directory, check=True, timeout=60)
