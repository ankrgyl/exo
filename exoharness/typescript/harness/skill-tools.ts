import type {
  Agent,
  ArtifactVersion,
  JsonObject,
  JsonValue,
  Message,
  ToolResult,
  TurnContext,
} from "./index";
import type { HarnessToolRegistry, ToolInstance } from "./tools";

// Durable, installable skills following the agentskills.io standard: a skill
// is a SKILL.md (YAML frontmatter with name + description, markdown body of
// instructions) plus optional supporting text files. Stored as agent
// artifacts so skills persist across conversations and survive sandbox
// rewinds. See exoharness/docs/design/skills.md for the design.
//
// Progressive disclosure: skillsInstruction injects only name + description
// each turn; use_skill loads the body; read_skill_file loads bundled files.
const SKILLS_INDEX_ARTIFACT_PATH = "skills/index.json";

// agentskills.io spec: lowercase alphanumeric with single hyphens, 1-64 chars.
const SKILL_NAME_PATTERN = /^[a-z0-9]+(-[a-z0-9]+)*$/;
// `index` would make skillArtifactPath collide with the catalog at
// SKILLS_INDEX_ARTIFACT_PATH, shadowing the index and bricking the skill.
const RESERVED_SKILL_NAMES = new Set(["index"]);
const MAX_NAME_CHARS = 64;
const MAX_DESCRIPTION_CHARS = 1024;
// Soft caps so a single install cannot make the store unreasonably large.
const MAX_SKILL_MD_CHARS = 100_000;
const MAX_FILE_CHARS = 200_000;
const MAX_FILES = 64;

export interface SkillFile {
  path: string;
  contents: string;
}

export interface SkillRecord {
  name: string;
  description: string;
  skillMd: string;
  files: SkillFile[];
}

interface SkillIndexEntry {
  name: string;
  description: string;
  installedAt: string;
  updatedAt: string;
}

interface SkillsIndex {
  skills: SkillIndexEntry[];
}

// The artifact subset both Agent and test fakes provide.
type SkillsHandle = Pick<
  Agent,
  "listArtifacts" | "readArtifactJson" | "writeArtifactJson"
>;

function skillsHandle(context: TurnContext): SkillsHandle {
  return context.exoharness.current.agent;
}

function skillArtifactPath(name: string): string {
  return `skills/${name}.json`;
}

// Reads and validates the index. Throws on a corrupt artifact so the write
// path refuses to bury it; skillsInstruction catches this and degrades. A
// missing artifact is a legitimately empty catalog.
async function readIndex(handle: SkillsHandle): Promise<SkillsIndex> {
  let raw: unknown;
  try {
    raw = await readLatestArtifactJson(handle, SKILLS_INDEX_ARTIFACT_PATH);
  } catch (cause) {
    throw new Error(
      `corrupt skills index artifact ${SKILLS_INDEX_ARTIFACT_PATH}: not valid JSON`,
      { cause },
    );
  }
  if (raw === null) {
    return { skills: [] };
  }
  if (!isSkillsIndex(raw)) {
    throw new Error(
      `corrupt skills index artifact ${SKILLS_INDEX_ARTIFACT_PATH}: invalid index shape`,
    );
  }
  return raw;
}

// TODO(storage-rework): install/uninstall are read-modify-write on the index
// with no compare-and-swap, same as the exo memory store. Two
// conversations mutating skills concurrently can lose one index update. Fix
// alongside the artifact versioning rework.
async function writeIndex(
  handle: SkillsHandle,
  index: SkillsIndex,
): Promise<void> {
  await handle.writeArtifactJson({
    path: SKILLS_INDEX_ARTIFACT_PATH,
    value: index as unknown as JsonValue,
  });
}

async function readSkill(
  handle: SkillsHandle,
  name: string,
): Promise<SkillRecord | null> {
  const raw = await readLatestArtifactJson(handle, skillArtifactPath(name));
  if (raw === null || !isSkillRecord(raw)) {
    return null;
  }
  return raw;
}

// Minimal YAML frontmatter reader: extracts top-level `key: value` scalar
// lines from the --- delimited block. Nested structures (e.g. a metadata
// map) are preserved in the raw skillMd but not interpreted, matching how we
// treat spec-optional fields.
export function parseSkillFrontmatter(
  skillMd: string,
): { fields: Record<string, string>; body: string } | null {
  const match = /^---\r?\n([\s\S]*?)\r?\n---\r?\n?([\s\S]*)$/.exec(skillMd);
  if (!match) {
    return null;
  }
  const fields: Record<string, string> = {};
  for (const line of match[1].split(/\r?\n/)) {
    const entry = /^([A-Za-z][A-Za-z0-9_-]*):\s*(.*)$/.exec(line);
    if (!entry) {
      continue;
    }
    let value = entry[2].trim();
    if (
      (value.startsWith('"') && value.endsWith('"') && value.length >= 2) ||
      (value.startsWith("'") && value.endsWith("'") && value.length >= 2)
    ) {
      value = value.slice(1, -1);
    }
    fields[entry[1]] = value;
  }
  return { fields, body: match[2] };
}

