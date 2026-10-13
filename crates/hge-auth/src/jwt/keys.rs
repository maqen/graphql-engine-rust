//! Verification keys: static keys from `type` + `key`, and JWK sets from `jwk_url`.

use base64::Engine as _;
use ring::{hmac, signature};
use serde_json::Value;

use crate::der::{self, PublicKey};

/// A JWS algorithm (`alg` header).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Alg {
    HS256,
    HS384,
    HS512,
    RS256,
    RS384,
    RS512,
    PS256,
    PS384,
    PS512,
    ES256,
    ES384,
    ES512,
    EdDSA,
}

impl Alg {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "HS256" => Alg::HS256,
            "HS384" => Alg::HS384,
            "HS512" => Alg::HS512,
            "RS256" => Alg::RS256,
            "RS384" => Alg::RS384,
            "RS512" => Alg::RS512,
            "PS256" => Alg::PS256,
            "PS384" => Alg::PS384,
            "PS512" => Alg::PS512,
            "ES256" => Alg::ES256,
            "ES384" => Alg::ES384,
            "ES512" => Alg::ES512,
            "EdDSA" => Alg::EdDSA,
            _ => return None,
        })
    }
}

/// An HMAC secret, prepared for each hash so verification does no key setup.
struct HmacKeys {
    k256: hmac::Key,
    k384: hmac::Key,
    k512: hmac::Key,
}

enum Material {
    Hmac(Box<HmacKeys>),
    Public(PublicKey),
}

/// One verification key.
pub struct Key {
    kid: Option<String>,
    /// The JWK's `alg`, when it pins one.
    alg: Option<Alg>,
    material: Material,
}

impl std::fmt::Debug for Key {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Key")
            .field("kid", &self.kid)
            .field("alg", &self.alg)
            .finish_non_exhaustive()
    }
}

impl Key {
    fn hmac(secret: &[u8], kid: Option<String>, alg: Option<Alg>) -> Self {
        Key {
            kid,
            alg,
            material: Material::Hmac(Box::new(HmacKeys {
                k256: hmac::Key::new(hmac::HMAC_SHA256, secret),
                k384: hmac::Key::new(hmac::HMAC_SHA384, secret),
                k512: hmac::Key::new(hmac::HMAC_SHA512, secret),
            })),
        }
    }

    fn public(key: PublicKey, kid: Option<String>, alg: Option<Alg>) -> Self {
        Key {
            kid,
            alg,
            material: Material::Public(key),
        }
    }

    /// Whether this key can check a token with this `alg` and `kid` header. A key without a
    /// `kid` is a candidate for any token.
    pub fn usable_for(&self, alg: Alg, kid: Option<&str>) -> bool {
        if let (Some(want), Some(have)) = (kid, &self.kid)
            && want != have
        {
            return false;
        }
        if self.alg.is_some_and(|a| a != alg) {
            return false;
        }
        use Alg::*;
        matches!(
            (&self.material, alg),
            (Material::Hmac(_), HS256 | HS384 | HS512)
                | (
                    Material::Public(PublicKey::Rsa { .. }),
                    RS256 | RS384 | RS512 | PS256 | PS384 | PS512
                )
                | (Material::Public(PublicKey::EcP256(_)), ES256)
                | (Material::Public(PublicKey::EcP384(_)), ES384)
                | (Material::Public(PublicKey::EcP521(_)), ES512)
                | (Material::Public(PublicKey::Ed25519(_)), EdDSA)
        )
    }

