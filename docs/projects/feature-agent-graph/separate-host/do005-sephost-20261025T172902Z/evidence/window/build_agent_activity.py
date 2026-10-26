"""Builds evidence/window/agent-activity.json from the event log of the agent session that operated the campaign.

Only tool names, timestamps, a process/in-process kind, a span type and a short note are copied. Prompts, commands, file contents and tool results that the
event log contains are not read into the output. The event log is append-only and still growing, so the output records how many bytes were used and the sha256
of exactly that prefix; the hash of the whole file at a later time will differ.

Usage: python build_agent_activity.py <events.jsonl> <output.json> <window-from-utc> <window-to-utc>
The window bounds use the event log's own format, e.g. 2026-10-25T18:22:00.000Z.
"""
import hashlib
import json
import re
import sys
from collections import Counter
from datetime import datetime, timezone
from pathlib import Path

PROCESS_TOOLS = {"powershell", "grep", "glob", "stop_powershell"}


def parse(stamp: str) -> datetime:
    return datetime.strptime(stamp, "%Y-%m-%dT%H:%M:%S.%fZ").replace(tzinfo=timezone.utc)


def iso(moment: datetime) -> str:
    return moment.isoformat(timespec="milliseconds")


def short(text, limit=100) -> str:
    text = " ".join(str(text or "").split())
    return text if len(text) <= limit else text[: limit - 1] + "…"


def note_for(name: str, args: dict, mode: str | None) -> str:
    if name == "powershell":
        return short(f"{args.get('description', '')} [{mode or 'sync'}]")
    if name in ("read_powershell", "stop_powershell"):
        return short(f"shellId={args.get('shellId')} delay={args.get('delay')}" if name == "read_powershell" else f"shellId={args.get('shellId')}")
    if name in ("view", "edit", "create"):
        return short(Path(str(args.get("path", ""))).name)
    if name in ("sql", "session_store_sql", "task"):
        return short(args.get("description", ""))
    return ""


