"""Keep the private compact real-open receiver matrix exhaustive (stdlib only)."""
import re
from pathlib import Path

from api_reference import rust_items

ROOT = Path(__file__).resolve().parents[1]
OWNERS = {"Database", "CollectionRef", "CollectionMut", "ReadSnapshot"}
# Associated constructors create a new resident store, not an operation on an
# installed compact handle. Open/repair variants are exercised in install_tests.
FACTORIES = {
    "Database::create",
    "Database::create_with_format",
    "Database::create_with_options",
    "Database::create_directory",
    "Database::create_in_memory",
    "Database::open",
    "Database::open_encrypted",
    "Database::open_read_only",
    "Database::open_read_only_encrypted",
    "Database::open_from_bytes",
    "Database::discard_incomplete_tail",
    "Database::discard_incomplete_tail_encrypted",
    "Database::ann_snapshot_path",
}
items = {i["key"].removeprefix("rust:") for i in rust_items(ROOT) if i["owner"] in OWNERS}
matrix = (ROOT / "src/compact/source/method_tests.rs").read_text(encoding="utf-8")
covered = set(re.findall(r'"((?:Database|CollectionRef|CollectionMut|ReadSnapshot)::\w+)"', matrix))
assert not (items - FACTORIES - covered), f"Missing compact method cases: {sorted(items - FACTORIES - covered)}"
assert not ((covered | FACTORIES) - items), f"Stale compact method cases: {sorted((covered | FACTORIES) - items)}"
print(f"PASS: compact matrix classifies {len(items)} methods ({len(covered)} receiver/acquisition cases, {len(FACTORIES)} associated factories/utilities)")
