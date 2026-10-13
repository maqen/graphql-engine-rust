//! The log timestamp, V2's `%FT%H:%M:%S%3Q%z`: `2019-05-30T23:40:24.654+0000`.
//!
//! Always UTC (V2 uses the local zone, which is UTC in containers).

use std::time::{SystemTime, UNIX_EPOCH};

/// Write the timestamp of `now` into `out` (28 bytes).
pub(crate) fn write_timestamp(out: &mut Vec<u8>, now: SystemTime) {
    let since = now.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = since.as_secs();
    let millis = since.subsec_millis();
    let (y, m, d) = civil_from_days((secs / 86_400) as i64);
    let rem = secs % 86_400;
    let (hh, mm, ss) = (rem / 3600, rem % 3600 / 60, rem % 60);
    use std::io::Write;
    let _ = write!(
        out,
        "{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}.{millis:03}+0000"
    );
}

/// A time as aeson writes a `UTCTime`: `2019-05-30T18:10:24.654Z`, with the fraction only
/// when there is one (trailing zeros dropped).
pub(crate) fn utc_iso8601(t: SystemTime) -> String {
    let since = t.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = since.as_secs();
    let (y, m, d) = civil_from_days((secs / 86_400) as i64);
    let rem = secs % 86_400;
    let (hh, mm, ss) = (rem / 3600, rem % 3600 / 60, rem % 60);
    let mut out = format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}");
    let nanos = since.subsec_nanos();
    if nanos > 0 {
        let frac = format!("{nanos:09}");
        out.push('.');
        out.push_str(frac.trim_end_matches('0'));
    }
    out.push('Z');
    out
}

/// Days since 1970-01-01 to a proleptic Gregorian date (Howard Hinnant's algorithm).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn ts(secs: u64, millis: u64) -> String {
        let mut out = vec![];
        write_timestamp(
            &mut out,
            UNIX_EPOCH + Duration::from_secs(secs) + Duration::from_millis(millis),
        );
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn formats_like_v2() {
        assert_eq!(ts(0, 0), "1970-01-01T00:00:00.000+0000");
        // 2019-05-30T18:10:24.654Z
        assert_eq!(ts(1_559_239_824, 654), "2019-05-30T18:10:24.654+0000");
        // A leap day.
        assert_eq!(ts(1_709_210_096, 7), "2024-02-29T12:34:56.007+0000");
    }

    #[test]
    fn utc_like_aeson() {
        let at = |secs, nanos| utc_iso8601(UNIX_EPOCH + Duration::new(secs, nanos));
        assert_eq!(at(1_559_239_824, 0), "2019-05-30T18:10:24Z");
        assert_eq!(at(1_559_239_824, 654_000_000), "2019-05-30T18:10:24.654Z");
    }
}
