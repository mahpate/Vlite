"""Exercise the C ABI and Python embedded binding without a server."""
import ctypes
import base64
import json
import math
from pathlib import Path
import subprocess
import sys
import tempfile
from concurrent.futures import ThreadPoolExecutor

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "bindings" / "python"))
from vlite_embedded import EmbeddedError, NativeLibrary  # noqa: E402


def fails(code, action):
    try:
        action()
        raise AssertionError("Expected " + code)
    except EmbeddedError as error:
        assert error.code == code, str(error)


def main():
    library_path = str(Path(sys.argv[1]).resolve())
    native = NativeLibrary(library_path)
    # Direct ABI invalid-pointer/length checks use null only, never dangling memory.
    for pointer, size in [(None, 0), (None, 1), (None, 8 * 1024 * 1024 + 1)]:
        result = native._lib.vlite_execute(0, pointer, size)
        try:
            assert json.loads(ctypes.string_at(result))["error"]["code"] == "invalid_input"
        finally:
            native._lib.vlite_response_free(result)
    native._lib.vlite_response_free(None)
    with tempfile.TemporaryDirectory(prefix="vlite-embedded-") as directory:
        path = Path(directory) / "unicode-\u03bb.vlite"
        db = native.create(path, 3)
        fails("locked", lambda: native.open(path))
        fails("io_error", lambda: native.create(path, 3))
        assert db.info() == {"dimensions": 3, "format_version": 3, "records": 0}
        metadata = {"n": {"type": "integer", "value": "-9223372036854775808"}}
        db.put("18446744073709551615", [1, 0, 0], payload_base64="AP8=", metadata=metadata)
        assert db.get("18446744073709551615")["metadata"] == metadata
        assert db.get("18446744073709551615")["payload_base64"] == "AP8="
        assert db.search([1, 0, 0], 1, metric="cosine")[0]["distance"] == 0
        assert db.search([1, 0, 0], 1, metric="inner_product")[0]["distance"] == -1
        fails("invalid_input", lambda: db.search([0, 0, 0], 0, metric="cosine"))
        fails("invalid_input", lambda: db.search([1, 0, 0], 1, metric="unsupported"))
        assert db.get_many(["18446744073709551615", "0", "18446744073709551615"]) == [
            db.get("18446744073709551615"), None, db.get("18446744073709551615")]
        assert db.get_many([]) == []
        fails("invalid_input", lambda: db.get_many(["1"] * 101))
        fails("invalid_input", lambda: db.get_many(["18446744073709551615", "01"]))
        db.put("8", [0, 0, 1], payload_base64=base64.b64encode(bytes(1024 * 1024)).decode())
        fails("response_too_large", lambda: db.get_many(["8"] * 8))
        assert db.delete("8")
        fails("index_not_ready", lambda: db.search_approximate_ready([1, 0, 0], 1))
        fails("invalid_input", lambda: db.get("01"))
        fails("invalid_input", lambda: db.put("1", [1, 2]))
        fails("invalid_input", lambda: db.put("1", [1, 0, 0], payload_base64="!"))
        fails("invalid_input", lambda: db.batch([
            {"op": "put", "id": "2", "vector": [0, 1, 0]},
            {"op": "put", "id": "3", "vector": [1]},
        ]))
        assert db.info()["records"] == 1
        db.batch([{"op": "put", "id": "2", "vector": [0, 1, 0]},
                  {"op": "delete", "id": "0"}])
        hits = db.search([1, 0, 0], 2, filter=[{"op": "eq", "field": "n", "value": metadata["n"]}])
        assert hits == [{"id": "18446744073709551615", "distance": 0}]
        db.build_index()
        assert db.search_approximate_ready([1, 0, 0], 1)[0]["distance"] == 0
        fails("index_not_ready", lambda: db.search_approximate_ready([1,0,0],1,metric="cosine"))
        db.build_index(metric="cosine")
        assert db.search_approximate_ready([1,0,0],1,metric="cosine")[0]["distance"] == 0
        fails("index_not_ready", lambda: db.search_approximate_ready([1,0,0],1))
        fails("invalid_input", lambda: db.build_index(metric="inner_product"))
        fails("invalid_input", lambda: db.search_approximate_ready([0,0,0],0,metric="cosine"))
        with ThreadPoolExecutor(max_workers=4) as pool:
            assert all(pool.map(lambda _: db.info()["records"] == 2, range(32)))
        handle = db._handle
        db.close()
        db.close()
        fails("invalid_handle", lambda: db.info())
        fails("invalid_handle", lambda: native._execute(handle, {"op": "info"}))
        with native.open(path) as reopened:
            assert reopened.get("2")["vector"] == [0, 1, 0]
            assert reopened.delete("2") and not reopened.delete("2")
            fails("not_found", lambda: reopened.get("2"))
        # A new interpreter uses the same file and library, not a network service.
        code = """
import sys
from vlite_embedded import NativeLibrary
with NativeLibrary(sys.argv[1]).open(sys.argv[2]) as db:
    assert db.info()['records']==1
    assert db.get('18446744073709551615')['payload_base64']=='AP8='
"""
        import os
        env = dict(os.environ, PYTHONPATH=str(ROOT / "bindings" / "python"))
        subprocess.run([sys.executable, "-c", code, library_path, str(path)],
                       env=env, check=True, timeout=20)
        # Damaged complete frames fail closed, and open never changes the file.
        damaged = bytearray(path.read_bytes())
        damaged[-1] ^= 1
        path.write_bytes(damaged)
        fails("corrupt", lambda: native.open(path))
        assert path.read_bytes() == damaged
        v4_surface(native, Path(directory))
    print("PASS C ABI and embedded Python: persistence, locks, Unicode, CRUD, atomicity, metadata, search, threads, "
          "corruption, v4 collections, revisions, planned/hybrid/quantized search and resumable imports")


