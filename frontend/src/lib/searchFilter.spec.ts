import { describe, expect, it } from "vitest";
import { filterEntries } from "./searchFilter";
import type { FileEntry } from "./api";

const entries: FileEntry[] = [
  { name: "Report.PDF", path: "/data/Report.PDF", kind: "file", size: 10 },
  { name: "notes.txt", path: "/data/notes.txt", kind: "file", size: 2 },
  { name: "assets", path: "/data/assets", kind: "directory" },
];

describe("filterEntries", () => {
  it("keeps everything when the query is empty or blank", () => {
    expect(filterEntries(entries, "")).toEqual(entries);
    expect(filterEntries(entries, "   ")).toEqual(entries);
  });

  it("matches names case-insensitively as a substring", () => {
    expect(filterEntries(entries, "report").map((entry) => entry.name)).toEqual(["Report.PDF"]);
    expect(filterEntries(entries, "PDF").map((entry) => entry.name)).toEqual(["Report.PDF"]);
    expect(filterEntries(entries, "txt")).toEqual([entries[1]]);
  });

  it("matches directories too and returns nothing for misses", () => {
    expect(filterEntries(entries, "asset").map((entry) => entry.name)).toEqual(["assets"]);
    expect(filterEntries(entries, "没有的文件")).toEqual([]);
  });
});
