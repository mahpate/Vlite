"""Stdlib-only preparation reference for the approved Gap 7 embedded design.

Run ``python tests\\ingestion_reference.py --self-test``. Goldens are hand-authored
in ``tests/fixtures/ingestion/golden.json``; this reference never writes them.

This models original-byte chunking, local dictionary extraction, ascii-key-v1
normalization, and source-local evidence validation/resolution only. It does not
model canonical identity digests, embeddings, storage plans, commits, receipts,
providers, serving, or performance. The design does not define the byte encoding
for decoded-output or per-observation attribute-size accounting; those two byte
ceilings are deliberately not asserted here.

Fixture source values are exact UTF-8 strings, hexadecimal bytes, or explicitly
concatenated/repeated byte pieces. A nonfinite confidence is represented in the
fixture by ``{"nonfinite": "nan"}`` (or ``"infinity"``); it is converted to an
IEEE float only when constructing a fixed local extraction observation.
"""

import argparse
import copy
import hashlib
import json
import math
from pathlib import Path
import sys


FIXTURE = Path(__file__).resolve().parent / "fixtures" / "ingestion" / "golden.json"
SCHEMA = "vlite.ingestion-preparation-golden"
MAX_SOURCE_BYTES = 1_048_576
MAX_CHUNKS = 512
MAX_CHUNK_BYTES = 16_384
MAX_ENTITY_KEY_BYTES = 256
MAX_CHUNK_ENTITIES = 256
MAX_CHUNK_RELATIONS = 512
MAX_DOCUMENT_ENTITIES = 4_096
MAX_DOCUMENT_RELATIONS = 8_192
MAX_DOCUMENT_MENTIONS = 16_384
MAX_RELATION_UNION_BYTES = 1_024
MAX_ATTRIBUTE_FIELDS = 16
MAX_STRING_BYTES = 4_096
ASCII_SPACE = b" \t\r\n"


class PreparationError(ValueError):
    """A stable local category; not a proposed public VLite error."""

    def __init__(self, category):
        super().__init__(category)
        self.category = category


def require(condition, category):
    if not condition:
        raise PreparationError(category)


def exact_keys(value, required, optional=()):
    return (isinstance(value, dict) and set(required) <= set(value)
            and set(value) <= set(required) | set(optional))


def utf8_bytes(value, category, nonempty=False, no_nul=False):
    require(isinstance(value, str), category)
    try:
        raw = value.encode("utf-8", "strict")
    except UnicodeEncodeError as error:
        raise PreparationError(category) from error
    require(not nonempty or bool(raw), category)
    require(not no_nul or b"\0" not in raw, category)
    return raw


def fixture_source(value):
    if isinstance(value, str):
        return utf8_bytes(value, "source_encoding")
    if isinstance(value, dict) and exact_keys(value, ("hex",)):
        try:
            return bytes.fromhex(value["hex"])
        except (TypeError, ValueError) as error:
            raise PreparationError("source_encoding") from error
    if isinstance(value, dict) and exact_keys(value, ("pieces",)):
        require(isinstance(value["pieces"], list), "source_encoding")
        result = bytearray()
        for piece in value["pieces"]:
            if isinstance(piece, dict) and exact_keys(piece, ("utf8",)):
                result.extend(utf8_bytes(piece["utf8"], "source_encoding"))
            elif isinstance(piece, dict) and exact_keys(
                    piece, ("repeat_utf8", "count")):
                unit = utf8_bytes(piece["repeat_utf8"], "source_encoding")
                count = piece["count"]
                require(type(count) is int and count >= 0, "source_encoding")
                result.extend(unit * count)
            elif isinstance(piece, dict) and exact_keys(
                    piece, ("repeat_hex", "count")):
                try:
                    unit = bytes.fromhex(piece["repeat_hex"])
                except (TypeError, ValueError) as error:
                    raise PreparationError("source_encoding") from error
                count = piece["count"]
                require(type(count) is int and count >= 0, "source_encoding")
                result.extend(unit * count)
            else:
                raise PreparationError("source_encoding")
        return bytes(result)
    raise PreparationError("source_encoding")


