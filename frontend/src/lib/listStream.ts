// 流式目录列表（files/listStream P1）前端会话管理：
//
// 协议契约（与 backend sidecar 共享，字段名逐字固定）：
// 1) call("files/listStream", { connectionId, path }) → ack { requestId, displayCharset? }
// 2) call("files/listCancel", { requestId }) → { cancelled: boolean }（best-effort，不 await）
// 3) 事件 files/list/chunk（经 api.onEvent）：
//    { requestId, seq, entries, done, total?, error?, partialCount? }
//    - entries 为 FileEntry[]（camelCase，与 files/list 一致）
//    - 终止帧 done:true；失败帧 error 非空 + partialCount（失败帧 entries 恒空）
//
// 本模块只做会话纯逻辑：requestId 匹配（不匹配/迟到帧按契约丢弃）、seq 去重、
// entries 累加（normalize + displayName 装饰由调用方注入）、done/error 终态交付、
// abandon（新导航取代）时结算等待。渲染节奏与导航 token 联动留在 App.vue。

import { normalizeEntries, type FileEntry } from "./api";

/** files/listStream ack：requestId 供事件帧路由与 listCancel；FTP 显示字符集随 ack 下发。 */
export interface ListStreamAck {
  requestId: string;
  displayCharset?: string;
}

/** files/list/chunk 事件帧（api.onEvent 的 params）。 */
export interface ListStreamChunk {
  requestId: string;
  seq: number;
  entries: FileEntry[];
  done: boolean;
  total?: number;
  error?: string;
  partialCount?: number;
}

/** 会话终态结果（done / 失败帧 / abandon 三种来源）。 */
export interface ListStreamResult {
  /** 累加条目：done 为全量；失败帧保留已到部分；abandon 保留弃置前的累加。 */
  entries: FileEntry[];
  /** 失败帧为 true（App 走「已加载 N 项后失败」横幅 + listingFailed 路径）。 */
  failed: boolean;
  errorMessage?: string;
  partialCount?: number;
  /** 仅 ack 失败回落 files/list 时携带（listStream 本身无 truncated 概念）。 */
  truncated?: boolean;
  /** 会话被新导航弃置而结算：结果仅用于结束等待，调用方按导航 token 丢弃。 */
  stale?: boolean;
}

export interface ListStreamBrowseHandlers {
  /** ack 首次成功登记时回调一次（charset/connState 归属在调用方）。 */
  onAck?: (ack: ListStreamAck) => void;
  /** 每个有效 chunk 落地后回调：到达顺序累加数组（流式渲染直接可用）。 */
  onChunk?: (entries: FileEntry[]) => void;
}

/** 条目装饰：normalize + displayName（charset 取自 ack，由调用方注入实现）。 */
type Decorate = (entries: FileEntry[], charset: string) => FileEntry[];

/** 失败帧转义的领域错误：loadDirectory 据此把横幅切到「已加载 N 项后失败」。 */
export class ListStreamPartialError extends Error {
  readonly count: number;
  constructor(message: string, count: number) {
    super(message);
    this.name = "ListStreamPartialError";
    this.count = count;
  }
}

/**
 * 单次浏览的流式会话（一次 files/listStream 请求对应一个实例）。
 * `result` 在 done 帧 / 失败帧 / abandon 三种情况下都会结算，等待方不会悬挂。
 */
