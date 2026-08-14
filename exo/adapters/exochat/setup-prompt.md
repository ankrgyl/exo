Set up an ExoChat adapter for this conversation.

Create a library ExoChat adapter if one does not already exist for this conversation, then make sure it is ready for the background adapter runner. Use these settings:

- name: `exochat`
- source: `library`
- type: `exochat`
- baseUrl: `null`
- channelId: `null`
- secret: `null`

After creating or confirming the adapter:

1. Call `list_adapters` (or use the create result) and read the ExoChat `chatUrl` when present.
2. **Briefly print the ExoChat URL again here** in your reply (full `https://…` link), even if the setup script already printed it.
3. Tell me clearly that I can talk to you **either through this terminal UI or through that ExoChat URL**, and that **the ExoChat URL keeps working even if I close or `/exit` this terminal chat** (as long as the agent/adapter runner is still up).
4. Mention the adapter id for debugging, and that ExoChat is text-only for now (no attachments).
