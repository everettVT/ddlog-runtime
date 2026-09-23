import DdlogVerify.Localize

/-!
# Binding aliases (lowering version 2)

Under `LoweringOptions::VERSION_2` (`alias_bindings: true`) a bound input `t`
whose only definition is the bridge `t(X..) :- s(X..)` is not generated at all:
every occurrence of `t` is rewritten to `s` (`Expansion::resolve_aliases`) and
the bridge disappears. The documentation claims that "public relation names and
contents are identical under every version".

`resolve_equiv` proves that renaming every target to its ultimate source
preserves every relation for any negation oracle that respects the resolution,
and `stable_resolve_iff` turns this into an exact correspondence of stable
models: a model of the version-1 program is precisely a model of the version-2
program on every relation that is not an alias target, extended with
`t := root t`.

The rewritten bridge becomes the no-op `s :- s`; `derives_drop_self_copies`
shows removing it (as Rust does, by never generating it) changes nothing.

Preconditions, and where the Rust code establishes them:

* `t ≠ s` and `t`'s only defining clause is the bridge: a bound target is a
  node input (never a rule head — `lower_clauses` rejects rules whose head is a
  declared input, and every leaf is lowered on its own in `Expansion::program`
  before composition) or a nested `Composite<k>_Input_*` (only ever defined by
  bindings), and "Multiple sources for input" rejects a second bridge.
* `t` carries no external facts: only top-level `Input_*` relations are
  `input relation`s of a composition.
-/

namespace DdlogVerify

variable {Q C : Type}

/-! ## Dropping the rewritten bridge -/

