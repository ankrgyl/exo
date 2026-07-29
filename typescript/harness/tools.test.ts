import { describe, expect, it } from "vitest";

import { previewText } from "./tools";

const PREVIEW_CHARS = 4_000;

function hasLoneSurrogate(text: string): boolean {
  for (let i = 0; i < text.length; i += 1) {
    const code = text.charCodeAt(i);
    if (code >= 0xd800 && code <= 0xdbff) {
      const next = text.charCodeAt(i + 1);
      if (!(next >= 0xdc00 && next <= 0xdfff)) {
        return true;
      }
      i += 1;
    } else if (code >= 0xdc00 && code <= 0xdfff) {
      return true;
    }
  }
  return false;
}

describe("previewText", () => {
  it("returns short text unchanged", () => {
    expect(previewText("hello 🚒")).toBe("hello 🚒");
  });

  it("truncates long text with a marker", () => {
    const long = "a".repeat(PREVIEW_CHARS + 100);
    const preview = previewText(long);
    expect(preview.endsWith("\n...[truncated]")).toBe(true);
    expect(preview.length).toBeLessThan(long.length);
  });

  it("never cuts inside a surrogate pair at the truncation boundary", () => {
    // Emoji straddles the boundary: its high surrogate is the 4000th code
    // unit, the low surrogate the 4001st. A naive slice keeps only the high
    // half, which strict JSON parsers downstream reject.
    const straddling = "a".repeat(PREVIEW_CHARS - 1) + "🚒 and more text";
    const preview = previewText(straddling);
    expect(hasLoneSurrogate(preview)).toBe(false);
  });

  it("keeps a pair that fits entirely inside the boundary", () => {
    const fitting = "a".repeat(PREVIEW_CHARS - 2) + "🚒 and more text";
    const preview = previewText(fitting);
    expect(hasLoneSurrogate(preview)).toBe(false);
    expect(preview.includes("🚒")).toBe(true);
  });
});
