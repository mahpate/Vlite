"""Run both clients against isolated authenticated native servers.

Usage: python tests/clients_e2e.py <vlite-server> [--legacy-only]
The format v4 scenario (collections, imports, credentials, ...) needs a server
supporting ``--create-format 4``; ``--legacy-only`` runs only the v3 checks.
"""
import json
import os
from pathlib import Path
import queue
import re
import secrets
import subprocess
import sys
import tempfile
import threading
import time

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "clients" / "python"))
from vlite_client import Client, ImportRejectedError, VLiteError  # noqa: E402


def python_test(base, token):
    client = Client(base, token)
    assert client.health()["status"] == "ok"
    assert client.ready()["status"] == "ready"
    assert client.database()["dimensions"] == 3
    assert not client.index_status()["ready"]
    client.put("0", {"vector": [0, 0, 1]})
    try:
        client.search({"vector": [1, 0, 0], "k": 1, "mode": "approximate_ready"})
        raise AssertionError("Expected index_not_ready")
    except VLiteError as error:
        assert error.code == "index_not_ready" and error.status == 409
    # A filter never turns ready-only search into a build or an exact fallback,
    # and malformed filters are rejected before readiness is considered.
    for flt, status, code in [([FAR], 409, "index_not_ready"),
                              ([{"op": "range", "field": "rank", "min": {"type": "string", "value": "1"}}], 422, "invalid_input")]:
        try:
            client.search({"vector": [1, 0, 0], "k": 1, "mode": "approximate_ready", "filter": flt})
            raise AssertionError("Expected filtered ready-only rejection")
        except VLiteError as error:
            assert (error.status, error.code) == (status, code), error
    record = {"vector": [1, 0, 0], "payload_base64": "AP8=",
              "metadata": {"n": {"type": "integer", "value": "-9223372036854775808"}}}
    client.put("18446744073709551615", record)
    assert plain(client.get("18446744073709551615")) == dict(id="18446744073709551615", **record)
    assert client.search({"vector": [1, 0, 0], "k": 1, "metric": "cosine"})[0]["distance"] == 0
    assert client.search({"vector": [1, 0, 0], "k": 1, "metric": "inner_product"})[0]["distance"] == -1
    client.batch([{"op": "put", "id": "2", "vector": [0, 1, 0]}, {"op": "delete", "id": "0"}])
    first = client.list_vectors(limit=1)
    assert first["items"][0]["id"] == "2" and first["next_after"] == "2"
    last = client.list_vectors(after=first["next_after"], limit=1)
    assert last["items"][0]["id"] == "18446744073709551615" and last["next_after"] is None
    assert client.search({"vector": [1, 0, 0], "k": 2, "filter": [
        {"op": "eq", "field": "n", "value": record["metadata"]["n"]},
    ]})[0]["id"] == "18446744073709551615"
    client.build_index()
    assert client.index_status()["ready"]
    for mode in ["approximate", "approximate_ready"]:
        assert client.search({"vector": [1, 0, 0], "k": 1, "mode": mode})[0]["distance"] == 0
    client.build_index(metric="cosine")
    assert client.index_status()["metric"] == "cosine"
    assert client.search({"vector": [1,0,0], "k": 1, "mode": "approximate_ready", "metric": "cosine"})[0]["distance"] == 0
    try:
        client.search({"vector": [1,0,0], "k": 1, "mode": "approximate_ready"})
        raise AssertionError("Expected metric mismatch")
    except VLiteError as error:
        assert error.code == "index_not_ready"
    assert client.metrics()["database_healthy"]
    for action, status in [
        (lambda: Client(base, "b" * 32).database(), 401),
        (lambda: client.batch([{"op": "put", "id": "3", "vector": [1, 2]}]), 422),
    ]:
        try:
            action()
            raise AssertionError("Expected structured error")
        except VLiteError as error:
            assert error.status == status and error.request_id
    assert client.database()["records"] == 2
    assert client.delete("2") and not client.delete("2")
    try:
        client.get("2")
        raise AssertionError("Expected not found")
    except VLiteError as error:
        assert error.status == 404
    conditional_test(client)
    filtered_ann_test(client)
    print("PASS Python live REST client")


FAR = {"op": "eq", "field": "group", "value": {"type": "string", "value": "far"}}
# Typed AND: "far" records with integer rank >= 2.
RESTRICTIVE = [FAR, {"op": "range", "field": "rank", "min": {"type": "integer", "value": "2"}}]
NO_MATCH = [[{"op": "eq", "field": "group", "value": {"type": "string", "value": "none"}}],
            [FAR, {"op": "range", "field": "rank", "min": {"type": "integer", "value": "1000"}}],
            [{"op": "eq", "field": "group", "value": {"type": "integer", "value": "1"}}]]  # strict type match


