import type {
  HarnessToolRegistry,
  JsonObject,
  ToolInstance,
  ToolResult,
  TurnContext,
} from "@exo/harness";

import { modelFamily, type ModelFamily } from "../typescript/model-family";

// File editing tools, shape picked per model family: str_replace demands a
// unique match, apply_patch takes the first match from a moving cursor as
// codex does. Matching is host-side pure functions; the sandbox moves bytes.

// Cap reads to protect host memory; tools return a confirmation, not the file.
const MAX_FILE_CHARS = 2_000_000;
// EX_NOINPUT: "missing" is distinguishable from "unreadable" without stderr parsing.
const READ_MISSING_EXIT_CODE = 66;

export type EditFamilyTools = "str_replace" | "apply_patch";

// "unknown" (router aliases, self-hosted models) gets str_replace: a bad
// match fails loudly, a bad patch envelope doesn't.
export function editToolsForFamily(family: ModelFamily): EditFamilyTools {
  return family === "openai" ? "apply_patch" : "str_replace";
}

export type EditOutcome =
  | { ok: true; contents: string }
  | { ok: false; error: string };

export function applyStrReplace(
  contents: string,
  oldString: string,
  newString: string,
): EditOutcome {
  if (oldString.length === 0) {
    return { ok: false, error: "old_string must not be empty" };
  }
  if (oldString === newString) {
    return { ok: false, error: "old_string and new_string are identical" };
  }
  const first = contents.indexOf(oldString);
  if (first === -1) {
    return {
      ok: false,
      error:
        "old_string was not found in the file; read the file first and copy the exact text, including indentation",
    };
  }
  if (contents.indexOf(oldString, first + oldString.length) !== -1) {
    const count = countOccurrences(contents, oldString);
    return {
      ok: false,
      error: `old_string matches ${count} places; include more surrounding context so it matches exactly one`,
    };
  }
  return {
    ok: true,
    contents:
      contents.slice(0, first) +
      newString +
      contents.slice(first + oldString.length),
  };
}

function countOccurrences(haystack: string, needle: string): number {
  let count = 0;
  let index = haystack.indexOf(needle);
  while (index !== -1) {
    count += 1;
    index = haystack.indexOf(needle, index + needle.length);
  }
  return count;
}

// One contiguous block and its replacement: context+removed lines in
// `before`, context+added lines in `after`.
export interface PatchHunk {
  // Text after "@@" (codex's change_context), sought before matching `before`.
  changeContext: string | null;
  before: string[];
  after: string[];
  // Set by "*** End of File": `before` must sit at the end of the file.
  endOfFile: boolean;
}

export type PatchOp =
  | { kind: "add"; path: string; contents: string }
  | { kind: "delete"; path: string }
  | {
      kind: "update";
      path: string;
      // "*** Move to: <path>": written there, the original removed.
      moveTo: string | null;
      hunks: PatchHunk[];
    };

export type PatchParse =
  | { ok: true; ops: PatchOp[] }
  | { ok: false; error: string };

const BEGIN_PATCH = "*** Begin Patch";
const END_PATCH = "*** End Patch";
const ADD_FILE = "*** Add File: ";
const DELETE_FILE = "*** Delete File: ";
const UPDATE_FILE = "*** Update File: ";
const MOVE_TO = "*** Move to: ";
const CHANGE_CONTEXT = "@@ ";
const CHANGE_CONTEXT_EMPTY = "@@";
const END_OF_FILE = "*** End of File";

