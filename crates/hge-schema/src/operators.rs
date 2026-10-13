//! Comparison operators beyond the generic ones: `_cast` targets, the PostGIS predicates
//! (`_st_contains`, `_st_d_within`, …) of `geometry` and `geography` columns and the
//! `ltree` operators (`_ancestor`, `_matches`, …), for both the `<scalar>_comparison_exp`
//! schema types and lowering.

use hge_core::{ColumnType, Error};
use hge_ir::{CompareOp, DWithin, SpatialOp, Value};
use indexmap::IndexMap;
use serde_json::Value as Json;

use crate::lower::to_value;
use crate::model::{InputObjectDef, InputValueDef, Schema, TypeDef, TypeRef};
use crate::scalars;

/// `_cast` targets of a column type, by field name: jsonb to `String`, geometry to
/// `geography` and back.
pub fn cast_targets(ty: &ColumnType) -> Vec<(&'static str, ColumnType)> {
    if ty.is_array() {
        return vec![];
    }
    let sibling = |name: &str| ColumnType {
        name: name.to_owned(),
        ..ty.clone()
    };
    match ty.name.as_str() {
        "jsonb" => vec![("String", ColumnType::builtin("text"))],
        "geometry" => vec![("geography", sibling("geography"))],
        "geography" => vec![("geometry", sibling("geometry"))],
        _ => vec![],
    }
}

/// The target type of a permission filter's `_cast: {<type>: …}`, which names the Postgres
/// type (`text`, not GraphQL's `String`).
pub fn permission_cast_target(ty: &ColumnType, type_name: &str) -> Option<ColumnType> {
    cast_targets(ty)
        .into_iter()
        .map(|(_, t)| t)
        .find(|t| t.name == type_name)
}

/// The target type of `_cast: {<field>: …}` on a column of type `ty`.
pub fn cast_target(ty: &ColumnType, field: &str) -> Option<ColumnType> {
    cast_targets(ty)
        .into_iter()
        .find(|(name, _)| *name == field)
        .map(|(_, t)| t)
}

const GEOMETRY_OPS: [(&str, &str); 8] = [
    (
        "_st_3d_intersects",
        "does the column spatially intersect the given geometry value in 3D",
    ),
    (
        "_st_contains",
        "does the column contain the given geometry value",
    ),
    (
        "_st_crosses",
        "does the column cross the given geometry value",
    ),
    (
        "_st_equals",
        "is the column equal to given geometry value (directionality is ignored)",
    ),
    (
        "_st_intersects",
        "does the column spatially intersect the given geometry value",
    ),
    (
        "_st_overlaps",
        "does the column 'spatially overlap' (intersect but not completely contain) the given geometry value",
    ),
    (
        "_st_touches",
        "does the column have atleast one point in common with the given geometry value",
    ),
    (
        "_st_within",
        "is the column contained in the given geometry value",
    ),
];

/// The PostGIS operator fields of `<ty>_comparison_exp`, adding the input types they use.
pub fn spatial_fields(schema: &mut Schema, ty: &ColumnType) -> Vec<InputValueDef> {
    if !scalars::is_geo(ty) {
        return vec![];
    }
    let value = scalars::column_type_ref(ty, true);
    if ty.name == "geography" {
        let input = ensure_d_within_input(schema, ty);
        return vec![
            InputValueDef::new("_st_d_within", TypeRef::named(input))
                .describe("is the column within a given distance from the given geography value"),
            InputValueDef::new("_st_intersects", value)
                .describe("does the column spatially intersect the given geography value"),
        ];
    }
    let input = ensure_d_within_input(schema, ty);
    let mut fields: Vec<InputValueDef> = GEOMETRY_OPS
        .iter()
        .map(|(op, desc)| InputValueDef::new(*op, value.clone()).describe(*desc))
        .collect();
    fields.push(
        InputValueDef::new("_st_3d_d_within", TypeRef::named(input.clone()))
            .describe("is the column within a given 3D distance from the given geometry value"),
    );
    fields.push(
        InputValueDef::new("_st_d_within", TypeRef::named(input))
            .describe("is the column within a given distance from the given geometry value"),
    );
    fields
}