def tagged(vector, group, rank):
    return {"vector": vector, "metadata": {"group": {"type": "string", "value": group},
                                           "rank": {"type": "integer", "value": str(rank)}}}


def check_filtered(client, metric, k, flt, eligible, expected):
    """ANN returns only eligible current records (at most k) and, on this tiny
    graph with ef_search above the live count, the same hits as the exact oracle."""
    query = {"vector": [1, 0, 0], "k": k, "metric": metric}
    exact = client.search(dict(query, filter=flt))
    assert [h["id"] for h in exact] == expected, (metric, exact)
    for mode in ["approximate", "approximate_ready"]:
        for extra in [{}, {"ef_search": 100}]:
            hits = client.search(dict(query, mode=mode, filter=flt, **extra))
            assert len(hits) <= k and {h["id"] for h in hits} <= eligible, (metric, mode, hits)
            assert hits == sorted(hits, key=lambda h: (h["distance"], int(h["id"]))), hits
            if extra:
                assert [h["id"] for h in hits] == expected, (metric, mode, hits, exact)
                assert all(abs(a["distance"] - b["distance"]) <= 1e-9 for a, b in zip(hits, exact)), (hits, exact)
        unfiltered = client.search(dict(query, mode=mode))
        assert len(unfiltered) == k and not {h["id"] for h in unfiltered} & eligible, "filter must be restrictive"


def filtered_ann_test(client):
    """Filtered ANN in both modes and metrics where every eligible record lies
    outside the unfiltered top k. Leaves the record count as it found it."""
    before = client.database()["records"]
    near = [str(200 + i) for i in range(16)]
    seed = [dict(op="put", id=i, **tagged([1, 0.05 * n, 0], "near", n)) for n, i in enumerate(near)]
    seed += [dict(op="put", id=i, **tagged(v, "far", r)) for i, v, r in [
        ("300", [0, 1, 0], 1), ("301", [0, 0, 1], 2), ("302", [0.1, 1, 0], 3),
        ("303", [-1, 0.2, 0], 4), ("304", [0, 1, 1], 5)]]
    for metric in ["squared_l2", "cosine"]:
        client.batch(seed)
        client.build_index(metric=metric)
        assert client.index_status()["ready"] and client.index_status()["metric"] == metric
        expected = ["302", "301", "304"]  # same order for both metrics (cosine 301/304 tie by ID)
        check_filtered(client, metric, 3, RESTRICTIVE, {"301", "302", "303", "304"}, expected)
        # Larger k than eligible count underfills rather than padding with ineligible records.
        check_filtered(client, metric, 10, RESTRICTIVE, {"301", "302", "303", "304"},
                       ["302", "301", "304", "303"])
        for flt in NO_MATCH:
            check_filtered(client, metric, 3, flt, set(), [])
        # Buffered updates/deletes without rebuilding: old graph entries must not leak.
        client.put("215", tagged([1, 0.75, 0], "far", 9))  # becomes eligible; still outside unfiltered top 3
        client.put("301", tagged([0, 0, 1], "near", 2))  # no longer eligible
        assert client.delete("302")
        client.batch([dict(op="put", id="305", **tagged([0.5, 1, 0], "far", 2)), {"op": "delete", "id": "304"}])
        status = client.index_status()
        assert status["ready"] and status["metric"] == metric and status["buffered_ids"] > 0, status
        check_filtered(client, metric, 3, RESTRICTIVE, {"215", "303", "305"}, ["215", "305", "303"])
        for flt in NO_MATCH:
            check_filtered(client, metric, 3, flt, set(), [])
        for mode in ["approximate", "approximate_ready"]:
            try:
                client.search({"vector": [1, 0, 0], "k": 1, "mode": mode, "metric": "inner_product", "filter": RESTRICTIVE})
                raise AssertionError("Expected inner_product ANN rejection")
            except VLiteError as error:
                assert error.status == 422 and error.code == "invalid_input", error
        client.batch([{"op": "delete", "id": op["id"]} for op in seed if op["id"] not in ("302", "304")]
                     + [{"op": "delete", "id": "305"}])
        assert client.database()["records"] == before
    print("PASS Python live filtered ANN")


def expect_412(action):
    try:
        action()
    except VLiteError as error:
        assert error.status == 412 and error.code == "precondition_failed" and error.request_id, error
        return
    raise AssertionError("Expected 412 precondition_failed")


