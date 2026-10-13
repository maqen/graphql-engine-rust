//! Deeply nested templates must neither overflow the stack (parse, eval, drop) nor be
//! accepted beyond the nesting limit. Runs on a 2 MiB thread, the default for spawned threads.

use hge_kriti::{Bindings, ErrorCode, Functions, Template};
use serde_json::{Value, json};

fn on_small_stack(f: impl FnOnce() + Send + 'static) {
    std::thread::Builder::new()
        .stack_size(2 * 1024 * 1024)
        .spawn(f)
        .unwrap()
        .join()
        .unwrap();
}

fn nested(open: &str, inner: &str, close: &str, n: usize) -> String {
    format!("{}{inner}{}", open.repeat(n), close.repeat(n))
}

/// Builds `{"a": {"a": … 1 …}}` `n` levels deep.
fn deep_body(n: usize) -> Value {
    (0..n).fold(json!(1), |v, _| json!({ "a": v }))
}

/// Builds a template with the given nesting level.
type Shape = Box<dyn Fn(usize) -> String>;

/// Templates parametrised by nesting level.
fn shapes() -> Vec<(&'static str, Shape)> {
    vec![
        ("arrays", Box::new(|n| nested("[", "1", "]", n))),
        ("parens", Box::new(|n| nested("(", "1", ")", n))),
        ("objects", Box::new(|n| nested("{\"a\": ", "1", "}", n))),
        ("wrappers", Box::new(|n| nested("{{ ", "1", " }}", n))),
        ("not", Box::new(|n| format!("{}true", "not ".repeat(n)))),
        ("calls", Box::new(|n| nested("size(", "1", ")", n))),
        (
            "ifs",
            Box::new(|n| nested("{{ if true }} ", "1", " {{ else }} 0 {{ end }}", n)),
        ),
        (
            "ranges",
            Box::new(|n| nested("{{ range _, x := [1] }} ", "x", " {{ end }}", n)),
        ),
        ("templates", Box::new(|n| nested("\"{{ ", "1", " }}\"", n))),
        ("fields", Box::new(|n| format!("$body{}", ".a".repeat(n)))),
        (
            "optionals",
            Box::new(|n| format!("$body{}", "?.a".repeat(n))),
        ),
        ("indexes", Box::new(|n| nested("$i[", "0", "]", n))),
    ]
}

#[test]
fn nesting_limit_is_enforced_without_overflow() {
    on_small_stack(|| {
        let vars = Bindings::new()
            .with("$body", deep_body(300))
            .with("$i", json!([0]));
        let funcs = Functions::new();
        for (name, build) in shapes() {
            // Close to the limit: accepted, evaluated and dropped.
            let t =
                Template::parse(&build(250)).unwrap_or_else(|e| panic!("{name}: {}", e.to_json()));
            if let Err(e) = t.eval(&vars, &funcs) {
                assert_ne!(e.error_code, ErrorCode::Parse, "{name}");
            }
            drop(t);
            // Far beyond the limit: a parse error, not a crash.
            let e = Template::parse(&build(5000)).unwrap_err();
            assert_eq!(e.error_code, ErrorCode::Parse, "{name}");
            assert_eq!(
                e.message, "Maximum nesting depth of 256 exceeded.",
                "{name}"
            );
        }
    });
}

#[test]
fn deep_values_evaluate() {
    on_small_stack(|| {
        let vars = Bindings::new().with("$body", deep_body(300));
        let t = Template::parse(&format!("$body{}", ".a".repeat(250))).unwrap();
        assert_eq!(t.eval(&vars, &Functions::new()).unwrap(), deep_body(50));
        let t = Template::parse(&nested("[", "1", "]", 250)).unwrap();
        let mut v = t.eval(&vars, &Functions::new()).unwrap();
        for _ in 0..250 {
            v = v.as_array().unwrap()[0].clone();
        }
        assert_eq!(v, json!(1));
    });
}
