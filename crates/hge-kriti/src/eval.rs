//! The evaluator (kriti's `Kriti/Eval.hs`).
//!
//! Variables and path lookups are evaluated by reference (`Cow::Borrowed`), so reading
//! `$body.input.id` never clones `$body`.

use crate::Bindings;
use crate::ast::{Accessor, BinOp, Elif, Expr, ExprKind};
use crate::error::{ErrorCode, KritiError, Span};
use crate::functions::Functions;
use crate::value::{as_int, encode, json_cmp, json_eq, type_name};
use serde_json::Value;
use std::borrow::Cow;
use std::cmp::Ordering;

type Res<'a> = Result<Cow<'a, Value>, KritiError>;

/// A `range` binding, chained to the enclosing scope.
struct Frame<'a> {
    name: &'a str,
    value: &'a Value,
    parent: Option<&'a Frame<'a>>,
}

#[derive(Clone, Copy)]
struct Env<'a> {
    vars: &'a Bindings,
    funcs: &'a Functions,
    frame: Option<&'a Frame<'a>>,
}

pub(crate) fn eval(expr: &Expr, vars: &Bindings, funcs: &Functions) -> Result<Value, KritiError> {
    let env = Env {
        vars,
        funcs,
        frame: None,
    };
    eval_in(expr, env).map(Cow::into_owned)
}

fn type_error(span: Span, got: &Value, expected: &str) -> KritiError {
    KritiError::new(
        ErrorCode::Type,
        format!(
            "Couldn't match expected type '{expected}' with actual type '{}'",
            type_name(got)
        ),
        span,
    )
}

fn attribute_error(span: Span, key: &str) -> KritiError {
    // sic: the pinned kriti source spells it "attritubte".
    KritiError::new(
        ErrorCode::Attribute,
        format!("'Object' has no attritubte '{key}'."),
        span,
    )
}

fn name_error(span: Span, name: &str) -> KritiError {
    KritiError::new(
        ErrorCode::Name,
        format!("Variable '{name}' not in scope"),
        span,
    )
}

/// Projects a field out of a value, preserving borrowedness.
fn project<'a>(
    base: Cow<'a, Value>,
    f: impl FnOnce(&Value) -> Option<&Value>,
) -> Option<Cow<'a, Value>> {
    match base {
        Cow::Borrowed(v) => f(v).map(Cow::Borrowed),
        Cow::Owned(v) => f(&v).cloned().map(Cow::Owned),
    }
}

fn lookup<'a>(env: Env<'a>, name: &str) -> Option<&'a Value> {
    let mut frame = env.frame;
    while let Some(f) = frame {
        if f.name == name {
            return Some(f.value);
        }
        frame = f.parent;
    }
    env.vars.get(name)
}

/// Thin dispatcher: every arm lives in its own out-of-line function so the recursive frame
/// stays small even in debug builds (deep templates must not overflow a 2 MiB stack).
fn eval_in<'a>(expr: &Expr, env: Env<'a>) -> Res<'a> {
    let span = expr.span;
    match &*expr.kind {
        ExprKind::String(_) | ExprKind::Number(_) | ExprKind::Bool(_) | ExprKind::Null => {
            Ok(Cow::Owned(literal(&expr.kind)))
        }
        ExprKind::Object(fields) => object(fields, env),
        ExprKind::Array(items) => array(items, env),
        ExprKind::Template(parts) => template(parts, env),
        ExprKind::Var(name) => lookup(env, name)
            .map(Cow::Borrowed)
            .ok_or_else(|| name_error(span, name)),
        ExprKind::Field(base, acc) => field(span, base, acc, env),
        ExprKind::Optional(base, fields) => optional(base, fields, env),
        ExprKind::If {
            cond,
            then,
            elifs,
            otherwise,
        } => iff(span, cond, then, elifs, otherwise, env),
        ExprKind::Binary(op, lhs, rhs) => binary(span, *op, lhs, rhs, env).map(Cow::Owned),
        ExprKind::Range {
            index,
            binder,
            over,
            body,
        } => range(span, index.as_deref(), binder, over, body, env),
        ExprKind::Call(name, arg) => call(span, name, arg, env),
    }
}