def plain(record):
    """Record without the additive per-record ``revision`` field (newer servers)."""
    if "revision" in record:
        assert re.fullmatch(r"0|[1-9][0-9]{0,19}", record["revision"]), record
    return {key: value for key, value in record.items() if key != "revision"}


def expect_error(action, status, code=None):
    try:
        action()
    except VLiteError as error:
        assert error.status == status and (code is None or error.code == code), (status, code, error)
        return error
    raise AssertionError(f"Expected HTTP {status} {code or ''}")


def conditional_test(client):
    """Separate records; leaves the record count as it found it."""
    per_record = client.capabilities()["features"].get("per_record_etags") is True
    before = client.database()["records"]
    wide, other, gone = "18446744073709551614", "100", "101"
    client.put(wide, {"vector": [1, 0, 0]})
    first = client.get_versioned(wide)
    assert plain(first["record"]) == plain(client.get(wide)) == {"id": wide, "vector": [1, 0, 0],
                                                                 "payload_base64": "", "metadata": {}}
    assert ("revision" in first["record"]) == per_record, first
    assert re.fullmatch(r'"[\x21\x23-\x7e]+"', first["etag"]), "strong opaque tag"
    assert client.get_versioned(wide)["etag"] == first["etag"], "stable without writes"

    client.put(wide, {"vector": [0, 1, 0]}, if_match=first["etag"])
    assert client.get(wide)["vector"] == [0, 1, 0]
    expect_412(lambda: client.put(wide, {"vector": [0, 0, 1]}, if_match=first["etag"]))
    expect_412(lambda: client.delete(wide, if_match=first["etag"]))
    assert client.get(wide)["vector"] == [0, 1, 0]

    current = client.get_versioned(wide)["etag"]
    client.put(other, {"vector": [1, 1, 0]})
    if per_record:  # unrelated writes and batches leave this record's tag valid
        client.put(wide, {"vector": [0, 1, 0]}, if_match=current)
    else:  # legacy database-wide tags: any write invalidates every tag
        expect_412(lambda: client.delete(wide, if_match=current))
    current = client.get_versioned(wide)["etag"]
    client.batch([{"op": "put", "id": gone, "vector": [1, 0, 1]}])
    if per_record:
        client.put(wide, {"vector": [0, 1, 0]}, if_match=current)
    else:
        expect_412(lambda: client.put(wide, {"vector": [0, 0, 1]}, if_match=current))
    assert client.get(wide)["vector"] == [0, 1, 0]

    stale = client.get_versioned(gone)["etag"]
    assert client.delete(gone)
    expect_412(lambda: client.put(gone, {"vector": [1, 0, 0]}, if_match=stale))
    try:
        client.get(gone)
        raise AssertionError("conditional put must not recreate a deleted record")
    except VLiteError as error:
        assert error.status == 404

    def race(calls):
        barrier, results = threading.Barrier(len(calls)), [None] * len(calls)

        def run(index):
            barrier.wait()
            try:
                results[index] = ("ok", calls[index]())
            except VLiteError as error:
                results[index] = ("err", error.status, error.code)
        threads = [threading.Thread(target=run, args=(i,)) for i in range(len(calls))]
        for thread in threads:
            thread.start()
        for thread in threads:
            thread.join(30)
        assert all(r is not None for r in results), results
        winners = [i for i, r in enumerate(results) if r[0] == "ok"]
        assert len(winners) == 1, results
        assert all(r == ("err", 412, "precondition_failed") for i, r in enumerate(results) if i != winners[0]), results
        return winners[0], results[winners[0]][1]

    writers = 8
    tag = client.get_versioned(wide)["etag"]
    winner, _ = race([lambda i=i: client.put(wide, {"vector": [i + 2, 0, 0]}, if_match=tag) for i in range(writers)])
    assert client.get(wide)["vector"] == [winner + 2, 0, 0]
    tag = client.get_versioned(wide)["etag"]
    _, deleted = race([lambda: client.delete(wide, if_match=tag) for _ in range(writers)])
    assert deleted is True
    expect_412(lambda: client.delete(wide, if_match=tag))

    tag = client.get_versioned(other)["etag"]
    assert client.delete(other, if_match=tag) is True
    assert client.database()["records"] == before
    print("PASS Python live conditional writes")


V4_FLAGS = ["collections", "imports", "per_record_etags", "search_auto", "search_hybrid", "search_explain",
            "quantization", "auto_maintenance", "backup_schedule", "backup_verify_drill", "slow_query_log",
            "prometheus_metrics", "scoped_credentials", "persistent_audit"]
