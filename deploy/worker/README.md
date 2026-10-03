# PAIR isolated worker

Container profile for the process that runs tool closures behind `pair_policy::Gate`
(spec section 9). Policy decides what may run; this profile limits the damage if a
decision was wrong.

| Control | Setting |
|---|---|
| Root filesystem | `read_only: true`; `/tmp` is a 128 MB `noexec` tmpfs |
| Host mounts | None in the compose profile (one named volume at `/workspace`). `pair-workflows` mounts only the task worktree, see below |
| Docker socket | Never mounted. Do not add it, and do not add `privileged`, `pid: host`, or `network_mode: host` |
| Network | `network_mode: none` |
| Privileges | `cap_drop: [ALL]`, `no-new-privileges`, non-root uid 10001 |
| Resources | 256 pids, 1 GiB memory (swap equal), 1 CPU, 1 GiB max file size |
| Environment | Fixed two variables. No `env_file`, no pass-through, no cloud admin credentials |

## How `pair-workflows` uses this profile

`ContainerSandbox` (the default for `Runner`) runs every build/test/acceptance command with
`docker run --rm` on `$PAIR_WORKER_IMAGE`, using exactly the settings above (network none,
read-only root, `cap-drop ALL`, `no-new-privileges`, pid/memory/ulimit limits, uid 10001,
128 MB noexec tmpfs at `/tmp`, `HOME=/tmp/home`). The `compose_profile_matches_sandbox_args`
test fails if this file and the generated arguments drift apart.

Differences from `compose run`, all deliberate:

- The task worktree is bind-mounted read-write at **its own host path** (so argv and cwd are
  identical inside and outside), and it is the ONLY mount. The original repository, its `.git`
  and its object store are never mounted, so code in the container cannot read history
  (`git show HEAD:.env`) or write hooks/config. The worktree's `.git` is only a pointer file
  that dangles inside the container; git-dependent build steps there will fail by design.
  Orchestrator git (diff, add, ls-files) runs on the host, pinned to the git directory resolved
  at worktree creation, with hooks/fsmonitor disabled.
- On Linux the bind-mounted worktree must be writable by uid 10001 (chown it or use a userns
  mapping); on Docker Desktop this is handled by the VM.
- A timed-out or abandoned command is stopped with `docker kill <name>`; docker daemon errors
  (exit 125) surface as environment failures, never as test failures.
- With `PAIR_WORKER_IMAGE` unset, commands are refused (fail closed). Host execution exists
  only as `HostSandbox`, refused unless `PAIR_ALLOW_HOST_EXEC=1`, and is meant for tests.
- `PolicyContext.workspace_root` is the host directory that contains the repo and the task
  worktrees (the policy runs on the orchestrator host, not inside the container).

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
