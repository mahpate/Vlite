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
    "vector": (15, "vector", None),
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
    "vector": fields(binding="binding", record="u64"),
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
                           surface="text", confidence=opt("f64"), extractor_version="digest",
                           attributes="attributes"),
    "relation_data": fields(source_key="text", revision_key="text", chunk_key="text",
                            confidence=opt("f64"), extractor_version="digest"),
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
                       extractor_alias="text", extractor_version="digest",
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
            return blob(b"\1" + struct_bytes(RECORDS[item], value, masked))
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
        need(type(value) in (float, int), "float")
        try:
            need(math.isfinite(value), "float")
            result = struct.pack("<f" if kind == "f32" else "<d",
                                 0.0 if value == 0 else value)
        except (OverflowError, struct.error) as error:
            raise Invalid("float") from error
        need(math.isfinite(struct.unpack("<f" if kind == "f32" else "<d", result)[0]),
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
        need(isinstance(value, dict) and len(value) <= 16, "attributes")
        raw = b"".join(blob(text(key)) + encode("attribute", value[key], masked)
                       for key in sorted(value, key=lambda key: text(key)))
        need(len(raw) <= 8192, "attribute_bytes")
        return blob(raw)
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
    return b"\1" + uint(REGISTRY[role][0], 1) + struct_bytes(IDENTITIES[role], values)


def named(name, value, masked=False):
    need(name in RECORDS, "record")
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
        return {"tag": tag, "fields": decode_struct(OPS[tag], reader)}
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
    return struct_bytes(RECORDS["entry"], entry, masked)


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
        parts = [raw[index:index + 3072] for index in range(0, len(raw), 3072)] or [b""]
        need(len(parts) <= 99, "fragments")
        for index, part in enumerate(parts):
            result[f"{prefix}{index:02}"] = b64(part)
    for field in ("source_key", "revision_key", "chunk_key"):
        if field in data and data[field] is not None:
            result[field] = data[field]
    metadata_bytes(result)
    return result


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
    need(physical(stored_identity, data) == properties, "stored_properties")
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
        "d00": "AQAAAAAAAAAAAA=="}, "stored-head-properties")
    need(metadata_binary({"z": 1, "a": "x"}).hex()
         == "0200000001006101010000007801007a020100000000000000",
         "metadata-name-u16-type-order")
    need(metadata_bytes(physical(sample, expand(templates["head_data"], templates))) == 152,
         "head-metadata-size-152")
    props = physical(sample, expand(templates["head_data"], templates))
    rejects(lambda: decode_physical(sample, dict(props, unexpected="x")),
            "stored_properties", "stored-unknown-property")
    broken = dict(props)
    broken["i01"] = broken.pop("i00")
    rejects(lambda: decode_physical(sample, broken), "fragment_indexes",
            "stored-missing-first-fragment")
    rejects(lambda: decode_physical(sample, dict(props, g7_kind="ent")),
            "stored_properties", "stored-wrong-role")
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
                    extractor_version="00" * 32, attributes=attrs)
    need(len(identity(source)) == 692 and len(identity(revision)) == 762
         and len(identity(chunk)) == 780 and len(identity(entity)) == 759,
         "max-identity-arithmetic")
    need(len(identity(mention)) == 1557
         and len(named("mention_data", evidence)) == 24851
         and metadata_bytes(physical(mention, evidence)) == 35613,
         "max-mention-C9-35613")
    relation = {"kind": "relation", "fields": dict(
        revision=revision, chunk=chunk, subject=entity, predicate="P" * 63,
        object=entity, start=0, end=16384, attributes=attrs)}
    need(len(identity(relation)) == 11349, "max-relation-identity-11349")
    relation_data = {key: value for key, value in evidence.items()
                     if key not in ("surface", "attributes")}
    need(metadata_bytes(physical(relation, relation_data)) < 65536,
         "max-relation-C9")
    return ["stored-head-properties", "all-stored-schema-roundtrips"]


def invariants(document):
    templates = document["templates"]
    rejects(lambda: encode("f64", 10 ** 1000), "float", "float-conversion-overflow")
    plan = expand(templates["plan_with_slots"], templates)
    base = digest_record("plan", plan, {"lineage": "a", "revision": 1})
    need(base == digest_record("plan", plan, {"lineage": "b", "revision": 999}),
         "runtime-substitution-invariance")
    changed = copy.deepcopy(plan)
    changed["request_digest"] = "01" * 32
    need(base != digest_record("plan", changed), "literal-inclusion")
    changed = copy.deepcopy(plan)
    slot = changed["batches"][0]["groups"][0]["operations"][0]["fields"]["object"][
        "data"]["plan_digest"]
    slot["value"] = "ab" * 32
    need(base == digest_record("plan", changed), "placeholder-invariance")
    slot["tag"] = 0
    need(base != digest_record("plan", changed), "placeholder-tag")
    batch = expand(templates["batch"], templates)
    batch_hash = digest_record("batch", batch)
    changed_batch = copy.deepcopy(batch)
    changed_batch["groups"][0]["operations"][0]["fields"]["object"]["data"][
        "plan_digest"]["value"] = "cd" * 32
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
    return ["runtime-substitution-invariance", "placeholder-invariance",
            "placeholder-tag", "typed-shard-exact-fit", "typed-shard-plus-one",
            "shard-split-27-1", "frame-size-14", "frame-size-16777216",
            "frame-body-plus-one", "collision-is-conflict"]


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
    need(isinstance(parts, list), "fixture_bytes")
    return b"".join(expected_bytes(atoms[part[1:]], atoms)
                    if isinstance(part, str) and part.startswith("@")
                    else expected_bytes(part, atoms) for part in parts)


def vector_bytes(vector):
    category, name, value = vector["category"], vector["type"], vector["input"]
    if category == "primitive":
        return encode(name, value)
    if category == "identity":
        return identity(value)
    if category == "record":
        return named(name, value, vector.get("masked", False))
    if category == "operation":
        return encode("operation", value, vector.get("masked", False))
    if category == "shard":
        return shard_binary(value)
    raise Invalid("fixture_category")


def verify(document):
    need(set(document) == {"schema", "templates", "atoms", "vectors", "negatives"}, "fixture")
    need(document["schema"] == "vlite.g7.canonical-v1.proposed", "fixture_schema")
    names = set()
    for template in document["vectors"]:
        vector = expand(template, document["templates"])
        need(vector["name"] not in names, "fixture_name")
        names.add(vector["name"])
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
        if vector["category"] == "primitive":
            reader = Reader(expected)
            decoded = decode(vector["type"], reader)
            need(reader.at == len(expected) and encode(vector["type"], decoded) == expected,
                 "roundtrip:" + vector["name"])
        if "sha256" in vector:
            need(hashlib.sha256(actual).hexdigest() == vector["sha256"],
                 "hash:" + vector["name"])
        if "framed_sha256" in vector:
            need(framed_hash("vlite.g7.v1." + vector["type"], actual).hex()
                 == vector["framed_sha256"], "framed-hash:" + vector["name"])
    for template in document["negatives"]:
        vector = expand(template, document["templates"])
        need(vector["name"] not in names, "fixture_name")
        names.add(vector["name"])
        try:
            if "decode_hex" in vector:
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
