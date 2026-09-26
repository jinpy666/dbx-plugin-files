// 插件工作台 UI 持久化单点适配（宿主 host.storage / window.dbxPlugin.storage）。
//
// 背景：工作台 iframe 是 sandbox="allow-scripts"（opaque origin），插件代码里
// 直接读 localStorage 会抛 SecurityError（宿主 plugin_storage.rs 头注释同款
// 结论，ssh preferences 曾为此整体迁 sidecar）。宿主自 Host API 1.2 起提供
// window.dbxPlugin.storage（get/set/delete，能力位 capabilities.storage）：
// 桌面端落 plugin-data/<id>/ui-storage.json，web 宿主落顶层文档 localStorage，
// dev host 有同形 mock。宿主没有"列键"方法，因此每个插件在创建 store 时
// 声明自己的键集合，水合阶段逐键拉入缓存。
//
// 通道降级：宿主桥 storage → 直接 localStorage（浏览器直连 / 老宿主，guarded）
// → 内存（仅当前会话）。读全部同步（启动水合 + 写穿缓存），插件调用点
// 保持 getItem/setItem/removeItem 的 Web Storage 语义，零 async 改造。
// 宿主档水合时对 localStorage 旧值做一次性惰性搬家（读旧键 → 写穿宿主），
// 老 web 直连 / dev 场景的无感升级；桌面 opaque origin 下 localStorage
// 天然不可读，搬家自然跳过。
//
// 只存非敏感 UI 状态：host.storage 是 UI-state store（单值 256 KiB / 总量
// 1 MiB / 1024 键，宿主端强制），凭据仍走连接表单 binding:"secret"，
// 大数据归 sidecar 的 DBX_PLUGIN_DATA_DIR。

/** Web Storage 子集；localStorage 与本模块的 store 均满足该形状。 */
export type KvBacking = Pick<Storage, "getItem" | "setItem" | "removeItem">;

/** 宿主桥 storage 面（Host API 1.2；见 pluginHostBridge storage 命名空间）。 */
export interface DbxPluginStorageBridge {
  get(key: string): Promise<unknown>;
  set(key: string, value: unknown): Promise<unknown>;
  delete(key: string): Promise<unknown>;
}

export type PluginKvChannel = "host" | "localStorage" | "memory";

export interface PluginKvStoreOptions {
  /** 注入宿主桥；null 强制跳过宿主档，undefined 按 window.dbxPlugin 解析。 */
  bridge?: DbxPluginStorageBridge | null;
  /** 注入 localStorage 档；null 强制跳过，undefined 按 guarded window.localStorage 解析。 */
  localStorage?: KvBacking | null;
  /**
   * 隐式模式等待宿主桥出现及其 ready 结算的上限（毫秒）。桥晚于模块求值注入时
   * 限时轮询 window.dbxPlugin；ready 永不结算时超时降级，避免 main.ts 的启动
   * await 无限阻塞。默认与 App.waitForHostApi 同款 8s；显式注入不受影响。
   */
  hostReadyTimeoutMs?: number;
}

export interface PluginKvStore extends KvBacking {
  /** 水合完成（宿主档存量键读入缓存 + 旧值搬家）。挂载前 await，保证首读命中。 */
  ready: Promise<void>;
  /** 实际生效通道；"memory" 表示仅会话内有效（无桥且 localStorage 不可用）。 */
  readonly channel: PluginKvChannel;
}

interface ResolvedChannels {
  bridge: DbxPluginStorageBridge | null;
  fallback: KvBacking | null;
}

interface DbxPluginApi {
  ready?: Promise<unknown>;
  capabilities?: { storage?: boolean };
  storage?: DbxPluginStorageBridge;
}

function resolveHostApi(): DbxPluginApi | null {
  try {
    return (window as unknown as { dbxPlugin?: DbxPluginApi }).dbxPlugin ?? null;
  } catch {
    /* 无 window（单测 node 环境）：无宿主桥 */
    return null;
  }
}

/** 是否存在 window（node 单测环境无 window，无需轮询宿主注入）。 */
function hasWindow(): boolean {
  try {
    return typeof window !== "undefined" && window !== null;
  } catch {
    return false;
  }
}

