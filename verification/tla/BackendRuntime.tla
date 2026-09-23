--------------------------- MODULE BackendRuntime ---------------------------
(***************************************************************************)
(* Model of ddlog-runtime's Backend (src/lib.rs), its bounded reads        *)
(* (src/bounded.rs), explicit checkpoints (src/checkpoint.rs) and the      *)
(* ProgramInstance admission guard (src/instance.rs).                      *)
(*                                                                         *)
(* One element of Backends is one Backend wrapped in one ProgramInstance.  *)
(* The native DDlog child is abstracted to the set of input facts it has  *)
(* committed (native[b]); derived outputs are functions of that set, so    *)
(* "outputs are consistent" reduces to native[b] = facts[b].               *)
(*                                                                         *)
(* Line references are to the files as read on 2026-09-23.                 *)
(***************************************************************************)
EXTENDS Naturals, FiniteSets

CONSTANTS
    Backends,             \* independent Backend/ProgramInstance owners
    Facts,                \* universe of (rendered) input facts
    Programs,             \* abstract program texts that can be installed
    ImpurePrograms,       \* programs with agent_* relations (checkpoint.rs:68-73)
    NoProg,               \* "no program installed" marker
    MaxRev, MaxVer, MaxOwner,
    \* ---- mode --------------------------------------------------------------
    Hosted,               \* TRUE: instance_id is Some (host/worlds), guard of
                          \* instance.rs:152-158 applies. FALSE: standalone /
                          \* library-level calls without that guard.
    \* ---- environment behaviours (TRUE = adversarial behaviour possible) ----
    NativeErrorText,      \* child may reject a commit and print error text
    LargeOutputs,         \* a dump / delta response may exceed 4 MiB
    TryWaitErr,           \* Child::try_wait may return Err for a live child
    CrossInstanceCursors, \* callers may present a cursor to another owner
    \* ---- proposed code fixes (FALSE = code as written) ---------------------
    FixDeltaCheck,        \* treat error text under `commit dump_changes` as failure
    FixInputOwner,        \* bind InputCursor to the live owner identity
    FixFailedGuard,       \* Backend ops refuse when failed (not only runtime None)
    FixReadDrain          \* oversized non-streaming reads drain and do not poison

VARIABLES
    facts,        \* Backend.facts           lib.rs:207   (retained, acknowledged inputs)
    native,       \* committed input set of the live child (not a code variable)
    runtime,      \* Backend.runtime         lib.rs:205   0 = None, else Runtime.identity
    alive,        \* whether the live child process is still running
    failed,       \* Backend.failed          lib.rs:212
    prog,         \* Backend.active_source   lib.rs:211   (abstract program id)
    version,      \* Backend.version         lib.rs:208
    revision,     \* Backend.revision        lib.rs:209
    ownerCtr,     \* global source of Runtime.identity (bounded.rs:16-28, lib.rs:94)
    qcur,         \* latest QueryCursor issued per backend  (bounded.rs:59-67)
    icur,         \* latest InputCursor issued per backend  (instance.rs:83-88)
    ckpt,         \* latest checkpoint written (checkpoint.rs:17-35)
    \* ---- ghost / history variables (not in the code) ----------------------
    epoch,        \* count of state changes (mutations/replacements) per backend
    everFailed,   \* backend has been failed at some point
    badAccept,    \* a cursor was accepted although unsound
    ackWhileFailed, \* an input transaction was acknowledged while failed = TRUE
    readPoisonLive  \* a read disabled an owner whose child was alive

vars == <<facts, native, runtime, alive, failed, prog, version, revision,
          ownerCtr, qcur, icur, ckpt, epoch, everFailed, badAccept,
          ackWhileFailed, readPoisonLive>>

MaxEpoch == MaxRev + 1
NoCursor == [kind |-> "none", from |-> CHOOSE x \in Backends : TRUE,
             owner |-> 0, rev |-> 0, epoch |-> 0]
Cursors  == [kind : {"q", "i"}, from : Backends, owner : 0..MaxOwner,
             rev : 0..MaxRev, epoch : 0..MaxEpoch]
NoCkpt   == [present |-> FALSE, facts |-> {}, rev |-> 0, ver |-> 0,
             prog |-> NoProg, consistent |-> TRUE]

\* Backend::health, lib.rs:283-291
Health(b) == IF failed[b] THEN "failed"
             ELSE IF runtime[b] # 0 THEN "ready" ELSE "uninitialized"

\* ProgramInstance::execute refusal of a failed hosted instance, instance.rs:152-158
Admits(b) == ~(Hosted /\ failed[b])

\* Proposed fix 4: every native Backend op also refuses a failed backend.
FailedGuard(b) == FixFailedGuard => ~failed[b]

\* Step labels are documentation only (no history variable, to keep the
\* state space small); action properties below refer to the actions directly.
Label(op, b) == TRUE

\* Every exchange error path: runtime = None; failed = true
\* (lib.rs:515-516, 532-533, 611-612, bounded.rs:189-190). Dropping the
\* Runtime kills the child (lib.rs:180-186).
Poison(b) ==
    /\ runtime'    = [runtime EXCEPT ![b] = 0]
    /\ alive'      = [alive EXCEPT ![b] = FALSE]
    /\ failed'     = [failed EXCEPT ![b] = TRUE]
    /\ everFailed' = [everFailed EXCEPT ![b] = TRUE]

Ghosts == <<badAccept, ackWhileFailed, readPoisonLive>>

TypeOK ==
    /\ facts \in [Backends -> SUBSET Facts]
    /\ native \in [Backends -> SUBSET Facts]
    /\ runtime \in [Backends -> 0..MaxOwner]
    /\ alive \in [Backends -> BOOLEAN]
    /\ failed \in [Backends -> BOOLEAN]
    /\ prog \in [Backends -> Programs \cup {NoProg}]
    /\ version \in [Backends -> 0..MaxVer]
    /\ revision \in [Backends -> 0..MaxRev]
    /\ ownerCtr \in 0..MaxOwner
    /\ qcur \in [Backends -> Cursors \cup {NoCursor}]
    /\ icur \in [Backends -> Cursors \cup {NoCursor}]
    /\ epoch \in [Backends -> 0..MaxEpoch]
    /\ everFailed \in [Backends -> BOOLEAN]

Init ==   \* Backend::new, lib.rs:222-240
    /\ facts = [b \in Backends |-> {}]
    /\ native = [b \in Backends |-> {}]
    /\ runtime = [b \in Backends |-> 0]
    /\ alive = [b \in Backends |-> FALSE]
    /\ failed = [b \in Backends |-> FALSE]
    /\ prog = [b \in Backends |-> NoProg]
    /\ version = [b \in Backends |-> 0]
    /\ revision = [b \in Backends |-> 0]
    /\ ownerCtr = 0
    /\ qcur = [b \in Backends |-> NoCursor]
    /\ icur = [b \in Backends |-> NoCursor]
    /\ ckpt = NoCkpt
    /\ epoch = [b \in Backends |-> 0]
    /\ everFailed = [b \in Backends |-> FALSE]
    /\ badAccept = FALSE
    /\ ackWhileFailed = FALSE
    /\ readPoisonLive = FALSE

(***************************************************************************)
(* Install / replacement: Backend::install_source, lib.rs:351-429          *)
(***************************************************************************)
InstallGuard(b, p) ==
    /\ Admits(b)                                   \* instance.rs:152-158
    /\ p \in ImpurePrograms => facts[b] = {}        \* operations.rs:95-97
    /\ revision[b] < MaxRev /\ version[b] < MaxVer  \* checked_add, lib.rs:369-373

\* Compile failure (lib.rs:395-400) or Runtime::start spawn failure
\* (lib.rs:401-406, 87): nothing but `attempt`/build dir changes.
InstallFailEarly(b, p) ==
    /\ InstallGuard(b, p)
    /\ \E op \in {"InstallCompileFail", "InstallStartFail"} : Label(op, b)
    /\ UNCHANGED <<facts, native, runtime, alive, failed, prog, version,
                   revision, ownerCtr, qcur, icur, ckpt, epoch, everFailed>>
    /\ UNCHANGED Ghosts

\* Candidate started (identity consumed, lib.rs:94) but replay exchange
\* failed (lib.rs:412) or printed output (lib.rs:413-417). Candidate is
\* dropped; the prior program is untouched.
InstallReplayFail(b, p) ==
    /\ InstallGuard(b, p)
    /\ ownerCtr < MaxOwner
    /\ ownerCtr' = ownerCtr + 1
    /\ Label("InstallReplayFail", b)
    /\ UNCHANGED <<facts, native, runtime, alive, failed, prog, version,
                   revision, qcur, icur, ckpt, epoch, everFailed>>
    /\ UNCHANGED Ghosts

\* Replay of all retained facts (lib.rs:407-412) then commit (lib.rs:418-424).
\* Note: install_source has no `failed` guard and clears it (lib.rs:419).
InstallOk(b, p) ==
    /\ InstallGuard(b, p)
    /\ ownerCtr < MaxOwner
    /\ ownerCtr' = ownerCtr + 1
    /\ runtime' = [runtime EXCEPT ![b] = ownerCtr + 1]
    /\ alive' = [alive EXCEPT ![b] = TRUE]
    /\ native' = [native EXCEPT ![b] = facts[b]]
    /\ failed' = [failed EXCEPT ![b] = FALSE]
    /\ prog' = [prog EXCEPT ![b] = p]
    /\ version' = [version EXCEPT ![b] = @ + 1]
    /\ revision' = [revision EXCEPT ![b] = @ + 1]
    /\ epoch' = [epoch EXCEPT ![b] = @ + 1]
    /\ Label("InstallOk", b)
    /\ UNCHANGED <<facts, qcur, icur, ckpt, everFailed>>
    /\ UNCHANGED Ghosts

(***************************************************************************)
(* Input transactions: Backend::apply_inner, lib.rs:460-522                *)
(* S is the staged set after validation (lib.rs:465-479); the child gets   *)
(* the net diff (lib.rs:480-491). dump = TRUE is `apply` (MCP apply_changes,*)
(* instance.rs:245,250); dump = FALSE is `apply_without_deltas`.           *)
(***************************************************************************)
ApplyGuard(b) ==
    /\ runtime[b] # 0            \* lib.rs:461-463 (only runtime, not failed)
    /\ Admits(b)                 \* instance.rs:152-158
    /\ FailedGuard(b)
    /\ revision[b] < MaxRev      \* lib.rs:464

\* Validation failure (lib.rs:466-479): returns before any native I/O.
ApplyInvalid(b) ==
    /\ ApplyGuard(b)
    /\ Label("ApplyInvalid", b)
    /\ UNCHANGED <<facts, native, runtime, alive, failed, prog, version,
                   revision, ownerCtr, qcur, icur, ckpt, epoch, everFailed>>
    /\ UNCHANGED Ghosts

\* Acknowledgement branch, lib.rs:505-507.
Acknowledge(b, S, op) ==
    /\ facts' = [facts EXCEPT ![b] = S]
    /\ revision' = [revision EXCEPT ![b] = @ + 1]
    /\ epoch' = [epoch EXCEPT ![b] = @ + 1]
    /\ ackWhileFailed' = (ackWhileFailed \/ failed[b])
    /\ Label(op, b)
    /\ UNCHANGED <<runtime, alive, failed, everFailed>>

\* Native commits and the marker is read: ack (empty reply for plain commit,
\* deltas for `commit dump_changes`).
ApplyCommitted(b, S, dump) ==
    /\ ApplyGuard(b) /\ alive[b]
    /\ native' = [native EXCEPT ![b] = S]
    /\ Acknowledge(b, S, "ApplyCommitted")
    /\ UNCHANGED <<prog, version, ownerCtr, qcur, icur, ckpt, badAccept,
                   readPoisonLive>>

\* Native rejects the transaction (does not commit) and prints error text.
\* lib.rs:497-503: only `!dump_deltas` turns non-empty output into an error.
ApplyErrorText(b, S, dump) ==
    /\ NativeErrorText
    /\ ApplyGuard(b) /\ alive[b]
    /\ UNCHANGED native
    /\ IF dump /\ ~FixDeltaCheck
       THEN Acknowledge(b, S, "ApplyErrTextAcked")          \* BUG (issue 1)
       ELSE /\ Poison(b)                                     \* lib.rs:514-520
            /\ Label("ApplyErrTextPoison", b)
            /\ UNCHANGED <<facts, revision, epoch, ackWhileFailed>>
    /\ UNCHANGED <<prog, version, ownerCtr, qcur, icur, ckpt, badAccept,
                   readPoisonLive>>

\* I/O error / child death before the child committed (includes a child
\* that had already died unobserved): lib.rs:514-520.
ApplyIOBefore(b, S, dump) ==
    /\ ApplyGuard(b)
    /\ Poison(b)
    /\ Label("ApplyIOBefore", b)
    /\ UNCHANGED <<facts, native, prog, version, revision, ownerCtr, qcur, icur,
                   ckpt, epoch, Ghosts>>

\* Child committed, then the ack was lost (death after commit), or the
\* reply exceeded 4 MiB (lib.rs:117-118; only with dump_changes deltas).
ApplyIOAfter(b, S, dump) ==
    /\ ApplyGuard(b) /\ alive[b]
    /\ \/ Label("ApplyIOAfterCommit", b)
       \/ LargeOutputs /\ dump /\ Label("ApplyOversizedDeltas", b)
    /\ native' = [native EXCEPT ![b] = S]
    /\ Poison(b)
    /\ UNCHANGED <<facts, prog, version, revision, ownerCtr, qcur, icur,
                   ckpt, epoch, Ghosts>>

(***************************************************************************)
(* Reads                                                                   *)
(***************************************************************************)
ReadGuard(b) == runtime[b] # 0 /\ Admits(b) /\ FailedGuard(b)

\* Non-streaming read (query / query_typed / why): read_runtime, lib.rs:523-549.
\* exchange caps the reply at 4 MiB without draining (lib.rs:117-118) and
\* read_runtime then poisons the owner (lib.rs:531-537).
Query(b) ==
    /\ ReadGuard(b)
    /\ IF ~alive[b]
       THEN /\ Poison(b) /\ Label("QueryIOPoison", b)
            /\ UNCHANGED readPoisonLive
       ELSE \/ /\ Label("QueryOk", b)
               /\ UNCHANGED <<runtime, alive, failed, everFailed, readPoisonLive>>
            \/ /\ LargeOutputs
               /\ IF FixReadDrain
                  THEN /\ Label("QueryOverflowErr", b)
                       /\ UNCHANGED <<runtime, alive, failed, everFailed,
                                      readPoisonLive>>
                  ELSE /\ Poison(b) /\ Label("QueryOverflowPoison", b)  \* BUG (issue 2)
                       /\ readPoisonLive' = TRUE
    /\ UNCHANGED <<facts, native, prog, version, revision, ownerCtr, qcur, icur,
                   ckpt, epoch, badAccept, ackWhileFailed>>

\* Bounded streaming read issuing a QueryCursor (bounded.rs:97-213).
\* Guard is only `runtime.as_mut()` (bounded.rs:123). Record/consumer errors
\* drain and never poison (bounded.rs:195-197); only I/O poisons (188-194).
BoundedRead(b) ==
    /\ ReadGuard(b)
    /\ IF ~alive[b]
       THEN /\ Poison(b) /\ Label("BoundedIOPoison", b) /\ UNCHANGED qcur
       ELSE /\ qcur' = [qcur EXCEPT ![b] =                   \* bounded.rs:201-211
                  [kind |-> "q", from |-> b, owner |-> runtime[b],
                   rev |-> revision[b], epoch |-> epoch[b]]]
            /\ Label("BoundedReadIssue", b)
            /\ UNCHANGED <<runtime, alive, failed, everFailed>>
    /\ UNCHANGED <<facts, native, prog, version, revision, ownerCtr, icur,
                   ckpt, epoch, Ghosts>>

