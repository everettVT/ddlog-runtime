---------------------------- MODULE WorldLifecycle ----------------------------
(***************************************************************************)
(* Concurrent lifecycle of managed worlds in src/worlds.rs (ddlog-runtime).  *)
(*                                                                         *)
(* Atomicity model                                                         *)
(*  - Every WorldManager method takes &mut self; in socket mode the manager *)
(*    sits in Arc<Mutex<WorldManager>> (src/bin/worlds_socket/mod.rs:88).  *)
(*    That whole-manager lock M is the only guard of per-world state, so   *)
(*    each manager method (Start, Stop, Status, DropManager) is ONE atomic  *)
(*    action.                                                              *)
(*  - The controls lock C (worlds.rs:1429) is held by start_with from      *)
(*    :818 to :936, so StopAll (worlds.rs:1432-1441, takes C but not M)     *)
(*    cannot interleave with Start. Stop's C section (:950-959) only stops *)
(*    controls, which commutes with StopAll, so Stop stays atomic too.     *)
(*  - The start thread (worlds.rs:909-935) takes neither M nor C. It is    *)
(*    split at every point where a process is spawned, tracked or waited   *)
(*    for (lib.rs:384-394, 69-88, 407-418).                                *)
(*  - A ProcessControl (processes.rs:9-18) is one per (world, generation):  *)
(*    ctlStopped[w][g]. track() = insert then check stopped                *)
(*    (processes.rs:44-48); stop() = set stopped then kill every tracked   *)
(*    group (processes.rs:36-43). Both are SeqCst and G-locked, so they    *)
(*    linearise; each is modelled as one atomic step.                      *)
(*  - A process is identified by <<world, generation, kind>>. The          *)
(*    generation is persisted before any thread runs (worlds.rs:895 then   *)
(*    :909), so the key is unique across owner incarnations.               *)
(***************************************************************************)
EXTENDS Naturals, FiniteSets

CONSTANTS
    Worlds,           \* world ids
    MaxGen,           \* bound on generations per world (starts)
    MaxOwners,        \* bound on owner incarnations (Recover steps + 1)
    MaxPersistFails,  \* bound on nondeterministic world.json write failures
    TestWorlds,       \* subset of Worlds with purpose = "test"
    KeepWorld,        \* keep_world passed to start_with for test worlds
    AllowPanic,       \* allow the start thread to panic (-> Disconnected, T8)
    AllowKill         \* allow SIGKILL owner exit (no cleanup at all)

Kinds   == {"comp", "rt"}
Gens    == 1..MaxGen
Keys    == Worlds \X Gens \X Kinds
States  == {"created", "starting", "running", "stopping", "stopped", "failed", "interrupted"}
Errors  == {"none", "persist", "install", "crash", "worker", "interrupted"}
Phases  == {"none", "lower", "compSpawned", "compWait", "rtSpawned", "rtTracked", "installed"}
MsgKinds == {"empty", "some", "none", "err"}
TestPhases == {"none", "building", "failed", "done"}
ExitModes == {"up", "kill", "signal", "drop"}

VARIABLES
    \* ---- manager-side, per world, guarded by M (World struct, worlds.rs:99-131)
    state,      \* World.state
    gen,        \* World.generation
    err,        \* World.error (abstracted to a cause tag)
    inst,       \* World.instance: 0 = None, else generation of the held instance
    pending,    \* World.pending.is_some()
    stopReq,    \* World.stop_requested
    wtest,      \* World.test (copied from test_progress by status, :1034-1043)
    histLast,   \* last entry of in-memory World.history: <<state, gen, err>>
    dirty,      \* ghost: a persist of this world failed since the last success
    \* ---- persisted world.json (persist, worlds.rs:1560-1592)
    disk,       \* <<state, gen, err>> of the last successful write
    \* ---- manager-wide
    ctlIn,      \* controls map C: world -> generation of its control (0 = absent)
    S,          \* WorldShutdown.stopped (worlds.rs:1428)
    owner,      \* current owner incarnation
    exitMode,   \* "up" while the owner process runs, else how it exited
    dropped,    \* Drop for WorldManager has run (receivers gone)
    cleanExit,  \* ghost: owner o exited after stop_all (signal or drop path)
    fails,      \* number of persist failures used
    \* ---- start thread + channel, per world
    thr,        \* [ph: Phases, g: generation]
    chan,       \* mpsc buffer: [k: MsgKinds, g: generation]
    tp,         \* test_progress Arc<Mutex<Value>> phase (shared with thread)
    \* ---- process controls and OS processes
    ctlStopped, \* [Worlds -> [Gens -> BOOLEAN]]  ProcessControl.stopped
    pr,         \* [Keys -> [alive, tracked, ok: BOOLEAN, owner: 0..MaxOwners]]
    \* ---- ghost for R4
    stopAllKilled

mgr   == <<state, gen, err, inst, pending, stopReq, wtest, histLast, dirty>>
glob  == <<disk, ctlIn, S, owner, exitMode, dropped, cleanExit, fails>>
tvars == <<thr, chan, tp>>
pvars == <<ctlStopped, pr, stopAllKilled>>
vars  == <<mgr, glob, tvars, pvars>>

Triple(w) == <<state[w], gen[w], err[w]>>
NoProc == [alive |-> FALSE, tracked |-> FALSE, ok |-> FALSE, owner |-> 0]
ManagerLive == exitMode = "up" /\ ~dropped
PersistOutcomes == IF fails < MaxPersistFails THEN {TRUE, FALSE} ELSE {TRUE}

\* Kill every tracked group of control <<w,g>>  (processes.rs:36-43).
KillTracked(p, w, g) ==
    [k \in Keys |-> IF k[1] = w /\ k[2] = g /\ p[k].tracked
                    THEN [p[k] EXCEPT !.alive = FALSE] ELSE p[k]]
\* Drop of a Runtime / Group: kill group, kill child, wait, untrack
\* (lib.rs:180-186, processes.rs:66-71).
DropProc(p, K) ==
    [k \in Keys |-> IF k \in K THEN [p[k] EXCEPT !.alive = FALSE, !.tracked = FALSE]
                    ELSE p[k]]
Rt(w, g) == <<w, g, "rt">>
Comp(w, g) == <<w, g, "comp">>

-----------------------------------------------------------------------------
TypeOK ==
    /\ state \in [Worlds -> States]
    /\ gen \in [Worlds -> 0..MaxGen]
    /\ err \in [Worlds -> Errors]
    /\ inst \in [Worlds -> 0..MaxGen]
    /\ pending \in [Worlds -> BOOLEAN]
    /\ stopReq \in [Worlds -> BOOLEAN]
    /\ wtest \in [Worlds -> TestPhases]
    /\ histLast \in [Worlds -> States \X (0..MaxGen) \X Errors]
    /\ dirty \in [Worlds -> BOOLEAN]
    /\ disk \in [Worlds -> States \X (0..MaxGen) \X Errors]
    /\ ctlIn \in [Worlds -> 0..MaxGen]
    /\ S \in BOOLEAN
    /\ owner \in 1..MaxOwners
    /\ exitMode \in ExitModes
    /\ dropped \in BOOLEAN
    /\ cleanExit \in [1..MaxOwners -> BOOLEAN]
    /\ fails \in 0..MaxPersistFails
    /\ thr \in [Worlds -> [ph: Phases, g: 0..MaxGen]]
    /\ chan \in [Worlds -> [k: MsgKinds, g: 0..MaxGen]]
    /\ tp \in [Worlds -> TestPhases]
    /\ ctlStopped \in [Worlds -> [Gens -> BOOLEAN]]
    /\ pr \in [Keys -> [alive: BOOLEAN, tracked: BOOLEAN,
                        ok: BOOLEAN, owner: 0..MaxOwners]]
    /\ stopAllKilled \in [Worlds -> BOOLEAN]

\* Worlds exist already (create, worlds.rs:661-720, persisted at :708).
Init ==
    /\ state = [w \in Worlds |-> "created"]
    /\ gen = [w \in Worlds |-> 0]
    /\ err = [w \in Worlds |-> "none"]
    /\ inst = [w \in Worlds |-> 0]
    /\ pending = [w \in Worlds |-> FALSE]
    /\ stopReq = [w \in Worlds |-> FALSE]
    /\ wtest = [w \in Worlds |-> "none"]
    /\ histLast = [w \in Worlds |-> <<"created", 0, "none">>]
    /\ dirty = [w \in Worlds |-> FALSE]
    /\ disk = [w \in Worlds |-> <<"created", 0, "none">>]
    /\ ctlIn = [w \in Worlds |-> 0]
    /\ S = FALSE
    /\ owner = 1
    /\ exitMode = "up"
    /\ dropped = FALSE
    /\ cleanExit = [o \in 1..MaxOwners |-> FALSE]
    /\ fails = 0
    /\ thr = [w \in Worlds |-> [ph |-> "none", g |-> 0]]
    /\ chan = [w \in Worlds |-> [k |-> "empty", g |-> 0]]
    /\ tp = [w \in Worlds |-> "none"]
    /\ ctlStopped = [w \in Worlds |-> [g \in Gens |-> FALSE]]
    /\ pr = [k \in Keys |-> NoProc]
    /\ stopAllKilled = [w \in Worlds |-> FALSE]

-----------------------------------------------------------------------------
(* T3 / T3f: start_with (worlds.rs:815-938), under M and C.                 *)
(* Preconditions :819 (!S, read under C) and :821 (no instance, no pending). *)
(* There is no check on the state string itself.                            *)
Start(w) ==
    /\ ManagerLive
    /\ ~S                                     \* ensure_starting_allowed :819
    /\ inst[w] = 0 /\ ~pending[w]             \* :821
    /\ gen[w] < MaxGen                        \* model bound
    /\ LET g == gen[w] + 1
           test == w \in TestWorlds
       IN
       /\ gen' = [gen EXCEPT ![w] = g]                        \* :836
       /\ stopReq' = [stopReq EXCEPT ![w] = FALSE]            \* :837
       /\ ctlStopped' = [ctlStopped EXCEPT ![w][g] = FALSE]   \* fresh hosted control :844
       /\ stopAllKilled' = [stopAllKilled EXCEPT ![w] = FALSE]
       /\ tp' = [tp EXCEPT ![w] = IF test THEN "building" ELSE "none"]   \* :878-894
       /\ wtest' = [wtest EXCEPT ![w] = IF test THEN "building" ELSE "none"]
       /\ \E ok \in PersistOutcomes :                         \* persist :895
            IF ok THEN
              \* T3: state=starting, error=None (:876-877), persisted, thread spawned
              /\ state' = [state EXCEPT ![w] = "starting"]
              /\ err' = [err EXCEPT ![w] = "none"]
              /\ ctlIn' = [ctlIn EXCEPT ![w] = g]                        \* :868
              /\ disk' = [disk EXCEPT ![w] = <<"starting", g, "none">>]
              /\ histLast' = [histLast EXCEPT ![w] = <<"starting", g, "none">>]
              /\ dirty' = [dirty EXCEPT ![w] = FALSE]
              /\ pending' = [pending EXCEPT ![w] = TRUE]                 \* :908
              /\ thr' = [thr EXCEPT ![w] = [ph |-> "lower", g |-> g]]    \* :909
              /\ chan' = [chan EXCEPT ![w] = [k |-> "empty", g |-> 0]]
              /\ UNCHANGED fails
            ELSE
              \* T3f (:895-905): tailer stopped, controls.remove, failed, NOT persisted
              /\ state' = [state EXCEPT ![w] = "failed"]
              /\ err' = [err EXCEPT ![w] = "persist"]
              /\ ctlIn' = [ctlIn EXCEPT ![w] = 0]                        \* :901
              /\ dirty' = [dirty EXCEPT ![w] = TRUE]
              /\ fails' = fails + 1
              /\ UNCHANGED <<disk, histLast, pending, thr, chan>>
    /\ UNCHANGED <<inst, S, owner, exitMode, dropped, cleanExit, pr>>

(* T9 / T10 / T11: stop (worlds.rs:939-968), under M then C.                 *)
(* No precondition at all besides the world existing.                        *)
Stop(w) ==
    /\ ManagerLive
    /\ LET g == inst[w]
           \* :941-945 instance.backend.control.stop(); drop(instance)
           p1 == IF g # 0 THEN DropProc(KillTracked(pr, w, g), {Rt(w, g)}) ELSE pr
           \* :950-959 controls[id].stop()
           c == ctlIn[w]
           p2 == IF c # 0 THEN KillTracked(p1, w, c) ELSE p1
           s == IF pending[w] THEN "stopping" ELSE "stopped"   \* :960-965
       IN
       /\ inst' = [inst EXCEPT ![w] = 0]
       /\ pr' = p2
       /\ ctlStopped' = [ctlStopped EXCEPT ![w] =
                           [x \in Gens |-> ctlStopped[w][x] \/ x = g \/ x = c]]
       /\ stopReq' = [stopReq EXCEPT ![w] = TRUE]                    \* :949
       /\ state' = [state EXCEPT ![w] = s]
       /\ \E ok \in PersistOutcomes :                               \* :966
            IF ok THEN /\ disk' = [disk EXCEPT ![w] = <<s, gen[w], err[w]>>]
                       /\ histLast' = [histLast EXCEPT ![w] = <<s, gen[w], err[w]>>]
                       /\ dirty' = [dirty EXCEPT ![w] = FALSE]
                       /\ UNCHANGED fails
                  ELSE /\ dirty' = [dirty EXCEPT ![w] = TRUE]
                       /\ fails' = fails + 1
                       /\ UNCHANGED <<disk, histLast>>
    /\ UNCHANGED <<gen, err, pending, wtest, ctlIn, S, owner, exitMode, dropped,
                   cleanExit, tvars, stopAllKilled>>
    \* NOTE: stop's trailing self.status(id) (:967) is the separate Status step.