/-- Removing self-copies `s :- s` (the rewritten bridge, which Rust never emits)
does not change what is derived. -/
theorem derives_drop_self_copies {P : Program Q C} {P' : Program Q C} {edb I : Interp Q C}
    (hsub : ∀ c ∈ P', c ∈ P) (hself : ∀ c ∈ P, c ∈ P' ∨ ∃ q, c = Clause.copy q q) (f : Fact Q C) :
    Derives P edb I f ↔ Derives P' edb I f := by
  constructor
  · intro h
    induction h with
    | edb h => exact Derives.edb h
    | rule hmem hg _ hn ih =>
      rcases hself _ hmem with h | ⟨q, hq⟩
      · exact Derives.rule h hg ih hn
      · cases hq
    | @copy d s0 xs hmem _ ih =>
      rcases hself _ hmem with h | ⟨q, hq⟩
      · exact Derives.copy h ih
      · simp only [Clause.copy.injEq] at hq
        obtain ⟨rfl, rfl⟩ := hq
        exact ih
    | op hmem hF =>
      rcases hself _ hmem with h | ⟨q, hq⟩
      · exact Derives.op h hF
      · cases hq
  · intro h
    induction h with
    | edb h => exact Derives.edb h
    | rule hmem hg _ hn ih => exact Derives.rule (hsub _ hmem) hg ih hn
    | copy hmem _ ih => exact Derives.copy (hsub _ hmem) ih
    | op hmem hF => exact Derives.op (hsub _ hmem) hF

/-! ## Resolving all aliases at once

`Expansion::resolve_aliases` does not alias one binding at a time: it maps every
target to its *ultimate* source (following target → source chains, which only
cross nesting levels outward) and renames all of them simultaneously. The model:
`T` is the set of aliased targets, `src t` the immediate source of target `t`,
and `root` the resolution map, characterised by
`root q = if T q then root (src q) else q`. A `depth` that strictly decreases
from a target to its source witnesses that chains terminate (Rust rejects a
cycle with "Cyclic binding alias"; `Composition.lean` explains why none occurs).
-/

structure ResolveOk (prog : Program Q C) (edb : Interp Q C) (T : Q → Prop) (src root : Q → Q)
    (depth : Q → Nat) : Prop where
  bridge : ∀ t, T t → Clause.copy t (src t) ∈ prog
  only : ∀ c ∈ prog, T c.head → c = Clause.copy c.head (src c.head)
  edb_out : ∀ t xs, T t → ¬ edb (t, xs)
  root_target : ∀ t, T t → root t = root (src t)
  root_other : ∀ q, ¬ T q → root q = q
  depth_lt : ∀ t, T t → depth (src t) < depth t

section
variable {prog : Program Q C} {edb : Interp Q C} {T : Q → Prop} {src root : Q → Q}
  {depth : Q → Nat}

theorem ResolveOk.root_not_target (R : ResolveOk prog edb T src root depth) :
    ∀ q, ¬ T (root q) := by
  intro q
  induction h : depth q using Nat.strongRecOn generalizing q with
  | _ n ih =>
    by_cases hq : T q
    · rw [R.root_target q hq]
      exact ih _ (h ▸ R.depth_lt q hq) _ rfl
    · rw [R.root_other q hq]; exact hq

theorem ResolveOk.root_root (R : ResolveOk prog edb T src root depth) (q : Q) :
    root (root q) = root q := R.root_other _ (R.root_not_target q)

theorem ResolveOk.head_not_target_of_ne_copy (R : ResolveOk prog edb T src root depth)
    {c : Clause Q C} (hc : c ∈ prog) (h : ∀ d s, c ≠ Clause.copy d s) : ¬ T c.head :=
  fun ht => h _ _ (R.only c hc ht)

/-- A target holds what its root holds (for every oracle). -/
theorem ResolveOk.lift (R : ResolveOk prog edb T src root depth) (I : Interp Q C) :
    ∀ q xs, Derives prog edb I (root q, xs) → Derives prog edb I (q, xs) := by
  intro q
  induction h : depth q using Nat.strongRecOn generalizing q with
  | _ n ih =>
    intro xs hd
    by_cases hq : T q
    · rw [R.root_target q hq] at hd
      exact Derives.copy (R.bridge q hq) (ih _ (h ▸ R.depth_lt q hq) _ rfl xs hd)
    · rwa [R.root_other q hq] at hd

theorem ResolveOk.to_root (R : ResolveOk prog edb T src root depth) (I : Interp Q C) :
    ∀ q xs, Derives prog edb I (q, xs) → Derives prog edb I (root q, xs) := by
  intro q
  induction h : depth q using Nat.strongRecOn generalizing q with
  | _ n ih =>
    intro xs hd
    by_cases hq : T q
    · rw [R.root_target q hq]
      apply ih _ (h ▸ R.depth_lt q hq) _ rfl xs
      rw [derives_copies (fun ys => R.edb_out q ys hq)] at hd
      · obtain ⟨s', hs', hd'⟩ := hd
        have := R.only _ hs' hq
        simp only [Clause.head, Clause.copy.injEq, true_and] at this
        exact this ▸ hd'
      · intro c hc hh
        exact ⟨src q, by have := R.only c hc (hh ▸ hq); rw [hh] at this; exact this⟩
    · rwa [R.root_other q hq]

/-- **Simultaneous alias resolution preserves every relation.** For an oracle that
respects the resolution, the version-1 program derives `q` iff the version-2
program (every target renamed to its root) derives `root q`. -/
theorem resolve_equiv (R : ResolveOk prog edb T src root depth) (I : Interp Q C)
    (hI : ∀ q xs, I (root q, xs) ↔ I (q, xs)) (q : Q) (xs : List C) :
    Derives prog edb I (q, xs) ↔ Derives (prog.rename root) edb I (root q, xs) := by
  constructor
  · intro hd
    suffices ∀ f, Derives prog edb I f → Derives (prog.rename root) edb I (root f.1, f.2) from
      this _ hd
    intro f hf
    induction hf with
    | edb h =>
      rename_i f
      obtain ⟨p, ys⟩ := f
      have hp : ¬ T p := fun ht => R.edb_out p ys ht h
      simp only [R.root_other p hp]
      exact Derives.edb h
    | @rule r σ hmem hguard _ hneg ih =>
      refine Derives.rule (r := r.rename root) (Program.mem_rename hmem) hguard ?_ ?_
      · intro a ha
        obtain ⟨a', ha', rfl⟩ := List.mem_map.1 ha
        exact ih a' ha'
      · intro a ha
        obtain ⟨a', ha', rfl⟩ := List.mem_map.1 ha
        intro hI'
        exact hneg a' ha' ((hI _ _).1 hI')
    | copy hmem _ ih => exact Derives.copy (Program.mem_rename (ρ := root) hmem) ih
    | @op d ins F ys hmem hF =>
      refine Derives.op (Program.mem_rename (ρ := root) hmem) ?_
      have : (List.map root ins).map (fun p ys => I (p, ys)) = ins.map (fun p ys => I (p, ys)) := by
        rw [List.map_map]
        congr 1
        funext p
        funext ys
        exact propext (hI p ys)
      simpa [Clause.rename, this] using hF
  · intro hd
    suffices ∀ f, Derives (prog.rename root) edb I f → Derives prog edb I f from
      R.lift I q xs (this _ hd)
    intro f hf
    induction hf with
    | edb h => exact Derives.edb h
    | @rule r' σ hmem hguard _ hneg ih =>
      obtain ⟨c, hc, hce⟩ := List.mem_map.1 hmem
      cases c with
      | rule r =>
        simp only [Clause.rename, Clause.rule.injEq] at hce
        subst hce
        have hhead : ¬ T r.head.pred :=
          R.head_not_target_of_ne_copy hc (fun _ _ h => by cases h)
        have hd : Derives prog edb I (r.head.ground σ) := by
          refine Derives.rule hc hguard ?_ ?_
          · intro a ha
            exact R.lift I _ _ (ih (a.rename root) (List.mem_map_of_mem ha))
          · intro a ha hIa
            exact hneg (a.rename root) (List.mem_map_of_mem ha) ((hI _ _).2 hIa)
        simpa [Rule.rename, Atom.ground, Atom.rename, R.root_other _ hhead] using hd
      | copy => simp [Clause.rename] at hce
      | op => simp [Clause.rename] at hce
    | @copy d' s' ys hmem _ ih =>
      obtain ⟨c, hc, hce⟩ := List.mem_map.1 hmem
      cases c with
      | copy d s0 =>
        simp only [Clause.rename, Clause.copy.injEq] at hce
        obtain ⟨rfl, rfl⟩ := hce
        by_cases hd : T d
        · have := R.only _ hc hd
          simp only [Clause.head, Clause.copy.injEq, true_and] at this
          subst this
          rw [R.root_target d hd]
          exact R.to_root I _ _ (R.lift I _ _ ih)
        · rw [R.root_other d hd]
          exact Derives.copy hc (R.lift I _ _ ih)
      | rule => simp [Clause.rename] at hce
      | op => simp [Clause.rename] at hce
    | @op d' ins' F ys hmem hF =>
      obtain ⟨c, hc, hce⟩ := List.mem_map.1 hmem
      cases c with
      | op d ins F0 =>
        simp only [Clause.rename, Clause.op.injEq] at hce
        obtain ⟨rfl, rfl, rfl⟩ := hce
        have hd : ¬ T d := R.head_not_target_of_ne_copy hc (fun _ _ h => by cases h)
        have : (List.map root ins).map (fun p ys => I (p, ys)) = ins.map (fun p ys => I (p, ys)) := by
          rw [List.map_map]
          congr 1
          funext p
          funext ys
          exact propext (hI p ys)
        rw [this] at hF
        rw [R.root_other d hd]
        exact Derives.op hc hF
      | rule => simp [Clause.rename] at hce
      | copy => simp [Clause.rename] at hce

/-- **Stable models of version 1 and version 2 correspond.** `M` is a stable model
of the version-1 program iff every target equals its root in `M` and `M` is a
stable model of the version-2 program on every relation that is not a target.
In particular every public relation (external inputs, external outputs and the
node outputs they copy are never targets) has the same contents under both. -/
theorem stable_resolve_iff (R : ResolveOk prog edb T src root depth) (M : Interp Q C) :
    Stable prog edb M ↔
      (∀ q xs, M (root q, xs) ↔ M (q, xs)) ∧
      ∀ q, ¬ T q → ∀ xs, M (q, xs) ↔ Derives (prog.rename root) edb M (q, xs) := by
  constructor
  · intro hM
    have hroot : ∀ q xs, M (root q, xs) ↔ M (q, xs) := fun q xs =>
      (hM _).trans ((Iff.intro (R.lift M q xs) (R.to_root M q xs)).trans (hM _).symm)
    refine ⟨hroot, fun q hq xs => ?_⟩
    rw [hM (q, xs), resolve_equiv R M hroot q xs, R.root_other q hq]
  · rintro ⟨hroot, hq⟩ ⟨q, xs⟩
    rw [resolve_equiv R M hroot q xs, ← hroot q xs]
    exact hq (root q) (R.root_not_target q) xs

end

end DdlogVerify
