"""Check bundled OpenAPI references and route parameter declarations."""
import json
from pathlib import Path
import re
import unittest

ROOT = Path(__file__).resolve().parents[1]
METHODS = {"get", "put", "post", "delete", "options", "head", "patch", "trace"}


def resolve(document, reference):
    if not reference.startswith("#/"):
        raise ValueError(f"external reference is not bundled: {reference}")
    value = document
    for token in reference[2:].split("/"):
        value = value[token.replace("~1", "/").replace("~0", "~")]
    return value


def validate(document):
    if document.get("openapi") != "3.1.0":
        raise ValueError("expected OpenAPI 3.1.0")

    def visit(value):
        if isinstance(value, dict):
            if "$ref" in value:
                resolve(document, value["$ref"])
            for item in value.values():
                visit(item)
        elif isinstance(value, list):
            for item in value:
                visit(item)

    visit(document)
    identifiers = set()
    for path, entry in document["paths"].items():
        if not path.startswith("/"):
            raise ValueError(f"invalid route: {path}")
        expected = set(re.findall(r"\{([^{}]+)\}", path))
        for method, operation in entry.items():
            if method not in METHODS:
                continue
            if not operation.get("responses"):
                raise ValueError(f"missing responses: {method} {path}")
            parameters = entry.get("parameters", []) + operation.get("parameters", [])
            declared = set()
            for parameter in parameters:
                if "$ref" in parameter:
                    parameter = resolve(document, parameter["$ref"])
                if parameter.get("in") == "path":
                    if parameter.get("required") is not True:
                        raise ValueError(f"optional path parameter: {method} {path}")
                    declared.add(parameter["name"])
            if declared != expected:
                raise ValueError(f"path parameters do not match: {method} {path}")
            identifier = operation.get("operationId")
            if identifier:
                if identifier in identifiers:
                    raise ValueError(f"duplicate operationId: {identifier}")
                identifiers.add(identifier)


