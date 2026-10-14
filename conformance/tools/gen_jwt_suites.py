#!/usr/bin/env python3
"""Generate the `auth` conformance suites from upstream test_jwt.py and test_jwt_claims_map.py.

Upstream signs a fresh token per test with a key made per class. Here every class gets a
fresh key when this tool runs, its public key goes into the suite's HASURA_GRAPHQL_JWT_SECRET,
and its tokens are signed now with a far-future `exp`, so the cases are static. The private
keys are thrown away. Responses are left out; record them from Hasura afterwards:

    python3 conformance/tools/gen_jwt_suites.py
    target/debug/hge-conformance --filter auth/ hasura --record

Tests whose token must expire relative to the time of the request (allowed_skew leeway,
subscription expiry) cannot be static and are not ported.

Needs PyJWT and cryptography (`pip install pyjwt cryptography`) and openssl.
"""

import copy
import json
import os
import shutil
import subprocess
import sys
import tempfile

import jwt
import yaml

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.join(HERE, "..")
SUITES = os.path.join(ROOT, "suites")
OUT = os.path.join(SUITES, "auth")
COVERAGE = os.path.join(ROOT, "upstream-coverage.yaml")
# The upstream classes all run on queries/graphql_query/permissions, already ported here.
BASE = os.path.join(SUITES, "permissions", "TestGraphqlQueryPermissions")
BASE_CASE = os.path.join(BASE, "cases", "test_user_select_unpublished_articles.yaml")

IAT = 1700000000
EXP = 4102444800  # 2100-01-01
EXPIRED = 1700000060
ENDPOINTS = ["/v1/graphql", "/v1alpha1/graphql"]


def keypair(kind, tmp):
    priv, pub = os.path.join(tmp, f"{kind}.key"), os.path.join(tmp, f"{kind}.pub")
    gen = {
        "rsa": [["openssl", "genrsa", "-out", priv, "2048"]],
        "ed25519": [["openssl", "genpkey", "-algorithm", "ed25519", "-out", priv]],
        "es": [["openssl", "ecparam", "-name", "prime256v1", "-genkey", "-noout", "-out", priv]],
    }[kind]
    for cmd in gen + [["openssl", "pkey", "-pubout", "-in", priv, "-out", pub]]:
        subprocess.run(cmd, check=True, capture_output=True)
    with open(priv) as f, open(pub) as g:
        return f.read(), g.read()


TYPES = {"rsa": ("RS512", "RS512"), "ed25519": ("Ed25519", "EdDSA"), "es": ("ES256", "ES256")}


class Cls:
    def __init__(self, file, name, kind, conf, tmp):
        self.file, self.name, self.kind, self.conf = file, name, kind, conf
        self.private, public = keypair(kind, tmp)
        ty, self.alg = TYPES[kind]
        self.server = {"type": ty, "key": public, **conf}

    def sign(self, claims, key=None, alg=None):
        return jwt.encode(claims, key or self.private, algorithm=alg or self.alg)

    def auth_header(self, token):
        h = self.conf.get("header", {"type": "Authorization"})
        if h["type"] == "Authorization":
            return {"Authorization": "Bearer " + token}
        if h["type"] == "Cookie":
            return {"Cookie": f"{h['name']}={token}"}
        return {h["name"]: token}


def base_claims():
    return {"sub": "1234567890", "name": "John Doe", "iat": IAT, "exp": EXP}


def set_claims(cls, claims, hasura, use_namespace=True):
    if cls.conf.get("claims_format") == "stringified_json":
        hasura = json.dumps(hasura)
    path = cls.conf.get("claims_namespace_path") if use_namespace else None
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


