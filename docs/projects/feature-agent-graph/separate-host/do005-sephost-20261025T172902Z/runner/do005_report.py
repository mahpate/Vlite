import sys

sys.dont_write_bytecode = True

"""Separate-host DO-005 comparison report generator.

Reads the campaign's raw records and evidence and writes only <campaign>/report*/comparison-report.{json,md} and ARTIFACTS.sha256.
Rejected, refused and not-attempted slots never enter a comparison statistic; they are listed in the ledger and the appendices.
Nothing here evaluates, approves or amends a gate or a baseline.
"""

import argparse
import bisect
import json
import re
import statistics
import subprocess
from collections import defaultdict, namedtuple
from datetime import datetime, timezone
from pathlib import Path

from do005_campaign import (
    ABANDON_AFTER_EXHAUSTED_ANY, ABANDON_AFTER_EXHAUSTED_NO_ACCEPTED, ATTEMPT_CAPS, MAX_CONSECUTIVE_REFUSALS, WORKLOAD_ORDER,
)
from do005_common import LABELS, LABEL_NAMES, now_utc, read_json, sha256_bytes, sha256_file, write_json
from do005_prepare import campaign_dir

REQUIRED_REPS = 5
REPORT_MODULE = "do005_report.py"
CLONE = Path(r"Q:\vlite")
SESSION_WORKTREE = Path(r"Q:\ARG\[REDACTED-DIR]\copilot-worktrees\Vlite\mahpate-bookish-robot")
HNSW_WORKLOADS = ("hnsw_small", "hnsw_100k_384")
BASELINE_DOC = Path("docs") / "projects" / "feature-agent-graph" / "baseline.md"
DETERMINISTIC_KEYS = ("format_version", "dataset_checksum", "query_checksum", "file_bytes", "final_file_bytes", "vector_data_bytes", "query_data_bytes",
                      "recall_at_k", "recall_denominator", "eligible_records", "returned_per_query", "final_live_records", "returned_counts")
HNSW_DETERMINISTIC_KEYS = ("m", "ef_construction", "ef_search", "seed", "recall_at_k", "recall_denominator", "returned_counts")
STALL_SAMPLE_SECONDS = 5.0  # ten times the monitor's nominal 0.5 s sample; written into the incident record (evidence/window/incident-vm-freeze.json) when the stall was found
BURST_LIMIT = 0.50  # the unchanged per-sample background limit, used only to list the offending samples
ELEVATED_SAMPLE = 0.30  # a lower bar than the policy, used only to compare how often samples are elevated near the agent's activity and away from it
ACTIVITY_TOLERANCE_SECONDS = 5.0  # a monitor sample coincides with a tool call when it comes within this many seconds of a moment the call was active
CLUSTER_GAP_SECONDS = 10.0  # over-limit samples of one attempt that are closer together than this are listed as one burst
LONG_RUN_SECONDS = 100.0  # the agent-activity comparison uses the long attempts only (the 100k HNSW runs); the others are too short for it
START_UP_SECONDS = 20.0  # left out of that comparison: a run's first seconds (data generation, the scanner's reaction to new files) are elevated whatever the agent does
ACTIVITY_CLASSES = ("process", "in_process", "turn", "none")

Metric = namedtuple("Metric", "key heading unit higher_is_better hnsw_only get")


def _bench(*path):
    def get(record):
        value = record.get("bench")
        for part in path:
            if not isinstance(value, dict):
                return None
            value = value.get(part)
        return value
    return get


def _run(key, scale=1.0):
    def get(record):
        value = (record.get("run") or {}).get(key)
        return None if value is None else value / scale
    return get


METRICS = (
    Metric("ingest_vectors_per_second", "Ingest", "vectors/s", True, False, _bench("ingest_vectors_per_second")),
    Metric("query_p50_ms", "Exact query p50", "ms", False, False, _bench("query_p50_ms")),
    Metric("query_p99_ms", "Exact query p99", "ms", False, False, _bench("query_p99_ms")),
    Metric("reopen_ms", "Reopen", "ms", False, False, _bench("reopen_ms")),
    Metric("peak_working_set_mib", "Peak working set", "MiB", False, False, _run("process_peak_working_set_bytes", 1048576)),
    Metric("recall_at_k", "Exact recall@k", "", True, False, _bench("recall_at_k")),
    Metric("hnsw_index_build_ms", "HNSW index build", "ms", False, True, _bench("hnsw", "index_build_ms")),
    Metric("hnsw_query_p50_ms", "HNSW query p50", "ms", False, True, _bench("hnsw", "query_p50_ms")),
    Metric("hnsw_query_p99_ms", "HNSW query p99", "ms", False, True, _bench("hnsw", "query_p99_ms")),
    Metric("hnsw_recall_at_k", "HNSW recall@k", "", True, True, _bench("hnsw", "recall_at_k")),
    Metric("child_wall_seconds", "Child wall time", "s", False, False, _run("elapsed_seconds")),
)
METRIC_BY_KEY = {m.key: m for m in METRICS}


# ---------------------------------------------------------------------------------------------- formatting
def num(value, unit: str = "") -> str:
    if value is None:
        return "–"
    value = float(value)
    if unit == "":
        return f"{value:.4g}"
    magnitude = abs(value)
    places = 0 if magnitude >= 1000 else 1 if magnitude >= 100 else 3 if magnitude >= 1 else 4
    return f"{value:,.{places}f}"


def pct(fraction, places: int = 2) -> str:
    return "–" if fraction is None else f"{fraction * 100:.{places}f}%"


def hhmmss(value) -> str:
    return value[11:19] if value else "–"


def md_table(headers, rows, aligns=None) -> str:
    aligns = aligns or ["l"] * len(headers)
    marks = {"l": ":--", "r": "--:", "c": ":-:"}
    lines = ["| " + " | ".join(headers) + " |", "| " + " | ".join(marks[a] for a in aligns) + " |"]
    for row in rows:
        lines.append("| " + " | ".join(str(cell).replace("|", "\\|").replace("\n", " ") for cell in row) + " |")
    return "\n" + "\n".join(lines) + "\n"


def plural(count: int, noun: str) -> str:
    return f"{count} {noun}" + ("" if count == 1 else "s")


def tidy_path(value: str) -> str:
    return value.replace("\\\\", "\\")


def session_line(text: str) -> str:
    rows = [line for line in text.splitlines() if line.strip()]
    return " ".join(rows[-1].lstrip(">").split()) if rows else ""


def census_changes(start, end) -> dict:
    """Watched-process counts that differ between the start and the end census (e.g. Windows servicing starting mid-campaign)."""
    if not start or not end:
        return {}
    names = set(start["watched_processes"]) | set(end["watched_processes"])
    return {n: (start["watched_processes"].get(n, 0), end["watched_processes"].get(n, 0)) for n in sorted(names)
            if start["watched_processes"].get(n, 0) != end["watched_processes"].get(n, 0)}


def summarise(values):
    values = [v for v in values if v is not None]
    if not values:
        return None
    return {"n": len(values), "median": statistics.median(values), "min": min(values), "max": max(values), "values": values}


def ms_date(value):
    match = re.search(r"-?\d+", str(value or ""))
    return datetime.fromtimestamp(int(match.group()) / 1000, tz=timezone.utc).isoformat(timespec="seconds") if match else None


def exists_optional(path: Path):
    return read_json(path) if path.exists() else None


# ---------------------------------------------------------------------------------------------- loading
def load_inputs(camp: Path, tag: str) -> dict:
    raw, ev = camp / "raw" / tag, camp / "evidence"
    window = ev / "window"
    records = [json.loads(line) for line in (raw / "runs.jsonl").read_text(encoding="utf-8").splitlines() if line.strip()]
    records.sort(key=lambda r: (r["started_utc"], r.get("attempt", 0)))
    data = {
        "camp": camp, "tag": tag, "raw": raw, "ev": ev, "records": records, "config": read_json(raw / "campaign-config.json"),
        "pins": read_json(ev / "pins.json"), "archives": read_json(ev / "archives.json"), "builds": read_json(ev / "builds.json"),
        "toolchain": read_json(ev / "toolchain.json"), "host": read_json(ev / "host.json"), "identity": read_json(ev / "reference-identity-checks.json"),
        "runner_hashes": read_json(ev / "runner-hashes.json"), "selftest": read_json(ev / "selftest.json"),
        "census_start": exists_optional(raw / "census-start.json"), "census_end": exists_optional(raw / "census-end.json"),
        "sources_start": exists_optional(raw / "sources-verified-start.json"), "sources_end": exists_optional(raw / "sources-verified-end.json"),
        "gate": exists_optional(window / "gate-pre.json"), "post_survey": exists_optional(window / "survey-post.json"),
        "explore": {p.stem.removeprefix("survey-"): read_json(p) for p in sorted(window.glob("survey-explore-*.json"))},
        "sessions": {name: exists_optional(window / f"sessions-{name}.json") for name in ("pre", "post")},
        "agent_activity": exists_optional(window / "agent-activity.json"),
        "log_lines": (raw / "campaign.log").read_text(encoding="utf-8").splitlines() if (raw / "campaign.log").exists() else [],
    }
    incident_path = window / "incident-vm-freeze.json"
    data["incident"] = exists_optional(incident_path)
    if incident_path.exists():
        written = incident_path.stat().st_mtime
        data["incident_file"] = {
            "last_modified_utc": datetime.fromtimestamp(written, tz=timezone.utc).isoformat(timespec="seconds"),
            "attempts_finished_before": sum(1 for r in records if r["outcome"] != "not_attempted" and r.get("finished_utc")
                                            and datetime.fromisoformat(r["finished_utc"]).timestamp() <= written),
            "attempts_recorded_now": sum(r["outcome"] != "not_attempted" for r in records)}
    data["stall_rule"] = apply_stall_quarantine(records)
    data["gate_surveys"] = {}
    if data["gate"]:
        for attempt in data["gate"]["attempts"]:
            path = window / attempt["survey_file"]
            if path.exists():
                data["gate_surveys"][attempt["survey_file"]] = read_json(path)
    baseline = Path(data["archives"]["cur"]["directory"]) / BASELINE_DOC
    data["baseline"] = parse_baseline(baseline)
    return data


def run_samples(record):
    return ((record.get("run") or {}).get("cpu") or {}).get("samples") or []


def apply_stall_quarantine(records) -> dict:
    """Report-side quality rule, stricter than the runner's and never looser: an attempt the runner accepted is quarantined (outcome
    "quarantined", excluded from every statistic) when any of its monitor samples lasted longer than STALL_SAMPLE_SECONDS, because the
    host then did not run for that long (see the host stall incident). The runner is unchanged; this rule can only remove runs."""
    for r in records:
        r["longest_sample_seconds"] = max((s["elapsed_seconds"] for s in run_samples(r)), default=0.0)
        if r["outcome"] == "accepted" and r["longest_sample_seconds"] > STALL_SAMPLE_SECONDS:
            r["runner_outcome"], r["outcome"] = "accepted", "quarantined"

    def ref(r):
        return {"repetition": r["repetition"], "workload": r["workload"], "label": r["label"], "attempt": r["attempt"], "started_utc": r["started_utc"],
                "runner_outcome": r.get("runner_outcome", r["outcome"]), "outcome": r["outcome"], "longest_sample_seconds": r["longest_sample_seconds"]}

    measured = [r for r in records if run_samples(r)]
    stalled = [r for r in measured if r["longest_sample_seconds"] > STALL_SAMPLE_SECONDS]
    unstalled = [r for r in measured if r["longest_sample_seconds"] <= STALL_SAMPLE_SECONDS]
    accepted = [r for r in records if r["outcome"] == "accepted"]
    return {"limit_seconds": STALL_SAMPLE_SECONDS, "attempts_with_samples": len(measured), "accepted_after_rule": len(accepted),
            "runner_accepted": len(accepted) + sum(r["outcome"] == "quarantined" for r in records),
            "quarantined": [ref(r) for r in records if r["outcome"] == "quarantined"], "stalled_attempts": [ref(r) for r in stalled],
            "longest_sample_in_accepted_runs": max((r["longest_sample_seconds"] for r in accepted), default=None),
            "longest_sample_in_unstalled_attempts": max((r["longest_sample_seconds"] for r in unstalled), default=None)}


def iso(epoch) -> str:
    return datetime.fromtimestamp(epoch, tz=timezone.utc).isoformat(timespec="seconds")


def run_timeline(record, logical=16):
    """(begin, finish, rows) of one attempt's monitored child in epoch seconds; each row is (sample start, sample end, sample, background fraction). The child starts at the
    attempt start plus the pre-run wait (wall span minus that wait minus the run's elapsed time was <= 0.05 s for every record), so times are good to about +-0.2 s."""
    samples = run_samples(record)
    if not samples or not record.get("preflight"):
        return None
    begin = clock = datetime.fromisoformat(record["started_utc"]).timestamp() + record["preflight"]["wait_seconds"]
    rows = []
    for sample in samples:
        seconds = sample["elapsed_seconds"]
        rows.append((clock, clock + seconds, sample, sample["background_cpu_seconds"] / (seconds * logical) if seconds else 0.0))
        clock += seconds
    return begin, clock, rows


def timed_samples(records, logical=16):
    """Every monitor sample as (record, end time in epoch seconds, sample, background fraction)."""
    for r in records:
        timeline = run_timeline(r, logical)
        for _, end, sample, fraction in timeline[2] if timeline else ():
            yield r, end, sample, fraction