def main() -> int:
    source, target, lo, hi = Path(sys.argv[1]), Path(sys.argv[2]), parse(sys.argv[3]), parse(sys.argv[4])
    raw = source.read_bytes()
    used = raw[: raw.rfind(b"\n") + 1]
    events = [json.loads(line) for line in used.decode("utf-8").split("\n") if line.strip()]

    starts, completes, notices, external_requests, external_done = {}, {}, [], {}, {}
    turns, open_turn = [], {}
    for ev in events:
        kind, data, stamp = ev["type"], ev.get("data") or {}, ev["timestamp"]
        if kind == "tool.execution_start":
            starts[data["toolCallId"]] = ev
        elif kind == "tool.execution_complete":
            completes[data["toolCallId"]] = ev
        elif kind == "external_tool.requested":
            external_requests[data["requestId"]] = ev
        elif kind == "external_tool.completed":
            external_done[data["requestId"]] = ev
        elif kind == "system.notification":
            notices.append(ev)
        elif kind == "assistant.turn_start":
            open_turn[(data.get("interactionId"), data.get("turnId"))] = {"start": stamp, "end": None}
            turns.append(open_turn[(data.get("interactionId"), data.get("turnId"))])
        elif kind == "assistant.message":
            current = open_turn.get((data.get("interactionId"), data.get("turnId")))
            if current is not None:
                current["end"] = stamp

    exits = {}
    for ev in notices:
        match = re.search(r"\(shellId: ([^)]+)\) has completed", (ev.get("data") or {}).get("content", ""))
        if match:
            exits[match.group(1)] = ev["timestamp"]

    calls, unknown_completion = [], 0
    for call_id, ev in starts.items():
        data = ev["data"]
        name, args = data["toolName"], data.get("arguments") or {}
        begin = parse(ev["timestamp"])
        done = completes.get(call_id)
        finish = parse(done["timestamp"]) if done else begin
        mode = args.get("mode") if name == "powershell" else None
        entry = {"tool": name, "kind": "process" if name in PROCESS_TOOLS else "in_process", "start_utc": iso(begin), "end_utc": iso(finish), "note": note_for(name, args, mode),
                 "_open": done is None}
        span = "instants"
        if name in ("powershell", "grep", "glob"):
            sleeps = name == "powershell" and "start-sleep" in str(args.get("command", "")).lower()
            if (name != "powershell" or mode in (None, "sync")) and not sleeps:
                span = "interval"
        entry["span"] = span
        if name == "powershell" and mode == "async" and args.get("shellId") in exits:
            entry["exit_utc"] = iso(parse(exits[args["shellId"]]))
        calls.append(entry)
    for request_id, ev in external_requests.items():
        begin = parse(ev["timestamp"])
        done = external_done.get(request_id)
        calls.append({"tool": ev["data"]["toolName"], "kind": "in_process", "start_utc": iso(begin), "end_utc": iso(parse(done["timestamp"]) if done else begin), "note": "app tool", "span": "instants"})

    calls = sorted((c for c in calls if lo <= datetime.fromisoformat(c["end_utc"]) and datetime.fromisoformat(c["start_utc"]) <= hi), key=lambda c: c["start_utc"])
    unknown_completion = sum(1 for c in calls if c.pop("_open", False))
    model_turns = [{"start_utc": iso(parse(t["start"])), "end_utc": iso(parse(t["end"]))} for t in turns
                   if t["end"] and lo <= parse(t["end"]) and parse(t["start"]) <= hi]
    model_turns.sort(key=lambda t: t["start_utc"])

    result = {
        "schema": "agent-activity/2",
        "recorded_utc": iso(datetime.now(timezone.utc)),
        "source": "the agent session's own event log (tool.execution_start/complete, external_tool.requested/completed, system.notification, assistant.turn_start and assistant.message events); "
                  "only tool names, timestamps and a short note were copied",
        "source_file": str(source),
        "source_bytes_used": len(used),
        "source_prefix_sha256": hashlib.sha256(used).hexdigest(),
        "builder": {"path": str(Path(__file__)), "sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest()},
        "window": {"from_utc": iso(lo), "to_utc": iso(hi)},
        "semantics": {
            "kind": "process = the tool starts an operating-system process (PowerShell, ripgrep); in_process = it runs inside the CLI process (file views and edits, SQL, waiting on a shell)",
            "span": "interval = the process runs between start_utc and end_utc (synchronous commands); instants = only the start and end are treated as activity (calls that wait, and asynchronous launches)",
            "exit_utc": "exit of an asynchronous shell, only where the runtime delivered a completion notification",
            "model_turns": "from the start of a model request to the end of its last streamed message, per turn; includes network latency",
        },
        "counts": {"calls": len(calls), "by_tool": dict(Counter(c["tool"] for c in calls).most_common()), "by_kind": dict(Counter(c["kind"] for c in calls)),
                   "by_span": dict(Counter(c["span"] for c in calls)), "calls_without_completion_event": unknown_completion, "model_turns": len(model_turns)},
        "limitations": [
            "Only tool-call boundaries and model-response intervals are recorded; the CPU used by the agent runtime, the desktop app (rendering, streaming) and the model client is not attributed to calls.",
            "The process/in-process split is assumed from the tool name: powershell, grep, glob and stop_powershell are taken to start operating-system processes, all other tools to run inside the CLI process.",
            "Asynchronous shells have an exit instant only where a completion notification exists.",
            "Timestamps come from the CLI's event log on the same host clock as the monitor.",
            "Activity of the user, other applications and other app sessions is not recorded here.",
        ],
        "calls": calls,
        "model_turns": model_turns,
    }
    target.write_text(json.dumps(result, indent=1) + "\n", encoding="utf-8")
    print("calls", len(calls), "model_turns", len(model_turns), "bytes_used", len(used), "calls_without_completion_event", unknown_completion)
    print("by_tool", result["counts"]["by_tool"])
    print("by_kind", result["counts"]["by_kind"], "by_span", result["counts"]["by_span"])
    print("exits", {k: v for k, v in exits.items()})
    print("first", calls[0]["start_utc"], "last", calls[-1]["end_utc"])
    return 0


if __name__ == "__main__":
    sys.exit(main())
