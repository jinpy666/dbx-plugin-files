// @vitest-environment happy-dom
import { describe, expect, it } from "vitest";
import { runBatchTasks } from "./batchRunner";

describe("runBatchTasks", () => {
  it("runs every task with bounded concurrency and reports progress", async () => {
    const seen: number[] = [];
    const progress: Array<[number, number]> = [];
    let running = 0;
    let peak = 0;
    const result = await runBatchTasks({
      count: 6,
      concurrency: 2,
      isCanceled: () => false,
      task: async (index) => {
        running += 1;
        peak = Math.max(peak, running);
        seen.push(index);
        running -= 1;
      },
      onProgress: (done, total) => progress.push([done, total]),
    });
    expect(seen.sort((a, b) => a - b)).toEqual([0, 1, 2, 3, 4, 5]);
    expect(peak).toBeLessThanOrEqual(2);
    expect(progress).toEqual([
      [1, 6],
      [2, 6],
      [3, 6],
      [4, 6],
      [5, 6],
      [6, 6],
    ]);
    expect(result).toEqual({ done: 6, total: 6, canceled: false });
  });

  it("stops claiming new work once canceled (R3-P2-9)", async () => {
    let canceled = false;
    const executed: number[] = [];
    const result = await runBatchTasks({
      count: 100,
      // 并发 1 保证时序确定：首个子任务完成后立即翻转取消标志
      concurrency: 1,
      isCanceled: () => canceled,
      task: async (index) => {
        executed.push(index);
        if (index === 0) canceled = true;
      },
    });
    expect(result.canceled).toBe(true);
    expect(result.done).toBe(1);
    expect(executed).toEqual([0]);
  });

  it("collects the first error without aborting the rest", async () => {
    const result = await runBatchTasks({
      count: 3,
      isCanceled: () => false,
      task: async (index) => {
        if (index === 1) throw new Error("boom");
      },
    });
    expect(result.canceled).toBe(false);
    expect(result.done).toBe(3);
    expect((result.error as Error).message).toBe("boom");
  });

  it("returns immediately for an empty batch", async () => {
    const result = await runBatchTasks({ count: 0, isCanceled: () => false, task: async () => undefined });
    expect(result).toEqual({ done: 0, total: 0, canceled: false });
  });
});