def burst_samples(records, limit=BURST_LIMIT, logical=16):
    """Every monitor sample of at least 0.1 s whose background CPU exceeds the per-sample limit, with its approximate UTC end time."""
    return [{"utc": datetime.fromtimestamp(clock, tz=timezone.utc).isoformat(timespec="seconds"), "repetition": r["repetition"], "workload": r["workload"],
             "label": r["label"], "attempt": r["attempt"], "outcome": r["outcome"], "background_fraction": fraction, "sample_seconds": sample["elapsed_seconds"]}
            for r, clock, sample, fraction in timed_samples(records, logical) if sample["elapsed_seconds"] >= 0.1 and fraction > limit]


def merge_windows(windows):
    merged = []
    for start, end in sorted(windows):
        if merged and start <= merged[-1][1]:
            merged[-1][1] = max(merged[-1][1], end)
        else:
            merged.append([start, end])
    return [(a, b) for a, b in merged]


def window_index(windows):
    return [a for a, _ in windows], [b for _, b in windows]


def touches(index, start, end) -> bool:
    """Whether any of the merged, sorted windows overlaps [start, end]."""
    starts, ends = index
    position = bisect.bisect_right(starts, end) - 1
    return position >= 0 and ends[position] >= start


def burst_clusters(records, limit=BURST_LIMIT, logical=16, gap=CLUSTER_GAP_SECONDS):
    """Bursts: samples of at least 0.1 s over the per-sample limit, grouped when consecutive ones of one attempt are less than `gap` seconds apart. Times are epoch
    seconds; `first` and `last` index the attempt's rows (`rows`), and the samples between them belong to the burst's span."""
    clusters = []
    for r in records:
        timeline = run_timeline(r, logical)
        if not timeline:
            continue
        current = None
        for index, (start, end, sample, fraction) in enumerate(timeline[2]):
            if sample["elapsed_seconds"] < 0.1 or fraction <= limit:
                continue
            if current and start - current["end"] < gap:
                current.update(end=end, last=index, over=current["over"] + 1, peak=max(current["peak"], fraction), longest=max(current["longest"], sample["elapsed_seconds"]))
            else:
                current = {"record": r, "rows": timeline[2], "start": start, "end": end, "first": index, "last": index, "over": 1, "peak": fraction,
                           "longest": sample["elapsed_seconds"]}
                clusters.append(current)
    return clusters


def cluster_row(c) -> dict:
    r = c["record"]
    row = {"start_utc": iso(c["start"]), "end_utc": iso(c["end"]), "length_seconds": c["end"] - c["start"], "repetition": r["repetition"], "workload": r["workload"],
           "label": r["label"], "attempt": r["attempt"], "outcome": r["outcome"], "samples_over_limit": c["over"], "span_samples": c["last"] - c["first"] + 1,
           "peak_background_fraction": c["peak"], "longest_sample_seconds": c["longest"], "stalled_attempt": r["longest_sample_seconds"] > STALL_SAMPLE_SECONDS}
    if "agent" in c:
        row["agent"] = c["agent"]
    return row


def activity_windows(activity):
    """The moments the agent was active, as (start, end, tool, kind, call number) in epoch seconds. A synchronous command (span "interval") is active from its start
    to its end; every other call counts at its start, at its end and, for an asynchronous shell whose completion was announced, at its exit."""
    windows = []
    for number, call in enumerate(activity["calls"]):
        start = datetime.fromisoformat(call["start_utc"]).timestamp()
        end = datetime.fromisoformat(call["end_utc"]).timestamp() if call.get("end_utc") else start
        tool, kind = call.get("tool", "?"), call.get("kind", "in_process")
        if call.get("span") == "interval":
            windows.append((start, end, tool, kind, number))
        else:
            windows += [(start, start, tool, kind, number), (end, end, tool, kind, number)]
        if call.get("exit_utc"):
            exit_at = datetime.fromisoformat(call["exit_utc"]).timestamp()
            windows.append((exit_at, exit_at, tool, kind, number))
    return sorted(windows)


def nearest_window(windows, kind, start, end):
    """The call of the given kind whose active moment is closest to [start, end]: {"tool", "relation" (before/during/after), "seconds"}, or None without such a call."""
    best = None
    for a, b, tool, k, _ in windows:
        if k != kind:
            continue
        distance, relation = (start - b, "before") if b < start else (a - end, "after") if a > end else (0.0, "during")
        if best is None or distance < best["seconds"]:
            best = {"tool": tool, "relation": relation, "seconds": distance}
    return best


def expected_coincidence(rows, first, last, padded):
    """The chance that a stretch of the same number of consecutive samples, placed anywhere in the attempt, coincides with the agent's process-starting calls
    (first value) or only with in-process calls (second value)."""
    length = last - first + 1
    positions = len(rows) - length + 1
    process = in_process_only = 0
    for p in range(positions):
        start, end = rows[p][0], rows[p + length - 1][1]
        hit_process = touches(padded["process"], start, end)
        process += hit_process
        in_process_only += (not hit_process) and touches(padded["in_process"], start, end)
    return process / positions, in_process_only / positions


def activity_coincidence(records, activity, logical, clusters, tolerance=ACTIVITY_TOLERANCE_SECONDS):
    """Timing of the agent's own tool calls against the monitor's samples. Calls that start an operating-system process (kind "process": PowerShell, ripgrep and what they
    launch) are kept apart from in-process calls (file views and edits); a sample coincides with a call when it comes within `tolerance` seconds of a moment the call was
    active. Annotates `clusters` in place. This measures timing only; no cause is inferred."""
    if not activity or not activity.get("calls"):
        return None
    windows = activity_windows(activity)
    turns = [(datetime.fromisoformat(t["start_utc"]).timestamp(), datetime.fromisoformat(t["end_utc"]).timestamp()) for t in activity.get("model_turns") or []]
    padded = {kind: window_index(merge_windows([(a - tolerance, b + tolerance) for a, b, _, k, _ in windows if k == kind])) for kind in ("process", "in_process")}
    in_turn = window_index(merge_windows(turns))

    def classify(start, end):
        if touches(padded["process"], start, end):
            return "process"
        if touches(padded["in_process"], start, end):
            return "in_process"
        return "turn" if touches(in_turn, start, end) else "none"

    tallies = {population: {name: {"samples": 0, "seconds": 0.0, "background_cpu_seconds": 0.0, "fractions": [], "over_elevated": 0, "over_limit": 0} for name in ACTIVITY_CLASSES}
               for population in ("all", "accepted")}
    spans, long_runs, used, excluded = [], [], 0, []
    for r in records:
        timeline = run_timeline(r, logical)
        if not timeline:
            continue
        begin, finish, rows = timeline
        spans.append((begin, finish))
        if finish - begin < LONG_RUN_SECONDS:
            continue
        calls = {kind: {n for a, b, _, k, n in windows if k == kind and a <= finish and b >= begin} for kind in ("process", "in_process")}
        cpu = r["run"]["cpu"]
        stalled = r["longest_sample_seconds"] > STALL_SAMPLE_SECONDS
        long_runs.append({"repetition": r["repetition"], "workload": r["workload"], "label": r["label"], "attempt": r["attempt"], "outcome": r["outcome"], "seconds": finish - begin,
                          "process_calls": len(calls["process"]), "in_process_calls": len(calls["in_process"]), "stalled": stalled,
                          "background_mean": cpu["mean_background_cpu_fraction"], "background_max_sample": cpu["max_background_cpu_fraction"]})
        if stalled:
            excluded.append({"repetition": r["repetition"], "workload": r["workload"], "label": r["label"], "attempt": r["attempt"], "outcome": r["outcome"]})
            continue
        used += 1
        for start, end, sample, fraction in rows:
            if sample["elapsed_seconds"] < 0.1 or start < begin + START_UP_SECONDS:
                continue
            name = classify(start, end)
            for population in ("all", "accepted") if r["outcome"] == "accepted" else ("all",):
                t = tallies[population][name]
                t["samples"] += 1
                t["seconds"] += sample["elapsed_seconds"]
                t["background_cpu_seconds"] += sample["background_cpu_seconds"]
                t["fractions"].append(fraction)
                t["over_elevated"] += fraction > ELEVATED_SAMPLE
                t["over_limit"] += fraction > BURST_LIMIT
    for population in tallies.values():
        for t in population.values():
            fractions = t.pop("fractions")
            t["mean_background"] = t["background_cpu_seconds"] / (t["seconds"] * logical) if t["seconds"] else None
            t["median_sample_background"] = statistics.median(fractions) if fractions else None

    span_index = window_index(merge_windows(spans))
    by_call = defaultdict(list)
    for a, b, _, kind, number in windows:
        by_call[number].append((a, b, kind))
    in_runs = {"process": 0, "in_process": 0}
    for moments in by_call.values():
        if any(touches(span_index, a, b) for a, b, _ in moments):
            in_runs[moments[0][2]] += 1

    observed = {"process": 0, "in_process_only": 0, "neither": 0}
    expected = {"process": 0.0, "in_process_only": 0.0, "neither": 0.0}
    for c in clusters:
        stalled = c["record"]["longest_sample_seconds"] > STALL_SAMPLE_SECONDS
        c["agent"] = {"stalled_attempt": stalled, "nearest_process_call": nearest_window(windows, "process", c["start"], c["end"]),
                      "nearest_in_process_call": nearest_window(windows, "in_process", c["start"], c["end"])}
        if stalled:
            continue
        hit_process, hit_in_process = touches(padded["process"], c["start"], c["end"]), touches(padded["in_process"], c["start"], c["end"])
        group = "process" if hit_process else "in_process_only" if hit_in_process else "neither"
        c["agent"]["coincides_with"] = group
        observed[group] += 1
        p_process, p_in_process = expected_coincidence(c["rows"], c["first"], c["last"], padded)
        expected["process"] += p_process
        expected["in_process_only"] += p_in_process
        expected["neither"] += 1 - p_process - p_in_process

    by_tool = defaultdict(int)
    for call in activity["calls"]:
        by_tool[call.get("tool", "?")] += 1
    return {"tolerance_seconds": tolerance, "source": activity.get("source"), "calls_total": len(activity["calls"]), "model_turns": len(turns),
            "calls_by_tool": dict(sorted(by_tool.items(), key=lambda kv: -kv[1])), "process_calls": sum(1 for c in activity["calls"] if c.get("kind") == "process"),
            "first_call_utc": iso(windows[0][0]), "last_call_utc": iso(max(w[1] for w in windows)),
            "process_calls_in_runs": in_runs["process"], "in_process_calls_in_runs": in_runs["in_process"],
            "comparison": {"populations": tallies, "attempts_used": used, "attempts_excluded_for_stall": excluded, "start_up_seconds": START_UP_SECONDS, "long_run_seconds": LONG_RUN_SECONDS,
                           "elevated_sample": ELEVATED_SAMPLE},
            "bursts": {"cluster_gap_seconds": CLUSTER_GAP_SECONDS, "analysed": sum(observed.values()), "observed": observed, "expected": expected},
            "long_runs": long_runs}


def parse_baseline(path: Path) -> dict:
    """The original host's published medians, kept as verbatim table cells next to their parsed numbers."""
    text = path.read_text(encoding="utf-8")
    workloads = {}
    context = {}
    for line in text.splitlines():
        if not line.startswith("|"):
            continue
        cells = [c.strip() for c in line.strip().strip("|").split("|")]
        match = re.search(r"`(" + "|".join(WORKLOAD_ORDER) + r")`", cells[0])
        if match and len(cells) in (7, 8):
            names = ("ingest", "p50", "p99", "reopen", "rss", "recall")
            values = cells[-6:]
            workloads[match.group(1)] = {name: {"verbatim": cell, "numbers": [float(x.replace(",", "")) for x in re.findall(r"\d[\d,]*\.?\d*", cell)]}
                                         for name, cell in zip(names, values)}
        elif cells[0] in ("OS", "CPU", "RAM", "Storage", "Toolchain", "Commit", "Runs"):
            context[cells[0]] = cells[1]
    builds = {}
    match = re.search(r"HNSW index build: median ([\d,\.]+) ms", text)
    if match:
        builds["hnsw_100k_384"] = float(match.group(1).replace(",", ""))
    match = re.search(r"`hnsw_small` index build: median ([\d,\.]+) ms", text)
    if match:
        builds["hnsw_small"] = float(match.group(1).replace(",", ""))
    sizes = re.search(r"File sizes: ([\d,]+) bytes \(exact and HNSW\) and ([\d,]+) bytes \(filtered", text)
    return {"path": str(path), "sha256": sha256_file(path), "workloads": workloads, "context": context, "hnsw_index_build_ms": builds,
            "file_bytes_100k": {"exact_and_hnsw": int(sizes.group(1).replace(",", "")), "filtered": int(sizes.group(2).replace(",", ""))} if sizes else None}


