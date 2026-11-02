"""Independent Python 3 stdlib logical oracle for P4 lossless graph schemas.

Run python -B tests\\compact_graph_schema_oracle.py [--self-test].
Fixtures and hand-authored expected answers are never generated or rewritten.
This is logical test evidence, not serialization, a private byte layout, a
benchmark, a production feature, or a full engine-parity proof.

Sources: docs/projects/roadmap/p4-compact-graph.md; engine/schema.rs
PropertyType, PropertyDefinition, LabelDefinition, RelationshipDefinition,
GraphSchema::validate, EqKey, OrdF64, index_node, usable_index, check_properties,
check_endpoints, check_graph; engine/model.rs validate_label/validate_labels;
engine/limits.rs MAX_SCHEMA_*, MAX_INDEXED_PROPERTIES; engine/properties.rs
validate_field_name/validate_value/validate; engine/filter.rs numeric_compare,
conditions_match/validate_conditions; tests/graph_schema.rs schema(),
declared_rules_are_enforced_and_rejections_write_nothing and
declared_indexes_return_exactly_what_scans_return.

Interchange vlite.compact-graph-schema-logical, version 1:
root = {schema, version, cases, invalid_cases, boundaries}.
case = {name, purpose, input, expected}; invalid = {name, input, error}.
input = {declarations, nodes, edges, queries}, all arrays. A Label declaration
is {kind, name, description, properties}; a Relationship additionally has
{endpoints, multiplicity}. Kinds and all enum tags use exact Rust variant
names. Properties are {name, type, required, unique, index}; index is null,
Exact or Range. Every field is explicit: this is NOT the public JSON API.
Declaration/property/endpoint order is irrelevant; repeated identities reject
instead of modeling builder replacement. Label and Relationship names occupy
separate namespaces. Canonical declarations put Labels before Relationships,
then order by UTF-8 name bytes; properties and endpoint pairs are byte ordered.
Endpoint labels need not have their own declarations. Description text and
all flags are preserved exactly, including empty descriptions and open schemas.

Node = {id, labels, properties}; edge = {id, source, relationship, destination,
properties}. IDs are exact u64 JSON integers, unique separately for nodes and
edges, including logical 0/u64::MAX; allocation metadata is not modeled.
Node label order/repetition is set-like (at most eight distinct labels).
Properties are canonical sorted unique arrays: {name, type, value} for String,
Integer or Boolean; {name, type, bits} for Float; {name, type, latitude_bits,
longitude_bits} for GeoPoint. Bits are 16 lowercase hex digits for finite
IEEE-754 binary64. Values retain type identity: bool is not int, int is not
float; integers remain exact i64, including above 2**53. Both zero bit patterns
are accepted, and equality/uniqueness normalize zeros as Rust EqKey does,
including both GeoPoint coordinates. No Unicode normalization/coercion occurs.
Property limits use existing validation charges, not a new storage format:
4 + sum(2 + UTF-8 name length + 1 + charge); charge is 4+string bytes, 8 for
Integer/Float, 1 for Boolean, 16 for GeoPoint. Coordinates have inclusive
latitude [-90,90], longitude [-180,180].

Well-shaped snapshots may fail conformance: required/type/label-scoped unique,
allowed directed endpoint pairs and UniqueTriple constraints. Undeclared
labels/relationships/properties remain unconstrained. Optional missing unique
values do not collide. All violations are reported deterministically in
node-ID/label/property order then edge-ID order; this is NOT Rust's first-error
write admission or diagnostic-precedence contract. Dangling edges are malformed,
even in empty schemas. Nonconforming snapshots cannot carry queries.

Query = {label, op, property, value} for Eq, or {label, op, property, min, max}
for Range. label is null (full scan) or a valid identifier. Query typed values
omit name. Range bounds are null or same-type Integer/Float, inclusive, at
least one present and min <= max. Results have ascending IDs and index choice
Exact (unique or Exact declaration), Range (Range declaration), or Scan.
This narrow one-condition reference constructs postings independently of its
scan matcher and checks the two answers agree. Wrong-typed query values never
coerce. Range-only properties do not provide Eq postings unless also unique.
No multi-condition planner selection, pagination/cursors, stats/metering,
GeoRadius index/query, observed summaries, or other read API is modeled.

Exclusions: temporal history/segments, mutations/ordered batches/rollback,
ensure_edge/key identity, graph revisions/high-water marks, TTL visibility or
cleanup, attachments/checkpoint-wide ownership, persisted bytes/recovery,
checkpoint publication, production activation and performance acceptance.
Retained physical snapshots are evaluated without a clock, not TTL-filtered.
Unknown/missing fields, malformed kinds/types, duplicate JSON keys, non-integer
JSON numbers, NaN/Infinity and invalid UTF-8 (including surrogates) reject.

Self-test checks hand-authored goldens and malformed diagnostics, seeded
semantics-preserving order shuffles, independently specified limit boundaries,
effective result-changing mutations, rejection mutations, corrupted expected
answers and strict fixture/JSON shapes. Boundary rows {name, limit, error}
specify engine limits; generated inputs exercise exactly limit and limit+1.
"""

