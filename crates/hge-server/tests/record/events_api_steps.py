"""Steps of `../events_api.json` (the event trigger metadata API); see record.py."""

DESCRIPTION = (
    "Recorded from hasura/graphql-engine:v2.51.0-ce on a fresh database (event processing "
    "paused); replayed in order. Steps marked todo are known differences and must keep "
    "failing until fixed."
)

M = "/v1/metadata"
Q = "/v2/query"


def md(type_, args):
    return (M, {"type": type_, "args": args})


def sql(s):
    return (Q, {"type": "run_sql", "args": {"sql": s}})


T1 = {"schema": "public", "name": "ev_t1"}
T2 = {"schema": "public", "name": "ev_t2"}
HOOK = "http://127.0.0.1:1/hook"

STEPS = [
    sql(
        "DROP TABLE IF EXISTS ev_t1, ev_t2, ev_untracked CASCADE;"
        " CREATE TABLE ev_t1(id serial primary key, c1 int, c2 text);"
        " CREATE TABLE ev_t2(id int primary key, v numeric);"
        " CREATE TABLE ev_untracked(id int primary key);"
    ),
    md("clear_metadata", {}),
    md(
        "replace_metadata",
        {
            "version": 3,
            "sources": [
                {
                    "name": "default",
                    "kind": "postgres",
                    "tables": [{"table": T1}, {"table": T2}],
                    "configuration": {
                        "connection_info": {
                            "database_url": {"from_env": "HASURA_GRAPHQL_DATABASE_URL"},
                            "isolation_level": "read-committed",
                            "use_prepared_statements": False,
                        }
                    },
                }
            ],
        },
    ),
    # Parse errors, in V2's check order.
    md("pg_create_event_trigger", {"name": "bad'name", "table": T1, "enable_manual": True}),
    md("pg_create_event_trigger", {"name": "a" * 43, "table": T1, "enable_manual": True}),
    md("pg_create_event_trigger", {"name": "bad'name" + "a" * 43, "table": T1}),
    md("pg_create_event_trigger", {"name": "t", "table": T1, "webhook": HOOK}),
    md("pg_create_event_trigger", {"name": "t", "table": T1, "enable_manual": False, "webhook": HOOK}),
    md("pg_create_event_trigger", {"name": "t", "table": T1, "insert": {"columns": "*"}}),
    md(
        "pg_create_event_trigger",
        {"name": "t", "table": T1, "insert": {"columns": "*"}, "webhook": HOOK, "webhook_from_env": "X"},
    ),
    md("pg_create_event_trigger", {"name": "t", "table": T1, "insert": {"columns": []}, "webhook": HOOK}),
    md(
        "pg_create_event_trigger",
        {"name": "t", "table": T1, "update": {"columns": "*", "payload": []}, "webhook": HOOK},
    ),
    md(
        "pg_create_event_trigger",
        {"name": "t", "table": T1, "update": {"columns": ["c1"], "payload": []}, "webhook": HOOK, "replace": True},
    ),
    md("pg_create_event_trigger", {"name": "t", "table": T1, "insert": {"columns": "x"}, "webhook": HOOK}),
    md("pg_create_event_trigger", {"table": T1, "insert": {"columns": "*"}, "webhook": HOOK}),
    md("pg_create_event_trigger", {"name": "t", "insert": {"columns": "*"}, "webhook": HOOK}),
    md(
        "pg_create_event_trigger",
        {"name": "t", "table": T1, "insert": {"columns": "*"}, "webhook": HOOK, "headers": [{"name": "a"}]},
    ),
    md(
        "pg_create_event_trigger",
        {
            "name": "t",
            "table": T1,
            "insert": {"columns": "*"},
            "webhook": HOOK,
            "headers": [{"name": "a", "value_from_env": "HASURA_GRAPHQL_X"}],
        },
    ),
    md("pg_create_event_trigger", {"name": "tk", "table": T1, "insert": {"columns": "*"}, "webhook": HOOK, "unknown_key": 1}),
    md("pg_create_event_trigger", {"name": "t", "table": T1, "insert": {"columns": "*"}, "webhook": HOOK, "retry_conf": {"num_retries": 1}}),
    # Semantic errors.
    md("pg_create_event_trigger", {"name": "t", "table": {"schema": "public", "name": "ev_untracked"}, "insert": {"columns": "*"}, "webhook": HOOK}),
    md("pg_create_event_trigger", {"name": "t", "table": "ev_untracked", "insert": {"columns": "*"}, "webhook": HOOK}),
    md("pg_create_event_trigger", {"name": "t", "source": "nope", "table": T1, "insert": {"columns": "*"}, "webhook": HOOK}),
    md("pg_create_event_trigger", {"name": "t", "table": T1, "insert": {"columns": "*"}, "webhook": HOOK, "replace": True}),
    md("pg_create_event_trigger", {"name": "t", "table": T1, "update": {"columns": ["nope"]}, "webhook": HOOK}),
    # V2 rejects this but leaves the trigger's SQL behind (we do not), so later catalog
    # queries leave `tp` out.
    md("pg_create_event_trigger", {"name": "tp", "table": T1, "update": {"columns": ["c1"], "payload": ["nope"]}, "webhook": HOOK}),
    md("pg_create_event_trigger", {"name": "t", "table": T1, "insert": {"columns": "*"}, "webhook_from_env": "EV_UNSET_HOOK"}),
    # Success paths.
    md(
        "pg_create_event_trigger",
        {
            "name": "t1",
            "table": T1,
            "insert": {"columns": "*"},
            "update": {"columns": ["c1"], "payload": ["id", "c1"]},
            "webhook": HOOK,
            "headers": [{"name": "X-A", "value": "b"}],
        },
    ),
    md(
        "pg_create_event_trigger",
        {
            "name": "manual-1",
            "table": "ev_t2",
            "enable_manual": True,
            "delete": {"columns": "*"},
            "retry_conf": {"num_retries": 2, "interval_sec": 5},
            "webhook_from_env": "PATH",
            "cleanup_config": {"schedule": "0 0 * * *", "clear_older_than": 24},
            "trigger_on_replication": True,
        },
    ),
    md("pg_create_event_trigger", {"name": "t1", "table": T1, "insert": {"columns": "*"}, "webhook": HOOK}),
    md("pg_create_event_trigger", {"name": "t1", "table": T2, "insert": {"columns": "*"}, "webhook": HOOK}),
    md("pg_create_event_trigger", {"name": "t1", "table": T2, "insert": {"columns": "*"}, "webhook": HOOK, "replace": True}),
    md(
        "pg_create_event_trigger",
        {"name": "t1", "table": T1, "insert": {"columns": "*"}, "delete": {"columns": "*"}, "webhook": HOOK, "replace": True},
    ),
    sql(
        "SELECT p.proname FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace"
        " WHERE n.nspname = 'hdb_catalog' AND p.proname LIKE 'notify_hasura_%' AND p.proname NOT LIKE '%_tp_%' ORDER BY 1"
    ),
    sql("SELECT tgname, tgrelid::regclass::text, tgenabled FROM pg_trigger WHERE tgname LIKE 'notify_hasura_%' AND tgname NOT LIKE '%_tp_%' ORDER BY 1"),
    md("export_metadata", {}),
    # Manual invocation.
    md("pg_invoke_event_trigger", {"name": "t1", "payload": {"a": 1}}),
    md("pg_invoke_event_trigger", {"name": "nope", "payload": {"a": 1}}),
    md("pg_invoke_event_trigger", {"name": "manual-1"}),
    md("pg_invoke_event_trigger", {"name": "manual-1", "source": "nope", "payload": {}}),
    md("pg_invoke_event_trigger", {"name": "manual-1", "payload": {"z": 1, "a": [1, 2.5, None, "x"], "m": {"b": True, "a": False}}}),
    sql(
        "UPDATE hdb_catalog.event_log SET id = '00000000-0000-0000-0000-00000000000a',"
        " created_at = '2024-01-02 03:04:05.5' WHERE trigger_name = 'manual-1'"
    ),
    # Row events.
    sql("INSERT INTO ev_t1(c1, c2) VALUES (1, 'x'); UPDATE ev_t1 SET c2 = 'y'; UPDATE ev_t1 SET c1 = 2; DELETE FROM ev_t1"),
    sql(
        "UPDATE hdb_catalog.event_log SET id = '00000000-0000-0000-0000-0000000000' || lpad(n::text, 2, '0'),"
        " created_at = '2024-01-01'::timestamp + n * interval '1 minute' FROM (SELECT id AS i, row_number() OVER (ORDER BY created_at, payload->>'op') AS n"
        " FROM hdb_catalog.event_log WHERE trigger_name = 't1') s WHERE id = s.i"
    ),
    sql(
        "SELECT id, trigger_name, payload->>'op', payload->'data'::text, tries FROM hdb_catalog.event_log WHERE trigger_name <> 'tp' ORDER BY id"
    ),
    md("pg_get_event_logs", {"name": "t1"}),
    md("pg_get_event_logs", {"name": "manual-1"}),
    md("pg_get_event_logs", {"name": "t1", "status": "all"}),
    md("pg_get_event_logs", {"name": "t1", "status": "pending", "limit": 1, "offset": 1}),
    md("pg_get_event_logs", {"name": "nope"}),
    md("pg_get_event_logs", {"name": "t1", "limit": -1}),
    # Redelivery.
    md("pg_redeliver_event", {"event_id": "nope"}),
    sql(
        "UPDATE hdb_catalog.event_log SET delivered = 't', tries = 3, locked = '2024-01-01 00:00:00+00', next_retry_at = '2024-02-01 00:00:00'"
        " WHERE id = '00000000-0000-0000-0000-000000000001'"
    ),
    md("pg_redeliver_event", {"event_id": "00000000-0000-0000-0000-000000000001"}),
    sql("SELECT delivered, error, tries, locked IS NULL, next_retry_at FROM hdb_catalog.event_log WHERE id = '00000000-0000-0000-0000-000000000001'"),
    sql("UPDATE hdb_catalog.event_log SET locked = now() WHERE id = '00000000-0000-0000-0000-000000000002'"),
    md("pg_redeliver_event", {"event_id": "00000000-0000-0000-0000-000000000002"}),
    sql(
        "UPDATE hdb_catalog.event_log SET locked = '2024-03-04 05:06:07.25+00', error = 't'"
        " WHERE id = '00000000-0000-0000-0000-000000000002'"
    ),
    md("pg_get_event_logs", {"name": "t1", "status": "processed"}),
    # Invocation logs.
    sql(
        "INSERT INTO hdb_catalog.event_invocation_logs (id, trigger_name, event_id, status, request, response, created_at) VALUES"
        " ('10000000-0000-0000-0000-000000000001', 't1', '00000000-0000-0000-0000-000000000001', 500,"
        "  '{\"version\":\"2\",\"payload\":{\"b\":1,\"a\":2},\"headers\":[]}', '{\"type\":\"client_error\",\"data\":{\"message\":\"x\"}}', '2024-01-05 00:00:00'),"
        " ('10000000-0000-0000-0000-000000000002', 't1', '00000000-0000-0000-0000-000000000001', NULL,"
        "  '{\"version\":\"2\"}', '{\"type\":\"webhook_response\",\"data\":{\"status\":200}}', '2024-01-06 00:00:00.123')"
    ),
    md("pg_get_event_invocation_logs", {"name": "t1"}),
    md("pg_get_event_invocation_logs", {"name": "t1", "limit": 1, "offset": 1}),
    md("pg_get_event_invocation_logs", {"name": "nope"}),
    md("pg_get_event_by_id", {"event_id": "00000000-0000-0000-0000-000000000001"}),
    md("pg_get_event_by_id", {"event_id": "00000000-0000-0000-0000-000000000001", "invocation_log_limit": 1}),
    md("pg_get_event_by_id", {"event_id": "nope"}),
    md("pg_get_event_by_id", {}),
    # Cleanups.
    md("cleanup_event_trigger_logs", {"event_trigger_name": "t1", "clear_older_than": 1}),
    md("pause_event_trigger_cleanups", {"event_triggers": {"sources": "*"}}),
    md("pause_event_trigger_cleanups", {"event_triggers": [{"event_triggers": ["t1"]}]}),
    md("pause_event_trigger_cleanups", {"event_triggers": [{"source_name": "nope", "event_triggers": ["t1"]}]}),
    md("export_metadata", {}),
    md("resume_event_trigger_cleanups", {"event_triggers": [{"event_triggers": ["manual-1"]}]}),
    md("resume_event_trigger_cleanups", {"event_triggers": {"sources": ["default"]}}),
    md("pause_event_trigger_cleanups", {"event_triggers": {"sources": ["nope"]}}),
    md("export_metadata", {}),
    # Deletion.
    md("pg_delete_event_trigger", {"name": "nope"}),
    md("pg_delete_event_trigger", {"name": "t1"}),
    md("pg_delete_event_trigger", {"name": "t1"}),
    sql("SELECT trigger_name, archived, count(*) FROM hdb_catalog.event_log WHERE trigger_name <> 'tp' GROUP BY 1, 2 ORDER BY 1, 2"),
    sql(
        "SELECT p.proname FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace"
        " WHERE n.nspname = 'hdb_catalog' AND p.proname LIKE 'notify_hasura_%' AND p.proname NOT LIKE '%_tp_%' ORDER BY 1"
    ),
    md("pg_get_event_by_id", {"event_id": "00000000-0000-0000-0000-000000000003", "invocation_log_limit": 0}),
    # Bulk: errors are reported under the item.
    (M, {"type": "bulk", "args": [
        {"type": "pg_create_event_trigger", "args": {"name": "t2", "table": T1, "insert": {"columns": "*"}, "webhook": HOOK}},
        {"type": "pg_create_event_trigger", "args": {"name": "t3", "table": T1}},
    ]}),
    (M, {"type": "bulk", "args": [
        {"type": "pg_create_event_trigger", "args": {"name": "t2", "table": T1, "insert": {"columns": "*"}, "webhook": HOOK}},
        {"type": "pg_create_event_trigger", "args": {"name": "t2", "table": T2, "insert": {"columns": "*"}, "webhook": HOOK}},
    ]}),
    md("export_metadata", {}),
    md("pg_delete_event_trigger", {"name": "manual-1"}),
    md("export_metadata", {}),
    # Argument decoding details.
    md("pg_create_event_trigger", {"name": None, "table": T1, "insert": {"columns": "*"}, "webhook": HOOK}),
    md("pg_create_event_trigger", {"name": "tf", "table": T1, "insert": {"columns": "*"}, "webhook": HOOK, "retry_conf": {"num_retries": 1.5, "interval_sec": 10}}),
    md("pg_create_event_trigger", {"name": "tf", "table": T1, "insert": {"columns": "*"}, "webhook": HOOK, "retry_conf": {"num_retries": 1.0, "interval_sec": 10}}),
    md("pg_create_event_trigger", {"name": "tc", "table": T1, "delete": 1, "enable_manual": "x", "webhook": HOOK}),
    md("pg_create_event_trigger", {"name": "tc", "table": T1, "insert": {"columns": "*"}, "webhook": HOOK, "cleanup_config": {"schedule": "garbage", "clear_older_than": 1}}),
    md("pg_create_event_trigger", {"name": "th", "table": T1, "insert": {"columns": "*"}, "webhook": HOOK, "headers": [{"name": "a", "value": "b", "extra": 1}]}),
    md("export_metadata", {}),
    md("pg_delete_event_trigger", {"name": "th"}),
    md("pg_delete_event_trigger", {"name": "tf"}),
    md("pause_event_trigger_cleanups", {"event_triggers": [{"event_triggers": ["zzz"]}]}),
    md("pg_get_event_logs", {"name": "tk", "source": "nope"}),
    md("pg_get_event_by_id", {"event_id": "x", "source": "nope"}),
    md("pg_redeliver_event", {"event_id": "x", "source": "nope"}),
    # A manual-only trigger has no SQL; deleting it still archives its events.
    md("pg_create_event_trigger", {"name": "td", "table": T2, "enable_manual": True, "webhook": HOOK}),
    md("pg_invoke_event_trigger", {"name": "td", "payload": {"a": 1}}),
    sql("SELECT payload->>'op', archived FROM hdb_catalog.event_log WHERE trigger_name = 'td'"),
    md("pg_delete_event_trigger", {"name": "td"}),
    sql("SELECT archived, count(*) FROM hdb_catalog.event_log WHERE trigger_name = 'td' GROUP BY 1"),
    # An event trigger made inconsistent by a dropped column can be dropped.
    md("pg_create_event_trigger", {"name": "tx", "table": T1, "update": {"columns": ["c2"]}, "webhook": HOOK}),
    (Q, {"type": "run_sql", "args": {"sql": "ALTER TABLE ev_t1 DROP COLUMN c2", "check_metadata_consistency": False}}),
    md("reload_metadata", {}),
    md("get_inconsistent_metadata", {}),
    md("drop_inconsistent_metadata", {}),
    md("export_metadata", {}),
]
