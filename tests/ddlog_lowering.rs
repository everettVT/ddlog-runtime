use ddlog_runtime::{lower, Schema};
use std::collections::BTreeMap;
fn schemas() -> BTreeMap<String, Schema> {
    serde_json::from_value(serde_json::json!({
        "finding":{"input":true,"fields":["int","int","int"]},
        "actionable":{"input":false,"fields":["int","int"]}
    }))
    .unwrap()
}
#[test]
fn lowers_existing_ast_and_checks_types() {
    let source = lower("actionable(P,F) :- finding(P,F,S), S =< 2.", &schemas()).unwrap();
    assert!(source.contains("v_S <= 2"));
    assert!(source.contains("Evidence0"));
    for bad in [
        "actionable(P,F) :- finding(P,F,S), !actionable(P,F).",
        "actionable(P,X) :- finding(P,F,S).",
        "actionable(P,F) :- finding(P,F,S), S < \"two\".",
        "actionable(P,F) :- actionable(P,F).",
        "actionable(P,F) :- finding(P,F).",
    ] {
        assert!(lower(bad, &schemas()).is_err(), "{bad}");
    }
}

#[test]
fn rejects_cli_incompatible_controls_before_install() {
    let schema = serde_json::from_value(serde_json::json!({
        "source": {"input":true,"fields":["string"]},
        "result": {"input":false,"fields":["string"]}
    }))
    .unwrap();
    assert!(lower("result(X) :- source(X), X = \"\u{0}\".", &schema).is_err());
    assert!(lower("result(X) :- source(X), X = \"café\".", &schema).is_ok());
}