#[inline(never)]
fn literal(kind: &ExprKind) -> Value {
    match kind {
        ExprKind::String(s) => Value::String(s.clone()),
        ExprKind::Number(n) => Value::Number(n.clone()),
        ExprKind::Bool(b) => Value::Bool(*b),
        _ => Value::Null,
    }
}

#[inline(never)]
fn object<'a>(fields: &[(String, Expr)], env: Env<'a>) -> Res<'a> {
    let mut entries = Vec::with_capacity(fields.len());
    for (k, v) in fields {
        entries.push((k.clone(), eval_in(v, env)?.into_owned()));
    }
    // Keys are already sorted and unique.
    Ok(Cow::Owned(Value::Object(entries.into_iter().collect())))
}

#[inline(never)]
fn array<'a>(items: &[Expr], env: Env<'a>) -> Res<'a> {
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        out.push(eval_in(item, env)?.into_owned());
    }
    Ok(Cow::Owned(Value::Array(out)))
}

#[inline(never)]
fn template<'a>(parts: &[Expr], env: Env<'a>) -> Res<'a> {
    let mut out = String::new();
    for part in parts {
        match &*eval_in(part, env)? {
            Value::String(s) => out.push_str(s),
            other => out.push_str(&encode(other)),
        }
    }
    Ok(Cow::Owned(Value::String(out)))
}

#[inline(never)]
fn iff<'a>(
    span: Span,
    cond: &Expr,
    then: &Expr,
    elifs: &[Elif],
    otherwise: &Expr,
    env: Env<'a>,
) -> Res<'a> {
    let branches =
        std::iter::once((span, cond, then)).chain(elifs.iter().map(|e| (e.span, &e.cond, &e.body)));
    for (cond_span, cond, body) in branches {
        match &*eval_in(cond, env)? {
            Value::Bool(true) => return eval_in(body, env),
            Value::Bool(false) => {}
            other => return Err(type_error(cond_span, other, "Boolean")),
        }
    }
    eval_in(otherwise, env)
}

#[inline(never)]
fn range<'a>(
    span: Span,
    index: Option<&str>,
    binder: &str,
    over: &Expr,
    body: &Expr,
    env: Env<'a>,
) -> Res<'a> {
    let over = eval_in(over, env)?;
    let Value::Array(items) = &*over else {
        return Err(type_error(span, &over, "Array"));
    };
    let mut out = Vec::with_capacity(items.len());
    for (i, item) in items.iter().enumerate() {
        let elem = Frame {
            name: binder,
            value: item,
            parent: env.frame,
        };
        let idx_value = Value::from(i);
        let idx_frame;
        let frame = match index {
            Some(name) => {
                idx_frame = Frame {
                    name,
                    value: &idx_value,
                    parent: Some(&elem),
                };
                &idx_frame
            }
            None => &elem,
        };
        let inner = Env {
            frame: Some(frame),
            ..env
        };
        out.push(eval_in(body, inner)?.into_owned());
    }
    Ok(Cow::Owned(Value::Array(out)))
}

#[inline(never)]
fn call<'a>(span: Span, name: &str, arg: &Expr, env: Env<'a>) -> Res<'a> {
    let value = eval_in(arg, env)?;
    match env.funcs.call(name, &value) {
        None => Err(name_error(span, name)),
        Some(Ok(v)) => Ok(Cow::Owned(v)),
        Some(Err(msg)) => Err(KritiError::new(ErrorCode::Function, msg, arg.span)),
    }
}

