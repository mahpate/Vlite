"""Independent stdlib byte reference for the PROPOSED Gap 7 canonical schema v1.

No Rust helpers, preparation-oracle imports or engine execution. Fixtures contain
hand-specified bytes, not this encoder's saved output. --self-test runs named
in-memory hand-mutants, restores each function, and verifies the source hash.
Runtime execution envelopes are deliberately separate from canonical records.
"""

import argparse
import base64
import copy
import hashlib
import json
import math
from pathlib import Path
import re
import struct
import sys


FIXTURE = Path(__file__).parent / "fixtures" / "ingestion" / "schema-v1" / "vectors.json"
REGISTRY = {
    "source": (1, "source", "src"),
    "revision": (2, "revision", "rev"),
    "chunk": (3, "chunk", "chk"),
    "entity": (4, "entity", "ent"),
    "has_revision": (5, "has-revision", "hr"),
    "has_chunk": (6, "has-chunk", "hc"),
    "mention": (7, "mention", "men"),
    "relation": (8, "relation", "rel"),
    "receipt": (9, "receipt", "rec"),
    "batch_receipt": (10, "batch-receipt", "bat"),
    "result_shard": (11, "result-shard", "ids"),
    "ownership_shard": (12, "ownership-shard", "own"),
    "delete_receipt": (13, "delete-receipt", "del"),
    "abort_receipt": (14, "abort-receipt", "abt"),
}
EXTRA_DOMAINS = ("pipeline", "request", "plan", "batch", "delete-action", "abort-action",
                 "policy", "properties")
ROOTS = ("receipt", "delete_receipt", "abort_receipt")


class Invalid(ValueError):
    pass


def need(condition, reason):
    if not condition:
        raise Invalid(reason)


def fields(**items):
    return tuple(items.items())


def ident(role="any"):
    return ("identity", role)


def record(name):
    return ("record", name)


def seq(item):
    return ("list", item)


def opt(item):
    return ("optional", item)


BINDING = fields(graph="text", collection="text", tenant="text", generation="digest")
IDENTITIES = {
    "source": fields(binding="binding", tenant="text", source_id="text"),
    "revision": fields(source=ident("source"), content="digest", pipeline="digest"),
    "chunk": fields(revision=ident("revision"), ordinal="u32", start="u32", end="u32"),
    "entity": fields(binding="binding", tenant="text", kind="text", normalized_key="text"),
    "has_revision": fields(source=ident("source"), revision=ident("revision")),
    "has_chunk": fields(revision=ident("revision"), chunk=ident("chunk")),
    "mention": fields(chunk=ident("chunk"), entity=ident("entity"), start="u32", end="u32"),
    "relation": fields(revision=ident("revision"), chunk=ident("chunk"),
                       subject=ident("entity"), predicate="text", object=ident("entity"),
                       start="u32", end="u32", attributes="attributes"),
    "receipt": fields(binding="binding", tenant="text", source_id="text",
                      content="digest", pipeline="digest"),
    "batch_receipt": fields(root=ident("root"), ordinal="u32"),
    "result_shard": fields(root=ident("root"), batch="u32", group="u32", index="u32"),
    "ownership_shard": fields(root=ident("root"), batch="u32", group="u32", index="u32"),
    "delete_receipt": fields(binding="binding", tenant="text", source_id="text",
                             expected="u64"),
    "abort_receipt": fields(binding="binding", tenant="text", source_id="text",
                            aborted=ident("root")),
}
RECORDS = {
    "head_data": fields(revision_key=opt("text"), revision_digest=opt("digest"),
                        pipeline=opt("digest"), status=("enum", 4),
                        job_receipt_key=opt("text"), last_batch="u32"),
    "revision_data": fields(source_key="text", uri=opt("text"), text_length="u32",
                            chunk_count="u32", receipt_key="text"),
    "chunk_data": fields(source_key="text", revision_key="text", ordinal="u32",
                         start="u32", end="u32", chunk_digest="digest", pipeline="digest"),
    "entity_data": (),
    "link_data": (),
    "mention_data": fields(source_key="text", revision_key="text", chunk_key="text",
                           surface="text", confidence=opt("f64"), extractor_version="text",
                           attributes="attributes"),
    "relation_data": fields(source_key="text", revision_key="text", chunk_key="text",
                            confidence=opt("f64"), extractor_version="text"),
    "receipt_data": fields(request_digest="digest", plan_digest="slot", batch_count="u32",
                           revision_key="text", outcome=("enum", 4), chunks="u32",
                           entities="u32", relations="u32", mentions="u32"),
    "batch_data": fields(batch_digest="slot", final="bool", created_anchors="u32",
                         reused_anchors="u32", result_shards="u32", ownership_shards="u32"),
    "vector_data": fields(tenant="text", source_key="text", revision_key="text",
                          chunk_key="text", text="bytes", vector=seq("f32")),
    "policy": fields(source_bytes="u32", chunks="u32", entities="u32", relations="u32",
                     mentions="u32", chunk_entities="u32", chunk_relations="u32",
                     decoded_bytes="u32", attribute_bytes="u32", attribute_fields="u32",
                     attribute_string_bytes="u32", frames="u32", operations="u32",
                     prepared_bytes="u64"),
    "entity_rule": fields(rule_id="text", literal="text", kind="text", key="text"),
    "relation_rule": fields(rule_id="text", predicate="text",
                            subject_kind="text", object_kind="text"),
    "pipeline": fields(chunker=("enum", 2), chunk_bytes="u32", overlap="u32",
                       extractor_alias="text", extractor_version="text",
                       entity_rules=seq(record("entity_rule")),
                       relation_rules=seq(record("relation_rule")), normalization="text",
                       embedder_alias="text", model_version="text", dimensions="u32",
                       binding_generation="digest", schema_version="u32",
                       policy=record("policy")),
    "request": fields(source=ident("source"), content_digest="digest", uri=opt("text"),
                      pipeline=record("pipeline"), expected_source="revision_ref",
                      mode=("enum", 2), text="bytes"),
    "entity_observation": fields(local_id="u32", kind="text", key="text", surface="text",
                                 start="u32", end="u32", attributes="attributes",
                                 confidence=opt("f64")),
    "relation_observation": fields(subject_local_id="u32", predicate="text",
                                   object_local_id="u32", start="u32", end="u32",
                                   attributes="attributes", confidence=opt("f64")),
    "accepted_chunk": fields(chunk=ident("chunk"), text="bytes",
                             entities=seq(record("entity_observation")),
                             relations=seq(record("relation_observation")),
                             vector=seq("f32")),
    "captured": fields(object=ident(), expected="revision_ref", properties_digest="digest"),
    "group": fields(ordinal="u32", operations=seq("operation")),
    "batch": fields(root=ident("root"), ordinal="u32", groups=seq(record("group"))),
    "plan": fields(request_digest="digest", captured=seq(record("captured")),
                   accepted=seq(record("accepted_chunk")), batches=seq(record("batch"))),
    "delete_action": fields(root=ident("delete_receipt"), policy_digest="digest",
                            mode=("enum", 2)),
    "abort_action": fields(root=ident("abort_receipt"), policy_digest="digest",
                           mode=("enum", 2)),
    "entry": fields(producing_op="u32", result_ordinal="u32", action=("enum", 3),
                    object_kind=("enum", 3), object_key="text", id="id_ref",
                    revision="revision_ref", collection=opt("u16"), record=opt("u64"),
                    attached_chunk_key=opt("text"), retained="bool"),
}
OPS = {
    0: fields(source=ident("source"), expected="revision_ref"),
    1: fields(edge=ident(), expected="revision_ref"),
    2: fields(chunk=ident("chunk"), collection="u16", record="u64", expected="revision_ref"),
    3: fields(collection="u16", record="u64"),
    4: fields(node=ident(), expected="revision_ref"),
    5: fields(object="object"),
    6: fields(collection="u16", record="u64", data=record("vector_data")),
    7: fields(chunk=ident("chunk"), collection="u16", record="u64"),
    8: fields(object="object"),
    9: fields(shard="shard"),
    10: fields(object="object"),
    11: fields(source=ident("source"), expected="revision_ref", data=record("head_data")),
    12: fields(object="object"),
    13: fields(source=ident("source"), data=record("head_data")),
}
DATA = dict(source="head_data", revision="revision_data", chunk="chunk_data",
            entity="entity_data", has_revision="link_data", has_chunk="link_data",
            mention="mention_data", relation="relation_data", receipt="receipt_data",
            batch_receipt="batch_data", delete_receipt="receipt_data",
            abort_receipt="receipt_data", vector="vector_data")


def uint(value, size):
    need(type(value) is int and 0 <= value < 1 << (size * 8), "integer_range")
    return value.to_bytes(size, "little")


def blob(value):
    return uint(len(value), 4) + value


def text(value):
    need(isinstance(value, str), "text_type")
    try:
        return value.encode("utf-8", "strict")
    except UnicodeError as error:
        raise Invalid("utf8") from error


def strict_utf8(raw):
    try:
        return raw.decode("utf-8", "strict")
    except UnicodeError as error:
        raise Invalid("utf8") from error


def attributes_key(value, masked=False):
    need(isinstance(value, dict) and len(value) <= 16, "attributes")
    raw = b"".join(blob(text(key)) + encode("attribute", value[key], masked)
                   for key in sorted(value, key=lambda key: text(key)))
    need(len(raw) <= 8192, "attribute_bytes")
    return raw


def unhex(value, size=None):
    need(isinstance(value, str) and len(value) % 2 == 0
         and all(char in "0123456789abcdef" for char in value), "hex")
    raw = bytes.fromhex(value)
    need(size is None or len(raw) == size, "digest_width")
    return raw


def exact(value, schema):
    need(isinstance(value, dict) and set(value) == {name for name, _ in schema},
         "fields")


def struct_bytes(schema, value, masked=False):
    exact(value, schema)
    return b"".join(encode(kind, value[name], masked) for name, kind in schema)


