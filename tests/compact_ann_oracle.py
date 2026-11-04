"""Independent P3c C1 byte reference; never imports or invokes Rust.

Layouts: p3c-compact-ann.md C/D, settings F.1; envelope: storage contract 4.
Rotation arithmetic, real file authentication and lifecycle are NOT modeled.
Rotated reconstruction requires explicit independently supplied row witnesses.
"""

import argparse
import copy
import hashlib
import json
import math
from pathlib import Path
import struct
import sys


ROOT = Path(__file__).resolve().parent / "fixtures" / "compact" / "ann-v1"
MAX_BODY = 4 * 1024 * 1024
TARGET = 1024 * 1024
U64 = (1 << 64) - 1
KINDS = (0x0200, 0x0201, 0x0202, 0x0203)
MUTANT = None


class Invalid(ValueError):
    pass


def require(condition, reason):
    if not condition:
        raise Invalid(reason)


def integer(value, maximum, reason):
    require(type(value) is int and 0 <= value <= maximum, reason)
    return value


def checked_add(a, b):
    integer(a, U64, "addition operand")
    integer(b, U64, "addition operand")
    require(a <= U64 - b, "addition overflow")
    return a + b


def checked_mul(a, b):
    integer(a, U64, "multiplication operand")
    integer(b, U64, "multiplication operand")
    require(not b or a <= U64 // b, "multiplication overflow")
    return a * b


def pack(fmt, *values):
    # The endian mutant intentionally selects the opposite byte order: on a
    # little-endian host a literal native-endian mutation would be equivalent.
    return struct.pack((">" if MUTANT == "native-endian" else "<") + fmt, *values)


def unpack(fmt, data, offset=0):
    return struct.unpack_from("<" + fmt, data, offset)


def bits(value):
    return struct.pack("<d", value)


def tagged(function):
    def run(*args, **kwargs):
        try:
            return {"outcome": "ok", **function(*args, **kwargs)}
        except (Invalid, struct.error, OverflowError) as error:
            return {"outcome": "corrupt", "reason": str(error)}
    return run


def raw(result):
    require(result["outcome"] == "ok", result.get("reason", result["outcome"]))
    return result


def hex_bytes(value, reason="hex bytes"):
    require(isinstance(value, str) and len(value) % 2 == 0
            and all(c in "0123456789abcdef" for c in value), reason)
    return bytes.fromhex(value)


def sha(transcript):
    return hashlib.sha256(transcript).digest()


def section_stream(block):
    sections = block["sections"]
    if MUTANT == "omit-set-section":
        sections = sections[:-1]
    return (pack("HHIIH", block["kind"], block.get("version", 1),
                 block["partition"], block["count"], len(sections))
            + b"".join(pack("I", len(s)) + s for s in sections))


@tagged
def digest_source(collection, dimensions, precision, blocks):
    integer(collection, 65535, "collection")
    require(1 <= dimensions <= 65536 and precision in (1, 2), "source shape")
    sequence = list(blocks)
    if MUTANT == "omit-B-section":
        sequence = copy.deepcopy(sequence)
        sequence[0]["sections"] = sequence[0]["sections"][:-1]
    transcript = (b"VLCANN-SOURCE-1" + pack("HIB", collection, dimensions, precision)
                  + b"".join(section_stream(block) for block in sequence))
    return {"digest": sha(transcript), "transcript": transcript}


@tagged
def digest_generation(lineage, sequence, collection, source_digest):
    require(len(source_digest) == 32, "B length")
    integer(lineage, U64, "lineage")
    integer(sequence, U64, "sequence")
    prefix = b"" if MUTANT == "omit-lineage" else pack("Q", lineage)
    transcript = (b"VLCANN-GENERATION-1" + prefix + pack("QH", sequence, collection)
                  + source_digest)
    return {"digest": sha(transcript), "transcript": transcript}


def components(row, dimensions, precision):
    data = hex_bytes(row["bits"])
    width = 4 if precision == 1 else 2
    require(len(data) == dimensions * width, "source component count")
    values = list(unpack(("f" if precision == 1 else "e") * dimensions, data))
    require(all(math.isfinite(x) for x in values), "source finite components")
    return values, data


@tagged
def digest_content(collection, dimensions, precision, rows):
    require(1 <= dimensions <= 65536 and precision in (1, 2), "content shape")
    require(len(rows) <= 1000000, "source row cap")
    transcript = b"VLCANN-CONTENT-1" + pack("HBIQ", collection, precision, dimensions, len(rows))
    previous = -1
    for row in rows:
        ident = integer(row["id"], U64, "source ID")
        require(ident > previous, "source ID order")
        previous = ident
        revision = integer(row["revision"], U64, "source revision")
        expiry = row.get("expiry")
        if expiry is not None:
            integer(expiry, U64, "expiry")
        _, data = components(row, dimensions, precision)
        if MUTANT == "drop-signed-zero":
            width = 4 if precision == 1 else 2
            negative_zero = b"\x00" * (width - 1) + b"\x80"
            data = b"".join(b"\x00" * width if data[i:i+width] == negative_zero
                            else data[i:i+width] for i in range(0, len(data), width))
        transcript += pack("QQBQ", ident, revision, int(expiry is not None), expiry or 0) + data
    return {"digest": sha(transcript), "transcript": transcript}


@tagged
def digest_set(settings, parameters, blocks):
    ordered = sorted(blocks, key=lambda b: (b["kind"], b["partition"]))
    transcript = b"VLCANN-SET-1" + settings + parameters
    for block in ordered:
        transcript += section_stream(block)
        if MUTANT == "hash-padding":
            transcript += b"\x00"
    return {"digest": sha(transcript), "transcript": transcript}


def envelope_guard(source_digest):
    require(len(source_digest) == 32, "B length")
    return source_digest[8:16] if MUTANT == "wrong-source-bytes" else source_digest[:8]


@tagged
def encode_envelope(kind, sections, collection=7, partition=0, count=1,
                    source_digest=bytes(32), position=0, version=1):
    integer(kind, 65535, "kind")
    integer(collection, 65535, "collection")
    integer(partition, (1 << 32) - 1, "partition")
    integer(count, (1 << 32) - 1, "count")
    integer(position, U64, "body file position")
    require(1 <= len(sections) <= 64, "section count")
    offset = 32 + 8 * len(sections)
    table, payload = [], b""
    for section in sections:
        padding = -(position + offset) % 64
        offset += padding
        table.append((offset, len(section)))
        payload += bytes(padding) + section
        offset = checked_add(offset, len(section))
    require(offset <= MAX_BODY, "body cap")
    header = (pack("HHHHQ", kind, version, 0, len(sections), collection | (partition << 16))
              + envelope_guard(source_digest) + pack("II", table[0][0], 0))
    body = header + b"".join(pack("II", *entry) for entry in table) + payload
    return {"block": {"kind": kind, "version": version, "collection": collection,
                      "partition": partition, "count": count, "body": body}}


def outer(block):
    body = block["body"]
    require(isinstance(body, bytes) and 40 <= len(body) <= MAX_BODY, "body length/cap")
    kind, version, flags, count, obj = unpack("HHHHQ", body)
    guard = body[16:24]
    first, reserved = unpack("II", body, 24)
    require(kind == block["kind"] and version == block["version"] and version >= 1,
            "prefix kind/version")
    require(flags == 0 and reserved == 0, "envelope reserved/flags")
    require(obj == block["collection"] | (block["partition"] << 16)
            and obj >> 48 == 0, "envelope object")
    integer(block["count"], (1 << 32) - 1, "prefix count")
    require(1 <= count <= 64 and 32 + count * 8 <= len(body), "section table count")
    sections, previous = [], 32 + count * 8
    for i in range(count):
        offset, length = unpack("II", body, 32 + i * 8)
        require(offset >= previous and offset - previous <=
                (64 if MUTANT == "allow-padding-64" else 63), "section padding/overlap")
        require(not any(body[previous:offset]), "nonzero padding")
        end = checked_add(offset, length)
        require(end <= len(body), "section length/offset")
        require(i != 0 or offset == first, "first section offset")
        sections.append(body[offset:end])
        previous = end
    require(MUTANT == "accept-trailing" or previous == len(body), "trailing bytes")
    return {"kind": kind, "version": version, "collection": block["collection"],
            "partition": block["partition"], "count": block["count"],
            "guard": guard, "sections": sections}


def settings_fields(data):
    require(len(data) >= 2, "settings version")
    version = unpack("H", data)[0]
    if version != 1:
        return None
    require(len(data) == 32, "settings length")
    length = unpack("H", data, 2)[0]
    profile, rerank, flags, ceiling, seed, family, rounds = unpack("BBHIQHH", data, 4)
    require(length == 32 and profile in (0, 1, 2) and 1 <= rerank <= 16
            and flags == 0 and ceiling == 100000 and data[24:] == bytes(8), "settings fields")
    require((family, rounds) == ((1, 8) if profile == 2 else (0, 0)), "rotation settings")
    require(profile == 2 or seed == 0, "nonrotated seed")
    return {"profile": profile, "seed": seed, "rerank": rerank}


@tagged
def encode_settings(profile=1, seed=0, rerank=4):
    data = pack("HHBBHIQHH", 1, 32, profile, rerank, 0, 100000, seed,
                1 if profile == 2 else 0, 8 if profile == 2 else 0) + bytes(8)
    settings_fields(data)
    return {"bytes": data}


def canonical_minimum(value):
    if MUTANT == "encode-raw-minimum":
        return value
    if MUTANT == "canonicalize-negative-minimum":
        return 0.0 if value <= 0 else value
    return 0.0 if value == 0 else value


def scalar_parameters(data, dimensions, stored=True):
    require(len(data) == 8 + dimensions * 16, "scalar parameters length")
    require(unpack("II", data) == (dimensions, 0), "scalar parameters header")
    values = unpack("d" * (2 * dimensions), data, 8)
    minima, scales = list(values[:dimensions]), list(values[dimensions:])
    require(all(math.isfinite(v) for v in values), "scalar parameters finite")
    require(not stored or MUTANT == "accept-negative-zero-minimum"
            or all(bits(v) != bytes.fromhex("0000000000000080") for v in minima),
            "noncanonical scalar minimum")
    require(all(v >= 0 and not math.copysign(1, v) < 0 for v in scales), "scalar scale")
    return minima, scales


@tagged
def train_scalar(dimensions, rows, precision=1, metric=0, canonicalize=True):
    require(1 <= dimensions <= 65536 and metric in (0, 1), "training shape/metric")
    minima, maxima = None, None
    for row in rows:
        values, _ = components(row, dimensions, precision)
        if metric == 1 and not any(v != 0 for v in values):
            continue
        if minima is None:
            minima, maxima = values[:], values[:]
        else:
            minima = [min(a, b) for a, b in zip(minima, values)]
            maxima = [max(a, b) for a, b in zip(maxima, values)]
    if minima is None:
        minima, scales = [0.0] * dimensions, [0.0] * dimensions
    else:
        scales = [(hi - lo) / 255.0 for lo, hi in zip(minima, maxima)]
    if canonicalize and MUTANT != "first-equal-minimum":
        minima = [canonical_minimum(value) for value in minima]
    scales = [0.0 if value == 0 else value for value in scales]
    parameters = pack("II", dimensions, 0) + b"".join(bits(v) for v in minima + scales)
    return {"parameters": parameters}


@tagged
def encode_scalar_vector(values, parameters):
    minima, scales = scalar_parameters(parameters, len(values))
    codes = []
    for value, low, scale in zip(values, minima, scales):
        require(math.isfinite(value), "scalar input finite")
        x = 0.0 if scale == 0 else (value - low) / scale
        code = round(x) if MUTANT == "half-even" else (
            math.floor(x + 0.5) if x >= 0 else math.ceil(x - 0.5))
        codes.append(min(255, max(0, code)))
    return {"codes": bytes(codes)}


def scalar_norm(codes, parameters):
    minima, scales = scalar_parameters(parameters, len(codes))
    total = 0.0
    for code, low, scale in zip(codes, minima, scales):
        value = low + float(code) * scale
        total += value * value
    return math.sqrt(total)


@tagged
def encode_scalar(start, dimensions, codes, parameters, **envelope):
    require(codes and all(len(row) == dimensions for row in codes), "scalar rows")
    sections = [pack("IIII", start, len(codes), dimensions, 0), b"".join(codes),
                b"".join(bits(scalar_norm(row, parameters)) for row in codes)]
    return raw(encode_envelope(0x0202, sections, count=len(codes), **envelope))


@tagged
def encode_rotated(start, dimensions, signs, terms, **envelope):
    require(signs and len(signs) == len(terms), "rotated rows")
    sections = [pack("IIII", start, len(signs), dimensions, 0),
                b"".join(signs), b"".join(b"".join(hex_bytes(v) for v in row) for row in terms)]
    result = raw(encode_envelope(0x0203, sections, count=len(signs), **envelope))
    check_code(outer(result["block"]))
    return result


@tagged
def encode_topology(start, total, nodes, **envelope):
    layers, neighbors, offsets, rows = [0], [], [0], b""
    for node in nodes:
        links = node["links"]
        rows += pack("QQIB", node["id"], node["revision"], node["source"], len(links)) + bytes(3)
        for targets in links:
            neighbors.extend(targets)
            offsets.append(len(neighbors))
        layers.append(len(offsets) - 1)
    sections = [pack("IIIII", start, len(nodes), len(offsets)-1, len(neighbors), total) + bytes(12),
                rows, pack("I" * len(layers), *layers),
                pack("I" * len(offsets), *offsets),
                pack("I" * len(neighbors), *neighbors)]
    return raw(encode_envelope(0x0201, sections, count=len(nodes), **envelope))


def level_value(ident, seed, m):
    integer(ident, U64, "level ID")
    integer(seed, U64, "level seed")
    require(type(m) is int and 2 <= m <= 64, "level m")
    state, level = ident ^ seed, 0
    while True:
        state = (state + 0x9e3779b97f4a7c15) & U64
        mixed = ((state ^ (state >> 30)) * 0xbf58476d1ce4e5b9) & U64
        mixed = ((mixed ^ (mixed >> 27)) * 0x94d049bb133111eb) & U64
        mixed ^= mixed >> 31
        if mixed % m or level == (17 if MUTANT == "skip-level-cap" else 16):
            return level
        level += 1


@tagged
def level(ident, seed, m):
    return {"level": level_value(ident, seed, m)}


def check_topology_data(block, m=64):
    sections = block["sections"]
    require(len(sections) == 5 and len(sections[0]) == 32, "topology sections/header")
    start, count, layers, edges, total = unpack("IIIII", sections[0])
    require(sections[0][20:] == bytes(12), "topology reserved")
    require(0 < count <= 256 and count == block["count"] and total <= 1000000
            and start + count <= total, "topology counts")
    require(count <= layers <= 17 * count and edges <= 1152 * count, "topology L/E caps")
    require([len(s) for s in sections[1:]] ==
            [24 * count, 4 * (count + 1), 4 * (layers + 1), 4 * edges], "topology lengths")
    lo = unpack("I" * (count + 1), sections[2])
    eo = unpack("I" * (layers + 1), sections[3])
    neighbors = unpack("I" * edges, sections[4])
    require(lo[0] == eo[0] == 0 and lo[-1] == layers and eo[-1] == edges, "offset sentinels")
    require(all(a <= b for a, b in zip(eo, eo[1:])), "neighbor offset order")
    nodes = []
    for index in range(count):
        ident, revision, source, levels = unpack("QQIB", sections[1], index * 24)
        require(sections[1][index*24+21:index*24+24] == bytes(3), "node reserved")
        require(1 <= levels <= 17 and lo[index+1] - lo[index] == levels, "node levels/offsets")
        require(source < 1000000 and (not nodes or nodes[-1]["id"] < ident), "node ID/source")
        links = []
        for layer in range(levels):
            at = lo[index] + layer
            targets = list(neighbors[eo[at]:eo[at+1]])
            degree = 2*m if layer == 0 else m
            if MUTANT == "degree-above":
                degree = 2*m + 1
            require(len(targets) <= degree, "degree")
            require(all(t < total or (MUTANT == "neighbor-N" and t == total)
                        for t in targets), "neighbor ordinal")
            require(start + index not in targets and len(set(targets)) == len(targets),
                    "self/duplicate neighbor")
            require(MUTANT == "reorder-neighbors" or targets == sorted(targets), "neighbor order")
            links.append(targets)
        nodes.append({"id": ident, "revision": revision, "source": source, "links": links})
    return {"start": start, "total": total, "nodes": nodes}


def check_code(block, parameters=None):
    sections = block["sections"]
    require(len(sections) == 3 and len(sections[0]) == 16, "code sections/header")
    start, count, dimensions, reserved = unpack("IIII", sections[0])
    require(reserved == 0 and 1 <= dimensions <= 65536 and 0 < count <= 4096
            and count == block["count"] and start + count <= 1000000, "code counts")
    rotated = block["kind"] == 0x0203
    stride = (dimensions + 7) // 8 if rotated else dimensions
    if rotated and MUTANT == "round-code-stride":
        stride = 1 << (stride - 1).bit_length()
    require(len(sections[1]) == count * stride
            and len(sections[2]) == count * (32 if rotated else 8), "code lengths")
    rows = []
    for i in range(count):
        code = sections[1][i*stride:(i+1)*stride]
        term_bytes = sections[2][i*(32 if rotated else 8):(i+1)*(32 if rotated else 8)]
        terms = unpack("dddd" if rotated else "d", term_bytes)
        require(MUTANT == "accept-NaN-norm" or all(math.isfinite(v) for v in terms),
                "nonfinite terms")
        require(all(terms[j] >= 0 for j in ((0, 1, 3) if rotated else (0,)))
                or MUTANT == "accept-NaN-norm", "negative term")
        if rotated:
            require(dimensions % 8 == 0 or code[-1] >> (dimensions % 8) == 0, "unused sign bits")
        elif parameters is not None:
            minima, scales = scalar_parameters(parameters, dimensions)
            require(all(scale != 0 or c == 0 for c, scale in zip(code, scales)), "constant code")
            require(term_bytes == bits(scalar_norm(code, parameters)), "wrong norm")
        rows.append((code, term_bytes))
    return {"start": start, "dimensions": dimensions, "rows": rows}


def descriptor_data(block):
    sections = block["sections"]
    require(block["partition"] == 0 and block["count"] == 1 and len(sections) == 3,
            "descriptor prefix/sections")
    binding, settings, parameters = sections
    require(len(binding) >= 10 and binding[:8] == b"VLCANN01", "binding version/magic")
    version = unpack("H", binding, 8)[0]
    if version != 1:
        return None
    require(len(binding) == 224, "binding length")
    collection, dimensions = unpack("HI", binding, 10)
    precision, metric, profile, state, reserved = unpack("BBBBI", binding, 16)
    require(collection == block["collection"] and 1 <= dimensions <= 65536
            and precision in (1, 2) and metric in (0, 1) and profile in (0, 1, 2)
            and state in (0, 1) and reserved == 0 and binding[216:] == bytes(8),
            "binding fields")
    lineage, sequence = unpack("QQ", binding, 24)
    physical, count, p = unpack("QII", binding, 136)
    q, entry, m, ef, maximum, quantizer, build = unpack("IIHHBBH", binding, 152)
    seed, rotation_seed = unpack("QQ", binding, 168)
    require(physical <= 1000000 and count <= physical and p <= 1000000 and q <= 1000000
            and 2 <= m <= 64 and m <= ef <= 4096 and maximum <= 16, "binding bounds")
    if state == 0 or count == 0:
        require(count == p == q == maximum == 0 and entry == 0xffffffff, "empty descriptor")
    else:
        require(0 < p <= count and entry < count and (0 < q <= count if profile else q == 0),
                "complete descriptor")
    if state == 0:
        require(binding[184:216] == bytes(32), "pending set digest")
    sf = settings_fields(settings)
    if sf is not None:
        require(sf["profile"] == profile and sf["seed"] == rotation_seed, "settings agreement")
    if quantizer != 1 or build != 1:
        return None
    if profile == 0:
        require(parameters == b"" and q == 0, "none parameters/codes")
    elif profile == 1:
        scalar_parameters(parameters, dimensions)
    else:
        require(len(parameters) >= 4, "rotation parameters version")
        family, algorithm = unpack("HH", parameters)
        if family != 1 or algorithm != 1:
            return None
        require(len(parameters) == 40, "rotation parameters length")
        family, algorithm, rounds, zero, dim, zero2, rs, c, s = unpack("HHHHIIQQQ", parameters)
        require((rounds, zero, dim, zero2, rs, c, s) ==
                (8, 0, dimensions, 0, rotation_seed, 0x3fe3333333333333, 0x3fe999999999999a),
                "rotation parameters")
    if sf is None:
        return None
    return {"collection": collection, "dimensions": dimensions, "precision": precision,
            "metric": metric, "profile": profile, "state": state, "lineage": lineage,
            "sequence": sequence, "G": binding[40:72], "B": binding[72:104],
            "C": binding[104:136], "physical": physical, "N": count, "P": p, "Q": q,
            "entry": entry, "m": m, "ef": ef, "max_level": maximum, "seed": seed,
            "rotation_seed": rotation_seed, "set_digest": binding[184:216],
            "settings": settings, "parameters": parameters}


@tagged
def encode_descriptor(fields, settings, parameters, **envelope):
    f = fields
    if f["profile"] == 1:
        minima, scales = scalar_parameters(parameters, f["dimensions"], stored=False)
        parameters = (pack("II", f["dimensions"], 0)
                      + b"".join(bits(canonical_minimum(v)) for v in minima)
                      + b"".join(bits(v) for v in scales))
    binding = (b"VLCANN01" + pack("HHI", 1, f["collection"], f["dimensions"])
               + pack("BBBBI", f["precision"], f["metric"], f["profile"], f["state"], 0)
               + pack("QQ", f["lineage"], f["sequence"]) + f["G"] + f["B"] + f["C"]
               + pack("QII", f["physical"], f["N"], f["P"])
               + pack("IIHHBBH", f["Q"], f["entry"], f["m"], f["ef"], f["max_level"], 1, 1)
               + pack("QQ", f["seed"], f["rotation_seed"]) + f["set_digest"] + bytes(8))
    return raw(encode_envelope(0x0200, [binding, settings, parameters],
                               collection=f["collection"], source_digest=f["B"], **envelope))


def checked_block(block, descriptor=None):
    parsed = outer(block)
    if parsed["kind"] not in KINDS:
        return {"outcome": "unknown_kind", **parsed}
    if parsed["version"] != 1:
        return {"outcome": "unsupported_version", **parsed}
    if parsed["kind"] == 0x0200:
        parsed["descriptor"] = descriptor_data(parsed)
        if parsed["descriptor"] is None:
            return {"outcome": "unsupported_version", **parsed}
    elif parsed["kind"] == 0x0201:
        parsed["topology"] = check_topology_data(parsed, descriptor["m"] if descriptor else 64)
    else:
        parsed["codes"] = check_code(parsed, descriptor["parameters"]
                                     if descriptor and parsed["kind"] == 0x0202 else None)
    return {"outcome": "ok", **parsed}


def check_block(block, descriptor=None):
    try:
        return checked_block(block, descriptor)
    except (Invalid, struct.error, OverflowError) as error:
        return {"outcome": "corrupt", "reason": str(error)}


check_descriptor = check_block
check_topology = check_block
check_scalar = check_block
check_rotated = check_block


def bind(blocks, source, requested, rotated_witness=None, image_complete=True):
    """Atomic logical installation after externally authenticated image framing."""
    try:
        require(image_complete, "missing/truncated image end")
        require(blocks, "missing descriptor")
        parsed, seen, last, expected_partition = [], set(), None, 0
        skip = False
        for block in blocks:
            identity = (block["collection"], block["kind"])
            if identity != last:
                require(identity not in seen, "resumed/out-of-order run")
                if last is not None:
                    require(identity > last, "kind/collection order")
                seen.add(identity)
                expected_partition = 0
                last = identity
            require(block["partition"] == expected_partition, "partition run")
            expected_partition += 1
            result = checked_block(block)
            skip |= result["outcome"] == "unsupported_version"
            parsed.append(result)
        descriptors = [b for b in parsed if b["kind"] == 0x0200]
        if not descriptors and all(b["outcome"] == "unknown_kind" for b in parsed):
            return {"outcome": "unknown_kind", "installed": False}
        require(len(descriptors) == 1, "missing/duplicate descriptor")
        descriptor = descriptors[0].get("descriptor")
        if descriptor is not None:
            # A supported sibling's known invariants still apply when another
            # block requests an unsupported codec. Do not mask corruption.
            for block in parsed:
                if block["outcome"] == "ok" and block["kind"] == 0x0201:
                    check_topology_data(block, descriptor["m"])
                elif block["outcome"] == "ok" and block["kind"] == 0x0202:
                    check_code(block, descriptor["parameters"])
        if skip:
            return {"outcome": "corrupt" if MUTANT == "unsupported-as-corrupt"
                    else "unsupported_version", "installed": False}
        require(descriptor is not None, "descriptor missing")
        d = descriptor
        require(all(b["collection"] == d["collection"] for b in parsed), "foreign child owner")
        require(all(b["guard"] == d["B"][:8] for b in parsed), "envelope source guard")
        require(d["G"] == raw(digest_generation(d["lineage"], d["sequence"],
                                              d["collection"], d["B"]))["digest"],
                "descriptor generation identity")
        children = [b for b in parsed if b["kind"] in KINDS[1:]]
        require(all(b["kind"] == 0x0201 or b["kind"] ==
                    (0x0202 if d["profile"] == 1 else 0x0203) for b in children),
                "unexpected code kind")
        p = [b for b in children if b["kind"] == 0x0201]
        q = [b for b in children if b["kind"] != 0x0201]
        require(len(p) == d["P"] and len(q) == d["Q"], "missing declared partition")
        if d["state"] == 0:
            require(not children, "pending children")
        else:
            expected = raw(digest_set(d["settings"], d["parameters"], children))["digest"]
            require(expected == d["set_digest"], "index-set digest")
        nodes, codes = [], []
        for b in p:
            topology = check_topology_data(b, d["m"])
            require(topology["start"] == len(nodes) and topology["total"] == d["N"],
                    "topology gap/overlap/global N")
            nodes.extend(topology["nodes"])
        for b in q:
            code = check_code(b, d["parameters"] if d["profile"] == 1 else None)
            require(code["start"] == len(codes) and code["dimensions"] == d["dimensions"],
                    "code gap/overlap/dimensions")
            codes.extend(code["rows"])
        require(len(nodes) == d["N"] and (len(codes) == d["N"] if d["profile"] else not codes),
                "coverage")
        require(all(a["id"] < b["id"] for a, b in zip(nodes, nodes[1:])), "global ID order")
        for ordinal, node in enumerate(nodes):
            require(len(node["links"]) == level_value(node["id"], d["seed"], d["m"]) + 1,
                    "recomputed level")
            for layer, targets in enumerate(node["links"]):
                require(all(t < len(nodes) and len(nodes[t]["links"]) > layer for t in targets),
                        "wrong-level neighbor")
        if nodes:
            maximum = max(len(n["links"]) - 1 for n in nodes)
            entry = next(i for i, n in enumerate(nodes) if len(n["links"]) == maximum + 1)
            require(d["max_level"] == maximum and d["entry"] == entry, "entry/max level")
        source_b = raw(digest_source(source["collection"], source["dimensions"],
                                     source["precision"], source["blocks"]))["digest"]
        source_g = raw(digest_generation(source["lineage"], source["sequence"],
                                         source["collection"], source_b))["digest"]
        source_c = raw(digest_content(source["collection"], source["dimensions"],
                                      source["precision"], source["rows"]))["digest"]
        require(set(requested) == {"metric", "profile", "m", "ef", "seed",
                                   "rotation_seed", "settings"}, "requested settings fields")
        matches = (d["B"] == source_b and d["G"] == source_g and d["C"] == source_c
                   and d["collection"] == source["collection"]
                   and d["dimensions"] == source["dimensions"]
                   and d["precision"] == source["precision"]
                   and d["physical"] == len(source["rows"])
                   and d["lineage"] == source["lineage"] and d["sequence"] == source["sequence"]
                   and all(d[key] == value for key, value in requested.items()))
        if not matches:
            return {"outcome": "corrupt" if MUTANT == "stale-as-corrupt" else "stale",
                    "installed": False}
        if d["state"] == 0:
            return {"outcome": "ok", "installed": False}
        eligible = [(i, row) for i, row in enumerate(source["rows"])
                    if d["metric"] == 0 or any(v != 0 for v in
                                              components(row, d["dimensions"], d["precision"])[0])]
        require(len(eligible) == len(nodes), "eligibility coverage")
        parameters = None
        if d["profile"] == 1:
            parameters = raw(train_scalar(d["dimensions"], source["rows"],
                                          d["precision"], d["metric"],
                                          canonicalize=MUTANT != "bind-raw-minimum"))["parameters"]
            require(parameters == d["parameters"], "retrained bounds")
        for index, (node, (ordinal, row)) in enumerate(zip(nodes, eligible)):
            require((node["id"], node["revision"], node["source"]) ==
                    (row["id"], row["revision"], ordinal), "source ID/revision/ordinal")
            if d["profile"] == 1:
                values, _ = components(row, d["dimensions"], d["precision"])
                require(raw(encode_scalar_vector(values, parameters))["codes"] == codes[index][0],
                        "source codes")
            elif d["profile"] == 2:
                require(rotated_witness is not None and len(rotated_witness) == len(nodes),
                        "explicit rotated reconstruction witness required")
                witness = rotated_witness[index]
                require(codes[index] == (hex_bytes(witness["signs"]),
                        b"".join(hex_bytes(v) for v in witness["terms"])), "rotated witness")
        return {"outcome": "ok", "installed": True, "nodes": len(nodes)}
    except (Invalid, struct.error, OverflowError) as error:
        return {"outcome": "corrupt", "installed": False, "reason": str(error)}


@tagged
def check_image_end(prefixes, body):
    """Validate the contract's end body over externally authenticated prefixes."""
    require(all(isinstance(p, bytes) and len(p) == 36 for p in prefixes), "image prefix width")
    require(isinstance(body, bytes) and len(body) == 80, "image end length")
    require(unpack("HHHHQ", body) == (2, 1, 1, 1, 0) and body[16:24] == bytes(8)
            and unpack("IIII", body, 24) == (40, 0, 40, 40), "image end envelope")
    require(unpack("Q", body, 40)[0] == len(prefixes), "image end count")
    require(body[48:] == hashlib.sha256(b"".join(prefixes)).digest(), "image prefix digest")
    return {"frames": len(prefixes)}


@tagged
def partition_plan(kind, rows):
    """Rows are bounded size diagnostics, never allocations from on-disk counts."""
    require(kind in KINDS[1:], "partition kind")
    maximum = (257 if MUTANT == "split-257" else 256) if kind == 0x0201 else 4096
    sections = 5 if kind == 0x0201 else 3
    overhead = 32 + sections * 8 + sections * 63
    # Topology includes fixed header and the two terminal offset sentinels.
    fixed = 40 if kind == 0x0201 else 16
    result, start = [], 0
    while start < len(rows):
        end, size = start, overhead + fixed
        while end < len(rows) and end - start < maximum:
            row = rows[end]
            integer(row, U64, "row bytes")
            candidate = checked_add(size, row)
            if candidate > TARGET and end > start:
                break
            require(candidate <= MAX_BODY, "HnswRebuildTooLarge")
            size, end = candidate, end + 1
        result.append({"start": start, "count": end-start, "worst_body": size})
        start = end
    return {"partitions": result}


def unique_object(pairs):
    obj = {}
    for key, value in pairs:
        require(key not in obj, "duplicate JSON key")
        obj[key] = value
    return obj


def load_json(path):
    return json.loads(path.read_text(encoding="utf-8"), object_pairs_hook=unique_object,
                      parse_constant=lambda value: (_ for _ in ()).throw(Invalid(value)))


def canonical(value):
    return json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=True,
                      allow_nan=False).encode("ascii")