function delay(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

/** 隐式宿主 ready 等待上限：与 App.waitForHostApi 的 deadline 语义对齐。 */
const IMPLICIT_HOST_READY_TIMEOUT_MS = 8000;
/** 隐式宿主桥出现轮询间隔：与 App.waitForHostApi 同款节拍。 */
const HOST_POLL_INTERVAL_MS = 50;

function resolveBridge(): DbxPluginStorageBridge | null {
  const api = resolveHostApi();
  if (api?.capabilities?.storage && api.storage) return api.storage;
  return null;
}

/** guarded localStorage：opaque origin 下访问即抛，这里把异常折断成 null/忽略。 */
function resolveFallback(): KvBacking | null {
  try {
    const ls = window.localStorage;
    ls.getItem("__dbx_plugin_storage_probe__");
    return {
      getItem: (key) => {
        try {
          return ls.getItem(key);
        } catch {
          return null;
        }
      },
      setItem: (key, value) => {
        try {
          ls.setItem(key, value);
        } catch {
          /* 配额/隐私模式：静默降级为会话内缓存 */
        }
      },
      removeItem: (key) => {
        try {
          ls.removeItem(key);
        } catch {
          /* 同上 */
        }
      },
    };
  } catch {
    return null;
  }
}

export function createPluginKvStore(keys: string[], options: PluginKvStoreOptions = {}): PluginKvStore {
  const cache = new Map<string, string>();
  // ready 结算前的同步写入先留在缓存；通道确定后统一写穿。每条记录入队时的
  // mutationVersion：flush 时被更新写入取代的 pending 不再重放，否则水合期间
  // 的直写会被旧值覆盖，持久层丢失更新。
  const pendingWrites = new Map<string, { value: string | null; mutationVersion: number }>();
  // 隐式模式在创建时快照宿主桥；桥可能晚于模块求值注入，ready 解析阶段会限时
  // 轮询 window.dbxPlugin（见 resolveAfterHostReady）。
  const explicitBridge = options.bridge !== undefined;
  const hostApi = explicitBridge ? null : resolveHostApi();
  let implicitHostFailed = false;
  // 隐式 ready 超时不是失败（宿主只是没就绪），但未就绪的 capability/storage
  // 同样不可信：超时后也不得把降级结论改判回 host。
  let hostExcluded = false;
  const mutationVersions = new Map<string, number>();
  let nextMutationVersion = 0;
  let readySettled = false;
  let resolved: ResolvedChannels | null = null;

  const ensure = (): ResolvedChannels => {
    if (!resolved) {
      resolved = {
        bridge: options.bridge !== undefined ? options.bridge : implicitHostFailed || hostExcluded ? null : resolveBridge(),
        fallback: options.localStorage !== undefined ? options.localStorage : resolveFallback(),
      };
    }
    return resolved;
  };

  const resolveAfterHostReady = async (): Promise<void> => {
    // 显式注入是测试/调用方明确选择，不等待全局宿主生命周期。
    if (explicitBridge) return;
    const deadline = Date.now() + (options.hostReadyTimeoutMs ?? IMPLICIT_HOST_READY_TIMEOUT_MS);
    // 桥可能晚于模块求值注入：限时轮询 dbxPlugin 出现再等 ready，而不是把
    // 创建时"暂无宿主"的一次性结论永久锁死（否则收藏会静默退化为会话内存储）。
    let api = hostApi;
    while (!api && hasWindow() && Date.now() < deadline) {
      await delay(HOST_POLL_INTERVAL_MS);
      api = resolveHostApi();
    }
    const hostReady = api?.ready;
    if (!hostReady) return;
    // ready 永不结算时 main.ts 的启动 await 会无限白屏：限时对冲，超时按
    // "暂无可用宿主存储"降级（与 App.initialize 的 Promise.any 策略同向）；
    // 拒绝仍是宿主初始化失败——残留 capability/storage 不是可用性证明，永久降级。
    let timedOut = false;
    try {
      await new Promise<void>((resolve, reject) => {
        const timer = setTimeout(() => {
          timedOut = true;
          resolve();
        }, Math.max(0, deadline - Date.now()));
        Promise.resolve(hostReady).then(
          () => {
            clearTimeout(timer);
            resolve();
          },
          (cause) => {
            clearTimeout(timer);
            reject(cause);
          },
        );
      });
    } catch {
      implicitHostFailed = true;
      return;
    }
    if (timedOut) hostExcluded = true;
  };

  /** 通道在首次操作时惰性判定：mock=1 注入 mock 宿主后创建的 store 也能命中桥。 */
  const channel = (): PluginKvChannel => {
    // 宿主仍在初始化时不得缓存降级结论；ready 后再给出最终通道。
    if (!readySettled && !explicitBridge) {
      if (!implicitHostFailed && !hostExcluded && resolveBridge()) return "host";
      return resolveFallback() ? "localStorage" : "memory";
    }
    const { bridge, fallback } = ensure();
    if (bridge) return "host";
    return fallback ? "localStorage" : "memory";
  };

  /** 写穿：host 桥异步投递（配额超限等失败仅告警，不阻断 UI）；localStorage 档同步。 */
  const persist = (key: string, value: string | null): void => {
    // ready 结算（含隐式轮询）前一律入队，避免误写入降级通道丢持久化。
    if (!readySettled) {
      pendingWrites.set(key, { value, mutationVersion: mutationVersions.get(key) ?? 0 });
      return;
    }
    const { bridge, fallback } = ensure();
    if (bridge) {
      const action = value === null ? bridge.delete(key) : bridge.set(key, value);
      void Promise.resolve()
        .then(() => action)
        .catch((error) => console.warn(`[pluginStorage] persist "${key}" failed`, error));
    } else if (fallback) {
      if (value === null) fallback.removeItem(key);
      else fallback.setItem(key, value);
    }
  };

  const ready = (async () => {
    await resolveAfterHostReady();
    readySettled = true;
    const mode = channel();
    if (mode === "localStorage") {
      // 直接 localStorage 档：同步水合（读取已在 ensure() 时验证可用）。
      const { fallback } = ensure();
      for (const key of keys) {
        const raw = fallback!.getItem(key);
        if (raw !== null && !cache.has(key)) cache.set(key, raw);
      }
    } else if (mode === "host") {
      const { bridge, fallback } = ensure();
      await Promise.all(
        keys.map(async (key) => {
          try {
            const value = await bridge!.get(key);
            if (value !== null && value !== undefined) {
              // 水合不覆盖水合前的写入（组件可能先渲染先写；pending 在队
              // 含 ready 前的 removeItem——宿主旧值不得复活）。
              if (!cache.has(key) && !pendingWrites.has(key)) cache.set(key, typeof value === "string" ? value : JSON.stringify(value));
              return;
            }
            // 宿主未命中：惰性搬家 localStorage 旧值（搬家只发生一次，写穿后旧档仍在，
            // 不删除——老宿主回退时数据可用）。
            const legacy = fallback?.getItem(key) ?? null;
            if (legacy !== null) {
              const mutationVersion = mutationVersions.get(key) ?? 0;
              // 同键在水合期间被用户改写（含 remove）时，不得恢复旧档值。
              if (!cache.has(key) && mutationVersion === 0) {
                cache.set(key, legacy);
                pendingWrites.set(key, { value: legacy, mutationVersion });
              }
            }
          } catch (error) {
            console.warn(`[pluginStorage] hydrate "${key}" failed`, error);
          }
        }),
      );
    }
    for (const [key, pending] of pendingWrites) {
      // 入队后被更新写入取代的 pending 不重放：直写已生效，重放旧值会覆盖新值。
      if (pending.mutationVersion !== (mutationVersions.get(key) ?? 0)) continue;
      persist(key, pending.value);
    }
    pendingWrites.clear();
  })();

  return {
    ready,
    get channel(): PluginKvChannel {
      return channel();
    },
    getItem(key: string): string | null {
      return cache.get(key) ?? null;
    },
    setItem(key: string, value: string): void {
      const normalized = String(value);
      cache.set(key, normalized);
      mutationVersions.set(key, ++nextMutationVersion);
      persist(key, normalized);
    },
    removeItem(key: string): void {
      cache.delete(key);
      mutationVersions.set(key, ++nextMutationVersion);
      persist(key, null);
    },
  };
}