\* Paged read of retained inputs issuing an InputCursor (instance.rs:357-389);
\* export_inputs requires health "ready" (lib.rs:566-569).
InputRead(b) ==
    /\ Health(b) = "ready" /\ Admits(b)
    /\ icur' = [icur EXCEPT ![b] =                            \* instance.rs:388
           [kind |-> "i", from |-> b, owner |-> runtime[b],
            rev |-> revision[b], epoch |-> epoch[b]]]
    /\ Label("InputReadIssue", b)
    /\ UNCHANGED <<facts, native, runtime, alive, failed, prog, version,
                   revision, ownerCtr, qcur, ckpt, epoch, everFailed, Ghosts>>

\* Ground truth: a continuation is sound on b iff issued by b's live owner
\* and nothing changed b's state since it was issued.
Sound(c, b) == c.from = b /\ c.owner = runtime[b] /\ c.epoch = epoch[b]

Presentable(c, b) == c.kind # "none" /\ (CrossInstanceCursors \/ c.from = b)

\* Continue a bounded read: check at bounded.rs:124-135.
UseQCursor(b) ==
    \E x \in Backends :
      LET c == qcur[x] IN
      /\ Presentable(c, b)
      /\ ReadGuard(b)
      /\ IF c.owner = runtime[b] /\ c.rev = revision[b]
         THEN IF alive[b]
              THEN /\ badAccept' = (badAccept \/ ~Sound(c, b))
                   /\ Label("QCursorAccepted", b)
                   /\ UNCHANGED <<runtime, alive, failed, everFailed>>
              ELSE /\ Poison(b) /\ Label("QCursorIOPoison", b)
                   /\ UNCHANGED badAccept
         ELSE /\ Label("QCursorRejected", b)
              /\ UNCHANGED <<runtime, alive, failed, everFailed, badAccept>>
      /\ UNCHANGED <<facts, native, prog, version, revision, ownerCtr, qcur,
                     icur, ckpt, epoch, ackWhileFailed, readPoisonLive>>

