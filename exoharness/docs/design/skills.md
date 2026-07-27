# Skills

Skills give an agent installable, durable capability packages: reusable
instructions (and supporting files) that the agent can discover cheaply every
turn and load fully only when a task calls for one.

## The standard we follow

OpenClaw, Hermes Agent, Claude Code, and the
[agentskills.io](https://agentskills.io) specification all converge on the same
pattern, and we adopt it verbatim at the format level:

- A skill is a directory whose entrypoint is `SKILL.md`: YAML frontmatter plus
  a markdown body of instructions, optionally bundling supporting files
  (`scripts/`, `references/`, `assets/`).
- Two required frontmatter fields: `name` (1–64 chars, lowercase alphanumeric
  plus single hyphens) and `description` (1–1024 chars — what the skill does
  _and when to use it_; this doubles as the routing signal). Other fields
  (`license`, `compatibility`, `metadata`) are accepted and preserved but not
  interpreted.
- **Progressive disclosure**, three stages:
  1. Only `name` + `description` of every installed skill is injected into the
     prompt each turn (~tens of tokens per skill).
  2. The `SKILL.md` body is loaded on demand when the model decides a skill
     applies (`use_skill`).
  3. Supporting files are read individually, only as needed
     (`read_skill_file`).

Because the on-disk format is the ecosystem standard, skills published for
Claude Code / OpenClaw / Hermes (e.g. `anthropics/skills`, `openai/skills`)
install here unchanged: read the `SKILL.md` and files, pass them to
`install_skill`.

## Storage: artifact-backed

Skills are stored as **agent artifacts**, not sandbox files. Rationale, from
the exo state inventory (`exo/docs/SELF-CONTROL.md`, area 2):
the sandbox filesystem is the one non-durable layer — it does not survive
rewinds or warm-container death — while agent artifacts survive sandbox
rewinds and service restarts, persist across every conversation for the agent,
and are versioned, so every install/update is auditable and recoverable. This
mirrors how exo memory (`memory/exo-memory.json`) and the sandbox
snapshot registry are persisted.

Layout:

- `skills/index.json` — the catalog: `{ skills: [{ name, description,
installedAt, updatedAt }] }`. Prompt assembly reads only this artifact each
  turn (stage 1), so listing cost does not grow with skill body sizes.
- `skills/<name>.json` — one artifact per skill: `{ name, description,
skillMd, files: [{ path, contents }] }`. Written before the index entry is
  published, so a skill listed in the index always has content.

Uninstall removes the index entry only; prior content-artifact versions remain
readable, consistent with the "reversible by default" principle. Reinstalling
the same name writes a new version and updates the index entry.

Known limitation (shared with the memory store): index updates are
read-modify-write without compare-and-swap, so two conversations installing
skills concurrently can lose one index update. Fix alongside the artifact
versioning rework (see the TODO in `exo/tools/memory-tools.ts`).

Supporting files are stored as UTF-8 text in v1. Binary assets are out of
scope; skills needing binaries should fetch them at use time (the body can
instruct the agent to download into the sandbox).

## Tool surface

Implemented in `exoharness/typescript/harness/skill-tools.ts` and exported from
`@exo/harness`, so any harness — not just exo — can register the tools and
inject the listing. Nothing in the module depends on exo; it only uses the
generic `Agent` artifact API.

- `install_skill(skillMd, files?)` — validates frontmatter per the spec (the
  skill name comes from the frontmatter, like the spec's name-must-match-
  directory rule), rejects non-relative or `..` file paths, writes the skill
  artifact, then publishes it in the index. Installing an existing name
  updates it.
- `list_skills()` — the catalog with descriptions (stage 1, also available as
  a tool).
- `use_skill(name)` — full `SKILL.md` body plus the paths (not contents) of
  bundled files (stage 2).
- `read_skill_file(name, path)` — one bundled file (stage 3).
- `uninstall_skill(name)` — removes the index entry.

Prompt injection: `skillsInstruction(context)` returns a developer message
listing `name — description` for every installed skill, with the standing
instruction to call `use_skill` before performing a matching task. It returns
`null` when no skills are installed, and degrades (loudly, without throwing)
if the index artifact is corrupt — same failure policy as exo memory:
prompt assembly must never brick the agent, and the write path still refuses
to bury a corrupt store.

## Installation paths

1. **Agent-driven** (works today): the agent fetches a skill in its sandbox
   (git clone, curl), reads `SKILL.md` and the supporting files with `shell`,
   and calls `install_skill`. This is also how the agent authors skills for
   itself — Hermes calls this pattern "procedural memory".
2. **Human-driven** (works today): paste a `SKILL.md` into chat and ask the
   agent to install it.
3. **Future**: an `install_skill_from_path` variant that reads a directory
   from the sandbox mount directly, and registry installs (ClawHub,
   agentskills.io) — both are additive tool-surface changes on the same
   store.

## Why not alternatives

- **Sandbox directory (`~/.skills/`)**: the standard location pattern
  elsewhere, but our sandbox filesystem is explicitly non-durable; skills
  would silently vanish on rewind or container cleanup.
- **Checked-in repo directory**: durable and auditable, but installs would
  require the code-edit + rebuild path, and skills are agent state, not
  program code. Repo-shipped _default_ skills can still be layered later by
  seeding the artifact store at setup time.
- **One blob for all skills** (like the memory store): simplest, but stage-1
  listing would then read every skill body each model round; the index split
  keeps the per-turn cost proportional to the catalog, not the content.
