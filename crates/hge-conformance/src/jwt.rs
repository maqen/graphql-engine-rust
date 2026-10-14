//! JWTs signed while a case runs, so recorded cases never expire.
//!
//! A request header whose value is `{jwt: {key, claims}, prefix}` is replaced by a token
//! signed with `conformance/keys/<key>.key` (`rsa`: RS512, `es`: ES256, `ed25519`: EdDSA), or
//! with HMAC for `key: {hs256: <secret>}`. String claims `{{now}}`, `{{now+N}}` and `{{now-N}}`
//! become Unix times. Suites name the matching public key in their environment as
//! `{{public_key:<key>}}`.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use ring::rand::SystemRandom;
use ring::signature::{self, EcdsaKeyPair, Ed25519KeyPair, RsaKeyPair};
use serde_json::{Value, json};

static KEYS_DIR: OnceLock<PathBuf> = OnceLock::new();

/// Where the key files are (`conformance/keys`, next to the suites).
pub fn set_keys_dir(dir: PathBuf) {
    let _ = KEYS_DIR.set(dir);
}

fn keys_dir() -> &'static Path {
    KEYS_DIR
        .get()
        .map(PathBuf::as_path)
        .unwrap_or(Path::new("conformance/keys"))
}

fn read_key(name: &str, ext: &str) -> Result<String, String> {
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(format!("bad key name {name:?}"));
    }
    let path = keys_dir().join(format!("{name}.{ext}"));
    std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))
}