def case(cls, method, headers_for, extra_headers=None, endpoints=ENDPOINTS):
    """One case: the base query once per endpoint, with these auth headers."""
    with open(BASE_CASE) as f:
        base = yaml.safe_load(f)["steps"][0]
    steps = []
    for ep in endpoints:
        s = copy.deepcopy(base)
        s.pop("response", None)
        s["url"] = ep
        s["headers"].update(headers_for)
        s["headers"].update(extra_headers or {})
        steps.append(s)
    return method, {
        "upstream": {"by_hand": True, "file": cls.file, "class": cls.name, "method": method},
        "admin_secret_header": False,
        "steps": steps,
    }


def basic_cases(cls):
    def tok(hasura, **over):
        c = set_claims(cls, base_claims(), hasura)
        c.update(over)
        return cls.auth_header(cls.sign(c))

    ok = {"x-hasura-user-id": "1", "x-hasura-default-role": "user", "x-hasura-allowed-roles": ["user"]}
    wrong_key = jwt.encode(
        set_claims(cls, base_claims(), ok),
        "not-the-key-" * 4,
        algorithm="HS384" if cls.alg == "HS256" else "HS256",
    )
    return [
        case(cls, "test_jwt_valid_claims_success", tok({"x-hasura-user-id": "1", "x-hasura-allowed-roles": ["user", "editor"], "x-hasura-default-role": "user"})),
        case(cls, "test_jwt_invalid_role_in_request_header", tok({"x-hasura-user-id": "1", "x-hasura-allowed-roles": ["contractor", "editor"], "x-hasura-default-role": "contractor"})),
        case(cls, "test_jwt_no_allowed_roles_in_claim", tok({"x-hasura-user-id": "1", "x-hasura-default-role": "user"})),
        case(cls, "test_jwt_invalid_allowed_roles_in_claim", tok({"x-hasura-user-id": "1", "x-hasura-allowed-roles": "user", "x-hasura-default-role": "user"})),
        case(cls, "test_jwt_no_default_role", tok({"x-hasura-user-id": "1", "x-hasura-allowed-roles": ["user"]})),
        case(cls, "test_jwt_expired", tok(ok, exp=EXPIRED)),
        case(cls, "test_jwt_invalid_signature", cls.auth_header(wrong_key)),
        case(cls, "test_jwt_no_audience_in_conf", tok(ok, aud="hasura-test-suite")),
        case(cls, "test_jwt_no_issuer_in_conf", tok(ok, iss="rubbish-issuer")),
    ]


def plain(cls, hasura, **over):
    c = set_claims(cls, base_claims(), hasura, use_namespace=False)
    c.update(over)
    return {"Authorization": "Bearer " + cls.sign(c)}


OK = {"x-hasura-user-id": "1", "x-hasura-default-role": "user", "x-hasura-allowed-roles": ["user"]}


def audience_cases(cls):
    aud = cls.conf["audience"]
    aud = aud if isinstance(aud, str) else aud[0]
    return [
        case(cls, "test_jwt_valid_audience", plain(cls, OK, aud=aud)),
        case(cls, "test_jwt_invalid_audience", plain(cls, OK, aud="rubbish_audience")),
    ]


def issuer_cases(cls):
    return [
        case(cls, "test_jwt_valid_issuer", plain(cls, OK, iss=cls.conf["issuer"])),
        case(cls, "test_jwt_invalid_issuer", plain(cls, OK, iss="rubbish_issuer")),
    ]


MYAPP = "https://myapp.com/jwt/claims"


def clean(d):
    out = {}
    for k, v in d.items():
        if isinstance(v, dict):
            v = clean(v)
            if v:
                out[k] = v
        elif v is not None:
            out[k] = v
    return out


