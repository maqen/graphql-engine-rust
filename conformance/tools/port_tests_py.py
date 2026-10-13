#!/usr/bin/env python3
"""Port Hasura's server/tests-py suites into conformance/suites/.

For every upstream test class that only needs Postgres (no webhook, JWT, remote schema or
action servers) and whose test methods only call `check_query_f`, this tool:

1. runs the class's upstream setup files against a real Hasura v2 CE container,
2. snapshots the result as `setup.sql` (pg_dump of the source database), `metadata.json`
   (`export_metadata`) and, for mutation classes, `data.sql` (the per-test data),
3. re-applies that snapshot the same way the Rust runner does, sends every request of every
   test method to Hasura and records the actual responses as the expected ones (upstream's
   own YAML is used to confirm the response: same status and same body, or both errors,
   since tests-py does not compare error messages),
4. replays each case a second time and drops cases whose responses are not deterministic.

Cases that do not match upstream's YAML, setups that fail and classes that cannot be ported
are listed in conformance/upstream-coverage.yaml with the reason.

Usage (needs docker, psql and pg_dump on PATH):

    python3 conformance/tools/port_tests_py.py \
        --upstream /path/to/graphql-engine/server/tests-py \
        --pg-url postgres://postgres:postgres@localhost:55432/postgres

Postgres must be reachable from containers started with `--network host` at the same URL
(e.g. `docker run -d --network host -e POSTGRES_PASSWORD=postgres -e PGPORT=55432
postgis/postgis:15-3.4`).
"""

import argparse
import ast
import json
import os
import re
import shutil
import subprocess
import sys
import time
import urllib.error
import urllib.request
from urllib.parse import urlparse, urlunparse

import yaml

IMAGE = "hasura/graphql-engine:v2.51.0-ce"
ADMIN_SECRET = "conformance-admin-secret"
SRC_DB = "conf_port_src"

# Classes whose tests are custom Python (websocket protocol, subscriptions). Their setup is
# snapshotted like any other class; their cases are written by hand in the suite directory
# (marked `upstream: {by_hand: true}`), recorded with `hge-conformance hasura --record`, and
# kept when this tool regenerates the suite.
HAND_PORTED = {
    "test_subscriptions.py::TestSubscriptionBasic",
    "test_subscriptions.py::TestSubscriptionBasicGraphQLWS",
    "test_subscriptions.py::TestSubscriptionLiveQueries",
    "test_subscriptions.py::TestSubscriptionLiveQueriesForGraphQLWS",
    "test_subscriptions.py::TestStreamingSubscription",
    "test_subscriptions.py::TestStreamingSubscriptionWithTies",
    "test_subscriptions.py::TestSubscriptionUDFWithSessionArg",
    "test_subscriptions.py::TestSubscriptionCohortIsolationMSSQLPostgres",
}
# Fixtures that only open websocket connections, which hand-ported cases do themselves.
WS_FIXTURES = {"ws_conn_init", "ws_conn_init_graphql_ws"}

SUPPORTED_FIXTURES = {
    "per_class_tests_db_state",
    "per_method_tests_db_state",
    "per_class_db_schema_for_mutation_tests",
    "per_method_db_data_for_mutation_tests",
    "postgis",
}

# Area of a suite: by test file, then refined by directory.
FILE_AREAS = {
    "test_graphql_queries.py": "queries",
    "test_graphql_mutations.py": "mutations",
    "test_graphql_introspection.py": "introspection",
    "test_validation.py": "validation",
    "test_v1_queries.py": "v1-query",
    "test_v2_queries.py": "v2-query",
    "test_metadata.py": "metadata",
    "test_inconsistent_meta.py": "metadata",
    "test_compat.py": "metadata",
    "test_roles_inheritance.py": "permissions",
    "test_naming_conventions.py": "naming-conventions",
    "test_schema_duplication.py": "schema",
    "test_graphql_read_only_source.py": "queries",
    "test_v1alpha1_endpoint.py": "queries",
    "test_endpoints.py": "rest-endpoints",
    "test_allowlist_queries.py": "allowlist",
    "test_openapi.py": "rest-endpoints",
    "test_config_api.py": "server",
    "test_apis_disabled.py": "server",
    "test_cors.py": "server",
    "test_compression.py": "server",
    "test_version.py": "server",
    "test_dev_endpoints.py": "server",
    "test_events.py": "events",
    "test_scheduled_triggers.py": "events",
    "test_actions.py": "actions",
    "test_subscriptions.py": "subscriptions",
    "test_schema_stitching.py": "remote-schemas",
    "test_remote_relationships.py": "remote-schemas",
    "test_remote_schema_permissions.py": "remote-schemas",
}
PERMISSION_DIRS = (
    "queries/graphql_query/permissions",
    "queries/graphql_query/agg_perm",
    "queries/graphql_query/root_col_perm",
    "queries/graphql_query/computed_fields_row_permission_bypass",
    "queries/unauthorized_role",
)