# ---------------------------------------------------------------------------------------------- analysis
def slot_table(records, workloads, reps):
    by_slot = defaultdict(list)
    for r in records:
        by_slot[(r["workload"], r["label"], r["repetition"])].append(r)
    table = {}
    for workload in workloads:
        for label in LABELS:
            group = [r for r in records if r["workload"] == workload and r["label"] == label]
            attempts = [r for r in group if r["outcome"] != "not_attempted"]
            accepted_reps = sorted({r["repetition"] for r in group if r["outcome"] == "accepted"})
            missing = [rep for rep in range(1, reps + 1) if rep not in accepted_reps]
            table[f"{workload}/{label}"] = {
                "workload": workload, "label": label, "reps_required": reps, "slots_accepted": len(accepted_reps), "accepted_repetitions": accepted_reps,
                "missing_repetitions": missing, "attempts": len(attempts), "accepted": sum(r["outcome"] == "accepted" for r in group),
                "rejected": sum(r["outcome"] == "rejected" for r in group), "refused": sum(r["outcome"] == "refused" for r in group),
                "not_attempted": sum(r["outcome"] == "not_attempted" for r in group), "quarantined": sum(r["outcome"] == "quarantined" for r in group),
                "attempt_cap_per_slot": ATTEMPT_CAPS[workload],
                "slots_exhausted": sum(1 for rep in range(1, reps + 1) if rep not in accepted_reps
                                       and len([r for r in by_slot[(workload, label, rep)] if r["outcome"] != "not_attempted"]) >= ATTEMPT_CAPS[workload]),
            }
    return table


def compare(records, workloads):
    out = {}
    for workload in workloads:
        rows = []
        for metric in METRICS:
            if metric.hnsw_only and workload not in HNSW_WORKLOADS:
                continue
            per = {label: summarise([metric.get(r) for r in records if r["outcome"] == "accepted" and r["workload"] == workload and r["label"] == label])
                   for label in LABELS}
            hist, cur = per["hist"], per["cur"]
            both = hist is not None and cur is not None
            rows.append({
                "metric": metric.key, "heading": metric.heading, "unit": metric.unit, "higher_is_better": metric.higher_is_better, "hist": hist, "cur": cur,
                "delta_percent_of_medians": (cur["median"] - hist["median"]) / hist["median"] * 100 if both and hist["median"] else None,
                "ranges_overlap": (not (hist["max"] < cur["min"] or cur["max"] < hist["min"])) if both else None,
            })
        out[workload] = rows
    return out


def determinism(records, workloads):
    out = {}
    for workload in workloads:
        runs = [r for r in records if r["workload"] == workload and r.get("bench")]
        entry = {"runs_used": {label: sum(r["label"] == label for r in runs) for label in LABELS}, "keys": {}}
        sources = [(key, lambda b, key=key: b.get(key)) for key in DETERMINISTIC_KEYS]
        if workload in HNSW_WORKLOADS:
            sources += [(f"hnsw.{key}", lambda b, key=key: (b.get("hnsw") or {}).get(key)) for key in HNSW_DETERMINISTIC_KEYS]
        for key, get in sources:
            seen = {label: sorted({json.dumps(get(r["bench"]), sort_keys=True) for r in runs if r["label"] == label}) for label in LABELS}
            entry["keys"][key] = {
                "hist": [json.loads(v) for v in seen["hist"]], "cur": [json.loads(v) for v in seen["cur"]],
                "stable_within_each_revision": all(len(v) <= 1 for v in seen.values()),
                "same_across_revisions": bool(seen["hist"]) and seen["hist"] == seen["cur"],
            }
        entry["all_stable_within_each_revision"] = all(k["stable_within_each_revision"] for k in entry["keys"].values())
        entry["differing_across_revisions"] = [k for k, v in entry["keys"].items() if v["hist"] and v["cur"] and not v["same_across_revisions"]]
        out[workload] = entry
    return out


def survey_digest(survey, logical):
    if not survey:
        return None
    samples = survey.get("samples_raw") or []
    buckets, elapsed, background = [], 0.0, 0.0
    for sample in samples:
        elapsed += sample["elapsed_seconds"]
        background += sample["background_cpu_seconds"]
        if elapsed >= 60:
            buckets.append(background / (elapsed * logical))
            elapsed = background = 0.0
    if elapsed >= 30:
        buckets.append(background / (elapsed * logical))
    top = [(t["image"], t.get("share_of_all_cpus")) for t in survey["contributors_whole_survey"]["top"][:6]]
    return {
        "name": survey["name"], "started_utc": survey["started_utc"], "finished_utc": survey["finished_utc"], "seconds": survey["seconds"],
        "mean": survey["mean_background_cpu_fraction"], "worst_sample": survey["max_background_cpu_fraction"], "quiet": survey["quiet"],
        "share_of_samples_above_mean_limit": survey["fraction_of_samples_above_mean_limit"],
        "five_second_probes_quiet": survey["five_second_probe_blocks_quiet"], "five_second_probes": survey["five_second_probe_blocks"],
        "top_contributors": top, "per_minute_means": buckets,
    }


def run_git(path: Path, *args) -> str:
    result = subprocess.run(["git", "--no-optional-locks", *args], cwd=path, capture_output=True, text=True, encoding="utf-8", errors="replace", timeout=120)
    return (result.stdout if result.returncode == 0 else f"[exit {result.returncode}] {result.stderr}").strip()


DRIFT_BUILD_INPUTS = ("Cargo.toml", "Cargo.lock", "build.rs", "rust-toolchain", "rust-toolchain.toml")
DRIFT_DOCS = "documentation and site assets"
DRIFT_TESTS = "tests and benches (not built by `cargo build --bin vlite-bench`)"
DRIFT_SERVER = "server code (`#[cfg(feature = \"server\")]`)"
DRIFT_EMBEDDED = "assets embedded only by server code"
DRIFT_BUILD = "build input (Cargo.toml, Cargo.lock, build.rs, toolchain)"
DRIFT_OTHER = "other (needs review)"


def classify_drift(pinned: str, head: str) -> dict:
    """Path- and cfg-based reading of what changed after the pin. It is an inference: nothing is built at the newer head."""
    files = []
    for line in run_git(CLONE, "diff", "--numstat", pinned, head).splitlines():
        added, deleted, path = line.split("\t", 2)
        files.append({"path": path, "added": None if added == "-" else int(added), "deleted": None if deleted == "-" else int(deleted)})
    lib_rs = run_git(CLONE, "show", f"{head}:src/lib.rs")
    server_gated = bool(re.search(r'#\[cfg\(feature = "server"\)\]\s*pub mod server;', lib_rs))
    default_match = re.search(r"^\s*default\s*=\s*\[([^\]]*)\]", run_git(CLONE, "show", f"{head}:Cargo.toml"), re.M)
    default_features = None if default_match is None else [f.strip().strip('"') for f in default_match.group(1).split(",") if f.strip()]

    def is_server_source(path):
        return path == "src/server.rs" or path.startswith("src/server/")

    def category(path):
        if path in DRIFT_BUILD_INPUTS or path.startswith(".cargo/"):
            return DRIFT_BUILD
        if path.startswith(("tests/", "benches/")):
            return DRIFT_TESTS
        if path.startswith(("site/", "docs/")) or path.lower().startswith("readme") or path.endswith(".md"):
            return DRIFT_DOCS
        if is_server_source(path):
            return DRIFT_SERVER if server_gated else DRIFT_OTHER
        if path.startswith("src/") and not path.endswith(".rs"):
            grep = run_git(CLONE, "grep", "-l", "-F", path[len("src/"):], head, "--", "*.rs")
            users = [line.split(":", 1)[1] for line in grep.splitlines() if ":" in line and not line.startswith("[exit")]
            library_users = [u for u in users if not u.startswith(("tests/", "benches/"))]
            if library_users and server_gated and all(is_server_source(u) for u in library_users):
                return DRIFT_EMBEDDED
        return DRIFT_OTHER

    by_category = {}
    for f in files:
        f["category"] = category(f["path"])
        entry = by_category.setdefault(f["category"], {"files": 0, "added": 0, "deleted": 0})
        entry["files"] += 1
        entry["added"] += f["added"] or 0
        entry["deleted"] += f["deleted"] or 0
    needs_review = [f["path"] for f in files if f["category"] in (DRIFT_BUILD, DRIFT_OTHER)]
    default_is_empty = default_features == []
    bench_changed = [f["path"] for f in files if f["path"].startswith("src/bin/vlite-bench")]
    if not needs_review and default_is_empty and server_gated and not bench_changed:
        conclusion = ("No file changed after the pin is compiled into a default-feature `vlite-bench` build: the default feature set is empty, the server module is behind its non-default feature, "
                      "and the changes are documents, tests, site/dashboard assets and server-feature code. This is an inference from paths and `cfg` gating; the newer head was not built or measured.")
    else:
        conclusion = ("REVIEW NEEDED: the changes after the pin cannot be shown to be outside the default-feature `vlite-bench` build "
                      f"(default features {default_features!r}, server module gated: {server_gated}, paths needing review: {len(needs_review)}). The newer head was not built or measured.")
    return {"head": head, "files_changed": len(files), "lines_added": sum(f["added"] or 0 for f in files), "lines_deleted": sum(f["deleted"] or 0 for f in files),
            "by_category": by_category, "needs_review": needs_review, "server_module_feature_gated": server_gated, "default_features": default_features,
            "bench_harness_changed": bench_changed, "conclusion": conclusion, "files": files}


def git_state(pins) -> dict:
    state = {}
    for name, path in (("clone", CLONE), ("session_worktree", SESSION_WORKTREE)):
        state[name] = {
            "path": str(path), "head": run_git(path, "rev-parse", "HEAD"), "branch": run_git(path, "rev-parse", "--abbrev-ref", "HEAD"),
            "status_porcelain": run_git(path, "status", "--porcelain=v1", "--untracked-files=all"),
            "reflog_last_5": run_git(path, "reflog", "-n", "5", "--format=%h %gs").splitlines(),
        }
        state[name]["clean"] = state[name]["status_porcelain"] == ""
    pinned = pins["current"]["sha"]
    remote = run_git(CLONE, "ls-remote", "origin", "refs/heads/main")
    remote_sha = remote.split()[0] if remote and not remote.startswith("[exit") else None
    state["origin_main_at_report_time"] = remote
    state["origin_main_sha_at_report_time"] = remote_sha
    state["pinned_current_sha"] = pinned
    state["origin_main_still_pinned_sha"] = remote_sha == pinned
    if remote_sha and remote_sha != pinned:
        # Only the source clone's refs and objects change; the measured archives are already extracted and hashed.
        fetch = run_git(CLONE, "fetch", "--no-tags", "origin", "main")
        state["origin_main_drift"] = {
            "fetch_output": fetch,
            "commits_after_pin": run_git(CLONE, "log", "--format=%H %cI %s", f"{pinned}..{remote_sha}").splitlines(),
            "analysis": classify_drift(pinned, remote_sha),
        }
    return state


def contributor_totals(records, outcome, logical):
    elapsed, totals = 0.0, defaultdict(float)
    for r in records:
        if r["outcome"] != outcome or not r.get("run_contributors") or not r.get("run"):
            continue
        elapsed += r["run"]["elapsed_seconds"]
        for item in r["run_contributors"]["top"]:
            totals[item["image"]] += item["cpu_seconds"]
    ranked = sorted(totals.items(), key=lambda kv: kv[1], reverse=True)[:6]
    return {"runs": sum(r["outcome"] == outcome for r in records), "elapsed_seconds": elapsed,
            "top": [(name, seconds / (elapsed * logical) if elapsed else None) for name, seconds in ranked]}


def attempt_note(record, mean_limit=0.10, peak_limit=0.50):
    if record["outcome"] == "accepted":
        return ""
    if record["outcome"] == "quarantined":
        return f"quarantined by the report: a monitor sample lasted {record['longest_sample_seconds']:.1f} s (> {STALL_SAMPLE_SECONDS:g} s, host stall); the runner had accepted it"
    if record["outcome"] == "not_attempted":
        return "not attempted: workload abandoned (operational time bound)"
    if record["outcome"] == "refused":
        return "refused: no quiet 5 s pre-run probe"
    cpu = (record.get("run") or {}).get("cpu") or {}
    notes = []
    if cpu.get("mean_background_cpu_fraction", 0) > mean_limit:
        notes.append("mean > 10%")
    if cpu.get("max_background_cpu_fraction", 0) > peak_limit:
        notes.append("sample > 50%")
    notes += [reason for reason in record.get("reasons", []) if not reason.startswith("contaminated")]
    return "; ".join(notes) or "; ".join(record.get("reasons", []))