STATS = {"records_scanned", "distance_evaluations", "quantized_distance_evaluations", "graph_nodes_visited",
         "buffered_records_merged", "reranked_candidates", "index_candidates"}
# (id, vector, title, category); cosine collection.
DOCS = [("1", [1, 0, 0], "red apple pie", "fruit"), ("2", [0.9, 0.1, 0], "green apple", "fruit"),
        ("3", [0, 1, 0], "banana bread", "fruit"), ("4", [0, 0, 1], "carrot soup", "veg"),
        ("5", [0.1, 0, 1], "apple carrot salad", "veg"), ("6", [0, 0.9, 0.1], "steel pan", "tool")]
FRUIT = [{"op": "eq", "field": "category", "value": {"type": "string", "value": "fruit"}}]


def titled(vector, title, category):
    return {"vector": vector, "metadata": {"title": {"type": "string", "value": title},
                                           "category": {"type": "string", "value": category}}}


def import_rows(first, count):
    return [dict(op="put", id=str(first + i), **titled([1, 0.01 * i, 0], "import row", "bulk")) for i in range(count)]


def descending(hits, key):
    return all(a[key] >= b[key] for a, b in zip(hits, hits[1:]))


def v4_collections(client):
    caps = client.capabilities()
    assert caps["role"] == "admin" and caps.get("format_version") == 4, caps
    missing = [flag for flag in V4_FLAGS if caps["features"].get(flag) is not True]
    assert not missing, f"capabilities.features missing {missing}"
    docs = client.create_collection("docs", 3, {"metric": "cosine", "text_fields": ["title"],
                                                "metadata_indexes": [{"field": "category", "kind": "eq"}]})
    assert docs["name"] == "docs" and docs["dimensions"] == 3 and docs["live_records"] == 0, docs
    assert isinstance(docs["id"], int) and docs["settings"]["metric"] == "cosine", docs
    assert client.create_collection("wide", 768)["dimensions"] == 768
    expect_error(lambda: client.create_collection("docs", 3), 409, "collection_exists")
    expect_error(lambda: client.collection("missing").get("1"), 404, "collection_not_found")
    listed = client.list_collections()
    assert listed["format_version"] == 4 and {"default", "docs", "wide"} <= {c["name"] for c in listed["items"]}, listed
    d, w = client.collection("docs"), client.collection("wide")
    client.put("1", {"vector": [0, 0, 1]})  # legacy route: the default collection
    d.batch([dict(op="put", id=i, **titled(v, t, c)) for i, v, t, c in DOCS])
    w.put("1", {"vector": [0.5] * 768})
    # Independent dimensions and ID spaces; nothing leaks across collections.
    assert client.get("1")["vector"] == [0, 0, 1] and d.get("1")["vector"] == [1, 0, 0]
    assert d.get("3")["vector"] == [0, 1, 0] and len(w.get("1")["vector"]) == 768
    expect_error(lambda: d.put("9", {"vector": [0.5] * 768}), 422)
    expect_error(lambda: w.put("9", {"vector": [1, 0, 0]}), 422)
    expect_error(lambda: w.get("2"), 404)
    expect_error(lambda: client.get("2"), 404)
    assert client.database()["records"] == 1 and client.get_collection("docs")["live_records"] == len(DOCS)
    assert [h["id"] for h in client.search({"vector": [0, 1, 0], "k": 10})] == ["1"]
    assert client.get_collection("docs")["text_index"]["declared_fields"] == 1  # a count; names are in settings.text_fields
    expect_error(lambda: client.drop_collection("docs"), 409, "collection_not_empty")
    expect_error(lambda: client.drop_collection("default"), 409, "default_collection_immutable")

    # Per-record revisions and tags: unrelated writes do not invalidate a tag,
    # and a tag never matches the same ID in another collection.
    first = d.get_versioned("1")
    d.put("2", titled([0.9, 0.1, 0], "green apple", "fruit"))
    d.batch([{"op": "put", "id": "4", **titled([0, 0, 1], "carrot soup", "veg")}])
    d.put("1", titled([1, 0, 0], "red apple pie", "fruit"), if_match=first["etag"])
    second = d.get_versioned("1")
    assert int(second["record"]["revision"]) > int(first["record"]["revision"]), (first, second)
    expect_412(lambda: d.delete("1", if_match=first["etag"]))
    expect_412(lambda: client.put("1", {"vector": [0, 0, 1]}, if_match=second["etag"]))
    assert client.get_versioned("1")["etag"] != second["etag"]
    print("PASS Python live collections, isolation and revisions")


