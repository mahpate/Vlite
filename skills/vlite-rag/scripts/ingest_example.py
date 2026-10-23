"""Create a new toy RAG database; requires Python 3.11+ and vlite-embedded."""

import argparse
import base64
from pathlib import Path
import sys


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--db", type=Path, required=True, help="New database path; never overwritten")
    parser.add_argument("--library", type=Path, help="Explicit vlite_ffi shared library instead of a wheel")
    parser.add_argument("--ann", action="store_true", help="Also build/query an unfiltered HNSW index")
    args = parser.parse_args()
    if args.db.exists():
        parser.error(f"Refusing to overwrite existing database: {args.db}")
    if not args.db.parent.is_dir():
        parser.error(f"Database parent directory does not exist: {args.db.parent}")
    try:
        from vlite_embedded import EmbeddedError, NativeLibrary
    except ImportError:
        parser.error("Install vlite-embedded, or add bindings\\python to PYTHONPATH for local sources")

    rows = [
        ("1", [1.0, 0.0, 0.0], "guide.md", "VLite stores vectors in a single database file."),
        ("2", [0.8, 0.2, 0.0], "retrieval.md", "Retrieve chunk IDs, then fetch citable payloads."),
        ("3", [0.0, 1.0, 0.0], "ops.md", "Close database owners before offline maintenance."),
    ]
    scope = [{"op": "eq", "field": "tenant",
              "value": {"type": "string", "value": "demo"}}]
    try:
        native = NativeLibrary(args.library)
        with native.create(args.db, 3, format=4) as db:
            db.create_collection("docs", 3, settings={
                "metric": "cosine",
                "metadata_indexes": [{"field": "tenant", "kind": "eq"}],
            })
            for record_id, vector, source, text in rows:
                db.put(record_id, vector, collection="docs", metadata={
                    "tenant": {"type": "string", "value": "demo"},
                    "source": {"type": "string", "value": source},
                    "chunk": {"type": "integer", "value": "0"},
                }, payload_base64=base64.b64encode(text.encode("utf-8")).decode("ascii"))
            query = [1.0, 0.0, 0.0]  # Toy query; no embedding model is invoked.
            hits = db.search(query, 2, collection="docs", metric="cosine", filter=scope)
            if len(hits) != 2 or hits[0]["id"] != "1":
                raise RuntimeError(f"Unexpected exact results: {hits!r}")
            for hit in hits:
                record = db.get(hit["id"], collection="docs")
                text = base64.b64decode(record["payload_base64"], validate=True).decode("utf-8")
                print(f"[{record['metadata']['source']['value']}#chunk-{hit['id']}] {text}")
            if args.ann:
                db.build_index(collection="docs", metric="cosine")
                approximate = db.search_approximate_ready(query, 2, collection="docs",
                                                          metric="cosine", ef_search=100)
                if not approximate or approximate[0]["id"] != "1":
                    raise RuntimeError(f"Unexpected ANN results: {approximate!r}")
                print("ANN ready: retrieved expected nearest chunk (unfiltered toy query).")
        with native.open(args.db, read_only=True) as db:
            if db.get("1", collection="docs")["id"] != "1":
                raise RuntimeError("Reopened record did not match")
        print(f"Created and reopened {args.db}; vectors are illustrative, not real embeddings.")
        return 0
    except (EmbeddedError, OSError, RuntimeError, ValueError) as error:
        print(f"VLite example failed: {error}. Any created database is left intact; inspect before reuse.",
              file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
