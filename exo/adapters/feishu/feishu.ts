import { isRecord } from "../protocol";

export type FeishuTriggerPolicy = "mentions_only" | "all_messages";

export type FeishuInboundMessage = {
  chatId: string;
  chatType: string | null;
  sender: string | null;
  messageId: string | null;
  mentionedBot: boolean;
  text: string;
};

// The Lark SDK's RequestHandle.parse spreads the event body flat into the object
// handed to EventDispatcher handlers ({...header, ...event}) for both schema 1.0
// and 2.0, so `message` / `sender` arrive at the top level — never nested under
// `.event`. Reading `event.event.*` silently drops every inbound message. A
// nested shape is accepted too, in case a future SDK version stops flattening.
export function parseInboundEvent(
  event: unknown,
  trigger: FeishuTriggerPolicy,
): FeishuInboundMessage | null {
  if (!isRecord(event)) {
    return null;
  }
  const body = isRecord(event.event) ? event.event : event;
  const message = body.message;
  if (!isRecord(message)) {
    return null;
  }

  const chatId = typeof message.chat_id === "string" ? message.chat_id : null;
  if (chatId === null) {
    return null;
  }

  const chatType =
    typeof message.chat_type === "string" ? message.chat_type : null;
  const mentionedBot = isMentionedBot(message);
  // DMs always wake the agent; mentions_only only filters group chatter,
  // matching the Slack adapter's wake semantics.
  if (trigger === "mentions_only" && chatType !== "p2p" && !mentionedBot) {
    return null;
  }

  const text = extractText(message);
  if (text === null || text.length === 0) {
    return null;
  }

  return {
    chatId,
    chatType,
    sender: pickSenderId(body.sender),
    messageId:
      typeof message.message_id === "string" ? message.message_id : null,
    mentionedBot,
    text,
  };
}

// Inbound wakeups hand the agent chat ids (oc_...). Targets starting with ou_
// are open ids for direct messages; everything else is a chat id.
export function receiveIdTypeForTarget(target: string): "open_id" | "chat_id" {
  return target.startsWith("ou_") ? "open_id" : "chat_id";
}

// Feishu's idempotency key for message sends. Requests sharing a uuid deliver
// at most one message per hour, which is what makes a redelivery safe. The
// runtime's outbound message id is stable across delivery attempts, so it is
// the right value; Feishu caps the field at 50 characters.
const FEISHU_SEND_UUID_MAX_CHARS = 50;

export function feishuSendUuid(deliveryId: string): string {
  return deliveryId.slice(0, FEISHU_SEND_UUID_MAX_CHARS);
}

function extractText(message: Record<string, unknown>): string | null {
  // Text messages carry message.content as a JSON string like
  // {"text":"hello @_user_1 world"}. Mention placeholders are left in the text
  // on purpose: the agent sees who was addressed via mentioned_bot and the raw
  // placeholders carry no secrets.
  if (message.message_type !== "text") {
    return null;
  }
  if (typeof message.content !== "string") {
    return null;
  }
  try {
    const parsed: unknown = JSON.parse(message.content);
    if (!isRecord(parsed) || typeof parsed.text !== "string") {
      return null;
    }
    return parsed.text;
  } catch {
    return null;
  }
}

function isMentionedBot(message: Record<string, unknown>): boolean {
  // With the recommended im:message.group_at_msg permission Feishu only
  // delivers group messages that @-mention this bot, so any group message we
  // see already addresses us. This check is a backstop for tenants that granted
  // the broader im:message.group_msg permission; telling the bot's own mention
  // apart from others would need an extra bot-info call.
  const mentions = message.mentions;
  return Array.isArray(mentions) && mentions.length > 0;
}

function pickSenderId(sender: unknown): string | null {
  if (!isRecord(sender)) {
    return null;
  }
  const senderId = sender.sender_id;
  if (!isRecord(senderId)) {
    return null;
  }
  if (typeof senderId.open_id === "string") {
    return senderId.open_id;
  }
  if (typeof senderId.user_id === "string") {
    return senderId.user_id;
  }
  return null;
}
