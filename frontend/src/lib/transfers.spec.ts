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
