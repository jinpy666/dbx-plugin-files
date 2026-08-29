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

const prefs: UiPrefs = { sort: { column: "size", direction: "desc" }, dualPane: false, rightTab: "preview" };

describe("ui prefs", () => {
  it("round-trips prefs through storage", () => {
    const storage = memoryStorage();
    saveUiPrefs(prefs, storage);
    expect(storage.getItem(UI_PREFS_KEY)).toContain('"column":"size"');
    expect(loadUiPrefs(storage)).toEqual(prefs);
  });

  it("falls back to defaults for missing or corrupt data", () => {
    expect(loadUiPrefs(memoryStorage())).toEqual({ sort: { column: "name", direction: "asc" }, dualPane: true, rightTab: "target" });
    expect(loadUiPrefs(memoryStorage({ [UI_PREFS_KEY]: "{broken" })).sort.column).toBe("name");
    expect(loadUiPrefs(memoryStorage({ [UI_PREFS_KEY]: '{"dualPane":"yes"}' })).dualPane).toBe(true);
  });

  it("sanitizes unknown sort columns and tabs", () => {
    const storage = memoryStorage({
      [UI_PREFS_KEY]: JSON.stringify({ sort: { column: "hacker", direction: "sideways" }, rightTab: "admin" }),
    });
    const loaded = loadUiPrefs(storage);
    expect(loaded.sort.column).toBe("name");
    expect(loaded.rightTab).toBe("target");
  });
});
