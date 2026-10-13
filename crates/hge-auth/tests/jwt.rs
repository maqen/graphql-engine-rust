//! JWT mode with static keys (spec 05 §5), ported from V2's test_jwt.py / JWTSpec cases.

mod common;

use common::*;
use serde_json::json;

fn hs_config() -> serde_json::Value {
    json!({"type": "HS256", "key": HS_KEY})
}

#[tokio::test]
async fn role_selection() {
    let a = jwt_auth(hs_config()).await;
    let token = hs(&claims(
        &["user", "editor"],
        "user",
        json!({"x-hasura-user-id": "1"}),
    ));
    let ok = run(&a, &[("authorization", &bearer(&token))])
        .await
        .unwrap();
    assert_eq!(ok.session.role.as_str(), "user");
    assert_eq!(ok.session.variables.get("x-hasura-user-id"), Some("1"));
    assert_eq!(ok.session.variables.get("x-hasura-role"), Some("user"));
    assert_eq!(ok.session.variables.get("x-hasura-allowed-roles"), None);
    assert!(!ok.session.backend_only);
    assert!(ok.expires_at.is_some());

    let ok = run(
        &a,
        &[
            ("authorization", &bearer(&token)),
            ("x-hasura-role", "editor"),
            // Request headers do not become session variables in JWT mode.
            ("x-hasura-user-id", "999"),
            ("x-hasura-use-backend-only-permissions", "true"),
        ],
    )
    .await
    .unwrap();
    assert_eq!(ok.session.role.as_str(), "editor");
    assert_eq!(ok.session.variables.get("x-hasura-user-id"), Some("1"));
    assert!(!ok.session.backend_only);

    // The first x-hasura-role header decides; an empty one falls back to the default.
    let ok = run(
        &a,
        &[
            ("authorization", &bearer(&token)),
            ("x-hasura-role", ""),
            ("x-hasura-role", "editor"),
        ],
    )
    .await
    .unwrap();
    assert_eq!(ok.session.role.as_str(), "user");

    let e = run(
        &a,
        &[
            ("authorization", &bearer(&token)),
            ("x-hasura-role", "admin"),
        ],
    )
    .await
    .unwrap_err();
    assert_eq!(
        err(e),
        (
            400,
            "access-denied",
            "Your requested role is not in allowed roles".into()
        )
    );

    // A default role outside the allowed roles is fine until it is used.
    let token = hs(&claims(&["editor"], "user", json!({})));
    let ok = run(
        &a,
        &[
            ("authorization", &bearer(&token)),
            ("x-hasura-role", "editor"),
        ],
    )
    .await
    .unwrap();
    assert_eq!(ok.session.role.as_str(), "editor");
    let e = run(&a, &[("authorization", &bearer(&token))])
        .await
        .unwrap_err();
    assert_eq!(err(e).2, "Your requested role is not in allowed roles");
}

#[tokio::test]
async fn admin_secret_wins_and_wrong_secret_fails() {
    let a = jwt_auth(hs_config()).await;
    let token = hs(&claims(&["user"], "user", json!({})));
    let ok = run(
        &a,
        &[
            ("authorization", &bearer(&token)),
            ("x-hasura-admin-secret", "secret"),
        ],
    )
    .await
    .unwrap();
    assert!(ok.session.is_admin());
    let e = run(
        &a,
        &[
            ("authorization", &bearer(&token)),
            ("x-hasura-admin-secret", "nope"),
        ],
    )
    .await
    .unwrap_err();
    assert_eq!(
        err(e),
        (
            401,
            "access-denied",
            "invalid \"x-hasura-admin-secret\"".into()
        )
    );
}

#[tokio::test]
async fn token_location_errors() {
    let a = jwt_auth(hs_config()).await;
    let e = run(&a, &[]).await.unwrap_err();
    assert_eq!(
        err(e),
        (
            400,
            "invalid-headers",
            "Missing 'Authorization' or 'Cookie' header in JWT authentication mode".into()
        )
    );
    for bad in ["Basic abc", "bearer abc", "Bearer", "Bearer a b"] {
        let e = run(&a, &[("authorization", bad)]).await.unwrap_err();
        assert_eq!(
            err(e),
            (
                400,
                "invalid-headers",
                "Malformed Authorization header".into()
            ),
            "{bad}"
        );
    }
    let token = hs(&claims(&["user"], "user", json!({})));
    let e = run(
        &a,
        &[
            ("authorization", &bearer(&token)),
            ("authorization", &bearer(&token)),
        ],
    )
    .await
    .unwrap_err();
    assert_eq!(
        err(e),
        (
            400,
            "invalid-headers",
            "Could not verify JWT: Multiple JWTs found".into()
        )
    );
}

