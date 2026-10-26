"""Prepare the separate-host DO-005 graph-off campaign: pins, clean git archives, restore, builds, host evidence.

Everything here runs BEFORE the quiet measurement window. Dependencies are restored
online once per archive (`cargo fetch --locked`); every build afterwards is
`--locked --offline`.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import shutil
import subprocess
import sys
import tarfile
import time
from pathlib import Path

from do005_common import (
    CARGO, CLONE, LABELS, RUSTC, RUSTUP, RUSTUP_TOOLCHAIN, TOOLCHAIN, dropped_env_names, git, global_memory, log,
    native, now_utc, powershell, read_json, require_windows, sha256_bytes, sha256_file, toolchain_env, write_json,
)

HIST_SHA_PREFIX = "81ff901"
HIST_FULL_SHA = "81ff901c54d674d1ad519dd4fa722e937db46c4b"


def campaign_dir(argument=None) -> Path:
    if argument:
        return Path(argument)
    return Path(Path(r"Q:\vlite-campaign\CURRENT_CAMPAIGN.txt").read_text(encoding="utf-8").strip())


def run_logged(command, cwd, env, stem: Path, timeout=7200) -> dict:
    stem.parent.mkdir(parents=True, exist_ok=True)
    start = time.perf_counter()
    started = now_utc()
    with open(f"{stem}.out.log", "wb") as out, open(f"{stem}.err.log", "wb") as err:
        child = subprocess.run(command, cwd=cwd, env=env, stdout=out, stderr=err, timeout=timeout)
    return {
        "command": [str(part) for part in command], "cwd": str(cwd), "returncode": child.returncode,
        "started_utc": started, "elapsed_seconds": round(time.perf_counter() - start, 3),
        "stdout_log": f"{stem}.out.log", "stderr_log": f"{stem}.err.log",
    }


def pin_revisions(camp: Path) -> dict:
    fetch = native(["git", "-C", str(CLONE), "fetch", "origin", "main"], timeout=300)
    remote_head = git("ls-remote", "origin", "refs/heads/main").split()[0]
    historical = git("rev-parse", f"{HIST_SHA_PREFIX}^{{commit}}")
    current = git("rev-parse", "origin/main^{commit}")
    if historical != HIST_FULL_SHA:
        raise SystemExit(f"historical revision resolved to {historical}, expected {HIST_FULL_SHA}")
    if current != remote_head:
        raise SystemExit(f"local origin/main {current} does not match the remote head {remote_head}")
    pins = {
        "pinned_utc": now_utc(),
        "historical": {"requested": HIST_SHA_PREFIX, "sha": historical, "subject": git("log", "-1", "--format=%s", historical),
                       "tree": git("rev-parse", f"{historical}^{{tree}}"), "committer_date": git("log", "-1", "--format=%cI", historical)},
        "current": {"requested": "origin/main", "sha": current, "subject": git("log", "-1", "--format=%s", current),
                    "tree": git("rev-parse", f"{current}^{{tree}}"), "committer_date": git("log", "-1", "--format=%cI", current)},
        "git_ls_remote_origin_main": remote_head,
        "git_fetch": fetch,
        "clone": str(CLONE),
        "clone_origin": git("remote", "get-url", "origin"),
        "git_version": git("--version"),
        "superseded_earlier_fetch": {
            "sha": "41cb396b0bc5a4ee6383924ddff4fb14ab1ff7ba",
            "note": "origin/main as first fetched when the campaign directory was created; the remote advanced to the pinned head before any archive or build existed, so the pin was taken again. Nothing was measured at the superseded revision.",
        },
        "ancestor_check": {
            "historical_is_ancestor_of_current": subprocess.run(["git", "merge-base", "--is-ancestor", historical, current], cwd=CLONE).returncode == 0,
            "commits_between": int(git("rev-list", "--count", f"{historical}..{current}")),
        },
    }
    write_json(camp / "evidence" / "pins.json", pins)
    return pins


def blob_sha1(data: bytes) -> str:
    return hashlib.sha1(b"blob %d\0" % len(data) + data).hexdigest()


def make_archive(camp: Path, label: str, sha: str, redo: bool) -> dict:
    root = camp / "src"
    root.mkdir(parents=True, exist_ok=True)
    destination = root / label
    tar_path = root / f"{label}-{sha[:12]}.tar"
    if destination.exists() or tar_path.exists():
        if not redo:
            raise SystemExit(f"{destination} already exists; refusing to overwrite (use --redo)")
        shutil.rmtree(destination, ignore_errors=True)
        tar_path.unlink(missing_ok=True)
    # Byte-exact blobs: with core.autocrlf=true (this host's global git config) a plain `git archive` rewrites text files to CRLF.
    archive_command = ["-c", "core.autocrlf=false", "-c", "core.eol=lf", "archive", "--format=tar", "-o", str(tar_path), sha]
    git(*archive_command)
    with tar_path.open("rb") as handle:
        commit_id = subprocess.run(["git", "get-tar-commit-id"], stdin=handle, capture_output=True, text=True).stdout.strip()
    if commit_id != sha:
        raise SystemExit(f"{label}: archive embeds commit {commit_id}, expected {sha}")
    destination.mkdir()
    with tarfile.open(tar_path) as archive:
        archive.extractall(destination, filter="data")
    listing = subprocess.run(["git", "ls-tree", "-r", "-z", "--full-tree", sha], cwd=CLONE, capture_output=True).stdout.split(b"\0")
    expected = {}
    for entry in listing:
        if not entry:
            continue
        meta, path = entry.split(b"\t", 1)
        mode, kind, blob = meta.decode().split()
        expected[path.decode("utf-8")] = (mode, kind, blob)
    problems, manifest = [], {}
    for path, (mode, kind, blob) in expected.items():
        target = destination / path
        if mode == "120000" or kind != "blob":
            problems.append(f"unexpected non-regular entry {path} ({mode} {kind})")
            continue
        if not target.is_file():
            problems.append(f"missing {path}")
            continue
        data = target.read_bytes()
        if blob_sha1(data) != blob:
            problems.append(f"blob mismatch {path}")
        manifest[path] = sha256_bytes(data)
    on_disk = {p.relative_to(destination).as_posix() for p in destination.rglob("*") if p.is_file()}
    extra = sorted(on_disk - set(expected))
    if extra:
        problems.append(f"files not in the git tree: {extra[:10]}")
    if (destination / ".git").exists():
        problems.append("archive contains .git")
    if problems:
        raise SystemExit(f"{label}: archive verification failed: {problems[:10]}")
    rollup = sha256_bytes("".join(f"{path}\0{digest}\n" for path, digest in sorted(manifest.items())).encode("utf-8"))
    manifest_path = camp / "evidence" / f"source-manifest-{label}.json"
    write_json(manifest_path, {"label": label, "sha": sha, "rollup_sha256": rollup, "files": dict(sorted(manifest.items()))})
    longest = max(len(str(destination / p)) for p in expected)
    return {
        "label": label, "sha": sha, "tree": git("rev-parse", f"{sha}^{{tree}}"), "directory": str(destination),
        "tar": str(tar_path), "tar_bytes": tar_path.stat().st_size, "tar_sha256": sha256_file(tar_path),
        "embedded_commit_id": commit_id, "files": len(manifest), "git_blob_sha1_verified_files": len(manifest),
        "archive_command": ["git", *archive_command],
        "source_rollup_sha256": rollup, "manifest": str(manifest_path), "manifest_sha256": sha256_file(manifest_path),
        "longest_extracted_path_chars": longest, "has_dot_git": False,
        "key_files_sha256": {name: manifest.get(name) for name in (
            "Cargo.toml", "Cargo.lock", "src/bin/vlite-bench.rs", "scripts/graph_baseline.py", "scripts/graph_reference.py",
            "scripts/graph_compat_check.py", "tests/measure_benchmark.py", "tests/graph_build_boundary.py")},
    }


def toolchain_facts(camp: Path) -> dict:
    env = toolchain_env(online=False)
    def run(command):
        result = subprocess.run(command, env=env, capture_output=True, text=True, encoding="utf-8", errors="replace")
        return {"command": command, "returncode": result.returncode, "stdout": result.stdout.strip(), "stderr": result.stderr.strip()}
    facts = {
        "rustup_toolchain": RUSTUP_TOOLCHAIN,
        "rustc_vV": run([RUSTC, "-vV"]), "cargo_vV": run([CARGO, "-vV"]), "rustup_show": run([RUSTUP, "show"]),
        "rustc_path": run([RUSTUP, "which", "rustc"]),
        "python": {"executable": sys.executable, "version": sys.version, "implementation": sys.implementation.name},
        "rustup_init_sha256": sha256_file(TOOLCHAIN / "downloads" / "rustup-init.exe"),
        "python_installer_sha256": sha256_file(TOOLCHAIN / "downloads" / "python-3.15.0-amd64.exe"),
        "rustc_exe_sha256": None, "cargo_exe_sha256": None,
        "inherited_rust_environment_removed": dropped_env_names(),
        "cargo_environment_overrides": {"CARGO_INCREMENTAL": "0", "CARGO_NET_OFFLINE": "true (all builds after restore)",
                                       "CARGO_HOME": str(TOOLCHAIN / "cargo"), "RUSTUP_HOME": str(TOOLCHAIN / "rustup")},
    }
    which = facts["rustc_path"]["stdout"]
    if which and Path(which).is_file():
        facts["rustc_exe_sha256"] = sha256_file(which)
    cargo_exe = Path(which).with_name("cargo.exe") if which else None
    if cargo_exe and cargo_exe.is_file():
        facts["cargo_exe_sha256"] = sha256_file(cargo_exe)
    # Which MSVC linker rustc actually uses (a throwaway compile in the campaign scratch area, not in either source tree).
    scratch = camp / "work" / "linker-probe"
    scratch.mkdir(parents=True, exist_ok=True)
    (scratch / "probe.rs").write_text("fn main() {}\n", encoding="utf-8")
    probe = subprocess.run([RUSTC, "--print", "link-args", "-C", "opt-level=0", "-o", str(scratch / "probe.exe"), str(scratch / "probe.rs")],
                           env=env, capture_output=True, text=True, encoding="utf-8", errors="replace", cwd=scratch)
    first = (probe.stdout.strip().splitlines() or [""])[0]
    linker = first.strip().strip('"').split('" ')[0].strip('"') if first else ""
    facts["linker_probe"] = {"returncode": probe.returncode, "first_link_arg_line": first[:600], "linker_path": linker}
    if linker and Path(linker).is_file():
        banner = subprocess.run([linker], capture_output=True, text=True, encoding="utf-8", errors="replace", cwd=scratch)
        facts["linker_probe"]["banner"] = (banner.stdout + banner.stderr).strip().splitlines()[:3]
        facts["linker_probe"]["sha256"] = sha256_file(linker)
    shutil.rmtree(scratch, ignore_errors=True)
    vswhere = Path(r"C:\Program Files (x86)\Microsoft Visual Studio\Installer\vswhere.exe")
    if vswhere.is_file():
        facts["vswhere"] = native([str(vswhere), "-all", "-products", "*", "-format", "json", "-utf8"])
    write_json(camp / "evidence" / "toolchain.json", facts)
    return facts


def restore_and_build(camp: Path, label: str, directory: Path, sha: str) -> dict:
    logs = camp / "evidence" / "logs"
    record = {"label": label, "sha": sha}
    record["fetch"] = run_logged([CARGO, "fetch", "--locked", "--verbose"], directory, toolchain_env(online=True), logs / f"cargo-fetch-{label}")
    if record["fetch"]["returncode"] != 0:
        raise SystemExit(f"{label}: cargo fetch --locked failed; see {record['fetch']['stderr_log']}")
    target = camp / "target" / label
    builds = []
    for name, extra in (("vlite-bench", ["--bin", "vlite-bench"]), ("vlite-ffi", ["-p", "vlite-ffi"])):
        command = [CARGO, "build", "--locked", "--offline", "--release", "-v", "--message-format=json-render-diagnostics", *extra, "--target-dir", str(target)]
        build = run_logged(command, directory, toolchain_env(online=False), logs / f"cargo-build-{label}-{name}")
        build["what"] = name
        if build["returncode"] != 0:
            raise SystemExit(f"{label}: build {name} failed; see {build['stderr_log']}")
        artifacts = []
        for line in Path(build["stdout_log"]).read_text(encoding="utf-8", errors="replace").splitlines():
            if not line.startswith("{"):
                continue
            message = json.loads(line)
            if message.get("reason") == "compiler-artifact":
                artifacts.append({"package_id": message["package_id"], "target": message["target"]["name"], "kind": message["target"]["kind"],
                                  "features": message.get("features", []), "profile": message.get("profile", {}), "fresh": message.get("fresh")})
        local = [a for a in artifacts if "path+file" in a["package_id"] or str(directory).replace("\\", "/").lower() in a["package_id"].lower()]
        build["compiled_units"] = len(artifacts)
        build["local_units"] = local
        build["graph_feature_enabled_anywhere"] = any("graph" in a["features"] for a in artifacts)
        build["graph_crate_compiled"] = any("vlite-graph" in a["package_id"] or "vlite_graph" in a["target"] for a in artifacts)
        build["local_features_union"] = sorted({f for a in local for f in a["features"]})
        builds.append(build)
    record["builds"] = builds
    binaries = {}
    for name, filename in (("vlite-bench", "vlite-bench.exe"), ("vlite-ffi", "vlite_ffi.dll")):
        path = target / "release" / filename
        binaries[name] = {"path": str(path), "bytes": path.stat().st_size, "sha256": sha256_file(path)}
    record["binaries"] = binaries
    record["target_dir"] = str(target)
    return record


def host_facts(camp: Path) -> dict:
    script = r"""
