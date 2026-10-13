//! SQL text for the source catalog and the per-trigger functions (pure; no IO).
//!
//! The catalog files are V2's own (`server/src-rsr/init_pg_source.sql` and
//! `pg_source_migrations/*.sql`), so databases created by V2 and by this engine are identical.

use hge_core::QualifiedTable;

use crate::model::{Op, SubscribeColumns, SubscribeOpSpec};

/// Current source catalog version (V2 v2.51.0).
pub const SOURCE_CATALOG_VERSION: i32 = 4;

/// Creates `event_log`, `event_invocation_logs`, `insert_event_log()`, … in `hdb_catalog`.
pub const INIT_PG_SOURCE: &str = include_str!("sql/init_pg_source.sql");

/// Drops the source catalog objects (V2 `drop_pg_source.sql`).
pub const DROP_PG_SOURCE: &str = include_str!("sql/drop_pg_source.sql");

/// `pg_source_migrations/<n>_to_<n+1>.sql`, indexed by `n`.
pub const SOURCE_MIGRATIONS: [&str; 4] = [
    include_str!("sql/source_0_to_1.sql"),
    include_str!("sql/source_1_to_2.sql"),
    include_str!("sql/source_2_to_3.sql"),
    include_str!("sql/source_3_to_4.sql"),
];

/// A column the trigger can see, as known to the engine when the function is created.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TriggerColumn {
    pub name: String,
    /// The type's name as in `pg_type.typname` (`int8`, `numeric`, `geometry`, …).
    pub type_name: String,
}

/// `pgFmtIdentifier`: wrap in double quotes, double inner quotes, truncate at NUL.
pub fn quote_ident(s: &str) -> String {
    let s = s.split('\0').next().unwrap_or("");
    format!("\"{}\"", s.replace('"', "\"\""))
}

/// `pgFmtLit`: a string literal, `E'…'` with doubled backslashes when needed.
pub fn quote_lit(s: &str) -> String {
    let s = s.split('\0').next().unwrap_or("");
    let body = s.replace('\'', "''");
    if body.contains('\\') {
        format!("E'{}'", body.replace('\\', "\\\\"))
    } else {
        format!("'{body}'")
    }
}

pub fn qualified_table(t: &QualifiedTable) -> String {
    format!("{}.{}", quote_ident(&t.schema), quote_ident(&t.name))
}

/// `notify_hasura_<trigger>_<OP>`: the name of both the function and the SQL trigger.
pub fn trigger_object_name(trigger: &str, op: Op) -> String {
    format!("notify_hasura_{trigger}_{}", op.as_str())
}

fn is_bignum(type_name: &str) -> bool {
    matches!(type_name, "int8" | "numeric" | "float8" | "money")
}

fn is_geo(type_name: &str) -> bool {
    matches!(type_name, "geometry" | "geography")
}

/// `toJSONableExp` for `OLD."col"` / `NEW."col"`.
fn column_expr(var: &str, col: &TriggerColumn, stringify_numeric: bool) -> String {
    let id = format!("{var}.{}", quote_ident(&col.name));
    if is_geo(&col.type_name) {
        format!(
            "ST_AsGeoJSON({id}, 15, 4)::json AS {}",
            quote_ident(&col.name)
        )
    } else if stringify_numeric && is_bignum(&col.type_name) {
        format!("({id})::text")
    } else {
        id
    }
}

fn applicable<'a>(spec: &SubscribeColumns, all: &'a [TriggerColumn]) -> Vec<&'a TriggerColumn> {
    match spec {
        SubscribeColumns::Star => all.iter().collect(),
        SubscribeColumns::Columns(names) => names
            .iter()
            .filter_map(|n| all.iter().find(|c| &c.name == n))
            .collect(),
    }
}

/// `(SELECT "e" FROM (SELECT <cols>) AS "e")`
fn row_exp(var: &str, cols: &[&TriggerColumn], stringify_numeric: bool) -> String {
    let cols: Vec<String> = cols
        .iter()
        .map(|c| column_expr(var, c, stringify_numeric))
        .collect();
    format!("(SELECT \"e\" FROM (SELECT {}) AS \"e\")", cols.join(", "))
}

/// The body (between `$$`) of `hdb_catalog."notify_hasura_<name>_<OP>"()`, as V2's
/// `trigger.sql.shakespeare` renders it.
pub fn trigger_function_body(
    trigger: &str,
    table: &QualifiedTable,
    op: Op,
    spec: &SubscribeOpSpec,
    all_columns: &[TriggerColumn],
    stringify_numeric: bool,
) -> String {
    let listen = applicable(&spec.columns, all_columns);
    let delivery = applicable(
        spec.payload.as_ref().unwrap_or(&SubscribeColumns::Star),
        all_columns,
    );
    let old_row = format!("row({})", row_exp("OLD", &listen, stringify_numeric));
    let new_row = format!("row({})", row_exp("NEW", &listen, stringify_numeric));
    let payload = |var| {
        format!(
            "row_to_json({})",
            row_exp(var, &delivery, stringify_numeric)
        )
    };
    let (old_payload, new_payload) = match op {
        Op::Insert => ("NULL".to_owned(), payload("NEW")),
        Op::Update => (payload("OLD"), payload("NEW")),
        Op::Delete => (payload("OLD"), "NULL".to_owned()),
    };
    let insert = format!(
        "PERFORM hdb_catalog.insert_event_log(CAST({} AS text), CAST({} AS text), CAST({} AS text), TG_OP, _data);",
        quote_lit(&table.schema),
        quote_lit(&table.name),
        quote_lit(trigger),
    );
    format!(
        r#"
  DECLARE
    _old record;
    _new record;
    _data json;
  BEGIN
    IF TG_OP = 'UPDATE' THEN
      _old := {old_row};
      _new := {new_row};
    ELSE
    /* initialize _old and _new with dummy values for INSERT and UPDATE events*/
      _old := row((select 1));
      _new := row((select 1));
    END IF;
    _data := json_build_object(
      'old', {old_payload},
      'new', {new_payload}
    );
    BEGIN
    /* NOTE: formerly we used TG_TABLE_NAME in place of tableName here. However in the case of
    partitioned tables this will give the name of the partitioned table and since we use the table name to
    get the event trigger configuration from the schema, this fails because the event trigger is only created
    on the original table.  */
      IF (TG_OP <> 'UPDATE') OR (_old <> _new) THEN
        {insert}
      END IF;
      EXCEPTION WHEN undefined_function THEN
        IF (TG_OP <> 'UPDATE') OR (_old *<> _new) THEN
          {insert}
        END IF;
    END;

    RETURN NULL;
  END;
"#
    )
}

