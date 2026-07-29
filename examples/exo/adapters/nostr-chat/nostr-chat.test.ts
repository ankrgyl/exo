import { describe, expect, it } from "vitest";
import { generateSecretKey, verifyEvent, type Event } from "nostr-tools/pure";

import {
  createChatEvent,
  isGroupChatEvent,
  normalizeRelayUrl,
  parseSecretKey,
  publicIdentity,
  relayInformationUrl,
  selectPreviousPrefixes,
  shouldTrigger,
} from "./nostr-chat";

describe("Nostr chat worker helpers", () => {
  it("normalizes relay and relay-information URLs", () => {
    expect(normalizeRelayUrl("wss://relay.example.test")).toBe(
      "wss://relay.example.test/",
    );
    expect(relayInformationUrl("wss://relay.example.test")).toBe(
      "https://relay.example.test/",
    );
    expect(() => normalizeRelayUrl("https://relay.example.test")).toThrow(
      "wss:// or ws://",
    );
    expect(() =>
      normalizeRelayUrl("wss://user@relay.example.test?secret=yes"),
    ).toThrow("must not contain credentials");
  });

  it("parses hex and nsec secret keys", () => {
    const secretKey = generateSecretKey();
    const hex = Buffer.from(secretKey).toString("hex");
    expect(parseSecretKey(hex)).toEqual(secretKey);
    expect(() => parseSecretKey("not-a-key")).toThrow(
      "64 hex characters or an nsec",
    );
  });

  it("creates a signed NIP-29 kind-9 event", () => {
    const secretKey = generateSecretKey();
    const event = createChatEvent(
      "hello",
      "group-one",
      ["12345678", "abcdef01"],
      secretKey,
      1_700_000_000,
    );
    expect(event.kind).toBe(9);
    expect(event.tags).toEqual([
      ["h", "group-one"],
      ["previous", "12345678", "abcdef01"],
    ]);
    expect(verifyEvent(event)).toBe(true);
    expect(isGroupChatEvent(event, "group-one")).toBe(true);
    expect(isGroupChatEvent(event, "group-two")).toBe(false);
  });

  it("selects three recent foreign timeline prefixes", () => {
    const events = [
      event("11111111aaaaaaaa", "alice", 10),
      event("22222222bbbbbbbb", "self", 12),
      event("33333333cccccccc", "bob", 11),
      event("11111111dddddddd", "carol", 9),
      event("44444444eeeeeeee", "dave", 8),
    ];
    expect(selectPreviousPrefixes(events, "self")).toEqual([
      "33333333",
      "11111111",
      "44444444",
    ]);
  });

  it("supports all-message and explicit-mention wake policies", () => {
    const secretKey = generateSecretKey();
    const identity = publicIdentity(secretKey);
    const base = event("11111111aaaaaaaa", "alice", 10);
    expect(shouldTrigger("all_messages", base, identity.pubkey)).toBe(true);
    expect(shouldTrigger("mentions_only", base, identity.pubkey)).toBe(false);
    expect(
      shouldTrigger(
        "mentions_only",
        { ...base, tags: [["p", identity.pubkey]] },
        identity.pubkey,
      ),
    ).toBe(true);
    expect(
      shouldTrigger(
        "mentions_only",
        { ...base, content: `hello nostr:${identity.npub}` },
        identity.pubkey,
      ),
    ).toBe(true);
    expect(
      shouldTrigger(
        "all_messages",
        { ...base, pubkey: identity.pubkey },
        identity.pubkey,
      ),
    ).toBe(false);
  });
});

function event(id: string, pubkey: string, createdAt: number): Event {
  return {
    id,
    pubkey,
    created_at: createdAt,
    kind: 9,
    tags: [["h", "group-one"]],
    content: "",
    sig: "0".repeat(128),
  };
}
