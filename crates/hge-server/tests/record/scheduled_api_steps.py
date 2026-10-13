"""Steps of `../scheduled_api.json` (cron triggers and one-off scheduled events); see record.py.

Events are given fixed ids and times through `run_sql` (all in the future, so nothing is
delivered while recording) to make responses deterministic.
"""

DESCRIPTION = (
    "Recorded from hasura/graphql-engine:v2.51.0-ce on a fresh database; replayed in order. "
    "Steps marked todo are known differences and must keep failing until fixed."
)

M = "/v1/metadata"
Q = "/v2/query"
HOOK = "http://127.0.0.1:1/hook"


def md(type_, args):
    return (M, {"type": type_, "args": args})


def sql(s):
    return (Q, {"type": "run_sql", "args": {"sql": s}})


def cron(**kw):
    args = {"name": "c", "webhook": HOOK, "schedule": "0 0 1 1 *", "include_in_metadata": True}
    args.update(kw)
    return md("create_cron_trigger", {k: v for k, v in args.items() if v is not None})


EVENTS_BY_TRIGGER = (
    "SELECT trigger_name, count(*), min(extract(month FROM scheduled_time AT TIME ZONE 'UTC')),"
    " min(extract(day FROM scheduled_time AT TIME ZONE 'UTC')), bool_and(scheduled_time > now())"
    " FROM hdb_catalog.hdb_cron_events GROUP BY 1 ORDER BY 1"
)