def area_of(test_file, class_dir):
    if class_dir and class_dir.startswith(PERMISSION_DIRS):
        return "permissions"
    return FILE_AREAS.get(test_file, test_file[len("test_"):-len(".py")].replace("_", "-"))


# ---------------------------------------------------------------- upstream test discovery


class Skip(Exception):
    pass


def literal(node, cls_dir):
    """Evaluate a path expression such as `self.dir() + "/x.yaml"`."""
    if isinstance(node, ast.Constant) and isinstance(node.value, str):
        return node.value
    if isinstance(node, ast.Call) and ast.unparse(node.func) in ("self.dir", "cls.dir"):
        if cls_dir is None:
            raise Skip("uses dir() without a dir")
        return cls_dir
    if isinstance(node, ast.BinOp) and isinstance(node.op, ast.Add):
        return literal(node.left, cls_dir) + literal(node.right, cls_dir)
    if isinstance(node, ast.JoinedStr):
        out = ""
        for v in node.values:
            if isinstance(v, ast.FormattedValue):
                out += literal(v.value, cls_dir)
            else:
                out += v.value
        return out
    if isinstance(node, ast.Call) and ast.unparse(node.func) == "hge_ctx.backend_suffix":
        return literal(node.args[0], cls_dir)  # Postgres is the default backend: no suffix
    if isinstance(node, ast.Call) and ast.unparse(node.func) == "os.path.join":
        return os.path.join(*[literal(a, cls_dir) for a in node.args])
    if isinstance(node, (ast.List, ast.Tuple)):
        return [literal(e, cls_dir) for e in node.elts]
    raise Skip(f"cannot evaluate {ast.unparse(node)}")


def method_files(fn, cls_dir):
    """The fixture files a test method checks, or Skip when it does anything else."""
    files = []
    add_auth = True

    def visit(stmts, http_branch_ok=True):
        nonlocal add_auth
        for st in stmts:
            if isinstance(st, ast.Expr) and isinstance(st.value, ast.Constant):
                continue  # docstring
            if isinstance(st, ast.Expr) and isinstance(st.value, ast.Call):
                call = st.value
                if ast.unparse(call.func) != "check_query_f":
                    raise Skip(f"calls {ast.unparse(call.func)}")
                files.append(literal(call.args[1], cls_dir))
                for kw in call.keywords:
                    if kw.arg == "add_auth":
                        add_auth = bool(ast.literal_eval(kw.value))
                    elif kw.arg == "transport":
                        pass
                    elif kw.arg == "gqlws":
                        pass
                    else:
                        raise Skip(f"check_query_f({kw.arg}=...)")
                continue
            if isinstance(st, ast.If):
                cond = ast.unparse(st.test).replace('"', "'")
                if cond in ("transport == 'http'",):
                    visit(st.body)
                    if st.orelse:
                        raise Skip("transport-specific else branch")
                    continue
                if cond in ("transport == 'websocket'", "transport != 'http'"):
                    continue
            if isinstance(st, ast.Pass):
                continue
            if ast.unparse(st) in ("hge_ctx.may_skip_test_teardown = True", "transport = 'http'"):
                continue
            raise Skip(f"custom test logic: {ast.unparse(st)[:60]}")

    visit(fn.body)
    if not files:
        raise Skip("no check_query_f call")
    return files, add_auth


