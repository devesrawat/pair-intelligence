// Loopback-only OpenAI-compatible mock. Serves canned streamed chat completions.
// Scenario is chosen by a marker in the latest user message: SCENARIO:deny | SCENARIO:approve | otherwise plain text.
import { appendFileSync } from "node:fs";
import { createServer, type IncomingMessage, type ServerResponse } from "node:http";

const HOST = "127.0.0.1";
const DEFAULT_PORT = 18901;
const FIXED_INPUT_TOKENS = 11;
const FIXED_OUTPUT_TOKENS = 7;
const TOOL_RESULT_PREVIEW_CHARS = 200;
const port = Number(process.env.MOCK_PORT ?? DEFAULT_PORT);
const logPath = process.env.MOCK_LOG ?? "mock-requests.jsonl";

type ChatMessage = { role?: string; content?: unknown };
type ChatRequest = {
  model?: string;
  stream?: boolean;
  messages?: ChatMessage[];
  tools?: Array<{ function?: { name?: string } }>;
};
type Plan = { text: string } | { toolName: string; args: Record<string, unknown> };

function textOf(content: unknown): string {
  if (typeof content === "string") return content;
  if (Array.isArray(content)) {
    return content
      .map((part: unknown) =>
        typeof part === "object" && part !== null && "text" in part
          ? String((part as { text: unknown }).text)
          : "",
      )
      .join("");
  }
  return "";
}

function readBody(req: IncomingMessage): Promise<string> {
  return new Promise((resolve, reject) => {
    const chunks: Buffer[] = [];
    req.on("data", (c: Buffer) => chunks.push(c));
    req.on("end", () => resolve(Buffer.concat(chunks).toString("utf8")));
    req.on("error", reject);
  });
}

function plan(body: ChatRequest): Plan {
  const messages = body.messages ?? [];
  // OpenClaw may append an internal-context user message after a tool result, so a tool
  // message anywhere after the final real prompt means the tool phase is over.
  const lastTool = [...messages].reverse().find((m) => m.role === "tool");
  if (lastTool) {
    return { text: `MOCK-DONE tool-result: ${textOf(lastTool.content).slice(0, TOOL_RESULT_PREVIEW_CHARS)}` };
  }
  // OpenClaw appends an internal-context user message after the real prompt, so scan all user turns.
  const prompt = messages
    .filter((m) => m.role === "user")
    .map((m) => textOf(m.content))
    .join("\n");
  if (prompt.includes("SCENARIO:deny")) {
    return { toolName: "exec", args: { command: "rm -rf /tmp/pair-spike-denied" } };
  }
  if (prompt.includes("SCENARIO:approve")) {
    return { toolName: "exec", args: { command: "curl http://127.0.0.1:1/never" } };
  }
  return { text: "MOCK-OK hello from the mock model" };
}

function sse(res: ServerResponse, obj: unknown): void {
  res.write(`data: ${JSON.stringify(obj)}\n\n`);
}

function respond(res: ServerResponse, body: ChatRequest): void {
  const p = plan(body);
  const model = body.model ?? "unknown";
  const base = { id: "chatcmpl-mock", object: "chat.completion.chunk", created: 0, model };
  const usage = {
    prompt_tokens: FIXED_INPUT_TOKENS,
    completion_tokens: FIXED_OUTPUT_TOKENS,
    total_tokens: FIXED_INPUT_TOKENS + FIXED_OUTPUT_TOKENS,
  };
  const toolCall = (name: string, args: Record<string, unknown>) => ({
    index: 0,
    id: "call_mock_1",
    type: "function",
    function: { name, arguments: JSON.stringify(args) },
  });
  if (body.stream === false) {
    const message =
      "text" in p
        ? { role: "assistant", content: p.text }
        : { role: "assistant", content: null, tool_calls: [toolCall(p.toolName, p.args)] };
    res.writeHead(200, { "content-type": "application/json" });
    res.end(
      JSON.stringify({
        ...base,
        object: "chat.completion",
        choices: [{ index: 0, message, finish_reason: "text" in p ? "stop" : "tool_calls" }],
        usage,
      }),
    );
    return;
  }
  res.writeHead(200, { "content-type": "text/event-stream", "cache-control": "no-cache" });
  sse(res, { ...base, choices: [{ index: 0, delta: { role: "assistant", content: "" }, finish_reason: null }] });
  if ("text" in p) {
    for (const word of p.text.split(" ")) {
      sse(res, { ...base, choices: [{ index: 0, delta: { content: `${word} ` }, finish_reason: null }] });
    }
    sse(res, { ...base, choices: [{ index: 0, delta: {}, finish_reason: "stop" }], usage });
  } else {
    sse(res, {
      ...base,
      choices: [{ index: 0, delta: { tool_calls: [toolCall(p.toolName, p.args)] }, finish_reason: null }],
    });
    sse(res, { ...base, choices: [{ index: 0, delta: {}, finish_reason: "tool_calls" }], usage });
  }
  res.write("data: [DONE]\n\n");
  res.end();
}

async function handle(req: IncomingMessage, res: ServerResponse): Promise<void> {
  const url = req.url ?? "";
  if (req.method === "GET" && url.startsWith("/v1/models")) {
    res.writeHead(200, { "content-type": "application/json" });
    res.end(
      JSON.stringify({
        object: "list",
        data: [
          { id: "mock-small", object: "model" },
          { id: "mock-routed", object: "model" },
        ],
      }),
    );
    return;
  }
  if (req.method === "POST" && url.startsWith("/v1/chat/completions")) {
    const body = JSON.parse(await readBody(req)) as ChatRequest;
    const messages = body.messages ?? [];
    appendFileSync(
      logPath,
      `${JSON.stringify({
        ts: new Date().toISOString(),
        model: body.model,
        stream: body.stream,
        messageCount: messages.length,
        lastRole: messages[messages.length - 1]?.role,
        lastMessagePreview: JSON.stringify(messages[messages.length - 1]?.content ?? null).slice(0, 300),
        tools: (body.tools ?? []).map((t) => t.function?.name),
        authorization: String(req.headers.authorization ?? "").replace(/Bearer\s+.+/, "Bearer <redacted>"),
      })}\n`,
    );
    respond(res, body);
    return;
  }
  res.writeHead(404).end();
}

createServer((req, res) => {
  handle(req, res).catch((err: unknown) => {
    res.writeHead(500).end(String(err));
  });
}).listen(port, HOST, () => {
  console.log(`mock openai listening on http://${HOST}:${port}`);
});