import argparse
import copy
import json
import math
from pathlib import Path
import random
import re
import struct
import sys


FIXTURE = Path(__file__).resolve().parent / "fixtures" / "compact" / "graph-schema-cases.json"
SCHEMA = "vlite.compact-graph-schema-logical"
TYPES = ("String", "Integer", "Float", "Boolean", "GeoPoint")
SEEDS = (17, 941, 2026)
LIMITS = {
    "labels": (256, "at most 256 labels"),
    "relationships": (256, "at most 256 relationships"),
    "declaration_properties": (64, "at most 64 declared properties"),
    "endpoints": (64, "at most 64 endpoints"),
    "description_bytes": (1024, "description: UTF-8 byte length"),
    "indexed_properties": (32, "at most 32 unique or indexed properties"),
    "identifier_bytes": (63, "invalid ASCII identifier"),
    "property_name_bytes": (256, "property name: UTF-8 byte length"),
    "node_labels": (8, "at most 8 distinct labels"),
    "snapshot_properties": (64, "at most 64 snapshot properties"),
    "string_bytes": (4096, "string: UTF-8 byte length"),
    "property_bytes": (65536, "properties exceed 65536 bytes"),
}


def require(condition, message):
    if not condition:
        raise ValueError(message)


def keys(value, fields, context):
    require(type(value) is dict and set(value) == set(fields.split()),
            f"{context}: expected keys {fields}")


def array(value, context):
    require(type(value) is list, f"{context}: expected array")


def integer(value, low, high, context):
    require(type(value) is int and low <= value <= high,
            f"{context}: expected integer in [{low}, {high}]")


def utf8(value, context, low=0, high=None):
    require(type(value) is str, f"{context}: expected UTF-8 string")
    try:
        encoded = value.encode("utf-8")
    except UnicodeEncodeError as error:
        raise ValueError(f"{context}: invalid UTF-8") from error
    require(low <= len(encoded) and (high is None or len(encoded) <= high),
            f"{context}: UTF-8 byte length outside [{low}, {high}]")
    return encoded


def identifier(value):
    require(type(value) is str and re.fullmatch(r"[A-Za-z0-9_]{1,63}", value),
            "invalid ASCII identifier")


def name_key(row):
    return row["name"].encode("utf-8")


def distinct_rows(rows, context):
    array(rows, context)
    seen = set()
    for row in rows:
        require(type(row) is dict and "name" in row, f"{context}: expected named object")
        name = utf8(row["name"], "property name", 1, 256)
        require(name not in seen, f"{context}: duplicate property name")
        seen.add(name)
    return sorted(rows, key=name_key)