function validateSkillName(name: string): string | null {
  if (name.length === 0 || name.length > MAX_NAME_CHARS) {
    return `frontmatter name must be 1-${MAX_NAME_CHARS} characters`;
  }
  if (!SKILL_NAME_PATTERN.test(name)) {
    return "frontmatter name must be lowercase letters, digits, and single hyphens (e.g. pdf-processing)";
  }
  if (RESERVED_SKILL_NAMES.has(name)) {
    return `${name} is a reserved skill name; choose a different name`;
  }
  return null;
}

function validateFilePath(path: string): string | null {
  if (path.length === 0) {
    return "file path is required";
  }
  if (path.startsWith("/") || path.includes("\\")) {
    return `file path must be relative with forward slashes: ${path}`;
  }
  const segments = path.split("/");
  if (segments.some((segment) => segment === "" || segment === "..")) {
    return `file path must not contain empty or .. segments: ${path}`;
  }
  return null;
}

function parseFilesArg(value: unknown): SkillFile[] | string {
  if (value === undefined || value === null) {
    return [];
  }
  if (!Array.isArray(value)) {
    return "files must be an array of { path, contents }";
  }
  if (value.length > MAX_FILES) {
    return `at most ${MAX_FILES} files per skill`;
  }
  const files: SkillFile[] = [];
  const seen = new Set<string>();
  for (const item of value) {
    if (
      typeof item !== "object" ||
      item === null ||
      typeof (item as { path?: unknown }).path !== "string" ||
      typeof (item as { contents?: unknown }).contents !== "string"
    ) {
      return "each file must be { path: string, contents: string }";
    }
    const file = item as { path: string; contents: string };
    const pathError = validateFilePath(file.path);
    if (pathError !== null) {
      return pathError;
    }
    if (file.contents.length > MAX_FILE_CHARS) {
      return `file ${file.path} exceeds ${MAX_FILE_CHARS} characters`;
    }
    if (seen.has(file.path)) {
      return `duplicate file path: ${file.path}`;
    }
    seen.add(file.path);
    files.push({ path: file.path, contents: file.contents });
  }
  return files;
}

function installSkillTool(): ToolInstance {
  return {
    source: "built_in",
    definition: {
      name: "install_skill",
      description:
        "Install (or update) a durable skill for this agent. A skill is a SKILL.md in the standard agent-skills format: YAML frontmatter with required name and description fields, then a markdown body of instructions. Optionally bundle supporting text files with relative paths (e.g. scripts/convert.py, references/api.md). The skill persists across conversations; its name and description are shown to you every turn, and you load the body with use_skill when a task matches. Installing an existing name updates it.",
      parameters: {
        type: "object",
        additionalProperties: false,
        properties: {
          skillMd: {
            type: "string",
            description:
              "Full SKILL.md contents. Must start with YAML frontmatter: ---\\nname: my-skill\\ndescription: What it does and when to use it.\\n---\\nfollowed by the markdown instructions. The name must be lowercase letters/digits/hyphens; it identifies the skill.",
          },
          files: {
            type: ["array", "null"],
            description:
              "Supporting text files bundled with the skill, or null for none.",
            items: {
              type: "object",
              additionalProperties: false,
              properties: {
                path: {
                  type: "string",
                  description:
                    "Relative path within the skill, e.g. scripts/run.sh.",
                },
                contents: {
                  type: "string",
                  description: "UTF-8 text contents of the file.",
                },
              },
              required: ["path", "contents"],
            },
          },
        },
        required: ["skillMd", "files"],
      },
    },
    handler: {
      async execute(args: JsonObject, execution): Promise<ToolResult> {
        const skillMd = typeof args.skillMd === "string" ? args.skillMd : "";
        if (skillMd.trim().length === 0) {
          return { ok: false, error: "skillMd is required" };
        }
        if (skillMd.length > MAX_SKILL_MD_CHARS) {
          return {
            ok: false,
            error: `skillMd exceeds ${MAX_SKILL_MD_CHARS} characters`,
          };
        }
        const parsed = parseSkillFrontmatter(skillMd);
        if (parsed === null) {
          return {
            ok: false,
            error:
              "skillMd must start with YAML frontmatter delimited by --- lines",
          };
        }
        const name = parsed.fields.name ?? "";
        const nameError = validateSkillName(name);
        if (nameError !== null) {
          return { ok: false, error: nameError };
        }
        const description = parsed.fields.description ?? "";
        if (
          description.length === 0 ||
          description.length > MAX_DESCRIPTION_CHARS
        ) {
          return {
            ok: false,
            error: `frontmatter description must be 1-${MAX_DESCRIPTION_CHARS} characters and should say what the skill does and when to use it`,
          };
        }
        const files = parseFilesArg(args.files);
        if (typeof files === "string") {
          return { ok: false, error: files };
        }
        const handle = skillsHandle(execution.context);
        // Validate the index is readable before writing anything, so a
        // corrupt catalog is surfaced instead of being overwritten.
        const index = await readIndex(handle);
        const record: SkillRecord = { name, description, skillMd, files };
        // Write content first, then publish in the index: a listed skill must
        // always have readable content.
        await handle.writeArtifactJson({
          path: skillArtifactPath(name),
          value: record as unknown as JsonValue,
        });
        const now = new Date().toISOString();
        const existing = index.skills.find((entry) => entry.name === name);
        const updated = existing !== undefined;
        if (existing) {
          existing.description = description;
          existing.updatedAt = now;
        } else {
          index.skills.push({
            name,
            description,
            installedAt: now,
            updatedAt: now,
          });
        }
        await writeIndex(handle, index);
        return { ok: true, name, updated, files: files.length };
      },
    },
  };
}

