//! Parameters sent in Postgres *text* format, so any type (including user-defined enums,
//! domains and composites) can be bound without a Rust-side codec. The SQL casts each
//! parameter to its column type.

use bytes::BytesMut;
use postgres_types::{Format, IsNull, ToSql, Type, to_sql_checked};

#[derive(Debug)]
pub struct TextParam<'a>(pub Option<&'a str>);

impl ToSql for TextParam<'_> {
    fn to_sql(
        &self,
        _ty: &Type,
        out: &mut BytesMut,
    ) -> Result<IsNull, Box<dyn std::error::Error + Sync + Send>> {
        match self.0 {
            Some(s) => {
                out.extend_from_slice(s.as_bytes());
                Ok(IsNull::No)
            }
            None => Ok(IsNull::Yes),
        }
    }

    fn accepts(_ty: &Type) -> bool {
        true
    }

    fn encode_format(&self, _ty: &Type) -> Format {
        Format::Text
    }

    to_sql_checked!();
}
