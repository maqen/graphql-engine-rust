//! Catalog introspection of tracked tables (modelled on V2's `pg_table_metadata.sql`).

use std::collections::HashMap;

use hge_core::{
    ColumnInfo, ColumnType, EnumTableRow, Error, ForeignKey, FunctionArg, FunctionCatalog,
    QualifiedFunction, QualifiedTable, SourceCatalog, TableCatalog, TableKind, TableMutability,
    TypeKind, UniqueConstraint, Volatility,
};
use indexmap::IndexMap;
use serde::Deserialize;

/// Returns one JSON array (as text) describing every requested table that exists.
pub const TABLES_SQL: &str = r#"
SELECT coalesce(json_agg(json_build_object(
  'schema', n.nspname,
  'name', c.relname,
  'kind', c.relkind::text,
  'description', pg_catalog.obj_description(c.oid, 'pg_class'),
  'columns', (
    SELECT coalesce(json_agg(json_build_object(
      'name', a.attname,
      'type_schema', coalesce(et.ns, bt.ns, t.ns),
      'type_name', coalesce(et.typname, bt.typname, t.typname),
      'type_kind', coalesce(et.typtype, bt.typtype, t.typtype)::text,
      'dims', CASE WHEN et.typname IS NOT NULL THEN greatest(a.attndims, 1) ELSE 0 END,
      'nullable', NOT a.attnotnull,
      'has_default', a.atthasdef,
      'is_identity', a.attidentity <> '',
      'is_generated', a.attgenerated <> '',
      'is_identity_always', a.attidentity = 'a',
      'description', pg_catalog.col_description(c.oid, a.attnum)
    ) ORDER BY a.attnum), '[]')
    FROM pg_catalog.pg_attribute a
    JOIN (SELECT ty.oid, ty.typname, ty.typtype, ty.typelem, ty.typcategory, ty.typbasetype,
                 tn.nspname AS ns
            FROM pg_catalog.pg_type ty
            JOIN pg_catalog.pg_namespace tn ON tn.oid = ty.typnamespace) t
      ON t.oid = a.atttypid
    LEFT JOIN (SELECT ty.oid, ty.typname, ty.typtype, tn.nspname AS ns
                 FROM pg_catalog.pg_type ty
                 JOIN pg_catalog.pg_namespace tn ON tn.oid = ty.typnamespace) bt
      ON t.typtype = 'd' AND bt.oid = t.typbasetype
    LEFT JOIN (SELECT ty.oid, ty.typname, ty.typtype, tn.nspname AS ns
                 FROM pg_catalog.pg_type ty
                 JOIN pg_catalog.pg_namespace tn ON tn.oid = ty.typnamespace) et
      ON t.typcategory = 'A' AND t.typelem <> 0 AND et.oid = t.typelem
    WHERE a.attrelid = c.oid AND a.attnum > 0 AND NOT a.attisdropped
  ),
  'primary_key', (
    SELECT json_agg(a.attname ORDER BY array_position(idx.indkey::int2[], a.attnum))
    FROM pg_catalog.pg_index idx
    JOIN pg_catalog.pg_attribute a ON a.attrelid = c.oid AND a.attnum = ANY (idx.indkey)
    WHERE idx.indrelid = c.oid AND idx.indisprimary
  ),
  'unique_constraints', (
    SELECT coalesce(json_agg(cols), '[]') FROM (
      SELECT (SELECT json_agg(a.attname ORDER BY array_position(idx.indkey::int2[], a.attnum))
                FROM pg_catalog.pg_attribute a
               WHERE a.attrelid = c.oid AND a.attnum = ANY (idx.indkey)) AS cols
      FROM pg_catalog.pg_index idx
      WHERE idx.indrelid = c.oid AND idx.indisunique AND NOT idx.indisprimary
    ) u
  ),
  'constraints', (
    SELECT coalesce(json_agg(json_build_object(
      'name', ic.relname,
      'columns', (SELECT coalesce(json_agg(a.attname ORDER BY array_position(idx.indkey::int2[], a.attnum)), '[]')
                    FROM pg_catalog.pg_attribute a
                   WHERE a.attrelid = c.oid AND a.attnum = ANY (idx.indkey))
    ) ORDER BY ic.relname), '[]')
    FROM pg_catalog.pg_index idx
    JOIN pg_catalog.pg_class ic ON ic.oid = idx.indexrelid
    WHERE idx.indrelid = c.oid AND idx.indisunique
  ),
  'updatable', pg_catalog.pg_relation_is_updatable(c.oid::regclass, true),
  'foreign_keys', (
    SELECT coalesce(json_agg(json_build_object(
      'constraint', r.conname,
      'ref_schema', fn.nspname,
      'ref_name', fc.relname,
      'columns', (
        SELECT json_agg(json_build_array(la.attname, ra.attname) ORDER BY k.ord)
        FROM unnest(r.conkey, r.confkey) WITH ORDINALITY AS k(l, r, ord)
        JOIN pg_catalog.pg_attribute la ON la.attrelid = r.conrelid AND la.attnum = k.l
        JOIN pg_catalog.pg_attribute ra ON ra.attrelid = r.confrelid AND ra.attnum = k.r
      )
    )), '[]')
    FROM pg_catalog.pg_constraint r
    JOIN pg_catalog.pg_class fc ON fc.oid = r.confrelid
    JOIN pg_catalog.pg_namespace fn ON fn.oid = fc.relnamespace
    WHERE r.conrelid = c.oid AND r.contype = 'f'
  )
)), '[]')::text
FROM pg_catalog.pg_class c
JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
JOIN jsonb_to_recordset(($1)::jsonb) AS tracked("schema" text, "name" text)
  ON tracked."schema" = n.nspname AND tracked."name" = c.relname
