// 薄 spec：验证 shared/frontend/pluginStorage 适配器行为与本插件工具链下
// import 解析成立（实现与文档只在 shared 维护）。
import { afterEach, describe, expect, it, vi } from "vitest";
import { createPluginKvStore, type DbxPluginStorageBridge, type KvBacking } from "../../../shared/frontend/pluginStorage";
import { PREFS_STORE_KEYS, prefsStore, UI_PREFS_KEY } from "./prefs";

function memoryBacking(initial: Record<string, string> = {}): KvBacking {
  const map = new Map(Object.entries(initial));
  return {
    getItem: (key) => map.get(key) ?? null,
    setItem: (key, value) => map.set(key, String(value)),
    removeItem: (key) => map.delete(key),
  };
}

function bridgeBacking(initial: Record<string, unknown> = {}): DbxPluginStorageBridge & { map: Map<string, unknown> } {
  const map = new Map(Object.entries(initial));
  return {
    map,
    get: async (key) => (map.has(key) ? structuredClone(map.get(key)) : null),
    set: async (key, value) => {
      map.set(key, value);
      return null;
    },
    delete: async (key) => {
      map.delete(key);
      return null;
    },
  };
}

function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((settle) => {
    resolve = settle;
  });
  return { promise, resolve };
}

afterEach(() => {
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
});

describe("pluginStorage host channel", () => {
  it("hydrates existing host values and serves sync reads", async () => {
    const bridge = bridgeBacking({ "dbx-files.ui": '{"sort":{"column":"size","direction":"desc"}}', legacy: { embedded: true } });
    const store = createPluginKvStore(["dbx-files.ui", "legacy"], { bridge, localStorage: null });
    await store.ready;
    expect(store.getItem("dbx-files.ui")).toContain('"column":"size"');
    // 非字符串 JSON 值（宿主端为任意 JSON 文档）序列化进字符串缓存。
    expect(store.getItem("legacy")).toBe('{"embedded":true}');
  });

  it("writes through set/remove to the bridge", async () => {
    const bridge = bridgeBacking();
    const store = createPluginKvStore(["k"], { bridge, localStorage: null });
    await store.ready;
    store.setItem("k", "v1");
    expect(store.getItem("k")).toBe("v1");
    await Promise.resolve();
    expect(bridge.map.get("k")).toBe("v1");
    store.removeItem("k");
    expect(store.getItem("k")).toBeNull();
    await Promise.resolve();
    expect(bridge.map.has("k")).toBe(false);
  });

  it("adopts legacy localStorage values once when the host key is missing", async () => {
    const bridge = bridgeBacking();
    const ls = memoryBacking({ fav: '["/docs"]' });
    const store = createPluginKvStore(["fav"], { bridge, localStorage: ls });
    await store.ready;
    expect(store.getItem("fav")).toBe('["/docs"]');
    await Promise.resolve();
    // 搬家 = 读旧值写穿宿主；旧档保留不删（老宿主回退仍可读）。
    expect(bridge.map.get("fav")).toBe('["/docs"]');
    expect(ls.getItem("fav")).toBe('["/docs"]');
  });

  it("keeps cache writes made before hydration (hydration never overwrites)", async () => {
    const bridge = bridgeBacking({ k: "from-host" });
    const store = createPluginKvStore(["k"], { bridge, localStorage: null });
    store.setItem("k", "from-ui");
    await store.ready;
    expect(store.getItem("k")).toBe("from-ui");
  });

  it("survives bridge get/set failures with a warning", async () => {
    const warn = vi.spyOn(console, "warn").mockImplementation(() => undefined);
    const store = createPluginKvStore(["boom"], {
      bridge: {
        get: async () => {
          throw new Error("quota");
        },
        set: async () => {
          throw new Error("quota");
        },
        delete: async () => {
          throw new Error("quota");
        },
      },
      localStorage: null,
    });
    await store.ready;
    expect(() => store.setItem("boom", "x")).not.toThrow();
    expect(store.getItem("boom")).toBe("x");
    expect(warn).toHaveBeenCalled();
  });
});

