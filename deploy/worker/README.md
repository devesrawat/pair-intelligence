# PAIR isolated worker

Container profile for the process that runs tool closures behind `pair_policy::Gate`
(spec section 9). Policy decides what may run; this profile limits the damage if a
decision was wrong.

| Control | Setting |
|---|---|
| Root filesystem | `read_only: true`; `/tmp` is a 128 MB `noexec` tmpfs |
| Host mounts | None. One named volume at `/workspace`, one volume per task |
| Docker socket | Never mounted. Do not add it, and do not add `privileged`, `pid: host`, or `network_mode: host` |
| Network | `network_mode: none` |
| Privileges | `cap_drop: [ALL]`, `no-new-privileges`, non-root uid 10001 |
| Resources | 256 pids, 1 GiB memory (swap equal), 1 CPU, 1 GiB max file size |
| Environment | Fixed two variables. No `env_file`, no pass-through, no cloud admin credentials |

The workspace root passed in `PolicyContext.workspace_root` must be `/workspace`.

## Process lifetime

Run with `docker compose run --rm` under a hard wall-clock limit set by the caller
(spec section 10: 15 minutes interactive, 30 minutes research). On timeout the caller
kills the container (`docker kill`), and the job is recorded as `interrupted`.

## Network access

The default is no network. If a task needs egress, do not change this file. Attach the
worker to an internal-only network whose only route is an allowlisting proxy that mirrors
the `egress` list in `config/policy.yaml`. Policy still denies destinations not on the list.

## Credentials

Inject narrowly scoped, short-lived credentials only for the call that needs them, through
the runtime. Nothing under `~/.ssh`, `~/.aws`, or similar exists in the container, and
`config/policy.yaml` denies those paths and `docker.sock` in any case.

## Verify

```sh
export PAIR_WORKER_IMAGE=<image> PAIR_TASK_ID=check
docker compose -f deploy/worker/compose.worker.yaml config   # inspect the resolved profile
docker compose -f deploy/worker/compose.worker.yaml run --rm worker sh -c \
  'touch /x; ls /var/run/docker.sock; wget -T2 -qO- http://example.com'
# expect: all three fail (read-only fs, no socket, no network)
```
