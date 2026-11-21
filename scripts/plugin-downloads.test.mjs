import { createHash } from "node:crypto";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { create } from "tar";
import { afterEach, describe, expect, it, vi } from "vitest";
import {
  pluginReleaseUrl,
  pluginRepository,
  readPluginArchive,
  verifyPluginDownloads,
  verifyRemoteArtifact,
} from "./plugin-downloads.mjs";

const bytes = Buffer.from("synthetic plugin archive");
const sha256 = createHash("sha256").update(bytes).digest("hex");
const releaseUrl =
  "https://github.com/denote-io/denote-synthetic/releases/download/v1.0.0/denote.synthetic-1.0.0.tgz";
const temporary = [];

afterEach(() => {
  vi.unstubAllGlobals();
  while (temporary.length) rmSync(temporary.pop(), { recursive: true, force: true });
});

async function archive(files) {
  const root = mkdtempSync(join(tmpdir(), "denote-plugin-download-"));
  temporary.push(root);
  const inputs = join(root, "inputs");
  for (const [path, content] of Object.entries(files)) {
    mkdirSync(dirname(join(inputs, path)), { recursive: true });
    writeFileSync(join(inputs, path), content);
  }
  const file = join(root, "archive.tgz");
  await create({ cwd: inputs, file, gzip: true, portable: true }, Object.keys(files));
  return readFileSync(file);
}

function entry(archiveBytes, id = "denote.synthetic") {
  const manifest = {
    id, version: "1.0.0", entrypoint: "dist/index.js", documentation: "guide.md",
  };
  return {
    manifest,
    artifact: {
      url: pluginReleaseUrl(id, "1.0.0"),
      sha256: createHash("sha256").update(archiveBytes).digest("hex"),
      sizeBytes: archiveBytes.length,
    },
    provenance: { sourceCommit: "a".repeat(40) },
    guide: "# Synthetic guide\n",
  };
}

const files = {
  "dist/index.js": "export default {};\n",
  "guide.md": "# Synthetic guide\n",
  "plugin.json": JSON.stringify({
    version: "1.0.0", id: "denote.synthetic", documentation: "guide.md", entrypoint: "dist/index.js",
  }),
};

describe("plugin release locations", () => {
  it("maps each trusted plugin to its own denote-io repository", () => {
    expect(pluginRepository("denote.json-yaml-viewer")).toBe("denote-io/denote-json-yaml-viewer");
    expect(pluginReleaseUrl("denote.kanban", "0.1.7")).toBe(
      "https://github.com/denote-io/denote-kanban/releases/download/v0.1.7/denote.kanban-0.1.7.tgz",
    );
    for (const id of ["synthetic.plugin", "denote.", "denote.Upper", "denote.a/b"]) {
      expect(() => pluginRepository(id)).toThrow("not a trusted");
    }
  });
});

