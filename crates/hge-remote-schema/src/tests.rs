use super::*;

#[test]
fn request_body_omits_absent_keys() {
    let mut call = RemoteCall {
        query: "query  { hello  }".into(),
        operation_name: None,
        variables: None,
        response_key: "hello".into(),
        typenames: None,
    };
    assert_eq!(request_body(&call), r#"{"query":"query  { hello  }"}"#);
    call.operation_name = Some("HelloMe".into());
    let mut vars = Map::new();
    vars.insert("v".into(), json!(42));
    call.variables = Some(vars);
    assert_eq!(
        request_body(&call),
        r#"{"operationName":"HelloMe","query":"query  { hello  }","variables":{"v":42}}"#
    );
}

fn message(e: RemoteError) -> String {
    match e {
        RemoteError::Hasura(e) => e.message,
        RemoteError::Graphql(errs) => Json::Array(errs).to_string(),
    }
}

#[test]
fn data_and_errors() {
    let s = RemoteSettings::default();
    let p = RemoteSettings {
        prioritize_data: true,
    };
    assert!(extract_data(json!({"data": {"a": 1}}), s).is_ok());
    assert_eq!(
        message(
            extract_data(json!({"data": {"a": 1}, "errors": [{"message": "x"}]}), s).unwrap_err()
        ),
        r#"[{"message":"x"}]"#
    );
    assert!(extract_data(json!({"data": {"a": 1}, "errors": [{"message": "x"}]}), p).is_ok());
    assert_eq!(
        message(extract_data(json!({"data": null, "errors": [{"message": "x"}]}), p).unwrap_err()),
        r#"[{"message":"x"}]"#
    );
    assert_eq!(
        message(extract_data(json!({"data": {}, "errors": []}), s).unwrap_err()),
        "Empty \"errors\" field in response from remote"
    );
    assert_eq!(
        message(extract_data(json!({"data": {}, "errors": {}}), s).unwrap_err()),
        "Invalid \"errors\" field in response from remote"
    );
    assert_eq!(
        message(extract_data(json!({}), s).unwrap_err()),
        "Missing \"data\" field with no errors in response from remote"
    );
    assert_eq!(
        message(extract_data(json!({"data": null}), s).unwrap_err()),
        "Received null \"data\" field with no errors in response from remote"
    );
}
