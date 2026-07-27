Set up an ExoChat adapter for this conversation.

Create a library ExoChat adapter if one does not already exist for this conversation, then make sure it is ready for the background adapter runner. Use these settings:

- name: `exochat`
- source: `library`
- type: `exochat`
- baseUrl: `null`
- channelId: `null`
- secret: `null`

After creating or confirming the adapter, explain that the setup script will print an ExoChat URL from `.exo/exo-adapters.log`. The user can open that URL in a browser or on their phone to chat with this agent. Briefly tell me the adapter id and that ExoChat is currently a text-only control channel.
