// @vitest-environment node
import { join, resolve } from "node:path";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { spawnSync } from "node:child_process";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  lockfileOnlyIgnores,
  projectVendoredSources,
  runCargoAudit,
  verifyBackportTestWorkspace,
  verifyResolvedBackports,
} from "./audit-rust-dependencies.mjs";

vi.mock("node:child_process", async (importOriginal) => ({
  ...await importOriginal(),
  spawnSync: vi.fn(),
}));

const root = resolve("synthetic-native");
const versions = {
  glib: "0.18.5",
  "glib-macros": "0.18.5",
  "gtk3-macros": "0.18.2",
  "tauri-plugin-notification": "2.4.0",
  "tauri-plugin-opener": "2.5.5",
  "tauri-plugin-single-instance": "2.4.5",
  urlpattern: "0.3.0",
};

function metadata(workspace = root) {
  return {
    packages: [
      {
        id: "synthetic-application",
        name: "denote",
        manifest_path: join(workspace, "Cargo.toml"),
      },
      {
        id: "synthetic-speech-helper",
        name: "denote-speech-worker",
        version: "0.1.0",
        source: null,
        manifest_path: join(workspace, "speech-worker", "Cargo.toml"),
      },
      ...Object.entries(versions).map(([name, version]) => ({
        id: `synthetic-${name}`,
        name,
        version,
        source: null,
        manifest_path: join(workspace, "vendor", name, "Cargo.toml"),
      })),
    ],
    workspace_members: ["synthetic-application", "synthetic-speech-helper"],
    workspace_default_members: ["synthetic-application"],
  };
}

const registry = "registry+https://github.com/rust-lang/crates.io-index";

function testMetadata() {
  const input = metadata();
  input.packages = input.packages.filter((entry) => entry.name in versions);
  input.packages.push({ id: "synthetic-tauri", name: "tauri", version: "2.11.6", source: registry });
  input.workspace_members = input.packages.filter((entry) => entry.source === null).map((entry) => entry.id);
  return input;
}

function shippedMetadata() {
  const input = metadata();
  input.packages.push({ id: "synthetic-tauri", name: "tauri", version: "2.11.6", source: registry });
  return input;
}

function lockfile() {
  return `version = 4\n\n${Object.entries(versions)
    .map(([name, version]) => `[[package]]\nname = "${name}"\nversion = "${version}"\n`)
    .join("\n")}`;
}

