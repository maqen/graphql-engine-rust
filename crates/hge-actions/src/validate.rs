//! Checking a handler's 2xx response against the action's output type (V2's
//! `validateResponse`). Only the shape is checked: scalar values inside objects are not
//! type-checked, and nested objects are not validated.

use hge_ir::action::{ActionOutput, OutputBase, OutputType};
use serde_json::Value;

const BUILTINS: &[&str] = &["Int", "Float", "String", "Boolean", "ID"];

/// `Err` carries V2's exact message.
pub fn validate(output: &ActionOutput, v: &Value) -> Result<(), String> {
    validate_type(output, &output.ty, v)
}

fn is_custom_scalar(output: &ActionOutput, ty: &OutputType) -> bool {
    match ty {
        OutputType::Named(_) => output.base == OutputBase::CustomScalar,
        _ => false,
    }
}

fn validate_type(output: &ActionOutput, ty: &OutputType, v: &Value) -> Result<(), String> {
    if is_custom_scalar(output, ty) {
        return Ok(());
    }
    let (non_null, inner) = match ty {
        OutputType::NonNull(t) => (true, t.as_ref()),
        other => (false, other),
    };
    if non_null && is_custom_scalar(output, inner) {
        return Ok(());
    }
    match (v, inner) {
        (Value::Null, _) => {
            if non_null {
                Err("got null for the action webhook response".into())
            } else {
                Ok(())
            }
        }
        (Value::Array(items), OutputType::List(elem)) => {
            for item in items {
                validate_type(output, elem, item)?;
            }
            Ok(())
        }
        (_, OutputType::List(_)) => Err("expecting array for the action webhook response".into()),
        (Value::Number(_), OutputType::Named(n)) => {
            if n == "Int" || n == "Float" {
                Ok(())
            } else {
                // V2 says "String" here even for numbers (quirk Q3).
                Err(format!(
                    "got scalar String for the action webhook response, expecting {n}"
                ))
            }
        }
        (Value::Bool(_), OutputType::Named(n)) => {
            if n == "Boolean" {
                Ok(())
            } else {
                Err(format!(
                    "got scalar Boolean for the action webhook response, expecting {n}"
                ))
            }
        }
        (Value::String(_), OutputType::Named(n)) => {
            if n == "String" || n == "ID" {
                Ok(())
            } else {
                Err(format!(
                    "got scalar String for the action webhook response, expecting {n}"
                ))
            }
        }
        (Value::Array(_), OutputType::Named(n)) => Err(format!(
            "got array for the action webhook response, expecting {n}"
        )),
        (Value::Object(obj), OutputType::Named(n)) => {
            if BUILTINS.contains(&n.as_str()) {
                return Err(format!(
                    "got object for the action webhook response, expecting {n}"
                ));
            }
            if let OutputBase::Object { required } = &output.base {
                for field in required {
                    match obj.get(field) {
                        None => {
                            return Err(format!(
                                "field \"{field}\" expected in webhook response, but not found"
                            ));
                        }
                        Some(Value::Null) => {
                            return Err(format!("expecting not null value for field \"{field}\""));
                        }
                        Some(_) => {}
                    }
                }
            }
            Ok(())
        }
        (_, OutputType::NonNull(t)) => validate_type(output, t, v),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn named(n: &str) -> OutputType {
        OutputType::Named(n.into())
    }
    fn nn(t: OutputType) -> OutputType {
        OutputType::NonNull(Box::new(t))
    }
    fn list(t: OutputType) -> OutputType {
        OutputType::List(Box::new(t))
    }
    fn out(ty: OutputType, base: OutputBase) -> ActionOutput {
        ActionOutput { ty, base }
    }

    #[test]
    fn scalar_messages() {
        let o = out(
            list(named("Int")).non_null_wrap(),
            OutputBase::BuiltinScalar,
        );
        assert_eq!(
            validate(&o, &json!(["foo"])).unwrap_err(),
            "got scalar String for the action webhook response, expecting Int"
        );
        let o = out(nn(list(nn(named("String")))), OutputBase::BuiltinScalar);
        assert_eq!(
            validate(&o, &json!(["foo", null])).unwrap_err(),
            "got null for the action webhook response"
        );
        assert_eq!(
            validate(&o, &json!("x")).unwrap_err(),
            "expecting array for the action webhook response"
        );
        let o = out(named("String"), OutputBase::BuiltinScalar);
        assert_eq!(
            validate(&o, &json!({"a": 1})).unwrap_err(),
            "got object for the action webhook response, expecting String"
        );
        assert!(validate(&o, &Value::Null).is_ok());
    }

    #[test]
    fn custom_scalars_accept_anything_per_level() {
        let o = out(nn(list(nn(named("myScalar")))), OutputBase::CustomScalar);
        assert!(validate(&o, &json!([{"foo": "bar"}])).is_ok());
        let o = out(
            nn(list(list(nn(named("myScalar"))))),
            OutputBase::CustomScalar,
        );
        assert_eq!(
            validate(&o, &json!([{"foo": "bar"}])).unwrap_err(),
            "expecting array for the action webhook response"
        );
    }

    #[test]
    fn object_required_fields() {
        let o = out(
            named("UserId"),
            OutputBase::Object {
                required: vec!["id".into()],
            },
        );
        assert_eq!(
            validate(&o, &json!({"x": 1})).unwrap_err(),
            "field \"id\" expected in webhook response, but not found"
        );
        assert_eq!(
            validate(&o, &json!({"id": null})).unwrap_err(),
            "expecting not null value for field \"id\""
        );
        assert!(validate(&o, &json!({"id": "abc", "extra": 1})).is_ok());
        assert_eq!(
            validate(&o, &json!([])).unwrap_err(),
            "got array for the action webhook response, expecting UserId"
        );
    }

    trait Wrap {
        fn non_null_wrap(self) -> OutputType;
    }
    impl Wrap for OutputType {
        fn non_null_wrap(self) -> OutputType {
            nn(self)
        }
    }
}
