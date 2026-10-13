//! Names of database objects.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize};

/// A schema-qualified table (or view / function) name.
///
/// Deserialises from either `{"schema": "s", "name": "t"}` or a bare `"t"` (schema `public`),
/// the two forms V2 metadata accepts.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
pub struct QualifiedTable {
    pub schema: String,
    pub name: String,
}

/// A schema-qualified function name; same forms as a table name.
pub type QualifiedFunction = QualifiedTable;

impl QualifiedTable {
    pub fn new(schema: impl Into<String>, name: impl Into<String>) -> Self {
        QualifiedTable {
            schema: schema.into(),
            name: name.into(),
        }
    }

    pub fn public(name: impl Into<String>) -> Self {
        Self::new("public", name)
    }

    /// V2's text form: `name` for tables in `public`, else `schema.name`.
    pub fn v2_text(&self) -> String {
        if self.schema == "public" {
            self.name.clone()
        } else {
            format!("{}.{}", self.schema, self.name)
        }
    }
}

impl fmt::Display for QualifiedTable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.schema, self.name)
    }
}

impl<'de> Deserialize<'de> for QualifiedTable {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Repr {
            Name(String),
            Qualified {
                #[serde(default = "public")]
                schema: String,
                name: String,
            },
        }
        fn public() -> String {
            "public".to_owned()
        }
        Ok(match Repr::deserialize(d)? {
            Repr::Name(name) => QualifiedTable::public(name),
            Repr::Qualified { schema, name } => QualifiedTable { schema, name },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserialises_both_forms() {
        let a: QualifiedTable = serde_json::from_str(r#""author""#).unwrap();
        let b: QualifiedTable =
            serde_json::from_str(r#"{"schema":"public","name":"author"}"#).unwrap();
        let c: QualifiedTable = serde_json::from_str(r#"{"name":"author"}"#).unwrap();
        assert_eq!(a, b);
        assert_eq!(b, c);
    }
}
