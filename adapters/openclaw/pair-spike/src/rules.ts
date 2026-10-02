export type ToolDecision =
  | { readonly kind: "allow" }
  | { readonly kind: "deny"; readonly reason: string }
  | { readonly kind: "approve"; readonly title: string; readonly description: string };

const DENY_COMMAND = /\brm\s+-rf\b/;
const APPROVE_COMMAND = /\b(curl|wget)\b/;

function commandOf(params: Readonly<Record<string, unknown>>): string {
  const value = params["command"];
  return typeof value === "string" ? value : "";
}

/** Pure policy: deny destructive exec, require approval for network egress commands. */
export function decideToolCall(
  toolName: string,
  params: Readonly<Record<string, unknown>>,
): ToolDecision {
  if (toolName !== "exec") return { kind: "allow" };
  const command = commandOf(params);
  if (DENY_COMMAND.test(command)) {
    return { kind: "deny", reason: "pair-spike: destructive command denied by policy" };
  }
  if (APPROVE_COMMAND.test(command)) {
    return {
      kind: "approve",
      title: "PAIR spike: network command",
      description: `exec wants to run a network command: ${command.slice(0, 120)}`,
    };
  }
  return { kind: "allow" };
}
