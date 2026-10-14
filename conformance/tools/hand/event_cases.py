#!/usr/bin/env python3
"""Write the hand-ported event trigger cases (upstream test_events.py).

Upstream's tests insert rows and then read the webhook's deliveries in Python (`check_event`),
so the port tool only snapshots these classes' setups (they are in its HAND_PORTED list) and
this script writes their cases. Upstream's webhook runs as the `events_webhook` helper, whose
`GET /__next_event` returns the next delivery as `{path, body, headers}` (404 when none comes
in time). A `check_event` becomes such a step: upstream's assertions are its expected value,
with `{{any}}` for what changes between runs (event id, timestamps, trace context), and
recording fills in the rest of the payload from Hasura. Every case starts by emptying the
webhook's queue, except test_queue_empty, which checks that nothing is left over.

    python3 conformance/tools/port_tests_py.py ... --only test_events.py   # setups
    python3 conformance/tools/hand/event_cases.py --upstream <graphql-engine>/server/tests-py
    target/debug/hge-conformance --filter events/ hasura --record
    python3 conformance/tools/hand/event_cases.py --upstream ... --check

`--check` confirms that the recorded deliveries and error statuses still say what upstream
asserts (recording overwrites a step that differs). Re-running without it overwrites the
recorded cases. Deliveries a case does not read (TestManualEvents invokes triggers it does
not wait for) are dropped by the next case's `/__reset`.
"""

import argparse
import json
import os
import shutil
import sys

import yaml

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.join(HERE, ".."))
from port_tests_py import dump_yaml  # noqa: E402

ROOT = os.path.join(HERE, "..", "..", "suites")
AREA = "events"
FILE = "test_events.py"
EV = "{{helper:events_webhook}}"
ANY = "{{any}}"
ADMIN = {"x-hasura-role": "admin"}
T1 = {"schema": "hge_tests", "name": "test_t1"}
UPSTREAM = None
# Upstream's assertions for every check_event step written: (case path, step index) -> expected.
CHECKS = {}


def load(path):
    with open(os.path.join(UPSTREAM, path)) as f:
        return yaml.safe_load(f)


def http(url, body, status=200, headers=None, description=None):
    step = {}
    if description:
        step["description"] = description
    step["url"] = url
    step["status"] = status
    if headers:
        step["headers"] = headers
    step["query"] = body
    step["response"] = None  # recorded from Hasura
    return step


def v1q(body, status=200, headers=None):
    return http("/v1/query", body, status, headers)


def insert(table, rows, headers=None):
    rows = rows if isinstance(rows, list) else [rows]
    return v1q({"type": "insert", "args": {"table": table, "objects": rows, "returning": []}}, headers=headers)


def update(table, where, set_, headers=None):
    return v1q({"type": "update", "args": {"table": table, "where": where, "$set": set_}}, headers=headers)


def delete(table, where, headers=None):
    return v1q({"type": "delete", "args": {"table": table, "where": where}}, headers=headers)


def run_sql(sql, status=200):
    return v1q({"type": "run_sql", "args": {"sql": sql}}, status)


def fixture(path, url="/v1/query", status=200):
    """A request read from one of upstream's YAML files (`hge_ctx.v1q_f` and the like)."""
    return http(url, load(path), status)


def check_query(path):
    """The steps of a `check_query_f` file; responses are recorded."""
    conf = load(path)
    out = []
    for doc in conf if isinstance(conf, list) else [conf]:
        step = http(doc["url"], doc["query"], doc.get("status", 200), doc.get("headers"), doc.get("description"))
        out.append(step)
    return out


def next_event(timeout, headers=()):
    url = f"{EV}/__next_event?timeout={timeout}"
    if headers:
        url += "&headers=" + ",".join(headers)
    return {"url": url, "status": 200, "response": None}