$ErrorActionPreference='Continue'
function Try-Get($block){ try { & $block } catch { "ERROR: " + $_.Exception.Message } }
function Flat($items){ foreach($o in @($items)){ if($o -is [string]){ $o; continue }; $h=[ordered]@{}; foreach($p in $o.PSObject.Properties){ $v=$p.Value; if($v -is [Enum]){ $v="$v" }; $h[$p.Name]=$v }; [pscustomobject]$h } }
$cs  = Try-Get { Get-CimInstance Win32_ComputerSystem | Select-Object Name,Manufacturer,Model,SystemType,HypervisorPresent,NumberOfProcessors,NumberOfLogicalProcessors,TotalPhysicalMemory,Domain,PartOfDomain }
$bios= Try-Get { Get-CimInstance Win32_BIOS | Select-Object Manufacturer,SMBIOSBIOSVersion,Version,ReleaseDate,SerialNumber }
$prod= Try-Get { Get-CimInstance Win32_ComputerSystemProduct | Select-Object Name,Vendor,Version,IdentifyingNumber,UUID }
$os  = Try-Get { Get-CimInstance Win32_OperatingSystem | Select-Object Caption,Version,BuildNumber,OSArchitecture,InstallDate,LastBootUpTime,FreePhysicalMemory,TotalVisibleMemorySize }
$cv  = Try-Get { Get-ItemProperty 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion' | Select-Object ProductName,DisplayVersion,CurrentBuild,UBR,BuildLabEx,EditionID }
$cpu = Try-Get { @(Get-CimInstance Win32_Processor | Select-Object Name,Manufacturer,Description,NumberOfCores,NumberOfLogicalProcessors,MaxClockSpeed,CurrentClockSpeed,L2CacheSize,L3CacheSize,ProcessorId,Architecture,Family,Revision) }
$mem = Try-Get { @(Get-CimInstance Win32_PhysicalMemory | Select-Object Capacity,Speed,Manufacturer,PartNumber) }
$vol = Try-Get { Flat (Get-Volume -DriveLetter Q | Select-Object DriveLetter,FileSystem,FileSystemLabel,DriveType,HealthStatus,Size,SizeRemaining,AllocationUnitSize) }
$ldisk = Try-Get { Get-CimInstance Win32_LogicalDisk -Filter "DeviceID='Q:'" | Select-Object DeviceID,FileSystem,Size,FreeSpace,VolumeName,DriveType }
$part= Try-Get { Flat (Get-Partition -DriveLetter Q | Select-Object DiskNumber,PartitionNumber,DriveLetter,Size,Type,GptType,IsBoot,IsSystem) }
$dn  = Try-Get { (Get-Partition -DriveLetter Q).DiskNumber }
$disk= Try-Get { Flat (Get-Disk -Number $dn | Select-Object Number,FriendlyName,Model,Manufacturer,BusType,PartitionStyle,Size,LogicalSectorSize,PhysicalSectorSize,HealthStatus,OperationalStatus,FirmwareVersion) }
$pd  = Try-Get { @(Flat (Get-PhysicalDisk | Select-Object DeviceId,FriendlyName,MediaType,BusType,Size,LogicalSectorSize,PhysicalSectorSize,SpindleSpeed,HealthStatus)) }
$dd  = Try-Get { (fsutil devdrv query Q: 2>&1) -join "`n" }
$vi  = Try-Get { (fsutil volume diskfree Q: 2>&1) -join "`n" }
$mp  = Try-Get { Get-MpComputerStatus | Select-Object AMServiceEnabled,AntispywareEnabled,AntivirusEnabled,RealTimeProtectionEnabled,BehaviorMonitorEnabled,OnAccessProtectionEnabled,IoavProtectionEnabled,NISEnabled,AMProductVersion,AMEngineVersion,AntivirusSignatureVersion,AntivirusSignatureLastUpdated,QuickScanEndTime,FullScanEndTime,IsTamperProtected,AMRunningMode }
$mpp = Try-Get { Get-MpPreference | Select-Object DisableRealtimeMonitoring,PerformanceModeStatus,ScanScheduleDay,ScanScheduleQuickScanTime,ScanAvgCPULoadFactor,DisableCpuThrottleOnIdleScans,EnableLowCpuPriority,ExclusionPath,ExclusionProcess,ExclusionExtension }
$pwr = Try-Get { (powercfg /getactivescheme 2>&1) -join "`n" }
$tz  = Try-Get { (Get-TimeZone | Select-Object Id,DisplayName,BaseUtcOffset) }
$mach= Try-Get { (Get-ItemProperty 'HKLM:\SOFTWARE\Microsoft\Cryptography' -Name MachineGuid).MachineGuid }
$wsl = Try-Get { ((wsl.exe -l -v 2>&1) -join "`n") -replace "`0","" }
$sess= Try-Get { ((quser 2>&1) -join "`n") }
$svc = Try-Get { @(Get-Service | Where-Object { $_.Status -eq 'Running' -and $_.Name -match 'wuauserv|UsoSvc|WSearch|SysMain|BITS|WdNisSvc|WinDefend|Sense|HvHost|vmcompute|WslService|docker|OneSyncSvc' } | Select-Object Name,DisplayName,StartType) }
$upd = Try-Get { @(Get-HotFix | Sort-Object InstalledOn -Descending | Select-Object -First 5 HotFixID,InstalledOn) }
[pscustomobject]@{ computer_system=$cs; bios=$bios; product=$prod; os=$os; current_version=$cv; cpu=$cpu; physical_memory=$mem; volume=$vol; logical_disk=$ldisk; partition=$part; disk=$disk; physical_disks=$pd; devdrive_query=$dd; diskfree=$vi; defender_status=$mp; defender_preference=$mpp; power_scheme=$pwr; timezone=$tz; machine_guid=$mach; wsl_list=$wsl; logged_on_sessions=$sess; selected_running_services=$svc; hotfixes=$upd } | ConvertTo-Json -Depth 6 -Compress
"""
    parsed, text, error = powershell(script, timeout=300)
    facts = parsed if isinstance(parsed, dict) else {"raw": text}
    if error:
        facts["powershell_stderr"] = error
    guid = facts.get("machine_guid")
    if isinstance(guid, str) and guid and not guid.startswith("ERROR"):
        facts["machine_guid_sha256"] = sha256_bytes(guid.encode("utf-8"))
        facts["machine_guid"] = "(redacted; hashed in machine_guid_sha256)"
    facts["captured_utc"] = now_utc()
    facts["python_memory_status"] = global_memory()
    import os, platform
    facts["python_platform"] = {"platform": platform.platform(), "machine": platform.machine(), "processor": platform.processor(),
                                "os_cpu_count": os.cpu_count(), "node": platform.node()}
    write_json(camp / "evidence" / "host.json", facts)
    return facts


def reference_identity(host: dict) -> dict:
    """The identity gate graph_reference.main() applies to the DO-005 reference host, evaluated here for the record only."""
    cpus = host.get("cpu") or []
    if isinstance(cpus, dict):
        cpus = [cpus]
    volume = host.get("volume") if isinstance(host.get("volume"), dict) else {}
    disk = host.get("disk") if isinstance(host.get("disk"), dict) else {}
    cs = host.get("computer_system") if isinstance(host.get("computer_system"), dict) else {}
    cv = host.get("current_version") if isinstance(host.get("current_version"), dict) else {}
    logical = cs.get("NumberOfLogicalProcessors")
    os_version = f"10.0.{cv.get('CurrentBuild')}" if cv.get("CurrentBuild") else None
    return {
        "cpu_is_xeon_8370c": any("8370C" in str(cpu.get("Name")) for cpu in cpus),
        "logical_cpus_16": logical == 16,
        "ram_bytes_equal_reference": host.get("python_memory_status", {}).get("ram_bytes") == 68_665_831_424,
        "os_version_10.0.26300": os_version == "10.0.26300",
        "volume_Q_ReFS": volume.get("DriveLetter") in ("Q", "Q:") and volume.get("FileSystem") == "ReFS",
        "disk_msft_virtual_sas": disk.get("FriendlyName") == "Msft Virtual Disk" and str(disk.get("BusType")) in ("SAS", "10"),
    }


def runner_hashes(camp: Path, pinned_cur: Path) -> dict:
    runner = {p.name: sha256_file(p) for p in sorted((camp / "runner").glob("*.py")) if p.name != "do005_report.py"}
    monitor = {name: sha256_file(pinned_cur / name) for name in ("scripts/graph_reference.py", "scripts/graph_baseline.py", "scripts/graph_compat_check.py", "tests/measure_benchmark.py")}
    return {"runner_modules": runner, "pinned_current_scripts": monitor, "python_executable_sha256": sha256_file(sys.executable)}


def main() -> int:
    require_windows()
    parser = argparse.ArgumentParser()
    parser.add_argument("--campaign")
    parser.add_argument("--redo", action="store_true")
    parser.add_argument("--stage", choices=("all", "pins", "archives", "build", "host", "toolchain", "hashes"), default="all")
    args = parser.parse_args()
    camp = campaign_dir(args.campaign)
    (camp / "evidence").mkdir(parents=True, exist_ok=True)
    stage = args.stage
    pins_path = camp / "evidence" / "pins.json"
    if stage in ("all", "pins"):
        if pins_path.exists() and not args.redo:
            raise SystemExit("pins.json already exists; the revisions are pinned once per campaign (use --redo only before any measurement)")
        pins = pin_revisions(camp)
        log(f"pinned historical {pins['historical']['sha']} current {pins['current']['sha']}")
    pins = read_json(pins_path)
    shas = {"hist": pins["historical"]["sha"], "cur": pins["current"]["sha"]}
    if stage in ("all", "archives"):
        archives = {label: make_archive(camp, label, shas[label], args.redo) for label in LABELS}
        write_json(camp / "evidence" / "archives.json", archives)
        for label, info in archives.items():
            log(f"{label}: {info['files']} files, tar sha256 {info['tar_sha256'][:16]}, rollup {info['source_rollup_sha256'][:16]}")
    archives_path = camp / "evidence" / "archives.json"
    archives = read_json(archives_path) if archives_path.exists() else {}
    if stage in ("all", "toolchain"):
        log("recording toolchain")
        toolchain_facts(camp)
    if stage in ("all", "host"):
        log("recording host evidence")
        host = host_facts(camp)
        write_json(camp / "evidence" / "reference-identity-checks.json", reference_identity(host))
    if stage in ("all", "build"):
        builds = {}
        for label in LABELS:
            log(f"restoring and building {label} ({shas[label][:12]})")
            builds[label] = restore_and_build(camp, label, Path(archives[label]["directory"]), shas[label])
            write_json(camp / "evidence" / "builds.json", builds)
    if stage in ("all", "build", "hashes"):
        write_json(camp / "evidence" / "runner-hashes.json", runner_hashes(camp, Path(archives["cur"]["directory"])))
    log("prepare stage complete")
    return 0


if __name__ == "__main__":
    sys.exit(main())
