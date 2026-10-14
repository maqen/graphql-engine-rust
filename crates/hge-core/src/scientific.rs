//! Haskell's `show` for numbers, as V2 error messages print them.

/// Haskell's `show` of the `Scientific` written as `s` (`formatScientific Generic Nothing`):
/// fixed notation for 0.1 <= |x| < 10^7, otherwise `d.ddde<exp>`; always a fractional digit.
pub fn scientific_show(s: &str) -> Option<String> {
    let (negative, s) = match s.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, s),
    };
    let (mantissa, exp) = match s.find(['e', 'E']) {
        Some(i) => (
            &s[..i],
            s[i + 1..].trim_start_matches('+').parse::<i64>().ok()?,
        ),
        None => (s, 0),
    };
    let (int, frac) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    if !(int.chars().chain(frac.chars())).all(|c| c.is_ascii_digit()) {
        return None;
    }
    // value = 0.<digits> * 10^e, digits without leading or trailing zeros.
    let all: String = format!("{int}{frac}");
    let leading = all.len() - all.trim_start_matches('0').len();
    let digits = all.trim_matches('0');
    let sign = if negative { "-" } else { "" };
    if digits.is_empty() {
        return Some(format!("{sign}0.0"));
    }
    let e = int.len() as i64 - leading as i64 + exp;
    let shown = if !(0..=7).contains(&e) {
        let (first, rest) = digits.split_at(1);
        let rest = if rest.is_empty() { "0" } else { rest };
        format!("{first}.{rest}e{}", e - 1)
    } else if e == 0 {
        format!("0.{digits}")
    } else {
        let e = e as usize;
        let int: String = digits
            .chars()
            .chain(std::iter::repeat('0'))
            .take(e)
            .collect();
        let frac = digits.get(e..).filter(|f| !f.is_empty()).unwrap_or("0");
        format!("{int}.{frac}")
    };
    Some(format!("{sign}{shown}"))
}

/// The text Postgres reads back from V2's binary encoding of the `Scientific` written as `s`
/// as a `numeric` (how V2 binds tracked function and native query arguments). The number is
/// not normalised: its fractional digits as written (`3.0` has one, `1.5e2` none) are padded
/// to whole base-10000 groups (`1.5` → `1.5000`, `3.0` → `3.0000`, `3` → `3`).
pub fn scientific_binary_numeric(s: &str) -> Option<String> {
    let (negative, s) = match s.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, s),
    };
    let (mantissa, exp) = match s.find(['e', 'E']) {
        Some(i) => (
            &s[..i],
            s[i + 1..].trim_start_matches('+').parse::<i64>().ok()?,
        ),
        None => (s, 0),
    };
    let (int, frac) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    if int.len() + frac.len() == 0
        || !(int.chars().chain(frac.chars())).all(|c| c.is_ascii_digit())
        // Beyond Postgres' numeric range (and a bound on the zeros written below).
        || exp.unsigned_abs() > 200_000
    {
        return None;
    }
    // value = coefficient * 10^exp, as written.
    let all = format!("{int}{frac}");
    let coefficient = all.trim_start_matches('0');
    let exp = exp - frac.len() as i64;
    let (int_part, frac_part) = if exp >= 0 {
        let int = match coefficient {
            "" => "0".to_owned(),
            c => format!("{c}{}", "0".repeat(exp as usize)),
        };
        (int, String::new())
    } else {
        let k = exp.unsigned_abs() as usize;
        match coefficient.len().checked_sub(k) {
            Some(n) if n > 0 => (coefficient[..n].to_owned(), coefficient[n..].to_owned()),
            _ => (
                "0".to_owned(),
                "0".repeat(k - coefficient.len()) + coefficient,
            ),
        }
    };
    let sign = if negative && !coefficient.is_empty() {
        "-"
    } else {
        ""
    };
    if frac_part.is_empty() {
        return Some(format!("{sign}{int_part}"));
    }
    let scale = frac_part.len().div_ceil(4) * 4;
    Some(format!("{sign}{int_part}.{frac_part:0<scale$}"))
}

#[cfg(test)]
mod binary_numeric_tests {
    use super::scientific_binary_numeric;

    #[test]
    fn pads_the_scale_to_base_10000_groups() {
        for (input, text) in [
            ("1.5", "1.5000"),
            ("3", "3"),
            ("3.0", "3.0000"),
            ("150.0", "150.0000"),
            ("-0.0", "0.0000"),
            ("1.50", "1.5000"),
            ("1.25", "1.2500"),
            ("1.123456789", "1.123456789000"),
            ("1e20", "100000000000000000000"),
            ("0.1", "0.1000"),
            ("-2.5", "-2.5000"),
            ("1e-5", "0.00001000"),
            ("12345.678", "12345.6780"),
            ("0", "0"),
            ("1.5E+2", "150"),
        ] {
            assert_eq!(
                scientific_binary_numeric(input).as_deref(),
                Some(text),
                "{input}"
            );
        }
        for invalid in [
            "x",
            "",
            ".",
            "e5",
            "1e9223372036854775807",
            "1.5e-9223372036854775808",
        ] {
            assert_eq!(scientific_binary_numeric(invalid), None, "{invalid}");
        }
    }
}
