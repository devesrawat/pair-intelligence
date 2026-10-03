// Loopback mock of pair-api for adapter tests. Records every request; each route is scripted.
import { createServer, type IncomingMessage, type Server, type ServerResponse } from "node:http";
import type { AddressInfo } from "node:net";

export type RecordedRequest = {
  readonly method: string;
  readonly path: string;
  readonly headers: Readonly<Record<string, string | string[] | undefined>>;
  readonly body: unknown;
};

export type Reply =
  | { readonly kind: "json"; readonly status: number; readonly body: unknown }
  | { readonly kind: "raw"; readonly status: number; readonly text: string }
  | { readonly kind: "hang" }
  | { readonly kind: "destroy" };

export type Script = (path: string, body: unknown) => Reply;

export type MockPair = {
  readonly url: string;
  readonly requests: RecordedRequest[];
  readonly close: () => Promise<void>;
};

export const ok = (data: unknown): Reply => ({
  kind: "json",
  status: 200,
  body: { success: true, data, error: null },
});

export const fail = (status: number, code: string): Reply => ({
  kind: "json",
  status,
  body: { success: false, data: null, error: { code, message: "scripted" } },
});

export const allow = (): Reply => ok({ decision: { decision: "allow" }, policy_version: "pv" });
export const deny = (reason: string): Reply =>
  ok({ decision: { decision: "deny", reason }, policy_version: "pv" });
export const needsApproval = (hash: string): Reply =>
  ok({ decision: { decision: "needs_approval", payload_hash: hash }, policy_version: "pv" });

async function readBody(req: IncomingMessage): Promise<string> {
  const chunks: Buffer[] = [];
  for await (const chunk of req) chunks.push(chunk as Buffer);
  return Buffer.concat(chunks).toString("utf8");
}

export async function startMockPair(script: Script): Promise<MockPair> {
  const requests: RecordedRequest[] = [];
  const open = new Set<ServerResponse>();
  const server: Server = createServer((req, res) => {
    open.add(res);
    void (async () => {
      const text = await readBody(req);
      let body: unknown = null;
      try {
        body = JSON.parse(text);
      } catch {
        body = text;
      }
      const path = req.url ?? "";
      requests.push({ method: req.method ?? "", path, headers: req.headers, body });
      const reply = script(path, body);
      if (reply.kind === "hang") return;
      if (reply.kind === "destroy") {
        req.socket.destroy();
        return;
      }
      if (reply.kind === "raw") {
        res.writeHead(reply.status, { "content-type": "application/json" });
        res.end(reply.text);
        return;
      }
      res.writeHead(reply.status, { "content-type": "application/json" });
      res.end(JSON.stringify(reply.body));
    })();
  });
  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  const { port } = server.address() as AddressInfo;
  return {
    url: `http://127.0.0.1:${port}`,
    requests,
    close: () =>
      new Promise<void>((resolve) => {
        for (const res of open) res.destroy();
        server.close(() => resolve());
      }),
  };
}

/** A URL on a loopback port nothing listens on. */
export async function unreachableUrl(): Promise<string> {
  const mock = await startMockPair(() => ok({}));
  const url = mock.url;
  await mock.close();
  return url;
}