(* status_with (worlds.rs:992-1134), under M.                                *)
(*   reap T4-T7 (:994-1023), Disconnected T8 (:1024-1031), test copy         *)
(*   (:1034-1043), crash detection T12 (:1048-1077), persist_if_changed     *)
(*   (:1078, :1550-1559).                                                   *)
Status(w) ==
    /\ ManagerLive
    /\ LET m == chan[w]
           reap == pending[w] /\ m.k # "empty"
           disc == pending[w] /\ m.k = "empty" /\ thr[w].ph = "none"
           s1 == IF reap THEN (IF stopReq[w] THEN "stopped"                   \* :998-1000
                               ELSE CASE m.k = "some" -> "running"            \* :1003-1007
                                      [] m.k = "none" -> "stopped"            \* :1008-1011
                                      [] OTHER        -> "failed")            \* :1012-1015
                 ELSE IF disc THEN "failed" ELSE state[w]                    \* :1026
           e1 == IF reap THEN (IF stopReq[w] THEN err[w]
                               ELSE IF m.k = "err" THEN "install" ELSE "none")
                 ELSE IF disc THEN "worker" ELSE err[w]
           i1 == IF reap /\ ~stopReq[w] /\ m.k = "some" THEN m.g ELSE inst[w]
           dropK == IF reap /\ stopReq[w] /\ m.k = "some" THEN {Rt(w, m.g)} ELSE {}  \* :999
           crashed == i1 # 0 /\ ~pr[Rt(w, i1)].alive                          \* :1049
           s2 == IF crashed THEN "failed" ELSE s1                            \* :1050
           e2 == IF crashed /\ e1 = "none" THEN "crash" ELSE e1              \* :1051 get_or_insert
           i2 == IF crashed THEN 0 ELSE i1                                   \* :1069
           p1 == DropProc(pr, dropK)
           p2 == IF crashed THEN DropProc(KillTracked(p1, w, i1), {Rt(w, i1)}) ELSE p1
           t2 == <<s2, gen[w], e2>>
       IN
       /\ pending' = [pending EXCEPT ![w] = pending[w] /\ ~(reap \/ disc)]   \* :997, :1025
       /\ chan' = [chan EXCEPT ![w] = IF reap THEN [k |-> "empty", g |-> 0] ELSE m]
       /\ state' = [state EXCEPT ![w] = s2]
       /\ err' = [err EXCEPT ![w] = e2]
       /\ inst' = [inst EXCEPT ![w] = i2]
       /\ pr' = p2
       /\ ctlStopped' = IF crashed THEN [ctlStopped EXCEPT ![w][i1] = TRUE] ELSE ctlStopped
       /\ wtest' = [wtest EXCEPT ![w] = tp[w]]                               \* :1040
       /\ IF t2 # histLast[w]                                                \* :1550-1555
          THEN \E ok \in PersistOutcomes :
                 IF ok THEN /\ disk' = [disk EXCEPT ![w] = t2]
                            /\ histLast' = [histLast EXCEPT ![w] = t2]
                            /\ dirty' = [dirty EXCEPT ![w] = FALSE]
                            /\ UNCHANGED fails
                       ELSE /\ dirty' = [dirty EXCEPT ![w] = TRUE]
                            /\ fails' = fails + 1
                            /\ UNCHANGED <<disk, histLast>>
          ELSE UNCHANGED <<disk, histLast, dirty, fails>>
    /\ UNCHANGED <<gen, stopReq, ctlIn, S, owner, exitMode, dropped, cleanExit,
                   thr, tp, stopAllKilled>>

