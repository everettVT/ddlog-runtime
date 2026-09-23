------------------------------ MODULE Registry ------------------------------
(***************************************************************************)
(* Model of the durable processor registry in src/registry.rs.            *)
(*                                                                         *)
(* Every file operation that the Rust code performs as one atomic_json     *)
(* call (temp file + fsync + rename/hard_link + dir fsync, 1411-1432) is   *)
(* one TLA+ step.  Writers serialize through the create-new lock file      *)
(* .update.lock (1434-1463).  The lock is a plain file: acquisition fails  *)
(* immediately if it exists (1441-1447), and Drop deletes it by path       *)
(* without checking its owner (1455-1462).  A crashed writer leaves it.    *)
(*                                                                         *)
(* Abstractions:                                                           *)
(*  - A version is identified by its content value (content hash of the    *)
(*    definition, 1283-1304); a version file therefore can never change.   *)
(*  - lineage, provenance, compositions and hashing are not modelled.      *)
(*  - Clients read cur/lcPtr unlocked when choosing a request (as a caller *)
(*    of processor_get/list would), so stale expectations arise from       *)
(*    interleaving rather than from arbitrary guesses.                     *)
(*  - Fork (278-297) has the same shape as Create (a fresh identity, one   *)
(*    version write, one pointer write) and is covered by Create.          *)
(*  - lifecycle_snapshot (543-584) taken UNDER the lock is treated as one  *)
(*    consistent read (no other writer can interleave while it holds).     *)
(***************************************************************************)
EXTENDS Naturals, Sequences, FiniteSets, TLC

CONSTANTS
    Writers,        \* processes issuing publish/archive/restore/create
    Importers,      \* processes running import_registry for ImportId
    Readers,        \* unlocked readers (lifecycle_snapshot + get(None))
    Vals,           \* definition contents (= versions)
    InitVal,        \* version of InitId at start
    InitId,         \* an existing processor
    ImportId,       \* a foreign processor that only import can bring in
    CreateIds,      \* identities that create/fork may draw (random ids)
    OpIds,          \* processors that publish/archive/restore may target
    ReadIds,        \* processors the unlocked readers read
    SrcV1, SrcV2,   \* source versions of ImportId, in import order (1251)
    SrcCur,         \* the source registry's current version of ImportId
    MaxRev,         \* bound on lifecycle revisions (model bound)
    MaxCrashes,     \* bound on process crashes (model bound)
    UnsafeOperator, \* TRUE: operator may delete the lock while a writer lives
    None

Ids == {InitId, ImportId} \cup CreateIds
SrcVers == <<SrcV1, SrcV2>>
Procs == Writers \cup Importers
NoEv == [st |-> "none", ver |-> None]
NoOp == [kind |-> "none", p |-> InitId, v |-> None, exp |-> None,
         expR |-> 0, st |-> "none", new |-> FALSE, i |-> 0]

VARIABLES
    exists,   \* processor directory exists
    versions, \* versions/<hash>.json files present (grow-only)
    cur,      \* current.json (None = absent)
    lcPtr,    \* lifecycle/current.json revision (0 = absent, implicit active)
    lcEv,     \* lifecycle/revisions/<r>.json events
    lock,     \* .update.lock exists
    pc, op, obsCur, obsRev,  \* per-process control state
    crashes,
    clobbers, \* ghost: pointer writes over a value the writer did not observe
    rpc, rp, rc, rr, rst, rever, rget, seen  \* reader state

regvars == <<exists, versions, cur, lcPtr, lcEv>>
procvars == <<pc, op, obsCur, obsRev>>
readvars == <<rpc, rp, rc, rr, rst, rever, rget>>
vars == <<regvars, lock, procvars, crashes, clobbers, readvars, seen>>

Status(p) == IF lcPtr[p] = 0 THEN "active" ELSE lcEv[p][lcPtr[p]].st
Pair(p) == <<cur[p], Status(p)>>

\* A pointer write replacing value `now` by `new` loses an update when the
\* writer did not observe `now` (under the lock, or by writing it itself)
\* and the write actually changes the pointer.
Clobber(now, observed, new) == now # observed /\ now # new

\* A process holds the lock (believes it does) in these control states.
Holds(w) == pc[w] \notin {"idle", "acq"}