def fixture_block(value):
    return {**{k: value[k] for k in ("kind", "version", "collection", "partition", "count")},
            "body": hex_bytes(value["body_hex"])}


def fixture_source(value):
    result = copy.deepcopy(value)
    for block in result["blocks"]:
        block["sections"] = [hex_bytes(v) for v in block["sections"]]
    return result


def assemble_frame(document, source, name):
    """Fixed offset ledger, independent of the encoder's alignment calculation."""
    rotated = name.startswith("rotated")
    values = {**source, "precision": f'{source["precision"]:02x}',
              "profile": "02" if rotated else "01",
              "guard": source["B"][:16],
              "set": document["rotated_set" if rotated else "scalar_set"],
              "settings": document["rotated_settings" if rotated else "settings"],
              "parameters": document["rotated_parameters" if rotated else "parameters"]}

    def expand(part):
        return values[part[1:]] if part.startswith("$") else part

    values["binding"] = "".join(expand(p) for p in document["binding_parts"])
    frame = document["frames"][name]
    header = hex_bytes("".join(expand(p) for p in frame["header"]))
    body = bytearray(frame["length"])
    body[:len(header)] = header
    last = len(header)
    for section in frame["sections"]:
        payload = hex_bytes(expand(section["hex"]))
        offset = section["offset"]
        require(last <= offset and offset + len(payload) <= len(body), "golden ledger overlap")
        body[offset:offset+len(payload)] = payload
        last = offset + len(payload)
    require(last == len(body), "golden ledger end")
    return {"kind": frame["kind"], "version": 1, "collection": 7,
            "partition": 0, "count": 1, "body": bytes(body)}