def scalar_boundaries(raw):
    try:
        text = raw.decode("utf-8", "strict")
    except UnicodeDecodeError as error:
        raise PreparationError("source_invalid_utf8") from error
    offsets = [0]
    offset = 0
    for scalar in text:
        offset += len(scalar.encode("utf-8"))
        offsets.append(offset)
    require(offset == len(raw), "source_invalid_utf8")
    return offsets


def largest_boundary_at_or_before(boundaries, offset):
    low, high = 0, len(boundaries)
    while low < high:
        middle = (low + high) // 2
        if boundaries[middle] <= offset:
            low = middle + 1
        else:
            high = middle
    return boundaries[low - 1]


def smallest_boundary_at_or_after(boundaries, offset):
    low, high = 0, len(boundaries)
    while low < high:
        middle = (low + high) // 2
        if boundaries[middle] < offset:
            low = middle + 1
        else:
            high = middle
    require(low < len(boundaries), "chunk_progress")
    return boundaries[low]


def validate_source(source, raw):
    require(exact_keys(source, ("tenant", "source_id"), ("uri",)), "source_fields")
    tenant = utf8_bytes(source["tenant"], "source_identity", nonempty=True, no_nul=True)
    source_id = utf8_bytes(source["source_id"], "source_identity",
                           nonempty=True, no_nul=True)
    require(len(tenant) <= 128 and len(source_id) <= 256, "source_identity")
    if "uri" in source:
        uri = utf8_bytes(source["uri"], "source_identity", no_nul=True)
        require(len(uri) <= 2_048, "source_identity")
    require(bool(raw), "source_empty")
    require(len(raw) <= MAX_SOURCE_BYTES, "source_too_large")
    scalar_boundaries(raw)
    return {
        "tenant": source["tenant"],
        "source_id": source["source_id"],
        "uri": source.get("uri"),
        "byte_length": len(raw),
    }


