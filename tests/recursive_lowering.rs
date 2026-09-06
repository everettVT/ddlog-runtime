use ddlog_runtime::{lower, lower_with_operators, star::Operator, Schema};
use std::collections::BTreeMap;

fn schema() -> BTreeMap<String, Schema> {
    serde_json::from_value(serde_json::json!({
        "edge": {"input": true, "fields": ["string", "string"]},
        "blocked": {"input": true, "fields": ["string", "int"]},
        "node": {"input": true, "fields": ["string"]},
        "path": {"input": false, "fields": ["string", "string"]},
        "eligible": {"input": false, "fields": ["string"]},
        "other": {"input": false, "fields": ["string"]}
    }))
    .unwrap()
}

const CLOSURE: &str = "path(X,Y) :- edge(X,Y).\n\
                      trans: path(X,Z) :- path(X,Y), edge(Y,Z).";

#[test]
fn positive_acyclic_source_is_byte_compatible() {
    let schemas = serde_json::from_value(serde_json::json!({
        "finding":{"input":true,"fields":["int","int","int"]},
        "actionable":{"input":false,"fields":["int","int"]}
    }))
    .unwrap();
    assert_eq!(
        lower("actionable(P,F) :- finding(P,F,S), S =< 2.", &schemas).unwrap(),
        "output relation R_actionable(f0: signed<64>, f1: signed<64>)\n\
         input relation R_finding(f0: signed<64>, f1: signed<64>, f2: signed<64>)\n\
         output relation Evidence0(v_F: signed<64>, v_P: signed<64>, v_S: signed<64>)\n\
         Evidence0(v_F, v_P, v_S) :- R_finding(v_P, v_F, v_S), v_S <= 2.\n\
         R_actionable(v_P, v_F) :- Evidence0(v_F, v_P, v_S).\n"
    );
}

#[test]
fn lowers_positive_recursive_rules_with_direct_witnesses() {
    let source = lower(CLOSURE, &schema()).unwrap();
    assert!(source.contains("Evidence1(v_X, v_Y, v_Z) :- R_path(v_X, v_Y), R_edge(v_Y, v_Z)."));
    assert!(source.contains("R_path(v_X, v_Z) :- Evidence1(v_X, v_Y, v_Z)."));
    // Mutual and unseeded positive recursion are valid finite-domain programs.
    lower(
        "eligible(X) :- node(X). eligible(X) :- other(X). other(X) :- eligible(X).",
        &schema(),
    )
    .unwrap();
    lower("eligible(X) :- eligible(X).", &schema()).unwrap();
}

#[test]
fn stratified_negation_accepts_bound_variables_constants_and_wildcards() {
    let source = lower(
        "eligible(X) :- !blocked(X,_), node(X), X \\= \"excluded\".",
        &schema(),
    )
    .unwrap();
    assert!(source
        .contains("Evidence0(v_X) :- R_node(v_X), v_X != \"excluded\", not R_blocked(v_X, _)."));
    lower(
        "eligible(X) :- node(X), !blocked(\"fixed\",2). other(X) :- node(X), !eligible(X).",
        &schema(),
    )
    .unwrap();
    // A stratified negation may consume a completed positive recursive SCC.
    lower(
        &format!("{CLOSURE} eligible(X) :- node(X), !path(X,\"blocked\")."),
        &schema(),
    )
    .unwrap();
}

#[test]
fn negation_does_not_bind_variables_or_bypass_type_and_arity_checks() {
    for (rules, message) in [
        (
            "eligible(X) :- node(X), !blocked(Y,_).",
            "Unbound negated variable Y",
        ),
        (
            "eligible(X) :- node(X), !blocked(_,X).",
            "Conflicting types for X",
        ),
        (
            "eligible(Y) :- node(X), !blocked(Y,_).",
            "Unbound head variable Y",
        ),
        ("eligible(X) :- node(X), !blocked(X).", "Arity mismatch"),
        (
            "eligible(X) :- node(X), !missing(X).",
            "Undeclared relation",
        ),
        (
            "eligible(X) :- node(X), !blocked(X,\"not_int\").",
            "mismatched term",
        ),
        ("eligible(_) :- node(X), !blocked(X,_).", "mismatched term"),
    ] {
        assert!(
            lower(rules, &schema()).unwrap_err().contains(message),
            "{rules}: expected {message}"
        );
    }
}