(* WorldShutdown::stop_all (worlds.rs:1432-1441): takes C only, never M.    *)
(* Sets S and stops every control in the map. World state is NOT touched.  *)
StopAll ==
    /\ exitMode = "up"
    /\ S' = TRUE
    /\ ctlStopped' = [w \in Worlds |-> [g \in Gens |->
                         ctlStopped[w][g] \/ (ctlIn[w] = g)]]
    /\ pr' = [k \in Keys |-> IF ctlIn[k[1]] = k[2] /\ pr[k].tracked
                             THEN [pr[k] EXCEPT !.alive = FALSE] ELSE pr[k]]
    /\ stopAllKilled' = [w \in Worlds |-> stopAllKilled[w] \/
                           (inst[w] # 0 /\ ctlIn[w] = inst[w] /\ pr[Rt(w, inst[w])].alive)]
    /\ UNCHANGED <<mgr, disk, ctlIn, owner, exitMode, dropped, cleanExit, fails, tvars>>

-----------------------------------------------------------------------------
(* The start thread, worlds.rs:909-935, one step per process interaction.   *)

\* Deliver the result (:934). If the receiver is gone (manager dropped),
\* send fails and the result is dropped, which drops a live Runtime.
Send(w, kind, g, p) ==
    IF pending[w]
    THEN /\ chan' = [chan EXCEPT ![w] = [k |-> kind, g |-> g]]
         /\ pr' = p
    ELSE /\ chan' = chan
         /\ pr' = IF kind = "some" THEN DropProc(p, {Rt(w, g)}) ELSE p

\* install_source: write files, then spawn the driver in its own group
\* (lib.rs:374-391, processes.rs:72-80). Spawn does NOT check control.stopped.
ThrSpawnComp(w) ==
    /\ thr[w].ph = "lower"
    /\ LET k == Comp(w, thr[w].g) IN
       pr' = [pr EXCEPT ![k] = [alive |-> TRUE, tracked |-> FALSE,
                                ok |-> FALSE, owner |-> owner]]
    /\ thr' = [thr EXCEPT ![w].ph = "compSpawned"]
    /\ UNCHANGED <<mgr, glob, chan, tp, ctlStopped, stopAllKilled>>