def claims_map_cases(cls):
    def tok(user_id=None, allowed=None, default=None):
        c = base_claims()
        c[MYAPP] = clean({"user": {"id": user_id}, "role": {"allowed": allowed, "default": default}})
        return {"Authorization": "Bearer " + cls.sign(c)}

    return [
        case(cls, "test_jwt_claims_map_valid_claims_success", tok("1", ["user", "editor"], "user")),
        case(cls, "test_jwt_claims_map_invalid_role_in_request_header", tok("1", ["contractor", "editor"], "contractor")),
        case(cls, "test_jwt_claims_map_no_allowed_roles_in_claim", tok("1", None, "user")),
        case(cls, "test_jwt_claims_map_invalid_allowed_roles_in_claim", tok("1", "user", "user")),
        case(cls, "test_jwt_claims_map_no_default_role", tok("1", ["user"])),
        case(cls, "test_jwt_claims_map_claim_not_found", tok(None, ["user", "editor"], "user")),
    ]


def static_claims_map_cases(cls):
    def tok(user_id=None):
        c = base_claims()
        c[MYAPP] = clean({"user": {"id": user_id}})
        return {"Authorization": "Bearer " + cls.sign(c)}

    return [
        case(cls, "test_jwt_claims_map_valid_claims_success", tok("1"), {"x-hasura-custom-header": "custom-value"}),
        case(cls, "test_jwt_claims_map_invalid_role_in_request_header", tok("1"), {"X-Hasura-Role": "random_string"}),
        case(cls, "test_jwt_claims_map_claim_not_found", tok(None)),
    ]


BASIC_CONFS = [
    ("", {}),
    ("AndCookie", {"header": {"type": "Cookie", "name": "hasura_user"}}),
    ("AndCustomHeader", {"header": {"type": "CustomHeader", "name": "hasura_user"}}),
    ("AndStringifiedJsonClaims", {"claims_format": "stringified_json"}),
    ("AndClaimsNamespacePathAtRoot", {"claims_namespace_path": "$"}),
    ("AndClaimsNamespacePathAtOneLevelOfNesting", {"claims_namespace_path": "$.hasura_claims"}),
    ("AndClaimsNamespacePathAtTwoLevelsOfNesting", {"claims_namespace_path": "$.hasura.claims"}),
    ("AndClaimsNamespacePathWithSpecialCharacters", {"claims_namespace_path": "$.hasura['claims%']"}),
]
KINDS = [("Rsa", "rsa"), ("Ed25519", "ed25519"), ("Es", "es")]

BASIC_CLAIMS_MAP = {
    "x-hasura-user-id": {"path": f"$.['{MYAPP}'].user.id"},
    "x-hasura-allowed-roles": {"path": f"$.['{MYAPP}'].role.allowed"},
    "x-hasura-default-role": {"path": f"$.['{MYAPP}'].role.default"},
}
DEFAULTS_CLAIMS_MAP = {
    "x-hasura-user-id": {"path": f"$.['{MYAPP}'].user.id", "default": "1"},
    "x-hasura-allowed-roles": {"path": f"$.['{MYAPP}'].role.allowed", "default": ["user", "editor"]},
    "x-hasura-default-role": {"path": f"$.['{MYAPP}'].role.default", "default": "user"},
}
STATIC_CLAIMS_MAP = {
    "x-hasura-user-id": {"path": f"$.['{MYAPP}'].user.id"},
    "x-hasura-allowed-roles": ["user", "editor"],
    "x-hasura-default-role": "user",
    "x-hasura-custom-header": "custom-value",
}