describe("pluginStorage default host bridge timing", () => {
  it("waits for a present host bridge to become storage-capable before hydrating and writing through", async () => {
    let resolveReady!: () => void;
    const ready = new Promise<void>((resolve) => {
      resolveReady = resolve;
    });
    const bridge = bridgeBacking({ fav: '["/persisted"]' });
    vi.stubGlobal("window", {
      dbxPlugin: {
        ready,
        capabilities: {},
      },
    });

    const store = createPluginKvStore(["fav"]);
    store.setItem("written-before-ready", "keep-me");
    (window as unknown as { dbxPlugin: { capabilities: { storage: boolean }; storage: DbxPluginStorageBridge } }).dbxPlugin.capabilities.storage = true;
    (window as unknown as { dbxPlugin: { storage: DbxPluginStorageBridge } }).dbxPlugin.storage = bridge;
    resolveReady();

    await store.ready;
    expect(store.channel).toBe("host");
    expect(store.getItem("fav")).toBe('["/persisted"]');
    expect(bridge.map.get("written-before-ready")).toBe("keep-me");
  });

  it("prefers host storage after ready even when localStorage is injected", async () => {
    let resolveReady!: () => void;
    const ready = new Promise<void>((resolve) => {
      resolveReady = resolve;
    });
    const bridge = bridgeBacking();
    const fallback = memoryBacking();
    vi.stubGlobal("window", {
      dbxPlugin: {
        ready,
        capabilities: {},
      },
    });

    const store = createPluginKvStore(["fav"], { localStorage: fallback });
    store.setItem("fav", "from-ui");
    (window as unknown as { dbxPlugin: { capabilities: { storage: boolean }; storage: DbxPluginStorageBridge } }).dbxPlugin.capabilities.storage = true;
    (window as unknown as { dbxPlugin: { storage: DbxPluginStorageBridge } }).dbxPlugin.storage = bridge;
    resolveReady();

    await store.ready;
    expect(store.channel).toBe("host");
    expect(bridge.map.get("fav")).toBe("from-ui");
    expect(fallback.getItem("fav")).toBeNull();
  });
});

describe("pluginStorage hydration write ordering", () => {
  function migrationBarrier() {
    const legacyRead = deferred<void>();
    const slowGet = deferred<unknown>();
    const writes: Array<[string, string | null]> = [];
    const fallback: KvBacking = {
      getItem: (key) => {
        if (key !== "fav") return null;
        legacyRead.resolve();
        return "legacy";
      },
      setItem: () => undefined,
      removeItem: () => undefined,
    };
    const bridge: DbxPluginStorageBridge = {
      get: async (key) => (key === "fav" ? null : slowGet.promise),
      set: async (key, value) => {
        writes.push([key, String(value)]);
        return null;
      },
      delete: async (key) => {
        writes.push([key, null]);
        return null;
      },
    };
    const store = createPluginKvStore(["fav", "slow"], { bridge, localStorage: fallback });
    return { legacyRead, slowGet, writes, store };
  }

  it("persists a set made after legacy migration is queued instead of the legacy value", async () => {
    const { legacyRead, slowGet, store, writes } = migrationBarrier();
    // fav 已从 host 读到 null，且 getItem 已读取 legacy 并把它加入 pendingWrites；
    // slow 的 get 仍挂起，因此 ready 尚未完成。
    await legacyRead.promise;
    store.setItem("fav", "from-ui");
    slowGet.resolve(null);

    await store.ready;
    await Promise.resolve();
    expect(store.getItem("fav")).toBe("from-ui");
    expect(writes).toEqual([["fav", "from-ui"]]);
  });

  it("persists a removal made after legacy migration is queued instead of restoring the legacy value", async () => {
    const { legacyRead, slowGet, store, writes } = migrationBarrier();
    // 与 set 用例相同的屏障：legacy 已排队，但 ready 仍被 slow key 阻塞。
    await legacyRead.promise;
    store.removeItem("fav");
    slowGet.resolve(null);

    await store.ready;
    await Promise.resolve();
    expect(store.getItem("fav")).toBeNull();
    expect(writes).toEqual([["fav", null]]);
  });
});

