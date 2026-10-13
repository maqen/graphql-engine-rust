//! Recursive-descent port of kriti's Happy grammar (`Kriti/Parser/Grammar.y`).
//!
//! Binary operators are non-associative and only combine atoms; function calls,
//! `not e` and the `{{ e }}` wrapper are expressions, not atoms. Every parse error is
//! reported with the span of the last lexed item, as kriti does.

use crate::ast::{Accessor, BinOp, Elif, Expr, ExprKind};
use crate::error::{ErrorCode, KritiError, Span};
use crate::lexer::{Lexed, Sym, Tok, Token, lex};
use serde_json::Number;

/// Identifiers the grammar matches by text; they are never variables or function names.
const KEYWORDS: [&str; 8] = ["if", "elif", "else", "end", "null", "range", "in", "not"];

pub(crate) fn parse(src: &str) -> Result<Expr, KritiError> {
    let Lexed { tokens, last_span } = lex(src)?;
    let mut p = Parser {
        tokens,
        i: 0,
        last_span,
    };
    let expr = p.expr()?;
    match p.peek() {
        None => Ok(expr),
        Some(_) => Err(p.unexpected()),
    }
}

struct Parser {
    tokens: Vec<Token>,
    i: usize,
    last_span: Span,
}

fn binop(tok: &Tok) -> Option<BinOp> {
    Some(match tok {
        Tok::Sym(Sym::Gt) => BinOp::Gt,
        Tok::Sym(Sym::Lt) => BinOp::Lt,
        Tok::Sym(Sym::Gte) => BinOp::Gte,
        Tok::Sym(Sym::Lte) => BinOp::Lte,
        Tok::Sym(Sym::NotEq) => BinOp::NotEq,
        Tok::Sym(Sym::Eq) => BinOp::Eq,
        Tok::Sym(Sym::And) => BinOp::And,
        Tok::Sym(Sym::Or) => BinOp::Or,
        Tok::Sym(Sym::DoubleQuestion) => BinOp::Default,
        Tok::Ident(s) if s == "in" => BinOp::In,
        _ => return None,
    })
}

fn is_keyword(tok: Option<&Tok>, kw: &str) -> bool {
    matches!(tok, Some(Tok::Ident(s)) if s == kw)
}

fn node(span: Span, kind: ExprKind) -> Expr {
    Expr { span, kind }
}

impl Parser {
    fn peek(&self) -> Option<&Tok> {
        self.tokens.get(self.i).map(|t| &t.tok)
    }

    fn peek2(&self) -> Option<&Tok> {
        self.tokens.get(self.i + 1).map(|t| &t.tok)
    }

    fn at(&self, sym: Sym) -> bool {
        self.peek() == Some(&Tok::Sym(sym))
    }

    fn bump(&mut self) -> Token {
        let t = self.tokens[self.i].clone();
        self.i += 1;
        t
    }

    fn unexpected(&self) -> KritiError {
        match self.peek() {
            Some(tok) => KritiError::new(
                ErrorCode::Parse,
                format!("Unexpected token '{}'.", tok.render()),
                self.last_span,
            ),
            None => KritiError::new(
                ErrorCode::Parse,
                "ParseError: Empty token stream.",
                self.last_span,
            ),
        }
    }

    fn expect(&mut self, sym: Sym) -> Result<Span, KritiError> {
        if self.at(sym) {
            Ok(self.bump().span)
        } else {
            Err(self.unexpected())
        }
    }

    fn expect_keyword(&mut self, kw: &str) -> Result<Span, KritiError> {
        if is_keyword(self.peek(), kw) {
            Ok(self.bump().span)
        } else {
            Err(self.unexpected())
        }
    }

    /// A non-keyword identifier.
    fn ident(&mut self) -> Result<(String, Span), KritiError> {
        match self.peek() {
            Some(Tok::Ident(s)) if !KEYWORDS.contains(&s.as_str()) => {
                let t = self.bump();
                let Tok::Ident(s) = t.tok else { unreachable!() };
                Ok((s, t.span))
            }
            _ => Err(self.unexpected()),
        }
    }

    fn is_control_open(&self) -> bool {
        self.at(Sym::DoubleCurlyOpen)
            && (is_keyword(self.peek2(), "range") || is_keyword(self.peek2(), "if"))
    }

