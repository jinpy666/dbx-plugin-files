// 「用而未声明」防线：组件里 t("...") 字面量键必须存在于七语字典。
// i18n.spec 钉住「字典内部七语齐平」，但测不出「模板用了、字典没有」——
// 那类缺口会直接把原始 key 渲染进 UI（workbenchMessage 的兜底就是 `|| key`）。
// 本测试扫描 frontend/src 全部 ts/vue 源码的 t('...') / t("...") 字面量，
// 断言其 ⊆ en 字典键集合（其余六语由 i18n.spec 的齐平测试保证同集合）。
// 动态键（t(`transferKind.${job.kind}`) 这类模板串）不在扫描范围，
// 由对应命名空间键 + 七语齐平测试兜底。
// 源码经 vite 的 ?raw glob 内联（frontend 无 @types/node，不用 node:fs）。

import { describe, expect, it } from "vitest";
import { messages } from "./i18n";

function flatten(prefix: string, value: unknown, out: Set<string>): Set<string> {
  if (value !== null && typeof value === "object") {
    for (const [key, child] of Object.entries(value as Record<string, unknown>)) {
      flatten(prefix ? `${prefix}.${key}` : key, child, out);
    }
  } else {
    out.add(prefix);
  }
  return out;
}

const sources = import.meta.glob("/src/**/*.{ts,vue}", {
  query: "?raw",
  import: "default",
  eager: true,
}) as Record<string, string>;

describe("every literal t() key exists in the dictionary", () => {
  it("source t(\"...\") literals ⊆ en key set", () => {
    const dictionary = flatten("", messages.en, new Set<string>());
    const files = Object.entries(sources).filter(([path]) => !path.endsWith(".spec.ts"));
    expect(files.length).toBeGreaterThan(50);

    const missing: string[] = [];
    // t('key') / t("key")，含模板属性里的 v-tip="t('key')"、:placeholder="t('key')"。
    const literal = /\bt\(\s*(['"])([^'"\n`]+)\1/g;
    for (const [path, content] of files) {
      for (const match of content.matchAll(literal)) {
        const key = match[2];
        if (!dictionary.has(key)) {
          missing.push(`${path}:${key}`);
        }
      }
    }
    expect(missing, `keys used in components but missing from i18n dictionary:\n${missing.join("\n")}`).toEqual([]);
  });
});