describe("pluginStorage host injection robustness", () => {
  function hydrationBarrier() {
    // ready 结算后进入水合、bridge.get 尚未返回的屏障：此刻的写入直写桥，
    // 用于构造「pending 重放 vs 水合期间新写入」的竞态窗口。
    const map = new Map<string, unknown>();
    const hydrating = deferred<void>();
    const releaseGet = deferred<unknown>();
    const bridge: DbxPluginStorageBridge = {
      get: async (key) => {
        if (key !== "k") return null;
        hydrating.resolve();
        return releaseGet.promise;
      },
      set: async (key, value) => {
        map.set(key, value);
        return null;
      },
      delete: async (key) => {
        map.delete(key);
        return null;
      },
    };
    return { map, hydrating, releaseGet, bridge };
  }

  it("does not replay a stale pre-ready write over a newer write made during hydration", async () => {
    let resolveReady!: () => void;
    const ready = new Promise<void>((resolve) => {
      resolveReady = resolve;
    });
    const { map, hydrating, releaseGet, bridge } = hydrationBarrier();
    vi.stubGlobal("window", {
      dbxPlugin: { ready, capabilities: {} },
    });

    const store = createPluginKvStore(["k"]);
    store.setItem("k", "v1");
    (window as unknown as { dbxPlugin: { capabilities: { storage: boolean }; storage: DbxPluginStorageBridge } }).dbxPlugin.capabilities.storage = true;
    (window as unknown as { dbxPlugin: { storage: DbxPluginStorageBridge } }).dbxPlugin.storage = bridge;
    resolveReady();
    await hydrating.promise;
    // 水合挂起期间的写入直写桥（v2）；flush 不得再重放 v1 盖掉它。
    store.setItem("k", "v2");
    releaseGet.resolve(null);

    await store.ready;
    expect(store.getItem("k")).toBe("v2");
    expect(map.get("k")).toBe("v2");
  });

  it("does not resurrect a removed key from host hydration when removal happened before ready", async () => {
    let resolveReady!: () => void;
    const ready = new Promise<void>((resolve) => {
      resolveReady = resolve;
    });
    const { map, hydrating, releaseGet, bridge } = hydrationBarrier();
    vi.stubGlobal("window", {
      dbxPlugin: { ready, capabilities: {} },
    });

    const store = createPluginKvStore(["k"]);
    store.removeItem("k");
    (window as unknown as { dbxPlugin: { capabilities: { storage: boolean }; storage: DbxPluginStorageBridge } }).dbxPlugin.capabilities.storage = true;
    (window as unknown as { dbxPlugin: { storage: DbxPluginStorageBridge } }).dbxPlugin.storage = bridge;
    map.set("k", "host-value");
    resolveReady();
    await hydrating.promise;
    releaseGet.resolve("host-value");

    await store.ready;
    expect(store.getItem("k")).toBeNull();
    await Promise.resolve();
    expect(map.has("k")).toBe(false);
  });

  it("adopts a host bridge injected after store creation within the ready deadline", async () => {
    const bridge = bridgeBacking({ fav: '["late"]' });
    vi.stubGlobal("window", {} as unknown as Window & { dbxPlugin?: unknown });

    const store = createPluginKvStore(["fav"], { localStorage: null, hostReadyTimeoutMs: 2000 });
    setTimeout(() => {
      (window as unknown as { dbxPlugin: unknown }).dbxPlugin = {
        ready: Promise.resolve(),
        capabilities: { storage: true },
        storage: bridge,
      };
    }, 20);

    await store.ready;
    expect(store.channel).toBe("host");
    expect(store.getItem("fav")).toBe('["late"]');
  });

  it("settles ready to the fallback channel when the implicit host ready never resolves", async () => {
    const fallback = memoryBacking();
    vi.stubGlobal("window", {
      dbxPlugin: { ready: new Promise<void>(() => undefined), capabilities: {} },
    });

    const store = createPluginKvStore(["k"], { localStorage: fallback, hostReadyTimeoutMs: 40 });
    await expect(store.ready).resolves.toBeUndefined();
    store.setItem("k", "after-timeout");
    expect(store.channel).toBe("localStorage");
    expect(fallback.getItem("k")).toBe("after-timeout");
  });
});

