"""Run with unittest discover -s clients/python/vlite_client/integrations."""
import importlib.util
import sys
import types
import unittest
from unittest.mock import patch

from vlite_client.integrations import langchain, llamaindex
from vlite_client.integrations._common import metadata_value, wire_id


class Records:
    def __init__(self):
        self.rows = {}
        self.requests = []
        self.collection_name = None

    def collection(self, name):
        self.collection_name = name
        return self

    def batch(self, rows):
        for row in rows:
            if row["op"] == "delete":
                self.rows.pop(row["id"], None)
            else:
                self.rows[row["id"]] = row

    def get(self, id):
        return self.rows[id]

    def search(self, request):
        self.requests.append(request)
        return [{"id": id, "distance": 0.25} for id in list(self.rows)[:request["k"]]]

    def list_vectors(self, *, limit, after):
        return {"items": [{"id": id, "metadata": row["metadata"]} for id, row in self.rows.items()], "next_after": None}


class Embedding:
    def embed_documents(self, texts):
        return [[1.0, 0.0] for _ in texts]

    def embed_query(self, text):
        return [1.0, 0.0]


class Document:
    def __init__(self, **values):
        self.__dict__.update(values)


class BaseStore:
    def __init__(self, **values):
        self.__dict__.update(values)


class Node:
    def __init__(self, id="external-id", doc_id="source"):
        self.node_id, self.ref_doc_id = id, doc_id
        self.metadata = {"rank": 5, "flag": True}

    def get_embedding(self):
        return [1.0, 0.0]


def module(name, **values):
    result = types.ModuleType(name)
    result.__dict__.update(values)
    return result


class FakeTests(unittest.TestCase):
    def test_langchain_without_dependency(self):
        fakes = {
            "langchain_core.vectorstores": module("langchain_core.vectorstores", VectorStore=object),
            "langchain_core.documents": module("langchain_core.documents", Document=Document),
        }
        with patch.dict(sys.modules, fakes):
            store_class = langchain._build_class()
            records = Records()
            store = store_class.from_texts(["hello"], Embedding(), [{"rank": 5}], client=records, collection="docs", ids=["external-id"])
            self.assertEqual(records.collection_name, "docs")
            self.assertEqual(records.rows[wire_id("external-id")]["metadata"]["rank"], {"type": "integer", "value": "5"})
            result = store.similarity_search_with_score("hello", filter=[{"op": "eq", "field": "rank", "value": metadata_value(5)}])
            self.assertEqual(result[0][0].page_content, "hello")
            self.assertEqual(result[0][0].id, "external-id")
            self.assertEqual(result[0][1], 0.25)
            self.assertEqual(records.requests[0]["filter"][0]["value"]["value"], "5")
            self.assertEqual(store.get_by_ids(["external-id"])[0].metadata, {"rank": 5})
            with self.assertRaises(ValueError):
                store.similarity_search("hello", mode="hybrid")
            self.assertTrue(store.delete(["external-id"]))
            self.assertEqual(records.rows, {})
            with self.assertRaises(ValueError):
                store.add_texts(["a", "b"], ids=["one"])
            with self.assertRaises(ValueError):
                store.delete()

    def test_llamaindex_without_dependency(self):
        fakes = {
            "llama_index.core.vector_stores.types": module("types", BasePydanticVectorStore=BaseStore, VectorStoreQueryResult=Document),
            "llama_index.core.vector_stores.utils": module("utils", node_to_metadata_dict=lambda n, **kw: {"id": n.node_id}, metadata_dict_to_node=lambda p: Node(p["id"])),
            "llama_index.core.bridge.pydantic": module("pydantic", PrivateAttr=lambda: None),
        }
        with patch.dict(sys.modules, fakes):
            store_class = llamaindex._build_class()
            records = Records()
            store = store_class(records, collection="docs", metric="cosine")
            self.assertEqual(store.add([Node()]), ["external-id"])
            query = types.SimpleNamespace(mode="default", query_embedding=[1.0, 0.0], similarity_top_k=2,
                    filters=types.SimpleNamespace(condition="and", filters=[types.SimpleNamespace(operator=">=", key="rank", value=5)]))
            result = store.query(query)
            self.assertEqual(result.ids, ["external-id"])
            self.assertEqual(result.similarities, [0.75])
            self.assertEqual(records.requests[0]["filter"], [{"op": "range", "field": "rank", "min": metadata_value(5)}])
            query.filters.condition = "or"
            with self.assertRaises(ValueError):
                store.query(query)
            query.filters.condition = "and"
            query.filters.filters[0].operator = "in"
            with self.assertRaises(ValueError):
                store.query(query)
            query.filters = None
            with self.assertRaises(ValueError):
                store.query(query, filter=[])
            store.delete("source")
            self.assertEqual(records.rows, {})

    def test_clear_missing_dependency_errors(self):
        with patch.dict(sys.modules, {"langchain_core.vectorstores": None}):
            with self.assertRaisesRegex(ImportError, r"vlite-client\[langchain\]"):
                langchain._build_class()
        with patch.dict(sys.modules, {"llama_index.core.vector_stores.types": None}):
            with self.assertRaisesRegex(ImportError, r"vlite-client\[llamaindex\]"):
                llamaindex._build_class()

    def test_wire_types(self):
        self.assertEqual(wire_id("18446744073709551615"), "18446744073709551615")
        self.assertEqual(wire_id("uuid"), wire_id("uuid"))
        self.assertEqual(metadata_value(True), {"type": "boolean", "value": True})
        for value in (None, [], {}, 2**63, float("nan")):
            with self.assertRaises(ValueError):
                metadata_value(value)


