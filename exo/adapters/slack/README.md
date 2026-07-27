# Slack Adapter

The Slack adapter is a local, non-central library adapter. Slack sends events to a public ngrok URL, ngrok forwards them to a local Exo worker, and the worker replies directly to Slack with the workspace-owned bot token.

The MVP is text-only. The recommended setup wakes on `app_mention` events and Slack DMs; untagged thread follow-ups are opt-in because they require broader Slack history scopes. There is no Exo-hosted Slack app and no Exo relay service in the message path.

## Setup

### Chat Setup

Start Exo normally:

```bash
./exo.sh
```

Then ask the agent:

```text
Help me set up Slack.
```

The agent will walk you through creating the Slack app, storing secrets, creating the adapter, starting ngrok, and enabling Slack Event Subscriptions.

This startup shortcut is still available, but optional:

```bash
./exo.sh --setup slack
```

You do not need to run `pnpm slack:setup` for the chat-driven flow.

### Coverage Profiles

Pick the narrowest profile that matches the workflow you want:

| Profile           | Wakes Exo On                                                                    | Extra Slack Visibility                                         | Adapter Trigger |
| ----------------- | ------------------------------------------------------------------------------- | -------------------------------------------------------------- | --------------- |
| `mentions`        | `@Exo` mentions only                                                            | Least privilege; no DMs                                        | `mentions_only` |
| `dm`              | `@Exo` mentions and Slack DMs                                                   | Reads DMs sent to the bot                                      | `mentions_only` |
| `public-threads`  | Mentions, DMs, and untagged follow-ups in active public-channel threads         | Reads public channel messages delivered to the app             | `mentions_only` |
| `private-threads` | Mentions, DMs, and untagged follow-ups in active public/private channel threads | Reads public and private channel messages delivered to the app | `mentions_only` |
| `all`             | Every subscribed public/private channel message, plus DMs                       | Noisiest; use `allowedChannels`                                | `all_messages`  |

Active thread profiles do not make Exo reply to every delivered message. They only let Slack deliver thread messages; the worker wakes Exo for a thread after Exo was mentioned or replied there, and the wakeup prompt tells Exo to stay silent unless the message appears directed at it.

### Manual Setup Details

#### 1. Create a Slack App

1. Open the Slack app dashboard: <https://api.slack.com/apps>.
2. Click **Create New App** > **From an app manifest**.
3. Pick your workspace.
4. Paste the manifest shown by the interactive wizard. If you are doing this without the wizard, print a profile-specific manifest with your chosen app and bot names:

   ```bash
   pnpm slack:setup --profile dm --app-name 'Exo Local' --bot-name 'Exo'
   ```

5. Open **Basic Information** and copy the **Signing Secret**.
6. Store it locally:

   ```bash
   exo secret set slack-signing-secret --value '<signing-secret>'
   ```

7. Open **OAuth & Permissions**, click **Install to Workspace**, approve the install, and copy the **Bot User OAuth Token** starting with `xoxb-`.
8. Store it locally:

   ```bash
   exo secret set slack-bot-token --value 'xoxb-...'
   ```

#### 2. Store the Secrets in Exo

```bash
exo secret set slack-signing-secret --value '<signing-secret>'
exo secret set slack-bot-token --value 'xoxb-...'
```

#### 3. Create the Exo Adapter

During interactive setup, the agent creates the adapter after you confirm the Slack secrets are stored. You can also send the setup prompt while starting a fresh local Exo agent:

```bash
./exo.sh \
  --agent exospooky \
  --conversation dev \
  --setup slack
```

The setup prompt creates a library adapter similar to:

```json
{
  "name": "slack-dev",
  "source": "library",
  "config": {
    "type": "slack",
    "botTokenSecretId": "slack-bot-token",
    "signingSecretId": "slack-signing-secret",
    "port": 3939,
    "path": "/slack/events",
    "defaultChannelId": null,
    "trigger": "mentions_only",
    "allowedChannels": null,
    "allowBots": false,
    "threadReplies": true,
    "progressMode": "update",
    "conversationScope": "target"
  }
}
```

Use `trigger: "all_messages"` only for the `all` profile, and set `allowedChannels` to specific Slack channel ids unless you intentionally want every subscribed channel to wake Exo.

#### 4. Start ngrok

In a separate terminal:

```bash
ngrok http 3939
```

The interactive wizard asks for your ngrok HTTPS origin and gives you the exact Slack Request URL. If you are doing this manually, you can also print it with:

```bash
pnpm slack:setup --profile dm --app-name 'Exo Local' --bot-name 'Exo' https://YOUR-NGROK-HOST.ngrok-free.app
```

#### 5. Enable Slack Events

In the Slack app dashboard:

1. Open **Event Subscriptions** and enable events.
2. Set **Request URL** to the URL printed by `pnpm slack:setup`, usually `https://...ngrok-free.app/slack/events`.
3. Under **Subscribe to bot events**, add the events for your selected profile:
   - `mentions`: `app_mention`
   - `dm`: `app_mention`, `message.im`
   - `public-threads`: `app_mention`, `message.channels`, `message.im`
   - `private-threads`: `app_mention`, `message.channels`, `message.groups`, `message.im`
   - `all`: `app_mention`, `message.channels`, `message.groups`, `message.im`
4. For every profile with DMs, open **App Home**, turn on **Messages Tab**, and check **Allow users to send Slash commands and messages from the messages tab**. The manifest sets this automatically for new apps.
5. Save changes and reinstall the app if Slack asks.
6. Invite the bot to a channel with `/invite @<bot display name>`.

#### 6. Test It

Mention the bot in Slack:

```text
@<bot display name> hello from Slack
```

The adapter wakes Exo with a target shaped as `CHANNEL_ID:THREAD_TS` for mentions. Use that target for channel replies; the worker posts back into the same Slack thread. With an active-thread profile, after Exo is mentioned or replies in a thread, later messages in that thread can wake Exo without another mention. The wakeup prompt tells Exo to reply only when the message appears directed at Exo, asks Exo to do something, or clearly needs an Exo response.

Slack DMs use synthetic targets shaped as `dm:USER_ID` for top-level DMs and `dm:USER_ID:THREAD_TS` for replies inside Slack DM threads. The worker opens the DM with `conversations.open` and sends to the matching DM or DM thread.

## Configuration

- `botTokenSecretId` is the Exo secret name or id containing the Slack Bot User OAuth Token.
- `signingSecretId` is the Exo secret name or id containing the Slack Signing Secret.
- `port` and `path` define the local event endpoint. The default setup uses `3939` and `/slack/events`.
- `trigger` is `mentions_only` or `all_messages`. With active-thread setup, `mentions_only` still wakes on Slack DMs from the `message.im` event and on active thread follow-ups from the `message.channels` and `message.groups` events.
- `allowedChannels` optionally restricts inbound wakeups to Slack channel ids.
- `threadReplies` makes inbound targets `CHANNEL_ID:THREAD_TS`, so replies post into the same Slack thread.
- `progressMode: "update"` posts a normal Slack progress message for explicit mentions and DMs, then replaces that same message with Exo's final reply. Use `"stream"` for Slack's native threaded streaming UI, or `"off"` if you only want final Slack messages.
- `conversationScope: "target"` creates a separate Exo conversation for each Slack target.

For all channel messages, set `trigger: "all_messages"`.
