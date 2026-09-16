import { afterEach, describe, expect, it, vi } from "vitest";
import {
  RateSampler,
  applyList,
  applyProgress,
  createTransferTracker,
  etaSeconds,
  formatEta,
  formatRate,
  isRetryableKind,
  percentOf,
  progressBytes,
  sortedJobs,
  splitTransferPath,
  type TransferJob,
} from "./transfers";

function job(overrides: Partial<TransferJob>): TransferJob {
  return {
    jobId: "job-1",
    connectionId: "conn",
    kind: "upload",
    state: "running",
    size: 100,
    transferred: 0,
    updatedAt: 1,
    ...overrides,
  };
}

describe("transfers", () => {
  it("splits a transfer path into parent and final name", () => {
    expect(splitTransferPath("/var/lib/dbx/report.pdf")).toEqual({ parent: "/var/lib/dbx", name: "report.pdf" });
  });

  it("omits an absent connection filter but preserves an explicitly supplied filter", async () => {
    const tracker = createTransferTracker();
    const calls = vi.fn();
    const invoke = async <T,>(method: string, params?: unknown): Promise<T> => {
      calls(method, params);
      return { jobs: [] } as T;
    };
    await tracker.refresh(invoke);
    expect(calls).toHaveBeenLastCalledWith("files/transfers/list", {});
    await tracker.refresh(invoke, "conn-1");
    expect(calls).toHaveBeenLastCalledWith("files/transfers/list", { connectionId: "conn-1" });
    await tracker.refresh(invoke, "");
    expect(calls).toHaveBeenLastCalledWith("files/transfers/list", { connectionId: "" });
  });

  it("upserts progress events keyed by jobId", () => {
    const jobs: Record<string, TransferJob> = {};
    applyProgress(jobs, { jobId: "a", kind: "upload", transferred: 10, size: 100, state: "running" });
    applyProgress(jobs, { jobId: "a", kind: "upload", transferred: 50, size: 100 });
    applyProgress(jobs, { taskId: "b", kind: "download", transferred: 5, size: 10 });
    expect(Object.keys(jobs).sort()).toEqual(["a", "b"]);
    expect(jobs.a.transferred).toBe(50);
    expect(jobs.b.kind).toBe("download");
  });

  it("completes jobs and keeps error text on failure", () => {
    const jobs: Record<string, TransferJob> = {};
    applyProgress(jobs, { jobId: "a", size: 100 });
    applyProgress(jobs, { jobId: "a", state: "completed", transferred: 100, size: 100 });
    expect(jobs.a.state).toBe("completed");
    applyProgress(jobs, { jobId: "b", size: 10 });
    applyProgress(jobs, { jobId: "b", state: "failed", error: "boom" });
    expect(jobs.b.error).toBe("boom");
  });

  it("computes percent from file or folder metrics", () => {
    expect(percentOf(job({ size: 200, transferred: 50 }))).toBe(25);
    expect(percentOf(job({ state: "completed", size: 0 }))).toBe(100);
    expect(percentOf(job({ kind: "syncDir", bytesTotal: 400, bytesDone: 100 }))).toBe(25);
  });

  it("renders degraded copy/move/rename byte progress (P-FILES ①a)", () => {
    // 降级 copy/move/rename 携带 bytesTotal/bytesDone：按字节渲染。
    expect(percentOf(job({ kind: "move", bytesTotal: 400, bytesDone: 100 }))).toBe(25);
    expect(percentOf(job({ kind: "copy", bytesTotal: 400, bytesDone: 400, state: "completed" }))).toBe(100);
    expect(percentOf(job({ kind: "rename", bytesTotal: 400, bytesDone: 300 }))).toBe(75);
    // 未带字节总量的 copy/move 仍按单文件字段渲染。
    expect(percentOf(job({ kind: "move", size: 100, transferred: 50 }))).toBe(50);
  });

  it("classifies dir jobs from files/transfers/list items", () => {
    const jobs: Record<string, TransferJob> = {};
    // list_merged 形状：DirJob 直接 serde 出 jobId/kind/status/bytes*。
    applyList(
      jobs,
      {
        jobs: [
          { jobId: "j-copy", kind: "copy", status: "running", sourcePath: "/s", targetPath: "/t", bytesDone: 10, bytesTotal: 40 },
          { jobId: "j-rename", kind: "rename", status: "queued", sourcePath: "/dir", targetPath: "/dir2" },
        ],
      },
      "conn-1",
    );
    expect(jobs["j-copy"].kind).toBe("copy");
    expect(jobs["j-copy"].bytesTotal).toBe(40);
    expect(jobs["j-rename"].state).toBe("queued");
  });

  it("sorts active jobs before history", () => {
    const jobs = {
      done: job({ jobId: "done", state: "completed", updatedAt: 99 }),
      running: job({ jobId: "running", state: "running", updatedAt: 1 }),
    };
    expect(sortedJobs(jobs).map((entry) => entry.jobId)).toEqual(["running", "done"]);
  });

  it("sorts history newest first by task creation time, not finishedAt", () => {
    const jobs = {
      older: job({ jobId: "older", state: "completed", createdAt: 100, finishedAt: 900 }),
      newer: job({ jobId: "newer", state: "failed", createdAt: 200, finishedAt: 300 }),
    };
    expect(sortedJobs(jobs).map((entry) => entry.jobId)).toEqual(["newer", "older"]);
  });

  it("normalizes files/transfers/list payloads", () => {
    const jobs: Record<string, TransferJob> = {};
    applyList(
      jobs,
      {
        jobs: [
          { jobId: "j1", status: "running", size: 10, transferred: 4, kind: "upload", remotePath: "/x.bin" },
          { taskId: "j2", state: "canceled", filesDone: 2, filesTotal: 5 },
        ],
      },
      "conn-1",
    );
    expect(jobs.j1.state).toBe("running");
    expect(jobs.j1.remotePath).toBe("/x.bin");
    expect(jobs.j2.state).toBe("canceled");
    expect(jobs.j2.filesTotal).toBe(5);
  });
});

