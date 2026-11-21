import { readFileSync, readdirSync, renameSync, rmSync, writeFileSync } from "node:fs";
import { extname, join, relative, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import {
  assertValidPluginBundles,
  assertValidPluginCatalogEntry,
  type PluginCatalogEntry,
} from "@denote/plugin-sdk";
import { pluginReleaseUrl, pluginRepository } from "./plugin-downloads.mjs";

export const projectRoot = fileURLToPath(new URL("..", import.meta.url));
const catalogPath = (root: string) => join(root, "plugins", "catalog.json");

/**
 * Source repositories a trusted plugin manifest may name: the Denote repository
 * that published versions up to the plugin migration, before or after its move
 * to denote-io, and the plugin's own denote-io repository.
 */
export function trustedPluginRepositories(pluginId: string): string[] {
  return [
    "https://github.com/mbianchidev/denote",
    "https://github.com/denote-io/denote",
    `https://github.com/${pluginRepository(pluginId)}`,
  ];
}

/** Returns every problem with the catalog; an empty list means it is valid. */
export function catalogErrors(value: unknown): string[] {
  if (!Array.isArray(value)) return ["plugins/catalog.json must be an array."];
  const errors: string[] = [];
  const ids = new Set<string>();
  for (const [index, entry] of value.entries()) {
    try {
      assertValidPluginCatalogEntry(entry);
      const { manifest, artifact, provenance } = entry;
      if (ids.has(manifest.id)) throw new Error(`Duplicate plugin ID ${manifest.id}.`);
      ids.add(manifest.id);
      if (!/^[0-9a-f]{40}$/.test(provenance.sourceCommit)) {
        throw new Error("provenance.sourceCommit must be a 40-character commit SHA.");
      }
      if (
        provenance.publisherId !== "denote" ||
        manifest.publisher.name !== "Denote" ||
        !trustedPluginRepositories(manifest.id).includes(manifest.repository)
      ) {
        throw new Error("The catalog accepts only the trusted Denote publisher.");
      }
      const expected = pluginReleaseUrl(manifest.id, manifest.version);
      if (artifact.url !== expected) {
        throw new Error(`artifact.url must be the plugin repository release asset ${expected}.`);
      }
    } catch (error) {
      errors.push(`catalog[${index}]: ${error instanceof Error ? error.message : String(error)}`);
    }
  }
  return errors;
}

export function readCatalog(root = projectRoot): PluginCatalogEntry[] {
  const value: unknown = JSON.parse(readFileSync(catalogPath(root), "utf8"));
  const errors = catalogErrors(value);
  if (errors.length) throw new Error(`Invalid plugin catalog:\n- ${errors.join("\n- ")}`);
  return value as PluginCatalogEntry[];
}

export function writeCatalog(catalog: PluginCatalogEntry[], root = projectRoot): void {
  const errors = catalogErrors(catalog);
  if (errors.length) throw new Error(`Invalid plugin catalog:\n- ${errors.join("\n- ")}`);
  const path = catalogPath(root);
  const temporary = `${path}.${process.pid}.tmp`;
  try {
    writeFileSync(temporary, `${JSON.stringify(catalog, null, 2)}\n`, { flag: "wx" });
    renameSync(temporary, path);
  } finally {
    rmSync(temporary, { force: true });
  }
}

/** The editor may use the SDK but never bundle a plugin implementation package. */
export function editorImportErrors(sourceRoot: string): string[] {
  const errors: string[] = [];
  const visit = (directory: string) => {
    for (const entry of readdirSync(directory, { withFileTypes: true })) {
      const path = join(directory, entry.name);
      if (entry.isDirectory()) {
        visit(path);
        continue;
      }
      if (![".ts", ".tsx", ".mts", ".js", ".jsx", ".mjs"].includes(extname(path))) continue;
      const source = readFileSync(path, "utf8");
      for (const match of source.matchAll(/(?:from\s+|import\s*\(\s*)["'](@denote\/plugin-[^"'/]+)/g)) {
        if (match[1] !== "@denote/plugin-sdk") {
          errors.push(`${relative(projectRoot, path)} must not bundle plugin implementation ${match[1]}.`);
        }
      }
    }
  };
  visit(sourceRoot);
  return errors;
}

if (process.argv[1] && pathToFileURL(resolve(process.argv[1])).href === import.meta.url) {
  const errors: string[] = [];
  try {
    const catalog: unknown = JSON.parse(readFileSync(catalogPath(projectRoot), "utf8"));
    errors.push(...catalogErrors(catalog));
    const ids = new Set(Array.isArray(catalog) ? catalog.map((entry) => entry?.manifest?.id) : []);
    try {
      assertValidPluginBundles(
        JSON.parse(readFileSync(join(projectRoot, "plugins", "bundles.json"), "utf8")),
        ids,
      );
    } catch (error) {
      errors.push(`plugins/bundles.json: ${error instanceof Error ? error.message : String(error)}`);
    }
    errors.push(...editorImportErrors(join(projectRoot, "src")));
    if (errors.length) {
      console.error(`Plugin catalog validation failed:\n- ${errors.join("\n- ")}`);
      process.exitCode = 1;
    } else {
      console.log(`Validated ${ids.size} catalog plugin(s) and their bundles.`);
    }
  } catch (error) {
    console.error(error instanceof Error ? error.message : String(error));
    process.exitCode = 1;
  }
}
