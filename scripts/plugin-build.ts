import { existsSync, readFileSync, readdirSync, realpathSync } from "node:fs";
import { createRequire } from "node:module";
import { basename, dirname, join, relative, sep } from "node:path";
import { fileURLToPath } from "node:url";
import { execFileSync } from "node:child_process";
import { pluginSdkModulePath, pluginSdkSourceCommit } from "./plugin-sdk-provenance";
import { pluginEntrypointConditions } from "./plugin-build-conditions";
import { build, type Plugin } from "vite";
import {
  parsePluginManifest,
  assertValidPluginCatalogEntry,
  type PluginCatalogEntry,
  type PluginManifest,
} from "@denote/plugin-sdk";

export const projectRoot = fileURLToPath(new URL("..", import.meta.url));
export const pluginsRoot = join(projectRoot, "plugins");
// The SDK is installed from denote-io/denote-plugin-sdk; packages/plugin-sdk is its historical path.
export const sdkRoot = realpathSync(join(
  dirname(createRequire(join(projectRoot, "package.json")).resolve("@denote/plugin-sdk")),
  "..",
));

export function pluginDirectories(pluginId?: string): string[] {
  const directories = readdirSync(pluginsRoot, { withFileTypes: true })
    .filter((entry) => entry.isDirectory())
    .map((entry) => join(pluginsRoot, entry.name))
    .sort();
  if (!pluginId) {
    return directories;
  }
  const selected = directories.find(
    (directory) => readPluginManifest(directory).id === pluginId,
  );
  if (!selected) {
    throw new Error(`Unknown plugin: ${pluginId}`);
  }
  return [selected];
}

export function readPluginManifest(pluginDirectory: string): PluginManifest {
  const manifestValue: unknown = JSON.parse(
    readFileSync(join(pluginDirectory, "plugin.json"), "utf8"),
  );
  return parsePluginManifest(manifestValue);
}

export async function buildPlugin(pluginDirectory: string): Promise<PluginManifest> {
  const manifest = readPluginManifest(pluginDirectory);
  const catalog: unknown = JSON.parse(readFileSync(join(pluginsRoot, "catalog.json"), "utf8"));
  if (!Array.isArray(catalog)) {
    throw new Error("Plugin catalog must be an array.");
  }
  const entries: PluginCatalogEntry[] = catalog.map((entry: unknown) => {
    assertValidPluginCatalogEntry(entry);
    return entry;
  });
  const sourceCommit = pluginSdkSourceCommit(manifest, entries);
  const entrypoints = [
    {
      path: manifest.entrypoint,
      minify: false,
      conditions: pluginEntrypointConditions("worker"),
    },
    ...(manifest.diagramRenderer
      ? [
          {
            path: manifest.diagramRenderer.entrypoint,
            minify: true,
            conditions: pluginEntrypointConditions("diagram-renderer"),
          },
        ]
      : []),
  ];
  for (const [index, entrypoint] of entrypoints.entries()) {
    const outputPath = join(pluginDirectory, entrypoint.path);
    const sourcePath = join(
      pluginDirectory,
      entrypoint.path.replace(/^dist\//, "src/").replace(/\.js$/, ".ts"),
    );
    await build({
      configFile: false,
      logLevel: "error",
      plugins: [
        ...(sourceCommit ? [pinnedPluginSdk(sourceCommit)] : []),
        pluginBoundary(pluginDirectory, sdkRoot),
        stableSourceLabels(manifest, sdkRoot),
      ],
      resolve: {
        conditions: entrypoint.conditions,
      },
      build: {
        emptyOutDir: index === 0,
        minify: entrypoint.minify,
        sourcemap: false,
        outDir: dirname(outputPath),
        lib: {
          entry: sourcePath,
          formats: ["es"],
          fileName: () => basename(outputPath),
        },
        rolldownOptions: {
          output: {
            codeSplitting: false,
          },
        },
      },
    });
  }
  console.log(`Built ${manifest.id}@${manifest.version}.`);
  return manifest;
}

function stableSourceLabels(manifest: PluginManifest, canonicalSdkRoot: string): Plugin {
  const sdkLabel = relative(projectRoot, canonicalSdkRoot).split(sep).join("/");
  return {
    name: "denote-stable-plugin-source-labels",
    renderChunk(code) {
      // Keep historical debug labels stable when a package moves on disk.
      return code
        .replace(
          /^\/\/#region plugins\/[^/]+\//gm,
          `//#region packages/plugins/${manifest.id}/`,
        )
        .split(`\n// ${sdkLabel}/`)
        .join("\n// packages/plugin-sdk/");
    },
  };
}

function pinnedPluginSdk(sourceCommit: string): Plugin {
  if (!/^[a-f0-9]{40}$/.test(sourceCommit)) {
    throw new Error("Plugin SDK provenance must be a complete source commit.");
  }
  return {
    name: "denote-pinned-plugin-sdk",
    enforce: "pre",
    load(id) {
      const path = pluginSdkModulePath(sdkRoot, id);
      if (!path || !sdkSourceExists(sourceCommit, path)) {
        // Pins made after the SDK moved build against the locked SDK release.
        return;
      }
      // Keep canonical module IDs (and bundle bytes), but rebuild an immutable
      // plugin against the SDK that produced it, not a later host capability.
      return execFileSync("git", ["show", `${sourceCommit}:${path}`], {
        cwd: projectRoot,
        encoding: "utf8",
        maxBuffer: 2 * 1024 * 1024,
        stdio: ["ignore", "pipe", "pipe"],
      });
    },
  };
}

function sdkSourceExists(sourceCommit: string, path: string): boolean {
  try {
    execFileSync("git", ["cat-file", "-e", `${sourceCommit}:${path}`], {
      cwd: projectRoot,
      stdio: ["ignore", "ignore", "ignore"],
    });
    return true;
  } catch {
    return false;
  }
}

function pluginBoundary(pluginDirectory: string, canonicalSdkRoot: string): Plugin {
  const canonicalRoot = realpathSync(pluginDirectory);
  return {
    name: "denote-plugin-boundary",
    moduleParsed(moduleInfo) {
      const modulePath = moduleInfo.id.split("?")[0];
      if (modulePath.startsWith("\0") || !existsSync(modulePath)) {
        return;
      }
      const canonicalPath = realpathSync(modulePath);
      if (
        canonicalPath === canonicalRoot ||
        canonicalPath.startsWith(`${canonicalRoot}${sep}`) ||
        canonicalPath === canonicalSdkRoot ||
        canonicalPath.startsWith(`${canonicalSdkRoot}${sep}`) ||
        canonicalPath.includes(`${sep}node_modules${sep}`)
      ) {
        return;
      }
      this.error(`Plugin module resolves outside its package: ${canonicalPath}.`);
    },
  };
}
