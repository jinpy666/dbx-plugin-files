// 传输 job 状态仓：事件驱动（files/transfer/progress）为主，轮询兜底
// （files/transfers/list）。进度节流由 sidecar 负责（≥200ms 或 ≥1%），
// 前端只负责渲染。语义对齐 tiny-rdm transferRuntime.js 的重写版。

import { reactive } from "vue";

export type TransferState = "queued" | "running" | "completed" | "failed" | "canceled";

export type TransferKind = "upload" | "download" | "copyDir" | "syncDir" | "copy" | "move" | "rename";

export interface TransferJob {
  jobId: string;
  taskId?: string;
  connectionId: string;
  kind: TransferKind;
  remotePath?: string;
  state: TransferState;
  size: number;
  transferred: number;
  filesDone?: number;
  filesTotal?: number;
  bytesDone?: number;
  bytesTotal?: number;
  error?: string;
  updatedAt: number;
}

export interface TransferProgressEvent {
  jobId?: string;
  taskId?: string;
  connectionId?: string;
  kind?: TransferKind;
  direction?: string;
  remotePath?: string;
  fileName?: string;
  state?: TransferState;
  size?: number;
  transferred?: number;
  filesDone?: number;
  filesTotal?: number;
  bytesDone?: number;
  bytesTotal?: number;
  error?: string;
}

export const ACTIVE_STATES: readonly TransferState[] = ["queued", "running"];

export function isActive(state: TransferState): boolean {
  return state === "queued" || state === "running";
}

/** 目录类 job：按字节进度渲染（degraded copy/move/rename 携带 bytesTotal 时同样按字节）。 */
export function isByteBased(job: TransferJob): boolean {
  if (job.kind === "copyDir" || job.kind === "syncDir") return true;
  return (job.kind === "copy" || job.kind === "move" || job.kind === "rename") && job.bytesTotal !== undefined;
}

export function percentOf(job: TransferJob): number {
  const total = isByteBased(job) ? job.bytesTotal ?? 0 : job.size;
  const done = isByteBased(job) ? job.bytesDone ?? 0 : job.transferred;
  if (job.state === "completed") return 100;
  if (!total) return 0;
  return Math.min(100, Math.round((done / total) * 100));
}

export function applyProgress(jobs: Record<string, TransferJob>, event: TransferProgressEvent): TransferJob | undefined {
  const jobId = event.jobId || event.taskId;
  if (!jobId) return undefined;
  const existing = jobs[jobId];
  const kind: TransferKind = event.kind ?? (event.direction === "download" ? "download" : "upload");
  const next: TransferJob = existing
    ? {
        ...existing,
        state: event.state ?? existing.state,
        size: event.size ?? existing.size,
        transferred: event.transferred ?? existing.transferred,
        filesDone: event.filesDone ?? existing.filesDone,
        filesTotal: event.filesTotal ?? existing.filesTotal,
        bytesDone: event.bytesDone ?? existing.bytesDone,
        bytesTotal: event.bytesTotal ?? existing.bytesTotal,
        error: event.error ?? existing.error,
        updatedAt: Date.now(),
      }
    : {
        jobId,
        taskId: event.taskId,
        connectionId: event.connectionId ?? "",
        kind,
        remotePath: event.remotePath ?? event.fileName,
        state: event.state ?? "running",
        size: event.size ?? 0,
        transferred: event.transferred ?? 0,
        filesDone: event.filesDone,
        filesTotal: event.filesTotal,
        bytesDone: event.bytesDone,
        bytesTotal: event.bytesTotal,
        error: event.error,
        updatedAt: Date.now(),
      };
  jobs[jobId] = next;
  return next;
}

export interface TransferListResponse {
  jobs: Array<Record<string, unknown>>;
}

/** 把 files/transfers/list 的返回归一化进本地 job 表（进行中 + 历史）。 */
export function applyList(jobs: Record<string, TransferJob>, payload: TransferListResponse, connectionId: string): void {
  for (const raw of payload.jobs ?? []) {
    const jobId = String(raw.jobId ?? raw.taskId ?? "");
    if (!jobId) continue;
    const state = String(raw.state ?? raw.status ?? "queued") as TransferState;
    const kind = String(raw.kind ?? raw.direction ?? "upload") as TransferKind;
    jobs[jobId] = {
      jobId,
      taskId: raw.taskId ? String(raw.taskId) : undefined,
      connectionId: String(raw.connectionId ?? connectionId),
      kind,
      remotePath: raw.remotePath ? String(raw.remotePath) : raw.fileName ? String(raw.fileName) : undefined,
      state,
      // sidecar 单文件 job 用 totalBytes/transferredBytes，目录 job 用
      // bytesTotal/bytesDone，ssh-sftp 兼容形状用 size/transferred。
      size: Number(raw.size ?? raw.totalBytes ?? raw.bytesTotal ?? 0),
      transferred: Number(raw.transferred ?? raw.transferredBytes ?? raw.bytesDone ?? 0),
      filesDone: raw.filesDone === undefined ? undefined : Number(raw.filesDone),
      filesTotal: raw.filesTotal === undefined ? undefined : Number(raw.filesTotal),
      bytesDone: raw.bytesDone === undefined ? undefined : Number(raw.bytesDone),
      bytesTotal: raw.bytesTotal === undefined ? undefined : Number(raw.bytesTotal),
      error: raw.error ? String(raw.error) : undefined,
      updatedAt: Date.now(),
    };
  }
}

export function sortedJobs(jobs: Record<string, TransferJob>): TransferJob[] {
  return Object.values(jobs).sort((a, b) => {
    const aActive = isActive(a.state) ? 0 : 1;
    const bActive = isActive(b.state) ? 0 : 1;
    if (aActive !== bActive) return aActive - bActive;
    return b.updatedAt - a.updatedAt;
  });
}

export function createTransferTracker() {
  const jobs = reactive<Record<string, TransferJob>>({});
  return reactive({
    jobs,
    /** 进度事件入口（App 的 onEvent 转发到这里）。 */
    onProgress(event: TransferProgressEvent) {
      return applyProgress(jobs, event);
    },
    /** 轮询兜底：事件流缺失（旧宿主/断流）时以 list 为准。 */
    async refresh(invoke: <T>(method: string, params?: unknown) => Promise<T>, connectionId?: string) {
      const payload = await invoke<TransferListResponse>("files/transfers/list", { connectionId: connectionId ?? "" });
      applyList(jobs, payload, connectionId ?? "");
    },
    /** 乐观登记：start RPC 已返回但首个进度事件未到达前的占位。 */
    register(job: TransferJob) {
      jobs[job.jobId] = job;
      return job;
    },
    activeList(): TransferJob[] {
      return sortedJobs(jobs).filter((job) => isActive(job.state));
    },
    historyList(): TransferJob[] {
      return sortedJobs(jobs).filter((job) => !isActive(job.state));
    },
  });
}

export type TransferTracker = ReturnType<typeof createTransferTracker>;