// Codex's apply_patch envelope, ported from codex-rs/apply-patch. An
// unrecognised directive rejects the whole patch.
export function parseApplyPatch(patch: string): PatchParse {
  const lines = patch.replace(/\r\n/g, "\n").split("\n");
  let index = 0;
  while (index < lines.length && lines[index].trim().length === 0) {
    index += 1;
  }
  // Trailing whitespace on markers is tolerated; leading is not (" *** x" is content).
  if ((lines[index] ?? "").trim() !== BEGIN_PATCH) {
    return { ok: false, error: `patch must start with "${BEGIN_PATCH}"` };
  }
  index += 1;

  const ops: PatchOp[] = [];
  let sawEnd = false;
  while (index < lines.length) {
    const line = lines[index];
    if (line.trimEnd() === END_PATCH) {
      sawEnd = true;
      index += 1;
      break;
    }
    if (line.trim().length === 0) {
      index += 1;
      continue;
    }
    if (line.startsWith(ADD_FILE)) {
      const path = line.slice(ADD_FILE.length).trim();
      const raw: string[] = [];
      index += 1;
      while (index < lines.length && !isDirective(lines[index])) {
        raw.push(lines[index]);
        index += 1;
      }
      const pathError = validatePath(path);
      if (pathError !== null) {
        return { ok: false, error: pathError };
      }
      let contents = "";
      for (const bodyLine of raw) {
        if (!bodyLine.startsWith("+")) {
          return {
            ok: false,
            error: `Add File body lines must start with "+" (got ${JSON.stringify(bodyLine)})`,
          };
        }
        // codex appends a newline per body line.
        contents += `${bodyLine.slice(1)}\n`;
      }
      ops.push({ kind: "add", path, contents });
      continue;
    }
    if (line.startsWith(DELETE_FILE)) {
      const path = line.slice(DELETE_FILE.length).trim();
      const pathError = validatePath(path);
      if (pathError !== null) {
        return { ok: false, error: pathError };
      }
      ops.push({ kind: "delete", path });
      index += 1;
      continue;
    }
    if (line.startsWith(UPDATE_FILE)) {
      const path = line.slice(UPDATE_FILE.length).trim();
      const pathError = validatePath(path);
      if (pathError !== null) {
        return { ok: false, error: pathError };
      }
      index += 1;
      let moveTo: string | null = null;
      if ((lines[index] ?? "").startsWith(MOVE_TO)) {
        moveTo = lines[index].slice(MOVE_TO.length).trim();
        const moveError = validatePath(moveTo);
        if (moveError !== null) {
          return { ok: false, error: moveError };
        }
        index += 1;
      }
      const hunks: PatchHunk[] = [];
      const openChunk = (changeContext: string | null): PatchHunk => {
        const chunk: PatchHunk = {
          changeContext,
          before: [],
          after: [],
          endOfFile: false,
        };
        hunks.push(chunk);
        return chunk;
      };
      while (index < lines.length && !isDirective(lines[index])) {
        const hunkLine = lines[index];
        index += 1;
        // "@@" opens a chunk; "@@ text" opens one anchored on that context.
        if (hunkLine === CHANGE_CONTEXT_EMPTY) {
          openChunk(null);
          continue;
        }
        if (hunkLine.startsWith(CHANGE_CONTEXT)) {
          openChunk(hunkLine.slice(CHANGE_CONTEXT.length));
          continue;
        }
        const chunk = hunks[hunks.length - 1] ?? openChunk(null);
        if (hunkLine.startsWith("+")) {
          chunk.after.push(hunkLine.slice(1));
          continue;
        }
        if (hunkLine.startsWith("-")) {
          chunk.before.push(hunkLine.slice(1));
          continue;
        }
        if (hunkLine.startsWith(" ") || hunkLine.length === 0) {
          // A bare empty line is context; a trailing one is handled at apply time.
          const text = hunkLine.length === 0 ? "" : hunkLine.slice(1);
          chunk.before.push(text);
          chunk.after.push(text);
          continue;
        }
        return {
          ok: false,
          error: `Update File lines must start with " ", "+", "-", or "@@" (got ${JSON.stringify(hunkLine)})`,
        };
      }
      if ((lines[index] ?? "").trimEnd() === END_OF_FILE) {
        index += 1;
        if (hunks.length > 0) {
          hunks[hunks.length - 1].endOfFile = true;
        }
        // codex ignores blank lines after the marker.
        while (index < lines.length && lines[index].length === 0) {
          index += 1;
        }
      }
      if (hunks.length === 0) {
        return { ok: false, error: `Update File ${path} has no hunks` };
      }
      ops.push({ kind: "update", path, moveTo, hunks });
      continue;
    }
    return {
      ok: false,
      error: `unrecognised patch directive: ${JSON.stringify(line)}`,
    };
  }
  if (!sawEnd) {
    return { ok: false, error: `patch must end with "${END_PATCH}"` };
  }
  if (ops.length === 0) {
    return { ok: false, error: "patch contains no file operations" };
  }
  return { ok: true, ops };
}

