//! Token signing and mock servers shared by the integration tests.
#![allow(dead_code)]

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use hge_auth::{AuthConfig, AuthError, Authenticated, Authenticator};
use http::{HeaderMap, HeaderName};
use ring::rand::SystemRandom;
use ring::signature;
use serde_json::{Value, json};

pub const HS_KEY: &str = "an-hmac-secret-that-is-at-least-32-bytes-long";
pub const RSA_PRIVATE: &[u8] = include_bytes!("../fixtures/rsa_private.der");
pub const RSA_PUBLIC: &str = include_str!("../fixtures/rsa_public.pem");
pub const EC_PRIVATE: &[u8] = include_bytes!("../fixtures/ec_private.pk8");
pub const EC_PUBLIC: &str = include_str!("../fixtures/ec_public.pem");
pub const ED_PRIVATE: &[u8] = include_bytes!("../fixtures/ed_private.pk8");
pub const ED_PUBLIC: &str = include_str!("../fixtures/ed_public.pem");

pub enum Signer<'a> {
    Hs256(&'a [u8]),
    Rs256,
    Es256,
    EdDsa,
}

pub fn sign(signer: Signer<'_>, header: Value, claims: &Value) -> String {
    let alg = match signer {
        Signer::Hs256(_) => "HS256",
        Signer::Rs256 => "RS256",
        Signer::Es256 => "ES256",
        Signer::EdDsa => "EdDSA",
    };
    let mut header = header;
    header["alg"] = json!(alg);
    header["typ"] = json!("JWT");
    let input = format!(
        "{}.{}",
        B64.encode(header.to_string()),
        B64.encode(claims.to_string())
    );
    let rng = SystemRandom::new();
    let sig = match signer {
        Signer::Hs256(key) => {
            let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, key);
            ring::hmac::sign(&key, input.as_bytes()).as_ref().to_vec()
        }
        Signer::Rs256 => {
            let kp = signature::RsaKeyPair::from_der(RSA_PRIVATE).unwrap();
            let mut sig = vec![0; kp.public().modulus_len()];
            kp.sign(
                &signature::RSA_PKCS1_SHA256,
                &rng,
                input.as_bytes(),
                &mut sig,
            )
            .unwrap();
            sig
        }
        Signer::Es256 => {
            let kp = signature::EcdsaKeyPair::from_pkcs8(
                &signature::ECDSA_P256_SHA256_FIXED_SIGNING,
                EC_PRIVATE,
                &rng,
            )
            .unwrap();
            kp.sign(&rng, input.as_bytes()).unwrap().as_ref().to_vec()
        }
        Signer::EdDsa => {
            let kp = signature::Ed25519KeyPair::from_pkcs8_maybe_unchecked(ED_PRIVATE).unwrap();
            kp.sign(input.as_bytes()).as_ref().to_vec()
        }
    };
    format!("{input}.{}", B64.encode(sig))
}

pub fn hs(claims: &Value) -> String {
    sign(Signer::Hs256(HS_KEY.as_bytes()), json!({}), claims)
}

/// The RSA public key as a JWK.
pub fn rsa_jwk(kid: &str) -> Value {
    let kp = signature::RsaKeyPair::from_der(RSA_PRIVATE).unwrap();
    let components: signature::RsaPublicKeyComponents<Vec<u8>> = kp.public().into();
    json!({
        "kty": "RSA", "kid": kid, "use": "sig", "alg": "RS256",
        "n": B64.encode(&components.n), "e": B64.encode(&components.e),
    })
}

pub fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
    let mut h = HeaderMap::new();
    for (k, v) in pairs {
        h.append(
            HeaderName::from_bytes(k.as_bytes()).unwrap(),
            v.parse().unwrap(),
        );
    }
    h
}

pub fn bearer(token: &str) -> String {
    format!("Bearer {token}")
}

pub fn far_future() -> u64 {
    9_999_999_999
}

pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

pub fn claims(allowed: &[&str], default: &str, extra: Value) -> Value {
    let mut hasura = json!({
        "x-hasura-allowed-roles": allowed,
        "x-hasura-default-role": default,
    });
    if let Value::Object(e) = extra {
        hasura.as_object_mut().unwrap().extend(e);
    }
    json!({"sub": "1", "exp": far_future(), "https://hasura.io/jwt/claims": hasura})
}

pub async fn jwt_auth(config: Value) -> Authenticator {
    Authenticator::new(AuthConfig {
        admin_secrets: vec!["secret".into()],
        jwt_secret: Some(config.to_string()),
        ..AuthConfig::default()
    })
    .await
    .unwrap()
}

pub async fn run(a: &Authenticator, pairs: &[(&str, &str)]) -> Result<Authenticated, AuthError> {
    a.authenticate(&headers(pairs), None).await
}

/// `(status code, code, message)` of an error, for compact assertions.
pub fn err(e: AuthError) -> (u16, &'static str, String) {
    (e.status.as_u16(), e.error.code.as_str(), e.error.message)
}

/// Serve a router on a random local port; returns its base URL.
pub async fn serve(router: axum::Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    format!("http://{addr}")
}