def discover(upstream):
    """Yield (test_file, class_name, info-or-skip-reason)."""
    for test_file in sorted(os.listdir(upstream)):
        if not (test_file.startswith("test_") and test_file.endswith(".py")):
            continue
        tree = ast.parse(open(os.path.join(upstream, test_file)).read())
        aliases = {}
        classes = {}
        for node in tree.body:
            if isinstance(node, ast.Assign) and len(node.targets) == 1 and isinstance(node.targets[0], ast.Name):
                aliases[node.targets[0].id] = node.value
            if isinstance(node, ast.ClassDef):
                classes[node.name] = node
        for node in tree.body:
            if isinstance(node, ast.ClassDef) and node.name.startswith("Test"):
                try:
                    info = class_info(node, aliases, classes, f"{test_file}::{node.name}" in HAND_PORTED)
                    yield test_file, node.name, info
                except Skip as e:
                    yield test_file, node.name, str(e)
                except ValueError as e:
                    yield test_file, node.name, f"cannot evaluate class settings: {e}"


def expand_decorator(d, aliases):
    """Resolve module-level aliases such as `usefixtures = pytest.mark.usefixtures`."""
    if isinstance(d, ast.Name) and d.id in aliases:
        return expand_decorator(aliases[d.id], aliases)
    if isinstance(d, ast.Call) and isinstance(d.func, ast.Name) and d.func.id in aliases:
        target = aliases[d.func.id]
        return ast.Call(func=target, args=d.args, keywords=d.keywords)
    return d


def class_info(node, aliases, classes, hand_ported=False):
    fixtures = set()
    env = {}
    admin_secret = False
    backends = None
    decorators = list(node.decorator_list)
    # Single inheritance within the module: inherit decorators, attributes and methods.
    bases = [b.id for b in node.bases if isinstance(b, ast.Name)]
    chain = [node]
    while bases and bases[0] in classes:
        base = classes[bases[0]]
        chain.append(base)
        decorators = list(base.decorator_list) + decorators
        bases = [b.id for b in base.bases if isinstance(b, ast.Name)]
    for d in decorators:
        d = expand_decorator(d, aliases)
        name = ast.unparse(d.func if isinstance(d, ast.Call) else d)
        args = d.args if isinstance(d, ast.Call) else []
        if name.endswith("usefixtures"):
            fixtures.update(ast.literal_eval(a) for a in args)
        elif name.endswith("mark.backend"):
            backends = [ast.literal_eval(a) for a in args]
        elif name.endswith("mark.admin_secret") or name.endswith("mark.requires_an_admin_secret"):
            admin_secret = True
        elif name.endswith("mark.no_admin_secret"):
            pass
        elif name.endswith("mark.hge_env"):
            k, v = (ast.literal_eval(a) for a in args)
            if v is not None:
                env[k] = str(v)
        elif name.endswith("mark.parametrize"):
            pname = ast.literal_eval(args[0])
            if pname != "transport":
                raise Skip(f"parametrized by {pname}")
        elif name.endswith("mark.skipif") or name.endswith("mark.skip"):
            raise Skip("skipped upstream")
        else:
            raise Skip(f"needs {name}")
    if backends is not None and "postgres" not in backends:
        raise Skip(f"backend {','.join(backends)}")
    unsupported = fixtures - SUPPORTED_FIXTURES - (WS_FIXTURES if hand_ported else set())
    if unsupported:
        raise Skip(f"needs fixture {', '.join(sorted(unsupported))}")
    if not fixtures & {"per_class_tests_db_state", "per_method_tests_db_state", "per_class_db_schema_for_mutation_tests"}:
        raise Skip("no database state fixture")

    attrs = {}
    methods = {}
    cls_dir = None
    for cls in reversed(chain):
        for m in cls.body:
            if isinstance(m, ast.Assign) and isinstance(m.targets[0], ast.Name):
                attrs[m.targets[0].id] = m.value
            if isinstance(m, ast.FunctionDef):
                if m.name == "dir":
                    ret = [s for s in m.body if isinstance(s, ast.Return)]
                    cls_dir = ast.literal_eval(ret[0].value)
                elif m.name.startswith("test"):
                    methods[m.name] = m
                elif any("fixture" in ast.unparse(d) for d in m.decorator_list):
                    raise Skip(f"class fixture {m.name}")
    version = "v1"
    if "setup_metadata_api_version" in attrs:
        version = ast.literal_eval(attrs["setup_metadata_api_version"])
    files = {}
    for attr in ("setup_files", "schema_setup_files", "values_setup_files"):
        if attr in attrs:
            v = literal(attrs[attr], cls_dir)
            files[attr] = v if isinstance(v, list) else [v]

    tests = {}
    for name, fn in methods.items():
        try:
            for d in fn.decorator_list:
                dn = ast.unparse(d)
                if "parametrize" in dn and "transport" in dn:
                    continue
                raise Skip(f"decorated with {dn[:50]}")
            if [a.arg for a in fn.args.args] not in (["self", "hge_ctx"], ["self", "hge_ctx", "transport"]):
                raise Skip("uses extra fixtures: " + ", ".join(a.arg for a in fn.args.args[2:]))
            tests[name] = method_files(fn, cls_dir)
        except Skip as e:
            tests[name] = str(e)
    return {
        "dir": cls_dir,
        "fixtures": sorted(fixtures),
        "env": env,
        "admin_secret": admin_secret,
        "version": version,
        "files": files,
        "tests": tests,
    }