def v4_search(client):
    d = client.collection("docs")
    exact = d.search({"vector": [1, 0, 0], "k": 3, "filter": FRUIT})
    assert [h["id"] for h in exact] == ["1", "2", "3"], exact
    auto = d.search_detailed({"vector": [1, 0, 0], "k": 3, "filter": FRUIT, "mode": "auto", "explain": True})
    assert auto["hits"] == exact, auto  # no index yet: auto never builds, so it plans an exact strategy
    assert auto["plan"]["strategy"] in {"exact_scan", "exact_indexed"} and auto["plan"]["live_records"] == len(DOCS), auto
    assert STATS <= set(auto["stats"]), auto
    explained = d.search_detailed({"vector": [1, 0, 0], "k": 3, "mode": "exact", "explain": True})
    assert explained["plan"] is None and explained["stats"]["records_scanned"] >= len(DOCS), explained
    assert d.search_detailed({"vector": [1, 0, 0], "k": 3}).get("stats") is None
    text = d.search({"mode": "hybrid", "text": "apple", "k": 5})
    assert {h["id"] for h in text} == {"1", "2", "5"} and descending(text, "score"), text
    assert all(h["bm25"] is not None and h["vector_rank"] is None for h in text), text
    fused = d.search({"mode": "hybrid", "vector": [0, 0, 1], "text": "apple", "k": 6, "vector_leg": "exact"})
    assert fused[0]["id"] == "5" and fused[0]["distance"] is not None and fused[0]["bm25"] is not None, fused
    assert descending(fused, "score"), fused
    print("PASS Python live filters, auto/explain and hybrid")


def v4_quantization(client):
    d = client.collection("docs")
    status = d.index_status()
    assert not status["ready"] and status["memory"] is None and status["graph_nodes"] == 0, status
    assert status["settings_ready"] is False, status
    expect_error(lambda: d.search({"vector": [1, 0, 0], "k": 1, "mode": "approximate_ready"}), 409, "index_not_ready")
    expect_error(lambda: d.ann_quality({"samples": 4, "k": 2}), 409, "index_not_ready")
    updated = client.set_collection_settings("docs", {"quantization": "scalar_u8", "rerank_multiplier": 4})
    settings = updated["collection"]["settings"]
    assert updated["persisted"] is True and settings["quantization"] == "scalar_u8", updated
    assert settings["rerank_multiplier"] == 4 and settings["metric"] == "cosine" and settings["text_fields"] == ["title"]
    d.build_index()  # omitted identity: the collection's settings
    status = d.index_status()
    assert status["ready"] and status["settings_ready"], status
    assert status["quantization"] == "scalar_u8" and status["metric"] == "cosine", status
    assert status["graph_nodes"] == len(DOCS) and status["memory"]["code_bytes"] > 0, status
    ann = d.search_detailed({"vector": [1, 0, 0], "k": 2, "mode": "approximate_ready", "explain": True})
    assert [h["id"] for h in ann["hits"]] == ["1", "2"] and ann["stats"]["quantized_distance_evaluations"] > 0, ann
    # A different identity is not ready: no rebuild and no exact fallback.
    expect_error(lambda: d.search({"vector": [1, 0, 0], "k": 1, "mode": "approximate_ready", "quantization": "none"}),
                 409, "index_not_ready")
    quality = d.ann_quality({"samples": 4, "k": 2})
    assert quality["skipped_reason"] is None and quality["samples_used"] > 0 and quality["recall_at_k"] >= 0.5, quality
    assert d.start_rebuild()["outcome"] == "running"
    deadline = time.monotonic() + 600
    while True:
        outcome = d.poll_rebuild()["outcome"]
        if outcome != "running":
            break
        assert time.monotonic() < deadline, "rebuild did not finish"
        time.sleep(0.05)
    assert outcome == "published", outcome
    assert d.index_status()["quantization"] == "scalar_u8"
    assert "identity" in d.maintenance_status()
    d.maintenance_tick()
    print("PASS Python live quantization, rebuild, ANN quality and memory")


