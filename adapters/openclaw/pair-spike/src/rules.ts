export type ToolDecision =
  | { readonly kind: "allow" }
  | { readonly kind: "deny"; readonly reason: string }
  | { readonly kind: "approve"; readonly title: string; readonly description: string };

/** Tools that run without approval. Everything not named here or in APPROVAL_TOOLS is denied. */
export const ALLOWED_TOOLS: ReadonlySet<string> = new Set([
  "read",
  "sessions_list",
  "sessions_history",
  "sessions_search",
  "image",
]);

/** Tools that reach the network: allowed only after human approval. */
export const APPROVAL_TOOLS: ReadonlySet<string> = new Set(["web_search", "web_fetch"]);

/** `exec` is allow-listed as a tool but every command is inspected (see `inspectCommand`). */
const EXEC_TOOL = "exec";

const COMMAND_PREVIEW_CHARS = 120;
const NETWORK_COMMAND = /(^|[\s;&|(`])(curl|wget|nc|ncat|ssh|scp|rsync|ftp|sftp)(\s|$)/;
/** Separators that start a new simple command (also `$(`, backticks and subshell parens). */
const SEGMENT_SPLIT = /&&|\|\||[;&|\n`()]|\$\(/;
const BLOCK_DEVICE_REDIRECT = /(^|[^<])>>?\s*\/dev\/(sd|nvme|disk|hd|vd|mmcblk)/;
const ROOT_TARGETS: ReadonlySet<string> = new Set(["/", "/*", "--no-preserve-root"]);

function commandOf(params: Readonly<Record<string, unknown>>): string | undefined {
  const value = params["command"];
  return typeof value === "string" && value.trim().length > 0 ? value : undefined;
}

/** Executable name of a word: strips quotes, a leading backslash (alias bypass) and the directory. */
function baseName(token: string): string {
  const stripped = token.replace(/^[\\'"]+|['"]+$/g, "");
  const slash = stripped.lastIndexOf("/");
  return slash >= 0 ? stripped.slice(slash + 1) : stripped;
}

function hasFlag(args: readonly string[], shortLetters: string, long: string): boolean {
  return args.some((a) => {
    if (a === "--") return false;
    if (a.startsWith("--")) return a === long;
    return a.startsWith("-") && [...a.slice(1)].some((c) => shortLetters.includes(c));
  });
}

/** Reason when `words` (starting at a candidate executable) is destructive; undefined otherwise. */
function destructiveReason(words: readonly string[]): string | undefined {
  const exe = baseName(words[0] ?? "");
  const args = words.slice(1);
  if (exe === "rm" && hasFlag(args, "rR", "--recursive")) return "recursive rm";
  if (exe === "rm" && args.some((a) => ROOT_TARGETS.has(a.replace(/^['"]+|['"]+$/g, "")))) {
    return "rm of root";
  }
  if (exe === "find" && args.some((a) => a === "-delete" || a === "-fdelete")) return "find -delete";
  if (exe === "dd" && args.some((a) => a.startsWith("of="))) return "dd of=";
  if (exe.startsWith("mkfs")) return "mkfs";
  if (exe === "wipefs" || exe === "shred") return exe;
  return undefined;
}

/** Deny reason for a destructive command, or undefined when none of the checks match. */
export function inspectCommand(command: string): string | undefined {
  if (BLOCK_DEVICE_REDIRECT.test(command)) return "redirect to block device";
  for (const segment of command.split(SEGMENT_SPLIT)) {
    const words = segment.trim().split(/\s+/).filter((w) => w.length > 0);
    // Any word may be the executable (`sudo -u x rm -rf`, `xargs rm -rf`, `bash -c "rm -rf x"`):
    // check every position. Over-matching (`echo rm -rf`) errs on the side of denying.
    for (let i = 0; i < words.length; i += 1) {
      const reason = destructiveReason(words.slice(i));
      if (reason !== undefined) return reason;
    }
  }
  return undefined;
}

/** Pure policy: tool-name allow-list with default deny; destructive exec denied; network needs approval. */
export function decideToolCall(
  toolName: string,
  params: Readonly<Record<string, unknown>>,
): ToolDecision {
  if (ALLOWED_TOOLS.has(toolName)) return { kind: "allow" };
  if (APPROVAL_TOOLS.has(toolName)) {
    return {
      kind: "approve",
      title: `PAIR spike: ${toolName}`,
      description: `${toolName} wants network access`,
    };
  }
  if (toolName !== EXEC_TOOL) {
    return { kind: "deny", reason: `pair-spike: tool '${toolName}' is not on the allow-list` };
  }
  const command = commandOf(params);
  if (command === undefined) {
    return { kind: "deny", reason: "pair-spike: exec without a command is denied" };
  }
  if (inspectCommand(command) !== undefined) {
    return { kind: "deny", reason: "pair-spike: destructive command denied by policy" };
  }
  if (NETWORK_COMMAND.test(command)) {
    return {
      kind: "approve",
      title: "PAIR spike: network command",
      description: `exec wants to run a network command: ${command.slice(0, COMMAND_PREVIEW_CHARS)}`,
    };
  }
  return { kind: "allow" };
}
