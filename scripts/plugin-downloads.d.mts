import type { PluginCatalogEntry } from "@denote/plugin-sdk";

export const PLUGIN_ORGANIZATION: string;
export const MAX_PLUGIN_PACKAGE_BYTES: number;

export function pluginRepository(pluginId: string): string;

export function pluginReleaseUrl(pluginId: string, version: string): string;

export function downloadArtifact(
  initialUrl: string,
  pluginId: string,
  maxBytes?: number,
  fetcher?: typeof fetch,
): Promise<Buffer>;

export function verifyRemoteArtifact(
  initialUrl: string,
  pluginId: string,
  expectedSize: number,
  expectedSha256: string,
  fetcher?: typeof fetch,
): Promise<Buffer>;

export function readPluginArchive(bytes: Uint8Array, pluginId: string): Promise<Map<string, Buffer>>;

export function verifyArchiveMetadata(
  entry: PluginCatalogEntry,
  files: ReadonlyMap<string, Buffer>,
): void;

export function verifyPluginDownloads(
  catalog: readonly PluginCatalogEntry[],
  options?: { stageDirectory?: string; fetcher?: typeof fetch },
): Promise<void>;