# ---------------------------------------------------------------- Postgres / Hasura plumbing


def db_url(base, name):
    u = urlparse(base)
    return urlunparse(u._replace(path="/" + name))


def psql(url, sql):
    r = subprocess.run(
        ["psql", "-X", "-q", "-v", "ON_ERROR_STOP=1", url],
        input=sql, text=True, capture_output=True,
    )
    if r.returncode != 0:
        raise RuntimeError(f"psql failed: {r.stderr.strip()[:2000]}")
    return r.stdout


def recreate_db(base, name):
    psql(db_url(base, "postgres"), f'DROP DATABASE IF EXISTS "{name}" WITH (FORCE);\nCREATE DATABASE "{name}";\n')


TRIGGER_RE = re.compile(r"^CREATE TRIGGER notify_hasura_.*?;\n", re.M | re.S)


def pg_dump(url, data_only=False):
    args = ["pg_dump", "--no-owner", "--no-privileges", "--no-security-labels",
            "-N", "hdb_catalog", "--inserts", "--rows-per-insert=500"]
    if data_only:
        args += ["--data-only", "--disable-triggers"]
    r = subprocess.run(args + [url], text=True, capture_output=True)
    if r.returncode != 0:
        raise RuntimeError(f"pg_dump failed: {r.stderr[:2000]}")
    out = TRIGGER_RE.sub("", r.stdout)
    # pg_dump's per-object banner comments and version header.
    out = re.sub(r"^--\n-- .*\n--\n", "", out, flags=re.M)
    out = re.sub(r"^-- (Dumped from|Dumped by|PostgreSQL database dump).*\n", "", out, flags=re.M)
    out = re.sub(r"\n{3,}", "\n\n", out)
    # psql meta-commands (pg_dump >= 17.6 emits \restrict) are not SQL.
    out = "\n".join(l for l in out.splitlines() if not l.startswith("\\")) + "\n"
    # Settings that differ between Postgres versions are noise for a fixture.
    out = re.sub(r"^SET (transaction_timeout|default_table_access_method) = .*\n", "", out, flags=re.M)
    return out