def declarations(rows):
    array(rows, "declarations")
    seen, result, counts, indexed = set(), [], {"Label": 0, "Relationship": 0}, 0
    for row in rows:
        require(type(row) is dict and type(row.get("kind")) is str
                and row["kind"] in counts, "unsupported declaration kind")
        kind = row["kind"]
        fields = "kind name description properties"
        keys(row, fields + (" endpoints multiplicity" if kind == "Relationship" else ""),
             "declaration")
        identifier(row["name"])
        identity = kind, row["name"]
        require(identity not in seen, "duplicate declaration identity")
        seen.add(identity)
        counts[kind] += 1
        utf8(row["description"], "description", 0, 1024)
        properties = distinct_rows(row["properties"], "declared properties")
        require(len(properties) <= 64, "at most 64 declared properties")
        for prop in properties:
            keys(prop, "name type required unique index", "property declaration")
            require(type(prop["type"]) is str and prop["type"] in TYPES,
                    "unsupported property type")
            require(type(prop["required"]) is bool and type(prop["unique"]) is bool,
                    "property flags must be boolean")
            require(prop["index"] is None or
                    (type(prop["index"]) is str and prop["index"] in ("Exact", "Range")),
                    "unsupported property index")
            require(prop["index"] != "Range" or prop["type"] in ("Integer", "Float"),
                    "range indexes require Integer or Float")
            posting = prop["unique"] or prop["index"] is not None
            require(kind != "Relationship" or not posting,
                    "relationship properties cannot be unique or indexed")
            indexed += int(posting)
        canonical = copy.deepcopy(row)
        canonical["properties"] = copy.deepcopy(properties)
        if kind == "Relationship":
            require(type(row["multiplicity"]) is str and
                    row["multiplicity"] in ("Multi", "UniqueTriple"),
                    "unsupported multiplicity")
            array(row["endpoints"], "endpoints")
            pairs = set()
            for pair in row["endpoints"]:
                keys(pair, "source destination", "endpoint")
                identifier(pair["source"])
                identifier(pair["destination"])
                identity = pair["source"], pair["destination"]
                require(identity not in pairs, "duplicate endpoint pair")
                pairs.add(identity)
            require(len(pairs) <= 64, "at most 64 endpoints")
            canonical["endpoints"] = [
                {"source": source, "destination": destination}
                for source, destination in sorted(
                    pairs, key=lambda pair: tuple(item.encode("utf-8") for item in pair))]
        result.append(canonical)
    require(counts["Label"] <= 256, "at most 256 labels")
    require(counts["Relationship"] <= 256, "at most 256 relationships")
    require(indexed <= 32, "at most 32 unique or indexed properties")
    return sorted(result, key=lambda row: (row["kind"] != "Label", name_key(row)))


def f64(bits):
    require(type(bits) is str and re.fullmatch("[0-9a-f]{16}", bits),
            "float bits: expected 16 lowercase hex digits")
    number = struct.unpack(">d", bytes.fromhex(bits))[0]
    require(math.isfinite(number), "float must be finite")
    return number


def typed(value, named=False):
    require(type(value) is dict and type(value.get("type")) is str and
            value["type"] in TYPES, "unsupported value type")
    tag = value["type"]
    fields = {"Float": "type bits", "GeoPoint": "type latitude_bits longitude_bits"}
    keys(value, ("name " if named else "") + fields.get(tag, "type value"), "typed value")
    if named:
        utf8(value["name"], "property name", 1, 256)
    if tag == "String":
        charge = 4 + len(utf8(value["value"], "string", 0, 4096))
        decoded = value["value"]
    elif tag == "Integer":
        integer(value["value"], -(2**63), 2**63 - 1, "Integer value")
        charge, decoded = 8, value["value"]
    elif tag == "Boolean":
        require(type(value["value"]) is bool, "Boolean value must be boolean")
        charge, decoded = 1, value["value"]
    elif tag == "Float":
        charge, decoded = 8, f64(value["bits"])
    else:
        latitude, longitude = f64(value["latitude_bits"]), f64(value["longitude_bits"])
        require(-90 <= latitude <= 90 and -180 <= longitude <= 180,
                "GeoPoint coordinates out of range")
        charge, decoded = 16, (latitude, longitude)
    return (tag, decoded), charge


def properties(rows):
    array(rows, "properties")
    require(len(rows) <= 64, "at most 64 snapshot properties")
    previous, result, charge = None, {}, 4
    for row in rows:
        value, size = typed(row, named=True)
        name = row["name"].encode("utf-8")
        require(previous is None or previous < name,
                "snapshot properties must be strictly sorted and unique")
        previous = name
        charge += 2 + len(name) + 1 + size
        result[row["name"]] = value
    require(charge <= 65536, "properties exceed 65536 bytes")
    return result