WHERE c.relkind IN ('r', 'v', 'm', 'f', 'p')
"#;

/// Returns the labels of every enum type, as a JSON array (text).
pub const ENUMS_SQL: &str = r#"
SELECT coalesce(json_agg(json_build_object(
  'schema', n.nspname,
  'name', t.typname,
  'values', (SELECT json_agg(e.enumlabel ORDER BY e.enumsortorder)
               FROM pg_catalog.pg_enum e WHERE e.enumtypid = t.oid)
)), '[]')::text
FROM pg_catalog.pg_type t
JOIN pg_catalog.pg_namespace n ON n.oid = t.typnamespace
WHERE t.typtype = 'e'
"#;

/// Every overload of the requested functions (`$1`: `[{"schema", "name"}]`), as a JSON
/// array (text).
pub const FUNCTIONS_SQL: &str = r#"
WITH types AS (
  SELECT ty.oid,
         coalesce(et.ns, bt.ns, tn.nspname) AS type_schema,
         coalesce(et.typname, bt.typname, ty.typname) AS type_name,
         coalesce(et.typtype, bt.typtype, ty.typtype)::text AS type_kind,
         CASE WHEN et.typname IS NOT NULL THEN 1 ELSE 0 END AS dims
    FROM pg_catalog.pg_type ty
    JOIN pg_catalog.pg_namespace tn ON tn.oid = ty.typnamespace
    LEFT JOIN (SELECT t.oid, t.typname, t.typtype, n.nspname AS ns
                 FROM pg_catalog.pg_type t JOIN pg_catalog.pg_namespace n ON n.oid = t.typnamespace) bt
      ON ty.typtype = 'd' AND bt.oid = ty.typbasetype
    LEFT JOIN (SELECT t.oid, t.typname, t.typtype, n.nspname AS ns
                 FROM pg_catalog.pg_type t JOIN pg_catalog.pg_namespace n ON n.oid = t.typnamespace) et
      ON ty.typcategory = 'A' AND ty.typelem <> 0 AND et.oid = ty.typelem
)
SELECT coalesce(json_agg(json_build_object(
  'schema', n.nspname,
  'name', p.proname,
  'description', pg_catalog.obj_description(p.oid, 'pg_proc'),
  'args', (
    SELECT coalesce(json_agg(json_build_object(
      'name', nullif(a.name, ''),
      'type_schema', t.type_schema, 'type_name', t.type_name,
      'type_kind', t.type_kind, 'dims', t.dims,
      'has_default', a.ord > p.pronargs - p.pronargdefaults
    ) ORDER BY a.ord), '[]')
    FROM (
      SELECT row_number() OVER (ORDER BY all_args.ord) AS ord, all_args.type_oid,
             p.proargnames[all_args.ord] AS name
        FROM unnest(coalesce(p.proallargtypes, p.proargtypes::oid[]))
               WITH ORDINALITY AS all_args(type_oid, ord)
       WHERE p.proargmodes IS NULL OR p.proargmodes[all_args.ord] IN ('i', 'b', 'v')
    ) a
    JOIN types t ON t.oid = a.type_oid
  ),
  -- A domain result stays a domain (V2 rejects it as a computed field).
  'return_schema', CASE WHEN raw.typtype = 'd' THEN raw_ns.nspname ELSE rt.type_schema END,
  'return_name', CASE WHEN raw.typtype = 'd' THEN raw.typname ELSE rt.type_name END,
  'return_kind', CASE WHEN raw.typtype = 'd' THEN 'd' ELSE rt.type_kind END,
  'return_dims', rt.dims,
  'returns_set', p.proretset,
  'volatility', p.provolatile::text,
  'variadic', p.provariadic <> 0
)), '[]')::text
FROM pg_catalog.pg_proc p
JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace
JOIN types rt ON rt.oid = p.prorettype
JOIN pg_catalog.pg_type raw ON raw.oid = p.prorettype
JOIN pg_catalog.pg_namespace raw_ns ON raw_ns.oid = raw.typnamespace
JOIN jsonb_to_recordset(($1)::jsonb) AS wanted("schema" text, "name" text)
  ON wanted."schema" = n.nspname AND wanted."name" = p.proname
