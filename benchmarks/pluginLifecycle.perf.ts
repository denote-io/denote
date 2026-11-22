import { expect, test, type Locator, type Page } from "@playwright/test";
import { build } from "vite";
import { stageReleasedPlugins } from "../src/test/releasedPlugins";
import catalogJson from "../plugins/catalog.json" with { type: "json" };
import { THEME_STORAGE_KEY } from "../src/lib/theme";
import { auditAccessibility } from "./accessibility";

let fixture = "";
async function setCardOpen(page: Page, card: Locator, open: boolean): Promise<void> {
  if (await card.evaluate((element) => element instanceof HTMLDetailsElement && element.open) !== open) {
    await card.locator(":scope > summary").focus();
    await page.keyboard.press("Enter");
  }
  await expect(card).toHaveJSProperty("open", open);
}

test.beforeAll(async () => {
  await stageReleasedPlugins();
  const result = await build({
    configFile: false, logLevel: "error",
    build: { write: false, lib: { entry: "benchmarks/pluginLifecycleFixture.ts", name: "syntheticPluginLifecycle", formats: ["iife"] } },
  });
  const bundles = Array.isArray(result) ? result : [result];
  if (bundles.length !== 1 || !("output" in bundles[0])) throw new Error("Expected one plugin lifecycle fixture.");
  const chunk = bundles[0].output.find((output) => output.type === "chunk");
  if (!chunk || chunk.type !== "chunk") throw new Error("Missing plugin lifecycle fixture.");
  fixture = chunk.code;
});