\* lifecycle_snapshot succeeds (543-584) and the processor is readable.
SnapOK(p) == /\ exists[p] /\ cur[p] # None
             /\ ~(Status(p) = "archived" /\ lcEv[p][lcPtr[p]].ver # cur[p])

\* Reader-history tracking: each action records the post-state pair of the
\* processor a busy reader is reading, so a reader's result can be checked
\* against every state that existed during its read interval.
StatusNext(p) == IF lcPtr'[p] = 0 THEN "active" ELSE lcEv'[p][lcPtr'[p]].st
Track == seen' = [r \in Readers |->
                    IF rpc'[r] = "idle" THEN {}
                    ELSE seen[r] \cup {<<cur'[rp'[r]], StatusNext(rp'[r])>>}]

Init ==
    /\ exists = [p \in Ids |-> p = InitId]
    /\ versions = [p \in Ids |-> IF p = InitId THEN {InitVal} ELSE {}]
    /\ cur = [p \in Ids |-> IF p = InitId THEN InitVal ELSE None]
    /\ lcPtr = [p \in Ids |-> 0]
    /\ lcEv = [p \in Ids |-> [r \in 1..MaxRev |-> NoEv]]
    /\ lock = FALSE
    /\ pc = [w \in Procs |-> "idle"]
    /\ op = [w \in Procs |-> NoOp]
    /\ obsCur = [w \in Procs |-> None]
    /\ obsRev = [w \in Procs |-> 0]
    /\ crashes = 0
    /\ clobbers = {}
    /\ rpc = [r \in Readers |-> "idle"]
    /\ rp = [r \in Readers |-> InitId]
    /\ rc = [r \in Readers |-> None]
    /\ rr = [r \in Readers |-> 0]
    /\ rst = [r \in Readers |-> "active"]
    /\ rever = [r \in Readers |-> None]
    /\ rget = [r \in Readers |-> None]
    /\ seen = [r \in Readers |-> {}]

-----------------------------------------------------------------------------
(* Writers: choose a request from an unlocked client read.                  *)

ChoosePublish(w) ==   \* processor_publish(processor_id, definition, expected)
    /\ pc[w] = "idle"
    /\ \E p \in OpIds, v \in Vals :
         /\ exists[p] /\ cur[p] # None
         /\ op' = [op EXCEPT ![w] =
                    [NoOp EXCEPT !.kind = "pub", !.p = p, !.v = v, !.exp = cur[p]]]
    /\ pc' = [pc EXCEPT ![w] = "acq"]
    /\ UNCHANGED <<regvars, lock, obsCur, obsRev, crashes, clobbers>>

ChooseTransition(w) ==  \* processor_archive / processor_restore (336-362)
    /\ pc[w] = "idle"
    /\ \E p \in OpIds, st \in {"active", "archived"} :
         /\ exists[p] /\ cur[p] # None
         /\ op' = [op EXCEPT ![w] =
                    [NoOp EXCEPT !.kind = "trans", !.p = p, !.exp = cur[p],
                                 !.expR = lcPtr[p], !.st = st]]
    /\ pc' = [pc EXCEPT ![w] = "acq"]
    /\ UNCHANGED <<regvars, lock, obsCur, obsRev, crashes, clobbers>>

ChooseCreate(w) ==  \* processor_create / processor_fork (199-220, 278-297)
    /\ pc[w] = "idle"
    /\ \E p \in CreateIds, v \in Vals :
         op' = [op EXCEPT ![w] = [NoOp EXCEPT !.kind = "create", !.p = p, !.v = v]]
    /\ pc' = [pc EXCEPT ![w] = "acq"]
    /\ UNCHANGED <<regvars, lock, obsCur, obsRev, crashes, clobbers>>

\* UpdateLock::acquire (1438-1453): create_new; fails at once if it exists.
Acquire(w) ==
    /\ pc[w] = "acq"
    /\ IF ~lock
         THEN /\ lock' = TRUE
              /\ pc' = [pc EXCEPT ![w] =
                          CASE op[w].kind = "pub"    -> "pchk"
                            [] op[w].kind = "trans"  -> "tchk"
                            [] op[w].kind = "create" -> "cdir"
                            [] op[w].kind = "imp"    -> "imp"]
         ELSE /\ UNCHANGED lock
              /\ pc' = [pc EXCEPT ![w] = "idle"]  \* "update lock exists"
    /\ UNCHANGED <<regvars, op, obsCur, obsRev, crashes, clobbers>>

