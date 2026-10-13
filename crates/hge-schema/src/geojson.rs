//! GeoJSON input for `geometry` / `geography` columns, validated with V2's rules and messages
//! (`Hasura.SQL.GeoJSON`). Valid input is sent to Postgres as GeoJSON text.

use serde_json::Value as Json;

/// Check `v` is a GeoJSON geometry V2 accepts; the error is V2's `parse-failed` message.
pub fn validate(v: &Json) -> Result<(), String> {
    geometry(v)
}

fn kind(v: &Json) -> &'static str {
    match v {
        Json::Null => "Null",
        Json::Bool(_) => "Boolean",
        Json::Number(_) => "Number",
        Json::String(_) => "String",
        Json::Array(_) => "Array",
        Json::Object(_) => "Object",
    }
}

/// aeson's `withArray` / `withObject` failure.
fn expected(name: &str, what: &str, v: &Json) -> String {
    format!(
        "parsing {name} failed, expected {what}, but encountered {}",
        kind(v)
    )
}

fn array<'a>(name: &str, v: &'a Json) -> Result<&'a Vec<Json>, String> {
    v.as_array().ok_or_else(|| expected(name, "Array", v))
}

fn geometry(v: &Json) -> Result<(), String> {
    let Json::Object(o) = v else {
        return Err(expected("Geometry", "Object", v));
    };
    let ty = match o.get("type") {
        Some(Json::String(t)) => t.as_str(),
        Some(other) => return Err(expected("Text", "String", other)),
        None => return Err("key \"type\" not found".to_owned()),
    };
    if ty == "GeometryCollection" {
        let geometries = o
            .get("geometries")
            .ok_or_else(|| "key \"geometries\" not found".to_owned())?;
        return array("[]", geometries)?.iter().try_for_each(geometry);
    }
    let check: fn(&Json) -> Result<(), String> = match ty {
        "Point" => position,
        "MultiPoint" => |c| array("[]", c)?.iter().try_for_each(position),
        "LineString" => line_string,
        "MultiLineString" => |c| array("[]", c)?.iter().try_for_each(line_string),
        "Polygon" => polygon,
        "MultiPolygon" => |c| array("[]", c)?.iter().try_for_each(polygon),
        other => return Err(format!("unexpected geometry type: {other}")),
    };
    let coordinates = o
        .get("coordinates")
        .ok_or_else(|| "key \"coordinates\" not found".to_owned())?;
    check(coordinates)
}

fn position(v: &Json) -> Result<(), String> {
    let items = array("Position", v)?;
    if items.len() < 2 {
        return Err("A Position needs at least 2 elements".to_owned());
    }
    // Elements past the third are ignored, as in V2.
    match items.iter().take(3).find(|n| !n.is_number()) {
        Some(bad) => Err(expected("Double", "Number", bad)),
        None => Ok(()),
    }
}

fn line_string(v: &Json) -> Result<(), String> {
    let items = array("LineString", v)?;
    if items.len() < 2 {
        return Err("A LineString needs at least 2 Positions".to_owned());
    }
    items.iter().try_for_each(position)
}

fn polygon(v: &Json) -> Result<(), String> {
    array("[]", v)?.iter().try_for_each(linear_ring)
}

fn linear_ring(v: &Json) -> Result<(), String> {
    let items = array("LinearRing", v)?;
    if items.len() < 4 {
        return Err("A LinearRing needs at least 4 Positions".to_owned());
    }
    items.iter().try_for_each(position)?;
    if items.first() != items.last() {
        return Err("the first and last locations have to be equal for a LinearRing".to_owned());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::validate;

    #[test]
    fn accepts_v2_geometries() {
        for g in [
            json!({"type": "Point", "coordinates": [1, 2]}),
            json!({"type": "Point", "coordinates": [1, 2, 3.5], "crs": {"type": "name"}}),
            json!({"type": "LineString", "coordinates": [[1, 2], [3, 4]]}),
            json!({"type": "Polygon", "coordinates": [[[0, 0], [1, 0], [1, 1], [0, 0]]]}),
            json!({"type": "GeometryCollection", "geometries": [
                {"type": "MultiPoint", "coordinates": [[1, 2]]}
            ]}),
        ] {
            assert_eq!(validate(&g), Ok(()), "{g}");
        }
    }

    #[test]
    fn rejects_with_v2_messages() {
        let cases = [
            (
                json!({"type": "Point", "coordinates": [1]}),
                "A Position needs at least 2 elements",
            ),
            (
                json!({"type": "LineString", "coordinates": [[1, 2]]}),
                "A LineString needs at least 2 Positions",
            ),
            (
                json!({"type": "Polygon", "coordinates": [[[0, 0], [1, 0], [0, 0]]]}),
                "A LinearRing needs at least 4 Positions",
            ),
            (
                json!({"type": "Polygon", "coordinates": [[[0, 0], [1, 0], [1, 1], [0, 1]]]}),
                "the first and last locations have to be equal for a LinearRing",
            ),
            (
                json!({"type": "Random", "coordinates": []}),
                "unexpected geometry type: Random",
            ),
        ];
        for (g, msg) in cases {
            assert_eq!(validate(&g), Err(msg.to_owned()), "{g}");
        }
    }
}