class Hasura:
    """A Hasura CE container (host network) with a given environment."""

    def __init__(self, pg_url, env, admin_secret, port, image):
        self.port = port
        self.url = f"http://127.0.0.1:{port}"
        self.secret = ADMIN_SECRET if admin_secret else None
        meta = f"conf_port_meta_{port}"
        recreate_db(pg_url, meta)
        self.name = f"conf-port-hge-{port}"
        subprocess.run(["docker", "rm", "-f", self.name], capture_output=True)
        cmd = ["docker", "run", "-d", "--rm", "--name", self.name, "--network", "host",
               "-e", f"HASURA_GRAPHQL_METADATA_DATABASE_URL={db_url(pg_url, meta)}",
               "-e", f"HASURA_GRAPHQL_SERVER_PORT={port}",
               "-e", "HASURA_GRAPHQL_ENABLE_TELEMETRY=false",
               "-e", f"HASURA_GRAPHQL_DATABASE_URL={db_url(pg_url, SRC_DB)}",
               "-e", "HASURA_GRAPHQL_STRINGIFY_NUMERIC_TYPES=true"]
        if self.secret:
            cmd += ["-e", f"HASURA_GRAPHQL_ADMIN_SECRET={self.secret}"]
        for k, v in env.items():
            cmd += ["-e", f"{k}={v}"]
        subprocess.run(cmd + [image], check=True, capture_output=True)
        for _ in range(120):
            try:
                urllib.request.urlopen(self.url + "/healthz", timeout=2)
                return
            except Exception:
                time.sleep(0.5)
        logs = subprocess.run(["docker", "logs", self.name], capture_output=True, text=True)
        raise RuntimeError(f"hasura did not start: {logs.stdout[-2000:]}{logs.stderr[-2000:]}")

    def stop(self):
        subprocess.run(["docker", "rm", "-f", self.name], capture_output=True)

    def request(self, path, body=None, headers=None, method=None, raw_body=None):
        h = {"Content-Type": "application/json"}
        h.update(headers or {})
        data = raw_body.encode() if raw_body is not None else (json.dumps(body).encode() if body is not None else None)
        req = urllib.request.Request(self.url + path, data=data, headers=h, method=method or ("POST" if data is not None else "GET"))
        try:
            with urllib.request.urlopen(req, timeout=60) as r:
                status, text, rh = r.status, r.read().decode(), r.headers
        except urllib.error.HTTPError as e:
            status, text, rh = e.code, e.read().decode(), e.headers
        try:
            parsed = json.loads(text)
        except ValueError:
            parsed = text
        return status, parsed, rh

    def admin(self, path, body):
        h = {"X-Hasura-Admin-Secret": self.secret} if self.secret else {}
        status, resp, _ = self.request(path, body, h)
        if status != 200:
            raise RuntimeError(f"{path} {json.dumps(body)[:300]} -> {status} {json.dumps(resp)[:1500]}")
        return resp


def apply_snapshot(hge, pg_url, setup_sql, metadata):
    """What the Rust runner does to put a suite in place."""
    hge.admin("/v1/metadata", {"type": "replace_metadata", "args": {"version": 3, "sources": []}})
    recreate_db(pg_url, SRC_DB)
    psql(db_url(pg_url, SRC_DB), setup_sql)
    # Sources stay on {"from_env": "HASURA_GRAPHQL_DATABASE_URL"}, set on the container.
    hge.admin("/v1/metadata", {"type": "replace_metadata", "args": metadata})


def reset_data(pg_url, data_sql):
    psql(db_url(pg_url, SRC_DB), TRUNCATE_ALL + data_sql)


TRUNCATE_ALL = """
DO $$ DECLARE r record; BEGIN
  FOR r IN SELECT format('%I.%I', n.nspname, c.relname) AS t FROM pg_class c
           JOIN pg_namespace n ON n.oid = c.relnamespace
           WHERE c.relkind IN ('r', 'p') AND n.nspname NOT IN ('pg_catalog', 'information_schema', 'hdb_catalog')
             AND n.nspname NOT LIKE 'pg_toast%' AND NOT EXISTS (SELECT 1 FROM pg_depend d WHERE d.objid = c.oid AND d.deptype = 'e')
  LOOP EXECUTE 'TRUNCATE ' || r.t || ' RESTART IDENTITY CASCADE'; END LOOP; END $$;
"""


# ---------------------------------------------------------------- porting one class


def load_yaml(path):
    with open(path) as f:
        return yaml.safe_load(f)


