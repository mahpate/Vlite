"""Independent reference bytes for precision publication Decisions W/P.

Only op17 PLAINTEXT BODY and raw sections of critical kinds 3/4/5 are checked.
No headers, compact envelopes, frame/checksum/encryption/image-terminator logic,
qualified WAL recovery, public APIs, or transport are modeled. Storage schema
versions and frame counts are explicit inputs, not parsed framing. Graph cases
contain a complete partition run with explicit retained-entity and floor context.

Op17 uses the unchanged op5 name/dimensions layout in src/format.rs. Settings
are deliberately constrained to its exact 27-byte default settings-v1 fixture;
general semantic settings validation remains the engine's responsibility.
Accepted hex and logical expectations are hand-authored in the JSON, never
generated from this checker or a Rust encoder. Re-encoding is an extra check.
Error labels are fixture-local: unsupported means UnsupportedFeature(32) for
op17 schema/precision, or an unsupported critical storage version; malformed
means corrupt structure/context. Neither label qualifies production error APIs.
"""

import argparse
import copy
import json
import math
from pathlib import Path
import re
import struct
import subprocess
import sys
import tempfile


FIXTURE = Path(__file__).resolve().parent / "fixtures" / "compact" / "precision-catalog-cases.json"
SCHEMA = "vlite.precision-catalog-golden"
SCOPE = ("Reference bytes only: op17 plaintext body and kind3/4/5 raw sections; "
         "no framing, checksums, encryption, image terminators, WAL recovery, "
         "public APIs or transport.")
SETTINGS_KEYS = ("version metric m ef_construction seed quantization rerank_multiplier "
                 "exact_max_candidates exact_max_selectivity_ppm metadata_indexes text_fields")
DEFAULT_SETTINGS = {
    "version": 1, "metric": 0, "m": 16, "ef_construction": 100, "seed": 42,
    "quantization": 0, "rerank_multiplier": 4, "exact_max_candidates": 4096,
    "exact_max_selectivity_ppm": 20000, "metadata_indexes": [], "text_fields": [],
}
U64_MAX = (1 << 64) - 1


class Rejected(ValueError):
    def __init__(self, category, reason):
        super().__init__(f"{category}: {reason}")
        self.category = category


def require(condition, message):
    if not condition:
        raise ValueError(message)


def check(condition, reason, category="malformed"):
    if not condition:
        raise Rejected(category, reason)


def keys(value, names, location):
    require(isinstance(value, dict) and set(value) == set(names.split()),
            f"{location}: expected only {names}")


def integer(value, maximum, location, minimum=0):
    require(type(value) is int and minimum <= value <= maximum,
            f"{location}: expected integer in {minimum}..{maximum}")
    return value


def array(value, maximum, location):
    require(isinstance(value, list) and len(value) <= maximum,
            f"{location}: expected array of at most {maximum} entries")
    return value


def hex_bytes(value, location):
    require(isinstance(value, str) and
            re.fullmatch(r"(?:[0-9a-f]{2}|[ \t\r\n])*", value) is not None,
            f"{location}: expected lowercase byte hex with optional ASCII whitespace")
    return bytes.fromhex(value)


def unique_object(pairs):
    result = {}
    for key, value in pairs:
        require(key not in result, f"duplicate JSON key {key!r}")
        result[key] = value
    return result


def reject_constant(value):
    raise ValueError(f"invalid JSON constant {value!r}")


def finite_float(value):
    number = float(value)
    require(math.isfinite(number), f"non-finite JSON number {value!r}")
    return number


def load_fixture(path):
    return json.loads(path.read_text(encoding="utf-8"), object_pairs_hook=unique_object,
                      parse_constant=reject_constant, parse_float=finite_float)


class Reader:
    def __init__(self, data):
        self.data = data
        self.offset = 0

    def take(self, length):
        check(length <= len(self.data) - self.offset, f"truncated at byte {self.offset}")
        start = self.offset
        self.offset += length
        return self.data[start:self.offset]

    def unpack(self, fmt):
        return struct.unpack("<" + fmt, self.take(struct.calcsize("<" + fmt)))

    def end(self):
        check(self.offset == len(self.data), "trailing section/body bytes")


