# Requests.tla: the registered-request (external inference) protocol

This is a TLA+ model of `AgentProgram` (`src/operations.rs`), the admission guards in
`ProgramInstance::execute` (`src/instance.rs:137-223`), and `Backend::apply_inner`
(`src/lib.rs:460-522`). It models one program instance with a registered-operation program
already installed.

## What is modelled

| Model element | Code |
| --- | --- |
| `alive`, `failed` | `Backend.runtime.is_some()`, `Backend.failed` (`lib.rs:205, 212`) |
| `nIntent`, `nCur`, `nClaimed`, `nResp` | the retained `agent_intent`, `agent_current`, `agent_claimed` and `agent_response` input facts in `Backend.facts` (`lib.rs:207`; schemas at `operations.rs:30-42`) |
| `memCur`, `known`, `claimed`, `output` | `AgentProgram.current` and `AgentProgram.requests` (`operations.rs:24-29`), which live only in memory |
| `AgentResult` (derived) | the `agent_result` rule (`operations.rs:81`): intent ∧ current ∧ response |
| `Submit` then `SubmitMem` | `operations.rs:111-166`: guards 118-134, the native transaction 143-154, then the in-memory update 155-162 |
| `Claim` then `ClaimMem` | `operations.rs:167-183`: runtime check 168, stale check 172, already-claimed check 175, native insert 178, in-memory update 179 |
| `CompleteOp` then `CompleteMem` | `operations.rs:184-204`: claim required 187, identical duplicate acknowledged or conflicting output rejected 190-197, native insert 198, in-memory update 200 |
| `Die("commit-then-die")` and `Die("die-before-commit")` | an exchange error kills the child; `facts` and `revision` are not advanced (`lib.rs:514-520`) |
| `Guard` | the failed-instance refusal, which applies only when an instance id is set (`instance.rs:152-158`). `Mode = "standalone"` is `host.rs:138`, where `instance_id = None` |
| `Reinstall` | the only reinstall path left open: `install_agent_program` with no input facts at all (`operations.rs:95-97`). The other install entry points are refused (`instance.rs:159-166, 221-223, 499-501`) |
| Workers (`Claim`, `Provider`, `WorkerComplete`, `WorkerForget`) | the external worker (`docs/inference.md`): an explicit claim, the provider call outside DDlog, then settlement. Settlement may arrive after the request went stale, and an identical settlement may be resent after a lost acknowledgement |
| `RogueComplete` (`AllowRogue`) | any other same-user client may call `complete_agent_request` with any id and output; there is no claimant binding |

### Abstractions
- **Operations are two steps.** `execute` runs behind the host mutex (`host.rs:188, 280`), so each
  operation is a native commit followed by an in-memory update. No other instance operation
  interleaves between the two steps (the `pending` variable); worker provider calls do.
- **The two failure kinds are distinct actions but look the same afterwards.** Once the child is
  killed, whatever it committed is gone and the retained facts are unchanged. The ghost variable
  `lastFailure` records which kind happened.
- **Request ids are `<<entity, revision, payload>>`.** The operation name and version are fixed
  for each `AgentProgram` (`operations.rs:135-142`).
- **Rejected requests change no state,** so they are modelled as disabled steps.
- **String validation, deltas and `Backend.revision` are omitted.** A new instance is a fresh
  model, since nothing is persisted.
- **VIEW:** the fingerprints drop an idle worker's stale `wid` and `wout` and the `lastFailure`
  label. Nothing reads them again, so the reduction is sound.

## Properties
| Property | Meaning |
| --- | --- |
| `AtMostOneOutputPerRequest` | at most one `agent_response(id, _)` fact per request |
| `NoCompletionWithoutClaim` | every response, native or in memory, belongs to a claimed request |
| `ClaimOnlyFresh` (action property) | a new `agent_claimed(id)` fact is added only while `agent_current(entity, id)` holds |
| `FreshFlagMatchesAgentResult` | in the state where `complete` returns (fresh or duplicate), `fresh` ⇔ the output is visible in `agent_result` |
| `ResultOnlyCurrent` | between operations, every `agent_result` row is the current request's recorded output, so there is at most one row per entity |
| `InMemoryNeverAheadOfNative` | everything recorded in memory was acknowledged natively first |
| `MemMatchesNativeBetweenOps` | between operations, including after any native failure, memory and the retained facts agree exactly |
| `UncertainNeverReclaimed` | each request is claimed, and sent to the provider, at most once per instance |
| `UncertainStaysClaimed` | a request whose settlement is uncertain stays claimed and is never re-admitted. This is the property "uncertain settlement never implies permission to repeat provider work" |
| `NoAckWhileRuntimeDead` (C5) | no settlement is acknowledged while the runtime is dead |
| `OnlyClaimantSettles` (C8) | only the worker that claimed a request commits its response |
| `Witness*` | non-vacuity checks, each expected to be violated: a late stale acknowledgement, a duplicate acknowledgement, an uncertain settlement, and a stale response kept natively |

