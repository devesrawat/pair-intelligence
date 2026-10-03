# Crash / retry scenarios (spec section 12, release gate "Recovery")

Ten named scenarios. Each is an integration test in `service/crates/jobs/tests/crash_scenarios.rs`
(same name as below, prefixed `crash_scenario_NN_`) against a throwaway PostgreSQL database.
Run: `cargo test -p pair-jobs --offline --test crash_scenarios`.

Method. The external system is a fake `Remote` that counts effects that actually landed
(`landed`) and answers reconcile questions truthfully. A crash is `JoinHandle::abort` (the worker
future is dropped and its lease stays behind) or a terminated database backend. Lease expiry is
forced with SQL (`expire_leases`); tests wait on latches or database state, never on the clock.
Scenarios 4 and 5 need a LIVE worker to notice it lost its lease, so they use a 3 s lease and wait
for that event (bounded by the heartbeat period, ttl / 3), not for a duration.

The invariant in every scenario: **the external effect lands exactly once** (`landed == 1` at the
end), the run reaches the stated terminal state, and no intent is left unresolved.

| # | Name | Injected failure point | Expected terminal state | External effect count |
|---|---|---|---|---|
| 1 | `crash_before_effect` | Worker dies before it writes the effect intent | Run `succeeded` after sweep + resume; intent `completed`; reconcile never called | 1 (0 before recovery) |
| 2 | `crash_after_effect_before_completion` | Effect lands, worker dies before the intent is marked `completed` | `succeeded`; intent `executing` then `completed`; reconcile finds it applied (1 call), effect closure not re-run | 1 |
| 3 | `crash_between_consume_approval_and_effect` | Approval consumed and bound to the intent, worker dies before sending | After resume the same run re-enters its own approval, reconciles (not applied), sends: `succeeded`; the spent approval cannot be consumed by anyone else (`Conflict`) | 1 (0 before recovery) |
| 4 | `slow_worker_after_lease_expiry` | Worker alive and mid-effect, lease forced to expire, successor claims | Successor `succeeded`; slow worker notices the lost lease and its in-flight effect future is dropped; intent `completed` | 1 |
| 5 | `cancel_during_inflight_effect` | Cancel arrives after the effect landed but before the worker saw its result | Run `cancelled` and stays cancelled (resume is a no-op, nothing claimable); intent `executing` until the orphan sweep settles it as `completed` without re-executing | 1 |
| 6 | `reconcile_callback_fails` | Crash mid-effect, then the reconcile callback itself errors | Run `failed` with `unreconciled_effect` prefix, intent `unknown`, no blind resend; a later orphan sweep with a working reconciler settles the intent `completed` | 1 |
| 7 | `approval_expires_while_parked` | Approval expires (SQL) while the run waits in `waiting_approval` | `grant` fails `ApprovalExpired`, run stays parked, no intent; a fresh approval releases it: `succeeded` | 0 while parked, then 1 |
| 8 | `duplicate_start_same_idempotency_key` | Eight concurrent starts with one key, a conflicting start, and a late duplicate after completion | One run row and one run id; different input with the same key is `Conflict`; late duplicate returns the finished run and queues nothing; `succeeded` | 1 |
| 9 | `two_workers_racing_one_run` | Four workers call `run_once` concurrently while the winner is held mid-effect | Exactly one claim (`lease_epoch == 1`), three workers get nothing, winner `succeeded` | 1 |
| 10 | `process_restart_mid_checkpoint` | The step-0 checkpoint transaction is blocked on a row lock and its database backend is terminated; a new pool/store/worker then takes over | Checkpoint did not commit (`next_step == 0`, no step row); after sweep + resume step 0 re-runs, its effect is replayed from the completed intent; `succeeded` with 2 step rows | 1 |

## Honest limits

- Crashes are simulated in-process (task abort, backend termination), not by killing a real
  process; the jobs state machine sees the same database state either way.
- The remote is a counter. Real providers may not support idempotency keys or truthful
  reconciliation; that is the section 9 reconcile-before-retry assumption, not tested here.
- Scenario 4 cannot rule out a double effect if the slow worker's effect completes inside the heartbeat
  gap (up to ttl / 3) before it notices the lost lease; the fence stops it writing intent state, not
  the remote call. Reconcile-before-retry covers that case only when the remote reports truthfully.
- No wiring: the jobs crate has no production caller yet (see `docs/STATUS.md`), so this proves the
  library, not the deployed service.
- Not yet run as part of CI at scale: the stability evidence is three consecutive local runs of the
  whole jobs suite.