def check_event(trig, table, op, data, headers=None, path="/", session=ADMIN, retry=0, timeout=3):
    """upstream validate.check_event: the next delivery, with what upstream asserts about it."""
    headers = headers or {}
    step = next_event(timeout, list(headers))
    step["description"] = f"{op} event of {trig}" + (f" (retry {retry})" if retry else "")
    step["response"] = {
        "path": path,
        "headers": headers,
        "body": {
            "id": ANY,
            "created_at": ANY,
            "trigger": {"name": trig},
            "table": table,
            "event": {"op": op, "data": data, "session_variables": session, "trace_context": ANY},
            "delivery_info": {"current_retry": retry},
        },
    }
    step["_check"] = True
    return step


def check_event_transformed(body, headers, removed, path, timeout=3):
    step = next_event(timeout, list(headers) + list(removed))
    step["description"] = "transformed delivery"
    step["response"] = {"path": path, "headers": {**headers, **{h: None for h in removed}}, "body": body}
    step["_check"] = True
    return step


def no_event(timeout):
    """`with pytest.raises(queue.Empty): check_event(..., get_timeout=timeout)`."""
    step = next_event(timeout)
    step["description"] = "no event"
    step["status"] = 404
    step["response"] = {"error": "no event"}
    return step


def reset():
    return {"description": "drop deliveries left by earlier cases", "url": f"{EV}/__reset",
            "status": 200, "response": {"dropped": ANY}}


def suite_dir(cls):
    d = os.path.join(ROOT, AREA, cls)
    if not os.path.isfile(os.path.join(d, "suite.yaml")):
        sys.exit(f"{d}: no suite; run port_tests_py.py --only {FILE} first")
    return d


def write_cases(cls, tests, d=None):
    """`tests`: {method: steps}. Replaces the suite's hand-written cases."""
    d = d or suite_dir(cls)
    cases = os.path.join(d, "cases")
    os.makedirs(cases, exist_ok=True)
    for name in os.listdir(cases) if WRITE else []:
        with open(os.path.join(cases, name)) as f:
            if (yaml.safe_load(f).get("upstream") or {}).get("by_hand"):
                os.remove(os.path.join(cases, name))
    for method, steps in tests.items():
        steps = steps if method == "test_queue_empty" else [reset()] + steps
        path = os.path.join(cases, f"{method}.yaml")
        out = []
        for i, step in enumerate(steps):
            key = (os.path.relpath(path, ROOT), i)
            if step.pop("_check", False):
                CHECKS[key] = step["response"]
            elif step["status"] != 200:
                # Upstream expects this status (an error, or no delivery).
                CHECKS[key] = {"_status": step["status"]}
            out.append(step)
        case = {"upstream": {"file": FILE, "class": cls, "method": method, "by_hand": True}, "steps": out}
        if not WRITE:
            continue
        with open(path, "w") as f:
            dump_yaml(case, f)


def crud(trig, path="/"):
    """Insert, update and delete a test_t1 row, checking each event (TestWebhookEvent and co)."""
    init = {"c1": 1, "c2": "hello"}
    world = {"c1": 1, "c2": "world"}
    return [
        insert(T1, init),
        check_event(trig, T1, "INSERT", {"old": None, "new": init}, path=path),
        update(T1, {"c1": 1}, {"c2": "world"}),
        check_event(trig, T1, "UPDATE", {"old": init, "new": world}, path=path),
        delete(T1, {"c1": 1}),
        check_event(trig, T1, "DELETE", {"old": world, "new": None}, path=path),
    ]


