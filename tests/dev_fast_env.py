"""The opt-in scripts/dev-fast-env.* files set, report, respect and undo exactly what they say.

Standard library only; each script runs in a real shell with a scrubbed environment, a stub
sccache on or off the PATH, and no cargo build. POSIX shells are tested on Linux and macOS,
Windows PowerShell and PowerShell 7 on Windows.

Usage: python tests/dev_fast_env.py
"""

import os
from pathlib import Path
import re
import shutil
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
SH = ROOT / "scripts" / "dev-fast-env.sh"
PS1 = ROOT / "scripts" / "dev-fast-env.ps1"
SET_BY_SCRIPT = ("RUSTC_WRAPPER", "CARGO_INCREMENTAL", "CARGO_PROFILE_DEV_DEBUG", "CARGO_PROFILE_TEST_DEBUG")
NAMES = "|".join(SET_BY_SCRIPT) + r"|CARGO_TARGET_[A-Z0-9_]+_LINKER|VLITE_DEV_FAST_SET"
WATCHED = f"^({NAMES})$"
ON_WINDOWS = os.name == "nt"

POSIX_BODY = f"""
. "$VLITE_SCRIPT" || exit 3
echo '--- enabled'
env | grep -E '^({NAMES})=' || true
if [ "${{VLITE_TEST_OFF:-}}" = 1 ]; then
    vlite_dev_fast_off
    echo '--- undone'
    env | grep -E '^({NAMES})=' || true
fi
"""

POWERSHELL_BODY = f"""
$watched = '{WATCHED}'
function Show {{ Get-ChildItem Env: | Where-Object {{ $_.Name -match $watched }} | ForEach-Object {{ "$($_.Name)=$($_.Value)" }} }}
. $env:VLITE_SCRIPT
'--- enabled'
Show
if ($env:VLITE_TEST_OFF -eq '1') {{
    . $env:VLITE_SCRIPT -Off
    '--- undone'
    Show
}}
"""


def shells():
    """(label, argv prefix, script, body) for every shell this platform can test."""
    found = []
    if ON_WINDOWS:
        for label in ("powershell", "pwsh"):
            exe = shutil.which(label)
            if exe:
                found.append((label, [exe, "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass",
                                      "-Command"], PS1, POWERSHELL_BODY))
    else:
        for label in ("bash", "sh", "zsh"):
            exe = shutil.which(label)
            if exe:
                found.append((label, [exe, "-c"], SH, POSIX_BODY))
    return found


def parse(block):
    return dict(line.split("=", 1) for line in block.splitlines() if "=" in line)


class Result:
    def __init__(self, completed):
        before, _, rest = completed.stdout.partition("--- enabled")
        enabled, _, undone = rest.partition("--- undone")
        self.said = before
        self.enabled = parse(enabled)
        self.undone = parse(undone)


class Environment:
    """A scrubbed environment, with an optional stub sccache in front of the PATH."""

    def __init__(self, test, with_sccache):
        self.directory = Path(tempfile.mkdtemp(prefix="dev-fast-"))
        test.addCleanup(shutil.rmtree, self.directory, True)
        dirs = [d for d in os.environ.get("PATH", "").split(os.pathsep) if d and not shutil.which("sccache", path=d)]
        if with_sccache:
            if ON_WINDOWS:
                (self.directory / "sccache.cmd").write_text("@exit /b 0\r\n")
            else:
                stub = self.directory / "sccache"
                stub.write_text("#!/bin/sh\nexit 0\n")
                stub.chmod(0o755)
            dirs.insert(0, str(self.directory))
        self.path = os.pathsep.join(dirs)

    def run(self, shell, extra=None, off=False):
        label, argv, script, body = shell
        env = {k: v for k, v in os.environ.items()
               if not re.match(WATCHED, k) and not k.startswith("VLITE_DEV_FAST_")}
        env.update(PATH=self.path, VLITE_SCRIPT=str(script), VLITE_TEST_OFF="1" if off else "0")
        env.update(extra or {})
        completed = subprocess.run(argv + [body], env=env, capture_output=True, text=True, timeout=180)
        assert completed.returncode == 0, (label, completed.stdout, completed.stderr)
        return Result(completed)