function listSkillsTool(): ToolInstance {
  return {
    source: "built_in",
    definition: {
      name: "list_skills",
      description:
        "List installed skills with their descriptions and timestamps. Use use_skill(name) to load a skill's full instructions.",
      parameters: {
        type: "object",
        additionalProperties: false,
        properties: {},
        required: [],
      },
    },
    handler: {
      async execute(_args: JsonObject, execution): Promise<ToolResult> {
        const index = await readIndex(skillsHandle(execution.context));
        return { ok: true, skills: index.skills as unknown as JsonValue };
      },
    },
  };
}

function useSkillTool(): ToolInstance {
  return {
    source: "built_in",
    definition: {
      name: "use_skill",
      description:
        "Load a skill's full SKILL.md instructions plus the paths of its bundled files. Call this before performing a task that matches an installed skill's description, then follow the returned instructions. Read bundled files with read_skill_file only when the instructions need them.",
      parameters: {
        type: "object",
        additionalProperties: false,
        properties: {
          name: {
            type: "string",
            description:
              "The skill name, as shown in the installed-skills list.",
          },
        },
        required: ["name"],
      },
    },
    handler: {
      async execute(args: JsonObject, execution): Promise<ToolResult> {
        const name = typeof args.name === "string" ? args.name.trim() : "";
        if (validateSkillName(name) !== null) {
          return { ok: false, error: "unknown skill name" };
        }
        const handle = skillsHandle(execution.context);
        const index = await readIndex(handle);
        if (!index.skills.some((entry) => entry.name === name)) {
          return {
            ok: false,
            error: `skill not installed: ${name}. Use list_skills to see installed skills.`,
          };
        }
        const record = await readSkill(handle, name);
        if (record === null) {
          return {
            ok: false,
            error: `skill content unreadable for ${name}; the stored artifact appears corrupt. Reinstall with install_skill.`,
          };
        }
        return {
          ok: true,
          name: record.name,
          description: record.description,
          skillMd: record.skillMd,
          files: record.files.map((file) => file.path),
        };
      },
    },
  };
}

function readSkillFileTool(): ToolInstance {
  return {
    source: "built_in",
    definition: {
      name: "read_skill_file",
      description:
        "Read one supporting file bundled with an installed skill, by the relative path listed in the use_skill result.",
      parameters: {
        type: "object",
        additionalProperties: false,
        properties: {
          name: {
            type: "string",
            description: "The skill name.",
          },
          path: {
            type: "string",
            description: "Relative file path within the skill.",
          },
        },
        required: ["name", "path"],
      },
    },
    handler: {
      async execute(args: JsonObject, execution): Promise<ToolResult> {
        const name = typeof args.name === "string" ? args.name.trim() : "";
        const path = typeof args.path === "string" ? args.path.trim() : "";
        if (validateSkillName(name) !== null) {
          return { ok: false, error: "unknown skill name" };
        }
        const record = await readSkill(skillsHandle(execution.context), name);
        if (record === null) {
          return { ok: false, error: `skill not installed: ${name}` };
        }
        const file = record.files.find((item) => item.path === path);
        if (file === undefined) {
          return {
            ok: false,
            error: `no file ${path} in skill ${name}`,
            files: record.files.map((item) => item.path),
          };
        }
        return { ok: true, name, path, contents: file.contents };
      },
    },
  };
}

