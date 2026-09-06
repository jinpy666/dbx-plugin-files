// @vitest-environment happy-dom
import { describe, expect, it } from "vitest";
import { validateFileName } from "./fileName";

describe("validateFileName (R3-P2-4)", () => {
  it("accepts ordinary names, emoji, leading dots and inner spaces", () => {
    expect(validateFileName("notes.txt")).toBeNull();
    expect(validateFileName("📁 文件 名.md")).toBeNull();
    expect(validateFileName(".hidden-dir")).toBeNull();
    expect(validateFileName("a.b.c")).toBeNull();
  });

  it("rejects empty and whitespace-only names", () => {
    expect(validateFileName("")).toBe("empty");
    expect(validateFileName("   ")).toBe("empty");
  });

  it("rejects names containing path separators (invisible nested creation)", () => {
    expect(validateFileName("a/b")).toBe("slash");
    expect(validateFileName("a/b/c")).toBe("slash");
    expect(validateFileName("a\\b")).toBe("slash");
  });

  it("rejects dot-only names", () => {
    expect(validateFileName(".")).toBe("dot");
    expect(validateFileName("..")).toBe("dot");
  });

  it("accepts names that merely look related to dots", () => {
    expect(validateFileName("...")).toBeNull();
    expect(validateFileName("..data")).toBeNull();
    expect(validateFileName("a..b")).toBeNull();
  });
});