describe("rate sampler (P-FILES ⑥)", () => {
  it("returns undefined on the first sample and instant rate on the second", () => {
    const sampler = new RateSampler();
    expect(sampler.sample("j", 0, 1000)).toBeUndefined();
    // 1000 字节 / 1s = 1000 B/s（首个有效窗口直接取瞬时值）。
    expect(sampler.sample("j", 1000, 2000)).toBe(1000);
  });

  it("smooths jittering windows with EWMA", () => {
    const sampler = new RateSampler();
    sampler.sample("j", 0, 0);
    const first = sampler.sample("j", 1000, 1000); // 1000 B/s
    expect(first).toBe(1000);
    // 第二个窗口 3000 B/s：0.35*3000 + 0.65*1000 = 1700。
    expect(sampler.sample("j", 4000, 2000)).toBe(1700);
  });

  it("rebaselines when cumulative bytes go backwards and keeps rate on idle windows", () => {
    const sampler = new RateSampler();
    sampler.sample("j", 0, 0);
    sampler.sample("j", 1000, 1000);
    // 字节回退 → 重新基线，不产出速率。
    expect(sampler.sample("j", 10, 2000)).toBeUndefined();
    // 停滞窗口（无新字节）→ 维持上一速率。
    expect(sampler.sample("j", 10, 3000)).toBe(0);
    sampler.reset("j");
    expect(sampler.sample("j", 500, 4000)).toBeUndefined();
  });

  it("picks dir-job bytes for sampling and formats rate/eta language-neutrally", () => {
    expect(progressBytes(job({ kind: "syncDir", bytesDone: 300 }))).toBe(300);
    expect(progressBytes(job({ transferred: 42 }))).toBe(42);
    expect(formatRate(2048)).toBe("2.0 KiB/s");
    expect(formatEta(45)).toBe("45s");
    expect(formatEta(192)).toBe("3m12s");
    expect(formatEta(3900)).toBe("1h05m");
  });

  it("computes eta only for running jobs with known rate and remaining bytes", () => {
    const running = job({ state: "running", size: 1000, transferred: 250, rateBps: 250 });
    expect(etaSeconds(running)).toBe(3);
    expect(etaSeconds(job({ state: "running", size: 1000, transferred: 250 }))).toBeUndefined();
    expect(etaSeconds(job({ state: "completed", size: 1000, transferred: 1000, rateBps: 250 }))).toBeUndefined();
    expect(etaSeconds(job({ kind: "syncDir", state: "running", bytesTotal: 1000, bytesDone: 0, rateBps: 0 }))).toBeUndefined();
  });
});

