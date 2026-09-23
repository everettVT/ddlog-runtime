# Lean proofs: program composition

Machine-checked proofs about the meaning of composed programs
(`src/composition.rs`), in Lean 4 with no dependencies beyond core.

```sh
curl -sSfL https://raw.githubusercontent.com/leanprover/elan/master/elan-init.sh | sh -s -- -y --default-toolchain none
cd verification/lean && lake build        # toolchain pinned in lean-toolchain
```

A successful build checks every theorem. The development has no `sorry`, and the
headline theorems use only Lean's standard axioms (`propext`, `Quot.sound`,
`Classical.choice`). `lake env lean` with `#print axioms <name>` shows this.

## What is modelled

| File | Models | Rust |
| --- | --- | --- |
| `Datalog.lean` | Rules with positive/negated atoms and comparison guards, binding copy rules, native operators; stable-model semantics | `lower.rs`, `composition::bridge`, `star::Operator` |
| `Localize.lean` | Generic isolation lemma for an injectively renamed sub-program fed only through bridges | — |
| `Composition.lean` | `Component` (leaf program with interface / composition of nodes, nested to any depth), `compile` (version 1 source expansion), `WellFormed` (the checks in `Expansion::manifest`) | `Expansion::{program, manifest}` |
| `Compose.lean` | Compositionality theorems | — |
| `Alias.lean` | Simultaneous alias resolution (version 2) on an arbitrary program | `Expansion::resolve_aliases` |
| `Version2.lean` | Alias resolution instantiated for a composition's bindings | `LoweringOptions::VERSION_2` |
| `Cycle.lean` | A concrete cyclic composition | — |

**Semantics.** `Derives prog edb I` is the least set of facts closed under the
clauses, with negated atoms and native-operator inputs read from a fixed
interpretation `I`. `Stable prog edb M` means `M = Derives prog edb M`, i.e. `M`
is a stable model. The lowerer admits only stratified programs (no negative or
native edge inside a dependency cycle). For those programs the unique stable
model is the stratified model that DDlog maintains. Every theorem is stated
either for all oracles `I` or for all stable models, so none of them needs a
stratification argument.

**Names.** Generated names are a free type: `loc r`, `input x`, `output o`,
`node a n`. Rust flattens node paths to `Module<k>_r` / `Composite<k>_…` with a
per-tree unique counter `k`. Identifiers start with a letter (`lower::ident`),
so that encoding is injective, and the model uses the path itself.

## Theorems

For a composition `c` whose nodes, inputs, bindings and outputs satisfy
`WellFormed`, write `D` for `Derives c.compile (c.edbOf E) I`, where `E` feeds
the external inputs.

- `compose_input`: `D (input x)` holds exactly the rows fed to `x`.
- `compose_output`: `D (output o)` holds exactly the rows of the node output
  port that `o` names.
- `compose_node` (isolation and interface preservation): inside node `a`, `D`
  derives exactly what `a`'s own compiled program derives when each input port
  `q` is fed `D` at the unique source bound to `q`. Private relations of other
  nodes cannot leak in. A nested composition node is again characterised by
  these theorems, so the statement holds at every depth.
- `compose_loc`, `compose_unknown_node`: nothing else is derived.
- `stable_restrict`: every stable model of the composition restricts to a
  stable model of every node.
- `Cycle.circulating_not_stable` and `Cycle.circulating_locally_stable`: the
  converse of `stable_restrict` is false. In a two-node cycle
  `A.out → B.in → B.out → A.in` with no external input, the only model is empty,
  yet a row circulating around the loop is a fixed point of each node on its
  own. Composition therefore has to be one global fixpoint, which is what
  source expansion gives. A per-node evaluation scheme would need extra
  justification.
- `resolve_equiv` and `stable_resolve_iff` (version 2): renaming every alias
  target to its ultimate source preserves every relation. `M` is a model of the
  version-1 program iff targets equal their roots in `M` and `M` is a model of
  the aliased program on every non-target.
- `composition_v1_v2`: the version-2 statement instantiated for a composition
  level. Its targets are the node input ports, and `input_not_target` and
  `output_not_target` show the public relations are never targets. This
  discharges the documented claim "public relation names and contents are
  identical under every version" for one level of bindings.

## Correspondence of `WellFormed` to Rust checks

| `WellFormed` field | Rust (`src/composition.rs`) |
| --- | --- |
| `aliases_nodup` | `nodes: BTreeMap` keys |
| `input_targets` | `endpoint(&nodes, target, true, scope)` |
| `binding_ends` | `endpoint(.., false, ..)` / `endpoint(.., true, ..)` per binding |
| `output_sources` | `endpoint(&nodes, output, false, scope)` |
| `single_source` | "Multiple sources for input …" (`assigned` set) |
| `connected` | "Unconnected input …" |
| `LeafOk` (version 2 only) | `lower_clauses`: "Rule head … must be an output relation"; `validate_interface` |

## Not modelled

- Types: facts are lists of constants. Arity and type checks (`compatible`,
  `check`) are what make DDlog accept the generated text. They do not affect
  the relational meaning.
- Stratification checking (`validate_dependencies`) and DDlog's evaluation
  itself. The runtime relies on DDlog computing the stratified model.
- The `Evidence<n>` explanation relations of version 1. Each rule `h :- body`
  becomes `Evidence(vars) :- body` and `h :- Evidence(vars)`, which projects
  back to the same relation `h`. This is not proved here.
- The alias proof covers one composition level. Rust resolves nested levels in
  the same pass, and the argument repeats level by level through `compose_node`.
- String-level injectivity of the generated names.
