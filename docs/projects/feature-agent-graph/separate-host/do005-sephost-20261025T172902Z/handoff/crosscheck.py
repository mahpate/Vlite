"""Independent, read-only cross-check of the DO-005 report against raw runs.jsonl and the evidence files.

QA aid for the operator; it is not part of the campaign's artifacts and writes nothing.
"""
import hashlib
import json
import math
import statistics
import subprocess
import sys
from collections import Counter, defaultdict
from datetime import datetime, timezone
from pathlib import Path

sys.dont_write_bytecode = True

CAMP = Path(r"Q:\vlite-campaign\do005-sephost-20261025T172902Z")
CLONE = Path(r"Q:\vlite")
WORKTREE = Path(r"Q:\ARG\[REDACTED-DIR]\copilot-worktrees\Vlite\mahpate-bookish-robot")
LOGICAL = 16
MEAN_LIMIT, PEAK_LIMIT, STALL, TOLERANCE = 0.10, 0.50, 5.0, 5.0
WORKLOADS = ("exact_small", "filtered_exact", "hnsw_small", "exact_100k_384", "filtered_100k_384", "hnsw_100k_384")
CAPS = {"exact_small": 15, "filtered_exact": 15, "hnsw_small": 15, "exact_100k_384": 8, "filtered_100k_384": 8, "hnsw_100k_384": 3}
LABELS = ("hist", "cur")
DETERMINISTIC = ("format_version", "dataset_checksum", "query_checksum", "file_bytes", "final_file_bytes", "vector_data_bytes", "query_data_bytes",
                 "recall_at_k", "recall_denominator", "eligible_records", "returned_per_query", "final_live_records", "returned_counts")
failures = []


def check(ok, message):
    print(("ok    " if ok else "FAIL  ") + message)
    if not ok:
        failures.append(message)