function isDirective(line: string): boolean {
  return line.startsWith("*** ");
}

// Models emit these where the file has the plain ASCII character.
const TYPOGRAPHIC: Record<string, string> = {
  // Dashes and the minus sign.
  "\u2010": "-",
  "\u2011": "-",
  "\u2012": "-",
  "\u2013": "-",
  "\u2014": "-",
  "\u2015": "-",
  "\u2212": "-",
  // Single quotes.
  "\u2018": "'",
  "\u2019": "'",
  "\u201a": "'",
  "\u201b": "'",
  // Double quotes.
  "\u201c": '"',
  "\u201d": '"',
  "\u201e": '"',
  "\u201f": '"',
  // Spaces.
  "\u00a0": " ",
  "\u2002": " ",
  "\u2003": " ",
  "\u2004": " ",
  "\u2005": " ",
  "\u2006": " ",
  "\u2007": " ",
  "\u2008": " ",
  "\u2009": " ",
  "\u200a": " ",
  "\u202f": " ",
  "\u205f": " ",
  "\u3000": " ",
};

function normalizeTypography(line: string): string {
  let out = "";
  for (const char of line) {
    out += TYPOGRAPHIC[char] ?? char;
  }
  return out;
}

// The relaxation ladder from codex's seek_sequence.
const MATCH_PASSES: ((a: string, b: string) => boolean)[] = [
  (a, b) => a === b,
  (a, b) => a.trimEnd() === b.trimEnd(),
  (a, b) => a.trim() === b.trim(),
  (a, b) => normalizeTypography(a.trim()) === normalizeTypography(b.trim()),
];

function matchesAt(
  lines: string[],
  block: string[],
  start: number,
  equal: (a: string, b: string) => boolean,
): boolean {
  for (let offset = 0; offset < block.length; offset += 1) {
    if (!equal(lines[start + offset], block[offset])) {
      return false;
    }
  }
  return true;
}

// Port of codex's seek_sequence: first match at or after `from` (first, not
// unique — hunks apply from an advancing cursor), each pass scanning the whole
// range before relaxing. endOfFile anchors the search to the tail, no fallback.
export function seekSequence(
  lines: string[],
  block: string[],
  from: number,
  endOfFile = false,
): number | null {
  if (block.length === 0) {
    return from;
  }
  if (block.length > lines.length) {
    return null;
  }
  const searchStart = endOfFile ? lines.length - block.length : from;
  for (const equal of MATCH_PASSES) {
    for (
      let start = searchStart;
      start + block.length <= lines.length;
      start += 1
    ) {
      if (matchesAt(lines, block, start, equal)) {
        return start;
      }
    }
  }
  return null;
}

interface Replacement {
  at: number;
  remove: number;
  insert: string[];
}

