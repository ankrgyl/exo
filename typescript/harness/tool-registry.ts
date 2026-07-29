import { execFile } from "node:child_process";
import { randomUUID } from "node:crypto";
import fs from "node:fs/promises";
import path from "node:path";
import { promisify } from "node:util";

import type { JsonObject, JsonValue } from "./index";

const execFileAsync = promisify(execFile);

export const DEFAULT_TOOL_REGISTRY_DIRECTORY = ".exo/tools";
export const TOOL_MANIFEST_FILE = "exo-tool.json";
const LOCK_SCHEMA_VERSION = 1;
const MANIFEST_SCHEMA_VERSION = 1;
// Advisory lock for lockfile read-modify-write sections. Locks older than
// this are treated as leftovers from a crashed holder and stolen.
const REGISTRY_LOCK_STALE_MS = 60_000;
const REGISTRY_LOCK_ACQUIRE_TIMEOUT_MS = 10_000;
const REGISTRY_LOCK_RETRY_DELAY_MS = 100;

export type ToolSource =
  | { type: "local"; path: string; subdirectory?: string }
  | {
      type: "git";
      repository: string;
      commit: string;
      subdirectory?: string;
    };

export interface ToolManifest {
  schemaVersion: 1;
  id: string;
  module: string;
}

export interface InstalledTool {
  id: string;
  source: ToolSource;
  initialization: JsonObject;
  installPath: string;
}

interface ToolLockfile {
  schemaVersion: 1;
  tools: InstalledTool[];
}

export interface InstallToolRequest {
  source: ToolSource;
  initialization: JsonObject;
}

export interface InstallValidation {
  toolName: string;
}

export interface ToolRegistrySnapshot {
  installed: InstalledTool[];
}

export async function readToolRegistry(
  registryDirectory = DEFAULT_TOOL_REGISTRY_DIRECTORY,
): Promise<ToolRegistrySnapshot> {
  const lockPath = toolLockfilePath(registryDirectory);
  let text: string;
  try {
    text = await fs.readFile(lockPath, "utf8");
  } catch (error) {
    if (isNotFoundError(error)) {
      return { installed: [] };
    }
    throw error;
  }

  try {
    const lockfile = parseLockfile(JSON.parse(text) as unknown);
    for (const installed of lockfile.tools) {
      assertManagedInstallPath(registryDirectory, installed.installPath);
    }
    return { installed: lockfile.tools };
  } catch (error) {
    throw new Error(
      `invalid tool lockfile ${lockPath}: ${errorMessage(error)}`,
    );
  }
}

export async function installToolSource(
  request: InstallToolRequest,
  validate: (
    modulePath: string,
    initialization: JsonObject,
  ) => Promise<InstallValidation>,
  registryDirectory = DEFAULT_TOOL_REGISTRY_DIRECTORY,
): Promise<InstalledTool> {
  const registryRoot = path.resolve(registryDirectory);
  const stagingRoot = path.join(registryRoot, ".staging");
  await fs.mkdir(stagingRoot, { recursive: true });
  const stagingPath = await fs.mkdtemp(path.join(stagingRoot, "candidate-"));
  let installPath: string | null = null;

  try {
    const sourceRoot = await materializeSource(request.source, stagingPath);
    const manifest = await readToolManifest(sourceRoot);
    const modulePath = resolveManifestModule(sourceRoot, manifest.module);
    await assertContainedFile(sourceRoot, modulePath);
    const validation = await validate(modulePath, request.initialization);
    // Everything from the snapshot read to the lockfile write is a
    // read-modify-write of the whole lockfile; hold the registry lock so a
    // concurrent install/remove cannot be silently reverted.
    return await withToolRegistryLock(registryDirectory, async () => {
      const snapshot = await readToolRegistry(registryDirectory);
      const existing =
        snapshot.installed.find((item) => item.id === manifest.id) ?? null;
      for (const item of snapshot.installed) {
        if (item.id === manifest.id) {
          continue;
        }
        let existingValidation: InstallValidation;
        try {
          existingValidation = await validate(
            await installedToolModulePath(item),
            item.initialization,
          );
        } catch (error) {
          console.error(
            `skipping broken installed tool ${item.id}: ${errorMessage(error)}`,
          );
          continue;
        }
        if (existingValidation.toolName === validation.toolName) {
          throw new Error(
            `tool name ${validation.toolName} is already installed by ${item.id}`,
          );
        }
      }

      installPath = path.join(
        registryRoot,
        "store",
        safePathSegment(manifest.id),
        randomUUID(),
      );
      await fs.mkdir(path.dirname(installPath), { recursive: true });
      await fs.rename(sourceRoot, installPath);
      await fs.rm(stagingPath, { recursive: true, force: true });

      const installed: InstalledTool = {
        id: manifest.id,
        source: request.source,
        initialization: request.initialization,
        installPath,
      };
      const nextTools = snapshot.installed.filter(
        (item) => item.id !== manifest.id,
      );
      nextTools.push(installed);
      try {
        await writeToolLockfile(
          { schemaVersion: LOCK_SCHEMA_VERSION, tools: nextTools },
          registryDirectory,
        );
      } catch (error) {
        await fs.rm(installPath, { recursive: true, force: true });
        throw error;
      }
      if (existing) {
        try {
          await fs.rm(existing.installPath, { recursive: true, force: true });
        } catch (error) {
          console.error(
            `unable to delete replaced tool store ${existing.installPath}: ${errorMessage(error)}`,
          );
        }
      }
      return installed;
    });
  } catch (error) {
    await fs.rm(stagingPath, { recursive: true, force: true });
    if (installPath !== null) {
      await fs.rm(installPath, { recursive: true, force: true });
    }
    throw error;
  }
}