#[test]
fn rejects_negative_cycles_including_through_positive_rules() {
    for rules in [
        "eligible(X) :- node(X), !eligible(X).",
        "eligible(X) :- other(X). other(X) :- node(X), !eligible(X).",
        "eligible(X) :- node(X), !other(X). other(X) :- node(X), !eligible(X).",
        "eligible(X) :- node(X), !other(X). other(X) :- eligible(X). other(X) :- other(X).",
    ] {
        assert!(
            lower(rules, &schema())
                .unwrap_err()
                .contains("Unstratified negation"),
            "{rules}"
        );
    }
}

#[test]
fn native_operator_dependencies_participate_in_cycle_validation() {
    let schemas = serde_json::from_value(serde_json::json!({
        "source":{"input":true,"fields":["int","int"]},
        "vertices":{"input":true,"fields":["int"]},
        "edges":{"input":false,"fields":["int","int"]},
        "labels":{"input":false,"fields":["int","int"]}
    }))
    .unwrap();
    let operators = [Operator::LargeSmallStar {
        vertices: "vertices".into(),
        edges: "edges".into(),
        output: "labels".into(),
    }];
    lower_with_operators("edges(X,Y) :- source(X,Y).", &schemas, &operators).unwrap();
    for rules in [
        "edges(X,Y) :- labels(X,Y).",
        "edges(X,Y) :- source(X,Y), !labels(X,Y).",
    ] {
        assert!(
            lower_with_operators(rules, &schemas, &operators).is_err(),
            "{rules}"
        );
    }
}

#[test]
fn expanding_the_rule_subset_preserves_remaining_rejections() {
    for rules in [
        "eligible(X :- node(X).",
        "eligible(\"literal\").",
        "eligible(X) :- node(X), now(T).",
        "eligible(X) :- node(X), Y = 1 + 2.",
        "eligible(count(X)) :- node(X).",
        "eligible(X) :- node(X), X > \"lexical\".",
    ] {
        assert!(lower(rules, &schema()).is_err(), "{rules}");
    }
}

/// This invokes the official DDlog compiler's validation pass only. It does not
/// compile generated Rust, execute Differential, or substitute an evaluator.
#[test]
#[ignore = "requires DDLOG_HOME and DDLOG_COMPILER for the pinned native distribution"]
fn native_compiler_accepts_recursive_witnesses_and_stratified_negation() {
    let compiler = std::env::var("DDLOG_COMPILER").expect("set DDLOG_COMPILER");
    let directory = std::env::temp_dir().join(format!(
        "ddlog-recursive-lowering-validation-{}",
        std::process::id()
    ));
    std::fs::create_dir(&directory).unwrap();
    for (name, rules) in [
        ("closure", CLOSURE.to_owned()),
        (
            "negation",
            "eligible(X) :- !blocked(X,_), node(X).".to_owned(),
        ),
        (
            "stratified_closure",
            format!("{CLOSURE} eligible(X) :- node(X), !path(X,\"blocked\")."),
        ),
        (
            "mutual_recursion",
            "eligible(X) :- node(X). eligible(X) :- other(X). other(X) :- eligible(X).".to_owned(),
        ),
        (
            "global_absence",
            "eligible(X) :- node(X), !blocked(_, _).".to_owned(),
        ),
    ] {
        let source = directory.join(format!("{name}.dl"));
        std::fs::write(&source, lower(&rules, &schema()).unwrap()).unwrap();
        let output = std::process::Command::new(&compiler)
            .arg("--action=validate")
            .arg("-i")
            .arg(&source)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{name}: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    std::fs::remove_dir_all(directory).unwrap();
}