/// Replace `{{public_key:<name>}}` with that public key's PEM, escaped for a JSON string
/// (the placeholder sits inside `HASURA_GRAPHQL_JWT_SECRET`'s JSON).
pub fn expand_env(value: &str) -> Result<String, String> {
    const OPEN: &str = "{{public_key:";
    let mut out = String::new();
    let mut rest = value;
    while let Some(i) = rest.find(OPEN) {
        out.push_str(&rest[..i]);
        let after = &rest[i + OPEN.len()..];
        let end = after
            .find("}}")
            .ok_or_else(|| format!("unclosed {OPEN} in {value:?}"))?;
        let pem = read_key(&after[..end], "pub")?;
        let quoted = Value::String(pem).to_string();
        out.push_str(&quoted[1..quoted.len() - 1]);
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    Ok(out)
}

/// The header value for `{jwt: ..., prefix: ...}`, or `None` when `v` is not such an object.
pub fn header_value(v: &Value) -> Option<Result<String, String>> {
    // Only exactly this shape, so ordinary data with a `jwt` field is left alone.
    let o = v.as_object()?;
    let spec = o.get("jwt").filter(|s| s.is_object())?;
    if o.keys().any(|k| k != "jwt" && k != "prefix") {
        return None;
    }
    let prefix = v.get("prefix").and_then(Value::as_str).unwrap_or("");
    Some(sign(spec).map(|t| format!("{prefix}{t}")))
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// `{{now}}`, `{{now+N}}`, `{{now-N}}` anywhere in the claims become Unix times.
fn resolve_times(v: &Value, now: i64) -> Result<Value, String> {
    Ok(match v {
        Value::String(s) if s.starts_with("{{now") && s.ends_with("}}") => {
            let offset = &s[5..s.len() - 2];
            let n = match offset.as_bytes().first() {
                None => 0,
                Some(b'+') => offset[1..]
                    .parse::<i64>()
                    .map_err(|e| format!("{s}: {e}"))?,
                Some(b'-') => -offset[1..]
                    .parse::<i64>()
                    .map_err(|e| format!("{s}: {e}"))?,
                _ => return Err(format!("bad time {s:?}")),
            };
            json!(now + n)
        }
        Value::Array(a) => Value::Array(
            a.iter()
                .map(|v| resolve_times(v, now))
                .collect::<Result<_, _>>()?,
        ),
        Value::Object(o) => Value::Object(
            o.iter()
                .map(|(k, v)| Ok((k.clone(), resolve_times(v, now)?)))
                .collect::<Result<_, String>>()?,
        ),
        v => v.clone(),
    })
}

fn der(pem: &str) -> Result<Vec<u8>, String> {
    let body: String = pem
        .lines()
        .filter(|l| !l.starts_with("-----"))
        .collect::<Vec<_>>()
        .concat();
    STANDARD.decode(body).map_err(|e| e.to_string())
}

/// Sign `{key, claims}` into a compact JWT.
pub fn sign(spec: &Value) -> Result<String, String> {
    let claims = resolve_times(spec.get("claims").unwrap_or(&json!({})), now())?;
    let key = spec.get("key").ok_or("jwt needs a key")?;
    let alg = match key {
        Value::String(k) if k == "rsa" => "RS512",
        Value::String(k) if k == "es" => "ES256",
        Value::String(k) if k == "ed25519" => "EdDSA",
        Value::Object(o) if o.contains_key("hs256") => "HS256",
        other => return Err(format!("unknown jwt key {other}")),
    };
    let header = json!({"alg": alg, "typ": "JWT"});
    let input = format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(header.to_string()),
        URL_SAFE_NO_PAD.encode(claims.to_string())
    );
    let rng = SystemRandom::new();
    let sig = match alg {
        "RS512" => {
            let pair = RsaKeyPair::from_pkcs8(&der(&read_key("rsa", "key")?)?)
                .map_err(|e| format!("rsa key: {e}"))?;
            let mut sig = vec![0; pair.public().modulus_len()];
            pair.sign(
                &signature::RSA_PKCS1_SHA512,
                &rng,
                input.as_bytes(),
                &mut sig,
            )
            .map_err(|e| e.to_string())?;
            sig
        }
        "ES256" => {
            let pair = EcdsaKeyPair::from_pkcs8(
                &signature::ECDSA_P256_SHA256_FIXED_SIGNING,
                &der(&read_key("es", "key")?)?,
                &rng,
            )
            .map_err(|e| format!("es key: {e}"))?;
            pair.sign(&rng, input.as_bytes())
                .map_err(|e| e.to_string())?
                .as_ref()
                .to_vec()
        }
        "EdDSA" => {
            let pair =
                Ed25519KeyPair::from_pkcs8_maybe_unchecked(&der(&read_key("ed25519", "key")?)?)
                    .map_err(|e| format!("ed25519 key: {e}"))?;
            pair.sign(input.as_bytes()).as_ref().to_vec()
        }
        _ => {
            let secret = key["hs256"].as_str().ok_or("hs256 needs a string secret")?;
            let k = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, secret.as_bytes());
            ring::hmac::sign(&k, input.as_bytes()).as_ref().to_vec()
        }
    };
    Ok(format!("{input}.{}", URL_SAFE_NO_PAD.encode(sig)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys() {
        set_keys_dir(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../conformance/keys"));
    }

    #[test]
    fn times_resolve() {
        let v = resolve_times(
            &json!({"exp": "{{now+60}}", "iat": "{{now}}", "a": ["{{now-1}}"]}),
            100,
        )
        .unwrap();
        assert_eq!(v, json!({"exp": 160, "iat": 100, "a": [99]}));
        assert!(resolve_times(&json!("{{now*2}}"), 0).is_err());
    }

    #[test]
    fn signs_with_every_key() {
        keys();
        for key in [
            json!("rsa"),
            json!("es"),
            json!("ed25519"),
            json!({"hs256": "x"}),
        ] {
            let t = sign(&json!({"key": key, "claims": {"sub": "1"}})).unwrap();
            let parts: Vec<_> = t.split('.').collect();
            assert_eq!(parts.len(), 3, "{t}");
            assert_eq!(URL_SAFE_NO_PAD.decode(parts[1]).unwrap(), br#"{"sub":"1"}"#);
        }
        assert!(header_value(&json!({"jwt": "x"})).is_none());
        assert!(header_value(&json!({"jwt": {"key": "rsa"}, "other": 1})).is_none());
        let h = header_value(&json!({"jwt": {"key": "rsa"}, "prefix": "Bearer "})).unwrap();
        assert!(h.unwrap().starts_with("Bearer ey"));
        assert!(header_value(&json!("plain")).is_none());
    }

    /// The public key's raw bytes (SubjectPublicKeyInfo minus its algorithm header), as ring
    /// takes them.
    fn spki_tail(name: &str, header_len: usize) -> Vec<u8> {
        der(&read_key(name, "pub").unwrap()).unwrap()[header_len..].to_vec()
    }

    #[test]
    fn signatures_verify() {
        keys();
        let verify =
            |key: &str, alg: &'static dyn signature::VerificationAlgorithm, pk: Vec<u8>| {
                let t = sign(&json!({"key": key, "claims": {"sub": "1"}})).unwrap();
                let (input, sig) = t.rsplit_once('.').unwrap();
                signature::UnparsedPublicKey::new(alg, pk)
                    .verify(input.as_bytes(), &URL_SAFE_NO_PAD.decode(sig).unwrap())
                    .unwrap_or_else(|e| panic!("{key}: {e}"));
            };
        // P-256 SPKI: 26-byte header then the 65-byte point; Ed25519: 12 then 32 bytes.
        verify(
            "es",
            &signature::ECDSA_P256_SHA256_FIXED,
            spki_tail("es", 26),
        );
        verify("ed25519", &signature::ED25519, spki_tail("ed25519", 12));
        // RSA SPKI: 24-byte header then the PKCS#1 RSAPublicKey that ring takes.
        verify(
            "rsa",
            &signature::RSA_PKCS1_2048_8192_SHA512,
            spki_tail("rsa", 24),
        );
    }

    #[test]
    fn public_keys_expand_into_json() {
        keys();
        let s = expand_env(r#"{"type":"RS512","key":"{{public_key:rsa}}"}"#).unwrap();
        let v: Value = serde_json::from_str(&s).unwrap();
        assert!(
            v["key"]
                .as_str()
                .unwrap()
                .starts_with("-----BEGIN PUBLIC KEY-----\n")
        );
        assert!(expand_env("{{public_key:../x}}").is_err());
    }
}
