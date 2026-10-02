# Runbook: rollback

Images use immutable tags (spec §11). Never deploy `latest`; set `PAIR_IMAGE_TAG` to a git SHA or release number. Keep the previous two tags on the host.

## Before every release
1. Record the running tag: `docker compose -f deploy/compose.yaml --profile app images`.
2. `scripts/backup` and note the file name in the release record.
3. Check the new release's migrations are expand-only (additive). Destructive changes are shipped one release after the code stops using them.

## Roll back the application (no schema change, or additive-only change)
1. `PAIR_IMAGE_TAG=<previous> docker compose -f deploy/compose.yaml --profile app up -d --wait pair-api`
2. `/healthz` 200, `/readyz` ready. The previous binary ignores newer migration rows it does not know; with additive migrations this is safe. Confirm with unauthenticated `/readyz` = 401, authenticated = 200.
3. Record the incident and the bad tag. Do not delete the bad image until the postmortem.

## Roll back after a destructive or data-corrupting release
1. Stop `pair-api` and workers so nothing writes.
2. Restore the pre-release backup into a scratch DB and verify ([backup-restore](backup-restore.md)).
3. Switch to the restored DB; start the previous tag.
4. Data written between the backup and the rollback is lost; list affected jobs from logs and reconcile reservations ([reservation-reconciliation](reservation-reconciliation.md)).

## OpenClaw / adapter upgrades
Pinned in `docs/upstream-audit.md`. Roll back by redeploying the previous pin; PAIR state lives in the companion service and is unaffected.

## Verify
`/readyz` ready, one authenticated request succeeds with a fresh `X-Trace-Id`.

**Unverified:** the `--profile app` image has not been built or run on a real host yet.
