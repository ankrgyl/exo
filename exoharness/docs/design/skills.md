# Skills

Skills give an agent installable, durable capability packages: reusable
instructions (and supporting files) that the agent can discover cheaply every
turn and load fully only when a task calls for one.

## The standard we follow

Following the [agentskills.io](https://agentskills.io) specification:

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

Skills are stored as **agent artifacts**, not sandbox files. This ensures durability and makes the skill accessible to the agent across environments.

Layout:

- `skills/index.json` — the catalog: `{ skills: [{ name, description,
installedAt, updatedAt }] }`. Prompt assembly reads only this artifact each
  turn (stage 1), so listing cost does not grow with skill body sizes.
- `skills/<name>.json` — one artifact per skill: `{ name, description,
skillMd, files: [{ path, contents }] }`. Written before the index entry is
  published, so a skill listed in the index always has content.

Uninstall removes the index entry only; prior content-artifact versions remain
readable. Reinstalling the same name writes a new version and updates the index entry.

Supporting files are stored as UTF-8 text in v1.

## Tool surface

Importable by any agent built over exoharness: `exoharness/typescript/harness/skill-tools.ts`.

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
if the index artifact is corrupt.

## Installation paths

1. **Agent-driven** (works today): the agent fetches a skill in its sandbox
   (git clone, curl), reads `SKILL.md` and the supporting files with `shell`,
   and calls `install_skill`. This is also how an agent can author skills for
   itself.
2. **Human-driven** (works today): paste a `SKILL.md` into chat and ask the
   agent to install it.
3. **Future**: an `install_skill_from_path` variant that reads a directory
   from the sandbox mount directly, and registry installs (ClawHub,
   agentskills.io) — both are additive tool-surface changes on the same
   store.
