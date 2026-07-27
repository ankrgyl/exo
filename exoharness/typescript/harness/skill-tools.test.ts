import { describe, expect, it } from "vitest";

import type { ArtifactVersion, JsonObject, TurnContext } from "./index";

import {
  createSkillToolInstances,
  parseSkillFrontmatter,
  skillsInstruction,
} from "./skill-tools";

// Minimal in-memory stand-in for the agent artifact handle. Each
// writeArtifactJson appends a new version at the same path, matching how the
// real store versions artifacts.
class FakeHandle {
  private versions: {
    artifactId: string;
    path: string;
    version: number;
    value: unknown;
  }[] = [];
  private seq = 0;

  async listArtifacts(): Promise<ArtifactVersion[]> {
    return this.versions.map(({ artifactId, path, version }) => ({
      artifactId,
      path,
      version,
      createdAt: "1970-01-01T00:00:00Z",
      sizeBytes: 0,
    }));
  }

  async readArtifactJson<T>({
    artifactId,
    version,
  }: {
    artifactId: string;
    version?: number;
  }): Promise<T | null> {
    const selected = this.versions.find(
      (item) =>
        item.artifactId === artifactId &&
        (version === undefined || item.version === version),
    );
    return selected ? (selected.value as T) : null;
  }

  async writeArtifactJson({
    path,
    value,
  }: {
    path: string;
    value: unknown;
  }): Promise<ArtifactVersion> {
    this.seq += 1;
    const version = this.seq;
    const artifactId = `${path}@${version}`;
    this.versions.push({ artifactId, path, version, value });
    return {
      artifactId,
      path,
      version,
      createdAt: "1970-01-01T00:00:00Z",
      sizeBytes: 0,
    };
  }
}

function makeContext(): { context: TurnContext; agent: FakeHandle } {
  const agent = new FakeHandle();
  const context = {
    exoharness: { current: { agent } },
  } as unknown as TurnContext;
  return { context, agent };
}

const toolsByName = Object.fromEntries(
  createSkillToolInstances().map((tool) => [tool.definition.name, tool]),
);

function call(name: string, args: JsonObject, context: TurnContext) {
  return toolsByName[name].handler.execute(args, { context });
}

const PDF_SKILL_MD = `---
name: pdf-processing
description: Extract PDF text, fill forms, merge files. Use when handling PDFs.
license: Apache-2.0
---

# PDF processing

Run scripts/extract.py against the input file.
`;

describe("skill frontmatter parsing", () => {
  it("parses required fields and returns the body", () => {
    const parsed = parseSkillFrontmatter(PDF_SKILL_MD);
    expect(parsed).not.toBeNull();
    expect(parsed?.fields.name).toBe("pdf-processing");
    expect(parsed?.fields.description).toContain("Extract PDF text");
    expect(parsed?.fields.license).toBe("Apache-2.0");
    expect(parsed?.body).toContain("# PDF processing");
  });

  it("strips surrounding quotes from values", () => {
    const parsed = parseSkillFrontmatter(
      '---\nname: quoted\ndescription: "A quoted description."\n---\nbody',
    );
    expect(parsed?.fields.description).toBe("A quoted description.");
  });

  it("returns null without a frontmatter block", () => {
    expect(parseSkillFrontmatter("# just markdown")).toBeNull();
  });
});

