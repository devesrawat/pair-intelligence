# Runbook: kill switch

Use when: spend is running away, a prompt-injection or policy bypass is suspected, a credential may be exposed, or an external mutation happened without approval. Act first, diagnose after.

Model output cannot override permissions or budgets (spec global constraints); this runbook is the human override.

## Read this first: what is actually enforced today

`pair-api` currently serves `/healthz`, `/readyz` and `/v1/whoami` only. It does **not** depend on the budget, jobs, workflows, router, memory or context crates; those exist as libraries and are not wired into the running service. The OpenClaw adapter does not call `pair-api`. Consequences:

- Nothing PAIR hosts today can refuse a model call, cap spend, or interrupt a job, because PAIR does not yet make or schedule them in the running service.
- Controls that live inside PAIR (budget caps, job interruption, adapter token rotation) are marked **NOT EFFECTIVE** below until `pair-api` hosts budget and jobs. Do not rely on them in an incident.
- The controls that work today are outside PAIR: the model provider's console, the container runtime, the host firewall.

## Controls that work today (stop at the lowest level that contains the problem)

**A. Cut the money at the provider (works regardless of PAIR state)**
In each provider console (Anthropic, Ollama Cloud, TypeSafe): revoke or rotate the API key, and lower the workspace spend limit to the minimum. This is the only control that holds if PAIR or the host is compromised. Never enable auto top-up.

**B. Stop the processes**
- API: `docker compose -f deploy/compose.yaml --profile app stop pair-api`
- Tool worker, if any is running: `docker kill $(docker ps -q --filter name=pair-worker)`. Containers started with `docker compose run --rm` die with the kill. A job that was in flight is not marked `interrupted` by anything: the jobs crate is not running to do it.
- OpenClaw gateway: stop the process however it is launched on the host (service manager or container). This is what actually stops agent turns and tool calls.
- `docker compose stop` for the whole stack if unsure. Database data is preserved (named volume).

**C. Cut credentials held on the host**
- Remove provider keys from `.env` and restart nothing that would reload them.
- Disconnect personal integrations (Gmail/Calendar OAuth grants) at the provider's account security page; external writes are disabled by default and must stay so.

**D. Isolate the host**
Remove ingress at the reverse proxy or firewall. The database is already bound to loopback in compose.

## Controls that do NOT work today

| Control | Why it does not work | Becomes effective when |
|---|---|---|
| Set `metered_daily_cap` / `metered_monthly_cap` to `0` in `config/budget.yaml` and restart `pair-api` | `pair-api` does not load `config/budget.yaml` or reserve budget; restarting it changes nothing. **NOT EFFECTIVE** | `pair-api` hosts the budget crate on the model-call path |
| Interrupt in-flight jobs (they "become `interrupted`" and resume on restart) | No jobs worker runs inside `pair-api`; there is nothing to interrupt or resume. **NOT EFFECTIVE** | `pair-api` hosts the jobs crate |
| Rotate `PAIR_SERVICE_TOKEN` to lock out the OpenClaw adapter | The adapter does not call `pair-api` and no sensitive endpoint sits behind the token. Rotation only affects `/readyz` and `/v1/whoami`. **NOT EFFECTIVE** as a kill measure | The adapter is wired to `pair-api` |

If the adapter must be locked out now, stop the OpenClaw gateway (B).

## Evidence to keep
Before restarting anything that wipes logs, save:
- `docker logs` of every container, and the name of the last backup;
- the provider console usage and billing pages for the window (the authoritative record of what was spent);
- OpenClaw gateway logs and, if the spike plugin was loaded, its `hooks.jsonl` / `usage.jsonl`;
- rows from the tables that exist (`budget_ledger`, `approvals`, `workflow_runs`, `effect_intents`), selected by timestamp.

Do **not** assume every model call, tool run and approval can be joined by `trace_id`: spec gate 8 (traceability) is unmeasured, `pair-api` assigns trace ids only to its own requests, and approval and tool-run records carry no correlation id yet.

## Restore service
1. Root cause understood, provider keys rotated, spend limits reviewed.
2. Reopen in order: database and `pair-api` (read-only use), then OpenClaw, then integrations.
3. Reconcile reservations ([reservation-reconciliation](reservation-reconciliation.md)).
4. Turn the incident into a policy/injection evaluation case.

**Unverified:** none of the above has been exercised on the real host. No single in-app kill command exists; candidate follow-up: a `PAIR_KILL_SWITCH` flag read by the budget reserve path once budget is hosted by `pair-api`.
