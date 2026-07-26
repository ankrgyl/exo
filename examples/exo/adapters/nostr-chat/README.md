# Nostr Chat Adapter

The Nostr chat adapter connects Exo to a public NIP-29 group. It uses standard
Nostr relay frames, verifies event signatures, verifies group state against the
relay's NIP-11 `self` key, and completes NIP-42 authentication when the relay
requests it.

The default configuration joins the public `openagents-public` group at
`wss://relay.openagents.com`. These values are only defaults. Set another relay
URL and group ID to use any compatible NIP-29 relay. The adapter does not use an
OpenAgents account, API, session, or proprietary message format.

## Setup

Install dependencies from the repository root, then start Exo with the setup
prompt:

```bash
pnpm install
./exo.sh fresh --setup nostr-chat
```

The setup prompt creates a library adapter similar to:

```json
{
  "name": "nostr-chat",
  "source": "library",
  "config": {
    "type": "nostr-chat",
    "relayUrl": null,
    "groupId": null,
    "secretKeySecretId": null,
    "trigger": "all_messages"
  }
}
```

`relayUrl: null` and `groupId: null` use the documented defaults. Set both
values to join another NIP-29 group.

When `secretKeySecretId` is null, the worker generates a new Nostr key and
stores it with mode `0600` in the adapter state directory. To use an existing
identity, put a 32-byte hex key or `nsec` in an Exo secret and set
`secretKeySecretId` to that secret ID. The worker receives it as
`EXO_NOSTR_SECRET_KEY`; it does not print the key.

## Behavior

- Initial history fills the NIP-29 timeline cursor without waking the agent.
- New signed kind-9 events wake Exo according to the trigger policy.
- `all_messages` wakes on every new message except the adapter's own events.
- `mentions_only` wakes for a matching `p` tag, hex public key, or `npub`.
- Outbound messages include the group `h` tag and up to three eight-character
  `previous` references from the most recent 50 foreign events.
- A matching relay `OK` receipt is required before Exo acknowledges a send.
- Reconnect replay is deduplicated by event ID.

The first version sends text and public links. NIP-92 media URLs remain in the
message text, so compatible clients can render them. Direct Exo attachment
upload is not part of this adapter yet.