def encode(kind, value, masked=False):
    if isinstance(kind, (tuple, list)):
        tag, item = kind
        if tag == "optional":
            return b"\0" if value is None else b"\1" + encode(item, value, masked)
        if tag == "list":
            need(isinstance(value, list), "list")
            return uint(len(value), 4) + b"".join(encode(item, row, masked) for row in value)
        if tag == "map":
            need(isinstance(value, dict), "map")
            return uint(len(value), 4) + b"".join(
                encode("text", key) + encode(item, value[key], masked)
                for key in sorted(value, key=text))
        if tag == "record":
            need(item in RECORDS, "record")
            return blob(named(item, value, masked))
        if tag == "identity":
            raw = identity(value)
            need(item == "any" or value["kind"] == item
                 or (item == "root" and value["kind"] in ROOTS), "identity_role")
            return blob(raw)
        if tag == "enum":
            need(type(value) is int and 0 <= value < item, "enum")
            return uint(value, 1)
        raise Invalid("type")
    if kind in ("u8", "u16", "u32", "u64"):
        return uint(value, int(kind[1:]) // 8)
    if kind == "i64":
        need(type(value) is int and -(1 << 63) <= value < 1 << 63, "integer_range")
        return value.to_bytes(8, "little", signed=True)
    if kind in ("f32", "f64"):
        need(type(value) is float, "float")
        try:
            need(math.isfinite(value), "float")
            if kind == "f32":
                result = struct.pack("<f", value)
                rounded = struct.unpack("<f", result)[0]
                if rounded == 0.0:
                    result = bytes(4)
            else:
                result = struct.pack("<d", 0.0 if value == 0 else value)
        except (OverflowError, struct.error) as error:
            raise Invalid("float") from error
        decoded = struct.unpack("<f" if kind == "f32" else "<d", result)[0]
        need(math.isfinite(decoded) and not (decoded == 0.0 and result[-1] & 0x80),
             "float")
        return result
    if kind == "bool":
        need(type(value) is bool, "boolean")
        return b"\1" if value else b"\0"
    if kind == "text":
        return blob(text(value))
    if kind == "bytes":
        return blob(unhex(value))
    if kind == "digest":
        return unhex(value, 32)
    if kind == "binding":
        exact(value, BINDING)
        for field in ("graph", "collection", "tenant"):
            maximum = 128 if field == "tenant" else 63
            need(1 <= len(text(value[field])) <= maximum and "\0" not in value[field],
                 "binding")
        return struct_bytes(BINDING, value, masked)
    if kind == "attributes":
        return blob(attributes_key(value, masked))
    if kind == "attribute":
        need(isinstance(value, list) and len(value) == 2, "attribute")
        tag, content = value
        need(type(tag) is int and 0 <= tag <= 5, "attribute_tag")
        types = (None, "bool", "i64", "u64", "f64", "text")
        if tag == 0:
            need(content is None, "attribute")
            return b"\0"
        if tag == 5:
            need(len(text(content)) <= 4096, "attribute_string")
        return uint(tag, 1) + encode(types[tag], content, masked)
    if kind in ("slot", "id_ref", "revision_ref"):
        need(isinstance(value, dict) and set(value) == {"tag", "value"}, "reference")
        tag, content = value["tag"], value["value"]
        maximum = 3 if kind in ("slot", "revision_ref") else 2
        need(type(tag) is int and 0 <= tag < maximum, "reference_tag")
        if kind == "slot":
            raw = encode("digest", content)
            if masked and tag != 0:
                raw = bytes(32)
        elif kind == "revision_ref" and tag == 0:
            need(content is None, "reference")
            raw = b""
        elif (kind == "id_ref" and tag == 0) or (kind == "revision_ref" and tag == 1):
            raw = encode("u64", content)
        else:
            need(isinstance(content, list) and len(content) == 2, "reference")
            raw = b"".join(encode("u32", part) for part in content)
        return uint(tag, 1) + raw
    if kind == "operation":
        need(isinstance(value, dict) and set(value) == {"tag", "fields"}, "operation")
        need(type(value["tag"]) is int and value["tag"] in OPS, "operation_tag")
        validate_operation(value)
        return uint(value["tag"], 1) + struct_bytes(OPS[value["tag"]], value["fields"], masked)
    if kind == "object":
        need(isinstance(value, dict) and set(value) == {"identity", "data"}, "object")
        role = value["identity"]["kind"]
        need(role in DATA, "object_role")
        return encode(ident(), value["identity"], masked) + encode(
            record(DATA[role]), value["data"], masked)
    if kind == "shard":
        return blob(shard_binary(value, masked))
    raise Invalid("type")


def identity(value):
    need(isinstance(value, dict) and set(value) == {"kind", "fields"}, "identity")
    role = value["kind"]
    need(role in REGISTRY, "identity_kind")
    values = value["fields"]
    need(isinstance(values, dict), "identity_fields")
    if "binding" in values and "tenant" in values:
        need(values["binding"]["tenant"] == values["tenant"], "tenant_mismatch")
    if role == "entity":
        need(is_identifier(values["kind"]) and len(text(values["normalized_key"])) in range(1, 257),
             "entity_identity")
        need(ascii_key(values["normalized_key"]) == values["normalized_key"],
             "normalized_key")
    if role in ("source", "receipt", "delete_receipt", "abort_receipt"):
        raw_source_id = text(values["source_id"])
        need(1 <= len(raw_source_id) <= 256 and "\0" not in values["source_id"],
             "source_id")
    if role == "chunk":
        need(values["start"] < values["end"], "chunk_span")
    if role in ("mention", "relation"):
        need(values["start"] < values["end"], "observation_span")
    if role == "relation":
        need(is_identifier(values["predicate"]), "predicate")
    if role == "abort_receipt":
        need(values["aborted"]["kind"] in ("receipt", "delete_receipt"), "abort_of_abort")
    return b"\1" + uint(REGISTRY[role][0], 1) + struct_bytes(IDENTITIES[role], values)


def is_identifier(value):
    raw = text(value)
    return 1 <= len(raw) <= 63 and re.fullmatch(rb"[A-Za-z0-9_]+", raw) is not None


def pipeline_label(value):
    raw = text(value)
    return (1 <= len(raw) <= 256
            and re.fullmatch(rb"[A-Za-z0-9._-]+", raw) is not None)


def extractor_label(value):
    return pipeline_label(value) and value.lower() not in ("latest", "default")


def ascii_key(value):
    raw = text(value).strip(b" \t\r\n")
    raw = re.sub(rb"[ \t\r\n]+", b" ", raw)
    return raw.translate(bytes.maketrans(b"ABCDEFGHIJKLMNOPQRSTUVWXYZ",
                                         b"abcdefghijklmnopqrstuvwxyz")).decode("utf-8")


def identity_order(value):
    role = value["kind"]
    schema = IDENTITIES[role]
    parts = []
    for (name, kind) in schema:
        field = value["fields"][name]
        if isinstance(kind, (tuple, list)) and kind[0] == "identity":
            part = identity_order(field)
        elif kind == "binding":
            part = tuple(text(field[key]) for key in ("graph", "collection", "tenant")) + (
                unhex(field["generation"], 32),)
        elif kind == "attributes":
            part = attributes_key(field)
        elif kind == "digest":
            part = unhex(field, 32)
        elif kind == "text":
            part = text(field)
        else:
            part = field
        parts.append(part)
    return (REGISTRY[role][0], tuple(parts))


def graph_key_order(value):
    return text(graph_key(value))


def validate_operation(value):
    tag, op = value["tag"], value["fields"]
    if tag == 5:
        need(op["object"]["identity"]["kind"] in ("revision", "entity", "chunk"),
             "operation_object_role")
    elif tag == 8:
        need(op["object"]["identity"]["kind"] in
             ("has_revision", "has_chunk", "mention", "relation"), "operation_object_role")
    elif tag == 10:
        need(op["object"]["identity"]["kind"] == "batch_receipt", "operation_object_role")
        need(op["object"]["data"]["batch_digest"]["tag"] == 2, "batch_slot_kind")
    elif tag == 12:
        need(op["object"]["identity"]["kind"] in
             ("receipt", "delete_receipt", "abort_receipt"), "operation_object_role")
        need(op["object"]["data"]["plan_digest"]["tag"] == 1, "plan_slot_kind")
    elif tag == 13:
        need(op["source"]["kind"] == "source", "operation_object_role")
    elif tag == 11:
        need(op["expected"]["tag"] in (1, 2), "head_patch_expected")


def validate_record(name, value):
    if name == "pipeline":
        need(value["normalization"] == "ascii-key-v1", "normalization")
        need(value["schema_version"] == 1 and value["dimensions"] > 0, "pipeline_version")
        need(extractor_label(value["extractor_version"]), "extractor_version")
        for field in ("extractor_alias", "embedder_alias"):
            need(pipeline_label(value[field]), "pipeline_label")
        need(extractor_label(value["model_version"]), "pipeline_label")
        for field in ("entity_rules", "relation_rules"):
            ids = [text(rule["rule_id"]) for rule in value[field]]
            need(ids == sorted(ids) and len(ids) == len(set(ids)), "rule_order")
    elif name in ("mention_data", "relation_data"):
        need(extractor_label(value["extractor_version"]), "extractor_version")
    elif name == "entity_rule":
        need(is_identifier(value["kind"]), "entity_kind")
        need(value["key"] and ascii_key(value["key"]) == value["key"], "normalized_key")
    elif name == "relation_rule":
        need(is_identifier(value["predicate"])
             and is_identifier(value["subject_kind"])
             and is_identifier(value["object_kind"]), "relation_rule_identifier")
    elif name == "request":
        need(value["expected_source"]["tag"] in (0, 1), "request_revision_ref")
        content = unhex(value["text"])
        strict_utf8(content)
        need(value["content_digest"] == hashlib.sha256(content).hexdigest(),
             "request_content_digest")
        need(value["pipeline"]["binding_generation"]
             == value["source"]["fields"]["binding"]["generation"], "binding_generation")
        if value["uri"] is not None:
            uri = text(value["uri"])
            need(len(uri) <= 2048 and "\0" not in value["uri"], "uri")
    elif name == "revision_data":
        if value["uri"] is not None:
            uri = text(value["uri"])
            need(len(uri) <= 2048 and "\0" not in value["uri"], "uri")
    elif name == "head_data":
        need(value["revision_key"] is None or "\0" not in value["revision_key"],
             "revision_key")
        need(value["job_receipt_key"] is None or "\0" not in value["job_receipt_key"],
             "job_receipt_key")
    elif name == "entity_observation":
        need(is_identifier(value["kind"]), "entity_kind")
        need(value["key"] and ascii_key(value["key"]) == value["key"], "normalized_key")
        need(value["start"] < value["end"], "observation_span")
        need(value["confidence"] is None or
             (0.0 <= value["confidence"] <= 1.0), "confidence")
    elif name == "relation_observation":
        need(is_identifier(value["predicate"]), "predicate")
        need(value["start"] < value["end"], "observation_span")
        need(value["confidence"] is None or
             (0.0 <= value["confidence"] <= 1.0), "confidence")
    elif name == "accepted_chunk":
        chunk_text = unhex(value["text"])
        strict_utf8(chunk_text)
        ids = [row["local_id"] for row in value["entities"]]
        need(len(ids) == len(set(ids)), "duplicate_local_id")
        for row in value["entities"]:
            validate_record("entity_observation", row)
            span = chunk_text[row["start"]:row["end"]]
            need(row["end"] <= len(chunk_text) and span == text(row["surface"]),
                 "observation_surface")
            decode_utf8_boundaries(chunk_text, row["start"], row["end"])
        for row in value["relations"]:
            validate_record("relation_observation", row)
            need(row["subject_local_id"] in ids and row["object_local_id"] in ids,
                 "relation_local_id")
            need(row["end"] <= len(chunk_text), "observation_span")
            decode_utf8_boundaries(chunk_text, row["start"], row["end"])
        need(all(type(v) is float for v in value["vector"]), "float")
    elif name == "receipt_data":
        need(value["plan_digest"]["tag"] == 1, "plan_slot_kind")
    elif name == "batch_data":
        need(value["batch_digest"]["tag"] == 2, "batch_slot_kind")
    elif name == "captured":
        need(value["expected"]["tag"] in (0, 1), "captured_revision_ref")
    elif name == "plan":
        rows = [identity_order(row["object"]) for row in value["captured"]]
        need(rows == sorted(rows) and len(rows) == len(set(rows)), "captured_order")
        ordinals = [row["chunk"]["fields"]["ordinal"] for row in value["accepted"]]
        need(strict_order(ordinals),
             "accepted_chunk_order")
        validate_plan_order(value)
    elif name == "vector_data":
        need(all("\0" not in value[field] for field in
                 ("tenant", "source_key", "revision_key", "chunk_key")), "vector_metadata")
        strict_utf8(unhex(value["text"]))
        need(bool(value["vector"]), "vector_dimensions")
        need(all(type(item) is float for item in value["vector"]), "float")
    elif name == "delete_action":
        need(value["root"]["kind"] == "delete_receipt", "action_root")
    elif name == "abort_action":
        need(value["root"]["kind"] == "abort_receipt", "action_root")
    elif name == "entry":
        validate_entry(value)


def decode_utf8_boundaries(raw, start, end):
    need(0 <= start < end <= len(raw), "observation_span")
    try:
        raw[:start].decode("utf-8", "strict")
        raw[:end].decode("utf-8", "strict")
    except UnicodeError as error:
        raise Invalid("observation_utf8_boundary") from error


def strict_order(values):
    return values == sorted(values) and len(values) == len(set(values))


def add_chunk_group_member(group_members, creation_groups, chunk, tag,
                           batch_ordinal, group_index):
    chunk_id = identity(chunk)
    group_members.append((tag, chunk_id))
    if tag in (5, 6, 7, 8):
        location = (batch_ordinal, group_index)
        previous = creation_groups.setdefault(chunk_id, location)
        need(previous == location, "chunk_creation_split")


def validate_group_chunk_membership(members):
    creation_ids = {chunk_id for tag, chunk_id in members if tag in (5, 6, 7, 8)}
    if creation_ids:
        need(len({chunk_id for _, chunk_id in members}) <= 1,
             "group_chunk_membership")
    else:
        cleanup_ids = {chunk_id for tag, chunk_id in members if tag in (2, 3, 4)}
        need(len(cleanup_ids) <= 1, "group_chunk_membership")


def validate_plan_order(plan):
    captured = {identity(row["object"]) for row in plan["captured"]}
    known = {raw for raw in captured
             if decode_identity(raw)["kind"] in ("source", "revision", "chunk", "entity")}
    plan_chunk_ordinals = []
    head_creates = 0
    chunk_creation_groups = {}
    for batch in plan["batches"]:
        groups = batch["groups"]
        need(groups and [group["ordinal"] for group in groups]
             == list(range(len(groups))), "group_order")
        need(all(group["operations"] for group in groups), "empty_group")
        ops = [op for group in batch["groups"] for op in group["operations"]]
        if not ops:
            raise Invalid("empty_batch")
        op_bytes = [encode("operation", op) for op in ops]
        need(len(op_bytes) == len(set(op_bytes)), "duplicate_operation")
        edge_ids = [identity(op["fields"]["edge"])
                    if op["tag"] == 1 else identity(op["fields"]["object"]["identity"])
                    for op in ops if op["tag"] in (1, 8)]
        need(len(edge_ids) == len(set(edge_ids)), "duplicate_edge")
        validate_batch_start(ops, batch["ordinal"], known)
        chunk_order = []
        previous_group = None
        previous_phase = -1
        anchor_keys, provenance_order, relation_order = [], [], []
        group_chunk_identities = []
        group_indexes = [group_index for group_index, group in enumerate(batch["groups"])
                         for _ in group["operations"]]
        detached_chunks = {}
        for group_index, group in enumerate(batch["groups"]):
            for operation in group["operations"]:
                if operation["tag"] == 2:
                    op_fields = operation["fields"]
                    detached_chunks[(group_index, op_fields["collection"], op_fields["record"])] = (
                        identity(op_fields["chunk"]))
        for index, operation in enumerate(ops):
            group_index = group_indexes[index]
            if group_index != previous_group:
                need(strict_order(anchor_keys), "node_key_order")
                need(strict_order(provenance_order), "provenance_identity_order")
                need(strict_order(relation_order), "relation_identity_order")
                validate_group_chunk_membership(group_chunk_identities)
                previous_phase = -1
                anchor_keys, provenance_order, relation_order = [], [], []
                group_chunk_identities = []
                previous_group = group_index
            tag, fields_ = operation["tag"], operation["fields"]
            if tag == 13:
                head_creates += 1
                need(head_creates == 1, "head_create_count")
                need(batch["ordinal"] == 1 and index == 0, "head_create_first")
                known.add(identity(fields_["source"]))
            phase = operation_phase(operation)
            need(phase >= previous_phase, "operation_order")
            previous_phase = phase
            if tag == 5:
                obj = fields_["object"]
                role, data = obj["identity"]["kind"], obj["identity"]
                if role == "revision":
                    need(identity(data["fields"]["source"]) in known, "revision_endpoint")
                    anchor_keys.append(graph_key_order(data))
                elif role == "chunk":
                    need(identity(data["fields"]["revision"]) in known, "chunk_endpoint")
                    ordinal = data["fields"]["ordinal"]
                    chunk_order.append(ordinal)
                    plan_chunk_ordinals.append(ordinal)
                    add_chunk_group_member(
                        group_chunk_identities, chunk_creation_groups, data, tag,
                        batch["ordinal"], group_index)
                elif role == "entity":
                    anchor_keys.append(graph_key_order(data))
                known.add(identity(data))
            elif tag == 8:
                ident_ = fields_["object"]["identity"]
                validate_edge_order(ident_, known)
                edge_fields = ident_["fields"]
                if ident_["kind"] in ("has_chunk", "mention", "relation"):
                    chunk = edge_fields["chunk"]
                    add_chunk_group_member(
                        group_chunk_identities, chunk_creation_groups, chunk, tag,
                        batch["ordinal"], group_index)
                if ident_["kind"] == "relation":
                    relation_order.append(identity_order(ident_))
                else:
                    provenance_order.append(identity_order(ident_))
            elif tag == 1:
                edge = fields_["edge"]
                edge_role = edge["kind"]
                if edge_role in ("has_chunk", "mention", "relation"):
                    add_chunk_group_member(
                        group_chunk_identities, chunk_creation_groups,
                        edge["fields"]["chunk"], tag, batch["ordinal"], group_index)
            elif tag == 4:
                node = fields_["node"]
                if node["kind"] == "chunk":
                    add_chunk_group_member(
                        group_chunk_identities, chunk_creation_groups, node, tag,
                        batch["ordinal"], group_index)
            elif tag == 2:
                add_chunk_group_member(
                    group_chunk_identities, chunk_creation_groups, fields_["chunk"], tag,
                    batch["ordinal"], group_index)
            elif tag == 3:
                key = (group_index, fields_["collection"], fields_["record"])
                need(key in detached_chunks, "group_chunk_membership")
                add_chunk_group_member(
                    group_chunk_identities, chunk_creation_groups,
                    decode_identity(detached_chunks[key]), tag,
                    batch["ordinal"], group_index)
            elif tag == 7:
                add_chunk_group_member(
                    group_chunk_identities, chunk_creation_groups, fields_["chunk"], tag,
                    batch["ordinal"], group_index)
                detached_chunks[(group_index, fields_["collection"], fields_["record"])] = (
                    identity(fields_["chunk"]))
            elif tag == 6:
                chunk_key = fields_["data"]["chunk_key"]
                matching = [decode_identity(raw) for raw in known
                            if decode_identity(raw)["kind"] == "chunk"
                            and graph_key(decode_identity(raw)) == chunk_key]
                need(len(matching) == 1, "group_chunk_membership")
                add_chunk_group_member(
                    group_chunk_identities, chunk_creation_groups, matching[0], tag,
                    batch["ordinal"], group_index)
            elif tag == 11:
                need(identity(fields_["source"]) in known, "head_patch_source")
            if tag == 12:
                need(index == len(ops) - 1 and batch["ordinal"] == 1, "root_receipt_last")
        need(strict_order(anchor_keys), "node_key_order")
        need(strict_order(provenance_order), "provenance_identity_order")
        need(strict_order(relation_order), "relation_identity_order")
        validate_group_chunk_membership(group_chunk_identities)
        need(strict_order(chunk_order), "chunk_ordinal_order")
    ordinals = [batch["ordinal"] for batch in plan["batches"]]
    need(ordinals == list(range(1, len(ordinals) + 1)), "batch_order")
    need(strict_order(plan_chunk_ordinals), "chunk_ordinal_order")


def operation_phase(operation):
    tag = operation["tag"]
    if tag == 13:
        return -1
    if tag == 0:
        return 0
    if tag == 1:
        return 1
    if tag == 2:
        return 2
    if tag == 3:
        return 3
    if tag == 4:
        return 4
    if tag == 5:
        role = operation["fields"]["object"]["identity"]["kind"]
        return 5 if role in ("revision", "entity") else 6
    if tag == 6:
        return 7
    if tag == 7:
        return 8
    if tag == 8:
        return (9 if operation["fields"]["object"]["identity"]["kind"]
                in ("has_revision", "has_chunk", "mention") else 10)
    if tag == 9:
        return 11
    if tag == 10:
        return 12
    if tag == 11:
        return 13
    if tag == 12:
        return 14
    raise Invalid("operation_tag")


def validate_batch_start(ops, ordinal, known):
    first = ops[0]
    if ordinal == 1 and first["tag"] == 13:
        return
    need(first["tag"] == 0, "batch_guard_first")
    need(identity(first["fields"]["source"]) in known, "guard_source")


def validate_edge_order(edge, known):
    role, links = edge["kind"], edge["fields"]
    endpoints = {
        "has_revision": ("source", "revision"),
        "has_chunk": ("revision", "chunk"),
        "mention": ("chunk", "entity"),
        "relation": ("revision", "chunk", "subject", "object"),
    }[role]
    for endpoint in endpoints:
        need(identity(links[endpoint]) in known, "edge_endpoint")


def named(name, value, masked=False):
    need(name in RECORDS, "record")
    validate_record(name, value)
    return b"\1" + struct_bytes(RECORDS[name], value, masked)


def framed_hash(domain, payload):
    need(domain in domains(), "domain")
    return hashlib.sha256(b"\1" + blob(domain.encode("ascii")) + blob(payload)).digest()


def domains():
    return ["vlite.g7.v1." + row[1] for row in REGISTRY.values()] + [
        "vlite.g7.v1." + suffix for suffix in EXTRA_DOMAINS]


def graph_key(value):
    role = value["kind"]
    need(role in REGISTRY and REGISTRY[role][2] is not None, "key_role")
    return "g7_" + REGISTRY[role][2] + "_" + framed_hash(
        "vlite.g7.v1." + REGISTRY[role][1], identity(value)).hex()


class Reader:
    def __init__(self, raw):
        self.raw, self.at = raw, 0

    def take(self, count):
        need(count >= 0 and self.at + count <= len(self.raw), "truncated")
        result = self.raw[self.at:self.at + count]
        self.at += count
        return result

    def number(self, size):
        return int.from_bytes(self.take(size), "little")

    def blob(self):
        return self.take(self.number(4))


def decode(kind, reader):
    if isinstance(kind, (tuple, list)):
        tag, item = kind
        if tag == "optional":
            presence = reader.number(1)
            need(presence in (0, 1), "presence")
            return None if presence == 0 else decode(item, reader)
        if tag == "list":
            count = reader.number(4)
            need(count <= len(reader.raw), "list_count")
            return [decode(item, reader) for _ in range(count)]
        if tag == "map":
            count, result, previous = reader.number(4), {}, None
            need(count <= len(reader.raw), "map_count")
            for _ in range(count):
                key = decode("text", reader)
                raw = text(key)
                need(previous is None or previous < raw, "map_order")
                result[key] = decode(item, reader)
                previous = raw
            return result
        if tag == "record":
            nested = Reader(reader.blob())
            need(nested.number(1) == 1, "version")
            result = decode_struct(RECORDS[item], nested)
            need(nested.at == len(nested.raw), "trailing")
            named(item, result)
            return result
        if tag == "identity":
            result = decode_identity(reader.blob())
            need(item == "any" or result["kind"] == item
                 or (item == "root" and result["kind"] in ROOTS), "identity_role")
            return result
        if tag == "enum":
            value = reader.number(1)
            need(value < item, "enum")
            return value
    if kind in ("u8", "u16", "u32", "u64"):
        return reader.number(int(kind[1:]) // 8)
    if kind == "i64":
        return int.from_bytes(reader.take(8), "little", signed=True)
    if kind in ("f32", "f64"):
        raw = reader.take(4 if kind == "f32" else 8)
        value = struct.unpack("<f" if kind == "f32" else "<d", raw)[0]
        need(math.isfinite(value) and not (value == 0 and raw[-1] & 128), "float")
        return value
    if kind == "bool":
        raw = reader.number(1)
        need(raw in (0, 1), "boolean")
        return bool(raw)
    if kind == "text":
        try:
            return reader.blob().decode("utf-8", "strict")
        except UnicodeError as error:
            raise Invalid("utf8") from error
    if kind == "bytes":
        return reader.blob().hex()
    if kind == "digest":
        return reader.take(32).hex()
    if kind == "binding":
        return decode_struct(BINDING, reader)
    if kind == "attribute":
        tag = reader.number(1)
        need(tag <= 5, "attribute_tag")
        return [tag, None if tag == 0 else decode(
            (None, "bool", "i64", "u64", "f64", "text")[tag], reader)]
    if kind == "attributes":
        nested, result, previous = Reader(reader.blob()), {}, None
        while nested.at < len(nested.raw):
            key = decode("text", nested)
            raw_key = text(key)
            need(previous is None or previous < raw_key, "map_order")
            result[key] = decode("attribute", nested)
            previous = raw_key
        return result
    if kind in ("slot", "id_ref", "revision_ref"):
        tag = reader.number(1)
        need(tag < (2 if kind == "id_ref" else 3), "reference_tag")
        if kind == "slot":
            value = decode("digest", reader)
        elif kind == "revision_ref" and tag == 0:
            value = None
        elif (kind == "id_ref" and tag == 0) or (kind == "revision_ref" and tag == 1):
            value = decode("u64", reader)
        else:
            value = [decode("u32", reader), decode("u32", reader)]
        return {"tag": tag, "value": value}
    if kind == "operation":
        tag = reader.number(1)
        need(tag in OPS, "operation_tag")
        value = {"tag": tag, "fields": decode_struct(OPS[tag], reader)}
        validate_operation(value)
        return value
    if kind == "object":
        value = decode(ident(), reader)
        need(value["kind"] in DATA, "object_role")
        return {"identity": value, "data": decode(record(DATA[value["kind"]]), reader)}
    if kind == "shard":
        return decode_shard(reader.blob())
    raise Invalid("type")


def decode_struct(schema, reader):
    return {name: decode(kind, reader) for name, kind in schema}


def decode_identity(raw):
    reader = Reader(raw)
    need(reader.number(1) == 1, "version")
    tag = reader.number(1)
    roles = [role for role, row in REGISTRY.items() if row[0] == tag]
    need(len(roles) == 1, "identity_kind")
    role = roles[0]
    value = {"kind": role, "fields": decode_struct(IDENTITIES[role], reader)}
    need(reader.at == len(raw) and identity(value) == raw, "noncanonical")
    return value


def entry_bytes(entry, masked=False):
    validate_entry(entry)
    return struct_bytes(RECORDS["entry"], entry, masked)


def validate_entry(entry):
    exact(entry, RECORDS["entry"])
    need(entry["object_kind"] in (0, 1, 2), "object_kind")
    if entry["object_kind"] == 2:
        need(entry["object_key"] == "" and entry["collection"] is not None
             and entry["record"] is not None and entry["attached_chunk_key"] is not None,
             "entry_vector_fields")
    else:
        need(bool(entry["object_key"]) and entry["collection"] is None
             and entry["record"] is None and entry["attached_chunk_key"] is None,
             "entry_graph_fields")


def entry_order(entry):
    return (entry["producing_op"], entry["result_ordinal"], entry["action"],
            entry["object_kind"], text(entry["object_key"]))


SHARD_HEADER = fields(channel=("enum", 2), root_key="text", batch="u32",
                      group="u32", index="u32")


def shard_binary(value, masked=False):
    need(isinstance(value, dict) and set(value) == {
        "channel", "root_key", "batch", "group", "index", "entries"}, "shard_fields")
    entries = value["entries"]
    need(isinstance(entries, list) and entries, "shard_empty")
    order = [entry_order(entry) for entry in entries]
    need(order == sorted(order) and len(set(order)) == len(order), "entry_order")
    for entry in entries:
        validate_entry(entry)
    header = {name: value[name] for name, _ in SHARD_HEADER}
    raw = b"\1" + struct_bytes(SHARD_HEADER, header, masked) + uint(len(entries), 4)
    raw += b"".join(entry_bytes(entry, masked) for entry in entries)
    need(len(raw) <= 3072, "shard_capacity")
    return raw


def decode_shard(raw):
    reader = Reader(raw)
    need(reader.number(1) == 1, "version")
    value = decode_struct(SHARD_HEADER, reader)
    count = reader.number(4)
    need(count <= len(raw), "entry_count")
    value["entries"] = [decode_struct(RECORDS["entry"], reader) for _ in range(count)]
    need(reader.at == len(raw) and shard_binary(value) == raw, "noncanonical")
    return value


def b64(raw):
    return base64.b64encode(raw).decode("ascii")


def unb64(value):
    need(isinstance(value, str), "base64")
    try:
        raw = base64.b64decode(value, validate=True)
    except (ValueError, UnicodeError) as error:
        raise Invalid("base64") from error
    need(b64(raw) == value, "base64_canonical")
    return raw


def split_shards(header, entries):
    exact(header, SHARD_HEADER)
    need(header["index"] == 0, "shard_index")
    output, current = [], []
    for entry in entries:
        candidate = dict(header, index=len(output), entries=current + [entry])
        try:
            shard_binary(candidate)
        except Invalid as error:
            if str(error) != "shard_capacity" or not current:
                raise
            output.append(dict(header, index=len(output), entries=current))
            current = [entry]
            shard_binary(dict(header, index=len(output), entries=current))
        else:
            current.append(entry)
    if current:
        output.append(dict(header, index=len(output), entries=current))
    return output


def validate_shards(header, shards):
    entries = [entry for shard in shards for entry in shard["entries"]]
    canonical = split_shards(header, entries)
    need([shard_binary(shard) for shard in shards]
         == [shard_binary(shard) for shard in canonical], "shard_backrefs_or_split")


def physical(identity_value, data):
    role = identity_value["kind"]
    need(role in DATA and role != "vector", "stored_role")
    result = {"g7_v": 1, "g7_kind": REGISTRY[role][2]}
    for prefix, raw in (("i", identity(identity_value)), ("d", named(DATA[role], data))):
        parts = split_fragments(raw)
        need(len(parts) <= 64, "fragments")
        for index, part in enumerate(parts):
            result[f"{prefix}{index:02}"] = b64(part)
    for field in ("source_key", "revision_key", "chunk_key"):
        if field in data and data[field] is not None:
            result[field] = data[field]
    result.update(readable_projection(identity_value, data))
    metadata_bytes(result)
    return result


def split_fragments(raw):
    return [raw[index:index + 3072] for index in range(0, len(raw), 3072)] or [b""]


def readable_projection(identity_value, data):
    role, fields_ = identity_value["kind"], identity_value["fields"]
    projection = {}
    if role == "source":
        projection["status"] = data["status"]
        projection["last_batch"] = data["last_batch"]
    elif role == "revision":
        if data["uri"] is not None:
            projection["uri"] = data["uri"]
        projection.update(text_length=data["text_length"], chunk_count=data["chunk_count"],
                          content_digest=fields_["content"])
    elif role == "chunk":
        projection.update(ordinal=data["ordinal"], start_byte=data["start"],
                          end_byte=data["end"], chunk_digest=data["chunk_digest"])
    elif role == "entity":
        projection.update(entity_kind=fields_["kind"], normalized_key=fields_["normalized_key"])
    elif role in ("mention", "relation"):
        projection.update(span_start=fields_["start"], span_end=fields_["end"])
        if data["confidence"] is not None:
            projection["confidence"] = (0.0 if data["confidence"] == 0.0
                                       else data["confidence"])
        projection["extractor_version"] = data["extractor_version"]
        if role == "mention" and len(text(data["surface"])) <= 4096:
            projection["surface"] = data["surface"]
    return projection


def physical_vector(data):
    validate_record("vector_data", data)
    return {
        "metadata": {
            "g7_v": 1,
            "tenant": data["tenant"],
            "source_key": data["source_key"],
            "revision_key": data["revision_key"],
            "chunk_key": data["chunk_key"],
        },
        "payload": unhex(data["text"]),
        "vector": b"".join(encode("f32", value) for value in data["vector"]),
        "expiry": None,
    }


def decode_physical(expected_identity, properties):
    metadata_bytes(properties)
    recovered = {}
    for prefix in ("i", "d"):
        keys = sorted(key for key in properties
                      if len(key) == 3 and key[0] == prefix and key[1:].isdigit())
        need(keys and keys == [f"{prefix}{index:02}" for index in range(len(keys))],
             "fragment_indexes")
        chunks = [unb64(properties[key]) for key in keys]
        need(all(len(chunk) == 3072 for chunk in chunks[:-1]), "fragment_split")
        recovered[prefix] = b"".join(chunks)
    stored_identity = decode_identity(recovered["i"])
    need(identity(stored_identity) == identity(expected_identity), "identity_conflict")
    role = stored_identity["kind"]
    need(role in DATA and role != "vector", "stored_role")
    reader = Reader(recovered["d"])
    need(reader.number(1) == 1, "version")
    data = decode_struct(RECORDS[DATA[role]], reader)
    need(reader.at == len(reader.raw), "noncanonical")
    need(metadata_binary(physical(stored_identity, data)) == metadata_binary(properties),
         "evidence_conflict")
    return data


def metadata_bytes(properties):
    return len(metadata_binary(properties))


def metadata_binary(properties):
    need(isinstance(properties, dict) and len(properties) <= 64, "property_fields")
    raw = uint(len(properties), 4)
    for key in sorted(properties, key=text):
        value = properties[key]
        length = len(text(key))
        need(1 <= length <= 256, "property_name")
        raw += uint(length, 2) + text(key)
        if isinstance(value, str):
            size = len(text(value))
            need(size <= 4096, "property_string")
            raw += b"\1" + blob(text(value))
        elif type(value) is int:
            need(-(1 << 63) <= value < 1 << 63, "property_integer")
            raw += b"\2" + value.to_bytes(8, "little", signed=True)
        elif type(value) is float:
            raw += b"\3" + encode("f64", value)
        else:
            raise Invalid("property_type")
    need(len(raw) <= 65536, "property_bytes")
    return raw


def lookup(expected, stored, forced_digest=None):
    expected_bytes, stored_bytes = identity(expected), identity(stored)
    digest = forced_digest or hashlib.sha256(expected_bytes).digest()
    other_digest = forced_digest or hashlib.sha256(stored_bytes).digest()
    need(digest != other_digest or expected_bytes == stored_bytes, "identity_conflict")
    return expected_bytes == stored_bytes


def digest_record(name, value, execution=None):
    # Execution is a separate envelope, never a set of keys filtered out of value.
    return framed_hash("vlite.g7.v1." + name.replace("_", "-"),
                       named(name, value, masked=True)).hex()


def frame_size(body_bytes, encrypted=False):
    need(type(body_bytes) is int and 0 <= body_bytes <= 16777216, "body_limit")
    return body_bytes + (80 if encrypted else 52)


def graph_body_size(payload_lengths):
    need(all(type(size) is int and size >= 0 for size in payload_lengths),
         "payload_length")
    need(len(payload_lengths) <= 4096, "entry_limit")
    body = 4 + sum(8 + size for size in payload_lengths)
    frame_size(body)
    return body


def rejects(action, reason, name):
    try:
        action()
    except Invalid as error:
        need(str(error) == reason, name + ":" + str(error))
    else:
        raise Invalid(name + ":accepted")


def stored_checks(document):
    """Named cross-record vectors with expectations fixed by field arithmetic."""
    templates = document["templates"]
    for template in document["vectors"]:
        vector = expand(template, templates)
        if vector["category"] == "record":
            raw = vector_bytes(vector)
            reader = Reader(raw[1:])
            need(raw[0] == 1, "version")
            value = decode_struct(RECORDS[vector["type"]], reader)
            need(reader.at == len(reader.raw)
                 and named(vector["type"], value) == raw, "record-roundtrip:" + vector["name"])
    role_samples = {}
    for template in document["vectors"]:
        row = expand(template, templates)
        if row["category"] == "record":
            role_samples[row["type"]] = row["input"]
    for template in document["vectors"]:
        row = expand(template, templates)
        if row["category"] != "identity":
            continue
        role = row["input"]["kind"]
        if role in DATA and role != "vector":
            data = role_samples[DATA[role]]
            props = physical(row["input"], data)
            need(decode_physical(row["input"], props) == data,
                 "stored-decoder:" + role)
            for prefix, expected in (("i", identity(row["input"])),
                                     ("d", named(DATA[role], data))):
                recovered = b"".join(unb64(props[key]) for key in sorted(props)
                                     if key.startswith(prefix) and key[1:].isdigit())
                need(recovered == expected, "stored-roundtrip:" + role)
            need(metadata_bytes(props) <= 65536, "stored-capacity:" + role)
    sample = expand(templates["source"], templates)
    need(physical(sample, expand(templates["head_data"], templates)) == {
        "g7_v": 1, "g7_kind": "src",
        "i00": b64(expected_bytes(document["atoms"]["source"], document["atoms"])),
        "d00": "AQAAAAAAAAAAAA==", "status": 0, "last_batch": 0},
         "stored-head-properties")
    need(metadata_binary({"z": 1, "a": "x"}).hex()
         == "0200000001006101010000007801007a020100000000000000",
         "metadata-name-u16-type-order")
    need(metadata_bytes(physical(sample, expand(templates["head_data"], templates))) == 190,
         "head-metadata-size-190")
    props = physical(sample, expand(templates["head_data"], templates))
    rejects(lambda: decode_physical(sample, dict(props, unexpected="x")),
            "evidence_conflict", "stored-unknown-property")
    broken = dict(props)
    broken["i01"] = broken.pop("i00")
    rejects(lambda: decode_physical(sample, broken), "fragment_indexes",
            "stored-missing-first-fragment")
    rejects(lambda: decode_physical(sample, dict(props, g7_kind="ent")),
            "evidence_conflict", "stored-wrong-role")
    rejects(lambda: metadata_bytes({"u": 1 << 63}), "property_integer", "no-native-u64")
    rejects(lambda: metadata_bytes({"n": None}), "property_type", "no-native-null")
    need(metadata_bytes({"s": "s" * 4096}) == 4108, "C9-string-4096")
    rejects(lambda: metadata_bytes({"s": "s" * 4097}), "property_string",
            "C9-string-4097")
    exact_map = {f"a{index:02}": "x" * (4096 if index < 15 else 3932)
                 for index in range(16)}
    need(metadata_bytes(exact_map) == 65536, "C9-metadata-65536")
    exact_map["a15"] += "x"
    rejects(lambda: metadata_bytes(exact_map), "property_bytes", "C9-metadata-65537")
    need(metadata_bytes({f"a{index:02}": "" for index in range(64)}) == 644,
         "C9-fields-64")
    rejects(lambda: metadata_bytes({f"a{index:02}": "" for index in range(65)}),
            "property_fields", "C9-fields-65")
    binding = dict(graph="g" * 63, collection="c" * 63, tenant="t" * 128,
                   generation="00" * 32)
    source = {"kind": "source", "fields": dict(binding=binding, tenant="t" * 128,
                                             source_id="s" * 256)}
    revision = {"kind": "revision", "fields": dict(source=source, content="00" * 32,
                                                  pipeline="00" * 32)}
    chunk = {"kind": "chunk", "fields": dict(revision=revision, ordinal=0,
                                            start=0, end=16384)}
    entity = {"kind": "entity", "fields": dict(binding=binding, tenant="t" * 128,
                                             kind="A" * 63, normalized_key="k" * 256)}
    attrs = {"k" * 4096: [5, "v" * 4087]}
    need(len(encode("attributes", attrs)) == 8196, "attribute-8192-plus-frame")
    rejects(lambda: encode("attributes", {"k" * 4096: [5, "v" * 4088]}),
            "attribute_bytes", "attributes-8193")
    mention = {"kind": "mention", "fields": dict(chunk=chunk, entity=entity,
                                                start=0, end=16384)}
    evidence = dict(source_key="g7_src_" + "0" * 64, revision_key="g7_rev_" + "0" * 64,
                    chunk_key="g7_chk_" + "0" * 64, surface="s" * 16384, confidence=1.0,
                    extractor_version="v" * 256, attributes=attrs)
    need(len(identity(source)) == 692 and len(identity(revision)) == 762
         and len(identity(chunk)) == 780 and len(identity(entity)) == 759,
         "max-identity-arithmetic")
    need(len(identity(mention)) == 1557
         and len(named("mention_data", evidence)) == 25079
         and len(physical(mention, evidence)) == 19
         and metadata_bytes(physical(mention, evidence)) == 36258,
         "max-mention-C9-projected")
    relation = {"kind": "relation", "fields": dict(
        revision=revision, chunk=chunk, subject=entity, predicate="P" * 63,
        object=entity, start=0, end=16384, attributes=attrs)}
    need(len(identity(relation)) == 11349, "max-relation-identity-11349")
    relation_data = {key: value for key, value in evidence.items()
                     if key not in ("surface", "attributes")}
    relation_bytes = metadata_bytes(physical(relation, relation_data))
    need(relation_bytes < 65536, "max-relation-C9")
    vector_data = {"tenant": "t" * 128, "source_key": "g7_src_" + "0" * 64,
                   "revision_key": "g7_rev_" + "0" * 64,
                   "chunk_key": "g7_chk_" + "0" * 64, "text": "61",
                   "vector": [1.0] * 1536}
    vector_record = physical_vector(vector_data)
    need(vector_record["metadata"] == {
        "g7_v": 1, "tenant": "t" * 128, "source_key": "g7_src_" + "0" * 64,
        "revision_key": "g7_rev_" + "0" * 64, "chunk_key": "g7_chk_" + "0" * 64}
         and vector_record["payload"] == b"a" and vector_record["expiry"] is None
         and len(vector_record["vector"]) == 6144
         and metadata_bytes(vector_record["metadata"]) < 65536,
         "vector-physical-form")
    root = {"kind": "receipt", "fields": {
        "binding": binding, "tenant": "t" * 128, "source_id": "s" * 256,
        "content": "00" * 32, "pipeline": "11" * 32}}
    identities = {
        "source": source,
        "revision": revision,
        "chunk": chunk,
        "entity": entity,
        "has_revision": {"kind": "has_revision", "fields": {"source": source,
                                                             "revision": revision}},
        "has_chunk": {"kind": "has_chunk", "fields": {"revision": revision,
                                                       "chunk": chunk}},
        "mention": mention,
        "relation": relation,
        "receipt": root,
        "batch_receipt": {"kind": "batch_receipt", "fields": {"root": root,
                                                               "ordinal": 0xffffffff}},
        "delete_receipt": {"kind": "delete_receipt", "fields": {
            "binding": binding, "tenant": "t" * 128, "source_id": "s" * 256,
            "expected": (1 << 64) - 1}},
        "abort_receipt": {"kind": "abort_receipt", "fields": {
            "binding": binding, "tenant": "t" * 128, "source_id": "s" * 256,
            "aborted": root}},
    }
    identities["batch_receipt"]["fields"]["root"] = identities["abort_receipt"]
    key_map = {role: graph_key(value) for role, value in identities.items()}
    maximal_data = {
        "source": dict(revision_key=key_map["revision"], revision_digest="00" * 32,
                       pipeline="11" * 32, status=3,
                       job_receipt_key=key_map["receipt"], last_batch=0xffffffff),
        "revision": dict(source_key=key_map["source"], uri="u" * 2048,
                         text_length=0xffffffff, chunk_count=0xffffffff,
                         receipt_key=key_map["receipt"]),
        "chunk": dict(source_key=key_map["source"], revision_key=key_map["revision"],
                      ordinal=0xffffffff, start=0, end=0xffffffff,
                      chunk_digest="00" * 32, pipeline="11" * 32),
        "entity": {},
        "has_revision": {},
        "has_chunk": {},
        "mention": evidence,
        "relation": relation_data,
        "receipt": dict(request_digest="22" * 32,
                        plan_digest={"tag": 1, "value": "11" * 32},
                        batch_count=0xffffffff, revision_key=key_map["revision"],
                        outcome=3, chunks=0xffffffff, entities=0xffffffff,
                        relations=0xffffffff, mentions=0xffffffff),
        "batch_receipt": expand(templates["batch_data"], templates),
        "delete_receipt": dict(request_digest="22" * 32,
                               plan_digest={"tag": 1, "value": "11" * 32},
                               batch_count=0xffffffff, revision_key="",
                               outcome=2, chunks=0xffffffff, entities=0xffffffff,
                               relations=0xffffffff, mentions=0xffffffff),
        "abort_receipt": dict(request_digest="22" * 32,
                              plan_digest={"tag": 1, "value": "11" * 32},
                              batch_count=0xffffffff, revision_key="",
                              outcome=3, chunks=0xffffffff, entities=0xffffffff,
                              relations=0xffffffff, mentions=0xffffffff),
    }
    physical_sizes = {}
    for role, identity_value in identities.items():
        properties = physical(identity_value, maximal_data[role])
        byte_count = metadata_bytes(properties)
        need(len(properties) <= 64 and byte_count <= 65536,
             "all-role-C9-capacity:" + role)
        physical_sizes[role] = (len(properties), byte_count)
    need(physical_sizes["mention"] == (19, 36258), "max-mention-C9-projected")
    need(physical_sizes["batch_receipt"] == (4, 2076), "max-batch-abort-root")
    need(len(physical_vector(vector_data)["metadata"]) == 5,
         "max-vector-metadata-fields")
    vector_metadata_bytes = metadata_bytes(physical_vector(vector_data)["metadata"])
    for role in ("result_shard", "ownership_shard"):
        shard_raw = shard_binary({"channel": 0 if role == "result_shard" else 1,
                                  "root_key": key_map["receipt"], "batch": 0xffffffff,
                                  "group": 0xffffffff, "index": 0xffffffff,
                                  "entries": [expand(templates["entry"], templates)]})
        need(len(shard_raw) <= 3072, "shard_capacity:" + role)
        props = {"data": b64(bytes(3072))}
        need(len(props) == 1 and metadata_bytes(props) <= 65536,
             "all-role-C9-capacity:" + role)
        physical_sizes[role] = (len(props), metadata_bytes(props))
    physical_sizes["vector_record_metadata"] = (
        len(physical_vector(vector_data)["metadata"]), vector_metadata_bytes)
    return ["stored-head-properties", "all-stored-schema-roundtrips",
            "max-mention-C9-projected", "max-relation-C9", "vector-physical-form"] + [
                f"C9-{role}:{fields_}-fields/{size}-bytes"
                for role, (fields_, size) in physical_sizes.items()] + [
                "all-graph-role-C9-capacity"]


def invariants(document):
    templates = document["templates"]
    rejects(lambda: encode("f64", 10 ** 1000), "float", "float-conversion-overflow")
    rejects(lambda: encode("f32", 1), "float", "float-requires-float")
    need(encode("f32", -1e-100) == bytes(4), "f32-underflow-zero")
    pipeline = expand(templates["pipeline"], templates)
    named("pipeline", dict(pipeline, extractor_version="v" * 256))
    for label in ("latest", "default", "v 1", "é", "x" * 257):
        broken = copy.deepcopy(pipeline)
        broken["extractor_version"] = label
        rejects(lambda: named("pipeline", broken), "extractor_version",
                "extractor-label-" + label[:8])
    rejects(lambda: named("pipeline", dict(pipeline, dimensions=0)),
            "pipeline_version", "pipeline-dimensions")
    rejects(lambda: named("pipeline", dict(pipeline, normalization="unicode")),
            "normalization", "pipeline-normalization")
    unsorted_rules = copy.deepcopy(pipeline)
    unsorted_rules["entity_rules"] = [
        {"rule_id": "z", "literal": "z", "kind": "K", "key": "z"},
        {"rule_id": "a", "literal": "a", "kind": "K", "key": "a"}]
    rejects(lambda: named("pipeline", unsorted_rules), "rule_order", "rule-sort")
    duplicate_rules = copy.deepcopy(unsorted_rules)
    duplicate_rules["entity_rules"] = [unsorted_rules["entity_rules"][1]] * 2
    rejects(lambda: named("pipeline", duplicate_rules), "rule_order", "rule-duplicate")
    rejects(lambda: identity({"kind": "relation", "fields": {
        "revision": expand(templates["revision"], templates),
        "chunk": expand(templates["chunk"], templates),
        "subject": expand(templates["entity"], templates), "predicate": "bad-name",
        "object": expand(templates["entity"], templates), "start": 0, "end": 1,
        "attributes": {}}}), "predicate", "predicate-charset")
    rejects(lambda: identity({"kind": "abort_receipt", "fields": {
        "binding": expand(templates["binding"], templates), "tenant": "t",
        "source_id": "s", "aborted": expand(templates["abort_receipt"], templates)}}),
        "abort_of_abort", "abort-of-abort")
    wrong_slot = expand(templates["receipt_data"], templates)
    wrong_slot["plan_digest"]["tag"] = 2
    rejects(lambda: named("receipt_data", wrong_slot), "plan_slot_kind", "receipt-slot-tag")
    wrong_batch_slot = expand(templates["batch_data"], templates)
    wrong_batch_slot["batch_digest"]["tag"] = 1
    rejects(lambda: named("batch_data", wrong_batch_slot), "batch_slot_kind", "batch-slot-tag")
    bad_request = expand(templates["request"], templates)
    bad_request["expected_source"] = {"tag": 2, "value": [1, 0]}
    rejects(lambda: named("request", bad_request), "request_revision_ref",
            "request-symbolic-revision")
    request_at_uri_limit = expand(templates["request"], templates)
    request_at_uri_limit["uri"] = "u" * 2048
    named("request", request_at_uri_limit)
    bad_generation = copy.deepcopy(request_at_uri_limit)
    bad_generation["pipeline"]["binding_generation"] = "11" * 32
    rejects(lambda: named("request", bad_generation), "binding_generation",
            "pipeline-binding-generation")
    accepts_delete_abort_target = {"kind": "abort_receipt", "fields": {
        "binding": expand(templates["binding"], templates), "tenant": "t",
        "source_id": "s", "aborted": expand(templates["delete_receipt"], templates)}}
    identity(accepts_delete_abort_target)
    plan = expand(templates["plan_with_slots"], templates)
    base = digest_record("plan", plan, {"lineage": "a", "revision": 1})
    need(base == digest_record("plan", plan, {"lineage": "b", "revision": 999}),
         "runtime-substitution-invariance")
    changed = copy.deepcopy(plan)
    changed["request_digest"] = "01" * 32
    need(base != digest_record("plan", changed), "literal-inclusion")
    changed = copy.deepcopy(plan)
    slot = next(op["fields"]["object"]["data"]["plan_digest"]
                for op in changed["batches"][0]["groups"][0]["operations"]
                if op["tag"] == 12)
    slot["value"] = "ab" * 32
    need(base == digest_record("plan", changed), "placeholder-invariance")
    slot["tag"] = 0
    rejects(lambda: digest_record("plan", changed), "plan_slot_kind", "placeholder-tag")
    batch = expand(templates["batch"], templates)
    batch_hash = digest_record("batch", batch)
    changed_batch = copy.deepcopy(batch)
    root_op = next(op for op in changed_batch["groups"][0]["operations"]
                   if op["tag"] == 12)
    root_op["fields"]["object"]["data"]["plan_digest"]["value"] = "cd" * 32
    need(batch_hash == digest_record("batch", changed_batch),
         "batch-plan-placeholder-invariance")
    slot = {"batch_digest": {"tag": 2, "value": "00" * 32}, "final": True,
            "created_anchors": 1, "reused_anchors": 0, "result_shards": 1,
            "ownership_shards": 1}
    masked_slot = named("batch_data", slot, True)
    slot["batch_digest"]["value"] = "ff" * 32
    need(named("batch_data", slot, True) == masked_slot
         and named("batch_data", slot) != masked_slot, "batch-slot-invariance")
    need(len(domains()) == len(set(domains())), "domain-registry-distinct")
    for raw, expected in ((b"", ""), (b"a", "YQ=="), (b"ab", "YWI="),
                          (b"abc", "YWJj")):
        need(b64(raw) == expected and unb64(expected) == raw, "base64-positive")
    need([len(part) for part in split_fragments(bytes(3072))] == [3072]
         and [len(part) for part in split_fragments(bytes(3073))] == [3072, 1]
         and [len(part) for part in split_fragments(b"")] == [0],
         "fragment-boundaries")
    for invalid in ("YQ", "YR==", "YQ=== ", "YQ==\n", "!!!!"):
        try:
            unb64(invalid)
        except Invalid:
            pass
        else:
            raise Invalid("base64-negative:" + invalid)
    header = dict(channel=0, root_key="g7_rec_" + "0" * 64, batch=1, group=0, index=0)
    entries = []
    for index in range(28):
        entry = expand(templates["entry"], templates)
        entry.update(producing_op=index, object_kind=1, object_key=(
            "g7_hr_" if index == 2 else "g7_men_") + "0" * 64)
        entry["revision"] = {"tag": 0, "value": None} if index < 2 else {
            "tag": 1, "value": 1}
        entries.append(entry)
    shard = dict(header, entries=entries)
    need(len(shard_binary(shard)) == 3072 and len(b64(shard_binary(shard))) == 4096,
         "typed-shard-exact-fit")
    need(decode_shard(shard_binary(shard)) == shard, "typed-shard-roundtrip")
    ownership = dict(shard, channel=1)
    need(shard_binary(ownership)[:2] == b"\1\1"
         and shard_binary(ownership)[2:] == shard_binary(shard)[2:],
         "ownership-channel")
    need(len(split_shards(header, entries)) == 1, "typed-shard-one")
    rejects(lambda: shard_binary(dict(header, entries=list(reversed(entries)))),
            "entry_order", "shard-unsorted")
    rejects(lambda: shard_binary(dict(header, entries=[entries[0], entries[0]])),
            "entry_order", "shard-duplicate")
    rejects(lambda: decode_shard(b"\2" + shard_binary(shard)[1:]), "version",
            "shard-version")
    rejects(lambda: decode_shard(shard_binary(shard) + b"\0"), "noncanonical",
            "shard-trailing-byte")
    need(graph_body_size([8, 20, 12]) == 68, "three-entry-body-68")
    need(graph_body_size([0] * 4096) == 32772, "entry-limit-4096")
    rejects(lambda: graph_body_size([0] * 4097), "entry_limit", "entry-limit-4097")
    larger = copy.deepcopy(entries)
    larger[2]["object_key"] = "g7_men_" + "0" * 64
    try:
        shard_binary(dict(header, entries=larger))
    except Invalid as error:
        need(str(error) == "shard_capacity", "typed-shard-plus-one")
    else:
        raise Invalid("typed-shard-plus-one")
    splits = split_shards(header, larger)
    need([len(part["entries"]) for part in splits] == [27, 1], "shard-split-27-1")
    need([part["index"] for part in splits] == [0, 1], "shard-split-index")
    validate_shards(header, splits)
    for field, wrong in (("root_key", "g7_rec_" + "1" * 64), ("batch", 2),
                         ("group", 1), ("index", 2), ("channel", 1)):
        altered = copy.deepcopy(splits)
        altered[0][field] = wrong
        rejects(lambda: validate_shards(header, altered), "shard_backrefs_or_split",
                "shard-backref-" + field)
    underfilled = [dict(header, entries=larger[:1]),
                   dict(header, index=1, entries=larger[1:])]
    rejects(lambda: validate_shards(header, underfilled), "shard_backrefs_or_split",
            "shard-nonmaximal")
    for body, plain, sealed in ((14, 66, 94), (16777216, 16777268, 16777296)):
        need(frame_size(body) == plain and frame_size(body, True) == sealed,
             "frame-size-" + str(body))
    try:
        frame_size(16777217)
    except Invalid as error:
        need(str(error) == "body_limit", "frame-body-plus-one")
    else:
        raise Invalid("frame-body-plus-one")
    first = expand(templates["source"], templates)
    second = copy.deepcopy(first)
    second["fields"]["source_id"] = "other"
    try:
        lookup(first, second, bytes(32))
    except Invalid as error:
        need(str(error) == "identity_conflict", "collision-is-conflict")
    else:
        raise Invalid("collision-accepted")
    need(lookup(first, first, bytes(32)), "identity-equality")
    malformed_plan = expand(templates["plan_with_slots"], templates)
    malformed_ops = malformed_plan["batches"][0]["groups"][0]["operations"]
    malformed_ops.reverse()
    rejects(lambda: named("plan", malformed_plan), "batch_guard_first",
            "new-source-head-first")
    source = expand(templates["source"], templates)
    revision = expand(templates["revision"], templates)
    chunk = expand(templates["chunk"], templates)
    entity = expand(templates["entity"], templates)
    head_op = {"tag": 13, "fields": {"source": source, "data": expand(
        templates["head_data"], templates)}}
    def create(role, identity_value, data):
        return {"tag": 5, "fields": {"object": {"identity": identity_value, "data": data}}}
    revision_op = create("revision", revision, expand(
        templates["revision_projection_data"], templates))
    chunk_op = create("chunk", chunk, expand(templates["chunk_projection_data"], templates))
    entity_op = create("entity", entity, {})
    has_revision = {"kind": "has_revision", "fields": {"source": source,
                                                       "revision": revision}}
    has_chunk = {"kind": "has_chunk", "fields": {"revision": revision, "chunk": chunk}}
    mention = {"kind": "mention", "fields": {"chunk": chunk, "entity": entity,
                                             "start": 0, "end": 1}}
    relation = {"kind": "relation", "fields": {"revision": revision, "chunk": chunk,
                "subject": entity, "predicate": "P", "object": entity,
                "start": 0, "end": 1, "attributes": {}}}
    relation_a = copy.deepcopy(relation)
    relation_a["fields"]["attributes"] = {"a": [5, "x" * 100]}
    relation_z = copy.deepcopy(relation)
    relation_z["fields"]["attributes"] = {"z": [0, None]}
    need(identity_order(relation_a) < identity_order(relation_z),
         "attributes-frozen-byte-order")
    source_a = {"kind": "source", "fields": {"binding": expand(
        templates["binding"], templates), "tenant": "t", "source_id": "a"}}
    source_z = copy.deepcopy(source_a)
    source_z["fields"]["source_id"] = "z"
    need(identity_order(source_a) < identity_order(source_z), "identity-string-byte-order")
    edge_ops = [{"tag": 8, "fields": {"object": {"identity": ident_,
                "data": data}}} for ident_, data in [
                (has_revision, {}), (has_chunk, {}),
                (mention, expand(templates["mention_projection_data"], templates)),
                (relation, expand(templates["relation_projection_data"], templates))]]
    edge_ops.sort(key=lambda op: identity_order(op["fields"]["object"]["identity"]))
    ordered = expand(templates["plan"], templates)
    ordered["batches"] = [dict(root=expand(templates["receipt"], templates),
                               ordinal=1, groups=[dict(ordinal=0, operations=[
                                   head_op, entity_op, revision_op, chunk_op,
                                   *edge_ops, expand(templates["root_operation"], templates)])])]
    named("plan", ordered)
    broken_edges = copy.deepcopy(ordered)
    ops = broken_edges["batches"][0]["groups"][0]["operations"]
    ops.remove(edge_ops[0])
    ops.insert(1, edge_ops[0])
    rejects(lambda: named("plan", broken_edges), "edge_endpoint", "edge-endpoint-order")
    late_head = copy.deepcopy(ordered)
    late_head["captured"] = expand(templates["captured_endpoints"], templates)
    late_head["batches"][0]["groups"][0]["operations"] = [
        expand(templates["guard_operation"], templates),
        expand(templates["source_create_other_operation"], templates),
        expand(templates["root_operation"], templates)]
    rejects(lambda: named("plan", late_head), "head_create_first", "head-create-position")
    existing = expand(templates["plan"], templates)
    existing["captured"] = [{"object": source, "expected": expand(
        templates["absent"], templates), "properties_digest": "00" * 32}]
    existing["batches"] = [dict(root=expand(templates["receipt"], templates),
                                ordinal=1, groups=[dict(ordinal=0, operations=[
                                    entity_op, expand(templates["root_operation"], templates)])])]
    rejects(lambda: named("plan", existing), "batch_guard_first", "existing-guard-first")

    mention_identity = expand(next(row["input"] for row in document["vectors"]
                                   if row["name"] == "projection-mention"), templates)["identity"]
    mention_data = expand(templates["mention_projection_data"], templates)
    small_properties = physical(mention_identity, mention_data)
    for changed in (
            {key: value for key, value in small_properties.items() if key != "surface"},
            dict(small_properties, surface="wrong"),
            dict(small_properties, rogue_projection="x")):
        rejects(lambda: decode_physical(mention_identity, changed),
                "evidence_conflict", "projection-conflict")
    long_evidence = dict(mention_data, surface="s" * 4097)
    long_properties = physical(mention_identity, long_evidence)
    need("surface" not in long_properties
         and decode_physical(mention_identity, long_properties) == long_evidence,
         "long-surface-omitted-projection")
    vector_data = expand(templates["vector_projection_data"], templates)
    vector_data["vector"] = [-0.0]
    need(physical_vector(vector_data)["vector"] == bytes(4)
         and named("vector_data", vector_data).endswith(bytes(4)),
         "vector-bits-plan-equal-physical")
    return ["runtime-substitution-invariance", "placeholder-invariance",
            "placeholder-tag", "typed-shard-exact-fit", "typed-shard-plus-one",
            "shard-split-27-1", "frame-size-14", "frame-size-16777216",
            "frame-body-plus-one", "collision-is-conflict", "schema-rule-negatives",
            "new-source-head-first", "edge-endpoint-order", "existing-guard-first",
            "projection-conflict", "long-surface-omitted-projection",
            "vector-bits-plan-equal-physical", "pipeline-label-256",
            "request-uri-2048", "pipeline-binding-generation",
            "abort-delete-root", "fragment-boundaries",
            "attributes-frozen-byte-order", "identity-string-byte-order"]


def expand(value, templates):
    if isinstance(value, dict):
        if set(value) == {"ref"}:
            need(value["ref"] in templates, "fixture_ref")
            return expand(copy.deepcopy(templates[value["ref"]]), templates)
        if set(value) == {"repeat", "count"}:
            return value["repeat"] * value["count"]
        return {key: expand(item, templates) for key, item in value.items()}
    if isinstance(value, list):
        return [expand(item, templates) for item in value]
    return value


def expected_bytes(parts, atoms):
    if isinstance(parts, str):
        return unhex(parts)
    if isinstance(parts, dict) and set(parts) == {"repeat_hex", "count"}:
        return bytes.fromhex(parts["repeat_hex"]) * parts["count"]
    need(isinstance(parts, list), "fixture_bytes")
    return b"".join(expected_bytes(atoms[part[1:]], atoms)
                    if isinstance(part, str) and part.startswith("@")
                    else expected_bytes(part, atoms) for part in parts)


def vector_bytes(vector):
    category, name, value = vector["category"], vector.get("type", ""), vector["input"]
    if category == "primitive":
        return encode(name, value)
    if category == "identity":
        return identity(value)
    if category == "record":
        return named(name, value, vector.get("masked", False))
    if category == "pipeline_label":
        pipeline = dict(value["pipeline"], **{value["field"]: value["value"]})
        return named("pipeline", pipeline)
    if category == "operation":
        return encode("operation", value, vector.get("masked", False))
    if category == "shard":
        return shard_binary(value)
    if category == "ordering":
        rows = value
        ordered = sorted(rows, key=lambda key: identity_order(rows[key]))
        return b"".join(blob(identity(rows[key])) for key in ordered)
    if category == "frame":
        return uint(frame_size(value["body"], value["encrypted"]), 4)
    raise Invalid("fixture_category")


def verify(document):
    need(set(document) == {"schema", "templates", "atoms", "vectors", "negatives"}, "fixture")
    need(document["schema"] == "vlite.g7.canonical-v1.proposed", "fixture_schema")
    names = set()
    for template in document["vectors"]:
        vector = expand(template, document["templates"])
        need(vector["name"] not in names, "fixture_name")
        names.add(vector["name"])
        if vector["category"] == "projection":
            actual = readable_projection(vector["input"]["identity"], vector["input"]["data"])
            need(actual == vector["expected"], "projection:" + vector["name"])
            continue
        if vector["category"] == "vector_record":
            actual = physical_vector(vector["input"])
            actual = dict(actual, payload=actual["payload"].hex(),
                          vector=actual["vector"].hex())
            need(actual == vector["expected"], "vector_record:" + vector["name"])
            continue
        if vector["category"] == "plan_order":
            named("plan", vector["input"])
            actual = [[[op["tag"] for op in group["operations"]]
                       for group in batch["groups"]] for batch in vector["input"]["batches"]]
            need(actual == vector["expected"], "plan_order:" + vector["name"])
            continue
        actual = vector_bytes(vector)
        expected = expected_bytes(vector["hex"], document["atoms"])
        need(actual == expected, "vector:" + vector["name"])
        if vector["category"] == "identity":
            need(decode_identity(expected) == vector["input"], "roundtrip:" + vector["name"])
            if REGISTRY[vector["input"]["kind"]][2] is not None:
                domain = "vlite.g7.v1." + REGISTRY[vector["input"]["kind"]][1]
                hand_hash = hashlib.sha256(b"\1" + struct.pack("<I", len(domain))
                                          + domain.encode("ascii")
                                          + struct.pack("<I", len(expected)) + expected).hexdigest()
                need(graph_key(vector["input"]).endswith(hand_hash), "key:" + vector["name"])
                if "key" in vector:
                    need(graph_key(vector["input"]) == vector["key"], "literal-key")
        if vector["category"] == "ordering":
            names_order = sorted(vector["input"],
                                 key=lambda key: identity_order(vector["input"][key]))
            need(names_order == vector["order"], "ordering:" + vector["name"])
        if vector["category"] == "collision":
            need(not lookup(vector["input"]["left"], vector["input"]["right"],
                            bytes(32)), "collision:" + vector["name"])
        if vector["category"] == "primitive":
            reader = Reader(expected)
            decoded = decode(vector["type"], reader)
            need(reader.at == len(expected) and encode(vector["type"], decoded) == expected,
                 "roundtrip:" + vector["name"])
        if "sha256" in vector:
            need(hashlib.sha256(actual).hexdigest() == vector["sha256"],
                 "hash:" + vector["name"])
        if "framed_sha256" in vector:
            if vector["category"] == "pipeline_label":
                value = vector["input"]
                digest = digest_record("pipeline", dict(
                    value["pipeline"], **{value["field"]: value["value"]}))
            else:
                digest = digest_record(vector["type"], vector["input"])
            need(digest
                 == vector["framed_sha256"], "framed-hash:" + vector["name"])
    for template in document["negatives"]:
        vector = expand(template, document["templates"])
        need(vector["name"] not in names, "fixture_name")
        names.add(vector["name"])
        try:
            if vector.get("category") == "collision":
                lookup(vector["input"]["left"], vector["input"]["right"], bytes(32))
            elif vector.get("category") == "projection":
                properties = physical(vector["input"]["identity"], vector["input"]["data"])
                if vector["input"]["change"] == "missing":
                    properties.pop(vector["input"]["property"])
                elif vector["input"]["change"] == "mismatch":
                    properties[vector["input"]["property"]] = vector["input"]["value"]
                else:
                    properties["unexpected_projection"] = "x"
                decode_physical(vector["input"]["identity"], properties)
            elif vector.get("category") == "frame":
                vector_bytes(vector)
            elif "decode_hex" in vector:
                raw = expected_bytes(vector["decode_hex"], document["atoms"])
                reader = Reader(raw)
                decoded = decode(vector["type"], reader)
                need(reader.at == len(raw) and encode(vector["type"], decoded) == raw,
                     "noncanonical")
            else:
                vector_bytes(vector)
        except Invalid as error:
            need(str(error) == vector["error"], "negative:" + vector["name"] + ":" + str(error))
        else:
            raise Invalid("accepted_negative:" + vector["name"])
    return len(names)


def self_test(document):
    source = Path(__file__).read_bytes()
    before = hashlib.sha256(source).hexdigest()
    checks = [
        ("order", "struct_bytes", lambda old: lambda schema, value, masked=False:
         old(tuple(reversed(schema)), value, masked)),
        ("width", "uint", lambda old: lambda value, size:
         old(value, 8 if size == 4 else size)),
        ("tag", "identity", lambda old: lambda value: b"\1\xff" + old(value)[2:]),
        ("domain", "framed_hash", lambda old: lambda domain, payload:
         old("vlite.g7.v1.plan", payload)),
        ("exclusion", "digest_record", lambda old: lambda name, value, execution=None:
         old(name, value) if execution is None else hashlib.sha256(
             (old(name, value) + str(execution)).encode("utf-8")).hexdigest()),
        ("placeholder", "named", lambda old: lambda name, value, masked=False:
         old(name, value, False)),
        ("boundary", "shard_binary", lambda old: lambda value, masked=False:
         strict_shard(old, value, masked)),
        ("frame-envelope", "frame_size", lambda old: lambda body, encrypted=False:
         old(body, encrypted) + 1),
        ("collision", "lookup", lambda old: lambda expected, stored, forced_digest=None: True),
        ("metadata-width", "metadata_binary", lambda old: lambda props:
         old(props)[:6] + b"\0\0" + old(props)[6:]),
        ("float-zero", "encode", lambda old: lambda kind, value, masked=False:
         struct.pack("<d", value) if kind == "f64" and value == 0
         else old(kind, value, masked)),
        ("semantic-comparator", "identity_order",
         lambda old: lambda value: identity(value)),
        ("head-first", "validate_plan_order", lambda old: lambda plan: None),
        ("edge-endpoint", "validate_edge_order", lambda old: lambda edge, known: None),
        ("vector-record-form", "physical_vector",
         lambda old: lambda data: dict(old(data), expiry="present")),
        ("readable-projection", "readable_projection",
         lambda old: lambda identity_value, data: {}),
        ("extractor-label", "extractor_label", lambda old: lambda value: True),
        ("fragment-final", "split_fragments",
         lambda old: lambda raw: old(raw) + ([b""] if raw and len(raw) % 3072 == 0 else [])),
    ]
    killed = []
    for label, name, replacement in checks:
        original = globals()[name]
        globals()[name] = replacement(original)
        try:
            try:
                verify(document)
                stored_checks(document)
                invariants(document)
            except Invalid as error:
                killed.append((label, str(error)))
            else:
                raise Invalid("surviving_mutant:" + label)
        finally:
            globals()[name] = original
        verify(document)
        stored_checks(document)
        invariants(document)

    def omit_check(*reasons):
        return lambda old: lambda condition, reason: (
            None if reason in reasons else old(condition, reason))

    def batch_global_phases(old):
        def mutated(plan):
            flattened = copy.deepcopy(plan)
            for batch in flattened["batches"]:
                batch["groups"] = [{"ordinal": 0, "operations": [
                    op for group in batch["groups"] for op in group["operations"]]}]
            return old(flattened)
        return mutated

    def skip_chunk_tag(tag):
        def replace(old):
            def mutated(members, creation_groups, chunk, op_tag,
                        batch_ordinal, group_index):
                if op_tag == tag:
                    return
                return old(members, creation_groups, chunk, op_tag,
                           batch_ordinal, group_index)
            return mutated
        return replace

    def omit_creation_split(old):
        def mutated(members, creation_groups, chunk, tag,
                    batch_ordinal, group_index):
            if tag in (5, 6, 7, 8):
                members.append((tag, identity(chunk)))
                creation_groups.setdefault(identity(chunk), (batch_ordinal, group_index))
                return
            return old(members, creation_groups, chunk, tag,
                       batch_ordinal, group_index)
        return mutated

    def make_cleanup_indivisible(old):
        def mutated(plan):
            for batch in plan["batches"]:
                if len(batch["groups"]) > 1:
                    raise Invalid("cleanup_group_split")
            return old(plan)
        return mutated

    def ordinal_chunk_membership(old):
        def mutated(members, creation_groups, chunk, tag,
                    batch_ordinal, group_index):
            if chunk["kind"] == "chunk":
                chunk_id = chunk["fields"]["ordinal"]
                members.append((tag, chunk_id))
                if tag in (5, 6, 7, 8):
                    location = (batch_ordinal, group_index)
                    previous = creation_groups.setdefault(chunk_id, location)
                    need(previous == location, "chunk_creation_split")
                return
            return old(members, creation_groups, chunk, tag,
                       batch_ordinal, group_index)
        return mutated

    def ignore_tag3_record_pair(old):
        def mutated(plan):
            candidate = copy.deepcopy(plan)
            for batch in candidate["batches"]:
                for group in batch["groups"]:
                    detach = next((op for op in group["operations"]
                                   if op["tag"] == 2), None)
                    if detach is None:
                        continue
                    for op in group["operations"]:
                        if op["tag"] == 3:
                            op["fields"]["collection"] = detach["fields"]["collection"]
                            op["fields"]["record"] = detach["fields"]["record"]
            return old(candidate)
        return mutated

    targeted = [
        ("provenance-graph-key", "plan-provenance-order", "identity_order",
         lambda old: lambda value: graph_key_order(value)
         if value["kind"] in ("has_revision", "has_chunk", "mention") else old(value)),
        ("provenance-unchecked", "plan-provenance-reversed", "need",
         omit_check("provenance_identity_order")),
        ("batch-global-phases", "plan-two-chunk-groups", "validate_plan_order",
         batch_global_phases),
        ("head-create-later-batch", "plan-head-create-batch-two", "need",
         omit_check("batch_guard_first", "head_create_first")),
        ("descending-chunks", "plan-chunk-ordinals-descending", "need",
         omit_check("chunk_ordinal_order")),
        ("descending-accepted", "accepted-chunks-descending", "need",
         omit_check("accepted_chunk_order")),
        ("reversed-relations", "plan-relations-reversed", "need",
         omit_check("relation_identity_order")),
        ("attach-put-same-phase", "plan-attach-before-put", "operation_phase",
         lambda old: lambda op: 7 if op["tag"] == 7 else old(op)),
        ("delete-detach-same-phase", "plan-delete-before-detach", "operation_phase",
         lambda old: lambda op: 2 if op["tag"] == 3 else old(op)),
        ("request-content-unchecked", "request-content-digest-mismatch", "need",
         omit_check("request_content_digest")),
        ("binding-generation-unchecked", "request-binding-generation-mismatch", "need",
         omit_check("binding_generation")),
        ("graph-entry-collection", "graph-entry-with-collection", "need",
         omit_check("entry_graph_fields")),
        ("empty-chunk", "chunk-empty-span", "need", omit_check("chunk_span")),
        ("confidence-over-one", "observation-confidence-above-one", "need",
         omit_check("confidence")),
        ("kind-64", "entity-kind-64", "need", omit_check("entity_identity")),
        ("literal-plan-slot", "plan-digest-literal-slot", "need",
         omit_check("plan_slot_kind")),
        ("absent-head-patch", "head-patch-absent", "need",
         omit_check("head_patch_expected")),
        ("case-sensitive-version", "extractor-label-uppercase-latest",
         "extractor_label", lambda old: lambda value: pipeline_label(value)
         and value not in ("latest", "default")),
        ("case-sensitive-model-version", "model-version-uppercase-default",
         "extractor_label", lambda old: lambda value: pipeline_label(value)
         and value not in ("latest", "default")),
        ("surface-strict-4096", "projection-mention-surface-4096", "readable_projection",
         lambda old: lambda ident_, data: {
             key: value for key, value in old(ident_, data).items()
             if key != "surface" or len(text(value)) < 4096}),
        ("f32-negative-underflow", "float32-underflow-positive-zero", "encode",
         lambda old: lambda kind, value, masked=False: struct.pack("<f", value)
         if kind == "f32" else old(kind, value, masked)),
        ("receipt-extra-projection", "projection-receipt-none", "readable_projection",
         lambda old: lambda ident_, data: dict(old(ident_, data), outcome=data["outcome"])
         if ident_["kind"] == "receipt" else old(ident_, data)),
        ("chunk-order-cross-batch", "plan-chunk-ordinals-across-batches", "need",
         omit_check("chunk_ordinal_order")),
        ("group-ordinal-gap", "plan-group-ordinals-gap", "need",
         omit_check("group_order")),
        ("group-ordinal-duplicate", "plan-group-ordinals-duplicate", "need",
         omit_check("group_order")),
        ("empty-group", "plan-empty-group", "need", omit_check("empty_group")),
        ("duplicate-operation", "plan-duplicate-operation", "need",
         omit_check("duplicate_operation")),
        ("duplicate-edge", "plan-duplicate-edge", "need",
         omit_check("duplicate_operation", "provenance_identity_order")),
        ("duplicate-accepted-ordinal", "plan-duplicate-accepted-ordinal", "need",
         omit_check("accepted_chunk_order")),
        ("mixed-group-membership", "plan-group-mixed-chunks", "need",
         omit_check("group_chunk_membership")),
        ("vector-put-group-membership", "plan-vector-put-wrong-chunk-group",
         "add_chunk_group_member", skip_chunk_tag(6)),
        ("creation-bundle-split", "plan-creation-group-split",
         "add_chunk_group_member", omit_creation_split),
        ("cleanup-groups-indivisible", "plan-cleanup-split",
         "validate_plan_order", make_cleanup_indivisible),
        ("chunk-identity-to-ordinal", "plan-chunk-identity-cross-revision-same-ordinal",
         "add_chunk_group_member", ordinal_chunk_membership),
        ("chunk-tag-1-membership", "plan-chunk-membership-tag-1",
         "add_chunk_group_member", skip_chunk_tag(1)),
        ("chunk-tag-2-membership", "plan-chunk-membership-tag-2",
         "add_chunk_group_member", skip_chunk_tag(2)),
        ("chunk-tag-3-record-pair", "plan-chunk-membership-tag-3",
         "validate_plan_order", ignore_tag3_record_pair),
        ("chunk-tag-4-membership", "plan-chunk-membership-tag-4",
         "add_chunk_group_member", skip_chunk_tag(4)),
        ("chunk-tag-7-membership", "plan-chunk-membership-tag-7",
         "add_chunk_group_member", skip_chunk_tag(7)),
        ("chunk-tag-8-membership", "plan-chunk-identity-cross-revision-same-ordinal",
         "add_chunk_group_member", skip_chunk_tag(8)),
        ("cross-group-duplicate-edge", "plan-cross-group-duplicate-edge", "need",
         omit_check("duplicate_edge")),
        ("second-head-create", "plan-second-head-create", "need",
         omit_check("head_create_count", "head_create_first")),
        ("label-256-rejected", "pipeline-label-256", "pipeline_label",
         lambda old: lambda value: old(value) and len(text(value)) <= 255),
        ("label-257-accepted", "pipeline-label-257", "pipeline_label",
         lambda old: lambda value: len(text(value)) <= 257
         and re.fullmatch(rb"[A-Za-z0-9._-]+", text(value)) is not None),
        ("label-plus-accepted", "pipeline-label-plus-sign", "pipeline_label",
         lambda old: lambda value: old(value) or value == "a+b"),
        ("enum-chunker-range", "pipeline-enum-chunker-out-of-range", "need",
         omit_check("enum")),
        ("enum-request-mode-range", "request-enum-mode-out-of-range", "need",
         omit_check("enum")),
        ("enum-head-status-range", "head-status-out-of-range", "need",
         omit_check("enum")),
        ("enum-receipt-outcome-range", "receipt-outcome-out-of-range", "need",
         omit_check("enum")),
        ("enum-entry-action-range", "entry-action-out-of-range", "need",
         omit_check("enum")),
        ("enum-entry-kind-range", "entry-kind-out-of-range", "need",
         omit_check("object_kind")),
        ("enum-shard-channel-range", "shard-channel-out-of-range", "need",
         omit_check("enum")),
        ("enum-delete-mode-range", "delete-mode-out-of-range", "need",
         omit_check("enum")),
        ("enum-abort-mode-range", "abort-mode-out-of-range", "need",
         omit_check("enum")),
    ]
    for field in ("extractor-alias", "embedder-alias"):
        targeted.append((field + "-reserved", field + "-default", "pipeline_label",
                         lambda old: lambda value: old(value)
                         and value.lower() not in ("latest", "default")))
    for field, suffix in (("extractor-alias", "-invalid"),
                          ("embedder-alias", "-invalid"), ("model-version", "-latest")):
        targeted.append((field + "-unpinned", field + suffix, "need",
                         omit_check("pipeline_label")))
    for record_name in ("pipeline", "request", "batch"):
        targeted.append(("framed-" + record_name, record_name, "framed_hash",
                         lambda old: lambda domain, payload: old(domain, payload + b"\0")))
    for label, vector_name, name, replacement in targeted:
        selected = dict(document, vectors=[], negatives=[])
        for category in ("vectors", "negatives"):
            selected[category] = [row for row in document[category]
                                  if row["name"] == vector_name]
        need(len(selected["vectors"]) + len(selected["negatives"]) == 1,
             "mutant_vector:" + vector_name)
        verify(selected)
        original = globals()[name]
        globals()[name] = replacement(original)
        try:
            try:
                verify(selected)
            except Invalid as error:
                killed.append((label, vector_name + ":" + str(error)))
            else:
                raise Invalid("surviving_mutant:" + label)
        finally:
            globals()[name] = original
        verify(selected)

    need(hashlib.sha256(Path(__file__).read_bytes()).hexdigest() == before,
         "source-restore")
    return killed


def strict_shard(original, value, masked):
    raw = original(value, masked)
    need(len(raw) < 3072, "mutant_strict_shard_capacity")
    return raw


def unique(pairs):
    result = {}
    for key, value in pairs:
        need(key not in result, "duplicate_json_key")
        result[key] = value
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--fixture", type=Path, default=FIXTURE)
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    try:
        document = json.loads(args.fixture.read_text(encoding="utf-8"),
                              object_pairs_hook=unique)
        count = verify(document)
        print(f"Verified {count} canonical-schema vectors.")
        print("Checked " + ", ".join(stored_checks(document) + invariants(document)))
        if args.self_test:
            for label, evidence in self_test(document):
                print(f"KILLED {label}: {evidence}")
            print("Source SHA-256 unchanged; no equivalent survivors claimed.")
    except (OSError, ValueError, KeyError, TypeError) as error:
        print(f"{args.fixture}: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
