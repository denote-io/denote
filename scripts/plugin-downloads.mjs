import { createHash } from "node:crypto";
import { mkdirSync, readFileSync, renameSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join, posix, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { Parser } from "tar";

export const PLUGIN_ORGANIZATION = "denote-io";
export const MAX_PLUGIN_PACKAGE_BYTES = 25 * 1024 * 1024;

const allowedHosts = new Set([
  "github.com",
  "raw.githubusercontent.com",
  "objects.githubusercontent.com",
  "release-assets.githubusercontent.com",
]);

/** The repository that publishes a trusted Denote plugin, such as denote-io/denote-kanban. */
export function pluginRepository(pluginId) {
  const name = /^denote\.([a-z0-9]+(?:-[a-z0-9]+)*)$/.exec(pluginId)?.[1];
  if (!name) {
    throw new Error(`${pluginId} is not a trusted denote.<name> plugin ID.`);
  }
  return `${PLUGIN_ORGANIZATION}/denote-${name}`;
}

/** The immutable release asset URL for a plugin version. */
export function pluginReleaseUrl(pluginId, version) {
  return `https://github.com/${pluginRepository(pluginId)}/releases/download/v${version}/${pluginId}-${version}.tgz`;
}

/** Downloads a bounded artifact, following only HTTPS redirects between GitHub hosts. */
export async function downloadArtifact(
  initialUrl,
  pluginId,
  maxBytes = MAX_PLUGIN_PACKAGE_BYTES,
  fetcher = fetch,
) {
  let url = initialUrl;
  const signal = AbortSignal.timeout(30_000);
  for (let redirects = 0; redirects <= 4; redirects += 1) {
    const parsed = new URL(url);
    if (
      parsed.protocol !== "https:" ||
      parsed.username !== "" ||
      parsed.password !== "" ||
      parsed.port !== "" ||
      !allowedHosts.has(parsed.hostname)
    ) {
      throw new Error(`Pinned artifact host is not allowed for ${pluginId}.`);
    }
    const response = await fetcher(url, { redirect: "manual", signal });
    if ([301, 302, 303, 307, 308].includes(response.status)) {
      const location = response.headers.get("location");
      await response.body?.cancel();
      if (!location || redirects === 4) {
        throw new Error(`Pinned artifact redirect limit exceeded for ${pluginId}.`);
      }
      url = new URL(location, url).toString();
      continue;
    }
    if (!response.ok) {
      await response.body?.cancel();
      throw new Error(
        `Pinned artifact is unavailable for ${pluginId} at ${initialUrl}: HTTP ${response.status}.`,
      );
    }
    const reader = response.body?.getReader();
    if (!reader) {
      throw new Error(`Pinned artifact has no response body for ${pluginId}.`);
    }
    const chunks = [];
    let received = 0;
    try {
      while (true) {
        const chunk = await reader.read();
        if (chunk.done) {
          break;
        }
        received += chunk.value.byteLength;
        if (received > maxBytes) {
          await reader.cancel();
          throw new Error(`Pinned artifact is larger than catalog metadata for ${pluginId}.`);
        }
        chunks.push(chunk.value);
      }
    } finally {
      reader.releaseLock();
    }
    return Buffer.concat(chunks);
  }
  throw new Error(`Pinned artifact redirect limit exceeded for ${pluginId}.`);
}

export async function verifyRemoteArtifact(
  initialUrl,
  pluginId,
  expectedSize,
  expectedSha256,
  fetcher = fetch,
) {
  if (
    !Number.isSafeInteger(expectedSize) ||
    expectedSize <= 0 ||
    expectedSize > MAX_PLUGIN_PACKAGE_BYTES ||
    !/^[0-9a-f]{64}$/.test(expectedSha256)
  ) {
    throw new Error(`Invalid pinned artifact integrity metadata for ${pluginId}.`);
  }
  const bytes = await downloadArtifact(initialUrl, pluginId, expectedSize, fetcher);
  if (
    bytes.length !== expectedSize ||
    createHash("sha256").update(bytes).digest("hex") !== expectedSha256
  ) {
    throw new Error(`Pinned artifact bytes do not match catalog metadata for ${pluginId}.`);
  }
  return bytes;
}

/** Reads a plugin archive in memory, accepting only bounded regular files at safe paths. */
export async function readPluginArchive(bytes, pluginId) {
  const files = new Map();
  let expanded = 0;
  let failure;
  const parser = new Parser({
    strict: true,
    onReadEntry(entry) {
      const path = entry.path;
      if (
        entry.type !== "File" ||
        posix.normalize(path) !== path ||
        path.startsWith("/") ||
        path.split("/").includes("..") ||
        files.has(path)
      ) {
        failure ??= new Error(`${pluginId} archive has an unexpected, duplicate, or unsafe entry: ${path}`);
        entry.resume();
        return;
      }
      const chunks = [];
      entry.on("data", (chunk) => {
        expanded += chunk.length;
        if (expanded > MAX_PLUGIN_PACKAGE_BYTES) {
          failure ??= new Error(`${pluginId} archive exceeds the expanded size limit.`);
        } else {
          chunks.push(chunk);
        }
      });
      entry.on("end", () => files.set(path, Buffer.concat(chunks)));
    },
  });
  await new Promise((resolvePromise, reject) => {
    parser.on("error", reject);
    parser.on("end", resolvePromise);
    parser.end(bytes);
  });
  if (failure) {
    throw failure;
  }
  return files;
}

/** Confirms an archive ships the catalog's exact manifest, guide, and entrypoints. */
export function verifyArchiveMetadata(entry, files) {
  const { manifest } = entry;
  const packaged = files.get("plugin.json");
  if (!packaged) {
    throw new Error(`${manifest.id} archive is missing plugin.json.`);
  }
  if (stableStringify(JSON.parse(packaged.toString("utf8"))) !== stableStringify(manifest)) {
    throw new Error(`${manifest.id} archive manifest does not match its catalog entry.`);
  }
  const guide = files.get(manifest.documentation);
  if (!guide || normalizeText(guide.toString("utf8")) !== normalizeText(entry.guide)) {
    throw new Error(`${manifest.id} archive guide does not match its catalog entry.`);
  }
  for (const path of [manifest.entrypoint, manifest.diagramRenderer?.entrypoint].filter(Boolean)) {
    if (!files.has(path)) {
      throw new Error(`${manifest.id} archive is missing ${path}.`);
    }
  }
}

/**
 * Downloads every catalog archive and verifies its size, SHA-256 digest,
 * manifest, and guide. With `stageDirectory`, also writes the verified
 * `<id>-<version>.tgz` and its extracted files under `<id>/` there.
 */
export async function verifyPluginDownloads(catalog, { stageDirectory, fetcher = fetch } = {}) {
  const failures = [];
  for (const entry of catalog) {
    const { manifest, artifact, provenance } = entry;
    try {
      if (!/^[0-9a-f]{40}$/.test(provenance.sourceCommit)) {
        throw new Error(`${manifest.id} requires a 40-character source commit.`);
      }
      const staged = stageDirectory
        ? join(stageDirectory, `${manifest.id}-${manifest.version}.tgz`)
        : null;
      const bytes = (staged && readStaged(staged, artifact)) ||
        await verifyRemoteArtifact(artifact.url, manifest.id, artifact.sizeBytes, artifact.sha256, fetcher);
      const files = await readPluginArchive(bytes, manifest.id);
      verifyArchiveMetadata(entry, files);
      if (staged) {
        writeAtomic(staged, bytes);
        const extracted = join(stageDirectory, manifest.id);
        rmSync(extracted, { recursive: true, force: true });
        for (const [path, content] of files) {
          const target = join(extracted, ...path.split("/"));
          mkdirSync(dirname(target), { recursive: true });
          writeFileSync(target, content);
        }
      }
    } catch (error) {
      failures.push(`${manifest.id}: ${error instanceof Error ? error.message : String(error)}`);
    }
  }
  if (failures.length > 0) {
    throw new Error(`Plugin download verification failed:\n${failures.join("\n")}`);
  }
}

function readStaged(path, artifact) {
  try {
    const bytes = readFileSync(path);
    return bytes.length === artifact.sizeBytes &&
      createHash("sha256").update(bytes).digest("hex") === artifact.sha256
      ? bytes
      : null;
  } catch {
    return null;
  }
}

function writeAtomic(path, bytes) {
  mkdirSync(dirname(path), { recursive: true });
  const temporary = `${path}.${process.pid}.tmp`;
  try {
    writeFileSync(temporary, bytes);
    renameSync(temporary, path);
  } finally {
    rmSync(temporary, { force: true });
  }
}

function stableStringify(value) {
  const sort = (item) => Array.isArray(item)
    ? item.map(sort)
    : item && typeof item === "object"
      ? Object.fromEntries(Object.keys(item).sort().map((key) => [key, sort(item[key])]))
      : item;
  return JSON.stringify(sort(value));
}

function normalizeText(value) {
  return value.replace(/\r\n/g, "\n").trim();
}

if (
  process.argv[1] &&
  pathToFileURL(resolve(process.argv[1])).href === import.meta.url
) {
  try {
    const args = process.argv.slice(2);
    if (args.length > 1 || (args.length === 1 && args[0] !== "--stage")) {
      throw new Error("Usage: node scripts/plugin-downloads.mjs [--stage]");
    }
    const root = fileURLToPath(new URL("..", import.meta.url));
    const catalog = JSON.parse(readFileSync(join(root, "plugins", "catalog.json"), "utf8"));
    const stageDirectory = args.includes("--stage") ? join(root, ".plugin-artifacts") : undefined;
    await verifyPluginDownloads(catalog, { stageDirectory });
    console.log(stageDirectory
      ? `Staged ${catalog.length} verified plugin archive(s) in ${stageDirectory}.`
      : `Verified ${catalog.length} published plugin archive(s).`);
  } catch (error) {
    console.error(error instanceof Error ? error.message : String(error));
    process.exitCode = 1;
  }
}