def run_upstream_setup(hge, pg_url, up, info):
    """Apply a class's upstream setup files; returns (setup_sql, data_sql, metadata)."""
    hge.admin("/v1/metadata", {"type": "replace_metadata", "args": {"version": 3, "sources": []}})
    recreate_db(pg_url, SRC_DB)
    ext = "CREATE EXTENSION IF NOT EXISTS citext; CREATE EXTENSION IF NOT EXISTS ltree; CREATE EXTENSION IF NOT EXISTS pgcrypto; CREATE SCHEMA hge_tests;"
    if "postgis" in info["fixtures"]:
        ext += " CREATE EXTENSION IF NOT EXISTS postgis;"
    psql(db_url(pg_url, SRC_DB), ext)
    hge.admin("/v1/metadata", {"type": "replace_metadata", "args": {"version": 3, "sources": [{
        "name": "default", "kind": "postgres", "tables": [], "customization": {},
        "configuration": {"connection_info": {"database_url": db_url(pg_url, SRC_DB)}}}]}})

    d = info["dir"]
    files = info["files"]

    def path(f):
        return os.path.join(up, f)

    def run(endpoint, fs):
        for f in fs:
            if os.path.isfile(path(f)):
                hge.admin(endpoint, load_yaml(path(f)))

    mutation = "per_class_db_schema_for_mutation_tests" in info["fixtures"]
    v1 = info["version"] == "v1"
    if mutation:
        if v1:
            run("/v1/query", files.get("schema_setup_files", [f"{d}/schema_setup.yaml"]))
        else:
            run("/v1/metadata", [f"{d}/pre_setup.yaml"])
            run("/v2/query", [f"{d}/schema_setup.yaml"])
            run("/v1/metadata", files.get("schema_setup_files", [f"{d}/setup.yaml"]))
        run("/v1/query", files.get("values_setup_files", [f"{d}/values_setup.yaml"]))
        data_sql = pg_dump(db_url(pg_url, SRC_DB), data_only=True)
        setup_sql = pg_dump(db_url(pg_url, SRC_DB))
    else:
        if v1:
            run("/v1/query", files.get("setup_files", [f"{d}/setup.yaml"]))
        else:
            run("/v1/metadata", [f"{d}/pre_setup.yaml"])
            run("/v2/query", [f"{d}/schema_setup.yaml"])
            run("/v1/metadata", files.get("setup_files", [f"{d}/setup.yaml"]))
        setup_sql = pg_dump(db_url(pg_url, SRC_DB))
        data_sql = None
    metadata = hge.admin("/v1/metadata", {"type": "export_metadata", "args": {}})
    sources = metadata.get("sources", [])
    if len(sources) != 1:
        raise Skip(f"setup leaves {len(sources)} sources")
    sources[0]["configuration"]["connection_info"]["database_url"] = {"from_env": "HASURA_GRAPHQL_DATABASE_URL"}
    return setup_sql, data_sql, metadata


def unordered_equal(a, b):
    return json.dumps(a, sort_keys=True) == json.dumps(b, sort_keys=True)


def is_error(body):
    return isinstance(body, dict) and bool(body.get("errors") or body.get("error"))


def send_step(hge, step, add_secret):
    headers = {str(k): str(v) for k, v in (step.get("headers") or {}).items()}
    if add_secret and hge.secret:
        headers["X-Hasura-Admin-Secret"] = hge.secret
    raw = step.get("body")
    if isinstance(raw, (dict, list)):
        raw = json.dumps(raw)
    return hge.request(step["url"], step.get("query"), headers, step.get("method"), raw)


def record_case(hge, up, files, add_secret):
    """Send a test method's steps to Hasura; returns the recorded steps or raises Skip."""
    steps = []
    for f in files:
        conf = load_yaml(os.path.join(up, f))
        for doc in conf if isinstance(conf, list) else [conf]:
            steps.append(doc)
    out = []
    for i, step in enumerate(steps):
        status, body, rh = send_step(hge, step, add_secret)
        want_status = step.get("status", 200)
        if status != want_status:
            raise Skip(f"step {i}: upstream returned {status}, its fixture expects {want_status}: {json.dumps(body)[:300]}")
        candidates = step.get("allowed_responses")
        candidates = [c.get("response") for c in candidates] if candidates else [step.get("response")]
        ok = any(
            c is None or unordered_equal(c, body) or (is_error(c) and is_error(body))
            for c in candidates
        )
        if not ok:
            raise Skip(f"step {i}: upstream response differs from its fixture: {json.dumps(body)[:300]}")
        rec = {}
        for k in ("description", "url", "method", "status", "headers", "query", "body"):
            if k in step:
                rec[k] = step[k]
        rec["status"] = status
        rec["response"] = body
        if step.get("resp_headers"):
            rec["resp_headers"] = {k: rh.get(k) for k in step["resp_headers"]}
        out.append(rec)
    return out