\* control.track(pid) (lib.rs:392 -> processes.rs:44-48): insert, then kill
\* if the control is already stopped.
ThrTrackComp(w) ==
    /\ thr[w].ph = "compSpawned"
    /\ LET g == thr[w].g
           k == Comp(w, g)
       IN pr' = [pr EXCEPT ![k].tracked = TRUE,
                           ![k].alive = pr[k].alive /\ ~ctlStopped[w][g]]
    /\ thr' = [thr EXCEPT ![w].ph = "compWait"]
    /\ UNCHANGED <<mgr, glob, chan, tp, ctlStopped, stopAllKilled>>

\* child.wait() returns (lib.rs:393), drop(group) (:394), then either Err
\* (:395-400) or Runtime::start spawns the native child (:401 -> :87).
ThrWaitComp(w) ==
    /\ thr[w].ph = "compWait"
    /\ LET g == thr[w].g
           k == Comp(w, g)
       IN /\ ~pr[k].alive
          /\ IF pr[k].ok
             THEN /\ pr' = [pr EXCEPT ![k].tracked = FALSE,
                                      ![Rt(w, g)] = [alive |-> TRUE,
                                          tracked |-> FALSE, ok |-> FALSE, owner |-> owner]]
                  /\ thr' = [thr EXCEPT ![w].ph = "rtSpawned"]
                  /\ UNCHANGED <<chan, tp>>
             ELSE /\ thr' = [thr EXCEPT ![w] = [ph |-> "none", g |-> 0]]
                  /\ tp' = [tp EXCEPT ![w] = IF w \in TestWorlds THEN "failed" ELSE tp[w]] \* :915-921
                  /\ Send(w, "err", g, [pr EXCEPT ![k].tracked = FALSE])
    /\ UNCHANGED <<mgr, glob, ctlStopped, stopAllKilled>>