def entities(rows, edge=False):
    array(rows, "edges" if edge else "nodes")
    result = {}
    for row in rows:
        keys(row, "id source relationship destination properties" if edge
             else "id labels properties", "edge" if edge else "node")
        integer(row["id"], 0, 2**64 - 1, "entity ID")
        require(row["id"] not in result, "duplicate entity ID")
        canonical = copy.deepcopy(row)
        canonical["properties"] = properties(row["properties"])
        if edge:
            integer(row["source"], 0, 2**64 - 1, "edge source")
            integer(row["destination"], 0, 2**64 - 1, "edge destination")
            identifier(row["relationship"])
        else:
            array(row["labels"], "node labels")
            for label in row["labels"]:
                identifier(label)
            canonical["labels"] = sorted(set(row["labels"]))
            require(len(canonical["labels"]) <= 8, "at most 8 distinct labels")
        result[row["id"]] = canonical
    return dict(sorted(result.items()))


def violations(schema, nodes, edges):
    labels = {row["name"]: row for row in schema if row["kind"] == "Label"}
    relationships = {row["name"]: row for row in schema if row["kind"] == "Relationship"}
    unique, triples, output = {}, {}, []
    for node in nodes.values():
        for label in node["labels"]:
            for prop in labels.get(label, {}).get("properties", []):
                value = node["properties"].get(prop["name"])
                if prop["unique"] and value is not None:
                    unique.setdefault((label, prop["name"], value), set()).add(node["id"])
    for edge in edges.values():
        key = edge["source"], edge["relationship"], edge["destination"]
        triples[key] = triples.get(key, 0) + 1

    def add(entity, row, element, rule, prop=None):
        output.append({"entity": entity, "id": row["id"], "element": element,
                       "rule": rule, "property": prop})

    def check(entity, row, element, definition):
        for prop in definition["properties"]:
            value = row["properties"].get(prop["name"])
            rule = None
            if value is None:
                if prop["required"]:
                    rule = "required"
            elif value[0] != prop["type"]:
                rule = "type"
            elif prop["unique"] and len(unique[(element, prop["name"], value)]) > 1:
                rule = "unique"
            if rule is not None:
                add(entity, row, element, rule, prop["name"])

    for node in nodes.values():
        for label in node["labels"]:
            if label in labels:
                check("node", node, label, labels[label])
    for edge in edges.values():
        relation = edge["relationship"]
        if relation not in relationships:
            continue
        definition = relationships[relation]
        check("edge", edge, relation, definition)
        if definition["endpoints"] and not any(
                pair["source"] in nodes[edge["source"]]["labels"] and
                pair["destination"] in nodes[edge["destination"]]["labels"]
                for pair in definition["endpoints"]):
            add("edge", edge, relation, "endpoints")
        if definition["multiplicity"] == "UniqueTriple" and triples[
                edge["source"], relation, edge["destination"]] > 1:
            add("edge", edge, relation, "unique_triple")
    return output


def query_parts(query):
    require(type(query) is dict and type(query.get("op")) is str and
            query["op"] in ("Eq", "Range"), "unsupported query operation")
    keys(query, "label op property " + ("value" if query["op"] == "Eq" else "min max"),
         "query")
    if query["label"] is not None:
        identifier(query["label"])
    utf8(query["property"], "query property", 1, 256)
    if query["op"] == "Eq":
        return typed(query["value"])[0], None
    low = None if query["min"] is None else typed(query["min"])[0]
    high = None if query["max"] is None else typed(query["max"])[0]
    require(low is not None or high is not None, "range requires min or max")
    require(all(bound is None or bound[0] in ("Integer", "Float") for bound in (low, high)),
            "range requires numeric bounds")
    require(low is None or high is None or (low[0] == high[0] and low[1] <= high[1]),
            "range bounds must have same numeric type and min <= max")
    return low, high


def scan_match(value, query, low, high):
    if value is None:
        return False
    if query["op"] == "Eq":
        return value == low
    return all(bound is None or
               (value[0] == bound[0] and
                (value[1] >= bound[1] if minimum else value[1] <= bound[1]))
               for bound, minimum in ((low, True), (high, False)))


