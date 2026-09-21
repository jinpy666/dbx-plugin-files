// 批量重命名计划计算单测（纯函数层）：字面/正则/忽略大小写替换、前后缀、
// 序号、行级错误（空名/计划重复/目录已存在）与非法正则整体降级。
import { describe, expect, it } from "vitest";
import { buildRenamePlan, transformName, type RenamePlanOptions } from "./batchRename";
import type { FileEntry } from "./api";

function entry(name: string): FileEntry {
  return { name, path: `/dir/${name}`, kind: "file" };
}

const base: RenamePlanOptions = {
  find: "",
  replace: "",
  regex: false,
  ignoreCase: false,
  prefix: "",
  suffix: "",
  numbering: false,
  numberingStart: 1,
};

describe("transformName", () => {
  it("performs literal find/replace", () => {
    expect(transformName("report-2024.txt", { ...base, find: "report", replace: "summary" }, 1)).toBe("summary-2024.txt");
  });

  it("replaces case-insensitively while keeping surrounding case", () => {
    expect(transformName("IMG-2024.jpg", { ...base, find: "img", replace: "photo", ignoreCase: true }, 1)).toBe("photo-2024.jpg");
    expect(transformName("IMG-2024.jpg", { ...base, find: "img", replace: "photo" }, 1)).toBe("IMG-2024.jpg");
  });

  it("applies regex patterns with global flag and optional ignore-case", () => {
    expect(transformName("q3-2024", { ...base, find: "-\\d{4}", replace: "_year", regex: true }, 1)).toBe("q3_year");
    expect(transformName("Q3-report", { ...base, find: "q3", replace: "x", regex: true, ignoreCase: true }, 1)).toBe("x-report");
  });

  it("appends prefix, suffix and zero-padded sequence numbers", () => {
    expect(transformName("a.txt", { ...base, prefix: "pre-", suffix: "-bak" }, 7)).toBe("pre-a.txt-bak");
    expect(transformName("a.txt", { ...base, numbering: true, numberingStart: 7 }, 7)).toBe("a.txt-007");
    // 非法起始值按 0 兜底，不产生 NaN 序号。
    expect(transformName("a.txt", { ...base, numbering: true, numberingStart: Number.NaN }, Number.NaN)).toBe("a.txt-000");
  });
});

describe("buildRenamePlan", () => {
  it("marks unchanged rows as not changed and counts applicable rows", () => {
    const plan = buildRenamePlan([entry("a.txt"), entry("b.md")], { ...base, find: "txt", replace: "log" });
    expect(plan.rows.map((row) => row.changed)).toEqual([true, false]);
    expect(plan.applicable).toBe(1);
  });

  it("flags empty new names", () => {
    const plan = buildRenamePlan([entry("a.txt")], { ...base, find: "a.txt", replace: "" });
    expect(plan.rows[0].newName).toBe("");
    expect(plan.rows[0].error).toBe("batchRenameErrorEmpty");
  });

  it("flags duplicates among planned names (second occurrence loses)", () => {
    const plan = buildRenamePlan([entry("a.txt"), entry("b.txt")], { ...base, find: "a|b", replace: "x", regex: true });
    expect(plan.rows[0].newName).toBe("x.txt");
    expect(plan.rows[0].error).toBe("");
    expect(plan.rows[1].error).toBe("batchRenameErrorDuplicate");
  });

  it("flags names that already exist in the directory", () => {
    const plan = buildRenamePlan([entry("a.txt"), entry("b.txt")], { ...base, find: "a", replace: "b" });
    expect(plan.rows[0].error).toBe("batchRenameErrorExists");
    expect(plan.applicable).toBe(0);
  });

  it("degrades to unchanged rows on an invalid regex", () => {
    const plan = buildRenamePlan([entry("a.txt")], { ...base, find: "(bad", replace: "x", regex: true });
    expect(plan.invalidRegex).toBe(true);
    expect(plan.rows[0].newName).toBe("a.txt");
    expect(plan.applicable).toBe(0);
  });

  it("numbers rows sequentially from the configured start", () => {
    const plan = buildRenamePlan([entry("a.txt"), entry("b.txt")], { ...base, numbering: true, numberingStart: 9 });
    expect(plan.rows.map((row) => row.newName)).toEqual(["a.txt-009", "b.txt-010"]);
  });
});