#[tokio::test]
async fn unauthorized_role_only_without_token() {
    let a = hge_auth::Authenticator::new(hge_auth::AuthConfig {
        admin_secrets: vec!["secret".into()],
        unauthorized_role: Some("anonymous".into()),
        jwt_secret: Some(hs_config().to_string()),
        ..Default::default()
    })
    .await
    .unwrap();
    let ok = run(&a, &[("x-hasura-user-id", "5")]).await.unwrap();
    assert_eq!(ok.session.role.as_str(), "anonymous");
    assert_eq!(ok.session.variables.get("x-hasura-user-id"), Some("5"));
    // An invalid token never falls back to the unauthorized role.
    let e = run(&a, &[("authorization", "Bearer a.b.c")])
        .await
        .unwrap_err();
    assert_eq!(err(e).1, "invalid-jwt");
}

#[tokio::test]
async fn verification_errors() {
    let a = jwt_auth(hs_config()).await;
    let check = |claims: serde_json::Value| hs(&claims);

    let mut c = claims(&["user"], "user", json!({}));
    c["exp"] = json!(now() - 10);
    let e = run(&a, &[("authorization", &bearer(&check(c)))])
        .await
        .unwrap_err();
    assert_eq!(
        err(e),
        (
            400,
            "invalid-jwt",
            "Could not verify JWT: JWTExpired".into()
        )
    );

    let mut c = claims(&["user"], "user", json!({}));
    c["nbf"] = json!(now() + 100);
    let e = run(&a, &[("authorization", &bearer(&check(c)))])
        .await
        .unwrap_err();
    assert_eq!(err(e).2, "Could not verify JWT: JWTNotYetValid");

    let other = sign(
        Signer::Hs256(b"another-secret-that-is-at-least-32-bytes"),
        json!({}),
        &claims(&["user"], "user", json!({})),
    );
    let e = run(&a, &[("authorization", &bearer(&other))])
        .await
        .unwrap_err();
    assert_eq!(
        err(e).2,
        "Could not verify JWT: JWSError JWSInvalidSignature"
    );

    let rs = sign(
        Signer::Rs256,
        json!({}),
        &claims(&["user"], "user", json!({})),
    );
    let e = run(&a, &[("authorization", &bearer(&rs))])
        .await
        .unwrap_err();
    assert_eq!(err(e).2, "Could not verify JWT: JWSError NoUsableKeys");

    let e = run(&a, &[("authorization", "Bearer notatoken")])
        .await
        .unwrap_err();
    let (status, code, message) = err(e);
    assert_eq!((status, code), (400, "invalid-jwt"));
    assert!(
        message.starts_with("Could not verify JWT: JWSError (CompactDecodeError"),
        "{message}"
    );

    // `alg: none` is never accepted.
    let parts: Vec<&str> = rs.split('.').collect();
    let none = format!("{}.{}.", base64_url(r#"{"alg":"none"}"#), parts[1]);
    let e = run(&a, &[("authorization", &bearer(&none))])
        .await
        .unwrap_err();
    assert_eq!(err(e).1, "invalid-jwt");
}

fn base64_url(s: &str) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(s)
}

#[tokio::test]
async fn skew_audience_issuer() {
    let a = jwt_auth(json!({
        "type": "HS256", "key": HS_KEY,
        "allowed_skew": 60, "audience": ["app", "other"], "issuer": "https://issuer"
    }))
    .await;
    let mut c = claims(&["user"], "user", json!({}));
    c["exp"] = json!(now() - 10);
    c["aud"] = json!("app");
    c["iss"] = json!("https://issuer");
    assert!(
        run(&a, &[("authorization", &bearer(&hs(&c)))])
            .await
            .is_ok(),
        "within skew"
    );

    c["aud"] = json!(["x", "other"]);
    assert!(
        run(&a, &[("authorization", &bearer(&hs(&c)))])
            .await
            .is_ok()
    );
    c["aud"] = json!("nope");
    let e = run(&a, &[("authorization", &bearer(&hs(&c)))])
        .await
        .unwrap_err();
    assert_eq!(err(e).2, "Could not verify JWT: JWTNotInAudience");

    c["aud"] = json!("app");
    c["iss"] = json!("https://elsewhere");
    let e = run(&a, &[("authorization", &bearer(&hs(&c)))])
        .await
        .unwrap_err();
    assert_eq!(err(e).2, "Could not verify JWT: JWTNotInIssuer");

    // Missing aud / iss pass unless required.
    let c = claims(&["user"], "user", json!({}));
    assert!(
        run(&a, &[("authorization", &bearer(&hs(&c)))])
            .await
            .is_ok()
    );
    let strict = jwt_auth(json!({
        "type": "HS256", "key": HS_KEY, "audience": "app", "issuer": "i",
        "extra_required_claims": ["audience"]
    }))
    .await;
    let e = run(&strict, &[("authorization", &bearer(&hs(&c)))])
        .await
        .unwrap_err();
    assert_eq!(err(e).2, "Could not verify JWT: JWTNotInAudience");
    let strict = jwt_auth(json!({
        "type": "HS256", "key": HS_KEY, "issuer": "i", "extra_required_claims": ["issuer"]
    }))
    .await;
    let e = run(&strict, &[("authorization", &bearer(&hs(&c)))])
        .await
        .unwrap_err();
    assert_eq!(err(e).2, "Could not verify JWT: JWTNotInIssuer");
}

#[tokio::test]
async fn claims_errors() {
    let a = jwt_auth(hs_config()).await;
    let go = |c: serde_json::Value| {
        let a = &a;
        async move {
            err(run(a, &[("authorization", &bearer(&hs(&c)))])
                .await
                .unwrap_err())
        }
    };
    assert_eq!(
        go(json!({"exp": far_future()})).await,
        (
            400,
            "jwt-invalid-claims",
            "claims key: 'https://hasura.io/jwt/claims' not found".into()
        )
    );
    assert_eq!(
        go(json!({"https://hasura.io/jwt/claims": "x"})).await,
        (
            400,
            "jwt-invalid-claims",
            "expecting a json object when claims_format is json".into()
        )
    );
    assert_eq!(
        go(json!({"https://hasura.io/jwt/claims": {"x-hasura-default-role": "user"}})).await,
        (
            400,
            "jwt-missing-role-claims",
            "JWT claim does not contain \"x-hasura-allowed-roles\"".into()
        )
    );
    assert_eq!(
        go(json!({"https://hasura.io/jwt/claims": {"x-hasura-allowed-roles": ["user"]}})).await,
        (
            400,
            "jwt-missing-role-claims",
            "JWT claim does not contain \"x-hasura-default-role\"".into()
        )
    );
    assert_eq!(
        go(json!({"https://hasura.io/jwt/claims": {"x-hasura-allowed-roles": "user", "x-hasura-default-role": "user"}})).await,
        (400, "jwt-invalid-claims", "invalid \"x-hasura-allowed-roles\"; should be a list of roles: parsing [] failed, expected Array, but encountered String".into())
    );
    assert_eq!(
        go(json!({"https://hasura.io/jwt/claims": {"x-hasura-allowed-roles": ["user"], "x-hasura-default-role": ["user"]}})).await,
        (400, "jwt-invalid-claims", "invalid \"x-hasura-default-role\"; should be a single role name: parsing Text failed, expected String, but encountered Array".into())
    );
    assert_eq!(
        go(claims(&["user"], "user", json!({"x-hasura-user-id": 1}))).await,
        (
            400,
            "jwt-invalid-claims",
            "x-hasura-* claims: parsing Text failed, expected String, but encountered Number"
                .into()
        )
    );
}

#[tokio::test]
async fn namespaces_and_formats() {
    // Upper-case claim names work; non x-hasura keys are dropped.
    let a = jwt_auth(hs_config()).await;
    let c = json!({"exp": far_future(), "https://hasura.io/jwt/claims": {
        "X-Hasura-Allowed-Roles": ["user"], "X-Hasura-Default-Role": "user",
        "X-Hasura-Org-Id": "7", "other": 1
    }});
    let ok = run(&a, &[("authorization", &bearer(&hs(&c)))])
        .await
        .unwrap();
    assert_eq!(ok.session.variables.get("x-hasura-org-id"), Some("7"));
    assert_eq!(ok.session.variables.get("other"), None);

    let a = jwt_auth(json!({"type": "HS256", "key": HS_KEY, "claims_namespace": "hasura", "claims_format": "stringified_json"})).await;
    let inner = json!({"x-hasura-allowed-roles": ["user"], "x-hasura-default-role": "user", "x-hasura-user-id": "3"});
    let c = json!({"exp": far_future(), "hasura": inner.to_string()});
    let ok = run(&a, &[("authorization", &bearer(&hs(&c)))])
        .await
        .unwrap();
    assert_eq!(ok.session.variables.get("x-hasura-user-id"), Some("3"));
    let c = json!({"exp": far_future(), "hasura": "{nope"});
    let e = run(&a, &[("authorization", &bearer(&hs(&c)))])
        .await
        .unwrap_err();
    assert_eq!(
        err(e).2,
        "expecting stringified json at: 'claims_namespace hasura', but found: {nope"
    );
    let c = json!({"exp": far_future(), "hasura": inner});
    let e = run(&a, &[("authorization", &bearer(&hs(&c)))])
        .await
        .unwrap_err();
    assert_eq!(
        err(e).2,
        "expecting a string when claims_format is stringified_json"
    );

    let a =
        jwt_auth(json!({"type": "HS256", "key": HS_KEY, "claims_namespace_path": "$.app.hasura"}))
            .await;
    let c = json!({"exp": far_future(), "app": {"hasura": {"x-hasura-allowed-roles": ["user"], "x-hasura-default-role": "user"}}});
    assert!(
        run(&a, &[("authorization", &bearer(&hs(&c)))])
            .await
            .is_ok()
    );
    let c = json!({"exp": far_future(), "app": {}});
    let e = run(&a, &[("authorization", &bearer(&hs(&c)))])
        .await
        .unwrap_err();
    assert_eq!(
        err(e),
        (
            400,
            "jwt-invalid-claims",
            "claims not found at claims_namespace_path: '$.app.hasura'".into()
        )
    );

    // `$`: the Hasura claims are top-level claims.
    let a = jwt_auth(json!({"type": "HS256", "key": HS_KEY, "claims_namespace_path": "$"})).await;
    let c = json!({"exp": far_future(), "x-hasura-allowed-roles": ["user"], "x-hasura-default-role": "user", "x-hasura-user-id": "9"});
    let ok = run(&a, &[("authorization", &bearer(&hs(&c)))])
        .await
        .unwrap();
    assert_eq!(ok.session.variables.get("x-hasura-user-id"), Some("9"));
}

#[tokio::test]
async fn claims_map() {
    let a = jwt_auth(json!({
        "type": "HS256", "key": HS_KEY,
        "claims_map": {
            "x-hasura-allowed-roles": {"path": "$.app.roles"},
            "x-hasura-default-role": {"path": "$.app.roles[0]", "default": "user"},
            "x-hasura-user-id": {"path": "$.sub"},
            "x-hasura-org": "acme",
            "x-hasura-team": {"path": "$.team", "default": "none"}
        }
    }))
    .await;
    let c = json!({"sub": "u1", "exp": far_future(), "app": {"roles": ["editor", "user"]}});
    let ok = run(&a, &[("authorization", &bearer(&hs(&c)))])
        .await
        .unwrap();
    assert_eq!(ok.session.role.as_str(), "editor");
    assert_eq!(ok.session.variables.get("x-hasura-user-id"), Some("u1"));
    assert_eq!(ok.session.variables.get("x-hasura-org"), Some("acme"));
    assert_eq!(ok.session.variables.get("x-hasura-team"), Some("none"));

    let c = json!({"exp": far_future(), "app": {"roles": ["user"]}});
    let e = run(&a, &[("authorization", &bearer(&hs(&c)))])
        .await
        .unwrap_err();
    assert_eq!(
        err(e),
        (
            400,
            "jwt-invalid-claims",
            "JWT claim from claims_map, \"x-hasura-user-id\" not found".into()
        )
    );

    let c = json!({"sub": "u1", "exp": far_future()});
    let e = run(&a, &[("authorization", &bearer(&hs(&c)))])
        .await
        .unwrap_err();
    assert_eq!(
        err(e),
        (
            400,
            "jwt-missing-role-claims",
            "JWT claim does not contain \"x-hasura-allowed-roles\"".into()
        )
    );

    let c = json!({"sub": 5, "exp": far_future(), "app": {"roles": ["user"]}});
    let e = run(&a, &[("authorization", &bearer(&hs(&c)))])
        .await
        .unwrap_err();
    assert_eq!(err(e).1, "jwt-invalid-claims");
}

#[tokio::test]
async fn cookie_and_custom_header() {
    let a = jwt_auth(
        json!({"type": "HS256", "key": HS_KEY, "header": {"type": "Cookie", "name": "jwt"}}),
    )
    .await;
    let token = hs(&claims(&["user"], "user", json!({})));
    let cookie = format!("a=b; jwt={token}; c=d");
    assert!(run(&a, &[("cookie", &cookie)]).await.is_ok());
    // An Authorization header is not looked at.
    let e = run(&a, &[("authorization", "Basic x")]).await.unwrap_err();
    assert_eq!(err(e).1, "invalid-headers");

    let a = jwt_auth(json!({"type": "HS256", "key": HS_KEY, "header": {"type": "CustomHeader", "name": "X-Auth-Token"}})).await;
    assert!(run(&a, &[("x-auth-token", &token)]).await.is_ok());
}

#[tokio::test]
async fn asymmetric_keys() {
    for (ty, key, signer) in [
        ("RS256", RSA_PUBLIC, Signer::Rs256),
        ("ES256", EC_PUBLIC, Signer::Es256),
        ("Ed25519", ED_PUBLIC, Signer::EdDsa),
    ] {
        let a = jwt_auth(json!({"type": ty, "key": key})).await;
        let token = sign(
            signer,
            json!({"kid": "any"}),
            &claims(&["user"], "user", json!({})),
        );
        let ok = run(&a, &[("authorization", &bearer(&token))]).await;
        assert!(ok.is_ok(), "{ty}: {ok:?}");
        let e = run(
            &a,
            &[(
                "authorization",
                &bearer(&hs(&claims(&["user"], "user", json!({})))),
            )],
        )
        .await
        .unwrap_err();
        assert_eq!(
            err(e).2,
            "Could not verify JWT: JWSError NoUsableKeys",
            "{ty}"
        );
    }
}

#[tokio::test]
async fn multiple_configs_by_issuer() {
    let a = hge_auth::Authenticator::new(hge_auth::AuthConfig {
        admin_secrets: vec!["secret".into()],
        jwt_secrets: Some(
            json!([
                {"type": "HS256", "key": HS_KEY, "issuer": "one"},
                {"type": "RS256", "key": RSA_PUBLIC, "issuer": "two"}
            ])
            .to_string(),
        ),
        ..Default::default()
    })
    .await
    .unwrap();
    let mut c = claims(&["user"], "user", json!({}));
    c["iss"] = json!("one");
    assert!(
        run(&a, &[("authorization", &bearer(&hs(&c)))])
            .await
            .is_ok()
    );
    c["iss"] = json!("two");
    let rs = sign(Signer::Rs256, json!({}), &c);
    assert!(run(&a, &[("authorization", &bearer(&rs))]).await.is_ok());
    c["iss"] = json!("three");
    let e = run(&a, &[("authorization", &bearer(&hs(&c)))])
        .await
        .unwrap_err();
    assert_eq!(err(e).2, "Could not verify JWT: JWTNotInIssuer");
    // No iss matches both configs.
    let c = claims(&["user"], "user", json!({}));
    let e = run(&a, &[("authorization", &bearer(&hs(&c)))])
        .await
        .unwrap_err();
    assert_eq!(err(e).2, "Could not verify JWT: Multiple JWTs found");
}

#[tokio::test]
async fn hostile_but_signed_claims_do_not_panic() {
    let a = jwt_auth(hs_config()).await;
    // A non-ASCII key whose 9th byte is inside a character.
    let c = claims(&["user"], "user", json!({"x-hasuraé": "1"}));
    assert!(
        run(&a, &[("authorization", &bearer(&hs(&c)))])
            .await
            .is_ok()
    );
    // An `exp` beyond any Duration.
    let mut c = claims(&["user"], "user", json!({}));
    c["exp"] = json!(1e20);
    let ok = run(&a, &[("authorization", &bearer(&hs(&c)))])
        .await
        .unwrap();
    assert_eq!(ok.expires_at, None);
    // jose rejects an empty audience list.
    let mut c = claims(&["user"], "user", json!({}));
    c["aud"] = json!([]);
    let e = run(&a, &[("authorization", &bearer(&hs(&c)))])
        .await
        .unwrap_err();
    assert_eq!(err(e).2, "Could not verify JWT: JWTNotInAudience");
}
