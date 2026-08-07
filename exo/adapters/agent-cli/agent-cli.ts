import os from "node:os";
import path from "node:path";

// Pure helpers for the agent-cli adapter worker. The worker listens on a
// local unix socket; the exo-cli client sends one request per invocation
// with the user's host working directory, and these helpers translate that
// host path into the agent sandbox's workspace mount.

export type AgentCliRequest = {
  cwd: string;
  prompt: string;
};

export function defaultSocketPath(homedir: string = os.homedir()): string {
  return path.join(homedir, ".exo", "agent-cli.sock");
}

export function parseAgentCliRequest(value: unknown): AgentCliRequest {
  if (!value || typeof value !== "object" || Array.isArray(value)) {
    throw new Error("agent-cli request must be a JSON object");
  }
  const record = value as Record<string, unknown>;
  if (typeof record.cwd !== "string" || !record.cwd.startsWith("/")) {
    throw new Error("agent-cli request cwd must be an absolute path");
  }
  if (typeof record.prompt !== "string" || record.prompt.trim().length === 0) {
    throw new Error("agent-cli request prompt must be a non-empty string");
  }
  return { cwd: record.cwd, prompt: record.prompt };
}

/// Maps a host working directory to its path under the sandbox workspace
/// mount, or null when the directory is outside the mounted root.
export function resolveSandboxCwd(
  cwd: string,
  mountRoot: string,
  mountPath: string,
): string | null {
  const root = trimTrailingSlash(mountRoot);
  const mount = trimTrailingSlash(mountPath);
  const directory = trimTrailingSlash(cwd);
  if (directory === root) {
    return mount;
  }
  if (directory.startsWith(`${root}/`)) {
    return `${mount}${directory.slice(root.length)}`;
  }
  return null;
}

export function composeMessageText(
  request: AgentCliRequest,
  mountRoot: string,
  mountPath: string,
): string {
  const sandboxCwd = resolveSandboxCwd(request.cwd, mountRoot, mountPath);
  const location =
    sandboxCwd === null
      ? `The user's working directory is \`${request.cwd}\` on the host, which is OUTSIDE the workspace mount (host \`${mountRoot}\`). You cannot read or write files there; if the request needs file access, reply explaining that the directory must be under \`${mountRoot}\`.`
      : `The user's working directory is mounted in your sandbox at \`${sandboxCwd}\` (host path \`${request.cwd}\`). Operate on the real files there with your shell tool, starting with \`cd ${sandboxCwd}\`.`;
  return `${location}\n\n${request.prompt}`;
}

function trimTrailingSlash(value: string): string {
  return value.length > 1 && value.endsWith("/")
    ? value.replace(/\/+$/, "")
    : value;
}