\* Continue an input page: check at instance.rs:360-369 (revision + predicate
\* only; no owner). FixInputOwner adds an owner comparison.
UseICursor(b) ==
    \E x \in Backends :
      LET c == icur[x] IN
      /\ Presentable(c, b)
      /\ Health(b) = "ready" /\ Admits(b)
      /\ IF c.rev = revision[b] /\ (FixInputOwner => c.owner = runtime[b])
         THEN /\ badAccept' = (badAccept \/ ~Sound(c, b))
              /\ Label("ICursorAccepted", b)
         ELSE /\ Label("ICursorRejected", b)
              /\ UNCHANGED badAccept
      /\ UNCHANGED <<facts, native, runtime, alive, failed, prog, version,
                     revision, ownerCtr, qcur, icur, ckpt, epoch, everFailed,
                     ackWhileFailed, readPoisonLive>>

(***************************************************************************)
(* Liveness observation and child death                                    *)
(***************************************************************************)
\* Spontaneous child death (not yet observed by the backend).
ChildCrash(b) ==
    /\ runtime[b] # 0 /\ alive[b]
    /\ alive' = [alive EXCEPT ![b] = FALSE]
    /\ Label("ChildCrash", b)
    /\ UNCHANGED <<facts, native, runtime, failed, prog, version, revision,
                   ownerCtr, qcur, icur, ckpt, epoch, everFailed, Ghosts>>

