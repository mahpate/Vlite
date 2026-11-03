"""Regression tests for the registered-route coverage guard."""
import json
import unittest
from pathlib import Path

from api_reference import canonical_rest_key, route_keys


ROOT = Path(__file__).resolve().parents[1]


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
