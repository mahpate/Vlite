"""Wire conversions shared by the optional adapters."""

import base64
import hashlib
import json
import math
import re


def wire_id(value):
    value = str(value)
    if re.fullmatch(r"0|[1-9][0-9]{0,19}", value) and int(value) <= 2**64 - 1:
        return value
    return str(int.from_bytes(hashlib.blake2b(value.encode(), digest_size=8).digest(), "big"))


def metadata_value(value):
    if isinstance(value, bool):
        return {"type": "boolean", "value": value}
    if isinstance(value, int) and -(2**63) <= value < 2**63:
        return {"type": "integer", "value": str(value)}
    if isinstance(value, float) and math.isfinite(value):
        return {"type": "float", "value": value}
    if isinstance(value, str):
        return {"type": "string", "value": value}
    raise ValueError("Filterable metadata must be finite string/bool/i64/float scalars")


def encode(vector, payload, metadata):
    return {"vector": vector,
            "payload_base64": base64.b64encode(json.dumps(payload, allow_nan=False).encode()).decode(),
            "metadata": {key: metadata_value(value) for key, value in metadata.items()}}


def decode(record):
    return json.loads(base64.b64decode(record["payload_base64"]))


def score(distance, metric):
    if metric == "cosine":
        return 1.0 - distance
    if metric == "inner_product":
        return -distance
    return 1.0 / (1.0 + distance)
