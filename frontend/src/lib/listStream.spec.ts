// 流式目录列表会话（files/listStream P1）纯逻辑单测：
// chunk 累加与 seq 去重、requestId 不匹配丢弃、done/error 终态交付、abandon
// （新导航取代）结算等待。渲染与导航联动在 App.listStream.spec.ts 覆盖。
import { describe, expect, it } from "vitest";
import { createChunkHoldback, createListStreamBrowse, type ListStreamChunk } from "./listStream";
import type { FileEntry } from "./api";

const entry = (path: string, size = 1): FileEntry => ({
  name: path.split("/").pop() ?? path,
  path,
  kind: "file",
  size,
  modifiedAt: "",
});

function chunkOf(overrides: Partial<ListStreamChunk> & Pick<ListStreamChunk, "requestId" | "seq">): ListStreamChunk {
  return { entries: [], done: false, ...overrides };
}

describe("createListStreamBrowse chunk 累加与 seq 去重", () => {
  it("accepts ordered chunks, appends entries and drops duplicate/late seq frames", async () => {
    const seen: FileEntry[][] = [];
    const browse = createListStreamBrowse({ onChunk: (entries) => seen.push([...entries]) });
    expect(browse.register({ requestId: "r1" })).toBe(true);
    expect(browse.handleChunk(chunkOf({ requestId: "r1", seq: 1, entries: [entry("/a")] }))).toBe(true);
    expect(browse.handleChunk(chunkOf({ requestId: "r1", seq: 2, entries: [entry("/b")] }))).toBe(true);
    // 重复帧（seq 已见）与迟到帧（seq 回退）一律丢弃，不计入累加。
    expect(browse.handleChunk(chunkOf({ requestId: "r1", seq: 2, entries: [entry("/dup")] }))).toBe(false);
    expect(browse.handleChunk(chunkOf({ requestId: "r1", seq: 1, entries: [entry("/late")] }))).toBe(false);
    expect(browse.handleChunk(chunkOf({ requestId: "r1", seq: 3, entries: [entry("/c")], done: true, total: 3 }))).toBe(true);
    const result = await browse.result;
    expect(result.failed).toBe(false);
    expect(result.entries.map((item) => item.path)).toEqual(["/a", "/b", "/c"]);
    // 每个（首个有效之外的）中间帧回调一次累加数组；done 帧不触发 onChunk。
    expect(seen.map((list) => list.map((item) => item.path))).toEqual([["/a"], ["/a", "/b"]]);
  });

  it("rejects frames with a mismatched requestId (contract: 不匹配丢弃)", async () => {
    const browse = createListStreamBrowse({});
    browse.register({ requestId: "r1" });
    expect(browse.handleChunk(chunkOf({ requestId: "other", seq: 1, entries: [entry("/x")] }))).toBe(false);
    expect(browse.handleChunk(chunkOf({ requestId: "other", seq: 2, entries: [], done: true }))).toBe(false);
    // 伪造帧不得污染会话状态：本会话仍可正常完成。
    expect(browse.handleChunk(chunkOf({ requestId: "r1", seq: 1, entries: [entry("/a")], done: true }))).toBe(true);
    expect((await browse.result).entries.map((item) => item.path)).toEqual(["/a"]);
  });

  it("rejects frames before the ack is registered (requestId 未登记)", () => {
    const browse = createListStreamBrowse({});
    expect(browse.requestId).toBeNull();
    expect(browse.handleChunk(chunkOf({ requestId: "r1", seq: 1, entries: [entry("/a")], done: true }))).toBe(false);
    expect(browse.count).toBe(0);
  });

  it("normalizes mock 'dir' kinds through the default decorate", async () => {
    const browse = createListStreamBrowse({});
    browse.register({ requestId: "r1" });
    browse.handleChunk(chunkOf({ requestId: "r1", seq: 1, entries: [{ name: "d", path: "/d", kind: "dir" as never }] }));
    browse.handleChunk(chunkOf({ requestId: "r1", seq: 2, entries: [], done: true }));
    expect((await browse.result).entries[0]).toMatchObject({ path: "/d", kind: "directory" });
  });

  it("passes the ack charset into the injected decorate", async () => {
    const charsets: string[] = [];
    const browse = createListStreamBrowse({
      decorate: (entries, charset) => {
        charsets.push(charset);
        return entries;
      },
    });
    browse.register({ requestId: "r1", displayCharset: "gbk" });
    browse.handleChunk(chunkOf({ requestId: "r1", seq: 1, entries: [entry("/a")] }));
    browse.handleChunk(chunkOf({ requestId: "r1", seq: 2, entries: [], done: true }));
    await browse.result;
    expect(charsets).toEqual(["gbk", "gbk"]);
  });
});

