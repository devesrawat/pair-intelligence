# Runbook: reservation reconciliation

Budget interface (spec §15): `reserve(TaskId, MaximumCost) -> ReservationId`, `reconcile(ReservationId, UsageReport) -> LedgerEntry`. Money is integer micro-USD. A reservation holds the *maximum* cost until reconciled; unresolved reservations lower available budget and, past the caps in `config/budget.yaml`, block new work. There is no automatic top-up and no automatic cap increase.

**Alert:** `reservation_unresolved` (warn at 30 min, critical at 120 min). Error code `reservation_unresolved` in `config/contracts.json`.

## Causes
- Worker crashed between provider call and reconcile.
- Provider outage or timeout with unknown usage.
- Restore from backup (ledger rows after the backup are missing).

## Procedure
1. List open reservations (table/column names follow the budget migration; verify before running):
   `SELECT id, task_id, max_micro_usd, created_at FROM reservations WHERE state = 'reserved' AND created_at < now() - interval '30 minutes' ORDER BY created_at`
2. For each, find the task's model-call trace. Three outcomes:
   - **Call completed:** reconcile with the reported usage (idempotent; reconciling twice must not double-charge).
   - **Call never started:** release the reservation (reconcile with zero usage).
   - **Unknown:** keep the reservation as the conservative upper bound until the provider usage page can be compared; then reconcile to the provider figure.
3. Compare the month-to-date ledger total with each provider's billing page (see `docs/provider-billing.md`). Differences above 5% are an incident: freeze spend ([kill-switch](kill-switch.md)) and investigate before reopening.
4. Never edit ledger rows by hand to fix totals; add a reconciling entry through the budget interface so the audit trail stays append-only.

## After a restore
Ledger rows newer than the backup are gone. Rebuild the missing spend from provider usage pages as manual reconciliation entries tagged `restore-gap`, so the caps remain conservative.

## Verify
No reservations older than the alert threshold; ledger total within tolerance of provider billing.