\* Backend::observed_health, lib.rs:270-278 (called by worlds.rs:1049):
\* try_wait Ok(Some)/Err sets failed but leaves runtime = Some.
ObservedHealth(b) ==
    /\ runtime[b] # 0
    /\ ~alive[b] \/ TryWaitErr
    /\ failed' = [failed EXCEPT ![b] = TRUE]
    /\ everFailed' = [everFailed EXCEPT ![b] = TRUE]
    /\ Label("ObservedHealthFailed", b)
    /\ UNCHANGED <<facts, native, runtime, alive, prog, version, revision,
                   ownerCtr, qcur, icur, ckpt, epoch, Ghosts>>

(***************************************************************************)
(* Checkpoints: checkpoint.rs                                              *)
(***************************************************************************)
\* checkpoint_bytes / save_checkpoint (checkpoint.rs:169-239): pure schema
\* (170) and export_inputs => health ready (175, lib.rs:567). Captures the
\* retained facts, not the native state. `consistent` is a ghost field.
SaveCheckpoint(b) ==
    /\ prog[b] \notin ImpurePrograms
    /\ Health(b) = "ready"
    /\ ckpt' = [present |-> TRUE, facts |-> facts[b], rev |-> revision[b],
                ver |-> version[b], prog |-> prog[b],
                consistent |-> (facts[b] = native[b])]
    /\ Label("SaveCheckpoint", b)
    /\ UNCHANGED <<facts, native, runtime, alive, failed, prog, version,
                   revision, ownerCtr, qcur, icur, epoch, everFailed, Ghosts>>