def analyse(d: dict) -> dict:
    records, config, host = d["records"], d["config"], d["host"]
    logical = host["computer_system"]["NumberOfLogicalProcessors"]
    workloads = [w for w in WORKLOAD_ORDER if w in config["workloads"]]
    reps = config["repetitions"]
    table = slot_table(records, workloads, reps)
    expected_slots = len(workloads) * len(LABELS) * reps
    accepted_slots = sum(v["slots_accepted"] for v in table.values())
    full_scope = tuple(workloads) == WORKLOAD_ORDER and reps == REQUIRED_REPS
    accepted = [r for r in records if r["outcome"] == "accepted"]
    gate = d["gate"]
    post = survey_digest(d["post_survey"], logical)
    pre = [survey_digest(s, logical) for s in d["gate_surveys"].values()]

    # Within each repetition and workload, which revision ran first.
    first = {"hist": 0, "cur": 0}
    for workload in workloads:
        for rep in range(1, reps + 1):
            pair = {r["label"]: r for r in accepted if r["workload"] == workload and r["repetition"] == rep}
            if len(pair) == 2:
                first[min(pair, key=lambda label: pair[label]["started_utc"])] += 1

    quiet_cpu = {}
    for key in table:
        workload, label = key.split("/")
        runs = [r for r in accepted if r["workload"] == workload and r["label"] == label]
        quiet_cpu[key] = {
            "accepted_runs": len(runs),
            "background_mean": summarise([r["run"]["cpu"]["mean_background_cpu_fraction"] for r in runs]),
            "background_peak": summarise([r["run"]["cpu"]["max_background_cpu_fraction"] for r in runs]),
            "runs_with_vacuous_peak_check": sum(bool(r["recomputed_cpu"]["peak_check_vacuous"]) for r in runs),
            "runs_shorter_than_1s": sum(r["run"]["elapsed_seconds"] < 1.0 for r in runs),
            "longest_monitor_sample_seconds": max((r["longest_sample_seconds"] for r in runs), default=None),
            "wall_seconds": summarise([r["run"]["elapsed_seconds"] for r in runs]),
        }

    sensitivity = {}
    for key in table:
        workload, label = key.split("/")
        done = [r for r in records if r["workload"] == workload and r["label"] == label and r.get("bench") and r.get("run") and r["run"]["returncode"] == 0]
        sensitivity[key] = {
            "accepted": summarise([METRIC_BY_KEY["ingest_vectors_per_second"].get(r) for r in done if r["outcome"] == "accepted"]),
            "rejected_excluded": summarise([METRIC_BY_KEY["ingest_vectors_per_second"].get(r) for r in done if r["outcome"] in ("rejected", "quarantined")]),
            "all_completed_attempts": summarise([METRIC_BY_KEY["ingest_vectors_per_second"].get(r) for r in done]),
            "wall_seconds_rejected_excluded": summarise([r["run"]["elapsed_seconds"] for r in done if r["outcome"] in ("rejected", "quarantined")]),
        }

    rejection_causes = {}
    for r in records:
        if r["outcome"] != "rejected":
            continue
        cpu = (r.get("run") or {}).get("cpu") or {}
        over_mean, over_peak = cpu.get("mean_background_cpu_fraction", 0) > 0.10, cpu.get("max_background_cpu_fraction", 0) > BURST_LIMIT
        kind = "both" if over_mean and over_peak else "peak_only" if over_peak else "mean_only" if over_mean else "other"
        rejection_causes.setdefault(r["workload"], {"peak_only": 0, "mean_only": 0, "both": 0, "other": 0})[kind] += 1

    starts = [r["started_utc"] for r in records]
    finishes = [r.get("finished_utc") for r in records if r.get("finished_utc")]
    fetch_done = max(datetime.fromisoformat(d["builds"][label]["fetch"]["started_utc"]).timestamp() + d["builds"][label]["fetch"]["elapsed_seconds"] for label in LABELS)
    builds_done = max(datetime.fromisoformat(b["started_utc"]).timestamp() + b["elapsed_seconds"] for label in LABELS for b in d["builds"][label]["builds"])
    first_window_start = min((datetime.fromisoformat(s["started_utc"]).timestamp() for s in pre if s), default=None)

    same_args = all(len({json.dumps(r["workload_args"]) for r in records if r["workload"] == w and r["outcome"] != "not_attempted"}) <= 1 for w in workloads)
    storage_dirs = {r["environment"]["VLITE_BENCH_DIR"] for r in records if r.get("environment")}
    all_have_monitor = all(r["run"]["cpu"].get("samples") for r in accepted)
    selftest = d["selftest"]
    git = git_state(d["pins"])

    runner_files = sorted(p for p in (d["camp"] / "runner").glob("*.py") if p.name != REPORT_MODULE)
    runner_now = {p.name: sha256_file(p) for p in runner_files}
    recorded = d["runner_hashes"]["runner_modules"]
    pinned_now = {name: sha256_file(Path(d["archives"]["cur"]["directory"]) / name) for name in d["runner_hashes"]["pinned_current_scripts"]}
    build_flags_ok = all({"--locked", "--offline", "--release"} <= set(b["command"]) for label in LABELS for b in d["builds"][label]["builds"])

    other_tags = {}
    for path in sorted((d["camp"] / "raw").iterdir()):
        if path.is_dir() and path.name != d["tag"]:
            runs_file = path / "runs.jsonl"
            other_tags[path.name] = sum(1 for line in runs_file.read_text(encoding="utf-8").splitlines() if line.strip()) if runs_file.exists() else 0

    order = {w: i for i, w in enumerate(WORKLOAD_ORDER)}
    accepted_values = [
        {"workload": r["workload"], "label": r["label"], "repetition": r["repetition"], "attempt": r["attempt"], "started_utc": r["started_utc"],
         **{m.key: m.get(r) for m in METRICS if not (m.hnsw_only and r["workload"] not in HNSW_WORKLOADS)},
         "background_mean": r["run"]["cpu"]["mean_background_cpu_fraction"]}
        for r in sorted(accepted, key=lambda r: (order[r["workload"]], r["repetition"], r["label"]))]

    sessions = d["sessions"]

    def others_idle(snapshot):
        return bool(snapshot) and snapshot.get("other_sessions_busy") == 0

    census_start, census_end = d["census_start"], d["census_end"]
    gate_passed = bool(gate and gate["passed"])
    # Pre-registered before the campaign: these three decide the status; the end-of-campaign checks are reported alongside.
    required_window = {
        "pre-window survey met the unchanged policy": gate_passed,
        "no build/benchmark processes at start": bool(census_start) and not census_start.get("blocking_processes"),
        "no other busy app sessions before the start": others_idle(sessions["pre"]),
    }
    reported_window = {
        "post survey met the policy": bool(post and post["quiet"]),
        "no build/benchmark processes at end": bool(census_end) and not census_end.get("blocking_processes"),
        "no other busy app sessions after the end": others_idle(sessions["post"]),
    }
    window_status = "met" if all(required_window.values()) else ("NOT MET" if not gate_passed else "partial")
    window_detail = ("required: " + "; ".join(f"{k}: {'yes' if v else 'NO or not recorded'}" for k, v in required_window.items()) +
                     " — also reported: " + "; ".join(f"{k}: {'yes' if v else 'NO or not recorded'}" for k, v in reported_window.items()) +
                     ". Every run additionally needed its own quiet pre-run probe and monitor-measured CPU within policy. Exclusivity of a shared host can be checked, not proven (see limitations).")
    clusters = burst_clusters(records, BURST_LIMIT, logical)
    activity = activity_coincidence(records, d["agent_activity"], logical, clusters)
    exclusive_status, exclusive_detail = window_status, window_detail
    if window_status == "met":
        if activity is None:
            exclusive_status = "partial"
            exclusive_detail += " The agent session that operated the campaign was active on the host while runs were measured and left no timestamped record, so exclusivity cannot be asserted."
        elif activity["process_calls_in_runs"] or activity["in_process_calls_in_runs"]:
            exclusive_status = "partial"
            exclusive_detail += (f" The pre-registered rule did not cover the agent session's own activity: {activity['process_calls_in_runs']} process-starting and "
                                 f"{activity['in_process_calls_in_runs']} in-process tool calls overlapped measured runs (section 4), so exclusivity is only partial.")

    window = {
        "pre_gate": gate, "pre_gate_surveys": pre, "post_survey": post, "exploratory": {name: survey_digest(s, logical) for name, s in d["explore"].items()},
        "census_start": census_start, "census_end": census_end, "census_changes": census_changes(census_start, census_end), "sessions": sessions,
        "status_rule": {"required": required_window, "reported": reported_window, "status": window_status},
        "exclusive_requirement_status": exclusive_status,
        "contributors_during_accepted_runs": contributor_totals(records, "accepted", logical),
        "contributors_during_rejected_runs": contributor_totals(records, "rejected", logical),
    }
    checks = [
        ("Both revisions pinned to full commit SHAs", "met", f"hist `{d['pins']['historical']['sha']}`, cur `{d['pins']['current']['sha']}`", "pins.json"),
        ("Separate clean `git archive` trees", "met" if all(d["archives"][l]["git_blob_sha1_verified_files"] == d["archives"][l]["files"] and not d["archives"][l]["has_dot_git"] for l in LABELS) else "not met",
         "every extracted file verified against its git blob SHA-1; no `.git` directory in either tree; "
         f"re-hashed at campaign start ({'ok' if (d['sources_start'] or {}).get('ok') else 'MISSING/FAILED'}) and end ({'ok' if (d['sources_end'] or {}).get('ok') else 'MISSING/FAILED'})",
         "archives.json, source-manifest-*.json, raw/<tag>/sources-verified-*.json"),
        ("Identical toolchain; release, default-feature builds", "met" if build_flags_ok and all(not b["graph_feature_enabled_anywhere"] and not b["graph_crate_compiled"] for l in LABELS for b in d["builds"][l]["builds"]) else "not met",
         f"rustc/cargo {d['toolchain']['rustup_toolchain']}; every build ran `cargo build --locked --offline --release` (flags verified in builds.json); the `graph` feature and the graph crate were not compiled in either build",
         "toolchain.json, builds.json, logs/cargo-*"),
        ("Identical workload arguments for both revisions", "met" if same_args else "not met", "each workload used one argument list for every recorded attempt of both revisions", "raw/<tag>/runs.jsonl, campaign-config.json"),
        ("Identical storage location", "met" if len(storage_dirs) == 1 else "not met", f"VLITE_BENCH_DIR, TEMP and TMP = {', '.join(sorted(storage_dirs))} (Q:, ReFS)", "raw/<tag>/runs.jsonl"),
        (f"{REQUIRED_REPS} repetitions of every smoke/reference workload", "met" if full_scope and accepted_slots == expected_slots else ("n/a (reduced scope)" if not full_scope else "NOT MET"),
         f"{accepted_slots} of {expected_slots} (workload x revision x repetition) slots have an accepted run"
         + ("" if full_scope else f"; this is a reduced-scope run ({len(workloads)} of {len(WORKLOAD_ORDER)} workloads x {reps} of {REQUIRED_REPS} repetitions), not the campaign"), "raw/<tag>/runs.jsonl"),
        ("Dependencies restored before the quiet window; locked/offline builds", "met" if first_window_start is None or max(fetch_done, builds_done) < first_window_start else "not met",
         "`cargo fetch` and both builds finished before the first window survey" + ("" if first_window_start else " (no window survey recorded)"), "builds.json, evidence/window/"),
        ("CPU policy unchanged (mean <= 10 %, any >= 0.1 s sample <= 50 %, scanner/System counted as background)", "met",
         f"pinned monitor sha256 `{selftest['wiring']['monitor_sha256'][:16]}…`; constants {json.dumps(selftest['wiring']['policy_constants'])}", "selftest.json"),
        ("The Windows CPU monitor was used for every measured run", "met" if all_have_monitor and selftest["wiring"]["campaign_runs_benchmarks_only_through_monitor_run_process"] else "not met",
         "every benchmark ran through the pinned `graph_reference.run_process`; "
         f"`graph_baseline.py measure` imports the CPU monitor: {selftest['wiring']['graph_baseline_measure_imports_cpu_monitor']}; "
         f"`measure_benchmark.py` samples CPU: {selftest['wiring']['measure_benchmark_samples_cpu']} (it records peak working set only)", "selftest.json, runs.jsonl"),
        ("Contaminated runs rejected, thresholds not relaxed", "met" if selftest["ok"] else "not met",
         f"{sum(r['outcome'] == 'rejected' for r in records)} rejected, {sum(r['outcome'] == 'refused' for r in records)} refused, {sum(r['outcome'] == 'quarantined' for r in records)} quarantined by the report's stalled-sample rule "
         f"(a monitor sample longer than {STALL_SAMPLE_SECONDS:g} s; the thresholds themselves were not touched); boundary tests and live violation tests passed: {selftest['ok']}", "selftest.json, evidence/window/incident-vm-freeze.json"),
        ("Exclusive quiet window verified", exclusive_status, exclusive_detail, "evidence/window/, raw/<tag>/census-*.json"),
        ("Host identity, OS, CPU, RAM, storage, toolchain, source/binary/runner hashes recorded", "met", "see the evidence directory and the sections below", "host.json, toolchain.json, builds.json, archives.json, runner-hashes.json"),
        ("Baseline documents not modified, no gate approved, nothing committed or pushed", "met" if git["clone"]["clean"] and git["session_worktree"]["clean"] else "CHECK",
         f"`git status` clean in the clone ({git['clone']['clean']}) and the session worktree ({git['session_worktree']['clean']}); the runner only reads sources through `git archive`, `fetch`, `ls-remote` and `rev-parse`",
         "git state section"),
    ]
    return {
        "generated_utc": now_utc(), "tag": d["tag"], "campaign_directory": str(d["camp"]), "config": config,
        "scope": {"workloads": workloads, "repetitions": reps, "full_scope": full_scope, "expected_slots": expected_slots, "accepted_slots": accepted_slots},
        "verdict": "COMPLETE" if full_scope and accepted_slots == expected_slots else ("DRY RUN / REDUCED SCOPE" if not full_scope else "INCOMPLETE"),
        "time": {"first_attempt_utc": min(starts) if starts else None, "last_attempt_finished_utc": max(finishes) if finishes else None},
        "slots": table, "comparison": compare(records, workloads), "quiet_cpu_of_accepted_runs": quiet_cpu, "sensitivity_informational": sensitivity,
        "determinism": determinism(records, workloads), "window": window, "revision_order_first_counts": first,
        "counts": {o: sum(r["outcome"] == o for r in records) for o in ("accepted", "rejected", "refused", "not_attempted", "quarantined")},
        "stall_rule": d["stall_rule"], "incident": d.get("incident"), "burst_samples_above_limit": burst_samples(records, BURST_LIMIT, logical),
        "burst_clusters": [cluster_row(c) for c in clusters],
        "agent_activity": activity,
        "rejection_causes": {w: rejection_causes[w] for w in WORKLOAD_ORDER if w in rejection_causes},
        "git_state": git, "requirement_checks": [{"requirement": a, "status": b, "detail": c, "evidence": e} for a, b, c, e in checks],
        "runner_hash_check": {"modules_unchanged_since_evidence": {name: recorded.get(name) == digest for name, digest in runner_now.items()}, "recorded": recorded, "now": runner_now,
                              "recorded_modules_missing_now": [n for n in recorded if n not in runner_now],
                              "report_generator": {"file": REPORT_MODULE, "sha256": sha256_file(Path(__file__)),
                                                   "note": "post-processing only; deliberately not frozen before the campaign, hash recorded at generation time"},
                              "pinned_scripts_unchanged": {name: pinned_now[name] == d["runner_hashes"]["pinned_current_scripts"][name] for name in pinned_now}},
        "logical_cpus": logical, "other_raw_tags": other_tags, "accepted_run_values": accepted_values,
        "ledger": [{"started_utc": r["started_utc"], "repetition": r["repetition"], "workload": r["workload"], "label": r["label"], "attempt": r["attempt"],
                    "outcome": r["outcome"], "wall_seconds": (r.get("run") or {}).get("elapsed_seconds"),
                    "background_mean": ((r.get("run") or {}).get("cpu") or {}).get("mean_background_cpu_fraction"),
                    "background_max_sample": ((r.get("run") or {}).get("cpu") or {}).get("max_background_cpu_fraction"), "note": attempt_note(r)} for r in records],
    }


