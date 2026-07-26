import { spawn } from "node:child_process";
import { randomUUID } from "node:crypto";
import {
  closeSync,
  mkdirSync,
  openSync,
  renameSync,
  writeFileSync,
} from "node:fs";
import { dirname, join } from "node:path";

import type {
  HarnessToolRegistry,
  JsonObject,
  ToolInstance,
  ToolResult,
} from "@exo/harness";

const GUARDIAN_SCRIPT = new URL(
  "./scripts/exo-service-guardian",
  import.meta.url,
).pathname;
const ROOT_DIR = new URL("../..", import.meta.url).pathname;
const STATE_DIR = join(ROOT_DIR, ".exo");
const DEFERRED_LOG_PATH = join(STATE_DIR, "exo-service-guardian-actions.log");
const UPDATE_DIR = join(STATE_DIR, "guardian-updates");
const DEFERRED_RESTART_DELAY_SECONDS = 2;

export function registerGuardianTools(registry: HarnessToolRegistry): void {
  registry.register(rebuildAndRestartExoTool());
}

export function rebuildAndRestartExoTool(): ToolInstance {
  return {
    source: "built_in",
    definition: {
      name: "rebuild_and_restart_exo",
      description:
        "Validate and rebuild Exo, then restart its guardian-managed scheduler and adapter services. This narrow operation is asynchronous: it durably records an update, lets the current turn finish, and returns an update id. The existing guardian reboot notice wakes active adapter conversations after a successful restart.",
      parameters: {
        type: "object",
        additionalProperties: false,
        properties: {},
        required: [],
      },
    },
    handler: {
      execute() {
        return Promise.resolve(queueRebuildAndRestart());
      },
    },
  };
}

function queueRebuildAndRestart(): ToolResult {
  const updateId = randomUUID();
  const outcomePath = join(UPDATE_DIR, `${updateId}.json`);
  mkdirSync(UPDATE_DIR, { recursive: true });
  writeJsonAtomically(outcomePath, {
    updateId,
    operation: "rebuild_and_restart_exo",
    status: "queued",
    requestedAt: new Date().toISOString(),
  });
  const result = runGuardianDeferredWithOutcome(
    ["restart-all", "--build"],
    updateId,
    outcomePath,
  );
  return {
    ok: true,
    updateId,
    status: "queued",
    deferred: true,
    pid: result.pid,
    delaySeconds: DEFERRED_RESTART_DELAY_SECONDS,
    outcomePath,
    logPath: result.logPath,
    command: result.command,
  };
}

function runGuardianDeferredWithOutcome(
  args: string[],
  updateId: string,
  outcomePath: string,
): {
  command: string[];
  logPath: string;
  pid: number | null;
} {
  mkdirSync(dirname(DEFERRED_LOG_PATH), { recursive: true });
  const logFd = openSync(DEFERRED_LOG_PATH, "a");
  const command = [GUARDIAN_SCRIPT, ...args];
  const child = spawn(
    "bash",
    [
      "-lc",
      'delay="$1"; record="$2"; update_id="$3"; shift 3; printf "\\n[%s] queued rebuild update %s:" "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$update_id"; for arg in "$@"; do printf " %q" "$arg"; done; printf "\\n"; sleep "$delay"; set +e; "$@"; code=$?; completed_at="$(date -u +%Y-%m-%dT%H:%M:%SZ)"; if [ "$code" -eq 0 ]; then status=succeeded; else status=failed; fi; tmp="${record}.tmp.$$"; printf \'{"updateId":"%s","operation":"rebuild_and_restart_exo","status":"%s","exitCode":%s,"completedAt":"%s"}\\n\' "$update_id" "$status" "$code" "$completed_at" >"$tmp"; mv "$tmp" "$record"; exit "$code"',
      "exo-rebuild-and-restart-deferred",
      String(DEFERRED_RESTART_DELAY_SECONDS),
      outcomePath,
      updateId,
      ...command,
    ],
    {
      cwd: ROOT_DIR,
      detached: true,
      stdio: ["ignore", logFd, logFd],
    },
  );
  child.unref();
  closeSync(logFd);
  return {
    command,
    logPath: DEFERRED_LOG_PATH,
    pid: child.pid ?? null,
  };
}

function writeJsonAtomically(path: string, value: JsonObject): void {
  const temporaryPath = `${path}.tmp.${process.pid}`;
  writeFileSync(temporaryPath, `${JSON.stringify(value)}\n`, "utf8");
  renameSync(temporaryPath, path);
}
