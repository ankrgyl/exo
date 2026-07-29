import { describe, expect, it } from "vitest";

import type { SandboxProcess } from "@exo/harness";

import {
  applyHunks,
  applyStrReplace,
  createEditToolInstances,
  editToolsForFamily,
  parseApplyPatch,
  readSandboxFile,
  validatePath,
  writeSandboxFile,
} from "./edit-tools";

describe("editToolsForFamily", () => {
  it("gives each family the shape it was post-trained on", () => {
    expect(editToolsForFamily("anthropic")).toBe("str_replace");
    expect(editToolsForFamily("openai")).toBe("apply_patch");
  });

  it("defaults unknown families to str_replace", () => {
    expect(editToolsForFamily("unknown")).toBe("str_replace");
  });
});

describe("applyStrReplace", () => {
  it("replaces a unique match", () => {
    const result = applyStrReplace("a\nb\nc\n", "b", "B");
    expect(result).toEqual({ ok: true, contents: "a\nB\nc\n" });
  });

  it("preserves surrounding bytes exactly", () => {
    const result = applyStrReplace("  indented(  )\n", "(  )", "(x)");
    expect(result).toEqual({ ok: true, contents: "  indented(x)\n" });
  });

  it("rejects a missing match rather than writing nothing silently", () => {
    const result = applyStrReplace("a\nb\n", "zzz", "x");
    expect(result.ok).toBe(false);
    if (!result.ok) {
      expect(result.error).toContain("not found");
    }
  });

  it("rejects an ambiguous match and reports the count", () => {
    // This is the case sed gets wrong: it would edit the first, or all.
    const result = applyStrReplace("x\nx\nx\n", "x", "y");
    expect(result.ok).toBe(false);
    if (!result.ok) {
      expect(result.error).toContain("3 places");
    }
  });

  it("rejects an empty old_string", () => {
    expect(applyStrReplace("a", "", "b").ok).toBe(false);
  });

  it("rejects a no-op replacement", () => {
    expect(applyStrReplace("a", "a", "a").ok).toBe(false);
  });

  it("allows deleting text", () => {
    expect(applyStrReplace("keep drop", " drop", "")).toEqual({
      ok: true,
      contents: "keep",
    });
  });
});

describe("validatePath", () => {
  it("accepts absolute paths", () => {
    expect(validatePath("/workspace/exo/examples/exo/harness.ts")).toBeNull();
  });

  it("rejects relative paths so the process cwd never matters", () => {
    expect(validatePath("examples/exo/harness.ts")).toContain("absolute");
  });

  it("rejects traversal segments", () => {
    expect(validatePath("/workspace/exo/../etc/passwd")).toContain("..");
  });

  it("rejects an empty path", () => {
    expect(validatePath("")).toContain("empty");
  });
});

