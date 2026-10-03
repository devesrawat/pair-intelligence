# PAIR

PAIR (Personal AI Runtime) is a single-user, cloud-only AI runtime: a Rust service that sits between an agent front end (OpenClaw, via an adapter) and cloud model providers, and enforces what the model is allowed to do and spend.

It is not a finished product. [`docs/STATUS.md`](docs/STATUS.md) is the honest snapshot of what is built, what is tested, and what is not. None of the release gates in the spec has been measured yet.

## What it does

- **Budget** — every model call reserves against a Postgres ledger (per-task, daily and monthly caps), then settles. Unknown price means no paid execution.
- **Routing** — an optional Jev classifier (shadow mode by default) recommends a tier; a config-driven router picks the model and fallbacks.
- **Policy** — a policy engine authorizes tool actions; untrusted content can never become instructions.
- **Providers** — Anthropic API, Ollama Cloud, and Claude through the `claude` CLI on your subscription. There is deliberately no local-model fallback.
- **Audit** — one trace id per turn across messages, model calls, approvals and tool runs.

## Model auth: API key or subscription

| Mode | Set | Billing |
|---|---|---|
| Anthropic API key | `ANTHROPIC_API_KEY` | Metered, dollar-budgeted |
| Ollama Cloud | `OLLAMA_API_KEY` | Metered, or `billing: subscription` for a plan |
| Claude subscription | `PAIR_CLAUDE_CODE=1` (and a logged-in `claude` CLI) | Zero marginal cost; bounded by the limits the CLI reports |

The subscription mode runs the unmodified `claude` CLI headless. Anthropic's published terms allow that for **one owner, on their own subscription, on their own host**. It is not for a service used by other people. Details and sources: [`docs/provider-billing.md`](docs/provider-billing.md).

## Layout

```
service/crates/   Rust workspace: core, policy, budget, models, memory, context,
                  jobs, workflows, telemetry, api, ops
migrations/       PostgreSQL migrations
config/           models, budget, policy, routing, alerts, API settings (YAML)
deploy/           Docker Compose for dev and production
adapters/openclaw OpenClaw plugin and live harness
scripts/          check, backup, restore, evaluate
docs/             spec, status, runbooks, review findings
```

OpenClaw is pinned at `v2026.9.7` in `upstream/` (git-ignored; see [`docs/upstream-audit.md`](docs/upstream-audit.md)).

## Develop

Requires Rust (stable), Docker (for the dev Postgres), and `openssl` for tokens.

```bash
cp .env.example .env                       # then fill in what you use
docker compose -f deploy/compose.dev.yaml up -d --wait postgres
cd service
cargo test --workspace                     # needs the dev Postgres on :55432
cargo clippy --workspace --all-targets -- -D warnings
cd .. && scripts/check smoke               # boots the real pair-api binary and probes it
```

Run the API:

```bash
export PAIR_SERVICE_TOKEN=$(openssl rand -hex 32)   # required, >= 16 chars
cargo run -p pair-api --manifest-path service/Cargo.toml
```

Every setting is documented in [`config/api.yaml`](config/api.yaml). Live provider tests are `#[ignore]`d; for example `cargo test -p pair-models --test live_smoke live_claude_code_smoke -- --ignored` makes one real call through your logged-in `claude` CLI.

## Docs

- [`docs/spec.md`](docs/spec.md): specification and plan
- [`docs/STATUS.md`](docs/STATUS.md): current state, owner-gated items, release gates
- [`docs/runbooks/`](docs/runbooks/): kill switch, backup and restore, provider outage, rollback

## License

MIT. See [`LICENSE`](LICENSE).
