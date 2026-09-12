// 传输 job 状态仓：事件驱动（files/transfer/progress）为主，轮询兜底
// （files/transfers/list）。进度节流由 sidecar 负责（≥200ms 或 ≥1%），
// 前端只负责渲染。语义对齐 tiny-rdm transferRuntime.js 的重写版。

import { reactive } from "vue";
import { formatBytes } from "./api";

export type TransferState = "queued" | "running" | "completed" | "failed" | "canceled";

export type TransferKind = "upload" | "download" | "copyDir" | "syncDir" | "copy" | "move" | "rename" | "extract" | "compress" | "delete";

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
  /** 平滑传输速率（字节/秒）。仅由 tracker 的速率采样器在活动 job 上维护。 */
  rateBps?: number;
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
    const existing = jobs[jobId];
    jobs[jobId] = {
      jobId,
      taskId: raw.taskId ? String(raw.taskId) : undefined,
      connectionId: String(raw.connectionId ?? connectionId),
      kind,
      // P2-8：list 缺 remotePath 时保留本地现值（undefined 不覆盖）——提交时
      // 登记的可读标题不被轮询兜底覆盖成裸 jobId（mock 曾缺该字段，暴露此问题）。
      remotePath: raw.remotePath ? String(raw.remotePath) : raw.fileName ? String(raw.fileName) : existing?.remotePath,
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

// ---------------------------------------------------------------------------
// 速率采样（P-FILES ⑥：速率显示准确性）
//
// 进度事件节流在 sidecar（≥200ms 或 ≥1%），事件间隔不均匀，直接用相邻两点
// 算瞬时速率会抖动；这里用指数滑动平均（EWMA, α=0.35）平滑。时间由调用方
// 注入，单测可注入固定时钟。
// ---------------------------------------------------------------------------

const RATE_EWMA_ALPHA = 0.35;

interface RateSample {
  lastBytes: number;
  lastTs: number;
  rateBps: number;
}

export class RateSampler {
  private samples = new Map<string, RateSample>();

  /** 记录一次累计字节采样，返回平滑速率（首次采样返回 undefined——还无从计算）。 */
  sample(jobId: string, bytes: number, now: number): number | undefined {
    const prev = this.samples.get(jobId);
    if (!prev) {
      this.samples.set(jobId, { lastBytes: bytes, lastTs: now, rateBps: 0 });
      return undefined;
    }
    // 累计字节回退（新 job 复用 id / 状态重置）→ 重新基线，不产出速率。
    if (bytes < prev.lastBytes) {
      this.samples.set(jobId, { lastBytes: bytes, lastTs: now, rateBps: 0 });
      return undefined;
    }
    const dtMs = now - prev.lastTs;
    const db = bytes - prev.lastBytes;
    let rate = prev.rateBps;
    if (dtMs > 0 && db > 0) {
      const instant = (db * 1000) / dtMs;
      rate = prev.rateBps > 0 ? RATE_EWMA_ALPHA * instant + (1 - RATE_EWMA_ALPHA) * prev.rateBps : instant;
    }
    this.samples.set(jobId, { lastBytes: bytes, lastTs: now, rateBps: rate });
    return rate;
  }

  reset(jobId: string): void {
    this.samples.delete(jobId);
  }
}

/** 当前 job 用于速率采样的累计字节数（目录 job 用 bytesDone，单文件用 transferred）。 */
export function progressBytes(job: TransferJob): number {
  return isByteBased(job) ? job.bytesDone ?? 0 : job.transferred;
}

/** 运行中 job 的预计剩余秒数；速率未知或已无可传输字节时返回 undefined。 */
export function etaSeconds(job: TransferJob): number | undefined {
  if (job.state !== "running" || !job.rateBps || job.rateBps <= 0) return undefined;
  const total = isByteBased(job) ? job.bytesTotal ?? 0 : job.size;
  const done = isByteBased(job) ? job.bytesDone ?? 0 : job.transferred;
  const remaining = Math.max(0, total - done);
  if (remaining <= 0) return undefined;
  return Math.max(1, Math.ceil(remaining / job.rateBps));
}

/** `1234567` → `"1.2 MB/s"`（复用 formatBytes，单位与分隔符语言无关）。 */
export function formatRate(bps: number): string {
  return `${formatBytes(bps)}/s`;
}

/** 剩余秒数 → `"45s" / "3m12s" / "1h04m"`。 */
export function formatEta(seconds: number): string {
  if (seconds < 60) return `${seconds}s`;
  const mins = Math.floor(seconds / 60);
  if (mins < 60) return `${mins}m${String(seconds % 60).padStart(2, "0")}s`;
  return `${Math.floor(mins / 60)}h${String(mins % 60).padStart(2, "0")}m`;
}

export function createTransferTracker() {
  const jobs = reactive<Record<string, TransferJob>>({});
  const sampler = new RateSampler();
  /** 速率采样 + 终态清理统一入口：事件与轮询、本地上传/下载泵都走这里。 */
  function trackProgress(event: TransferProgressEvent): TransferJob | undefined {
    const job = applyProgress(jobs, event);
    if (!job) return undefined;
    if (isActive(job.state)) {
      const rate = sampler.sample(job.jobId, progressBytes(job), Date.now());
      if (rate !== undefined) jobs[job.jobId] = { ...job, rateBps: rate };
    } else {
      sampler.reset(job.jobId);
      if (job.rateBps !== undefined) jobs[job.jobId] = { ...job, rateBps: undefined };
    }
    return jobs[job.jobId] ?? job;
  }
  return reactive({
    jobs,
    /** 进度事件入口（App 的 onEvent 转发到这里）。 */
    onProgress(event: TransferProgressEvent) {
      return trackProgress(event);
    },
    /** 轮询兜底：事件流缺失（旧宿主/断流）时以 list 为准。 */
    async refresh(invoke: <T>(method: string, params?: unknown) => Promise<T>, connectionId?: string) {
      const payload = await invoke<TransferListResponse>("files/transfers/list", connectionId === undefined ? {} : { connectionId });
      applyList(jobs, payload, connectionId ?? "");
      // 轮询同样喂给速率采样器（轮询间隔不均，EWMA 平滑后仍可用）。
      for (const job of Object.values(jobs)) {
        if (!isActive(job.state)) {
          sampler.reset(job.jobId);
          continue;
        }
        const rate = sampler.sample(job.jobId, progressBytes(job), Date.now());
        if (rate !== undefined) jobs[job.jobId] = { ...job, rateBps: rate };
      }
    },
    /** 乐观登记：start RPC 已返回但首个进度事件未到达前的占位。 */
    register(job: TransferJob) {
      jobs[job.jobId] = job;
      return job;
    },
    /** 清理本地完成态 job（历史）；活动 job 不受影响。 */
    clearFinished(): number {
      let removed = 0;
      for (const [jobId, job] of Object.entries(jobs)) {
        if (!isActive(job.state)) {
          sampler.reset(jobId);
          delete jobs[jobId];
          removed += 1;
        }
      }
      return removed;
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

// ---------------------------------------------------------------------------
// 失败任务重试（P-FILES ⑦）
//
// 只有与后端方法一一对应、可原样重发的 job 才可重试（copy/move/rename/
// syncDir/copyDir）；upload/download/extract 的原始请求结构不同，不在此列
// （App 侧还会校验提交时登记的 retry 参数，双保险）。
// ---------------------------------------------------------------------------

const RETRYABLE_KINDS: ReadonlySet<string> = new Set(["copy", "move", "rename", "syncDir", "copyDir"]);

export function isRetryableKind(kind: string): boolean {
  return RETRYABLE_KINDS.has(kind);
}
