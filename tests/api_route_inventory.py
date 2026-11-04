"""Regression tests for the registered-route coverage guard."""
import json
import unittest
from pathlib import Path

from api_reference import canonical_rest_key, rest_keys, route_keys


ROOT = Path(__file__).resolve().parents[1]

RESERVED_ROUTE_INVENTORY = {
    "collection": set("""
rest:DELETE /v1/collections/{collection}
rest:DELETE /v1/collections/{collection}/kv/entry
rest:DELETE /v1/collections/{collection}/vectors/{id}
rest:DELETE /v1/vectors/{id}
rest:GET /v1/collections
rest:GET /v1/collections/{collection}
rest:GET /v1/collections/{collection}/kv/entries
rest:GET /v1/collections/{collection}/kv/entry
rest:GET /v1/collections/{collection}/vectors
rest:GET /v1/collections/{collection}/vectors/{id}
rest:GET /v1/database
rest:GET /v1/vectors
rest:GET /v1/vectors/{id}
rest:PATCH /v1/collections/{collection}/vectors/{id}/metadata
rest:PATCH /v1/vectors/{id}/metadata
rest:POST /v1/batches
rest:POST /v1/collections
rest:POST /v1/collections/{collection}/batches
rest:POST /v1/collections/{collection}/embed
rest:POST /v1/collections/{collection}/facets
rest:POST /v1/collections/{collection}/query
rest:POST /v1/collections/{collection}/recommend
rest:POST /v1/collections/{collection}/search
rest:POST /v1/collections/{collection}/search/dense-sparse
rest:POST /v1/collections/{collection}/search/disk-ann
rest:POST /v1/collections/{collection}/search/geo
rest:POST /v1/collections/{collection}/search/grouped
rest:POST /v1/collections/{collection}/search/named
rest:POST /v1/collections/{collection}/search/sparse
rest:POST /v1/collections/{collection}/stats/numeric
rest:POST /v1/collections/{collection}/vectors/configure
rest:POST /v1/search
rest:PUT /v1/collections/{collection}/kv/entry
rest:PUT /v1/collections/{collection}/settings
rest:PUT /v1/collections/{collection}/vectors/{id}
rest:PUT /v1/collections/{collection}/vectors/{id}/named
rest:PUT /v1/vectors/{id}
""".strip().splitlines()),
    "graph": set("""
rest:DELETE /v1/graphs/{graph}
rest:GET /v1/graphs
rest:GET /v1/graphs/{graph}
rest:GET /v1/graphs/{graph}/schema
rest:POST /v1/graphs
rest:POST /v1/graphs/{graph}/batch
rest:POST /v1/graphs/{graph}/edges/get
rest:POST /v1/graphs/{graph}/explain
rest:POST /v1/graphs/{graph}/neighbors
rest:POST /v1/graphs/{graph}/nodes/find
rest:POST /v1/graphs/{graph}/nodes/get
rest:POST /v1/graphs/{graph}/query
rest:POST /v1/graphs/{graph}/retrieve
rest:POST /v1/graphs/{graph}/shortest-path
rest:POST /v1/graphs/{graph}/traverse
rest:PUT /v1/graphs/{graph}/schema
""".strip().splitlines()),
    "feed-export": set("""
rest:GET /v1/admin/replication/export
rest:GET /v1/changes
""".strip().splitlines()),
    "imports": set("""
rest:GET /v1/imports/{job}
rest:POST /v1/collections/{collection}/imports
rest:POST /v1/imports/{job}/abort
rest:POST /v1/imports/{job}/finish
rest:PUT /v1/imports/{job}/chunks/{chunk}
""".strip().splitlines()),
    "index-admin": set("""
rest:GET /v1/collections/{collection}/index/hnsw
rest:GET /v1/collections/{collection}/index/hnsw/maintenance
rest:GET /v1/collections/{collection}/index/hnsw/rebuild
rest:GET /v1/index/hnsw
rest:GET /v1/index/hnsw/maintenance
rest:GET /v1/index/hnsw/rebuild
rest:POST /v1/collections/{collection}/index/disk-ann
rest:POST /v1/collections/{collection}/index/hnsw
rest:POST /v1/collections/{collection}/index/hnsw/maintenance/tick
rest:POST /v1/collections/{collection}/index/hnsw/quality
rest:POST /v1/collections/{collection}/index/hnsw/rebuild
rest:POST /v1/collections/{collection}/index/hnsw/rebuild/cancel
rest:POST /v1/collections/{collection}/index/hnsw/rebuild/poll
rest:POST /v1/index/hnsw
rest:POST /v1/index/hnsw/maintenance/tick
rest:POST /v1/index/hnsw/quality
rest:POST /v1/index/hnsw/rebuild
rest:POST /v1/index/hnsw/rebuild/cancel
rest:POST /v1/index/hnsw/rebuild/poll
rest:GET /v1/admin/maintenance/auto
rest:PUT /v1/admin/maintenance/auto
rest:POST /v1/admin/maintenance/auto/tick
rest:PUT /v1/index/hnsw/maintenance
""".strip().splitlines()),
    "observability": set("""
rest:GET /metrics
rest:GET /v1/admin/diagnostics/slow-queries
rest:GET /v1/admin/metrics/summary
""".strip().splitlines()),
    "webhooks": set("""
rest:DELETE /v1/admin/webhooks/{id}
rest:GET /v1/admin/webhooks
rest:GET /v1/admin/webhooks/{id}
rest:POST /v1/admin/webhooks
""".strip().splitlines()),
    "tenant-metadata": set("""
rest:DELETE /v1/admin/tenants/{id}
rest:GET /v1/admin/tenants
rest:GET /v1/admin/tenants/{id}/usage
""".strip().splitlines()),
}


