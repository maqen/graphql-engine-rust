"""Record `../event_transforms.json`: request and response transforms on event triggers, cron
triggers and one-off events, as V2 delivers them.

usage: transforms_steps.py <hasura-url> <out.json>

Start V2 on a fresh database with a short fetch interval, and `hook.py 9911` (the webhook):

    docker run -d --name hge2 --network host \\
      -e HASURA_GRAPHQL_DATABASE_URL=postgres://postgres:postgres@localhost:5432/tx_record \\
      -e HASURA_GRAPHQL_EVENTS_FETCH_INTERVAL=200 \\
      -e HASURA_GRAPHQL_SERVER_PORT=8099 hasura/graphql-engine:v2.51.0-ce

The file holds the requests (`__HOOK__` is the webhook base URL), the observation queries
and what they returned once every event was processed, normalised by `normalise` (which the
test mirrors).
"""

import json
import re
import sys
import time
import urllib.error
import urllib.request

HOOK = "__HOOK__"

FULL_REQUEST = {
    "version": 2,
    "template_engine": "Kriti",
    "method": "PUT",
    "url": "{{$base_url}}/x/{{$body.event.data.new.id}}",
    "query_params": {"a": "{{$body.table.name}}"},
    "request_headers": {
        "add_headers": {"x-added": "v", "x-sess": "{{$session_variables['x-hasura-user-id']}}"},
        "remove_headers": ["x-conf"],
    },
    "body": {
        "action": "transform",
        "template": '{"n": {{$body.event.data.new.name}}, "op": {{$body.event.op}}, '
        '"s": {{$session_variables}}, "t": {{$body.trigger.name}}}',
    },
}
FULL_RESPONSE = {
    "version": 2,
    "body": {
        "action": "transform",
        "template": '{"got": {{$body.hello}}, "st": {{$response.status}}, '
        '"req": {{$request}}, "sv": {{$session_variables}}}',
    },
}


def md(type_, args):
    return {"url": "/v1/metadata", "body": {"type": type_, "args": args}}


def sql(s):
    return {"url": "/v2/query", "body": {"type": "run_sql", "args": {"sql": s}}}


def trigger(name, path, request=None, response=None):
    args = {
        "name": name,
        "table": "tx",
        "insert": {"columns": "*"},
        "webhook": HOOK + path,
        "headers": [{"name": "x-conf", "value": "c"}],
        "retry_conf": {"num_retries": 0, "interval_sec": 1},
    }
    if request is not None:
        args["request_transform"] = request
    if response is not None:
        args["response_transform"] = response
    return md("pg_create_event_trigger", args)


def cron(name, path, **kw):
    args = {
        "name": name,
        "webhook": HOOK + path,
        "schedule": "0 0 1 1 *",
        "include_in_metadata": True,
        "payload": {"k": 1},
        "headers": [{"name": "x-conf", "value": "c"}],
    }
    args.update(kw)
    return md("create_cron_trigger", args)


STEPS = [
    sql("CREATE TABLE tx (id int PRIMARY KEY, name text)"),
    # (`pg_track_table`, through the whole metadata.)
    md(
        "replace_metadata",
        {
            "version": 3,
            "sources": [
                {
                    "name": "default",
                    "kind": "postgres",
                    "tables": [{"table": {"schema": "public", "name": "tx"}}],
                    "configuration": {
                        "connection_info": {"database_url": {"from_env": "HASURA_GRAPHQL_DATABASE_URL"}}
                    },
                }
            ],
        },
    ),
    # Every part of the request rewritten, the response too.
    trigger("e1", "/hook1", FULL_REQUEST, FULL_RESPONSE),
    # A request transform that fails: no attempt.
    trigger("e2", "/hook2", {"version": 2, "body": {"action": "transform", "template": "{{$body.nope.x}}"}}),
    # A response transform that fails: an engine-side error.
    trigger("e3", "/hook3", None, {"version": 2, "body": {"action": "transform", "template": "{{$body.nope.y}}"}}),
    trigger(
        "e4",
        "/hook4",
        {
            "version": 2,
            "method": "PATCH",
            "body": {"action": "x_www_form_urlencoded", "form_template": {"name": "{{$body.event.data.new.name}}"}},
        },
    ),
    trigger("e5", "/hook5", {"version": 2, "body": {"action": "remove"}}, {"version": 2, "body": {"action": "remove"}}),
    # Version 1; the response transform is not applied to a failed call.
    trigger("e6", "/fail6", {"version": 1, "body": "{{$body.event.op}}"}, FULL_RESPONSE),
    trigger("e7", "/hook7", {"version": 2, "url": "{{$base_url}}?q={{$body.table.name}}"}),
    # Cron triggers: no session variables, and the transforms are part of the body.
    cron(
        "c1",
        "/cron1",
        request_transform={
            "version": 2,
            "method": "PUT",
            "query_params": {"q": "1"},
            "body": {"action": "transform", "template": '{"keys": {{$body}}, "sv": {{$session_variables}}}'},
        },
        response_transform={"version": 2, "body": {"action": "transform", "template": '{"req": {{$request}}, "sv": {{$session_variables}}}'}},
    ),
    cron("c2", "/cron2", response_transform={"version": 2, "body": {"action": "transform", "template": '{"req": {{$request}}}'}}),
    # Fails: `$session_variables` is null for cron triggers.
    cron("c3", "/cron3", request_transform=FULL_REQUEST),
    # One-off events do not keep their transforms.
    md(
        "create_scheduled_event",
        {
            "webhook": HOOK + "/oneoff",
            "schedule_at": "2099-01-01T00:00:00Z",
            "payload": {"k": 2},
            "request_transform": {"version": 2, "method": "PUT"},
        },
    ),
    {
        "url": "/v1/graphql",
        "body": {"query": 'mutation { insert_tx_one(object: {id: 7, name: "ann"}) { id } }'},
        "headers": {"x-hasura-role": "admin", "x-hasura-user-id": "u1"},
    },
    sql(
        "INSERT INTO hdb_catalog.hdb_cron_events (trigger_name, scheduled_time) "
        "VALUES ('c1', now()), ('c2', now()), ('c3', now())"
    ),
    sql("UPDATE hdb_catalog.hdb_scheduled_events SET scheduled_time = now()"),
]

