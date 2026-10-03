/**
 * OpenClaw tool call -> PAIR ActionRequest. Explicit table; anything not in it is denied, and
 * anything in it that cannot be mapped faithfully (shell metacharacters, unknown exec options)
 * is denied too: the service must judge exactly what will run, never a lossy summary of it.
 */

export type PairTool = "shell.exec" | "fs.read" | "fs.write" | "fs.edit" | "web.search" | "web.fetch";

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

/** exec options that do not change what runs (title, timing, pty). Any other key is unmappable. */
const EXEC_BENIGN_KEYS: ReadonlySet<string> = new Set([
  "command",
  "title",
  "yieldMs",
  "background",
  "timeoutSeconds",
  "pty",
]);

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
    } else if (UNQUOTED_FORBIDDEN.has(ch)) {
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

function mapExec(params: Readonly<Record<string, unknown>>): MapResult {
  const extra = Object.keys(params).filter((k) => !EXEC_BENIGN_KEYS.has(k));
  if (extra.length > 0) return deny(`exec option not mappable: ${extra.sort().join(",")}`);
  const command = stringParam(params, "command");
  if (command === undefined) return deny("exec without a command");
  const words = tokenizeSimpleCommand(command);
  if (words === undefined) return deny("exec command is not a plain command (shell syntax)");
  const [executable, ...args] = words;
  if (executable === undefined || executable === "") return deny("exec command has no executable");
  return { ok: true, action: { tool: "shell.exec", executable, args, paths: [] } };
}

function mapFs(tool: PairTool, params: Readonly<Record<string, unknown>>, pathOptional: boolean): MapResult {
  const path = pathParam(params);
  if (path === undefined) {
    return pathOptional
      ? { ok: true, action: { tool, args: [], paths: [] } }
      : deny(`${tool} without a path`);
  }
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
      return { ok: true, action: { tool, args: [], paths: [] } };
    case "web_fetch": {
      const url = stringParam(params, "url");
      return url === undefined ? deny("web_fetch without a url") : { ok: true, action: { tool, args: [], paths: [], destination: url } };
    }
    default:
      return deny(`tool '${toolName}' has no PAIR mapping`);
  }
}
