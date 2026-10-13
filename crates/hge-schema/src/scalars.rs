//! Postgres type → GraphQL scalar mapping (V2 rules, see `mkScalarTypeName` in V2's
//! `Hasura.Backends.Postgres.SQL.Types`).

use hge_core::{ColumnType, TypeKind};

use crate::model::TypeRef;

/// Built-in GraphQL scalars, always present in the schema.
pub const BUILTIN_SCALARS: &[&str] = &["Boolean", "Float", "ID", "Int", "String"];

/// Canonical V2 name of a Postgres type, resolving aliases (`int4` → `integer`, …).
pub(crate) fn canonical(name: &str) -> &str {
    match name {
        "int2" | "smallint" => "smallint",
        "int4" | "int" | "integer" | "serial" => "integer",
        "int8" | "bigint" | "bigserial" => "bigint",
        "float4" | "real" => "real",
        "float8" | "double precision" => "float8",
        "numeric" | "decimal" => "numeric",
        "bool" | "boolean" => "boolean",
        "bpchar" | "char" | "character" => "bpchar",
        "varchar" | "character varying" => "varchar",
        "timestamp without time zone" => "timestamp",
        "timestamp with time zone" => "timestamptz",
        "time with time zone" => "timetz",
        other => other,
    }
}

/// The GraphQL scalar name for a (non-array) Postgres type.
pub fn scalar_name(ty: &ColumnType) -> String {
    match ty.kind {
        TypeKind::Composite => return format!("{}_scalar", ty.name),
        TypeKind::Enum => return ty.name.clone(),
        _ => {}
    }
    match canonical(&ty.name) {
        "integer" => "Int".to_owned(),
        "boolean" => "Boolean".to_owned(),
        "real" => "Float".to_owned(),
        "text" | "varchar" => "String".to_owned(),
        other => other.to_owned(),
    }
}

/// The GraphQL type of a column's value (V2 renders arrays as lists: `[String!]`).
pub fn column_type_ref(ty: &ColumnType, nullable: bool) -> TypeRef {
    let mut t = TypeRef::nn(scalar_name(&ty.element()));
    for _ in 0..ty.dimensions {
        t = t.list().non_null();
    }
    if nullable { t.nullable().clone() } else { t }
}

/// Name of the comparison expression type for a column type.
pub fn comparison_exp_name(ty: &ColumnType) -> String {
    if ty.is_array() {
        format!("{}_array_comparison_exp", scalar_name(&ty.element()))
    } else {
        format!("{}_comparison_exp", scalar_name(ty))
    }
}

pub fn is_string_type(ty: &ColumnType) -> bool {
    !ty.is_array()
        && ty.kind != TypeKind::Enum
        && matches!(
            canonical(&ty.name),
            "text" | "varchar" | "citext" | "bpchar"
        )
}

pub fn is_jsonb(ty: &ColumnType) -> bool {
    !ty.is_array() && ty.name == "jsonb"
}

pub fn is_json_like(ty: &ColumnType) -> bool {
    !ty.is_array() && matches!(ty.name.as_str(), "json" | "jsonb")
}

pub fn is_geo(ty: &ColumnType) -> bool {
    !ty.is_array() && matches!(ty.name.as_str(), "geometry" | "geography")
}

/// Types V2 serialises as strings under `HASURA_GRAPHQL_STRINGIFY_NUMERIC_TYPES`.
pub fn is_big_num(ty: &ColumnType) -> bool {
    !ty.is_array()
        && matches!(
            canonical(&ty.name),
            "bigint" | "numeric" | "float8" | "money"
        )
}

/// Numeric types (V2's `isNumType`): the columns `sum`, `avg`, `stddev`, … apply to.
pub fn is_numeric(ty: &ColumnType) -> bool {
    !ty.is_array()
        && matches!(ty.kind, TypeKind::Base | TypeKind::Domain)
        && matches!(
            canonical(&ty.name),
            "smallint" | "integer" | "bigint" | "real" | "float8" | "numeric" | "money"
        )
}

/// Types `max` and `min` apply to (V2's `isComparableType`): every type V2 knows by name
/// except json, jsonb, geometry, geography and boolean, plus native enums, composites and
/// arrays. Types V2 does not know (`time`, `interval`, `inet`, ranges, …) are excluded.
pub fn is_comparable(ty: &ColumnType) -> bool {
    if ty.is_array() || matches!(ty.kind, TypeKind::Enum | TypeKind::Composite) {
        return true;
    }
    matches!(ty.kind, TypeKind::Base | TypeKind::Domain)
        && matches!(
            canonical(&ty.name),
            "smallint"
                | "integer"
                | "bigint"
                | "serial"
                | "bigserial"
                | "real"
                | "float8"
                | "numeric"
                | "money"
                | "bpchar"
                | "varchar"
                | "text"
                | "citext"
                | "date"
                | "timestamp"
                | "timestamptz"
                | "timetz"
                | "raster"
                | "uuid"
                | "ltree"
                | "lquery"
                | "ltxtquery"
        )
}

/// Whether a column has exactly the Postgres type `name` (aggregation predicate arguments
/// accept only `float8` or `bool` columns).
pub fn is_exactly(ty: &ColumnType, name: &str) -> bool {
    !ty.is_array()
        && matches!(ty.kind, TypeKind::Base | TypeKind::Domain)
        && canonical(&ty.name) == canonical(name)
}

/// GraphQL built-in scalars whose input values we validate strictly.
pub fn builtin_kind(name: &str) -> Option<BuiltinScalar> {
    Some(match name {
        "Int" => BuiltinScalar::Int,
        "Float" => BuiltinScalar::Float,
        "String" => BuiltinScalar::String,
        "Boolean" => BuiltinScalar::Boolean,
        "ID" => BuiltinScalar::Id,
        _ => return None,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuiltinScalar {
    Int,
    Float,
    String,
    Boolean,
    Id,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_like_v2() {
        let t = |n: &str| scalar_name(&ColumnType::builtin(n));
        assert_eq!(t("int4"), "Int");
        assert_eq!(t("int8"), "bigint");
        assert_eq!(t("int2"), "smallint");
        assert_eq!(t("float8"), "float8");
        assert_eq!(t("float4"), "Float");
        assert_eq!(t("varchar"), "String");
        assert_eq!(t("bpchar"), "bpchar");
        assert_eq!(t("timestamptz"), "timestamptz");
        let arr = ColumnType::builtin("text").array_of();
        assert_eq!(column_type_ref(&arr, true).to_string(), "[String!]");
        assert_eq!(column_type_ref(&arr, false).to_string(), "[String!]!");
        assert_eq!(comparison_exp_name(&arr), "String_array_comparison_exp");
    }

    #[test]
    fn aggregate_applicability_like_v2() {
        let t = ColumnType::builtin;
        assert!(is_numeric(&t("int4")) && is_numeric(&t("numeric")) && is_numeric(&t("money")));
        assert!(!is_numeric(&t("text")) && !is_numeric(&t("int4").array_of()));
        assert!(is_comparable(&t("text")) && is_comparable(&t("timestamptz")));
        assert!(is_comparable(&t("jsonb").array_of()));
        for name in [
            "bool", "json", "jsonb", "geometry", "time", "interval", "inet",
        ] {
            assert!(!is_comparable(&t(name)), "{name}");
        }
        assert!(is_exactly(&t("double precision"), "float8"));
        assert!(!is_exactly(&t("numeric"), "float8"));
    }
}