export function createListStreamBrowse(handlers: ListStreamBrowseHandlers & { decorate?: Decorate } = {}) {
  const decorate: Decorate = handlers.decorate ?? ((list) => normalizeEntries(list));
  const onAck = handlers.onAck;
  const onChunk = handlers.onChunk;

  let ack: ListStreamAck | null = null; // 已登记的 ack（事件帧 requestId 匹配依据）
  let lastSeq = -1; // 最近一次接受的 seq（去重/迟到帧丢弃）
  let accumulated: FileEntry[] = []; // chunk 累加器（到达顺序）
  let terminal = false; // 已进入终态（done/失败帧/abandon），后续帧一律拒绝
  let abandoned = false; // 已被新导航弃置
  let settle!: (result: ListStreamResult) => void;
  const result = new Promise<ListStreamResult>((resolve) => {
    settle = resolve;
  });

  return {
    /** 终态 promise：done / 失败帧 / abandon 都会结算（abandon 带 stale 标记）。 */
    result,
    /** ack 登记后的 requestId（未登记为 null；App 据此把事件帧路由到本会话）。 */
    get requestId(): string | null {
      return ack?.requestId ?? null;
    },
    /** 会话是否已结束（终态或弃置）：abandonStream 的幂等判断用。 */
    get finished(): boolean {
      return terminal || abandoned;
    },
    /** 累加条目数（快照/调试用）。 */
    get count(): number {
      return accumulated.length;
    },
    /**
     * 事件帧入口（App 按 requestId 路由后调用），返回是否被本会话接受。
     * 未登记 ack、requestId 不匹配、迟到/重复帧（seq <= lastSeq）、终态后、
     * 弃置后的帧一律拒绝（契约：requestId 不匹配丢弃）。
     */
    handleChunk(chunk: ListStreamChunk): boolean {
      if (!ack || abandoned || terminal) return false;
      if (chunk.requestId !== ack.requestId) return false;
      if (!Number.isFinite(chunk.seq) || chunk.seq <= lastSeq) return false;
      lastSeq = chunk.seq;
      accumulated = accumulated.concat(decorate(chunk.entries ?? [], ack.displayCharset ?? ""));
      if (chunk.error) {
        // 失败帧：保留已到条目 + partialCount（失败帧 entries 恒空，不计入）。
        terminal = true;
        settle({ entries: accumulated, failed: true, errorMessage: chunk.error, partialCount: chunk.partialCount ?? accumulated.length });
        return true;
      }
      if (chunk.done) {
        terminal = true;
        settle({ entries: accumulated, failed: false });
        return true;
      }
      onChunk?.(accumulated);
      return true;
    },
    /**
     * ack 登记：幂等；弃置后迟到的 ack 不再登记。
     * 返回 false 表示会话已被弃置（调用方应对该 requestId 发 best-effort 取消）。
     */
    register(value: ListStreamAck): boolean {
      if (ack || abandoned) return false;
      ack = value;
      onAck?.(value);
      return true;
    },
    /**
     * 新导航弃置本会话：后续帧一律拒绝并结算等待（stale:true，等待方不悬挂）。
     * 返回已登记的 requestId（供 best-effort files/listCancel），未登记为 null。
     */
    abandon(): string | null {
      if (abandoned) return null;
      abandoned = true;
      if (!terminal) {
        terminal = true;
        settle({ entries: accumulated, failed: false, stale: true });
      }
      return ack?.requestId ?? null;
    },
  };
}

export type ListStreamBrowse = ReturnType<typeof createListStreamBrowse>;

// ---- ack 前到达帧的预缓冲（真实宿主桥竞态兜底）-----------------------------
//
// 真实 sidecar 在返回 ack 前就 spawn 会话任务（main.rs 的 handler 先注册再
// tokio::spawn），快路径的 done 帧或流式首帧可能先于 ack 应答写入宿主桥
// （同一 stdout 通道内的写入顺序无保证）。此时 requestId 尚未登记，按契约
// 「不匹配丢弃」会让会话永久悬挂。缓冲层按 requestId 暂存无人认领的帧，
// ack 登记后原序重放；容量/TTL 双上限防泄漏（回落/伪造帧自动过期）。

const HOLDACK_FRAME_CAP = 64; // 单 requestId 最多暂存帧数（64 × ≤256 条目）
const HOLDACK_TTL_MS = 5000; // 暂存寿命：ack 永远不来（回落/伪造）时自动过期
const HOLDACK_REQUEST_CAP = 8; // 并发暂存的 requestId 数（双栏场景远用不满）

export function createChunkHoldback(now: () => number = () => Date.now()) {
  const buckets = new Map<string, { frames: ListStreamChunk[]; at: number }>();
  const sweep = () => {
    const stamp = now();
    for (const [id, bucket] of buckets) if (stamp - bucket.at > HOLDACK_TTL_MS) buckets.delete(id);
  };
  return {
    /** ack 前到达且无人认领的帧：按 requestId 暂存（FIFO，超上限丢新帧保头部）。 */
    push(chunk: ListStreamChunk): void {
      sweep();
      let bucket = buckets.get(chunk.requestId);
      if (!bucket) {
        if (buckets.size >= HOLDACK_REQUEST_CAP) {
          const oldest = [...buckets.entries()].sort(([, a], [, b]) => a.at - b.at)[0];
          if (oldest) buckets.delete(oldest[0]);
        }
        bucket = { frames: [], at: now() };
        buckets.set(chunk.requestId, bucket);
      }
      bucket.at = now();
      if (bucket.frames.length < HOLDACK_FRAME_CAP) bucket.frames.push(chunk);
    },
    /** ack 登记后原序取出并清除该 requestId 的暂存（乱序由 seq 单调检查兜底）。 */
    drain(requestId: string): ListStreamChunk[] {
      const bucket = buckets.get(requestId);
      if (!bucket) return [];
      buckets.delete(requestId);
      return bucket.frames;
    },
    /** 会话弃置时主动清理（不等 TTL）。 */
    drop(requestId: string): void {
      buckets.delete(requestId);
    },
  };
}

export type ChunkHoldback = ReturnType<typeof createChunkHoldback>;