def decode_name(reader, length, allow_default=False):
    check(1 <= length <= 63, "name length outside 1..63")
    raw = reader.take(length)
    check(re.fullmatch(rb"[a-z][a-z0-9_-]{0,62}", raw) is not None, "invalid collection name")
    name = raw.decode("ascii")
    check(allow_default or name != "default", "named create cannot use default")
    return name


def validate_binding_list(value, location, with_dimensions=False, maximum=64, unique_names=True):
    names = "id name dimensions" if with_dimensions else "id name"
    seen = set()
    seen_names = set()
    for item in array(value, maximum, location):
        keys(item, names, location)
        integer(item["id"], 65535, location + ".id")
        name = item["name"]
        require(isinstance(name, str) and re.fullmatch(r"[a-z][a-z0-9_-]{0,62}", name),
                location + ": invalid name")
        require(item["id"] not in seen and (not unique_names or name not in seen_names),
                location + ": duplicate binding")
        require((item["id"] == 0) == (name == "default"), location + ": invalid default binding")
        seen.add(item["id"])
        seen_names.add(name)
        if with_dimensions:
            integer(item["dimensions"], 65536, location + ".dimensions", 1)
    return value


def validate_context(kind, context):
    if kind == "op17":
        keys(context, "collection_id", "op17 context")
        integer(context["collection_id"], 65535, "collection_id", 1)
    elif kind == "catalog":
        keys(context, "live_collections", "catalog context")
        validate_binding_list(context["live_collections"], "live_collections", True)
        require(any(item["id"] == 0 for item in context["live_collections"]),
                "live_collections: default required")
    elif kind == "imports":
        keys(context, "live_collections dropped_bindings", "imports context")
        validate_binding_list(context["live_collections"], "live_collections")
        validate_binding_list(context["dropped_bindings"], "dropped_bindings",
                              maximum=1088, unique_names=False)
        live_ids = {item["id"] for item in context["live_collections"]}
        require(all(item["id"] != 0 and item["id"] not in live_ids
                    for item in context["dropped_bindings"]), "dropped_bindings: live/default ID")
    else:
        keys(context, "floor retained_nodes retained_edges", "graph context")
        integer(context["floor"], U64_MAX, "floor", 1)
        for name in ("retained_nodes", "retained_edges"):
            values = array(context[name], 1 << 64, name)
            for value in values:
                integer(value, U64_MAX, name)
            require(values == sorted(set(values)), name + ": expected unique ascending IDs")


def validate_input(kind, value):
    if kind == "op17":
        keys(value, "body_hex", "op17 input")
        hex_bytes(value["body_hex"], "body_hex")
    elif kind in ("catalog", "imports"):
        keys(value, "schema_version frame_count section_hex", kind + " input")
        integer(value["schema_version"], 65535, "schema_version")
        integer(value["frame_count"], (1 << 32) - 1, "frame_count")
        hex_bytes(value["section_hex"], "section_hex")
    else:
        keys(value, "schema_version partitions", "graph input")
        integer(value["schema_version"], 65535, "schema_version")
        partitions = array(value["partitions"], 1 << 32, "partitions")
        require(partitions, "partitions: expected at least one")
        for part in partitions:
            keys(part, "partition frame_count nodes_hex edges_hex", "partition input")
            integer(part["partition"], (1 << 32) - 1, "partition")
            integer(part["frame_count"], (1 << 32) - 1, "frame_count")
            hex_bytes(part["nodes_hex"], "nodes_hex")
            hex_bytes(part["edges_hex"], "edges_hex")