describe("published plugin downloads", () => {
  it("verifies every archive's bytes, manifest, and guide and reports every failure", async () => {
    const good = await archive(files);
    const other = entry(good, "denote.other");
    const fetch = vi.fn().mockImplementation(async (url) =>
      url.includes("denote-other") ? new Response(null, { status: 404 }) : new Response(good));
    vi.stubGlobal("fetch", fetch);

    await expect(verifyPluginDownloads([entry(good), other]))
      .rejects.toThrow(/^Plugin download verification failed:\ndenote.other: .*HTTP 404\.$/);
    await expect(verifyPluginDownloads([entry(good)])).resolves.toBeUndefined();
  });

  it.each([
    ["manifest", { ...files, "plugin.json": JSON.stringify({ id: "denote.synthetic", version: "2.0.0" }) }],
    ["guide", { ...files, "guide.md": "# Different guide\n" }],
    ["entrypoint", { "guide.md": files["guide.md"], "plugin.json": files["plugin.json"] }],
  ])("rejects an archive whose %s does not match the catalog", async (part, contents) => {
    const changed = await archive(contents);
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue(new Response(changed)));

    await expect(verifyPluginDownloads([entry(changed)])).rejects.toThrow(part === "entrypoint" ? "missing dist/index.js" : part);
  });

  it("stages verified archives and their files, reusing verified staged bytes", async () => {
    const good = await archive(files);
    const stageDirectory = mkdtempSync(join(tmpdir(), "denote-plugin-stage-"));
    temporary.push(stageDirectory);
    const fetch = vi.fn().mockResolvedValue(new Response(good));
    vi.stubGlobal("fetch", fetch);

    await verifyPluginDownloads([entry(good)], { stageDirectory });
    await verifyPluginDownloads([entry(good)], { stageDirectory });

    expect(fetch).toHaveBeenCalledTimes(1);
    expect(readFileSync(join(stageDirectory, "denote.synthetic-1.0.0.tgz"))).toEqual(good);
    expect(readFileSync(join(stageDirectory, "denote.synthetic", "dist", "index.js"), "utf8"))
      .toBe(files["dist/index.js"]);
  });

  it("rejects unsafe archive entries without writing them", async () => {
    const root = mkdtempSync(join(tmpdir(), "denote-plugin-unsafe-"));
    temporary.push(root);
    mkdirSync(join(root, "inputs", "dist"), { recursive: true });
    writeFileSync(join(root, "inputs", "dist", "index.js"), "export default {};\n");
    const file = join(root, "unsafe.tgz");
    await create({ cwd: join(root, "inputs"), file, gzip: true, preservePaths: true, prefix: "../" }, ["dist/index.js"]);

    await expect(readPluginArchive(readFileSync(file), "denote.synthetic")).rejects.toThrow("unsafe entry");
    expect(existsSync(join(root, "dist"))).toBe(false);
  });

  it("rejects an unpinned source ref before making a request", async () => {
    const catalog = [entry(bytes)];
    catalog[0].provenance.sourceCommit = "main";
    const fetch = vi.fn();
    vi.stubGlobal("fetch", fetch);

    await expect(verifyPluginDownloads(catalog)).rejects.toThrow("40-character source commit");
    expect(fetch).not.toHaveBeenCalled();
  });
});

describe("remote artifact integrity", () => {
  it("verifies release asset redirects and exact bytes", async () => {
    const assetUrl = "https://release-assets.githubusercontent.com/synthetic.tgz";
    const fetch = vi.fn()
      .mockResolvedValueOnce(new Response(null, { status: 302, headers: { location: assetUrl } }))
      .mockResolvedValueOnce(new Response(bytes));
    vi.stubGlobal("fetch", fetch);

    await verifyRemoteArtifact(releaseUrl, "denote.synthetic", bytes.length, sha256);

    expect(fetch).toHaveBeenCalledTimes(2);
  });

  it.each([
    "http://github.com/synthetic.tgz",
    "https://example.invalid/synthetic.tgz",
    "https://user@github.com/synthetic.tgz",
    "https://github.com:444/synthetic.tgz",
  ])("rejects unapproved redirect %s before requesting it", async (location) => {
    const fetch = vi.fn().mockResolvedValue(
      new Response(null, { status: 302, headers: { location } }),
    );
    vi.stubGlobal("fetch", fetch);

    await expect(verifyRemoteArtifact(releaseUrl, "denote.synthetic", bytes.length, sha256))
      .rejects.toThrow("host is not allowed");
    expect(fetch).toHaveBeenCalledTimes(1);
  });

  it.each(["short", "oversized synthetic plugin archive", "different plugin bytes!"])(
    "rejects incorrect download bytes: %s",
    async (content) => {
      vi.stubGlobal("fetch", vi.fn().mockResolvedValue(new Response(content)));

      await expect(verifyRemoteArtifact(releaseUrl, "denote.synthetic", bytes.length, sha256))
        .rejects.toThrow(/catalog metadata/);
    },
  );

  it("bounds redirect chains without falling back to another artifact", async () => {
    const fetch = vi.fn().mockImplementation(async () =>
      new Response(null, { status: 302, headers: { location: releaseUrl } }),
    );
    vi.stubGlobal("fetch", fetch);

    await expect(verifyRemoteArtifact(releaseUrl, "denote.synthetic", bytes.length, sha256))
      .rejects.toThrow("redirect limit exceeded");
    expect(fetch).toHaveBeenCalledTimes(5);
  });
});