\* control.track(child.id()) in Runtime::start (lib.rs:88).
ThrTrackRt(w) ==
    /\ thr[w].ph = "rtSpawned"
    /\ LET g == thr[w].g
           k == Rt(w, g)
       IN pr' = [pr EXCEPT ![k].tracked = TRUE,
                           ![k].alive = pr[k].alive /\ ~ctlStopped[w][g]]
    /\ thr' = [thr EXCEPT ![w].ph = "rtTracked"]
    /\ UNCHANGED <<mgr, glob, chan, tp, ctlStopped, stopAllKilled>>

\* Replay exchange (lib.rs:407-417): succeeds iff the child is alive.
\* On failure the local Runtime is dropped (kill + wait) and Err is sent.
ThrReplay(w) ==
    /\ thr[w].ph = "rtTracked"
    /\ LET g == thr[w].g IN
       IF pr[Rt(w, g)].alive
       THEN /\ thr' = [thr EXCEPT ![w].ph = "installed"]
            /\ UNCHANGED <<pr, chan, tp>>
       ELSE /\ thr' = [thr EXCEPT ![w] = [ph |-> "none", g |-> 0]]
            /\ tp' = [tp EXCEPT ![w] = IF w \in TestWorlds THEN "failed" ELSE tp[w]]
            /\ Send(w, "err", g, DropProc(pr, {Rt(w, g)}))
    /\ UNCHANGED <<mgr, glob, ctlStopped, stopAllKilled>>