#[inline(never)]
fn field<'a>(span: Span, base: &Expr, acc: &Accessor, env: Env<'a>) -> Res<'a> {
    let value = eval_in(base, env)?;
    match (&*value, acc) {
        (Value::Object(_), Accessor::Key(k)) => {
            project(value, |v| v.get(k)).ok_or_else(|| attribute_error(span, k))
        }
        (Value::Object(_), Accessor::Expr(key)) => match &*eval_in(key, env)? {
            Value::String(k) => {
                project(value, |v| v.get(k)).ok_or_else(|| attribute_error(span, k))
            }
            other => Err(type_error(base.span, other, "String")),
        },
        (Value::Array(_), Accessor::Expr(key)) => match &*eval_in(key, env)? {
            Value::Number(n) => match as_int(n) {
                Some(i) => usize::try_from(i)
                    .ok()
                    .and_then(|i| project(value, |v| v.get(i)))
                    .ok_or_else(|| {
                        KritiError::new(ErrorCode::Index, "Can only range over an array", span)
                    }),
                None => Err(type_error(span, &Value::Number(n.clone()), "Integer")),
            },
            other => Err(type_error(span, other, "Integer")),
        },
        (other, _) => Err(type_error(base.span, other, "Object")),
    }
}

#[inline(never)]
fn optional<'a>(base: &Expr, fields: &[Accessor], env: Env<'a>) -> Res<'a> {
    let value = eval_in(base, env).unwrap_or(Cow::Owned(Value::Null));
    // kriti evaluates the bracket keys right to left (its field list is reversed).
    let mut keys: Vec<Cow<'_, Value>> = Vec::with_capacity(fields.len());
    for acc in fields.iter().rev() {
        keys.push(match acc {
            Accessor::Key(k) => Cow::Owned(Value::String(k.clone())),
            Accessor::Expr(e) => eval_in(e, env)?,
        });
    }
    keys.reverse();
    match &*value {
        Value::Object(_) | Value::Array(_) => {}
        Value::Null => return Ok(Cow::Owned(Value::Null)),
        other => return Err(type_error(base.span, other, "Object")),
    }
    let mut current = value;
    for key in &keys {
        let next = project(current, |v| match (&**key, v) {
            (Value::String(k), Value::Object(m)) => m.get(k),
            (Value::Number(n), Value::Array(a)) => as_int(n)
                .and_then(|i| usize::try_from(i).ok())
                .and_then(|i| a.get(i)),
            _ => None,
        });
        match next {
            Some(v) => current = v,
            None => return Ok(Cow::Owned(Value::Null)),
        }
    }
    Ok(current)
}

#[inline(never)]
fn binary(
    span: Span,
    op: BinOp,
    lhs: &Expr,
    rhs: &Expr,
    env: Env<'_>,
) -> Result<Value, KritiError> {
    if op == BinOp::Default {
        let a = eval_in(lhs, env)?;
        return Ok(if a.is_null() {
            eval_in(rhs, env)?.into_owned()
        } else {
            a.into_owned()
        });
    }
    let a = eval_in(lhs, env)?;
    let b = eval_in(rhs, env)?;
    let (a, b) = (&*a, &*b);
    Ok(Value::Bool(match op {
        BinOp::Eq => json_eq(a, b),
        BinOp::NotEq => !json_eq(a, b),
        BinOp::Lt => json_cmp(a, b) == Ordering::Less,
        BinOp::Gt => json_cmp(a, b) == Ordering::Greater,
        BinOp::Lte => json_cmp(a, b) != Ordering::Greater,
        BinOp::Gte => json_cmp(a, b) != Ordering::Less,
        BinOp::And | BinOp::Or => match (a, b) {
            (Value::Bool(p), Value::Bool(q)) => {
                if op == BinOp::And {
                    *p && *q
                } else {
                    *p || *q
                }
            }
            (other, Value::Bool(_)) | (_, other) => return Err(type_error(span, other, "Boolean")),
        },
        BinOp::In => match (a, b) {
            (Value::String(k), Value::Object(m)) => m.contains_key(k),
            (other, Value::Object(_)) => return Err(type_error(span, other, "String")),
            (_, Value::Array(items)) => items.iter().any(|v| json_eq(a, v)),
            (_, other) => return Err(type_error(span, other, "Array")),
        },
        BinOp::Default => unreachable!("handled above"),
    }))
}
