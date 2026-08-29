import { describe, expect, it } from "vitest";
import { applyList, applyProgress, percentOf, sortedJobs, type TransferJob } from "./transfers";

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