export async function removeInstalledTool(
  id: string,
  registryDirectory = DEFAULT_TOOL_REGISTRY_DIRECTORY,
): Promise<InstalledTool | null> {
  return withToolRegistryLock(registryDirectory, async () => {
    const snapshot = await readToolRegistry(registryDirectory);
    const installed = snapshot.installed.find((item) => item.id === id) ?? null;
    if (!installed) {
      return null;
    }
    await writeToolLockfile(
      {
        schemaVersion: LOCK_SCHEMA_VERSION,
        tools: snapshot.installed.filter((item) => item.id !== id),
      },
      registryDirectory,
    );
    await fs.rm(installed.installPath, { recursive: true, force: true });
    return installed;
  });
}

export async function readInstalledToolManifest(
  installed: InstalledTool,
): Promise<ToolManifest> {
  const manifest = await readToolManifest(installed.installPath);
  if (manifest.id !== installed.id) {
    throw new Error(
      `installed tool id ${installed.id} does not match manifest id ${manifest.id}`,
    );
  }
  return manifest;
}

export async function installedToolModulePath(
  installed: InstalledTool,
): Promise<string> {
  const manifest = await readInstalledToolManifest(installed);
  const modulePath = resolveManifestModule(
    installed.installPath,
    manifest.module,
  );
  await assertContainedFile(installed.installPath, modulePath);
  return modulePath;
}

export function parseToolSource(value: JsonValue): ToolSource {
  const source = strictObject(value, "source");
  const type = requiredString(source, "type", "source");
  if (type === "local") {
    rejectUnknownKeys(source, ["type", "path", "subdirectory"], "local source");
    const sourcePath = normalizeWorkspaceRelativePath(
      requiredString(source, "path", "source"),
    );
    const subdirectory = optionalString(source, "subdirectory", "source");
    if (subdirectory !== undefined) {
      validateRelativePath(subdirectory, "local source subdirectory");
    }
    return {
      type,
      path: sourcePath,
      ...(subdirectory === undefined ? {} : { subdirectory }),
    };
  }
  if (type === "git") {
    rejectUnknownKeys(
      source,
      ["type", "repository", "commit", "subdirectory"],
      "git source",
    );
    const commit = requiredString(source, "commit", "source");
    if (!/^[0-9a-fA-F]{40,64}$/.test(commit)) {
      throw new Error(
        "git source commit must be a pinned 40-64 digit hex hash",
      );
    }
    const subdirectory = optionalString(source, "subdirectory", "source");
    if (subdirectory !== undefined) {
      validateRelativePath(subdirectory, "git source subdirectory");
    }
    return {
      type,
      repository: requiredString(source, "repository", "source"),
      commit: commit.toLowerCase(),
      ...(subdirectory === undefined ? {} : { subdirectory }),
    };
  }
  throw new Error("source.type must be local or git");
}

export function parseInitialization(value: JsonValue | undefined): JsonObject {
  if (value === undefined) {
    return {};
  }
  return strictObject(value, "initialization");
}

