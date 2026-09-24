import { describe, expect, it } from "vitest";
import { groupByRemoteDir, nextAvailableName, resolveUploadNames, sanitizeConflictPolicy, type UploadQueueItem } from "./conflictPolicy";

describe("sanitizeConflictPolicy", () => {
  it("keeps known policies and defaults everything else to ask", () => {
    expect(sanitizeConflictPolicy("ask")).toBe("ask");
    expect(sanitizeConflictPolicy("rename")).toBe("rename");
    expect(sanitizeConflictPolicy("overwrite")).toBe("overwrite");
    expect(sanitizeConflictPolicy("bogus")).toBe("ask");
    expect(sanitizeConflictPolicy(undefined)).toBe("ask");
  });
});

describe("nextAvailableName", () => {
  it("appends (n) before the extension, skipping taken names", () => {
    expect(nextAvailableName("report.pdf", new Set(["report (1).pdf"]))).toBe("report (2).pdf");
    expect(nextAvailableName("report.pdf", new Set())).toBe("report (1).pdf");
  });

  it("treats dotfiles and extensionless names as one stem", () => {
    expect(nextAvailableName(".hidden", new Set())).toBe(".hidden (1)");
    expect(nextAvailableName("archive", new Set(["archive (1)"]))).toBe("archive (2)");
  });

  it("registers the picked name so batch renames never collide", () => {
    const taken = new Set<string>();
    const first = nextAvailableName("log.txt", taken);
    const second = nextAvailableName("log.txt", taken);
    expect(first).toBe("log (1).txt");
    expect(second).toBe("log (2).txt");
  });
});

describe("resolveUploadNames", () => {
  const item = (name: string): UploadQueueItem => ({ name, size: 1, readChunk: async () => new Uint8Array() });

  it("passes everything through in overwrite mode", () => {
    const items = [item("a.txt"), item("b.txt")];
    const resolved = resolveUploadNames(items, new Set(["a.txt"]), "overwrite");
    expect(resolved.map((entry) => entry.name)).toEqual(["a.txt", "b.txt"]);
    expect(resolved[0]).toBe(items[0]);
  });

  it("renames only conflicting files and avoids batch self-collisions", () => {
    const items = [item("a.txt"), item("b.txt"), item("a (1).txt")];
    const resolved = resolveUploadNames(items, new Set(["a.txt"]), "rename");
    expect(resolved.map((entry) => entry.name)).toEqual(["a (2).txt", "b.txt", "a (1).txt"]);
  });

  it("leaves non-conflicting batches untouched", () => {
    const items = [item("x.txt")];
    const resolved = resolveUploadNames(items, new Set(["a.txt"]), "rename");
    expect(resolved.map((entry) => entry.name)).toEqual(["x.txt"]);
  });
});

describe("groupByRemoteDir", () => {
  const item = (name: string, remoteDir?: string): UploadQueueItem => ({ name, remoteDir, size: 1, readChunk: async () => new Uint8Array() });

  it("groups by remoteDir with the flat upload batch under the root key", () => {
    const groups = groupByRemoteDir([item("a.txt"), item("b.txt")]);
    expect([...groups.keys()]).toEqual([""]);
    expect(groups.get("")!.map((entry) => entry.name)).toEqual(["a.txt", "b.txt"]);
  });

  it("keeps folder-upload subdirectories in separate conflict groups", () => {
    const groups = groupByRemoteDir([
      item("a.txt", "docs"),
      item("b.txt", "docs/sub"),
      item("c.txt", "docs"),
    ]);
    expect([...groups.keys()].sort()).toEqual(["docs", "docs/sub"]);
    expect(groups.get("docs")!.map((entry) => entry.name)).toEqual(["a.txt", "c.txt"]);
    expect(groups.get("docs/sub")!.map((entry) => entry.name)).toEqual(["b.txt"]);
  });
});
