//! The plain-scalar resolution of go-yaml v3 (`resolve.go`), reduced to the one question
//! the encoder asks: would this text, written unquoted, read back as a string?

/// True when `s`, written as a plain scalar, resolves to `!!str` in go-yaml v3.
pub fn resolves_to_str(s: &str) -> bool {
    let Some(&first) = s.as_bytes().first() else {
        return false; // "" is null
    };
    if matches!(
        s,
        "true"
            | "True"
            | "TRUE"
            | "false"
            | "False"
            | "FALSE"
            | "~"
            | "null"
            | "Null"
            | "NULL"
            | ".nan"
            | ".NaN"
            | ".NAN"
            | ".inf"
            | ".Inf"
            | ".INF"
            | "+.inf"
            | "+.Inf"
            | "+.INF"
            | "-.inf"
            | "-.Inf"
            | "-.INF"
            | "<<"
    ) {
        return false;
    }
    match first {
        b'.' => !go_parse_float(s),
        b'+' | b'-' | b'0'..=b'9' => {
            if is_timestamp(s) {
                return false;
            }
            let plain = s.replace('_', "");
            if go_parse_int_base0(&plain) {
                return false;
            }
            if is_yaml_style_float(&plain) && go_parse_float(&plain) {
                return false;
            }
            let digits_after = |p: &str, radix: u32| {
                plain
                    .strip_prefix(p)
                    .is_some_and(|d| !d.is_empty() && d.chars().all(|c| c.is_digit(radix)))
            };
            !(digits_after("0b", 2)
                || digits_after("-0b", 2)
                || digits_after("0o", 8)
                || digits_after("-0o", 8))
        }
        _ => true,
    }
}

/// Go's `strconv.ParseInt(s, 0, 64)` or `ParseUint(s, 0, 64)` succeeding (ignoring range,
/// since an out-of-range integer still resolves as a float).
fn go_parse_int_base0(s: &str) -> bool {
    let body = s.strip_prefix(['+', '-']).unwrap_or(s);
    let (digits, radix) =
        if let Some(d) = body.strip_prefix("0x").or_else(|| body.strip_prefix("0X")) {
            (d, 16)
        } else if let Some(d) = body.strip_prefix("0b").or_else(|| body.strip_prefix("0B")) {
            (d, 2)
        } else if let Some(d) = body.strip_prefix("0o").or_else(|| body.strip_prefix("0O")) {
            (d, 8)
        } else if body.len() > 1 && body.starts_with('0') {
            (&body[1..], 8)
        } else {
            (body, 10)
        };
    if digits.is_empty() || !digits.chars().all(|c| c.is_digit(radix)) {
        return false;
    }
    // Out of range for both int64 and uint64 is an error in Go.
    let neg = s.starts_with('-');
    match u128::from_str_radix(digits, radix) {
        Ok(v) if neg => v <= 1u128 << 63,
        Ok(v) => v <= u128::from(u64::MAX),
        Err(_) => false,
    }
}

/// `^[-+]?(\.[0-9]+|[0-9]+(\.[0-9]*)?)([eE][-+]?[0-9]+)?$`
fn is_yaml_style_float(s: &str) -> bool {
    let b = s.as_bytes();
    let mut i = 0;
    if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
        i += 1;
    }
    let digits = |i: &mut usize| {
        let start = *i;
        while *i < b.len() && b[*i].is_ascii_digit() {
            *i += 1;
        }
        *i - start
    };
    if i < b.len() && b[i] == b'.' {
        i += 1;
        if digits(&mut i) == 0 {
            return false;
        }
    } else {
        if digits(&mut i) == 0 {
            return false;
        }
        if i < b.len() && b[i] == b'.' {
            i += 1;
            digits(&mut i);
        }
    }
    if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
        i += 1;
        if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
            i += 1;
        }
        if digits(&mut i) == 0 {
            return false;
        }
    }
    i == b.len()
}

/// Go's `strconv.ParseFloat(s, 64)` succeeding: decimal or hex floats, inf/nan words, and
/// no overflow.
fn go_parse_float(s: &str) -> bool {
    let body = s.strip_prefix(['+', '-']).unwrap_or(s);
    let lower = body.to_ascii_lowercase();
    if matches!(lower.as_str(), "inf" | "infinity" | "nan") {
        return true;
    }
    if let Some(hex) = lower.strip_prefix("0x") {
        return is_hex_float(hex);
    }
    if !body
        .bytes()
        .all(|c| c.is_ascii_digit() || matches!(c, b'.' | b'e' | b'E' | b'+' | b'-'))
    {
        return false;
    }
    match body.parse::<f64>() {
        Ok(v) => v.is_finite(),
        Err(_) => false,
    }
}

