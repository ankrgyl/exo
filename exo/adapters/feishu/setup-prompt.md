Set up a Feishu (Lark) adapter with an interactive wizard.

Do not create the adapter immediately. First walk the user through Feishu app creation and local secret setup. Ask one short question at a time, wait for the user to confirm each step, and never ask the user to paste the app secret into chat.

Feishu's open platform has a long-connection mode: the worker opens an outbound WebSocket to Feishu, so no public callback URL and no tunnel are needed. The whole setup runs on a laptop behind NAT.

Use this flow:

1. Ask whether their tenant is on Feishu (`feishu.cn`, mainland China) or Lark (`larksuite.com`, international). This selects the `domain` setting and which console they use:
   - Feishu: `https://open.feishu.cn`
   - Lark: `https://open.larksuite.com`

2. Ask the user to create a **custom app** (企业自建应用 / Custom App) in that console and pick a bot name. Recommend the name `Exo`, but let the user choose. In the app's **Features** section they must enable the **Bot** capability.

3. Ask the user to add these permissions (Permissions & Scopes):
   - `im:message:send_as_bot` — send messages as the bot.
   - `im:message.group_at_msg` — receive group messages that @-mention the bot.
   - `im:message.p2p_msg` — receive direct messages.

   For `all_messages` group coverage they should also add `im:message.group_msg` if their tenant offers it; mentions-only is the recommended default.

4. Ask the user to open **Events & Callbacks**, set the subscription mode to **long connection** (长连接), and subscribe to the `im.message.receive_v1` event. This is what removes the need for a public URL.

5. Ask the user to publish the app (create a release version and approve it; for custom apps this is usually instant self-approval), then copy the **App ID** (`cli_...`) from the app's credentials page and reply with it. The app id is not a secret and can be pasted into chat.

6. Tell the user to copy the **App Secret** from the same page and store it locally with:

   ```bash
   exo secret set feishu-app-secret --value '<app-secret>'
   ```

   Tell the user not to paste the value into chat. Ask them to reply `app secret stored` when done.

7. After the user confirms the secret is stored, create a library Feishu adapter if one does not already exist for this conversation. Use these settings:

- name: `feishu-dev`
- source: `library`
- type: `feishu`
- appId: the `cli_...` id from step 5
- appSecretSecretId: `feishu-app-secret`
- domain: `feishu` or `lark` from step 1
- trigger: `mentions_only`
- defaultTarget: `null`

8. Tell the user to add the bot to a group chat (group settings → Bots → Add Bot) or open a direct chat with it, then @-mention the bot in the group or send it a DM. Confirm the message wakes this conversation and that the reply arrives back in Feishu.

If adapter creation reports a missing app secret, tell the user to run the `exo secret set` command above and continue from adapter creation. If the long connection fails at startup, the most common causes are an unpublished app release, a wrong domain choice, or event subscription still set to callback mode instead of long connection.