WHERE p.prokind = 'f'
"#;

#[derive(Deserialize)]
struct RawFunction {
    schema: String,
    name: String,
    description: Option<String>,
    args: Vec<RawArg>,
    return_schema: String,
    return_name: String,
    return_kind: String,
    return_dims: u32,
    returns_set: bool,
    volatility: String,
    variadic: bool,
}

#[derive(Deserialize)]
struct RawArg {
    name: Option<String>,
    type_schema: String,
    type_name: String,
    type_kind: String,
    dims: u32,
    has_default: bool,
}

#[derive(Deserialize)]
struct RawTable {
    schema: String,
    name: String,
    kind: String,
    #[serde(default)]
    description: Option<String>,
    columns: Vec<RawColumn>,
    primary_key: Option<Vec<String>>,
    unique_constraints: Vec<Option<Vec<String>>>,
    constraints: Vec<UniqueConstraint>,
    /// `pg_relation_is_updatable` bit mask.
    updatable: i32,
    foreign_keys: Vec<RawFk>,
}

#[derive(Deserialize)]
struct RawColumn {
    name: String,
    type_schema: String,
    type_name: String,
    type_kind: String,
    dims: u32,
    nullable: bool,
    has_default: bool,
    is_identity: bool,
    is_generated: bool,
    is_identity_always: bool,
    #[serde(default)]
    description: Option<String>,
}

#[derive(Deserialize)]
struct RawFk {
    constraint: String,
    ref_schema: String,
    ref_name: String,
    columns: Vec<(String, String)>,
}

#[derive(Deserialize)]
struct RawEnum {
    schema: String,
    name: String,
    values: Option<Vec<String>>,
}

/// The `$1` parameter of [`TABLES_SQL`].
pub fn tables_param(tables: &[QualifiedTable]) -> String {
    serde_json::to_string(tables).expect("serialisable")
}

pub(crate) fn type_kind(code: &str) -> TypeKind {
    match code {
        "b" => TypeKind::Base,
        "e" => TypeKind::Enum,
        "c" => TypeKind::Composite,
        "d" => TypeKind::Domain,
        "r" | "m" => TypeKind::Range,
        "p" => TypeKind::Pseudo,
        _ => TypeKind::Other,
    }
}

/// Parse the output of [`FUNCTIONS_SQL`].
pub fn parse_functions(
    functions_json: &str,
) -> Result<HashMap<QualifiedFunction, Vec<FunctionCatalog>>, Error> {
    let raw: Vec<RawFunction> = serde_json::from_str(functions_json)
        .map_err(|e| Error::unexpected(format!("could not parse function catalog: {e}")))?;
    let mut out: HashMap<QualifiedFunction, Vec<FunctionCatalog>> = HashMap::new();
    for f in raw {
        let name = QualifiedFunction::new(f.schema, f.name);
        let args = f
            .args
            .into_iter()
            .map(|a| FunctionArg {
                name: a.name,
                ty: ColumnType {
                    schema: a.type_schema,
                    name: a.type_name,
                    kind: type_kind(&a.type_kind),
                    dimensions: a.dims,
                },
                has_default: a.has_default,
            })
            .collect();
        out.entry(name.clone()).or_default().push(FunctionCatalog {
            name,
            description: f.description,
            args,
            return_type: ColumnType {
                schema: f.return_schema,
                name: f.return_name,
                kind: type_kind(&f.return_kind),
                dimensions: f.return_dims,
            },
            returns_set: f.returns_set,
            volatility: match f.volatility.as_str() {
                "i" => Volatility::Immutable,
                "s" => Volatility::Stable,
                _ => Volatility::Volatile,
            },
            variadic: f.variadic,
        });
    }
    Ok(out)
}

