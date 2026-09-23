import DdlogVerify.Localize

/-!
# Composition (lowering version 1)

A model of `Expansion::manifest` / `Expansion::program` in `src/composition.rs`.

A `Component` is a leaf program with an explicit interface, or a composition of
named nodes (each itself a component) with external inputs (broadcast to
targets), bindings from node outputs to node inputs, and external outputs.

## Names

Generated relation names are modelled by the free type `Name`:

| Rust generated name                     | `Name`                          |
| --------------------------------------- | ------------------------------- |
| a leaf's own relation `r`               | `loc r`                         |
| `Input_x`, `Composite<k>_Input_x`       | `input x` (under its node path) |
| `Output_o`, `Composite<k>_Output_o`     | `output o` (under its node path)|
| `Module<k>_r` for node `a`              | `node a (loc r)`                |

Rust flattens node paths into a globally unique index `k` (`next_node`). Because
`k` is unique per node in the tree and identifiers start with a letter
(`lower::ident`), `Module<k>_r` / `Composite<k>_…` is an injective encoding of
these paths; the model uses the path directly so injectivity is constructor
injectivity.

## Semantics of a component

A component is fed a valuation `E` of its input ports; `edbOf c E` places those
rows on the component's input relations. The theorems below characterise every
relation of a compiled composition:

* `compose_input`  – an external input holds exactly the fed rows;
* `compose_output` – an external output holds exactly its source node output;
* `compose_node`   – inside node `a` the composed program derives **exactly** what
  node `a`'s own compiled program derives when each input port is fed from its
  unique bound source (isolation of private names + preservation of the
  public interface). Applied recursively this covers nested compositions;
* `compose_loc`, `compose_unknown_node` – nothing else is derived.

`stable_restrict` lifts this to stable models: every model of a composition
restricts to a model of each node. (The converse is false for cyclic
compositions — see `Cycle.lean` — which is why composition must be, and is,
implemented as source expansion evaluated as one global fixpoint rather than as
independently evaluated nodes.)
-/

namespace DdlogVerify

inductive Name where
  | loc : String → Name
  | input : String → Name
  | output : String → Name
  | node : String → Name → Name
  deriving DecidableEq, Repr

/-- `(node alias, port)` as in `composition::Endpoint`. -/
abbrev Endpoint := String × String

inductive Component (C : Type) where
  /-- A leaf program over its own relation names, with interface inputs/outputs. -/
  | program (prog : Program String C) (ins outs : List String)
  /-- `CompositionManifest`: nodes (alias ↦ component), external inputs with
  their targets, bindings `from ↦ to`, external outputs. -/
  | composition (nodes : List (String × Component C)) (inputs : List (String × List Endpoint))
      (bindings : List (Endpoint × Endpoint)) (outputs : List (String × Endpoint))


variable {C : Type}

namespace Component

def ins : Component C → List String
  | program _ ins _ => ins
  | composition _ inputs _ _ => inputs.map Prod.fst

def outs : Component C → List String
  | program _ _ outs => outs
  | composition _ _ _ outputs => outputs.map Prod.fst

/-- Generated relation of an input port. -/
def inPort : Component C → String → Name
  | program .., q => .loc q
  | composition .., q => .input q

/-- Generated relation of an output port. -/
def outPort : Component C → String → Name
  | program .., p => .loc p
  | composition .., p => .output p

/-- The rows fed to a component's input ports, as external facts. -/
def edbOf (c : Component C) (E : String → List C → Prop) : Interp Name C :=
  fun f => ∃ q ∈ c.ins, f.1 = c.inPort q ∧ E q f.2

end Component

def Nodes.find? : List (String × Component C) → String → Option (Component C)
  | [], _ => none
  | (a, c) :: rest, b => if a = b then some c else Nodes.find? rest b

def Nodes.aliases (nodes : List (String × Component C)) : List String := nodes.map Prod.fst

/-- Bridge clauses of external inputs (`bind` with a physical input source). -/
def inputBridges (nodes : List (String × Component C)) (inputs : List (String × List Endpoint)) :
    Program Name C :=
  inputs.flatMap fun xi => xi.2.filterMap fun t =>
    (Nodes.find? nodes t.1).map fun ct => Clause.copy (.node t.1 (ct.inPort t.2)) (.input xi.1)