def slug(s):
    return re.sub(r"[^A-Za-z0-9_.-]+", "_", s)


def port_class(hge, pg_url, up, out_root, test_file, cls, info, coverage):
    area = area_of(test_file, info["dir"])
    suite_dir = os.path.join(out_root, area, slug(cls))
    cov = {"file": test_file, "class": cls, "area": area, "tests": {}}
    coverage.append(cov)
    try:
        setup_sql, data_sql, metadata = run_upstream_setup(hge, pg_url, up, info)
    except (Skip, RuntimeError) as e:
        cov["skipped"] = f"setup failed upstream: {str(e)[:300]}"
        return
    mutation = "per_class_db_schema_for_mutation_tests" in info["fixtures"]
    isolation = "data" if mutation else ("database" if "per_method_tests_db_state" in info["fixtures"] else "none")

    def prepare():
        if isolation == "data":
            reset_data(pg_url, data_sql)
        elif isolation == "database":
            apply_snapshot(hge, pg_url, setup_sql, metadata)

    try:
        apply_snapshot(hge, pg_url, setup_sql, metadata)
    except RuntimeError as e:
        cov["skipped"] = f"snapshot does not apply: {str(e)[:300]}"
        return
    cases = {}
    hand = hand_cases(suite_dir)
    hand_methods = {method for method, _ in hand.values()}
    for name, t in info["tests"].items():
        if isinstance(t, str) and name in hand_methods:
            cov["tests"][name] = "ported by hand"
            continue
        if isinstance(t, str):
            cov["tests"][name] = f"skipped: {t}"
            continue
        files, add_auth = t
        try:
            prepare()
            first = record_case(hge, up, files, add_auth)
            prepare()
            second = record_case(hge, up, files, add_auth)
            def outcome(steps):
                return [(s["status"], json.dumps(s["response"])) for s in steps]
            if outcome(first) != outcome(second):
                raise Skip("response is not deterministic")
        except (Skip, RuntimeError) as e:
            cov["tests"][name] = f"skipped: {str(e)[:400]}"
            continue
        case = {"upstream": {"file": test_file, "class": cls, "method": name, "fixtures": files}}
        if not add_auth:
            case["admin_secret_header"] = False
        case["steps"] = first
        cases[name] = case
        cov["tests"][name] = "ported"
    shutil.rmtree(suite_dir, ignore_errors=True)
    if not cases and not hand and f"{test_file}::{cls}" not in HAND_PORTED:
        return
    os.makedirs(os.path.join(suite_dir, "cases"), exist_ok=True)
    suite = {
        "description": f"Ported from hasura/graphql-engine server/tests-py/{test_file}::{cls}",
        "upstream": {"file": test_file, "class": cls, "dir": info["dir"]},
        "admin_secret": info["admin_secret"],
        "env": {"HASURA_GRAPHQL_STRINGIFY_NUMERIC_TYPES": "true", **info["env"]},
        "isolation": isolation,
    }
    with open(os.path.join(suite_dir, "suite.yaml"), "w") as f:
        dump_yaml(suite, f)
    with open(os.path.join(suite_dir, "setup.sql"), "w") as f:
        f.write(setup_sql)
    if data_sql is not None:
        with open(os.path.join(suite_dir, "data.sql"), "w") as f:
            f.write(data_sql)
    with open(os.path.join(suite_dir, "metadata.json"), "w") as f:
        json.dump(metadata, f, indent=2)
        f.write("\n")
    for name, case in cases.items():
        with open(os.path.join(suite_dir, "cases", f"{name}.yaml"), "w") as f:
            dump_yaml(case, f)
    for file_name, (_, text) in hand.items():
        with open(os.path.join(suite_dir, "cases", file_name), "w") as f:
            f.write(text)


def hand_cases(suite_dir):
    """Hand-written cases of a suite: {file name: (upstream method, text)}."""
    out = {}
    cases_dir = os.path.join(suite_dir, "cases")
    if not os.path.isdir(cases_dir):
        return out
    for file_name in sorted(os.listdir(cases_dir)):
        with open(os.path.join(cases_dir, file_name)) as f:
            text = f.read()
        up = (yaml.safe_load(text) or {}).get("upstream") or {}
        if up.get("by_hand"):
            out[file_name] = (up.get("method", file_name), text)
    return out