\* publish_versioned under the lock (256-268): ensure_active, then CAS.
PubCheck(w) ==
    /\ pc[w] = "pchk"
    /\ LET p == op[w].p IN
         IF SnapOK(p) /\ Status(p) = "active" /\ cur[p] = op[w].exp
           THEN /\ obsCur' = [obsCur EXCEPT ![w] = cur[p]]
                /\ pc' = [pc EXCEPT ![w] = "wver"]
           ELSE /\ UNCHANGED obsCur
                /\ pc' = [pc EXCEPT ![w] = "rel"]   \* conflict/archived
    /\ UNCHANGED <<regvars, lock, op, obsRev, crashes, clobbers>>

\* create_locked (991-999): create_new of the identity directory.
CreateDir(w) ==
    /\ pc[w] = "cdir"
    /\ LET p == op[w].p IN
         IF ~exists[p]
           THEN /\ exists' = [exists EXCEPT ![p] = TRUE]
                /\ obsCur' = [obsCur EXCEPT ![w] = None]
                /\ pc' = [pc EXCEPT ![w] = "wver"]
           ELSE /\ UNCHANGED <<exists, obsCur>>
                /\ pc' = [pc EXCEPT ![w] = "rel"]   \* collision fails closed
    /\ UNCHANGED <<versions, cur, lcPtr, lcEv, lock, op, obsRev, crashes, clobbers>>

\* publish_locked (1011-1039): write the version file if absent (no replace).
WriteVersion(w) ==
    /\ pc[w] = "wver"
    /\ versions' = [versions EXCEPT ![op[w].p] = @ \cup {op[w].v}]
    /\ pc' = [pc EXCEPT ![w] = "wcur"]
    /\ UNCHANGED <<exists, cur, lcPtr, lcEv, lock, op, obsCur, obsRev, crashes, clobbers>>

\* publish_locked (1040-1049): unconditional rename over current.json.
WriteCur(w) ==
    /\ pc[w] = "wcur"
    /\ LET p == op[w].p IN
         /\ cur' = [cur EXCEPT ![p] = op[w].v]
         /\ clobbers' = IF Clobber(cur[p], obsCur[w], op[w].v)
                          THEN clobbers \cup {<<w, p, "cur">>} ELSE clobbers
    /\ obsCur' = [obsCur EXCEPT ![w] = op[w].v]
    /\ pc' = [pc EXCEPT ![w] = "rel"]
    /\ UNCHANGED <<exists, versions, lcPtr, lcEv, lock, op, obsRev, crashes>>

\* transition under the lock (376-409).
TransCheck(w) ==
    /\ pc[w] = "tchk"
    /\ LET p == op[w].p
           r == lcPtr[p] + 1 IN
         IF /\ SnapOK(p)
            /\ cur[p] = op[w].exp            \* 377-379 version CAS
            /\ lcPtr[p] = op[w].expR         \* 380-382 revision CAS
            /\ Status(p) # op[w].st          \* 383-385 same-target no-op
            /\ r <= MaxRev                   \* model bound (overflow 389)
            /\ lcEv[p][r] = NoEv             \* 406-409 uncommitted event
           THEN /\ obsCur' = [obsCur EXCEPT ![w] = cur[p]]
                /\ obsRev' = [obsRev EXCEPT ![w] = lcPtr[p]]
                /\ pc' = [pc EXCEPT ![w] = "wev"]
           ELSE /\ UNCHANGED <<obsCur, obsRev>>
                /\ pc' = [pc EXCEPT ![w] = "rel"]
    /\ UNCHANGED <<regvars, lock, op, crashes, clobbers>>

