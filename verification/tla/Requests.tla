------------------------------ MODULE Requests ------------------------------
(***************************************************************************)
(* Model of the registered request (external inference) protocol:         *)
(*   src/operations.rs  AgentProgram::{install,submit,claim,complete}      *)
(*   src/instance.rs    ProgramInstance::execute guards (137-223)          *)
(*   src/lib.rs         Backend::apply_inner (460-522)                     *)
(*                                                                         *)
(* One ProgramInstance is modelled.  The host serializes execute() behind  *)
(* a Mutex (host.rs:188,280), so an operation is two steps -- native       *)
(* commit (Backend::apply), then the AgentProgram in-memory update -- and  *)
(* no other operation can interleave between them ("pending").  Worker     *)
(* provider calls happen outside the instance and do interleave.           *)
(*                                                                         *)
(* Native state is Backend.facts (the retained, acknowledged input facts;  *)
(* lib.rs:207).  A native failure (exchange error) kills the child and     *)
(* leaves Backend.facts unchanged (lib.rs:514-520): whether the child had  *)
(* committed before dying ("commit-then-die") or not ("die-before-commit") *)
(* is invisible afterwards, because the child's state dies with it.        *)
(*                                                                         *)
(* Request identity: <<entity, revision, payload>>; the operation name and *)
(* version are fixed per AgentProgram (operations.rs:135-142).             *)
(* Rejected requests change nothing and are modelled as disabled steps.    *)
(***************************************************************************)
EXTENDS Naturals, FiniteSets, TLC

CONSTANTS
    Entities, Revs, Payloads, Outs,
    Workers,        \* inference workers: claim, call provider, complete
    AllowRogue,     \* other same-user clients may complete arbitrary ids
    Mode,           \* "host" (instance_id set) or "standalone" (host.rs:138)
    MaxFailures,
    None

ASSUME Mode \in {"host", "standalone"}

Ids == Entities \X Revs \X Payloads
Ent(id) == id[1]
Rev(id) == id[2]
Pl(id)  == id[3]
NoPend == [kind |-> "none", id |-> None, who |-> None, out |-> None]
NoAck  == [id |-> None, out |-> None, fresh |-> FALSE, dup |-> FALSE]

VARIABLES
    \* Backend
    alive,     \* runtime is Some (lib.rs:205)
    failed,    \* Backend.failed (lib.rs:212)
    nIntent, nCur, nClaimed, nResp,   \* retained agent_* input facts
    \* AgentProgram (operations.rs:24-29), session-local
    memCur,    \* current: entity -> id
    known,     \* domain of requests
    claimed,   \* requests[id].claimed
    output,    \* requests[id].output
    pending,   \* native commit done, in-memory update not yet applied
    \* workers
    wst, wid, wout,
    \* ghosts
    failures, lastFailure, uncertain, claimsOk, provider, ack, deadAck

backend == <<alive, failed, nIntent, nCur, nClaimed, nResp>>
mem == <<memCur, known, claimed, output>>
workers == <<wst, wid, wout>>
vars == <<backend, mem, pending, workers, failures, lastFailure, uncertain,
          claimsOk, provider, ack, deadAck>>

\* ProgramInstance::execute rejects everything but status on a failed
\* instance, only when it has an instance id (instance.rs:152-158).
Guard == Mode = "standalone" \/ ~failed
Idle == pending = NoPend

Init ==
    /\ alive = TRUE /\ failed = FALSE       \* program installed (85-108)
    /\ nIntent = {} /\ nCur = {} /\ nClaimed = {} /\ nResp = {}
    /\ memCur = [e \in Entities |-> None]
    /\ known = {} /\ claimed = {}
    /\ output = [id \in Ids |-> None]
    /\ pending = NoPend
    /\ wst = [k \in Workers |-> "idle"]
    /\ wid = [k \in Workers |-> None]
    /\ wout = [k \in Workers |-> None]
    /\ failures = 0 /\ lastFailure = "none"
    /\ uncertain = {}
    /\ claimsOk = [id \in Ids |-> 0]
    /\ provider = [id \in Ids |-> 0]
    /\ ack = NoAck /\ deadAck = FALSE

\* Native exchange failure (lib.rs:514-520): child killed, facts unchanged.
Die(kind) ==
    /\ alive /\ failures < MaxFailures
    /\ alive' = FALSE /\ failed' = TRUE
    /\ failures' = failures + 1
    /\ lastFailure' = kind
    /\ UNCHANGED <<nIntent, nCur, nClaimed, nResp>>
DieAny == Die("commit-then-die") \/ Die("die-before-commit")

-----------------------------------------------------------------------------
(* submit (operations.rs:111-166)                                           *)
SubmitAdmissible(e, r, p) ==
    LET old == memCur[e] IN
      IF old = None THEN TRUE
      ELSE /\ r >= Rev(old)                         \* 128-130
           /\ (r = Rev(old) => p = Pl(old))         \* 131-133

Submit(e, r, p) ==
    LET id == <<e, r, p>>
        old == memCur[e] IN
    /\ Idle /\ Guard /\ alive                  \* apply needs a runtime (461)
    /\ SubmitAdmissible(e, r, p)
    /\ \/ /\ nIntent' = nIntent \cup {id}      \* 143-154, one transaction
          /\ nCur' = (IF old # None THEN nCur \ {<<e, old>>} ELSE nCur)
                       \cup {<<e, id>>}
          /\ pending' = [NoPend EXCEPT !.kind = "submit", !.id = id]
          /\ UNCHANGED <<alive, failed, nClaimed, nResp, failures, lastFailure, uncertain>>
       \/ /\ DieAny /\ UNCHANGED <<pending, uncertain>>
    /\ UNCHANGED <<mem, workers, claimsOk, provider, deadAck>>
    /\ ack' = NoAck

SubmitMem ==                                   \* 155-162
    /\ pending.kind = "submit"
    /\ memCur' = [memCur EXCEPT ![Ent(pending.id)] = pending.id]
    /\ known' = known \cup {pending.id}        \* or_insert keeps old state
    /\ pending' = NoPend
    /\ UNCHANGED <<backend, claimed, output, workers, failures, lastFailure,
                   uncertain, claimsOk, provider, deadAck>>
    /\ ack' = NoAck

-----------------------------------------------------------------------------
(* claim (operations.rs:167-183), by a worker                               *)
Claim(k, id) ==
    /\ Idle /\ Guard /\ wst[k] = "idle"
    /\ alive                                   \* 168-170
    /\ id \in known                            \* 171
    /\ memCur[Ent(id)] = id                    \* 172-174 freshness
    /\ id \notin claimed /\ output[id] = None  \* 175-177
    /\ \/ /\ nClaimed' = nClaimed \cup {id}    \* 178
          /\ pending' = [NoPend EXCEPT !.kind = "claim", !.id = id, !.who = k]
          /\ UNCHANGED <<alive, failed, nIntent, nCur, nResp, failures, lastFailure>>
       \/ /\ DieAny /\ UNCHANGED pending
    /\ UNCHANGED <<mem, workers, uncertain, claimsOk, provider, deadAck>>
    /\ ack' = NoAck

ClaimMem ==                                    \* 179
    /\ pending.kind = "claim"
    /\ claimed' = claimed \cup {pending.id}
    /\ claimsOk' = [claimsOk EXCEPT ![pending.id] = @ + 1]
    /\ wst' = [wst EXCEPT ![pending.who] = "calling"]
    /\ wid' = [wid EXCEPT ![pending.who] = pending.id]
    /\ pending' = NoPend
    /\ UNCHANGED <<backend, memCur, known, output, wout, failures, lastFailure,
                   uncertain, provider, deadAck>>
    /\ ack' = NoAck

\* The provider call runs outside DDlog, after an explicit claim.
Provider(k) ==
    /\ wst[k] = "calling"
    /\ \E o \in Outs : wout' = [wout EXCEPT ![k] = o]
    /\ provider' = [provider EXCEPT ![wid[k]] = @ + 1]
    /\ wst' = [wst EXCEPT ![k] = "settling"]
    /\ UNCHANGED <<backend, mem, pending, wid, failures, lastFailure, uncertain,
                   claimsOk, deadAck>>
    /\ ack' = NoAck

-----------------------------------------------------------------------------
(* complete (operations.rs:184-204); who is a worker or "env"               *)
Settled(who) == IF who \in Workers THEN [wst EXCEPT ![who] = "done"] ELSE wst

CompleteOp(who, id, out) ==
    /\ Idle /\ Guard
    /\ id \in known                            \* 186
    /\ id \in claimed                          \* 187-189
    /\ IF output[id] # None
         THEN \* identical duplicate: acknowledged, no backend call (190-197);
              \* a conflicting one is rejected (disabled here).
              /\ output[id] = out
              /\ ack' = [id |-> id, out |-> out, dup |-> TRUE,
                         fresh |-> memCur[Ent(id)] = id]
              /\ deadAck' = (deadAck \/ ~alive)
              /\ wst' = Settled(who)
              /\ UNCHANGED <<backend, pending, failures, lastFailure, uncertain>>
         ELSE /\ alive                         \* apply: "Install a program first"
              /\ \/ /\ nResp' = nResp \cup {<<id, out>>}   \* 198-199
                    /\ pending' = [kind |-> "complete", id |-> id, who |-> who, out |-> out]
                    /\ UNCHANGED <<alive, failed, nIntent, nCur, nClaimed,
                                   failures, lastFailure, uncertain, wst>>
                 \/ /\ DieAny                  \* outcome uncertain
                    /\ uncertain' = uncertain \cup {id}
                    /\ UNCHANGED <<pending, wst>>  \* worker retains its result
              /\ ack' = NoAck
              /\ UNCHANGED deadAck
    /\ UNCHANGED <<mem, wid, wout, claimsOk, provider>>