def hand_coverage(out_root):
    """(file, class) -> {method: 'ported by hand'} for hand-written suites the tool does not own."""
    out = {}
    for area in sorted(os.listdir(out_root)) if os.path.isdir(out_root) else []:
        for suite in sorted(os.listdir(os.path.join(out_root, area))):
            for _, text in hand_cases(os.path.join(out_root, area, suite)).values():
                up = yaml.safe_load(text)["upstream"]
                out.setdefault((up["file"], up["class"]), {})[up["method"]] = "ported by hand"
    return out


class Dumper(yaml.SafeDumper):
    pass


def _str(dumper, s):
    style = "|" if "\n" in s else None
    return dumper.represent_scalar("tag:yaml.org,2002:str", s, style=style)


Dumper.add_representer(str, _str)


def dump_yaml(obj, f):
    yaml.dump(obj, f, Dumper=Dumper, sort_keys=False, allow_unicode=True, width=100)


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--upstream", required=True, help="path to graphql-engine/server/tests-py")
    ap.add_argument("--pg-url", required=True, help="superuser URL, reachable from host-network containers")
    ap.add_argument("--out", default=os.path.join(os.path.dirname(__file__), "..", "suites"))
    ap.add_argument("--coverage", default=os.path.join(os.path.dirname(__file__), "..", "upstream-coverage.yaml"))
    ap.add_argument("--image", default=IMAGE)
    ap.add_argument("--only", help="regex on <file>::<class>")
    ap.add_argument("--port", type=int, default=58180)
    args = ap.parse_args()

    found = list(discover(args.upstream))
    coverage = []
    groups = {}
    for test_file, cls, info in found:
        if args.only and not re.search(args.only, f"{test_file}::{cls}"):
            continue
        if isinstance(info, str):
            coverage.append({"file": test_file, "class": cls, "skipped": info})
            continue
        key = (info["admin_secret"], tuple(sorted(info["env"].items())))
        groups.setdefault(key, []).append((test_file, cls, info))

    for i, ((secret, env), classes) in enumerate(sorted(groups.items(), key=lambda kv: str(kv[0]))):
        print(f"== group {i}: admin_secret={secret} env={dict(env)} ({len(classes)} classes)", flush=True)
        try:
            hge = Hasura(args.pg_url, dict(env), secret, args.port + i, args.image)
        except RuntimeError as e:
            for test_file, cls, info in classes:
                coverage.append({"file": test_file, "class": cls, "skipped": f"hasura did not start with this env: {str(e)[:200]}"})
            continue
        try:
            for test_file, cls, info in classes:
                t0 = time.time()
                port_class(hge, args.pg_url, args.upstream, args.out, test_file, cls, info, coverage)
                c = coverage[-1]
                n = sum(1 for v in c.get("tests", {}).values() if v == "ported")
                print(f"  {test_file}::{cls}: {n}/{len(c.get('tests', {}))} ported {c.get('skipped', '')} ({time.time() - t0:.1f}s)", flush=True)
        finally:
            hge.stop()

    # Hand-written suites for classes this tool cannot snapshot (no database fixture).
    hand = hand_coverage(args.out)
    for c in coverage:
        tests = hand.get((c["file"], c["class"]))
        if tests and "skipped" in c:
            del c["skipped"]
            c["tests"] = tests
    if args.only and os.path.exists(args.coverage):
        # Keep the other classes' entries.
        with open(args.coverage) as f:
            old = yaml.safe_load(f) or []
        coverage += [c for c in old if not re.search(args.only, f"{c['file']}::{c['class']}")]
    coverage.sort(key=lambda c: (c["file"], c["class"]))
    with open(args.coverage, "w") as f:
        f.write("# Generated by conformance/tools/port_tests_py.py: every upstream tests-py class and\n")
        f.write("# test method, and whether it was ported (or why not).\n")
        dump_yaml(coverage, f)


if __name__ == "__main__":
    sys.exit(main())