\* 412: event file via hard_link (fails if it already exists).
WriteEvent(w) ==
    /\ pc[w] = "wev"
    /\ LET p == op[w].p
           r == obsRev[w] + 1 IN
         IF lcEv[p][r] = NoEv
           THEN /\ lcEv' = [lcEv EXCEPT ![p][r] = [st |-> op[w].st, ver |-> obsCur[w]]]
                /\ pc' = [pc EXCEPT ![w] = "wptr"]
           ELSE /\ UNCHANGED lcEv
                /\ pc' = [pc EXCEPT ![w] = "rel"]
    /\ UNCHANGED <<exists, versions, cur, lcPtr, lock, op, obsCur, obsRev, crashes, clobbers>>

\* 413-421: rename over lifecycle/current.json.
WritePtr(w) ==
    /\ pc[w] = "wptr"
    /\ LET p == op[w].p IN
         /\ lcPtr' = [lcPtr EXCEPT ![p] = obsRev[w] + 1]
         /\ clobbers' = IF lcPtr[p] # obsRev[w]
                          THEN clobbers \cup {<<w, p, "rev">>} ELSE clobbers
    /\ pc' = [pc EXCEPT ![w] = "rel"]
    /\ UNCHANGED <<exists, versions, cur, lcEv, lock, op, obsCur, obsRev, crashes>>

\* Drop for UpdateLock (1455-1462): remove the path, whoever created it.
Release(w) ==
    /\ pc[w] = "rel"
    /\ lock' = FALSE
    /\ pc' = [pc EXCEPT ![w] = "idle"]
    /\ UNCHANGED <<regvars, op, obsCur, obsRev, crashes, clobbers>>

-----------------------------------------------------------------------------
(* import_registry for one foreign processor (812-959).                     *)

\* 905-910: new_processors decided BEFORE the lock is taken (922).
DecideNew(w) ==
    /\ pc[w] = "idle"
    /\ op' = [op EXCEPT ![w] = [NoOp EXCEPT !.kind = "imp", !.p = ImportId,
                                  !.new = (cur[ImportId] = None), !.i = 1]]
    /\ obsCur' = [obsCur EXCEPT ![w] = cur[ImportId]]
    /\ pc' = [pc EXCEPT ![w] = "acq"]
    /\ UNCHANGED <<regvars, lock, obsRev, crashes, clobbers>>

\* import_locked (766-805) for SrcVers[i], under the lock (923-943).
ImportRecord(w) ==
    /\ pc[w] = "imp"
    /\ LET p == ImportId
           v == SrcVers[op[w].i]
           newHere == cur[p] = None           \* 780-784, checked under lock
       IN
         /\ versions' = [versions EXCEPT ![p] = @ \cup {v}]
         /\ IF newHere
              THEN /\ exists' = [exists EXCEPT ![p] = TRUE]
                   /\ cur' = [cur EXCEPT ![p] = v]          \* 792-803
                   /\ obsCur' = [obsCur EXCEPT ![w] = v]
              ELSE UNCHANGED <<exists, cur, obsCur>>
         /\ op' = [op EXCEPT ![w].i = @ + 1]
         /\ pc' = [pc EXCEPT ![w] =
                     IF op[w].i = Len(SrcVers) THEN "icommit" ELSE "imp"]
    /\ UNCHANGED <<lcPtr, lcEv, lock, obsRev, crashes, clobbers>>

\* 944-956: unconditional current.json write for every "new" processor.
ImportCommit(w) ==
    /\ pc[w] = "icommit"
    /\ LET p == ImportId IN
         IF op[w].new
           THEN /\ cur' = [cur EXCEPT ![p] = SrcCur]
                /\ exists' = [exists EXCEPT ![p] = TRUE]
                /\ clobbers' = IF Clobber(cur[p], obsCur[w], SrcCur)
                                 THEN clobbers \cup {<<w, p, "cur">>} ELSE clobbers
                /\ obsCur' = [obsCur EXCEPT ![w] = SrcCur]
           ELSE UNCHANGED <<cur, exists, clobbers, obsCur>>
    /\ pc' = [pc EXCEPT ![w] = "rel"]
    /\ UNCHANGED <<versions, lcPtr, lcEv, lock, op, obsRev, crashes>>

-----------------------------------------------------------------------------
(* Crashes and the operator.                                                *)