describe("transfer tracker", () => {
  it("samples rate through onProgress and clears it on terminal states", () => {
    // 采样时间取 Date.now()，用 fake timers 驱动确定性时间窗口。
    vi.useFakeTimers();
    vi.setSystemTime(1_000);
    try {
      const tracker = createTransferTracker();
      tracker.register({ ...job({ jobId: "j", state: "running", size: 1000, transferred: 0, updatedAt: 1 }) });
      tracker.onProgress({ jobId: "j", state: "running", transferred: 500 });
      // 只有一次采样窗口 → 无速率，等第二个事件。
      expect(tracker.jobs.j.rateBps).toBeUndefined();
      vi.setSystemTime(2_000);
      tracker.onProgress({ jobId: "j", state: "running", transferred: 1000 });
      expect(tracker.jobs.j.rateBps).toBe(500);
      tracker.onProgress({ jobId: "j", state: "completed", transferred: 1000 });
      expect(tracker.jobs.j.rateBps).toBeUndefined();
    } finally {
      vi.useRealTimers();
    }
  });

  it("clearFinished removes only finished jobs and keeps active ones", () => {
    const tracker = createTransferTracker();
    tracker.register({ ...job({ jobId: "done", state: "completed", updatedAt: 1 }) });
    tracker.register({ ...job({ jobId: "run", state: "running", updatedAt: 2 }) });
    expect(tracker.clearFinished()).toBe(1);
    expect(tracker.jobs.done).toBeUndefined();
    expect(tracker.jobs.run).toBeDefined();
  });

  it("refresh feeds the sampler via list payloads (polling fallback)", async () => {
    vi.useFakeTimers();
    vi.setSystemTime(1_000);
    try {
      const tracker = createTransferTracker();
      let listCount = 0;
      const payloads = [
        { jobs: [{ jobId: "j", status: "running", kind: "upload", size: 1000, transferred: 100 }] },
        { jobs: [{ jobId: "j", status: "running", kind: "upload", size: 1000, transferred: 600 }] },
      ];
      const invoke = async <T,>(_method: string): Promise<T> => payloads[Math.min(listCount++, payloads.length - 1)] as unknown as T;
      await tracker.refresh(invoke);
      expect(tracker.jobs.j.rateBps).toBeUndefined();
      vi.setSystemTime(2_000);
      await tracker.refresh(invoke);
      expect(tracker.jobs.j.rateBps).toBe(500);
    } finally {
      vi.useRealTimers();
    }
  });
});

describe("isRetryableKind", () => {
  it("accepts the job kinds that map 1:1 to a re-issuable backend method", () => {
    expect(isRetryableKind("copy")).toBe(true);
    expect(isRetryableKind("move")).toBe(true);
    expect(isRetryableKind("rename")).toBe(true);
    expect(isRetryableKind("syncDir")).toBe(true);
    expect(isRetryableKind("copyDir")).toBe(true);
  });

  it("rejects kinds whose original request shape is not replayable", () => {
    expect(isRetryableKind("upload")).toBe(false);
    expect(isRetryableKind("download")).toBe(false);
    expect(isRetryableKind("")).toBe(false);
  });
});

describe("applyList remotePath preservation (P2-8)", () => {
  it("keeps the locally registered readable title when the list payload lacks remotePath", () => {
    const jobs: Record<string, TransferJob> = {
      j: job({ jobId: "j", kind: "copy", remotePath: "/docs/a.txt → /docs/a-copy.txt" }),
    };
    // 轮询兜底（mock/旧 sidecar）缺 remotePath：不得把标题覆盖成 undefined/jobId
    applyList(jobs, { jobs: [{ jobId: "j", status: "running", kind: "copy" }] }, "conn");
    expect(jobs.j.remotePath).toBe("/docs/a.txt → /docs/a-copy.txt");
  });

  it("prefers the payload remotePath when present (real sidecar contract)", () => {
    const jobs: Record<string, TransferJob> = {
      j: job({ jobId: "j", kind: "copy", remotePath: "stale" }),
    };
    applyList(jobs, { jobs: [{ jobId: "j", status: "running", kind: "copy", remotePath: "/fresh/path" }] }, "conn");
    expect(jobs.j.remotePath).toBe("/fresh/path");
  });
});

