import { messageText, type Message } from "@exo/harness";

const PI_PRIOR_MESSAGE_MAX_CHARS = 8_000;
const PI_PRIOR_TOOL_RESULT_MAX_CHARS = 4_000;
const PI_PRIOR_HISTORY_MAX_CHARS = 24_000;

export interface CappedPiHistory {
  messages: Message[];
  sourceMessageCount: number;
  droppedMessageCount: number;
  truncatedMessageCount: number;
  textChars: number;
}

interface HistoryCandidate {
  message: Message;
  textChars: number;
  truncated: boolean;
}

export function capPiHistory(messages: Message[]): CappedPiHistory {
  const candidates = messages.map(toHistoryCandidate);
  const selected: HistoryCandidate[] = [];
  let textChars = 0;
  let droppedMessageCount = 0;

  for (let index = candidates.length - 1; index >= 0; index -= 1) {
    const candidate = candidates[index];
    if (
      selected.length > 0 &&
      textChars + candidate.textChars > PI_PRIOR_HISTORY_MAX_CHARS
    ) {
      droppedMessageCount = index + 1;
      break;
    }
    selected.push(candidate);
    textChars += candidate.textChars;
  }
  selected.reverse();

  return {
    messages: selected.map((candidate) => candidate.message),
    sourceMessageCount: candidates.length,
    droppedMessageCount,
    truncatedMessageCount: selected.filter((candidate) => candidate.truncated)
      .length,
    textChars,
  };
}

function toHistoryCandidate(message: Message): HistoryCandidate {
  const maxChars =
    message.role === "tool"
      ? PI_PRIOR_TOOL_RESULT_MAX_CHARS
      : PI_PRIOR_MESSAGE_MAX_CHARS;
  const { text, truncated } = truncateHistoryText(
    messageText(message),
    maxChars,
  );
  const cappedMessage: Message = truncated
    ? ({ role: message.role, content: text } as Message)
    : message;
  return {
    message: cappedMessage,
    textChars: text.length,
    truncated,
  };
}

function truncateHistoryText(
  text: string,
  maxChars: number,
): { text: string; truncated: boolean } {
  if (text.length <= maxChars) {
    return { text, truncated: false };
  }
  const omittedChars = text.length - maxChars;
  const suffix = `\n\n[truncated ${omittedChars} characters from prior conversation history]`;
  return {
    text: `${text.slice(0, Math.max(0, maxChars - suffix.length))}${suffix}`,
    truncated: true,
  };
}