CompleteMem ==                                 \* 200-203
    /\ pending.kind = "complete"
    /\ output' = [output EXCEPT ![pending.id] = pending.out]
    /\ ack' = [id |-> pending.id, out |-> pending.out, dup |-> FALSE,
               fresh |-> memCur[Ent(pending.id)] = pending.id]
    /\ wst' = Settled(pending.who)
    /\ pending' = NoPend
    /\ UNCHANGED <<backend, memCur, known, claimed, wid, wout, failures,
                   lastFailure, uncertain, claimsOk, provider, deadAck>>

\* A worker settles (possibly late, after its request became stale), and
\* may resend an identical settlement after a lost acknowledgement.
WorkerComplete(k) ==
    /\ wst[k] \in {"settling", "done"}
    /\ CompleteOp(k, wid[k], wout[k])

RogueComplete ==
    /\ AllowRogue
    /\ \E id \in Ids, o \in Outs : CompleteOp("env", id, o)

WorkerForget(k) ==
    /\ wst[k] \in {"settling", "done"}
    /\ wst' = [wst EXCEPT ![k] = "idle"]
    /\ wid' = [wid EXCEPT ![k] = None]
    /\ wout' = [wout EXCEPT ![k] = None]
    /\ UNCHANGED <<backend, mem, pending, failures, lastFailure, uncertain,
                   claimsOk, provider, deadAck>>
    /\ ack' = NoAck

