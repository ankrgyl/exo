Set up a public Nostr chat adapter for this conversation.

Create a library Nostr chat adapter if one does not already exist for this
conversation. Use these settings:

- name: `nostr-chat`
- source: `library`
- type: `nostr-chat`
- relayUrl: `null`
- groupId: `null`
- secretKeySecretId: `null`
- trigger: `all_messages`

Null relay and group values use the documented public defaults. The adapter
generates and protects a new Nostr key when `secretKeySecretId` is null. Do not
request, print, or expose the generated secret key.

After the adapter is ready, report its ID, relay URL, and NIP-29 group ID. The
worker prints its public `npub` when it connects. Explain that the user can
replace the relay and group values with any compatible NIP-29 deployment.
