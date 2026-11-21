import { readFileSync, readdirSync } from "node:fs";
import { join, relative, sep } from "node:path";
import type { Code, Nodes } from "mdast";
import { fromMarkdown } from "mdast-util-from-markdown";
import { parse as parseDiagram } from "pele";
import { beforeAll, describe, expect, it } from "vitest";
import { createCalendarSnapshot } from "../plugins/calendars";
import { createPdfFixture } from "../test/pdfFixtures";
import {
  activateReleasedPlugin,
  stageReleasedPlugins,
} from "../test/releasedPlugins";
import type { SearchDocument } from "../types";
import { extractTags, resolveInternalLink } from "./markdown";
import {
  CORE_SYNTAX_LANGUAGES,
  detectSourceLanguage,
} from "./syntaxLanguages";

const welcomeRoot = join(process.cwd(), "docs", "user-guide");
const codeRoot = join(welcomeRoot, "code");
const pluginRoot = join(welcomeRoot, "plugins");

describe("Welcome vault example inventory", () => {
  // Plugin samples are validated against the released plugin bundles.
  beforeAll(async () => {
    await stageReleasedPlugins([
      "denote.calendar", "denote.kanban", "denote.note-graph", "denote.task-lists",
    ]);
  }, 120_000);

  it("represents every distinct core syntax language with invented code", () => {
    const files = collectFiles(codeRoot).map((path) =>
      relative(codeRoot, path).split(sep).join("/"),
    );
    const detected = new Set(
      files
        .map((path) => detectSourceLanguage(path)?.id ?? null)
        .filter((id): id is NonNullable<typeof id> => id !== null),
    );
    if (files.includes("hello.pp")) {
      detected.add("puppet");
    }

    expect([...detected].sort()).toEqual(
      CORE_SYNTAX_LANGUAGES.map((language) => language.id).sort(),
    );
    expect(files).toEqual(
      expect.arrayContaining([
        "Dockerfile",
        "CMakeLists.txt",
        "Makefile",
        "Jenkinsfile",
        ".editorconfig",
        ".env.example",
        "go.mod",
        "BUILD.bazel",
        "_helpers.tpl",
        "Cargo.lock",
        "hello.component.html",
        "hello.mariadb.sql",
        "hello.mssql.sql",
        "hello.sqlite.sql",
        "meson.build",
        "hello.pas",
        "hello.pp",
      ]),
    );
    expect(detectSourceLanguage("hello.pp")).toBeNull();
  });

  function collectFiles(directory: string): string[] {
    return readdirSync(directory, { withFileTypes: true }).flatMap((entry) => {
      const path = join(directory, entry.name);
      return entry.isDirectory() ? collectFiles(path) : [path];
    });
  }

  it("keeps the checked-in PDF equal to the deterministic synthetic fixture", () => {
    const pdf = readFileSync(
      join(welcomeRoot, "examples", "Hello document.pdf"),
    );
    const expected = createPdfFixture([
      { lines: ["Hello document", "Synthetic local PDF."] },
    ]);

    expect(pdf).toEqual(Buffer.from(expected));
    expect(pdf.subarray(0, 8).toString("latin1")).toMatch(/^%PDF-1\.7/);
    expect(pdf.toString("latin1")).not.toMatch(
      /\/(?:JavaScript|JS|OpenAction|AA|AcroForm|Annots|EmbeddedFiles|URI)\b/,
    );
  });

  it("keeps JSON and YAML examples parseable, nested, and alias-free", async () => {
    const json = JSON.parse(
      readFileSync(join(welcomeRoot, "examples", "Sample data.json"), "utf8"),
    );
    expect(json).toMatchObject({
      project: { name: "Orbit notebook", active: true },
    });
    expect(Array.isArray(json.project.steps)).toBe(true);

    const { parseDocument } = await import("yaml");
    const yamlSource = readFileSync(
      join(welcomeRoot, "examples", "Sample data.yaml"),
      "utf8",
    );
    const yaml = parseDocument(yamlSource, {
      schema: "core",
      strict: true,
      merge: false,
      uniqueKeys: true,
    });
    expect(yaml.errors).toEqual([]);
    expect(yamlSource).not.toMatch(/[&*!][A-Za-z0-9_-]+/);
    expect(yaml.toJS()).toMatchObject({
      project: { name: "Orbit notebook", active: true },
    });
  });

  it("resolves plugin-example links and their guide entry points inside the canonical vault", () => {
    const availablePaths = collectFiles(welcomeRoot).map((path) =>
      relative(welcomeRoot, path).split(sep).join("/"),
    );
    const paths = [
      "Welcome.md",
      "docs/Feature reference.md",
      "docs/Optional plugins.md",
      ...availablePaths.filter((path) => path.startsWith("plugins/") && path.endsWith(".md")),
    ];

    for (const path of paths) {
      const nodes: Nodes[] = [fromMarkdown(readFileSync(join(welcomeRoot, path), "utf8"))];
      for (let node = nodes.pop(); node; node = nodes.pop()) {
        if ("children" in node) nodes.push(...node.children);
        if (
          (node.type === "link" || node.type === "image" || node.type === "definition") &&
          !/^(?:[a-z][a-z\d+.-]*:|\/\/)/i.test(node.url)
        ) {
          expect(
            resolveInternalLink(path, node.url, availablePaths),
            `${path} -> ${node.url}`,
          ).not.toBeNull();
        }
      }
    }
  });

  it("keeps the plugin data examples parseable with a plain YAML alias", async () => {
    const sharedData = {
      model: "Paper glider",
      ready: false,
      wingspan: 18.5,
      nextTrial: null,
      materials: [
        { item: "square sheet", count: 2 },
        { item: "paper clip", count: 1 },
      ],
      notes: {},
    };
    const json = JSON.parse(pluginSource("Structured data.json"));
    expect(json).toMatchObject(sharedData);
    expect(json.trials).toEqual({ indoor: [3, 4, 5], outdoor: [] });

    const { isAlias, parseDocument } = await import("yaml");
    const yaml = parseDocument(pluginSource("Structured data.yaml"), {
      schema: "core",
      strict: true,
      merge: false,
      uniqueKeys: true,
    });
    expect(yaml.errors).toEqual([]);
    expect(yaml.toJS()).toMatchObject(sharedData);
    const alias = yaml.get("repeat", true);
    expect(isAlias(alias)).toBe(true);
    if (!isAlias(alias)) throw new Error("The YAML example must retain its alias.");
    expect(alias.source).toBe("indoor");
  });

  it("finds both calendar samples by date and only the ordinary note by activity", async () => {
    const documents: SearchDocument[] = [
      "Calendar daily note.md",
      "Calendar dated note.md",
    ].map((name) => ({
      path: `plugins/${name}`,
      title: name,
      kind: "markdown",
      content: pluginSource(name),
      contentHash: "synthetic",
      encoding: "utf8",
      lineEnding: "lf",
      tags: [],
      bookmarked: false,
      lastOpenedAt: null,
      createdAt: Date.parse("2026-10-01T12:00:00Z"),
      modifiedAt: Date.parse("2026-10-02T12:00:00Z"),
    }));
    const snapshot = createCalendarSnapshot(
      documents.map(({ path }) => ({ path, modifiedAt: null })),
      { documents, skippedCount: 0, truncated: false },
    );
    const request = {
      ...snapshot,
      startDate: "2026-10-01",
      endDate: "2026-10-31",
      timeZone: "UTC",
    };
    const calendar = await activateReleasedPlugin("denote.calendar");
    const withoutMetadata = await activateReleasedPlugin("denote.calendar", {
      settings: { dateMetadata: false },
    });
    const { query } = calendar.registration("calendar");
    const dated = await query({ ...request, view: "dated" });
    expect(dated.notices).toEqual([]);
    expect(dated.days[13].notes.map((note) => note.path).sort()).toEqual(
      documents.map((document) => document.path).sort(),
    );
    expect(dated.days[13].dailyNotePath).toBe("Daily/2026-10-14.md");
    expect(
      (await withoutMetadata.registration("calendar").query({ ...request, view: "dated" }))
        .days[13].notes.map((note) => note.path),
    ).toEqual(["plugins/Calendar daily note.md"]);
    for (const view of ["created", "updated"] as const) {
      const model = await query({ ...request, view });
      expect(model.notices).toEqual([]);
      expect(model.days.flatMap((day) =>
        day.notes.map((note) => ({ date: day.date, path: note.path })),
      )).toEqual([{
        date: view === "created" ? "2026-10-01" : "2026-10-02",
        path: "plugins/Calendar dated note.md",
      }]);
    }
    await calendar.dispose();
    await withoutMetadata.dispose();
  });

  it("keeps the original Kanban board runnable with three populated columns", async () => {
    const kanban = await activateReleasedPlugin("denote.kanban");
    const model = await kanban.registration("kanbanBoard").parse({
      path: "plugins/Kanban board.kanban.md",
      source: pluginSource("Kanban board.kanban.md"),
    });
    expect(model.error).toBeNull();
    expect(model.columns.map((column) => column.title)).toEqual(["Todo", "In progress", "Done"]);
    expect(model.columns.map((column) => column.cards.length)).toEqual([1, 1, 1]);
    await kanban.dispose();
  });

  it("parses the titled Mermaid decision loop locally", () => {
    const blocks = fromMarkdown(pluginSource("Mermaid flow.md")).children.filter(
      (node): node is Code => node.type === "code" && node.lang === "mermaid",
    );
    expect(blocks).toHaveLength(1);
    expect(blocks[0].value).toMatch(/^%% denote:title: Paper glider trial\n/);
    expect(parseDiagram(blocks[0].value)).toEqual(
      expect.objectContaining({ type: "flowchart" }),
    );
  });

  it("shows the tagged graph chain at the documented local depths", async () => {
    const noteGraph = await activateReleasedPlugin("denote.note-graph");
    const index = noteGraph.registration("noteGraph");
    const documents = [
      "Note graph.md", "Graph sketch.md", "Graph supplies.md", "README.md",
    ].map((name) => ({
      path: `plugins/${name}`,
      title: name,
      source: pluginSource(name),
      tags: extractTags(pluginSource(name)),
    }));
    await index.index({
      mode: "replace", documents, removedPaths: [], skippedCount: 0, truncated: false,
    });
    const query = {
      activePath: "plugins/Note graph.md",
      folder: "plugins",
      tag: "graph-example",
      orphanFilter: "all" as const,
    };
    const global = await index.query({ ...query, scope: "global", depth: 2 });
    expect(global.notices).toEqual([]);
    expect(global.nodes).toHaveLength(3);
    expect(global.edges.map((edge) => [edge.sourceId, edge.targetId]).sort()).toEqual([
      ["plugins/Graph sketch.md", "plugins/Graph supplies.md"],
      ["plugins/Note graph.md", "plugins/Graph sketch.md"],
    ]);
    expect((await index.query({ ...query, scope: "local", depth: 1 })).nodes).toHaveLength(2);
    expect((await index.query({ ...query, scope: "local", depth: 2 })).nodes).toHaveLength(3);
    await noteGraph.dispose();
  });

  it("indexes only the four real task examples and honors their filters", async () => {
    const document = {
      path: "plugins/Advanced tasks.md",
      title: "Advanced tasks",
      source: pluginSource("Advanced tasks.md"),
    };
    const taskLists = await activateReleasedPlugin("denote.task-lists");
    const index = taskLists.registration("taskList");
    await index.index({
      mode: "replace", documents: [document], removedPaths: [], skippedCount: 0, truncated: false,
    });
    // A Markdown parse failure surfaces only as a notice in the public model.
    const parsed = await index.query({
      tag: null, path: "", status: "all", due: "all", today: "2026-10-14", timeZone: "UTC",
    });
    expect(parsed.notices).toEqual([]);
    expect(parsed.totalTasks).toBe(4);
    expect(parsed.tasks).toHaveLength(4);
    expect(
      [...parsed.tasks].sort((left, right) => left.line - right.line)
        .map((task) => task.headingPath),
    ).toEqual([
      ["Advanced tasks", "Prepare"], ["Advanced tasks", "Prepare"],
      ["Advanced tasks", "Trial"], ["Advanced tasks", "Trial"],
    ]);
    const query = {
      tag: "glider", path: document.path, today: "2026-10-14", timeZone: "UTC",
    };
    expect((await index.query({ ...query, status: "all", due: "all" })).tasks).toHaveLength(4);
    expect((await index.query({ ...query, status: "completed", due: "all" })).tasks).toHaveLength(1);
    expect((await index.query({ ...query, status: "open", due: "today" })).tasks).toMatchObject([
      { text: "Fold the first model #glider due:2026-10-14" },
    ]);
    await taskLists.dispose();
  });
});

function pluginSource(name: string): string {
  // Native UTF-8 reads normalize line endings before creating plugin snapshots.
  return readFileSync(join(pluginRoot, name), "utf8").replace(/\r\n?/g, "\n");
}
