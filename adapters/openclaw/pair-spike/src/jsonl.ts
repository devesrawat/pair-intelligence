import { appendFileSync, mkdirSync } from "node:fs";
import { dirname } from "node:path";

/** Append one timestamped JSON line, creating parent directories. Throws if the path is unwritable. */
export function appendJsonl(path: string, record: Readonly<Record<string, unknown>>): void {
  mkdirSync(dirname(path), { recursive: true });
  appendFileSync(path, `${JSON.stringify({ ts: new Date().toISOString(), ...record })}\n`);
}
