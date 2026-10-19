"""Synchronous VLite REST client. No retries or redirect following."""

import json
from http.client import HTTPException
import math
import re
from typing import Literal, NotRequired, TypedDict
from urllib.error import HTTPError, URLError
from urllib.parse import urlencode, urlsplit
from urllib.request import HTTPRedirectHandler, ProxyHandler, Request, build_opener


class StringValue(TypedDict):
    type: Literal["string"]
    value: str


class IntegerValue(TypedDict):
    type: Literal["integer"]
    value: str


class FloatValue(TypedDict):
    type: Literal["float"]
    value: float


class BooleanValue(TypedDict):
    type: Literal["boolean"]
    value: bool


MetadataValue = StringValue | IntegerValue | FloatValue | BooleanValue
Metadata = dict[str, MetadataValue]


class Put(TypedDict):
    vector: list[float]
    payload_base64: NotRequired[str]
    metadata: NotRequired[Metadata]


class Record(TypedDict):
    id: str
    vector: list[float]
    payload_base64: str
    metadata: Metadata


class Summary(TypedDict):
    id: str
    metadata: Metadata


class Page(TypedDict):
    items: list[Summary]
    next_after: str | None


class Equal(TypedDict):
    op: Literal["eq"]
    field: str
    value: MetadataValue


class Range(TypedDict):
    op: Literal["range"]
    field: str
    min: NotRequired[IntegerValue | FloatValue | None]
    max: NotRequired[IntegerValue | FloatValue | None]


class Search(TypedDict):
    vector: list[float]
    k: int
    mode: NotRequired[Literal["exact", "approximate", "approximate_ready"]]
    ef_search: NotRequired[int]
    filter: NotRequired[list[Equal | Range] | None]


class BatchPut(Put):
    op: Literal["put"]
    id: str


class BatchDelete(TypedDict):
    op: Literal["delete"]
    id: str


class Hit(TypedDict):
    id: str
    distance: float


class DatabaseInfo(TypedDict):
    dimensions: int
    format_version: int
    records: int


class IndexConfig(TypedDict):
    m: int
    ef_construction: int
    seed: int


class IndexStatus(TypedDict):
    ready: bool
    buffered_ids: int
    buffer_vector_bytes: int
    max_buffer_ids: int
    max_buffer_vector_bytes: int
    config: IndexConfig


class VLiteError(Exception):
    """HTTP failure; status, server code and request ID are available."""

    def __init__(self, status: int, code: str, message: str, request_id: str | None):
        self.status, self.code, self.request_id = status, code, request_id
        super().__init__(f"HTTP {status}: {code}: {message}")


class TransportError(Exception):
    """Network/timeout/invalid response. A mutation may already be committed."""


class _NoRedirect(HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


def _id(value: str) -> str:
    if not isinstance(value, str) or not re.fullmatch(r"0|[1-9][0-9]{0,19}", value) or int(value) > 18446744073709551615:
        raise ValueError("ID must be a canonical decimal u64 string")
    return value


class Client:
    def __init__(self, base_url: str, token: str, *, timeout: float = 30):
        url = urlsplit(base_url)
        if url.scheme not in ("http", "https") or not url.hostname or url.username is not None or url.password is not None or url.query or url.fragment or url.path not in ("", "/"):
            raise ValueError("base_url must be an HTTP(S) origin without credentials, path, query or fragment")
        if not re.fullmatch(r"[A-Za-z0-9_-]{32,256}", token):
            raise ValueError("token must contain 32..256 base64url characters")
        if not math.isfinite(timeout) or timeout <= 0:
            raise ValueError("timeout must be finite and positive")
        self._base, self._token, self._timeout = base_url.rstrip("/"), token, timeout
        # Do not send the bearer token through implicitly configured environment proxies.
        self._opener = build_opener(ProxyHandler({}), _NoRedirect())

    def _request(self, method: str, path: str, body=None):
        data = None if body is None else json.dumps(body, allow_nan=False).encode("utf-8")
        headers = {"Authorization": "Bearer " + self._token, "Accept": "application/json"}
        if data is not None:
            headers["Content-Type"] = "application/json"
        request = Request(self._base + path, data=data, headers=headers, method=method)
        try:
            with self._opener.open(request, timeout=self._timeout) as response:
                raw = response.read()
                return json.loads(raw) if raw else None
        except HTTPError as error:
            with error:
                raw = error.read()
                try:
                    detail = json.loads(raw)["error"]
                    code, message = detail["code"], detail["message"]
                except (ValueError, KeyError, TypeError):
                    code, message = "http_error", "Server returned a nonstandard error response"
                raise VLiteError(error.code, code, message, error.headers.get("X-Request-ID")) from None
        except (URLError, OSError, HTTPException, ValueError) as error:
            raise TransportError("Request failed or response was invalid; mutation outcome may be unknown; no retry performed") from error

    def health(self) -> dict[str, str]:
        return self._request("GET", "/health")

    def ready(self) -> dict[str, str]:
        return self._request("GET", "/ready")

    def database(self) -> DatabaseInfo:
        return self._request("GET", "/v1/database")

    def metrics(self) -> dict[str, int | float | bool | None]:
        return self._request("GET", "/metrics")

    def get(self, id: str) -> Record:
        return self._request("GET", "/v1/vectors/" + _id(id))

    def put(self, id: str, record: Put) -> None:
        self._request("PUT", "/v1/vectors/" + _id(id), record)

    def delete(self, id: str) -> bool:
        return self._request("DELETE", "/v1/vectors/" + _id(id))["deleted"]

    def list_vectors(self, *, limit: int = 25, after: str | None = None) -> Page:
        if type(limit) is not int or not 1 <= limit <= 100:
            raise ValueError("limit must be an integer in 1..100")
        query = {"limit": str(limit)}
        if after is not None:
            query["after"] = _id(after)
        return self._request("GET", "/v1/vectors?" + urlencode(query))

    def batch(self, operations: list[BatchPut | BatchDelete]) -> None:
        for operation in operations:
            _id(operation["id"])
        self._request("POST", "/v1/batches", {"operations": operations})

    def search(self, request: Search) -> list[Hit]:
        return self._request("POST", "/v1/search", request)["hits"]

    def index_status(self) -> IndexStatus:
        return self._request("GET", "/v1/index/hnsw")

    def build_index(self) -> None:
        self._request("POST", "/v1/index/hnsw", {})