    /// `expr : atom OP atom | ap | '{{' expr '}}'`, `ap : ident '(' expr ')' | 'not' expr | atom`.
    fn expr(&mut self) -> Result<Expr, KritiError> {
        if self.at(Sym::DoubleCurlyOpen) && !self.is_control_open() {
            self.bump();
            let e = self.expr()?;
            self.expect(Sym::DoubleCurlyClose)?;
            return Ok(e);
        }
        if is_keyword(self.peek(), "not") {
            let kw = self.bump().span;
            let arg = self.expr()?;
            return Ok(node(
                kw.join(arg.span),
                ExprKind::Call("not".into(), Box::new(arg)),
            ));
        }
        if matches!(self.peek(), Some(Tok::Ident(s)) if !KEYWORDS.contains(&s.as_str()))
            && self.peek2() == Some(&Tok::Sym(Sym::ParenOpen))
        {
            let (name, start) = self.ident()?;
            self.bump();
            let arg = self.expr()?;
            let end = self.expect(Sym::ParenClose)?;
            return Ok(node(start.join(end), ExprKind::Call(name, Box::new(arg))));
        }
        let lhs = self.atom()?;
        let Some(op) = self.peek().and_then(binop) else {
            return Ok(lhs);
        };
        self.bump();
        let rhs = self.atom()?;
        Ok(node(
            lhs.span.join(rhs.span),
            ExprKind::Binary(op, Box::new(lhs), Box::new(rhs)),
        ))
    }

    /// An atom followed by any number of `.ident`, `[…]` and `?…` postfixes.
    fn atom(&mut self) -> Result<Expr, KritiError> {
        let mut e = self.primary()?;
        loop {
            if self.at(Sym::Dot) || self.at(Sym::SquareOpen) {
                let (acc, end) = self.accessor()?;
                e = node(e.span.join(end), ExprKind::Field(Box::new(e), acc));
            } else if self.at(Sym::Question) {
                self.bump();
                let mut fields = Vec::new();
                while self.at(Sym::Dot) || self.at(Sym::SquareOpen) {
                    fields.push(self.accessor()?.0);
                }
                e = node(e.span, ExprKind::Optional(Box::new(e), fields));
            } else {
                return Ok(e);
            }
        }
    }

    /// `.ident`, `['lit']` or `[atom]`; returns the accessor and the span of its last token.
    fn accessor(&mut self) -> Result<(Accessor, Span), KritiError> {
        if self.at(Sym::Dot) {
            self.bump();
            let (name, span) = self.ident()?;
            return Ok((Accessor::Key(name), span));
        }
        self.expect(Sym::SquareOpen)?;
        let key = if self.at(Sym::SingleQuote) {
            self.bump();
            let lit = match self.peek() {
                Some(Tok::Str(_)) => self.bump(),
                _ => return Err(self.unexpected()),
            };
            self.expect(Sym::SingleQuote)?;
            let Tok::Str(s) = lit.tok else { unreachable!() };
            node(lit.span, ExprKind::String(s))
        } else {
            self.atom()?
        };
        let end = self.expect(Sym::SquareClose)?;
        Ok((Accessor::Expr(Box::new(key)), end))
    }

    fn primary(&mut self) -> Result<Expr, KritiError> {
        let Some(tok) = self.peek() else {
            return Err(self.unexpected());
        };
        match tok {
            Tok::Ident(s) if s == "null" => Ok(node(self.bump().span, ExprKind::Null)),
            Tok::Ident(_) => {
                let (name, span) = self.ident()?;
                Ok(node(span, ExprKind::Var(name)))
            }
            Tok::Bool(b) => {
                let b = *b;
                Ok(node(self.bump().span, ExprKind::Bool(b)))
            }
            Tok::Int(_) | Tok::Num(_) => {
                let t = self.bump();
                let (Tok::Int(text) | Tok::Num(text)) = &t.tok else {
                    unreachable!()
                };
                let n = parse_number(text).ok_or_else(|| {
                    KritiError::new(
                        ErrorCode::Parse,
                        format!("Unexpected token '{text}'."),
                        t.span,
                    )
                })?;
                Ok(node(t.span, ExprKind::Number(n)))
            }
            Tok::Sym(Sym::ParenOpen) => {
                self.bump();
                let e = self.expr()?;
                self.expect(Sym::ParenClose)?;
                Ok(e)
            }
            Tok::Sym(Sym::StringBegin) => self.string_template(),
            Tok::Sym(Sym::SquareOpen) => self.array(),
            Tok::Sym(Sym::CurlyOpen) => self.object(),
            Tok::Sym(Sym::DoubleCurlyOpen) => {
                let open = self.bump().span;
                if is_keyword(self.peek(), "range") {
                    self.range(open)
                } else if is_keyword(self.peek(), "if") {
                    self.iff(open)
                } else {
                    Err(self.unexpected())
                }
            }
            _ => Err(self.unexpected()),
        }
    }