function uninstallSkillTool(): ToolInstance {
  return {
    source: "built_in",
    definition: {
      name: "uninstall_skill",
      description:
        "Remove an installed skill by name. The skill disappears from the installed-skills list; prior artifact versions remain in history.",
      parameters: {
        type: "object",
        additionalProperties: false,
        properties: {
          name: {
            type: "string",
            description: "The skill name to remove.",
          },
        },
        required: ["name"],
      },
    },
    handler: {
      async execute(args: JsonObject, execution): Promise<ToolResult> {
        const name = typeof args.name === "string" ? args.name.trim() : "";
        if (name.length === 0) {
          return { ok: false, error: "name is required" };
        }
        const handle = skillsHandle(execution.context);
        const index = await readIndex(handle);
        const before = index.skills.length;
        index.skills = index.skills.filter((entry) => entry.name !== name);
        const removed = before - index.skills.length;
        if (removed > 0) {
          await writeIndex(handle, index);
        }
        return { ok: removed > 0, name, removed };
      },
    },
  };
}

export function createSkillToolInstances(): ToolInstance[] {
  return [
    installSkillTool(),
    listSkillsTool(),
    useSkillTool(),
    readSkillFileTool(),
    uninstallSkillTool(),
  ];
}

export function registerSkillTools(registry: HarnessToolRegistry): void {
  for (const tool of createSkillToolInstances()) {
    registry.register(tool);
  }
}

// Build the developer message listing installed skills (progressive
// disclosure stage 1). Returns null when no skills are installed.
export async function skillsInstruction(
  context: TurnContext,
): Promise<Message | null> {
  let index: SkillsIndex;
  try {
    index = await readIndex(skillsHandle(context));
  } catch (error) {
    // Prompt assembly runs every model round, so a corrupt index must not
    // brick the agent. Degrade the read; the write path still throws, so
    // nothing overwrites the corrupt artifact while it is broken.
    const detail = error instanceof Error ? error.message : String(error);
    console.error(`skills unavailable during prompt assembly: ${detail}`);
    return {
      role: "developer",
      content:
        "Your installed skills could not be read this turn (the skills index artifact appears corrupt). Do not assume none are installed; if a task seems to match a skill, tell the user skills are temporarily unavailable.",
    };
  }
  if (index.skills.length === 0) {
    return null;
  }
  const lines = index.skills.map(
    (entry) => `- ${entry.name}: ${entry.description}`,
  );
  return {
    role: "developer",
    content: `Installed skills. Before performing a task that matches a skill's description, call use_skill(name) and follow the returned instructions; read bundled files with read_skill_file as needed. Manage skills with install_skill and uninstall_skill.\n\n${lines.join("\n")}`,
  };
}

async function readLatestArtifactJson(
  handle: SkillsHandle,
  path: string,
): Promise<unknown | null> {
  const latest = latestArtifactVersion(await handle.listArtifacts(), path);
  if (latest === null) {
    return null;
  }
  return handle.readArtifactJson({
    artifactId: latest.artifactId,
    version: latest.version,
  });
}

function latestArtifactVersion(
  artifacts: ArtifactVersion[],
  path: string,
): ArtifactVersion | null {
  return (
    artifacts
      .filter((artifact) => artifact.path === path)
      .sort((a, b) => b.version - a.version)[0] ?? null
  );
}

function isSkillsIndex(value: unknown): value is SkillsIndex {
  if (!isRecord(value) || !Array.isArray(value.skills)) {
    return false;
  }
  return value.skills.every(
    (entry) =>
      isRecord(entry) &&
      typeof entry.name === "string" &&
      typeof entry.description === "string" &&
      typeof entry.installedAt === "string" &&
      typeof entry.updatedAt === "string",
  );
}

function isSkillRecord(value: unknown): value is SkillRecord {
  if (
    !isRecord(value) ||
    typeof value.name !== "string" ||
    typeof value.description !== "string" ||
    typeof value.skillMd !== "string" ||
    !Array.isArray(value.files)
  ) {
    return false;
  }
  return value.files.every(
    (file) =>
      isRecord(file) &&
      typeof file.path === "string" &&
      typeof file.contents === "string",
  );
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}
