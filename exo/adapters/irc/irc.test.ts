import { describe, expect, it } from "vitest";

import { isIrcErrorNumeric, parseIrcLine, shouldTrigger } from "./irc";

describe("IRC worker helpers", () => {
  it("parses PING and PRIVMSG lines", () => {
    expect(parseIrcLine("PING :abc")).toEqual({
      type: "ping",
      token: "abc",
    });
    expect(parseIrcLine(":alice!u@h PRIVMSG #exo :hello exo-bot")).toEqual({
      type: "privmsg",
      message: {
        nick: "alice",
        target: "#exo",
        text: "hello exo-bot",
        raw: ":alice!u@h PRIVMSG #exo :hello exo-bot",
      },
    });
  });

  it("applies mention trigger policy", () => {
    expect(shouldTrigger("mention", "exo-bot", "alice", "hello exo-bot")).toBe(
      true,
    );
    expect(shouldTrigger("mention", "exo-bot", "alice", "hello")).toBe(false);
    expect(shouldTrigger("mention", "exo-bot", "exo-bot", "exo-bot")).toBe(
      false,
    );
    expect(shouldTrigger("all_messages", "exo-bot", "alice", "hello")).toBe(
      true,
    );
  });

  it("detects IRC error numerics", () => {
    expect(
      isIrcErrorNumeric(
        ":cadmium.libera.chat 432 * exo-test-12345 :Erroneous Nickname",
      ),
    ).toBe(true);
    expect(isIrcErrorNumeric(":cadmium.libera.chat 001 exo :Welcome")).toBe(
      false,
    );
  });
});