describe("createListStreamBrowse 终态与取消", () => {
  it("delivers the failure frame with partial entries, partialCount and error", async () => {
    const browse = createListStreamBrowse({});
    browse.register({ requestId: "r1" });
    browse.handleChunk(chunkOf({ requestId: "r1", seq: 1, entries: [entry("/a"), entry("/b")] }));
    // 失败帧：error 非空 + partialCount，entries 恒空（不得计入累加）。
    expect(browse.handleChunk(chunkOf({ requestId: "r1", seq: 2, entries: [], done: true, error: "boom", partialCount: 2 }))).toBe(true);
    const result = await browse.result;
    expect(result.failed).toBe(true);
    expect(result.errorMessage).toBe("boom");
    expect(result.partialCount).toBe(2);
    expect(result.entries.map((item) => item.path)).toEqual(["/a", "/b"]);
    // 终态后的任何帧（含补发/重试帧）一律拒绝。
    expect(browse.handleChunk(chunkOf({ requestId: "r1", seq: 3, entries: [entry("/c")] }))).toBe(false);
  });

  it("abandon settles the wait with stale result, returns the requestId and rejects later frames", async () => {
    const browse = createListStreamBrowse({});
    browse.register({ requestId: "r1" });
    browse.handleChunk(chunkOf({ requestId: "r1", seq: 1, entries: [entry("/a")] }));
    expect(browse.finished).toBe(false);
    expect(browse.abandon()).toBe("r1");
    expect(browse.finished).toBe(true);
    const result = await browse.result;
    expect(result.stale).toBe(true);
    expect(result.failed).toBe(false);
    expect(result.entries.map((item) => item.path)).toEqual(["/a"]);
    expect(browse.handleChunk(chunkOf({ requestId: "r1", seq: 2, entries: [], done: true }))).toBe(false);
    // 弃置后的迟到 ack 不再登记（返回 false 供调用方发起 best-effort 取消）。
    expect(browse.register({ requestId: "r2" })).toBe(false);
    // 幂等：二次 abandon 不再重复返回 requestId。
    expect(browse.abandon()).toBeNull();
  });

  it("is idempotent on repeated register and ignores the second ack", () => {
    const acks: string[] = [];
    const browse = createListStreamBrowse({ onAck: (ack) => acks.push(ack.requestId) });
    expect(browse.register({ requestId: "r1" })).toBe(true);
    expect(browse.register({ requestId: "r2" })).toBe(false);
    expect(acks).toEqual(["r1"]);
    expect(browse.requestId).toBe("r1");
  });
});

// ack 前到达帧的预缓冲（真实宿主桥竞态：事件可能先于 RPC 应答写入）。
describe("createChunkHoldback (ack 前到达帧预缓冲)", () => {
  const frame = (requestId: string, seq: number): ListStreamChunk => ({ requestId, seq, entries: [], done: false });

  it("drains buffered frames in arrival order and clears the bucket", () => {
    const holdback = createChunkHoldback();
    holdback.push(frame("r1", 1));
    holdback.push(frame("r1", 2));
    holdback.push(frame("r1", 3));
    expect(holdback.drain("r1").map((chunk) => chunk.seq)).toEqual([1, 2, 3]);
    expect(holdback.drain("r1")).toEqual([]);
  });

  it("drops a specific bucket on drop and keeps others", () => {
    const holdback = createChunkHoldback();
    holdback.push(frame("r1", 1));
    holdback.push(frame("r2", 1));
    holdback.drop("r1");
    expect(holdback.drain("r1")).toEqual([]);
    expect(holdback.drain("r2")).toHaveLength(1);
  });

  it("expires buckets after the TTL (fake clock)", () => {
    let now = 0;
    const holdback = createChunkHoldback(() => now);
    holdback.push(frame("r1", 1));
    now = 4_999;
    holdback.push(frame("r2", 1)); // sweep 触发但 r1 未过期
    expect(holdback.drain("r1")).toHaveLength(1);
    holdback.push(frame("r2", 2));
    now = 10_000;
    holdback.push(frame("r3", 1)); // sweep：r1/r2 均过 TTL
    expect(holdback.drain("r2")).toEqual([]);
    expect(holdback.drain("r3")).toHaveLength(1);
  });

  it("caps frames per requestId (keeps the head, drops the tail)", () => {
    const holdback = createChunkHoldback();
    for (let seq = 1; seq <= 70; seq += 1) holdback.push(frame("r1", seq));
    expect(holdback.drain("r1")).toHaveLength(64);
  });
});
