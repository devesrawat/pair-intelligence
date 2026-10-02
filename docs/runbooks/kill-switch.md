# Runbook: kill switch

Use when: spend is running away, a prompt-injection or policy bypass is suspected, a credential may be exposed, or an external mutation happened without approval. Act first, diagnose after.

Model output cannot override permissions or budgets (spec global constraints); this runbook is the human override.

## Levels (stop at the lowest level that contains the problem)

**1. Freeze spend only**
Set `metered_daily_cap` and `metered_monthly_cap` to `0` in `config/budget.yaml` and restart `pair-api`. New reservations are refused (`budget_exceeded`); reads, memory, and approvals keep working. Undo by restoring the values via a reviewed commit. Never use auto top-up (`auto_top_up: false` stays).

**2. Stop all model calls and background work**
`docker compose -f deploy/compose.yaml --profile app stop pair-api` and stop the worker containers. Jobs in flight become `interrupted` and resume only when you restart them. Queued work is preserved.

**3. Cut credentials**
- Rotate `PAIR_SERVICE_TOKEN` in `.env` (>= 16 chars, random) and restart: every caller, including the OpenClaw adapter, is rejected until given the new token.
- Revoke or rotate provider API keys in each provider console, and lower the provider-side spend limit to the minimum. This is the only control that works if PAIR itself is compromised.
- Disconnect personal integrations; external writes are disabled by default and must stay so.

**4. Isolate the host**
Remove ingress at the reverse proxy / firewall. The database is already private (loopback only in compose).

## Evidence to keep
Before restarting anything that wipes logs: save `docker logs` for every container, the last backup name, and `SELECT * FROM` approvals / tool-execution / model-call records for the window (every call, route, tool run, and approval is traceable by `trace_id`).

## Restore service
1. Root cause understood, credentials rotated, caps reviewed.
2. Reopen in order: DB and `pair-api` (read-only use), then workers, then integrations.
3. Reconcile reservations ([reservation-reconciliation](reservation-reconciliation.md)).
4. Turn the incident into a policy/injection evaluation case.

**Unverified:** no single in-app kill command exists yet; levels 1-4 are manual. Candidate follow-up: a `PAIR_KILL_SWITCH` flag read by the budget reserve path.
