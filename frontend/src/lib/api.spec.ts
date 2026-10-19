import { afterEach, describe, expect, it, vi } from "vitest";
import { DEFAULT_CALL_TIMEOUT_MS, bindApi, call, entryKey, FilesApiError, isHostMethodMissing, normalizeEntry, normalizeEntries, pathFromEntryKey, type FileEntry } from "./api";
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

describe("entryKey（issue #82 同名文件+目录双实体身份）", () => {
  it("same-name file and dir entries get distinct keys despite equal paths", () => {
    // OSS 平铺 key：`new-api` 对象与 `new-api/` 前缀组合法共存，两条目 path 同值。
    const file: FileEntry = { name: "new-api", path: "/new-api", kind: "file", size: 93 };
    const dir: FileEntry = { name: "new-api", path: "/new-api", kind: "directory" };
    expect(entryKey(file)).not.toBe(entryKey(dir));
    expect(entryKey(file)).toBe("file:/new-api");
    expect(entryKey(dir)).toBe("dir:/new-api");
  });

  it("pathFromEntryKey round-trips both kinds and passes bare paths through", () => {
    expect(pathFromEntryKey(entryKey({ path: "/new-api", kind: "directory" }))).toBe("/new-api");
    expect(pathFromEntryKey(entryKey({ path: "/new-api", kind: "file" }))).toBe("/new-api");
    // 宿主快照等历史出口的裸 path 原样透传（防御）。
    expect(pathFromEntryKey("/plain/path")).toBe("/plain/path");
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

describe("call() 默认超时（B1：断链自愈的前端侧边界）", () => {
  afterEach(() => {
    vi.useRealTimers();
  });

  it("rejects with FilesApiError when the bridge promise never settles", async () => {
    vi.useFakeTimers();
    bindApi(() => new Promise<never>(() => {}), null);
    const pending = call("files/list");
    const assertion = expect(pending).rejects.toBeInstanceOf(FilesApiError);
    await vi.advanceTimersByTimeAsync(DEFAULT_CALL_TIMEOUT_MS);
    await assertion;
    await expect(pending).rejects.toThrow("files/list timed out after 15000 ms");
  });

  it("keeps a fast resolution untouched and injects connectionId as before", async () => {
    const invoke = vi.fn((_method: string, params?: unknown) => Promise.resolve({ ok: true, params }));
    // vi.fn 的单态签名与 Invoke 的泛型签名不兼容，仅测试桩处放宽。
    bindApi(invoke as unknown as Parameters<typeof bindApi>[0], "conn-1");
    await expect(call("files/capabilities")).resolves.toEqual({ ok: true, params: { connectionId: "conn-1" } });
  });

  it("lets an explicit timeoutMs override the default and forwards it to the bridge", async () => {
    vi.useFakeTimers();
    let seenOptions: unknown;
    const invoke = vi.fn((_method: string, _params: unknown, options?: { timeoutMs?: number }) => {
      seenOptions = options;
      return new Promise<never>(() => {});
    });
    bindApi(invoke, null);
    const explicit = 30 * 60 * 1000;
    let rejected = false;
    const pending = call("files/upload/finish", { taskId: "t1" }, { timeoutMs: explicit }).catch((cause: unknown) => {
      rejected = true;
      return cause;
    });

    // 显式值优先：默认 15s 到点不落败（超长任务不受默认超时影响）。
    await vi.advanceTimersByTimeAsync(DEFAULT_CALL_TIMEOUT_MS);
    expect(rejected).toBe(false);

    await vi.advanceTimersByTimeAsync(explicit - DEFAULT_CALL_TIMEOUT_MS);
    const cause = (await pending) as FilesApiError;
    expect(rejected).toBe(true);
    expect(cause).toBeInstanceOf(FilesApiError);
    expect(cause.message).toContain("timed out");
    // 显式 options 仍透传给宿主桥（支持的实现可更早 enforce）。
    expect(seenOptions).toEqual({ timeoutMs: explicit });
  });
});
