//! SQL functions in the schema: computed fields (functions attached to a table) and tracked
//! functions (root fields).
//!
//! Function metadata stays untyped in `hge-metadata` (`serde_json::Value`); it is parsed here,
//! next to the code that resolves it against the function catalog.

pub mod computed;
pub mod tracked;

use hge_core::{ColumnType, FunctionArg, FunctionCatalog, QualifiedFunction, SourceCatalog};
use hge_ir::{FunctionArgValue, FunctionArgument, FunctionCall, Value};
use serde::Deserialize;

pub use computed::{ComputedFieldInfo, ComputedReturn};
pub use tracked::{ExposedAs, FunctionInfo};

/// `computed_fields[]` of a table.
#[derive(Debug, Clone, Deserialize)]
pub struct ComputedFieldMeta {
    pub name: String,
    pub definition: ComputedFieldDefinition,
    #[serde(default)]
    pub comment: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ComputedFieldDefinition {
    pub function: QualifiedFunction,
    #[serde(default)]
    pub table_argument: Option<String>,
    #[serde(default)]
    pub session_argument: Option<String>,
}

/// The single overload of `name`, or V2's error.
pub(crate) fn lookup<'c>(
    catalog: &'c SourceCatalog,
    name: &QualifiedFunction,
) -> Result<&'c FunctionCatalog, String> {
    match catalog.functions.get(name).map(Vec::as_slice) {
        None | Some([]) => Err(format!("no such function exists: \"{}\"", name.v2_text())),
        Some([f]) => Ok(f),
        Some(_) => Err(format!(
            "function \"{}\" is overloaded. Overloaded functions are not supported",
            name.v2_text()
        )),
    }
}

/// How V2 prints a type in function errors: `pg_catalog.integer`-style for built-ins is
/// approximated by `schema.name`.
/// A type's name as V2 prints a qualified name: without the `public` schema.
pub(crate) fn type_text(ty: &ColumnType) -> String {
    if ty.schema == "public" {
        ty.name.clone()
    } else {
        format!("{}.{}", ty.schema, ty.name)
    }
}

pub(crate) fn is_json(arg: &FunctionArg) -> bool {
    arg.ty.dimensions == 0 && arg.ty.schema == "pg_catalog" && arg.ty.name == "json"
}

/// The arguments a user passes (`args: {…}`) with their GraphQL names: every argument but
/// the `implicit` ones (table row, session), unnamed ones as `arg_<n>` (n counting unnamed
/// arguments only).
pub fn input_args<'a>(
    args: &'a [FunctionArg],
    implicit: impl Fn(usize) -> bool + 'a,
) -> impl Iterator<Item = (usize, String, &'a FunctionArg)> + 'a {
    let mut unnamed = 0;
    args.iter().enumerate().filter_map(move |(i, a)| {
        if implicit(i) {
            return None;
        }
        let name = a.name.clone().unwrap_or_else(|| {
            unnamed += 1;
            format!("arg_{unnamed}")
        });
        Some((i, name, a))
    })
}

/// A call of `function`: `implicit` and `inputs` give argument values by index into `args`;
/// omitted arguments take their defaults.
pub fn build_call(
    function: &QualifiedFunction,
    args: &[FunctionArg],
    implicit: Vec<(usize, FunctionArgValue)>,
    inputs: Vec<(usize, Value)>,
) -> Result<FunctionCall, &'static str> {
    let mut values: Vec<Option<FunctionArgValue>> = vec![None; args.len()];
    for (i, v) in implicit {
        values[i] = Some(v);
    }
    for (i, v) in inputs {
        values[i] = Some(FunctionArgValue::Value(v));
    }
    Ok(FunctionCall {
        function: function.clone(),
        args: arrange_args(args, values)?
            .into_iter()
            .map(|p| match p {
                Passing::Positional(value) => FunctionArgument { name: None, value },
                Passing::Named(name, value) => FunctionArgument {
                    name: Some(name),
                    value,
                },
            })
            .collect(),
    })
}

/// One argument of a call as V2 passes it: positionally up to the first omitted argument,
/// then by name.
#[derive(Debug, Clone, PartialEq)]
pub enum Passing<T> {
    Positional(T),
    Named(String, T),
}

/// Arrange call arguments V2's way. `values[i]` is the value for the function's `i`-th input
/// argument, `None` when omitted (its default is used).
pub fn arrange_args<T>(
    args: &[FunctionArg],
    values: Vec<Option<T>>,
) -> Result<Vec<Passing<T>>, &'static str> {
    let mut out = Vec::with_capacity(values.len());
    let mut gap = false;
    for (arg, value) in args.iter().zip(values) {
        match value {
            None if !arg.has_default => return Err("Non default arguments cannot be omitted"),
            None => gap = true,
            Some(v) if !gap => out.push(Passing::Positional(v)),
            Some(v) => match &arg.name {
                Some(name) => out.push(Passing::Named(name.clone(), v)),
                None => return Err("Only last set of positional arguments can be omitted"),
            },
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arg(name: Option<&str>, has_default: bool) -> FunctionArg {
        FunctionArg {
            name: name.map(str::to_owned),
            ty: ColumnType::builtin("int4"),
            has_default,
        }
    }

    #[test]
    fn positional_then_named() {
        let args = [
            arg(Some("a"), false),
            arg(Some("b"), true),
            arg(Some("c"), true),
        ];
        assert_eq!(
            arrange_args(&args, vec![Some(1), None, Some(3)]),
            Ok(vec![Passing::Positional(1), Passing::Named("c".into(), 3)])
        );
        assert_eq!(
            arrange_args(&args, vec![Some(1), Some(2), None]),
            Ok(vec![Passing::Positional(1), Passing::Positional(2)])
        );
        assert_eq!(
            arrange_args::<i32>(&args, vec![None, None, None]),
            Err("Non default arguments cannot be omitted")
        );
        let unnamed = [arg(None, true), arg(None, true)];
        assert_eq!(
            arrange_args(&unnamed, vec![None, Some(2)]),
            Err("Only last set of positional arguments can be omitted")
        );
    }
}