def basic():
    write_cases("TestCreateEventQueryPostgresMSSQL", {"test_basic": crud("t1_all")})
    m = {"schema": "hge_tests", "name": "measurement"}
    row = {"city_id": 1, "logdate": "2006-02-02", "peaktemp": 1, "unitsales": 1}
    write_cases("TestCreateEventQueryPostgres", {"test_partitioned_table_basic_insert": [
        fixture("queries/event_triggers/basic/partition_table_setup.yaml"),
        insert(m, row),
        check_event("measurement_all", m, "INSERT", {"old": None, "new": row}),
        fixture("queries/event_triggers/basic/partition_table_teardown.yaml"),
    ]})
    write_cases("TestWebhookEvent", {"test_basic": crud("t1_all")})
    write_cases("TestEventWebhookTemplateURL", {"test_basic": crud("t1_all", "/trigger")})
    init = {"c1": 1, "c2": "hello"}
    write_cases("TestEventHeadersPostgresMSSQL", {"test_basic": [
        insert(T1, init),
        check_event("t1_all", T1, "INSERT", {"old": None, "new": init},
                    headers={"X-Header-From-Value": "MyValue", "X-Header-From-Env": "MyEnvValue"}),
    ]})
    s1 = {"x-hasura-role": "admin", "x-hasura-allowed-roles": "['admin','user']", "x-hasura-user-id": "1"}
    s2 = {"x-hasura-role": "admin", "x-hasura-random": "some_random_info"}
    world = {"c1": 1, "c2": "world"}
    write_cases("TestEventSessionVariables", {"test_basic": [
        insert(T1, init, headers=s1),
        check_event("t1_all", T1, "INSERT", {"old": None, "new": init}, session=s1),
        update(T1, {"c1": 1}, {"c2": "world"}, headers={**s2, "X-Random-Header": "not_session_variable"}),
        check_event("t1_all", T1, "UPDATE", {"old": init, "new": world}, session=s2),
        delete(T1, {"c1": 1}),
        check_event("t1_all", T1, "DELETE", {"old": world, "new": None}),
    ]})


def data_format():
    big = {"schema": "hge_tests", "name": "test_bigint"}
    geo = {"schema": "hge_tests", "name": "test_geojson"}

    def point(i):
        return {"id": i, "location": {"coordinates": [-43.77, 45.64], "crs": {
            "type": "name", "properties": {"name": "urn:ogc:def:crs:EPSG::4326"}}, "type": "Point"}}
    write_cases("TestEventDataFormat", {
        "test_bigint": [
            insert(big, {"id": 50755254975729665, "name": "hello"}),
            check_event("bigint_all", big, "INSERT",
                        {"old": None, "new": {"id": "50755254975729665", "name": "hello"}}),
        ],
        "test_geojson": [
            update(geo, {"id": 1}, {"id": 2}),
            check_event("geojson_all", geo, "UPDATE", {"old": point(1), "new": point(2)}),
        ],
    })


def retry_conf():
    init = {"c1": 1, "c2": "hello"}
    data = {"old": None, "new": init}

    def table(n):
        return {"schema": "hge_tests", "name": f"test_t{n}"}
    write_cases("TestEventRetryConfPostgresMSSQL", {
        "test_basic": [insert(table(1), init)] + [
            check_event("t1_retry", table(1), "INSERT", data, path="/fail", retry=r) for r in range(5)]
        + [no_event(3)],
        "test_timeout_short": [insert(table(2), init)] + [
            check_event("t2_timeout_short", table(2), "INSERT", data, path="/sleep_2s", retry=r, timeout=5)
            for r in range(3)] + [no_event(3)],
        # Upstream sleeps 2 s and then waits up to 3 s.
        "test_timeout_long": [
            insert(table(3), init),
            check_event("t3_timeout_long", table(3), "INSERT", data, path="/sleep_2s", timeout=5),
            no_event(3),
        ],
        # Upstream runs this last, to check that no retries are left over. Cases here run in
        # alphabetical order and each one starts on a fresh database, so the cases above end
        # with that check themselves.
        "test_queue_empty": [no_event(3)],
    })