def decode_op17(value, context):
    data = hex_bytes(value["body_hex"], "body_hex")
    check(len(data) <= 16460, "op17 body exceeds 16460 bytes")
    reader = Reader(data)
    schema, precision = reader.unpack("HB")
    check(schema == 1 and precision in (1, 2), "op17 schema/precision", "unsupported")
    check(reader.take(5) == bytes(5), "op17 reserved bytes")
    length, = reader.unpack("B")
    name = decode_name(reader, length)
    dimensions, = reader.unpack("I")
    check(1 <= dimensions <= 65536, "dimensions outside 1..65536")
    remaining = len(data) - reader.offset
    check(remaining <= 16384, "settings exceed 16384 bytes")
    settings = Reader(reader.take(remaining))
    # The fixed fields and two zero counts are the real settings-v1 layout.
    fields = settings.unpack("HBHHQBBIIBB")
    settings.end()
    decoded = dict(zip(SETTINGS_KEYS.split()[:9], fields[:9]))
    check(fields[9:] == (0, 0), "only zero-index/zero-text settings fixture supported")
    decoded.update(metadata_indexes=[], text_fields=[])
    check(decoded == DEFAULT_SETTINGS, "only default settings-v1 fixture supported")
    reader.end()
    return {"schema": schema, "precision": precision, "name": name,
            "dimensions": dimensions, "settings_length": remaining, "settings": decoded}


def decode_catalog(value, context):
    check(value["schema_version"] == 1, "critical catalog version", "unsupported")
    count = value["frame_count"]
    check(1 <= count <= 64, "catalog count outside 1..64")
    data = hex_bytes(value["section_hex"], "section_hex")
    check(len(data) == 8 + 8 * count, "catalog length/count mismatch")
    reader = Reader(data)
    opt_in, reserved = reader.unpack("II")
    check(opt_in == 1 and reserved == 0, "catalog opt-in/reserved")
    entries = []
    previous = -1
    for _ in range(count):
        ident, precision, reserved, dimensions = reader.unpack("HBBI")
        check(precision in (1, 2), "catalog precision")
        check(reserved == 0, "catalog entry reserved")
        check(ident > previous, "duplicate/unsorted catalog IDs")
        previous = ident
        entries.append({"id": ident, "precision": precision, "dimensions": dimensions})
    expected = {item["id"]: item["dimensions"] for item in context["live_collections"]}
    check({item["id"]: item["dimensions"] for item in entries} == expected,
          "catalog live collection coverage/dimensions mismatch")
    reader.end()
    return {"opt_in": opt_in, "entries": entries}


def decode_imports(value, context):
    check(value["schema_version"] == 1, "critical import version", "unsupported")
    check(value["frame_count"] <= 1088, "import frame count exceeds 1088")
    data = hex_bytes(value["section_hex"], "section_hex")
    check(len(data) <= 155088, "import section exceeds 155088 bytes")
    reader = Reader(data)
    max_job, count, closed_count = reader.unpack("QII")
    check(count <= 1088 and closed_count <= 1024 and closed_count <= count,
          "import count limits")
    check(count == value["frame_count"], "import frame/job count mismatch")
    check(16 + 73 * count + 8 * closed_count <= len(data) <=
          16 + 135 * count + 8 * closed_count, "import length/count mismatch")
    live = {(item["id"], item["name"]) for item in context["live_collections"]}
    dropped_bindings = {(item["id"], item["name"]) for item in context["dropped_bindings"]}
    jobs = []
    previous = 0
    closed = set()
    open_count = 0
    for _ in range(count):
        job, collection, state, dropped, length = reader.unpack("QHBBB")
        check(reader.take(3) == bytes(3), "import entry reserved")
        last_chunk, committed, skipped = reader.unpack("QQQ")
        digest = reader.take(32).hex()
        name = decode_name(reader, length, True)
        check(previous < job <= max_job, "duplicate/unsorted job or max-job mismatch")
        previous = job
        check(state in (0, 1, 2) and dropped in (0, 1), "import state/dropped enum")
        binding = (collection, name)
        check(binding in (dropped_bindings if dropped else live), "import drop/binding mismatch")
        check(not dropped or state != 0, "dropped import cannot be open")
        if state:
            closed.add(job)
        else:
            open_count += 1
        jobs.append({"job": job, "collection": collection, "state": state, "dropped": dropped,
                     "name": name, "last_chunk": last_chunk, "committed": committed,
                     "skipped": skipped, "digest_hex": digest})
    check(open_count <= 64, "more than 64 open imports")
    order = [reader.unpack("Q")[0] for _ in range(closed_count)]
    check(len(set(order)) == len(order) and set(order) == closed,
          "closed-order membership mismatch")
    reader.end()
    return {"max_job": max_job, "jobs": jobs, "closed_order": order}


