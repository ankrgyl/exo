Set up an IRC adapter for testing.

Create a built-in IRC adapter if one does not already exist for this conversation, then make sure it is ready for the background adapter runner. If an enabled IRC adapter already exists, confirm it instead of creating a second one. Use these settings:

- name: `undernet-exo-test-plain`
- server: `irc.undernet.org`
- port: `6667`
- tls: `false`
- nick: `""` (leave blank to auto-generate `exoNNNN`)
- username: `""` (leave blank to default to `exo`)
- realname: `Exo Test Bot`
- channel: `#exo`
- passwordSecretId: `null`
- trigger: `mention`

After creating or confirming the adapter, briefly tell me the adapter id, nick, channel, and exactly what message I should send from IRC to test it.