def triggers_by_operation():
    init = {"c1": 1, "c2": "hello"}
    world = {"c1": 1, "c2": "world"}
    write_cases("TestDeleteEventQuery", {"test_delete_basic": [
        insert(T1, init), no_event(0),
        update(T1, {"c1": 1}, {"c2": "world"}), no_event(0),
        # Upstream waits a little here, to catch stray events from above.
        delete(T1, {"c1": 1}), no_event(2),
    ]})
    write_cases("TestEventInsertOnly", {"test_insert_only": [
        insert(T1, init),
        check_event("t1_insert", T1, "INSERT", {"old": None, "new": init}),
        update(T1, {"c1": 1}, {"c2": "world"}), no_event(0),
        delete(T1, {"c1": 1}), no_event(2),
    ]})
    w2 = {"c1": 2, "c2": "world"}
    write_cases("TestEventSelCols", {
        "test_selected_cols": [
            insert(T1, init),
            check_event("t1_cols", T1, "INSERT", {"old": None, "new": init}),
            update(T1, {"c1": 1}, {"c2": "world"}), no_event(0),
            update(T1, {"c1": 1}, {"c1": 2}),
            check_event("t1_cols", T1, "UPDATE", {"old": world, "new": w2}),
            delete(T1, {"c1": 2}),
            check_event("t1_cols", T1, "DELETE", {"old": w2, "new": None}),
        ],
        "test_selected_cols_dep": [
            run_sql("alter table hge_tests.test_t1 drop column c1", 400),
            run_sql("alter table hge_tests.test_t1 drop column c2"),
        ],
    })
    write_cases("TestEventSelPayload", {
        "test_selected_payload": [
            insert(T1, init),
            check_event("t1_payload", T1, "INSERT", {"old": None, "new": init}),
            update(T1, {"c1": 1}, {"c2": "world"}),
            check_event("t1_payload", T1, "UPDATE", {"old": {"c1": 1}, "new": {"c1": 1}}),
            update(T1, {"c1": 1}, {"c1": 2}),
            check_event("t1_payload", T1, "UPDATE", {"old": {"c1": 1}, "new": {"c1": 2}}),
            delete(T1, {"c1": 2}),
            check_event("t1_payload", T1, "DELETE", {"old": {"c2": "world"}, "new": None}),
        ],
        "test_selected_payload_dep": [
            run_sql("alter table hge_tests.test_t1 drop column c1", 400),
            run_sql("alter table hge_tests.test_t1 drop column c2", 400),
        ],
    })


def update_query():
    """TestUpdateEventQuery has no database fixture: its class fixture creates the table and the
    trigger, then replaces the trigger. Every case runs that fixture first."""
    cls = "TestUpdateEventQuery"
    d = os.path.join(ROOT, AREA, cls)
    if WRITE:
        shutil.rmtree(d, ignore_errors=True)
        os.makedirs(d)
        with open(os.path.join(d, "suite.yaml"), "w") as f:
            dump_yaml({
                "description": f"Hand-ported from hasura/graphql-engine server/tests-py/{FILE}::{cls}",
                "upstream": {"file": FILE, "class": cls, "dir": "queries/event_triggers/update_query"},
                "admin_secret": False,
                "env": {"HASURA_GRAPHQL_STRINGIFY_NUMERIC_TYPES": "true", "EVENT_WEBHOOK_HANDLER": EV},
                "helpers": ["events_webhook"],
                "isolation": "database",
            }, f)
        with open(os.path.join(d, "setup.sql"), "w") as f:
            f.write("CREATE SCHEMA hge_tests;\n")
        with open(os.path.join(d, "metadata.json"), "w") as f:
            json.dump({"version": 3, "sources": [{
                "name": "default", "kind": "postgres", "tables": [],
                "configuration": {"connection_info": {
                    "database_url": {"from_env": "HASURA_GRAPHQL_DATABASE_URL"},
                    "isolation_level": "read-committed", "use_prepared_statements": False}},
            }]}, f, indent=2)
            f.write("\n")
    t = "queries/event_triggers/update_query"
    init = {"c1": 1, "c2": "hello", "c3": {"name": "clarke"}}
    r1 = {"c1": 1, "c2": "world", "c3": {"name": "clarke"}}
    r2 = {"c1": 1, "c2": "world", "c3": {"name": "bellamy"}}
    r3 = {"c1": 2, "c2": "world", "c3": {"name": "bellamy"}}
    write_cases(cls, {"test_update_basic": [
        fixture(f"{t}/create-setup.yaml"),
        fixture(f"{t}/update-setup.yaml"),
        insert(T1, init), no_event(0),
        update(T1, {"c1": 1}, {"c2": "world"}), no_event(0),
        update(T1, {"c1": 1}, {"c3": {"name": "bellamy"}}),
        check_event("t1_cols", T1, "UPDATE", {"old": r1, "new": r2}, path="/new"),
        update(T1, {"c1": 1}, {"c1": 2}),
        check_event("t1_cols", T1, "UPDATE", {"old": r2, "new": r3}, path="/new"),
        delete(T1, {"c1": 2}),
        check_event("t1_cols", T1, "DELETE", {"old": r3, "new": None}, path="/new"),
        fixture(f"{t}/teardown.yaml"),
    ]}, d)


