"""Shared pieces of the benchmark and parity tools: the query suite, engine
specs and a tiny HTTP client. Standard library only.

Query files live in bench/queries/*.graphql. Header lines starting with
`#@ key: value` configure the case:

  description  free text
  role         x-hasura-role to send (default: admin, i.e. no role header)
  user_id      x-hasura-user-id to send
  variables    JSON object of GraphQL variables
  auth         "admin" (default, sends the admin secret) or "none"
  bench        "false" to exclude from load runs (error cases, etc.)
  parity       "false" to exclude from parity runs
  rest         "<METHOD> <path>" to call a REST endpoint instead of /v1/graphql
"""

from __future__ import annotations

import gzip
import json
import os
import re
import urllib.error
import urllib.request
from dataclasses import dataclass, field
from pathlib import Path

BENCH_DIR = Path(__file__).resolve().parent.parent
QUERIES_DIR = BENCH_DIR / "queries"
EXPECTED_DIR = BENCH_DIR / "expected"
RESULTS_DIR = BENCH_DIR / "results"
COMPOSE_FILE = BENCH_DIR / "docker-compose.yml"

ADMIN_SECRET = os.environ.get("BENCH_ADMIN_SECRET", "benchsecret")
HANDLER_URL = os.environ.get("BENCH_HANDLER_URL", "http://localhost:3000")

_HEADER_RE = re.compile(r"^#@\s*([a-z_]+)\s*:\s*(.*)$")


@dataclass
class Case:
    name: str
    description: str
    query: str
    variables: dict | None = None
    role: str | None = None
    user_id: str | None = None
    auth: str = "admin"
    bench: bool = True
    parity: bool = True
    rest: tuple[str, str] | None = None  # (method, path)

    @property
    def is_mutation(self) -> bool:
        return self.query.lstrip().startswith("mutation")

    def headers(self) -> dict[str, str]:
        # Real clients ask for gzip and Hasura compresses large bodies, so
        # load and parity runs both send it explicitly.
        h = {"content-type": "application/json", "accept-encoding": "gzip"}
        if self.auth == "admin":
            h["x-hasura-admin-secret"] = ADMIN_SECRET
        if self.role:
            h["x-hasura-role"] = self.role
        if self.user_id is not None:
            h["x-hasura-user-id"] = str(self.user_id)
        return h

    def method(self) -> str:
        return self.rest[0] if self.rest else "POST"

    def path(self) -> str:
        return self.rest[1] if self.rest else "/v1/graphql"

    def body(self) -> bytes | None:
        if self.rest:
            return None
        payload: dict = {"query": self.query}
        if self.variables is not None:
            payload["variables"] = self.variables
        return json.dumps(payload).encode()


def _parse_bool(value: str) -> bool:
    return value.strip().lower() not in ("false", "no", "0", "off")


def load_case(path: Path) -> Case:
    meta: dict[str, str] = {}
    body_lines = []
    for line in path.read_text().splitlines():
        m = _HEADER_RE.match(line)
        if m:
            meta[m.group(1)] = m.group(2).strip()
        else:
            body_lines.append(line)
    rest = None
    if "rest" in meta:
        method, rest_path = meta["rest"].split(None, 1)
        rest = (method.upper(), rest_path.strip())
    return Case(
        name=path.stem,
        description=meta.get("description", ""),
        query="\n".join(body_lines).strip() + "\n",
        variables=json.loads(meta["variables"]) if "variables" in meta else None,
        role=meta.get("role"),
        user_id=meta.get("user_id"),
        auth=meta.get("auth", "admin"),
        bench=_parse_bool(meta.get("bench", "true")),
        parity=_parse_bool(meta.get("parity", "true")),
        rest=rest,
    )


def load_suite(patterns: list[str] | None = None) -> list[Case]:
    cases = [load_case(p) for p in sorted(QUERIES_DIR.glob("*.graphql"))]
    if patterns:
        cases = [c for c in cases if any(re.search(p, c.name) for p in patterns)]
    return cases


