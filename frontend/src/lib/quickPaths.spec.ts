import { describe, expect, it } from "vitest";
import { normalizeQuickPaths, quickPathLabelKey, type QuickPath } from "./quickPaths";

describe("normalizeQuickPaths", () => {
  it("keeps known keys with a path, preserving backend order", () => {
    const paths: QuickPath[] = [
      { key: "root", path: "/" },
      { key: "home", path: "/Users/jin" },
      { key: "desktop", path: "/Users/jin/Desktop" },
      { key: "downloads", path: "/Users/jin/Downloads" },
    ];
    expect(normalizeQuickPaths(paths)).toEqual(paths);
  });

  it("drops unknown keys and empty paths", () => {
    expect(
      normalizeQuickPaths([
        { key: "vendor", path: "/x" },
        { key: "home", path: "" },
        { key: "documents", path: "/Users/jin/Documents" },
      ]),
    ).toEqual([{ key: "documents", path: "/Users/jin/Documents" }]);
  });

  it("tolerates missing payload", () => {
    expect(normalizeQuickPaths(undefined)).toEqual([]);
    expect(normalizeQuickPaths(null)).toEqual([]);
  });
});

describe("quickPathLabelKey", () => {
  it("maps known keys and falls back to root for unknown keys", () => {
    expect(quickPathLabelKey("downloads")).toBe("quickDownloads");
    expect(quickPathLabelKey("whatever")).toBe("quickRoot");
  });
});