\* Build the StartOutcome (:914-933): instance world -> Ok(Some); test world ->
\* run_scenarios (:925, :1305-1362) then Ok(Some) if keep_world, else
\* drop(instance) and Ok(None). Then send (:934).
ThrFinish(w) ==
    /\ thr[w].ph = "installed"
    /\ LET g == thr[w].g
           test == w \in TestWorlds
       IN /\ thr' = [thr EXCEPT ![w] = [ph |-> "none", g |-> 0]]
          /\ tp' = [tp EXCEPT ![w] = IF test THEN "done" ELSE tp[w]]
          /\ IF test /\ ~KeepWorld
             THEN Send(w, "none", g, DropProc(pr, {Rt(w, g)}))
             ELSE Send(w, "some", g, pr)
    /\ UNCHANGED <<mgr, glob, ctlStopped, stopAllKilled>>

\* A panic in the start thread (e.g. :916 unwrap, or in run_scenarios):
\* unwinding drops the instance (kill + wait) and the Sender -> Disconnected.
ThrPanic(w) ==
    /\ AllowPanic
    /\ thr[w].ph \in {"lower", "installed"}
    /\ pr' = IF thr[w].ph = "installed" THEN DropProc(pr, {Rt(w, thr[w].g)}) ELSE pr
    /\ thr' = [thr EXCEPT ![w] = [ph |-> "none", g |-> 0]]
    /\ UNCHANGED <<mgr, glob, chan, tp, ctlStopped, stopAllKilled>>

ThreadStep(w) ==
    \/ ThrSpawnComp(w) \/ ThrTrackComp(w) \/ ThrWaitComp(w)
    \/ ThrTrackRt(w) \/ ThrReplay(w) \/ ThrFinish(w) \/ ThrPanic(w)

-----------------------------------------------------------------------------
(* Environment.                                                             *)

\* The compiler driver exits by itself, successfully or not.
CompilerFinish(k) ==
    /\ k[3] = "comp" /\ pr[k].alive
    /\ \E ok \in BOOLEAN : pr' = [pr EXCEPT ![k].alive = FALSE, ![k].ok = ok]
    /\ UNCHANGED <<mgr, glob, tvars, ctlStopped, stopAllKilled>>

\* The native child dies (crash, OOM, or stdin EOF after its owner exited).
Crash(k) ==
    /\ k[3] = "rt" /\ pr[k].alive
    /\ pr' = [pr EXCEPT ![k].alive = FALSE]
    /\ UNCHANGED <<mgr, glob, tvars, ctlStopped, stopAllKilled>>

-----------------------------------------------------------------------------
(* Owner exit and recovery.                                                 *)

(* Drop for WorldManager (worlds.rs:1443-1459): stop_all, then every world  *)
(* holding an instance is stopped, dropped, marked stopped and persisted    *)
(* (errors ignored; modelled as success). Worlds with a pending start are  *)
(* NOT touched. Dropping the World drops its Receiver; a buffered message   *)
(* is dropped with the channel (its Runtime dies).                          *)
DropManager ==
    /\ ManagerLive
    /\ LET pa == [k \in Keys |-> IF ctlIn[k[1]] = k[2] /\ pr[k].tracked
                                 THEN [pr[k] EXCEPT !.alive = FALSE] ELSE pr[k]]
           K == {Rt(w, inst[w]) : w \in {x \in Worlds : inst[x] # 0}}
                \cup {Rt(w, chan[w].g) : w \in {x \in Worlds : chan[x].k = "some"}}
           newState == [w \in Worlds |-> IF inst[w] # 0 THEN "stopped" ELSE state[w]]
       IN
       /\ S' = TRUE
       /\ ctlStopped' = [w \in Worlds |-> [g \in Gens |->
                            ctlStopped[w][g] \/ ctlIn[w] = g \/ inst[w] = g]]
       /\ pr' = DropProc(pa, K)
       /\ state' = newState
       /\ disk' = [w \in Worlds |-> IF inst[w] # 0
                                    THEN <<"stopped", gen[w], err[w]>> ELSE disk[w]]
       /\ histLast' = [w \in Worlds |-> IF inst[w] # 0
                                        THEN <<"stopped", gen[w], err[w]>> ELSE histLast[w]]
       /\ inst' = [w \in Worlds |-> 0]
       /\ pending' = [w \in Worlds |-> FALSE]
       /\ chan' = [w \in Worlds |-> [k |-> "empty", g |-> 0]]
       /\ dropped' = TRUE
    /\ UNCHANGED <<gen, err, stopReq, wtest, dirty, ctlIn, owner, exitMode, cleanExit,
                   fails, thr, tp, stopAllKilled>>