def golden_source(document, source):
    return {"collection": 7, "dimensions": 1, "precision": source["precision"],
            "lineage": 9, "sequence": 11,
            "rows": [{"id": 0, "revision": 1, "bits": source["dense"]}],
            "blocks": [{"kind": 256, "version": 1, "partition": 0, "count": 1,
                        "sections": [hex_bytes(source["dense"] if s == "$dense" else s)
                                     for s in document["source_sections"]]}]}


def equal(actual, expected, name):
    require(type(actual) is type(expected) and actual == expected,
            f"{name}: expected {expected!r}, got {actual!r}")


def malformed_case(case, frames, descriptor):
    block = copy.deepcopy(frames[case["block"]])
    data = bytearray(block["body"])
    if "at" in case:
        replacement = hex_bytes(case["hex"])
        data[case["at"]:case["at"]+len(replacement)] = replacement
    if "truncate" in case:
        data = data[:case["truncate"]]
    if "append" in case:
        data.extend(hex_bytes(case["append"]))
    block["body"] = bytes(data)
    if "prefix_version" in case:
        block["version"] = case["prefix_version"]
    result = check_block(block, descriptor if block["kind"] == 0x0202 else None)
    equal(result["outcome"], case.get("outcome", "corrupt"), case["name"])


def check_golden(document, source):
    src = golden_source(document, source)
    frames = {name: assemble_frame(document, source, name) for name in document["frames"]}
    b = raw(digest_source(7, 1, source["precision"], src["blocks"]))
    g = raw(digest_generation(9, 11, 7, b["digest"]))
    c = raw(digest_content(7, 1, source["precision"], src["rows"]))
    for result, key, stream in ((b, "B", "source_transcript"), (g, "G", "generation_transcript"),
                                (c, "C", "content_transcript")):
        equal(result["transcript"].hex(), source[stream], stream)
        equal(result["digest"].hex(), source[key], key)
        equal(hashlib.sha256(hex_bytes(source[stream])).hexdigest(), source[key], "literal SHA")
    for name in frames:
        raw(check_block(frames[name]))
    descriptor = raw(check_block(frames["descriptor"]))["descriptor"]
    for name in ("descriptor", "rotated_descriptor"):
        d = raw(check_block(frames[name]))["descriptor"]
        encoded = raw(encode_descriptor(d, d["settings"], d["parameters"]))["block"]
        equal(encoded, frames[name], name + " exact bytes")
    topology = [{"id": 0, "revision": 1, "source": 0, "links": [[]]}]
    equal(raw(encode_topology(0, 1, topology, source_digest=b["digest"]))["block"],
          frames["topology"], "topology exact bytes")
    equal(raw(train_scalar(1, src["rows"], source["precision"]))["parameters"],
          hex_bytes(document["parameters"]), "literal trained parameters")
    equal(raw(encode_scalar(0, 1, [b"\x00"], hex_bytes(document["parameters"]),
                             source_digest=b["digest"]))["block"],
          frames["scalar"], "scalar exact bytes")
    witness = [{"signs": "01", "terms": ["000000000000f03f", "000000000000f03f",
                                        "0000000000000000", "0000000000000000"]}]
    equal(raw(encode_rotated(0, 1, [b"\x01"], [witness[0]["terms"]],
                              source_digest=b["digest"]))["block"],
          frames["rotated"], "rotated exact bytes")
    for rotated in (False, True):
        names = ("rotated_descriptor", "topology", "rotated") if rotated else (
            "descriptor", "topology", "scalar")
        blocks = [frames[n] for n in names]
        d = raw(check_block(blocks[0]))["descriptor"]
        result = raw(digest_set(d["settings"], d["parameters"],
                               [outer(block) for block in blocks[1:]]))
        equal(result["digest"].hex(), document["rotated_set" if rotated else "scalar_set"],
              "literal index-set digest")
        equal(bind(blocks, src, requested_config(d), witness if rotated else None),
              {"outcome": "ok", "installed": True, "nodes": 1}, "atomic golden bind")
    return src, frames, descriptor