def v4_imports(client):
    """Leaves one open job (chunks 1-2 committed) to resume after a restart."""
    d = client.collection("docs")
    job = client.begin_import("docs")
    assert job["state"] == "open" and job["durable"] is False and int(job["last_chunk"]) == 0, job
    assert job["last_digest"] is None and job["collection_dropped"] is False, job
    rows = import_rows(100, 2)
    assert client.import_chunk(job["job"], "1", rows)["outcome"] == "committed"
    assert client.import_chunk(job["job"], "1", rows)["outcome"] == "already_committed"
    expect_error(lambda: client.import_chunk(job["job"], "1", rows[:1]), 409, "import_conflict")
    expect_error(lambda: client.import_chunk(job["job"], "3", rows), 409, "import_conflict")
    bad = [{"op": "put", "id": "110", "vector": [1, 0, 0]}, {"op": "put", "id": "111", "vector": [1, 0]}]
    try:
        client.import_chunk(job["job"], "2", bad)
        raise AssertionError("Expected rejected chunk")
    except ImportRejectedError as error:
        assert error.status == 422 and error.report["outcome"] == "rejected", error.report
        assert [row["row"] for row in error.report["invalid"]] == [1], error.report
    expect_error(lambda: d.get("110"), 404)  # a rejected chunk writes nothing
    skipped = client.import_chunk(job["job"], "2", bad, policy="skip_invalid")
    assert skipped["outcome"] == "committed" and skipped["invalid_rows"] == 1, skipped
    assert d.get("110")["vector"] == [1, 0, 0]
    status = client.import_status(job["job"])
    assert status["state"] == "open" and status["durable"] is True and int(status["last_chunk"]) == 2, status
    assert status["rows_committed"] == 3 and status["rows_skipped"] == 1, status
    assert re.fullmatch(r"[0-9a-f]{64}", status["last_digest"]) and status["collection_dropped"] is False, status
    expect_error(lambda: client.import_status("999999"), 404, "import_not_found")
    # A reservation without chunks cannot finish; aborting drops it.
    reserved = client.begin_import("wide")
    expect_error(lambda: client.finish_import(reserved["job"]), 409)
    client.abort_import(reserved["job"])
    # An aborted durable job refuses further chunks.
    wide_row = [{"op": "put", "id": "500", "vector": [0.0] * 768}]
    aborted = client.begin_import("wide")
    assert client.import_chunk(aborted["job"], "1", wide_row)["outcome"] == "committed"
    assert client.abort_import(aborted["job"])["state"] == "aborted"
    expect_error(lambda: client.import_chunk(aborted["job"], "2", wide_row), 409, "import_conflict")
    client.collection("wide").delete("500")
    print("PASS Python live import replay, rejection and conflicts")
    return job["job"], rows, bad


def v4_roles(client, base, reader):
    """Returns a revoked scoped token that must stay rejected after a restart."""
    ro = Client(base, reader)
    assert ro.capabilities()["role"] == "read_only"
    ro_docs = ro.collection("docs")
    for mode in ["exact", "approximate_ready", "auto"]:
        assert ro_docs.search({"vector": [1, 0, 0], "k": 1, "mode": mode})[0]["id"] == "1", mode
    assert ro_docs.search({"mode": "hybrid", "text": "apple", "k": 1})
    expect_error(lambda: ro_docs.search({"vector": [1, 0, 0], "k": 1, "mode": "approximate"}), 403, "read_only")
    expect_error(lambda: ro_docs.put("9", {"vector": [1, 0, 0]}), 403)

    expires = int(time.time() * 1000) + 3_600_000
    issued = client.create_credential({"name": "docs-reader", "collections": ["docs"],
                                       "operations": ["read", "search"], "expires_unix_ms": expires})
    credential, token = issued["credential"], issued["token"]
    assert re.fullmatch(r"vlk_[a-z2-7]{12}_[A-Za-z0-9_-]{43}", token) and credential["id"] in token, credential
    assert credential["state"] == "active" and credential["collections"] == ["docs"], credential
    assert re.fullmatch(r"[0-9a-f]{16}", credential["lineage"]), credential
    listed = client.list_credentials()
    assert listed["capacity"] == 256 and token not in json.dumps(listed), "secrets are never listed"
    scoped = Client(base, token)
    caps = scoped.capabilities()
    assert caps["role"] == "scoped" and caps["scope"]["credential_id"] == credential["id"], caps
    assert set(caps["scope"]["operations"]) == {"read", "search"} and caps["scope"]["collections"] == ["docs"], caps
    assert [c["name"] for c in scoped.list_collections()["items"]] == ["docs"]
    sd = scoped.collection("docs")
    assert sd.get("1")["vector"] == [1, 0, 0] and sd.search({"vector": [1, 0, 0], "k": 1, "mode": "auto"})
    for action in [lambda: sd.put("9", {"vector": [1, 0, 0]}), lambda: scoped.get("1"),
                   lambda: scoped.collection("wide").get("1"),
                   lambda: sd.search({"vector": [1, 0, 0], "k": 1, "mode": "approximate"})]:
        expect_error(action, 403, "forbidden_scope")
    expect_error(lambda: scoped.list_credentials(), 403)

    rotated = client.rotate_credential(credential["id"], grace_seconds=0)
    assert rotated["token"] != token and rotated["previous"]["id"] == credential["id"], rotated["credential"]
    assert rotated["credential"]["rotated_from"] == credential["id"], rotated["credential"]
    expect_error(lambda: Client(base, token).collection("docs").get("1"), 401)
    assert Client(base, rotated["token"]).collection("docs").get("1")["id"] == "1"
    assert client.revoke_credential(rotated["credential"]["id"])["credential"]["state"] == "revoked"
    expect_error(lambda: Client(base, rotated["token"]).collection("docs").get("1"), 401, "unauthorized")

    # Scopes bind the collection identity: drop and recreate under the same name fails closed.
    client.create_collection("tmp", 2)
    bound = client.create_credential({"name": "tmp-reader", "collections": ["tmp"], "operations": ["read"],
                                      "expires_unix_ms": expires})
    expect_error(lambda: Client(base, bound["token"]).collection("tmp").get("1"), 404)
    assert client.drop_collection("tmp") is True
    client.create_collection("tmp", 2)
    expect_error(lambda: Client(base, bound["token"]).collection("tmp").get("1"), 403, "forbidden_scope")
    audit = client.persistent_audit(limit=1000)
    assert audit["enabled"] is True and audit["items"], audit
    assert token not in json.dumps(audit) and rotated["token"] not in json.dumps(audit)
    print("PASS Python live read-only and scoped credential roles")
    return rotated["token"]


