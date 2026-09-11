import { describe, expect, it } from "vitest";
import {
  errorBannerOf,
  i18nTextOf,
  messages,
  resolveWorkbenchLocale,
  workbenchMessage,
  type ErrorBannerState,
  type I18nInput,
  type I18nText,
  type WorkbenchLocale,
} from "./i18n";

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

// R5-P2-7：弹层标题/正文存 key + 参数、渲染时求值——切 locale 即时跟随
describe("i18nTextOf (R5-P2-7 弹层文本存 key 惰性求值)", () => {
  const title: I18nText = { key: "newFolderTitle" };

  it("resolves the stored key against the given locale", () => {
    const zh = i18nTextOf(title, "zh-CN");
    const en = i18nTextOf(title, "en");
    expect(zh).toBe(workbenchMessage("zh-CN", "newFolderTitle"));
    expect(en).toBe(workbenchMessage("en", "newFolderTitle"));
    expect(zh).not.toBe(en);
  });

  it("fills values at evaluation time, not storage time", () => {
    const text: I18nText = { key: "deleteTitle", values: { count: 3 } };
    expect(i18nTextOf(text, "zh-CN")).toBe(workbenchMessage("zh-CN", "deleteTitle", { count: 3 }));
  });

  it("joins multi-part bodies with a single space", () => {
    const body: I18nText[] = [{ key: "deleteBody" }, { key: "deleteRecursiveWarn" }];
    expect(i18nTextOf(body, "en")).toBe(`${workbenchMessage("en", "deleteBody")} ${workbenchMessage("en", "deleteRecursiveWarn")}`);
  });

  it("returns an empty string for the unset placeholder ({ key: '' })", () => {
    expect(i18nTextOf({ key: "" }, "zh-CN")).toBe("");
  });
});

// R5-P2-7 收尾（round3）：notice/error 横幅惰性求值——已翻译字符串透传兼容、
// key 形态渲染时求值（横幅存活期间切 locale 即时跟随）。
describe("i18nTextOf string passthrough (notice/error 惰性求值收尾)", () => {
  it("passes already-translated strings through untouched for every locale", () => {
    const raw: I18nInput = "raw translated text";
    expect(i18nTextOf(raw, "zh-CN")).toBe("raw translated text");
    expect(i18nTextOf(raw, "en")).toBe("raw translated text");
  });

  it("joins mixed string/I18nText arrays with a single space", () => {
    expect(i18nTextOf(["raw", { key: "deleted" }], "en")).toBe(`raw ${workbenchMessage("en", "deleted")}`);
  });
});

describe("errorBannerOf (错误横幅存活期间切 locale 跟随)", () => {
  const notFoundFailure: ErrorBannerState = { kind: "failure", detail: "NotFound: /a", friendlyRaw: "NotFound: /a" };

  it("returns an empty string for the cleared banner", () => {
    expect(errorBannerOf("", "zh-CN")).toBe("");
  });

  it("re-evaluates the operationFailed wrapper and the friendly inner per locale on the same stored state", () => {
    const zh = errorBannerOf(notFoundFailure, "zh-CN");
    const en = errorBannerOf(notFoundFailure, "en");
    expect(zh).toBe(workbenchMessage("zh-CN", "operationFailed", { error: workbenchMessage("zh-CN", "errNotFound") }));
    expect(en).toBe(workbenchMessage("en", "operationFailed", { error: workbenchMessage("en", "errNotFound") }));
    expect(zh).not.toBe(en);
  });

  it("passes unknown error text through inside the localized wrapper", () => {
    const raw: ErrorBannerState = { kind: "failure", detail: "weird backend boom", friendlyRaw: "weird backend boom" };
    expect(errorBannerOf(raw, "zh-CN")).toBe(workbenchMessage("zh-CN", "operationFailed", { error: "weird backend boom" }));
  });

  it("evaluates a stored i18n banner per locale", () => {
    const state: ErrorBannerState = { kind: "i18n", text: { key: "nameExists", values: { name: "a.txt" } }, detail: "already exists" };
    expect(errorBannerOf(state, "zh-CN")).toBe(workbenchMessage("zh-CN", "nameExists", { name: "a.txt" }));
    expect(errorBannerOf(state, "en")).toBe(workbenchMessage("en", "nameExists", { name: "a.txt" }));
  });

  it("evaluates nested inner I18nText (destMustDiffer style wrappers) per locale", () => {
    const state: ErrorBannerState = { kind: "failure", detail: "", inner: { key: "destMustDiffer" } };
    expect(errorBannerOf(state, "en")).toBe(workbenchMessage("en", "operationFailed", { error: workbenchMessage("en", "destMustDiffer") }));
    expect(errorBannerOf(state, "ja")).toBe(workbenchMessage("ja", "operationFailed", { error: workbenchMessage("ja", "destMustDiffer") }));
  });

  it("treats failure state without inner/friendlyRaw as an empty inner text", () => {
    expect(errorBannerOf({ kind: "failure", detail: "" }, "en")).toBe(workbenchMessage("en", "operationFailed", { error: "" }));
  });
});
