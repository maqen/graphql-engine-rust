//! Hand-written port of kriti's Alex lexer (`Kriti/Parser/Lexer.x`).
//!
//! The lexer keeps a stack of modes (default, `expr`, `string`, `literal`) and applies
//! Alex's longest-match rule (ties go to the earlier rule). Like the original, the whole
//! input is lexed before parsing, and the span of the last lexed item (token, whitespace or
//! comment) is kept because kriti reports it for every parse error.

use crate::error::{ErrorCode, KritiError, Position, Span};

/// Non-literal symbols.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Sym {
    Colon,
    Dot,
    Comma,
    Question,
    DoubleQuestion,
    Eq,
    NotEq,
    Gt,
    Gte,
    Lt,
    Lte,
    And,
    Or,
    SingleQuote,
    CurlyOpen,
    CurlyClose,
    DoubleCurlyOpen,
    DoubleCurlyClose,
    SquareOpen,
    SquareClose,
    ParenOpen,
    ParenClose,
    Underscore,
    Assign,
    StringBegin,
    StringEnd,
}

impl Sym {
    fn as_str(self) -> &'static str {
        match self {
            Sym::Colon => ":",
            Sym::Dot => ".",
            Sym::Comma => ",",
            Sym::Question => "?",
            Sym::DoubleQuestion => "??",
            Sym::Eq => "==",
            Sym::NotEq => "!=",
            Sym::Gt => ">",
            Sym::Gte => ">=",
            Sym::Lt => "<",
            Sym::Lte => "<=",
            Sym::And => "&&",
            Sym::Or => "||",
            Sym::SingleQuote => "'",
            Sym::CurlyOpen => "{",
            Sym::CurlyClose => "}",
            Sym::DoubleCurlyOpen => "{{",
            Sym::DoubleCurlyClose => "}}",
            Sym::SquareOpen => "[",
            Sym::SquareClose => "]",
            Sym::ParenOpen => "(",
            Sym::ParenClose => ")",
            Sym::Underscore => "_",
            Sym::Assign => ":=",
            Sym::StringBegin | Sym::StringEnd => "\"",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Tok {
    /// A (possibly merged) string fragment, already unescaped.
    Str(String),
    Ident(String),
    /// Integer literal, kept as source text.
    Int(String),
    /// Decimal / exponent literal, kept as source text.
    Num(String),
    Bool(bool),
    Sym(Sym),
}

impl Tok {
    /// kriti's `serializeToken`, used in `Unexpected token '…'.` messages.
    pub(crate) fn render(&self) -> String {
        match self {
            Tok::Str(s) => format!("\"{s}\""),
            Tok::Ident(s) | Tok::Int(s) | Tok::Num(s) => s.clone(),
            Tok::Bool(true) => "true".into(),
            Tok::Bool(false) => "false".into(),
            Tok::Sym(s) => s.as_str().into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Token {
    pub tok: Tok,
    pub span: Span,
}

pub(crate) struct Lexed {
    pub tokens: Vec<Token>,
    /// Span of the last item the lexer consumed (kriti's `parseSpan` after lexing).
    pub last_span: Span,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Default,
    Expr,
    Str,
    Literal,
}

struct Lexer<'s> {
    src: &'s str,
    chars: Vec<char>,
    i: usize,
    pos: Position,
    prev_end: Position,
    last_span: Span,
    modes: Vec<Mode>,
    tokens: Vec<Token>,
}

/// Lexes a whole template.
pub(crate) fn lex(src: &str) -> Result<Lexed, KritiError> {
    let start = Position::new(0, 1);
    let mut lx = Lexer {
        src,
        chars: src.chars().collect(),
        i: 0,
        pos: start,
        prev_end: start,
        last_span: Span::new(start, start),
        modes: Vec::new(),
        tokens: Vec::new(),
    };
    while lx.i < lx.chars.len() {
        match lx.mode() {
            Mode::Default | Mode::Expr => lx.code()?,
            Mode::Str => lx.string()?,
            Mode::Literal => lx.literal()?,
        }
    }
    Ok(Lexed {
        tokens: lx.tokens,
        last_span: lx.last_span,
    })
}

/// Haskell's `$white`: space, tab, newline, carriage return, form feed, vertical tab.
fn is_white(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r' | '\x0c' | '\x0b')
}

fn is_ident_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '$' | '_' | '-')
}

impl Lexer<'_> {
    fn mode(&self) -> Mode {
        self.modes.last().copied().unwrap_or(Mode::Default)
    }

    fn peek(&self, k: usize) -> Option<char> {
        self.chars.get(self.i + k).copied()
    }

    /// A lex error after the DFA consumed `stuck_after` ASCII characters of a partial match
    /// (Alex reports the position where scanning got stuck).
    fn error(&self, stuck_after: usize) -> KritiError {
        let start = Position::new(self.pos.line, self.pos.column + stuck_after);
        let end = Position::new(start.line, start.column + self.src.len());
        KritiError::new(
            ErrorCode::Lex,
            format!("Invalid Lexeme: '{}'", self.src),
            Span::new(start, end),
        )
    }

    /// Consumes `n` characters and returns the span from the previous item's end.
    fn advance(&mut self, n: usize) -> Span {
        for _ in 0..n {
            let c = self.chars[self.i];
            if c == '\n' {
                self.pos.line += 1;
                self.pos.column = 1;
            } else if c.len_utf8() == 1 {
                // sic: kriti's `alexGetByte` does not advance the column for multi-byte chars.
                self.pos.column += 1;
            }
            self.i += 1;
        }
        let span = Span::new(self.prev_end, self.pos);
        self.prev_end = self.pos;
        self.last_span = span;
        span
    }

    fn emit(&mut self, n: usize, tok: Tok) {
        let span = self.advance(n);
        if let Tok::Str(s) = &tok
            && let Some(Token {
                tok: Tok::Str(prev),
                span: prev_span,
            }) = self.tokens.last_mut()
        {
            prev.push_str(s);
            *prev_span = prev_span.join(span);
            return;
        }
        self.tokens.push(Token { tok, span });
    }

    fn text(&self, n: usize) -> String {
        self.chars[self.i..self.i + n].iter().collect()
    }

    fn count_while(&self, from: usize, f: impl Fn(char) -> bool) -> usize {
        self.chars[(self.i + from).min(self.chars.len())..]
            .iter()
            .take_while(|c| f(**c))
            .count()
    }

    /// Default and `expr` modes.
    fn code(&mut self) -> Result<(), KritiError> {
        let c = self.chars[self.i];
        if is_white(c) {
            let n = self.count_while(0, is_white);
            self.advance(n);
            return Ok(());
        }
        if c == '#' {
            let n = self.count_while(0, |c| c != '\n');
            self.advance(n);
            return Ok(());
        }
        if c == '$' || c.is_ascii_alphabetic() {
            let n = if c == '$' && !self.peek(1).is_some_and(|c| c.is_ascii_alphabetic()) {
                1
            } else {
                1 + self.count_while(1, is_ident_char)
            };
            let text = self.text(n);
            let tok = match text.as_str() {
                "true" => Tok::Bool(true),
                "false" => Tok::Bool(false),
                _ => Tok::Ident(text),
            };
            self.emit(n, tok);
            return Ok(());
        }
        if c.is_ascii_digit() || (c == '-' && self.peek(1).is_some_and(|c| c.is_ascii_digit())) {
            let (int_len, num_len) = self.number_lengths();
            let (n, num) = if num_len > int_len {
                (num_len, true)
            } else {
                (int_len, false)
            };
            let text = self.text(n);
            self.emit(n, if num { Tok::Num(text) } else { Tok::Int(text) });
            return Ok(());
        }
        let next = self.peek(1);
        let (n, sym) = match (c, next) {
            ('"', _) => {
                self.modes.push(Mode::Str);
                (1, Sym::StringBegin)
            }
            ('\'', _) => {
                self.modes.push(Mode::Literal);
                (1, Sym::SingleQuote)
            }
            ('{', Some('{')) => {
                self.modes.push(Mode::Expr);
                (2, Sym::DoubleCurlyOpen)
            }
            ('}', Some('}')) if self.mode() == Mode::Expr => {
                self.modes.pop();
                (2, Sym::DoubleCurlyClose)
            }
            (':', Some('=')) => (2, Sym::Assign),
            ('?', Some('?')) => (2, Sym::DoubleQuestion),
            ('=', Some('=')) => (2, Sym::Eq),
            ('!', Some('=')) => (2, Sym::NotEq),
            ('>', Some('=')) => (2, Sym::Gte),
            ('<', Some('=')) => (2, Sym::Lte),
            ('&', Some('&')) => (2, Sym::And),
            ('|', Some('|')) => (2, Sym::Or),
            (':', _) => (1, Sym::Colon),
            ('.', _) => (1, Sym::Dot),
            (',', _) => (1, Sym::Comma),
            ('?', _) => (1, Sym::Question),
            ('>', _) => (1, Sym::Gt),
            ('<', _) => (1, Sym::Lt),
            ('_', _) => (1, Sym::Underscore),
            ('{', _) => (1, Sym::CurlyOpen),
            ('}', _) => (1, Sym::CurlyClose),
            ('[', _) => (1, Sym::SquareOpen),
            (']', _) => (1, Sym::SquareClose),
            ('(', _) => (1, Sym::ParenOpen),
            (')', _) => (1, Sym::ParenClose),
            // Prefixes of `==`, `!=`, `&&`, `||` and negative numbers are consumed first.
            ('=' | '!' | '&' | '|' | '-', _) => return Err(self.error(1)),
            _ => return Err(self.error(0)),
        };
        self.emit(n, Tok::Sym(sym));
        Ok(())
    }

    /// Match lengths of `-?[0-9]+` and `-?(0|[1-9][0-9]*)(\.[0-9]+)?([eE][+-]?[0-9]+)?`.
    fn number_lengths(&self) -> (usize, usize) {
        let digit = |c: char| c.is_ascii_digit();
        let sign = usize::from(self.chars[self.i] == '-');
        let int_len = sign + self.count_while(sign, digit);
        let mut k = sign;
        if self.peek(k) == Some('0') {
            k += 1;
        } else {
            k += self.count_while(k, digit);
        }
        if self.peek(k) == Some('.') && self.peek(k + 1).is_some_and(digit) {
            k += 1 + self.count_while(k + 1, digit);
        }
        if matches!(self.peek(k), Some('e' | 'E')) {
            let s = usize::from(matches!(self.peek(k + 1), Some('+' | '-')));
            let d = self.count_while(k + 1 + s, digit);
            if d > 0 {
                k += 1 + s + d;
            }
        }
        (int_len, k)
    }

    fn string(&mut self) -> Result<(), KritiError> {
        let c = self.chars[self.i];
        match c {
            '"' => {
                self.modes.pop();
                self.emit(1, Tok::Sym(Sym::StringEnd));
            }
            '{' if self.peek(1) == Some('{') => {
                self.modes.push(Mode::Expr);
                self.emit(2, Tok::Sym(Sym::DoubleCurlyOpen));
            }
            '{' => self.emit(1, Tok::Str("{".into())),
            '\\' => {
                let s = match self.peek(1) {
                    Some('{') => "{",
                    Some('"') => "\"",
                    Some('\\') => "\\",
                    // sic: kriti keeps both characters of `\/`.
                    Some('/') => "\\/",
                    Some('b') => "\u{8}",
                    Some('f') => "\u{c}",
                    Some('n') => "\n",
                    Some('r') => "\r",
                    Some('t') => "\t",
                    Some('u') => {
                        let hex: String = self.chars[self.i + 2..]
                            .iter()
                            .take(4)
                            .take_while(|c| c.is_ascii_hexdigit())
                            .collect();
                        if hex.len() != 4 {
                            return Err(self.error(2 + hex.len()));
                        }
                        let code = u32::from_str_radix(&hex, 16).unwrap_or(0xFFFD);
                        let ch = char::from_u32(code).unwrap_or('\u{FFFD}');
                        self.emit(6, Tok::Str(ch.to_string()));
                        return Ok(());
                    }
                    _ => return Err(self.error(1)),
                };
                self.emit(2, Tok::Str(s.into()));
            }
            _ => {
                let n = self.count_while(0, |c| !matches!(c, '\\' | '"' | '{'));
                let text = self.text(n);
                self.emit(n, Tok::Str(text));
            }
        }
        Ok(())
    }

    fn literal(&mut self) -> Result<(), KritiError> {
        match self.chars[self.i] {
            '\'' => {
                self.modes.pop();
                self.emit(1, Tok::Sym(Sym::SingleQuote));
            }
            '{' => return Err(self.error(0)),
            _ => {
                let n = self.count_while(0, |c| !matches!(c, '\'' | '{'));
                let text = self.text(n);
                self.emit(n, Tok::Str(text));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toks(src: &str) -> Vec<Tok> {
        lex(src)
            .unwrap()
            .tokens
            .into_iter()
            .map(|t| t.tok)
            .collect()
    }

    #[test]
    fn identifiers_and_keywords() {
        assert_eq!(
            toks("$body foo-bar a$b $ true trueish"),
            vec![
                Tok::Ident("$body".into()),
                Tok::Ident("foo-bar".into()),
                Tok::Ident("a$b".into()),
                Tok::Ident("$".into()),
                Tok::Bool(true),
                Tok::Ident("trueish".into()),
            ]
        );
    }

    #[test]
    fn numbers_longest_match() {
        assert_eq!(
            toks("1 -2 1.5 1e3 007 01.5"),
            vec![
                Tok::Int("1".into()),
                Tok::Int("-2".into()),
                Tok::Num("1.5".into()),
                Tok::Num("1e3".into()),
                Tok::Int("007".into()),
                Tok::Int("01".into()),
                Tok::Sym(Sym::Dot),
                Tok::Int("5".into()),
            ]
        );
    }

    #[test]
    fn curly_close_depends_on_mode() {
        assert_eq!(
            toks("}}"),
            vec![Tok::Sym(Sym::CurlyClose), Tok::Sym(Sym::CurlyClose)]
        );
        assert_eq!(
            toks("{{ x }}"),
            vec![
                Tok::Sym(Sym::DoubleCurlyOpen),
                Tok::Ident("x".into()),
                Tok::Sym(Sym::DoubleCurlyClose)
            ]
        );
    }

    #[test]
    fn string_escapes_and_merging() {
        assert_eq!(
            toks(r#""a\n\{{σ\/{b""#),
            vec![
                Tok::Sym(Sym::StringBegin),
                Tok::Str("a\n{{σ\\/{b".into()),
                Tok::Sym(Sym::StringEnd)
            ]
        );
    }

    #[test]
    fn spans_are_zero_based_lines_one_based_columns() {
        let lexed = lex("{\n  \"σ\": 1 }\n").unwrap();
        let str_tok = &lexed.tokens[2];
        assert_eq!(str_tok.tok, Tok::Str("σ".into()));
        assert_eq!(
            str_tok.span,
            Span::new(Position::new(1, 4), Position::new(1, 4))
        );
        assert_eq!(
            lexed.last_span,
            Span::new(Position::new(1, 10), Position::new(2, 1))
        );
    }

    #[test]
    fn lex_error_reports_whole_source() {
        let err = lex("{{ a @ b }}").err().unwrap();
        assert_eq!(err.error_code, ErrorCode::Lex);
        assert_eq!(err.message, "Invalid Lexeme: '{{ a @ b }}'");
        assert_eq!(
            err.span,
            Span::new(Position::new(0, 6), Position::new(0, 17))
        );
        let col = |src: &str| lex(src).err().unwrap().span.start.column;
        assert_eq!(col("{{ a = b }}"), 7);
        assert_eq!(col(r#""\u12""#), 6);
        assert_eq!(col(r#"{{ "\q" }}"#), 6);
        assert_eq!(col("-a"), 2);
        assert_eq!(col("'a{'"), 3);
        assert_eq!(col("σ@"), 1);
    }
}
