#!/usr/bin/env python3
"""Write the JWT suites whose claims depend on the current time (upstream test_jwt.py's
TestJwtExpirySkew* and TestSubscriptionJwtExpiry* classes).

The other JWT classes use pre-signed tokens (conformance/tools/gen_jwt_suites.py). These need a
token made when the case runs, so every case names a key in conformance/keys/ and the runner
signs it (see the `jwt` header form in conformance/README.md). Expected responses start as
upstream's assertions and are then recorded from Hasura with
`hge-conformance --filter Expiry hasura --record`.

Re-running this overwrites the case files with unrecorded ones: record again afterwards.
"""

import copy
import json
import os
import shutil
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.join(HERE, ".."))
from port_tests_py import dump_yaml  # noqa: E402

ROOT = os.path.join(HERE, "..", "..", "suites")
SOURCE = "permissions/TestGraphqlQueryPermissions"  # its setup.sql and metadata.json
AREA = "auth"

KEY_TYPE = {"rsa": "RS512", "ed25519": "Ed25519", "es": "ES256"}
KEYS = {"rsa": "Rsa", "ed25519": "Ed25519", "es": "Es"}
ENDPOINTS = ["/v1/graphql", "/v1alpha1/graphql"]

QUERY = """query {
  article (
    where: { is_published : {_eq : false}}
  ) {
    id
    title
    content
    is_published
    author {
      id
      name
    }
  }
}
"""
OK = {"data": {"article": [{"id": 1, "title": "Article 1", "content": "Sample article content 1",
                            "is_published": False, "author": {"id": 1, "name": "Author 1"}}]}}


def err(code, message):
    return {"errors": [{"extensions": {"code": code, "path": "$"}, "message": message}]}


def set_claims(claims, hasura, path):
    claims = copy.deepcopy(claims)
    if path is None:
        claims["https://hasura.io/jwt/claims"] = hasura
    elif path == "$":
        claims.update(hasura)
    elif path == "$.hasura_claims":
        claims["hasura_claims"] = hasura
    elif path == "$.hasura.claims":
        claims["hasura"] = {"claims": hasura}
    elif path == "$.hasura['claims%']":
        claims["hasura"] = {"claims%": hasura}
    else:
        raise ValueError(path)
    return claims


def base_claims(hours):
    return {"sub": "1234567890", "name": "John Doe", "iat": "{{now}}", "exp": f"{{{{now+{hours * 3600}}}}}"}


def step(url, headers, status, response):
    return {"url": url, "status": status,
            "headers": {"X-Hasura-Role": "user", "X-Hasura-User-Id": "1", **headers},
            "query": {"query": QUERY}, "response": response}


def write_suite(file, cls, key, conf, cases):
    d = os.path.join(ROOT, AREA, cls)
    shutil.rmtree(d, ignore_errors=True)
    os.makedirs(os.path.join(d, "cases"))
    secret = json.dumps({"type": KEY_TYPE[key], "key": f"{{{{public_key:{key}}}}}", **conf})
    suite = {
        "description": f"Hand-ported from hasura/graphql-engine server/tests-py/{file}::{cls}",
        "upstream": {"file": file, "class": cls, "dir": "queries/graphql_query/permissions"},
        "admin_secret": True,
        "env": {"HASURA_GRAPHQL_STRINGIFY_NUMERIC_TYPES": "true", "HASURA_GRAPHQL_JWT_SECRET": secret},
        "isolation": "none",
        "snapshot": SOURCE,
    }
    with open(os.path.join(d, "suite.yaml"), "w") as f:
        dump_yaml(suite, f)
    for name, method, steps in cases:
        case = {"upstream": {"file": file, "class": cls, "method": method, "by_hand": True},
                "admin_secret_header": False, "steps": steps}
        with open(os.path.join(d, "cases", name + ".yaml"), "w") as f:
            dump_yaml(case, f)


# user_select_query_unpublished_articles.yaml
USER = {"x-hasura-user-id": "1", "x-hasura-default-role": "user", "x-hasura-allowed-roles": ["user"]}


def simple_cases(key, tests, hours):
    """Classes whose tests send the Authorization header with standard claims."""
    out = []
    for method, extra, resp in tests:
        for ep in ENDPOINTS:
            claims = set_claims(base_claims(hours), USER, None)
            claims.update(extra)
            hdr = {"Authorization": {"jwt": {"key": key, "claims": claims}, "prefix": "Bearer "}}
            status = 400 if "errors" in resp and ep != "/v1/graphql" else 200
            out.append((f"{method}_{ep.split('/')[1]}", method, [step(ep, hdr, status, resp)]))
    return out


def expiry_skew_main():
    """TestJwtExpirySkew*: a token expired 30 s ago is still accepted with allowed_skew: 60."""
    for key, kname in KEYS.items():
        write_suite("test_jwt.py", f"TestJwtExpirySkewWith{kname}", key, {"allowed_skew": 60},
                    simple_cases(key, [("test_jwt_expiry_leeway", {"exp": "{{now-30}}"}, OK)], 10))


def subscription_expiry_main():
    """AbstractTestSubscriptionJwtExpiry: the server closes a websocket when its token expires."""
    f = "test_jwt.py"
    for key, kname in KEYS.items():
        claims = set_claims({"sub": "1234567890", "name": "John Doe", "iat": "{{now}}", "exp": "{{now+4}}"},
                            USER, None)
        token = {"jwt": {"key": key, "claims": claims}, "prefix": "Bearer "}
        steps = [
            {"ws_connect": {"protocol": "graphql-ws"}},
            {"ws_send": {"message": {"type": "connection_init", "payload": {"headers": {"Authorization": token}}}}},
            {"ws_expect": {"message": {"type": "connection_ack"}}},
            {"ws_expect_close": {"timeout_ms": 10000}},
        ]
        write_suite(f, f"TestSubscriptionJwtExpiryWith{kname}", key, {},
                    [("test_jwt_expiry", "test_jwt_expiry", steps)])


if __name__ == "__main__":
    expiry_skew_main()
    subscription_expiry_main()
