import { createHash } from "node:crypto";

// The relay's replay-protection id rides at envelope level, outside the
// ciphertext, so the relay can see it. Send a hash of the command id rather
// than the id itself: deterministic across retries (a replayed command
// presents the same key), meaningless to the relay.
export function sendDedupeId(commandId: string): string {
  return createHash("sha256")
    .update(commandId)
    .digest("base64url")
    .slice(0, 22);
}

export type SendAck = {
  id: string;
  duplicate: boolean;
};

// Acks are relay metadata like presence, not encrypted frames: the relay has
// no channel key, and the ack carries only the id the sender already exposed.
export function parseSendAck(value: unknown): SendAck | null {
  if (!value || typeof value !== "object" || Array.isArray(value)) {
    return null;
  }
  const record = value as Record<string, unknown>;
  if (record.channel !== "exo.chat.ack") {
    return null;
  }
  if (typeof record.id !== "string" || typeof record.duplicate !== "boolean") {
    return null;
  }
  return { id: record.id, duplicate: record.duplicate };
}