class OpenApiTests(unittest.TestCase):
    def test_bundled_document(self):
        validate(json.loads((ROOT / "src" / "openapi.json").read_text(encoding="utf-8")))

    def test_rejects_missing_reference(self):
        with self.assertRaises(KeyError):
            validate({
                "openapi": "3.1.0", "paths": {},
                "value": {"$ref": "#/components/schemas/Missing"},
            })

    def test_feature_contract_surfaces(self):
        document = json.loads((ROOT / "src" / "openapi.json").read_text(encoding="utf-8"))
        request = document["components"]["schemas"]["SearchRequest"]
        self.assertEqual(set(request["properties"]["mode"]["enum"]), {
            "exact", "approximate", "approximate_ready", "auto", "hybrid",
        })
        legacy = document["paths"]["/v1/search"]["post"]["requestBody"]
        self.assertEqual(legacy["content"]["application/json"]["schema"],
                         {"$ref": "#/components/schemas/SearchRequest"})
        response = document["paths"]["/v1/capabilities"]["get"]["responses"]["200"]
        capabilities = response["content"]["application/json"]["schema"]
        self.assertIn("format_version", capabilities["required"])
        self.assertIn("scoped", capabilities["properties"]["role"]["enum"])
        metrics = document["paths"]["/metrics"]["get"]["responses"]["200"]["content"]
        self.assertIn("text/plain", metrics)

    def test_batch_routes_document_the_handler_answers(self):
        paths = json.loads((ROOT / "src" / "openapi.json").read_text(encoding="utf-8"))["paths"]
        legacy = paths["/v1/batches"]["post"]["responses"]
        named = paths["/v1/collections/{c}/batches"]["post"]["responses"]
        # One handler serves both routes: success is 204 No Content, never 200.
        for responses in (legacy, named):
            self.assertIn("204", responses)
            self.assertNotIn("200", responses)
            self.assertNotIn("content", responses["204"])
        # The named route answers what the legacy one does, plus a 403 (read_only
        # or forbidden_scope, from route authorization and the collection guard)
        # and the guard's 404 (collection_not_found). The legacy entry lists no
        # 403 although the shared handler refuses a read-only token there too.
        self.assertEqual(set(named), set(legacy) | {"403", "404"})
        for status, response in legacy.items():
            self.assertEqual(named[status], response, status)

    def test_rejects_missing_path_parameter(self):
        with self.assertRaises(ValueError):
            validate({
                "openapi": "3.1.0",
                "paths": {"/items/{id}": {"get": {"responses": {"200": {}}}}},
            })

    def test_tenant_deployment_routes_are_registered_with_their_contracts(self):
        document = json.loads((ROOT / "src" / "openapi.json").read_text(encoding="utf-8"))
        paths, schemas = document["paths"], document["components"]["schemas"]
        source = (ROOT / "src" / "server" / "tenant_provisioning.rs").read_text(encoding="utf-8")
        routes = re.search(r"fn routes\(\).*?\n}", source, re.S).group(0)
        served = set(re.findall(r'"(/v1/admin/tenant-deployment[^"]*)",?\s*(get|post)\(', routes))
        self.assertEqual(len(served), 7, served)
        # Every served route is documented, with no extra method.
        for path, method in served:
            self.assertTrue(path in paths, f"{path} is not in src/openapi.json")
            self.assertIn(method, paths[path], path)
            self.assertEqual(set(paths[path]) - {"parameters"}, {method}, path)
        submission = {"$ref": "#/components/schemas/TenantDeploymentSubmission"}
        action = {"$ref": "#/components/schemas/TenantDeploymentAction"}

        def body(path):
            return paths[path]["post"]["requestBody"]["content"]["application/json"]["schema"]

        submits = [path for path, method in served
                   if method == "post" and not path.endswith("{action}")]
        self.assertEqual(len(submits), 4)
        for path in submits:
            self.assertEqual(body(path), submission, path)
            responses = paths[path]["post"]["responses"]
            self.assertTrue({"200", "202", "401", "403", "409", "422", "503"} <= set(responses),
                            path)
            for status in ("200", "202"):
                answer = responses[status]["content"]["application/json"]["schema"]
                self.assertEqual(answer, {"$ref": "#/components/schemas/TenantDeploymentAccepted"})
        operation = "/v1/admin/tenant-deployment-operations/{operation}"
        self.assertTrue({"200", "401", "403", "404", "409", "503"}
                        <= set(paths[operation]["get"]["responses"]))
        # ITEM-023: the list is GET-only, paged by limit and an exclusive after.
        listing = "/v1/admin/tenant-deployment-operations"
        self.assertIn((listing, "get"), served)
        self.assertEqual(set(paths[listing]), {"get"})
        listed = paths[listing]["get"]
        self.assertTrue({"200", "400", "401", "403", "409", "503"} <= set(listed["responses"]))
        self.assertEqual(listed["responses"]["200"]["content"]["application/json"]["schema"],
                         {"$ref": "#/components/schemas/TenantDeploymentOperationPage"})
        page = schemas["TenantDeploymentOperationPage"]
        self.assertFalse(page["additionalProperties"])
        self.assertEqual(set(page["required"]), {"operations", "next_after"})
        self.assertEqual(page["properties"]["operations"]["items"],
                         {"$ref": "#/components/schemas/TenantDeploymentOperation"})
        self.assertIn({"type": "null"}, page["properties"]["next_after"]["anyOf"])
        query = {p["name"]: p["schema"] for p in listed["parameters"] if p["in"] == "query"}
        self.assertEqual(set(query), {"limit", "after"})
        self.assertEqual((query["limit"]["minimum"], query["limit"]["maximum"],
                          query["limit"]["default"]), (1, 100, 50))
        self.assertEqual(query["after"]["pattern"], "^[0-9a-f]{32}$")
        acting = operation + "/{action}"
        self.assertEqual(body(acting), action)
        self.assertTrue({"200", "401", "403", "404", "409", "422", "503"}
                        <= set(paths[acting]["post"]["responses"]))
        # The action enum is exactly the verbs the handler parses.
        verbs = set(re.findall(r'Self::\w+ => "([a-z-]+)"',
                               (ROOT / "src" / "server" / "tenant_provisioning" / "admin.rs")
                               .read_text(encoding="utf-8")))
        self.assertEqual(verbs, {"resume", "cancel", "acknowledge-delivery",
                                 "replace-credentials", "rollback", "release"})
        parameters = paths[acting].get("parameters", []) + paths[acting]["post"].get(
            "parameters", [])
        (declared,) = [p for p in parameters if p["name"] == "action"]
        self.assertEqual(set(declared["schema"]["enum"]), verbs)
        # Request schemas are strict and require what the handler requires.
        self.assertFalse(schemas["TenantDeploymentSubmission"]["additionalProperties"])
        self.assertEqual(set(schemas["TenantDeploymentSubmission"]["required"]),
                         {"idempotency_key", "request"})
        kinds = {variant["properties"]["kind"]["const"]
                 for variant in schemas["TenantDeploymentSubmission"]["properties"]["request"]
                 ["oneOf"]}
        self.assertEqual(kinds, {"create", "set_readers", "migrate", "retire"})
        self.assertFalse(schemas["TenantDeploymentAction"]["additionalProperties"])
        self.assertEqual(set(schemas["TenantDeploymentAction"]["required"]), {
            "idempotency_key", "expected_operation_generation", "expected_lifecycle_generation",
        })
        view = schemas["TenantDeploymentOperation"]
        self.assertTrue({"operation_id", "kind", "tenant_id", "phase", "next_step",
                         "operation_generation", "lifecycle_generation", "receipt_digest",
                         "observed"} <= set(view["required"]))
        self.assertEqual(set(view["properties"]["phase"]["enum"]), {
            "accepted", "reserved", "cancelling", "activated", "succeeded", "cancelled",
        })


if __name__ == "__main__":
    unittest.main()