    /// Check a signature. The key must be [`usable_for`](Self::usable_for) `alg`.
    pub fn verify(&self, alg: Alg, message: &[u8], sig: &[u8]) -> bool {
        use Alg::*;
        match &self.material {
            Material::Hmac(keys) => {
                let key = match alg {
                    HS256 => &keys.k256,
                    HS384 => &keys.k384,
                    HS512 => &keys.k512,
                    _ => return false,
                };
                hmac::verify(key, message, sig).is_ok()
            }
            Material::Public(PublicKey::Rsa { n, e }) if !ring_rsa_size(n) => {
                legacy::verify_rsa(n, e, alg, message, sig)
            }
            Material::Public(PublicKey::Rsa { n, e }) => {
                let params = match alg {
                    RS256 => &signature::RSA_PKCS1_2048_8192_SHA256,
                    RS384 => &signature::RSA_PKCS1_2048_8192_SHA384,
                    RS512 => &signature::RSA_PKCS1_2048_8192_SHA512,
                    PS256 => &signature::RSA_PSS_2048_8192_SHA256,
                    PS384 => &signature::RSA_PSS_2048_8192_SHA384,
                    PS512 => &signature::RSA_PSS_2048_8192_SHA512,
                    _ => return false,
                };
                signature::RsaPublicKeyComponents { n, e }
                    .verify(params, message, sig)
                    .is_ok()
            }
            Material::Public(PublicKey::EcP256(point)) => {
                signature::UnparsedPublicKey::new(&signature::ECDSA_P256_SHA256_FIXED, point)
                    .verify(message, sig)
                    .is_ok()
            }
            Material::Public(PublicKey::EcP384(point)) => {
                signature::UnparsedPublicKey::new(&signature::ECDSA_P384_SHA384_FIXED, point)
                    .verify(message, sig)
                    .is_ok()
            }
            Material::Public(PublicKey::Ed25519(key)) => {
                signature::UnparsedPublicKey::new(&signature::ED25519, key)
                    .verify(message, sig)
                    .is_ok()
            }
            Material::Public(PublicKey::EcP521(point)) => legacy::verify_p521(point, message, sig),
        }
    }
}

/// The keys of one JWT config.
#[derive(Debug, Default)]
pub struct KeySet(pub Vec<Key>);

impl KeySet {
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn has_kid(&self, kid: &str) -> bool {
        self.0.iter().any(|k| k.kid.as_deref() == Some(kid))
    }
}

/// ring verifies RSA moduli of 2048 to 8192 bits; others take the slow path.
fn ring_rsa_size(n: &[u8]) -> bool {
    let bits = match n.first() {
        Some(&b) => (n.len() - 1) * 8 + (8 - b.leading_zeros() as usize),
        None => 0,
    };
    (2048..=8192).contains(&bits)
}

/// RustCrypto fallbacks for keys V2 (jose) accepts but ring cannot check: RSA outside
/// 2048-8192 bits and P-521. Slower, so only used for those keys.
mod legacy {
    use p521::ecdsa::signature::Verifier as _;
    use rsa::{BigUint, Pkcs1v15Sign, Pss, RsaPublicKey};
    use sha2::{Digest, Sha256, Sha384, Sha512};

    use super::Alg;

    pub(super) fn verify_rsa(n: &[u8], e: &[u8], alg: Alg, message: &[u8], sig: &[u8]) -> bool {
        let Ok(key) = RsaPublicKey::new_with_max_size(
            BigUint::from_bytes_be(n),
            BigUint::from_bytes_be(e),
            16_384,
        ) else {
            return false;
        };
        let result = match alg {
            Alg::RS256 => key.verify(Pkcs1v15Sign::new::<Sha256>(), &Sha256::digest(message), sig),
            Alg::RS384 => key.verify(Pkcs1v15Sign::new::<Sha384>(), &Sha384::digest(message), sig),
            Alg::RS512 => key.verify(Pkcs1v15Sign::new::<Sha512>(), &Sha512::digest(message), sig),
            Alg::PS256 => key.verify(Pss::new::<Sha256>(), &Sha256::digest(message), sig),
            Alg::PS384 => key.verify(Pss::new::<Sha384>(), &Sha384::digest(message), sig),
            Alg::PS512 => key.verify(Pss::new::<Sha512>(), &Sha512::digest(message), sig),
            _ => return false,
        };
        result.is_ok()
    }

    /// ES512: P-521 with SHA-512, fixed-size `r || s` signature.
    pub(super) fn verify_p521(point: &[u8], message: &[u8], sig: &[u8]) -> bool {
        let (Ok(key), Ok(sig)) = (
            p521::ecdsa::VerifyingKey::from_sec1_bytes(point),
            p521::ecdsa::Signature::from_slice(sig),
        ) else {
            return false;
        };
        key.verify(message, &sig).is_ok()
    }
}