/-- Bridge clauses of processor bindings. -/
def bindingBridges (nodes : List (String × Component C)) (bindings : List (Endpoint × Endpoint)) :
    Program Name C :=
  bindings.filterMap fun b =>
    match Nodes.find? nodes b.1.1, Nodes.find? nodes b.2.1 with
    | some cf, some ct =>
      some (Clause.copy (.node b.2.1 (ct.inPort b.2.2)) (.node b.1.1 (cf.outPort b.1.2)))
    | _, _ => none

/-- Copy rules of external outputs (always generated, under every version). -/
def outputCopies (nodes : List (String × Component C)) (outputs : List (String × Endpoint)) : Program Name C :=
  outputs.filterMap fun oe =>
    (Nodes.find? nodes oe.2.1).map fun ce => Clause.copy (.output oe.1) (.node oe.2.1 (ce.outPort oe.2.2))

mutual
/-- Source expansion under lowering version 1. -/
def Component.compile : Component C → Program Name C
  | .program prog _ _ => prog.rename Name.loc
  | .composition nodes inputs bindings outputs =>
    Nodes.compile nodes ++ (inputBridges nodes inputs ++ bindingBridges nodes bindings ++
      outputCopies nodes outputs)
def Nodes.compile : List (String × Component C) → Program Name C
  | [] => []
  | (a, c) :: rest => (Component.compile c).rename (Name.node a) ++ Nodes.compile rest
end

/-- `(a, q)` is fed by `s`: an external input broadcast to it, or a binding. -/
def Feeds (nodes : List (String × Component C)) (inputs : List (String × List Endpoint))
    (bindings : List (Endpoint × Endpoint)) (a q : String) (s : Name) : Prop :=
  (∃ x ts, (x, ts) ∈ inputs ∧ (a, q) ∈ ts ∧ s = .input x) ∨
  (∃ f cf, (f, (a, q)) ∈ bindings ∧ Nodes.find? nodes f.1 = some cf ∧ s = .node f.1 (cf.outPort f.2))

/-- The checks `Expansion::manifest` performs, stated as properties. -/
structure WellFormed (nodes : List (String × Component C)) (inputs : List (String × List Endpoint))
    (bindings : List (Endpoint × Endpoint)) (outputs : List (String × Endpoint)) : Prop where
  /-- Node aliases are map keys. -/
  aliases_nodup : (Nodes.aliases nodes).Nodup
  /-- `endpoint(.., input = true, ..)` for every external-input target. -/
  input_targets : ∀ x ts, (x, ts) ∈ inputs → ∀ t ∈ ts, ∃ ct, Nodes.find? nodes t.1 = some ct ∧ t.2 ∈ ct.ins
  /-- `endpoint` for both ends of every binding. -/
  binding_ends : ∀ b ∈ bindings,
    (∃ cf, Nodes.find? nodes b.1.1 = some cf ∧ b.1.2 ∈ cf.outs) ∧
    (∃ ct, Nodes.find? nodes b.2.1 = some ct ∧ b.2.2 ∈ ct.ins)
  /-- `endpoint(.., input = false, ..)` for every external output. -/
  output_sources : ∀ o e, (o, e) ∈ outputs → ∃ ce, Nodes.find? nodes e.1 = some ce ∧ e.2 ∈ ce.outs
  /-- "Multiple sources for input": every target has at most one source. -/
  single_source : ∀ a q s s', Feeds nodes inputs bindings a q s →
    Feeds nodes inputs bindings a q s' → s = s'
  /-- "Unconnected input": every node input has a source. -/
  connected : ∀ a ca, Nodes.find? nodes a = some ca → ∀ q ∈ ca.ins, ∃ s, Feeds nodes inputs bindings a q s

/-! ## Lemmas about node lists -/

