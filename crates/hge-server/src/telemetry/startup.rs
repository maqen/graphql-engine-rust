//! Helpers for `startup` logs (spec 11 §5.4).

/// A database URL with its password replaced by `...`, as V2 logs it in
/// `postgres_connection` (`postgres://user:...@host:5432/db`); so are `sslpassword` and
/// `password` query parameters. Unparseable URLs give `None` (also when an `@` follows the
/// host, which means a password with `/`, `?` or `#` that could otherwise leak).
pub fn mask_database_url(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    let (authority_end, _) = rest
        .char_indices()
        .find(|(_, c)| matches!(c, '/' | '?' | '#'))
        .unwrap_or((rest.len(), ' '));
    let (authority, tail) = rest.split_at(authority_end);
    if tail.contains('@') {
        return None;
    }
    let authority = match authority.rsplit_once('@') {
        Some((userinfo, host)) => match userinfo.split_once(':') {
            Some((user, _password)) => format!("{user}:...@{host}"),
            None => format!("{userinfo}@{host}"),
        },
        None => authority.to_owned(),
    };
    let tail = match tail.split_once('?') {
        Some((path, query)) => {
            let query: Vec<String> = query
                .split('&')
                .map(|kv| match kv.split_once('=') {
                    Some((k @ ("sslpassword" | "password"), _)) => format!("{k}=..."),
                    _ => kv.to_owned(),
                })
                .collect();
            format!("{path}?{}", query.join("&"))
        }
        None => tail.to_owned(),
    };
    Some(format!("{scheme}://{authority}{tail}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_passwords() {
        assert_eq!(
            mask_database_url("postgres://user:secret@host:5432/db").as_deref(),
            Some("postgres://user:...@host:5432/db")
        );
        assert_eq!(
            mask_database_url("postgresql://u:p@ss@h/db?sslmode=require&sslpassword=x").as_deref(),
            Some("postgresql://u:...@h/db?sslmode=require&sslpassword=...")
        );
        assert_eq!(
            mask_database_url("postgres://host/db").as_deref(),
            Some("postgres://host/db")
        );
        assert_eq!(
            mask_database_url("postgres://u@h/db?password=x").as_deref(),
            Some("postgres://u@h/db?password=...")
        );
        assert_eq!(mask_database_url("not a url"), None);
        for leaky in [
            "postgres://u:pa/ss@host/db",
            "postgres://u:p#w@h/db",
            "postgres://u:p?w@h",
        ] {
            assert_eq!(mask_database_url(leaky), None, "{leaky}");
        }
    }
}