# ---------------------------------------------------------------------------------------------- markdown
def banner(model, d) -> str:
    scope = model["scope"]
    host = d["host"]
    lines = [f"> **Status: {model['verdict']}** — {scope['accepted_slots']} of {scope['expected_slots']} (workload × revision × repetition) slots have an accepted run "
             f"({model['counts']['rejected']} attempts rejected, {model['counts']['refused']} refused, {model['counts']['not_attempted']} slots not attempted"
             + (f", {model['counts']['quarantined']} quarantined for a stalled monitor sample" if model["counts"]["quarantined"] else "") + ").",
             f"> **Separate host** `{host['computer_system']['Name']}` — {host['cpu']['Name'].strip()}, {host['computer_system']['NumberOfLogicalProcessors']} logical CPUs. "
             "This report does not replace, amend or approve the original host's baseline; no gate is evaluated or approved; nothing was committed or pushed; baseline documents are untouched.",
             "> **CPU acceptance policy unchanged**: background CPU mean ≤ 10 %, every sample of at least 0.1 s ≤ 50 %, scanner/System CPU counted as background, measured by the pinned `scripts/graph_reference.py` monitor."]
    if model["verdict"] != "COMPLETE":
        lines.append("> Slots without an accepted run are **missing evidence**, not zero or interpolated values; see \"Missing evidence\".")
    return "\n".join(lines)


def section_summary(model, d) -> str:
    rows = []
    for workload in model["scope"]["workloads"]:
        cells = [f"`{workload}`"]
        for label in LABELS:
            s = model["slots"][f"{workload}/{label}"]
            cells += [f"{s['slots_accepted']}/{s['reps_required']}", f"{s['accepted']}/{s['rejected']}/{s['refused']}/{s['not_attempted']}" + (f" +{s['quarantined']}q" if s["quarantined"] else "")]
        rows.append(cells)
    text = ["## 1. Outcome", md_table(["Workload", "hist accepted slots", "hist attempts a/r/f/n", "cur accepted slots", "cur attempts a/r/f/n"], rows, ["l", "r", "r", "r", "r"]),
            "", "a/r/f/n = accepted / rejected (contaminated or failed) / refused (no quiet pre-run probe) / not attempted; +Nq = N runs the runner accepted but this report quarantined (a monitor sample longer than "
            f"{STALL_SAMPLE_SECONDS:g} s, section 4). "
            "Retries are bounded per slot (operational time bounds, not quality thresholds): " + ", ".join(f"`{k}` {v}" for k, v in ATTEMPT_CAPS.items()) +
            f"; a workload stops being attempted after {ABANDON_AFTER_EXHAUSTED_NO_ACCEPTED} slots exhausted their attempts with nothing accepted (or {ABANDON_AFTER_EXHAUSTED_ANY} exhausted slots overall); "
            f"the campaign stops after {MAX_CONSECUTIVE_REFUSALS} consecutive refused pre-run probes."]
    missing = [(k, v) for k, v in model["slots"].items() if v["missing_repetitions"]]
    if missing:
        text += ["", "### Missing evidence"]
        for key, v in missing:
            why = []
            if v["slots_exhausted"]:
                why.append(f"{v['slots_exhausted']} slot(s) used all {v['attempt_cap_per_slot']} allowed attempts without passing the CPU policy")
            if v["not_attempted"]:
                why.append(f"{v['not_attempted']} slot(s) not attempted after the workload was abandoned")
            if v["refused"]:
                why.append(f"{v['refused']} attempt(s) refused (no quiet pre-run probe)")
            if v["quarantined"]:
                why.append(f"{v['quarantined']} attempt(s) quarantined (stalled monitor sample)")
            text.append(f"- `{key}`: repetitions {v['missing_repetitions']} have no accepted run" + (f" — {'; '.join(why)}" if why else ""))
    return "\n".join(text)


def section_results(model) -> str:
    text = ["## 2. Results — historical `81ff901` vs current `origin/main`",
            "Statistics use **accepted runs only** (median, min–max, n). Δ is (cur median − hist median) / hist median. With n ≤ 5 no significance is claimed; "
            "\"ranges overlap\" only says the two observed ranges intersect. Higher is better for throughput and recall; lower is better for times and memory."]
    for workload, rows in model["comparison"].items():
        text += ["", f"### `{workload}`"]
        body = []
        for row in rows:
            def cell(s):
                return "no accepted run" if not s else f"{num(s['median'], row['unit'])} ({num(s['min'], row['unit'])}–{num(s['max'], row['unit'])}) n={s['n']}"
            delta = "–" if row["delta_percent_of_medians"] is None else f"{row['delta_percent_of_medians']:+.1f}%"
            overlap = "–" if row["ranges_overlap"] is None else ("overlap" if row["ranges_overlap"] else "disjoint")
            body.append([row["heading"], row["unit"] or "–", cell(row["hist"]), cell(row["cur"]), delta, overlap])
        text.append(md_table(["Metric", "Unit", "hist: median (min–max)", "cur: median (min–max)", "Δ medians", "ranges"], body, ["l", "l", "r", "r", "r", "c"]))
    first = model["revision_order_first_counts"]
    text += ["", f"Revision order inside each accepted pair alternates by design; the historical revision ran first in {plural(first['hist'], 'pair')} and the current one in {plural(first['cur'], 'pair')}."]
    return "\n".join(text)


def section_cpu(model) -> str:
    rows = []
    for key, q in model["quiet_cpu_of_accepted_runs"].items():
        mean, peak, wall = q["background_mean"], q["background_peak"], q["wall_seconds"]
        peak_cell = "–" if not peak else ("n/a (vacuous)" if q["runs_with_vacuous_peak_check"] == q["accepted_runs"] else pct(peak["max"]))
        rows.append([f"`{key}`", q["accepted_runs"], pct(mean["median"]) if mean else "–", pct(mean["max"]) if mean else "–", peak_cell,
                     f"{q['runs_with_vacuous_peak_check']}", f"{q['runs_shorter_than_1s']}", num(wall["median"], "s") if wall else "–",
                     "–" if q["longest_monitor_sample_seconds"] is None else f"{q['longest_monitor_sample_seconds']:.2f}"])
    contrib = model["window"]
    text = ["## 3. CPU-policy outcome of the measured runs",
            "Background CPU is system busy time minus the benchmark's own CPU, sampled every 0.5 s by the pinned monitor; the limits are 10 % (mean over the run) and 50 % (any sample of at least 0.1 s).",
            md_table(["Workload/revision", "accepted", "median mean bg", "max mean bg", "max sample bg", "peak check vacuous", "runs < 1 s", "median wall (s)", "longest monitor sample (s)"], rows,
                     ["l", "r", "r", "r", "r", "r", "r", "r", "r"]),
            "", "\"Peak check vacuous\": the run was shorter than 0.1 s, so the monitor's single final sample is not counted for the peak rule and acceptance rests on the mean alone. "
            f"\"Longest monitor sample\" is the report's stalled-sample check (a run is quarantined above {STALL_SAMPLE_SECONDS:g} s; nominal sample length 0.5 s); for the sub-second workloads it is the run's own length.",
            "", "Where the background CPU went (share of all logical CPUs over the measured child runs; top entries of each run's process census, so a lower bound):"]
    for name, title in (("contributors_during_accepted_runs", "accepted runs"), ("contributors_during_rejected_runs", "rejected runs")):
        c = contrib[name]
        text.append(f"- {title} ({c['runs']} runs, {c['elapsed_seconds']:.0f} s): " + (", ".join(f"{img} {pct(share)}" for img, share in c["top"]) or "none"))
    causes = model["rejection_causes"]
    if causes:
        text += ["", "Why attempts were rejected by the unchanged limits:",
                 md_table(["Workload", "rejected", "only the 50 % sample limit", "only the 10 % mean limit", "both", "other (not CPU)"],
                          [[f"`{w}`", sum(c.values()), c["peak_only"], c["mean_only"], c["both"], c["other"]] for w, c in causes.items()], ["l", "r", "r", "r", "r", "r"])]
    return "\n".join(text)


def section_window(model, d) -> str:
    w = model["window"]
    text = ["## 4. Quiet-window evidence"]
    gate = w["pre_gate"]
    if gate:
        text += [f"**Pre-window gate**: {'PASSED' if gate['passed'] else 'NOT PASSED'} — up to {gate['tries_allowed']} passive surveys of {gate['survey_seconds']:g} s under the unchanged policy; "
                 f"{len(gate['attempts'])} used."]
    rows = [[s["name"], hhmmss(s["started_utc"]), f"{s['seconds']:.0f}", pct(s["mean"]), pct(s["worst_sample"]), "yes" if s["quiet"] else "NO", f"{s['five_second_probes_quiet']}/{s['five_second_probes']}",
             ", ".join(f"{img} {pct(share)}" for img, share in s["top_contributors"][:4])]
            for s in (*w["pre_gate_surveys"], w["post_survey"]) if s]
    if rows:
        text.append(md_table(["Survey", "UTC", "s", "mean bg", "worst sample", "policy met", "5 s probes quiet", "top contributors (share of all CPUs)"], rows, ["l", "l", "r", "r", "r", "c", "r", "l"]))
    text += ["", "**Exploratory long surveys** (not gating; taken while the host was otherwise unused, to characterise its floor):"]
    for name, s in w["exploratory"].items():
        text.append(f"- `{name}`: {s['seconds']:.0f} s, mean {pct(s['mean'])}, worst sample {pct(s['worst_sample'])}, policy met: {s['quiet']}, 5 s probes quiet {s['five_second_probes_quiet']}/{s['five_second_probes']}; "
                    f"per-minute means: {', '.join(f'{m * 100:.1f}' for m in s['per_minute_means'])} %; top: " + ", ".join(f"{img} {pct(share)}" for img, share in s["top_contributors"]))
    text += ["", "**Process censuses and other app sessions:**"]
    for label, census in (("start", w["census_start"]), ("end", w["census_end"])):
        if census:
            text.append(f"- Process census at campaign {label} ({sum(census['watched_processes'].values())} watched processes of {len(census['watched_processes'])} kinds): blocking build/benchmark processes: {census.get('blocking_processes') or 'none'}.")
    if w["census_start"] and w["census_end"]:
        changes = w["census_changes"]
        text.append("- Watched-process differences between the start and end census: " + (", ".join(f"{n} {a}→{b}" for n, (a, b) in changes.items()) or "none")
                    + ". (Servicing processes such as `TiWorker.exe`/`TrustedInstaller.exe` appearing mid-campaign would be a contamination source; full censuses are in `raw/<tag>/census-*.json`.)")
    for name, s in w["sessions"].items():
        if s:
            text.append(f"- Other app sessions ({name}, {s.get('taken_utc')}): {s.get('other_sessions_busy', '?')} busy of {s.get('other_sessions_total', '?')} (this session excluded); {s.get('note', '')}")
        else:
            text.append(f"- Other app sessions ({name}): no snapshot recorded.")
    status = w["status_rule"]
    text += ["", f"**Quiet-window status: {status['status']}**, by a rule fixed before the campaign: it is `met` only if the pre-window survey passed the unchanged policy, no build/benchmark processes were running at the start, "
             "and no other app session was busy before the start; `NOT MET` if the pre-window survey never passed; otherwise `partial`. The post survey and the end-of-campaign checks are reported but do not decide the status "
             "(contamination during a run is caught by that run's own monitor check).",
             "", "Every individual run additionally required a quiet 5-second pre-run probe (the pinned `calm_preflight`) and was judged by the monitor's own samples; contaminated runs were rejected and their CPU numbers kept in the ledger."]
    return "\n".join(text) + "\n" + section_incident(model, d) + "\n" + section_agent_activity(model, d)


