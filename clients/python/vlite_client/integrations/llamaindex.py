"""LlamaIndex adapter, loaded only when VLiteLlamaIndexVectorStore is requested."""

from ._common import decode, encode, metadata_value, score, wire_id


def _build_class():
    try:
        from llama_index.core.vector_stores.types import BasePydanticVectorStore, VectorStoreQueryResult
        from llama_index.core.vector_stores.utils import node_to_metadata_dict, metadata_dict_to_node
        from llama_index.core.bridge.pydantic import PrivateAttr
    except ImportError as exc:
        raise ImportError("LlamaIndex support requires: pip install 'vlite-client[llamaindex]'") from exc

    class VLiteLlamaIndexVectorStore(BasePydanticVectorStore):
        """Stores complete node JSON in payloads and scalar metadata for filters."""

        stores_text: bool = True
        is_embedding_query: bool = True
        metric: str = "squared_l2"
        search_mode: str = "exact"
        _client: object = PrivateAttr()
        _records: object = PrivateAttr()

        def __init__(self, client, *, collection=None, metric="squared_l2", mode="exact", **kwargs):
            super().__init__(metric=metric, search_mode=mode, **kwargs)
            self._client = client
            self._records = client.collection(collection) if collection else client

        @property
        def client(self):
            return self._client

        @classmethod
        def class_name(cls):
            return "VLiteLlamaIndexVectorStore"

        def add(self, nodes, **kwargs):
            rows = []
            ids = []
            for node in nodes:
                metadata = dict(node.metadata)
                if "_vlite_ref_doc_id" in metadata:
                    raise ValueError("_vlite_ref_doc_id is reserved by the VLite adapter")
                if node.ref_doc_id is not None:
                    metadata["_vlite_ref_doc_id"] = node.ref_doc_id
                payload = node_to_metadata_dict(node, remove_text=False, flat_metadata=False)
                rows.append(dict(op="put", id=wire_id(node.node_id), **encode(node.get_embedding(), payload, metadata)))
                ids.append(node.node_id)
            for offset in range(0, len(rows), 4096):
                self._records.batch(rows[offset:offset + 4096])
            return ids

        def delete_nodes(self, node_ids=None, filters=None, **kwargs):
            if filters is not None or node_ids is None:
                raise ValueError("delete_nodes requires explicit node_ids and no filters")
            rows = [{"op": "delete", "id": wire_id(id)} for id in node_ids]
            for offset in range(0, len(rows), 4096):
                self._records.batch(rows[offset:offset + 4096])

        def delete(self, ref_doc_id, **kwargs):
            # Browse all pages before mutating; no top-k truncation of document deletion.
            ids = []
            after = None
            while True:
                page = self._records.list_vectors(limit=100, after=after)
                for item in page["items"]:
                    value = item["metadata"].get("_vlite_ref_doc_id")
                    if value == {"type": "string", "value": ref_doc_id}:
                        ids.append(item["id"])
                after = page["next_after"]
                if after is None:
                    break
            self.delete_nodes(ids)

        def query(self, query, **kwargs):
            mode = getattr(query.mode, "value", query.mode)
            if mode not in (None, "default"):
                raise ValueError("Only the default dense LlamaIndex query mode is supported")
            if query.query_embedding is None:
                raise ValueError("query_embedding is required")
            if getattr(query, "doc_ids", None) or getattr(query, "node_ids", None):
                raise ValueError("doc_ids/node_ids query restrictions are not supported; use metadata filters")
            conditions = []
            if query.filters is not None:
                condition = getattr(query.filters.condition, "value", query.filters.condition)
                if condition not in (None, "and"):
                    raise ValueError("VLite supports AND metadata filters only")
                for item in query.filters.filters:
                    if not hasattr(item, "operator"):
                        raise ValueError("Nested metadata filters are not supported")
                    operator = getattr(item.operator, "value", item.operator)
                    if operator == "==":
                        conditions.append({"op": "eq", "field": item.key, "value": metadata_value(item.value)})
                    elif operator in (">=", "<="):
                        value = metadata_value(item.value)
                        if value["type"] not in ("integer", "float"):
                            raise ValueError("Range bounds must be numeric")
                        conditions.append({"op": "range", "field": item.key,
                                           "min" if operator == ">=" else "max": value})
                    else:
                        raise ValueError("Unsupported filter: use equality or inclusive numeric ranges")
            request = {"vector": query.query_embedding, "k": query.similarity_top_k,
                       "metric": self.metric, "mode": self.search_mode, "filter": conditions}
            if any(key in kwargs for key in ("filter", "vector", "k", "metric", "mode")):
                raise ValueError("Set filters on VectorStoreQuery and metric/mode on the store")
            if self.search_mode == "hybrid":
                raise ValueError("This adapter supports dense distance modes only")
            request.update(kwargs)
            hits = self._records.search(request)
            nodes = [metadata_dict_to_node(decode(self._records.get(hit["id"]))) for hit in hits]
            return VectorStoreQueryResult(nodes=nodes, ids=[node.node_id for node in nodes],
                    similarities=[score(hit["distance"], self.metric) for hit in hits])

    return VLiteLlamaIndexVectorStore


def __getattr__(name):
    if name == "VLiteLlamaIndexVectorStore":
        value = _build_class()
        globals()[name] = value
        return value
    raise AttributeError(name)
