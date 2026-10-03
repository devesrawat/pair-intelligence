/**
 * `git` is the one executable the adapter parses itself. pair-api classifies by PAIR tool name
 * alone, so `git push` sent as `shell.exec` (class local_edit) would skip approval and egress
 * checks. The adapter therefore either maps a git command to the right PAIR tool or denies it.
 */
import type { ActionBody, MapResult } from "./mapping.ts";

/** Read and local-commit subcommands only. Everything else (fetch, clone, config, ...) is denied. */
export const GIT_ALLOWED_SUBCOMMANDS: ReadonlySet<string> = new Set([
  "status",
  "diff",
  "log",
  "show",
  "add",
  "commit",
  "rev-parse",
  "ls-files",
]);

/** The only options accepted before the subcommand. `-c`, `-C`, `--exec-path`, ... are not here. */
const GIT_SAFE_GLOBAL_FLAGS: ReadonlySet<string> = new Set(["--no-pager", "--no-optional-locks"]);

/** Flags that make git run or write something the subcommand name does not show; denied anywhere. */
const GIT_FORBIDDEN_FLAGS: readonly string[] = [
  "--upload-pack",
  "--receive-pack",
  "--exec-path",
  "--config-env",
  "--config",
  "--ext-diff",
  "--textconv",
  "--output",
  "--open-files-in-pager",
  "--git-dir",
  "--work-tree",
  "--exec",
  "--repo",
];

/** Push flags the adapter forwards. Force, delete, prune and value-taking flags are denied locally. */
const GIT_PUSH_FLAGS: ReadonlySet<string> = new Set([
  "-u",
  "--set-upstream",
  "-n",
  "--dry-run",
  "-q",
  "--quiet",
  "-v",
  "--verbose",
  "--tags",
  "--follow-tags",
  "--no-verify",
  "--atomic",
]);

const PUSH = "push";
const deny = (reason: string): MapResult => ({ ok: false, reason });

function isForbiddenFlag(arg: string): boolean {
  return GIT_FORBIDDEN_FLAGS.some((flag) => arg === flag || arg.startsWith(`${flag}=`));
}

/** Map `git <args>` (executable already known to be git) to a PAIR action, or deny it. */
export function mapGit(executable: string, args: readonly string[]): MapResult {
  let index = 0;
  for (; index < args.length; index += 1) {
    const arg = args[index] ?? "";
    if (!arg.startsWith("-")) break;
    if (!GIT_SAFE_GLOBAL_FLAGS.has(arg)) return deny("git option before the subcommand is not allowed");
  }
  const subcommand = args[index];
  if (subcommand === undefined) return deny("git without a subcommand");
  if (args.some(isForbiddenFlag)) return deny("git flag that changes config or execution is not allowed");
  if (subcommand === PUSH) return mapPush(executable, args, index);
  if (!GIT_ALLOWED_SUBCOMMANDS.has(subcommand)) return deny("git subcommand is not on the allow-list");
  return { ok: true, action: { tool: "shell.exec", executable, args, paths: [] } };
}

function mapPush(executable: string, args: readonly string[], subIndex: number): MapResult {
  let remote: string | undefined;
  for (const arg of args.slice(subIndex + 1)) {
    if (arg.startsWith("-")) {
      if (!GIT_PUSH_FLAGS.has(arg)) return deny("git push flag is not allowed");
    } else if (remote === undefined) {
      remote = arg;
    }
  }
  if (remote === undefined) return deny("git push without an explicit remote cannot be egress-checked");
  const action: ActionBody = { tool: "git.push", executable, args, paths: [], destination: remote };
  return { ok: true, action };
}
