import { expect, it } from "vitest";
import { assertValidPluginCatalogEntry, checkPluginCompatibility } from "@denote/plugin-sdk";
import catalogJson from "../plugins/catalog.json";

// Catalog plugins that need host support first published after 1.0.0, with the
// first Denote release that provides it. Every other plugin runs on all of 1.x.
const LATER_MINIMUM: Record<string, string> = {
  "denote.sync": "1.1.0",
};

it("keeps every catalog plugin compatible with API 1 throughout Denote 1.x", () => {
  for (const value of catalogJson) {
    const entry: unknown = value;
    assertValidPluginCatalogEntry(entry);
    const minimum = LATER_MINIMUM[entry.manifest.id];
    if (minimum) {
      expect(entry.manifest.compatibility.minimumDenoteVersion).toBe(minimum);
      expect(checkPluginCompatibility(entry.manifest, "1.0.0", 1).compatible,
        `${entry.manifest.id} on Denote 1.0.0`).toBe(false);
    }
    for (const host of minimum ? [minimum, "1.42.0"] : ["1.0.0-rc.1", "1.0.0", "1.42.0"]) {
      expect(checkPluginCompatibility(entry.manifest, host, 1),
        `${entry.manifest.id} on Denote ${host}`).toEqual({ compatible: true, reason: null });
    }
    expect(checkPluginCompatibility(entry.manifest, "1.0.0", 2).compatible).toBe(false);
  }
});
