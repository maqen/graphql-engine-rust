//! Cron schedules with the exact semantics of the Haskell `cron` package,
//! version 0.7.0, which is what Hasura GraphQL Engine v2 (v2.51.0 pins
//! `cron ==0.7.0` in `cabal.project.freeze`) uses for cron triggers.
//!
//! Hasura parses a trigger's `schedule` with `System.Cron.parseCronSchedule`
//! (no trimming or other pre-processing, see `Hasura.Base.Instances`) and
//! renders it back with `serializeCronSchedule`. Upcoming events are computed
//! with `System.Cron.Schedule.nextMatch`. This module reproduces all three.
//!
//! Everything here was checked against the package source
//! (`System.Cron.Parser`, `System.Cron.Types`, `System.Cron.Internal.Check`):
//!
//! * The whole input is lower-cased first (`toLower`), so `@DAILY`, `JAN` and
//!   `Mon` are accepted.
//! * Keywords: `@yearly`, `@monthly`, `@weekly`, `@daily`, `@hourly` only.
//!   They must be the entire input (no surrounding whitespace).
//! * Otherwise exactly five fields separated by exactly one ASCII space
//!   (`A.char ' '`), followed by end of input. Leading/trailing whitespace,
//!   tabs and double spaces are rejected by Hasura. [`CronSchedule::parse`]
//!   reproduces that strictly; [`CronSchedule::parse_lenient`] additionally
//!   trims the input and accepts any run of whitespace between fields.
//! * A field is one of: `*`, `n`, `a-b`, a comma list of at least two of
//!   those, or a single *stepped* base field `base/step` where `base` is `*`,
//!   `n` or `a-b`. Steps cannot appear inside lists (`*/15,30` is invalid).
//! * `n/step` IS valid: it means `n, n+step, ...` up to the field maximum.
//!   `a-b/step` means `a, a+step, ...` up to `b`.
//! * Month names `jan`..`dec` are accepted in the month field and day names
//!   `mon`..`sun` (mon = 1 ... sun = 7) in the day-of-week field, also as
//!   range bounds. They serialise back as numbers.
//! * Ranges: minute 0-59, hour 0-23, day-of-month 1-31, month 1-12,
//!   day-of-week 0-7 (0 and 7 are both Sunday). Range bounds must satisfy
//!   `a <= b`. The step must be > 0 *and* lie inside the field's range
//!   (`validCF`: `inRange (mn, mx) step`), so `*/60` minutes, `*/24` hours,
//!   `*/0` and `*/32` day-of-month are all invalid; `*/7` day-of-week is fine.
//! * Day-of-month / day-of-week: a field counts as *unrestricted* ("star")
//!   when it is `*`, `*/1`, or a list containing `*` (`isStar` in
//!   `Internal.Check`). When BOTH are restricted a day matches if EITHER
//!   matches (Vixie cron OR); otherwise both must match (the unrestricted one
//!   trivially does).
//! * `nextMatch` returns the first whole minute strictly after the given
//!   instant, all in UTC. It returns `Nothing` only when the schedule is
//!   statically unsatisfiable: when the day fields are not both restricted
//!   and no selected month can contain the smallest selected day of month
//!   (e.g. `0 0 30 2 *`, `0 0 31 4,6 *`). Otherwise a match always exists
//!   (the worst case is Feb 29, at most 8 years away, e.g. 2096 -> 2104).
//! * Serialisation re-renders the parsed AST: fields joined by single
//!   spaces, keywords expanded (`@weekly` -> `0 0 * * 0`), numbers printed in
//!   decimal without leading zeros (`05` -> `5`), names as numbers, `7` kept
//!   as `7`, lists comma-joined in the original order, steps as `base/step`.

use std::fmt;

const SECS_PER_MIN: i64 = 60;
const SECS_PER_HOUR: i64 = 3_600;
const SECS_PER_DAY: i64 = 86_400;
/// Safety bound for the search loop. Satisfiable schedules always match
/// within 8 years; this only guards against bugs/extreme inputs.
const SEARCH_HORIZON_DAYS: i64 = 366 * 30;

/// Which of the five positions a field occupies (drives ranges and names).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Minute,
    Hour,
    DayOfMonth,
    Month,
    DayOfWeek,
}

impl Kind {
    fn bounds(self) -> (u32, u32) {
        match self {
            Kind::Minute => (0, 59),
            Kind::Hour => (0, 23),
            Kind::DayOfMonth => (1, 31),
            Kind::Month => (1, 12),
            Kind::DayOfWeek => (0, 7),
        }
    }

    /// Error message used by the Haskell parser for an invalid field.
    fn out_of_range_msg(self) -> &'static str {
        match self {
            Kind::Minute => "minutes out of range",
            Kind::Hour => "hours out of range",
            Kind::DayOfMonth => "day of month out of range",
            Kind::Month => "month out of range",
            Kind::DayOfWeek => "day of week out of range",
        }
    }

    fn name(self) -> &'static str {
        match self {
            Kind::Minute => "minute",
            Kind::Hour => "hour",
            Kind::DayOfMonth => "day-of-month",
            Kind::Month => "month",
            Kind::DayOfWeek => "day-of-week",
        }
    }
}

/// `BaseField` from `System.Cron.Types`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BaseField {
    Star,
    Specific(u32),
    Range(u32, u32),
}

/// `CronField` from `System.Cron.Types`.
#[derive(Clone, Debug, PartialEq, Eq)]
enum CronField {
    Field(BaseField),
    /// Always holds at least two elements (singleton lists do not parse).
    List(Vec<BaseField>),
    Step(BaseField, u32),
}

impl CronField {
    /// `isStar` from `System.Cron.Internal.Check`.
    fn is_star(&self) -> bool {
        match self {
            CronField::Field(BaseField::Star) => true,
            CronField::List(items) => items.contains(&BaseField::Star),
            CronField::Step(BaseField::Star, 1) => true,
            _ => false,
        }
    }

    /// `expandF`: the set of matching values as a bit mask (bit `v` set when
    /// value `v` matches), using the field's `(lo, hi)` bounds.
    fn expand(&self, (lo, hi): (u32, u32)) -> u64 {
        fn base(bf: BaseField, lo: u32, hi: u32) -> u64 {
            match bf {
                BaseField::Star => range_mask(lo, hi, 1),
                BaseField::Specific(n) => 1u64 << n,
                BaseField::Range(a, b) => range_mask(a, b, 1),
            }
        }
        match self {
            CronField::Field(bf) => base(*bf, lo, hi),
            CronField::List(items) => items.iter().fold(0, |m, bf| m | base(*bf, lo, hi)),
            // `expandBFStepped`
            CronField::Step(bf, step) => match *bf {
                BaseField::Star => range_mask(lo, hi, *step),
                BaseField::Range(a, b) => range_mask(a, b.min(hi), *step),
                BaseField::Specific(n) => range_mask(n, hi, *step),
            },
        }
    }
}

/// Bits `start, start+step, ...` up to and including `end` (`fillTo`).
fn range_mask(start: u32, end: u32, step: u32) -> u64 {
    let mut mask = 0u64;
    let mut v = start;
    while v <= end && v < 64 {
        mask |= 1u64 << v;
        v += step;
    }
    mask
}

impl fmt::Display for BaseField {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BaseField::Star => f.write_str("*"),
            BaseField::Specific(n) => write!(f, "{n}"),
            BaseField::Range(a, b) => write!(f, "{a}-{b}"),
        }
    }
}

impl fmt::Display for CronField {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CronField::Field(bf) => bf.fmt(f),
            CronField::List(items) => {
                for (i, bf) in items.iter().enumerate() {
                    if i > 0 {
                        f.write_str(",")?;
                    }
                    bf.fmt(f)?;
                }
                Ok(())
            }
            CronField::Step(bf, step) => write!(f, "{bf}/{step}"),
        }
    }
}

/// A parsed cron schedule (Haskell `System.Cron.Types.CronSchedule`), plus a
/// pre-compiled bit-mask form used for fast matching.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CronSchedule {
    minute: CronField,
    hour: CronField,
    day_of_month: CronField,
    month: CronField,
    day_of_week: CronField,
    compiled: Compiled,
}

/// Expanded value sets (`Expanded` in `Internal.Check`) as bit masks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Compiled {
    minutes: u64,
    hours: u64,
    /// bits 1..=31
    doms: u64,
    /// bits 1..=12
    months: u64,
    /// bits 0..=6, Sunday = 0 (7 folded into 0)
    dows: u64,
    dom_restricted: bool,
    dow_restricted: bool,
    /// `satisfiable` from `expand`; `false` means `nextMatch` is `Nothing`.
    satisfiable: bool,
}

impl CronSchedule {
    /// Parse exactly like Haskell `parseCronSchedule` (strict: no surrounding
    /// whitespace, single spaces between the five fields).
    pub fn parse(s: &str) -> Result<CronSchedule, String> {
        let lower = s.to_lowercase();
        if let Some(sched) = Self::keyword(&lower) {
            return Ok(sched);
        }
        Self::parse_fields(lower.split(' ').collect())
    }

    /// Like [`CronSchedule::parse`] but trims surrounding whitespace and
    /// accepts any run of whitespace between fields. Hasura itself rejects
    /// such input; use this only where being liberal is desired.
    pub fn parse_lenient(s: &str) -> Result<CronSchedule, String> {
        let lower = s.trim().to_lowercase();
        if let Some(sched) = Self::keyword(&lower) {
            return Ok(sched);
        }
        Self::parse_fields(lower.split_whitespace().collect())
    }

    /// [`CronSchedule::parse`] with the attoparsec messages V2 reports for a bad schedule
    /// (`Failed reading: takeWhile1`, `' ': not enough input`, `endOfInput`, ...).
    pub fn parse_v2(s: &str) -> Result<CronSchedule, String> {
        CronSchedule::parse(s).map_err(|_| {
            let lower = s.to_lowercase();
            let parts: Vec<&str> = lower.split(' ').collect();
            let kinds = [
                Kind::Minute,
                Kind::Hour,
                Kind::DayOfMonth,
                Kind::Month,
                Kind::DayOfWeek,
            ];
            for (i, kind) in kinds.into_iter().enumerate() {
                let Some(part) = parts.get(i) else {
                    return "' ': not enough input".to_owned();
                };
                if let Err(e) = parse_field(part, kind) {
                    return if e.starts_with(kind.out_of_range_msg()) {
                        format!("Failed reading: {}", kind.out_of_range_msg())
                    } else {
                        "Failed reading: takeWhile1".to_owned()
                    };
                }
            }
            "endOfInput".to_owned()
        })
    }

