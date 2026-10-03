import { createHash, randomUUID } from "node:crypto";

/** Fixed namespace so one OpenClaw run id always yields the same PAIR task id (UUIDv5). */
const TASK_NAMESPACE = "6ba7b811-9dad-11d1-80b4-00c04fd430c8";
const UUID_V5_VERSION = 0x50;
const UUID_VERSION_MASK = 0x0f;
const UUID_VARIANT_MASK = 0x3f;
const UUID_VARIANT_RFC4122 = 0x80;
const VERSION_BYTE = 6;
const VARIANT_BYTE = 8;
const UUID_BYTES = 16;

export function sha256Hex(value: string): string {
  return createHash("sha256").update(value).digest("hex");
}

/** Stable hash of a params object for audit logs (never the params themselves). */
export function paramsHash(params: unknown): string {
  try {
    return sha256Hex(JSON.stringify(params) ?? "undefined");
  } catch {
    return "unhashable";
  }
}

export function newTraceId(): string {
  return randomUUID();
}

/** RFC 4122 UUIDv5 of `name` in the adapter's namespace. */
export function taskIdFor(name: string): string {
  const namespace = Buffer.from(TASK_NAMESPACE.replaceAll("-", ""), "hex");
  const digest = createHash("sha1").update(namespace).update(name).digest();
  const bytes = Buffer.from(digest.subarray(0, UUID_BYTES));
  bytes[VERSION_BYTE] = ((bytes[VERSION_BYTE] ?? 0) & UUID_VERSION_MASK) | UUID_V5_VERSION;
  bytes[VARIANT_BYTE] = ((bytes[VARIANT_BYTE] ?? 0) & UUID_VARIANT_MASK) | UUID_VARIANT_RFC4122;
  const hex = bytes.toString("hex");
  return `${hex.slice(0, 8)}-${hex.slice(8, 12)}-${hex.slice(12, 16)}-${hex.slice(16, 20)}-${hex.slice(20)}`;
}