def decode_graph(value, context):
    check(value["schema_version"] == 1, "critical graph version", "unsupported")
    result = []
    ids = {"nodes": [], "edges": []}
    for index, part in enumerate(value["partitions"]):
        check(part["partition"] == index, "graph partition run must start at zero without gaps")
        check(part["frame_count"] <= 4096, "graph frame count exceeds 4096")
        data = {name: hex_bytes(part[name + "_hex"], name) for name in ids}
        check(sum(map(len, data.values())) <= 65552, "graph partition exceeds 65552 bytes")
        counts = {}
        readers = {}
        for name in ids:
            reader = Reader(data[name])
            count, reserved = reader.unpack("II")
            check(reserved == 0, "graph section reserved")
            check(count <= 4096, "graph section count exceeds 4096")
            check(len(data[name]) == 8 + 16 * count, "graph section length/count mismatch")
            readers[name] = reader
            counts[name] = count
        check(sum(counts.values()) <= 4096 and sum(counts.values()) == part["frame_count"],
              "graph combined/frame count mismatch")
        check(sum(counts.values()) != 0 or len(value["partitions"]) == 1,
              "empty graph requires one zero-count block")
        decoded = {"partition": index, "nodes": [], "edges": []}
        for name in ids:
            for _ in range(counts[name]):
                ident, revision = readers[name].unpack("QQ")
                check(not ids[name] or ident > ids[name][-1],
                      "duplicate/unsorted graph IDs across partitions")
                check(1 <= revision <= context["floor"], "graph revision outside 1..floor")
                ids[name].append(ident)
                decoded[name].append({"id": ident, "revision": revision})
            readers[name].end()
        result.append(decoded)
    check(ids["nodes"] == context["retained_nodes"] and ids["edges"] == context["retained_edges"],
          "graph retained entity coverage mismatch")
    return {"partitions": result}


DECODERS = {"op17": decode_op17, "catalog": decode_catalog,
            "imports": decode_imports, "graph": decode_graph}


