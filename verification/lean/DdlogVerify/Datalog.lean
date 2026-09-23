/-!
# Core relational language

A model of the rule language that `src/lower.rs` accepts, abstracted over the
type of relation names `P` and of constants `C`.

* A `Rule` has a head atom, positive and negated body atoms, and a `guard`
  that stands for the conjunction of its comparisons. Comparisons only read the
  substitution, so renaming relations never touches them.
* `Clause.copy dst src` is the generated binding rule `dst(X0..Xn) :- src(X0..Xn)`
  produced by `bridge` in `src/composition.rs`. Schemas are typed, so every row
  of `src` has the arity of `dst`; the model copies whole rows.
* `Clause.op dst ins F` is a native operator (the Large-Star/Small-Star
  connected-components transformer). Its output is an arbitrary function `F`
  of the complete contents of its input relations. Like negation it reads the
  interpretation `I` rather than the derivation in progress: cycles through
  operators are rejected by `validate_dependencies`, so its inputs are always
  complete lower-stratum relations.

## Semantics

`Derives prog edb I` is the least set of facts closed under the clauses, with
negated atoms and operator inputs evaluated against the fixed interpretation
`I` (the Gelfond–Lifschitz reduct). `Stable prog edb M` says `M` is a fixed
point of that construction, i.e. a stable model. The lowerer admits only
stratified programs (no negative or native edge inside a dependency cycle),
and for stratified programs the unique stable model is the stratified
(perfect) model that DDlog computes. Every theorem in this development holds
for *every* stable model, so no stratification argument is needed here.
-/

namespace DdlogVerify

inductive Term (C : Type) where
  | var : Nat → Term C
  | const : C → Term C

abbrev Subst (C : Type) := Nat → C

def Term.eval {C : Type} (σ : Subst C) : Term C → C
  | .var n => σ n
  | .const c => c

structure Atom (P C : Type) where
  pred : P
  args : List (Term C)

abbrev Fact (P C : Type) := P × List C

def Atom.ground {P C : Type} (σ : Subst C) (a : Atom P C) : Fact P C :=
  (a.pred, a.args.map (Term.eval σ))

structure Rule (P C : Type) where
  head : Atom P C
  pos : List (Atom P C)
  neg : List (Atom P C)
  guard : Subst C → Prop

inductive Clause (P C : Type) where
  | rule : Rule P C → Clause P C
  | copy : P → P → Clause P C
  | op : P → List P → (List (List C → Prop) → List C → Prop) → Clause P C

/-- The relation a clause derives. -/
def Clause.head {P C : Type} : Clause P C → P
  | .rule r => r.head.pred
  | .copy dst _ => dst
  | .op dst _ _ => dst

abbrev Program (P C : Type) := List (Clause P C)
abbrev Interp (P C : Type) := Fact P C → Prop

/-- Least model of `prog` over `edb`, reading negation and operator inputs from `I`. -/
inductive Derives {P C : Type} (prog : Program P C) (edb : Interp P C) (I : Interp P C) :
    Fact P C → Prop where
  | edb {f} : edb f → Derives prog edb I f
  | rule {r : Rule P C} {σ : Subst C} :
      Clause.rule r ∈ prog → r.guard σ →
      (∀ a ∈ r.pos, Derives prog edb I (a.ground σ)) →
      (∀ a ∈ r.neg, ¬ I (a.ground σ)) →
      Derives prog edb I (r.head.ground σ)
  | copy {dst src : P} {xs : List C} :
      Clause.copy dst src ∈ prog → Derives prog edb I (src, xs) →
      Derives prog edb I (dst, xs)
  | op {dst : P} {ins : List P} {F} {xs : List C} :
      Clause.op dst ins F ∈ prog → F (ins.map fun p ys => I (p, ys)) xs →
      Derives prog edb I (dst, xs)

/-- `M` is a stable model of `prog` over `edb`. -/
def Stable {P C : Type} (prog : Program P C) (edb : Interp P C) (M : Interp P C) : Prop :=
  ∀ f, M f ↔ Derives prog edb M f

/-! ## Renaming relations -/

section Rename
variable {P Q C : Type}

def Atom.rename (ρ : P → Q) (a : Atom P C) : Atom Q C := ⟨ρ a.pred, a.args⟩

def Rule.rename (ρ : P → Q) (r : Rule P C) : Rule Q C :=
  ⟨r.head.rename ρ, r.pos.map (Atom.rename ρ), r.neg.map (Atom.rename ρ), r.guard⟩

def Clause.rename (ρ : P → Q) : Clause P C → Clause Q C
  | .rule r => .rule (r.rename ρ)
  | .copy d s => .copy (ρ d) (ρ s)
  | .op d ins F => .op (ρ d) (ins.map ρ) F

def Program.rename (ρ : P → Q) (prog : Program P C) : Program Q C :=
  prog.map (Clause.rename ρ)

@[simp] theorem Atom.ground_rename (ρ : P → Q) (σ : Subst C) (a : Atom P C) :
    (a.rename ρ).ground σ = (ρ (a.ground σ).1, (a.ground σ).2) := rfl

@[simp] theorem Clause.head_rename (ρ : P → Q) (c : Clause P C) :
    (c.rename ρ).head = ρ c.head := by
  cases c <;> rfl

theorem Program.mem_rename {ρ : P → Q} {prog : Program P C} {c : Clause P C}
    (h : c ∈ prog) : c.rename ρ ∈ prog.rename ρ :=
  List.mem_map_of_mem h

end Rename

end DdlogVerify