RestoreGuard(b) ==  \* checkpoint.rs:263-265
    /\ ckpt.present
    /\ Health(b) = "uninitialized" /\ version[b] = 0 /\ facts[b] = {}

\* Integrity/validation failure (checkpoint.rs:269-270) or candidate
\* compile/replay failure (276-280): self is untouched.
RestoreFail(b) ==
    /\ RestoreGuard(b)
    /\ Label("RestoreFail", b)
    /\ UNCHANGED <<facts, native, runtime, alive, failed, prog, version,
                   revision, ownerCtr, qcur, icur, ckpt, epoch, everFailed>>
    /\ UNCHANGED Ghosts

\* Candidate backend replays the checkpoint facts, then revision/version are
\* overwritten from the checkpoint and *self = candidate (checkpoint.rs:272-283).
RestoreOk(b) ==
    /\ RestoreGuard(b)
    /\ ownerCtr < MaxOwner
    /\ ownerCtr' = ownerCtr + 1
    /\ facts' = [facts EXCEPT ![b] = ckpt.facts]
    /\ native' = [native EXCEPT ![b] = ckpt.facts]
    /\ runtime' = [runtime EXCEPT ![b] = ownerCtr + 1]
    /\ alive' = [alive EXCEPT ![b] = TRUE]
    /\ failed' = [failed EXCEPT ![b] = FALSE]
    /\ prog' = [prog EXCEPT ![b] = ckpt.prog]
    /\ version' = [version EXCEPT ![b] = ckpt.ver]
    /\ revision' = [revision EXCEPT ![b] = ckpt.rev]
    /\ epoch' = [epoch EXCEPT ![b] = @ + 1]
    /\ Label("RestoreOk", b)
    /\ UNCHANGED <<qcur, icur, ckpt, everFailed, Ghosts>>