def v4_operations(client):
    maintenance = client.auto_maintenance()
    assert maintenance["config"]["enabled"] is False and "ticks" in maintenance["state"], maintenance
    changed = client.set_auto_maintenance({"enabled": False, "interval_secs": 60})
    assert changed["config"]["interval_ms"] == 60_000 and changed["config"]["enabled"] is False, changed
    ticks = client.auto_maintenance()["state"]["ticks"]
    client.tick_auto_maintenance()
    assert client.auto_maintenance()["state"]["ticks"] == ticks + 1
    schedule = client.backup_schedule()
    assert schedule["config"]["enabled"] is False and "runs" in schedule["state"], schedule
    changed = client.set_backup_schedule({"enabled": False, "interval_secs": 3600})
    assert changed["config"]["interval_secs"] == 3600, changed
    created = client.create_backup()
    name = created["name"]
    assert created["manifest"] is not None and created["manifest"]["name"] == name, created
    verified = client.verify_backup(name)
    assert verified["verified"] is True and verified["manifest"]["verified_unix_ms"], verified
    drill = client.drill_backup(name)
    assert drill["name"] == name and drill["drill"]["status"] == "passed", drill
    client.run_backup_schedule()
    assert client.backup_schedule()["state"]["runs"] >= 1
    assert any(item["name"] == name for item in client.list_backups()["items"])
    slow = client.slow_queries()
    assert slow["capacity"] == 256 and isinstance(slow["items"], list), slow
    text = client.prometheus_metrics()
    assert "# TYPE" in text and "vlite" in text, text[:200]
    assert client.metrics()["database_healthy"]
    print("PASS Python live operations, backups and metrics")


def v4_after_restart(client, base, saved, job, rows, bad, revoked):
    d = client.collection("docs")
    detail = client.get_collection("docs")
    assert detail["settings_persisted"] and detail["settings"]["quantization"] == "scalar_u8", detail
    assert detail["settings"]["rerank_multiplier"] == 4 and client.get_collection("wide")["dimensions"] == 768
    assert d.get_versioned("3")["etag"] == saved, "v4 tags survive a restart"
    d.put("3", titled([0, 1, 0], "banana bread", "fruit"), if_match=saved)
    expect_412(lambda: d.put("3", titled([0, 1, 0], "banana bread", "fruit"), if_match=saved))
    status = client.import_status(job)
    assert status["state"] == "open" and int(status["last_chunk"]) == 2, status
    with tempfile.TemporaryDirectory(prefix="vlite-import-") as directory:
        path = Path(directory) / "rows.ndjson"
        lines = rows + bad + import_rows(120, 2)
        path.write_text("".join(json.dumps(row) + "\n" for row in lines), encoding="utf-8")
        # Resume: skips chunk 1, replays chunk 2 (already_committed), commits chunk 3, finishes.
        finished = client.import_ndjson(path, "docs", chunk_size=2, policy="skip_invalid", job=job)
    assert finished["state"] == "finished" and int(finished["last_chunk"]) == 3, finished
    vector = d.get("121")["vector"]  # stored as f32: 0.01 round-trips approximately
    assert vector[0] == 1 and vector[2] == 0 and abs(vector[1] - 0.01) < 1e-6, vector
    expect_error(lambda: d.get("111"), 404)
    expect_error(lambda: Client(base, revoked).collection("docs").get("1"), 401)
    assert client.persistent_audit(limit=10)["items"]
    print("PASS Python live v4 restart: collections, tags, import resume, revocation")


