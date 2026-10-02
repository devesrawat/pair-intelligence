# Routing evaluation (spec 6.1, Task 10)

## Status

- `datasets/routing.jsonl`: 100 cases, distribution 30 coding / 20 research / 15 planning / 15 recall / 10 transformation / 10 mixed-or-uncertain.
  Split 60 `dev` / 40 `held_out`, stratified per category.
- **Every label is `label_status: draft_unreviewed`.** The owner must review intent, difficulty, workflow, data class and acceptance check before any number below means anything.
- `datasets/routing.held_out.sha256` freezes the held-out lines. `held_out_split_is_frozen` fails if they change. Editing them creates a new evaluation, not a tuned one. Calibrate thresholds on `dev` only.
- No accuracy, cost saving or quality claim exists for Jev. Nothing has been run against the live API.

## Run

```bash
scripts/evaluate baseline   # fixed baseline + deterministic rules; no network
scripts/evaluate jev        # additionally calls Jev; requires TYPESAFE_API_KEY (owner's key, env only)
scripts/check routing       # clippy + tests for the router and classifier
```

`baseline` reports intent/difficulty agreement with the **draft** labels, **modeled** generation cost (placeholder prices in `config/models.yaml`, 500 assumed output tokens, no retries) and local compute latency. The fixed baseline predicts no labels, so its accuracy is N/A. `jev` sends only `data_class: public` cases to the vendor and reports classifier latency, classifier cost from returned usage, and agreement with draft labels. It does **not** measure downstream acceptance.

## What still needs the owner

1. Review and correct the 100 labels; then re-freeze the held-out digest deliberately.
2. Create the TypeSafe key and set a prepaid balance/spend cap (docs/provider-billing.md). Verify `jev-1.13.0` direct pinning and retention.
3. Run each routed task through the real downstream workflow and record acceptance. Compare strategies on accepted-task cost including retries, p50/p95 latency and escalation frequency.
4. Calibrate per-question thresholds for the exact (returned model, `questionVersion`) pair on `dev`; put them in `routing.thresholds`. Without an entry the router uses the baseline.

## 7-day shadow pilot (out of scope for the build; how to run it)

1. Set `routing.classifier.mode: shadow` and export `TYPESAFE_API_KEY`. Shadow records the recommendation and executes the baseline; it never changes the selected tier.
2. Run for at least 7 days **and** 100 eligible real requests (extend if traffic is lower). Store normalised outputs, returned `model`, request IDs, latency and cost. State text is stored only as length and SHA-256.
3. Label the shadowed requests, run the downstream acceptance check for both the baseline route and the recommended route (replay), and compute the gates below.

## Activation gates (spec 6.1; engineering gates, not statistical proof)

All must hold on held-out cases and shadow traffic before adding a category to `routing.classifier.active_categories` and switching `mode: active`:

- at least 90% intent accuracy on held-out cases (40 cases is too few for strong generalization claims);
- no policy bypass (hard constraints always run first; classifier output cannot grant anything);
- no loss of observed downstream acceptance versus the baseline;
- at least 15% lower variable cost per accepted task;
- classifier p95 within 1 second on representative traffic.

Activate only validated categories behind the flag with a one-click return to `shadow`/`disabled`. Revert on any policy violation, repeated under-routing, or a rolling accepted-task cost increase. Record rejected routing changes as results under `evals/results/`; do not tune on the held-out set.
