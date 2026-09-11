import { describe, expect, it } from "vitest";
import { loadUiPrefs, saveUiPrefs, UI_PREFS_KEY, type UiPrefs } from "./prefs";

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

const prefs: UiPrefs = { sort: { column: "size", direction: "desc" }, sideTab: "quick", sideCollapsed: true };

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
      sideTab: "tree",
      sideCollapsed: false,
    });
    expect(loadUiPrefs(memoryStorage({ [UI_PREFS_KEY]: "{broken" })).sort.column).toBe("name");
  });

  it("sanitizes unknown sort columns and legacy fields", () => {
    const storage = memoryStorage({
      [UI_PREFS_KEY]: JSON.stringify({ sort: { column: "hacker", direction: "sideways" }, rightTab: "admin", sideTab: "magic" }),
    });
    const loaded = loadUiPrefs(storage);
    expect(loaded.sort.column).toBe("name");
    expect(loaded.sideTab).toBe("tree");
    expect("rightTab" in loaded).toBe(false);
  });

  it("ignores legacy dualPane in stored prefs (session-only toggle now)", () => {
    // 历史版本持久化过 dualPane；新版本不读不写，加载结果恒不含该字段。
    const storage = memoryStorage({ [UI_PREFS_KEY]: '{"dualPane":true,"sort":{"column":"name","direction":"asc"}}' });
    const loaded = loadUiPrefs(storage);
    expect("dualPane" in loaded).toBe(false);
  });
});