    fn keyword(s: &str) -> Option<CronSchedule> {
        let spec = match s {
            "@yearly" => "0 0 1 1 *",
            "@monthly" => "0 0 1 * *",
            "@weekly" => "0 0 * * 0",
            "@daily" => "0 0 * * *",
            "@hourly" => "0 * * * *",
            _ => return None,
        };
        Self::parse_fields(spec.split(' ').collect()).ok()
    }

    fn parse_fields(parts: Vec<&str>) -> Result<CronSchedule, String> {
        if parts.len() != 5 {
            return Err(format!(
                "invalid cron schedule: expected 5 space-separated fields \
                 (minute hour day-of-month month day-of-week), got {}",
                parts.len()
            ));
        }
        let minute = parse_field(parts[0], Kind::Minute)?;
        let hour = parse_field(parts[1], Kind::Hour)?;
        let day_of_month = parse_field(parts[2], Kind::DayOfMonth)?;
        let month = parse_field(parts[3], Kind::Month)?;
        let day_of_week = parse_field(parts[4], Kind::DayOfWeek)?;
        let compiled = compile(&minute, &hour, &day_of_month, &month, &day_of_week);
        Ok(CronSchedule {
            minute,
            hour,
            day_of_month,
            month,
            day_of_week,
            compiled,
        })
    }

    /// `scheduleMatches`: does the minute containing `t` match?
    pub fn matches(&self, t: i64) -> bool {
        let c = &self.compiled;
        if !c.satisfiable {
            return false;
        }
        let days = t.div_euclid(SECS_PER_DAY);
        let sod = t.rem_euclid(SECS_PER_DAY);
        let (_, m, d) = civil_from_days(days);
        bit(c.hours, (sod / SECS_PER_HOUR) as u32)
            && bit(c.minutes, ((sod % SECS_PER_HOUR) / SECS_PER_MIN) as u32)
            && bit(c.months, m)
            && self.day_matches(days, d)
    }

    fn day_matches(&self, days: i64, dom: u32) -> bool {
        let c = &self.compiled;
        let dom_ok = bit(c.doms, dom);
        let dow_ok = bit(c.dows, weekday(days));
        if c.dom_restricted && c.dow_restricted {
            dom_ok || dow_ok
        } else {
            dom_ok && dow_ok
        }
    }

    /// First matching minute strictly after `after` (Unix seconds, UTC).
    /// The result always has seconds = 0. `None` when the schedule can never
    /// match (Haskell `nextMatch` returning `Nothing`).
    pub fn next_after(&self, after: i64) -> Option<i64> {
        let c = &self.compiled;
        if !c.satisfiable {
            return None;
        }
        let start = after
            .div_euclid(SECS_PER_MIN)
            .checked_mul(SECS_PER_MIN)?
            .checked_add(SECS_PER_MIN)?;
        let mut days = start.div_euclid(SECS_PER_DAY);
        let sod = start.rem_euclid(SECS_PER_DAY);
        // Earliest hour/minute still allowed on the current day.
        let mut min_hour = (sod / SECS_PER_HOUR) as u32;
        let mut min_minute = ((sod % SECS_PER_HOUR) / SECS_PER_MIN) as u32;
        let end = days.checked_add(SEARCH_HORIZON_DAYS)?;

        while days <= end {
            let (y, m, d) = civil_from_days(days);
            if !bit(c.months, m) {
                // Skip to the first day of the next month.
                let (ny, nm) = if m == 12 { (y + 1, 1) } else { (y, m + 1) };
                days = days_from_civil(ny, nm, 1);
                min_hour = 0;
                min_minute = 0;
                continue;
            }
            if self.day_matches(days, d)
                && let Some((hh, mm)) = Self::first_time_of_day(c, min_hour, min_minute)
            {
                return days
                    .checked_mul(SECS_PER_DAY)?
                    .checked_add(i64::from(hh) * SECS_PER_HOUR + i64::from(mm) * SECS_PER_MIN);
            }
            days += 1;
            min_hour = 0;
            min_minute = 0;
        }
        None
    }

    /// Earliest matching (hour, minute) at or after `min_hour:min_minute`.
    fn first_time_of_day(c: &Compiled, min_hour: u32, min_minute: u32) -> Option<(u32, u32)> {
        let h = next_bit(c.hours, min_hour)?;
        let from_minute = if h == min_hour { min_minute } else { 0 };
        next_bit(c.minutes, from_minute)
            .map(|mi| (h, mi))
            // No minute left in hour `h`: first minute of the next valid hour.
            .or_else(|| next_bit(c.hours, h + 1).map(|h2| (h2, c.minutes.trailing_zeros())))
    }

    /// `n` successive matches strictly after `after`.
    pub fn next_n(&self, after: i64, n: usize) -> Vec<i64> {
        let mut out = Vec::with_capacity(n.min(1024));
        let mut cur = after;
        while out.len() < n {
            match self.next_after(cur) {
                Some(t) => {
                    out.push(t);
                    cur = t;
                }
                None => break,
            }
        }
        out
    }
}

/// `serializeCronSchedule`.
impl fmt::Display for CronSchedule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} {} {} {} {}",
            self.minute, self.hour, self.day_of_month, self.month, self.day_of_week
        )
    }
}

impl std::str::FromStr for CronSchedule {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        CronSchedule::parse(s)
    }
}

fn compile(
    minute: &CronField,
    hour: &CronField,
    dom: &CronField,
    month: &CronField,
    dow: &CronField,
) -> Compiled {
    let mut dows = dow.expand(Kind::DayOfWeek.bounds());
    // `remapSunday`: 7 means Sunday (0).
    if dows & (1 << 7) != 0 {
        dows = (dows & !(1 << 7)) | 1;
    }
    let doms = dom.expand(Kind::DayOfMonth.bounds());
    let months = month.expand(Kind::Month.bounds());
    let dom_restricted = !dom.is_star();
    let dow_restricted = !dow.is_star();
    // `satisfiable`: with the OR rule some weekday always exists; otherwise
    // a selected month must be long enough for the smallest selected day.
    let min_dom = doms.trailing_zeros();
    let satisfiable = (dom_restricted && dow_restricted)
        || (1..=12u32).any(|m| bit(months, m) && min_dom <= max_days_in_month(m));
    Compiled {
        minutes: minute.expand(Kind::Minute.bounds()),
        hours: hour.expand(Kind::Hour.bounds()),
        doms,
        months,
        dows,
        dom_restricted,
        dow_restricted,
        satisfiable,
    }
}

fn max_days_in_month(m: u32) -> u32 {
    match m {
        2 => 29,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

fn parse_field(text: &str, kind: Kind) -> Result<CronField, String> {
    let field = if let Some((base, step)) = text.split_once('/') {
        let base = parse_base(base, kind)?;
        let step = parse_decimal(step)
            .ok_or_else(|| format!("invalid stepping in {} field: {text:?}", kind.name()))?;
        if step == 0 {
            return Err(format!(
                "invalid stepping in {} field: {text:?}",
                kind.name()
            ));
        }
        CronField::Step(base, step)
    } else if text.contains(',') {
        let items = text
            .split(',')
            .map(|p| parse_base(p, kind))
            .collect::<Result<Vec<_>, _>>()?;
        CronField::List(items)
    } else {
        CronField::Field(parse_base(text, kind)?)
    };
    if !valid_field(&field, kind) {
        return Err(format!("{}: {text:?}", kind.out_of_range_msg()));
    }
    Ok(field)
}

/// `validCF`.
fn valid_field(field: &CronField, kind: Kind) -> bool {
    let (lo, hi) = kind.bounds();
    let in_range = |v: u32| (lo..=hi).contains(&v);
    let valid_base = |bf: &BaseField| match *bf {
        BaseField::Star => true,
        BaseField::Specific(n) => in_range(n),
        BaseField::Range(a, b) => in_range(a) && in_range(b),
    };
    match field {
        CronField::Field(bf) => valid_base(bf),
        CronField::List(items) => items.iter().all(valid_base),
        CronField::Step(bf, step) => valid_base(bf) && in_range(*step),
    }
}

fn parse_base(text: &str, kind: Kind) -> Result<BaseField, String> {
    if text == "*" {
        return Ok(BaseField::Star);
    }
    let bad = || format!("invalid {} field: {text:?}", kind.name());
    if let Some((a, b)) = text.split_once('-') {
        let a = parse_value(a, kind).ok_or_else(bad)?;
        let b = parse_value(b, kind).ok_or_else(bad)?;
        if a > b {
            return Err(format!(
                "start of range must be less than or equal to end in {} field: {text:?}",
                kind.name()
            ));
        }
        return Ok(BaseField::Range(a, b));
    }
    parse_value(text, kind)
        .map(BaseField::Specific)
        .ok_or_else(bad)
}

/// `supportParser`: a decimal, or a (lower-case) month/day name.
fn parse_value(text: &str, kind: Kind) -> Option<u32> {
    const MONTHS: [&str; 12] = [
        "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
    ];
    const DAYS: [&str; 7] = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];
    let names: &[&str] = match kind {
        Kind::Month => &MONTHS,
        Kind::DayOfWeek => &DAYS,
        _ => &[],
    };
    if let Some(i) = names.iter().position(|n| *n == text) {
        return Some(i as u32 + 1);
    }
    parse_decimal(text)
}

/// Attoparsec `decimal`: one or more ASCII digits. Values too large for a
/// `u32` are treated as invalid (they would be out of range anyway).
fn parse_decimal(text: &str) -> Option<u32> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    // Leading zeros are allowed; cap length to avoid overflow surprises.
    let trimmed = text.trim_start_matches('0');
    if trimmed.len() > 9 {
        return Some(u32::MAX);
    }
    Some(trimmed.parse().unwrap_or(0))
}

fn bit(mask: u64, v: u32) -> bool {
    v < 64 && mask & (1u64 << v) != 0
}

/// Smallest set bit `>= from`.
fn next_bit(mask: u64, from: u32) -> Option<u32> {
    if from >= 64 {
        return None;
    }
    let rest = mask >> from;
    (rest != 0).then(|| from + rest.trailing_zeros())
}

