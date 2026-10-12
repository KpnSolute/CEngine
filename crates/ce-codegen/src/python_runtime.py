# ---- runtime (identical in every generated client) -------------------------------------------
from __future__ import annotations

import json
import urllib.error
import urllib.parse
import urllib.request
import uuid
from dataclasses import dataclass, field
from datetime import date, datetime
from decimal import Decimal
from typing import Any, ClassVar, Generic, Iterator, Literal, Optional, TypeVar, Union


class CEngineError(Exception):
    """An error answer from the server (RFC 9457 problem), with its stable machine code."""

    def __init__(self, status: int, problem: dict[str, Any]):
        self.status = status
        self.code: str = problem.get("code", "unknown")
        self.detail: str = problem.get("detail", "")
        self.request_id: str = problem.get("request_id", "")
        self.retryable: bool = bool(problem.get("retryable", False))
        self.errors: list[dict[str, Any]] = problem.get("errors", [])
        self.problem = problem
        super().__init__(f"{status} {self.code}: {self.detail}")


def _enc(value: Any) -> Any:
    """A Python value as the JSON the API expects. Decimals travel as exact digit strings."""
    if isinstance(value, Decimal):
        return format(value, "f")
    if isinstance(value, datetime):
        if value.tzinfo is None:
            raise ValueError("datetimes must be timezone-aware")
        return value.isoformat()
    if isinstance(value, date):
        return value.isoformat()
    if isinstance(value, float):
        raise TypeError("use Decimal, not float, for decimal fields")
    return value


def _dec(kind: str, value: Any) -> Any:
    if value is None:
        return None
    if kind == "decimal":
        return Decimal(str(value))
    if kind == "date":
        return date.fromisoformat(value)
    if kind == "datetime":
        return datetime.fromisoformat(value)
    return value


class Filter:
    """A filter expression. Combine with `&` (and), `|` (or) and `~` (not)."""

    def __init__(self, node: dict[str, Any]):
        self.node = node

    def __and__(self, other: Filter) -> Filter:
        return Filter({"and": [self.node, other.node]})

    def __or__(self, other: Filter) -> Filter:
        return Filter({"or": [self.node, other.node]})

    def __invert__(self) -> Filter:
        return Filter({"not": self.node})

    def to_json(self) -> str:
        return json.dumps(self.node, separators=(",", ":"))


@dataclass(frozen=True)
class Sort:
    field: str
    descending: bool = False

    def __str__(self) -> str:
        return ("-" if self.descending else "") + self.field


class Field:
    """A field of an entity, as a filter and sort builder: `Item.f.price > 3`."""

    def __init__(self, name: str):
        self.name = name

    def _op(self, op: str, value: Any = None) -> Filter:
        node: dict[str, Any] = {"field": self.name, "op": op}
        if op != "null" or value is not None:
            node["value"] = _enc(value) if not isinstance(value, (list, tuple)) else [_enc(v) for v in value]
        return Filter(node)

    def __eq__(self, value: Any) -> Filter:  # type: ignore[override]
        return self._op("eq", value)

    def __ne__(self, value: Any) -> Filter:  # type: ignore[override]
        return self._op("ne", value)

    def __gt__(self, value: Any) -> Filter:
        return self._op("gt", value)

    def __ge__(self, value: Any) -> Filter:
        return self._op("gte", value)

    def __lt__(self, value: Any) -> Filter:
        return self._op("lt", value)

    def __le__(self, value: Any) -> Filter:
        return self._op("lte", value)

    __hash__ = None  # type: ignore[assignment]

    def is_in(self, values: Union[list, tuple]) -> Filter:
        return self._op("in", list(values))

    def not_in(self, values: Union[list, tuple]) -> Filter:
        return self._op("nin", list(values))

    def like(self, pattern: str) -> Filter:
        return self._op("like", pattern)

    def ilike(self, pattern: str) -> Filter:
        return self._op("ilike", pattern)

    def is_null(self, want: bool = True) -> Filter:
        return self._op("null", want)

    def asc(self) -> Sort:
        return Sort(self.name, False)

    def desc(self) -> Sort:
        return Sort(self.name, True)


T = TypeVar("T")


@dataclass
class Page(Generic[T]):
    """One page of a list. `next` is the cursor for the following page, or None at the end."""

    items: list[T]
    next: Optional[str] = None
    request_id: str = ""

    def __iter__(self) -> Iterator[T]:
        return iter(self.items)

    def __len__(self) -> int:
        return len(self.items)