def encode_expected(kind, expected, original):
    """Encode explicit expected fields, never use this to manufacture goldens."""
    result = copy.deepcopy(original)
    if kind == "op17":
        keys(expected, "schema precision name dimensions settings_length settings", "op17 expected")
        integer(expected["schema"], 65535, "schema")
        integer(expected["precision"], 255, "precision")
        integer(expected["dimensions"], 65536, "dimensions", 1)
        integer(expected["settings_length"], 16384, "settings_length")
        keys(expected["settings"], SETTINGS_KEYS, "settings expected")
        settings = expected["settings"]
        for name in SETTINGS_KEYS.split()[:9]:
            integer(settings[name], U64_MAX, "settings." + name)
        require(settings == DEFAULT_SETTINGS, "expected settings must be the default-v1 fixture")
        require(expected["settings_length"] == 27, "expected settings_length must be 27")
        require(isinstance(expected["name"], str) and
                re.fullmatch(r"[a-z][a-z0-9_-]{0,62}", expected["name"]) and
                expected["name"] != "default", "expected op17 name")
        name = expected["name"].encode("ascii")
        fixed = struct.pack("<HBHHQBBIIBB",
                            *(settings[key] for key in SETTINGS_KEYS.split()[:9]), 0, 0)
        result["body_hex"] = (struct.pack("<HB5xB", expected["schema"], expected["precision"],
                                          len(name)) + name +
                              struct.pack("<I", expected["dimensions"]) + fixed).hex()
    elif kind == "catalog":
        keys(expected, "opt_in entries", "catalog expected")
        integer(expected["opt_in"], (1 << 32) - 1, "opt_in")
        data = struct.pack("<II", expected["opt_in"], 0)
        for entry in array(expected["entries"], 64, "entries"):
            keys(entry, "id precision dimensions", "catalog entry")
            integer(entry["id"], 65535, "id")
            integer(entry["precision"], 255, "precision")
            integer(entry["dimensions"], 65536, "dimensions", 1)
            data += struct.pack("<HBBI", entry["id"], entry["precision"], 0, entry["dimensions"])
        result["section_hex"] = data.hex()
    elif kind == "imports":
        keys(expected, "max_job jobs closed_order", "imports expected")
        integer(expected["max_job"], U64_MAX, "max_job")
        jobs = array(expected["jobs"], 1088, "jobs")
        order = array(expected["closed_order"], 1024, "closed_order")
        data = struct.pack("<QII", expected["max_job"], len(jobs), len(order))
        for job in jobs:
            keys(job, "job collection state dropped name last_chunk committed skipped digest_hex",
                 "expected job")
            for name in ("job", "last_chunk", "committed", "skipped"):
                integer(job[name], U64_MAX, name)
            integer(job["collection"], 65535, "collection")
            integer(job["state"], 2, "state")
            integer(job["dropped"], 1, "dropped")
            require(isinstance(job["name"], str) and
                    re.fullmatch(r"[a-z][a-z0-9_-]{0,62}", job["name"]), "expected job name")
            name = job["name"].encode("ascii")
            digest = hex_bytes(job["digest_hex"], "digest_hex")
            require(len(digest) == 32, "digest_hex must contain 32 bytes")
            data += struct.pack("<QHBBB3xQQQ", job["job"], job["collection"], job["state"],
                                job["dropped"], len(name), job["last_chunk"],
                                job["committed"], job["skipped"]) + digest + name
        for job in order:
            integer(job, U64_MAX, "closed_order job", 1)
            data += struct.pack("<Q", job)
        result["section_hex"] = data.hex()
    else:
        keys(expected, "partitions", "graph expected")
        parts = array(expected["partitions"], 1 << 32, "expected partitions")
        require(len(parts) == len(result["partitions"]), "expected partition count mismatch")
        for part, output in zip(parts, result["partitions"]):
            keys(part, "partition nodes edges", "expected partition")
            integer(part["partition"], (1 << 32) - 1, "partition")
            output["partition"] = part["partition"]
            for name in ("nodes", "edges"):
                entries = array(part[name], 4096, name)
                data = struct.pack("<II", len(entries), 0)
                for entry in entries:
                    keys(entry, "id revision", "expected graph entry")
                    integer(entry["id"], U64_MAX, "entity ID")
                    integer(entry["revision"], U64_MAX, "revision", 1)
                    data += struct.pack("<QQ", entry["id"], entry["revision"])
                output[name + "_hex"] = data.hex()
    return result


def section_targets(kind, value):
    if kind == "op17":
        return {"body": (value, "body_hex")}
    if kind in ("catalog", "imports"):
        return {"section": (value, "section_hex")}
    return {f"{i}.{name}": (part, name + "_hex")
            for i, part in enumerate(value["partitions"]) for name in ("nodes", "edges")}


def payload_bytes(kind, value):
    return {target: hex_bytes(holder[key], target)
            for target, (holder, key) in section_targets(kind, value).items()}


def apply_edits(kind, value, edits):
    result = copy.deepcopy(value)
    targets = section_targets(kind, result)
    for edit in array(edits, 16, "edits"):
        require(isinstance(edit, dict), "edit: expected object")
        if "length" in edit:
            keys(edit, "target length", "truncate edit")
        else:
            keys(edit, "target offset hex", "byte edit")
        require(isinstance(edit["target"], str) and edit["target"] in targets,
                "edit: unknown target")
        holder, key = targets[edit["target"]]
        data = hex_bytes(holder[key], key)
        if "length" in edit:
            length = integer(edit["length"], len(data), "truncate length")
            require(length < len(data), "truncate edit must remove bytes")
            data = data[:length]
        else:
            offset = integer(edit["offset"], len(data), "edit offset")
            replacement = hex_bytes(edit["hex"], "edit hex")
            # Appending is useful for explicit trailing-byte/oversize rejection.
            data = data[:offset] + replacement + data[offset + len(replacement):]
        holder[key] = data.hex()
    return result