def section_incident(model, d) -> str:
    rule, inc = model["stall_rule"], d.get("incident")
    text = ["", "### Host stall incident and the stalled-sample rule"]
    if inc:
        rec, tl = inc["affected_record"], inc["guest_timeline_utc"]
        text += [f"**{inc['title']}.** {inc['how_detected']}", "",
                 f"- Estimated stall: {tl['estimated_freeze_start_utc']}; about {tl['frozen_interval_minutes_about']} minutes. The system tick count advanced by {tl['tick_count_delta_ms']:,} ms between two Time-Service events "
                 f"{tl['wall_clock_delta_between_the_two_time_service_events_s']} s apart, so both clocks include the gap.",
                 f"- Affected attempt: repetition {rec['repetition']}, `{rec['workload']}`, {rec['label']}, attempt {rec['attempt']} ({hhmmss(rec['started_utc'])}–{hhmmss(rec['finished_utc'])} UTC): recorded {rec['elapsed_seconds_recorded']} s "
                 f"instead of the usual {rec['typical_elapsed_seconds_for_this_workload']} s; longest monitor sample {rec['longest_monitor_sample_seconds']} s; verdict of the unchanged policy: {rec['monitor_verdict']}."]
        text += [f"- {item}" for item in inc["findings"]]
        text += [f"- Handling: {item}" for item in inc["handling"]]
        text.append("- Evidence file: `evidence/window/incident-vm-freeze.json`.")
    else:
        text.append("No host stall incident record exists for this campaign.")
    quarantined = rule["quarantined"]
    note = d.get("incident_file")
    when = (f" The rule is written in the incident record (`incident-vm-freeze.json`, last modified {note['last_modified_utc']} UTC, when {note['attempts_finished_before']} of the campaign's "
            f"{note['attempts_recorded_now']} recorded attempts had finished) and can only remove runs." if note else " It can only remove runs.")
    text += ["", f"**Stalled-sample rule** (applied by this report generator to every run; the frozen runner is unchanged and the CPU thresholds are untouched): a run the runner accepted is quarantined and excluded from every "
             f"statistic when any monitor sample lasted longer than {rule['limit_seconds']:g} s (ten times the nominal 0.5 s).{when}",
             f"- Result: {len(quarantined)} of the runner's {rule['runner_accepted']} accepted runs quarantined" + ("." if not quarantined else ": "
                + "; ".join(f"rep {q['repetition']} `{q['workload']}` {q['label']} attempt {q['attempt']} (longest sample {q['longest_sample_seconds']:.1f} s)" for q in quarantined) + "."),
             f"- Longest monitor sample in any run that remains accepted: {'–' if rule['longest_sample_in_accepted_runs'] is None else format(rule['longest_sample_in_accepted_runs'], '.3f') + ' s'}; "
             f"longest in any attempt without a stalled sample: {'–' if rule['longest_sample_in_unstalled_attempts'] is None else format(rule['longest_sample_in_unstalled_attempts'], '.3f') + ' s'}; "
             f"attempts containing a sample over the limit: " + (", ".join(f"rep {s['repetition']} `{s['workload']}` {s['label']} attempt {s['attempt']} ({s['longest_sample_seconds']:.0f} s, {s['outcome']})" for s in rule["stalled_attempts"]) or "none") + "."]
    return "\n".join(text)


def section_agent_activity(model, d) -> str:
    co = model["agent_activity"]
    text = ["", "### Agent activity during the measurement window",
            "The agent session that operated this campaign shared the host with the measured runs. While runs were in progress it polled the campaign log, read and analysed raw results and edited files, "
            "and nothing in the protocol prevented that. Its processes (PowerShell, Python, ripgrep) and the Defender scans they trigger count as background CPU, as the policy requires: "
            "runs that exceeded the unchanged limits stay rejected, and accepted runs may still contain agent-induced bursts that stayed within the limits."]
    if not co:
        text.append("No timestamped record of the agent's tool calls was written (`evidence/window/agent-activity.json` is absent), so the timing of that activity against the monitor's samples is not analysed here.")
        return "\n".join(text)
    tolerance, comparison, bursts = co["tolerance_seconds"], co["comparison"], co["bursts"]
    activity_file = d["ev"] / "window" / "agent-activity.json"
    text += ["",
             f"- Record: `evidence/window/agent-activity.json` (sha256 `{sha256_file(activity_file)}`) lists {plural(co['calls_total'], 'tool call')} from {co['first_call_utc']} to {co['last_call_utc']} UTC, "
             f"{co['process_calls']} of them starting an operating-system process, and {plural(co['model_turns'], 'model turn')}; by tool: {', '.join(f'{k} {v}' for k, v in co['calls_by_tool'].items())}. Source: {co.get('source') or 'not stated'}.",
             f"- Method: a synchronous command counts as active from its start to its end; every other call counts at its start, at its end and, for an asynchronous shell whose completion was announced, at its exit. "
             f"A monitor sample *coincides* with a call when it comes within ±{tolerance:g} s of such a moment. Calls that start an operating-system process (PowerShell, ripgrep and what they launch) are kept apart from "
             "in-process calls (file views, edits, waiting on a shell); a *model turn* is a model request in progress, network latency included. This measures timing only.",
             f"- Overlap with measured runs: {co['process_calls_in_runs']} process-starting and {co['in_process_calls_in_runs']} in-process calls overlapped at least one measured run (any outcome)."]

    def share(count, total):
        return f"{count} ({pct(count / total, 1)})" if total else "–"

    labels = {"process": "a process-starting call", "in_process": "only in-process calls", "turn": "only a model turn in progress", "none": "none of these"}
    everything, accepted = comparison["populations"]["all"], comparison["populations"]["accepted"]
    hours = sum(t["seconds"] for t in everything.values()) / 3600
    skipped = comparison["attempts_excluded_for_stall"]
    text += ["", f"**Background CPU near and away from the agent's activity.** Monitor samples of at least 0.1 s from the {comparison['attempts_used']} long (≥ {comparison['long_run_seconds']:g} s) attempts, "
             f"leaving out each run's first {comparison['start_up_seconds']:g} s" + (" and the stalled attempt (" + ", ".join(f"rep {s['repetition']} {s['label']} attempt {s['attempt']}" for s in skipped) + ")" if skipped else "")
             + f" — {hours:.1f} h in all — sorted by what the agent was doing within ±{tolerance:g} s of the sample (the first matching row):"]
    rows = [[labels[name], f"{everything[name]['seconds']:,.0f}", pct(everything[name]["mean_background"]), pct(everything[name]["median_sample_background"]),
             share(everything[name]["over_elevated"], everything[name]["samples"]), share(everything[name]["over_limit"], everything[name]["samples"]),
             f"{accepted[name]['seconds']:,.0f}", pct(accepted[name]["mean_background"]), share(accepted[name]["over_elevated"], accepted[name]["samples"])] for name in ACTIVITY_CLASSES]
    text.append(md_table(["The agent was doing", "all long attempts: seconds", "mean background", "median sample", f"samples over {comparison['elevated_sample']:.0%}", f"samples over {BURST_LIMIT:.0%}",
                          "accepted attempts only: seconds", "mean background", f"samples over {comparison['elevated_sample']:.0%}"], rows, ["l", "r", "r", "r", "r", "r", "r", "r", "r"]))
    text.append("Rows differ in what else was going on besides the agent's tool call: the table shows how background CPU co-varied with the agent's activity, not what caused it. The means are time-weighted like the monitor's own.")
    if bursts["analysed"]:
        names = {"process": "a process-starting call", "in_process_only": "only in-process calls", "neither": "neither"}
        rows = [[names[key], bursts["observed"][key], f"{bursts['expected'][key]:.1f}"] for key in names]
        text += ["", f"**Bursts over the {BURST_LIMIT:.0%} limit.** The {bursts['analysed']} bursts in the attempts without a stall (consecutive over-limit samples less than {bursts['cluster_gap_seconds']:g} s apart; listed in appendix D with "
                 f"the nearest call), by what the agent was doing within ±{tolerance:g} s of the burst. *Expected* is the number to expect if each burst had landed at a random place in its own attempt "
                 "(a stretch of the same length slid over every position of that attempt):",
                 md_table(["The agent was doing", "bursts observed", "bursts expected if unrelated"], rows, ["l", "r", "r"])]
    long_runs = co["long_runs"]
    by_outcome = defaultdict(list)
    for item in long_runs:
        by_outcome[item["outcome"]].append(item["process_calls"])
    if by_outcome:
        idle = defaultdict(lambda: [0, 0])
        for item in long_runs:
            idle[item["outcome"]][1] += 1
            idle[item["outcome"]][0] += not (item["process_calls"] or item["in_process_calls"])
        text.append(f"Process-starting calls overlapping each long (≥ {comparison['long_run_seconds']:g} s) attempt, by outcome: "
                    + "; ".join(f"{outcome} n={len(v)}, median {statistics.median(v):g} (min {min(v)}, max {max(v)})" for outcome, v in sorted(by_outcome.items()))
                    + ". Attempts that overlapped no tool call of any kind: " + "; ".join(f"{outcome} {idle[outcome][0]} of {idle[outcome][1]}" for outcome in sorted(idle)) + ".")
        rows = [[i["repetition"], f"`{i['workload']}`", i["label"], i["attempt"], i["outcome"] + (" (stalled)" if i["stalled"] else ""), pct(i["background_mean"]), pct(i["background_max_sample"]),
                 i["process_calls"], i["in_process_calls"]]
                for i in sorted(long_runs, key=lambda i: (i["repetition"], i["workload"], i["label"], i["attempt"]))]
        text.append(md_table(["rep", "workload", "rev", "try", "outcome", "bg mean", "bg max sample", "process-starting calls", "in-process calls"], rows, ["r", "l", "l", "r", "l", "r", "r", "r", "r"]))
    text += ["", "Coincidence in time is not proof of cause: Defender, the VPN/zero-trust client, management agents and the guest itself also burst on their own (section 10). "
             "The tables are evidence for the reader; no run was re-labelled because of them."]
    exclusive, rule = model["window"]["exclusive_requirement_status"], model["window"]["status_rule"]["status"]
    if exclusive != rule:
        text.append(f"The pre-registered status rule above did not cover this activity, so section 7 reports the exclusive-quiet-window requirement as **{exclusive}** rather than `{rule}`.")
    return "\n".join(text)


def section_host(d) -> str:
    h, reference = d["host"], d["baseline"]["context"]
    disk, vol = h["disk"], h["volume"]
    rows = [
        ["Computer", f"`{h['computer_system']['Name']}` ({h['computer_system']['Model']}, {h['bios']['SMBIOSBIOSVersion']}; hypervisor present: {h['computer_system']['HypervisorPresent']})", "–"],
        ["OS", f"{h['os']['Caption']} {h['os']['Version']} ({h['current_version']['DisplayVersion']}, UBR {h['current_version']['UBR']}), booted {ms_date(h['os']['LastBootUpTime'])}", reference.get("OS", "–")],
        ["CPU", f"{h['cpu']['Name'].strip()} ({h['cpu']['Description']}), {h['cpu']['NumberOfCores']} cores / {h['cpu']['NumberOfLogicalProcessors']} logical, {h['cpu']['MaxClockSpeed']} MHz, L2 {h['cpu']['L2CacheSize']} KB, L3 {h['cpu']['L3CacheSize']} KB", reference.get("CPU", "–")],
        ["RAM", f"{h['computer_system']['TotalPhysicalMemory']:,} bytes", reference.get("RAM", "–")],
        ["Storage", f"`{vol['DriveLetter']}:` {vol['FileSystem']} `{vol['FileSystemLabel']}`, {vol['Size']:,} bytes ({vol['SizeRemaining']:,} free at capture) on `{disk['FriendlyName']}` ({disk['BusType']}, {disk['Size']:,} bytes, {h['physical_disks']['MediaType']}); Dev Drive settings not readable without administrator rights",
         reference.get("Storage", "–")],
        ["Power plan / time zone", f"{h['power_scheme'].split('(')[-1].rstrip(')')}; {h['timezone']['DisplayName']}", "–"],
        ["Security software", f"Defender real-time protection {h['defender_status'].get('RealTimeProtectionEnabled')}; exclusions not viewable (not administrator); sensor and management agents run continuously (see window evidence)", "The reference host is described as never idle either (security and management agents)"],
        ["Interactive sessions", f"`{session_line(h['logged_on_sessions'])}` (`query user`); WSL `Ubuntu-24.04` was running and was left untouched", "–"],
    ]
    checks = d["identity"]
    text = ["## 5. Host identity", md_table(["Item", "This host (captured before the measurements)", "Original reference host (verbatim from the pinned baseline.md)"], rows),
            "", "Identity checks against the reference host (the pinned `graph_reference.main()` gate, evaluated for the record only; its `main()` was not run):",
            md_table(["Check", "Result"], [[k, "yes" if v else "**NO**"] for k, v in checks.items()]),
            "", "The CPU model and the RAM size differ from the reference host, so this is a **different machine**; results here are not comparable with the original baseline for REG-010 purposes."]
    return "\n".join(text)