/// A key from the config's `type` and `key` (V2's `parseHmacKey` / PEM parsing).
pub fn static_key(ty: &str, key: &str) -> Result<Key, String> {
    let min_hmac = match ty {
        "HS256" => Some(32),
        "HS384" => Some(48),
        "HS512" => Some(64),
        _ => None,
    };
    if let Some(min) = min_hmac {
        if key.len() < min {
            return Err(format!(
                "Invalid JWK: Key size too small; should be atleast {min} characters"
            ));
        }
        return Ok(Key::hmac(key.as_bytes(), None, None));
    }
    let wants: fn(&PublicKey) -> bool = match ty {
        "RS256" | "RS384" | "RS512" => |k| matches!(k, PublicKey::Rsa { .. }),
        "ES256" => |k| matches!(k, PublicKey::EcP256(_)),
        "ES384" => |k| matches!(k, PublicKey::EcP384(_)),
        "ES512" => |k| matches!(k, PublicKey::EcP521(_)),
        "Ed25519" => |k| matches!(k, PublicKey::Ed25519(_)),
        _ => return Err(format!("Invalid JWK: Key type: {ty} is not supported")),
    };
    let public =
        der::parse_pem(key).map_err(|e| format!("Invalid JWK: Could not decode PEM: {e}"))?;
    if !wants(&public) {
        return Err(format!(
            "Invalid JWK: the key is not a valid {ty} public key"
        ));
    }
    Ok(Key::public(public, None, None))
}

/// Parse a JWK set as served by a `jwk_url`. Keys whose `use` is neither `sig` nor `enc`,
/// and keys of unsupported types, are skipped (as V2's robust parser does for `use`).
pub fn parse_jwk_set(body: &[u8]) -> Result<KeySet, String> {
    let doc: Value = serde_json::from_slice(body).map_err(|e| e.to_string())?;
    let keys = doc
        .get("keys")
        .and_then(Value::as_array)
        .ok_or("expected an object with a \"keys\" array")?;
    let mut out = vec![];
    for jwk in keys {
        match jwk.get("use").and_then(Value::as_str) {
            None | Some("sig") | Some("enc") => {}
            Some(_) => continue,
        }
        match parse_jwk(jwk) {
            Ok(key) => out.push(key),
            Err(e) => tracing::warn!(target: "jwk-refresh-log", error = %e, "skipping JWK"),
        }
    }
    Ok(KeySet(out))
}

fn parse_jwk(jwk: &Value) -> Result<Key, String> {
    let field = |name: &str| -> Result<Vec<u8>, String> {
        let s = jwk
            .get(name)
            .and_then(Value::as_str)
            .ok_or_else(|| format!("JWK is missing \"{name}\""))?;
        b64_lenient(s).map_err(|e| format!("JWK \"{name}\": {e}"))
    };
    let kid = jwk.get("kid").and_then(Value::as_str).map(str::to_owned);
    let alg = match jwk.get("alg").and_then(Value::as_str) {
        None => None,
        Some(a) => Some(Alg::parse(a).ok_or_else(|| format!("unsupported JWK alg {a}"))?),
    };
    let kty = jwk.get("kty").and_then(Value::as_str).unwrap_or_default();
    let key = match kty {
        "RSA" => PublicKey::Rsa {
            // Strip leading zero bytes (seen from Cognito), as V2 does.
            n: der::strip_zeros(&field("n")?).to_vec(),
            e: der::strip_zeros(&field("e")?).to_vec(),
        },
        "EC" => {
            let mut point = vec![4u8];
            point.extend(field("x")?);
            point.extend(field("y")?);
            match jwk.get("crv").and_then(Value::as_str) {
                Some("P-256") => PublicKey::EcP256(point),
                Some("P-384") => PublicKey::EcP384(point),
                Some("P-521") => PublicKey::EcP521(point),
                other => return Err(format!("unsupported EC curve {other:?}")),
            }
        }
        "OKP" => match jwk.get("crv").and_then(Value::as_str) {
            Some("Ed25519") => PublicKey::Ed25519(field("x")?),
            other => return Err(format!("unsupported OKP curve {other:?}")),
        },
        "oct" => return Ok(Key::hmac(&field("k")?, kid, alg)),
        other => return Err(format!("unsupported JWK kty {other:?}")),
    };
    Ok(Key::public(key, kid, alg))
}

/// base64url without padding, also accepting the standard alphabet and padding (V2
/// normalises JWK values the same way).
pub fn b64_lenient(s: &str) -> Result<Vec<u8>, base64::DecodeError> {
    let normalized: String = s
        .trim_end_matches('=')
        .chars()
        .map(|c| match c {
            '+' => '-',
            '/' => '_',
            c => c,
        })
        .collect();
    base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(normalized)
}
