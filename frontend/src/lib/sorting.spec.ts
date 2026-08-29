import { describe, expect, it } from "vitest";
import { compareEntries, sortEntries, toggleSortState, type SortState } from "./sorting";
import type { FileEntry } from "./api";

function entry(overrides: Partial<FileEntry>): FileEntry {
  return { name: "x", path: "/x", kind: "file", ...overrides };
}

const state = (column: SortState["column"], direction: SortState["direction"]): SortState => ({ column, direction });

describe("sorting", () => {
  it("toggles direction on the same column and resets on a new column", () => {
    expect(toggleSortState(state("name", "asc"), "name")).toEqual(state("name", "desc"));
    expect(toggleSortState(state("name", "desc"), "name")).toEqual(state("name", "asc"));
    expect(toggleSortState(state("name", "desc"), "size")).toEqual(state("size", "asc"));
  });

  it("always keeps directories before files", () => {
    const sorted = sortEntries(
      [entry({ name: "z", path: "/z" }), entry({ name: "a", path: "/a", kind: "directory" })],
      state("name", "desc"),
    );
    expect(sorted.map((item) => item.name)).toEqual(["a", "z"]);
  });

  it("compares by name", () => {
    const files = [entry({ name: "b", path: "/b" }), entry({ name: "a", path: "/a" }), entry({ name: "c", path: "/c" })];
    expect(sortEntries(files, state("name", "asc")).map((item) => item.name)).toEqual(["a", "b", "c"]);
    expect(sortEntries(files, state("name", "desc")).map((item) => item.name)).toEqual(["c", "b", "a"]);
  });

  it("compares by size with missing sizes treated as zero", () => {
    const files = [entry({ name: "a", size: 30 }), entry({ name: "b", size: 10 }), entry({ name: "c" })];
    expect(sortEntries(files, state("size", "asc")).map((item) => item.name)).toEqual(["c", "b", "a"]);
    expect(sortEntries(files, state("size", "desc")).map((item) => item.name)).toEqual(["a", "b", "c"]);
  });

  it("compares by modified time", () => {
    const files = [
      entry({ name: "old", modifiedAt: "2024-01-01T00:00:00Z" }),
      entry({ name: "new", modifiedAt: "2025-06-01T00:00:00Z" }),
      entry({ name: "unknown" }),
    ];
    expect(sortEntries(files, state("modified", "asc")).map((item) => item.name)).toEqual(["unknown", "old", "new"]);
    expect(sortEntries(files, state("modified", "desc")).map((item) => item.name)).toEqual(["new", "old", "unknown"]);
  });

  it("does not mutate the input array", () => {
    const files = [entry({ name: "b" }), entry({ name: "a" })];
    sortEntries(files, state("name", "asc"));
    expect(files.map((item) => item.name)).toEqual(["b", "a"]);
    expect(compareEntries(entry({ name: "a" }), entry({ name: "b" }), "name", "asc")).toBeLessThan(0);
  });
});