def section_sources(model, d) -> str:
    pins, arch, builds = d["pins"], d["archives"], d["builds"]
    rows = [[LABEL_NAMES[l], f"`{pins['historical' if l == 'hist' else 'current']['sha']}`", f"`{pins['historical' if l == 'hist' else 'current']['tree']}`",
             pins['historical' if l == 'hist' else 'current']['subject'], pins['historical' if l == 'hist' else 'current']['committer_date']] for l in LABELS]
    arows = [[LABEL_NAMES[l], arch[l]["files"], f"`{arch[l]['tar_sha256']}`", f"`{arch[l]['source_rollup_sha256']}`", f"`{arch[l]['manifest_sha256'][:16]}…`"] for l in LABELS]
    brows = []
    for l in LABELS:
        for name, info in builds[l]["binaries"].items():
            brows.append([LABEL_NAMES[l], name, f"{info['bytes']:,}", f"`{info['sha256']}`"])
    tc = d["toolchain"]
    bench_src = "src/bin/vlite-bench.rs"
    same_harness = arch["hist"]["key_files_sha256"].get(bench_src) is not None and arch["hist"]["key_files_sha256"].get(bench_src) == arch["cur"]["key_files_sha256"].get(bench_src)
    versions = {label: sorted({r["bench"]["engine_version"] for r in d["records"] if r["label"] == label and r.get("bench")}) for label in LABELS}
    generator = model["runner_hash_check"]["report_generator"]
    text = ["## 6. Pinned sources, toolchain, binaries and runner",
            f"Revisions pinned {pins['pinned_utc']}; `git ls-remote origin main` agreed with the fetched `origin/main`; the historical commit is an ancestor of the current one ({pins['ancestor_check']['commits_between']} commits between). "
            f"An earlier fetch of `origin/main` (`{pins['superseded_earlier_fetch']['sha']}`) was superseded before anything was built or measured.",
            md_table(["Revision", "Commit", "Tree", "Subject", "Committer date"], rows),
            "", md_table(["Source archive", "Files", "Tar SHA-256", "Source roll-up SHA-256", "Manifest SHA-256"], arows),
            "", "Archives were produced with `git -c core.autocrlf=false -c core.eol=lf archive` (this host's git has `core.autocrlf=true`, which would otherwise write CRLF) and every file was verified against its git blob; the repository's own `graph_compat_check.extract` uses a plain `git archive`. Both revisions are treated identically.",
            "", md_table(["Revision", "Binary", "Bytes", "SHA-256"], brows),
            "", (f"`{bench_src}` is byte-identical in both revisions (sha256 `{arch['cur']['key_files_sha256'][bench_src][:16]}…`), so both revisions are measured by the same harness; only the library differs. " if same_harness
                 else f"`{bench_src}` differs between the revisions; the harness is therefore not identical. ") +
            f"The bench reports engine versions {', '.join(f'`{v}` ({LABEL_NAMES[l]})' for l, vs in versions.items() for v in vs) or 'none (no parseable output)'}.",
            "", f"Toolchain: {tc['rustc_vV']['stdout'].splitlines()[0]}; {tc['cargo_vV']['stdout'].splitlines()[0]}; target `x86_64-pc-windows-msvc`; Python {tc['python']['version'].split()[0]}; linker `{tidy_path(tc['linker_probe']['linker_path'])}` (sha256 `{tc['linker_probe']['sha256']}`). "
            "Builds ran offline and locked after one online `cargo fetch` per revision; `CARGO_INCREMENTAL=0`; inherited Rust/Cargo environment variables were dropped.",
            "", "Runner and pinned-script hashes (the measurement modules were recorded before the campaign and re-checked when this report was generated):",
            md_table(["File", "SHA-256", "unchanged since recorded"],
                     [[f"runner/{n}", f"`{h}`", "yes" if model["runner_hash_check"]["modules_unchanged_since_evidence"].get(n) else "**NO**"] for n, h in d["runner_hashes"]["runner_modules"].items()] +
                     [[f"pinned {n}", f"`{h}`", "yes" if model["runner_hash_check"]["pinned_scripts_unchanged"][n] else "**NO**"] for n, h in d["runner_hashes"]["pinned_current_scripts"].items()] +
                     [["python.exe", f"`{d['runner_hashes']['python_executable_sha256']}`", "–"],
                      [f"runner/{generator['file']} (report generator)", f"`{generator['sha256']}`", "post-processing only; hash taken when this report was generated"]])]
    return "\n".join(text)


def section_protocol(model) -> str:
    rows = [[c["requirement"], c["status"], c["detail"], c["evidence"]] for c in model["requirement_checks"]]
    st = model["git_state"]
    text = ["## 7. Protocol and requirement checks", md_table(["Requirement", "Status", "Detail", "Evidence"], rows),
            "", f"Git state at report time — clone `{st['clone']['path']}`: HEAD `{st['clone']['head'][:12]}`, clean={st['clone']['clean']}; session worktree: HEAD `{st['session_worktree']['head'][:12]}` on `{st['session_worktree']['branch']}`, clean={st['session_worktree']['clean']}. "
            "Neither working tree was used for measuring or building; the clone served only as the source for `git archive`."]
    drift = st.get("origin_main_drift")
    if st["origin_main_still_pinned_sha"]:
        text.append(f"`origin/main` was still the pinned commit `{st['pinned_current_sha']}` when this report was generated.")
    elif drift:
        a = drift["analysis"]
        text += ["", f"**`origin/main` advanced after the pin.** The measured revision stays the pinned `{st['pinned_current_sha']}`, as the protocol requires; at report time `origin/main` was `{st['origin_main_sha_at_report_time']}` "
                 f"({plural(len(drift['commits_after_pin']), 'commit')} after the pin, {plural(a['files_changed'], 'file')} changed, +{a['lines_added']:,}/−{a['lines_deleted']:,} lines). Nothing at the newer head was built or measured.",
                 "", *[f"- `{line}`" for line in drift["commits_after_pin"][:20]],
                 "", f"Files changed after the pin, classified by path and `cfg` gating at the newer head (default features there: `{a['default_features']!r}`; "
                 f"`pub mod server;` behind `#[cfg(feature = \"server\")]`: {a['server_module_feature_gated']}; `src/bin/vlite-bench*` changed: {bool(a['bench_harness_changed'])}):",
                 md_table(["Category", "Files", "+lines", "−lines"], [[cat, v["files"], f"{v['added']:,}", f"{v['deleted']:,}"] for cat, v in sorted(a["by_category"].items(), key=lambda kv: -kv[1]["files"])], ["l", "r", "r", "r"]),
                 "", f"**{a['conclusion']}**"]
        if a["needs_review"]:
            text += ["", "Paths needing review:", *[f"- `{p}`" for p in a["needs_review"][:40]]]
        text.append("If the newer head is wanted as the comparison revision, the campaign must be re-pinned and rebuilt; this report does not do that.")
    else:
        text.append(f"`origin/main` at report time: `{st['origin_main_at_report_time']}` (could not be compared with the pin).")
    return "\n".join(text)


def section_determinism(model) -> str:
    rows = []
    for workload, entry in model["determinism"].items():
        keys = entry["keys"]
        rows.append([f"`{workload}`", f"{entry['runs_used']['hist']}/{entry['runs_used']['cur']}", "yes" if entry["all_stable_within_each_revision"] else "**NO**",
                     f"{sum(1 for v in keys.values() if v['same_across_revisions'])}/{len(keys)}", ", ".join(entry["differing_across_revisions"]) or "none"])
    return "\n".join(["## 8. Determinism checks", "Deterministic outputs (checksums, file sizes, recall, returned counts) over every attempt that produced parseable output (accepted and rejected alike; CPU contamination cannot change them).",
                      md_table(["Workload", "runs hist/cur", "stable within each revision", "keys identical across revisions", "keys that differ across revisions"], rows, ["l", "r", "c", "r", "l"])])


def section_cross_host(model, d) -> str:
    base = d["baseline"]
    mapping = (("Ingest (vectors/s)", "ingest", [("ingest_vectors_per_second", 0)]),
               ("Query p50 (ms)", "p50", [("query_p50_ms", 0), ("hnsw_query_p50_ms", 1)]), ("Query p99 (ms)", "p99", [("query_p99_ms", 0), ("hnsw_query_p99_ms", 1)]),
               ("Reopen (ms)", "reopen", [("reopen_ms", 0)]), ("Peak RSS (MiB)", "rss", [("peak_working_set_mib", 0)]), ("Recall", "recall", [("recall_at_k", 0), ("hnsw_recall_at_k", 1)]))
    rows = []
    for workload in model["scope"]["workloads"]:
        original = base["workloads"].get(workload)
        if not original:
            continue
        comp = {r["metric"]: r for r in model["comparison"][workload]}
        for heading, column, items in mapping:
            for key, index in items:
                row = comp.get(key)
                hist = row["hist"] if row else None
                if hist is None:
                    continue
                numbers = original[column]["numbers"]
                reference = numbers[index] if index < len(numbers) else None
                tag = "" if len(items) == 1 else (" (exact)" if index == 0 else " (HNSW)")
                if column == "recall":
                    ratio_cell = "–" if reference is None else ("equal" if abs(hist["median"] - reference) < 1e-9 else "differs")
                else:
                    ratio_cell = "–" if not reference else f"{hist['median'] / reference:.2f}×"
                rows.append([f"`{workload}`", heading + tag, original[column]["verbatim"], f"{num(hist['median'], row['unit'])} (n={hist['n']})", ratio_cell])
        if workload in base["hnsw_index_build_ms"]:
            row = comp.get("hnsw_index_build_ms")
            if row and row["hist"]:
                rows.append([f"`{workload}`", "HNSW index build (ms)", f"{base['hnsw_index_build_ms'][workload]:,}", f"{num(row['hist']['median'], 'ms')} (n={row['hist']['n']})", f"{row['hist']['median'] / base['hnsw_index_build_ms'][workload]:.2f}×"])
    text = ["## 9. Informational only — this host's historical-revision medians vs the original baseline",
            "The pinned `baseline.md` measured the **same commit `81ff901`** on the reference host (5 runs, medians, no CPU-policy check). Its own text says results are comparable only with reruns on that machine and volume, "
            "so this table is context for the host difference, **not** a REG-010 comparison and not a replacement for the original numbers.",
            md_table(["Workload", "Metric", "Original host (verbatim cell)", "This host, hist, median", "This / original"], rows, ["l", "l", "l", "r", "r"]) if rows else "No accepted historical-revision runs to compare."]
    return "\n".join(text)


def section_limits(model, d) -> str:
    explore = model["window"]["exploratory"]
    floor = ""
    if explore:
        longest = max(explore.values(), key=lambda s: s["seconds"])
        floor = (f"A {longest['seconds']:.0f} s exploratory survey of the otherwise unused host had a mean of {pct(longest['mean'])} and a worst sample of {pct(longest['worst_sample'])}; "
                 f"only {longest['five_second_probes_quiet']} of {longest['five_second_probes']} five-second probes were individually quiet; top contributors "
                 + ", ".join(f"{img} {pct(share)}" for img, share in longest["top_contributors"][:4]) + ". ")
    smoke = [e["background_mean"] for e in model["ledger"] if e["workload"] in ("exact_small", "filtered_exact", "hnsw_small") and e["background_mean"] is not None]
    smoke_note = (f" Across this campaign's {len(smoke)} smoke attempts the monitor's background mean ranged from {pct(min(smoke))} to {pct(max(smoke))}." if smoke else "")
    drift_state = model["git_state"].get("origin_main_drift")
    moving = (f"An earlier fetch (`{d['pins']['superseded_earlier_fetch']['sha'][:12]}`) was already superseded before anything was built; at report time the remote head was `{model['git_state']['origin_main_sha_at_report_time'][:12]}`, "
              f"{plural(len(drift_state['commits_after_pin']), 'commit')} after the pinned revision (section 7)." if drift_state
              else f"An earlier fetch (`{d['pins']['superseded_earlier_fetch']['sha'][:12]}`) was already superseded before anything was built; at report time the remote head was still the pinned commit.")
    rule = model["stall_rule"]
    longest_ok = rule["longest_sample_in_accepted_runs"]
    stall_text = (f"The guest did not run for about 74 minutes once during the campaign (section 4); that attempt was rejected, and this report quarantines any run the runner accepted if it contains a monitor sample over {STALL_SAMPLE_SECONDS:g} s "
                  f"({len(rule['quarantined'])} quarantined; longest sample in a remaining accepted run {'–' if longest_ok is None else format(longest_ok, '.2f') + ' s'}). The mechanism is not recorded inside the guest, so a platform-level pause is an inference, "
                  "and stalls shorter than the check cannot be ruled out beyond what the monitor's sample lengths show.")
    hnsw_slots = [model["slots"][key] for key in (f"hnsw_100k_384/{label}" for label in LABELS) if key in model["slots"]]
    hnsw_causes = model["rejection_causes"].get("hnsw_100k_384")
    long_runs = ""
    if hnsw_slots and hnsw_causes:
        measured = sum(s["accepted"] + s["rejected"] + s["quarantined"] for s in hnsw_slots)
        stalled = sum(s["quarantined"] for s in hnsw_slots)
        long_runs = (f"The long `hnsw_100k_384` runs (about 13 minutes each) were accepted in {sum(s['accepted'] for s in hnsw_slots)} of {measured} measured attempts"
                     + (f" ({stalled} more passed the runner's check but were quarantined)" if stalled else "") + "; "
                     f"of the {sum(hnsw_causes.values())} rejected ones, {hnsw_causes['peak_only']} failed only the 50 % per-sample limit, {hnsw_causes['mean_only']} only the 10 % mean limit and {hnsw_causes['both']} both"
                     + (f" ({hnsw_causes['other']} for another reason)" if hnsw_causes["other"] else "") + " (section 3, appendix D). ")
    activity = model.get("agent_activity")
    if activity:
        pops, bursts = activity["comparison"]["populations"]["all"], activity["bursts"]
        measured = (f"In the long runs, background CPU averaged {pct(pops['process']['mean_background'])} in samples within ±{activity['tolerance_seconds']:g} s of a process-starting call and "
                    f"{pct(pops['none']['mean_background'])} in samples near no call or model turn, and {bursts['observed']['process']} of the {bursts['analysed']} bursts over the 50 % limit outside the stalled attempt coincided with such a call "
                    f"({bursts['expected']['process']:.1f} expected if unrelated); appendix D lists every burst. That timing evidence neither excludes nor establishes a contribution to any individual rejection or to bursts inside accepted runs. ")
    else:
        measured = "No timestamped record of the agent's calls exists, so its contribution to rejected runs and to bursts inside accepted ones cannot be assessed. "
    items = [
        f"**Host noise floor.** {floor}The pinned monitor documents the reference host as about 5 % busy with bursts near 45 %; the policy is calibrated to that floor. {long_runs}"
        "Nothing was relaxed; slots that could not be measured are reported as missing.",
        "**Host stall.** " + stall_text,
        "**Different machine.** The CPU (AMD EPYC 9V74 vs Intel Xeon Platinum 8370C) and the exact RAM size differ from the reference host; only the logical CPU count, OS build and storage type match.",
        "**Short runs and the monitor.** The smoke workloads finish in tens of milliseconds, far below the monitor's 0.5 s sampling interval and its 0.1 s floor for the peak rule, so for them the peak check is vacuous and acceptance rests on a mean over a handful of scheduler ticks (system times advance in clock-tick quanta of roughly 15.6 ms)."
        + smoke_note + " Their CPU check is therefore weak, and retrying until a run passes is a selection effect; appendix B shows how far accepted-only statistics differ from all-attempt statistics.",
        "**Shared host and agent activity.** The agent harness that ran the campaign and the user's desktop session run on the same machine. Their CPU counts as background, as the policy requires, and the agent session was **not** idle during measurement: "
        "it polled the campaign log and analysed results while runs were in progress (section 4, \"Agent activity during the measurement window\"). " + measured +
        "Exclusivity of a shared host can be checked, not proven. In both exploratory surveys a sensor (`SenseCE.exe`) used about 4.4 % of all CPUs on its own; it, the VPN/zero-trust client, management agents and Defender could not be paused (not administrator, and they belong to the organisation).",
        "**n ≤ 5.** Medians and ranges only; no confidence intervals or significance claims. Order within each pair alternates to balance warm-cache effects but cannot remove them.",
        "**Storage.** Both revisions used the same Q: ReFS Dev Drive directory; the physical backing of the virtual disk is unknown (as on the reference host) and Dev Drive/Defender-exclusion settings could not be read without administrator rights.",
        "**Scope.** Default-feature (graph off) release builds of `vlite-bench` only. Graph-on builds, `vlite-graph-cli` and the REG-013 build-boundary check were not part of this campaign.",
        "**Archive line endings.** Both archives were LF-normalised identically and verified against their git blobs; the repository's own helper would produce CRLF on this host. Whether line endings could change the compiled binaries was not tested, and a byte-for-byte reproduction of the original host's build inputs is not claimed.",
        "**Moving `origin/main`.** " + moving + " The pinned commit is the only revision measured.",
    ]
    return "\n".join(["## 10. Limitations", *[f"- {item}" for item in items]])