def installed(name):
    try:
        return importlib.util.find_spec(name) is not None
    except (ImportError, ValueError):
        return False


class RealPackageTests(unittest.TestCase):
    @unittest.skipUnless(installed("langchain_core"), "langchain-core not installed")
    def test_langchain_real(self):
        from langchain_core.embeddings import Embeddings
        class RealEmbedding(Embeddings):
            def embed_documents(self, texts):
                return Embedding().embed_documents(texts)
            def embed_query(self, text):
                return Embedding().embed_query(text)
        store = langchain._build_class()(Records(), RealEmbedding())
        ids = store.add_texts(["hello"], [{"rank": 5}])
        self.assertEqual(store.similarity_search("hello")[0].page_content, "hello")
        self.assertEqual(store.get_by_ids(ids)[0].id, ids[0])
        self.assertEqual(store.as_retriever().invoke("hello")[0].page_content, "hello")
        store.delete(ids)

    @unittest.skipUnless(installed("llama_index.core"), "llama-index-core not installed")
    def test_llamaindex_real(self):
        from llama_index.core.schema import TextNode, NodeRelationship, RelatedNodeInfo
        from llama_index.core.vector_stores.types import VectorStoreQuery, MetadataFilters, MetadataFilter, FilterOperator
        records = Records()
        store = llamaindex._build_class()(records, collection="docs")
        node = TextNode(text="hello", id_="node-1", embedding=[1.0, 0.0], metadata={"rank": 5},
                        relationships={NodeRelationship.SOURCE: RelatedNodeInfo(node_id="doc-1")})
        self.assertEqual(store.add([node]), ["node-1"])
        result = store.query(VectorStoreQuery(query_embedding=[1.0, 0.0], similarity_top_k=1,
                filters=MetadataFilters(filters=[MetadataFilter(key="rank", operator=FilterOperator.GTE, value=5)])))
        self.assertEqual(result.nodes[0].get_content(), "hello")
        self.assertEqual(result.ids, ["node-1"])
        store.delete("doc-1")
        self.assertEqual(records.rows, {})


if __name__ == "__main__":
    unittest.main()
