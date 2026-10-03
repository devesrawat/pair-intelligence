# Runbook: kill switch

Use when: spend is running away, a prompt-injection or policy bypass is suspected, a credential may be exposed, or an external mutation happened without approval. Act first, diagnose after.

Model output cannot override permissions or budgets (spec global constraints); this runbook is the human override.

## Read this first: what is enforced today

`pair-api` hosts the budget, policy, routing, context and conversation crates behind `/v1/turn`, `/v1/budget/reserve`, `/v1/budget/reconcile`, `/v1/policy/authorize` and `/v1/approvals`. The OpenClaw adapter (`pair-spike` plugin) now calls the policy and budget endpoints, so levels 1 and 3 reach OpenClaw's own tool and model calls **when the adapter is loaded with `allowConversationAccess=true`** (proven in `adapters/openclaw/spike/wired.sh`, see [spike doc](../spike-openclaw-plugin.md)). Limits:

- Without `allowConversationAccess=true` the model-call gate does not exist: levels 1 and 3 then stop tool calls (the adapter denies every tool and logs `pair-spike: plugins.entries.pair-spike.hooks.allowConversationAccess is not true` at start) but **not model calls**. Check the gateway log for that line.
- Only runs that start after the change are refused. A run already past `before_agent_run` finishes its model calls with no further reservation; a later attempt of the same run (retry, compaction) is blocked.
- The adapter path's budget is a per-run **estimate**, not a bound: caps are enforced on holds, not on realised spend, and a long tool loop can overspend its hold (the per-run tool-call guard, default 10, limits tool round trips). Zeroing caps does not recover spend already incurred.
- Anything PAIR holds for approval (for example `git push`) is blocked on the adapter path, not approved.
- Proven with a mock model only; no real provider call has been made.
- The control that holds regardless of PAIR is the provider console (A) and stopping the OpenClaw gateway (B).
- Interrupting jobs (level 2) has no operator command and stays **NOT EFFECTIVE** (see level 2).

## Levels, lowest first (stop at the lowest level that contains the problem)

### Level 1: zero the budget caps (EFFECTIVE for `pair-api` calls; proven)

Edit `config/budget.yaml` (or the file named by `PAIR_BUDGET_CONFIG`) and set **every** cap to `0.00`: `metered_monthly_cap`, `metered_daily_cap`, `classifier_monthly_subcap`, `default_task_cap`, `research_task_cap`, `coding_task_cap`. Setting only the daily cap to `0` fails startup validation (a task cap above the daily cap is rejected), so all six must change together. Restart `pair-api` (caps are read once at startup; there is no live reload; a baked image needs a rebuilt or re-mounted file).

Effect: every new `POST /v1/turn` and `POST /v1/budget/reserve` is refused with `budget_exceeded` (HTTP 402) before any provider call. In-flight turns finish and settle normally (a turn outlives its HTTP request, so a client timeout does not stop it). Reservations already unresolved stay counted. A turn cut off between reserve and reconcile (process killed, task dropped) is settled as `unresolved`, never left `held`.

Proof: `crates/api/tests/kill_switch.rs::zeroing_the_shipped_budget_yaml_as_the_runbook_says_refuses_turns_and_adapter_reserves` (performs this exact edit on the shipped file), `turn.rs::caps_to_zero_refuses_new_turns`.

Adapter path (proven live, `wired.sh` phase 7): with every cap at `0.00` the adapter's reserve returns 402 `budget_exceeded`, the run is blocked at `before_agent_run`, and the model server saw zero requests. Not covered: spend already incurred; OpenClaw model calls that bypass the adapter (hook not loaded, no conversation opt-in); classifier calls made by other processes.

### Level 2: interrupt in-flight jobs (NOT EFFECTIVE)

There is no interrupt command, and no workflow step handler is registered in `pair-api`, so there are no PAIR-driven jobs to interrupt. The pieces a future command needs exist in the jobs crate: `JobStore::operator_interrupt` marks a running run interrupted on purpose, and the sweeper does not undo it (it requeues only crash interruptions, and fails a run that has been interrupted 5 times instead of looping). Nothing calls `operator_interrupt` yet, so do not rely on this level; `resume` is how an interrupted run goes back to the queue.

### Level 3: rotate `PAIR_SERVICE_TOKEN` (EFFECTIVE for callers of `pair-api`; proven)