/// `st_d_within_input` / `st_d_within_geography_input`.
fn ensure_d_within_input(schema: &mut Schema, ty: &ColumnType) -> String {
    let geography = ty.name == "geography";
    let name = if geography {
        "st_d_within_geography_input"
    } else {
        "st_d_within_input"
    };
    if !schema.types.contains_key(name) {
        let mut fields = IndexMap::new();
        let mut add = |f: InputValueDef| {
            fields.insert(f.name.clone(), f);
        };
        add(InputValueDef::new("distance", TypeRef::nn("Float")));
        add(InputValueDef::new(
            "from",
            scalars::column_type_ref(ty, false),
        ));
        if geography {
            let mut f = InputValueDef::new("use_spheroid", TypeRef::named("Boolean"));
            f.default_value = Some("true".to_owned());
            add(f);
        }
        schema.add(TypeDef::InputObject(InputObjectDef {
            name: name.to_owned(),
            description: None,
            fields,
        }));
    }
    name.to_owned()
}

/// Lower a PostGIS operator (`op` without its `_` or `$` prefix) on a column of type `ty`.
/// `None` if `op` is not one; `value` turns an operand into a [`Value`] of the given type.
pub fn spatial_op(
    op: &str,
    v: &Json,
    ty: &ColumnType,
    path: &str,
    value: &dyn Fn(&Json, &ColumnType, &str) -> Result<Value, Error>,
) -> Option<Result<CompareOp, Error>> {
    if !scalars::is_geo(ty) {
        return None;
    }
    let f = SpatialOp::from_name(op);
    let three_d = match op {
        _ if f.is_some() => false,
        "st_d_within" => false,
        "st_3d_d_within" => true,
        _ => return None,
    };
    // Only `st_intersects` and `st_d_within` exist for geography.
    if ty.name == "geography" && !matches!(op, "st_intersects" | "st_d_within") {
        return Some(Err(Error::validation(format!(
            "the operator _{op} is only supported on geometry columns"
        ))
        .with_path(path)));
    }
    if let Some(f) = f {
        return Some(value(v, ty, path).map(|v| CompareOp::Spatial(f, v)));
    }
    Some(d_within(three_d, v, ty, path, value))
}

fn d_within(
    three_d: bool,
    v: &Json,
    ty: &ColumnType,
    path: &str,
    value: &dyn Fn(&Json, &ColumnType, &str) -> Result<Value, Error>,
) -> Result<CompareOp, Error> {
    let obj = v
        .as_object()
        .ok_or_else(|| Error::validation("expected an object").with_path(path))?;
    let field = |name: &str| {
        obj.get(name).filter(|v| !v.is_null()).ok_or_else(|| {
            Error::validation(format!("missing required field '{name}'")).with_path(path)
        })
    };
    let geography = ty.name == "geography";
    Ok(CompareOp::DWithin(Box::new(DWithin {
        three_d,
        from: value(field("from")?, ty, &format!("{path}.from"))?,
        distance: value(
            field("distance")?,
            &ColumnType::builtin("float8"),
            &format!("{path}.distance"),
        )?,
        // V2 defaults an absent `use_spheroid` to true for geography (an explicit null stays
        // NULL, so `ST_DWithin` matches nothing).
        use_spheroid: match obj.get("use_spheroid") {
            _ if !geography => None,
            None => Some(Value::literal("true", ColumnType::builtin("bool"))),
            Some(s) => Some(value(
                s,
                &ColumnType::builtin("bool"),
                &format!("{path}.use_spheroid"),
            )?),
        },
    })))
}

/// [`spatial_op`] for GraphQL `where` input.
pub fn graphql_spatial_op(
    op: &str,
    v: &Json,
    ty: &ColumnType,
    path: &str,
) -> Option<Result<CompareOp, Error>> {
    spatial_op(op.strip_prefix('_')?, v, ty, path, &to_value)
}

fn is_ltree(ty: &ColumnType) -> bool {
    !ty.is_array() && ty.name == "ltree"
}

/// A type from the same schema as `ty` (the ltree extension's `lquery` and `ltxtquery`).
fn sibling(ty: &ColumnType, name: &str) -> ColumnType {
    ColumnType {
        name: name.to_owned(),
        dimensions: 0,
        ..ty.clone()
    }
}