class RouteInventoryTests(unittest.TestCase):
    def test_chains_qualified_methods_and_handler_bodies(self):
        source = '''
        Router::new().route("/one/{id}", get(fetch).put(save).delete(remove))
          .route("/health", get(|| async { map.get("key"); "post(fake)" }))
          .route("/patch", axum::routing::patch(update))
        '''
        self.assertEqual(route_keys(source), {
            "rest:GET /one/{id}", "rest:PUT /one/{id}", "rest:DELETE /one/{id}",
            "rest:GET /health", "rest:PATCH /patch",
        })

    def test_constant_routes(self):
        self.assertEqual(route_keys('const PATH: &str = "/graph"; Router::new().route(PATH, get(read).put(write))'),
                         {"rest:GET /graph", "rest:PUT /graph"})

    def test_unknown_method_or_constant_fails(self):
        for source in ('.route("/one", custom_router())', '.route(UNKNOWN, get(read))',
                       '.route(format!("/{}", name), get(read))', '.nest("/prefix", router)'):
            with self.assertRaises(AssertionError):
                route_keys(source)

    def test_comments_and_strings_do_not_create_routes(self):
        self.assertEqual(route_keys('''
            // .route("/fake", get(read))
            let documentation = r#".route("/also-fake", post(write))"#;
            Router::new().route("/real", get(read))
        '''), {"rest:GET /real"})

    def test_graph_query_dispatch(self):
        source = '''
            const QUERIES: [(&str, &str, Kind); 1] = [
                ("/v1/graphs/{graph}/nodes/get", "get-node", Kind::GetNode),
            ];
            for (route, _, kind) in QUERIES {
                reads = reads.route(route, query_route(kind));
            }
            fn query_route(kind: Kind) -> MethodRouter { post(handler) }
        '''
        self.assertEqual(route_keys(source), {"rest:POST /v1/graphs/{graph}/nodes/get"})

    def test_historical_placeholder_names(self):
        self.assertEqual(canonical_rest_key("rest:GET /v1/collections/{c}"),
                         "rest:GET /v1/collections/{collection}")
        self.assertEqual(canonical_rest_key("rest:PUT /v1/imports/{job}/chunks/{n}"),
                         "rest:PUT /v1/imports/{job}/chunks/{chunk}")

    def test_reserved_resource_routes_have_explicit_classification(self):
        classified = set().union(*RESERVED_ROUTE_INVENTORY.values())
        self.assertEqual(
            len(classified),
            sum(map(len, RESERVED_ROUTE_INVENTORY.values())),
            "a route cannot be assigned to multiple guard families",
        )
        registered = rest_keys(ROOT)

        def needs_reserved_guard(route):
            method, path = route.removeprefix("rest:").split(" ", 1)
            return (
                path.startswith((
                    "/v1/collections",
                    "/v1/vectors",
                    "/v1/graphs",
                    "/v1/imports",
                    "/v1/index/hnsw",
                    "/v1/admin/webhooks",
                ))
                or path in {
                    "/v1/database",
                    "/v1/search",
                    "/v1/batches",
                    "/v1/changes",
                    "/v1/admin/replication/export",
                    "/v1/admin/maintenance/auto",
                    "/v1/admin/maintenance/auto/tick",
                    "/v1/admin/diagnostics/slow-queries",
                    "/v1/admin/metrics/summary",
                    "/metrics",
                }
                or (
                    path.startswith("/v1/admin/tenants")
                    and method in {"GET", "DELETE"}
                )
            )

        candidates = {route for route in registered if needs_reserved_guard(route)}
        self.assertEqual(
            candidates,
            classified,
            f"update explicit reserved-route classification: "
            f"unclassified={sorted(candidates - classified)}, "
            f"stale={sorted(classified - candidates)}",
        )


