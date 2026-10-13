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
