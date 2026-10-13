//! The Kriti template language (v0.3.3), as used by Hasura webhook request/response
//! transforms.
//!
//! Kriti is a superset of JSON: values may embed `{{ expr }}`, strings interpolate
//! `"a{{ e }}b"`, and `{{ range … }}` / `{{ if … }}` are values. This crate is IO-free:
//! parse once with [`Template::parse`], then [`Template::eval`] against [`Bindings`] and
//! [`Functions`].
//!
//! ```
//! use hge_kriti::{Bindings, Functions, Template};
//! use serde_json::json;
//!
//! let t = Template::parse_string_template("{{$base_url}}/users/{{$body.input.id}}").unwrap();
//! let vars = Bindings::new()
//!     .with("$base_url", json!("http://api"))
//!     .with("$body", json!({"input": {"id": 7}}));
//! assert_eq!(t.eval(&vars, &Functions::new()).unwrap(), json!("http://api/users/7"));
//! ```
//!
//! Semantics follow the Haskell implementation (pinned kriti-lang `c8fd863`), including
//! its error messages. Known limits of the `serde_json` value model (no arbitrary
//! precision): integers beyond `u64`/`i64` and decimals are held as `f64`, so very large or
//! very precise numbers lose precision, and number literals overflowing `f64` are rejected.
//! Output objects have sorted keys (aeson's Map-based objects); interpolated non-string
//! values use aeson's compact encoding with sorted keys.

mod ast;
mod error;
mod eval;
mod functions;
mod lexer;
mod parser;
mod value;

use serde_json::Value;
use std::collections::HashMap;

pub use error::{ErrorCode, KritiError, Position, Span};
pub use functions::{CustomFn, Functions, get_session_variable_fn};

/// A parsed Kriti template.
#[derive(Debug, Clone, PartialEq)]
pub struct Template {
    source: String,
    expr: ast::Expr,
}

impl Template {
    /// Parses a Kriti program.
    pub fn parse(src: &str) -> Result<Template, KritiError> {
        Ok(Template {
            source: src.to_owned(),
            expr: parser::parse(src)?,
        })
    }

    /// Parses an "unescaped" string template (urls, headers, query params, form fields):
    /// the program `"` + `src` + `"`, which always evaluates to a string. Literal `"` and
    /// `\` must be escaped Kriti-style, and error columns are shifted by one.
    pub fn parse_string_template(src: &str) -> Result<Template, KritiError> {
        let program = format!("\"{src}\"");
        Ok(Template {
            expr: parser::parse(&program)?,
            source: program,
        })
    }

    /// The source text that was parsed.
    pub fn source(&self) -> &str {
        &self.source
    }

    /// Evaluates the template. Objects in the result have sorted keys.
    pub fn eval(&self, vars: &Bindings, funcs: &Functions) -> Result<Value, KritiError> {
        let mut v = eval::eval(&self.expr, vars, funcs)?;
        value::sort_keys(&mut v);
        Ok(v)
    }
}

/// Variables in scope, by name including the `$` (e.g. `$body`, `$session_variables`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Bindings {
    vars: HashMap<String, Value>,
}

impl Bindings {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, name: impl Into<String>, value: Value) -> &mut Self {
        self.vars.insert(name.into(), value);
        self
    }

    /// Builder form of [`Bindings::insert`].
    pub fn with(mut self, name: impl Into<String>, value: Value) -> Self {
        self.insert(name, value);
        self
    }

    pub fn get(&self, name: &str) -> Option<&Value> {
        self.vars.get(name)
    }
}

