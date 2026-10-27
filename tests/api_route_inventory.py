"""Regression tests for the registered-route coverage guard."""
import unittest

from api_reference import canonical_rest_key, route_keys


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


if __name__ == "__main__":
    unittest.main()