def validate_chunk_config(strategy, chunk_bytes, overlap_bytes):
    require(strategy in ("FixedUtf8V1", "ParagraphV1"), "chunk_strategy")
    require(type(chunk_bytes) is int and 1_024 <= chunk_bytes <= MAX_CHUNK_BYTES,
            "chunk_size")
    require(type(overlap_bytes) is int
            and 0 <= overlap_bytes <= min(1_024, chunk_bytes // 4),
            "chunk_overlap")


def split_chunks(raw, strategy, chunk_bytes, overlap_bytes):
    boundaries = scalar_boundaries(raw)
    chunks = []
    start = 0
    while start < len(raw):
        hard_end = largest_boundary_at_or_before(
            boundaries, min(start + chunk_bytes, len(raw)))
        require(hard_end > start, "chunk_progress")
        end = hard_end
        if strategy == "ParagraphV1" and hard_end < len(raw):
            lower = start + (chunk_bytes + 1) // 2
            breaks = []
            search = start
            while True:
                found = raw.find(b"\n\n", search, hard_end)
                if found < 0:
                    break
                after = found + 2
                if lower <= after <= hard_end:
                    breaks.append(after)
                search = found + 1
            if breaks:
                end = breaks[-1]
        # A UTF-8 scalar is at most four bytes, so this check is bounded by the
        # already validated source and makes accidental byte slicing fail closed.
        require(end in boundaries and start in boundaries, "chunk_boundary")
        try:
            text = raw[start:end].decode("utf-8", "strict")
        except UnicodeDecodeError as error:
            raise PreparationError("chunk_boundary") from error
        chunks.append({
            "ordinal": len(chunks),
            "start": start,
            "end": end,
            "text": text,
            "text_hex": raw[start:end].hex(),
            "sha256": hashlib.sha256(raw[start:end]).hexdigest(),
        })
        if end == len(raw):
            break
        next_start = smallest_boundary_at_or_after(boundaries, end - overlap_bytes)
        if next_start <= start:
            next_start = smallest_boundary_at_or_after(boundaries, start + 1)
        require(next_start > start and next_start < len(raw), "chunk_progress")
        start = next_start
        require(len(chunks) <= MAX_CHUNKS, "chunk_limit")
    require(len(chunks) <= MAX_CHUNKS, "chunk_limit")
    return chunks


def normalize_key(key):
    raw = utf8_bytes(key, "entity_key", nonempty=True)
    first, last = 0, len(raw)
    while first < last and raw[first] in ASCII_SPACE:
        first += 1
    while last > first and raw[last - 1] in ASCII_SPACE:
        last -= 1
    result = bytearray()
    in_space = False
    for value in raw[first:last]:
        if value in ASCII_SPACE:
            if not in_space:
                result.append(0x20)
            in_space = True
        else:
            result.append(value + 0x20 if 0x41 <= value <= 0x5A else value)
            in_space = False
    require(bool(result), "entity_key_empty")
    require(len(result) <= MAX_ENTITY_KEY_BYTES, "entity_key_too_long")
    return result.decode("utf-8", "strict")


def canonical_scalar(value):
    if value is None or type(value) in (str, bool, int):
        if isinstance(value, str):
            raw = utf8_bytes(value, "attribute_value")
            require(len(raw) <= MAX_STRING_BYTES, "attribute_string_limit")
        return value
    if type(value) is float:
        require(math.isfinite(value), "confidence_invalid")
        return 0.0 if value == 0.0 else value
    raise PreparationError("attribute_value")


def canonical_attributes(value):
    require(isinstance(value, dict) and len(value) <= MAX_ATTRIBUTE_FIELDS,
            "attribute_fields")
    result = {}
    for key in sorted(value, key=lambda item: utf8_bytes(item, "attribute_key")):
        key_bytes = utf8_bytes(key, "attribute_key", nonempty=True)
        require(len(key_bytes) <= MAX_STRING_BYTES, "attribute_string_limit")
        result[key] = canonical_scalar(value[key])
    return result


def validate_span(span, chunk):
    require(isinstance(span, list) and len(span) == 2
            and all(type(value) is int for value in span), "evidence_span")
    start, end = span
    require(0 <= start < end <= len(chunk["text_hex"]) // 2, "evidence_span")
    raw = bytes.fromhex(chunk["text_hex"])
    boundaries = scalar_boundaries(raw)
    require(start in boundaries and end in boundaries, "evidence_boundary")
    return start, end


def confidence_value(value):
    if isinstance(value, dict) and exact_keys(value, ("nonfinite",)):
        marker = value["nonfinite"]
        if marker == "nan":
            value = float("nan")
        elif marker == "infinity":
            value = float("inf")
        elif marker == "-infinity":
            value = float("-inf")
    require(value is None or (type(value) in (int, float)
            and math.isfinite(value) and 0.0 <= value <= 1.0), "confidence_invalid")
    return value


def validate_extraction(chunk, extraction):
    require(exact_keys(extraction, ("entities", "relations")), "extraction_fields")
    entities, relations = extraction["entities"], extraction["relations"]
    require(isinstance(entities, list) and isinstance(relations, list),
            "extraction_fields")
    require(len(entities) <= MAX_CHUNK_ENTITIES, "chunk_entity_limit")
    require(len(relations) <= MAX_CHUNK_RELATIONS, "chunk_relation_limit")
    local = {}
    for observation in entities:
        require(exact_keys(observation, (
            "local_id", "kind", "key", "surface", "span", "attributes", "confidence")),
            "entity_fields")
        local_id = observation["local_id"]
        require(type(local_id) is int and 0 <= local_id <= 0xFFFFFFFF,
                "local_id_invalid")
        require(local_id not in local, "local_id_duplicate")
        kind = utf8_bytes(observation["kind"], "entity_kind", nonempty=True, no_nul=True)
        key = normalize_key(observation["key"])
        surface = utf8_bytes(observation["surface"], "entity_surface", nonempty=True)
        start, end = validate_span(observation["span"], chunk)
        chunk_bytes = bytes.fromhex(chunk["text_hex"])
        require(surface == chunk_bytes[start:end], "entity_surface_mismatch")
        attrs = canonical_attributes(observation["attributes"])
        confidence = confidence_value(observation["confidence"])
        local[local_id] = {
            "local_id": local_id,
            "kind": observation["kind"],
            "normalized_key": key,
            "surface": observation["surface"],
            "span": [start, end],
            "attributes": attrs,
            "confidence": confidence,
        }
        require(bool(kind), "entity_kind")
    checked_relations = []
    for observation in relations:
        require(exact_keys(observation, (
            "subject_local_id", "predicate", "object_local_id", "span",
            "attributes", "confidence")), "relation_fields")
        subject_id, object_id = observation["subject_local_id"], observation["object_local_id"]
        require(type(subject_id) is int and type(object_id) is int
                and subject_id in local and object_id in local, "local_reference_invalid")
        predicate = utf8_bytes(observation["predicate"], "predicate",
                               nonempty=True, no_nul=True)
        start, end = validate_span(observation["span"], chunk)
        attrs = canonical_attributes(observation["attributes"])
        confidence = confidence_value(observation["confidence"])
        checked_relations.append({
            "subject_local_id": subject_id,
            "predicate": observation["predicate"],
            "object_local_id": object_id,
            "span": [start, end],
            "attributes": attrs,
            "confidence": confidence,
        })
        require(bool(predicate), "predicate")
    return list(local.values()), checked_relations


def _validate_rule(rule, required, category):
    require(exact_keys(rule, required), category)
    for field in ("rule_id", "kind", "key", "predicate", "subject_kind", "object_kind"):
        if field in rule:
            utf8_bytes(rule[field], category, nonempty=True, no_nul=True)


def dictionary_entities(chunk, rules):
    require(isinstance(rules, list), "entity_rules")
    seen_ids = set()
    checked = []
    for rule in rules:
        _validate_rule(rule, ("rule_id", "literal", "kind", "key"), "entity_rules")
        rule_id = utf8_bytes(rule["rule_id"], "entity_rules", nonempty=True, no_nul=True)
        require(rule_id not in seen_ids, "entity_rule_duplicate")
        seen_ids.add(rule_id)
        literal = utf8_bytes(rule["literal"], "entity_rules", nonempty=True)
        utf8_bytes(rule["kind"], "entity_kind", nonempty=True, no_nul=True)
        normalize_key(rule["key"])
        checked.append((rule_id, rule, literal))
    checked.sort(key=lambda item: item[0])
    raw = bytes.fromhex(chunk["text_hex"])
    boundaries = scalar_boundaries(raw)
    observations = []
    for start in boundaries[:-1]:
        for _, rule, literal in checked:
            end = start + len(literal)
            if end <= len(raw) and end in boundaries and raw.startswith(literal, start):
                observations.append({
                    "local_id": len(observations),
                    "kind": rule["kind"],
                    "key": rule["key"],
                    "surface": raw[start:end].decode("utf-8"),
                    "span": [start, end],
                    "attributes": {},
                    "confidence": None,
                })
                require(len(observations) <= MAX_CHUNK_ENTITIES, "chunk_entity_limit")
    return observations


def dictionary_relations(chunk, entities, rules):
    require(isinstance(rules, list), "relation_rules")
    seen_ids, candidates = set(), []
    raw = bytes.fromhex(chunk["text_hex"])
    for rule in rules:
        _validate_rule(rule, (
            "rule_id", "predicate", "subject_kind", "object_kind"), "relation_rules")
        rule_id = utf8_bytes(rule["rule_id"], "relation_rules", nonempty=True, no_nul=True)
        require(rule_id not in seen_ids, "relation_rule_duplicate")
        seen_ids.add(rule_id)
        utf8_bytes(rule["predicate"], "predicate", nonempty=True, no_nul=True)
        subjects = [item for item in entities if item["kind"] == rule["subject_kind"]]
        objects = [item for item in entities if item["kind"] == rule["object_kind"]]
        for subject in subjects:
            s0, s1 = subject["span"]
            if b"\n" in raw[s0:s1]:
                continue
            subject_line = raw[:s0].count(b"\n")
            for obj in objects:
                if subject["local_id"] == obj["local_id"]:
                    continue
                o0, o1 = obj["span"]
                if b"\n" in raw[o0:o1] or raw[:o0].count(b"\n") != subject_line:
                    continue
                start, end = min(s0, o0), max(s1, o1)
                if end - start > MAX_RELATION_UNION_BYTES:
                    continue
                candidates.append({
                    "subject_local_id": subject["local_id"],
                    "predicate": rule["predicate"],
                    "object_local_id": obj["local_id"],
                    "span": [start, end],
                    "attributes": {},
                    "confidence": None,
                })
                require(len(candidates) <= MAX_CHUNK_RELATIONS, "chunk_relation_limit")
    candidates.sort(key=lambda item: (
        item["span"][0], item["span"][1], item["subject_local_id"],
        item["predicate"].encode("utf-8"), item["object_local_id"]))
    return candidates


def json_key(value):
    return json.dumps(value, ensure_ascii=False, sort_keys=True,
                      separators=(",", ":"), allow_nan=False)


def expand_observations(value, field):
    if isinstance(value, dict):
        require(exact_keys(value, ("repeat", "value"), ("local_id_start",)),
                "extraction_fields")
        count = value["repeat"]
        require(type(count) is int and count >= 0, "extraction_fields")
        template = value["value"]
        require(isinstance(template, dict), "extraction_fields")
        output = []
        for index in range(count):
            item = copy.deepcopy(template)
            if field == "entities":
                start = value.get("local_id_start", template.get("local_id", 0))
                require(type(start) is int, "local_id_invalid")
                item["local_id"] = start + index
            output.append(item)
        return output
    require(isinstance(value, list), "extraction_fields")
    return value


def entity_identity(binding, tenant, observation):
    return (binding, tenant, observation["kind"], observation["normalized_key"])


def identity_bytes(identity):
    return tuple(value.encode("utf-8") for value in identity)


def run_case(case):
    require(exact_keys(case, (
        "name", "source", "strategy", "chunk_bytes", "overlap_bytes",
        "binding", "source_identity", "entity_rules", "relation_rules", "expected"),
        ("extractions",)), "case_fields")
    raw = fixture_source(case["source"])
    source = validate_source(case["source_identity"], raw)
    validate_chunk_config(case["strategy"], case["chunk_bytes"], case["overlap_bytes"])
    binding = case["binding"]
    utf8_bytes(binding, "binding", nonempty=True, no_nul=True)
    chunks = split_chunks(raw, case["strategy"], case["chunk_bytes"], case["overlap_bytes"])

    fixed = case.get("extractions")
    if fixed is not None:
        require(isinstance(fixed, list), "extraction_fields")
        fixed_by_ordinal = {}
        for item in fixed:
            require(exact_keys(item, ("chunk", "entities", "relations")), "extraction_fields")
            ordinal = item["chunk"]
            require(type(ordinal) is int and 0 <= ordinal < len(chunks),
                    "local_reference_invalid")
            require(ordinal not in fixed_by_ordinal, "extraction_fields")
            fixed_by_ordinal[ordinal] = {
                "entities": expand_observations(item["entities"], "entities"),
                "relations": expand_observations(item["relations"], "relations")}
    else:
        fixed_by_ordinal = {}

    anchors = {}
    mentions = {}
    relations = {}
    for chunk in chunks:
        ordinal = chunk["ordinal"]
        if fixed is None:
            extracted_entities = dictionary_entities(chunk, case["entity_rules"])
            extracted_relations = dictionary_relations(
                chunk, extracted_entities, case["relation_rules"])
        else:
            extraction = fixed_by_ordinal.get(ordinal, {"entities": [], "relations": []})
            extracted_entities, extracted_relations = validate_extraction(chunk, extraction)
        if fixed is None:
            extraction = {"entities": extracted_entities, "relations": extracted_relations}
            extracted_entities, extracted_relations = validate_extraction(chunk, extraction)

        local = {item["local_id"]: item for item in extracted_entities}
        resolved = {}
        for observation in extracted_entities:
            ident = entity_identity(binding, source["tenant"], observation)
            resolved[observation["local_id"]] = ident
            anchors[ident] = {
                "binding": ident[0], "tenant": ident[1], "kind": ident[2],
                "normalized_key": ident[3],
            }
            start, end = observation["span"]
            source_start = chunk["start"] + start
            source_end = chunk["start"] + end
            require(0 <= source_start < source_end <= source["byte_length"],
                    "evidence_source_span")
            source_span = [source_start, source_end]
            key = (ordinal, ident, source_span[0], source_span[1])
            value = {
                "chunk": ordinal, "entity": anchors[ident],
                "source_span": source_span, "chunk_span": [start, end],
                "surface": observation["surface"],
                "attributes": observation["attributes"],
                "confidence": observation["confidence"],
            }
            prior = mentions.get(key)
            if prior is not None and json_key(prior) != json_key(value):
                raise PreparationError("mention_conflict")
            mentions[key] = value
        for observation in extracted_relations:
            subject_id, object_id = (observation["subject_local_id"],
                                     observation["object_local_id"])
            require(subject_id in local and object_id in local, "local_reference_invalid")
            subject, object_ = resolved[subject_id], resolved[object_id]
            start, end = observation["span"]
            source_start = chunk["start"] + start
            source_end = chunk["start"] + end
            require(0 <= source_start < source_end <= source["byte_length"],
                    "evidence_source_span")
            source_span = [source_start, source_end]
            attrs = observation["attributes"]
            key = (ordinal, subject, observation["predicate"], object_,
                   source_span[0], source_span[1], json_key(attrs))
            value = {
                "chunk": ordinal, "subject": anchors[subject],
                "predicate": observation["predicate"], "object": anchors[object_],
                "source_span": source_span, "chunk_span": [start, end],
                "attributes": attrs, "confidence": observation["confidence"],
            }
            prior = relations.get(key)
            if prior is not None and json_key(prior) != json_key(value):
                raise PreparationError("relation_conflict")
            relations[key] = value

    require(len(anchors) <= MAX_DOCUMENT_ENTITIES, "document_entity_limit")
    require(len(relations) <= MAX_DOCUMENT_RELATIONS, "document_relation_limit")
    require(len(mentions) <= MAX_DOCUMENT_MENTIONS, "document_mention_limit")
    return {
        "source": source,
        "chunks": chunks,
        "entities": sorted(anchors.values(), key=lambda item: tuple(
            value.encode("utf-8") for value in (
                item["binding"], item["tenant"], item["kind"], item["normalized_key"]))),
        "mentions": sorted(mentions.values(), key=lambda item: (
            item["chunk"], item["source_span"][0], item["source_span"][1],
            identity_bytes((item["entity"]["binding"], item["entity"]["tenant"],
                            item["entity"]["kind"], item["entity"]["normalized_key"])))),
        "relations": sorted(relations.values(), key=lambda item: (
            item["chunk"], item["source_span"][0], item["source_span"][1],
            identity_bytes((item["subject"]["binding"], item["subject"]["tenant"],
                            item["subject"]["kind"], item["subject"]["normalized_key"])),
            item["predicate"].encode("utf-8"),
            identity_bytes((item["object"]["binding"], item["object"]["tenant"],
                            item["object"]["kind"], item["object"]["normalized_key"])),
            json_key(item["attributes"]))),
    }


def expand_expected(case):
    expected = copy.deepcopy(case["expected"])
    if "error" in expected:
        return expected
    raw = fixture_source(case["source"])
    if "chunks_repeat" in expected:
        pattern = expected.pop("chunks_repeat")
        require(exact_keys(pattern, ("count", "length", "stride", "sha256")),
                "golden_chunk_fields")
        require(type(pattern["count"]) is int and pattern["count"] >= 0
                and type(pattern["length"]) is int and pattern["length"] > 0
                and type(pattern["stride"]) is int and pattern["stride"] >= 0,
                "golden_chunk_fields")
        expected["chunks"] = [
            {"ordinal": index, "start": index * pattern["stride"],
             "end": index * pattern["stride"] + pattern["length"],
             "sha256": pattern["sha256"]}
            for index in range(pattern["count"])]
    for chunk in expected["chunks"]:
        require(exact_keys(chunk, ("ordinal", "start", "end", "sha256")),
                "golden_chunk_fields")
        start, end = chunk["start"], chunk["end"]
        require(type(start) is int and type(end) is int and 0 <= start < end <= len(raw),
                "golden_chunk_span")
        chunk["text"] = raw[start:end].decode("utf-8", "strict")
        chunk["text_hex"] = raw[start:end].hex()
    return expected


def verify_document(document):
    require(exact_keys(document, ("schema", "version", "cases")), "root_fields")
    require(document["schema"] == SCHEMA, "schema")
    require(type(document["version"]) is int and document["version"] == 1, "version")
    require(isinstance(document["cases"], list) and bool(document["cases"]), "cases")
    names = set()
    for case in document["cases"]:
        require(isinstance(case, dict) and isinstance(case.get("name"), str)
                and case["name"], "case_name")
        require(case["name"] not in names, "case_duplicate")
        names.add(case["name"])
        expected = expand_expected(case)
        try:
            actual = run_case(case)
        except PreparationError as error:
            actual = {"error": error.category}
        require(json_key(actual) == json_key(expected),
                f"golden_mismatch:{case['name']}")
    return len(names)


def unique_object(pairs):
    result = {}
    for key, value in pairs:
        require(key not in result, f"duplicate JSON key {key!r}")
        result[key] = value
    return result


def reject_constant(value):
    raise ValueError(f"invalid JSON constant {value!r}")


def finite_float(value):
    result = float(value)
    require(math.isfinite(result), "nonfinite JSON number")
    return result


def load_fixture(path):
    return json.loads(path.read_text(encoding="utf-8"),
                      object_pairs_hook=unique_object,
                      parse_constant=reject_constant, parse_float=finite_float)


def _mutant_check(document, label, name, replacement, case_name):
    original = globals()[name]
    globals()[name] = replacement(original)
    try:
        try:
            case = next(item for item in document["cases"] if item["name"] == case_name)
            verify_document({"schema": SCHEMA, "version": 1, "cases": [case]})
        except (PreparationError, ValueError, UnicodeError):
            return
        raise PreparationError(f"self_test_mutant_survived:{label}")
    finally:
        globals()[name] = original


def self_test(document):
    detected = []
    mutants = [
        ("overlap-ignored", "split_chunks",
         lambda original: lambda raw, strategy, size, overlap:
         original(raw, strategy, size, 0), "fixed-utf8-boundaries-overlap-bom-combining"),
        ("utf8-scalar-split", "largest_boundary_at_or_before",
         lambda original: lambda boundaries, offset: offset,
         "fixed-utf8-boundaries-overlap-bom-combining"),
        ("unicode-casefold", "normalize_key",
         lambda original: lambda key: key.strip().lower(),
         "turkish-i-and-non-ascii-preserved"),
        ("evidence-span-accepted", "validate_span",
         lambda original: lambda span, chunk: (0, 1),
         "unaligned-local-evidence-span-rejects"),
        ("relation-endpoints-swapped", "dictionary_relations",
         lambda original: lambda chunk, entities, rules: [
             dict(item, subject_local_id=item["object_local_id"],
                  object_local_id=item["subject_local_id"])
             for item in original(chunk, entities, rules)],
         "ascii-only-normalization-and-line-local-relations"),
    ]
    for label, name, replacement, case_name in mutants:
        _mutant_check(document, label, name, replacement, case_name)
        detected.append(label)

    broken = copy.deepcopy(document)
    positive = next((case for case in broken["cases"]
                     if "chunks" in case["expected"]), None)
    require(positive is not None, "self_test_positive_case")
    positive["expected"]["chunks"][0]["sha256"] = "0" * 64
    try:
        verify_document(broken)
    except PreparationError:
        pass
    else:
        raise PreparationError("self_test_golden_mutation_survived")

    # These are generated only in memory to check the individual float guard;
    # fixture JSON itself remains standards-compliant and is never rewritten.
    case = copy.deepcopy(next(
        item for item in document["cases"]
        if "extractions" in item and isinstance(item["extractions"][0]["entities"], list)))
    case["extractions"][0]["entities"][0]["confidence"] = float("nan")
    try:
        run_case(case)
    except PreparationError as error:
        require(error.category == "confidence_invalid", "self_test_nonfinite_confidence")
    else:
        raise PreparationError("self_test_nonfinite_confidence_survived")

    def base_case(name, unit, count, extractions):
        return {
            "name": name,
            "source": {"pieces": [{"repeat_utf8": unit, "count": count}]},
            "strategy": "FixedUtf8V1", "chunk_bytes": 1_024, "overlap_bytes": 0,
            "binding": "binding-a",
            "source_identity": {"tenant": "tenant-a", "source_id": name},
            "entity_rules": [], "relation_rules": [], "extractions": extractions,
            "expected": {},
        }

    def run_boundary(case, field, maximum):
        try:
            result = run_case(case)
        except PreparationError as error:
            raise PreparationError(
                f"self_test_document_limit_at_boundary:{field}:{error.category}") from error
        require(len(result[field]) == maximum,
                f"self_test_document_limit_count:{field}")

    for total, should_accept in ((MAX_DOCUMENT_ENTITIES, True),
                                 (MAX_DOCUMENT_ENTITIES + 1, False)):
        chunks = (total + MAX_CHUNK_ENTITIES - 1) // MAX_CHUNK_ENTITIES
        extractions = []
        for ordinal in range(chunks):
            start = ordinal * MAX_CHUNK_ENTITIES
            entities = [
                {"local_id": index, "kind": "K", "key": f"entity-{start + index}",
                 "surface": "a", "span": [0, 1], "attributes": {}, "confidence": None}
                for index in range(min(MAX_CHUNK_ENTITIES, total - start))]
            extractions.append({"chunk": ordinal, "entities": entities, "relations": []})
        case = base_case("document-entities-" + str(total), "a" * 1_024, chunks, extractions)
        if should_accept:
            run_boundary(case, "entities", MAX_DOCUMENT_ENTITIES)
        else:
            try:
                run_case(case)
            except PreparationError as error:
                require(error.category == "document_entity_limit",
                        "self_test_document_entity_limit")
            else:
                raise PreparationError("self_test_document_entity_limit_survived")

    for total, should_accept in ((MAX_DOCUMENT_RELATIONS, True),
                                 (MAX_DOCUMENT_RELATIONS + 1, False)):
        chunks = (total + MAX_CHUNK_RELATIONS - 1) // MAX_CHUNK_RELATIONS
        unit = "AB" + "x" * 1_022
        extractions = []
        remaining = total
        for ordinal in range(chunks):
            count = min(MAX_CHUNK_RELATIONS, remaining)
            relations = [
                {"subject_local_id": 0, "predicate": f"predicate-{ordinal}-{index}",
                 "object_local_id": 1, "span": [0, 2], "attributes": {},
                 "confidence": None}
                for index in range(count)]
            extractions.append({
                "chunk": ordinal,
                "entities": [
                    {"local_id": 0, "kind": "Left", "key": "left", "surface": "A",
                     "span": [0, 1], "attributes": {}, "confidence": None},
                    {"local_id": 1, "kind": "Right", "key": "right", "surface": "B",
                     "span": [1, 2], "attributes": {}, "confidence": None},
                ],
                "relations": relations,
            })
            remaining -= count
        case = base_case("document-relations-" + str(total), unit, chunks, extractions)
        if should_accept:
            run_boundary(case, "relations", MAX_DOCUMENT_RELATIONS)
        else:
            try:
                run_case(case)
            except PreparationError as error:
                require(error.category == "document_relation_limit",
                        "self_test_document_relation_limit")
            else:
                raise PreparationError("self_test_document_relation_limit_survived")

    for chunks, should_accept in ((MAX_DOCUMENT_MENTIONS // MAX_CHUNK_ENTITIES, True),
                                  (MAX_DOCUMENT_MENTIONS // MAX_CHUNK_ENTITIES + 1, False)):
        unit = "a" * MAX_CHUNK_ENTITIES + "x" * (1_024 - MAX_CHUNK_ENTITIES)
        entities = [
            {"local_id": index, "kind": "K", "key": "same", "surface": "a",
             "span": [index, index + 1], "attributes": {}, "confidence": None}
            for index in range(MAX_CHUNK_ENTITIES)]
        extractions = [
            {"chunk": ordinal, "entities": entities, "relations": []}
            for ordinal in range(chunks)]
        case = base_case("document-mentions-" + str(chunks), unit, chunks, extractions)
        if should_accept:
            run_boundary(case, "mentions", MAX_DOCUMENT_MENTIONS)
        else:
            try:
                run_case(case)
            except PreparationError as error:
                require(error.category == "document_mention_limit",
                        "self_test_document_mention_limit")
            else:
                raise PreparationError("self_test_document_mention_limit_survived")
    detected.extend([
        "document-entities-4096/4097", "document-relations-8192/8193",
        "document-mentions-16384/16385",
    ])
    return detected


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--fixture", type=Path, default=FIXTURE, help="override golden JSON")
    parser.add_argument("--self-test", action="store_true",
                        help="also kill representative hand-mutants")
    args = parser.parse_args()
    try:
        document = load_fixture(args.fixture)
        count = verify_document(document)
        print(f"Verified {count} hand-authored Gap 7 preparation goldens.")
        if args.self_test:
            detected = self_test(document)
            print("Killed hand-mutants: " + ", ".join(detected))
            print("Rejected a hand-mutated golden and nonfinite confidence.")
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        print(f"{args.fixture}: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