def assert_rejected(kind, value, context, expected, location):
    try:
        DECODERS[kind](value, context)
    except Rejected as error:
        require(error.category == expected, f"{location}: expected {expected}, got {error}")
    else:
        raise ValueError(f"{location}: invalid payload accepted")


def validate_document(document):
    keys(document, "schema version scope cases invalid_cases", "root")
    require(document["schema"] == SCHEMA, "schema: unsupported fixture schema")
    require(type(document["version"]) is int and document["version"] == 1,
            "version: expected integer 1")
    require(document["scope"] == SCOPE, "scope: reference-only boundary must be explicit")
    cases = array(document["cases"], 1000, "cases")
    require(cases, "cases: expected hand-authored accepted cases")
    by_name = {}
    for case in cases:
        keys(case, "name kind input context expected", "case")
        name = case["name"]
        require(isinstance(name, str) and name.strip() and name not in by_name,
                "case: empty/duplicate name")
        kind = case["kind"]
        require(isinstance(kind, str) and kind in DECODERS, f"{name}: unknown kind")
        validate_input(kind, case["input"])
        validate_context(kind, case["context"])
        encoded = encode_expected(kind, case["expected"], case["input"])
        try:
            actual = DECODERS[kind](case["input"], case["context"])
        except Rejected as error:
            raise ValueError(f"{name}: accepted golden rejected: {error}") from error
        require(actual == case["expected"], f"{name}: logical golden mismatch: {actual!r}")
        require(payload_bytes(kind, encoded) == payload_bytes(kind, case["input"]),
                f"{name}: additional byte round-trip mismatch")
        by_name[name] = case
    require({case["kind"] for case in cases} == set(DECODERS), "cases: all four kinds required")
    seen = set(by_name)
    invalid = array(document["invalid_cases"], 1000, "invalid_cases")
    require(invalid, "invalid_cases: expected rejection cases")
    for case in invalid:
        keys(case, "name base edits input_override context_override error", "invalid case")
        name = case["name"]
        require(isinstance(name, str) and name.strip() and name not in seen,
                "invalid case: empty/duplicate name")
        seen.add(name)
        require(isinstance(case["base"], str) and case["base"] in by_name,
                f"{name}: unknown base")
        require(case["error"] in ("unsupported", "malformed"), f"{name}: unknown error")
        base = by_name[case["base"]]
        kind = base["kind"]
        value = apply_edits(kind, base["input"], case["edits"])
        override = case["input_override"]
        require(isinstance(override, dict) and set(override) <= set(value),
                f"{name}: unknown input override keys")
        value.update(override)
        context = base["context"] if case["context_override"] is None else case["context_override"]
        validate_input(kind, value)
        validate_context(kind, context)
        assert_rejected(kind, value, context, case["error"], name)
    return len(cases), len(invalid)


def validate_fixture(path):
    return validate_document(load_fixture(path))


