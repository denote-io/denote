import { existsSync, readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { isDeepStrictEqual } from "node:util";
import type {
  DenotePlugin,
  PluginActivationContext,
  PluginCapabilities,
  PluginCapability,
  PluginCatalogEntry,
  PluginDisposable,
  PluginManifest,
} from "@denote/plugin-sdk";
import { verifyPluginDownloads } from "../../scripts/plugin-downloads.mjs";

// jsdom replaces the global URL, so resolve from the module path instead.
const REPOSITORY_ROOT = join(dirname(fileURLToPath(import.meta.url)), "..", "..");

/** Released plugin archives staged by `npm run fetch:plugins`. */
export const RELEASED_PLUGIN_ROOT = join(REPOSITORY_ROOT, ".plugin-artifacts");

const CATALOG_PATH = join(REPOSITORY_ROOT, "plugins", "catalog.json");

type RegisteringCapability = {
  [K in keyof PluginCapabilities]-?: NonNullable<PluginCapabilities[K]> extends {
    register: (registration: never) => PluginDisposable;
  }
    ? K
    : never;
}[keyof PluginCapabilities];

export type ReleasedPluginRegistration<K extends RegisteringCapability> = Parameters<
  NonNullable<PluginCapabilities[K]>["register"]
>[0];

/** Mirrors the plugin worker: authoring and sensitive features share one capability. */
const REGISTERING_PERMISSIONS: Partial<Record<PluginCapability, RegisteringCapability>> = {
  commands: "commands",
  sidebar: "sidebar",
  status: "status",
  "editor-decoration": "editorDecoration",
  "emoji-picker": "emojiPicker",
  "structured-viewer": "structuredViewer",
  "kanban-board": "kanbanBoard",
  "task-list": "taskList",
  "note-graph": "noteGraph",
  calendar: "calendar",
  reminders: "reminders",
  "time-tracking": "timeTracking",
  clocks: "clocks",
  "diagram-renderer": "diagramRenderer",
  "code-intelligence": "codeIntelligence",
  tooling: "tooling",
  terminal: "terminal",
  "text-styling": "authoring",
  "note-comments": "authoring",
  presentation: "authoring",
  speech: "sensitiveTools",
  "credential-store": "sensitiveTools",
  sync: "sensitiveTools",
};

export interface ActivatedReleasedPlugin {
  manifest: PluginManifest;
  logs: Array<{ level: string; message: string }>;
  /** The single live registration the plugin made through one capability. */
  registration<K extends RegisteringCapability>(
    capability: K,
  ): ReleasedPluginRegistration<K>;
  dispose(): Promise<void>;
}

export function releasedPluginCatalog(): PluginCatalogEntry[] {
  return JSON.parse(readFileSync(CATALOG_PATH, "utf8")) as PluginCatalogEntry[];
}

export function releasedPluginPath(pluginId: string, ...segments: string[]): string {
  return join(RELEASED_PLUGIN_ROOT, pluginId, ...segments);
}

/**
 * Ensures the verified release archives for `pluginIds` (default: the whole
 * catalog) are extracted under `.plugin-artifacts/<plugin-id>/`. Only missing
 * or stale entries are downloaded, so concurrent readers of already staged
 * plugins are never disturbed.
 */
export async function stageReleasedPlugins(
  pluginIds?: readonly string[],
): Promise<PluginCatalogEntry[]> {
  const catalog = releasedPluginCatalog();
  const entries = pluginIds === undefined
    ? catalog
    : pluginIds.map((id) => {
      const entry = catalog.find((candidate) => candidate.manifest.id === id);
      if (!entry) throw new Error(`${id} is not in plugins/catalog.json.`);
      return entry;
    });
  const missing = entries.filter((entry) => !isStaged(entry));
  if (missing.length > 0) {
    await verifyPluginDownloads(missing, { stageDirectory: RELEASED_PLUGIN_ROOT });
  }
  return entries;
}

/**
 * Imports a released plugin bundle and activates it with a minimal recording
 * host. Only the capabilities granted by the manifest permissions are present,
 * and settings start from the manifest defaults like a fresh installation.
 */
export async function activateReleasedPlugin(
  pluginId: string,
  options: { settings?: Record<string, unknown> } = {},
): Promise<ActivatedReleasedPlugin> {
  const [entry] = await stageReleasedPlugins([pluginId]);
  const entrypoint = pathToFileURL(releasedPluginPath(pluginId, entry.manifest.entrypoint)).href;
  const module = (await import(/* @vite-ignore */ entrypoint)) as { default: DenotePlugin };
  const plugin = module.default;
  if (plugin.manifest.id !== pluginId || plugin.manifest.version !== entry.manifest.version) {
    throw new Error(`${pluginId} bundle does not match its catalog entry.`);
  }

  const registrations: Array<{ capability: RegisteringCapability; value: unknown; live: boolean }> = [];
  const subscriptions: PluginDisposable[] = [];
  const logs: ActivatedReleasedPlugin["logs"] = [];
  const capabilities: Partial<Record<RegisteringCapability, { register: (value: unknown) => PluginDisposable }>> = {};
  for (const { capability: permission } of entry.manifest.permissions) {
    const capability = REGISTERING_PERMISSIONS[permission];
    if (capability && !capabilities[capability]) {
      capabilities[capability] = {
        register(value) {
          const record = { capability, value, live: true };
          registrations.push(record);
          return { dispose: () => { record.live = false; } };
        },
      };
    }
  }
  const storage = new Map<string, unknown>();
  const settings = {
    ...Object.fromEntries(
      Object.entries(entry.manifest.settings?.properties ?? {})
        .map(([key, definition]) => [key, definition.default]),
    ),
    ...options.settings,
  };
  const log = (level: string) => (message: string) => { logs.push({ level, message }); };
  const context: PluginActivationContext = {
    pluginId,
    logger: { debug: log("debug"), info: log("info"), warn: log("warn"), error: log("error") },
    storage: {
      get: async <T>(key: string) => (storage.has(key) ? structuredClone(storage.get(key)) as T : null),
      set: async (key, value) => { storage.set(key, structuredClone(value)); },
      delete: async (key) => { storage.delete(key); },
      clear: async () => { storage.clear(); },
    },
    settings: { getAll: async () => structuredClone(settings) },
    capabilities: capabilities as PluginCapabilities,
    subscriptions: { add: (disposable) => { subscriptions.push(disposable); } },
  };
  await plugin.activate(context);

  return {
    manifest: plugin.manifest,
    logs,
    registration<K extends RegisteringCapability>(capability: K) {
      const live = registrations.filter((record) => record.capability === capability && record.live);
      if (live.length !== 1) {
        throw new Error(`Expected one ${capability} registration from ${pluginId}, found ${live.length}.`);
      }
      return live[0].value as ReleasedPluginRegistration<K>;
    },
    async dispose() {
      for (const disposable of subscriptions.splice(0).reverse()) await disposable.dispose();
      await plugin.deactivate?.();
    },
  };
}

function isStaged(entry: PluginCatalogEntry): boolean {
  const manifestPath = releasedPluginPath(entry.manifest.id, "plugin.json");
  if (!existsSync(manifestPath)) return false;
  try {
    const staged: unknown = JSON.parse(readFileSync(manifestPath, "utf8"));
    return isDeepStrictEqual(staged, entry.manifest) &&
      [entry.manifest.entrypoint, entry.manifest.diagramRenderer?.entrypoint]
        .every((path) => path === undefined || existsSync(releasedPluginPath(entry.manifest.id, path)));
  } catch {
    return false;
  }
}
