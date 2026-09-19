// rcloneServices（rclone-custom 透传编辑器的建议与示例）契约：
// - RCLONE_BACKEND_TYPES 是 backend type 词表：小写字母/数字、无重复，
//   与 sidecar `custom_rclone_type` 的校验规则一致；
// - datalist 建议去重有序；
// - JSON 示例必须是合法对象草稿。
import { describe, expect, it } from "vitest";
import { CUSTOM_CONFIG_HINTS, customServiceSuggestions, RCLONE_BACKEND_TYPES } from "./rcloneServices";

describe("rcloneServices", () => {
  it("backend type vocabulary is lowercase-alnum and duplicate-free", () => {
    expect(RCLONE_BACKEND_TYPES.size).toBeGreaterThan(50);
    for (const type of RCLONE_BACKEND_TYPES) {
      expect(type).toMatch(/^[a-z0-9]+$/);
    }
  });

  it("covers the custom-pass-through headline backends", () => {
    for (const type of ["b2", "box", "http", "mega", "hdfs", "azurefiles", "protondrive", "memory", "alias"]) {
      expect(RCLONE_BACKEND_TYPES.has(type), type).toBe(true);
    }
  });

  it("datalist suggestions are sorted and unique", () => {
    const suggestions = customServiceSuggestions();
    expect(new Set(suggestions).size).toBe(suggestions.length);
    expect([...suggestions]).toEqual([...suggestions].sort());
    expect(suggestions).toContain("b2");
  });

  it("config hints parse as JSON object drafts", () => {
    for (const [service, hint] of Object.entries(CUSTOM_CONFIG_HINTS)) {
      expect(RCLONE_BACKEND_TYPES.has(service), service).toBe(true);
      const parsed = JSON.parse(hint) as unknown;
      expect(parsed).toBeTypeOf("object");
      expect(parsed).not.toBeNull();
      expect(Array.isArray(parsed)).toBe(false);
    }
  });
});