def v4_surface(native, directory):
    fails("invalid_input", lambda: native.create(directory / "bad.vlite", 2, format=6))
    with native.create(directory / "legacy.vlite", 2) as legacy:
        fails("requires_format_v4", lambda: legacy.create_collection("docs", 2))
        legacy.put("1", [1, 0])
        revision = legacy.revision("1")
        assert revision.isdigit() and legacy.get("1")["revision"] == revision
    path = directory / "v4.vlite"
    db = native.create(path, 2, format=4)
    assert db.info()["format_version"] == 4
    info = db.create_collection("docs", 2, settings={
        "metadata_indexes": [{"field": "tag", "kind": "eq"}], "text_fields": ["title"],
        "hnsw": {"seed": "18446744073709551615"}})
    assert info["name"] == "docs" and info["settings"]["hnsw"]["seed"] == "18446744073709551615"
    fails("collection_exists", lambda: db.create_collection("docs", 2))
    fails("invalid_input", lambda: db.create_collection("x", 2, settings={"unknown": 1}))
    assert [c["name"] for c in db.list_collections()] == ["default", "docs"]

    first = db.put("1", [1, 0], collection="docs", expected_revision="absent")
    assert first.isdigit() and db.revision("1", collection="docs") == first
    fails("precondition_failed", lambda: db.put("1", [0, 1], collection="docs", expected_revision="absent"))
    second = db.put("1", [0, 1], collection="docs", expected_revision=first)
    assert second != first
    fails("precondition_failed", lambda: db.delete("1", collection="docs", expected_revision=first))
    fails("invalid_input", lambda: db.put("1", [0, 1], collection="docs", expected_revision="01"))
    assert db.put("1", [0, 1], collection="docs") is None
    assert db.delete("1", collection="docs", expected_revision=db.revision("1", collection="docs"))
    assert db.revision("1", collection="docs") is None
    fails("collection_not_found", lambda: db.get("1", collection="missing"))

    db.batch([{"op": "put", "id": str(i), "vector": [math.cos(i / 7), math.sin(i / 7)], "metadata": {
        "tag": {"type": "string", "value": "rare" if i == 3 else "common"},
        "title": {"type": "string", "value": "red apple" if i == 5 else "green pear"}}} for i in range(40)],
        collection="docs")
    exact = db.search([1, 0], 3, collection="docs")
    assert exact[0]["id"] == "0"
    explained = db.search([1, 0], 3, collection="docs", explain=True)
    assert explained["hits"] == exact and explained["stats"]["records_scanned"] == 40
    fails("invalid_input", lambda: db._execute("search", vector=[1, 0], k=1, ef_search=5))
    rare = [{"op": "eq", "field": "tag", "value": {"type": "string", "value": "rare"}}]
    planned = db.search_auto([1, 0], 3, collection="docs", filter=rare, explain=True)
    assert planned["plan"]["strategy"] == "exact_indexed" and [h["id"] for h in planned["hits"]] == ["3"]
    assert "plan" not in db.search_auto([1, 0], 3, collection="docs")

    fails("index_not_ready", lambda: db.ann_memory(collection="docs"))
    db.build_index(collection="docs", quantization="scalar_u8", rerank_multiplier=4)
    planned = db.search_auto([1, 0], 1, collection="docs", quantization="scalar_u8", explain=True)
    assert planned["plan"]["strategy"] == "ann" and planned["hits"][0]["id"] == "0"
    assert planned["stats"]["quantized_distance_evaluations"] > 0
    assert db.search_approximate_ready([1, 0], 1, collection="docs", ef_search=20, quantization="scalar_u8")[0]["id"] == "0"
    assert db.ann_memory(collection="docs", quantization="scalar_u8")["code_bytes"] == 80
    quality = db.ann_quality(collection="docs", quantization="scalar_u8", samples=4, k=3, ef_search=40, seed="7")
    assert quality["samples_used"] == 4 and quality["recall_at_k"] > 0.5
    assert db.ann_quality(collection="docs", quantization="scalar_u8",
                          max_distance_evaluations=1)["skipped_reason"] == "budget"

    hybrid = db.search_hybrid("apple", 3, collection="docs", vector=[1, 0], explain=True)
    assert hybrid["hits"][0]["id"] == "5" and hybrid["hits"][0]["text_rank"] == 1 and "stats" in hybrid
    ready = db.search_hybrid("apple", 3, collection="docs", vector=[1, 0], vector_leg="approximate_ready",
                             quantization="scalar_u8", ef_search=60,
                             fusion={"rrf_k": 10, "vector_weight": 1, "text_weight": 2})
    assert len(ready["hits"]) == 3
    fails("invalid_input", lambda: db.search_hybrid("apple", 3, collection="docs", ef_search=5))

    assert db.set_collection_settings("docs", {"text_fields": ["title"], "rerank_multiplier": 8}) == {"persisted": True}

    job = db.next_import_job_id()
    rows = [{"op": "put", "id": "100", "vector": [1, 1]}, {"op": "put", "id": "101", "vector": [1, 0, 0]}]
    rejected = db.import_chunk(job, "1", rows, collection="docs")
    assert rejected["outcome"] == "rejected"
    assert [(r["row"], r["code"]) for r in rejected["invalid"]] == [(1, "dimension_mismatch")]
    committed = db.import_chunk(job, "1", rows, collection="docs", policy="skip_invalid")
    assert committed["outcome"] == "committed" and committed["rows_committed"] == 1 and committed["rows_skipped"] == 1
    fails("import_conflict", lambda: db.import_chunk(job, "1", rows[:1], collection="docs", policy="skip_invalid"))
    db.close()

    with native.open(path) as db:
        status = db.import_status(job)
        assert status["state"] == "open" and status["collection"] == "docs" and status["last_chunk"] == "1"
        assert len(status["last_digest"]) == 64
        assert db.import_chunk(job, "1", rows, collection="docs", policy="skip_invalid")["outcome"] == "already_committed"
        assert db.import_finish(job, collection="docs")["state"] == "finished"
        fails("import_conflict", lambda: db.import_abort(job, collection="docs"))
        fails("import_not_found", lambda: db.import_status("18446744073709551615"))
        assert db.get("100", collection="docs")["id"] == "100"
        assert db.list_collections()[1]["settings"]["rerank_multiplier"] == 8
        fails("collection_not_empty", lambda: db.drop_collection("docs"))
        db.create_collection("empty", 3)
        db.drop_collection("empty")
        fails("default_collection_immutable", lambda: db.drop_collection("default"))
        aborted_job = db.next_import_job_id()
        db.import_chunk(aborted_job, "1", [{"op": "put", "id": "9", "vector": [1, 1]}])
        aborted = db.import_abort(aborted_job)
        assert aborted["state"] == "aborted" and aborted["collection"] == "default"
        assert db.get("9")["vector"] == [1, 1]


if __name__ == "__main__":
    main()
