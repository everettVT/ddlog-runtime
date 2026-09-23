import DdlogVerify.Compose

/-!
# Cyclic compositions: nodes cannot be evaluated separately

`stable_restrict` says every model of a composition restricts to a model of
each node. The converse — "if every node is at a fixed point given its inputs,
the whole is a model" — is **false** as soon as bindings form a cycle, which the
runtime permits (`compile_resolved_with`: "there is no additional node-cycle
rule").

Counterexample: nodes `A` and `B`, each `out(X) :- in(X)`, with `A.out → B.in`
and `B.out → A.in`, no external input. The global least model is empty, but
the interpretation in which the row `()` circulates around the loop is a
fixed point of every node separately.

So the composed meaning is the *global* fixpoint of the expanded program — which
is what source expansion into a single DDlog program computes — and not
whatever an independent, per-node evaluation (e.g. a message-passing scheduler
iterating nodes to quiescence from a non-empty start) might settle on.
-/

namespace DdlogVerify

namespace Cycle

def relay : Component Unit := .program [.copy "out" "in"] ["in"] ["out"]

def loop : Component Unit :=
  .composition [("A", relay), ("B", relay)] []
    [(("A", "out"), ("B", "in")), (("B", "out"), ("A", "in"))] [("o", ("A", "out"))]

def noInput : String → List Unit → Prop := fun _ _ => False

/-- The row `()` everywhere on the loop. -/
def circulating : Interp Name Unit := fun f =>
  f.2 = [()] ∧ (f.1 = .node "A" (.loc "in") ∨ f.1 = .node "A" (.loc "out") ∨
    f.1 = .node "B" (.loc "in") ∨ f.1 = .node "B" (.loc "out") ∨ f.1 = .output "o")

theorem loop_wf : WellFormed [("A", relay), ("B", relay)] []
    [(("A", "out"), ("B", "in")), (("B", "out"), ("A", "in"))] [("o", ("A", "out"))] where
  aliases_nodup := by decide
  input_targets := by simp
  binding_ends := by simp [Nodes.find?, relay, Component.ins, Component.outs]
  output_sources := by simp [Nodes.find?, relay, Component.outs]
  single_source := by
    intro a q s s' hs hs'
    rcases hs with ⟨_, _, h, _⟩ | ⟨f, cf, hb, hf, rfl⟩
    · simp at h
    rcases hs' with ⟨_, _, h, _⟩ | ⟨f', cf', hb', hf', rfl⟩
    · simp at h
    simp only [List.mem_cons, Prod.mk.injEq, List.not_mem_nil, or_false] at hb hb'
    rcases hb with ⟨rfl, rfl, rfl⟩ | ⟨rfl, rfl, rfl⟩ <;>
    rcases hb' with ⟨rfl, h1, h2⟩ | ⟨rfl, h1, h2⟩ <;>
    simp_all [Nodes.find?]
  connected := by
    intro a ca ha q hq
    simp only [Nodes.find?] at ha
    split at ha
    · cases ha
      simp only [relay, Component.ins, List.mem_cons, List.not_mem_nil, or_false] at hq
      subst hq
      rename_i h; subst h
      exact ⟨_, Or.inr ⟨("B", "out"), relay, by simp, by simp [Nodes.find?], rfl⟩⟩
    · split at ha
      · cases ha
        simp only [relay, Component.ins, List.mem_cons, List.not_mem_nil, or_false] at hq
        subst hq
        rename_i _ h; subst h
        exact ⟨_, Or.inr ⟨("A", "out"), relay, by simp, by simp [Nodes.find?], rfl⟩⟩
      · simp at ha

/-- The composed program has only copy clauses and no external facts, so it
derives nothing: its (unique) stable model is empty. -/
theorem loop_derives_nothing (I : Interp Name Unit) (f : Fact Name Unit) :
    ¬ Derives loop.compile (loop.edbOf noInput) I f := by
  intro h
  induction h with
  | edb h =>
    obtain ⟨q, hq, _⟩ := h
    simp [loop, Component.ins] at hq
  | rule hmem =>
    simp [loop, Component.compile, Nodes.compile, relay, Program.rename, Clause.rename,
      inputBridges, bindingBridges, outputCopies, Nodes.find?] at hmem
  | copy _ _ ih => exact ih
  | op hmem =>
    simp [loop, Component.compile, Nodes.compile, relay, Program.rename, Clause.rename,
      inputBridges, bindingBridges, outputCopies, Nodes.find?] at hmem

theorem circulating_not_stable : ¬ Stable loop.compile (loop.edbOf noInput) circulating := by
  intro h
  have := (h (.output "o", [()])).1 ⟨rfl, by simp⟩
  exact loop_derives_nothing _ _ this

/-- The relay program derives `out` exactly where it is fed `in`. -/
theorem relay_derives (E : String → List Unit → Prop) (I : Interp Name Unit) (n : Name)
    (xs : List Unit) :
    Derives relay.compile (relay.edbOf E) I (n, xs) ↔
      (n = .loc "in" ∨ n = .loc "out") ∧ E "in" xs := by
  have hmem : ∀ c, c ∈ relay.compile ↔ c = Clause.copy (Name.loc "out") (Name.loc "in") := by
    intro c
    simp [relay, Component.compile, Program.rename, Clause.rename]
  have hin : Derives relay.compile (relay.edbOf E) I (.loc "in", xs) ↔ E "in" xs := by
    rw [derives_underived]
    · simp [Component.edbOf, relay, Component.ins, Component.inPort]
    · intro c hc; rw [hmem] at hc; subst hc; simp [Clause.head]
  by_cases h1 : n = .loc "in"
  · subst h1; simp [hin]
  by_cases h2 : n = .loc "out"
  · subst h2
    rw [derives_copies]
    · simp only [hmem, Clause.copy.injEq, true_and]
      simp [hin]
    · rintro ys ⟨q, hq, he, _⟩
      simp [relay, Component.ins, Component.inPort] at hq he
      subst hq; simp at he
    · intro c hc _; rw [hmem] at hc; exact ⟨_, hc⟩
  · simp only [h1, h2, false_or, false_and, iff_false]
    intro hd
    rw [derives_underived] at hd
    · obtain ⟨q, hq, he, _⟩ := hd
      simp [relay, Component.ins, Component.inPort] at hq he
      subst hq; exact h1 he
    · intro c hc; rw [hmem] at hc; subst hc; simpa [Clause.head] using Ne.symm h2

/-- …yet each node, fed from `circulating` at its bound source, is at a fixed
point of its own program. -/
theorem circulating_locally_stable (a : String) (ha : a = "A" ∨ a = "B") :
    Stable relay.compile
      (relay.edbOf fun q ys => circulating
        (sourceOf [("A", relay), ("B", relay)] []
          [(("A", "out"), ("B", "in")), (("B", "out"), ("A", "in"))] a q, ys))
      (pullback (Name.node a) circulating) := by
  have src : ∀ q, q = "in" → sourceOf [("A", relay), ("B", relay)] []
      [(("A", "out"), ("B", "in")), (("B", "out"), ("A", "in"))] a q =
      .node (if a = "A" then "B" else "A") (.loc "out") := by
    rintro q rfl
    rcases ha with rfl | rfl
    · exact sourceOf_eq loop_wf (Or.inr ⟨("B", "out"), relay, by simp, by simp [Nodes.find?], rfl⟩)
    · exact sourceOf_eq loop_wf (Or.inr ⟨("A", "out"), relay, by simp, by simp [Nodes.find?], rfl⟩)
  rintro ⟨n, xs⟩
  rw [relay_derives, src "in" rfl]
  rcases ha with rfl | rfl <;>
  · simp only [pullback, circulating, ite_true]
    cases n <;> simp
    · rename_i s
      by_cases h : s = "in" <;> by_cases h' : s = "out" <;> simp_all