fn is_hex_float(s: &str) -> bool {
    let Some((mant, exp)) = s.split_once('p') else {
        return false;
    };
    let exp = exp.strip_prefix(['+', '-']).unwrap_or(exp);
    let (a, b) = mant.split_once('.').unwrap_or((mant, ""));
    !(a.is_empty() && b.is_empty())
        && a.chars().chain(b.chars()).all(|c| c.is_ascii_hexdigit())
        && !exp.is_empty()
        && exp.chars().all(|c| c.is_ascii_digit())
}

/// go-yaml's `parseTimestamp`: four digits and a dash, then one of the allowed layouts
/// (`2006-1-2T15:4:5.999999999Z07:00`, the same with `t`, `2006-1-2 15:4:5.999999999`,
/// or `2006-1-2`).
fn is_timestamp(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() < 5 || !b[..4].iter().all(u8::is_ascii_digit) || b[4] != b'-' {
        return false;
    }
    let mut p = Parser { b, i: 5 };
    let Some(month) = p.num(1, 2) else {
        return false;
    };
    if !p.eat(b'-') {
        return false;
    }
    let Some(day) = p.num(1, 2) else { return false };
    if !(1..=12).contains(&month) || day < 1 || day > days_in_month(&b[..4], month) {
        return false;
    }
    if p.done() {
        return true;
    }
    let sep = b[p.i];
    p.i += 1;
    if !matches!(sep, b'T' | b't' | b' ') {
        return false;
    }
    let (Some(h), true, Some(m), true, Some(sec)) = (
        p.num(1, 2),
        p.eat(b':'),
        p.num(1, 2),
        p.eat(b':'),
        p.num(1, 2),
    ) else {
        return false;
    };
    if h > 23 || m > 59 || sec > 59 {
        return false;
    }
    if p.eat(b'.') && p.num(1, 9).is_none() {
        return false;
    }
    if sep == b' ' {
        return p.done();
    }
    if p.eat(b'Z') {
        return p.done();
    }
    if p.eat(b'+') || p.eat(b'-') {
        let (Some(zh), true, Some(zm)) = (p.num(2, 2), p.eat(b':'), p.num(2, 2)) else {
            return false;
        };
        return zh < 24 && zm < 60 && p.done();
    }
    false
}

fn days_in_month(year: &[u8], month: u32) -> u32 {
    let y: u32 = std::str::from_utf8(year)
        .ok()
        .and_then(|y| y.parse().ok())
        .unwrap_or(0);
    match month {
        2 if (y.is_multiple_of(4) && !y.is_multiple_of(100)) || y.is_multiple_of(400) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

struct Parser<'a> {
    b: &'a [u8],
    i: usize,
}

impl Parser<'_> {
    fn num(&mut self, min: usize, max: usize) -> Option<u32> {
        let start = self.i;
        while self.i < self.b.len() && self.i - start < max && self.b[self.i].is_ascii_digit() {
            self.i += 1;
        }
        if self.i - start < min {
            return None;
        }
        std::str::from_utf8(&self.b[start..self.i])
            .ok()?
            .parse()
            .ok()
    }
    fn eat(&mut self, c: u8) -> bool {
        if self.b.get(self.i) == Some(&c) {
            self.i += 1;
            true
        } else {
            false
        }
    }
    fn done(&self) -> bool {
        self.i == self.b.len()
    }
}

#[cfg(test)]
mod tests {
    use super::resolves_to_str;

    #[test]
    fn plain_scalars() {
        for s in [
            "abc",
            "yes",
            "on",
            "y",
            "0b",
            "1.2.3",
            "08x",
            "-",
            "1_a",
            "2020-13-01",
            "x123",
        ] {
            assert!(resolves_to_str(s), "{s}");
        }
        for s in [
            "",
            "~",
            "null",
            "true",
            "123",
            "-1",
            "0x1F",
            "017",
            "08",
            "1_000",
            "1e5",
            ".5",
            "+.inf",
            "2020-01-01",
            "2020-1-2T3:04:05Z",
            "2001-12-14 21:59:43.10",
            "0b101",
            "0o17",
            "1.",
            "99999999999999999999",
        ] {
            assert!(!resolves_to_str(s), "{s}");
        }
    }
}