describe("Rust source-backport audit", () => {
  it("requires the reviewed local packages and application-only default", () => {
    expect(() => verifyResolvedBackports(metadata(), root)).not.toThrow();
  });

  it.each([
    ["registry source", (entry) => { entry.source = "registry+https://github.com/rust-lang/crates.io-index"; }],
    ["Git source", (entry) => { entry.source = "git+https://example.invalid/synthetic"; }],
    ["unreviewed version", (entry) => { entry.version = "9.9.9"; }],
    ["unreviewed path", (entry) => { entry.manifest_path = join(root, "unapproved", "Cargo.toml"); }],
  ])("rejects a speech helper with %s", (_label, change) => {
    const input = metadata();
    change(input.packages.find((entry) => entry.name === "denote-speech-worker"));
    expect(() => verifyResolvedBackports(input, root)).toThrow(/speech helper/i);
  });

  it("requires one reviewed helper without making it a default workspace member", () => {
    const missing = metadata();
    missing.packages = missing.packages.filter((entry) => entry.name !== "denote-speech-worker");
    expect(() => verifyResolvedBackports(missing, root)).toThrow(/speech helper/i);
    const duplicate = metadata();
    duplicate.packages.push({ ...duplicate.packages.find((entry) => entry.name === "denote-speech-worker") });
    expect(() => verifyResolvedBackports(duplicate, root)).toThrow(/speech helper/i);
    const defaults = metadata();
    defaults.workspace_default_members.push("synthetic-speech-helper");
    expect(() => verifyResolvedBackports(defaults, root)).toThrow(/default/i);
  });

  it("keeps backports patch-only so dependency automation cannot edit their manifests", () => {
    const input = metadata();
    input.workspace_members.push("synthetic-glib");
    expect(() => verifyResolvedBackports(input, root)).toThrow(/patch-only/);
  });

  describe("backport test workspace", () => {
    it("accepts only the reviewed backports with the application's runtime family", () => {
      expect(() => verifyBackportTestWorkspace(testMetadata(), shippedMetadata(), root)).not.toThrow();
    });

    it.each([
      ["a missing member", (input) => { input.workspace_members.shift(); }, /glib/],
      ["an extra member", (input) => { input.workspace_members.push("synthetic-tauri"); }, /only the reviewed/],
      ["a registry backport", (input) => { input.packages[0].source = registry; }, /glib/],
      ["a Git dependency", (input) => {
        input.packages.push({ id: "synthetic-git", name: "synthetic-git", source: "git+https://example.invalid/synthetic" });
      }, /Unreviewed/],
      ["a different runtime version", (input) => { input.packages.at(-1).version = "2.12.7"; }, /tauri runtime/],
    ])("rejects %s", (_label, change, message) => {
      const input = testMetadata();
      change(input);
      expect(() => verifyBackportTestWorkspace(input, shippedMetadata(), root)).toThrow(message);
    });
  });

  describe("lockfile-only advisories", () => {
    beforeEach(() => {
      vi.spyOn(console, "log").mockImplementation(() => {});
    });

    afterEach(() => {
      vi.restoreAllMocks();
    });

    const chrono = '[[package]]\nname = "chrono"\nversion = "0.4.45"\n';

    it("ignores the chrono advisory only while no workspace compiles chrono", () => {
      const isCompiled = vi.fn(() => false);
      expect(lockfileOnlyIgnores(lockfile() + chrono, isCompiled)).toEqual(["RUSTSEC-2026-0543"]);
      expect(isCompiled).toHaveBeenCalledWith("chrono");
      expect(console.log).toHaveBeenCalledWith(expect.stringMatching(/Ignoring RUSTSEC-2026-0543/));
    });

    it("keeps the advisory denied once chrono is compiled", () => {
      expect(lockfileOnlyIgnores(lockfile() + chrono, () => true)).toEqual([]);
    });

    it("does not add an exception for a lockfile without chrono", () => {
      const isCompiled = vi.fn();
      expect(lockfileOnlyIgnores(lockfile(), isCompiled)).toEqual([]);
      expect(isCompiled).not.toHaveBeenCalled();
    });
  });

  describe("required Rust registry checks", () => {
    beforeEach(() => {
      vi.mocked(spawnSync).mockReset();
      vi.spyOn(process.stdout, "write").mockImplementation(() => true);
      vi.spyOn(process.stderr, "write").mockImplementation(() => true);
    });

    afterEach(() => {
      vi.restoreAllMocks();
    });

    function result(overrides = {}) {
      return {
        status: 0,
        signal: null,
        stdout: "",
        stderr: "    Updating crates.io index\n    Scanning synthetic.lock for vulnerabilities\n",
        ...overrides,
      };
    }

    it("fetches registry metadata for the published identities and denies yanked releases", () => {
      vi.mocked(spawnSync).mockReturnValue(result());
      const projected = projectVendoredSources(lockfile());

      runCargoAudit("-", projected);

      expect(spawnSync).toHaveBeenCalledWith("cargo", [
        "audit", "--file", "-", "--color", "never",
        "--deny", "unmaintained", "--deny", "yanked",
      ], expect.objectContaining({ input: projected, encoding: "utf8" }));
    });

    it("passes only the requested lockfile-only exceptions to cargo-audit", () => {
      vi.mocked(spawnSync).mockReturnValue(result());

      runCargoAudit("synthetic.lock", undefined, ["RUSTSEC-2026-0543"]);

      expect(spawnSync.mock.calls[0][1].slice(-2)).toEqual(["--ignore", "RUSTSEC-2026-0543"]);
    });

    it("keeps the version-based unsoundness warning visible without ignoring its advisory", () => {
      const diagnostic = "Crate: glib\nWarning: unsound\nID: RUSTSEC-2024-0429\n" +
        "warning: 1 allowed warning found\n";
      vi.mocked(spawnSync).mockReturnValue(result({ stderr: diagnostic }));

      expect(() => runCargoAudit("synthetic.lock")).not.toThrow();

      expect(process.stderr.write).toHaveBeenCalledWith(diagnostic);
      expect(spawnSync.mock.calls[0][1]).not.toContain("--ignore");
    });

    it.each([
      "error: couldn't check if the package is yanked: No such crate: synthetic-crate",
      "error: couldn't check if the package is yanked: No such version: synthetic-crate 1.2.3",
      "warning: couldn't update crates.io index: synthetic network failure",
      "warning: couldn't open crates.io index: synthetic cache failure",
      "error: Failed to download crates.io index",
      "warning: synthetic required check could not run",
      "\u001b[31merror:\u001b[0m couldn't check if the package is yanked: synthetic failure",
    ])("rejects an incomplete check even when cargo-audit returns zero: %s", (diagnostic) => {
      vi.mocked(spawnSync).mockReturnValue(result({ stderr: `${diagnostic}\n` }));

      expect(() => runCargoAudit("synthetic.lock")).toThrow(/incomplete/i);
      expect(process.stderr.write).toHaveBeenCalledWith(`${diagnostic}\n`);
    });

    it("also rejects required-check errors printed on stdout", () => {
      vi.mocked(spawnSync).mockReturnValue(result({
        stdout: "error: synthetic registry lookup failed\n",
      }));

      expect(() => runCargoAudit("synthetic.lock")).toThrow(/incomplete/i);
    });

    it.each([
      ["denied yanked release", { status: 1, stderr: "warning: 1 denied warning found\n" }, /cargo audit failed/],
      ["missing cargo executable", { status: null, error: new Error("synthetic ENOENT") }, /Could not run cargo audit/],
      ["terminated audit", { status: null, signal: "SIGTERM" }, /cargo audit terminated by SIGTERM/],
      ["audit timeout", { status: null, error: new Error("synthetic ETIMEDOUT") }, /Could not run cargo audit/],
    ])("fails explicitly for a %s", (_label, outcome, message) => {
      vi.mocked(spawnSync).mockReturnValue(result(outcome));

      expect(() => runCargoAudit("synthetic.lock")).toThrow(message);
    });
  });

  it("rejects inactive vendor lockfiles that can retain an unaudited dependency graph", () => {
    const workspace = mkdtempSync(join(tmpdir(), "denote-synthetic-audit-"));
    try {
      const vendor = join(workspace, "vendor", "glib");
      mkdirSync(vendor, { recursive: true });
      writeFileSync(join(vendor, "Cargo.lock"), lockfile());
      expect(() => verifyResolvedBackports(metadata(workspace), workspace))
        .toThrow(/workspace Cargo.lock/);
    } finally {
      rmSync(workspace, { recursive: true, force: true });
    }
  });

  it.each(Object.keys(versions))("rejects a registry bypass for %s", (name) => {
    const input = metadata();
    input.packages.find((entry) => entry.name === name).source =
      "registry+https://github.com/rust-lang/crates.io-index";
    expect(() => verifyResolvedBackports(input, root)).toThrow(name);
  });

  it.each([
    ["registry source", (entry) => { entry.source = "registry+https://github.com/rust-lang/crates.io-index"; }],
    ["different version", (entry) => { entry.version = "0.20.0"; }],
    ["unapproved path", (entry) => { entry.manifest_path = join(root, "unapproved", "Cargo.toml"); }],
  ])("rejects a backport with %s", (_label, change) => {
    const input = metadata();
    change(input.packages.find((entry) => entry.name === "glib"));
    expect(() => verifyResolvedBackports(input, root)).toThrow(/glib/);
  });

  it("rejects missing or duplicate packages rather than auditing a detached test copy", () => {
    const missing = metadata();
    missing.packages = missing.packages.filter((entry) => entry.name !== "glib");
    expect(() => verifyResolvedBackports(missing, root)).toThrow(/glib/);

    const duplicate = metadata();
    duplicate.packages.push({ ...duplicate.packages.find((entry) => entry.name === "glib") });
    expect(() => verifyResolvedBackports(duplicate, root)).toThrow(/glib/);
  });

  it("does not make GTK a default macOS or Windows build requirement", () => {
    const input = metadata();
    input.workspace_default_members.push("synthetic-glib");
    expect(() => verifyResolvedBackports(input, root)).toThrow(/default/);
  });

  it.each([null, "git+https://example.invalid/synthetic"])(
    "rejects unreviewed dependency sources instead of silently skipping advisories: %s",
    (source) => {
      const input = metadata();
      input.packages.push({ id: "synthetic-extra", name: "synthetic-extra", source });
      expect(() => verifyResolvedBackports(input, root)).toThrow(/Unreviewed dependency source/);
    },
  );

  it("normalizes Cargo manifest separators on Windows", () => {
    const input = metadata();
    for (const entry of input.packages) {
      entry.manifest_path = entry.manifest_path.replaceAll("\\", "/");
    }
    expect(() => verifyResolvedBackports(input, root)).not.toThrow();
  });

  it("projects original registry identities without changing names, versions, or dependencies", () => {
    const dependency = '\n[[package]]\nname = "synthetic-dependency"\nversion = "1.2.3"\nsource = "registry+https://github.com/rust-lang/crates.io-index"\n';
    const input = lockfile() + dependency;
    const projected = projectVendoredSources(input);
    for (const [name, version] of Object.entries(versions)) {
      expect(projected).toContain(
        `name = "${name}"\nversion = "${version}"\nsource = "registry+https://github.com/rust-lang/crates.io-index"\nchecksum = "`,
      );
    }
    expect(projected).toContain(dependency);
    expect(input).toBe(lockfile() + dependency);
  });

  it("supports Windows line endings", () => {
    expect(projectVendoredSources(lockfile().replaceAll("\n", "\r\n")))
      .toBe(projectVendoredSources(lockfile()));
  });

  it("refuses missing, relabelled, duplicated, or non-local lockfile records", () => {
    expect(() => projectVendoredSources(lockfile().replace('name = "glib"', 'name = "missing"')))
      .toThrow(/glib/);
    expect(() => projectVendoredSources(lockfile().replace('version = "0.18.5"', 'version = "0.20.0"')))
      .toThrow(/glib/);
    expect(() => projectVendoredSources(lockfile() + '\n[[package]]\nname = "glib"\nversion = "0.18.5"\n'))
      .toThrow(/glib/);
    expect(() => projectVendoredSources(lockfile().replace('name = "glib"', 'name = "glib"\nsource = "git+https://example.invalid/synthetic"')))
      .toThrow(/glib/);
  });
});
