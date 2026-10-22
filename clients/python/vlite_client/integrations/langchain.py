"""LangChain adapter, loaded only when VLiteVectorStore is requested."""

from uuid import uuid4
from ._common import decode, encode, score, wire_id


def _build_class():
    try:
        from langchain_core.vectorstores import VectorStore
        from langchain_core.documents import Document
    except ImportError as exc:
        raise ImportError("LangChain support requires: pip install 'vlite-client[langchain]'") from exc

    class VLiteVectorStore(VectorStore):
        """A REST-backed store. Collection creation and ANN builds are explicit.

        Metadata is flat and scalar, matching VLite. IDs may be arbitrary strings;
        non-u64 IDs are deterministically hashed to u64 (see the client guide).
        """

        def __init__(self, client, embedding, *, collection=None, metric="squared_l2", mode="exact"):
            self.client = client
            self.records = client.collection(collection) if collection else client
            self._embedding = embedding
            self.metric = metric
            self.mode = mode

        @property
        def embeddings(self):
            return self._embedding

        def add_texts(self, texts, metadatas=None, *, ids=None, **kwargs):
            texts = list(texts)
            ids = list(ids) if ids is not None else [str(uuid4()) for _ in texts]
            metadatas = list(metadatas) if metadatas is not None else [{} for _ in texts]
            if len(ids) != len(texts) or len(metadatas) != len(texts):
                raise ValueError("texts, metadatas and ids must have the same length")
            vectors = self._embedding.embed_documents(texts)
            if len(vectors) != len(texts):
                raise ValueError("embedding count does not match texts")
            rows = [dict(op="put", id=wire_id(id), **encode(vector,
                    {"id": id, "text": text, "metadata": metadata}, metadata))
                    for id, text, vector, metadata in zip(ids, texts, vectors, metadatas)]
            # Preserve the server's atomic batch bound; chunks are separate commits.
            for offset in range(0, len(rows), 4096):
                self.records.batch(rows[offset:offset + 4096])
            return ids

        def delete(self, ids=None, **kwargs):
            if ids is None:
                raise ValueError("Explicit ids are required; delete-all is not implicit")
            rows = [{"op": "delete", "id": wire_id(id)} for id in ids]
            for offset in range(0, len(rows), 4096):
                self.records.batch(rows[offset:offset + 4096])
            return True

        def get_by_ids(self, ids):
            from vlite_client import VLiteError
            result = []
            for id in ids:
                try:
                    value = decode(self.records.get(wire_id(id)))
                except VLiteError as exc:
                    if exc.status == 404 and exc.code == "not_found":
                        continue
                    raise
                result.append(Document(id=value["id"], page_content=value["text"], metadata=value["metadata"]))
            return result

        def similarity_search_with_score_by_vector(self, embedding, k=4, **kwargs):
            request = {"vector": embedding, "k": k, "metric": self.metric, "mode": self.mode}
            # REST condition arrays, including typed equality and inclusive ranges.
            if kwargs.get("mode", self.mode) == "hybrid":
                raise ValueError("This adapter returns distance scores; hybrid retrieval is not supported")
            request.update(kwargs)
            hits = self.records.search(request)
            result = []
            for hit in hits:
                value = decode(self.records.get(hit["id"]))
                result.append((Document(id=value["id"], page_content=value["text"], metadata=value["metadata"]), hit["distance"]))
            return result

        def similarity_search_with_score(self, query, k=4, **kwargs):
            return self.similarity_search_with_score_by_vector(self._embedding.embed_query(query), k, **kwargs)

        def similarity_search(self, query, k=4, **kwargs):
            return [doc for doc, _ in self.similarity_search_with_score(query, k, **kwargs)]

        def similarity_search_by_vector(self, embedding, k=4, **kwargs):
            return [doc for doc, _ in self.similarity_search_with_score_by_vector(embedding, k, **kwargs)]

        def _select_relevance_score_fn(self):
            return lambda distance: score(distance, self.metric)

        @classmethod
        def from_texts(cls, texts, embedding, metadatas=None, **kwargs):
            ids = kwargs.pop("ids", None)
            store = cls(embedding=embedding, **kwargs)
            store.add_texts(texts, metadatas, ids=ids)
            return store

    return VLiteVectorStore


def __getattr__(name):
    if name == "VLiteVectorStore":
        value = _build_class()
        globals()[name] = value
        return value
    raise AttributeError(name)