def appendix_ledger(model) -> str:
    def peak(e):
        return "n/a" if e["wall_seconds"] is not None and e["wall_seconds"] < 0.1 else pct(e["background_max_sample"])

    rows = [[i + 1, hhmmss(e["started_utc"]), e["repetition"], f"`{e['workload']}`", e["label"], e["attempt"] or "–", e["outcome"],
             "–" if e["wall_seconds"] is None else f"{e['wall_seconds']:.3f}", pct(e["background_mean"]), peak(e), e["note"]]
            for i, e in enumerate(model["ledger"])]
    return "\n".join(["## Appendix A — attempt ledger (chronological, every attempt)",
                      md_table(["#", "UTC", "rep", "workload", "rev", "try", "outcome", "wall s", "bg mean", "bg max sample", "note"], rows, ["r", "l", "r", "l", "l", "r", "l", "r", "r", "r", "l"]),
                      "\"bg max sample\" is n/a for runs under 0.1 s: the monitor does not count a sample that short for its peak rule."])


def appendix_sensitivity(model) -> str:
    rows = []
    for key, s in model["sensitivity_informational"].items():
        acc, rej, allr = s["accepted"], s["rejected_excluded"], s["all_completed_attempts"]
        drift = "–" if not (acc and allr and acc["median"]) else f"{(allr['median'] - acc['median']) / acc['median'] * 100:+.1f}%"
        rows.append([f"`{key}`", "–" if not acc else f"{num(acc['median'], 'v/s')} (n={acc['n']})", "–" if not rej else f"{num(rej['median'], 'v/s')} (n={rej['n']})",
                     "–" if not allr else f"{num(allr['median'], 'v/s')} (n={allr['n']})", drift])
    return "\n".join(["## Appendix B — selection effect of rejecting runs (informational; rejected runs are excluded from every statistic above)",
                      "Median ingest (vectors/s) of accepted runs, of rejected runs that completed, and of all completed attempts.",
                      md_table(["Workload/revision", "accepted", "rejected (excluded)", "all completed attempts", "all vs accepted"], rows, ["l", "r", "r", "r", "r"])])


def appendix_values(model) -> str:
    parts = ["## Appendix C — every accepted run (the values behind the medians)"]
    for workload in model["scope"]["workloads"]:
        runs = [v for v in model["accepted_run_values"] if v["workload"] == workload]
        parts += ["", f"### `{workload}`"]
        if not runs:
            parts.append("No accepted run.")
            continue
        metrics = [m for m in METRICS if not (m.hnsw_only and workload not in HNSW_WORKLOADS) and m.key != "child_wall_seconds"]
        headers = ["rep", "rev", "try"] + [f"{m.heading} ({m.unit})" if m.unit else m.heading for m in metrics] + ["bg mean"]
        rows = [[v["repetition"], v["label"], v["attempt"]] + [num(v.get(m.key), m.unit) for m in metrics] + [pct(v["background_mean"])] for v in runs]
        parts.append(md_table(headers, rows, ["r", "l", "r"] + ["r"] * (len(metrics) + 1)))
    return "\n".join(parts)


def appendix_bursts(model) -> str:
    clusters = sorted(model["burst_clusters"], key=lambda c: c["start_utc"])
    head = "## Appendix D — bursts above the 50 % per-sample limit"
    if not clusters:
        return "\n".join([head, "No monitor sample of at least 0.1 s exceeded the limit in any attempt."])
    with_agent = model["agent_activity"] is not None

    def nearest(call):
        if not call:
            return "none recorded"
        if call["relation"] == "during":
            return f"`{call['tool']}` during the burst"
        return f"`{call['tool']}` {call['seconds']:.1f} s {call['relation']}" if call["seconds"] <= 60 else "none within 60 s"

    rows, previous = [], None
    for c in clusters:
        start, end = datetime.fromisoformat(c["start_utc"]), datetime.fromisoformat(c["end_utc"])
        gap = "–" if previous is None else f"{(start - previous).total_seconds() / 60:.1f}"
        previous = end if previous is None else max(previous, end)
        row = [c["start_utc"][5:19].replace("T", " "), f"{c['length_seconds']:.1f}", c["repetition"], f"`{c['workload']}`", c["label"], c["attempt"],
               c["outcome"] + (" (stalled)" if c["stalled_attempt"] else ""), c["samples_over_limit"], pct(c["peak_background_fraction"]), f"{c['longest_sample_seconds']:.2f}", gap]
        if with_agent:
            agent = c["agent"]
            row += ["stalled attempt, not analysed"] * 2 if agent["stalled_attempt"] else [nearest(agent["nearest_process_call"]), nearest(agent["nearest_in_process_call"])]
        rows.append(row)
    samples = model["burst_samples_above_limit"]
    attempts = {(b["repetition"], b["workload"], b["label"], b["attempt"]) for b in samples}
    headers = ["UTC start (MM-DD HH:MM:SS)", "length s", "rep", "workload", "rev", "try", "outcome", "samples over", "peak", "longest sample s", "gap since previous burst (min)"]
    aligns = ["l", "r", "r", "l", "l", "r", "l", "r", "r", "r", "r"]
    if with_agent:
        headers += ["nearest process-starting call", "nearest in-process call"]
        aligns += ["l", "l"]
    return "\n".join([head,
                      f"{len(samples)} samples in {len(attempts)} attempts form {len(clusters)} bursts: consecutive over-limit samples of one attempt less than {CLUSTER_GAP_SECONDS:g} s apart (any outcome; each of those attempts fails the unchanged "
                      "50 % per-sample limit). \"UTC start\" is the approximate start of the burst (attempt start + pre-run wait + cumulative sample time, about ±0.2 s); \"length\" runs from the start of its first to the end of its last "
                      "over-limit sample; \"samples over\" counts the samples above the limit; \"gap\" is the time since the previous burst ended."
                      + (" The last two columns name the call of that kind closest in time to the burst (\"during\": a moment the call was active lies inside it)." if with_agent else "")
                      + " Every individual sample is in `comparison-report.json` (`burst_samples_above_limit`) and in the monitor samples of `raw/<tag>/runs.jsonl`. The table is evidence for the reader; no cause is asserted.",
                      md_table(headers, rows, aligns)])


def section_artifacts(model, d) -> str:
    others = model["other_raw_tags"]
    lines = ["## Artifacts",
             f"- Raw outputs: `raw/{model['tag']}/runs.jsonl` holds one JSON record per attempt — accepted, rejected, refused or not attempted — with the child's complete stdout and stderr, the monitor's per-sample CPU, "
             "the pre-run probes and process censuses; `campaign.log`, `campaign-config.json`, the start/end process censuses and the start/end source re-verification sit beside it.",
             "- Evidence: `evidence/` (pins, archives, toolchain, builds, host, reference-identity checks, runner and pinned-script hashes, self-test, window surveys, the host stall incident record, build logs).",
             "- Integrity: `ARTIFACTS.sha256` lists the SHA-256 of every evidence, raw, runner and report file; its roll-up is in `ARTIFACTS.rollup.txt`. The roll-up cannot be printed inside this report, which is one of the hashed files.",
             "- Machine-readable twin of this report: `comparison-report.json`."]
    if others:
        lines.append("- Other data under `raw/` (runner self-tests and dry runs, kept for transparency and excluded from every statistic here): "
                     + ", ".join(f"`{k}` ({plural(v, 'record')})" for k, v in others.items()) + "."
                     + (" The dry runs' window surveys are in `evidence/window/dryrun/`." if (d["camp"] / "evidence" / "window" / "dryrun").exists() else ""))
    return "\n".join(lines)


def utc_span(first, last) -> str:
    """'2026-10-25 18:30:03 – 2026-10-26 00:59:01 UTC'; the date is given once when both ends fall on the same day."""
    if not first or not last:
        return "–"
    a, b = (datetime.fromisoformat(v).astimezone(timezone.utc).strftime("%Y-%m-%d %H:%M:%S") for v in (first, last))
    return f"{a} – {b[11:] if a[:10] == b[:10] else b} UTC"


def render(model, d) -> str:
    parts = ["# DO-005 graph-off benchmark — separate-host comparison report", f"Campaign `{model['campaign_directory']}` (tag `{model['tag']}`), generated {model['generated_utc']}; "
             f"measurements {utc_span(model['time']['first_attempt_utc'], model['time']['last_attempt_finished_utc'])}.",
             banner(model, d), section_summary(model, d), section_results(model), section_cpu(model), section_window(model, d), section_host(d), section_sources(model, d),
             section_protocol(model), section_determinism(model), section_cross_host(model, d), section_limits(model, d), section_artifacts(model, d),
             appendix_ledger(model), appendix_sensitivity(model), appendix_values(model), appendix_bursts(model)]
    return "\n\n".join(parts) + "\n"


# ---------------------------------------------------------------------------------------------- output
def artifact_index(camp: Path, out_dir: Path) -> str:
    files = [p for root in (camp / "evidence", camp / "raw", camp / "runner", out_dir) for p in root.rglob("*")
             if p.is_file() and "__pycache__" not in p.parts and p.name not in ("ARTIFACTS.sha256", "ARTIFACTS.rollup.txt")]
    lines = [f"{sha256_file(p)}  {p.relative_to(camp).as_posix()}" for p in sorted(files)]
    data = ("\n".join(lines) + "\n").encode("utf-8")
    (out_dir / "ARTIFACTS.sha256").write_bytes(data)
    rollup = sha256_bytes(data)
    (out_dir / "ARTIFACTS.rollup.txt").write_bytes(f"{rollup}  ARTIFACTS.sha256 ({len(lines)} files)\n".encode("utf-8"))
    return rollup


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--campaign")
    parser.add_argument("--tag", default="main")
    args = parser.parse_args()
    camp = campaign_dir(args.campaign)
    out_dir = camp / ("report" if args.tag == "main" else f"report-{args.tag}")
    out_dir.mkdir(exist_ok=True)
    data = load_inputs(camp, args.tag)
    model = analyse(data)
    (out_dir / "comparison-report.md").write_text(render(model, data), encoding="utf-8")
    write_json(out_dir / "comparison-report.json", model)
    rollup = artifact_index(camp, out_dir)
    counts = model["counts"]
    print(f"report {out_dir}: verdict {model['verdict']}; slots {model['scope']['accepted_slots']}/{model['scope']['expected_slots']}; "
          f"accepted {counts['accepted']} rejected {counts['rejected']} refused {counts['refused']} quarantined {counts['quarantined']} not_attempted {counts['not_attempted']}; artifact rollup {rollup}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