// Port of codex's compute_replacements: resolve all hunks against the
// original lines, then apply.
export function applyHunks(contents: string, hunks: PatchHunk[]): EditOutcome {
  const original = contents.split("\n");
  // Drop the final newline's empty element; restored on the way out.
  if (original[original.length - 1] === "") {
    original.pop();
  }
  const replacements: Replacement[] = [];
  let lineIndex = 0;
  for (const hunk of hunks) {
    if (hunk.changeContext !== null) {
      const found = seekSequence(
        original,
        [hunk.changeContext],
        lineIndex,
        false,
      );
      if (found === null) {
        return {
          ok: false,
          error: `could not find context ${JSON.stringify(hunk.changeContext)}`,
        };
      }
      lineIndex = found + 1;
    }
    if (hunk.before.length === 0) {
      // No context and no removals: a pure addition at end of file.
      replacements.push({
        at: original.length,
        remove: 0,
        insert: hunk.after,
      });
      continue;
    }
    let before = hunk.before;
    let after = hunk.after;
    let found = seekSequence(original, before, lineIndex, hunk.endOfFile);
    if (found === null && before[before.length - 1] === "") {
      // A trailing empty stands for the popped final newline; retry without it.
      before = before.slice(0, -1);
      if (after[after.length - 1] === "") {
        after = after.slice(0, -1);
      }
      found = seekSequence(original, before, lineIndex, hunk.endOfFile);
    }
    if (found === null) {
      const quoted = hunk.before.find((line) => line.trim().length > 0);
      return {
        ok: false,
        error: `hunk context was not found: ${JSON.stringify(quoted ?? hunk.before[0])}`,
      };
    }
    replacements.push({ at: found, remove: before.length, insert: after });
    lineIndex = found + before.length;
  }
  replacements.sort((left, right) => left.at - right.at);
  const out: string[] = [];
  let copied = 0;
  for (const replacement of replacements) {
    out.push(...original.slice(copied, replacement.at));
    out.push(...replacement.insert);
    copied = replacement.at + replacement.remove;
  }
  out.push(...original.slice(copied));
  // codex always terminates the file with a newline.
  if (out[out.length - 1] !== "") {
    out.push("");
  }
  return { ok: true, contents: out.join("\n") };
}

// Absolute paths only, so the process cwd never matters.
export function validatePath(path: string): string | null {
  if (path.length === 0) {
    return "path must not be empty";
  }
  if (!path.startsWith("/")) {
    return `path must be absolute (got ${JSON.stringify(path)})`;
  }
  if (path.split("/").includes("..")) {
    return "path must not contain '..' segments";
  }
  return null;
}

// The TurnContext subset these helpers need, so tests can fake it.
export type SandboxHandle = Pick<TurnContext, "startSandboxProcess">;

async function drain(stream: ReadableStream<string>): Promise<string> {
  const reader = stream.getReader();
  const chunks: string[] = [];
  let total = 0;
  try {
    for (;;) {
      const { done, value } = await reader.read();
      if (done) {
        break;
      }
      if (typeof value !== "string") {
        continue;
      }
      total += value.length;
      if (total > MAX_FILE_CHARS) {
        throw new Error(
          `file exceeds ${MAX_FILE_CHARS} characters; edit it with shell instead`,
        );
      }
      chunks.push(value);
    }
  } finally {
    reader.releaseLock();
  }
  return chunks.join("");
}

export interface ReadResult {
  exists: boolean;
  contents: string;
}

export async function readSandboxFile(
  sandbox: SandboxHandle,
  path: string,
): Promise<ReadResult> {
  // A dedicated exit code distinguishes "missing" from "unreadable".
  const process = await sandbox.startSandboxProcess({
    command: [
      "sh",
      "-c",
      // A directory is a real error, not a missing file.
      `if [ -d "$1" ]; then echo "is a directory" >&2; exit 1; fi; [ -f "$1" ] || exit ${READ_MISSING_EXIT_CODE}; cat -- "$1"`,
      "exo-edit-read",
      path,
    ],
  });
  // close() in a finally: drain() can throw mid-stream.
  try {
    const [contents, errorText, exitCode] = await Promise.all([
      drain(process.stdout),
      drain(process.stderr),
      process.wait(),
    ]);
    if (exitCode === READ_MISSING_EXIT_CODE) {
      return { exists: false, contents: "" };
    }
    if (exitCode !== 0) {
      throw new Error(
        `failed to read ${path}: ${errorText.trim() || `exit code ${String(exitCode)}`}`,
      );
    }
    if (contents.includes("\0")) {
      throw new Error(`${path} looks binary; these tools only edit text files`);
    }
    return { exists: true, contents };
  } finally {
    await process.close();
  }
}