test("repeated enable and disable cycles release workers, ports, closures, listeners and DOM nodes", async ({ page }, info) => {
  test.setTimeout(240_000);
  const cycledIds = [
    "denote.kanban", "denote.mermaid", "denote.time-tracking",
    "denote.git", "denote.terminal", "denote.emoji-picker",
  ];
  const manifests = cycledIds.map((id) => {
    const entry = catalogJson.find((candidate) => candidate.manifest.id === id);
    if (!entry) throw new Error(`Missing catalog plugin ${id}.`);
    return entry.manifest;
  });
  const errors: string[] = [];
  page.on("pageerror", (error) => errors.push(error.message));
  await page.addInitScript({ content:
    `localStorage.setItem(${JSON.stringify(THEME_STORAGE_KEY)}, "dark");\n${fixture}` });
  await page.goto("/");
  await page.getByRole("button", { name: "Authoring.md", exact: true }).click();
  await page.getByRole("button", { name: "Open settings", exact: true }).click();
  await page.getByRole("button", { name: "Plugins", exact: true }).click();
  const settings = page.getByRole("dialog", { name: "Settings", exact: true });
  const cdp = await page.context().newCDPSession(page);
  await cdp.send("Performance.enable");
  await cdp.send("HeapProfiler.enable");
  const settledMetrics = async () => {
    await cdp.send("HeapProfiler.collectGarbage");
    await cdp.send("HeapProfiler.collectGarbage");
    const { metrics } = await cdp.send("Performance.getMetrics");
    const value = (name: string) => {
      const metric = metrics.find((candidate) => candidate.name === name);
      if (!metric) throw new Error(`Missing browser metric ${name}.`);
      return metric.value;
    };
    const live = async (constructor: string) => {
      const prototype = await cdp.send("Runtime.evaluate", { expression: `${constructor}.prototype`, objectGroup: "leak-check" });
      const prototypeId = prototype.result.objectId;
      if (!prototypeId) throw new Error(`Missing ${constructor} prototype.`);
      const { objects } = await cdp.send("Runtime.queryObjects", { prototypeObjectId: prototypeId, objectGroup: "leak-check" });
      if (!objects.objectId) throw new Error(`Missing ${constructor} instances.`);
      const length = await cdp.send("Runtime.callFunctionOn", {
        objectId: objects.objectId, functionDeclaration: "function () { return this.length; }", returnByValue: true,
      });
      // Query results pin every match; release them so later samples are not inflated.
      await cdp.send("Runtime.releaseObjectGroup", { objectGroup: "leak-check" });
      return Number(length.result.value);
    };
    return {
      heapBytes: value("JSHeapUsedSize"),
      listeners: value("JSEventListeners"),
      nodes: value("Nodes"),
      documents: value("Documents"),
      workers: await live("Worker"),
      ports: await live("MessagePort"),
      functions: await live("Function"),
      maps: await live("Map"),
      promises: await live("Promise"),
    };
  };
  const cycle = async () => {
    for (const manifest of manifests) {
      const card = settings.locator(".plugin-card").filter({ has: page.getByText(manifest.name, { exact: true }) });
      await setCardOpen(page, card, true);
      await card.getByRole("button", { name: "Enable", exact: true }).focus();
      await page.keyboard.press("Enter");
      await card.getByRole("button", { name: "Approve and enable", exact: true }).focus();
      await page.keyboard.press("Enter");
      await expect.poll(() => page.evaluate(() => window.__DENOTE_PLUGIN_LIFECYCLE__.activeWorkers)).toContain(manifest.id);
      await expect(card.locator(":scope > summary")).toContainText("Enabled");
      await setCardOpen(page, card, true);
      await card.getByRole("button", { name: "Disable and remove code", exact: true }).focus();
      await page.keyboard.press("Enter");
      if (manifest.permissions.some((permission) => permission.capability === "clocks")) {
        const confirm = page.getByRole("button", { name: "Stop clocks and disable", exact: true });
        await confirm.focus();
        await page.keyboard.press("Enter");
      }
      await expect.poll(() => page.evaluate(() => window.__DENOTE_PLUGIN_LIFECYCLE__.activeWorkers)).toEqual([]);
      await setCardOpen(page, card, false);
    }
    expect(await page.evaluate(() => window.__DENOTE_PLUGIN_LIFECYCLE__.installed)).toEqual([]);
  };

  // The first cycle loads lazy modules and staged bundles; later cycles must
  // return to that settled baseline.
  await cycle();
  const baseline = await settledMetrics();
  const measuredCycles = 5;
  for (let index = 0; index < measuredCycles; index += 1) await cycle();
  const final = await settledMetrics();
  await cdp.detach();
  await info.attach("plugin-cycle-metrics.json", {
    body: JSON.stringify({ plugins: cycledIds, measuredCycles, baseline, final }, null, 2),
    contentType: "application/json",
  });

  const lifecycle = await page.evaluate(() => ({
    started: window.__DENOTE_PLUGIN_LIFECYCLE__.startedWorkers.length,
    terminated: window.__DENOTE_PLUGIN_LIFECYCLE__.terminatedWorkers.length,
    committed: window.__DENOTE_PLUGIN_LIFECYCLE__.committed,
    removed: window.__DENOTE_PLUGIN_LIFECYCLE__.removed,
  }));
  expect(lifecycle.terminated).toBe(lifecycle.started);
  for (const id of cycledIds) {
    expect(lifecycle.committed.filter((value) => value === id)).toHaveLength(measuredCycles + 1);
    expect(lifecycle.removed.filter((value) => value === id)).toHaveLength(measuredCycles + 1);
  }
  // Thirty plugin cycles follow the warm-up. One leaked worker, port, closure,
  // runtime map, promise, listener or DOM subtree per cycle exceeds these
  // budgets. Remaining heap growth is V8 compiled code and bounded browser
  // performance-entry buffers, so the heap budget is deliberately coarser.
  const pluginCycles = cycledIds.length * measuredCycles;
  expect(final.documents).toBe(baseline.documents);
  expect(final.workers).toBe(baseline.workers);
  expect(final.ports).toBe(baseline.ports);
  for (const metric of ["functions", "maps", "promises", "listeners"] as const) {
    expect(final[metric] - baseline[metric], metric).toBeLessThan(pluginCycles / 2);
  }
  expect(final.nodes - baseline.nodes).toBeLessThan(300);
  expect(final.heapBytes - baseline.heapBytes).toBeLessThan(8 * 1024 * 1024);
  expect(await page.evaluate(() => window.__DENOTE_AUTHORING__.saves)).toEqual([]);
  expect(await page.evaluate(() => window.__DENOTE_AUTHORING__.unexpectedCommands)).toEqual([]);
  expect(errors).toEqual([]);
});