## TLC results (TLC2 2026.09.22, 4 workers)

The constants for all runs are Entities={e1}, Revs={0,1}, Payloads={x,y}, Outs={o1,o2},
Workers={k1,k2}, AllowRogue=TRUE, MaxFailures=1. `Requests_C8.cfg` differs only in
Workers={k1}.

| Config | Result |
| --- | --- |
| `Requests.cfg` (host mode) | **All properties hold.** 106362 states generated, 17986 distinct, depth 19, 3 s |
| `Requests_standalone.cfg` (standalone mode, all properties except C5) | **All hold.** 305094 states generated, 38759 distinct, depth 21, 4 s |
| `Requests_C5.cfg` (standalone mode) | **`NoAckWhileRuntimeDead` violated** at depth 9 (3335 states generated, 1014 distinct) |
| `Requests_C8.cfg` | **`OnlyClaimantSettles` violated** at depth 6 |
| Witness runs | all four are reachable (counterexamples of depth 6-9) |

Entities={e1,e2} was also started. After more than 1.6M distinct states it had not finished, so
it was stopped; the per-entity logic has no cross-entity interaction apart from sharing the runtime.

### C5 counterexample (`Requests_C5.cfg`)
1. `Submit(e1,1,x)` and `SubmitMem`: request R is admitted.
2. `Claim(k1,R)` and `ClaimMem`: worker k1 holds R and is calling the provider.
3. `RogueComplete(R,o1)` and `CompleteMem`: another client settles R with o1, and the response
   returns `fresh:true`.
4. A later `Submit(e1,1,x)` hits a native failure (commit-then-die), so `alive=FALSE` and
   `failed=TRUE`.
5. `RogueComplete(R,o1)` sends an identical settlement. It returns `duplicate:true, fresh:true`
   while the runtime is dead (`deadAck=TRUE`).

In code terms: the duplicate branch (`operations.rs:190-197`) returns before touching the backend,
and the failed-instance guard (`instance.rs:152-158`) applies only when an instance id is set. In
host mode the same sequence is refused, which is why `Requests.cfg` holds.

**Suggested fix:** in `complete`, check `backend.runtime.is_some()`, or check that health is not
"failed", before the duplicate branch, as `claim` does at `operations.rs:168`. Alternatively, apply
the `instance.rs:152` guard regardless of the instance id.

### C8 counterexample (`Requests_C8.cfg`)
1. R is submitted, and worker k1 claims it and starts the provider call.
2. `RogueComplete` commits a response for R from a client that never claimed it.
3. When k1 later settles with a different output, it is rejected as a conflict, and k1's provider
   work is wasted.

In code: `complete` checks only `request.claimed` (`operations.rs:187`), never who claimed.
**Suggested fix:** have `claim` return a claim token (for example a random nonce stored in
`Request`) and require it in `complete`.

## What the model confirms
- **No double completion and no duplicate response facts.** Duplicates are handled before the
  backend is touched, conflicting outputs are rejected, and memory is updated only after the
  native acknowledgement.
- **Commit-then-die leaves the request claimed and unsettled.** Memory and the retained facts stay
  equal. The request can never be claimed again in that instance (claimed, and in host mode the
  whole instance is refused), so there is no path to repeat provider work inside the instance.
  Across instances, re-settlement is possible because ids are deterministic (`operations.rs:135-142`).
  That is outside this model and left to the worker.
- **No claim survives a reinstall.** The only reinstall path needs empty facts, and
  `MemMatchesNativeBetweenOps` shows empty facts imply no requests.
- **The fresh flag and `agent_result` agree.** A late result for a stale request is kept natively
  but never shown.

Full TLC output for each counterexample is in `traces/Requests_C5.out` and `traces/Requests_C8.out`.