def others():
    m = "queries/event_triggers/manual_events"
    enabled = fixture(f"{m}/enabled.yaml", "/v1/metadata")
    enabled["response"] = {"event_id": ANY}
    manual = [enabled, fixture(f"{m}/disabled.yaml", "/v1/metadata", 400)]
    # Upstream's reload names a source "mssql", which a Postgres-only run does not have
    # (Hasura reloads all the same).
    reload = http("/v1/metadata", {"type": "reload_metadata", "args": {"source": "mssql", "reload_sources": True}})
    write_cases("TestManualEvents", {
        "test_basic": manual,
        "test_basic_with_reload_metadata": [s for _ in range(5) for s in manual + [reload] + manual],
    })
    row = {"id": 0, "first_name": "Simon", "last_name": "Marlow"}
    write_cases("TestEventTransform", {"test_basic": check_query("queries/event_triggers/transform/basic_transform.yaml") + [
        insert({"schema": "hge_tests", "name": "test_t1"}, row),
        check_event_transformed(row, {"foo": "bar"}, ["user-agent"], "/?foo=bar"),
    ]})


def subset_diff(want, got, path="$"):
    """Where `got` does not have what upstream asserts (`want`), or None."""
    if want == ANY:
        return None
    if isinstance(want, dict):
        if not isinstance(got, dict):
            return f"{path}: not an object"
        for k, v in want.items():
            if k not in got:
                return f"{path}.{k}: missing"
            d = subset_diff(v, got[k], f"{path}.{k}")
            if d:
                return d
        return None
    return None if want == got else f"{path}: {json.dumps(want)} != {json.dumps(got)}"


class NoDates(yaml.SafeLoader):
    """Recorded cases are written by the runner, which leaves dates as plain strings."""


NoDates.yaml_implicit_resolvers = {
    k: [(tag, r) for tag, r in v if tag != "tag:yaml.org,2002:timestamp"]
    for k, v in yaml.SafeLoader.yaml_implicit_resolvers.items()
}


def check():
    bad = 0
    for (path, i), want in sorted(CHECKS.items()):
        with open(os.path.join(ROOT, path)) as f:
            step = yaml.load(f, NoDates)["steps"][i]
        if "_status" in want:
            d = None if step["status"] == want["_status"] else f"status {step['status']} != {want['_status']}"
        else:
            d = subset_diff(want, step["response"]) or (None if step["status"] == 200 else f"status {step['status']}")
        if d:
            bad += 1
            print(f"{path} step {i}: {d}")
    print(f"{len(CHECKS) - bad}/{len(CHECKS)} checked steps say what upstream asserts")
    return bad == 0


WRITE = True


def main():
    global UPSTREAM, WRITE
    p = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    p.add_argument("--upstream", required=True, help="graphql-engine's server/tests-py")
    p.add_argument("--check", action="store_true", help="check recorded cases instead of writing them")
    args = p.parse_args()
    UPSTREAM = args.upstream
    WRITE = not args.check
    basic()
    data_format()
    retry_conf()
    triggers_by_operation()
    update_query()
    others()
    if args.check:
        sys.exit(0 if check() else 1)


if __name__ == "__main__":
    main()