def start_server(binary, database, env, *args):
    with tempfile.TemporaryFile("w+") as errors:
        process = subprocess.Popen([str(binary), str(database), *args, "--bind", "127.0.0.1:0"],
                                   stdout=subprocess.PIPE, stderr=errors, text=True, env=env)
        lines = queue.Queue()
        threading.Thread(target=lambda: lines.put(process.stdout.readline()), daemon=True).start()
        try:
            line = lines.get(timeout=120).strip()
        except queue.Empty:
            line = ""
        if not line.startswith("LISTENING http://"):
            stop_server(process)
            errors.seek(0)
            raise AssertionError(f"Server did not start: {errors.read()[-2000:]}")
    return process, line.removeprefix("LISTENING ")


def stop_server(process):
    if process.poll() is None:
        process.terminate()
    process.wait(timeout=15)
    process.stdout.close()


def v4_main(binary):
    """Bounded format v4 scenario across two server lifetimes, then the JavaScript v4 script."""
    with tempfile.TemporaryDirectory(prefix="vlite-client-v4-") as directory:
        admin_dir = Path(directory) / "admin"
        admin_dir.mkdir()
        admin, reader = secrets.token_urlsafe(32), secrets.token_urlsafe(32)
        env = dict(os.environ, VLITE_AUTH_TOKEN=admin, VLITE_READ_ONLY_TOKEN=reader, VLITE_ADMIN_DIR=str(admin_dir))
        for name in ["VLITE_AUTH_TOKEN_FILE", "VLITE_READ_ONLY_TOKEN_FILE"]:
            env.pop(name, None)
        database = Path(directory) / "data.vlite"
        process, base = start_server(binary, database, env, "--create-if-missing", "3", "--create-format", "4")
        try:
            client = Client(base, admin)
            v4_collections(client)
            v4_search(client)
            v4_quantization(client)
            job, rows, bad = v4_imports(client)
            revoked = v4_roles(client, base, reader)
            v4_operations(client)
            saved = client.collection("docs").get_versioned("3")["etag"]
        finally:
            stop_server(process)
        process, base = start_server(binary, database, env)
        try:
            v4_after_restart(Client(base, admin), base, saved, job, rows, bad, revoked)
        finally:
            stop_server(process)
    with tempfile.TemporaryDirectory(prefix="vlite-client-v4js-") as directory:
        admin_dir = Path(directory) / "admin"
        admin_dir.mkdir()
        admin, reader = secrets.token_urlsafe(32), secrets.token_urlsafe(32)
        env = dict(os.environ, VLITE_AUTH_TOKEN=admin, VLITE_READ_ONLY_TOKEN=reader, VLITE_ADMIN_DIR=str(admin_dir))
        for name in ["VLITE_AUTH_TOKEN_FILE", "VLITE_READ_ONLY_TOKEN_FILE"]:
            env.pop(name, None)
        process, base = start_server(binary, Path(directory) / "data.vlite", env,
                                     "--create-if-missing", "3", "--create-format", "4")
        try:
            subprocess.run(["node", str(ROOT / "clients" / "javascript" / "e2e-v4.mjs")],
                           env=dict(env, VLITE_URL=base), check=True, timeout=120)
        finally:
            stop_server(process)


def main():
    binary = Path(sys.argv[1]).resolve()
    for language in ["python", "javascript"]:
        with tempfile.TemporaryDirectory(prefix="vlite-client-") as directory:
            token = secrets.token_urlsafe(32)
            env = dict(os.environ, VLITE_AUTH_TOKEN=token)
            env.pop("VLITE_AUTH_TOKEN_FILE", None)
            process, base = start_server(binary, Path(directory) / "data.vlite", env, "--create-if-missing", "3")
            try:
                if language == "python":
                    python_test(base, token)
                else:
                    subprocess.run(["node", str(ROOT / "clients" / "javascript" / "e2e.mjs")],
                                   env=dict(env, VLITE_URL=base), check=True, timeout=60)
            finally:
                stop_server(process)
    if "--legacy-only" not in sys.argv[2:]:
        v4_main(binary)


if __name__ == "__main__":
    main()