async function materializeSource(
  source: ToolSource,
  stagingPath: string,
): Promise<string> {
  if (source.type === "local") {
    const workspaceRoot = await fs.realpath(process.cwd());
    const localPath = path.resolve(workspaceRoot, source.path);
    let realLocalPath: string;
    try {
      realLocalPath = await fs.realpath(localPath);
    } catch (error) {
      if (isNotFoundError(error)) {
        throw new Error(
          `workspace-local tool source does not exist: ${source.path}. ` +
            "Write it under the mounted Exo workspace (for example " +
            "/workspace/exo/.exo/tool-sources/my-tool) and pass the relative " +
            "path .exo/tool-sources/my-tool.",
        );
      }
      throw error;
    }
    if (
      realLocalPath === workspaceRoot ||
      !realLocalPath.startsWith(`${workspaceRoot}${path.sep}`)
    ) {
      throw new Error(
        `local tool source must resolve inside the workspace: ${source.path}`,
      );
    }
    if (!(await fs.stat(realLocalPath)).isDirectory()) {
      throw new Error(`local tool source is not a directory: ${source.path}`);
    }
    const copiedPath = path.join(stagingPath, "source");
    await fs.cp(realLocalPath, copiedPath, { recursive: true });
    return source.subdirectory
      ? resolveContainedPath(copiedPath, source.subdirectory)
      : copiedPath;
  }

  const checkoutPath = path.join(stagingPath, "repository");
  await execGit([
    "clone",
    "--no-checkout",
    "--",
    source.repository,
    checkoutPath,
  ]);
  await execGit(["-C", checkoutPath, "checkout", "--detach", source.commit]);
  const { stdout } = await execGit(["-C", checkoutPath, "rev-parse", "HEAD"]);
  if (stdout.trim().toLowerCase() !== source.commit.toLowerCase()) {
    throw new Error("git checkout did not resolve to the pinned commit");
  }
  return source.subdirectory
    ? resolveContainedPath(checkoutPath, source.subdirectory)
    : checkoutPath;
}

async function execGit(args: string[]): Promise<{ stdout: string }> {
  try {
    const result = await execFileAsync("git", args, {
      encoding: "utf8",
      maxBuffer: 4 * 1024 * 1024,
    });
    return { stdout: result.stdout };
  } catch (error) {
    throw new Error(`git ${args[0]} failed: ${errorMessage(error)}`);
  }
}

async function readToolManifest(sourceRoot: string): Promise<ToolManifest> {
  const manifestPath = path.join(sourceRoot, TOOL_MANIFEST_FILE);
  let value: unknown;
  try {
    value = JSON.parse(await fs.readFile(manifestPath, "utf8")) as unknown;
  } catch (error) {
    throw new Error(
      `unable to read ${TOOL_MANIFEST_FILE}: ${errorMessage(error)}`,
    );
  }
  return parseManifest(value);
}

function parseManifest(value: unknown): ToolManifest {
  const manifest = strictUnknownObject(value, "tool manifest");
  rejectUnknownKeys(
    manifest,
    ["schemaVersion", "id", "module"],
    "tool manifest",
  );
  if (manifest.schemaVersion !== MANIFEST_SCHEMA_VERSION) {
    throw new Error(
      `tool manifest schemaVersion must be ${MANIFEST_SCHEMA_VERSION}`,
    );
  }
  const id = requiredUnknownString(manifest, "id", "tool manifest");
  if (!/^tool:[A-Za-z0-9][A-Za-z0-9._/-]{0,127}$/.test(id)) {
    throw new Error("tool manifest id must be a stable tool: namespace id");
  }
  const module = requiredUnknownString(manifest, "module", "tool manifest");
  validateRelativePath(module, "tool manifest module");
  return {
    schemaVersion: 1,
    id,
    module,
  };
}

function parseLockfile(value: unknown): ToolLockfile {
  const lockfile = strictUnknownObject(value, "tool lockfile");
  rejectUnknownKeys(lockfile, ["schemaVersion", "tools"], "tool lockfile");
  if (lockfile.schemaVersion !== LOCK_SCHEMA_VERSION) {
    throw new Error(
      `tool lockfile schemaVersion must be ${LOCK_SCHEMA_VERSION}`,
    );
  }
  if (!Array.isArray(lockfile.tools)) {
    throw new Error("tool lockfile.tools must be an array");
  }
  return {
    schemaVersion: 1,
    tools: lockfile.tools.map((item, index) =>
      parseInstalledTool(item, `tool lockfile.tools[${index}]`),
    ),
  };
}