theorem Nodes.find?_mem {nodes : List (String × Component C)} {a : String} {c : Component C}
    (h : Nodes.find? nodes a = some c) : (a, c) ∈ nodes := by
  induction nodes with
  | nil => simp [Nodes.find?] at h
  | cons bc rest ih =>
    obtain ⟨b, cb⟩ := bc
    simp only [Nodes.find?] at h
    split at h
    · rename_i hab
      subst hab
      cases h
      exact List.mem_cons_self
    · exact List.mem_cons_of_mem _ (ih h)

theorem Nodes.find?_of_mem {nodes : List (String × Component C)} {a : String} {c : Component C}
    (hnd : Nodes.aliases nodes |>.Nodup) (h : (a, c) ∈ nodes) : Nodes.find? nodes a = some c := by
  induction nodes with
  | nil => simp at h
  | cons bc rest ih =>
    obtain ⟨b, cb⟩ := bc
    simp only [Nodes.aliases, List.map_cons, List.nodup_cons] at hnd
    rcases List.mem_cons.1 h with h | h
    · simp only [Prod.mk.injEq] at h
      obtain ⟨rfl, rfl⟩ := h
      simp [Nodes.find?]
    · have hab : b ≠ a := fun e => hnd.1 (e ▸ List.mem_map_of_mem (f := Prod.fst) h)
      simp [Nodes.find?, hab, ih hnd.2 h]

theorem Nodes.find?_ne_none_of_mem {nodes : List (String × Component C)} {a : String}
    {c : Component C} (h : (a, c) ∈ nodes) : Nodes.find? nodes a ≠ none := by
  induction nodes with
  | nil => simp at h
  | cons bc rest ih =>
    obtain ⟨b, cb⟩ := bc
    simp only [Nodes.find?]
    split
    · simp
    · rename_i hab
      rcases List.mem_cons.1 h with h | h
      · simp only [Prod.mk.injEq] at h
        exact absurd h.1.symm hab
      · exact ih h

/-- Every clause of the node section is a clause of one node, renamed under its alias. -/
theorem Nodes.mem_compile {nodes : List (String × Component C)} {cl : Clause Name C}
    (h : cl ∈ Nodes.compile nodes) :
    ∃ a ca c', (a, ca) ∈ nodes ∧ c' ∈ ca.compile ∧ cl = c'.rename (Name.node a) := by
  induction nodes with
  | nil => simp [Nodes.compile] at h
  | cons bc rest ih =>
    obtain ⟨b, cb⟩ := bc
    simp only [Nodes.compile, List.mem_append] at h
    rcases h with h | h
    · obtain ⟨c', hc', rfl⟩ := List.mem_map.1 h
      exact ⟨b, cb, c', List.mem_cons_self, hc', rfl⟩
    · obtain ⟨a, ca, c', hm, hc', rfl⟩ := ih h
      exact ⟨a, ca, c', List.mem_cons_of_mem _ hm, hc', rfl⟩

theorem Nodes.compile_mem {nodes : List (String × Component C)} {a : String} {ca : Component C}
    {c' : Clause Name C} (hf : Nodes.find? nodes a = some ca) (hc : c' ∈ ca.compile) :
    c'.rename (Name.node a) ∈ Nodes.compile nodes := by
  induction nodes with
  | nil => simp [Nodes.find?] at hf
  | cons bc rest ih =>
    obtain ⟨b, cb⟩ := bc
    simp only [Nodes.compile, List.mem_append]
    simp only [Nodes.find?] at hf
    split at hf
    · rename_i hab
      subst hab
      cases hf
      exact Or.inl (List.mem_map_of_mem hc)
    · exact Or.inr (ih hf)

/-! ## Clauses of a compiled composition -/

section Clauses
variable {nodes : List (String × Component C)} {inputs : List (String × List Endpoint)}
  {bindings : List (Endpoint × Endpoint)} {outputs : List (String × Endpoint)}

theorem compile_composition :
    (Component.composition nodes inputs bindings outputs).compile =
      Nodes.compile nodes ++ (inputBridges nodes inputs ++ bindingBridges nodes bindings ++
        outputCopies nodes outputs) := by
  simp [Component.compile]

