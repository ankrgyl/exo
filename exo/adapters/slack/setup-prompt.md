Set up a Slack adapter for local testing with an interactive wizard.

Do not create the adapter immediately. First walk the user through Slack app creation and local secret setup. Ask one short question at a time, wait for the user to confirm each step, and never ask the user to paste the Slack bot token or signing secret into chat.

Use this flow:

1. Ask what Slack app name and bot display name they want. Recommend app name `Exo Local` and bot display name `Exo`, but let the user choose. Use the chosen names in the manifest and in later `/invite @...` and test-message instructions.

2. Ask which Slack coverage profile they want. Recommend **Mentions + DMs** unless they specifically want untagged thread follow-ups.
   - **Mentions only**: least privilege. Exo wakes only when mentioned in a channel. No Slack DMs.
   - **Mentions + DMs**: recommended default. Exo wakes on `@Exo` mentions and direct messages. Untagged thread replies are ignored.
   - **Public active threads**: adds public-channel `message.channels` access so replies in threads where Exo has participated can wake Exo without another mention.
   - **Public + private active threads**: adds public and private channel history access, including `message.groups`, so this also works in private channels where the bot is invited.
   - **All subscribed channel messages**: advanced and noisy. Exo wakes on every subscribed public/private channel message. Only use this with `allowedChannels` set to a small allowlist.

3. Show the Slack app manifest for the selected names and profile, then ask the user to create a Slack app from it at `https://api.slack.com/apps`. Use these scope rules:
   - Mentions only: `app_mentions:read`, `chat:write`.
   - Mentions + DMs: mentions-only scopes plus `im:history`, `im:write`; include the App Home messages tab manifest block.
   - Public active threads: mentions + DMs scopes plus `channels:history`.
   - Public + private active threads: public active thread scopes plus `groups:history`.
   - All subscribed channel messages: same scopes as public + private active threads.

4. After the user confirms the app is created, explain that Slack shows the Signing Secret immediately on the app's Basic Information page. Ask the user to copy the Signing Secret and store it locally with:

   ```bash
   exo secret set slack-signing-secret --value '<signing-secret>'
   ```

   Tell the user not to paste the value into chat. Ask them to reply `signing secret stored` when done.

5. Then tell the user to open **OAuth & Permissions** in the Slack app sidebar, click **Install to Workspace**, approve the install, copy the **Bot User OAuth Token** that starts with `xoxb-`, and store it locally with:

   ```bash
   exo secret set slack-bot-token --value 'xoxb-...'
   ```

   Tell the user not to paste the token into chat. Ask them to reply `bot token stored` when done.

6. After the user confirms both secrets were stored, create a library Slack adapter if one does not already exist for this conversation. Use these settings:

- name: `slack-dev`
- source: `library`
- type: `slack`
- botTokenSecretId: `slack-bot-token`
- signingSecretId: `slack-signing-secret`
- port: `3939`
- path: `/slack/events`
- defaultChannelId: `null`
- trigger: `mentions_only` for every profile except **All subscribed channel messages**, which uses `all_messages`
- allowedChannels: `null` except for **All subscribed channel messages**; for that profile, ask for Slack channel ids and use that allowlist unless the user explicitly wants every subscribed channel
- allowBots: `false`
- threadReplies: `true`
- progressMode: `update` unless the user says they only want final Slack messages with no progress UI, or explicitly asks for Slack's native threaded streaming UI
- conversationScope: `target`

7. Explain that `progressMode: update` shows progress by posting a normal Slack message for explicit mentions and DMs, then replacing that same message with Exo's final reply. It does not show progress for ambient active-thread messages, because Exo may decide not to answer those. If the user asks for native Slack text streaming, use `progressMode: stream`, but note that Slack's native streaming API replies in threads.

8. After creating or confirming the adapter, ask the user to start ngrok in a host terminal:

   ```bash
   ngrok http 3939
   ```

9. Ask the user to paste the ngrok HTTPS origin, such as `https://example.ngrok-free.app`. Reply with the Slack Event Subscriptions Request URL by appending `/slack/events`.
10. Tell the user to enable Event Subscriptions in Slack, paste that Request URL, and subscribe to the bot events for the selected profile:

- Mentions only: `app_mention`.
- Mentions + DMs: `app_mention`, `message.im`.
- Public active threads: `app_mention`, `message.channels`, `message.im`.
- Public + private active threads: `app_mention`, `message.channels`, `message.groups`, `message.im`.
- All subscribed channel messages: `app_mention`, `message.channels`, `message.groups`, `message.im`.

For every profile with DMs, open **App Home**, turn on **Messages Tab**, and check **Allow users to send Slash commands and messages from the messages tab**. Save changes, reinstall if Slack asks, invite the bot to a channel with `/invite @<bot display name>`, mention the bot in a channel, and test the paths enabled by the selected profile.

When Slack messages wake the agent, Slack targets work like this:

- Channel/thread replies use the inbound target, usually `CHANNEL_ID:THREAD_TS`.
- After Exo is mentioned or replies in a public or private channel thread, later messages in that thread can wake Exo without another mention. Only respond externally if the message appears directed at Exo, asks Exo to do something, or clearly needs an Exo response; otherwise do nothing.
- Direct-message replies use `dm:USER_ID` for top-level DMs and `dm:USER_ID:THREAD_TS` for replies inside Slack DM threads.
- For "DM me when you are uncomfortable answering" behavior, send a brief safe public response first, then optionally DM a safe alternative or clarification. Do not use DM to provide forbidden content privately.

Slack app manifest:

```yaml
display_information:
  name: <chosen app name>
features:
  app_home:
    home_tab_enabled: false
    messages_tab_enabled: true
    messages_tab_read_only_enabled: false
  bot_user:
    display_name: <chosen bot display name>
    always_online: false
oauth_config:
  scopes:
    bot:
      - app_mentions:read
      - chat:write
      - im:history
      - im:write
settings:
  org_deploy_enabled: false
  socket_mode_enabled: false
  token_rotation_enabled: false
```

The manifest above is the recommended **Mentions + DMs** profile. Fill in the user's chosen app name and bot display name before showing it. For **Mentions only**, remove the `app_home` block and the `im:*` scopes. For **Public active threads**, add `channels:history`. For **Public + private active threads** and **All subscribed channel messages**, add both `channels:history` and `groups:history`.

If the user says the app and secrets already exist, skip directly to adapter creation. If adapter creation reports a missing Slack token or signing secret, tell the user to run the relevant `exo secret set ... --value ...` command above and then continue from adapter creation.
