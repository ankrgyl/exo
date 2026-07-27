Set up a WhatsApp adapter for testing.

Create a library WhatsApp adapter if one does not already exist for this conversation, then make sure it is ready for the background adapter runner. Use these settings:

- name: `whatsapp-dev`
- source: `library`
- type: `whatsapp`
- authDir: `null`
- linkMethod: `qr` unless the user asks for a pairing code
- phoneNumber: `null` unless linkMethod is `pairing-code`
- trigger: `all_messages`
- allowedChats: `null`

After creating or confirming the adapter, explain that the setup script will try to print the QR code from `.exo/exo-adapters.log`. If it does not appear immediately, tell the user to watch that log and scan the QR code with WhatsApp's linked-device flow. Briefly tell me the adapter id, where the auth state will be stored, and what message I should send from WhatsApp to test it.