\* A process dies at any point; a held lock file stays behind (4-5). The
\* process identity restarts as a fresh process.
Crash(w) ==
    /\ pc[w] # "idle"
    /\ crashes < MaxCrashes
    /\ crashes' = crashes + 1
    /\ pc' = [pc EXCEPT ![w] = "idle"]
    /\ UNCHANGED <<regvars, lock, op, obsCur, obsRev, clobbers>>

\* Operator reconciliation: deletes a lock file.  The header (4-5) requires
\* the operator to establish writer absence first; UnsafeOperator drops that.
OperatorDeleteLock ==
    /\ lock
    /\ UnsafeOperator \/ \A w \in Procs : ~Holds(w)
    /\ lock' = FALSE
    /\ UNCHANGED <<regvars, procvars, crashes, clobbers>>

-----------------------------------------------------------------------------
(* Unlocked readers: lifecycle_snapshot (543-584), then get(None)'s second *)
(* current() read (302-311).                                                *)

RStep(r) ==
    /\ UNCHANGED <<regvars, lock, procvars, crashes, clobbers>>
    /\ \/ /\ rpc[r] = "idle"
          /\ \E p \in ReadIds : exists[p] /\ cur[p] # None /\ rp' = [rp EXCEPT ![r] = p]
          /\ rpc' = [rpc EXCEPT ![r] = "c1"]
          /\ UNCHANGED <<rc, rr, rst, rever, rget>>
       \/ /\ rpc[r] = "c1"                                   \* 544
          /\ rc' = [rc EXCEPT ![r] = cur[rp[r]]]
          /\ rpc' = [rpc EXCEPT ![r] = "p1"]
          /\ UNCHANGED <<rp, rr, rst, rever, rget>>
       \/ /\ rpc[r] = "p1"                                   \* 545
          /\ rr' = [rr EXCEPT ![r] = lcPtr[rp[r]]]
          /\ rpc' = [rpc EXCEPT ![r] = "ev"]
          /\ UNCHANGED <<rp, rc, rst, rever, rget>>
       \/ /\ rpc[r] = "ev"                                   \* 546-571
          /\ rst' = [rst EXCEPT ![r] =
                       IF rr[r] = 0 THEN "active" ELSE lcEv[rp[r]][rr[r]].st]
          /\ rever' = [rever EXCEPT ![r] =
                       IF rr[r] = 0 THEN rc[r] ELSE lcEv[rp[r]][rr[r]].ver]
          /\ rpc' = [rpc EXCEPT ![r] = "c2"]
          /\ UNCHANGED <<rp, rc, rr, rget>>
       \/ /\ rpc[r] = "c2"                                   \* 572
          /\ rpc' = [rpc EXCEPT ![r] = IF cur[rp[r]] = rc[r] THEN "p2" ELSE "idle"]
          /\ UNCHANGED <<rp, rc, rr, rst, rever, rget>>
       \/ /\ rpc[r] = "p2"                                   \* 573-579
          /\ rpc' = [rpc EXCEPT ![r] =
                       IF lcPtr[rp[r]] # rr[r] THEN "idle"
                       ELSE IF rst[r] = "archived" /\ rever[r] # rc[r] THEN "idle"
                       ELSE "snap"]
          /\ UNCHANGED <<rp, rc, rr, rst, rever, rget>>
       \/ /\ rpc[r] = "snap"         \* get(None): archived -> error (303, 330)
          /\ rpc' = [rpc EXCEPT ![r] = IF rst[r] = "active" THEN "g" ELSE "idle"]
          /\ UNCHANGED <<rp, rc, rr, rst, rever, rget>>
       \/ /\ rpc[r] = "g"                                    \* 304
          /\ rget' = [rget EXCEPT ![r] = cur[rp[r]]]
          /\ rpc' = [rpc EXCEPT ![r] = "gdone"]
          /\ UNCHANGED <<rp, rc, rr, rst, rever>>
       \/ /\ rpc[r] = "gdone"
          /\ rpc' = [rpc EXCEPT ![r] = "idle"]
          /\ UNCHANGED <<rp, rc, rr, rst, rever, rget>>

-----------------------------------------------------------------------------
WriterStep(w) ==
    \/ ChoosePublish(w) \/ ChooseTransition(w) \/ ChooseCreate(w)
    \/ Acquire(w) \/ PubCheck(w) \/ CreateDir(w) \/ WriteVersion(w)
    \/ WriteCur(w) \/ TransCheck(w) \/ WriteEvent(w) \/ WritePtr(w)
    \/ Release(w)