describe("parseApplyPatch", () => {
  it("parses an update with context", () => {
    const parsed = parseApplyPatch(
      [
        "*** Begin Patch",
        "*** Update File: /workspace/exo/a.ts",
        "@@ function f()",
        " const a = 1;",
        "-const b = 2;",
        "+const b = 3;",
        "*** End Patch",
      ].join("\n"),
    );
    expect(parsed).toEqual({
      ok: true,
      ops: [
        {
          kind: "update",
          path: "/workspace/exo/a.ts",
          moveTo: null,
          hunks: [
            {
              changeContext: "function f()",
              before: ["const a = 1;", "const b = 2;"],
              after: ["const a = 1;", "const b = 3;"],
              endOfFile: false,
            },
          ],
        },
      ],
    });
  });

  it("splits hunks on @@ markers", () => {
    const parsed = parseApplyPatch(
      [
        "*** Begin Patch",
        "*** Update File: /a",
        "@@",
        "-one",
        "+ONE",
        "@@",
        "-two",
        "+TWO",
        "*** End Patch",
      ].join("\n"),
    );
    expect(parsed.ok).toBe(true);
    if (parsed.ok && parsed.ops[0].kind === "update") {
      expect(parsed.ops[0].hunks).toHaveLength(2);
    }
  });

  it("parses add and delete operations", () => {
    const parsed = parseApplyPatch(
      [
        "*** Begin Patch",
        "*** Add File: /new.ts",
        "+export const x = 1;",
        "+",
        "*** Delete File: /old.ts",
        "*** End Patch",
      ].join("\n"),
    );
    expect(parsed).toEqual({
      ok: true,
      ops: [
        { kind: "add", path: "/new.ts", contents: "export const x = 1;\n\n" },
        { kind: "delete", path: "/old.ts" },
      ],
    });
  });

  it("accepts CRLF input", () => {
    const parsed = parseApplyPatch(
      "*** Begin Patch\r\n*** Delete File: /a\r\n*** End Patch\r\n",
    );
    expect(parsed.ok).toBe(true);
  });

  it("requires the envelope", () => {
    expect(parseApplyPatch("*** Update File: /a\n-x\n+y\n").ok).toBe(false);
    expect(parseApplyPatch("*** Begin Patch\n*** Delete File: /a\n").ok).toBe(
      false,
    );
  });

  it("rejects relative paths inside the patch", () => {
    const parsed = parseApplyPatch(
      "*** Begin Patch\n*** Delete File: relative.ts\n*** End Patch",
    );
    expect(parsed.ok).toBe(false);
  });

  it("rejects unprefixed body lines", () => {
    const parsed = parseApplyPatch(
      "*** Begin Patch\n*** Update File: /a\nbare line\n*** End Patch",
    );
    expect(parsed.ok).toBe(false);
  });

  // Real codex directives; failing to parse them would reject valid patches.
  it('parses "*** End of File" as an anchor rather than rejecting', () => {
    const parsed = parseApplyPatch(
      [
        "*** Begin Patch",
        "*** Update File: /a",
        "-last",
        "+LAST",
        "*** End of File",
        "*** End Patch",
      ].join("\n"),
    );
    expect(parsed.ok).toBe(true);
    if (parsed.ok && parsed.ops[0].kind === "update") {
      expect(parsed.ops[0].hunks[0].endOfFile).toBe(true);
    }
  });

  it('parses "*** Move to:" as a rename', () => {
    const parsed = parseApplyPatch(
      [
        "*** Begin Patch",
        "*** Update File: /old.ts",
        "*** Move to: /new.ts",
        "-x",
        "+y",
        "*** End Patch",
      ].join("\n"),
    );
    expect(parsed.ok).toBe(true);
    if (parsed.ok && parsed.ops[0].kind === "update") {
      expect(parsed.ops[0].moveTo).toBe("/new.ts");
    }
  });

  it("rejects an empty patch", () => {
    expect(parseApplyPatch("*** Begin Patch\n*** End Patch").ok).toBe(false);
  });

  // codex keeps a trailing blank as context; apply time retries without it.
  it("keeps a trailing blank line as context", () => {
    const parsed = parseApplyPatch(
      [
        "*** Begin Patch",
        "*** Update File: /a",
        "-one",
        "+ONE",
        "",
        "*** End Patch",
      ].join("\n"),
    );
    expect(parsed.ok).toBe(true);
    if (parsed.ok && parsed.ops[0].kind === "update") {
      expect(parsed.ops[0].hunks[0].before).toEqual(["one", ""]);
    }
  });

  it("records the text after @@ as the chunk's context", () => {
    const parsed = parseApplyPatch(
      [
        "*** Begin Patch",
        "*** Update File: /a",
        "@@ def f():",
        "-x",
        "+y",
        "*** End Patch",
      ].join("\n"),
    );
    expect(parsed.ok).toBe(true);
    if (parsed.ok && parsed.ops[0].kind === "update") {
      expect(parsed.ops[0].hunks[0].changeContext).toBe("def f():");
    }
  });

  // Add File body lines must carry "+"; a bare blank is an error.
  it("rejects a bare blank line in an Add File body", () => {
    const parsed = parseApplyPatch(
      [
        "*** Begin Patch",
        "*** Add File: /new.ts",
        "+x",
        "",
        "*** End Patch",
      ].join("\n"),
    );
    expect(parsed.ok).toBe(false);
  });

  // " " is context and "+" an added blank; only bare "" is a separator.
  it("keeps deliberately marked blank lines", () => {
    const parsed = parseApplyPatch(
      [
        "*** Begin Patch",
        "*** Update File: /a",
        " one",
        " ",
        "-two",
        "+TWO",
        "*** End Patch",
      ].join("\n"),
    );
    expect(parsed.ok).toBe(true);
    if (parsed.ok && parsed.ops[0].kind === "update") {
      expect(parsed.ops[0].hunks[0].before).toEqual(["one", "", "two"]);
    }
  });

  it("keeps interior blank lines as context", () => {
    const parsed = parseApplyPatch(
      [
        "*** Begin Patch",
        "*** Update File: /a",
        " one",
        "",
        "-two",
        "+TWO",
        "*** End Patch",
      ].join("\n"),
    );
    expect(parsed.ok).toBe(true);
    if (parsed.ok && parsed.ops[0].kind === "update") {
      expect(parsed.ops[0].hunks[0].before).toEqual(["one", "", "two"]);
    }
  });

  it("tolerates trailing whitespace on the envelope markers", () => {
    const parsed = parseApplyPatch(
      "*** Begin Patch  \n*** Delete File: /a\n*** End Patch  ",
    );
    expect(parsed.ok).toBe(true);
  });

  // Leading whitespace is content: " *** x" is not a directive.
  it("does not treat an indented directive-like line as a directive", () => {
    const parsed = parseApplyPatch(
      [
        "*** Begin Patch",
        "*** Update File: /a",
        " *** not a directive",
        "-x",
        "+y",
        "*** End Patch",
      ].join("\n"),
    );
    expect(parsed.ok).toBe(true);
    if (parsed.ok && parsed.ops[0].kind === "update") {
      expect(parsed.ops[0].hunks[0].before).toEqual([
        "*** not a directive",
        "x",
      ]);
    }
  });
});

