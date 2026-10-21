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

    def test_rejects_missing_path_parameter(self):
        with self.assertRaises(ValueError):
            validate({
                "openapi": "3.1.0",
                "paths": {"/items/{id}": {"get": {"responses": {"200": {}}}}},
            })


if __name__ == "__main__":
    unittest.main()