test("every catalog worker requires approval, preserves content, exposes accessible surfaces and releases on disable", async ({ page }, info) => {
  test.setTimeout(120_000);
  const errors: string[] = [];
  page.on("pageerror", (error) => errors.push(error.message));
  await page.addInitScript({ content:
    `localStorage.setItem(${JSON.stringify(THEME_STORAGE_KEY)}, "dark");\n${fixture}` });
  await page.goto("/");
  await page.getByRole("button", { name: "Authoring.md", exact: true }).click();
  const original = await page.evaluate(() => ({
    documents: window.__DENOTE_AUTHORING__.documents,
    comments: window.__DENOTE_AUTHORING__.comments,
    audio: window.__DENOTE_AUTHORING__.audio,
  }));
  await page.getByRole("button", { name: "Open settings", exact: true }).click();
  await page.getByRole("button", { name: "Plugins", exact: true }).click();
  const settings = page.getByRole("dialog", { name: "Settings", exact: true });
  await expect(settings.locator(".plugin-card")).toHaveCount(catalogJson.length);
  expect(await page.evaluate(() => window.__DENOTE_PLUGIN_LIFECYCLE__.prepared)).toEqual([]);
  expect(await page.evaluate(() => window.__DENOTE_PLUGIN_LIFECYCLE__.entrypointReads)).toEqual([]);
  expect(await page.evaluate(() => window.__DENOTE_PLUGIN_LIFECYCLE__.activeWorkers)).toEqual([]);
  await page.evaluate((id) => {
    window.__DENOTE_PLUGIN_LIFECYCLE__.failNextEnable = id;
  }, catalogJson[0].manifest.id);

  for (const { manifest } of catalogJson) {
    const card = settings.locator(".plugin-card").filter({ has: page.getByText(manifest.name, { exact: true }) });
    await setCardOpen(page, card, true);
    await card.getByRole("button", { name: "Enable", exact: true }).focus();
    await page.keyboard.press("Enter");
    expect(await page.evaluate(() => window.__DENOTE_PLUGIN_LIFECYCLE__.entrypointReads)).not.toContain(manifest.id);
    await card.getByRole("button", { name: "Approve and enable", exact: true }).focus();
    await page.keyboard.press("Enter");
    if (manifest.id === catalogJson[0].manifest.id) {
      await expect(card.getByText("Synthetic package verification failed. Review permissions and retry.", { exact: true })).toBeVisible();
      expect(await page.evaluate(() => window.__DENOTE_PLUGIN_LIFECYCLE__.activeWorkers)).toEqual([]);
      expect(await page.evaluate(() => window.__DENOTE_PLUGIN_LIFECYCLE__.installed)).toEqual([]);
      await auditAccessibility(page, info, "Failed package and keyboard retry", ".editor-settings-dialog");
      await card.getByRole("button", { name: "Approve and enable", exact: true }).focus();
      await page.keyboard.press("Enter");
    }
    await expect.poll(() => page.evaluate(() => window.__DENOTE_PLUGIN_LIFECYCLE__.committed)).toContain(manifest.id);
    await expect(card.locator(":scope > summary")).toContainText("Enabled");
    await setCardOpen(page, card, true);
    await expect(card.getByRole("button", { name: "Disable and remove code", exact: true })).toBeVisible();
    await setCardOpen(page, card, false);
  }
  await settings.getByRole("button", { name: "Done", exact: true }).click();
  const recoveredFailure = page.getByRole("alert").filter({
    hasText: "Synthetic package verification failed. Review permissions and retry.",
  });
  await recoveredFailure.getByRole("button", { name: "Dismiss error", exact: true }).focus();
  await page.keyboard.press("Enter");
  await expect(page.getByRole("alert")).toHaveCount(0);
  const ids = catalogJson.map((entry) => entry.manifest.id).sort();
  expect(await page.evaluate(() => window.__DENOTE_PLUGIN_LIFECYCLE__.activeWorkers)).toEqual(ids);
  expect(await page.evaluate(() => window.__DENOTE_AUTHORING__.saves)).toEqual([]);
  expect(await page.evaluate(() => ({
    documents: window.__DENOTE_AUTHORING__.documents,
    comments: window.__DENOTE_AUTHORING__.comments,
    audio: window.__DENOTE_AUTHORING__.audio,
  }))).toEqual(original);

  const core = new Set(["Files", "Search", "Bookmarks", "Recent files", "Trash"]);
  for (const theme of ["dark", "light"]) {
    if (theme === "light") {
      await page.getByRole("button", { name: "Switch to light mode", exact: true }).click();
    }
    await expect(page.locator("html")).toHaveAttribute("data-theme", theme);
    const labels = await page.locator(".activity-rail button[aria-pressed]").evaluateAll((buttons) =>
      buttons.map((button) => button.getAttribute("aria-label") ?? ""));
    expect(labels.every(Boolean)).toBe(true);
    for (const label of labels.filter((label) => !core.has(label))) {
      const control = page.locator(".activity-rail").getByRole("button", { name: label, exact: true });
      await expect(control).toBeEnabled();
      await control.focus();
      await page.keyboard.press("Enter");
      await expect(control).toHaveAttribute("aria-pressed", "true");
      await expect(page.getByRole("alert")).toHaveCount(0);
      await auditAccessibility(page, info, `${theme}: ${label} active surface`);
    }
  }

  await page.getByRole("button", { name: "Open settings", exact: true }).click();
  await page.getByRole("button", { name: "Plugins", exact: true }).click();
  for (const { manifest } of catalogJson) {
    const card = settings.locator(".plugin-card").filter({ has: page.getByText(manifest.name, { exact: true }) });
    await setCardOpen(page, card, true);
    await card.getByRole("button", { name: "Disable and remove code", exact: true }).focus();
    await page.keyboard.press("Enter");
    if (manifest.permissions.some((permission) => permission.capability === "clocks")) {
      const confirm = page.getByRole("button", { name: "Stop clocks and disable", exact: true });
      await expect(confirm).toBeVisible();
      await auditAccessibility(page, info, "Clock stop confirmation");
      await confirm.focus();
      await page.keyboard.press("Enter");
    }
    await expect.poll(() => page.evaluate(() => window.__DENOTE_PLUGIN_LIFECYCLE__.activeWorkers)).not.toContain(manifest.id);
    await setCardOpen(page, card, false);
  }
  expect(await page.evaluate(() => window.__DENOTE_PLUGIN_LIFECYCLE__.activeWorkers)).toEqual([]);
  expect(await page.evaluate(() => window.__DENOTE_PLUGIN_LIFECYCLE__.installed)).toEqual([]);
  expect((await page.evaluate(() => window.__DENOTE_PLUGIN_LIFECYCLE__.terminatedWorkers)).sort()).toEqual(ids);
  expect((await page.evaluate(() => window.__DENOTE_PLUGIN_LIFECYCLE__.removed)).sort()).toEqual(ids);
  expect(await page.evaluate(() => window.__DENOTE_AUTHORING__.saves)).toEqual([]);
  expect(await page.evaluate(() => ({
    documents: window.__DENOTE_AUTHORING__.documents,
    comments: window.__DENOTE_AUTHORING__.comments,
    audio: window.__DENOTE_AUTHORING__.audio,
  }))).toEqual(original);
  expect(await page.evaluate(() => window.__DENOTE_AUTHORING__.microphoneRequests)).toBe(0);
  expect(await page.evaluate(() => window.__DENOTE_AUTHORING__.unexpectedCommands)).toEqual([]);
  expect(errors).toEqual([]);
});