def sha256(path):
    digest = hashlib.sha256()
    with open(path, "rb") as handle:
        for chunk in iter(lambda: handle.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


def load(path):
    return json.loads(Path(path).read_text(encoding="utf-8"))


def samples_of(record):
    return (((record.get("run") or {}).get("cpu")) or {}).get("samples") or []


def cpu_of(record):
    samples = samples_of(record)
    if not samples:
        return None
    elapsed = sum(s["elapsed_seconds"] for s in samples)
    background = sum(s["background_cpu_seconds"] for s in samples)
    counted = [s["background_cpu_seconds"] / (s["elapsed_seconds"] * LOGICAL) for s in samples if s["elapsed_seconds"] >= 0.1]
    return {"mean": background / (elapsed * LOGICAL), "peak": max(counted, default=0.0), "longest": max(s["elapsed_seconds"] for s in samples)}


def tag_of(r):
    return f"rep {r['repetition']} {r['workload']} {r['label']} try {r['attempt']}"


def git(path, *args):
    result = subprocess.run(["git", "--no-optional-locks", *args], cwd=path, capture_output=True, text=True, encoding="utf-8", errors="replace", timeout=120)
    return result.stdout.strip() if result.returncode == 0 else f"[exit {result.returncode}] {result.stderr.strip()}"


records = [json.loads(line) for line in (CAMP / "raw" / "main" / "runs.jsonl").read_text(encoding="utf-8").splitlines() if line.strip()]
report = load(CAMP / "report" / "comparison-report.json")
print(f"{len(records)} records; runner outcomes {dict(Counter(r['outcome'] for r in records))}; report counts {report['counts']}; verdict {report['verdict']}")

print("\n== A. CPU policy recomputed from the raw monitor samples ==")
measured = [r for r in records if samples_of(r)]
stored_mismatch, verdict_mismatch, cpu_quiet_rejects, quarantine = [], [], [], []
for r in measured:
    cpu, stored = cpu_of(r), r["run"]["cpu"]
    if abs(cpu["mean"] - stored["mean_background_cpu_fraction"]) > 1e-9 or abs(cpu["peak"] - stored["max_background_cpu_fraction"]) > 1e-9:
        stored_mismatch.append(tag_of(r))
    quiet = cpu["mean"] <= MEAN_LIMIT and cpu["peak"] <= PEAK_LIMIT
    if r["outcome"] == "accepted":
        if not quiet or r["run"]["returncode"] != 0 or not isinstance(r.get("bench"), dict):
            verdict_mismatch.append(tag_of(r))
        if cpu["longest"] > STALL:
            quarantine.append(tag_of(r))
    elif r["outcome"] == "rejected" and quiet:
        cpu_quiet_rejects.append((tag_of(r), r.get("reasons")))
check(not stored_mismatch, f"stored CPU summaries equal the recomputed ones for {len(measured)} measured attempts {stored_mismatch[:3]}")
check(not verdict_mismatch, f"every accepted run recomputes as quiet (mean <= {MEAN_LIMIT:.0%}, peak <= {PEAK_LIMIT:.0%}), exit 0, parsed output {verdict_mismatch[:3]}")
print(f"       rejected although CPU-quiet by recomputation: {cpu_quiet_rejects}")
print(f"       accepted runs with a monitor sample > {STALL:g} s (quarantine candidates): {quarantine}")
check(len(quarantine) == len(report["stall_rule"]["quarantined"]), f"quarantined runs: {len(quarantine)} here, {len(report['stall_rule']['quarantined'])} in the report")
check(all(abs(r["recomputed_cpu"]["mean"] - cpu_of(r)["mean"]) < 1e-9 and abs(r["recomputed_cpu"]["peak"] - cpu_of(r)["peak"]) < 1e-9 for r in measured if r.get("recomputed_cpu")),
      "the runner's own recomputed_cpu block equals this recomputation")
unmonitored = [tag_of(r) for r in records if r["outcome"] == "accepted" and not samples_of(r)]
check(not unmonitored, f"every accepted run carries monitor samples {unmonitored[:3]}")

print("\n== B. Slot accounting ==")
groups = defaultdict(list)
for r in records:
    groups[(r["workload"], r["label"], r["repetition"])].append(r)
bad = []
for key, rs in groups.items():
    numbers = sorted(r["attempt"] for r in rs if r["outcome"] != "not_attempted")
    if numbers != list(range(1, len(numbers) + 1)) or len(numbers) > CAPS[key[0]]:
        bad.append((key, numbers))
check(not bad, f"attempt numbers run 1..n per slot and stay within the caps {bad[:3]}")
good = {}
for r in records:
    cpu = cpu_of(r)
    if r["outcome"] == "accepted" and not (cpu and cpu["longest"] > STALL):
        key = (r["workload"], r["label"], r["repetition"])
        check(key not in good, f"one accepted record per slot {key}") if key in good else None
        good[key] = r
total = 0
for w in WORKLOADS:
    for label in LABELS:
        mine = sorted(rep for (ww, ll, rep) in good if ww == w and ll == label)
        theirs = report["slots"][f"{w}/{label}"]["slots_accepted"]
        total += len(mine)
        check(len(mine) == theirs, f"{w}/{label}: accepted slots {len(mine)} {mine} (report {theirs})")
check(total == report["scope"]["accepted_slots"], f"total accepted slots {total} of {report['scope']['expected_slots']} (report {report['scope']['accepted_slots']})")
for w in WORKLOADS:
    for label in LABELS:
        rs = [r for r in records if r["workload"] == w and r["label"] == label]
        counts = Counter(r["outcome"] for r in rs)
        slot = report["slots"][f"{w}/{label}"]
        check(counts["rejected"] == slot["rejected"] and counts["refused"] == slot["refused"] and counts["not_attempted"] == slot["not_attempted"],
              f"{w}/{label} outcomes {dict(counts)} vs report rejected {slot['rejected']} refused {slot['refused']} not_attempted {slot['not_attempted']}")

print("\n== C. Medians recomputed from the accepted records ==")


def safe(fn, record):
    try:
        return fn(record)
    except (KeyError, TypeError):
        return None


GET = {
    "ingest_vectors_per_second": lambda r: r["bench"]["ingest_vectors_per_second"],
    "query_p50_ms": lambda r: r["bench"]["query_p50_ms"],
    "query_p99_ms": lambda r: r["bench"]["query_p99_ms"],
    "reopen_ms": lambda r: r["bench"]["reopen_ms"],
    "peak_working_set_mib": lambda r: r["run"]["process_peak_working_set_bytes"] / 1048576,
    "recall_at_k": lambda r: r["bench"]["recall_at_k"],
    "hnsw_index_build_ms": lambda r: r["bench"]["hnsw"]["index_build_ms"],
    "hnsw_query_p50_ms": lambda r: r["bench"]["hnsw"]["query_p50_ms"],
    "hnsw_query_p99_ms": lambda r: r["bench"]["hnsw"]["query_p99_ms"],
    "hnsw_recall_at_k": lambda r: r["bench"]["hnsw"]["recall_at_k"],
    "child_wall_seconds": lambda r: r["run"]["elapsed_seconds"],
}
differences, compared = [], 0
for w, rows in report["comparison"].items():
    for row in rows:
        medians = {}
        for label in LABELS:
            values = [v for (ww, ll, rep), r in sorted(good.items()) if ww == w and ll == label for v in [safe(GET[row["metric"]], r)] if v is not None]
            stat = row[label]
            compared += 1
            if not values:
                if stat is not None:
                    differences.append((w, row["metric"], label, "report has a value, none recomputed"))
                continue
            medians[label] = statistics.median(values)
            if stat is None or stat["n"] != len(values) or not all(math.isclose(a, b, rel_tol=1e-12, abs_tol=1e-12) for a, b in
                                                                   ((stat["median"], medians[label]), (stat["min"], min(values)), (stat["max"], max(values)))):
                differences.append((w, row["metric"], label, f"report {stat and (stat['n'], stat['median'])} vs {len(values), medians[label]}"))
        if len(medians) == 2 and medians["hist"]:
            delta = (medians["cur"] - medians["hist"]) / medians["hist"] * 100
            if row["delta_percent_of_medians"] is None or not math.isclose(delta, row["delta_percent_of_medians"], rel_tol=1e-9, abs_tol=1e-9):
                differences.append((w, row["metric"], "delta", f"{row['delta_percent_of_medians']} vs {delta}"))
check(not differences, f"{compared} (workload, metric, revision) summaries equal; differences: {differences[:4]}")
for w in ("exact_100k_384", "hnsw_100k_384"):
    for row in report["comparison"][w]:
        if row["metric"] in ("ingest_vectors_per_second", "reopen_ms", "query_p50_ms", "hnsw_index_build_ms") and row["delta_percent_of_medians"] is not None:
            print(f"       {w} {row['metric']}: hist {row['hist']['median']:.4g} (n={row['hist']['n']}), cur {row['cur']['median']:.4g} (n={row['cur']['n']}), delta {row['delta_percent_of_medians']:+.1f}%, ranges {'overlap' if row['ranges_overlap'] else 'disjoint'}")

print("\n== D. Determinism and workload identity ==")
for w in WORKLOADS:
    runs = [r for r in records if r["workload"] == w and isinstance(r.get("bench"), dict)]
    args = {json.dumps(r["workload_args"]) for r in records if r["workload"] == w and r["outcome"] != "not_attempted"}
    check(len(args) <= 1, f"{w}: one argument list for every attempt of both revisions")
    seen = {label: {key: {json.dumps(r["bench"].get(key), sort_keys=True) for r in runs if r["label"] == label} for key in DETERMINISTIC} for label in LABELS}
    unstable = [key for label in LABELS for key, v in seen[label].items() if len(v) > 1]
    across = sorted(key for key in DETERMINISTIC if seen["hist"][key] and seen["cur"][key] and seen["hist"][key] != seen["cur"][key])
    theirs = report["determinism"][w]["differing_across_revisions"]
    check(not unstable and [k for k in theirs if "." not in k] == across, f"{w}: unstable within a revision {unstable}; differing across revisions {across} (report {theirs})")
envs = {json.dumps(r.get("environment"), sort_keys=True) for r in records if r.get("environment")}
check(len(envs) == 1, f"one benchmark environment (storage location) for every attempt: {sorted(envs)}")

print("\n== E. Binaries, sources and runner hashes ==")
builds = load(CAMP / "evidence" / "builds.json")
for label in LABELS:
    info = builds[label]["binaries"]["vlite-bench"]
    check(sha256(info["path"]) == info["sha256"], f"{label} vlite-bench.exe hash now equals the build evidence ({info['sha256'][:16]})")
actual = {(r["label"], r["binary_sha256_actual"]) for r in records if r.get("binary_sha256_actual")}
check(actual == {(label, builds[label]["binaries"]["vlite-bench"]["sha256"]) for label in LABELS}, "every attempt ran the recorded binary of its revision")
hashes = load(CAMP / "evidence" / "runner-hashes.json")
for name, digest in hashes["runner_modules"].items():
    check(sha256(CAMP / "runner" / name) == digest, f"runner/{name} unchanged since it was recorded")
archives = load(CAMP / "evidence" / "archives.json")
for name, digest in hashes["pinned_current_scripts"].items():
    check(sha256(Path(archives["cur"]["directory"]) / name) == digest, f"pinned {name} unchanged")
for tag in ("sources-verified-start.json", "sources-verified-end.json"):
    path = CAMP / "raw" / "main" / tag
    check(path.exists() and load(path)["ok"], f"{tag} present and ok")

print("\n== F. Git state: nothing modified, committed or pushed ==")
for name, path in (("clone", CLONE), ("session worktree", WORKTREE)):
    status = git(path, "status", "--porcelain=v1", "--untracked-files=all")
    check(status == "", f"{name} working tree clean (HEAD {git(path, 'rev-parse', 'HEAD')[:12]}, reflog {git(path, 'reflog', '-n', '3', '--format=%h %gs')!r})")
    print(f"       {name} branch {git(path, 'rev-parse', '--abbrev-ref', 'HEAD')}, unpushed commits vs upstream: {git(path, 'rev-list', '--count', '@{upstream}..HEAD')}")

print("\n== G. Agent-activity coincidence (independent brute-force implementation) ==")
activity_path = CAMP / "evidence" / "window" / "agent-activity.json"
if not activity_path.exists():
    check(report.get("agent_activity") is None, "no agent-activity.json, and the report has no coincidence analysis")
else:
    activity = load(activity_path)
    mine = report["agent_activity"]
    CLASSES = ("process", "in_process", "turn", "none")
    LONG_RUN, START_UP, CLUSTER_GAP, ELEVATED = 100.0, 20.0, 10.0, 0.30

    def epoch(text):
        return datetime.fromisoformat(text).timestamp()

    def stamp(value):
        return datetime.fromtimestamp(value, tz=timezone.utc).isoformat(timespec="seconds")

    calls = []  # (kind, tool, [(a, b), ...]): the stretches during which the call counts as active
    for call in activity["calls"]:
        a = epoch(call["start_utc"])
        b = epoch(call["end_utc"]) if call.get("end_utc") else a
        pieces = [(a, b)] if call.get("span") == "interval" else [(a, a), (b, b)]
        if call.get("exit_utc"):
            pieces.append((epoch(call["exit_utc"]),) * 2)
        calls.append((call["kind"], call["tool"], pieces))
    turns = [(epoch(t["start_utc"]), epoch(t["end_utc"])) for t in activity.get("model_turns") or []]

    def within(kind, s, e):
        return any(a - TOLERANCE <= e and b + TOLERANCE >= s for k, _, pieces in calls if k == kind for a, b in pieces)

    def classify(s, e):
        if within("process", s, e):
            return "process"
        if within("in_process", s, e):
            return "in_process"
        return "turn" if any(a <= e and b >= s for a, b in turns) else "none"

    def timeline(r):
        samples = samples_of(r)
        if not samples or not r.get("preflight"):
            return None
        clock = begin = epoch(r["started_utc"]) + r["preflight"]["wait_seconds"]
        rows = []
        for s in samples:
            rows.append((clock, clock + s["elapsed_seconds"], s, s["background_cpu_seconds"] / (s["elapsed_seconds"] * LOGICAL) if s["elapsed_seconds"] else 0.0))
            clock += s["elapsed_seconds"]
        return begin, clock, rows

    def approx(x, y, tolerance=1e-6):
        return (x is None and y is None) or (x is not None and y is not None and abs(x - y) <= tolerance * max(1.0, abs(y)))

    tally = {pop: {c: {"samples": 0, "seconds": 0.0, "cpu": 0.0, "fractions": [], "elevated": 0, "limit": 0} for c in CLASSES} for pop in ("all", "accepted")}
    spans, long_found, used = [], {}, 0
    for r in records:
        t = timeline(r)
        if not t:
            continue
        begin, finish, rows = t
        spans.append((begin, finish))
        if finish - begin < LONG_RUN:
            continue
        stalled = cpu_of(r)["longest"] > STALL
        overlap = {kind: sum(1 for k, _, pieces in calls if k == kind and any(a <= finish and b >= begin for a, b in pieces)) for kind in ("process", "in_process")}
        long_found[(r["repetition"], r["workload"], r["label"], r["attempt"])] = (overlap, r["outcome"], stalled)
        if stalled:
            continue
        used += 1
        for s0, s1, s, f in rows:
            if s["elapsed_seconds"] < 0.1 or s0 < begin + START_UP:
                continue
            c = classify(s0, s1)
            for pop in ("all", "accepted") if r["outcome"] == "accepted" else ("all",):
                x = tally[pop][c]
                x["samples"] += 1
                x["seconds"] += s["elapsed_seconds"]
                x["cpu"] += s["background_cpu_seconds"]
                x["fractions"].append(f)
                x["elevated"] += f > ELEVATED
                x["limit"] += f > PEAK_LIMIT

    check(mine["calls_total"] == len(activity["calls"]) and mine["model_turns"] == len(turns), f"{len(activity['calls'])} calls and {len(turns)} model turns in the record equal the report's")
    check(mine["process_calls"] == sum(1 for k, *_ in calls if k == "process"), "process-starting call count equals the report's")
    check(mine["comparison"]["attempts_used"] == used, f"long non-stalled attempts used: {used} here, {mine['comparison']['attempts_used']} in the report")
    for pop in ("all", "accepted"):
        for c in CLASSES:
            x, theirs = tally[pop][c], mine["comparison"]["populations"][pop][c]
            mean = x["cpu"] / (x["seconds"] * LOGICAL) if x["seconds"] else None
            median = statistics.median(x["fractions"]) if x["fractions"] else None
            same = (x["samples"], x["elevated"], x["limit"]) == (theirs["samples"], theirs["over_elevated"], theirs["over_limit"]) and approx(x["seconds"], theirs["seconds"]) \
                and approx(mean, theirs["mean_background"]) and approx(median, theirs["median_sample_background"])
            check(same, f"near/far {pop}/{c}: {x['samples']} samples, {x['seconds']:.1f} s, mean {mean if mean is None else format(mean, '.4%')}, median {median if median is None else format(median, '.4%')}, "
                        f">{ELEVATED:.0%} {x['elevated']}, >{PEAK_LIMIT:.0%} {x['limit']}")

    inside = {"process": 0, "in_process": 0}
    for kind, _, pieces in calls:
        if any(a <= f and b >= s for s, f in spans for a, b in pieces):
            inside[kind] += 1
    check(inside == {"process": mine["process_calls_in_runs"], "in_process": mine["in_process_calls_in_runs"]}, f"calls overlapping a measured run: {inside} vs the report")
    mine_long = {(i["repetition"], i["workload"], i["label"], i["attempt"]): i for i in mine["long_runs"]}
    check(set(mine_long) == set(long_found), f"{len(long_found)} long attempts listed, same set as the report")
    wrong = [key for key, (overlap, outcome, stalled) in long_found.items() if key in mine_long and (overlap["process"], overlap["in_process"], outcome, stalled) !=
             (mine_long[key]["process_calls"], mine_long[key]["in_process_calls"], mine_long[key]["outcome"], mine_long[key]["stalled"])]
    check(not wrong, f"per-attempt call overlaps, outcomes and stall flags equal the report's {wrong[:3]}")

    clusters = []
    for r in records:
        t = timeline(r)
        if not t:
            continue
        current = None
        for index, (s0, s1, s, f) in enumerate(t[2]):
            if s["elapsed_seconds"] < 0.1 or f <= PEAK_LIMIT:
                continue
            if current is not None and s0 - current["end"] < CLUSTER_GAP:
                current.update(end=s1, last=index, over=current["over"] + 1, peak=max(current["peak"], f))
            else:
                current = {"r": r, "rows": t[2], "start": s0, "end": s1, "first": index, "last": index, "over": 1, "peak": f}
                clusters.append(current)
    check(sum(c["over"] for c in clusters) == len(report["burst_samples_above_limit"]), f"{sum(c['over'] for c in clusters)} over-limit samples in {len(clusters)} bursts; "
          f"report lists {len(report['burst_samples_above_limit'])} samples")
    key = lambda start, r: (start, r["repetition"], r["workload"], r["label"], r["attempt"])
    mine_clusters = {key(c["start_utc"], c): c for c in report["burst_clusters"]}
    check(len(mine_clusters) == len(clusters) == len(report["burst_clusters"]), f"{len(clusters)} bursts here, {len(report['burst_clusters'])} in the report")
    bad = []
    for c in clusters:
        theirs = mine_clusters.get(key(stamp(c["start"]), c["r"]))
        if not theirs or theirs["samples_over_limit"] != c["over"] or not approx(theirs["peak_background_fraction"], c["peak"]) or not approx(theirs["length_seconds"], c["end"] - c["start"]) \
                or theirs["span_samples"] != c["last"] - c["first"] + 1 or theirs["outcome"] != c["r"]["outcome"]:
            bad.append((stamp(c["start"]), tag_of(c["r"])))
    check(not bad, f"every burst's start, length, sample count, span and peak equal the report's {bad[:3]}")

    def nearest(kind, s, e):
        best = []
        for k, tool, pieces in calls:
            if k != kind:
                continue
            for a, b in pieces:
                best.append((0.0 if a <= e and b >= s else (s - b if b < s else a - e), "during" if a <= e and b >= s else "before" if b < s else "after", tool))
        low = min(d for d, *_ in best)
        return low, {rel for d, rel, _ in best if d == low}, {tool for d, _, tool in best if d == low}

    observed, expected, wrong_near = Counter(), {"process": 0.0, "in_process_only": 0.0, "neither": 0.0}, []
    for c in clusters:
        theirs = mine_clusters.get(key(stamp(c["start"]), c["r"]))
        stalled = cpu_of(c["r"])["longest"] > STALL
        if theirs and theirs["stalled_attempt"] != stalled:
            wrong_near.append(("stall flag", stamp(c["start"])))
        for kind, field in (("process", "nearest_process_call"), ("in_process", "nearest_in_process_call")):
            distance, relations, tools = nearest(kind, c["start"], c["end"])
            got = theirs["agent"][field] if theirs else None
            if not got or not approx(got["seconds"], distance, 1e-6) or got["relation"] not in relations or got["tool"] not in tools:
                wrong_near.append((kind, stamp(c["start"])))
        if stalled:
            continue
        p, q = within("process", c["start"], c["end"]), within("in_process", c["start"], c["end"])
        observed["process" if p else "in_process_only" if q else "neither"] += 1
        length, positions, hits_p, hits_q = c["last"] - c["first"] + 1, len(c["rows"]) - (c["last"] - c["first"] + 1) + 1, 0, 0
        for i in range(positions):
            s0, s1 = c["rows"][i][0], c["rows"][i + length - 1][1]
            hp = within("process", s0, s1)
            hits_p += hp
            hits_q += (not hp) and within("in_process", s0, s1)
        expected["process"] += hits_p / positions
        expected["in_process_only"] += hits_q / positions
        expected["neither"] += 1 - hits_p / positions - hits_q / positions
    check(not wrong_near, f"nearest process-starting / in-process call and stall flag of every burst equal the report's {wrong_near[:3]}")
    theirs_b = mine["bursts"]
    check(dict(observed) == {k: v for k, v in theirs_b["observed"].items() if v} and sum(observed.values()) == theirs_b["analysed"], f"bursts observed by coincidence class: {dict(observed)} vs {theirs_b['observed']}")
    check(all(approx(expected[k], theirs_b["expected"][k], 1e-9) for k in expected), f"bursts expected if unrelated: {({k: round(v, 3) for k, v in expected.items()})} vs {({k: round(v, 3) for k, v in theirs_b['expected'].items()})}")

    markdown = (CAMP / "report" / "comparison-report.md").read_text(encoding="utf-8")
    appendix = markdown.split("## Appendix D", 1)[1]
    rows_d = [line for line in appendix.splitlines() if line.startswith("| ") and not line.startswith(("| UTC", "| :--"))]
    check(len(rows_d) == len(clusters), f"Appendix D lists {len(rows_d)} rows for {len(clusters)} bursts")
    check(f"`{sha256(activity_path)}`" in markdown, "the report cites the activity record's current sha256")
    exclusive = report["window"]["exclusive_requirement_status"]
    check((exclusive == "partial") == bool(inside["process"] or inside["in_process"]), f"exclusive-quiet-window status {exclusive!r} is consistent with {inside} calls overlapping runs")
    print(f"       {len(clusters)} bursts, {sum(c['over'] for c in clusters)} samples; observed {dict(observed)}; near/far all: " + "; ".join(f"{c} {tally['all'][c]['cpu'] / (tally['all'][c]['seconds'] * LOGICAL):.2%}" for c in CLASSES if tally['all'][c]['seconds']))

print("\n== H. ARTIFACTS.sha256 integrity ==")
index_path = CAMP / "report" / "ARTIFACTS.sha256"
listed = {}
for line in index_path.read_text(encoding="utf-8").splitlines():
    digest, name = line.split("  ", 1)
    listed[name] = digest
stale = [name for name, digest in listed.items() if not (CAMP / name).is_file() or sha256(CAMP / name) != digest]
check(not stale, f"{len(listed)} indexed files hash as listed {stale[:3]}")
present = {p.relative_to(CAMP).as_posix() for root in ("evidence", "raw", "runner", "report") for p in (CAMP / root).rglob("*")
           if p.is_file() and "__pycache__" not in p.parts and p.name not in ("ARTIFACTS.sha256", "ARTIFACTS.rollup.txt")}
check(present == set(listed), f"index covers exactly the files on disk (missing {sorted(present - set(listed))[:3]}, extra {sorted(set(listed) - present)[:3]})")
rollup = (CAMP / "report" / "ARTIFACTS.rollup.txt").read_text(encoding="utf-8").split()[0]
check(rollup == sha256(index_path), f"roll-up {rollup[:16]} equals the hash of ARTIFACTS.sha256")

print(f"\n{len(failures)} failure(s)")
for item in failures:
    print("  -", item)
sys.exit(1 if failures else 0)