-----------------------------------------------------------------------------
(* Reinstall inside the same instance: install_agent_program requires an   *)
(* empty input session (operations.rs:95-97); lemmalog_install_rules and   *)
(* processor_install are refused once an agent exists (instance.rs:159-166,*)
(* 221-223, 499-501).  Only the empty-facts path remains.                   *)
Reinstall ==
    /\ Idle /\ Guard
    /\ nIntent = {} /\ nCur = {} /\ nClaimed = {} /\ nResp = {}
    /\ alive' = TRUE /\ failed' = FALSE
    /\ memCur' = [e \in Entities |-> None]
    /\ known' = {} /\ claimed' = {} /\ output' = [id \in Ids |-> None]
    /\ UNCHANGED <<nIntent, nCur, nClaimed, nResp, pending, workers, failures,
                   lastFailure, uncertain, claimsOk, provider, deadAck>>
    /\ ack' = NoAck

Next ==
    \/ \E e \in Entities, r \in Revs, p \in Payloads : Submit(e, r, p)
    \/ SubmitMem
    \/ \E k \in Workers, id \in Ids : Claim(k, id)
    \/ ClaimMem
    \/ \E k \in Workers : Provider(k) \/ WorkerComplete(k) \/ WorkerForget(k)
    \/ RogueComplete
    \/ CompleteMem
    \/ Reinstall