def classes(tmp):
    for suffix, conf in BASIC_CONFS:
        for kname, kind in KINDS:
            # Upstream has no Ed25519 / ES class with a custom header.
            if suffix == "AndCustomHeader" and kind != "rsa":
                continue
            yield Cls("test_jwt.py", f"TestJwtBasicWith{kname}{suffix}", kind, conf, tmp), basic_cases
    for kname, kind in KINDS:
        for suffix, aud in [("SingleAudience", "myapp-1234"), ("ListOfAudiences", ["myapp-1234", "myapp-9876"])]:
            yield Cls("test_jwt.py", f"TestJwtAudienceCheckWith{kname}And{suffix}", kind, {"audience": aud}, tmp), audience_cases
        yield Cls("test_jwt.py", f"TestJwtIssuerCheckWith{kname}", kind, {"issuer": "https://hasura.com"}, tmp), issuer_cases
    for kname, kind in [("RSA", "rsa"), ("Ed25519", "ed25519")]:
        yield Cls("test_jwt_claims_map.py", f"TestJWTClaimsMapBasicWith{kname}", kind, {"claims_map": BASIC_CLAIMS_MAP}, tmp), claims_map_cases
        yield Cls("test_jwt_claims_map.py", f"TestJWTClaimsMapBasicWith{kname}AndDefaultValues", kind, {"claims_map": DEFAULTS_CLAIMS_MAP}, tmp), claims_map_cases
        yield Cls("test_jwt_claims_map.py", f"TestJWTClaimsMapWithStaticHasuraClaimsMapValuesWith{kname}", kind, {"claims_map": STATIC_CLAIMS_MAP}, tmp), static_claims_map_cases


NOT_STATIC = "token expiry is relative to the request time, so the token cannot be static"


class Dumper(yaml.SafeDumper):
    pass


Dumper.add_representer(
    str,
    lambda d, s: d.represent_scalar("tag:yaml.org,2002:str", s, style="|" if "\n" in s else None),
)


def dump(obj, path):
    with open(path, "w") as f:
        yaml.dump(obj, f, Dumper=Dumper, sort_keys=False, allow_unicode=True, width=100)


def main():
    with open(os.path.join(BASE, "suite.yaml")) as f:
        base_suite = yaml.safe_load(f)
    shutil.rmtree(OUT, ignore_errors=True)
    coverage = {}
    with tempfile.TemporaryDirectory() as tmp:
        for cls, make in classes(tmp):
            d = os.path.join(OUT, cls.name)
            os.makedirs(os.path.join(d, "cases"))
            # Symlinks, so the 37 suites share one copy of the base suite's database.
            for f in ("setup.sql", "metadata.json"):
                os.symlink(os.path.relpath(os.path.join(BASE, f), d), os.path.join(d, f))
            dump(
                {
                    "description": f"Ported by hand from hasura/graphql-engine server/tests-py/{cls.file}::{cls.name} "
                    "by conformance/tools/gen_jwt_suites.py",
                    "admin_secret": True,
                    "env": {
                        **base_suite.get("env", {}),
                        "HASURA_GRAPHQL_JWT_SECRET": json.dumps(cls.server),
                    },
                    "isolation": "none",
                },
                os.path.join(d, "suite.yaml"),
            )
            tests = {}
            for method, body in make(cls):
                dump(body, os.path.join(d, "cases", method + ".yaml"))
                tests[method] = "ported by hand"
            coverage[(cls.file, cls.name)] = tests
    for kname in ["Rsa", "Ed25519", "Es"]:
        coverage[("test_jwt.py", f"TestJwtExpirySkewWith{kname}")] = {"test_jwt_expiry_leeway": f"skipped: {NOT_STATIC}"}
        coverage[("test_jwt.py", f"TestSubscriptionJwtExpiryWith{kname}")] = {"test_jwt_expiry": f"skipped: {NOT_STATIC}"}

    with open(COVERAGE) as f:
        header = "".join(line for line in f if line.startswith("#"))
    with open(COVERAGE) as f:
        entries = yaml.safe_load(f)
    for e in entries:
        tests = coverage.pop((e["file"], e["class"]), None)
        if tests is not None:
            e.pop("skipped", None)
            e["area"] = "auth"
            e["tests"] = tests
    if coverage:
        sys.exit(f"classes missing from upstream-coverage.yaml: {sorted(coverage)}")
    with open(COVERAGE, "w") as f:
        f.write(header)
        yaml.dump(entries, f, Dumper=Dumper, sort_keys=False, allow_unicode=True, width=100)


if __name__ == "__main__":
    main()
