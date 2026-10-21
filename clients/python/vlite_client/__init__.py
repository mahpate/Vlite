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


class VersionedRecord(TypedDict):
    record: Record
    etag: str


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
    """One request shape for every mode. Approximate modes support squared_l2 and
    cosine only. ``filter`` is an AND of typed conditions (None/[] keeps every
    record), valid in exact mode and, on servers advertising
    ``features.filtered_ann``, in both approximate modes; those return at most k
    eligible records and may return fewer. ``ef_search`` (approximate only) then
    controls eligible-candidate retention, not a visit/time cap."""

    vector: list[float]
    k: int
    metric: NotRequired[Literal["squared_l2", "cosine", "inner_product"]]
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
    metric: NotRequired[Literal["squared_l2", "cosine"] | None]
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


class UnsupportedError(Exception):
    """The server does not advertise conditional writes; nothing was mutated."""


class _NoRedirect(HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


def _id(value: str) -> str:
    if not isinstance(value, str) or not re.fullmatch(r"0|[1-9][0-9]{0,19}", value) or int(value) > 18446744073709551615:
        raise ValueError("ID must be a canonical decimal u64 string")
    return value


# One strong RFC 9110 entity tag of visible ASCII: no W/ prefix, wildcard,
# whitespace, control characters, inner quotes or comma-separated lists.
_ETAG = re.compile(r'"[\x21\x23-\x7e]*"')


def _etag(value: str) -> str:
    if not isinstance(value, str) or not _ETAG.fullmatch(value):
        raise ValueError("if_match must be one exact strong entity tag returned by get_versioned")
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

    def _request(self, method: str, path: str, body=None, *, if_match: str | None = None, etag: bool = False):
        data = None if body is None else json.dumps(body, allow_nan=False).encode("utf-8")
        headers = {"Authorization": "Bearer " + self._token, "Accept": "application/json"}
        if data is not None:
            headers["Content-Type"] = "application/json"
        if if_match is not None:
            headers["If-Match"] = if_match
        request = Request(self._base + path, data=data, headers=headers, method=method)
        try:
            with self._opener.open(request, timeout=self._timeout) as response:
                raw = response.read()
                value = json.loads(raw) if raw else None
                if not etag:
                    return value
                tags = response.headers.get_all("ETag") or []
                if len(tags) != 1 or not _ETAG.fullmatch(tags[0]):
                    raise ValueError("response must carry exactly one strong ETag")
                return value, tags[0]
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

    def _require_conditional_writes(self) -> None:
        # Older servers ignore If-Match; a fresh check per write avoids silently
        # downgrading to an unconditional mutation (never cached across restarts).
        try:
            caps = self._request("GET", "/v1/capabilities")
        except VLiteError as error:
            if error.status != 404:
                raise
            caps = None
        features = caps.get("features") if isinstance(caps, dict) else None
        if not (isinstance(caps, dict) and type(caps.get("api_version")) is int and caps["api_version"] == 1
                and isinstance(features, dict) and features.get("conditional_writes") is True):
            raise UnsupportedError("Server does not advertise conditional_writes (API version 1); no write was sent")

    def _conditional(self, if_match: str | None) -> str | None:
        if if_match is None:
            return None
        _etag(if_match)
        self._require_conditional_writes()
        return if_match

    def get_versioned(self, id: str) -> VersionedRecord:
        """Return {"record": ..., "etag": ...}; the opaque tag is valid only until any
        committed database write or service restart. A missing/invalid ETag
        (for example, an older server) raises TransportError."""
        record, tag = self._request("GET", "/v1/vectors/" + _id(id), etag=True)
        return {"record": record, "etag": tag}

    def put(self, id: str, record: Put, *, if_match: str | None = None) -> None:
        """Replace a record. With if_match, send it unchanged as If-Match; a stale
        tag raises VLiteError 412 precondition_failed and nothing is retried. A
        conditional write first checks /v1/capabilities and raises UnsupportedError
        on servers that do not advertise conditional_writes."""
        path = "/v1/vectors/" + _id(id)
        self._request("PUT", path, record, if_match=self._conditional(if_match))

    def delete(self, id: str, *, if_match: str | None = None) -> bool:
        """Delete a record. With if_match, a stale tag or missing record raises
        VLiteError 412 precondition_failed and nothing is retried. Conditional deletes
        check capabilities first, like put."""
        path = "/v1/vectors/" + _id(id)
        return self._request("DELETE", path, if_match=self._conditional(if_match))["deleted"]

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
        """Send the request unchanged. There is no exact fallback: an older server's
        422 for an approximate filter, or 409 from approximate_ready, is raised."""
        return self._request("POST", "/v1/search", request)["hits"]

    def index_status(self) -> IndexStatus:
        return self._request("GET", "/v1/index/hnsw")

    def build_index(self, *, metric: Literal["squared_l2", "cosine"] = "squared_l2") -> None:
        self._request("POST", "/v1/index/hnsw", {} if metric == "squared_l2" else {"metric": metric})