describe("applyHunks", () => {
  it("applies a unique hunk", () => {
    const result = applyHunks("one\ntwo\nthree\n", [
      {
        changeContext: null,
        before: ["two"],
        after: ["TWO"],
        endOfFile: false,
      },
    ]);
    expect(result).toEqual({ ok: true, contents: "one\nTWO\nthree\n" });
  });

  it("applies several hunks in order", () => {
    const result = applyHunks("a\nb\nc\n", [
      { changeContext: null, before: ["a"], after: ["A"], endOfFile: false },
      { changeContext: null, before: ["c"], after: ["C"], endOfFile: false },
    ]);
    expect(result).toEqual({ ok: true, contents: "A\nb\nC\n" });
  });

  it("applies to the first match, as apply_patch is positional", () => {
    expect(
      applyHunks("x\ny\nx\n", [
        { changeContext: null, before: ["x"], after: ["z"], endOfFile: false },
      ]),
    ).toEqual({
      ok: true,
      contents: "z\ny\nx\n",
    });
  });

  it("rejects a hunk whose context is absent", () => {
    const result = applyHunks("a\n", [
      { changeContext: null, before: ["nope"], after: ["x"], endOfFile: false },
    ]);
    expect(result.ok).toBe(false);
    if (!result.ok) {
      expect(result.error).toContain("not found");
    }
  });

  it("appends a chunk that has no context lines", () => {
    // codex treats empty old_lines as a pure addition at end of file.
    expect(
      applyHunks("a\n", [
        { changeContext: null, before: [], after: ["added"], endOfFile: false },
      ]),
    ).toEqual({ ok: true, contents: "a\nadded\n" });
  });

  it("seeks past @@ context before matching", () => {
    // "x" appears in both functions; the context picks the second.
    expect(
      applyHunks("function a() {\nx\n}\nfunction b() {\nx\n}\n", [
        {
          changeContext: "function b() {",
          before: ["x"],
          after: ["FOUND"],
          endOfFile: false,
        },
      ]),
    ).toEqual({
      ok: true,
      contents: "function a() {\nx\n}\nfunction b() {\nFOUND\n}\n",
    });
  });

  it("retries without a trailing empty context line", () => {
    // Direct search fails on the trailing ""; the retry without it succeeds.
    expect(
      applyHunks("a\nb\n", [
        {
          changeContext: null,
          before: ["b", ""],
          after: ["B", ""],
          endOfFile: false,
        },
      ]),
    ).toEqual({ ok: true, contents: "a\nB\n" });
  });

  it("can delete lines", () => {
    expect(
      applyHunks("a\nb\nc\n", [
        { changeContext: null, before: ["b"], after: [], endOfFile: false },
      ]),
    ).toEqual({
      ok: true,
      contents: "a\nc\n",
    });
  });

  // A hunk can open on a blank, so quoting before[0] would say `not found: ""`.
  it("quotes the first line with content when context is missing", () => {
    const result = applyHunks("a\n", [
      {
        changeContext: null,
        before: ["", "missing line"],
        after: ["x"],
        endOfFile: false,
      },
    ]);
    expect(result.ok).toBe(false);
    if (!result.ok) {
      expect(result.error).toContain("missing line");
    }
  });

  it("matches when the patch differs only in trailing whitespace", () => {
    expect(
      applyHunks("const a = 1;   \nconst b = 2;\n", [
        {
          changeContext: null,
          before: ["const a = 1;"],
          after: ["const a = 9;"],
          endOfFile: false,
        },
      ]),
    ).toEqual({ ok: true, contents: "const a = 9;\nconst b = 2;\n" });
  });

  it("matches when the patch differs only in indentation", () => {
    expect(
      applyHunks("    indented();\n", [
        {
          changeContext: null,
          before: ["indented();"],
          after: ["  moved();"],
          endOfFile: false,
        },
      ]),
    ).toEqual({ ok: true, contents: "  moved();\n" });
  });

  // A model writes an EN DASH where the file has a plain hyphen.
  it("matches across typographic punctuation", () => {
    expect(
      applyHunks("// a - b\n", [
        {
          changeContext: null,
          before: ["// a \u2013 b"],
          after: ["// a + b"],
          endOfFile: false,
        },
      ]),
    ).toEqual({ ok: true, contents: "// a + b\n" });
  });

  it("matches across curly quotes", () => {
    expect(
      applyHunks('const s = "hi";\n', [
        {
          changeContext: null,
          before: ["const s = \u201chi\u201d;"],
          after: ['const s = "bye";'],
          endOfFile: false,
        },
      ]),
    ).toEqual({ ok: true, contents: 'const s = "bye";\n' });
  });

  it("prefers an exact match over a relaxed one", () => {
    // The exact pass wins before any relaxation is considered.
    expect(
      applyHunks("x\n  x\n", [
        {
          changeContext: null,
          before: ["x"],
          after: ["EXACT"],
          endOfFile: false,
        },
      ]),
    ).toEqual({ ok: true, contents: "EXACT\n  x\n" });
  });

  it("edits two identical lines in sequence", () => {
    // Editing the same text in two places is normal; uniqueness would reject it.
    expect(
      applyHunks("x\nx\n", [
        { changeContext: null, before: ["x"], after: ["A"], endOfFile: false },
        { changeContext: null, before: ["x"], after: ["B"], endOfFile: false },
      ]),
    ).toEqual({ ok: true, contents: "A\nB\n" });
  });

  it("anchors an end-of-file hunk at the tail", () => {
    // "x" appears twice; the anchor picks the last one rather than the first.
    expect(
      applyHunks("x\ny\nx\n", [
        {
          changeContext: null,
          before: ["x"],
          after: ["LAST"],
          endOfFile: true,
        },
      ]),
    ).toEqual({ ok: true, contents: "x\ny\nLAST\n" });
  });

  it("tolerates a trailing newline when anchoring at the tail", () => {
    // "a\nb\n" splits with a trailing ""; the last content line sits before it.
    expect(
      applyHunks("a\nb\n", [
        { changeContext: null, before: ["b"], after: ["B"], endOfFile: true },
      ]),
    ).toEqual({ ok: true, contents: "a\nB\n" });
  });

  it("never matches behind the cursor", () => {
    // An out-of-order patch is refused, not silently misapplied.
    const result = applyHunks("first\nsecond\n", [
      {
        changeContext: null,
        before: ["second"],
        after: ["SECOND"],
        endOfFile: false,
      },
      {
        changeContext: null,
        before: ["first"],
        after: ["FIRST"],
        endOfFile: false,
      },
    ]);
    expect(result.ok).toBe(false);
    if (!result.ok) {
      expect(result.error).toContain("first");
    }
  });
});