    /// `'{{' 'end' '}}'`, returning the closing span.
    fn end_block(&mut self) -> Result<Span, KritiError> {
        self.expect(Sym::DoubleCurlyOpen)?;
        self.expect_keyword("end")?;
        self.expect(Sym::DoubleCurlyClose)
    }

    fn range(&mut self, open: Span) -> Result<Expr, KritiError> {
        self.expect_keyword("range")?;
        let index = if self.at(Sym::Underscore) {
            self.bump();
            None
        } else {
            Some(self.ident()?.0)
        };
        self.expect(Sym::Comma)?;
        let (binder, _) = self.ident()?;
        self.expect(Sym::Assign)?;
        let over = self.expr()?;
        self.expect(Sym::DoubleCurlyClose)?;
        let body = self.expr()?;
        let close = self.end_block()?;
        Ok(node(
            open.join(close),
            ExprKind::Range {
                index,
                binder,
                over: Box::new(over),
                body: Box::new(body),
            },
        ))
    }

    fn iff(&mut self, open: Span) -> Result<Expr, KritiError> {
        self.expect_keyword("if")?;
        let cond = self.expr()?;
        self.expect(Sym::DoubleCurlyClose)?;
        let then = self.expr()?;
        let mut elifs = Vec::new();
        loop {
            self.expect(Sym::DoubleCurlyOpen)?;
            if is_keyword(self.peek(), "elif") {
                let kw = self.bump().span;
                let cond = self.expr()?;
                self.expect(Sym::DoubleCurlyClose)?;
                let body = self.expr()?;
                elifs.push(Elif {
                    span: kw.join(cond.span),
                    cond,
                    body,
                });
            } else {
                self.expect_keyword("else")?;
                break;
            }
        }
        self.expect(Sym::DoubleCurlyClose)?;
        let otherwise = self.expr()?;
        let close = self.end_block()?;
        Ok(node(
            open.join(close),
            ExprKind::If {
                cond: Box::new(cond),
                then: Box::new(then),
                elifs,
                otherwise: Box::new(otherwise),
            },
        ))
    }

    fn string_template(&mut self) -> Result<Expr, KritiError> {
        let open = self.expect(Sym::StringBegin)?;
        let mut parts = Vec::new();
        loop {
            match self.peek() {
                Some(Tok::Str(_)) => {
                    let t = self.bump();
                    let Tok::Str(s) = t.tok else { unreachable!() };
                    parts.push(node(t.span, ExprKind::String(s)));
                }
                Some(Tok::Sym(Sym::DoubleCurlyOpen)) => {
                    self.bump();
                    parts.push(self.expr()?);
                    self.expect(Sym::DoubleCurlyClose)?;
                }
                _ => break,
            }
        }
        let close = self.expect(Sym::StringEnd)?;
        Ok(node(open.join(close), ExprKind::Template(parts)))
    }

    fn array(&mut self) -> Result<Expr, KritiError> {
        let open = self.expect(Sym::SquareOpen)?;
        let mut items = Vec::new();
        if !self.at(Sym::SquareClose) {
            items.push(self.expr()?);
            while self.at(Sym::Comma) {
                self.bump();
                items.push(self.expr()?);
            }
        }
        let close = self.expect(Sym::SquareClose)?;
        Ok(node(open.join(close), ExprKind::Array(items)))
    }

    fn object(&mut self) -> Result<Expr, KritiError> {
        let open = self.expect(Sym::CurlyOpen)?;
        let mut fields: Vec<(String, Expr)> = Vec::new();
        if !self.at(Sym::CurlyClose) {
            loop {
                self.expect(Sym::StringBegin)?;
                let key = match self.peek() {
                    Some(Tok::Str(_)) => match self.bump().tok {
                        Tok::Str(s) => s,
                        _ => unreachable!(),
                    },
                    _ => String::new(),
                };
                self.expect(Sym::StringEnd)?;
                self.expect(Sym::Colon)?;
                let value = self.expr()?;
                if !fields.iter().any(|(k, _)| *k == key) {
                    fields.push((key, value));
                }
                if !self.at(Sym::Comma) {
                    break;
                }
                self.bump();
            }
        }
        let close = self.expect(Sym::CurlyClose)?;
        fields.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(node(open.join(close), ExprKind::Object(fields)))
    }
}