/// Scalars the ltree operators of `ty` take besides `ty` itself.
pub fn ltree_scalars(ty: &ColumnType) -> Vec<ColumnType> {
    if !is_ltree(ty) {
        return vec![];
    }
    vec![sibling(ty, "lquery"), sibling(ty, "ltxtquery")]
}

/// The ltree operator fields of `ltree_comparison_exp`.
pub fn ltree_fields(ty: &ColumnType) -> Vec<InputValueDef> {
    if !is_ltree(ty) {
        return vec![];
    }
    let ltree = scalars::column_type_ref(ty, true);
    let ltree_list = scalars::column_type_ref(ty, false).list();
    vec![
        InputValueDef::new("_ancestor", ltree.clone())
            .describe("is the left argument an ancestor of right (or equal)?"),
        InputValueDef::new("_ancestor_any", ltree_list.clone())
            .describe("does array contain an ancestor of `ltree`?"),
        InputValueDef::new("_descendant", ltree)
            .describe("is the left argument a descendant of right (or equal)?"),
        InputValueDef::new("_descendant_any", ltree_list)
            .describe("does array contain a descendant of `ltree`?"),
        InputValueDef::new("_matches", TypeRef::named("lquery"))
            .describe("does `ltree` match `lquery`?"),
        InputValueDef::new("_matches_any", TypeRef::nn("String").list())
            .describe("does `ltree` match any `lquery` in `lquery[]`?"),
        InputValueDef::new("_matches_fulltext", TypeRef::named("ltxtquery"))
            .describe("does `ltree` match `ltxtquery`?"),
    ]
}

/// Lower an ltree operator (`op` without its `_` or `$` prefix) on a column of type `ty`.
/// `None` if `op` is not one or `ty` is not `ltree`; `value` as for [`spatial_op`].
pub fn ltree_op(
    op: &str,
    v: &Json,
    ty: &ColumnType,
    path: &str,
    value: &dyn Fn(&Json, &ColumnType, &str) -> Result<Value, Error>,
) -> Option<Result<CompareOp, Error>> {
    if !is_ltree(ty) {
        return None;
    }
    // V2 renders the ltree operators with the generic `@>`, `<@`, `~` and `?`.
    let (f, arg_ty): (fn(Value) -> CompareOp, ColumnType) = match op {
        "ancestor" => (CompareOp::Contains, ty.clone()),
        "ancestor_any" => (CompareOp::Contains, ty.array_of()),
        "descendant" => (CompareOp::ContainedIn, ty.clone()),
        "descendant_any" => (CompareOp::ContainedIn, ty.array_of()),
        "matches" => (CompareOp::Regex, sibling(ty, "lquery")),
        "matches_any" => (CompareOp::HasKey, sibling(ty, "lquery").array_of()),
        "matches_fulltext" => (CompareOp::MatchesFulltext, sibling(ty, "ltxtquery")),
        _ => return None,
    };
    Some(value(v, &arg_ty, path).map(f))
}

