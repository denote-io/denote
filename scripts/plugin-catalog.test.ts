// @vitest-environment node
import { createHash } from "node:crypto";
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { create } from "tar";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { PluginCatalogEntry } from "@denote/plugin-sdk";
import catalogJson from "../plugins/catalog.json";
import { catalogErrors, editorImportErrors } from "./plugin-catalog";
import { applyCatalogEntry, releaseCatalogEntry } from "./update-plugin";

const catalog = catalogJson as PluginCatalogEntry[];
const temporary: string[] = [];

afterEach(() => {
  vi.unstubAllGlobals();
  while (temporary.length) rmSync(temporary.pop()!, { recursive: true, force: true });
});

function clone(): PluginCatalogEntry {
  return structuredClone(catalog.find((entry) => entry.manifest.id === "denote.reference")!);
}

describe("plugin catalog", () => {
  it("accepts the embedded catalog", () => {
    expect(catalogErrors(catalog)).toEqual([]);
  });

  it("requires each plugin's own repository release asset", () => {
    const entry = clone();
    const { version } = entry.manifest;
    for (const url of [
      `https://github.com/mbianchidev/denote/releases/download/v0.14.3/denote.reference-${version}.tgz`,
      `https://github.com/denote-io/denote-kanban/releases/download/v${version}/denote.reference-${version}.tgz`,
      `https://github.com/denote-io/denote-reference/releases/download/v9.9.9/denote.reference-${version}.tgz`,
    ]) {
      entry.artifact.url = url;
      expect(catalogErrors([entry]).join(), url).toContain("artifact.url must be");
    }
  });

  it("accepts only trusted Denote repositories and publishers", () => {
    const entry = clone();
    for (const repository of [
      "https://github.com/mbianchidev/denote",
      "https://github.com/denote-io/denote",
      "https://github.com/denote-io/denote-reference",
    ]) {
      entry.manifest.repository = repository;
      expect(catalogErrors([entry]), repository).toEqual([]);
    }
    entry.manifest.repository = "https://github.com/denote-io/denote-kanban";
    expect(catalogErrors([entry]).join()).toContain("trusted Denote publisher");
    entry.manifest.repository = "https://github.com/denote-io/denote-reference";
    entry.provenance.sourceCommit = "main";
    expect(catalogErrors([entry]).join()).toContain("sourceCommit");
    expect(catalogErrors([clone(), clone()]).join()).toContain("Duplicate plugin ID");
  });

  it("keeps plugin implementation packages out of the editor", () => {
    const root = mkdtempSync(join(tmpdir(), "denote-editor-imports-"));
    temporary.push(root);
    writeFileSync(join(root, "allowed.ts"), 'import { parsePluginManifest } from "@denote/plugin-sdk";\n');
    writeFileSync(join(root, "blocked.ts"), 'const plugin = await import("@denote/plugin-kanban");\n');
    expect(editorImportErrors(root)).toEqual([
      expect.stringContaining("must not bundle plugin implementation @denote/plugin-kanban"),
    ]);
  });
});

describe("plugin catalog updates", () => {
  async function releaseArchive(entry: PluginCatalogEntry): Promise<Buffer> {
    const root = mkdtempSync(join(tmpdir(), "denote-plugin-release-"));
    temporary.push(root);
    const files: Record<string, string> = {
      [entry.manifest.entrypoint]: "export default {};\n",
      [entry.manifest.documentation]: entry.guide,
      "plugin.json": JSON.stringify(entry.manifest),
    };
    for (const [path, content] of Object.entries(files)) {
      mkdirSync(dirname(join(root, "inputs", path)), { recursive: true });
      writeFileSync(join(root, "inputs", path), content);
    }
    await create({ cwd: join(root, "inputs"), file: join(root, "release.tgz"), gzip: true, portable: true }, Object.keys(files));
    return readFileSync(join(root, "release.tgz"));
  }

  it("records a release archive's manifest, guide, digest, and tag commit", async () => {
    const current = clone();
    const next = structuredClone(current);
    next.manifest.version = "9.0.0";
    next.manifest.repository = "https://github.com/denote-io/denote-reference";
    const bytes = await releaseArchive(next);
    const fetch = vi.fn().mockResolvedValue(new Response(new Uint8Array(bytes)));
    vi.stubGlobal("fetch", fetch);

    const entry = await releaseCatalogEntry("denote.reference", "9.0.0", "c".repeat(40));

    expect(fetch).toHaveBeenCalledWith(
      "https://github.com/denote-io/denote-reference/releases/download/v9.0.0/denote.reference-9.0.0.tgz",
      expect.objectContaining({ redirect: "manual" }),
    );
    expect(entry).toEqual({
      manifest: next.manifest,
      artifact: {
        url: "https://github.com/denote-io/denote-reference/releases/download/v9.0.0/denote.reference-9.0.0.tgz",
        sha256: createHash("sha256").update(bytes).digest("hex"),
        sizeBytes: bytes.length,
      },
      provenance: { publisherId: "denote", sourceCommit: "c".repeat(40), trusted: true },
      guide: next.guide,
    });
    const updated = applyCatalogEntry(catalog, entry);
    expect(updated.map((item) => item.manifest.id)).toEqual(catalog.map((item) => item.manifest.id));
    expect(catalogErrors(updated)).toEqual([]);
  });

  it("rejects an archive for another version and never replaces published bytes", async () => {
    const current = clone();
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue(new Response(new Uint8Array(await releaseArchive(current)))));
    await expect(releaseCatalogEntry("denote.reference", "9.0.0", "c".repeat(40)))
      .rejects.toThrow(`declares denote.reference@${current.manifest.version}`);

    const changed = { ...current, artifact: { ...current.artifact, sha256: "d".repeat(64) } };
    expect(() => applyCatalogEntry(catalog, changed)).toThrow("Released versions are immutable");
    expect(applyCatalogEntry(catalog, current)).toEqual(catalog);
  });
});