def self_test(document):
    count = 0
    for case in document["cases"]:
        kind = case["kind"]
        # Every strict prefix, not just convenient field boundaries.
        for target, (holder, key) in section_targets(kind, case["input"]).items():
            for length in range(len(hex_bytes(holder[key], key))):
                value = apply_edits(kind, case["input"], [{"target": target, "length": length}])
                assert_rejected(kind, value, case["context"], "malformed",
                                f"self-test {case['name']} {target} prefix {length}")
                count += 1
    for kind, size, target in (("op17", 16461, "body"), ("imports", 155089, "section"),
                               ("graph", 65545, "0.nodes")):
        case = next(case for case in document["cases"] if case["kind"] == kind)
        value = copy.deepcopy(case["input"])
        holder, key = section_targets(kind, value)[target]
        holder[key] = "00" * size
        assert_rejected(kind, value, case["context"], "malformed", f"oversized {kind}")
        count += 1
    op17 = next(case for case in document["cases"] if case["kind"] == "op17")
    value = copy.deepcopy(op17["input"])
    data = hex_bytes(value["body_hex"], "body")
    create_prefix_length = 8 + 1 + data[8] + 4
    value["body_hex"] = (data[:create_prefix_length] + bytes(16385)).hex()
    assert_rejected("op17", value, op17["context"], "malformed", "oversized settings")
    count += 1
    op17_index = next(index for index, case in enumerate(document["cases"]) if case["kind"] == "op17")
    broken = copy.deepcopy(document)
    expected = broken["cases"][op17_index]["expected"]
    expected["precision"] = 3 - expected["precision"]
    text = json.dumps(document)
    corruptions = [
        ("logical golden", json.dumps(broken), "golden mismatch"),
        ("duplicate key", text.replace('"version": 1', '"version": 1, "version": 1', 1),
         "duplicate JSON key"),
        ("NaN", text.replace('"version": 1', '"version": NaN', 1), "invalid JSON constant"),
        ("Infinity", text.replace('"version": 1', '"version": Infinity', 1), "invalid JSON constant"),
        ("float overflow", text.replace('"version": 1', '"version": 1e999', 1), "non-finite JSON"),
        ("boolean version", text.replace('"version": 1', '"version": true', 1), "expected integer 1"),
        ("unknown root key", text.replace("{", '{"unknown": 0,', 1), "expected only"),
    ]
    for label, data, diagnostic in corruptions:
        with tempfile.TemporaryDirectory(prefix="vlite-precision-oracle-") as directory:
            path = Path(directory) / "invalid.json"
            path.write_text(data, encoding="utf-8")
            result = subprocess.run([sys.executable, str(Path(__file__).resolve()), str(path)],
                                    capture_output=True, text=True, timeout=30, check=False)
            require(result.returncode == 1 and diagnostic in result.stderr,
                    f"self-test {label}: wrong exit/diagnostic: {result.stdout} {result.stderr}")
            count += 1
    # Exact nested keys are enforced, not merely root keys.
    for path in (("cases", op17_index), ("cases", op17_index, "input"),
                 ("cases", op17_index, "context"), ("cases", op17_index, "expected"),
                 ("cases", op17_index, "expected", "settings"), ("invalid_cases", 0)):
        broken = copy.deepcopy(document)
        nested = broken
        for component in path:
            nested = nested[component]
        nested["unknown"] = 0
        try:
            validate_document(broken)
        except ValueError as error:
            require("expected only" in str(error), f"nested key rejection: {error}")
        else:
            raise ValueError(f"unknown nested key accepted: {path}")
        count += 1
    broken = copy.deepcopy(document)
    broken["invalid_cases"][0]["edits"] = [{"target": "section", "length": 0, "unknown": 0}]
    try:
        validate_document(broken)
    except ValueError as error:
        require("expected only" in str(error), f"nested edit key rejection: {error}")
    else:
        raise ValueError("unknown nested edit key accepted")
    count += 1
    return count


def main():
    parser = argparse.ArgumentParser(description=SCOPE + " " + __doc__)
    parser.add_argument("fixture", nargs="?", type=Path, default=FIXTURE,
                        help="default: fixtures/compact/precision-catalog-cases.json beside __file__")
    parser.add_argument("--self-test", action="store_true",
                        help="also test every truncated prefix, oversize limits and strict JSON/CLI errors")
    args = parser.parse_args()
    try:
        accepted, rejected = validate_fixture(args.fixture)
        checks = self_test(load_fixture(args.fixture)) if args.self_test else None
    except (OSError, ValueError, struct.error, subprocess.SubprocessError) as error:
        print(f"{args.fixture}: {error}", file=sys.stderr)
        return 1
    print(f"Verified {accepted} accepted and {rejected} rejected precision byte cases: {args.fixture}")
    if checks is not None:
        print(f"Self-test: {checks} rejection checks passed")
    print(SCOPE)
    return 0


if __name__ == "__main__":
    sys.exit(main())
