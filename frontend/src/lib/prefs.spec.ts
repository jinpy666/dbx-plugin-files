import { describe, expect, it } from "vitest";
import { loadDownloadDir, loadUiPrefs, persistDownloadDir, saveUiPrefs, DOWNLOAD_DIR_KEY, UI_PREFS_KEY, type UiPrefs } from "./prefs";

function brokenStorage(): Storage {
  const unavailable = () => {
    throw new Error("unavailable");
  };
  return {
    get length(): number {
      throw new Error("unavailable");
    },
    clear: unavailable,
    getItem: unavailable,
    key: unavailable,
    removeItem: unavailable,
    setItem: unavailable,
  } as Storage;
}

function memoryStorage(initial: Record<string, string> = {}): Storage {
  const map = new Map(Object.entries(initial));
  return {
    get length() {
      return map.size;
    },
    clear: () => map.clear(),
    getItem: (key) => map.get(key) ?? null,
    key: (index) => [...map.keys()][index] ?? null,
    removeItem: (key) => map.delete(key),
    setItem: (key, value) => map.set(key, String(value)),
  } as Storage;
}

const prefs: UiPrefs = {
  sort: { column: "size", direction: "desc" },
  leftSideTab: "quick",
  rightSideTab: "tree",
  leftSideCollapsed: true,
  rightSideCollapsed: false,
};

describe("ui prefs", () => {
  it("round-trips prefs through storage", () => {
    const storage = memoryStorage();
    saveUiPrefs(prefs, storage);
    expect(storage.getItem(UI_PREFS_KEY)).toContain('"column":"size"');
    expect(storage.getItem(UI_PREFS_KEY)).not.toContain("dualPane");
    expect(loadUiPrefs(storage)).toEqual(prefs);
  });

  it("falls back to defaults for missing or corrupt data", () => {
    expect(loadUiPrefs(memoryStorage())).toEqual({
      sort: { column: "name", direction: "asc" },
      leftSideTab: "quick",
      rightSideTab: "tree",
      leftSideCollapsed: false,
      rightSideCollapsed: false,
    });
    expect(loadUiPrefs(memoryStorage({ [UI_PREFS_KEY]: "{broken" })).sort.column).toBe("name");
  });

  it("sanitizes unknown sort columns and legacy fields", () => {
    const storage = memoryStorage({
      [UI_PREFS_KEY]: JSON.stringify({ sort: { column: "hacker", direction: "sideways" }, rightTab: "admin", sideTab: "magic" }),
    });
    const loaded = loadUiPrefs(storage);
    expect(loaded.sort.column).toBe("name");
    expect(loaded.leftSideTab).toBe("quick");
    expect(loaded.rightSideTab).toBe("tree");
    expect("rightTab" in loaded).toBe(false);
  });

  it("ignores legacy dualPane in stored prefs (session-only toggle now)", () => {
    // 历史版本持久化过 dualPane；新版本不读不写，加载结果恒不含该字段。
    const storage = memoryStorage({ [UI_PREFS_KEY]: '{"dualPane":true,"sort":{"column":"name","direction":"asc"}}' });
    const loaded = loadUiPrefs(storage);
    expect("dualPane" in loaded).toBe(false);
  });
});

describe("download dir preference (对标 ssh downloadDir)", () => {
  it("round-trips a trimmed directory through storage", () => {
    const storage = memoryStorage();
    persistDownloadDir("  /Users/me/Downloads  ", storage);
    expect(storage.getItem(DOWNLOAD_DIR_KEY)).toBe("/Users/me/Downloads");
    expect(loadDownloadDir(storage)).toBe("/Users/me/Downloads");
  });

  it("empty value clears the preference (back to default dir)", () => {
    const storage = memoryStorage({ [DOWNLOAD_DIR_KEY]: "/old" });
    persistDownloadDir("", storage);
    expect(storage.getItem(DOWNLOAD_DIR_KEY)).toBeNull();
    expect(loadDownloadDir(storage)).toBe("");
  });

  it("missing key and unavailable storage fall back to empty string", () => {
    expect(loadDownloadDir(memoryStorage())).toBe("");
    expect(loadDownloadDir(brokenStorage())).toBe("");
    expect(() => persistDownloadDir("/x", brokenStorage())).not.toThrow();
  });
});
