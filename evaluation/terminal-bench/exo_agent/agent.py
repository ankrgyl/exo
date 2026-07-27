"""Harbor adapter for running exo on Terminal-Bench 2.0.

NOTE on the word "agent": here it is the *Harbor* concept — a subclass of
harbor's `BaseInstalledAgent` that adapts exo to Harbor's task format (Harbor
loads it via `--agent-import-path`, then calls `install()` / `run()` per task).
It is glue, not an exo agent. The actual coding agent is whatever exo harness
this adapter selects via `exo agent create --harness …` (the base uses exo's
Simple Coding Agent; subclasses select codex / claude-code).

What the adapter does: installs a slim exo bundle (binary + pruned node_modules +
harness sources) into the task container and drives it with `exo conversation
send`, configured with a local-process sandbox so exo's shell == the task
container's shell.

Modeled on harbor.agents.installed.openclaw (the closest in-tree analog: a
local-first, config-driven CLI agent), but invoking exo's CLI.

Run it via:
  harbor run -d terminal-bench@2.0 \
    --agent-import-path exo_agent.agent:ExoAgent \
    -m openai/gpt-5.5 --ae OPENAI_API_KEY=$OPENAI_API_KEY -l 1
(with PYTHONPATH including this dir and EXO_BUNDLE pointing at exo-bundle.tar.gz)
"""

from __future__ import annotations

import os
import shlex
from typing import Any

from harbor.agents.installed.base import BaseInstalledAgent
from harbor.environments.base import BaseEnvironment
from harbor.models.agent.context import AgentContext