class _Http:
    def __init__(self, base_url: str, token: str, timeout: float):
        self.base = base_url.rstrip("/")
        self.token = token
        self.timeout = timeout

    def call(
        self,
        method: str,
        path: str,
        *,
        params: Optional[dict[str, str]] = None,
        body: Optional[dict[str, Any]] = None,
        headers: Optional[dict[str, str]] = None,
    ) -> tuple[int, Any, dict[str, str]]:
        url = self.base + path
        if params:
            url += "?" + urllib.parse.urlencode(params)
        data = None if body is None else json.dumps(body, separators=(",", ":")).encode()
        h = {"Authorization": f"Bearer {self.token}", "Accept": "application/json"}
        if data is not None:
            h["Content-Type"] = "application/json"
        h.update(headers or {})
        req = urllib.request.Request(url, data=data, method=method, headers=h)
        try:
            with urllib.request.urlopen(req, timeout=self.timeout) as res:
                raw = res.read()
                return res.status, (json.loads(raw) if raw else None), dict(res.headers)
        except urllib.error.HTTPError as e:
            raw = e.read()
            try:
                problem = json.loads(raw)
            except ValueError:
                problem = {"code": "bad_response", "detail": raw.decode("utf-8", "replace")[:200]}
            raise CEngineError(e.code, problem) from None


class Model:
    """Base of the generated record classes."""

    id: str
    version: int

    _entity: ClassVar[str]
    _columns: ClassVar[dict[str, tuple[str, bool]]]  # json name -> (type, read-only)
    _pyname: ClassVar[dict[str, str]]  # json name -> attribute name

    @classmethod
    def from_json(cls, data: dict[str, Any]):
        kwargs: dict[str, Any] = {"id": data["id"], "version": data["version"]}
        for name, (kind, _) in cls._columns.items():
            if name in data:
                kwargs[cls._pyname[name]] = _dec(kind, data[name])
        return cls(**kwargs)

    def to_json(self) -> dict[str, Any]:
        """The writable fields that are set, as the JSON body of a create."""
        out: dict[str, Any] = {}
        for name, (_, readonly) in self._columns.items():
            value = getattr(self, self._pyname[name])
            if not readonly and value is not None:
                out[name] = _enc(value)
        return out


M = TypeVar("M", bound=Model)


class Resource(Generic[M]):
    """The endpoints of one entity."""

    def __init__(self, http: _Http, inst: str, model: type[M]):
        self._http = http
        self._path = f"/v1/{inst}/{model._entity}"
        self._model = model

    def list(
        self,
        *,
        where: Optional[Filter] = None,
        sort: Optional[Union[Sort, list[Sort]]] = None,
        limit: Optional[int] = None,
        after: Optional[str] = None,
        q: Optional[str] = None,
        select: Optional[list[str]] = None,
    ) -> Page[M]:
        params: dict[str, str] = {}
        if where is not None:
            params["filter"] = where.to_json()
        if sort is not None:
            keys = sort if isinstance(sort, list) else [sort]
            params["sort"] = ",".join(str(k) for k in keys)
        if limit is not None:
            params["limit"] = str(limit)
        if after is not None:
            params["after"] = after
        if q is not None:
            params["q"] = q
        if select is not None:
            params["select"] = ",".join(select)
        _, body, _ = self._http.call("GET", self._path, params=params)
        return Page(
            [self._model.from_json(r) for r in body["data"]],
            body["meta"].get("next"),
            body["meta"].get("request_id", ""),
        )

    def iter(self, **kwargs: Any) -> Iterator[M]:
        """Every matching record, fetching page after page."""
        after = kwargs.pop("after", None)
        while True:
            page = self.list(after=after, **kwargs)
            yield from page
            if not page.next:
                return
            after = page.next

    def get(self, id: str) -> M:
        _, body, _ = self._http.call("GET", f"{self._path}/{id}")
        return self._model.from_json(body["data"])

    def create(self, record: Union[M, dict[str, Any]], *, key: Optional[str] = None) -> M:
        """Creates a record. The idempotency key defaults to a fresh one; pass your own to make
        a retry of this exact call safe."""
        body = record.to_json() if isinstance(record, Model) else {k: _enc(v) for k, v in record.items()}
        _, out, _ = self._http.call(
            "POST", self._path, body=body, headers={"Idempotency-Key": key or str(uuid.uuid4())}
        )
        return self._model.from_json(out["data"])

    def update(self, record: Union[M, str], changes: dict[str, Any], *, version: Optional[int] = None) -> M:
        """Patches a record at the version you last saw; a stale version is a 412 error."""
        id_ = record if isinstance(record, str) else record.id
        ver = version if version is not None else getattr(record, "version", None)
        if ver is None:
            raise ValueError("pass the record, or version=")
        _, out, _ = self._http.call(
            "PATCH",
            f"{self._path}/{id_}",
            body={k: _enc(v) for k, v in changes.items()},
            headers={"If-Match": f'"{ver}"'},
        )
        return self._model.from_json(out["data"])

    def delete(self, record: Union[M, str], *, version: Optional[int] = None) -> None:
        id_ = record if isinstance(record, str) else record.id
        ver = version if version is not None else getattr(record, "version", None)
        if ver is None:
            raise ValueError("pass the record, or version=")
        self._http.call("DELETE", f"{self._path}/{id_}", headers={"If-Match": f'"{ver}"'})
# ---- end of runtime ---------------------------------------------------------------------------