// In-memory sandbox fake; records commands so tests can assert on invocation.
function streamOf(text: string): ReadableStream<string> {
  return new ReadableStream<string>({
    start(controller) {
      if (text.length > 0) {
        controller.enqueue(text);
      }
      controller.close();
    },
  });
}

interface FakeRun {
  stdout?: string;
  stderr?: string;
  exitCode: number | null;
}

class FakeSandbox {
  readonly commands: string[][] = [];
  readonly stdin: string[] = [];
  closed = 0;

  constructor(private readonly run: (command: string[]) => FakeRun) {}

  startSandboxProcess = async (request: {
    command: string[];
  }): Promise<SandboxProcess> => {
    this.commands.push(request.command);
    const result = this.run(request.command);
    const stdin = this.stdin;
    const markClosed = (): void => {
      this.closed += 1;
    };
    return {
      reused: false,
      stdout: streamOf(result.stdout ?? ""),
      stderr: streamOf(result.stderr ?? ""),
      async writeStdin(data: string) {
        stdin.push(data);
      },
      async closeStdin() {},
      async close() {
        markClosed();
      },
      async wait() {
        return result.exitCode;
      },
    };
  };
}

describe("readSandboxFile", () => {
  it("returns file contents on success", async () => {
    const sandbox = new FakeSandbox(() => ({
      stdout: "hello\nworld\n",
      exitCode: 0,
    }));
    await expect(readSandboxFile(sandbox, "/a.ts")).resolves.toEqual({
      exists: true,
      contents: "hello\nworld\n",
    });
  });

  it("reports a missing file without throwing", async () => {
    const sandbox = new FakeSandbox(() => ({ exitCode: 66 }));
    await expect(readSandboxFile(sandbox, "/gone.ts")).resolves.toEqual({
      exists: false,
      contents: "",
    });
  });

  it("throws with stderr text on an unexpected failure", async () => {
    const sandbox = new FakeSandbox(() => ({
      stderr: "is a directory\n",
      exitCode: 1,
    }));
    await expect(readSandboxFile(sandbox, "/dir")).rejects.toThrow(
      "is a directory",
    );
  });

  it("treats a null exit code as failure", async () => {
    const sandbox = new FakeSandbox(() => ({ exitCode: null }));
    await expect(readSandboxFile(sandbox, "/a")).rejects.toThrow(
      "failed to read",
    );
  });

  it("refuses binary files", async () => {
    const sandbox = new FakeSandbox(() => ({
      stdout: "text\0binary",
      exitCode: 0,
    }));
    await expect(readSandboxFile(sandbox, "/a.png")).rejects.toThrow("binary");
  });

  it("refuses a file over the size cap", async () => {
    const sandbox = new FakeSandbox(() => ({
      stdout: "x".repeat(2_000_001),
      exitCode: 0,
    }));
    await expect(readSandboxFile(sandbox, "/big")).rejects.toThrow("exceeds");
    expect(sandbox.closed).toBe(1);
  });

  it("closes the process even when it throws", async () => {
    const sandbox = new FakeSandbox(() => ({
      stdout: "\0",
      exitCode: 0,
    }));
    await expect(readSandboxFile(sandbox, "/a")).rejects.toThrow();
    expect(sandbox.closed).toBe(1);
  });

  // The path is a positional "$1", never spliced into the script text.
  it("passes the path as a positional argument, not interpolated", async () => {
    const sandbox = new FakeSandbox(() => ({ exitCode: 66 }));
    const nasty = '/tmp/a";rm -rf /;"';
    await readSandboxFile(sandbox, nasty);
    const command = sandbox.commands[0];
    expect(command[0]).toBe("sh");
    expect(command[1]).toBe("-c");
    expect(command[2]).not.toContain("rm -rf");
    expect(command[command.length - 1]).toBe(nasty);
  });
});