def lookup(query, schema, nodes):
    low, high = query_parts(query)
    definition = next((row for row in schema if row["kind"] == "Label"
                       and row["name"] == query["label"]), None)
    prop = next((prop for prop in definition["properties"]
                 if prop["name"] == query["property"]), None) if definition else None
    index = "Scan"
    if prop is not None:
        if query["op"] == "Eq" and (prop["unique"] or prop["index"] == "Exact"):
            index = "Exact"
        elif query["op"] == "Range" and prop["index"] == "Range":
            index = "Range"
    eligible = [node for node in nodes.values()
                if query["label"] is None or query["label"] in node["labels"]]
    scanned = [node["id"] for node in eligible if scan_match(
        node["properties"].get(query["property"]), query, low, high)]
    if index == "Scan":
        return {"ids": scanned, "index": index}
    postings = {}
    for node in eligible:
        value = node["properties"].get(query["property"])
        if value is not None:
            postings.setdefault(value, set()).add(node["id"])
    if index == "Exact":
        indexed = sorted(postings.get(low, set()))
    else:
        bound_type = (low or high)[0]
        minimum = None if low is None else low[1]
        maximum = None if high is None else high[1]
        found = set()
        for (tag, number), ids in postings.items():
            if tag == bound_type and (minimum is None or minimum <= number) and (
                    maximum is None or number <= maximum):
                found.update(ids)
        indexed = sorted(found)
    require(indexed == scanned, "derived postings disagree with independent scan")
    return {"ids": indexed, "index": index}


def snapshot(value):
    keys(value, "declarations nodes edges queries", "input")
    schema = declarations(value["declarations"])
    nodes, edges = entities(value["nodes"]), entities(value["edges"], edge=True)
    require(all(edge["source"] in nodes and edge["destination"] in nodes
                for edge in edges.values()), "dangling edge")
    array(value["queries"], "queries")
    errors = violations(schema, nodes, edges)
    require(not errors or not value["queries"], "queries require a conforming snapshot")
    return {"declarations": schema, "conforms": not errors, "violations": errors,
            "results": [lookup(query, schema, nodes) for query in value["queries"]]}


def canonical_json(value):
    return json.dumps(value, ensure_ascii=True, sort_keys=True, separators=(",", ":"))


def unique_object(pairs):
    result = {}
    for name, value in pairs:
        require(name not in result, f"duplicate JSON key {name!r}")
        result[name] = value
    return result


def reject_number(value):
    raise ValueError(f"invalid non-integer JSON number {value!r}")


def parse_json(text):
    return json.loads(text, object_pairs_hook=unique_object,
                      parse_constant=reject_number, parse_float=reject_number)


def rejection(action, diagnostic):
    try:
        action()
    except ValueError as error:
        require(diagnostic in str(error),
                f"expected rejection {diagnostic!r}, got {str(error)!r}")
    else:
        raise ValueError(f"malformed input accepted: {diagnostic}")


def validate_expected(value):
    keys(value, "declarations conforms violations results", "expected")
    require(type(value["conforms"]) is bool, "expected conforms must be boolean")
    require(canonical_json(declarations(value["declarations"])) ==
            canonical_json(value["declarations"]), "expected declarations are not canonical")
    array(value["violations"], "expected violations")
    require(value["conforms"] == (not value["violations"]), "expected conformance mismatch")
    for row in value["violations"]:
        keys(row, "entity id element rule property", "expected violation")
        require(type(row["entity"]) is str and row["entity"] in ("node", "edge"),
                "expected entity kind")
        integer(row["id"], 0, 2**64 - 1, "expected violation ID")
        identifier(row["element"])
        require(type(row["rule"]) is str and
                row["rule"] in ("required", "type", "unique", "endpoints", "unique_triple"),
                "expected violation rule")
        if row["property"] is not None:
            utf8(row["property"], "expected property", 1, 256)
    array(value["results"], "expected results")
    for row in value["results"]:
        keys(row, "ids index", "expected result")
        require(type(row["index"]) is str and row["index"] in ("Scan", "Exact", "Range"),
                "expected index choice")
        array(row["ids"], "expected IDs")
        for item in row["ids"]:
            integer(item, 0, 2**64 - 1, "expected result ID")
        require(row["ids"] == sorted(set(row["ids"])), "expected IDs not sorted and unique")