class ExoAgent(BaseInstalledAgent):
    """Harbor installed-agent adapter for exo (see module docstring — "agent" is
    Harbor's term for this glue, not an exo agent). Base variant drives exo's
    Simple Coding Agent harness with an OpenAI model."""

    SUPPORTS_ATIF: bool = False

    _BUNDLE_REMOTE = "/tmp/exo-bundle.tar.gz"
    _EXO_HOME = "/opt/exo"
    _EXO_ROOT = "/tmp/exoroot"
    _HARNESS = "/opt/exo/examples/simple-coding-agent/harness.ts"
    _OUTPUT = "/logs/agent/exo.txt"

    # Overridable by subclasses (codex / claude-code variants). Base = exo's own
    # Simple Coding Agent harness driven by an OpenAI model.
    _HARNESS_ARG = _HARNESS         # value for `exo agent create --harness`
    _SECRET_NAME = "openai"         # exo secret name
    _SECRET_ENV = "OPENAI_API_KEY"  # env var the secret reads from
    _KEY_ENV = "OPENAI_API_KEY"     # key injected into the agent's process env
    _EXTRA_ENV: dict[str, str] = {}  # extra env for the exo process (propagates
    #                                  to the wrapped CLI via the local-process bridge)

    # Ensure node >= 20 AND npm before a global npm install. Base images vary:
    # some ship no node, some an old node, some node-but-no-npm — any of which
    # silently lost those tasks. Install node 22 (which bundles npm) if either is
    # missing/insufficient. Mirrors how Harbor's native codex agent provisions
    # node 22 before installing.
    _ENSURE_NODE_NPM = (
        'have=$(node -v 2>/dev/null | sed "s/v//;s/\\..*//" || echo 0); '
        'if ! command -v npm >/dev/null 2>&1 || [ "${have:-0}" -lt 20 ]; then '
        "( command -v apk >/dev/null 2>&1 && apk add --no-cache nodejs npm ) || "
        "( ( command -v curl >/dev/null 2>&1 || "
        "( apt-get update && apt-get install -y --no-install-recommends curl ca-certificates ) ); "
        "curl -fsSL https://deb.nodesource.com/setup_22.x | bash - && apt-get install -y nodejs ); "
        "fi; "
    )

    def __init__(self, *args: Any, exo_bundle: str | None = None, **kwargs: Any) -> None:
        super().__init__(*args, **kwargs)
        _default_bundle = os.path.join(
            os.path.dirname(os.path.dirname(os.path.abspath(__file__))),
            "exo-bundle.tar.gz",
        )
        self._exo_bundle = exo_bundle or os.environ.get("EXO_BUNDLE", _default_bundle)

    @staticmethod
    def name() -> str:
        return "exo"

    def version(self) -> str | None:
        return self._version or "dev"

    @property
    def _model(self) -> str:
        # Harbor passes e.g. "openai/gpt-5.5"; exo registers the bare model name.
        # No default: a silent fallback would run (and score) the wrong model.
        model = self._parsed_model_name or self.model_name
        if not model:
            raise ValueError(
                "No model specified — pass `-m <provider>/<model>` to harbor run "
                "(e.g. -m openai/gpt-5.5). Refusing to default to avoid scoring "
                "the wrong model."
            )
        return model

    def _exo(self) -> str:
        return f"exo --root {self._EXO_ROOT} --secret-backend file"

    async def install(self, environment: BaseEnvironment) -> None:
        # Ship + unpack the slim exo bundle; expose `exo` on PATH (world-readable).
        # The bundle is self-contained (static-musl exo + vendored node + deps), so
        # this needs NO internet — required for sandboxes with allow_internet=false.
        # tar is present on essentially all task base images.
        await environment.upload_file(self._exo_bundle, self._BUNDLE_REMOTE)
        await self.exec_as_root(
            environment,
            f"mkdir -p {self._EXO_HOME} && "
            f"tar xzf {self._BUNDLE_REMOTE} -C {self._EXO_HOME} && "
            f"chmod -R a+rX {self._EXO_HOME} && "
            f"ln -sf {self._EXO_HOME}/target/x86_64-unknown-linux-musl/release/exo "
            "/usr/local/bin/exo",
        )
        # Node: prefer one already in the image; else use the vendored glibc node;
        # else (old-glibc image with no node + internet) fall back to NodeSource.
        # Uses ( ) subshells, not { } groups, to avoid f-string brace collisions.
        await self.exec_as_root(
            environment,
            "command -v node >/dev/null 2>&1 || "
            f"( {self._EXO_HOME}/node --version >/dev/null 2>&1 && "
            f"ln -sf {self._EXO_HOME}/node /usr/local/bin/node ) || "
            "( ( command -v curl >/dev/null 2>&1 || ( apt-get update && "
            "apt-get install -y --no-install-recommends curl ca-certificates ) ); "
            "curl -fsSL https://deb.nodesource.com/setup_22.x | bash - && "
            "apt-get install -y nodejs )",
            env={"DEBIAN_FRONTEND": "noninteractive"},
        )
        # Variant agents (codex / claude-code) install their wrapped CLI here.
        await self._install_agent_cli(environment)

    async def _install_agent_cli(self, environment: BaseEnvironment) -> None:
        """No-op for the base agent; codex/claude-code variants override to
        install their CLI (codex / claude) into the task container."""
        return None

    def _pre_send_cmd(self) -> str | None:
        """Optional shell command run (as the agent user, from the bundle dir)
        right before the task is sent — for setup the wrapped CLI reads at
        launch. None for the base agent."""
        return None

    async def run(
        self,
        instruction: str,
        environment: BaseEnvironment,
        context: AgentContext,
    ) -> Any:
        key = self._get_env(self._KEY_ENV) or ""
        env = {self._KEY_ENV: key, **self._EXTRA_ENV}
        exo = self._exo()
        # Run exo from the bundle dir so tsx resolves @exo/harness via tsconfig.json.
        cd = f"cd {self._EXO_HOME} && "

        # Discover the task working directory so the agent's shell operates there.
        pwd_res = await self.exec_as_agent(environment, "pwd")
        taskdir = (getattr(pwd_res, "stdout", None) or "/").strip() or "/"

        # One-time exo setup: model + agent (harness + local-process sandbox).
        await self.exec_as_agent(
            environment,
            f"{cd}{exo} secret set {self._SECRET_NAME} --env {self._SECRET_ENV}",
            env=env,
        )
        await self.exec_as_agent(
            environment,
            f"{cd}{exo} model register {shlex.quote(self._model)} --secret {self._SECRET_NAME}",
            env=env,
        )
        await self.exec_as_agent(
            environment,
            f"{cd}{exo} agent create --slug t --model {shlex.quote(self._model)} "
            f"--harness {self._HARNESS_ARG} --sandbox-provider local-process Exo",
            env=env,
        )
        await self.exec_as_agent(environment, f"{cd}{exo} conversation create t c", env=env)
        # Point exo's shell at the task dir (local-process => host path == container path).
        await self.exec_as_agent(
            environment,
            f"{cd}{exo} conversation mount add t c {shlex.quote(taskdir)} {shlex.quote(taskdir)}",
            env=env,
        )

        # Per-variant setup that must exist before the wrapped CLI launches
        # (e.g. codex reads $CODEX_HOME/config.toml at app-server start). Run as
        # the agent user so files are writable by the harness's own setup.
        pre = self._pre_send_cmd()
        if pre:
            await self.exec_as_agent(environment, f"{cd}{pre}", env=env)

        # Drive the task; capture transcript for Harbor. We also append any
        # wrapped-CLI stderr (codex app-server / login) to the log — those
        # processes run inside the sandbox and their crashes otherwise surface
        # only as an opaque "broken pipe" from exo.
        await self.exec_as_agent(
            environment,
            # `--` so an instruction beginning with '-' isn't parsed as a CLI flag.
            f"mkdir -p /logs/agent; {cd}{exo} conversation send t c -- "
            f"{shlex.quote(instruction)} > {self._OUTPUT} 2>&1; rc=$?; "
            "for s in /tmp/codex-app-server.stderr /tmp/codex-login.stderr "
            "/tmp/codex-setup.stderr; do "
            'if [ -f "$s" ]; then echo "===== $s ====="; cat "$s"; fi; '
            f"done >> {self._OUTPUT} 2>&1; "
            # Claude Code's stderr is routed to exo events, not a file — surface it.
            f"{self._CLAUDE_STDERR_SCRIPT} >> {self._OUTPUT} 2>&1; "
            f"cat {self._OUTPUT}; exit $rc",
            env=env,
        )

        # Harvest exo's recorded token usage + cost from its event store BEFORE
        # the container is torn down, and write it to the agent logs so the
        # report can show tokens/$ per task. Best-effort: never fail the task.
        try:
            await self.exec_as_agent(environment, self._USAGE_SCRIPT, env=env)
        except Exception as e:  # noqa: BLE001
            self.logger.debug("exo usage harvest skipped: %s", e)

    # Dump any claude_process_stderr events exo recorded (Claude Code's startup
    # stderr) so a "process exited with code 1" has a visible cause.
    _CLAUDE_STDERR_SCRIPT = (
        "node -e '"
        "const fs=require(\"fs\");"
        "function w(d){let o=[];let es;try{es=fs.readdirSync(d,{withFileTypes:true})}catch(e){return o}"
        "for(const e of es){const p=d+\"/\"+e.name;if(e.isDirectory())o=o.concat(w(p));"
        "else if(e.name.endsWith(\".json\"))o.push(p)}return o}"
        "for(const f of w(\"/tmp/exoroot\").filter(f=>f.includes(\"/events/\"))){"
        "let t;try{t=fs.readFileSync(f,\"utf8\")}catch(e){continue}"
        "if(t.includes(\"claude_process_stderr\")){"
        "const m=t.match(/\"text\":\"((?:[^\"\\\\]|\\\\.)*)\"/g)||[];"
        "for(const x of m)console.log(\"===== claude stderr =====\\n\"+x)}}"
        "' 2>/dev/null || true"
    )

    # node is installed in install(); exo records a `usage` block per turn in its
    # event JSONs — including its own cost_usd. We trust exo's cost_usd (it prices
    # turns itself; no hardcoded price table to rot) and only sum the token fields
    # for the per-task breakdown (prompt/completion/cached/reasoning) that a single
    # cost figure can't give.
    _USAGE_SCRIPT = r"""
node -e '
const fs=require("fs");
function walk(d){let o=[];let es;try{es=fs.readdirSync(d,{withFileTypes:true});}catch(e){return o;}for(const e of es){const p=d+"/"+e.name;if(e.isDirectory())o=o.concat(walk(p));else if(e.name.endsWith(".json"))o.push(p);}return o;}
let p=0,c=0,ca=0,r=0,cost=0;
const files=walk("/tmp/exoroot").filter(f=>f.includes("/events/"));
const sum=(t,re)=>{let s=0,m;const rx=new RegExp(re,"g");while(m=rx.exec(t))s+=+m[1];return s;};
for(const f of files){let t;try{t=fs.readFileSync(f,"utf8");}catch(e){continue;}
  p+=sum(t,"\"prompt_tokens\":\\s*(\\d+)");c+=sum(t,"\"completion_tokens\":\\s*(\\d+)");
  ca+=sum(t,"\"prompt_cached_tokens\":\\s*(\\d+)");r+=sum(t,"\"completion_reasoning_tokens\":\\s*(\\d+)");
  cost+=sum(t,"\"cost_usd\":\\s*([0-9.]+)");}
const out={event_files:files.length,prompt_tokens:p,completion_tokens:c,cached_tokens:ca,
  reasoning_tokens:r,cost_usd:Math.round(cost*1e6)/1e6,cost_source:"exo usage.cost_usd"};
fs.mkdirSync("/logs/agent",{recursive:true});
fs.writeFileSync("/logs/agent/exo_usage.json",JSON.stringify(out,null,2));
console.log("exo_usage",JSON.stringify(out));
'
""".strip()