/// Parse the outputs of [`TABLES_SQL`] and [`ENUMS_SQL`].
pub fn parse_catalog(tables_json: &str, enums_json: &str) -> Result<SourceCatalog, Error> {
    let raw: Vec<RawTable> = serde_json::from_str(tables_json)
        .map_err(|e| Error::unexpected(format!("could not parse table catalog: {e}")))?;
    let raw_enums: Vec<RawEnum> = serde_json::from_str(enums_json)
        .map_err(|e| Error::unexpected(format!("could not parse enum catalog: {e}")))?;

    let mut tables = HashMap::new();
    for t in raw {
        let name = QualifiedTable::new(t.schema, t.name);
        let kind = match t.kind.as_str() {
            "v" => TableKind::View,
            "m" => TableKind::MaterializedView,
            "f" => TableKind::ForeignTable,
            "p" => TableKind::PartitionedTable,
            _ => TableKind::Table,
        };
        let columns: IndexMap<String, ColumnInfo> = t
            .columns
            .into_iter()
            .map(|c| {
                let kind = type_kind(&c.type_kind);
                (
                    c.name.clone(),
                    ColumnInfo {
                        name: c.name,
                        ty: ColumnType {
                            schema: c.type_schema,
                            name: c.type_name,
                            kind,
                            dimensions: c.dims,
                        },
                        nullable: c.nullable,
                        has_default: c.has_default,
                        is_identity: c.is_identity,
                        is_generated: c.is_generated,
                        is_identity_always: c.is_identity_always,
                        description: c.description,
                    },
                )
            })
            .collect();
        let foreign_keys = t
            .foreign_keys
            .into_iter()
            .map(|fk| ForeignKey {
                constraint: fk.constraint,
                ref_table: QualifiedTable::new(fk.ref_schema, fk.ref_name),
                columns: fk.columns,
            })
            .collect();
        tables.insert(
            name.clone(),
            TableCatalog {
                name,
                kind,
                description: t.description,
                columns,
                primary_key: t.primary_key,
                unique_constraints: t.unique_constraints.into_iter().flatten().collect(),
                foreign_keys,
                constraints: t.constraints,
                mutability: TableMutability {
                    insertable: t.updatable & 8 != 0,
                    updatable: t.updatable & 4 != 0,
                    deletable: t.updatable & 16 != 0,
                },
            },
        );
    }
    let enums = raw_enums
        .into_iter()
        .map(|e| ((e.schema, e.name), e.values.unwrap_or_default()))
        .collect();
    Ok(SourceCatalog {
        tables,
        enums,
        functions: HashMap::new(),
        ..Default::default()
    })
}

/// The query reading an enum table's rows (V2's `fetchEnumValuesFromDb`): the key and the
/// comment column (or `NULL`), both as text.
pub fn enum_rows_sql(table: &QualifiedTable, key: &str, comment: Option<&str>) -> String {
    let mut sql = "SELECT ".to_owned();
    crate::sql::push_ident(&mut sql, key);
    sql.push_str("::text, ");
    match comment {
        Some(c) => crate::sql::push_ident(&mut sql, c),
        None => sql.push_str("NULL"),
    }
    sql.push_str("::text FROM ");
    crate::sql::push_ident(&mut sql, &table.schema);
    sql.push('.');
    crate::sql::push_ident(&mut sql, &table.name);
    sql
}

/// V2's text for a failed enum table read (`defaultTxErrorHandler`): a transaction rollback
/// keeps Postgres's message (`sqlstate`, message); anything else, connection failures and
/// timeouts included, is "database query error".
pub fn enum_read_error(db: Option<(&str, &str)>) -> String {
    match db {
        Some((state, message)) if state.starts_with("40") => message.to_owned(),
        _ => "database query error".to_owned(),
    }
}

/// The rows returned by [`enum_rows_sql`].
pub fn enum_rows(rows: Vec<Vec<Option<String>>>) -> Vec<EnumTableRow> {
    rows.into_iter()
        .map(|mut r| EnumTableRow {
            comment: r.pop().flatten(),
            value: r.pop().flatten().unwrap_or_default(),
        })
        .collect()
}

#[cfg(test)]
mod enum_error_tests {
    use super::*;

    #[test]
    fn enum_read_errors_match_v2() {
        assert_eq!(
            enum_read_error(Some(("42501", "permission denied for table e"))),
            "database query error"
        );
        assert_eq!(enum_read_error(None), "database query error");
        assert_eq!(
            enum_read_error(Some(("40001", "could not serialize access"))),
            "could not serialize access"
        );
    }
}