describe("skill tools", () => {
  it("installs a skill and injects it into the prompt", async () => {
    const { context } = makeContext();
    const result = (await call(
      "install_skill",
      {
        skillMd: PDF_SKILL_MD,
        files: [{ path: "scripts/extract.py", contents: "print('hi')" }],
      },
      context,
    )) as { ok: boolean; name: string; updated: boolean };

    expect(result.ok).toBe(true);
    expect(result.name).toBe("pdf-processing");
    expect(result.updated).toBe(false);

    const message = await skillsInstruction(context);
    expect(message).not.toBeNull();
    const content = String(message?.content);
    expect(content).toContain("pdf-processing");
    expect(content).toContain("Use when handling PDFs.");
    // Stage 1 must not include the body.
    expect(content).not.toContain("# PDF processing");
  });

  it("loads the body with use_skill and files with read_skill_file", async () => {
    const { context } = makeContext();
    await call(
      "install_skill",
      {
        skillMd: PDF_SKILL_MD,
        files: [{ path: "scripts/extract.py", contents: "print('hi')" }],
      },
      context,
    );

    const used = (await call(
      "use_skill",
      { name: "pdf-processing" },
      context,
    )) as { ok: boolean; skillMd: string; files: string[] };
    expect(used.ok).toBe(true);
    expect(used.skillMd).toContain("# PDF processing");
    expect(used.files).toEqual(["scripts/extract.py"]);

    const file = (await call(
      "read_skill_file",
      { name: "pdf-processing", path: "scripts/extract.py" },
      context,
    )) as { ok: boolean; contents: string };
    expect(file.ok).toBe(true);
    expect(file.contents).toBe("print('hi')");
  });

  it("updates an existing skill in place", async () => {
    const { context } = makeContext();
    await call("install_skill", { skillMd: PDF_SKILL_MD }, context);
    const second = (await call(
      "install_skill",
      {
        skillMd:
          "---\nname: pdf-processing\ndescription: Updated description.\n---\nNew body.",
      },
      context,
    )) as { ok: boolean; updated: boolean };
    expect(second.ok).toBe(true);
    expect(second.updated).toBe(true);

    const listed = (await call("list_skills", {}, context)) as {
      skills: { name: string; description: string }[];
    };
    expect(listed.skills).toHaveLength(1);
    expect(listed.skills[0].description).toBe("Updated description.");

    const used = (await call(
      "use_skill",
      { name: "pdf-processing" },
      context,
    )) as { skillMd: string };
    expect(used.skillMd).toContain("New body.");
  });

  it("uninstalls a skill", async () => {
    const { context } = makeContext();
    await call("install_skill", { skillMd: PDF_SKILL_MD }, context);
    const removed = (await call(
      "uninstall_skill",
      { name: "pdf-processing" },
      context,
    )) as { ok: boolean; removed: number };
    expect(removed.ok).toBe(true);
    expect(removed.removed).toBe(1);

    expect(await skillsInstruction(context)).toBeNull();
    const used = (await call(
      "use_skill",
      { name: "pdf-processing" },
      context,
    )) as { ok: boolean };
    expect(used.ok).toBe(false);
  });

  it("rejects missing frontmatter, bad names, and bad descriptions", async () => {
    const { context } = makeContext();
    const noFrontmatter = (await call(
      "install_skill",
      { skillMd: "# no frontmatter" },
      context,
    )) as { ok: boolean; error: string };
    expect(noFrontmatter.ok).toBe(false);
    expect(noFrontmatter.error).toMatch(/frontmatter/);

    const badName = (await call(
      "install_skill",
      { skillMd: "---\nname: Bad_Name\ndescription: x\n---\nbody" },
      context,
    )) as { ok: boolean };
    expect(badName.ok).toBe(false);

    const noDescription = (await call(
      "install_skill",
      { skillMd: "---\nname: ok-name\n---\nbody" },
      context,
    )) as { ok: boolean; error: string };
    expect(noDescription.ok).toBe(false);
    expect(noDescription.error).toMatch(/description/);
  });

  it("rejects the reserved name index without touching the catalog", async () => {
    const { context } = makeContext();
    await call("install_skill", { skillMd: PDF_SKILL_MD }, context);
    const result = (await call(
      "install_skill",
      {
        skillMd:
          "---\nname: index\ndescription: Shadows the catalog artifact.\n---\nbody",
      },
      context,
    )) as { ok: boolean; error: string };
    expect(result.ok).toBe(false);
    expect(result.error).toMatch(/reserved/);

    // The catalog must remain readable and unchanged.
    const listed = (await call("list_skills", {}, context)) as {
      ok: boolean;
      skills: { name: string }[];
    };
    expect(listed.ok).toBe(true);
    expect(listed.skills.map((skill) => skill.name)).toEqual([
      "pdf-processing",
    ]);
  });

  it("rejects absolute and traversal file paths", async () => {
    const { context } = makeContext();
    for (const path of ["/etc/passwd", "../escape.txt", "a//b"]) {
      const result = (await call(
        "install_skill",
        { skillMd: PDF_SKILL_MD, files: [{ path, contents: "x" }] },
        context,
      )) as { ok: boolean };
      expect(result.ok).toBe(false);
    }
  });

  it("returns null instruction when no skills are installed", async () => {
    const { context } = makeContext();
    expect(await skillsInstruction(context)).toBeNull();
  });

  it("makes the write path reject on a corrupt index", async () => {
    const { context, agent } = makeContext();
    await agent.writeArtifactJson({
      path: "skills/index.json",
      value: { skills: "not-an-array" },
    });
    await expect(
      call("install_skill", { skillMd: PDF_SKILL_MD }, context),
    ).rejects.toThrow(/corrupt skills index/);
  });

  it("degrades the prompt read path on a corrupt index instead of throwing", async () => {
    const { context, agent } = makeContext();
    await agent.writeArtifactJson({
      path: "skills/index.json",
      value: { skills: "not-an-array" },
    });
    const message = await skillsInstruction(context);
    expect(message).not.toBeNull();
    expect(String(message?.content)).toMatch(/unavailable|corrupt/i);
  });
});
