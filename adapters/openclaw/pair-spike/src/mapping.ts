/**
 * OpenClaw tool call -> PAIR ActionRequest. Explicit table; anything not in it is denied, and
 * anything in it that cannot be mapped faithfully (shell metacharacters, unknown exec options)
 * is denied too: the service must judge exactly what will run, never a lossy summary of it.
 */

import { mapGit } from "./git-mapping.ts";

export type PairTool = "shell.exec" | "git.push" | "fs.read" | "fs.write" | "fs.edit" | "web.search" | "web.fetch";

/** The only OpenClaw tools the adapter will submit to PAIR. Everything else is denied. */
export const TOOL_MAP: ReadonlyMap<string, PairTool> = new Map<string, PairTool>([
  ["exec", "shell.exec"],
  ["read", "fs.read"],
  ["ls", "fs.read"],
  ["write", "fs.write"],
  ["edit", "fs.edit"],
  ["web_search", "web.search"],
  ["web_fetch", "web.fetch"],
]);

export type ActionBody = {
  readonly tool: PairTool;
  readonly executable?: string;
  readonly args: readonly string[];
  readonly paths: readonly string[];
  readonly destination?: string;
};

export type MapResult =
  | { readonly ok: true; readonly action: ActionBody }
  | { readonly ok: false; readonly reason: string };

/**
 * exec options that do not change what runs (title, timing, pty). Any other key is unmappable.
 * `background` and a non-positive `timeoutSeconds` remove the time limit, so they are checked by
 * value in `execOptionsDenial`.
 */
const EXEC_BENIGN_KEYS: ReadonlySet<string> = new Set([
  "command",
  "title",
  "yieldMs",
  "background",
  "timeoutSeconds",
  "pty",
]);

/** zsh expands an unquoted word starting with `=` to a command path (`cat =ssh`). */
const ZSH_EQUALS_EXPANSION = "=";
const DOT_GIT = ".git";
const PATH_COMPONENT_SPLIT = /[\\/:=,]/;

/** Characters that let a shell run, redirect or expand something the tokenizer cannot see. */
const UNQUOTED_FORBIDDEN = new Set([";", "&", "|", "<", ">", "(", ")", "`", "$", "\\", "\n", "\r", "{", "}", "*", "?", "[", "]", "~", "#", "!"]);
const DOUBLE_QUOTE_FORBIDDEN = new Set(["$", "`", "\\", "!"]);

const deny = (reason: string): MapResult => ({ ok: false, reason });

/** Split a simple command line into words. Returns undefined when it is not a plain, expansion-free command. */
export function tokenizeSimpleCommand(command: string): readonly string[] | undefined {
  const words: string[] = [];
  let current = "";
  let inWord = false;
  let quote: "'" | '"' | undefined;
  for (const ch of command) {
    if (quote === "'") {
      if (ch === "'") quote = undefined;
      else current += ch;
    } else if (quote === '"') {
      if (ch === '"') quote = undefined;
      else if (DOUBLE_QUOTE_FORBIDDEN.has(ch)) return undefined;
      else current += ch;
    } else if (ch === "'" || ch === '"') {
      quote = ch;
      inWord = true;
    } else if (ch === " " || ch === "\t") {
      if (inWord) words.push(current);
      current = "";
      inWord = false;
    } else if (UNQUOTED_FORBIDDEN.has(ch) || (ch === ZSH_EQUALS_EXPANSION && !inWord)) {
      return undefined;
    } else {
      current += ch;
      inWord = true;
    }
  }
  if (quote !== undefined) return undefined;
  if (inWord) words.push(current);
  return words;
}

function stringParam(params: Readonly<Record<string, unknown>>, key: string): string | undefined {
  const value = params[key];
  return typeof value === "string" && value.trim().length > 0 ? value : undefined;
}

function pathParam(params: Readonly<Record<string, unknown>>): string | undefined {
  return stringParam(params, "path") ?? stringParam(params, "file_path");
}

/** True when any `/`, `\`, `:`, `=` or `,` separated piece of `value` is `.git` (case-insensitive). */
export function hasDotGitComponent(value: string): boolean {
  return value.split(PATH_COMPONENT_SPLIT).some((piece) => piece.toLowerCase() === DOT_GIT);
}

/** Reason when an exec option removes the time limit or detaches the process; undefined when benign. */
function execOptionsDenial(params: Readonly<Record<string, unknown>>): string | undefined {
  const background = params["background"];
  if (background !== undefined && background !== false) return "exec background is not allowed";
  const timeout = params["timeoutSeconds"];
  if (timeout !== undefined && !(typeof timeout === "number" && Number.isFinite(timeout) && timeout > 0)) {
    return "exec timeoutSeconds must be a positive number";
  }
  return undefined;
}

function mapExec(params: Readonly<Record<string, unknown>>): MapResult {
  const extra = Object.keys(params).filter((k) => !EXEC_BENIGN_KEYS.has(k));
  if (extra.length > 0) return deny(`exec option not mappable: ${extra.sort().join(",")}`);
  const optionsDenial = execOptionsDenial(params);
  if (optionsDenial !== undefined) return deny(optionsDenial);
  const command = stringParam(params, "command");
  if (command === undefined) return deny("exec without a command");
  const words = tokenizeSimpleCommand(command);
  if (words === undefined) return deny("exec command is not a plain command (shell syntax)");
  const [executable, ...args] = words;
  if (executable === undefined || executable === "") return deny("exec command has no executable");
  if (words.some(hasDotGitComponent)) return deny("exec argument names a .git path");
  if (executable.slice(executable.lastIndexOf("/") + 1) === "git") return mapGit(executable, args);
  return { ok: true, action: { tool: "shell.exec", executable, args, paths: [] } };
}

function mapFs(tool: PairTool, params: Readonly<Record<string, unknown>>, pathOptional: boolean): MapResult {
  const path = pathParam(params);
  if (path === undefined) {
    return pathOptional
      ? { ok: true, action: { tool, args: [], paths: [] } }
      : deny(`${tool} without a path`);
  }
  if (hasDotGitComponent(path)) return deny(`${tool} path names a .git component`);
  return { ok: true, action: { tool, args: [], paths: [path] } };
}

/** Map one OpenClaw tool call. Pure: no I/O, never throws on hostile input shapes. */
export function mapToolCall(toolName: string, params: Readonly<Record<string, unknown>>): MapResult {
  const tool = TOOL_MAP.get(toolName);
  if (tool === undefined) return deny(`tool '${toolName}' has no PAIR mapping`);
  switch (toolName) {
    case "exec":
      return mapExec(params);
    case "read":
    case "write":
    case "edit":
      return mapFs(tool, params, false);
    case "ls":
      return mapFs(tool, params, true);
    case "web_search":
      // The search backend is chosen by OpenClaw, so there is no destination to egress-check.
      return deny("web_search has no destination the egress allowlist can check");
    case "web_fetch": {
      const url = stringParam(params, "url");
      return url === undefined ? deny("web_fetch without a url") : { ok: true, action: { tool, args: [], paths: [], destination: url } };
    }
    default:
      return deny(`tool '${toolName}' has no PAIR mapping`);
  }
}