export async function writeSandboxFile(
  sandbox: SandboxHandle,
  path: string,
  contents: string,
): Promise<void> {
  // Sibling temp then rename: same filesystem, nothing left behind on failure.
  const process = await sandbox.startSandboxProcess({
    command: [
      "sh",
      "-c",
      'tmp="$1.exo-edit-tmp"; if cat > "$tmp" && mv -- "$tmp" "$1"; then exit 0; fi; rm -f -- "$tmp"; exit 1',
      "exo-edit-write",
      path,
    ],
  });
  try {
    const stdout = drain(process.stdout);
    const stderr = drain(process.stderr);
    await process.writeStdin(contents);
    await process.closeStdin();
    const [, errorText, exitCode] = await Promise.all([
      stdout,
      stderr,
      process.wait(),
    ]);
    if (exitCode !== 0) {
      throw new Error(
        `failed to write ${path}: ${errorText.trim() || `exit code ${String(exitCode)}`}`,
      );
    }
  } finally {
    await process.close();
  }
}

async function deleteSandboxFile(
  sandbox: SandboxHandle,
  path: string,
): Promise<void> {
  const process = await sandbox.startSandboxProcess({
    command: ["sh", "-c", 'rm -f -- "$1"', "exo-edit-delete", path],
  });
  try {
    const [, errorText, exitCode] = await Promise.all([
      drain(process.stdout),
      drain(process.stderr),
      process.wait(),
    ]);
    if (exitCode !== 0) {
      throw new Error(
        `failed to delete ${path}: ${errorText.trim() || `exit code ${String(exitCode)}`}`,
      );
    }
  } finally {
    await process.close();
  }
}

function stringArg(args: JsonObject, name: string): string | null {
  const value = args[name];
  return typeof value === "string" ? value : null;
}

function strReplaceTool(): ToolInstance {
  return {
    source: "built_in",
    definition: {
      name: "str_replace",
      description:
        "Replace an exact string in a file in the sandbox. old_string must appear EXACTLY ONCE in the file: if it matches zero or several places the edit is rejected and nothing is written, so include enough surrounding context (including indentation) to be unambiguous. Prefer this over editing with shell/sed, which cannot tell you whether it changed what you intended. Paths must be absolute; Exo's own source tree is mounted at /workspace/exo.",
      parameters: {
        type: "object",
        additionalProperties: false,
        properties: {
          path: {
            type: "string",
            description: "Absolute path of the file to edit.",
          },
          old_string: {
            type: "string",
            description:
              "The exact text to replace, copied from the file including indentation.",
          },
          new_string: {
            type: "string",
            description: "The replacement text.",
          },
        },
        required: ["path", "old_string", "new_string"],
      },
    },
    handler: {
      async execute(args, execution): Promise<ToolResult> {
        const path = stringArg(args, "path");
        const oldString = stringArg(args, "old_string");
        const newString = stringArg(args, "new_string");
        if (path === null || oldString === null || newString === null) {
          return {
            ok: false,
            error: "path, old_string, and new_string must all be strings",
          };
        }
        const pathError = validatePath(path);
        if (pathError !== null) {
          return { ok: false, error: pathError };
        }
        const file = await readSandboxFile(execution.context, path);
        if (!file.exists) {
          return { ok: false, error: `${path} does not exist` };
        }
        const outcome = applyStrReplace(file.contents, oldString, newString);
        if (!outcome.ok) {
          return { ok: false, error: outcome.error };
        }
        await writeSandboxFile(execution.context, path, outcome.contents);
        return { ok: true, path, replaced: 1 };
      },
    },
  };
}

function createFileTool(): ToolInstance {
  return {
    source: "built_in",
    definition: {
      name: "create_file",
      description:
        "Create a new file in the sandbox with the given contents. Fails if the file already exists — use str_replace to modify an existing file. Paths must be absolute.",
      parameters: {
        type: "object",
        additionalProperties: false,
        properties: {
          path: {
            type: "string",
            description: "Absolute path of the file to create.",
          },
          contents: {
            type: "string",
            description: "Full contents of the new file.",
          },
        },
        required: ["path", "contents"],
      },
    },
    handler: {
      async execute(args, execution): Promise<ToolResult> {
        const path = stringArg(args, "path");
        const contents = stringArg(args, "contents");
        if (path === null || contents === null) {
          return { ok: false, error: "path and contents must both be strings" };
        }
        const pathError = validatePath(path);
        if (pathError !== null) {
          return { ok: false, error: pathError };
        }
        const existing = await readSandboxFile(execution.context, path);
        if (existing.exists) {
          return {
            ok: false,
            error: `${path} already exists; use str_replace to modify it`,
          };
        }
        await writeSandboxFile(execution.context, path, contents);
        return { ok: true, path, bytes: contents.length };
      },
    },
  };
}