STEPS = [
    md("clear_metadata", {}),
    sql("DELETE FROM hdb_catalog.hdb_cron_events; DELETE FROM hdb_catalog.hdb_scheduled_events"),
    # Cron trigger argument errors.
    md("create_cron_trigger", {}),
    cron(name=None),
    cron(webhook=None),
    cron(schedule=None),
    cron(include_in_metadata=None),
    cron(schedule="garbage"),
    cron(schedule="* * * *"),
    cron(schedule="61 * * * *"),
    cron(schedule="* * * * * *"),
    cron(retry_conf={"num_retries": -1}),
    cron(retry_conf={"retry_interval_seconds": -1}),
    cron(retry_conf={"num_retries": "x"}),
    cron(headers=[{"name": "a"}]),
    cron(name=1),
    # Creation.
    cron(
        name="c1",
        payload={"b": 1, "a": 2},
        headers=[{"name": "X", "value": "y"}],
        comment="hi",
        retry_conf={"num_retries": 1, "retry_interval_seconds": 1.5},
    ),
    cron(name="c1"),
    cron(name="c2", replace=True),
    cron(name="c2", schedule="@daily", include_in_metadata=False),
    cron(name="bad name", schedule="*/5 * * * *"),
    md("delete_cron_trigger", {"name": "bad name"}),
    cron(name="c3", webhook="{{SCHED_UNSET_HOOK}}"),
    md("get_cron_triggers", {}),
    (M, {"type": "get_cron_triggers"}),
    md("export_metadata", {}),
    sql(EVENTS_BY_TRIGGER),
    sql(
        "UPDATE hdb_catalog.hdb_cron_events e SET id = '20000000-0000-0000-0000-' || lpad(s.n::text, 12, '0'),"
        " scheduled_time = '2030-01-01T00:00:00Z'::timestamptz + s.n * interval '1 day',"
        " created_at = '2026-01-01T00:00:00.25Z'"
        " FROM (SELECT id, row_number() OVER (ORDER BY scheduled_time) AS n FROM hdb_catalog.hdb_cron_events"
        " WHERE trigger_name = 'c1') s WHERE e.id = s.id"
    ),
    # Listing cron events.
    md("get_scheduled_events", {"type": "cron", "trigger_name": "c1", "limit": 2, "offset": 1, "get_rows_count": True}),
    md("get_scheduled_events", {"type": "cron", "trigger_name": "c1", "limit": 1, "order_by": [{"column": "scheduled_time", "type": "desc"}]}),
    md("get_scheduled_events", {"type": "cron", "trigger_name": "c1", "status": ["delivered", "error"], "get_rows_count": True}),
    md("get_scheduled_events", {"type": "cron", "trigger_name": "nope"}),
    md("get_scheduled_events", {"type": "cron"}),
    md("get_scheduled_events", {"type": "cron", "trigger_name": "c1", "limit": 1, "order_by": [{"column": "nope", "type": "asc"}]}),
    md("get_scheduled_events", {"type": "cron", "trigger_name": "c1", "status": ["bogus"]}),
    md("get_scheduled_events", {"type": "nope"}),
    md("get_scheduled_events", {}),
    # One-off events.
    md(
        "create_scheduled_event",
        {
            "webhook": HOOK,
            "schedule_at": "2030-05-01T10:00:00Z",
            "payload": {"z": 1, "a": [1, 2]},
            "headers": [{"name": "X", "value": "y"}],
            "retry_conf": {"num_retries": 2, "timeout_seconds": 5},
            "comment": "one",
        },
    ),
    md("create_scheduled_event", {"webhook": HOOK, "schedule_at": "2030-05-01 12:30:00.5+02:00"}),
    md("create_scheduled_event", {"webhook": HOOK, "schedule_at": "2030-05-01T10:00:00"}),
    md("create_scheduled_event", {"schedule_at": "2030-05-01T10:00:00Z"}),
    md("create_scheduled_event", {"webhook": HOOK}),
    md("create_scheduled_event", {"webhook": HOOK, "schedule_at": "nope"}),
    md("create_scheduled_event", {"webhook": HOOK, "schedule_at": "2030-05-01T10:00:00Z", "retry_conf": {"num_retries": -2}}),
    sql(
        "UPDATE hdb_catalog.hdb_scheduled_events e SET id = '30000000-0000-0000-0000-' || lpad(s.n::text, 12, '0'),"
        " created_at = '2026-01-02T00:00:00Z'"
        " FROM (SELECT id, row_number() OVER (ORDER BY scheduled_time) AS n FROM hdb_catalog.hdb_scheduled_events) s"
        " WHERE e.id = s.id"
    ),
    md("get_scheduled_events", {"type": "one_off"}),
    md("get_scheduled_events", {"type": "one_off", "get_rows_count": True, "status": ["scheduled"], "limit": 1, "offset": 1}),
    # Invocations.
    sql(
        "INSERT INTO hdb_catalog.hdb_scheduled_event_invocation_logs (id, event_id, status, request, response, created_at) VALUES"
        " ('40000000-0000-0000-0000-000000000001', '30000000-0000-0000-0000-000000000001', 500,"
        "  '{\"version\":\"2\",\"payload\":{\"b\":1,\"a\":2}}', '{\"type\":\"client_error\",\"data\":{\"message\":\"x\"}}', '2026-01-03T00:00:00Z'),"
        " ('40000000-0000-0000-0000-000000000002', '30000000-0000-0000-0000-000000000002', NULL, '{\"v\":1}', '{\"v\":2}', '2026-01-04T00:00:00.5Z');"
        " INSERT INTO hdb_catalog.hdb_cron_event_invocation_logs (id, event_id, status, request, response, created_at) VALUES"
        " ('50000000-0000-0000-0000-000000000001', '20000000-0000-0000-0000-000000000001', 200, '{\"a\":1}', '{\"b\":2}', '2026-01-05T00:00:00Z'),"
        " ('50000000-0000-0000-0000-000000000002', '20000000-0000-0000-0000-000000000002', 200, '{\"a\":1}', '{\"b\":2}', '2026-01-06T00:00:00Z')"
    ),
    md("get_scheduled_event_invocations", {"type": "one_off"}),
    md("get_scheduled_event_invocations", {"type": "one_off", "get_rows_count": True, "limit": 1}),
    md("get_scheduled_event_invocations", {"type": "one_off", "event_id": "30000000-0000-0000-0000-000000000001"}),
    md("get_scheduled_event_invocations", {"type": "cron", "trigger_name": "c1", "get_rows_count": True}),
    md("get_scheduled_event_invocations", {"type": "cron", "event_id": "20000000-0000-0000-0000-000000000002"}),
    md("get_scheduled_event_invocations", {"type": "cron"}),
    md("get_scheduled_event_invocations", {"type": "cron", "trigger_name": "nope"}),
    md("get_scheduled_event_invocations", {}),
    # Deleting events.
    md("delete_scheduled_event", {"type": "one_off", "event_id": "30000000-0000-0000-0000-000000000002"}),
    md("delete_scheduled_event", {"type": "one_off", "event_id": "nope"}),
    md("delete_scheduled_event", {"type": "cron", "event_id": "20000000-0000-0000-0000-000000000001"}),
    md("delete_scheduled_event", {"event_id": "x"}),
    md("get_scheduled_events", {"type": "one_off", "get_rows_count": True}),
    md("get_scheduled_event_invocations", {"type": "cron", "trigger_name": "c1"}),
    # Replacing and deleting cron triggers.
    md("delete_cron_trigger", {"name": "nope"}),
    cron(name="c1", schedule="0 0 2 1 *", replace=True),
    sql(EVENTS_BY_TRIGGER),
    md("get_cron_triggers", {}),
    md("delete_cron_trigger", {"name": "c1"}),
    sql(EVENTS_BY_TRIGGER),
    md("delete_cron_trigger", {"name": "c2"}),
    md("get_cron_triggers", {}),
    md("export_metadata", {}),
]
