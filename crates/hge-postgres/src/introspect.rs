//! Catalog introspection of tracked tables (modelled on V2's `pg_table_metadata.sql`).

use std::collections::HashMap;

use hge_core::{
    ColumnInfo, ColumnType, Error, ForeignKey, QualifiedTable, SourceCatalog, TableCatalog,
    TableKind, TableMutability, TypeKind, UniqueConstraint,
};
use indexmap::IndexMap;
use serde::Deserialize;

/// Returns one JSON array (as text) describing every requested table that exists.
pub const TABLES_SQL: &str = r#"
SELECT coalesce(json_agg(json_build_object(
  'schema', n.nspname,
  'name', c.relname,
  'kind', c.relkind::text,
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
      'is_identity_always', a.attidentity = 'a'
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

#[derive(Deserialize)]
struct RawTable {
    schema: String,
    name: String,
    kind: String,
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
                let kind = match c.type_kind.as_str() {
                    "b" => TypeKind::Base,
                    "e" => TypeKind::Enum,
                    "c" => TypeKind::Composite,
                    "d" => TypeKind::Domain,
                    "r" | "m" => TypeKind::Range,
                    "p" => TypeKind::Pseudo,
                    _ => TypeKind::Other,
                };
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
                        description: None,
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
                description: None,
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
    Ok(SourceCatalog { tables, enums })
}