def validate_fixture(root):
    keys(root, "schema version cases invalid_cases boundaries", "fixture")
    require(root["schema"] == SCHEMA, "unsupported fixture schema")
    integer(root["version"], 1, 1, "fixture version")
    names = set()
    for group in ("cases", "invalid_cases", "boundaries"):
        array(root[group], group)
        require(bool(root[group]), f"{group}: expected nonempty array")
        for row in root[group]:
            keys(row, {"cases": "name purpose input expected",
                       "invalid_cases": "name input error",
                       "boundaries": "name limit error"}[group], group)
            utf8(row["name"], "case name", 1)
            require(row["name"] not in names, "duplicate fixture case name")
            names.add(row["name"])
            if group == "cases":
                utf8(row["purpose"], "purpose", 1)
                validate_expected(row["expected"])
                require(canonical_json(snapshot(row["input"])) == canonical_json(row["expected"]),
                        f"{row['name']}: expected answer mismatch")
            elif group == "invalid_cases":
                utf8(row["error"], "error", 1)
                rejection(lambda row=row: snapshot(row["input"]), row["error"])
            else:
                require(row["name"] in LIMITS, "unknown boundary")
                limit, error = LIMITS[row["name"]]
                integer(row["limit"], limit, limit, "boundary limit")
                require(row["error"] == error, "boundary diagnostic mismatch")
    require({row["name"] for row in root["boundaries"]} == set(LIMITS),
            "missing boundary coverage")
    return len(root["cases"]), len(root["invalid_cases"])


def empty_input():
    return {"declarations": [], "nodes": [], "edges": [], "queries": []}


def label_definition(name="L", props=None):
    return {"kind": "Label", "name": name, "description": "", "properties": props or []}


def relationship_definition(name="R"):
    return {"kind": "Relationship", "name": name, "description": "", "properties": [],
            "endpoints": [], "multiplicity": "Multi"}


def property_definition(name="p", tag="Integer", index=None, unique=False):
    return {"name": name, "type": tag, "required": False, "unique": unique, "index": index}