/// Day of week for days since 1970-01-01 (a Thursday); Sunday = 0.
fn weekday(days: i64) -> u32 {
    (days + 4).rem_euclid(7) as u32
}

/// Howard Hinnant's `days_from_civil`.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let m = i64::from(m);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Howard Hinnant's `civil_from_days`: (year, month 1-12, day 1-31).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Format Unix seconds as `YYYY-MM-DDTHH:MM:SSZ` (UTC).
pub fn format_utc(secs: i64) -> String {
    let days = secs.div_euclid(SECS_PER_DAY);
    let sod = secs.rem_euclid(SECS_PER_DAY);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        sod / SECS_PER_HOUR,
        (sod % SECS_PER_HOUR) / SECS_PER_MIN,
        sod % SECS_PER_MIN
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v2_messages() {
        let err = |s: &str| CronSchedule::parse_v2(s).unwrap_err();
        assert_eq!(err("garbage"), "Failed reading: takeWhile1");
        assert_eq!(err("* * * *"), "' ': not enough input");
        assert_eq!(err("61 * * * *"), "Failed reading: minutes out of range");
        assert_eq!(err("* * * * * *"), "endOfInput");
        assert_eq!(
            CronSchedule::parse_v2("@daily").unwrap().to_string(),
            "0 0 * * *"
        );
    }

    /// Parse `YYYY-MM-DDTHH:MM:SSZ`.
    fn ts(s: &str) -> i64 {
        let b = s.as_bytes();
        assert_eq!(b.len(), 20, "bad timestamp {s}");
        let n = |r: std::ops::Range<usize>| s[r].parse::<i64>().unwrap();
        let days = days_from_civil(n(0..4), n(5..7) as u32, n(8..10) as u32);
        days * SECS_PER_DAY + n(11..13) * 3600 + n(14..16) * 60 + n(17..19)
    }

    fn next5(expr: &str, start: &str) -> Vec<String> {
        CronSchedule::parse(expr)
            .unwrap_or_else(|e| panic!("{expr}: {e}"))
            .next_n(ts(start), 5)
            .into_iter()
            .map(format_utc)
            .collect()
    }

    // ---------------------------------------------------------------- civil

    #[test]
    fn civil_round_trip() {
        for days in (-800_000..800_000).step_by(37) {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m, d), days);
        }
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(weekday(0), 4); // Thursday
        assert_eq!(weekday(days_from_civil(2024, 1, 7)), 0); // Sunday
    }

    #[test]
    fn format_utc_works() {
        assert_eq!(format_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_utc(1_709_164_800), "2024-02-29T00:00:00Z");
        assert_eq!(format_utc(-1), "1969-12-31T23:59:59Z");
        assert_eq!(ts("2024-02-29T00:00:00Z"), 1_709_164_800);
    }

    // ------------------------------------------------------------- oracle

    /// Golden values produced by Python `croniter` 6.2.4 (see the oracle
    /// script `gen.py`). None of these expressions use `*/1` or a list
    /// containing `*` in the day fields, where croniter and Haskell `cron`
    /// could disagree on the DOM/DOW OR rule, so they agree everywhere.
    #[rustfmt::skip]
    const GOLDEN: &[(&str, &str, [&str; 5])] = &[
    ("* * * * *", "2024-01-01T00:00:00Z", ["2024-01-01T00:01:00Z", "2024-01-01T00:02:00Z", "2024-01-01T00:03:00Z", "2024-01-01T00:04:00Z", "2024-01-01T00:05:00Z"]),
    ("* * * * *", "2024-02-28T23:59:30Z", ["2024-02-29T00:00:00Z", "2024-02-29T00:01:00Z", "2024-02-29T00:02:00Z", "2024-02-29T00:03:00Z", "2024-02-29T00:04:00Z"]),
    ("* * * * *", "2025-12-31T23:59:00Z", ["2026-01-01T00:00:00Z", "2026-01-01T00:01:00Z", "2026-01-01T00:02:00Z", "2026-01-01T00:03:00Z", "2026-01-01T00:04:00Z"]),
    ("* * * * *", "2023-06-15T12:34:56Z", ["2023-06-15T12:35:00Z", "2023-06-15T12:36:00Z", "2023-06-15T12:37:00Z", "2023-06-15T12:38:00Z", "2023-06-15T12:39:00Z"]),
    ("*/7 * * * *", "2024-01-01T00:00:00Z", ["2024-01-01T00:07:00Z", "2024-01-01T00:14:00Z", "2024-01-01T00:21:00Z", "2024-01-01T00:28:00Z", "2024-01-01T00:35:00Z"]),
    ("*/7 * * * *", "2024-02-28T23:59:30Z", ["2024-02-29T00:00:00Z", "2024-02-29T00:07:00Z", "2024-02-29T00:14:00Z", "2024-02-29T00:21:00Z", "2024-02-29T00:28:00Z"]),
    ("*/7 * * * *", "2025-12-31T23:59:00Z", ["2026-01-01T00:00:00Z", "2026-01-01T00:07:00Z", "2026-01-01T00:14:00Z", "2026-01-01T00:21:00Z", "2026-01-01T00:28:00Z"]),
    ("*/7 * * * *", "2023-06-15T12:34:56Z", ["2023-06-15T12:35:00Z", "2023-06-15T12:42:00Z", "2023-06-15T12:49:00Z", "2023-06-15T12:56:00Z", "2023-06-15T13:00:00Z"]),
    ("*/15 * * * *", "2024-01-01T00:00:00Z", ["2024-01-01T00:15:00Z", "2024-01-01T00:30:00Z", "2024-01-01T00:45:00Z", "2024-01-01T01:00:00Z", "2024-01-01T01:15:00Z"]),
    ("*/15 * * * *", "2024-02-28T23:59:30Z", ["2024-02-29T00:00:00Z", "2024-02-29T00:15:00Z", "2024-02-29T00:30:00Z", "2024-02-29T00:45:00Z", "2024-02-29T01:00:00Z"]),
    ("*/15 * * * *", "2025-12-31T23:59:00Z", ["2026-01-01T00:00:00Z", "2026-01-01T00:15:00Z", "2026-01-01T00:30:00Z", "2026-01-01T00:45:00Z", "2026-01-01T01:00:00Z"]),
    ("*/15 * * * *", "2023-06-15T12:34:56Z", ["2023-06-15T12:45:00Z", "2023-06-15T13:00:00Z", "2023-06-15T13:15:00Z", "2023-06-15T13:30:00Z", "2023-06-15T13:45:00Z"]),
    ("5 * * * *", "2024-01-01T00:00:00Z", ["2024-01-01T00:05:00Z", "2024-01-01T01:05:00Z", "2024-01-01T02:05:00Z", "2024-01-01T03:05:00Z", "2024-01-01T04:05:00Z"]),
    ("5 * * * *", "2024-02-28T23:59:30Z", ["2024-02-29T00:05:00Z", "2024-02-29T01:05:00Z", "2024-02-29T02:05:00Z", "2024-02-29T03:05:00Z", "2024-02-29T04:05:00Z"]),
    ("5 * * * *", "2025-12-31T23:59:00Z", ["2026-01-01T00:05:00Z", "2026-01-01T01:05:00Z", "2026-01-01T02:05:00Z", "2026-01-01T03:05:00Z", "2026-01-01T04:05:00Z"]),
    ("5 * * * *", "2023-06-15T12:34:56Z", ["2023-06-15T13:05:00Z", "2023-06-15T14:05:00Z", "2023-06-15T15:05:00Z", "2023-06-15T16:05:00Z", "2023-06-15T17:05:00Z"]),
    ("0 * * * *", "2024-01-01T00:00:00Z", ["2024-01-01T01:00:00Z", "2024-01-01T02:00:00Z", "2024-01-01T03:00:00Z", "2024-01-01T04:00:00Z", "2024-01-01T05:00:00Z"]),
    ("0 * * * *", "2024-02-28T23:59:30Z", ["2024-02-29T00:00:00Z", "2024-02-29T01:00:00Z", "2024-02-29T02:00:00Z", "2024-02-29T03:00:00Z", "2024-02-29T04:00:00Z"]),
    ("0 * * * *", "2025-12-31T23:59:00Z", ["2026-01-01T00:00:00Z", "2026-01-01T01:00:00Z", "2026-01-01T02:00:00Z", "2026-01-01T03:00:00Z", "2026-01-01T04:00:00Z"]),
    ("0 * * * *", "2023-06-15T12:34:56Z", ["2023-06-15T13:00:00Z", "2023-06-15T14:00:00Z", "2023-06-15T15:00:00Z", "2023-06-15T16:00:00Z", "2023-06-15T17:00:00Z"]),
    ("0 0 * * *", "2024-01-01T00:00:00Z", ["2024-01-02T00:00:00Z", "2024-01-03T00:00:00Z", "2024-01-04T00:00:00Z", "2024-01-05T00:00:00Z", "2024-01-06T00:00:00Z"]),
    ("0 0 * * *", "2024-02-28T23:59:30Z", ["2024-02-29T00:00:00Z", "2024-03-01T00:00:00Z", "2024-03-02T00:00:00Z", "2024-03-03T00:00:00Z", "2024-03-04T00:00:00Z"]),
    ("0 0 * * *", "2025-12-31T23:59:00Z", ["2026-01-01T00:00:00Z", "2026-01-02T00:00:00Z", "2026-01-03T00:00:00Z", "2026-01-04T00:00:00Z", "2026-01-05T00:00:00Z"]),
    ("0 0 * * *", "2023-06-15T12:34:56Z", ["2023-06-16T00:00:00Z", "2023-06-17T00:00:00Z", "2023-06-18T00:00:00Z", "2023-06-19T00:00:00Z", "2023-06-20T00:00:00Z"]),
    ("30 6 * * *", "2024-01-01T00:00:00Z", ["2024-01-01T06:30:00Z", "2024-01-02T06:30:00Z", "2024-01-03T06:30:00Z", "2024-01-04T06:30:00Z", "2024-01-05T06:30:00Z"]),
    ("30 6 * * *", "2024-02-28T23:59:30Z", ["2024-02-29T06:30:00Z", "2024-03-01T06:30:00Z", "2024-03-02T06:30:00Z", "2024-03-03T06:30:00Z", "2024-03-04T06:30:00Z"]),
    ("30 6 * * *", "2025-12-31T23:59:00Z", ["2026-01-01T06:30:00Z", "2026-01-02T06:30:00Z", "2026-01-03T06:30:00Z", "2026-01-04T06:30:00Z", "2026-01-05T06:30:00Z"]),
    ("30 6 * * *", "2023-06-15T12:34:56Z", ["2023-06-16T06:30:00Z", "2023-06-17T06:30:00Z", "2023-06-18T06:30:00Z", "2023-06-19T06:30:00Z", "2023-06-20T06:30:00Z"]),
    ("0 12 * * 1-5", "2024-01-01T00:00:00Z", ["2024-01-01T12:00:00Z", "2024-01-02T12:00:00Z", "2024-01-03T12:00:00Z", "2024-01-04T12:00:00Z", "2024-01-05T12:00:00Z"]),
    ("0 12 * * 1-5", "2024-02-28T23:59:30Z", ["2024-02-29T12:00:00Z", "2024-03-01T12:00:00Z", "2024-03-04T12:00:00Z", "2024-03-05T12:00:00Z", "2024-03-06T12:00:00Z"]),
    ("0 12 * * 1-5", "2025-12-31T23:59:00Z", ["2026-01-01T12:00:00Z", "2026-01-02T12:00:00Z", "2026-01-05T12:00:00Z", "2026-01-06T12:00:00Z", "2026-01-07T12:00:00Z"]),
    ("0 12 * * 1-5", "2023-06-15T12:34:56Z", ["2023-06-16T12:00:00Z", "2023-06-19T12:00:00Z", "2023-06-20T12:00:00Z", "2023-06-21T12:00:00Z", "2023-06-22T12:00:00Z"]),
    ("0 0 * * 0", "2024-01-01T00:00:00Z", ["2024-01-07T00:00:00Z", "2024-01-14T00:00:00Z", "2024-01-21T00:00:00Z", "2024-01-28T00:00:00Z", "2024-02-04T00:00:00Z"]),
    ("0 0 * * 0", "2024-02-28T23:59:30Z", ["2024-03-03T00:00:00Z", "2024-03-10T00:00:00Z", "2024-03-17T00:00:00Z", "2024-03-24T00:00:00Z", "2024-03-31T00:00:00Z"]),
    ("0 0 * * 0", "2025-12-31T23:59:00Z", ["2026-01-04T00:00:00Z", "2026-01-11T00:00:00Z", "2026-01-18T00:00:00Z", "2026-01-25T00:00:00Z", "2026-02-01T00:00:00Z"]),
    ("0 0 * * 0", "2023-06-15T12:34:56Z", ["2023-06-18T00:00:00Z", "2023-06-25T00:00:00Z", "2023-07-02T00:00:00Z", "2023-07-09T00:00:00Z", "2023-07-16T00:00:00Z"]),
    ("0 0 * * 7", "2024-01-01T00:00:00Z", ["2024-01-07T00:00:00Z", "2024-01-14T00:00:00Z", "2024-01-21T00:00:00Z", "2024-01-28T00:00:00Z", "2024-02-04T00:00:00Z"]),
    ("0 0 * * 7", "2024-02-28T23:59:30Z", ["2024-03-03T00:00:00Z", "2024-03-10T00:00:00Z", "2024-03-17T00:00:00Z", "2024-03-24T00:00:00Z", "2024-03-31T00:00:00Z"]),
    ("0 0 * * 7", "2025-12-31T23:59:00Z", ["2026-01-04T00:00:00Z", "2026-01-11T00:00:00Z", "2026-01-18T00:00:00Z", "2026-01-25T00:00:00Z", "2026-02-01T00:00:00Z"]),
    ("0 0 * * 7", "2023-06-15T12:34:56Z", ["2023-06-18T00:00:00Z", "2023-06-25T00:00:00Z", "2023-07-02T00:00:00Z", "2023-07-09T00:00:00Z", "2023-07-16T00:00:00Z"]),
    ("0 0 * * 6,0", "2024-01-01T00:00:00Z", ["2024-01-06T00:00:00Z", "2024-01-07T00:00:00Z", "2024-01-13T00:00:00Z", "2024-01-14T00:00:00Z", "2024-01-20T00:00:00Z"]),
    ("0 0 * * 6,0", "2024-02-28T23:59:30Z", ["2024-03-02T00:00:00Z", "2024-03-03T00:00:00Z", "2024-03-09T00:00:00Z", "2024-03-10T00:00:00Z", "2024-03-16T00:00:00Z"]),
    ("0 0 * * 6,0", "2025-12-31T23:59:00Z", ["2026-01-03T00:00:00Z", "2026-01-04T00:00:00Z", "2026-01-10T00:00:00Z", "2026-01-11T00:00:00Z", "2026-01-17T00:00:00Z"]),
    ("0 0 * * 6,0", "2023-06-15T12:34:56Z", ["2023-06-17T00:00:00Z", "2023-06-18T00:00:00Z", "2023-06-24T00:00:00Z", "2023-06-25T00:00:00Z", "2023-07-01T00:00:00Z"]),
    ("0 0 1 * *", "2024-01-01T00:00:00Z", ["2024-02-01T00:00:00Z", "2024-03-01T00:00:00Z", "2024-04-01T00:00:00Z", "2024-05-01T00:00:00Z", "2024-06-01T00:00:00Z"]),
    ("0 0 1 * *", "2024-02-28T23:59:30Z", ["2024-03-01T00:00:00Z", "2024-04-01T00:00:00Z", "2024-05-01T00:00:00Z", "2024-06-01T00:00:00Z", "2024-07-01T00:00:00Z"]),
    ("0 0 1 * *", "2025-12-31T23:59:00Z", ["2026-01-01T00:00:00Z", "2026-02-01T00:00:00Z", "2026-03-01T00:00:00Z", "2026-04-01T00:00:00Z", "2026-05-01T00:00:00Z"]),
    ("0 0 1 * *", "2023-06-15T12:34:56Z", ["2023-07-01T00:00:00Z", "2023-08-01T00:00:00Z", "2023-09-01T00:00:00Z", "2023-10-01T00:00:00Z", "2023-11-01T00:00:00Z"]),
    ("0 0 1 1 *", "2024-01-01T00:00:00Z", ["2025-01-01T00:00:00Z", "2026-01-01T00:00:00Z", "2027-01-01T00:00:00Z", "2028-01-01T00:00:00Z", "2029-01-01T00:00:00Z"]),
    ("0 0 1 1 *", "2024-02-28T23:59:30Z", ["2025-01-01T00:00:00Z", "2026-01-01T00:00:00Z", "2027-01-01T00:00:00Z", "2028-01-01T00:00:00Z", "2029-01-01T00:00:00Z"]),
    ("0 0 1 1 *", "2025-12-31T23:59:00Z", ["2026-01-01T00:00:00Z", "2027-01-01T00:00:00Z", "2028-01-01T00:00:00Z", "2029-01-01T00:00:00Z", "2030-01-01T00:00:00Z"]),
    ("0 0 1 1 *", "2023-06-15T12:34:56Z", ["2024-01-01T00:00:00Z", "2025-01-01T00:00:00Z", "2026-01-01T00:00:00Z", "2027-01-01T00:00:00Z", "2028-01-01T00:00:00Z"]),
    ("0 0 1,15 * *", "2024-01-01T00:00:00Z", ["2024-01-15T00:00:00Z", "2024-02-01T00:00:00Z", "2024-02-15T00:00:00Z", "2024-03-01T00:00:00Z", "2024-03-15T00:00:00Z"]),
    ("0 0 1,15 * *", "2024-02-28T23:59:30Z", ["2024-03-01T00:00:00Z", "2024-03-15T00:00:00Z", "2024-04-01T00:00:00Z", "2024-04-15T00:00:00Z", "2024-05-01T00:00:00Z"]),
    ("0 0 1,15 * *", "2025-12-31T23:59:00Z", ["2026-01-01T00:00:00Z", "2026-01-15T00:00:00Z", "2026-02-01T00:00:00Z", "2026-02-15T00:00:00Z", "2026-03-01T00:00:00Z"]),
    ("0 0 1,15 * *", "2023-06-15T12:34:56Z", ["2023-07-01T00:00:00Z", "2023-07-15T00:00:00Z", "2023-08-01T00:00:00Z", "2023-08-15T00:00:00Z", "2023-09-01T00:00:00Z"]),
    ("0 0 1,15 * 1", "2024-01-01T00:00:00Z", ["2024-01-08T00:00:00Z", "2024-01-15T00:00:00Z", "2024-01-22T00:00:00Z", "2024-01-29T00:00:00Z", "2024-02-01T00:00:00Z"]),
    ("0 0 1,15 * 1", "2024-02-28T23:59:30Z", ["2024-03-01T00:00:00Z", "2024-03-04T00:00:00Z", "2024-03-11T00:00:00Z", "2024-03-15T00:00:00Z", "2024-03-18T00:00:00Z"]),
    ("0 0 1,15 * 1", "2025-12-31T23:59:00Z", ["2026-01-01T00:00:00Z", "2026-01-05T00:00:00Z", "2026-01-12T00:00:00Z", "2026-01-15T00:00:00Z", "2026-01-19T00:00:00Z"]),
    ("0 0 1,15 * 1", "2023-06-15T12:34:56Z", ["2023-06-19T00:00:00Z", "2023-06-26T00:00:00Z", "2023-07-01T00:00:00Z", "2023-07-03T00:00:00Z", "2023-07-10T00:00:00Z"]),
    ("0 0 13 * 5", "2024-01-01T00:00:00Z", ["2024-01-05T00:00:00Z", "2024-01-12T00:00:00Z", "2024-01-13T00:00:00Z", "2024-01-19T00:00:00Z", "2024-01-26T00:00:00Z"]),
    ("0 0 13 * 5", "2024-02-28T23:59:30Z", ["2024-03-01T00:00:00Z", "2024-03-08T00:00:00Z", "2024-03-13T00:00:00Z", "2024-03-15T00:00:00Z", "2024-03-22T00:00:00Z"]),
    ("0 0 13 * 5", "2025-12-31T23:59:00Z", ["2026-01-02T00:00:00Z", "2026-01-09T00:00:00Z", "2026-01-13T00:00:00Z", "2026-01-16T00:00:00Z", "2026-01-23T00:00:00Z"]),
    ("0 0 13 * 5", "2023-06-15T12:34:56Z", ["2023-06-16T00:00:00Z", "2023-06-23T00:00:00Z", "2023-06-30T00:00:00Z", "2023-07-07T00:00:00Z", "2023-07-13T00:00:00Z"]),
    ("0 0 29 2 *", "2024-01-01T00:00:00Z", ["2024-02-29T00:00:00Z", "2028-02-29T00:00:00Z", "2032-02-29T00:00:00Z", "2036-02-29T00:00:00Z", "2040-02-29T00:00:00Z"]),
    ("0 0 29 2 *", "2024-02-28T23:59:30Z", ["2024-02-29T00:00:00Z", "2028-02-29T00:00:00Z", "2032-02-29T00:00:00Z", "2036-02-29T00:00:00Z", "2040-02-29T00:00:00Z"]),
    ("0 0 29 2 *", "2025-12-31T23:59:00Z", ["2028-02-29T00:00:00Z", "2032-02-29T00:00:00Z", "2036-02-29T00:00:00Z", "2040-02-29T00:00:00Z", "2044-02-29T00:00:00Z"]),
    ("0 0 29 2 *", "2023-06-15T12:34:56Z", ["2024-02-29T00:00:00Z", "2028-02-29T00:00:00Z", "2032-02-29T00:00:00Z", "2036-02-29T00:00:00Z", "2040-02-29T00:00:00Z"]),
    ("0 0 29 2 1", "2024-01-01T00:00:00Z", ["2024-02-05T00:00:00Z", "2024-02-12T00:00:00Z", "2024-02-19T00:00:00Z", "2024-02-26T00:00:00Z", "2024-02-29T00:00:00Z"]),
    ("0 0 29 2 1", "2024-02-28T23:59:30Z", ["2024-02-29T00:00:00Z", "2025-02-03T00:00:00Z", "2025-02-10T00:00:00Z", "2025-02-17T00:00:00Z", "2025-02-24T00:00:00Z"]),
    ("0 0 29 2 1", "2025-12-31T23:59:00Z", ["2026-02-02T00:00:00Z", "2026-02-09T00:00:00Z", "2026-02-16T00:00:00Z", "2026-02-23T00:00:00Z", "2027-02-01T00:00:00Z"]),
    ("0 0 29 2 1", "2023-06-15T12:34:56Z", ["2024-02-05T00:00:00Z", "2024-02-12T00:00:00Z", "2024-02-19T00:00:00Z", "2024-02-26T00:00:00Z", "2024-02-29T00:00:00Z"]),
    ("59 23 31 12 *", "2024-01-01T00:00:00Z", ["2024-12-31T23:59:00Z", "2025-12-31T23:59:00Z", "2026-12-31T23:59:00Z", "2027-12-31T23:59:00Z", "2028-12-31T23:59:00Z"]),
    ("59 23 31 12 *", "2024-02-28T23:59:30Z", ["2024-12-31T23:59:00Z", "2025-12-31T23:59:00Z", "2026-12-31T23:59:00Z", "2027-12-31T23:59:00Z", "2028-12-31T23:59:00Z"]),
    ("59 23 31 12 *", "2025-12-31T23:59:00Z", ["2026-12-31T23:59:00Z", "2027-12-31T23:59:00Z", "2028-12-31T23:59:00Z", "2029-12-31T23:59:00Z", "2030-12-31T23:59:00Z"]),
    ("59 23 31 12 *", "2023-06-15T12:34:56Z", ["2023-12-31T23:59:00Z", "2024-12-31T23:59:00Z", "2025-12-31T23:59:00Z", "2026-12-31T23:59:00Z", "2027-12-31T23:59:00Z"]),
    ("0 0 31 * *", "2024-01-01T00:00:00Z", ["2024-01-31T00:00:00Z", "2024-03-31T00:00:00Z", "2024-05-31T00:00:00Z", "2024-07-31T00:00:00Z", "2024-08-31T00:00:00Z"]),
    ("0 0 31 * *", "2024-02-28T23:59:30Z", ["2024-03-31T00:00:00Z", "2024-05-31T00:00:00Z", "2024-07-31T00:00:00Z", "2024-08-31T00:00:00Z", "2024-10-31T00:00:00Z"]),
    ("0 0 31 * *", "2025-12-31T23:59:00Z", ["2026-01-31T00:00:00Z", "2026-03-31T00:00:00Z", "2026-05-31T00:00:00Z", "2026-07-31T00:00:00Z", "2026-08-31T00:00:00Z"]),
    ("0 0 31 * *", "2023-06-15T12:34:56Z", ["2023-07-31T00:00:00Z", "2023-08-31T00:00:00Z", "2023-10-31T00:00:00Z", "2023-12-31T00:00:00Z", "2024-01-31T00:00:00Z"]),
    ("0 0 30 * *", "2024-01-01T00:00:00Z", ["2024-01-30T00:00:00Z", "2024-03-30T00:00:00Z", "2024-04-30T00:00:00Z", "2024-05-30T00:00:00Z", "2024-06-30T00:00:00Z"]),
    ("0 0 30 * *", "2024-02-28T23:59:30Z", ["2024-03-30T00:00:00Z", "2024-04-30T00:00:00Z", "2024-05-30T00:00:00Z", "2024-06-30T00:00:00Z", "2024-07-30T00:00:00Z"]),
    ("0 0 30 * *", "2025-12-31T23:59:00Z", ["2026-01-30T00:00:00Z", "2026-03-30T00:00:00Z", "2026-04-30T00:00:00Z", "2026-05-30T00:00:00Z", "2026-06-30T00:00:00Z"]),
    ("0 0 30 * *", "2023-06-15T12:34:56Z", ["2023-06-30T00:00:00Z", "2023-07-30T00:00:00Z", "2023-08-30T00:00:00Z", "2023-09-30T00:00:00Z", "2023-10-30T00:00:00Z"]),
    ("15,45 9-17 * * 1-5", "2024-01-01T00:00:00Z", ["2024-01-01T09:15:00Z", "2024-01-01T09:45:00Z", "2024-01-01T10:15:00Z", "2024-01-01T10:45:00Z", "2024-01-01T11:15:00Z"]),
    ("15,45 9-17 * * 1-5", "2024-02-28T23:59:30Z", ["2024-02-29T09:15:00Z", "2024-02-29T09:45:00Z", "2024-02-29T10:15:00Z", "2024-02-29T10:45:00Z", "2024-02-29T11:15:00Z"]),
    ("15,45 9-17 * * 1-5", "2025-12-31T23:59:00Z", ["2026-01-01T09:15:00Z", "2026-01-01T09:45:00Z", "2026-01-01T10:15:00Z", "2026-01-01T10:45:00Z", "2026-01-01T11:15:00Z"]),
    ("15,45 9-17 * * 1-5", "2023-06-15T12:34:56Z", ["2023-06-15T12:45:00Z", "2023-06-15T13:15:00Z", "2023-06-15T13:45:00Z", "2023-06-15T14:15:00Z", "2023-06-15T14:45:00Z"]),
    ("0 */6 * * *", "2024-01-01T00:00:00Z", ["2024-01-01T06:00:00Z", "2024-01-01T12:00:00Z", "2024-01-01T18:00:00Z", "2024-01-02T00:00:00Z", "2024-01-02T06:00:00Z"]),
    ("0 */6 * * *", "2024-02-28T23:59:30Z", ["2024-02-29T00:00:00Z", "2024-02-29T06:00:00Z", "2024-02-29T12:00:00Z", "2024-02-29T18:00:00Z", "2024-03-01T00:00:00Z"]),
    ("0 */6 * * *", "2025-12-31T23:59:00Z", ["2026-01-01T00:00:00Z", "2026-01-01T06:00:00Z", "2026-01-01T12:00:00Z", "2026-01-01T18:00:00Z", "2026-01-02T00:00:00Z"]),
    ("0 */6 * * *", "2023-06-15T12:34:56Z", ["2023-06-15T18:00:00Z", "2023-06-16T00:00:00Z", "2023-06-16T06:00:00Z", "2023-06-16T12:00:00Z", "2023-06-16T18:00:00Z"]),
    ("0 8-20/4 * * *", "2024-01-01T00:00:00Z", ["2024-01-01T08:00:00Z", "2024-01-01T12:00:00Z", "2024-01-01T16:00:00Z", "2024-01-01T20:00:00Z", "2024-01-02T08:00:00Z"]),
    ("0 8-20/4 * * *", "2024-02-28T23:59:30Z", ["2024-02-29T08:00:00Z", "2024-02-29T12:00:00Z", "2024-02-29T16:00:00Z", "2024-02-29T20:00:00Z", "2024-03-01T08:00:00Z"]),
    ("0 8-20/4 * * *", "2025-12-31T23:59:00Z", ["2026-01-01T08:00:00Z", "2026-01-01T12:00:00Z", "2026-01-01T16:00:00Z", "2026-01-01T20:00:00Z", "2026-01-02T08:00:00Z"]),
    ("0 8-20/4 * * *", "2023-06-15T12:34:56Z", ["2023-06-15T16:00:00Z", "2023-06-15T20:00:00Z", "2023-06-16T08:00:00Z", "2023-06-16T12:00:00Z", "2023-06-16T16:00:00Z"]),
    ("10-20/5 * * * *", "2024-01-01T00:00:00Z", ["2024-01-01T00:10:00Z", "2024-01-01T00:15:00Z", "2024-01-01T00:20:00Z", "2024-01-01T01:10:00Z", "2024-01-01T01:15:00Z"]),
    ("10-20/5 * * * *", "2024-02-28T23:59:30Z", ["2024-02-29T00:10:00Z", "2024-02-29T00:15:00Z", "2024-02-29T00:20:00Z", "2024-02-29T01:10:00Z", "2024-02-29T01:15:00Z"]),
    ("10-20/5 * * * *", "2025-12-31T23:59:00Z", ["2026-01-01T00:10:00Z", "2026-01-01T00:15:00Z", "2026-01-01T00:20:00Z", "2026-01-01T01:10:00Z", "2026-01-01T01:15:00Z"]),
    ("10-20/5 * * * *", "2023-06-15T12:34:56Z", ["2023-06-15T13:10:00Z", "2023-06-15T13:15:00Z", "2023-06-15T13:20:00Z", "2023-06-15T14:10:00Z", "2023-06-15T14:15:00Z"]),
    ("0 0 28-31 * *", "2024-01-01T00:00:00Z", ["2024-01-28T00:00:00Z", "2024-01-29T00:00:00Z", "2024-01-30T00:00:00Z", "2024-01-31T00:00:00Z", "2024-02-28T00:00:00Z"]),
    ("0 0 28-31 * *", "2024-02-28T23:59:30Z", ["2024-02-29T00:00:00Z", "2024-03-28T00:00:00Z", "2024-03-29T00:00:00Z", "2024-03-30T00:00:00Z", "2024-03-31T00:00:00Z"]),
    ("0 0 28-31 * *", "2025-12-31T23:59:00Z", ["2026-01-28T00:00:00Z", "2026-01-29T00:00:00Z", "2026-01-30T00:00:00Z", "2026-01-31T00:00:00Z", "2026-02-28T00:00:00Z"]),
    ("0 0 28-31 * *", "2023-06-15T12:34:56Z", ["2023-06-28T00:00:00Z", "2023-06-29T00:00:00Z", "2023-06-30T00:00:00Z", "2023-07-28T00:00:00Z", "2023-07-29T00:00:00Z"]),
    ("0 22 * * 1-5", "2024-01-01T00:00:00Z", ["2024-01-01T22:00:00Z", "2024-01-02T22:00:00Z", "2024-01-03T22:00:00Z", "2024-01-04T22:00:00Z", "2024-01-05T22:00:00Z"]),
    ("0 22 * * 1-5", "2024-02-28T23:59:30Z", ["2024-02-29T22:00:00Z", "2024-03-01T22:00:00Z", "2024-03-04T22:00:00Z", "2024-03-05T22:00:00Z", "2024-03-06T22:00:00Z"]),
    ("0 22 * * 1-5", "2025-12-31T23:59:00Z", ["2026-01-01T22:00:00Z", "2026-01-02T22:00:00Z", "2026-01-05T22:00:00Z", "2026-01-06T22:00:00Z", "2026-01-07T22:00:00Z"]),
    ("0 22 * * 1-5", "2023-06-15T12:34:56Z", ["2023-06-15T22:00:00Z", "2023-06-16T22:00:00Z", "2023-06-19T22:00:00Z", "2023-06-20T22:00:00Z", "2023-06-21T22:00:00Z"]),
    ("0 0 1-7 * 1", "2024-01-01T00:00:00Z", ["2024-01-02T00:00:00Z", "2024-01-03T00:00:00Z", "2024-01-04T00:00:00Z", "2024-01-05T00:00:00Z", "2024-01-06T00:00:00Z"]),
    ("0 0 1-7 * 1", "2024-02-28T23:59:30Z", ["2024-03-01T00:00:00Z", "2024-03-02T00:00:00Z", "2024-03-03T00:00:00Z", "2024-03-04T00:00:00Z", "2024-03-05T00:00:00Z"]),
    ("0 0 1-7 * 1", "2025-12-31T23:59:00Z", ["2026-01-01T00:00:00Z", "2026-01-02T00:00:00Z", "2026-01-03T00:00:00Z", "2026-01-04T00:00:00Z", "2026-01-05T00:00:00Z"]),
    ("0 0 1-7 * 1", "2023-06-15T12:34:56Z", ["2023-06-19T00:00:00Z", "2023-06-26T00:00:00Z", "2023-07-01T00:00:00Z", "2023-07-02T00:00:00Z", "2023-07-03T00:00:00Z"]),
    ("0 9 * 3-5 *", "2024-01-01T00:00:00Z", ["2024-03-01T09:00:00Z", "2024-03-02T09:00:00Z", "2024-03-03T09:00:00Z", "2024-03-04T09:00:00Z", "2024-03-05T09:00:00Z"]),
    ("0 9 * 3-5 *", "2024-02-28T23:59:30Z", ["2024-03-01T09:00:00Z", "2024-03-02T09:00:00Z", "2024-03-03T09:00:00Z", "2024-03-04T09:00:00Z", "2024-03-05T09:00:00Z"]),
    ("0 9 * 3-5 *", "2025-12-31T23:59:00Z", ["2026-03-01T09:00:00Z", "2026-03-02T09:00:00Z", "2026-03-03T09:00:00Z", "2026-03-04T09:00:00Z", "2026-03-05T09:00:00Z"]),
    ("0 9 * 3-5 *", "2023-06-15T12:34:56Z", ["2024-03-01T09:00:00Z", "2024-03-02T09:00:00Z", "2024-03-03T09:00:00Z", "2024-03-04T09:00:00Z", "2024-03-05T09:00:00Z"]),
    ("0 0 1 */3 *", "2024-01-01T00:00:00Z", ["2024-04-01T00:00:00Z", "2024-07-01T00:00:00Z", "2024-10-01T00:00:00Z", "2025-01-01T00:00:00Z", "2025-04-01T00:00:00Z"]),
    ("0 0 1 */3 *", "2024-02-28T23:59:30Z", ["2024-04-01T00:00:00Z", "2024-07-01T00:00:00Z", "2024-10-01T00:00:00Z", "2025-01-01T00:00:00Z", "2025-04-01T00:00:00Z"]),
    ("0 0 1 */3 *", "2025-12-31T23:59:00Z", ["2026-01-01T00:00:00Z", "2026-04-01T00:00:00Z", "2026-07-01T00:00:00Z", "2026-10-01T00:00:00Z", "2027-01-01T00:00:00Z"]),
    ("0 0 1 */3 *", "2023-06-15T12:34:56Z", ["2023-07-01T00:00:00Z", "2023-10-01T00:00:00Z", "2024-01-01T00:00:00Z", "2024-04-01T00:00:00Z", "2024-07-01T00:00:00Z"]),
    ("5 4 * * 0", "2024-01-01T00:00:00Z", ["2024-01-07T04:05:00Z", "2024-01-14T04:05:00Z", "2024-01-21T04:05:00Z", "2024-01-28T04:05:00Z", "2024-02-04T04:05:00Z"]),
    ("5 4 * * 0", "2024-02-28T23:59:30Z", ["2024-03-03T04:05:00Z", "2024-03-10T04:05:00Z", "2024-03-17T04:05:00Z", "2024-03-24T04:05:00Z", "2024-03-31T04:05:00Z"]),
    ("5 4 * * 0", "2025-12-31T23:59:00Z", ["2026-01-04T04:05:00Z", "2026-01-11T04:05:00Z", "2026-01-18T04:05:00Z", "2026-01-25T04:05:00Z", "2026-02-01T04:05:00Z"]),
    ("5 4 * * 0", "2023-06-15T12:34:56Z", ["2023-06-18T04:05:00Z", "2023-06-25T04:05:00Z", "2023-07-02T04:05:00Z", "2023-07-09T04:05:00Z", "2023-07-16T04:05:00Z"]),
    ("0 0,12 1 2 *", "2024-01-01T00:00:00Z", ["2024-02-01T00:00:00Z", "2024-02-01T12:00:00Z", "2025-02-01T00:00:00Z", "2025-02-01T12:00:00Z", "2026-02-01T00:00:00Z"]),
    ("0 0,12 1 2 *", "2024-02-28T23:59:30Z", ["2025-02-01T00:00:00Z", "2025-02-01T12:00:00Z", "2026-02-01T00:00:00Z", "2026-02-01T12:00:00Z", "2027-02-01T00:00:00Z"]),
    ("0 0,12 1 2 *", "2025-12-31T23:59:00Z", ["2026-02-01T00:00:00Z", "2026-02-01T12:00:00Z", "2027-02-01T00:00:00Z", "2027-02-01T12:00:00Z", "2028-02-01T00:00:00Z"]),
    ("0 0,12 1 2 *", "2023-06-15T12:34:56Z", ["2024-02-01T00:00:00Z", "2024-02-01T12:00:00Z", "2025-02-01T00:00:00Z", "2025-02-01T12:00:00Z", "2026-02-01T00:00:00Z"]),
    ("0 0 * 2 *", "2024-01-01T00:00:00Z", ["2024-02-01T00:00:00Z", "2024-02-02T00:00:00Z", "2024-02-03T00:00:00Z", "2024-02-04T00:00:00Z", "2024-02-05T00:00:00Z"]),
    ("0 0 * 2 *", "2024-02-28T23:59:30Z", ["2024-02-29T00:00:00Z", "2025-02-01T00:00:00Z", "2025-02-02T00:00:00Z", "2025-02-03T00:00:00Z", "2025-02-04T00:00:00Z"]),
    ("0 0 * 2 *", "2025-12-31T23:59:00Z", ["2026-02-01T00:00:00Z", "2026-02-02T00:00:00Z", "2026-02-03T00:00:00Z", "2026-02-04T00:00:00Z", "2026-02-05T00:00:00Z"]),
    ("0 0 * 2 *", "2023-06-15T12:34:56Z", ["2024-02-01T00:00:00Z", "2024-02-02T00:00:00Z", "2024-02-03T00:00:00Z", "2024-02-04T00:00:00Z", "2024-02-05T00:00:00Z"]),
    ("0 0 15 6 *", "2024-01-01T00:00:00Z", ["2024-06-15T00:00:00Z", "2025-06-15T00:00:00Z", "2026-06-15T00:00:00Z", "2027-06-15T00:00:00Z", "2028-06-15T00:00:00Z"]),
    ("0 0 15 6 *", "2024-02-28T23:59:30Z", ["2024-06-15T00:00:00Z", "2025-06-15T00:00:00Z", "2026-06-15T00:00:00Z", "2027-06-15T00:00:00Z", "2028-06-15T00:00:00Z"]),
    ("0 0 15 6 *", "2025-12-31T23:59:00Z", ["2026-06-15T00:00:00Z", "2027-06-15T00:00:00Z", "2028-06-15T00:00:00Z", "2029-06-15T00:00:00Z", "2030-06-15T00:00:00Z"]),
    ("0 0 15 6 *", "2023-06-15T12:34:56Z", ["2024-06-15T00:00:00Z", "2025-06-15T00:00:00Z", "2026-06-15T00:00:00Z", "2027-06-15T00:00:00Z", "2028-06-15T00:00:00Z"]),
    ("1 2 3 4 5", "2024-01-01T00:00:00Z", ["2024-04-03T02:01:00Z", "2024-04-05T02:01:00Z", "2024-04-12T02:01:00Z", "2024-04-19T02:01:00Z", "2024-04-26T02:01:00Z"]),
    ("1 2 3 4 5", "2024-02-28T23:59:30Z", ["2024-04-03T02:01:00Z", "2024-04-05T02:01:00Z", "2024-04-12T02:01:00Z", "2024-04-19T02:01:00Z", "2024-04-26T02:01:00Z"]),
    ("1 2 3 4 5", "2025-12-31T23:59:00Z", ["2026-04-03T02:01:00Z", "2026-04-10T02:01:00Z", "2026-04-17T02:01:00Z", "2026-04-24T02:01:00Z", "2027-04-02T02:01:00Z"]),
    ("1 2 3 4 5", "2023-06-15T12:34:56Z", ["2024-04-03T02:01:00Z", "2024-04-05T02:01:00Z", "2024-04-12T02:01:00Z", "2024-04-19T02:01:00Z", "2024-04-26T02:01:00Z"]),
    ("0 0 1 1,7 *", "2024-01-01T00:00:00Z", ["2024-07-01T00:00:00Z", "2025-01-01T00:00:00Z", "2025-07-01T00:00:00Z", "2026-01-01T00:00:00Z", "2026-07-01T00:00:00Z"]),
    ("0 0 1 1,7 *", "2024-02-28T23:59:30Z", ["2024-07-01T00:00:00Z", "2025-01-01T00:00:00Z", "2025-07-01T00:00:00Z", "2026-01-01T00:00:00Z", "2026-07-01T00:00:00Z"]),
    ("0 0 1 1,7 *", "2025-12-31T23:59:00Z", ["2026-01-01T00:00:00Z", "2026-07-01T00:00:00Z", "2027-01-01T00:00:00Z", "2027-07-01T00:00:00Z", "2028-01-01T00:00:00Z"]),
    ("0 0 1 1,7 *", "2023-06-15T12:34:56Z", ["2023-07-01T00:00:00Z", "2024-01-01T00:00:00Z", "2024-07-01T00:00:00Z", "2025-01-01T00:00:00Z", "2025-07-01T00:00:00Z"]),
    ("0 23 * * 5-7", "2024-01-01T00:00:00Z", ["2024-01-05T23:00:00Z", "2024-01-06T23:00:00Z", "2024-01-07T23:00:00Z", "2024-01-12T23:00:00Z", "2024-01-13T23:00:00Z"]),
    ("0 23 * * 5-7", "2024-02-28T23:59:30Z", ["2024-03-01T23:00:00Z", "2024-03-02T23:00:00Z", "2024-03-03T23:00:00Z", "2024-03-08T23:00:00Z", "2024-03-09T23:00:00Z"]),
    ("0 23 * * 5-7", "2025-12-31T23:59:00Z", ["2026-01-02T23:00:00Z", "2026-01-03T23:00:00Z", "2026-01-04T23:00:00Z", "2026-01-09T23:00:00Z", "2026-01-10T23:00:00Z"]),
    ("0 23 * * 5-7", "2023-06-15T12:34:56Z", ["2023-06-16T23:00:00Z", "2023-06-17T23:00:00Z", "2023-06-18T23:00:00Z", "2023-06-23T23:00:00Z", "2023-06-24T23:00:00Z"]),
    ("30 2 * * 2", "2024-01-01T00:00:00Z", ["2024-01-02T02:30:00Z", "2024-01-09T02:30:00Z", "2024-01-16T02:30:00Z", "2024-01-23T02:30:00Z", "2024-01-30T02:30:00Z"]),
    ("30 2 * * 2", "2024-02-28T23:59:30Z", ["2024-03-05T02:30:00Z", "2024-03-12T02:30:00Z", "2024-03-19T02:30:00Z", "2024-03-26T02:30:00Z", "2024-04-02T02:30:00Z"]),
    ("30 2 * * 2", "2025-12-31T23:59:00Z", ["2026-01-06T02:30:00Z", "2026-01-13T02:30:00Z", "2026-01-20T02:30:00Z", "2026-01-27T02:30:00Z", "2026-02-03T02:30:00Z"]),
    ("30 2 * * 2", "2023-06-15T12:34:56Z", ["2023-06-20T02:30:00Z", "2023-06-27T02:30:00Z", "2023-07-04T02:30:00Z", "2023-07-11T02:30:00Z", "2023-07-18T02:30:00Z"]),
    ("0 0 */10 * *", "2024-01-01T00:00:00Z", ["2024-01-11T00:00:00Z", "2024-01-21T00:00:00Z", "2024-01-31T00:00:00Z", "2024-02-01T00:00:00Z", "2024-02-11T00:00:00Z"]),
    ("0 0 */10 * *", "2024-02-28T23:59:30Z", ["2024-03-01T00:00:00Z", "2024-03-11T00:00:00Z", "2024-03-21T00:00:00Z", "2024-03-31T00:00:00Z", "2024-04-01T00:00:00Z"]),
    ("0 0 */10 * *", "2025-12-31T23:59:00Z", ["2026-01-01T00:00:00Z", "2026-01-11T00:00:00Z", "2026-01-21T00:00:00Z", "2026-01-31T00:00:00Z", "2026-02-01T00:00:00Z"]),
    ("0 0 */10 * *", "2023-06-15T12:34:56Z", ["2023-06-21T00:00:00Z", "2023-07-01T00:00:00Z", "2023-07-11T00:00:00Z", "2023-07-21T00:00:00Z", "2023-07-31T00:00:00Z"]),
    ("45 23 * * 6", "2024-01-01T00:00:00Z", ["2024-01-06T23:45:00Z", "2024-01-13T23:45:00Z", "2024-01-20T23:45:00Z", "2024-01-27T23:45:00Z", "2024-02-03T23:45:00Z"]),
    ("45 23 * * 6", "2024-02-28T23:59:30Z", ["2024-03-02T23:45:00Z", "2024-03-09T23:45:00Z", "2024-03-16T23:45:00Z", "2024-03-23T23:45:00Z", "2024-03-30T23:45:00Z"]),
    ("45 23 * * 6", "2025-12-31T23:59:00Z", ["2026-01-03T23:45:00Z", "2026-01-10T23:45:00Z", "2026-01-17T23:45:00Z", "2026-01-24T23:45:00Z", "2026-01-31T23:45:00Z"]),
    ("45 23 * * 6", "2023-06-15T12:34:56Z", ["2023-06-17T23:45:00Z", "2023-06-24T23:45:00Z", "2023-07-01T23:45:00Z", "2023-07-08T23:45:00Z", "2023-07-15T23:45:00Z"]),
    ("0 0 31 1-12 *", "2024-01-01T00:00:00Z", ["2024-01-31T00:00:00Z", "2024-03-31T00:00:00Z", "2024-05-31T00:00:00Z", "2024-07-31T00:00:00Z", "2024-08-31T00:00:00Z"]),
    ("0 0 31 1-12 *", "2024-02-28T23:59:30Z", ["2024-03-31T00:00:00Z", "2024-05-31T00:00:00Z", "2024-07-31T00:00:00Z", "2024-08-31T00:00:00Z", "2024-10-31T00:00:00Z"]),
    ("0 0 31 1-12 *", "2025-12-31T23:59:00Z", ["2026-01-31T00:00:00Z", "2026-03-31T00:00:00Z", "2026-05-31T00:00:00Z", "2026-07-31T00:00:00Z", "2026-08-31T00:00:00Z"]),
    ("0 0 31 1-12 *", "2023-06-15T12:34:56Z", ["2023-07-31T00:00:00Z", "2023-08-31T00:00:00Z", "2023-10-31T00:00:00Z", "2023-12-31T00:00:00Z", "2024-01-31T00:00:00Z"]),
    ];

    #[test]
    fn matches_croniter_golden() {
        let mut failures = Vec::new();
        for (expr, start, want) in GOLDEN {
            let got = next5(expr, start);
            if got != want.to_vec() {
                failures.push(format!("{expr} after {start}: got {got:?}, want {want:?}"));
            }
            // Every result must also satisfy `matches`.
            let s = CronSchedule::parse(expr).unwrap();
            for w in want {
                assert!(s.matches(ts(w)), "{expr} should match {w}");
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
        assert!(GOLDEN.len() >= 160);
    }

    /// Cross-check `next_after` against a brute-force minute scan using
    /// `matches` for many expressions and start points.
    #[test]
    fn next_after_agrees_with_brute_force() {
        let exprs = [
            "*/7 * * * *",
            "0 0 1,15 * 1",
            "0 12 * * 1-5",
            "5/20 3/7 * * *",
            "0 0 * * */1",
            "0 0 1 * */1",
            "0 0 1 * */2",
            "30 * 2-6/2 * 7",
            "0 0 13 * fri",
            "59 23 31 12 *",
            "0 0 *,5 * 3",
            "1-50/7 4-20/5 * * *",
        ];
        let base = ts("2024-02-27T22:13:41Z");
        for e in exprs {
            let s = CronSchedule::parse(e).unwrap();
            let mut t = base;
            for _ in 0..20 {
                let got = s.next_after(t).unwrap();
                let mut brute = (t.div_euclid(60) + 1) * 60;
                while !s.matches(brute) {
                    brute += 60;
                }
                assert_eq!(
                    format_utc(got),
                    format_utc(brute),
                    "{e} after {}",
                    format_utc(t)
                );
                t = got + 17;
            }
        }
    }

    // ------------------------------------------------------ Haskell-specific

    #[test]
    fn strictly_after_and_second_rounding() {
        let s = CronSchedule::parse("0 12 * * *").unwrap();
        assert_eq!(
            format_utc(s.next_after(ts("2024-05-01T12:00:00Z")).unwrap()),
            "2024-05-02T12:00:00Z"
        );
        assert_eq!(
            format_utc(s.next_after(ts("2024-05-01T11:59:59Z")).unwrap()),
            "2024-05-01T12:00:00Z"
        );
        assert_eq!(
            format_utc(s.next_after(ts("2024-05-01T12:00:01Z")).unwrap()),
            "2024-05-02T12:00:00Z"
        );
        let every = CronSchedule::parse("* * * * *").unwrap();
        assert_eq!(
            format_utc(every.next_after(ts("2024-05-01T12:00:59Z")).unwrap()),
            "2024-05-01T12:01:00Z"
        );
    }

    #[test]
    fn dom_dow_or_rule() {
        // 2024-01: Mondays are 1, 8, 15, 22, 29.
        assert_eq!(
            next5("0 0 10,20 * 1", "2024-01-01T00:00:00Z"),
            [
                "2024-01-08T00:00:00Z",
                "2024-01-10T00:00:00Z",
                "2024-01-15T00:00:00Z",
                "2024-01-20T00:00:00Z",
                "2024-01-22T00:00:00Z"
            ]
        );
        // Haskell `isStar`: `*/1` is unrestricted, so only DOM 1 constrains.
        // (A plain Vixie implementation would OR with "every day".)
        assert_eq!(
            next5("0 0 1 * */1", "2024-01-01T00:00:00Z"),
            [
                "2024-02-01T00:00:00Z",
                "2024-03-01T00:00:00Z",
                "2024-04-01T00:00:00Z",
                "2024-05-01T00:00:00Z",
                "2024-06-01T00:00:00Z"
            ]
        );
        // A list containing `*` is also unrestricted.
        assert_eq!(
            next5("0 0 *,5 * 3", "2024-01-01T00:00:00Z"),
            [
                "2024-01-03T00:00:00Z",
                "2024-01-10T00:00:00Z",
                "2024-01-17T00:00:00Z",
                "2024-01-24T00:00:00Z",
                "2024-01-31T00:00:00Z"
            ]
        );
        // `*/2` is restricted: OR of DOM 1 and Sun/Tue/Thu/Sat.
        assert_eq!(
            next5("0 0 1 * */2", "2024-01-01T00:00:00Z"),
            [
                "2024-01-02T00:00:00Z",
                "2024-01-04T00:00:00Z",
                "2024-01-06T00:00:00Z",
                "2024-01-07T00:00:00Z",
                "2024-01-09T00:00:00Z"
            ]
        );
        // OR makes a schedule with an impossible date satisfiable.
        assert_eq!(
            next5("0 0 31 2 1", "2024-01-31T00:00:00Z"),
            [
                "2024-02-05T00:00:00Z",
                "2024-02-12T00:00:00Z",
                "2024-02-19T00:00:00Z",
                "2024-02-26T00:00:00Z",
                "2025-02-03T00:00:00Z"
            ]
        );
    }

    #[test]
    fn sunday_zero_and_seven() {
        let a = next5("0 0 * * 0", "2024-01-01T00:00:00Z");
        assert_eq!(a, next5("0 0 * * 7", "2024-01-01T00:00:00Z"));
        assert_eq!(a, next5("0 0 * * sun", "2024-01-01T00:00:00Z"));
        assert_eq!(a, next5("@weekly", "2024-01-01T00:00:00Z"));
        assert_eq!(a[0], "2024-01-07T00:00:00Z");
        // 5-7 = Fri, Sat, Sun
        assert_eq!(
            next5("0 0 * * 5-7", "2024-01-01T00:00:00Z"),
            [
                "2024-01-05T00:00:00Z",
                "2024-01-06T00:00:00Z",
                "2024-01-07T00:00:00Z",
                "2024-01-12T00:00:00Z",
                "2024-01-13T00:00:00Z"
            ]
        );
    }

    #[test]
    fn specific_with_step() {
        // `n/step` runs from n to the field max.
        let s = CronSchedule::parse("50/4 * * * *").unwrap();
        assert_eq!(
            s.next_n(ts("2024-01-01T00:00:00Z"), 4)
                .into_iter()
                .map(format_utc)
                .collect::<Vec<_>>(),
            [
                "2024-01-01T00:50:00Z",
                "2024-01-01T00:54:00Z",
                "2024-01-01T00:58:00Z",
                "2024-01-01T01:50:00Z"
            ]
        );
        // DOW 1/3 = 1, 4, 7(=Sun)
        assert_eq!(
            next5("0 0 * * 1/3", "2024-01-01T00:00:00Z"),
            [
                "2024-01-04T00:00:00Z",
                "2024-01-07T00:00:00Z",
                "2024-01-08T00:00:00Z",
                "2024-01-11T00:00:00Z",
                "2024-01-14T00:00:00Z"
            ]
        );
    }

    #[test]
    fn leap_day_gap_over_2100() {
        // 2100 is not a leap year: the next Feb 29 after 2096 is in 2104.
        let s = CronSchedule::parse("0 0 29 2 *").unwrap();
        assert_eq!(
            format_utc(s.next_after(ts("2096-03-01T00:00:00Z")).unwrap()),
            "2104-02-29T00:00:00Z"
        );
    }

    #[test]
    fn unsatisfiable_returns_none() {
        for e in [
            "0 0 31 2 *",
            "0 0 30 2 *",
            "0 0 31 4,6,9,11 *",
            "0 0 30-31 2 *",
            "* * 31 2 */1",
        ] {
            let s = CronSchedule::parse(e).unwrap();
            assert_eq!(s.next_after(0), None, "{e}");
            assert!(s.next_n(0, 3).is_empty());
            assert!(!s.matches(ts("2024-02-29T00:00:00Z")));
        }
        // Satisfiable via the smallest selected day.
        let s = CronSchedule::parse("0 0 29,31 2 *").unwrap();
        assert_eq!(
            format_utc(s.next_after(ts("2025-01-01T00:00:00Z")).unwrap()),
            "2028-02-29T00:00:00Z"
        );
    }

    #[test]
    fn extreme_inputs_do_not_panic() {
        let s = CronSchedule::parse("* * * * *").unwrap();
        assert_eq!(s.next_after(i64::MAX), None);
        assert!(s.next_after(i64::MIN / 2).is_some());
        let y = CronSchedule::parse("@yearly").unwrap();
        assert_eq!(
            format_utc(y.next_after(ts("2024-01-01T00:00:00Z")).unwrap()),
            "2025-01-01T00:00:00Z"
        );
    }

    #[test]
    fn next_after_is_fast() {
        let s = CronSchedule::parse("0 0 29 2 *").unwrap();
        let t0 = std::time::Instant::now();
        let v = s.next_n(ts("2000-01-01T00:00:00Z"), 200);
        assert_eq!(v.len(), 200);
        assert!(t0.elapsed() < std::time::Duration::from_secs(2));
    }

    // ------------------------------------------------------- serialisation

    #[test]
    fn serialisation() {
        let cases = [
            ("@yearly", "0 0 1 1 *"),
            ("@monthly", "0 0 1 * *"),
            ("@weekly", "0 0 * * 0"),
            ("@daily", "0 0 * * *"),
            ("@hourly", "0 * * * *"),
            ("@DAILY", "0 0 * * *"),
            ("*/5 * * * *", "*/5 * * * *"),
            ("05 07 1-5 * 7", "5 7 1-5 * 7"),
            ("0 9-17/2 1,15,30 */3 mon-fri", "0 9-17/2 1,15,30 */3 1-5"),
            ("0 0 * JAN,Dec SUN", "0 0 * 1,12 7"),
            ("10/5 * * * *", "10/5 * * * *"),
            ("3,1,2 * * * *", "3,1,2 * * * *"),
        ];
        for (input, want) in cases {
            let s = CronSchedule::parse(input).unwrap();
            assert_eq!(s.to_string(), want, "{input}");
            // Round trip.
            assert_eq!(CronSchedule::parse(want).unwrap(), s);
        }
    }

    // ---------------------------------------------------------- parse errors

    #[test]
    fn parse_errors() {
        let bad = [
            "",
            "* * * *",
            "* * * * * *", // seconds/year field
            "60 * * * *",
            "* 24 * * *",
            "* * 0 * *",
            "* * 32 * *",
            "* * * 0 *",
            "* * * 13 *",
            "* * * * 8",
            "*/0 * * * *",
            "*/60 * * * *", // step must be within 0-59
            "* */24 * * *",
            "* * */32 * *",
            "5-1 * * * *",
            "@reboot",
            "@annually",
            "@midnight",
            "@daily ", // no trailing whitespace in Haskell
            " * * * * *",
            "* * * * * ",
            "*  * * * *", // double space
            "*\t* * * *",
            "* * * * MON#2",
            "* * L * *",
            "* * 15W * *",
            "* * ? * *",
            "*/15,30 * * * *", // steps not allowed in lists
            "1,2/5 * * * *",
            "1, * * * *",
            ",1 * * * *",
            "-5 * * * *",
            "*-5 * * * *",
            "1-2-3 * * * *",
            "*/ * * * *",
            "*/-1 * * * *",
            "+5 * * * *",
            "* * * * monday",
            "* * jan * *", // names only in month / dow
            "* * * mon *",
            "* * * * jan",
            "99999999999 * * * *",
        ];
        for b in bad {
            assert!(CronSchedule::parse(b).is_err(), "{b:?} should be rejected");
        }
        assert!(
            CronSchedule::parse("60 * * * *")
                .unwrap_err()
                .contains("minutes out of range")
        );
    }

    #[test]
    fn parse_accepts_haskell_extensions() {
        // Month and day names are accepted by cron 0.7.0 (case-insensitive
        // because the input is lower-cased before parsing).
        for ok in [
            "* * * JAN *",
            "* * * jan-mar *",
            "0 0 * * MON-FRI",
            "0 0 * * sat,sun",
            "0 0 * * 0-7",
            "*/59 * * * *",
            "* */23 * * *",
            "0 0 * * */7",
            "00 00 01 01 00",
            "1-1 * * * *",
        ] {
            assert!(CronSchedule::parse(ok).is_ok(), "{ok:?} should parse");
        }
    }

    #[test]
    fn lenient_parse() {
        let s = CronSchedule::parse_lenient("  0   12\t* * 1-5 \n").unwrap();
        assert_eq!(s.to_string(), "0 12 * * 1-5");
        assert_eq!(
            CronSchedule::parse_lenient(" @hourly ")
                .unwrap()
                .to_string(),
            "0 * * * *"
        );
        assert!(CronSchedule::parse_lenient("0 12 * * 1-5 *").is_err());
    }
}