class ExoCodexAgent(ExoAgent):
    """exo driving the Codex CLI via `--harness codex` — same OpenAI model + tasks
    as ExoAgent, but the actual coding agent is Codex. Tests "Codex over Exo"."""

    _HARNESS_ARG = "codex"
    # Match the version Harbor's native codex agent installs, so native vs
    # over-exo is the SAME agent. Note: make sure to update if version changes
    # for apples-to-apples comparison.
    _CODEX_VERSION = "0.142.3"

    @staticmethod
    def name() -> str:
        return "exo-codex"

    def _pre_send_cmd(self) -> str | None:
        # Enable codex's built-in web_search tool. exo's codex harness creates
        # threads via app-server with only its own dynamicTools (the exoharness
        # shell), so codex's web_search is OFF — unlike native codex, which has
        # it on by default. codex reads $CODEX_HOME/config.toml at app-server
        # start (CODEX_HOME defaults to /tmp/exo-codex-home in the harness).
        return (
            "mkdir -p /tmp/exo-codex-home && "
            "printf '[tools]\\nweb_search = true\\n' "
            "> /tmp/exo-codex-home/config.toml"
        )

    async def _install_agent_cli(self, environment: BaseEnvironment) -> None:
        # exo's codex harness spawns `codex app-server`, so the codex CLI must be
        # on PATH in the task container. Ensure a new-enough node, install codex
        # (pinned to _CODEX_VERSION to match native), and expose it on PATH.
        await self.exec_as_root(
            environment,
            self._ENSURE_NODE_NPM +
            f"npm install -g @openai/codex@{self._CODEX_VERSION}; "
            # Resolve the REAL codex binary and symlink it onto a stable PATH
            # location — but never onto itself (npm's global bin is often
            # /usr/local/bin, which would make a self-referential dangling link
            # and break `codex` => "command not found" in the sandbox).
            'cx="$(command -v codex || echo "$(npm prefix -g)/bin/codex")"; '
            '[ "$cx" != /usr/local/bin/codex ] && ln -sf "$cx" /usr/local/bin/codex; '
            "command -v codex",
            env={"DEBIAN_FRONTEND": "noninteractive"},
        )