def boundary_input(name, count):
    value = empty_input()
    if name in ("labels", "relationships"):
        constructor = label_definition if name == "labels" else relationship_definition
        value["declarations"] = [constructor(f"N{i:03}") for i in range(count)]
    elif name == "declaration_properties":
        value["declarations"] = [label_definition(props=[
            property_definition(f"p{i:03}") for i in range(count)])]
    elif name == "indexed_properties":
        # Unique + Range still counts once; two labels make the budget global.
        value["declarations"] = [
            label_definition(f"L{group}", [
                property_definition(f"p{i:03}", index="Range", unique=True)
                for i in range(count) if i % 2 == group]) for group in range(2)]
    elif name == "endpoints":
        relationship = relationship_definition()
        relationship["endpoints"] = [{"source": f"S{i:03}", "destination": "D"}
                                     for i in range(count)]
        value["declarations"] = [relationship]
    elif name == "description_bytes":
        definition = label_definition()
        definition["description"] = "\u00e9" * (count // 2) + "x" * (count % 2)
        value["declarations"] = [definition]
    elif name == "identifier_bytes":
        value["declarations"] = [label_definition("A" * count),
                                 relationship_definition("R" * count)]
    elif name == "property_name_bytes":
        value["declarations"] = [label_definition(props=[
            property_definition("\u00e9" * (count // 2) + "x" * (count % 2))])]
    else:
        node = {"id": 0, "labels": [], "properties": []}
        value["nodes"] = [node]
        if name == "node_labels":
            node["labels"] = [f"L{i}" for i in range(count)]
        elif name == "snapshot_properties":
            node["properties"] = [{"name": f"p{i:03}", "type": "Boolean", "value": False}
                                  for i in range(count)]
        elif name == "string_bytes":
            node["properties"] = [{"name": "s", "type": "String", "value": "x" * count}]
        elif name == "property_bytes":
            # 4 count bytes + sixteen (10 overhead + string bytes) = count.
            # All names have three bytes; no value exceeds its 4096-byte cap.
            total_strings = count - 4 - 16 * 10
            lengths = [4086] * 15 + [total_strings - 15 * 4086]
            node["properties"] = [{"name": f"s{i:02}", "type": "String", "value": "x" * size}
                                  for i, size in enumerate(lengths)]
        else:
            raise ValueError(f"unknown boundary {name}")
    return value


def shuffled_input(value, seed):
    value = copy.deepcopy(value)
    rng = random.Random(seed)
    for field in ("declarations", "nodes", "edges"):
        rng.shuffle(value[field])
    for row in value["declarations"]:
        rng.shuffle(row["properties"])
        if row["kind"] == "Relationship":
            rng.shuffle(row["endpoints"])
    for node in value["nodes"]:
        rng.shuffle(node["labels"])
    return value


def self_test(root):
    counts = validate_fixture(root)
    shuffles = 0
    for case in root["cases"]:
        for seed in SEEDS:
            actual = snapshot(shuffled_input(case["input"], seed))
            require(canonical_json(actual) == canonical_json(case["expected"]),
                    f"{case['name']}: shuffle {seed} changed semantics")
            shuffles += 1
    boundaries = 0
    for row in root["boundaries"]:
        good = boundary_input(row["name"], row["limit"])
        require(snapshot(good)["conforms"], f"{row['name']}: boundary did not conform")
        bad = boundary_input(row["name"], row["limit"] + 1)
        require(canonical_json(good) != canonical_json(bad), "ineffective boundary mutation")
        rejection(lambda bad=bad: snapshot(bad), row["error"])
        boundaries += 2

    mutations = 0
    # Start from independent tiny conforming graphs, not oracle-generated goldens.
    base = empty_input()
    prop = property_definition(unique=True)
    prop["required"] = True
    base["declarations"] = [label_definition(props=[prop])]
    base["nodes"] = [{"id": 0, "labels": ["L"], "properties": [
        {"name": "p", "type": "Integer", "value": 9007199254740992}]},
        {"id": 1, "labels": ["L"], "properties": [
            {"name": "p", "type": "Integer", "value": 9007199254740993}]}]
    base["queries"] = [{"label": "L", "op": "Eq", "property": "p",
                       "value": {"type": "Integer", "value": 9007199254740993}}]
    initial = snapshot(base)
    require(initial["conforms"] and initial["results"] == [{"ids": [1], "index": "Exact"}],
            "independent exact-integer assertion failed")
    variants = []
    changed = copy.deepcopy(base)
    changed["queries"][0]["value"]["value"] -= 1
    variants.append((changed, True, [{"ids": [0], "index": "Exact"}], None))
    changed = copy.deepcopy(base)
    changed["declarations"][0]["properties"][0]["unique"] = False
    variants.append((changed, True, [{"ids": [1], "index": "Scan"}], None))
    for action, rule in (("missing", "required"), ("wrong_type", "type"), ("collision", "unique")):
        changed = copy.deepcopy(base)
        changed["queries"] = []
        if action == "missing":
            changed["nodes"][1]["properties"] = []
        elif action == "wrong_type":
            changed["nodes"][1]["properties"] = [{"name": "p", "type": "Boolean", "value": True}]
        else:
            changed["nodes"][1]["properties"][0]["value"] -= 1
        variants.append((changed, False, [], rule))
    for changed, conforms, results, rule in variants:
        require(canonical_json(changed) != canonical_json(base), "ineffective input mutation")
        actual = snapshot(changed)
        require(canonical_json(actual) != canonical_json(initial), "mutation did not change result")
        require(actual["conforms"] == conforms and actual["results"] == results,
                "mutation expected answer mismatch")
        if rule:
            require(actual["violations"] and all(row["rule"] == rule
                                               for row in actual["violations"]),
                    "mutation did not trigger intended constraint")
        mutations += 1
    structure = empty_input()
    relation = relationship_definition()
    relation["multiplicity"] = "UniqueTriple"
    relation["endpoints"] = [{"source": "A", "destination": "B"}]
    structure["declarations"] = [relation]
    structure["nodes"] = [{"id": 0, "labels": ["A"], "properties": []},
                          {"id": 1, "labels": ["B"], "properties": []}]
    structure["edges"] = [{"id": 0, "source": 0, "relationship": "R",
                           "destination": 1, "properties": []}]
    require(snapshot(structure)["conforms"], "independent structural baseline failed")
    for change, rule in (("reverse", "endpoints"), ("parallel", "unique_triple")):
        changed = copy.deepcopy(structure)
        if change == "reverse":
            changed["edges"][0].update(source=1, destination=0)
        else:
            edge = copy.deepcopy(changed["edges"][0])
            edge["id"] = 1
            changed["edges"].append(edge)
        require(canonical_json(changed) != canonical_json(structure),
                "ineffective structural mutation")
        actual = snapshot(changed)
        expected_ids = [0] if change == "reverse" else [0, 1]
        require(not actual["conforms"] and actual["violations"] == [
            {"entity": "edge", "id": item, "element": "R", "rule": rule, "property": None}
            for item in expected_ids], "structural mutation missed intended violations")
        repaired = copy.deepcopy(changed)
        if change == "reverse":
            repaired["declarations"][0]["endpoints"] = []
        else:
            repaired["declarations"][0]["multiplicity"] = "Multi"
        require(canonical_json(repaired) != canonical_json(changed) and
                snapshot(repaired)["conforms"], "structural rule mutation was ineffective")
        mutations += 2
    rejected = [
        ("duplicate declaration identity", lambda value: value["declarations"].append(
            copy.deepcopy(value["declarations"][0]))),
        ("property flags must be boolean", lambda value: value["declarations"][0][
            "properties"][0].update(required=1)),
        ("Integer value", lambda value: value["nodes"][0]["properties"][0].update(value=True)),
        ("expected keys", lambda value: value.update(extra=0)),
    ]
    for error, mutate in rejected:
        changed = copy.deepcopy(base)
        mutate(changed)
        require(canonical_json(changed) != canonical_json(base), "ineffective rejection mutation")
        rejection(lambda changed=changed: snapshot(changed), error)
        mutations += 1
    malformed_fixture_checks = 0
    for mutate, error in (
            (lambda value: value.update(extra=0), "expected keys"),
            (lambda value: value.update(version=True), "fixture version"),
            (lambda value: value["cases"].append(copy.deepcopy(value["cases"][0])),
             "duplicate fixture case name"),
            (lambda value: value["cases"][0]["expected"].update(conforms=False),
             "expected conformance mismatch"),
            (lambda value: value["cases"][0]["expected"]["declarations"].append(
                label_definition("Unexpected")), "expected answer mismatch"),
            (lambda value: value["invalid_cases"][0].update(error="not the actual error"),
             "expected rejection"),
            (lambda value: value["boundaries"][0].update(limit=True), "boundary limit"),
            (lambda value: value["cases"][0].update(extra=0), "expected keys"),
    ):
        changed = copy.deepcopy(root)
        mutate(changed)
        require(canonical_json(changed) != canonical_json(root), "ineffective fixture mutation")
        rejection(lambda changed=changed: validate_fixture(changed), error)
        malformed_fixture_checks += 1
    for text, error in (
            ('{"schema":1,"schema":2}', "duplicate JSON key"),
            ('{"nested":{"x":1,"x":2}}', "duplicate JSON key"),
            ('{"value":NaN}', "invalid non-integer JSON number"),
            ('{"value":Infinity}', "invalid non-integer JSON number"),
            ('{"value":1.0}', "invalid non-integer JSON number"),
            ('{"value":1e0}', "invalid non-integer JSON number"),
            ('{', "Expecting property name"),
    ):
        rejection(lambda text=text: parse_json(text), error)
        malformed_fixture_checks += 1
    print(f"self-test: {counts[0]} goldens, {counts[1]} malformed inputs, "
          f"{shuffles} seeded shuffles, {boundaries} boundary checks, "
          f"{mutations} effective mutations, {malformed_fixture_checks} fixture/JSON rejections")


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--self-test", action="store_true")
    parser.add_argument("--fixtures", type=Path, default=FIXTURE)
    args = parser.parse_args()
    try:
        root = parse_json(args.fixtures.read_text(encoding="utf-8"))
        if args.self_test:
            self_test(root)
        else:
            valid, invalid = validate_fixture(root)
            print(f"schema oracle: {valid} goldens, {invalid} malformed inputs")
    except (OSError, ValueError) as error:
        print(f"schema oracle error: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