def topology_probe(name):
    nodes = [{"id": i, "revision": 1, "source": i, "links": [[]]} for i in range(6)]
    if name == "neighbor-N":
        nodes[0]["links"] = [[6]]
    elif name == "reorder-neighbors":
        nodes[0]["links"] = [[2, 1]]
    elif name == "self-neighbor":
        nodes[0]["links"] = [[0]]
    elif name == "duplicate-neighbor":
        nodes[0]["links"] = [[1, 1]]
    elif name == "degree-zero":
        nodes[0]["links"] = [[1, 2, 3, 4, 5]]
    elif name == "degree-above":
        for node in nodes:
            node["links"] = [[], []]
        nodes[0]["links"][1] = [1, 2, 3, 4, 5]
    else:
        raise Invalid("unknown topology probe")
    block = raw(encode_topology(0, 6, nodes))["block"]
    equal(check_block(block, {"m": 2})["outcome"], "corrupt", name)


def partition_case(case):
    rows = case.get("rows", [case.get("row", 0)] * case.get("repeat", 0))
    result = partition_plan(case["kind"], rows)
    equal(result["outcome"], case.get("outcome", "ok"), case["name"])
    if result["outcome"] == "ok":
        equal([p["count"] for p in result["partitions"]], case["counts"], case["name"] + " counts")
        equal([p["worst_body"] for p in result["partitions"]], case["sizes"], case["name"] + " bytes")


