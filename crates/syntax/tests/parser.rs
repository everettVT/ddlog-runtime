use lemmalog_syntax::{parse_program, AggFn, Lit, Term};

#[test]
fn original_language_remains_available_independent_of_runtime_support() {
    let program = parse_program(
        "# full source grammar remains shared\n\
         seed(\"Alice\", -9).\n\
         temporal: current(E) :- edge(E,VF,VT), now(T), VF =< T, T < VT.\n\
         clear(E) :- edge(E,_,_), !blocked(E).\n\
         grouped(E, count(V)) :- pair(E,V).\n\
         next(E,D) :- depth(E,Prev), D = Prev + 1.\n",
    )
    .unwrap();
    assert_eq!(program.len(), 5);
    assert!(program[0].is_fact);
    assert_eq!(
        program[0].head.args,
        vec![Term::Sym("Alice".into()), Term::Int(-9)]
    );
    assert_eq!(program[1].name.as_deref(), Some("temporal"));
    assert!(matches!(program[1].body[1], Lit::Now(_)));
    assert!(matches!(program[2].body[1], Lit::Neg(_)));
    assert!(matches!(
        program[3].head.args[1],
        Term::Agg(AggFn::Count, _)
    ));
}

#[test]
fn invalid_and_incomplete_inputs_remain_recoverable_parse_errors() {
    for source in [
        "a(",
        "a(\"unterminated).",
        "a(9223372036854775808).",
        "a(X) :- .",
        "@",
    ] {
        assert!(parse_program(source).is_err(), "{source}");
    }
}