# Response headers come from the webhook's HTTP stack, so they are left out.
OBSERVE = {
    "event_log": "SELECT trigger_name, delivered, error, tries, locked IS NULL "
    "FROM hdb_catalog.event_log ORDER BY trigger_name",
    "event_invocations": "SELECT trigger_name, status, request::text, "
    "(response::jsonb #- '{data,headers}')::text "
    "FROM hdb_catalog.event_invocation_logs ORDER BY trigger_name",
    "cron_events": "SELECT trigger_name, status, tries FROM hdb_catalog.hdb_cron_events "
    "WHERE scheduled_time < now() ORDER BY trigger_name",
    "cron_invocations": "SELECT e.trigger_name, i.status, i.request::text, "
    "(i.response::jsonb #- '{data,headers}')::text "
    "FROM hdb_catalog.hdb_cron_event_invocation_logs i "
    "JOIN hdb_catalog.hdb_cron_events e ON e.id = i.event_id ORDER BY e.trigger_name",
    "one_off": "SELECT e.status, e.tries, i.status, i.request::text "
    "FROM hdb_catalog.hdb_scheduled_events e "
    "LEFT JOIN hdb_catalog.hdb_scheduled_event_invocation_logs i ON i.event_id = e.id",
}

# Request headers the webhook reports; the rest depend on the HTTP client.
HOOK_HEADERS = ["content-type", "user-agent", "x-added", "x-conf", "x-sess"]

PATTERNS = [
    (re.compile(r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}"), "<uuid>"),
    (re.compile(r"\d{4}-\d\d-\d\d[T ][0-9:.]+Z?"), "<time>"),
    (re.compile(r"(?<![0-9a-zA-Z])[0-9a-f]{16}(?:[0-9a-f]{16})?(?![0-9a-zA-Z])"), "<hex>"),
    (re.compile(r"hasura-graphql-engine/[^\"\\]*"), "<user-agent>"),
]


def normalise(text):
    for pattern, replacement in PATTERNS:
        text = pattern.sub(replacement, text)
    return text


def main():
    base, out = sys.argv[1:3]
    hook = "http://127.0.0.1:9911"

    def post(step):
        body = json.dumps(step["body"]).replace(HOOK, hook).encode()
        headers = {"content-type": "application/json", **step.get("headers", {})}
        req = urllib.request.Request(base + step["url"], data=body, headers=headers)
        try:
            with urllib.request.urlopen(req) as r:
                return r.status, json.loads(r.read())
        except urllib.error.HTTPError as e:
            return e.code, json.loads(e.read())

    urllib.request.urlopen(hook + "/reset").read()
    responses = [post(s) for s in STEPS]
    time.sleep(15)
    observed = {
        name: post(sql(q))[1]["result"][1:] for name, q in OBSERVE.items()
    }
    with urllib.request.urlopen(hook + "/log") as r:
        calls = json.loads(r.read())
    observed["webhook"] = sorted(
        (
            {
                "method": c["method"],
                "path": c["path"],
                "headers": {k: v for k, v in sorted(c["headers"].items()) if k in HOOK_HEADERS},
                "body": c["body"],
            }
            for c in calls
        ),
        key=lambda c: c["path"],
    )
    observed = json.loads(normalise(json.dumps(observed)))
    result = {
        "description": "Recorded from hasura/graphql-engine:v2.51.0-ce by record/transforms_steps.py.",
        "steps": [dict(s, status=r[0]) for s, r in zip(STEPS, responses)],
        "observe": OBSERVE,
        "webhook_headers": HOOK_HEADERS,
        "observed": observed,
    }
    with open(out, "w") as f:
        json.dump(result, f, indent=1)
        f.write("\n")


if __name__ == "__main__":
    main()