@dataclass
class Engine:
    """An engine under test: a name, its base URL, and optionally the
    docker compose service running it (enables startup/memory metrics)."""

    name: str
    url: str
    service: str | None = None
    extra: dict = field(default_factory=dict)

    @classmethod
    def parse(cls, spec: str) -> "Engine":
        # name=url[@service]
        name, _, rest = spec.partition("=")
        if not rest:
            raise ValueError(f"engine spec must be name=url[@service], got {spec!r}")
        url, service = rest, None
        head, sep, tail = rest.rpartition("@")
        if sep and "/" not in tail and ":" not in tail:  # a service name, not URL userinfo
            url, service = head, tail
        return cls(name=name, url=url.rstrip("/"), service=service)


class RawNum:
    """A JSON number kept as its source text, so `15.90` and `15.9` (or `1`
    and `1.0`) stay different: clients see the bytes, not a float."""

    __slots__ = ("text",)

    def __init__(self, text: str):
        self.text = text

    def __eq__(self, other):
        return isinstance(other, RawNum) and other.text == self.text

    def __hash__(self):
        return hash(self.text)

    def __repr__(self):
        return self.text

    def value(self) -> float:
        return float(self.text)


class _OrderedObject(dict):
    """dict that remembers it came from JSON so order can be inspected."""

    def __init__(self, pairs):
        super().__init__(pairs)
        self.order = [k for k, _ in pairs]


def loads(text):
    """Parse JSON keeping key order and number text."""
    return json.loads(
        text, object_pairs_hook=_OrderedObject, parse_float=RawNum, parse_int=RawNum, parse_constant=RawNum
    )


def dumps(v, indent=2, _level=0) -> str:
    """Serialise values from `loads`, writing numbers back verbatim."""
    pad, inner = " " * (indent * _level), " " * (indent * (_level + 1))
    if isinstance(v, RawNum):
        return v.text
    if isinstance(v, dict):
        if not v:
            return "{}"
        items = [f"{inner}{json.dumps(k, ensure_ascii=False)}: {dumps(x, indent, _level + 1)}" for k, x in v.items()]
        return "{\n" + ",\n".join(items) + "\n" + pad + "}"
    if isinstance(v, list):
        if not v:
            return "[]"
        return "[\n" + ",\n".join(inner + dumps(x, indent, _level + 1) for x in v) + "\n" + pad + "]"
    return json.dumps(v, ensure_ascii=False)


@dataclass
class Response:
    status: int
    body: bytes  # decoded (gunzipped) body
    headers: dict = field(default_factory=dict)
    wire_bytes: int = 0  # body size as sent over the wire

    def json(self):
        return loads(self.body)


def _response(status, headers, raw) -> Response:
    hdrs = {k.lower(): v for k, v in headers.items()}
    body = gzip.decompress(raw) if hdrs.get("content-encoding") == "gzip" else raw
    return Response(status, body, hdrs, len(raw))


def send(engine: Engine, case: Case, timeout: float = 30.0) -> Response:
    req = urllib.request.Request(
        engine.url + case.path(), data=case.body(), headers=case.headers(), method=case.method()
    )
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            return _response(r.status, r.headers, r.read())
    except urllib.error.HTTPError as e:
        return _response(e.code, e.headers, e.read())


def http_get(url: str, timeout: float = 5.0) -> tuple[int, bytes]:
    try:
        with urllib.request.urlopen(url, timeout=timeout) as r:
            return r.status, r.read()
    except urllib.error.HTTPError as e:
        return e.code, e.read()


def http_post_json(url: str, payload: dict, timeout: float = 5.0) -> tuple[int, bytes]:
    req = urllib.request.Request(
        url, data=json.dumps(payload).encode(), headers={"content-type": "application/json"}
    )
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            return r.status, r.read()
    except urllib.error.HTTPError as e:
        return e.code, e.read()
