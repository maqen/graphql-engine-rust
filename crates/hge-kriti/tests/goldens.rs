//! Upstream kriti-lang golden tests (`test/data`, pinned commit c8fd863).

use hge_kriti::{Bindings, ErrorCode, Functions, Template};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

fn data(rel: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/data")
        .join(rel)
}

fn kriti_files(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<_> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "kriti"))
        .collect();
    files.sort();
    files
}

/// Canonical text: sorted keys, integral floats printed as integers.
fn canonical(v: &Value) -> String {
    fn norm(v: &Value) -> Value {
        match v {
            Value::Array(a) => Value::Array(a.iter().map(norm).collect()),
            Value::Object(m) => {
                let mut entries: Vec<_> = m.iter().map(|(k, v)| (k.clone(), norm(v))).collect();
                entries.sort_by(|a, b| a.0.cmp(&b.0));
                Value::Object(entries.into_iter().collect())
            }
            Value::Number(n) => match n.as_f64() {
                Some(f) if !n.is_i64() && !n.is_u64() && f.fract() == 0.0 => (f as i64).into(),
                _ => v.clone(),
            },
            _ => v.clone(),
        }
    }
    norm(v).to_string()
}

#[test]
fn eval_success_goldens() {
    let source: Value =
        serde_json::from_str(&fs::read_to_string(data("eval/success/source.json")).unwrap())
            .unwrap();
    let vars = Bindings::new().with("$", source);
    let funcs = Functions::new();
    let files = kriti_files(&data("eval/success/examples"));
    assert_eq!(files.len(), 34);
    let mut failures = Vec::new();
    for path in files {
        let name = path.file_stem().unwrap().to_str().unwrap().to_owned();
        let src = fs::read_to_string(&path).unwrap();
        let golden: Value = serde_json::from_str(
            &fs::read_to_string(data(&format!("eval/success/golden/{name}.json"))).unwrap(),
        )
        .unwrap();
        match Template::parse(&src).and_then(|t| t.eval(&vars, &funcs)) {
            Ok(v) if canonical(&v) == canonical(&golden) => {}
            Ok(v) => failures.push(format!("{name}: got {v}, want {golden}")),
            Err(e) => failures.push(format!("{name}: error {}", e.to_json())),
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn parser_success_examples() {
    let files = kriti_files(&data("parser/success"));
    assert_eq!(files.len(), 15);
    for path in files {
        let src = fs::read_to_string(&path).unwrap();
        if let Err(e) = Template::parse(&src) {
            panic!("{}: {}", path.display(), e.to_json());
        }
    }
}

#[test]
fn parser_failure_goldens() {
    // (file, message, start (line, col), end (line, col)) from `parser/failure/golden`.
    let cases = [
        (
            "missingClosingBracket",
            "Unexpected token '}'.",
            (0, 12),
            (1, 1),
        ),
        ("numAsKey", "Unexpected token '1'.", (0, 12), (1, 1)),
        (
            "trailingCommaInArray",
            "Unexpected token ']'.",
            (0, 9),
            (1, 1),
        ),
    ];
    for (name, message, start, end) in cases {
        let src = fs::read_to_string(data(&format!("parser/failure/{name}.kriti"))).unwrap();
        let e = Template::parse(&src).unwrap_err();
        assert_eq!(e.error_code, ErrorCode::Parse, "{name}");
        assert_eq!(e.message, message, "{name}");
        assert_eq!((e.span.start.line, e.span.start.column), start, "{name}");
        assert_eq!((e.span.end.line, e.span.end.column), end, "{name}");
    }
}

/// kriti's JSON edge cases (`jsonParse` in `test/Spec.hs`).
#[test]
fn json_edge_cases() {
    let run =
        |s: &str| Template::parse(s).and_then(|t| t.eval(&Bindings::new(), &Functions::new()));
    assert_eq!(run(r#""\u001c""#).unwrap(), Value::String("\u{1c}".into()));
    assert_eq!(run(r#""σ""#).unwrap(), Value::String("σ".into()));
    assert_eq!(run(r#""{""#).unwrap(), Value::String("{".into()));
    assert_eq!(run(r#""\{{""#).unwrap(), Value::String("{{".into()));
    assert_eq!(run(r#""σ""#).unwrap(), Value::String("σ".into()));
    assert_eq!(run(r#""A""#).unwrap(), Value::String("A".into()));
    let json = r#"{"a": [1, -2.5, true, false, null, {"b": "c"}], "": {}}"#;
    assert_eq!(
        canonical(&run(json).unwrap()),
        canonical(&serde_json::from_str(json).unwrap())
    );
}
