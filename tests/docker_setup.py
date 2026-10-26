"""Exercise bootstrap branches with fake system commands; never install/run Docker."""

from pathlib import Path
import os
import shutil
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[1]
SHELL = shutil.which("sh")
if not SHELL:
    raise SystemExit("A POSIX sh is required (Git for Windows includes it).")


def run_case(name, docker, *, expected=0, message="", extra=(), root="0", ubuntu="24.04", missing=False, deploy=False, ce=False):
    with tempfile.TemporaryDirectory() as directory:
        work = Path(directory)
        commands = work / "bin"
        commands.mkdir()
        os_release = work / "os-release"
        os_release.write_text(f"ID=ubuntu\nVERSION_ID={ubuntu}\nVERSION_CODENAME=noble\n")
        (work / "scripts").mkdir()
        (work / "compose.yaml").write_text((ROOT / "compose.yaml").read_text())
        script = work / "scripts" / "setup.sh"
        source = (ROOT / "scripts" / "docker-dashboard.sh").read_text()
        # Redirect OS detection only in this isolated test copy.
        source = source.replace("/etc/os-release", f'"{os_release.as_posix()}"')
        if missing:
            source = source.replace("command -v docker >/dev/null 2>&1", '[ -f "$INSTALLED_MARKER" ]')
        script.write_text(source)
        for command, body in {
            "id": f"echo {root}",
            "docker": 'printf "%s\\n" "$*" >> "$COMMAND_LOG"\n' +
                      ('if [ "$*" = "compose version" ] && [ ! -f "$INSTALLED_MARKER" ]; then exit 1; fi\n' if ce else "") + docker,
            "dpkg-query": "echo 'install ok installed'" if ce else "exit 1",
            "apt-get": 'case "$*" in *docker-ce*|*docker-compose-plugin*) touch "$INSTALLED_MARKER" ;; esac',
            "install": "exit 0",
            "dpkg": "echo amd64",
            "curl": 'while [ "$#" -gt 0 ]; do if [ "$1" = -o ]; then echo key > "$2"; exit 0; fi; shift; done; exit 0',
        }.items():
            executable = commands / command
            executable.write_text("#!/bin/sh\n" + body + "\n")
            executable.chmod(0o755)
        env = dict(os.environ, PATH=str(commands) + os.pathsep + os.environ["PATH"],
                   INSTALLED_MARKER=(work / "installed").as_posix(),
                   TEST_COMMANDS=commands.as_posix(), COMMAND_LOG=(work / "commands.log").as_posix())
        arguments = ["--prepare-only", *extra]
        if deploy:
            arguments = ["--deploy-dir", (work / "deployment").as_posix(), "--yes", *extra]
        command = [SHELL, "-c", '''
mock_bin=$(cd "$TEST_COMMANDS" && pwd) || exit 98
export PATH="$mock_bin:$PATH"
for tool in id docker dpkg-query apt-get install dpkg curl; do
    case "$(command -v "$tool")" in "$mock_bin/"*) ;; *) exit 98 ;; esac
done
exec sh "$@"
''', "test", str(script), *arguments]
        result = subprocess.run(
            command,
            env=env, capture_output=True, text=True, input="n\n", timeout=15,
        )
        assert result.returncode == expected, (name, result.stdout, result.stderr)
        assert message in result.stdout + result.stderr, (name, result.stdout, result.stderr)
        if missing:
            assert (work / "installed").exists() == (expected == 0), name
        if deploy:
            token = work / "deployment" / ".secrets" / "api-token"
            assert len(token.read_text()) >= 40
            token.write_text("keep-existing-token")
            repeat = subprocess.run(command, env=env, capture_output=True, text=True, timeout=15)
            assert repeat.returncode == 0, (repeat.stdout, repeat.stderr)
            assert token.read_text() == "keep-existing-token"
            assert "VLITE_API_IMAGE=ghcr.io/mahpate/vlite-api:v0.5.1" in (work / "deployment" / ".env").read_text()
            calls = (work / "commands.log").read_text()
            assert "up -d --no-build --wait api dashboard" in calls
            assert "--volumes" not in calls


ready = """
case "$*" in
  "compose version") echo v2.29.0 ;;
  "compose version --short") echo 2.29.0 ;;
  "info") exit 0 ;;
  "info --format {{.OSType}}") echo linux ;;
  "compose -f compose.yaml --profile dashboard pull --quiet api dashboard") exit 0 ;;
  "compose -f compose.yaml --profile dashboard up -d --no-build --wait api dashboard") exit 0 ;;
  *) exit 91 ;;
esac
"""
run_case("existing Docker", ready)
run_case("old Compose", ready.replace("2.29.0", "2.19.0"), expected=1, message="2.20 or newer")
run_case("Windows containers", ready.replace("echo linux", "echo windows"), expected=1, message="Linux containers")
run_case("socket permissions", ready.replace('"info") exit 0', '"info") exit 1'), root="1000",
         expected=1, message="cannot access")
run_case("existing Docker missing plugin", "exit 1", expected=1, message="existing Docker/container runtime")
run_case("unsupported Ubuntu", "exit 1", ubuntu="20.04", expected=1, message="not supported")
run_case("invalid ports", ready, extra=("--api-port", "8081"), expected=1, message="ports must differ")
run_case("missing option value", ready, extra=("--api-port",), expected=1, message="requires a value")
run_case("invalid image tag", ready, extra=("--version", "$(id)"), expected=1, message="image tag")
run_case("maximum dimensions", ready, extra=("--dimensions", "65536"))
run_case("excessive dimensions", ready, extra=("--dimensions", "65537"), expected=1, message="Dimensions must be between")
run_case("fresh Docker installation", ready, missing=True, extra=("--yes",))
run_case("declined installation", ready, missing=True, expected=1, message="Installation declined")
run_case("missing Docker CE Compose plugin", ready, ce=True, extra=("--yes",))
run_case("deployment and token preservation", ready, deploy=True)

source = (ROOT / "scripts" / "docker-dashboard.sh").read_text()
assert "docker compose -f compose.yaml --profile dashboard up -d --no-build --wait" in source
assert '[ ! -s .secrets/api-token ]' in source
assert 'chmod 700 "$DEPLOY_DIR" "$DEPLOY_DIR/.secrets"' in source
assert "docker-buildx-plugin docker-compose-plugin" in source
print("Docker setup: existing/old Compose, container OS, permissions, runtime conflict, Ubuntu and argument guards passed (mocked).")
