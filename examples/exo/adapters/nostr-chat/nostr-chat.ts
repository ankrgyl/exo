import {
  finalizeEvent,
  getPublicKey,
  verifyEvent,
  type Event,
  type VerifiedEvent,
} from "nostr-tools/pure";
import { decode, npubEncode } from "nostr-tools/nip19";

export type NostrChatTrigger = "all_messages" | "mentions_only";

export function normalizeRelayUrl(value: string): string {
  const url = new URL(value);
  if (url.protocol !== "wss:" && url.protocol !== "ws:") {
    throw new Error("Nostr relay URL must use wss:// or ws://");
  }
  if (url.username || url.password || url.search || url.hash) {
    throw new Error(
      "Nostr relay URL must not contain credentials, a query, or a fragment",
    );
  }
  return url.toString();
}

export function relayInformationUrl(relayUrl: string): string {
  const url = new URL(normalizeRelayUrl(relayUrl));
  url.protocol = url.protocol === "wss:" ? "https:" : "http:";
  return url.toString();
}

export function parseSecretKey(value: string): Uint8Array {
  const trimmed = value.trim();
  if (/^[0-9a-f]{64}$/i.test(trimmed)) {
    return Uint8Array.from(Buffer.from(trimmed, "hex"));
  }
  if (trimmed.startsWith("nsec1")) {
    const decoded = decode(trimmed);
    if (decoded.type === "nsec") {
      return decoded.data;
    }
  }
  throw new Error("Nostr secret key must be 64 hex characters or an nsec");
}

export function selectPreviousPrefixes(
  events: Event[],
  ownPubkey: string,
  limit = 3,
): string[] {
  return [...events]
    .sort(
      (left, right) =>
        right.created_at - left.created_at || right.id.localeCompare(left.id),
    )
    .filter((event) => event.pubkey !== ownPubkey)
    .map((event) => event.id.slice(0, 8))
    .filter((prefix, index, prefixes) => prefixes.indexOf(prefix) === index)
    .slice(0, limit);
}

export function createChatEvent(
  text: string,
  groupId: string,
  previousPrefixes: string[],
  secretKey: Uint8Array,
  createdAt = Math.floor(Date.now() / 1_000),
): VerifiedEvent {
  const tags = [["h", groupId]];
  if (previousPrefixes.length > 0) {
    tags.push(["previous", ...previousPrefixes]);
  }
  return finalizeEvent(
    {
      kind: 9,
      content: text,
      created_at: createdAt,
      tags,
    },
    secretKey,
  );
}

export function isGroupChatEvent(
  event: Event,
  groupId: string,
): event is VerifiedEvent {
  return (
    event.kind === 9 &&
    event.tags.some(
      (tag) => tag[0] === "h" && tag.length === 2 && tag[1] === groupId,
    ) &&
    verifyEvent(event)
  );
}

export function shouldTrigger(
  trigger: NostrChatTrigger,
  event: Event,
  ownPubkey: string,
): boolean {
  if (event.pubkey === ownPubkey) {
    return false;
  }
  if (trigger === "all_messages") {
    return true;
  }
  const npub = npubEncode(ownPubkey);
  return (
    event.tags.some(
      (tag) => tag[0] === "p" && tag.length >= 2 && tag[1] === ownPubkey,
    ) ||
    event.content.toLowerCase().includes(ownPubkey.toLowerCase()) ||
    event.content.toLowerCase().includes(npub.toLowerCase())
  );
}

export function publicIdentity(secretKey: Uint8Array): {
  pubkey: string;
  npub: string;
} {
  const pubkey = getPublicKey(secretKey);
  return { pubkey, npub: npubEncode(pubkey) };
}