class Gap8OpenApiTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.openapi = json.loads((ROOT / "src" / "openapi.json").read_text(encoding="utf-8"))
        cls.schemas = cls.openapi["components"]["schemas"]
        cls.paths = cls.openapi["paths"]

    def test_existing_gap8_routes_reference_typed_schemas_and_provider_errors(self):
        routes = {
            "/v1/collections/{collection}/embed": "EmbedRequest",
            "/v1/search": "SearchRequest",
            "/v1/collections/{c}/search": "SearchRequest",
        }
        provider_responses = {
            "409": "ProviderNotConfiguredError",
            "422": "ProviderInvalidInputError",
            "429": "ProviderBusyError",
            "502": "ProviderInvalidResponseError",
            "503": "ProviderUnavailableError",
            "504": "ProviderTimeoutError",
        }

        for route, request_schema in routes.items():
            with self.subTest(route=route):
                operation = self.paths[route]["post"]
                request_ref = operation["requestBody"]["content"]["application/json"]["schema"]["$ref"]
                self.assertEqual(request_ref, f"#/components/schemas/{request_schema}")

                responses = operation["responses"]
                self.assertIn("401", responses)
                self.assertIn("403", responses)
                for status, provider_schema in provider_responses.items():
                    response = responses[status]
                    schema = response["content"]["application/json"]["schema"]
                    if status in {"409", "422", "429", "503"}:
                        self.assertIn("anyOf", schema)
                        self.assertNotIn("oneOf", schema)
                        refs = {branch.get("$ref") for branch in schema["anyOf"]}
                        self.assertIn("#/components/schemas/Error", refs)
                        self.assertIn(f"#/components/schemas/{provider_schema}", refs)
                        if status == "409":
                            self.assertIn(
                                "#/components/schemas/ProviderSnapshotConflictError", refs
                            )
                    else:
                        self.assertEqual(schema["$ref"], f"#/components/schemas/{provider_schema}")

                self.assertNotIn("Retry-After", responses["409"].get("headers", {}))
                retry_after = responses["429"]["headers"]["Retry-After"]["schema"]
                self.assertEqual(retry_after["type"], "integer")

                success_schema = responses["200"]["content"]["application/json"]["schema"]
                if route.endswith("/embed"):
                    self.assertEqual(success_schema["$ref"], "#/components/schemas/EmbedResponse")
                    embed_request = self.schemas["EmbedRequest"]
                    provider = embed_request["properties"]["provider"]
                    self.assertEqual(provider["allOf"][0]["$ref"],
                                     "#/components/schemas/ProviderAlias")
                    self.assertEqual(provider["default"], "hashing")
                    self.assertEqual(self.schemas["EmbedResponse"]["required"],
                                     ["vector", "dimensions", "provider"])
                else:
                    self.assertIn("hits", success_schema["properties"])
                    self.assertNotIn("ranking", success_schema.get("required", []))
                    ranking = success_schema["properties"]["ranking"]
                    self.assertEqual(ranking["properties"]["provider"]["$ref"],
                                     "#/components/schemas/ProviderAlias")
                    self.assertEqual(ranking["properties"]["order"]["enum"], ["score_desc_id_asc"])
                    hit = success_schema["properties"]["hits"]["items"]
                    for optional_score in ("score", "rerank_score"):
                        self.assertIn(optional_score, hit["properties"])
                        self.assertNotIn(optional_score, hit.get("required", []))

    def test_exactly_seven_named_provider_error_codes_are_published(self):
        expected = {
            "ProviderNotConfiguredError": "provider_not_configured",
            "ProviderInvalidInputError": "invalid_provider_input",
            "ProviderInvalidResponseError": "provider_invalid_response",
            "ProviderUnavailableError": "provider_unavailable",
            "ProviderTimeoutError": "provider_timeout",
            "ProviderBusyError": "provider_busy",
            "ProviderSnapshotConflictError": "provider_snapshot_conflict",
        }
        provider_errors = {
            name: schema for name, schema in self.schemas.items()
            if name.startswith("Provider") and name.endswith("Error")
        }
        self.assertEqual(set(provider_errors), set(expected))

        published_codes = {}
        for name, schema in provider_errors.items():
            code = schema["allOf"][1]["properties"]["error"]["properties"]["code"]["const"]
            published_codes[name] = code
        self.assertEqual(published_codes, expected)

    def test_rerank_provider_and_lambda_are_conditionally_allowed(self):
        rerank = self.schemas["RerankOptions"]
        self.assertEqual(rerank["properties"]["mode"]["enum"], ["exact", "mmr", "custom"])
        conditions = rerank["allOf"]
        custom = next(condition for condition in conditions
                      if condition["if"]["properties"]["mode"].get("const") == "custom")
        self.assertIn("provider", custom["then"]["required"])
        self.assertEqual(custom["else"]["not"]["required"], ["provider"])

        non_mmr = next(condition for condition in conditions
                       if condition["if"]["properties"]["mode"].get("not") == {"const": "mmr"})
        self.assertEqual(non_mmr["then"]["not"]["required"], ["lambda"])


if __name__ == "__main__":
    unittest.main()