(* The owner process exits. Start threads die with it; children live in     *)
(* their own process groups (processes.rs:77) and are NOT killed by the OS.  *)
(*  "signal": SIGINT/SIGTERM -> stop_all, then exit(143)                     *)
(*            (src/bin/ddlog-worlds.rs:132-139), or socket serve returning   *)
(*            after stop_all (worlds_socket/mod.rs:113). Requires S.        *)
(*  "drop":   main returns after Drop (stdio EOF, ddlog-worlds.rs:158-175).  *)
(*  "kill":   SIGKILL / machine failure; nothing runs.                       *)
Exit(mode) ==
    /\ exitMode = "up"
    /\ CASE mode = "signal" -> S /\ ~dropped
         [] mode = "drop"   -> dropped
         [] mode = "kill"   -> AllowKill
         [] OTHER           -> FALSE
    /\ exitMode' = mode
    /\ cleanExit' = [cleanExit EXCEPT ![owner] = (mode # "kill")]
    /\ thr' = [w \in Worlds |-> [ph |-> "none", g |-> 0]]
    /\ chan' = [w \in Worlds |-> [k |-> "empty", g |-> 0]]
    \* No control survives the process: nothing tracks the orphans any more.
    /\ pr' = [k \in Keys |-> [pr[k] EXCEPT !.tracked = FALSE]]
    /\ UNCHANGED <<mgr, disk, ctlIn, S, owner, dropped, fails, tp, ctlStopped, stopAllKilled>>

(* T2: WorldManager::new (worlds.rs:194-265) in a fresh owner. The flock is *)
(* free once the old process is gone (worlds.rs:1513-1549). starting /      *)
(* running / stopping records become interrupted and are persisted (:240-245).*)
Recover ==
    /\ exitMode # "up"
    /\ owner < MaxOwners
    /\ LET rs(w) == IF disk[w][1] \in {"starting", "running", "stopping"}
                    THEN <<"interrupted", disk[w][2], "interrupted">> ELSE disk[w]
       IN /\ disk' = [w \in Worlds |-> rs(w)]
          /\ histLast' = [w \in Worlds |-> rs(w)]
          /\ state' = [w \in Worlds |-> rs(w)[1]]
          /\ gen' = [w \in Worlds |-> rs(w)[2]]
          /\ err' = [w \in Worlds |-> rs(w)[3]]
    /\ inst' = [w \in Worlds |-> 0]
    /\ pending' = [w \in Worlds |-> FALSE]
    /\ stopReq' = [w \in Worlds |-> FALSE]
    /\ dirty' = [w \in Worlds |-> FALSE]
    /\ wtest' = wtest            \* stale test phase is kept as persisted
    /\ ctlIn' = [w \in Worlds |-> 0]
    /\ S' = FALSE
    /\ owner' = owner + 1
    /\ exitMode' = "up"
    /\ dropped' = FALSE
    /\ tp' = [w \in Worlds |-> "none"]
    /\ UNCHANGED <<cleanExit, fails, thr, chan, pvars>>

\* Explicit terminal stuttering: the last owner incarnation has exited.
Done == exitMode # "up" /\ owner = MaxOwners /\ UNCHANGED vars

-----------------------------------------------------------------------------
Next ==
    \/ \E w \in Worlds : Start(w) \/ Stop(w) \/ Status(w) \/ ThreadStep(w)
    \/ StopAll
    \/ \E k \in Keys : CompilerFinish(k) \/ Crash(k)
    \/ DropManager
    \/ \E m \in {"signal", "drop", "kill"} : Exit(m)
    \/ Recover
    \/ Done

Fairness ==
    /\ \A w \in Worlds : WF_vars(ThreadStep(w)) /\ WF_vars(Status(w))
    /\ WF_vars(Recover)