impl<K: Into<String>> FromIterator<(K, Value)> for Bindings {
    fn from_iter<I: IntoIterator<Item = (K, Value)>>(iter: I) -> Self {
        Self {
            vars: iter.into_iter().map(|(k, v)| (k.into(), v)).collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn run(src: &str, vars: &Bindings) -> Result<Value, KritiError> {
        Template::parse(src)?.eval(vars, &Functions::new())
    }

    fn body(v: Value) -> Bindings {
        Bindings::new().with("$body", v)
    }

    fn cols(e: &KritiError) -> (usize, usize, usize, usize) {
        (
            e.span.start.line,
            e.span.start.column,
            e.span.end.line,
            e.span.end.column,
        )
    }

    #[test]
    fn attribute_error_json_and_span() {
        let e = run("{{ $body.world }}", &body(json!({"hello": 1}))).unwrap_err();
        assert_eq!(
            e.to_json(),
            json!({
                "error_code": "Attribute Error",
                "message": "'Object' has no attritubte 'world'.",
                "source_position": {"start_line": 0, "start_column": 4, "end_line": 0, "end_column": 15}
            })
        );
        assert_eq!(
            e.to_json().to_string(),
            r#"{"error_code":"Attribute Error","message":"'Object' has no attritubte 'world'.","source_position":{"end_column":15,"end_line":0,"start_column":4,"start_line":0}}"#
        );
    }

    #[test]
    fn name_and_type_errors() {
        let e = run("{{ $nope }}", &Bindings::new()).unwrap_err();
        assert_eq!(e.error_code, ErrorCode::Name);
        assert_eq!(e.message, "Variable '$nope' not in scope");
        assert_eq!(cols(&e), (0, 4, 0, 9));

        let e = run("{{ $body.a }}", &body(json!([1]))).unwrap_err();
        assert_eq!(e.error_code, ErrorCode::Type);
        assert_eq!(
            e.message,
            "Couldn't match expected type 'Object' with actual type 'Array'"
        );
        assert_eq!(cols(&e), (0, 4, 0, 9));

        let e = run("{{ if 1 }} 1 {{ else }} 2 {{ end }}", &Bindings::new()).unwrap_err();
        assert_eq!(
            e.message,
            "Couldn't match expected type 'Boolean' with actual type 'Number'"
        );

        let e = run("{{ range x, y := 1 }} 1 {{ end }}", &Bindings::new()).unwrap_err();
        assert_eq!(
            e.message,
            "Couldn't match expected type 'Array' with actual type 'Number'"
        );
        assert_eq!(cols(&e), (0, 1, 0, 34));

        let e = run("{{ (1 && true) }}", &Bindings::new()).unwrap_err();
        assert_eq!(
            e.message,
            "Couldn't match expected type 'Boolean' with actual type 'Number'"
        );
        let e = run("{{ 1 in {} }}", &Bindings::new()).unwrap_err();
        assert_eq!(
            e.message,
            "Couldn't match expected type 'String' with actual type 'Number'"
        );
        let e = run("{{ 1 in 2 }}", &Bindings::new()).unwrap_err();
        assert_eq!(
            e.message,
            "Couldn't match expected type 'Array' with actual type 'Number'"
        );
    }

    #[test]
    fn index_errors() {
        let e = run("{{ $body[5] }}", &body(json!([1]))).unwrap_err();
        assert_eq!(e.error_code, ErrorCode::Index);
        assert_eq!(e.message, "Can only range over an array");
        assert_eq!(cols(&e), (0, 4, 0, 12));
        let e = run("{{ $body[-1] }}", &body(json!([1]))).unwrap_err();
        assert_eq!(e.error_code, ErrorCode::Index);
        let e = run("{{ $body[1.5] }}", &body(json!([1, 2]))).unwrap_err();
        assert_eq!(
            e.message,
            "Couldn't match expected type 'Integer' with actual type 'Number'"
        );
        let e = run("{{ $body[1] }}", &body(json!({"1": 1}))).unwrap_err();
        assert_eq!(
            e.message,
            "Couldn't match expected type 'String' with actual type 'Number'"
        );
        assert_eq!(run("{{ $body[1.0] }}", &body(json!([1, 2]))), Ok(json!(2)));
        assert_eq!(
            run("{{ $body['a b'] }}", &body(json!({"a b": 3}))),
            Ok(json!(3))
        );
    }

    #[test]
    fn function_errors_use_argument_span() {
        let funcs = Functions::new().with("getSessionVariable", get_session_variable_fn(None));
        let t = Template::parse(r#"{{ getSessionVariable("x-hasura-user-id") }}"#).unwrap();
        let e = t.eval(&Bindings::new(), &funcs).unwrap_err();
        assert_eq!(e.error_code, ErrorCode::Function);
        assert_eq!(e.message, "Session variable \"x-hasura-user-id\" not found");
        assert_eq!(cols(&e), (0, 23, 0, 41));

        let e = run("{{ nope(1) }}", &Bindings::new()).unwrap_err();
        assert_eq!(e.error_code, ErrorCode::Name);
        assert_eq!(e.message, "Variable 'nope' not in scope");
        assert_eq!(cols(&e), (0, 4, 0, 11));

        // The argument is evaluated first, so its error wins over the unknown function.
        let e = run("{{ nope($x) }}", &Bindings::new()).unwrap_err();
        assert_eq!(e.message, "Variable '$x' not in scope");
    }

    #[test]
    fn get_session_variable_in_template() {
        let session = json!({"x-hasura-user-id": "42"});
        let funcs = Functions::new().with(
            "getSessionVariable",
            get_session_variable_fn(session.as_object().cloned()),
        );
        let t =
            Template::parse_string_template(r#"Bearer {{getSessionVariable("X-Hasura-User-Id")}}"#)
                .unwrap();
        assert_eq!(t.eval(&Bindings::new(), &funcs), Ok(json!("Bearer 42")));
    }

    #[test]
    fn optional_chaining() {
        let vars = body(json!({"a": {"b": [10, 20]}, "s": "str"}));
        assert_eq!(run("{{ $body?.a.b[1] }}", &vars), Ok(json!(20)));
        assert_eq!(run("{{ $body?.x.b[0] }}", &vars), Ok(json!(null)));
        assert_eq!(run("{{ $body.a?.b[9] }}", &vars), Ok(json!(null)));
        assert_eq!(run("{{ $body?['a']['b'][0] }}", &vars), Ok(json!(10)));
        assert_eq!(run("{{ $missing? }}", &vars), Ok(json!(null)));
        assert_eq!(run("{{ $missing?.a ?? 5 }}", &vars), Ok(json!(5)));
        assert_eq!(run("{{ $body.nope? }}", &vars), Ok(json!(null)));
        assert_eq!(run("{{ $body.a?.b?[0] }}", &vars), Ok(json!(10)));
        let e = run("{{ $body.s?.x }}", &vars).unwrap_err();
        assert_eq!(
            e.message,
            "Couldn't match expected type 'Object' with actual type 'String'"
        );
        // Errors in bracket keys still propagate.
        assert!(run("{{ $body?[$k] }}", &vars).is_err());
        // `??` does not swallow errors.
        assert!(run("{{ $body.x ?? 1 }}", &vars).is_err());
    }

    #[test]
    fn string_templates() {
        let vars = Bindings::new()
            .with("$base_url", json!("http://example.com"))
            .with(
                "$body",
                serde_json::from_str(r#"{"input":{"id":5,"o":{"b":1,"a":[1,null]}}}"#).unwrap(),
            );
        let t = Template::parse_string_template("{{$base_url}}/users/{{$body.input.id}}").unwrap();
        assert_eq!(
            t.eval(&vars, &Functions::new()),
            Ok(json!("http://example.com/users/5"))
        );
        let t = Template::parse_string_template("{{$body.input.o}}|{{$body.input.x?}}").unwrap();
        assert_eq!(
            t.eval(&vars, &Functions::new()),
            Ok(json!(r#"{"a":[1,null],"b":1}|null"#))
        );
        let t = Template::parse_string_template(r#"say \"hi\" \\ {{ 1.5 }}"#).unwrap();
        assert_eq!(
            t.eval(&vars, &Functions::new()),
            Ok(json!(r#"say "hi" \ 1.5"#))
        );
        // Columns are shifted by the added quote.
        let e = Template::parse_string_template("{{$body.nope}}")
            .unwrap()
            .eval(&vars, &Functions::new())
            .unwrap_err();
        assert_eq!(cols(&e), (0, 4, 0, 14));
        assert_eq!(
            Template::parse_string_template("")
                .unwrap()
                .eval(&vars, &Functions::new()),
            Ok(json!(""))
        );
    }

    #[test]
    fn operators() {
        let v = Bindings::new();
        assert_eq!(run("{{ 1 == 1.0 }}", &v), Ok(json!(true)));
        assert_eq!(
            run("{{ [1, {\"a\": 2}] == [1.0, {\"a\": 2}] }}", &v),
            Ok(json!(true))
        );
        assert_eq!(run("{{ \"a\" < \"b\" }}", &v), Ok(json!(true)));
        assert_eq!(run("{{ {} < [] }}", &v), Ok(json!(true)));
        assert_eq!(run("{{ 3 >= 3 }}", &v), Ok(json!(true)));
        assert_eq!(run("{{ 2 in [1, 2.0] }}", &v), Ok(json!(true)));
        assert_eq!(run("{{ null ?? 1 }}", &v), Ok(json!(1)));
        assert_eq!(run("{{ false ?? 1 }}", &v), Ok(json!(false)));
        assert_eq!(run("{{ not true }}", &v), Ok(json!(false)));
        assert_eq!(run("{{ not (1 == 2) }}", &v), Ok(json!(true)));
        assert_eq!(run("{{ (true || false) && true }}", &v), Ok(json!(true)));
    }

    #[test]
    fn range_bindings_shadow_and_index() {
        let vars = Bindings::new().with("x", json!("outer"));
        assert_eq!(
            run(
                "{{ range i, x := [\"a\", \"b\"] }} \"{{i}}{{x}}\" {{ end }}",
                &vars
            ),
            Ok(json!(["0a", "1b"]))
        );
        assert_eq!(
            run(
                "[{{ range _, x := [1] }} {{ x }} {{ end }}, {{ x }}]",
                &vars
            ),
            Ok(json!([[1], "outer"]))
        );
    }

    #[test]
    fn output_objects_are_sorted() {
        let vars = body(serde_json::from_str(r#"{"z":1,"a":{"y":1,"b":2}}"#).unwrap());
        let out = run("{\"k\": {{ $body }}, \"b\": 1}", &vars).unwrap();
        assert_eq!(out.to_string(), r#"{"b":1,"k":{"a":{"b":2,"y":1},"z":1}}"#);
    }

    #[test]
    fn numbers_round_trip() {
        let v = Bindings::new();
        assert_eq!(run("9223372036854775807", &v), Ok(json!(i64::MAX)));
        assert_eq!(run("18446744073709551615", &v), Ok(json!(u64::MAX)));
        assert_eq!(run("-12", &v), Ok(json!(-12)));
        assert_eq!(run("1.25", &v), Ok(json!(1.25)));
        assert_eq!(run("1e3", &v).unwrap().to_string(), "1000");
        assert_eq!(run("\"{{ 2.50 }}\"", &v), Ok(json!("2.5")));
        let big = body(serde_json::from_str("{\"n\": 12345678901234567890}").unwrap());
        assert_eq!(
            run("{{ $body.n }}", &big).unwrap().to_string(),
            "12345678901234567890"
        );
    }

    #[test]
    fn parse_errors_serialize() {
        let e = Template::parse("{{ $.path }\n").unwrap_err();
        assert_eq!(
            e.to_json(),
            json!({
                "error_code": "Parse Error",
                "message": "Unexpected token '}'.",
                "source_position": {"start_line": 0, "start_column": 12, "end_line": 1, "end_column": 1}
            })
        );
        let e = Template::parse("{{ ! }}").unwrap_err();
        assert_eq!(e.error_code, ErrorCode::Lex);
        assert_eq!(e.to_json()["error_code"], json!("Lex Error"));
    }
}
