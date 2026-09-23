import DdlogVerify.Datalog

/-!
# Localization

The key lemma behind composition. A composed program contains a renamed copy
`ρ(sub)` of a component and, for each of the component's input relations `l`,
one bridge `ρ l :- src l`. If *nothing else* derives a relation in the image of
`ρ` and the external facts never mention that image, then the facts the composed
program derives inside the image are exactly the facts the component derives on
its own when its inputs are fed with the composed program's facts at the bridge
sources.

The statement is for an arbitrary negation oracle `I`, so it transfers to stable
models (and hence to the stratified model the native engine maintains).
-/

namespace DdlogVerify

variable {P Q C : Type}

structure Embedding (prog : Program Q C) (edb : Interp Q C)
    (ρ : P → Q) (sub : Program P C) (In : P → Prop) (src : P → Q) : Prop where
  inj : ∀ a b, ρ a = ρ b → a = b
  /-- Every component clause is present, renamed. -/
  sub_mem : ∀ c ∈ sub, c.rename ρ ∈ prog
  /-- Every component input has its bridge. -/
  bridge_mem : ∀ l, In l → Clause.copy (ρ l) (src l) ∈ prog
  /-- Nothing else derives into the image of `ρ`; a bridge into `ρ l` always comes
  from the one source `src l` (no input has two sources). -/
  dom : ∀ c ∈ prog, ∀ p, c.head = ρ p →
    (∃ c' ∈ sub, c = c'.rename ρ) ∨ (In p ∧ c = Clause.copy (ρ p) (src p))
  /-- External facts never land inside the component. -/
  edb_out : ∀ p xs, ¬ edb (ρ p, xs)

/-- The component's own external facts: its inputs, read at the bridge sources. -/
def fedInputs (In : P → Prop) (D : Interp Q C) (src : P → Q) : Interp P C :=
  fun f => In f.1 ∧ D (src f.1, f.2)

def pullback (ρ : P → Q) (I : Interp Q C) : Interp P C := fun f => I (ρ f.1, f.2)

theorem ground_rename_eq {ρ : P → Q} {σ : Subst C} {a : Atom P C} :
    (a.rename ρ).ground σ = (ρ a.pred, (a.ground σ).2) := rfl

theorem localize_to {prog : Program Q C} {edb I : Interp Q C} {ρ : P → Q}
    {sub : Program P C} {In : P → Prop} {src : P → Q}
    (E : Embedding prog edb ρ sub In src) :
    ∀ f, Derives prog edb I f → ∀ p, f.1 = ρ p →
      Derives sub (fedInputs In (Derives prog edb I) src) (pullback ρ I) (p, f.2) := by
  intro f hf
  induction hf with
  | edb h =>
    intro p hp
    rename_i f
    obtain ⟨q, xs⟩ := f
    simp only at hp
    subst hp
    exact absurd h (E.edb_out p xs)
  | @rule r σ hmem hguard _ hneg ih =>
    intro p hp
    rcases E.dom _ hmem p hp with ⟨c', hc', hc⟩ | ⟨_, hc⟩
    · cases c' with
      | rule r' =>
        simp only [Clause.rename, Clause.rule.injEq] at hc
        subst hc
        have hp' : r'.head.pred = p := E.inj _ _ hp
        subst hp'
        refine Derives.rule hc' hguard ?_ ?_
        · intro a ha
          exact ih (a.rename ρ) (List.mem_map_of_mem ha) a.pred rfl
        · intro a ha
          exact hneg (a.rename ρ) (List.mem_map_of_mem ha)
      | copy => simp [Clause.rename] at hc
      | op => simp [Clause.rename] at hc
    · cases hc
  | @copy dst s xs hmem hs ih =>
    intro p hp
    simp only at hp
    rcases E.dom _ hmem p hp with ⟨c', hc', hc⟩ | ⟨hin, hc⟩
    · cases c' with
      | copy d' s' =>
        simp only [Clause.rename, Clause.copy.injEq] at hc
        obtain ⟨h1, h2⟩ := hc
        subst h2
        have : d' = p := E.inj _ _ (h1.symm.trans hp)
        subst this
        exact Derives.copy hc' (ih s' rfl)
      | rule => simp [Clause.rename] at hc
      | op => simp [Clause.rename] at hc
    · simp only [Clause.copy.injEq] at hc
      obtain ⟨_, h2⟩ := hc
      subst h2
      exact Derives.edb ⟨hin, hs⟩
  | @op dst ins F xs hmem hF =>
    intro p hp
    simp only at hp
    rcases E.dom _ hmem p hp with ⟨c', hc', hc⟩ | ⟨_, hc⟩
    · cases c' with
      | op d' ins' F' =>
        simp only [Clause.rename, Clause.op.injEq] at hc
        obtain ⟨h1, h2, h3⟩ := hc
        subst h2 h3
        have : d' = p := E.inj _ _ (h1.symm.trans hp)
        subst this
        refine Derives.op hc' ?_
        simpa [List.map_map, pullback, Function.comp_def] using hF
      | rule => simp [Clause.rename] at hc
      | copy => simp [Clause.rename] at hc
    · cases hc

