# Lemmalog syntax

A dependency-free source parser, AST and uninterned terms extracted from Lemmalog. It contains no evaluator, interner, memory state, serde, MCP or native compiler integration.

```rust
use lemmalog_syntax::{parse_program, Term};
let rules = parse_program("visible(X) :- item(X).").unwrap();
assert_eq!(rules[0].head.args, vec![Term::Var("X".into())]);
```

Parsing establishes source syntax only. Each evaluator separately validates its supported constructs and semantics. The DDlog runtime currently accepts a narrower subset than this parser. MIT licensed with the upstream copyright retained.