Spec == Init /\ [][Next]_vars

\* State-space reduction for TLC (VIEW): an idle worker's stale wid/wout are
\* never read again, and lastFailure is a label only.
View ==
    <<backend, mem, pending, wst,
      [k \in Workers |-> IF wst[k] = "idle" THEN <<>> ELSE <<wid[k], wout[k]>>],
      failures, uncertain, claimsOk, provider, ack, deadAck>>

-----------------------------------------------------------------------------
(* Derived native view: agent_result (operations.rs:81).                    *)
AgentResult ==
    { <<Ent(x[1]), Rev(x[1]), x[2]>> :
        x \in { y \in nResp : y[1] \in nIntent /\ <<Ent(y[1]), y[1]>> \in nCur } }

(* Properties.                                                              *)
AtMostOneOutputPerRequest ==
    \A id \in Ids : Cardinality({o \in Outs : <<id, o>> \in nResp}) <= 1

NoCompletionWithoutClaim ==
    /\ \A id \in Ids, o \in Outs : <<id, o>> \in nResp => id \in nClaimed
    /\ \A id \in Ids : output[id] # None => id \in claimed

\* A claim is only ever admitted for the entity's current request.
ClaimOnlyFresh ==
    [][\A id \in Ids : (id \notin nClaimed /\ id \in nClaimed')
                         => <<Ent(id), id>> \in nCur]_vars

\* The fresh flag returned by complete (fresh or duplicate) says exactly
\* whether this output is what agent_result now shows.
FreshFlagMatchesAgentResult ==
    ack # NoAck =>
      (ack.fresh <=> <<Ent(ack.id), Rev(ack.id), ack.out>> \in AgentResult)

\* agent_result only shows the current request's recorded output, at most
\* one row per entity (checked between operations).
ResultOnlyCurrent ==
    Idle =>
      \A row \in AgentResult :
        LET c == memCur[row[1]] IN
          c # None /\ Rev(c) = row[2] /\ output[c] = row[3]

\* Everything the AgentProgram records was acknowledged natively first.
\* (memCur may lag behind nCur only during a pending submit, whose native
\* transaction already replaced the old agent_current fact.)
InMemoryNeverAheadOfNative ==
    /\ known \subseteq nIntent
    /\ pending.kind # "submit" =>
         \A e \in Entities : memCur[e] # None => <<e, memCur[e]>> \in nCur
    /\ claimed \subseteq nClaimed
    /\ \A id \in Ids : output[id] # None => <<id, output[id]>> \in nResp

\* Between operations -- including after any native failure -- the
\* in-memory AgentProgram and the retained native facts agree exactly.
MemMatchesNativeBetweenOps ==
    Idle =>
      /\ known = nIntent
      /\ nCur = {<<e, memCur[e]>> : e \in {e2 \in Entities : memCur[e2] # None}}
      /\ claimed = nClaimed
      /\ nResp = {<<id, output[id]>> : id \in {i \in Ids : output[i] # None}}

\* Uncertain settlement never implies permission to repeat provider work:
\* a request is claimed (and sent to the provider) at most once per instance.
UncertainNeverReclaimed ==
    \A id \in Ids : claimsOk[id] <= 1 /\ provider[id] <= 1
UncertainStaysClaimed ==
    \A id \in uncertain : id \in claimed /\ claimsOk[id] = 1

\* C8: only the worker that claimed a request settles it natively (the
\* runtime has no claimant binding: operations.rs:184-199).
OnlyClaimantSettles == pending.kind = "complete" => pending.who \in Workers

\* C5: no settlement is acknowledged while the runtime is dead.
NoAckWhileRuntimeDead == ~deadAck

\* Non-vacuity witnesses: each is expected to be VIOLATED (reachable).
WitnessNoLateStaleAck == ~(ack # NoAck /\ ~ack.fresh /\ ~ack.dup)
WitnessNoDuplicateAck == ~ack.dup
WitnessNoUncertain == uncertain = {}
WitnessNoLateResultStored ==   \* a stale response is retained natively
    ~\E id \in Ids, o \in Outs : <<id, o>> \in nResp /\ <<Ent(id), id>> \notin nCur
=============================================================================
