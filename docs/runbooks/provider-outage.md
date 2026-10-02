# Runbook: provider outage

**Alert:** `provider_outage` (`readyz.checks.providers` = warn). PAIR stays ready; model calls to the affected provider fail.

## Detect
1. `curl -s -H "Authorization: Bearer $PAIR_SERVICE_TOKEN" -H "X-Actor: owner" http://127.0.0.1:8080/readyz | jq '.providers, (.checks[]|select(.name=="providers"))'`
2. Confirm it is the provider and not us: check the provider status page and the last successful call time in the model-call trace (spec §12: every call is traced with latency and retries).

## Contain
- Do **not** raise retry counts. Retries are capped (`execution.max_model_attempts: 3` in `config/budget.yaml`); each attempt reserves budget.
- Routing falls back only to providers already allowed by policy and registry. Never enable a new provider or a local model as an emergency fallback (spec global constraints).
- If all providers are down: leave PAIR up. Memory, approvals, and budget remain available; queued jobs wait (`queued`) or move to `interrupted` and resume when the provider returns.

## Recover
1. When the provider recovers, `/readyz` providers check returns to ok.
2. Check jobs stuck in `interrupted`/`failed` during the window; resume or cancel per the jobs interface. Side-effecting steps use durable intent records, so resume does not repeat them.
3. Reconcile reservations created during the outage: [reservation-reconciliation](reservation-reconciliation.md).

## Follow-up
Record start/end, affected tasks, and cost impact. If the outage exposed a routing gap, add an evaluation case (spec §16 Task 16).
