import { describe, expect, it } from "vitest";
import { messages, resolveWorkbenchLocale, workbenchMessage, type WorkbenchLocale } from "./i18n";

const LOCALES = Object.keys(messages) as WorkbenchLocale[];
const EXPECTED_LOCALES: WorkbenchLocale[] = ["en", "es", "it", "ja", "pt-BR", "zh-CN", "zh-TW"];

/** 递归收集字典键（嵌套对象用点路径展开）。 */
function keyPaths(node: unknown, prefix = ""): string[] {
  if (!node || typeof node !== "object") return [];
  return Object.entries(node as Record<string, unknown>).flatMap(([key, value]) => {
    const path = prefix ? `${prefix}.${key}` : key;
    return value && typeof value === "object" ? keyPaths(value, path) : [path];
  });
}

describe("i18n alignment", () => {
  it("exposes exactly the seven supported locales", () => {
    expect([...LOCALES].sort()).toEqual([...EXPECTED_LOCALES].sort());
  });

  it("has identical key sets across all locales", () => {
    const reference = keyPaths(messages.en).sort();
    expect(reference.length).toBeGreaterThan(50);
    for (const locale of LOCALES) {
      expect(keyPaths(messages[locale]).sort(), `locale ${locale} key set diverges`).toEqual(reference);
    }
  });

  it("keeps placeholder names identical across locales", () => {
    for (const locale of LOCALES) {
      for (const key of keyPaths(messages[locale])) {
        const en = workbenchMessage("en", key);
        const translated = workbenchMessage(locale, key);
        const placeholders = (value: string) => [...value.matchAll(/\{(\w+)\}/g)].map((match) => match[1]).sort().join(",");
        expect(placeholders(translated), `locale ${locale} key ${key} placeholders`).toBe(placeholders(en));
        expect(translated).not.toBe(key);
      }
    }
  });

  it("resolves locale fallbacks", () => {
    expect(resolveWorkbenchLocale("zh-TW")).toBe("zh-TW");
    expect(resolveWorkbenchLocale("zh-HK")).toBe("zh-TW");
    expect(resolveWorkbenchLocale("pt")).toBe("pt-BR");
    expect(resolveWorkbenchLocale("fr")).toBe("en");
  });

  it("fills values into placeholders", () => {
    expect(workbenchMessage("zh-CN", "entriesCount", { count: 3 })).toBe("3 项");
    expect(workbenchMessage("en", "paneTransferred", { count: 2 })).toContain("2");
  });
});