describe("pluginStorage degraded channels", () => {
  it("uses localStorage synchronously when no bridge exists", async () => {
    const ls = memoryBacking({ a: "1" });
    const store = createPluginKvStore(["a", "b"], { bridge: null, localStorage: ls });
    await store.ready;
    expect(store.channel).toBe("localStorage");
    expect(store.getItem("a")).toBe("1");
    store.setItem("b", "2");
    expect(ls.getItem("b")).toBe("2");
    store.removeItem("a");
    expect(ls.getItem("a")).toBeNull();
  });

  it("falls back to memory when neither channel exists", async () => {
    const store = createPluginKvStore(["k"], { bridge: null, localStorage: null });
    await store.ready;
    expect(store.channel).toBe("memory");
    store.setItem("k", "session");
    expect(store.getItem("k")).toBe("session");
  });

  it("settles to injected localStorage when host ready rejects without propagating the rejection", async () => {
    const fallback = memoryBacking();
    const ready = new Promise<void>((_resolve, reject) => queueMicrotask(() => reject(new Error("host initialization failed"))));
    vi.stubGlobal("window", {
      dbxPlugin: {
        ready,
        capabilities: {},
      },
    });

    const store = createPluginKvStore(["k"], { localStorage: fallback });
    await expect(store.ready).resolves.toBeUndefined();
    store.setItem("k", "fallback");
    await Promise.resolve();

    expect(store.channel).toBe("localStorage");
    expect(fallback.getItem("k")).toBe("fallback");
  });

  it("permanently disables implicit host storage when ready rejects despite a residual bridge", async () => {
    const fallback = memoryBacking();
    const bridge = {
      get: vi.fn(async () => "stale"),
      set: vi.fn(async () => null),
      delete: vi.fn(async () => null),
    } satisfies DbxPluginStorageBridge;
    const ready = new Promise<void>((_resolve, reject) => queueMicrotask(() => reject(new Error("host initialization failed"))));
    vi.stubGlobal("window", {
      dbxPlugin: {
        ready,
        capabilities: { storage: true },
        storage: bridge,
      },
    });

    const store = createPluginKvStore(["k"], { localStorage: fallback });
    store.setItem("k", "written-before-reject");
    await expect(store.ready).resolves.toBeUndefined();
    await Promise.resolve();

    expect(store.channel).toBe("localStorage");
    expect(store.getItem("k")).toBe("written-before-reject");
    expect(fallback.getItem("k")).toBe("written-before-reject");
    expect(bridge.get).not.toHaveBeenCalled();
    expect(bridge.set).not.toHaveBeenCalled();
    expect(bridge.delete).not.toHaveBeenCalled();
  });

  it("uses normal fallback when window exists without dbxPlugin", async () => {
    const fallback = memoryBacking({ k: "legacy" });
    vi.stubGlobal("window", {});

    const store = createPluginKvStore(["k"], { localStorage: fallback, hostReadyTimeoutMs: 30 });
    await store.ready;

    expect(store.channel).toBe("localStorage");
    expect(store.getItem("k")).toBe("legacy");
  });
});

describe("files prefsStore wiring", () => {
  it("declares every persisted prefs key and resolves to memory in node tests", async () => {
    expect(new Set(PREFS_STORE_KEYS)).toEqual(new Set([UI_PREFS_KEY, "dbx-files.downloadDir", "dbx-files.openApp", "dbx-files.favorites", "dbx-files.conflictPolicy"]));
    // node 环境无 window：默认解析应落到内存档而不是抛错。
    expect(prefsStore.channel).toBe("memory");
    await prefsStore.ready;
  });
});