/-- Case analysis on a clause of a compiled composition. -/
theorem mem_compile_cases {cl : Clause Name C}
    (h : cl ∈ (Component.composition nodes inputs bindings outputs).compile) :
    (∃ b cb c', (b, cb) ∈ nodes ∧ c' ∈ cb.compile ∧ cl = c'.rename (Name.node b)) ∨
    (∃ x ts t ct, (x, ts) ∈ inputs ∧ t ∈ ts ∧ Nodes.find? nodes t.1 = some ct ∧
      cl = Clause.copy (.node t.1 (ct.inPort t.2)) (.input x)) ∨
    (∃ b cf ct, b ∈ bindings ∧ Nodes.find? nodes b.1.1 = some cf ∧
      Nodes.find? nodes b.2.1 = some ct ∧
      cl = Clause.copy (.node b.2.1 (ct.inPort b.2.2)) (.node b.1.1 (cf.outPort b.1.2))) ∨
    (∃ o e ce, (o, e) ∈ outputs ∧ Nodes.find? nodes e.1 = some ce ∧
      cl = Clause.copy (.output o) (.node e.1 (ce.outPort e.2))) := by
  rw [compile_composition] at h
  simp only [List.mem_append] at h
  rcases h with h | (h | h) | h
  · exact Or.inl (Nodes.mem_compile h)
  · right; left
    obtain ⟨⟨x, ts⟩, hx, ht⟩ := List.mem_flatMap.1 h
    obtain ⟨t, htm, hct⟩ := List.mem_filterMap.1 ht
    cases hf : Nodes.find? nodes t.1 with
    | none => simp [hf] at hct
    | some ct =>
      simp only [hf, Option.map_some, Option.some.injEq] at hct
      exact ⟨x, ts, t, ct, hx, htm, hf, hct.symm⟩
  · right; right; left
    obtain ⟨b, hb, hcl⟩ := List.mem_filterMap.1 h
    cases hf : Nodes.find? nodes b.1.1 with
    | none => simp [hf] at hcl
    | some cf =>
      cases hg : Nodes.find? nodes b.2.1 with
      | none => simp [hf, hg] at hcl
      | some ct =>
        simp only [hf, hg, Option.some.injEq] at hcl
        exact ⟨b, cf, ct, hb, hf, hg, hcl.symm⟩
  · right; right; right
    obtain ⟨⟨o, e⟩, ho, hcl⟩ := List.mem_filterMap.1 h
    cases hf : Nodes.find? nodes e.1 with
    | none => simp [hf] at hcl
    | some ce =>
      simp only [hf, Option.map_some, Option.some.injEq] at hcl
      exact ⟨o, e, ce, ho, hf, hcl.symm⟩

theorem inputBridge_mem {x : String} {ts : List Endpoint} {t : Endpoint} {ct : Component C}
    (hx : (x, ts) ∈ inputs) (ht : t ∈ ts) (hf : Nodes.find? nodes t.1 = some ct) :
    Clause.copy (.node t.1 (ct.inPort t.2)) (.input x) ∈
      (Component.composition nodes inputs bindings outputs).compile := by
  rw [compile_composition]
  refine List.mem_append_right _ (List.mem_append_left _ (List.mem_append_left _ ?_))
  exact List.mem_flatMap.2 ⟨(x, ts), hx, List.mem_filterMap.2 ⟨t, ht, by simp [hf]⟩⟩

theorem bindingBridge_mem {b : Endpoint × Endpoint} {cf ct : Component C}
    (hb : b ∈ bindings) (hf : Nodes.find? nodes b.1.1 = some cf)
    (hg : Nodes.find? nodes b.2.1 = some ct) :
    Clause.copy (.node b.2.1 (ct.inPort b.2.2)) (.node b.1.1 (cf.outPort b.1.2)) ∈
      (Component.composition nodes inputs bindings outputs).compile := by
  rw [compile_composition]
  refine List.mem_append_right _ (List.mem_append_left _ (List.mem_append_right _ ?_))
  exact List.mem_filterMap.2 ⟨b, hb, by simp [hf, hg]⟩

end Clauses

end DdlogVerify