def padding_64_probe():
    body = (struct.pack("<HHHHQ", 0x0204, 1, 0, 1, 7) + bytes(8)
            + struct.pack("<IIII", 104, 0, 104, 1) + bytes(64) + b"x")
    result = check_block({"kind": 0x0204, "version": 1, "collection": 7,
                          "partition": 0, "count": 1, "body": body})
    equal(result["outcome"], "corrupt", "padding-64")


def scalar_tie_probe():
    parameters = bytes.fromhex("01000000000000000000000000000000000000000000f03f")
    equal(raw(encode_scalar_vector([126.5], parameters))["codes"], bytes([127]), "half-away")
    equal(raw(encode_scalar_vector([-2.0], parameters))["codes"], bytes([0]), "clamp below")
    equal(raw(encode_scalar_vector([300.0], parameters))["codes"], bytes([255]), "clamp above")


def signed_zero_probe():
    plus = [{"id": 0, "revision": 1, "bits": "00000000"}]
    minus = [{"id": 0, "revision": 1, "bits": "00000080"}]
    positive = raw(digest_content(7, 1, 1, plus))
    negative = raw(digest_content(7, 1, 1, minus))
    require(positive["digest"] != negative["digest"], "C preserves signed zero")
    equal(negative["transcript"][-4:], bytes.fromhex("00000080"), "literal negative zero")


def rotated_768_probe():
    # 768/8=96, not the next power-of-two 128-byte stride.
    signs = bytes.fromhex("55" * 96)
    terms = ["000000000000f03f", "000000000000f03f", "0000000000000080", "0000000000000000"]
    block = raw(encode_rotated(0, 768, [signs], [terms]))["block"]
    parsed = raw(check_block(block))
    equal(len(parsed["sections"][1]), 96, "768 sign bytes")
    equal(parsed["sections"][2][16:24], bytes.fromhex("0000000000000080"), "term signed zero")


def bind_probes(document, source, selected=None):
    src, frames, d = check_golden(document, source)
    original = [frames[n] for n in ("descriptor", "topology", "scalar")]
    probes = 0

    def expect(name, blocks=original, current=src, requested=None, outcome="corrupt",
               complete=True):
        nonlocal probes
        if selected is not None and name != selected:
            return
        config = {**requested_config(d), **(requested or {})}
        result = bind(blocks, current, config, image_complete=complete)
        equal(result["outcome"], outcome, name)
        equal(result["installed"], outcome == "ok", name + " atomicity")
        probes += 1

    expect("missing-partition", original[:-1])
    expect("duplicate-partition", original + [original[-1]])
    expect("out-of-order-run", [original[0], original[2], original[1]])
    expect("missing-descriptor", original[1:])
    expect("truncated-image-end", complete=False)
    foreign = copy.deepcopy(src)
    foreign["rows"][0]["bits"] = "00000040" if source["precision"] == 1 else "0040"
    expect("foreign-valid-set", current=foreign, outcome="stale")
    expect("foreign-requested-config", requested={"m": 3}, outcome="stale")
    for field in ("lineage", "sequence"):
        current = copy.deepcopy(src)
        current[field] += 1
        expect("changed-" + field, current=current, outcome="stale")
    collision = copy.deepcopy(original)
    bd = bytearray(collision[0]["body"])
    bd[64+72+31] ^= 1
    new_b = bytes(bd[136:168])
    # A full, self-consistent foreign binding with the SAME envelope prefix.
    bd[104:136] = hashlib.sha256(
        b"VLCANN-GENERATION-1" + struct.pack("<QQH", 9, 11, 7) + new_b).digest()
    collision[0]["body"] = bytes(bd)
    expect("same-prefix-different-B", collision, outcome="stale")
    unsupported = copy.deepcopy(original)
    unsupported[1]["version"] = 2
    bd = bytearray(unsupported[1]["body"])
    bd[2:4] = b"\x02\x00"
    unsupported[1]["body"] = bytes(bd)
    expect("unsupported-set-atomic", unsupported, outcome="unsupported_version")
    broken = copy.deepcopy(unsupported)
    broken[-1]["body"] = broken[-1]["body"][:-1]
    expect("corruption-beats-unsupported", broken)
    broken = copy.deepcopy(unsupported)
    bd = bytearray(broken[2]["body"])
    bd[192:200] = struct.pack("<d", 2.0)
    broken[2]["body"] = bytes(bd)
    expect("known-wrong-norm-beats-unsupported", broken)
    broken = copy.deepcopy(original)
    bd = bytearray(broken[0]["body"])
    bd[16] ^= 1
    broken[0]["body"] = bytes(bd)
    expect("wrong-envelope-guard", broken)
    # These are standalone-valid but source-inconsistent parameters. Reseal the
    # derived digest so the intended retraining check, not a checksum, rejects.
    for name, offset, replacement in (
            ("wrong-minimum", 392, struct.pack("<d", 2.0)),
            ("wrong-scale", 400, struct.pack("<d", 1.0))):
        broken = copy.deepcopy(original)
        bd = bytearray(broken[0]["body"])
        bd[offset:offset+8] = replacement
        broken[0]["body"] = bytes(bd)
        param = bytes(bd[384:408])
        code = b"\x00"
        # min=2,scale=0 decodes norm 2; min=1,scale=1 decodes norm 1.
        norm = struct.pack("<d", 2.0 if name == "wrong-minimum" else 1.0)
        sb = bytearray(broken[2]["body"])
        sb[128:129], sb[192:200] = code, norm
        broken[2]["body"] = bytes(sb)
        pieces = [outer(b) for b in broken[1:]]
        digest = hashlib.sha256(b"VLCANN-SET-1" + d["settings"] + param
                                + b"".join(section_stream(b) for b in pieces)).digest()
        bd[248:280] = digest
        broken[0]["body"] = bytes(bd)
        expect(name, broken)
    return probes


def empty_probes(document, source):
    src = golden_source(document, source)
    parameters = raw(train_scalar(1, [], source["precision"]))["parameters"]
    equal(parameters, bytes.fromhex("0100000000000000" + "00"*16), "empty +0 parameters")
    b = raw(digest_source(7, 1, source["precision"], src["blocks"]))["digest"]
    g = raw(digest_generation(9, 11, 7, b))["digest"]
    c = raw(digest_content(7, 1, source["precision"], src["rows"]))["digest"]
    settings = hex_bytes(document["settings"])
    fields = {"collection": 7, "dimensions": 1, "precision": source["precision"], "metric": 0,
              "profile": 1, "state": 0, "lineage": 9, "sequence": 11, "G": g, "B": b, "C": c,
              "physical": 1, "N": 0, "P": 0, "Q": 0, "entry": 0xffffffff, "m": 2, "ef": 2,
              "max_level": 0, "seed": 0, "rotation_seed": 0, "set_digest": bytes(32)}
    block = raw(encode_descriptor(fields, settings, parameters))["block"]
    config = requested_config({**fields, "settings": settings})
    equal(bind([block], src, config), {"outcome": "ok", "installed": False}, "state-0 physical source")
    for cosine_zero in (False, True):
        current = copy.deepcopy(src)
        current["rows"] = ([{"id": 0, "revision": 1,
                             "bits": "00000000" if source["precision"] == 1 else "0000"}]
                           if cosine_zero else [])
        current["blocks"] = []
        fields["B"] = raw(digest_source(7, 1, source["precision"], []))["digest"]
        fields["G"] = raw(digest_generation(9, 11, 7, fields["B"]))["digest"]
        fields["C"] = raw(digest_content(7, 1, source["precision"], current["rows"]))["digest"]
        fields["physical"], fields["state"], fields["metric"] = len(current["rows"]), 1, int(cosine_zero)
        fields["set_digest"] = hashlib.sha256(b"VLCANN-SET-1" + settings + parameters).digest()
        block = raw(encode_descriptor(fields, settings, parameters))["block"]
        config = requested_config({**fields, "settings": settings})
        equal(bind([block], current, config), {"outcome": "ok", "installed": True, "nodes": 0},
              "N=0 complete" + str(cosine_zero))
    return 3


def requested_config(descriptor):
    return {key: descriptor[key] for key in
            ("metric", "profile", "m", "ef", "seed", "rotation_seed", "settings")}


def arithmetic_probes():
    equal(checked_add(U64 - 1, 1), U64, "addition at cap")
    equal(checked_mul(U64, 1), U64, "multiplication at cap")
    for function, operands in ((checked_add, (U64, 1)), (checked_mul, (1 << 63, 2))):
        failed = False
        try:
            function(*operands)
        except Invalid:
            failed = True
        require(failed, "checked arithmetic overflow")
    body = raw(encode_envelope(0x0204, [bytes(MAX_BODY - 64)]))["block"]
    equal(len(body["body"]), MAX_BODY, "body exactly 4 MiB")
    equal(check_block(body)["outcome"], "unknown_kind", "unknown body at cap")
    equal(encode_envelope(0x0204, [bytes(MAX_BODY - 63)])["outcome"], "corrupt", "body over cap")
    body["body"] += b"\x00"
    equal(check_block(body)["outcome"], "corrupt", "decoder body over cap")
    for position in (1, 17, 63, 64, 1000):
        encoded = raw(encode_envelope(0x0204, [b"abc", b"xyz"], position=position))["block"]
        parsed = check_block(encoded)
        equal(parsed["outcome"], "unknown_kind", "advisory reader alignment")
        equal(parsed["sections"], [b"abc", b"xyz"], "position independent payloads")
        equal(raw(encode_envelope(0x0204, [b"abc", b"xyz"], position=position))["block"],
              encoded, "same-position deterministic bytes")


