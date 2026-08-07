Set up a Signal adapter for testing.

Create a library Signal adapter if one does not already exist for this conversation, then make sure it is ready for the background adapter runner. Use these settings:

- name: `signal-dev`
- source: `library`
- type: `signal`
- account: `null`
- deviceName: `Exo`
- configDir: `null`
- trigger: `all_messages`
- allowedContacts: `null`

Assume the user already has a Signal account on a phone with a real phone number and has set a Signal username. The Signal adapter uses `signal-cli` locally and will start linked-device setup when `account` is `null`. If `signal-cli` is missing, tell the user to install it first, for example with `brew install signal-cli` on macOS.

If outbound sends fail with `NETWORK_FAILURE` and an error like `IdentityKeyDeserializer has no default (no arg) constructor`, the installed `signal-cli` is likely a GraalVM/native build with incomplete reflection metadata. Tell the user to put the JVM signal-cli distribution first on `PATH` before starting the adapter runner.

After creating or confirming the adapter, explain that the setup script will try to print a Signal linked-device QR code from `.exo/exo-adapters.log` and pause before entering the REPL. The user should scan it from Signal: Settings > Linked devices > Link new device. If no QR appears immediately, tell the user to watch `.exo/exo-adapters.log`; the adapter may already be linked, `signal-cli` may be missing, or the link flow may still be starting.

Briefly tell me the adapter id, where the adapter state is stored, and that outbound targets should be Signal usernames such as `u:example.01` unless an inbound wakeup provides a more precise target.