/// [`ltree_op`] for GraphQL `where` input.
pub fn graphql_ltree_op(
    op: &str,
    v: &Json,
    ty: &ColumnType,
    path: &str,
) -> Option<Result<CompareOp, Error>> {
    ltree_op(op.strip_prefix('_')?, v, ty, path, &to_value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn geo(name: &str) -> ColumnType {
        ColumnType {
            schema: "public".into(),
            ..ColumnType::builtin(name)
        }
    }

    #[test]
    fn cast_targets_by_type() {
        assert_eq!(
            cast_target(&geo("geometry"), "geography"),
            Some(geo("geography"))
        );
        assert_eq!(
            cast_target(&geo("geography"), "geometry"),
            Some(geo("geometry"))
        );
        assert_eq!(
            cast_target(&ColumnType::builtin("jsonb"), "String"),
            Some(ColumnType::builtin("text"))
        );
        assert_eq!(cast_target(&geo("geometry"), "String"), None);
        assert!(cast_targets(&ColumnType::builtin("int4")).is_empty());
    }

    #[test]
    fn permission_casts_name_postgres_types() {
        let jsonb = ColumnType::builtin("jsonb");
        assert_eq!(
            permission_cast_target(&jsonb, "text"),
            Some(ColumnType::builtin("text"))
        );
        assert_eq!(permission_cast_target(&jsonb, "String"), None);
        assert_eq!(
            permission_cast_target(&geo("geometry"), "geography"),
            Some(geo("geography"))
        );
    }

    #[test]
    fn geography_has_only_intersects_and_d_within() {
        let point = json!({"type": "Point", "coordinates": [1, 2]});
        assert!(matches!(
            graphql_spatial_op("_st_contains", &point, &geo("geography"), "$"),
            Some(Err(_))
        ));
        assert!(matches!(
            graphql_spatial_op("_st_intersects", &point, &geo("geography"), "$"),
            Some(Ok(_))
        ));
    }

    #[test]
    fn spatial_operators() {
        let point = json!({"type": "Point", "coordinates": [1, 2]});
        let op = graphql_spatial_op("_st_within", &point, &geo("geometry"), "$").unwrap();
        assert!(matches!(op, Ok(CompareOp::Spatial(SpatialOp::Within, _))));
        // Not a PostGIS operator, or not a geo column: left to the generic operators.
        assert!(graphql_spatial_op("_eq", &point, &geo("geometry"), "$").is_none());
        assert!(
            graphql_spatial_op("_st_within", &point, &ColumnType::builtin("text"), "$").is_none()
        );
    }

    #[test]
    fn d_within_defaults_use_spheroid_for_geography_only() {
        let arg = json!({"distance": 10, "from": {"type": "Point", "coordinates": [1, 2]}});
        let Some(Ok(CompareOp::DWithin(d))) =
            graphql_spatial_op("_st_d_within", &arg, &geo("geography"), "$")
        else {
            panic!("expected DWithin");
        };
        assert_eq!(
            d.use_spheroid,
            Some(Value::literal("true", ColumnType::builtin("bool")))
        );
        assert!(!d.three_d);
        let Some(Ok(CompareOp::DWithin(d))) =
            graphql_spatial_op("_st_3d_d_within", &arg, &geo("geometry"), "$")
        else {
            panic!("expected DWithin");
        };
        assert_eq!(d.use_spheroid, None);
        assert!(d.three_d);
        let explicit_null = json!({"distance": 1, "from": {"type": "Point", "coordinates": [1, 2]}, "use_spheroid": null});
        let Some(Ok(CompareOp::DWithin(d))) =
            graphql_spatial_op("_st_d_within", &explicit_null, &geo("geography"), "$")
        else {
            panic!("expected DWithin");
        };
        assert_eq!(
            d.use_spheroid,
            Some(Value::null(ColumnType::builtin("bool")))
        );
        let missing = graphql_spatial_op(
            "_st_d_within",
            &json!({"distance": 1}),
            &geo("geometry"),
            "$",
        );
        assert!(matches!(missing, Some(Err(_))));
    }

    #[test]
    fn ltree_operators() {
        let ltree = geo("ltree");
        let op = graphql_ltree_op("_ancestor_any", &json!(["A.B", "C"]), &ltree, "$.p");
        let Some(Ok(CompareOp::Contains(v))) = op else {
            panic!("expected Contains, got {op:?}");
        };
        assert_eq!(v.ty(), &ltree.array_of());
        let Some(Ok(CompareOp::Regex(v))) =
            graphql_ltree_op("_matches", &json!("*.A.*"), &ltree, "$")
        else {
            panic!("expected Regex");
        };
        assert_eq!(v.ty().name, "lquery");
        assert!(matches!(
            graphql_ltree_op("_matches_fulltext", &json!("a & b"), &ltree, "$"),
            Some(Ok(CompareOp::MatchesFulltext(_)))
        ));
        // Not an ltree column, or not an ltree operator.
        assert!(
            graphql_ltree_op("_ancestor", &json!("A"), &ColumnType::builtin("text"), "$").is_none()
        );
        assert!(graphql_ltree_op("_eq", &json!("A"), &ltree, "$").is_none());
        // ltree arguments are checked as label paths (by `to_value`); lquery ones are not.
        let Some(Err(e)) = graphql_ltree_op("_descendant_any", &json!(["A", "B."]), &ltree, "$.p")
        else {
            panic!("expected an error");
        };
        assert_eq!(e.path, "$.p[1]");
        assert!(matches!(
            graphql_ltree_op("_matches", &json!("A.*{1,}"), &ltree, "$"),
            Some(Ok(_))
        ));
    }
}