def end_probes():
    prefixes = [bytes(36), b"\x01" + bytes(35)]
    header = bytes.fromhex("0200010001000100" + "00"*16 + "28000000000000002800000028000000")
    body = header + struct.pack("<Q", 2) + hashlib.sha256(b"".join(prefixes)).digest()
    equal(raw(check_image_end(prefixes, body))["frames"], 2, "complete end")
    for length in (0, 31, 39, 40, 47, 79):
        equal(check_image_end(prefixes, body[:length])["outcome"], "corrupt", "truncated end")
    for offset in (0, 2, 4, 6, 8, 16, 24, 28, 32, 36, 40, 48, 79):
        broken = bytearray(body)
        broken[offset] ^= 1
        equal(check_image_end(prefixes, bytes(broken))["outcome"], "corrupt", "end corruption")
    equal(check_image_end(prefixes[:-1], body)["outcome"], "corrupt", "truncated image prefix")


def canonical_zero_probes():
    for precision, zero, negative in ((1, "00000000", "00000080"), (2, "0000", "0080")):
        rows = [{"id": 0, "revision": 1, "bits": zero},
                {"id": 1, "revision": 1, "bits": negative}]
        for order in (rows, list(reversed(rows))):
            result = raw(train_scalar(1, order, precision))
            equal(result["parameters"], bytes.fromhex("0100000000000000" + "00"*16),
                  "R-HP5-01 canonical minimum and constant scale")
        # Empty cosine training is settled: both all-zero vectors are excluded.
        result = raw(train_scalar(1, rows, precision, metric=1))
        equal(result["parameters"], bytes.fromhex("0100000000000000" + "00"*16),
              "excluded cosine zeros are not an ambiguous minimum")


def minima_frame(document, case, precision):
    """Hand-positioned, unbound pending descriptor; no encoder supplies bytes."""
    ledger = document["descriptor"]
    binding = hex_bytes("".join(f"{precision:02x}" if p == "$precision" else p
                               for p in ledger["binding_parts"]))
    equal(len(binding), 224, "minima binding ledger width")
    body = bytearray(ledger["length"])
    header = hex_bytes(ledger["header"])
    body[:len(header)] = header
    settings = bytes.fromhex("0100200001040000a08601000000000000000000000000000000000000000000")
    for offset, payload in zip(ledger["sections"],
                               (binding, settings, hex_bytes(case["parameters"]))):
        body[offset:offset+len(payload)] = payload
    equal(len(body), ledger["length"], "minima body ledger width")
    return {"kind": 0x0200, "version": 1, "collection": 7,
            "partition": 0, "count": 1, "body": bytes(body)}


def minima_golden(document, case, precision):
    expected = minima_frame(document, case, precision)
    d = raw(check_block(expected))["descriptor"]
    rows = [{"id": ident, "revision": 1, "bits": value}
            for ident, value in zip(case.get("row_ids", [0, 1]),
                                    case["rows"][str(precision)])]
    parameters = raw(train_scalar(3, rows, precision))["parameters"]
    equal(parameters, hex_bytes(case["parameters"]), case["name"] + " parameters")
    for row, code in zip(rows, case["codes"]):
        values, _ = components(row, 3, precision)
        equal(raw(encode_scalar_vector(values, parameters))["codes"], hex_bytes(code),
              case["name"] + " codes")
    encoded = raw(encode_descriptor(d, d["settings"], parameters))["block"]
    equal(encoded, expected, case["name"] + " descriptor bytes")
    if case["name"] == "zero-tie-minus-first":
        plus = next(c for c in document["cases"] if c["name"] == "zero-tie-plus-first")
        equal(expected, minima_frame(document, plus, precision), "row-order identical descriptor")
        ordered = sorted(rows, key=lambda row: row["id"])
        plus_rows = [{"id": i, "revision": 1, "bits": value}
                     for i, value in enumerate(plus["rows"][str(precision)])]
        equal(raw(digest_content(7, 3, precision, ordered)),
              raw(digest_content(7, 3, precision, plus_rows)),
              "training order does not change canonical source identity")
    # Encode also canonicalizes supplied (not yet persisted) negative-zero bounds.
    if case["name"] == "all-zero-constant":
        supplied = bytearray(parameters)
        supplied[8:16] = bytes.fromhex("0000000000000080")
        equal(raw(encode_descriptor(d, d["settings"], bytes(supplied)))["block"], expected,
              "descriptor encode canonicalizes supplied minimum")


def single_row_set(document, source, component_bits):
    """Construct negative-test inputs, never golden expected bytes."""
    src = golden_source(document, source)
    src["rows"][0]["bits"] = component_bits
    src["blocks"][0]["sections"][1] = hex_bytes(component_bits)
    d = raw(check_block(assemble_frame(document, source, "descriptor")))["descriptor"]
    d["B"] = raw(digest_source(7, 1, source["precision"], src["blocks"]))["digest"]
    d["G"] = raw(digest_generation(9, 11, 7, d["B"]))["digest"]
    d["C"] = raw(digest_content(7, 1, source["precision"], src["rows"]))["digest"]
    parameters = bytes.fromhex("0100000000000000" + "00"*16)
    topology = [{"id": 0, "revision": 1, "source": 0, "links": [[]]}]
    children = [raw(encode_topology(0, 1, topology, source_digest=d["B"]))["block"],
                raw(encode_scalar(0, 1, [b"\x00"], parameters,
                                  source_digest=d["B"]))["block"]]
    d["set_digest"] = raw(digest_set(d["settings"], parameters,
                                    [outer(b) for b in children]))["digest"]
    descriptor = raw(encode_descriptor(d, d["settings"], parameters))["block"]
    return [descriptor] + children, src, requested_config(d)


def minima_bind_golden(document, source, case):
    blocks, src, config = single_row_set(document, source,
                                         case["source_bits"][str(source["precision"])])
    d = raw(check_block(blocks[0]))["descriptor"]
    equal(d["parameters"], hex_bytes(case["parameters"]), case["name"] + " persisted bounds")
    equal(bind(blocks, src, config), {"outcome": case["outcome"], "installed": True, "nodes": 1},
          case["name"])


def minima_malformed(document, minima, source, case):
    if case.get("mode") == "bind":
        blocks, src, config = single_row_set(
            document, source, "00000000" if source["precision"] == 1 else "0000")
        # Reseal the set digest independently: a checksum mismatch must not mask
        # rejection of the stored -0 minimum against retrained +0.
        data = bytearray(blocks[0]["body"])
        data[case["at"]:case["at"]+8] = hex_bytes(case["hex"])
        transcript = b"VLCANN-SET-1" + data[320:352] + data[384:408]
        for child in blocks[1:]:
            sections = outer(child)["sections"]
            transcript += struct.pack("<HHIIH", child["kind"], 1, 0, 1, len(sections))
            for section in sections:
                transcript += struct.pack("<I", len(section)) + section
        data[248:280] = hashlib.sha256(transcript).digest()
        blocks[0]["body"] = bytes(data)
        result = bind(blocks, src, config)
        equal(result["installed"], False, case["name"] + " atomic")
    else:
        golden = next(c for c in minima["cases"] if c["name"] == "all-zero-constant")
        block = minima_frame(minima, golden, source["precision"])
        data = bytearray(block["body"])
        data[case["at"]:case["at"]+8] = hex_bytes(case["hex"])
        block["body"] = bytes(data)
        result = check_block(block)
    equal(result["outcome"], "corrupt", case["name"])
    equal(result["reason"], "noncanonical scalar minimum", case["name"] + " boundary")


