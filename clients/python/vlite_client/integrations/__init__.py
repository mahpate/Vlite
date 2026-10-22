"""Optional framework adapters. Importing this package requires no framework dependencies."""

__all__ = ["VLiteVectorStore", "VLiteLlamaIndexVectorStore"]


def __getattr__(name):
    if name == "VLiteVectorStore":
        from .langchain import VLiteVectorStore
        return VLiteVectorStore
    if name == "VLiteLlamaIndexVectorStore":
        from .llamaindex import VLiteLlamaIndexVectorStore
        return VLiteLlamaIndexVectorStore
    raise AttributeError(name)