function parseInstalledTool(value: unknown, label: string): InstalledTool {
  const item = strictUnknownObject(value, label);
  rejectUnknownKeys(
    item,
    ["id", "source", "initialization", "installPath"],
    label,
  );
  const id = requiredUnknownString(item, "id", label);
  if (!/^tool:[A-Za-z0-9][A-Za-z0-9._/-]{0,127}$/.test(id)) {
    throw new Error(`${label}.id must be a stable tool: namespace id`);
  }
  return {
    id,
    source: parseToolSource(item.source as JsonValue),
    initialization: strictUnknownJsonObject(
      item.initialization,
      `${label}.initialization`,
    ),
    installPath: requiredUnknownString(item, "installPath", label),
  };
}

/// Cross-process advisory lock serializing lockfile read-modify-write
/// sections. Acquired by atomically creating `registry.lock` (O_EXCL); locks
/// older than the stale threshold are stolen so a crashed holder cannot wedge
/// the registry forever.
async function withToolRegistryLock<T>(
  registryDirectory: string,
  action: () => Promise<T>,
): Promise<T> {
  const lockPath = path.join(path.resolve(registryDirectory), "registry.lock");
  await fs.mkdir(path.dirname(lockPath), { recursive: true });
  const deadline = Date.now() + REGISTRY_LOCK_ACQUIRE_TIMEOUT_MS;
  for (;;) {
    try {
      const handle = await fs.open(lockPath, "wx");
      try {
        await handle.writeFile(`${process.pid}\n`, "utf8");
      } finally {
        await handle.close();
      }
      break;
    } catch (error) {
      if (!isAlreadyExistsError(error)) {
        throw error;
      }
      try {
        const stat = await fs.stat(lockPath);
        if (Date.now() - stat.mtimeMs > REGISTRY_LOCK_STALE_MS) {
          await fs.rm(lockPath, { force: true });
          continue;
        }
      } catch (statError) {
        if (!isNotFoundError(statError)) {
          throw statError;
        }
        // Holder released between our open and stat; try again immediately.
        continue;
      }
      if (Date.now() >= deadline) {
        throw new Error(
          `timed out waiting for tool registry lock ${lockPath}; ` +
            "another install or removal may be in progress",
        );
      }
      await sleep(REGISTRY_LOCK_RETRY_DELAY_MS);
    }
  }
  try {
    return await action();
  } finally {
    await fs.rm(lockPath, { force: true });
  }
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

function isAlreadyExistsError(error: unknown): boolean {
  return (
    error !== null &&
    typeof error === "object" &&
    "code" in error &&
    (error as { code?: unknown }).code === "EEXIST"
  );
}

async function writeToolLockfile(
  lockfile: ToolLockfile,
  registryDirectory: string,
): Promise<void> {
  await fs.mkdir(registryDirectory, { recursive: true });
  const lockPath = toolLockfilePath(registryDirectory);
  const tempPath = `${lockPath}.${process.pid}.${Date.now()}.tmp`;
  await fs.writeFile(
    tempPath,
    `${JSON.stringify(lockfile, null, 2)}\n`,
    "utf8",
  );
  await fs.rename(tempPath, lockPath);
}

function toolLockfilePath(registryDirectory: string): string {
  return path.join(registryDirectory, "tools.lock.json");
}

function assertManagedInstallPath(
  registryDirectory: string,
  installPath: string,
): void {
  const storeRoot = path.resolve(registryDirectory, "store");
  const resolvedInstallPath = path.resolve(installPath);
  if (!resolvedInstallPath.startsWith(`${storeRoot}${path.sep}`)) {
    throw new Error(
      "tool lockfile installPath must be inside the managed store",
    );
  }
}

function resolveManifestModule(sourceRoot: string, module: string): string {
  return resolveContainedPath(sourceRoot, module);
}

async function assertContainedFile(
  root: string,
  candidate: string,
): Promise<void> {
  const [realRoot, realCandidate] = await Promise.all([
    fs.realpath(root),
    fs.realpath(candidate),
  ]);
  if (
    realCandidate === realRoot ||
    !realCandidate.startsWith(`${realRoot}${path.sep}`)
  ) {
    throw new Error("tool manifest module escapes its source directory");
  }
  if (!(await fs.stat(realCandidate)).isFile()) {
    throw new Error("tool manifest module must resolve to a file");
  }
}

function resolveContainedPath(root: string, relativePath: string): string {
  const resolvedRoot = path.resolve(root);
  const resolved = path.resolve(resolvedRoot, relativePath);
  if (
    resolved !== resolvedRoot &&
    !resolved.startsWith(`${resolvedRoot}${path.sep}`)
  ) {
    throw new Error(`path escapes tool source: ${relativePath}`);
  }
  return resolved;
}

function validateRelativePath(value: string, label: string): void {
  if (
    value.length === 0 ||
    path.isAbsolute(value) ||
    value.split(/[\\/]/u).some((part) => part === ".." || part.length === 0)
  ) {
    throw new Error(`${label} must be a contained relative path`);
  }
}

function normalizeWorkspaceRelativePath(value: string): string {
  if (path.isAbsolute(value)) {
    throw new Error(
      "local source path must be workspace-relative, not an absolute host or " +
        "sandbox path. Write the tool under /workspace/exo/.exo/tool-sources/ " +
        "and pass .exo/tool-sources/<name>.",
    );
  }
  const parts = value.split(/[\\/]/u);
  if (
    parts.some((part) => part.length === 0 || part === "." || part === "..")
  ) {
    throw new Error(
      "local source path must be a contained workspace-relative path without " +
        "'.' or '..' segments",
    );
  }
  return parts.join("/");
}

function strictObject(value: JsonValue, label: string): JsonObject {
  if (value === null || typeof value !== "object" || Array.isArray(value)) {
    throw new Error(`${label} must be an object`);
  }
  return value;
}

function strictUnknownJsonObject(value: unknown, label: string): JsonObject {
  if (!isJsonObject(value)) {
    throw new Error(`${label} must be a JSON object`);
  }
  return value;
}

function strictUnknownObject(
  value: unknown,
  label: string,
): Record<string, unknown> {
  if (value === null || typeof value !== "object" || Array.isArray(value)) {
    throw new Error(`${label} must be an object`);
  }
  return value as Record<string, unknown>;
}

function rejectUnknownKeys(
  value: Readonly<Record<string, unknown>>,
  allowed: string[],
  label: string,
): void {
  const unknownKey = Object.keys(value).find((key) => !allowed.includes(key));
  if (unknownKey) {
    throw new Error(`${label}.${unknownKey} is not allowed`);
  }
}

function requiredString(value: JsonObject, key: string, label: string): string {
  const field = value[key];
  if (typeof field !== "string" || field.length === 0) {
    throw new Error(`${label}.${key} must be a non-empty string`);
  }
  return field;
}

function optionalString(
  value: JsonObject,
  key: string,
  label: string,
): string | undefined {
  const field = value[key];
  if (field === undefined || field === null) {
    return undefined;
  }
  if (typeof field !== "string" || field.length === 0) {
    throw new Error(`${label}.${key} must be a non-empty string`);
  }
  return field;
}

function requiredUnknownString(
  value: Readonly<Record<string, unknown>>,
  key: string,
  label: string,
): string {
  const field = value[key];
  if (typeof field !== "string" || field.length === 0) {
    throw new Error(`${label}.${key} must be a non-empty string`);
  }
  return field;
}

function isJsonObject(value: unknown): value is JsonObject {
  if (value === null || typeof value !== "object" || Array.isArray(value)) {
    return false;
  }
  return Object.values(value).every(isJsonValue);
}

function isJsonValue(value: unknown): value is JsonValue {
  if (
    value === null ||
    typeof value === "string" ||
    typeof value === "number" ||
    typeof value === "boolean"
  ) {
    return true;
  }
  if (Array.isArray(value)) {
    return value.every(isJsonValue);
  }
  return isJsonObject(value);
}

function safePathSegment(value: string): string {
  return value.replace(/[^A-Za-z0-9_.-]+/gu, "_").slice(0, 128);
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

function isNotFoundError(error: unknown): boolean {
  return (
    error !== null &&
    typeof error === "object" &&
    "code" in error &&
    (error as { code?: unknown }).code === "ENOENT"
  );
}