def boundary_bind(document, source, case):
    """Resealed negative inputs isolate semantic checks, not digest failures."""
    mode = case["mode"]
    src = golden_source(document, source)
    src["rows"] = [{"id": ident, "revision": 1, "bits": source["dense"]}
                   for ident in (0, 2, 4)]
    nodes = [{"id": ident, "revision": 1, "source": i, "links": [[] for _ in range(levels)]}
             for i, (ident, levels) in enumerate(((0, 1), (2, 3), (4, 3)))]
    nodes[0]["links"][0] = [1]
    d = raw(check_block(assemble_frame(document, source, "descriptor")))["descriptor"]
    d.update({"physical": 3, "N": 3, "P": 3, "Q": 2, "entry": 1, "max_level": 2})
    parameters = hex_bytes(document["parameters"])
    codes = [b"\x00"] * 3
    topology_starts, code_starts = [0, 1, 2], [0, 2]
    if mode == "topology-gap":
        topology_starts[1] = 2
    elif mode == "topology-overlap":
        topology_starts[1] = 0
    elif mode == "code-gap":
        code_starts[1] = 3
    elif mode == "code-overlap":
        code_starts[1] = 1
    elif mode == "duplicate-id":
        nodes[2]["id"] = 2
    elif mode == "source-id":
        nodes[2]["id"] = 5  # Contract worksheet: ID 5 also has level 2.
    elif mode == "source-revision":
        nodes[2]["revision"] = 2
    elif mode == "source-ordinal":
        nodes[2]["source"] = 1
    elif mode == "wrong-level":
        nodes[1]["links"][1] = [0]
    elif mode == "level":
        nodes[2]["links"].pop()
    elif mode == "entry":
        d["entry"] = 2
    elif mode == "maximum":
        d["max_level"] = 1
    elif mode == "eligibility":
        d["metric"] = 1
        src["rows"][0]["bits"] = "00000000" if source["precision"] == 1 else "0000"
    elif mode == "source-code":
        parameters = bytes.fromhex("0100000000000000" + "00"*8 + "000000000000f03f")
        for i, row in enumerate(src["rows"]):
            row["bits"] = (("00000000" if i == 0 else "00007f43")
                           if source["precision"] == 1 else ("0000" if i == 0 else "f85b"))
        codes = [b"\x00", b"\xfe", b"\xff"]
    if mode == "none":
        d.update({"profile": 0, "Q": 0})
        d["settings"] = raw(encode_settings(profile=0))["bytes"]
        parameters = b""
    src["blocks"][0]["count"] = 3
    src["blocks"][0]["sections"][1] = b"".join(hex_bytes(r["bits"]) for r in src["rows"])
    d["B"] = raw(digest_source(7, 1, source["precision"], src["blocks"]))["digest"]
    d["G"] = raw(digest_generation(9, 11, 7, d["B"]))["digest"]
    d["C"] = raw(digest_content(7, 1, source["precision"], src["rows"]))["digest"]
    children = [raw(encode_topology(start, 3, [node], partition=i,
                                    source_digest=d["B"]))["block"]
                for i, (start, node) in enumerate(zip(topology_starts, nodes))]
    if d["profile"]:
        children += [raw(encode_scalar(start, 1, rows, parameters, partition=i,
                                       source_digest=d["B"]))["block"]
                     for i, (start, rows) in enumerate(zip(code_starts, (codes[:2], codes[2:])))]
    d["set_digest"] = raw(digest_set(d["settings"], parameters,
                                    [outer(b) for b in children]))["digest"]
    descriptor = raw(encode_descriptor(d, d["settings"], parameters))["block"]
    config = requested_config(d)
    if mode == "metadata":
        src["blocks"][0]["sections"][-1] = b'{"a":1}'
    elif mode == "revision":
        src["rows"][-1]["revision"] = 2
    elif mode == "expiry":
        src["rows"][-1]["expiry"] = 0
    elif mode in ("config-ef", "config-seed", "config-rotation"):
        key = {"config-ef": "ef", "config-seed": "seed",
               "config-rotation": "rotation_seed"}[mode]
        config[key] += 1
    result = bind([descriptor] + children, src, config)
    outcome = case.get("outcome", "corrupt")
    equal(result["outcome"], outcome, case["name"])
    equal(result["installed"], outcome == "ok", case["name"] + " atomic")
    if "reason" in case:
        equal(result["reason"], case["reason"], case["name"] + " boundary")


def numeric_boundary(case, precision):
    rows = [{"id": 0, "revision": 1, "bits": case["bits"][str(precision)]}]
    result = train_scalar(1, rows, precision)
    equal(result["outcome"], case.get("outcome", "ok"), case["name"])
    if result["outcome"] == "ok":
        expected = bytes.fromhex("0100000000000000" + case["minimum_f64"][str(precision)]
                                 + "00"*8)
        equal(result["parameters"], expected, case["name"] + " exact promoted minimum")
        values, _ = components(rows[0], 1, precision)
        equal(raw(encode_scalar_vector(values, expected))["codes"], b"\x00",
              case["name"] + " constant code")
        equal(bits(scalar_norm(b"\x00", expected)), bits(abs(values[0])),
              case["name"] + " constant norm")


def dimensions_boundary():
    parameters = struct.pack("<II", 65536, 0) + bytes(16 * 65536)
    codes = bytes(65536)
    block = raw(encode_scalar(0, 65536, [codes], parameters))["block"]
    result = raw(check_block(block, {"parameters": parameters}))
    equal(result["codes"]["dimensions"], 65536, "dimension upper boundary")
    equal(result["codes"]["rows"], [(codes, bytes(8))], "maximum-dimension constant row")


def topology_boundary(mode):
    if mode == "rows":
        nodes = [{"id": i, "revision": 1, "source": i, "links": [[] for _ in range(17)]}
                 for i in range(256)]
        block = raw(encode_topology(0, 256, nodes))["block"]
        result = raw(check_block(block))
        equal(len(result["topology"]["nodes"]), 256, "256 complete nodes")
        equal(unpack("I", result["sections"][0], 8)[0], 4352, "17*256 layer lists")
    else:
        links = [list(range(1, 129))] + [list(range(1, 65)) for _ in range(16)]
        if mode == "edges-over":
            links[0].append(129)
        node = {"id": 0, "revision": 1, "source": 0, "links": links}
        block = raw(encode_topology(0, 130, [node]))["block"]
        result = check_block(block, {"m": 64})
        if mode == "edges-over":
            equal(result, {"outcome": "corrupt", "reason": "topology L/E caps"}, "E over 1152")
        else:
            equal(result["outcome"], "ok", "E exactly 1152")
            equal(unpack("I", result["sections"][0], 12)[0], 1152, "E literal count")


def code_rows_boundary():
    parameters = bytes.fromhex("0100000000000000" + "00"*16)
    block = raw(encode_scalar(0, 1, [b"\x00"] * 4096, parameters))["block"]
    equal(len(raw(check_block(block, {"parameters": parameters}))["codes"]["rows"]),
          4096, "4096 complete code rows")


def envelope_boundaries():
    for count in (1, 64):
        block = raw(encode_envelope(0x0204, [b""] * count))["block"]
        parsed = check_block(block)
        equal(parsed["outcome"], "unknown_kind", "unknown kind with legal section count")
        equal(len(parsed["sections"]), count, "section count boundary")
        equal(bind([block], {}, {}), {"outcome": "unknown_kind", "installed": False},
              "unknown collection set is not installed")
    body = (struct.pack("<HHHHQ", 0x0204, 1, 0, 1, 7) + bytes(8)
            + struct.pack("<IIII", 103, 0, 103, 1) + bytes(63) + b"x")
    equal(check_block({"kind": 0x0204, "version": 1, "collection": 7,
                       "partition": 0, "count": 1, "body": body})["outcome"],
          "unknown_kind", "63 zero padding bytes accepted")


def descriptor_boundaries(minima):
    case = next(c for c in minima["cases"] if c["name"] == "all-zero-constant")
    d = raw(check_block(minima_frame(minima, case, 1)))["descriptor"]
    d.update({"physical": 1000000, "m": 64, "ef": 4096})
    for rerank in (1, 16):
        settings = raw(encode_settings(rerank=rerank))["bytes"]
        block = raw(encode_descriptor(d, settings, d["parameters"]))["block"]
        parsed = raw(check_block(block))["descriptor"]
        equal((parsed["physical"], parsed["m"], parsed["ef"]), (1000000, 64, 4096),
              "descriptor upper boundaries without source allocation")


def source_boundaries(precision):
    dense = "0000803f" if precision == 1 else "003c"
    row = {"id": U64, "revision": U64, "expiry": U64, "bits": dense}
    result = raw(digest_content(7, 1, precision, [row]))
    expected = (bytes.fromhex("564c43414e4e2d434f4e54454e542d310700")
                + bytes([precision]) + bytes.fromhex("010000000100000000000000")
                + b"\xff"*16 + b"\x01" + b"\xff"*8 + hex_bytes(dense))
    equal(result["transcript"], expected, "literal ID/revision/expiry u64 maxima")
    absent = {"id": 0, "revision": 0, "bits": dense}
    no_expiry = raw(digest_content(7, 1, precision, [absent]))
    zero_expiry = raw(digest_content(7, 1, precision, [{**absent, "expiry": 0}]))
    require(no_expiry["digest"] != zero_expiry["digest"], "absent expiry differs from present zero")
    equal(zero_expiry["transcript"][-len(hex_bytes(dense))-9:-len(hex_bytes(dense))],
          b"\x01" + bytes(8), "literal present-zero expiry")
    for rows in ([absent, absent], [row, absent]):
        equal(digest_content(7, 1, precision, rows)["outcome"], "corrupt",
              "duplicate/descending source IDs")
    class OversizedRows:
        def __len__(self):
            return 1000001
        def __iter__(self):
            raise Invalid("must reject count before iterating")
    equal(digest_content(7, 1, precision, OversizedRows()),
          {"outcome": "corrupt", "reason": "source row cap"}, "source row cap before iteration")


def scalar_numeric_boundaries(precision):
    rows = [{"id": 0, "revision": 1, "bits": "00000000" if precision == 1 else "0000"},
            {"id": 1, "revision": 1, "bits": "00007f43" if precision == 1 else "f85b"}]
    parameters = bytes.fromhex("0100000000000000" + "00"*8 + "000000000000f03f")
    equal(raw(train_scalar(1, rows, precision))["parameters"], parameters, "0..255 scale exactly one")
    for value, code in ((0.0, 0), (255.0, 255), (math.nextafter(126.5, 0.0), 126),
                        (126.5, 127), (math.nextafter(126.5, math.inf), 127)):
        equal(raw(encode_scalar_vector([value], parameters))["codes"], bytes([code]),
              "half-away adjacent binary64 witnesses")
    norm_parameters = bytes.fromhex("0200000000000000" + "00"*16 + "000000000000f03f"*2)
    equal(bits(scalar_norm(b"\x03\x04", norm_parameters)), bytes.fromhex("0000000000001440"),
          "ordered decoded 3-4 norm equals literal 5")
    cosine_rows = [
        {"id": 0, "revision": 1, "bits": "0000000000000080" if precision == 1 else "00000080"},
        {"id": 1, "revision": 1, "bits": "000000800000803f" if precision == 1 else "0080003c"}]
    expected = bytes.fromhex("0200000000000000" + "00"*8 + "000000000000f03f" + "00"*16)
    equal(raw(train_scalar(2, cosine_rows, precision, metric=1))["parameters"], expected,
          "cosine excludes zero rows, canonicalizes zero coordinate of eligible row")
    for value in (math.inf, -math.inf, math.nan):
        equal(encode_scalar_vector([value], parameters)["outcome"], "corrupt",
              "nonfinite encode input")