/// `CREATE OR REPLACE FUNCTION` for one op, with the body from [`trigger_function_body`].
pub fn create_trigger_function_sql(trigger: &str, op: Op, body: &str) -> String {
    format!(
        "CREATE OR REPLACE function hdb_catalog.{}() RETURNS trigger\n  LANGUAGE plpgsql\n  AS $${body}$$;",
        quote_ident(&trigger_object_name(trigger, op))
    )
}

/// `CREATE TRIGGER` on the user's table.
pub fn create_trigger_sql(trigger: &str, op: Op, table: &QualifiedTable) -> String {
    let name = quote_ident(&trigger_object_name(trigger, op));
    format!(
        "CREATE TRIGGER {name} AFTER {} ON {} FOR EACH ROW EXECUTE PROCEDURE hdb_catalog.{name}()",
        op.as_str(),
        qualified_table(table)
    )
}

/// `trigger_on_replication: true`.
pub fn enable_always_sql(trigger: &str, op: Op, table: &QualifiedTable) -> String {
    format!(
        "ALTER TABLE {} ENABLE ALWAYS TRIGGER {}",
        qualified_table(table),
        quote_ident(&trigger_object_name(trigger, op))
    )
}

/// Drop one op's function (and, through `CASCADE`, its SQL trigger).
pub fn drop_trigger_function_sql(trigger: &str, op: Op) -> String {
    format!(
        "DROP FUNCTION IF EXISTS hdb_catalog.{}() CASCADE",
        quote_ident(&trigger_object_name(trigger, op))
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cols() -> Vec<TriggerColumn> {
        [
            ("id", "int4"),
            ("name", "text"),
            ("big", "int8"),
            ("geo", "geometry"),
        ]
        .into_iter()
        .map(|(n, t)| TriggerColumn {
            name: n.into(),
            type_name: t.into(),
        })
        .collect()
    }

    #[test]
    fn quoting() {
        assert_eq!(quote_ident("my-trigger\"x"), "\"my-trigger\"\"x\"");
        assert_eq!(quote_lit("it's"), "'it''s'");
        assert_eq!(quote_lit("a\\b"), "E'a\\\\b'");
    }

    #[test]
    fn update_function_uses_listen_and_payload_columns() {
        let spec = SubscribeOpSpec {
            columns: SubscribeColumns::Columns(vec!["name".into()]),
            payload: Some(SubscribeColumns::Columns(vec!["id".into(), "big".into()])),
        };
        let t = QualifiedTable::public("users");
        let body = trigger_function_body("user_upd", &t, Op::Update, &spec, &cols(), true);
        assert!(body.contains(r#"_old := row((SELECT "e" FROM (SELECT OLD."name") AS "e"));"#));
        assert!(body.contains(
            r#"'old', row_to_json((SELECT "e" FROM (SELECT OLD."id", (OLD."big")::text) AS "e")),"#
        ));
        assert!(body.contains(
            "PERFORM hdb_catalog.insert_event_log(CAST('public' AS text), CAST('users' AS text), CAST('user_upd' AS text), TG_OP, _data);"
        ));
        let sql = create_trigger_function_sql("user_upd", Op::Update, &body);
        assert!(sql.starts_with(
            "CREATE OR REPLACE function hdb_catalog.\"notify_hasura_user_upd_UPDATE\"() RETURNS trigger"
        ));
    }

    #[test]
    fn insert_and_delete_payloads() {
        let spec = SubscribeOpSpec {
            columns: SubscribeColumns::Star,
            payload: None,
        };
        let t = QualifiedTable::new("s", "T");
        let ins = trigger_function_body("t", &t, Op::Insert, &spec, &cols(), false);
        assert!(ins.contains("'old', NULL,"));
        assert!(ins.contains(r#"ST_AsGeoJSON(NEW."geo", 15, 4)::json AS "geo""#));
        assert!(ins.contains(r#"NEW."big""#) && !ins.contains("::text"));
        let del = trigger_function_body("t", &t, Op::Delete, &spec, &cols(), false);
        assert!(del.contains("'new', NULL"));
        assert_eq!(
            create_trigger_sql("t", Op::Delete, &t),
            r#"CREATE TRIGGER "notify_hasura_t_DELETE" AFTER DELETE ON "s"."T" FOR EACH ROW EXECUTE PROCEDURE hdb_catalog."notify_hasura_t_DELETE"()"#
        );
    }
}