describe("writeSandboxFile", () => {
  it("sends the contents on stdin and succeeds on exit 0", async () => {
    const sandbox = new FakeSandbox(() => ({ exitCode: 0 }));
    await writeSandboxFile(sandbox, "/a.ts", "new contents\n");
    expect(sandbox.stdin).toEqual(["new contents\n"]);
    expect(sandbox.closed).toBe(1);
  });

  it("writes via a temp file and renames", async () => {
    const sandbox = new FakeSandbox(() => ({ exitCode: 0 }));
    await writeSandboxFile(sandbox, "/a.ts", "x");
    const script = sandbox.commands[0][2];
    expect(script).toContain(".exo-edit-tmp");
    expect(script).toContain("mv --");
    // The temp file must be cleaned up on failure.
    expect(script).toContain("rm -f --");
  });

  it("throws with stderr text on failure", async () => {
    const sandbox = new FakeSandbox(() => ({
      stderr: "no space left on device",
      exitCode: 1,
    }));
    await expect(writeSandboxFile(sandbox, "/a.ts", "x")).rejects.toThrow(
      "no space left on device",
    );
    expect(sandbox.closed).toBe(1);
  });
});

describe("apply_patch handler", () => {
  function applyPatchHandler() {
    const [tool] = createEditToolInstances("openai");
    return tool;
  }

  it("is the only tool registered for the OpenAI family", () => {
    expect(applyPatchHandler().definition.name).toBe("apply_patch");
  });

  it("composes two blocks that touch the same file", () => {
    // A second block on one file sees the first block's result.
    const sandbox = new FakeSandbox((command) =>
      command[2].includes("cat >")
        ? { exitCode: 0 }
        : { stdout: "one\ntwo\n", exitCode: 0 },
    );
    return applyPatchHandler()
      .handler.execute(
        {
          patch: [
            "*** Begin Patch",
            "*** Update File: /a",
            "-one",
            "+ONE",
            "*** Update File: /a",
            "-two",
            "+TWO",
            "*** End Patch",
          ].join("\n"),
        },
        { context: sandbox as never },
      )
      .then((result) => {
        expect(result).toEqual({ ok: true, files_changed: 1 });
        expect(sandbox.stdin).toEqual(["ONE\nTWO\n"]);
      });
  });

  it("writes nothing when one hunk in a multi-file patch fails", async () => {
    const sandbox = new FakeSandbox((command) => {
      const path = command[command.length - 1];
      return path === "/a"
        ? { stdout: "one\n", exitCode: 0 }
        : { stdout: "nope\n", exitCode: 0 };
    });
    const result = await applyPatchHandler().handler.execute(
      {
        patch: [
          "*** Begin Patch",
          "*** Update File: /a",
          "-one",
          "+ONE",
          "*** Update File: /b",
          "-missing",
          "+x",
          "*** End Patch",
        ].join("\n"),
      },
      { context: sandbox as never },
    );
    expect(result).toMatchObject({ ok: false });
    expect(sandbox.stdin).toEqual([]);
  });

  it("applies a valid single-file patch", async () => {
    const sandbox = new FakeSandbox((command) =>
      command[2].includes("cat >")
        ? { exitCode: 0 }
        : { stdout: "one\ntwo\n", exitCode: 0 },
    );
    const result = await applyPatchHandler().handler.execute(
      {
        patch: [
          "*** Begin Patch",
          "*** Update File: /a",
          "-one",
          "+ONE",
          "*** End Patch",
        ].join("\n"),
      },
      { context: sandbox as never },
    );
    expect(result).toEqual({ ok: true, files_changed: 1 });
    expect(sandbox.stdin).toEqual(["ONE\ntwo\n"]);
  });
});