(***************************************************************************)
Next ==
    \E b \in Backends :
        \/ \E p \in Programs :
              InstallFailEarly(b, p) \/ InstallReplayFail(b, p) \/ InstallOk(b, p)
        \/ ApplyInvalid(b)
        \/ \E S \in SUBSET Facts, dump \in BOOLEAN :
              \/ ApplyCommitted(b, S, dump)
              \/ ApplyErrorText(b, S, dump)
              \/ ApplyIOBefore(b, S, dump)
              \/ ApplyIOAfter(b, S, dump)
        \/ Query(b) \/ BoundedRead(b) \/ InputRead(b)
        \/ UseQCursor(b) \/ UseICursor(b)
        \/ ChildCrash(b) \/ ObservedHealth(b)
        \/ SaveCheckpoint(b) \/ RestoreFail(b) \/ RestoreOk(b)

Spec == Init /\ [][Next]_vars

(***************************************************************************)
(* Safety properties (docs/specification.md, "Input changes", "Activation", *)
(* "Bounded reads", "Checkpoints").                                        *)
(***************************************************************************)
\* "acknowledged state changes only after completion": whenever the backend
\* reports ready, its retained inputs are what the live child committed.
RetainedMatchesNative ==
    \A b \in Backends : Health(b) = "ready" => facts[b] = native[b]