Spec == Init /\ [][Next]_vars /\ Fairness

-----------------------------------------------------------------------------
(* Safety properties.                                                        *)

\* I1: instance.is_some() <=> state = running (while the manager is live).
I1 == ManagerLive => \A w \in Worlds : (inst[w] # 0) <=> (state[w] = "running")
\* I2: pending => starting or stopping.
I2 == ManagerLive => \A w \in Worlds : pending[w] => state[w] \in {"starting", "stopping"}
\* I3: never both an instance and a pending start.
I3 == ManagerLive => \A w \in Worlds : ~(inst[w] # 0 /\ pending[w])
\* I4: a live start thread always has its receiver in World.pending.
I4 == ManagerLive => \A w \in Worlds : thr[w].ph # "none" => pending[w]
\* I5: starting / stopping => pending.
I5 == ManagerLive => \A w \in Worlds : state[w] \in {"starting", "stopping"} => pending[w]
\* I6: stop_requested => stopping or stopped (violated with panics: T8, R10).
I6 == ManagerLive => \A w \in Worlds : stopReq[w] => state[w] \in {"stopping", "stopped"}

\* A reaped instance, a buffered result and a live thread all belong to the
\* world's current generation: no stale start can install.
StaleThreadNeverInstalls ==
    ManagerLive => \A w \in Worlds :
        /\ inst[w] # 0 => inst[w] = gen[w]
        /\ chan[w].k # "empty" => chan[w].g = gen[w]
        /\ thr[w].ph # "none" => thr[w].g = gen[w]

\* Within one owner incarnation a world never has two live native children,
\* nor even two live managed processes (compiler + runtime) at once.
NoTwoLiveNativeChildrenPerWorldWithinOwner ==
    \A w \in Worlds : \A o \in 1..MaxOwners :
        Cardinality({g \in Gens : pr[Rt(w, g)].alive /\ pr[Rt(w, g)].owner = o}) <= 1
NoTwoLiveProcessesPerWorldWithinOwner ==
    \A w \in Worlds : \A o \in 1..MaxOwners :
        Cardinality({k \in Keys : k[1] = w /\ pr[k].alive /\ pr[k].owner = o}) <= 1

\* The last in-memory history entry is exactly what world.json holds, and the
\* in-memory (state, generation, error) differs from disk only after a failed
\* write that has not yet been retried successfully.
DiskHistoryConsistency ==
    exitMode = "up" => \A w \in Worlds :
        /\ histLast[w] = disk[w]
        /\ Triple(w) # disk[w] => dirty[w]

\* A live instance's native child is tracked by the control in the map
\* (so stop / stop_all can reach it).
LiveInstanceIsTracked ==
    ManagerLive => \A w \in Worlds :
        inst[w] # 0 /\ pr[Rt(w, inst[w])].alive =>
            pr[Rt(w, inst[w])].tracked /\ ctlIn[w] = inst[w]

\* ---- Properties expected to FAIL (separate configs) ----------------------

\* R1: after an owner exits through stop_all (signal or Drop), no process it
\* spawned survives.
OwnerCleanExitLeavesNoOrphan ==
    \A k \in Keys : pr[k].alive /\ pr[k].owner # 0 => ~cleanExit[pr[k].owner]

\* R4: a world whose running instance was killed by stop_all is reported
\* stopped, never failed.
NoFailedAfterStopAll ==
    ManagerLive => \A w \in Worlds : stopAllKilled[w] => state[w] # "failed"

\* R2: across owner incarnations a world never has two live processes.
NoTwoLiveProcessesPerWorldAcrossOwners ==
    \A w \in Worlds : Cardinality({k \in Keys : k[1] = w /\ pr[k].alive}) <= 1

-----------------------------------------------------------------------------
(* Liveness.                                                                 *)

\* A world stopped while its start was pending reaches stopped (unless the
\* owner goes away first).
StoppingEventuallyStopped ==
    \A w \in Worlds :
        (ManagerLive /\ state[w] = "stopping") ~> (state[w] = "stopped" \/ ~ManagerLive)

\* Once a world's control is stopped, every live process of that control
\* spawned by the live owner dies (while that owner stays up).
StoppedControlEventuallyKills ==
    \A k \in Keys :
        (exitMode = "up" /\ pr[k].alive /\ pr[k].owner = owner /\ ctlStopped[k[1]][k[2]])
            ~> (~pr[k].alive \/ exitMode # "up")
=============================================================================
