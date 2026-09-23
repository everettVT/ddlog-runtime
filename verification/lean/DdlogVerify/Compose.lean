import DdlogVerify.Composition

/-!
# Compositionality theorems

For a well-formed composition `c` (every check of `Expansion::manifest`
passed), the relations of its generated program are characterised exactly:
external inputs, external outputs, and — the central statement — every node
behaves as its own compiled program fed from its unique bound sources.
-/

namespace DdlogVerify

open Classical

variable {C : Type}

/-- The port name a generated port relation stands for. -/
def portOf : Name → String
  | .loc q => q
  | .input q => q
  | .output q => q
  | .node _ _ => ""

@[simp] theorem portOf_inPort (c : Component C) (q : String) : portOf (c.inPort q) = q := by
  cases c <;> rfl

section
variable {nodes : List (String × Component C)} {inputs : List (String × List Endpoint)}
  {bindings : List (Endpoint × Endpoint)} {outputs : List (String × Endpoint)}

/-- The unique source bound to input port `q` of node `a`. -/
noncomputable def sourceOf (nodes : List (String × Component C))
    (inputs : List (String × List Endpoint)) (bindings : List (Endpoint × Endpoint))
    (a q : String) : Name :=
  if h : ∃ s, Feeds nodes inputs bindings a q s then Classical.choose h else .loc ""

theorem sourceOf_eq (W : WellFormed nodes inputs bindings outputs) {a q : String} {s : Name}
    (h : Feeds nodes inputs bindings a q s) : sourceOf nodes inputs bindings a q = s := by
  have hex : ∃ s, Feeds nodes inputs bindings a q s := ⟨s, h⟩
  simp only [sourceOf, hex, dite_true]
  exact W.single_source a q _ _ (Classical.choose_spec hex) h

theorem inPort_inj {c : Component C} {q q' : String} (h : c.inPort q = c.inPort q') : q = q' := by
  have := congrArg portOf h
  simpa using this