/// Converts a numeric literal to JSON, keeping integral values (`1.5e3`) as integers like
/// Haskell's `Scientific`. Falls back to `f64`; `None` if the value is not finite.
pub(crate) fn parse_number(text: &str) -> Option<Number> {
    if let Ok(i) = text.parse::<i64>() {
        return Some(i.into());
    }
    if let Ok(u) = text.parse::<u64>() {
        return Some(u.into());
    }
    let (mantissa, exp) = match text.find(['e', 'E']) {
        Some(at) => (&text[..at], text[at + 1..].parse::<i64>().ok()?),
        None => (text, 0),
    };
    let frac_len = mantissa.find('.').map_or(0, |dot| mantissa.len() - dot - 1);
    let eff = exp - i64::try_from(frac_len).ok()?;
    if (0..=38).contains(&eff) {
        let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
        let integer = digits
            .parse::<i128>()
            .ok()
            .and_then(|d| d.checked_mul(10i128.checked_pow(u32::try_from(eff).ok()?)?));
        if let Some(v) = integer {
            if let Ok(i) = i64::try_from(v) {
                return Some(i.into());
            }
            if let Ok(u) = u64::try_from(v) {
                return Some(u.into());
            }
        }
    }
    Number::from_f64(text.parse::<f64>().ok()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn err(src: &str) -> String {
        parse(src).err().unwrap().message
    }

    #[test]
    fn numbers() {
        assert_eq!(parse_number("1.5e3"), Some(1500.into()));
        assert_eq!(parse_number("-2"), Some((-2).into()));
        assert_eq!(parse_number("18446744073709551615"), Some(u64::MAX.into()));
        assert_eq!(parse_number("1.0").and_then(|n| n.as_f64()), Some(1.0));
        assert!(parse_number("1.0").unwrap().is_f64());
        assert_eq!(parse_number("2.5e-1").and_then(|n| n.as_f64()), Some(0.25));
        assert_eq!(parse_number("1e400"), None);
    }

    #[test]
    fn binary_operators_are_non_associative() {
        assert_eq!(err("{{ a && b && c }}"), "Unexpected token '&&'.");
        assert_eq!(err("{{ a == 1 && b }}"), "Unexpected token '&&'.");
        assert_eq!(err("{{ a ?? b ?? c }}"), "Unexpected token '??'.");
        assert!(parse("{{ (a && b) && c }}").is_ok());
        assert!(parse("{{ (a == 1) && b }}").is_ok());
    }

    #[test]
    fn calls_and_wrappers_are_not_atoms() {
        assert_eq!(err("{{ size(x) > 2 }}"), "Unexpected token '>'.");
        assert!(parse("{{ (size(x)) > 2 }}").is_ok());
        assert!(parse("{{ not x == 1 }}").is_ok());
        assert!(parse("{{ {{ {{ $body }} }} }}").is_ok());
    }

    #[test]
    fn else_is_mandatory() {
        assert_eq!(err("{{ if true }} 1 {{ end }}"), "Unexpected token 'end'.");
        assert!(parse("{{ if true }} 1 {{ elif false }} 2 {{ else }} 3 {{ end }}").is_ok());
    }

    #[test]
    fn keywords_are_not_identifiers() {
        assert_eq!(err("{{ $body.end }}"), "Unexpected token 'end'.");
        assert_eq!(err("{{ range }}"), "Unexpected token '}}'.");
    }

    #[test]
    fn unexpected_eof_and_empty_input() {
        assert_eq!(err(""), "ParseError: Empty token stream.");
        assert_eq!(err("[1, 2"), "ParseError: Empty token stream.");
        assert_eq!(err("\"abc"), "ParseError: Empty token stream.");
    }

    #[test]
    fn object_keys_are_plain_strings_and_first_wins() {
        assert_eq!(err("{\"a{{x}}\": 1}"), "Unexpected token '{{'.");
        let e = parse("{\"b\": 1, \"a\": 2, \"b\": 3}").unwrap();
        let ExprKind::Object(fields) = e.kind else {
            panic!()
        };
        let keys: Vec<_> = fields.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, ["a", "b"]);
        assert_eq!(fields[1].1.kind, ExprKind::Number(1.into()));
    }

    #[test]
    fn parse_error_span_is_last_lexed_item() {
        let e = parse("$body.hello }}").err().unwrap();
        assert_eq!(e.message, "Unexpected token '}'.");
        assert_eq!((e.span.start.column, e.span.end.column), (14, 15));
    }
}