// issue#6-1b：历史任务时间不得跟着本机时钟走——轮询兜底每 5s 全量刷新一次，
// 若一律写 Date.now()，历史列表展示的「上传/下载时间」会持续漂移到当前时刻。
describe("applyList historical timestamps (issue #6)", () => {
  it("adopts the sidecar finishedAt for a first-seen finished job instead of the poll wall clock", () => {
    vi.useFakeTimers({ now: 1_000_000 });
    try {
      const jobs: Record<string, TransferJob> = {};
      applyList(jobs, { jobs: [{ taskId: "t1", status: "completed", finishedAt: 500_000, startedAt: 400_000 }] }, "conn");
      expect(jobs.t1.finishedAt).toBe(500_000);
      expect(jobs.t1.updatedAt).toBe(500_000);
    } finally {
      vi.useRealTimers();
    }
  });

  it("does not clobber an existing job's updatedAt on later polls", () => {
    vi.useFakeTimers({ now: 1_000_000 });
    try {
      const jobs: Record<string, TransferJob> = {
        j1: job({ jobId: "j1", state: "completed", updatedAt: 123_456, finishedAt: 123_456 }),
      };
      // 5s 后的轮询（墙钟已是 1_000_000）：updatedAt 保持完成时刻不变；
      // finishedAt 继续采信 sidecar（服务端权威完成时刻）。
      applyList(jobs, { jobs: [{ jobId: "j1", status: "completed", finishedAt: 999_999 }] }, "conn");
      expect(jobs.j1.updatedAt).toBe(123_456);
      expect(jobs.j1.finishedAt).toBe(999_999);
    } finally {
      vi.useRealTimers();
    }
  });

  it("falls back to startedAt, then now, when the payload carries no timestamps", () => {
    vi.useFakeTimers({ now: 1_000_000 });
    try {
      const jobs: Record<string, TransferJob> = {};
      applyList(jobs, { jobs: [{ taskId: "t1", status: "running", startedAt: 900_000 }] }, "conn");
      expect(jobs.t1.updatedAt).toBe(900_000);
      applyList(jobs, { jobs: [{ taskId: "t2", status: "queued" }] }, "conn");
      expect(jobs.t2.updatedAt).toBe(1_000_000);
    } finally {
      vi.useRealTimers();
    }
  });
});

// ---------------------------------------------------------------------------
// 本机落盘 localPath（对标 ssh 面板：下载完成后可定位/打开）
// ---------------------------------------------------------------------------

describe("localPath propagation", () => {
  it("completion event carries localPath and later events keep it", () => {
    const store = createTransferTracker();
    store.onProgress({ jobId: "t1", taskId: "t1", kind: "download", state: "running", size: 100, transferred: 10 });
    store.onProgress({ jobId: "t1", taskId: "t1", state: "completed", transferred: 100, localPath: "/Downloads/a.bin" });
    expect(store.jobs.t1.localPath).toBe("/Downloads/a.bin");
    // 迟到的轮询行不带 localPath 时不得清掉已有值。
    store.onProgress({ jobId: "t1", taskId: "t1", state: "completed", transferred: 100 });
    expect(store.jobs.t1.localPath).toBe("/Downloads/a.bin");
  });

  it("applyList hydrates localPath from persisted history rows", () => {
    const jobs: Record<string, TransferJob> = {};
    applyList(jobs, { jobs: [{ taskId: "h1", kind: "download", status: "completed", localPath: "/Downloads/old.bin" }] }, "conn");
    expect(jobs.h1.localPath).toBe("/Downloads/old.bin");
  });

  it("tracker.remove drops exactly one job", () => {
    const store = createTransferTracker();
    store.register(job({ jobId: "a", state: "completed", updatedAt: 1 }));
    store.register(job({ jobId: "b", state: "completed", updatedAt: 2 }));
    expect(store.remove("a")).toBe(true);
    expect(store.remove("a")).toBe(false);
    expect(store.historyList().map((item) => item.jobId)).toEqual(["b"]);
  });
});