ImporterStep(w) ==
    \/ DecideNew(w) \/ Acquire(w) \/ ImportRecord(w) \/ ImportCommit(w)
    \/ Release(w)

SysStep ==
    \/ \E w \in Writers : WriterStep(w)
    \/ \E w \in Importers : ImporterStep(w)
    \/ \E w \in Procs : Crash(w)
    \/ OperatorDeleteLock

Next ==
    \/ SysStep /\ UNCHANGED readvars /\ Track
    \/ \E r \in Readers : RStep(r) /\ Track

Spec == Init /\ [][Next]_vars

\* State-space reduction for TLC (VIEW): values left behind in the control
\* variables of an idle process or reader are never read again (every path
\* out of "idle" overwrites them first), so they are dropped from state
\* fingerprints.  This is a sound bisimulation quotient.
WriterSymmetry == Permutations(Writers) \cup Permutations(Importers)

View ==
    <<regvars, lock, pc,
      [w \in Procs |-> IF pc[w] = "idle" THEN <<>> ELSE <<op[w], obsCur[w], obsRev[w]>>],
      crashes, clobbers, rpc,
      [r \in Readers |-> IF rpc[r] = "idle" THEN <<>>
                         ELSE <<rp[r], rc[r], rr[r], rst[r], rever[r], rget[r], seen[r]>>]>>

-----------------------------------------------------------------------------
(* Properties.                                                              *)

\* Every pointer write (current.json or lifecycle/current.json) replaces
\* exactly the value its writer observed under the lock (or wrote itself).
NoLostUpdate == clobbers = {}

CurAlwaysPointsToExistingVersion ==
    \A p \in Ids : cur[p] # None => cur[p] \in versions[p]

\* The invariant lifecycle_snapshot enforces at registry.rs:577-579.
ArchivedEventMatchesCur ==
    \A p \in Ids : Status(p) = "archived" => lcEv[p][lcPtr[p]].ver = cur[p]

\* A reader's snapshot / get result equals the real state at some instant
\* of its read interval.
SnapshotLinearizable ==
    \A r \in Readers : rpc[r] \in {"snap", "g", "gdone"} => <<rc[r], rst[r]>> \in seen[r]
GetLinearizable ==
    \A r \in Readers : rpc[r] = "gdone" => <<rget[r], "active">> \in seen[r]

VersionsImmutable ==
    [][\A p \in Ids :
         /\ versions[p] \subseteq versions'[p]
         /\ \A r \in 1..MaxRev : lcEv[p][r] # NoEv => lcEv'[p][r] = lcEv[p][r]]_vars

\* C4: an event file written without its pointer blocks every later
\* transition of that processor forever (406-409).
\* An orphan is an event file for revision lcPtr+1 that no live process is
\* about to commit (its writer crashed between 412 and 413).
Orphan(p) == /\ lcPtr[p] < MaxRev
             /\ lcEv[p][lcPtr[p] + 1] # NoEv
             /\ \A w \in Procs : ~(pc[w] = "wptr" /\ op[w].p = p)
OrphanBlocksTransitions ==
    [][\A p \in Ids : Orphan(p) => (lcPtr'[p] = lcPtr[p] /\ Orphan(p)')]_vars
NoOrphanEvent == \A p \in Ids : ~Orphan(p)   \* violated: shows C4 is reachable

\* Liveness form of C4: with weakly fair writers that keep issuing requests
\* from fresh reads, an orphaned event should eventually be resolved by a
\* committed transition.  Expected to FAIL (the orphan is permanent).
WriterNext(w) == WriterStep(w) /\ UNCHANGED readvars /\ Track
FairSpec == /\ Spec
            /\ \A w \in Writers : WF_vars(WriterNext(w))
            \* writers attempt archive/restore infinitely often
            /\ \A w \in Writers :
                 SF_vars(ChooseTransition(w) /\ UNCHANGED readvars /\ Track)
OrphanEventuallyResolved == \A p \in Ids : Orphan(p) ~> ~Orphan(p)
=============================================================================
