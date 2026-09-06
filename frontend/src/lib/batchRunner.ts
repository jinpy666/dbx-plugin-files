// 批量执行器（P2-7 并发分批 + R3-P2-9 取消）：固定并发跑 N 个子任务，
// 逐个完成即回报进度；isCanceled() 翻真后不再领取新分片（在跑的等待自然
// 结束），最终以 canceled 终态返回——此前取消是假成功（本地伪 job 无中断
// 机制，files/transfer/cancel 对 local-batch-* 返回 success 但任务继续跑完）。
// 纯逻辑抽出便于 vitest 覆盖取消/错误/进度语义。

export interface BatchRunResult {
  done: number;
  total: number;
  canceled: boolean;
  /** 首个失败原因（canceled 时可能同时存在已完成子任务的错误）。 */
  error?: unknown;
}

export async function runBatchTasks(input: {
  count: number;
  concurrency?: number;
  isCanceled: () => boolean;
  task: (index: number) => Promise<void>;
  /** 每完成一个子任务（成功或失败）回调一次：done 为累计完成数。 */
  onProgress?: (done: number, total: number) => void;
}): Promise<BatchRunResult> {
  const total = input.count;
  if (total <= 0) return { done: 0, total, canceled: false };
  const concurrency = Math.max(1, Math.min(input.concurrency ?? 8, total));
  let cursor = 0;
  let done = 0;
  let firstError: unknown;
  let canceled = false;

  const worker = async () => {
    while (true) {
      if (input.isCanceled()) {
        canceled = true;
        return;
      }
      if (cursor >= total) return;
      const index = cursor++;
      try {
        await input.task(index);
      } catch (cause) {
        firstError ??= cause;
      }
      done += 1;
      input.onProgress?.(done, total);
    }
  };

  await Promise.all(Array.from({ length: concurrency }, worker));
  return { done, total, canceled, error: firstError };
}
