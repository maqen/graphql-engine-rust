//! Expiry from `Cache-Control` / `Expires` values (JWK responses and webhook bodies).

use std::time::{Duration, SystemTime};

/// The parsed directives V2 looks at.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct CacheControl {
    pub max_age: Option<u64>,
    pub s_max_age: Option<u64>,
    pub must_revalidate: bool,
    pub no_cache: bool,
    pub no_store: bool,
}

/// Parse a `Cache-Control` value. Unknown directives are ignored; a malformed number is an
/// error, as in V2's parser.
pub(crate) fn parse(value: &str) -> Result<CacheControl, String> {
    let mut cc = CacheControl::default();
    for directive in value.split(',') {
        let directive = directive.trim();
        if directive.is_empty() {
            continue;
        }
        let (name, arg) = match directive.split_once('=') {
            Some((n, a)) => (n.trim(), Some(a.trim().trim_matches('"'))),
            None => (directive, None),
        };
        let seconds = || {
            arg.and_then(|a| a.parse::<u64>().ok())
                // V2's `findMaxAge` error text.
                .ok_or_else(|| "could not parse max-age/s-maxage value".to_owned())
        };
        match name.to_ascii_lowercase().as_str() {
            "max-age" => cc.max_age = Some(seconds()?),
            "s-maxage" => cc.s_max_age = Some(seconds()?),
            "must-revalidate" => cc.must_revalidate = true,
            "no-cache" => cc.no_cache = true,
            "no-store" => cc.no_store = true,
            _ => {}
        }
    }
    Ok(cc)
}

/// V2's `determineJwkExpiryLifetime` for a `Cache-Control` value: `None` means "no
/// decision, look at `Expires`".
pub(crate) fn jwk_expiry(cc: &CacheControl, now: SystemTime) -> Option<SystemTime> {
    let max_age = cc.max_age.or(cc.s_max_age);
    if cc.must_revalidate {
        Some(add_secs(now, max_age.unwrap_or(0)))
    } else if cc.no_cache || cc.no_store {
        Some(now)
    } else {
        max_age.map(|s| add_secs(now, s))
    }
}

/// `now + secs`, saturating far in the future instead of overflowing on huge ages.
pub(crate) fn add_secs(now: SystemTime, secs: u64) -> SystemTime {
    // Ten thousand years is "never" for any cache.
    now.checked_add(Duration::from_secs(secs.min(315_360_000_000)))
        .unwrap_or(now)
}

/// Parse an HTTP date (`Expires`).
pub(crate) fn parse_http_date(value: &str) -> Result<SystemTime, String> {
    httpdate::parse_http_date(value.trim()).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directives() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1000);
        let at = |s: &str| jwk_expiry(&parse(s).unwrap(), now);
        assert_eq!(
            at("public, max-age=60"),
            Some(now + Duration::from_secs(60))
        );
        assert_eq!(at("s-maxage=5"), Some(now + Duration::from_secs(5)));
        assert_eq!(
            at("max-age=60, must-revalidate"),
            Some(now + Duration::from_secs(60))
        );
        assert_eq!(at("must-revalidate"), Some(now));
        assert_eq!(at("no-cache, max-age=60"), Some(now));
        assert_eq!(at("no-store"), Some(now));
        assert_eq!(at("public"), None);
        assert!(parse("max-age=abc").is_err());
        assert!(at("max-age=18446744073709551615").unwrap() > now);
        assert!(parse_http_date("Wed, 21 Oct 2015 07:28:00 GMT").is_ok());
        assert!(parse_http_date("tomorrow").is_err());
    }
}
