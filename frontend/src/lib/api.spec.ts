import { describe, expect, it } from "vitest";
import { isHostMethodMissing, normalizeEntry, normalizeEntries, type FileEntry } from "./api";
import { sortEntries, type SortState } from "./sorting";

const modifiedSort: SortState = { column: "modified", direction: "asc" };

describe("normalizeEntry", () => {
  it("keeps dir→directory normalization", () => {
    // 线上 wire 契约 kind 是 "dir"|"file"（model.rs），以真实形状传入。
    expect(normalizeEntry({ name: "d", path: "/d", kind: "dir" } as unknown as FileEntry).kind).toBe("directory");
    expect(normalizeEntry({ name: "f", path: "/f", kind: "file" }).kind).toBe("file");
  });

  it("coerces backend epoch-millis modifiedAt to an ISO string", () => {
    // 真实后端 model.rs 的 FileEntry.modified_at 是 u64 epoch 毫秒；前端契约是 string。
    const entry = normalizeEntry({ name: "a", path: "/a", kind: "file", modifiedAt: 1_700_000_000_000 as unknown as string });
    expect(entry.modifiedAt).toBe("2023-11-14T22:13:20.000Z");
  });

  it("passes ISO-string modifiedAt through untouched (mock/旧宿主契约)", () => {
    const entry = normalizeEntry({ name: "a", path: "/a", kind: "file", modifiedAt: "2025-06-01T00:00:00Z" });
    expect(entry.modifiedAt).toBe("2025-06-01T00:00:00Z");
  });

  it("keeps missing modifiedAt missing", () => {
    expect(normalizeEntry({ name: "a", path: "/a", kind: "file" }).modifiedAt).toBeUndefined();
  });
});

describe("dual-pane modified sort regression (双栏 modifiedAt localeCompare 崩溃)", () => {
  it("sorting backend-shaped entries by modified does not throw", () => {
    // 回归：后端条目未归一化时 sortEntries 调 number.localeCompare 直接 TypeError。
    // 同 kind（目录恒置顶会提前返回，走不到 modified 比较），必须两条都是 file。
    const raw = normalizeEntries([
      { name: "old", path: "/old", kind: "file", modifiedAt: 1_600_000_000_000 as unknown as string },
      { name: "new", path: "/new", kind: "file", modifiedAt: 1_700_000_000_000 as unknown as string },
    ]);
    expect(() => sortEntries(raw, modifiedSort)).not.toThrow();
    expect(sortEntries(raw, modifiedSort).map((item) => item.name)).toEqual(["old", "new"]);
  });
});

describe("isHostMethodMissing", () => {
  it("detects old-host bridge errors only", () => {
    expect(isHostMethodMissing(new Error("Unsupported plugin host method 'host.reopenConnection'"))).toBe(true);
    expect(isHostMethodMissing(new Error("Connection reopen is unavailable"))).toBe(true);
    expect(isHostMethodMissing(new Error("Method not found: files/list"))).toBe(true);
    expect(isHostMethodMissing(new Error("Storage connect failed: bad credentials"))).toBe(false);
  });
});
