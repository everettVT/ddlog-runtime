import DdlogVerify.Compose
import DdlogVerify.Alias

/-!
# Lowering version 1 and version 2 agree on compositions

`resolve_equiv` / `stable_resolve_iff` are stated for an abstract set of alias
targets. Here they are instantiated for the bindings of a well-formed
composition: the targets are the node input ports, each renamed to the source
it is bound to (an external input or another node's output port).

This covers the aliasing of one composition level. Rust resolves every level
at once; an inner level's targets are renamed inside the child's generated
program, which is related to the child's own semantics by `compose_node`, so the
argument repeats level by level.

Leaf conditions (`LeafOk`), both enforced by Rust before composition:

* a leaf never derives one of its interface inputs (`lower_clauses`: "Rule head …
  must be an output relation"; interface inputs must be declared inputs,
  `validate_interface`);
* no relation is both an interface input and an interface output
  (`validate_interface`: outputs must be declared derived relations).
-/

namespace DdlogVerify

open Classical

variable {C : Type}

def Component.LeafOk : Component C → Prop
  | .program prog ins outs => (∀ c ∈ prog, c.head ∉ ins) ∧ ∀ p ∈ outs, p ∉ ins
  | .composition .. => True

/-- A compiled composition never derives one of its own input relations. -/
theorem composition_no_input_head {nodes : List (String × Component C)}
    {inputs : List (String × List Endpoint)} {bindings : List (Endpoint × Endpoint)}
    {outputs : List (String × Endpoint)} {cl : Clause Name C}
    (h : cl ∈ (Component.composition nodes inputs bindings outputs).compile) (x : String) :
    cl.head ≠ .input x := by
  rcases mem_compile_cases h with
    ⟨_, _, _, _, _, rfl⟩ | ⟨_, _, _, _, _, _, _, rfl⟩ | ⟨_, _, _, _, _, _, rfl⟩ |
    ⟨_, _, _, _, _, rfl⟩ <;> first | (simp [Clause.head_rename]; done) | simp [Clause.head]

/-- No clause of a leaf-correct component derives one of its input ports. -/
theorem no_inPort_head {ca : Component C} (hl : ca.LeafOk) {c' : Clause Name C}
    (hc : c' ∈ ca.compile) {q : String} (hq : q ∈ ca.ins) : c'.head ≠ ca.inPort q := by
  cases ca with
  | program prog ins outs =>
    obtain ⟨c0, hc0, rfl⟩ := List.mem_map.1 hc
    simp only [Clause.head_rename, Component.inPort]
    intro h
    have h' : c0.head = q := Name.loc.inj h
    exact hl.1 c0 hc0 (h' ▸ hq)
  | composition => exact composition_no_input_head hc q

section
variable {nodes : List (String × Component C)} {inputs : List (String × List Endpoint)}
  {bindings : List (Endpoint × Endpoint)} {outputs : List (String × Endpoint)}

/-- The aliased targets: every node input port. -/
def Target (nodes : List (String × Component C)) (n : Name) : Prop :=
  ∃ a ca q, Nodes.find? nodes a = some ca ∧ q ∈ ca.ins ∧ n = .node a (ca.inPort q)

/-- The immediate source of a target. -/
noncomputable def bindSrc (nodes : List (String × Component C))
    (inputs : List (String × List Endpoint)) (bindings : List (Endpoint × Endpoint)) : Name → Name
  | .node a m => sourceOf nodes inputs bindings a (portOf m)
  | n => n

/-- `Expansion::resolve_aliases` at one level: targets go to their source. -/
noncomputable def resolveMap (nodes : List (String × Component C))
    (inputs : List (String × List Endpoint)) (bindings : List (Endpoint × Endpoint)) (n : Name) :
    Name :=
  if Target nodes n then bindSrc nodes inputs bindings n else n

theorem source_not_target (W : WellFormed nodes inputs bindings outputs)
    (hleaf : ∀ a ca, Nodes.find? nodes a = some ca → ca.LeafOk)
    {a q : String} {s : Name} (hs : Feeds nodes inputs bindings a q s) : ¬ Target nodes s := by
  rintro ⟨b, cb, q', hb, hq', he⟩
  rcases hs with ⟨x, _, _, _, rfl⟩ | ⟨f, cf, hbd, hcf, rfl⟩
  · cases he
  · simp only [Name.node.injEq] at he
    obtain ⟨rfl, he⟩ := he
    rw [hcf] at hb
    cases hb
    obtain ⟨⟨cf', hcf', hout⟩, _⟩ := W.binding_ends _ hbd
    simp only at hcf' hout
    rw [hcf] at hcf'
    cases hcf'
    cases cb with
    | program prog ins outs =>
      simp only [Component.outPort, Component.inPort, Name.loc.injEq] at he
      exact (hleaf _ _ hcf).2 _ hout (he ▸ hq')
    | composition => simp [Component.outPort, Component.inPort] at he

theorem composition_resolveOk (W : WellFormed nodes inputs bindings outputs)
    (hleaf : ∀ a ca, Nodes.find? nodes a = some ca → ca.LeafOk) (E : String → List C → Prop) :
    ResolveOk (Component.composition nodes inputs bindings outputs).compile
      ((Component.composition nodes inputs bindings outputs).edbOf E)
      (Target nodes) (bindSrc nodes inputs bindings) (resolveMap nodes inputs bindings)
      (fun n => if Target nodes n then 1 else 0) where
  bridge := by
    rintro _ ⟨a, ca, q, ha, hq, rfl⟩
    exact (node_embedding W (E := E) ha).bridge_mem _ ⟨q, hq, rfl⟩
  only := by
    intro cl hcl ⟨a, ca, q, ha, hq, he⟩
    rcases (node_embedding W (E := E) ha).dom cl hcl _ he with ⟨c', hc', rfl⟩ | ⟨_, hce⟩
    · simp only [Clause.head_rename, Name.node.injEq, true_and] at he
      exact absurd he (no_inPort_head (hleaf a ca ha) hc' hq)
    · rw [hce]
      simp [Clause.head, bindSrc]
  edb_out := by
    rintro _ xs ⟨a, ca, q, _, _, rfl⟩ ⟨q', _, he, _⟩
    simp [Component.inPort] at he
  root_target := by
    rintro _ ⟨a, ca, q, ha, hq, rfl⟩
    have hT : Target nodes (.node a (ca.inPort q)) := ⟨a, ca, q, ha, hq, rfl⟩
    obtain ⟨s, hs⟩ := W.connected a ca ha q hq
    have hsrc : bindSrc nodes inputs bindings (.node a (ca.inPort q)) = s := by
      simp [bindSrc, sourceOf_eq W hs]
    have hns := source_not_target W hleaf hs
    simp only [resolveMap, hT, ite_true, hsrc, hns, ite_false]
  root_other := by
    intro q hq
    simp [resolveMap, hq]
  depth_lt := by
    rintro _ ⟨a, ca, q, ha, hq, rfl⟩
    have hT : Target nodes (.node a (ca.inPort q)) := ⟨a, ca, q, ha, hq, rfl⟩
    obtain ⟨s, hs⟩ := W.connected a ca ha q hq
    have hsrc : bindSrc nodes inputs bindings (.node a (ca.inPort q)) = s := by
      simp [bindSrc, sourceOf_eq W hs]
    simp [hT, hsrc, source_not_target W hleaf hs]

/-- **Version 1 ≡ version 2 for a composition level.** A stable model of the
version-1 composition is exactly a model of the aliased (version-2) program on
every relation that is not a node input port, with each input port equal to
its source. External inputs, external outputs and node outputs are never
targets, so the public relations are identical under both lowerings. -/
theorem composition_v1_v2 (W : WellFormed nodes inputs bindings outputs)
    (hleaf : ∀ a ca, Nodes.find? nodes a = some ca → ca.LeafOk) (E : String → List C → Prop)
    (M : Interp Name C) :
    let c := Component.composition nodes inputs bindings outputs
    Stable c.compile (c.edbOf E) M ↔
      (∀ n xs, M (resolveMap nodes inputs bindings n, xs) ↔ M (n, xs)) ∧
      ∀ n, ¬ Target nodes n → ∀ xs,
        M (n, xs) ↔ Derives (c.compile.rename (resolveMap nodes inputs bindings)) (c.edbOf E) M (n, xs) :=
  stable_resolve_iff (composition_resolveOk W hleaf E) M

/-- External outputs are never alias targets. -/
theorem output_not_target (o : String) : ¬ Target nodes (C := C) (.output o) := by
  rintro ⟨_, _, _, _, _, h⟩; cases h

/-- External inputs are never alias targets. -/
theorem input_not_target (x : String) : ¬ Target nodes (C := C) (.input x) := by
  rintro ⟨_, _, _, _, _, h⟩; cases h

end

end DdlogVerify