theorem node_embedding (W : WellFormed nodes inputs bindings outputs)
    {E : String → List C → Prop} {a : String} {ca : Component C}
    (ha : Nodes.find? nodes a = some ca) :
    Embedding (Component.composition nodes inputs bindings outputs).compile
      ((Component.composition nodes inputs bindings outputs).edbOf E)
      (Name.node a) ca.compile (fun n => ∃ q ∈ ca.ins, n = ca.inPort q)
      (fun n => sourceOf nodes inputs bindings a (portOf n)) where
  inj := fun _ _ h => (Name.node.inj h).2
  sub_mem := fun c' hc' => by
    rw [compile_composition]
    exact List.mem_append_left _ (Nodes.compile_mem ha hc')
  bridge_mem := by
    rintro n ⟨q, hq, rfl⟩
    obtain ⟨s, hs⟩ := W.connected a ca ha q hq
    simp only [portOf_inPort, sourceOf_eq W hs]
    rcases hs with ⟨x, ts, hx, hts, rfl⟩ | ⟨f, cf, hb, hcf, rfl⟩
    · exact inputBridge_mem (t := (a, q)) hx hts ha
    · exact bindingBridge_mem (b := (f, (a, q))) hb hcf ha
  dom := by
    intro cl hcl p hp
    rcases mem_compile_cases hcl with
      ⟨b, cb, c', hb, hc', rfl⟩ | ⟨x, ts, t, ct, hx, ht, hf, rfl⟩ |
      ⟨bd, cf, ct, hbd, hf, hg, rfl⟩ | ⟨o, e, ce, ho, hf, rfl⟩
    · left
      simp only [Clause.head_rename] at hp
      have hba : b = a := (Name.node.inj hp).1
      subst hba
      have : cb = ca := by
        have := Nodes.find?_of_mem W.aliases_nodup hb
        rw [ha] at this
        exact (Option.some.inj this).symm
      subst this
      exact ⟨c', hc', rfl⟩
    · right
      simp only [Clause.head] at hp
      obtain ⟨h1, h2⟩ := Name.node.inj hp
      obtain ⟨ta, tq⟩ := t
      simp only at h1 h2 hf
      subst h1 h2
      rw [ha] at hf
      cases hf
      obtain ⟨ct', hct', hq⟩ := W.input_targets x ts hx _ ht
      rw [ha] at hct'
      cases hct'
      refine ⟨⟨tq, hq, rfl⟩, ?_⟩
      simp only [portOf_inPort]
      rw [sourceOf_eq W (Or.inl ⟨x, ts, hx, ht, rfl⟩)]
    · right
      simp only [Clause.head] at hp
      obtain ⟨h1, h2⟩ := Name.node.inj hp
      obtain ⟨fr, ta, tq⟩ := bd
      simp only at h1 h2 hg hf
      subst h1 h2
      rw [ha] at hg
      cases hg
      obtain ⟨_, ct', hct', hq⟩ := W.binding_ends _ hbd
      simp only at hct' hq
      rw [ha] at hct'
      cases hct'
      refine ⟨⟨tq, hq, rfl⟩, ?_⟩
      simp only [portOf_inPort]
      rw [sourceOf_eq W (Or.inr ⟨fr, cf, hbd, hf, rfl⟩)]
    · simp [Clause.head] at hp
  edb_out := by
    rintro p xs ⟨q, _, hq, _⟩
    simp [Component.inPort] at hq

/-- **Node isolation and interface preservation.** Inside node `a`, the composed
program derives exactly what `a`'s own compiled program derives when each of its
input ports `q` is fed with the composed contents of its unique source. -/
theorem compose_node (W : WellFormed nodes inputs bindings outputs)
    (E : String → List C → Prop) (I : Interp Name C)
    {a : String} {ca : Component C} (ha : Nodes.find? nodes a = some ca) (n : Name) (xs : List C) :
    let c := Component.composition nodes inputs bindings outputs
    Derives c.compile (c.edbOf E) I (.node a n, xs) ↔
      Derives ca.compile
        (ca.edbOf fun q ys => Derives c.compile (c.edbOf E) I (sourceOf nodes inputs bindings a q, ys))
        (pullback (Name.node a) I) (n, xs) := by
  intro c
  rw [localize (node_embedding W (E := E) ha) n xs]
  have : fedInputs (fun n => ∃ q ∈ ca.ins, n = ca.inPort q) (Derives c.compile (c.edbOf E) I)
      (fun n => sourceOf nodes inputs bindings a (portOf n)) =
      ca.edbOf (fun q ys => Derives c.compile (c.edbOf E) I (sourceOf nodes inputs bindings a q, ys)) := by
    funext f
    apply propext
    constructor
    · rintro ⟨⟨q, hq, he⟩, hd⟩
      exact ⟨q, hq, he, by simpa [he] using hd⟩
    · rintro ⟨q, hq, he, hd⟩
      exact ⟨⟨q, hq, he⟩, by simpa [he] using hd⟩
  rw [this]

/-- An external input holds exactly the rows fed to it. -/
theorem compose_input (E : String → List C → Prop) (I : Interp Name C) (x : String) (xs : List C) :
    let c := Component.composition nodes inputs bindings outputs
    Derives c.compile (c.edbOf E) I (.input x, xs) ↔ x ∈ inputs.map Prod.fst ∧ E x xs := by
  intro c
  rw [derives_underived]
  · constructor
    · rintro ⟨q, hq, he, hE⟩
      simp only [c, Component.inPort, Name.input.injEq] at he
      subst he
      exact ⟨hq, hE⟩
    · rintro ⟨hq, hE⟩
      exact ⟨x, hq, rfl, hE⟩
  · intro cl hcl
    rcases mem_compile_cases hcl with
      ⟨b, cb, c', _, _, rfl⟩ | ⟨_, _, _, _, _, _, _, rfl⟩ | ⟨_, _, _, _, _, _, rfl⟩ |
      ⟨_, _, _, _, _, rfl⟩ <;> first | (simp [Clause.head_rename]; done) | simp [Clause.head]

/-- An external output holds exactly the rows of the node output it names. -/
theorem compose_output (E : String → List C → Prop) (I : Interp Name C) (o : String) (xs : List C) :
    let c := Component.composition nodes inputs bindings outputs
    Derives c.compile (c.edbOf E) I (.output o, xs) ↔
      ∃ e ce, (o, e) ∈ outputs ∧ Nodes.find? nodes e.1 = some ce ∧
        Derives c.compile (c.edbOf E) I (.node e.1 (ce.outPort e.2), xs) := by
  intro c
  rw [derives_copies]
  · constructor
    · rintro ⟨s, hs, hd⟩
      rcases mem_compile_cases hs with
        ⟨b, cb, c', _, _, he⟩ | ⟨_, _, _, _, _, _, _, he⟩ | ⟨_, _, _, _, _, _, he⟩ |
        ⟨o', e, ce, ho, hf, he⟩
      · cases c' <;> simp [Clause.rename] at he
      · simp at he
      · simp at he
      · simp only [Clause.copy.injEq, Name.output.injEq] at he
        obtain ⟨rfl, rfl⟩ := he
        exact ⟨e, ce, ho, hf, hd⟩
    · rintro ⟨e, ce, ho, hf, hd⟩
      refine ⟨_, ?_, hd⟩
      rw [compile_composition]
      refine List.mem_append_right _ (List.mem_append_right _ ?_)
      exact List.mem_filterMap.2 ⟨(o, e), ho, by simp [hf]⟩
  · rintro ys ⟨q, _, hq, _⟩
    simp [Component.inPort] at hq
  · intro cl hcl hh
    rcases mem_compile_cases hcl with
      ⟨b, cb, c', _, _, rfl⟩ | ⟨_, _, _, _, _, _, _, rfl⟩ | ⟨_, _, _, _, _, _, rfl⟩ |
      ⟨o', e, ce, _, _, rfl⟩
    · simp [Clause.head_rename] at hh
    · simp [Clause.head] at hh
    · simp [Clause.head] at hh
    · simp only [Clause.head, Name.output.injEq] at hh
      subst hh
      exact ⟨_, rfl⟩

/-- A composition derives nothing under a bare leaf name. -/
theorem compose_loc (E : String → List C → Prop) (I : Interp Name C) (s : String) (xs : List C) :
    let c := Component.composition nodes inputs bindings outputs
    ¬ Derives c.compile (c.edbOf E) I (.loc s, xs) := by
  intro c h
  rw [derives_underived] at h
  · obtain ⟨q, _, hq, _⟩ := h
    simp [c, Component.inPort] at hq
  · intro cl hcl
    rcases mem_compile_cases hcl with
      ⟨b, cb, c', _, _, rfl⟩ | ⟨_, _, _, _, _, _, _, rfl⟩ | ⟨_, _, _, _, _, _, rfl⟩ |
      ⟨_, _, _, _, _, rfl⟩ <;> first | (simp [Clause.head_rename]; done) | simp [Clause.head]

/-- A composition derives nothing under an alias it does not declare. -/
theorem compose_unknown_node (E : String → List C → Prop) (I : Interp Name C)
    {a : String} (ha : Nodes.find? nodes a = none) (n : Name) (xs : List C) :
    let c := Component.composition nodes inputs bindings outputs
    ¬ Derives c.compile (c.edbOf E) I (.node a n, xs) := by
  intro c h
  rw [derives_underived] at h
  · obtain ⟨q, _, hq, _⟩ := h
    simp [c, Component.inPort] at hq
  · intro cl hcl hh
    rcases mem_compile_cases hcl with
      ⟨b, cb, c', hb, _, rfl⟩ | ⟨_, _, t, _, _, _, hf, rfl⟩ | ⟨bd, _, _, _, _, hg, rfl⟩ |
      ⟨_, _, _, _, _, rfl⟩
    · simp only [Clause.head_rename, Name.node.injEq] at hh
      obtain ⟨rfl, _⟩ := hh
      exact Nodes.find?_ne_none_of_mem hb ha
    · simp only [Clause.head, Name.node.injEq] at hh
      rw [hh.1, ha] at hf
      cases hf
    · simp only [Clause.head, Name.node.injEq] at hh
      rw [hh.1, ha] at hg
      cases hg
    · simp [Clause.head] at hh

/-- **Stable models restrict to stable models of every node.** In any stable model
of a well-formed composition, each node's relations form a stable model of that
node's own program, fed from the model's contents at its bound sources; external
inputs and outputs are as above. -/
theorem stable_restrict (W : WellFormed nodes inputs bindings outputs)
    (E : String → List C → Prop) (M : Interp Name C)
    (hM : Stable (Component.composition nodes inputs bindings outputs).compile
      ((Component.composition nodes inputs bindings outputs).edbOf E) M)
    {a : String} {ca : Component C} (ha : Nodes.find? nodes a = some ca) :
    Stable ca.compile (ca.edbOf fun q ys => M (sourceOf nodes inputs bindings a q, ys))
      (pullback (Name.node a) M) := by
  intro f
  have hfun : (fun q ys => M (sourceOf nodes inputs bindings a q, ys)) =
      (fun q ys => Derives (Component.composition nodes inputs bindings outputs).compile
        ((Component.composition nodes inputs bindings outputs).edbOf E) M
        (sourceOf nodes inputs bindings a q, ys)) := by
    funext q ys
    exact propext (hM _)
  rw [hfun]
  exact (hM _).trans (compose_node W E M ha f.1 f.2)

end

end DdlogVerify