function applyPatchTool(): ToolInstance {
  return {
    source: "built_in",
    definition: {
      name: "apply_patch",
      description:
        'Apply a patch to files in the sandbox. The patch is an envelope: "*** Begin Patch", then one or more of "*** Add File: <path>" (body lines prefixed "+"), "*** Delete File: <path>", or "*** Update File: <path>" (optionally "*** Move to: <path>" to rename it, then hunks of " " context, "-" removed and "+" added lines, separated by "@@" and closed by "*** End of File" if the hunk ends the file), then "*** End Patch". Hunks apply in order, each located at or after the previous one, so list them in the order they appear in the file. Context matching tolerates differing whitespace and typographic punctuation. The whole patch is resolved before anything is written: if any hunk cannot be located, nothing is written at all. Paths must be absolute; Exo\'s own source tree is mounted at /workspace/exo.',
      parameters: {
        type: "object",
        additionalProperties: false,
        properties: {
          patch: {
            type: "string",
            description: "The full patch envelope.",
          },
        },
        required: ["patch"],
      },
    },
    handler: {
      async execute(args, execution): Promise<ToolResult> {
        const patch = stringArg(args, "patch");
        if (patch === null) {
          return { ok: false, error: "patch must be a string" };
        }
        const parsed = parseApplyPatch(patch);
        if (!parsed.ok) {
          return { ok: false, error: parsed.error };
        }
        // Resolve everything against an in-memory working copy first: a
        // failing patch writes nothing, and ops on one file compose.
        const working = new Map<string, string | null>();
        const load = async (path: string): Promise<string | null> => {
          const cached = working.get(path);
          if (cached !== undefined) {
            return cached;
          }
          const file = await readSandboxFile(execution.context, path);
          const contents = file.exists ? file.contents : null;
          working.set(path, contents);
          return contents;
        };
        for (const op of parsed.ops) {
          const current = await load(op.path);
          if (op.kind === "add") {
            if (current !== null) {
              return {
                ok: false,
                error: `${op.path} already exists; use "*** Update File" instead of "*** Add File"`,
              };
            }
            working.set(op.path, op.contents);
            continue;
          }
          if (op.kind === "delete") {
            if (current === null) {
              return { ok: false, error: `${op.path} does not exist` };
            }
            working.set(op.path, null);
            continue;
          }
          if (current === null) {
            return { ok: false, error: `${op.path} does not exist` };
          }
          const outcome = applyHunks(current, op.hunks);
          if (!outcome.ok) {
            return { ok: false, error: `${op.path}: ${outcome.error}` };
          }
          if (op.moveTo === null) {
            working.set(op.path, outcome.contents);
            continue;
          }
          // "*** Move to": write the destination, delete the original.
          if ((await load(op.moveTo)) !== null && op.moveTo !== op.path) {
            return { ok: false, error: `${op.moveTo} already exists` };
          }
          working.set(op.path, null);
          working.set(op.moveTo, outcome.contents);
        }
        const touched = new Set(working.keys());
        for (const path of touched) {
          const contents = working.get(path) ?? null;
          if (contents === null) {
            await deleteSandboxFile(execution.context, path);
            continue;
          }
          await writeSandboxFile(execution.context, path, contents);
        }
        return {
          ok: true,
          files_changed: touched.size,
        };
      },
    },
  };
}

// Exported separately from registration so tests can exercise the handlers.
export function createEditToolInstances(family: ModelFamily): ToolInstance[] {
  return editToolsForFamily(family) === "apply_patch"
    ? [applyPatchTool()]
    : [strReplaceTool(), createFileTool()];
}

// `model` is the upstream id from the resolved binding, not the binding name.
export function registerEditTools(
  registry: HarnessToolRegistry,
  model: string,
): void {
  for (const tool of createEditToolInstances(modelFamily(model))) {
    registry.register(tool);
  }
}