\* Continuations are accepted only from the same live owner with no
\* mutation, replacement or restore since issue.
CursorSound == ~badAccept

\* No input transaction is acknowledged by a backend that reports failed.
NoAckWhileFailed == ~ackWhileFailed

\* "uncertain native failure disables continued use" (hosted instances).
HostedPoisonSticky ==
    Hosted => \A b \in Backends : everFailed[b] => failed[b]

\* A read of a live child never disables the owner.
ReadsPreserveHealth == ~readPoisonLive

\* Checkpoints only from a ready, pure backend; they hold acknowledged
\* (= native-committed) inputs and a valid identity (checkpoint.rs:79-85).
CheckpointSound ==
    ckpt.present =>
        /\ ckpt.consistent
        /\ ckpt.prog \notin ImpurePrograms
        /\ ckpt.ver >= 1 /\ ckpt.rev >= ckpt.ver

VersionLeRevision == \A b \in Backends : version[b] <= revision[b]

\* ---- reachability witnesses (expected to be VIOLATED; sanity checks) --------
\* Only a restore can leave epoch < revision (it copies the checkpoint revision).
NeverRestored == \A b \in Backends : epoch[b] >= revision[b]
\* A checkpoint is written at some point.
NeverCheckpointed == ~ckpt.present

\* ---- action properties ----------------------------------------------------
RevisionMonotone ==
    [][\A b \in Backends : revision'[b] >= revision[b]]_vars

\* Revision moves only on a real native acknowledgement, a successful
\* replacement or a restore; apply/install bump it by exactly one.
RevisionOnlyOnAck ==
    [][\A b \in Backends :
         revision'[b] # revision[b] =>
            \/ /\ \E p \in Programs : InstallOk(b, p)
               /\ revision'[b] = revision[b] + 1
            \/ /\ \E S \in SUBSET Facts, d \in BOOLEAN : ApplyCommitted(b, S, d)
               /\ revision'[b] = revision[b] + 1
            \/ RestoreOk(b)]_vars

\* "failed replacement preserves the prior usable program".
FailedReplacementPreserves ==
    [][\A b \in Backends, p \in Programs :
         (InstallFailEarly(b, p) \/ InstallReplayFail(b, p)) =>
            UNCHANGED <<facts, native, runtime, alive, failed, prog, version,
                        revision, epoch>>]_vars

\* Restore reproduces the checkpointed acknowledged state in the fresh owner.
RestoreReproduces ==
    [][\A b \in Backends :
         RestoreOk(b) =>
            /\ facts'[b] = ckpt.facts /\ native'[b] = ckpt.facts
            /\ revision'[b] = ckpt.rev /\ version'[b] = ckpt.ver
            /\ prog'[b] = ckpt.prog /\ ~failed'[b] /\ runtime'[b] # 0]_vars
=============================================================================