def unsupported_boundary(document, source, case):
    name = case["name"]
    frame = "rotated_descriptor" if name.startswith("rotation-") else "descriptor"
    original = assemble_frame(document, source, frame)
    sections = [bytearray(s) for s in outer(original)["sections"]]
    if name == "binding-v2-short":
        sections[0] = bytearray(b"VLCANN01\x02\x00")
    elif name.startswith("settings-v2"):
        sections[1] = bytearray(b"\x02\x00")
        if name.endswith("bad-entry"):
            sections[0][156:160] = bytes.fromhex("01000000")
        elif name.endswith("bad-minimum"):
            sections[2][8:16] = bytes.fromhex("0000000000000080")
    elif name == "rotation-family-v2-short":
        sections[2] = bytearray.fromhex("02000100")
    elif name == "rotation-algorithm-v2-short":
        sections[2] = bytearray.fromhex("01000200")
    elif name == "quantizer-v2-bad-settings":
        sections[0][165] = 2
        sections[1][6] = 1
    block = raw(encode_envelope(0x0200, [bytes(s) for s in sections],
                                source_digest=hex_bytes(source["B"])))["block"]
    result = check_block(block)
    equal(result["outcome"], case.get("outcome", "unsupported_version"), name)
    if "reason" in case:
        equal(result["reason"], case["reason"], name + " understood boundary")


def tampered_expectation(document, source, mode):
    changed = copy.deepcopy(document)
    s = copy.deepcopy(source)
    if mode == "norm-bit":
        changed["frames"]["scalar"]["sections"][2]["hex"] = "010000000000f03f"
    elif mode == "source-ordinal":
        node = changed["frames"]["topology"]["sections"][1]
        node["hex"] = node["hex"][:32] + "01000000" + node["hex"][40:]
    else:
        s["B"] = ("0" if s["B"][0] != "0" else "1") + s["B"][1:]
    failed = False
    try:
        # Deliberately bypass manifest hashes: the byte/transcript comparison
        # itself must reject the independently tampered expected golden.
        check_golden(changed, s)
    except Invalid:
        failed = True
    require(failed, "undetected tampered expected " + mode)
    check_golden(document, source)


def self_test():
    global MUTANT
    document = load_json(ROOT / "goldens.json")
    malformed = load_json(ROOT / "malformed.json")
    minima = load_json(ROOT / "scalar-minima.json")
    boundaries = load_json(ROOT / "boundaries.json")
    require(boundaries["schema"] == "vlite.compact-ann-boundaries"
            and boundaries["version"] == 1, "boundary schema")
    require(minima["schema"] == "vlite.compact-ann-scalar-minima"
            and minima["version"] == 1, "minima schema")
    require(document["schema"] == "vlite.compact-ann-bytes" and document["version"] == 1,
            "golden schema")
    require(malformed["schema"] == "vlite.compact-ann-malformed" and malformed["version"] == 1,
            "malformed schema")
    manifest = load_json(ROOT / "manifest.json")
    require(manifest["schema"] == "vlite.compact-ann-manifest" and manifest["version"] == 1,
            "manifest schema")
    equal(set(manifest["files"]), {"goldens.json", "malformed.json", "scalar-minima.json",
                                  "boundaries.json"}, "manifest complete file inventory")
    for filename, expected in manifest["files"].items():
        require(filename in ("goldens.json", "malformed.json", "scalar-minima.json",
                             "boundaries.json"), "manifest file")
        equal(hashlib.sha256(canonical(load_json(ROOT / filename))).hexdigest(), expected,
              "fixture content hash " + filename)
    cases = {}
    for case in minima["cases"]:
        for precision in (1, 2):
            cases[f'{case["name"]}-p{precision}'] = (
                lambda c=case, p=precision: minima_golden(minima, c, p))
    for source in document["sources"]:
        precision = source["precision"]
        for case in boundaries["bind_cases"]:
            cases[f'{case["name"]}-p{precision}'] = (
                lambda c=case, s=source: boundary_bind(document, s, c))
        for case in boundaries["numeric_cases"]:
            cases[f'{case["name"]}-p{precision}'] = (
                lambda c=case, p=precision: numeric_boundary(c, p))
        for case in boundaries["unsupported_cases"]:
            cases[f'{case["name"]}-p{precision}'] = (
                lambda c=case, s=source: unsupported_boundary(document, s, c))
        for mode in boundaries["tampered_expectations"]:
            cases[f'tampered-expected-{mode}-p{precision}'] = (
                lambda m=mode, s=source: tampered_expectation(document, s, m))
        cases[f'empty-p{precision}'] = lambda s=source: empty_probes(document, s)
        cases[f'source-boundaries-p{precision}'] = lambda p=precision: source_boundaries(p)
        cases[f'scalar-numeric-boundaries-p{precision}'] = (
            lambda p=precision: scalar_numeric_boundaries(p))
        for case in minima["bind_cases"]:
            cases[f'{case["name"]}-p{precision}'] = (
                lambda c=case, s=source: minima_bind_golden(document, s, c))
        for case in malformed["minima_cases"]:
            cases[f'{case["name"]}-p{precision}'] = (
                lambda c=case, s=source: minima_malformed(document, minima, s, c))
    for source in document["sources"]:
        precision = source["precision"]
        src, frames, descriptor = check_golden(document, source)
        cases[f"golden-{precision}"] = lambda s=source: check_golden(document, s)
        for case in malformed["cases"]:
            name = f'{case["name"]}-p{precision}'
            require(name not in cases, "duplicate case")
            cases[name] = lambda c=case, f=frames, d=descriptor: malformed_case(c, f, d)
        for name in ("missing-partition", "duplicate-partition", "out-of-order-run",
                     "missing-descriptor", "truncated-image-end", "foreign-valid-set",
                     "foreign-requested-config", "changed-lineage", "changed-sequence",
                     "same-prefix-different-B", "unsupported-set-atomic",
                     "corruption-beats-unsupported", "known-wrong-norm-beats-unsupported",
                     "wrong-envelope-guard", "wrong-minimum", "wrong-scale"):
            cases[f'bind-{name}-p{precision}'] = (
                lambda n=name, s=source: equal(bind_probes(document, s, n), 1,
                                              "one named legacy bind scenario"))
    for case in document["partitions"]:
        cases[case["name"]] = lambda c=case: partition_case(c)
    for case in document["level_cases"]:
        cases[case["name"]] = lambda c=case: equal(
            raw(level(c["id"], c["seed"], c["m"]))["level"], c["level"], c["name"])
    for name in ("neighbor-N", "reorder-neighbors", "self-neighbor", "duplicate-neighbor",
                 "degree-zero", "degree-above"):
        cases[name] = lambda n=name: topology_probe(n)
    cases.update({"padding-64": padding_64_probe, "half-away": scalar_tie_probe,
                  "signed-zero-C": signed_zero_probe, "rotated-768": rotated_768_probe,
                  "checked-arithmetic": arithmetic_probes, "image-end": end_probes,
                  "canonical-zero-minimum": canonical_zero_probes})
    cases.update({"dimensions-65536": dimensions_boundary,
                  "topology-256-L4352": lambda: topology_boundary("rows"),
                  "topology-E1152": lambda: topology_boundary("edges"),
                  "topology-E1153": lambda: topology_boundary("edges-over"),
                  "code-4096-real-rows": code_rows_boundary,
                  "envelope-boundaries": envelope_boundaries,
                  "descriptor-upper-boundaries": lambda: descriptor_boundaries(minima)})
    for case in cases.values():
        case()
    mutants = {
        "wrong-source-bytes": "golden-1", "omit-set-section": "golden-1",
        "reorder-neighbors": "reorder-neighbors", "neighbor-N": "neighbor-N",
        "skip-level-cap": "level-cap", "degree-above": "degree-above",
        "round-code-stride": "rotated-768", "hash-padding": "golden-1",
        "native-endian": "golden-1", "accept-trailing": "trailing-byte-p1",
        "stale-as-corrupt": "bind-foreign-valid-set-p1",
        "unsupported-as-corrupt": "bind-unsupported-set-atomic-p1",
        "split-257": "topology-257", "allow-padding-64": "padding-64",
        "half-even": "half-away", "accept-NaN-norm": "rotated-norm-NaN-p1",
        "omit-B-section": "golden-1", "omit-lineage": "golden-1",
        "drop-signed-zero": "signed-zero-C",
        "encode-raw-minimum": "all-zero-constant-p1",
        "accept-negative-zero-minimum": "minimum-negative-zero-middle-p1",
        "bind-raw-minimum": "bind-canonical-retrained-minus-zero-p1",
        "first-equal-minimum": "zero-tie-minus-first-p1",
        "canonicalize-negative-minimum": "negative-minimum-preserved-p1",
    }
    equal(mutants, manifest["mutants"], "committed mutation map")
    killed = []
    for mutant, name in mutants.items():
        MUTANT = mutant
        failed = False
        try:
            cases[name]()
        except Invalid:
            failed = True
        finally:
            MUTANT = None
        require(failed, f"surviving mutant: {mutant} in {name}")
        cases[name]()
        killed.append({"mutant": mutant, "case": name})
    return {"named_checks": len(cases), "malformed_per_precision": (
                len(malformed["cases"]) + len(malformed["minima_cases"])),
            "golden_body_ledgers": (len(document["frames"]) * len(document["sources"])
                                    + len(minima["cases"]) * 2),
            "mutants_killed": len(killed), "mutants_survived": 0, "mutants_equivalent": 0,
            "mutation_evidence": killed, "rotation_arithmetic": "not_modeled",
            "status": "c1_reference", "open_items": []}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--self-test", action="store_true", help="goldens, malformed cases and mutants")
    args = parser.parse_args()
    try:
        counts = self_test()
        print(json.dumps(counts, sort_keys=True, allow_nan=False))
        return 0
    except (OSError, Invalid, struct.error, KeyError, TypeError) as error:
        print(f"compact_ann_oracle: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
