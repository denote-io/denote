import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import { resolve } from "node:path";
import { pathToFileURL } from "node:url";
import { parsePluginManifest, type PluginCatalogEntry } from "@denote/plugin-sdk";
import { readCatalog, writeCatalog } from "./plugin-catalog";
import {
  downloadArtifact, pluginReleaseUrl, pluginRepository, readPluginArchive, verifyArchiveMetadata,
} from "./plugin-downloads.mjs";

const SEMVER = /^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?$/;

/** Resolves the commit a plugin release tag points to. */
export function releaseCommit(pluginId: string, version: string): string {
  const repository = `https://github.com/${pluginRepository(pluginId)}.git`;
  const tag = `refs/tags/v${version}`;
  const refs = execFileSync("git", ["ls-remote", repository, tag, `${tag}^{}`], {
    encoding: "utf8", stdio: ["ignore", "pipe", "pipe"], timeout: 60_000,
  }).trim().split("\n").filter(Boolean).map((line) => line.split("\t"));
  // An annotated tag lists its peeled commit as <tag>^{}.
  const commit = (refs.find(([, ref]) => ref === `${tag}^{}`) ?? refs.find(([, ref]) => ref === tag))?.[0];
  if (!commit || !/^[0-9a-f]{40}$/.test(commit)) {
    throw new Error(`${repository} has no v${version} release tag.`);
  }
  return commit;
}

/**
 * Builds the catalog entry for a published plugin release: the archive's own
 * manifest and guide, its exact size and SHA-256 digest, and the tagged commit.
 */
export async function releaseCatalogEntry(
  pluginId: string, version: string, sourceCommit = releaseCommit(pluginId, version),
): Promise<PluginCatalogEntry> {
  const url = pluginReleaseUrl(pluginId, version);
  const bytes = await downloadArtifact(url, pluginId);
  const files = await readPluginArchive(bytes, pluginId);
  const manifestBytes = files.get("plugin.json");
  if (!manifestBytes) throw new Error(`${pluginId} ${version} archive is missing plugin.json.`);
  const manifest = parsePluginManifest(JSON.parse(manifestBytes.toString("utf8")));
  if (manifest.id !== pluginId || manifest.version !== version) {
    throw new Error(`The v${version} archive declares ${manifest.id}@${manifest.version}.`);
  }
  const guide = files.get(manifest.documentation)?.toString("utf8").replace(/\r\n/g, "\n");
  if (guide === undefined) throw new Error(`${pluginId} ${version} archive is missing ${manifest.documentation}.`);
  const entry: PluginCatalogEntry = {
    manifest,
    artifact: { url, sha256: createHash("sha256").update(bytes).digest("hex"), sizeBytes: bytes.length },
    provenance: { publisherId: "denote", sourceCommit, trusted: true },
    guide,
  };
  verifyArchiveMetadata(entry, files);
  return entry;
}

/** Replaces or adds one plugin's catalog entry; published versions are immutable. */
export function applyCatalogEntry(catalog: PluginCatalogEntry[], entry: PluginCatalogEntry): PluginCatalogEntry[] {
  const index = catalog.findIndex((item) => item.manifest.id === entry.manifest.id);
  const current = index >= 0 ? catalog[index] : undefined;
  if (current?.manifest.version === entry.manifest.version && (
    current.artifact.sha256 !== entry.artifact.sha256 || current.artifact.sizeBytes !== entry.artifact.sizeBytes
  )) {
    throw new Error(`${entry.manifest.id}@${entry.manifest.version} is already in the catalog with different bytes. Released versions are immutable.`);
  }
  return index >= 0
    ? catalog.map((item, position) => (position === index ? entry : item))
    : [...catalog, entry];
}

if (process.argv[1] && pathToFileURL(resolve(process.argv[1])).href === import.meta.url) {
  try {
    const [pluginId, requested, ...extra] = process.argv.slice(2);
    const version = requested?.replace(/^v/, "");
    if (!pluginId || !version || !SEMVER.test(version) || extra.length) {
      throw new Error("Usage: npm run update:plugin -- <plugin-id> <version>");
    }
    const entry = await releaseCatalogEntry(pluginId, version);
    writeCatalog(applyCatalogEntry(readCatalog(), entry));
    console.log(
      `Updated ${pluginId} to ${version} (${entry.artifact.sizeBytes} bytes, sha256 ${entry.artifact.sha256}, ` +
      `source ${entry.provenance.sourceCommit}). Review plugins/catalog.json and plugins/bundles.json, ` +
      "then run npm run check:plugins and npm run check:plugin-downloads.",
    );
  } catch (error) {
    console.error(error instanceof Error ? error.message : String(error));
    process.exitCode = 1;
  }
}
