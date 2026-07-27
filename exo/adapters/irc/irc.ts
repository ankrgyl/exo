export type IrcTriggerPolicy = "mention" | "all_messages";

export type IrcPrivateMessage = {
  nick: string;
  target: string;
  text: string;
  raw: string;
};

export type IrcLine =
  | { type: "ping"; token: string }
  | { type: "privmsg"; message: IrcPrivateMessage }
  | { type: "other"; raw: string };

export function parseIrcLine(raw: string): IrcLine {
  const pingToken = raw.match(/^PING :(.*)$/)?.[1];
  if (pingToken !== undefined) {
    return { type: "ping", token: pingToken };
  }
  if (!raw.startsWith(":")) {
    return { type: "other", raw };
  }
  const [prefixPart, ...restParts] = raw.slice(1).split(" ");
  const rest = restParts.join(" ");
  if (!prefixPart || !rest.startsWith("PRIVMSG ")) {
    return { type: "other", raw };
  }
  const textIndex = rest.indexOf(" :");
  if (textIndex < 0) {
    return { type: "other", raw };
  }
  const target = rest.slice("PRIVMSG ".length, textIndex);
  const text = rest.slice(textIndex + 2);
  return {
    type: "privmsg",
    message: {
      nick: prefixPart.split("!")[0] ?? prefixPart,
      target,
      text,
      raw,
    },
  };
}

export function shouldTrigger(
  policy: IrcTriggerPolicy,
  nick: string,
  sender: string,
  text: string,
): boolean {
  if (sender.toLowerCase() === nick.toLowerCase()) {
    return false;
  }
  if (policy === "all_messages") {
    return true;
  }
  return text.toLowerCase().includes(nick.toLowerCase());
}

export function isIrcErrorNumeric(raw: string): boolean {
  if (!raw.startsWith(":")) {
    return false;
  }
  const code = raw.slice(1).split(" ")[1];
  return Boolean(code?.match(/^[45]\d\d$/));
}