class DevFastScripts(unittest.TestCase):
    def check_each_shell(self, check):
        found = shells()
        if not found:
            self.skipTest("no shell to test the dev-fast scripts with on this platform")
        for shell in found:
            with self.subTest(shell=shell[0]):
                check(shell)

    def test_usage_comment_is_ten_lines_or_fewer(self):
        for path in (SH, PS1):
            lines = path.read_text(encoding="utf-8").splitlines()
            comment = 0
            while comment < len(lines) and lines[comment].startswith("#"):
                comment += 1
            self.assertTrue(6 <= comment <= 10, (path.name, comment))

    def test_no_cargo_config_is_committed(self):
        # A rustc-wrapper or linker in .cargo/config.toml would break contributors who lack the tool.
        for name in ("config.toml", "config"):
            self.assertFalse((ROOT / ".cargo" / name).exists(), name)

    def test_missing_sccache_is_reported_and_debug_info_is_lighter(self):
        environment = Environment(self, with_sccache=False)

        def check(shell):
            result = environment.run(shell)
            self.assertIn("sccache not found", result.said)
            self.assertNotIn("RUSTC_WRAPPER", result.enabled)
            self.assertNotIn("CARGO_INCREMENTAL", result.enabled)
            self.assertIn("CARGO_INCREMENTAL left at the Cargo default", result.said)
            self.assertEqual(result.enabled["CARGO_PROFILE_DEV_DEBUG"], "line-tables-only")
            self.assertEqual(result.enabled["CARGO_PROFILE_TEST_DEBUG"], "line-tables-only")
            self.assertIn("CARGO_PROFILE_DEV_DEBUG=line-tables-only", result.said)

        self.check_each_shell(check)

    def test_sccache_turns_incremental_off_unless_forced_on(self):
        environment = Environment(self, with_sccache=True)

        def check(shell):
            result = environment.run(shell)
            self.assertEqual(result.enabled["RUSTC_WRAPPER"], "sccache")
            self.assertEqual(result.enabled["CARGO_INCREMENTAL"], "0")
            forced = environment.run(shell, {"VLITE_DEV_FAST_INCREMENTAL": "1"})
            self.assertEqual(forced.enabled["CARGO_INCREMENTAL"], "1")
            self.assertIn("from VLITE_DEV_FAST_INCREMENTAL", forced.said)
            lighter = environment.run(shell, {"VLITE_DEV_FAST_DEBUG": "0"})
            self.assertEqual(lighter.enabled["CARGO_PROFILE_DEV_DEBUG"], "0")
            self.assertEqual(lighter.enabled["CARGO_PROFILE_TEST_DEBUG"], "0")

        self.check_each_shell(check)

    def test_your_own_settings_are_kept_and_never_undone(self):
        environment = Environment(self, with_sccache=True)
        mine = {"RUSTC_WRAPPER": "my-wrapper", "CARGO_PROFILE_DEV_DEBUG": "2"}

        def check(shell):
            result = environment.run(shell, mine, off=True)
            self.assertEqual(result.enabled["RUSTC_WRAPPER"], "my-wrapper")
            self.assertEqual(result.enabled["CARGO_PROFILE_DEV_DEBUG"], "2")
            self.assertIn("kept your RUSTC_WRAPPER=my-wrapper", result.said)
            self.assertIn("kept your CARGO_PROFILE_DEV_DEBUG=2", result.said)
            # The pre-set values survive the undo; only what the script set is removed.
            self.assertEqual(result.undone["RUSTC_WRAPPER"], "my-wrapper")
            self.assertEqual(result.undone["CARGO_PROFILE_DEV_DEBUG"], "2")
            self.assertNotIn("CARGO_PROFILE_TEST_DEBUG", result.undone)
            self.assertNotIn("CARGO_INCREMENTAL", result.undone)
            self.assertNotIn("VLITE_DEV_FAST_SET", result.undone)

        self.check_each_shell(check)

    def test_off_unsets_what_the_script_set(self):
        environment = Environment(self, with_sccache=True)

        def check(shell):
            result = environment.run(shell, off=True)
            self.assertIn("VLITE_DEV_FAST_SET", result.enabled)
            for name in SET_BY_SCRIPT:
                self.assertIn(name, result.enabled)
            self.assertEqual(result.undone, {})

        self.check_each_shell(check)

    @unittest.skipIf(ON_WINDOWS, "the POSIX script is sourced from POSIX shells")
    def test_running_the_posix_script_instead_of_sourcing_it_changes_nothing(self):
        for label in ("bash", "sh", "zsh"):
            exe = shutil.which(label)
            if not exe:
                continue
            with self.subTest(shell=label):
                completed = subprocess.run([exe, str(SH)], capture_output=True, text=True, timeout=60)
                self.assertNotEqual(completed.returncode, 0)
                self.assertIn("source this file", completed.stderr)
                self.assertEqual(completed.stdout, "")

    @unittest.skipUnless(ON_WINDOWS, "the linker override is for Windows")
    def test_linker_override_points_at_an_existing_file_or_is_skipped(self):
        environment = Environment(self, with_sccache=False)

        def check(shell):
            result = environment.run(shell, off=True)
            linkers = {k: v for k, v in result.enabled.items() if re.match(r"CARGO_TARGET_.*_LINKER$", k)}
            if linkers:
                for name, value in linkers.items():
                    self.assertTrue(name.endswith("_PC_WINDOWS_MSVC_LINKER"), name)
                    self.assertTrue(Path(value).is_file(), value)
                self.assertEqual(result.undone, {})
            else:
                self.assertIn("linker unchanged", result.said)

        self.check_each_shell(check)


if __name__ == "__main__":
    unittest.main()
