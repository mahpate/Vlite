"""Verify compact float16 golden cases independently of the storage codec."""

import argparse
import json
import math
from pathlib import Path
import re
import struct
import sys


FIXTURE = Path(__file__).resolve().parent / "fixtures" / "compact" / "float16-cases.json"


def unique_object(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise ValueError(f"duplicate JSON key {key!r}")
        result[key] = value
    return result


def reject_constant(value):
    raise ValueError(f"invalid JSON constant {value!r}")


def require(condition, message):
    if not condition:
        raise ValueError(message)


def require_hex(value, width, location):
    require(
        isinstance(value, str) and re.fullmatch(rf"[0-9a-f]{{{width}}}", value),
        f"{location}: expected exactly {width} lowercase hexadecimal digits",
    )


def oracle(input_bits):
    value = struct.unpack(">f", bytes.fromhex(input_bits))[0]
    if not math.isfinite(value):
        return {"error": "non_finite"}
    try:
        half = struct.pack(">e", value)
    except OverflowError:
        return {"error": "overflow"}
    canonical = struct.pack(">f", struct.unpack(">e", half)[0])
    return {
        "expected_f16_bits": half.hex(),
        "canonical_f32_bits": canonical.hex(),
    }


def validate_fixture(path):
    document = json.loads(
        path.read_text(encoding="utf-8"),
        object_pairs_hook=unique_object,
        parse_constant=reject_constant,
    )
    require(isinstance(document, dict), "root: expected an object")
    require(set(document) == {"version", "cases"}, "root: expected only version and cases")
    require(
        type(document["version"]) is int and document["version"] == 1,
        "version: expected integer 1",
    )
    cases = document["cases"]
    require(isinstance(cases, list), "cases: expected an array")
    require(len(cases) >= 40, "cases: expected at least 40 distinct edge cases")
    names = set()
    inputs = set()
    mismatches = []
    for index, case in enumerate(cases):
        location = f"cases[{index}]"
        require(isinstance(case, dict), f"{location}: expected an object")
        name = case.get("name")
        require(isinstance(name, str) and bool(name.strip()), f"{location}.name: expected a nonempty string")
        require(name not in names, f"{location}: duplicate name {name!r}")
        names.add(name)
        location = f"{location} ({name})"
        input_bits = case.get("input_f32_bits")
        require_hex(input_bits, 8, f"{location}.input_f32_bits")
        require(input_bits not in inputs, f"{location}: duplicate input_f32_bits {input_bits}")
        inputs.add(input_bits)
        if "error" in case:
            require(
                set(case) == {"name", "input_f32_bits", "error"},
                f"{location}: rejected case must contain only name, input_f32_bits and error",
            )
            require(
                case["error"] in ("non_finite", "overflow"),
                f"{location}.error: expected non_finite or overflow",
            )
        else:
            require(
                set(case) == {"name", "input_f32_bits", "expected_f16_bits", "canonical_f32_bits"},
                f"{location}: accepted case must contain name, input_f32_bits, "
                "expected_f16_bits and canonical_f32_bits",
            )
            require_hex(case["expected_f16_bits"], 4, f"{location}.expected_f16_bits")
            require_hex(case["canonical_f32_bits"], 8, f"{location}.canonical_f32_bits")
        expected = oracle(input_bits)
        actual = {key: value for key, value in case.items() if key not in ("name", "input_f32_bits")}
        if actual != expected:
            mismatches.append(
                f"{location}, input {input_bits}: fixture has {actual}; struct oracle expects {expected}"
            )
    require(not mismatches, "\n".join(mismatches))
    return len(cases)


def main():
    parser = argparse.ArgumentParser(
        description=(
            "Verify binary32 -> binary16 round-to-nearest-even -> canonical binary32 "
            "goldens using Python stdlib struct 'e', independently of the codec. "
            "Check binary32 finiteness before conversion: infinities and all NaNs are "
            "non_finite. Finite overflow means rounding to infinity (struct.pack raises "
            "OverflowError), not merely exceeding 65504; finite values rounding to "
            "65504 are accepted. Preserve subnormals, underflow and signed zero."
        )
    )
    parser.add_argument(
        "fixture",
        nargs="?",
        type=Path,
        default=FIXTURE,
        help="fixture to verify (default: fixtures/compact/float16-cases.json beside this script)",
    )
    args = parser.parse_args()
    try:
        count = validate_fixture(args.fixture)
    except (OSError, ValueError, struct.error) as error:
        print(f"{args.fixture}: {error}", file=sys.stderr)
        return 1
    print(f"Verified {count} float16 golden cases: {args.fixture}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