theorem localize_from {prog : Program Q C} {edb I : Interp Q C} {ρ : P → Q}
    {sub : Program P C} {In : P → Prop} {src : P → Q}
    (E : Embedding prog edb ρ sub In src) :
    ∀ f, Derives sub (fedInputs In (Derives prog edb I) src) (pullback ρ I) f →
      Derives prog edb I (ρ f.1, f.2) := by
  intro f hf
  induction hf with
  | edb h =>
    exact Derives.copy (E.bridge_mem _ h.1) h.2
  | @rule r σ hmem hguard _ hneg ih =>
    have hm := E.sub_mem _ hmem
    exact Derives.rule (r := r.rename ρ) hm hguard
      (fun a ha => by
        obtain ⟨a', ha', rfl⟩ := List.mem_map.1 ha
        exact ih a' ha')
      (fun a ha => by
        obtain ⟨a', ha', rfl⟩ := List.mem_map.1 ha
        exact hneg a' ha')
  | @copy dst s xs hmem _ ih =>
    exact Derives.copy (E.sub_mem _ hmem) ih
  | @op dst ins F xs hmem hF =>
    refine Derives.op (E.sub_mem _ hmem) ?_
    simpa [List.map_map, pullback, Function.comp_def] using hF

/-- **Localization.** Inside the image of `ρ`, the composed program derives exactly
what the component derives from its fed inputs. -/
theorem localize {prog : Program Q C} {edb I : Interp Q C} {ρ : P → Q}
    {sub : Program P C} {In : P → Prop} {src : P → Q}
    (E : Embedding prog edb ρ sub In src) (p : P) (xs : List C) :
    Derives prog edb I (ρ p, xs) ↔
      Derives sub (fedInputs In (Derives prog edb I) src) (pullback ρ I) (p, xs) :=
  ⟨fun h => localize_to E _ h p rfl, fun h => localize_from E _ h⟩

/-- A fact whose relation no clause derives is derived only if it is external. -/
theorem derives_underived {prog : Program Q C} {edb I : Interp Q C} {q : Q}
    (h : ∀ c ∈ prog, c.head ≠ q) (xs : List C) :
    Derives prog edb I (q, xs) ↔ edb (q, xs) := by
  constructor
  · intro hd
    generalize hf : (q, xs) = f at hd
    cases hd with
    | edb h' => exact hf ▸ h'
    | @rule r σ hmem =>
      exact absurd (congrArg Prod.fst hf).symm (h _ hmem)
    | copy hmem => exact absurd (congrArg Prod.fst hf).symm (h _ hmem)
    | op hmem => exact absurd (congrArg Prod.fst hf).symm (h _ hmem)
  · exact Derives.edb

/-- A relation derived only by copies (and no external facts) holds exactly the
union of the copied sources. -/
theorem derives_copies {prog : Program Q C} {edb I : Interp Q C} {q : Q}
    (hedb : ∀ xs, ¬ edb (q, xs))
    (h : ∀ c ∈ prog, c.head = q → ∃ s, c = Clause.copy q s) (xs : List C) :
    Derives prog edb I (q, xs) ↔
      ∃ s, Clause.copy q s ∈ prog ∧ Derives prog edb I (s, xs) := by
  constructor
  · intro hd
    generalize hf : (q, xs) = f at hd
    cases hd with
    | edb h' => exact absurd (hf ▸ h') (hedb xs)
    | @rule r σ hmem =>
      obtain ⟨s, hs⟩ := h _ hmem (congrArg Prod.fst hf).symm
      cases hs
    | @copy d s ys hmem hs =>
      simp only [Prod.mk.injEq] at hf
      obtain ⟨rfl, rfl⟩ := hf
      exact ⟨s, hmem, hs⟩
    | @op d ins F ys hmem =>
      obtain ⟨s, hs⟩ := h _ hmem (congrArg Prod.fst hf).symm
      cases hs
  · rintro ⟨s, hmem, hs⟩
    exact Derives.copy hmem hs

end DdlogVerify