class ExoClaudeCodeAgent(ExoAgent):
    """exo driving the Claude Code CLI via `--harness claude-code` — same Sonnet
    model + tasks, but the coding agent is Claude Code. Tests "Claude Code over Exo"."""

    _HARNESS_ARG = "claude-code"
    _SECRET_NAME = "anthropic"
    _SECRET_ENV = "ANTHROPIC_API_KEY"
    _KEY_ENV = "ANTHROPIC_API_KEY"
    # Task containers run as root; claude-code refuses --dangerously-skip-permissions
    # (which the harness's bypassPermissions maps to) as root unless IS_SANDBOX=1.
    # exo inherits this and the local-process bridge propagates it to claude-code.
    _EXTRA_ENV = {"IS_SANDBOX": "1"}

    @staticmethod
    def name() -> str:
        return "exo-claude-code"

    async def _install_agent_cli(self, environment: BaseEnvironment) -> None:
        # exo's claude-code harness spawns the Claude Code CLI and expects it at
        # /usr/local/bin/claude-code (DEFAULT_CLAUDE_CODE_SANDBOX_EXECUTABLE). The
        # npm package installs a `claude` binary, so symlink it to that path.
        # procps (ps) is needed by claude-code at runtime; ensure node >= 20 first.
        await self.exec_as_root(
            environment,
            "( command -v apk >/dev/null 2>&1 && apk add --no-cache procps ) || "
            "( apt-get update && apt-get install -y --no-install-recommends procps ) || true; "
            + self._ENSURE_NODE_NPM +
            "npm install -g @anthropic-ai/claude-code; "
            # The harness spawns /usr/local/bin/claude-code; the npm package
            # ships a `claude` binary. Resolve the real path and link it there.
            # (The @anthropic-ai/claude-agent-sdk the harness imports now ships
            # inside the exo bundle, so no in-container SDK install is needed.)
            'cl="$(command -v claude || echo "$(npm prefix -g)/bin/claude")"; '
            'ln -sf "$cl" /usr/local/bin/claude-code; '
            "command -v claude-code",
            env={"DEBIAN_FRONTEND": "noninteractive"},
        )
