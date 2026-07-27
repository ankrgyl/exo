# Feishu (Lark) Adapter

The Feishu adapter is a local, non-central library adapter for Feishu (`feishu.cn`) and Lark (`larksuite.com`) tenants. It uses the open platform's long-connection mode: the worker opens an outbound WebSocket for inbound events and calls `im.message.create` for replies, so no public callback URL, tunnel, or relay is needed. This makes it the easiest adapter to run from mainland China networks, where it needs no proxy at all.

The MVP is text-only. Group chats wake the agent on @-mentions by default; direct messages always wake it. Attachments are rejected with a clear error for now.

## Setup

### Chat Setup

Start Exo normally:

```bash
./exo.sh
```

Then ask the agent:

```text
Help me set up Feishu.
```

The agent walks you through creating a custom app in the Feishu/Lark open platform console, granting bot permissions, enabling long-connection event subscriptions, storing the app secret, and creating the adapter.

### Manual Setup

1. Create a custom app at `https://open.feishu.cn` (or `https://open.larksuite.com` for Lark) and enable the **Bot** capability.
2. Add permissions: `im:message:send_as_bot`, `im:message.group_at_msg`, `im:message.p2p_msg`.
3. Under **Events & Callbacks**, select **long connection** mode and subscribe to `im.message.receive_v1`.
4. Publish an app release, then copy the app credentials.
5. Store the app secret:

   ```bash
   exo secret set feishu-app-secret --value '<app-secret>'
   ```

6. Create the adapter from the REPL by asking the agent, with config:

   ```json
   {
     "type": "feishu",
     "appId": "cli_...",
     "appSecretSecretId": "feishu-app-secret",
     "domain": "feishu",
     "trigger": "mentions_only",
     "defaultTarget": null
   }
   ```

7. Add the bot to a group (group settings → Bots) or DM it, then @-mention it. Replies go back to the same chat.

## Configuration

| Field               | Meaning                                                                                                    |
| ------------------- | ---------------------------------------------------------------------------------------------------------- |
| `appId`             | App id (`cli_...`) from the open platform console. Not a secret.                                           |
| `appSecretSecretId` | Exo secret containing the app secret. The worker receives it as `EXO_FEISHU_APP_SECRET`.                   |
| `domain`            | `feishu` for feishu.cn tenants, `lark` for larksuite.com tenants.                                          |
| `trigger`           | `mentions_only` (default; DMs always wake) or `all_messages`.                                              |
| `defaultTarget`     | Optional chat id (`oc_...`) or user open id (`ou_...`) used when `send_adapter_message` has a null target. |

## Behavior notes

- Inbound targets are Feishu chat ids (`oc_...`). Sends to targets starting with `ou_` are delivered as direct messages via `open_id`.
- `mentions_only` counts any @-mention in a group as addressing the bot; distinguishing the bot's own mention would need an extra credentials call and is left for later.
- The Lark SDK's internal loggers write non-JSON lines to stdout, which would corrupt the worker protocol; the worker installs a stdout filter before importing the SDK, so SDK noise lands on stderr instead.
- The long connection reconnects automatically after network drops; `connected` is only reported after the first successful handshake.