Set a new `PAIR_SERVICE_TOKEN` (at least 16 characters) and restart `pair-api`. The old token is refused on every endpoint except `/healthz`. Also rotate `PAIR_APPROVER_TOKEN` if approvals may have been minted improperly; with it unset, no approval can be created at all.

Proof: `kill_switch.rs::rotated_service_token_locks_out_the_old_token`, `turn.rs::turn_requires_service_token`.

Adapter path (proven live, `wired.sh` phase 6): after rotation, the adapter (still holding the old token) gets HTTP 401 on `/v1/policy/authorize` and `/v1/budget/reserve`, so tool calls are blocked and new runs are refused before any model call. The adapter is configured through `PAIR_SERVICE_TOKEN` in the gateway's environment: update it and restart the gateway to restore service. Not covered: a run already past `before_agent_run` completes its model calls (its tool calls are blocked; a later attempt of that run is blocked at `before_agent_run` when its reservation was already reconciled); the same `allowConversationAccess` caveat as above.

## Controls outside PAIR

**A. Cut the money at the provider (works regardless of PAIR state)**
In each provider console (Anthropic, Ollama Cloud, TypeSafe): revoke or rotate the API key, and lower the workspace spend limit to the minimum. This is the only control that holds if PAIR or the host is compromised. Never enable auto top-up.

**B. Stop the processes**
- API: `docker compose -f deploy/compose.yaml --profile app stop pair-api`. SIGTERM stops accepting requests, waits up to `PAIR_SHUTDOWN_DRAIN_SECS` (default 20) for in-flight turns (they outlive their HTTP request) to finish and settle, and signals the background tasks, which may finish their current iteration for up to the same time before they are aborted. A turn still running when the process ends keeps its reservation counted: `held` until reconciled by hand, or `unresolved` if the drop guard ran ([reservation-reconciliation](reservation-reconciliation.md)).
- Tool worker, if any is running: `docker kill $(docker ps -q --filter name=pair-worker)`. Containers started with `docker compose run --rm` die with the kill.
- OpenClaw gateway: stop the process however it is launched on the host (service manager or container). This is what actually stops agent turns and tool calls.
- `docker compose stop` for the whole stack if unsure. Database data is preserved (named volume).

**C. Cut credentials held on the host**
- Remove provider keys (`ANTHROPIC_API_KEY`, `OLLAMA_API_KEY`, `TYPESAFE_API_KEY`) from `.env` and restart `pair-api`: it then has no provider adapter and every model call fails closed.
- Disconnect personal integrations (Gmail/Calendar OAuth grants) at the provider's account security page; external writes are disabled by default and must stay so.

**D. Isolate the host**
Remove ingress at the reverse proxy or firewall. The database is already bound to loopback in compose.

## Evidence to keep
Before restarting anything that wipes logs, save:
- `docker logs` of every container, and the name of the last backup;
- the provider console usage and billing pages for the window (the authoritative record of what was spent);
- OpenClaw gateway logs and, if the spike plugin was loaded, its `hooks.jsonl` / `usage.jsonl`;
- rows from the tables that exist (`budget_reservations`, `budget_ledger`, `model_calls`, `task_model_attempts`, `audit_events`, `approvals`, `workflow_runs`, `effect_intents`), selected by timestamp.

Turns carry one `trace_id` through `messages`, `model_calls` and `audit_events`. Do **not** assume every model call, tool run and approval can be joined by `trace_id`: spec gate 8 (traceability) is unmeasured, approvals and tool runs carry no correlation id yet, and OpenClaw's own calls never reach these tables.

## Restore service
1. Root cause understood, provider keys rotated, spend limits reviewed.
2. Restore the caps in `config/budget.yaml` and restart.
3. Reopen in order: database and `pair-api` (read-only use), then OpenClaw, then integrations.
4. Reconcile reservations ([reservation-reconciliation](reservation-reconciliation.md)): failed provider calls and a failed classifier call leave their full reservation `unresolved`, still counted against the caps.
5. Turn the incident into a policy/injection evaluation case.

**Unverified:** none of the above has been exercised on the real host. There is no single in-app kill command; candidate follow-up: a `PAIR_KILL_SWITCH` flag read by the reserve path so the caps need not be edited.
