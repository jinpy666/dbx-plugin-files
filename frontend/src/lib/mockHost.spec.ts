// @vitest-environment happy-dom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

// R3-P2-2 回归：mock files/delete / files/purge 必须按 connectionId 路由到
// 对应树——此前写死远端树，双栏左栏（__local__）删除「通知成功但条目原样存在」。

async function setup(query = "") {
  window.history.replaceState(null, "", `/?mock=1${query}`);
  const { installMockHost } = await import("./mockHost");
  installMockHost();
  return window.dbxPlugin!;
}

function names(payload: unknown): string[] {
  const entries = (payload as { entries: Array<{ name: string }> }).entries ?? [];
  return entries.map((entry) => entry.name);
}

describe("mockHost host.* contract", () => {
  it("host.listConnections exposes both built-in connections with names", async () => {
    const api = await setup();
    const list = await api.request<Array<{ id: string; name: string }>>("host.listConnections");
    const ids = list.map((item) => item.id);
    expect(ids).toContain("mock-conn");
    expect(ids).toContain("__local__");
    for (const item of list) expect(item.name).toBeTruthy();
  });

  it("keeps rejecting unsupported host methods", async () => {
    const api = await setup();
    await expect(api.request("host.unknownMethod")).rejects.toThrow("Unsupported plugin host method");
  });
});

describe("mockHost delete/purge connection routing (R3-P2-2)", () => {
  it("files/delete with connectionId=__local__ removes from the local tree only", async () => {
    const api = await setup();
    await api.invoke("files/mkdir", { path: "/probe-del", connectionId: "__local__" });
    expect(names(await api.invoke("files/list", { path: "/", connectionId: "__local__" }))).toContain("probe-del");

    await api.invoke("files/delete", { path: "/probe-del", connectionId: "__local__" });

    expect(names(await api.invoke("files/list", { path: "/", connectionId: "__local__" }))).not.toContain("probe-del");
    // 远端树从未有该条目，也不因误路由被错误改动
    expect(names(await api.invoke("files/list", { path: "/" }))).not.toContain("probe-del");
  });

  it("files/purge with connectionId=__local__ removes the local subtree", async () => {
    const api = await setup();
    await api.invoke("files/mkdir", { path: "/probe-purge", connectionId: "__local__" });
    await api.invoke("files/write", { path: "/probe-purge/child.txt", dataBase64: "", connectionId: "__local__" });

    await api.invoke("files/purge", { path: "/probe-purge", connectionId: "__local__" });

    expect(names(await api.invoke("files/list", { path: "/", connectionId: "__local__" }))).not.toContain("probe-purge");
  });

  it("default connectionId keeps routing to the remote tree", async () => {
    const api = await setup();
    await api.invoke("files/mkdir", { path: "/probe-remote-del" });
    expect(names(await api.invoke("files/list", { path: "/" }))).toContain("probe-remote-del");
    await api.invoke("files/delete", { path: "/probe-remote-del" });
    expect(names(await api.invoke("files/list", { path: "/" }))).not.toContain("probe-remote-del");
  });
});

describe("mockHost query and lifecycle contracts", () => {
  beforeEach(() => {
    Reflect.deleteProperty(window, "dbxPlugin");
    vi.useFakeTimers();
  });
  afterEach(() => {
    vi.clearAllTimers();
    vi.useRealTimers();
    Reflect.deleteProperty(window, "dbxPlugin");
  });

  const connection = (id: string) => ({ connection: { id, external_config: { protocol: "fs", root: "/mock" } } });

  it("records failed upload completion as failed rather than canceled", async () => {
    const api = await setup();
    const { taskId } = await api.invoke<{ taskId: string }>("files/upload/start", { connectionId: "mock-conn", remotePath: "/error/.~超市电费.xlsx", size: 0 });
    await expect(api.invoke("files/upload/finish", { taskId })).rejects.toThrow("mock backend failure");
    expect(await api.invoke("files/transfer/status", { jobId: taskId })).toMatchObject({
      job: { status: "failed", error: expect.stringContaining("mock backend failure") },
    });
    await expect(api.invoke("files/transfer/cancel", { taskId })).rejects.toThrow("Transfer task was not found");
  });

  it("attributes local writes and upload completion to their original connection", async () => {
    const api = await setup();
    await api.invoke("files/mkdir", { path: "/remote" });
    await api.invoke("files/mkdir", { connectionId: "__local__", path: "/local" });
    await api.invoke("files/write", { connectionId: "__local__", path: "/local/a", dataBase64: "" });
    await api.invoke("files/rename", { connectionId: "__local__", path: "/local/a", newPath: "/local/b" });
    const { taskId } = await api.invoke<{ taskId: string }>("files/upload/start", { connectionId: "__local__", remotePath: "/local/upload", size: 0 });
    await api.invoke("files/upload/finish", { taskId });
    const local = await api.invoke<{ entries: Array<Record<string, unknown>> }>("files/audit/list", { connectionId: "__local__" });
    expect(local.entries.map((entry) => entry.action)).toEqual(["files/upload", "files/rename", "files/write", "files/mkdir"]);
    expect(local.entries.every((entry) => entry.connectionId === "__local__")).toBe(true);
    expect(await api.invoke("files/audit/list", { connectionId: "mock-conn" })).toMatchObject({ entries: [{ path: "/remote", connectionId: "mock-conn" }] });
  });

  it.each(["2", -1, 0.5, true, {}, [], 2 ** 64].map((limit) => ({ limit })))("rejects an audit limit outside the JSON unsigned-integer contract: $limit", async ({ limit }) => {
    const api = await setup();
    await expect(api.invoke("files/audit/list", { limit })).rejects.toThrow("limit must be a non-negative integer");
  });

  it("defaults and clamps audit limits while retaining newest-first ordering", async () => {
    const api = await setup();
    for (let i = 0; i < 105; i++) await api.invoke("files/mkdir", { path: `/entry-${i}` });
    const query = (limit?: unknown) => api.invoke<{ entries: Array<{ path: string }> }>("files/audit/list", { limit });
    expect((await query()).entries).toHaveLength(100);
    expect((await query(null)).entries).toHaveLength(100);
    expect((await query(0)).entries.map((entry) => entry.path)).toEqual(["/entry-104"]);
    expect((await query(1001)).entries).toHaveLength(105);
  });

  it("filters directory jobs by either endpoint and single-file jobs by their owning connection", async () => {
    const api = await setup();
    // 同路径跨连接合法；单文件任务在 start 后即进入 list/status。
    const dir = await api.invoke<{ jobId: string }>("files/copy", { sourceConnectionId: "mock-conn", targetConnectionId: "__local__", sourcePath: "/docs", targetPath: "/docs" });
    const local = await api.invoke<{ taskId: string }>("files/upload/start", { connectionId: "__local__", remotePath: "/local-upload", size: 0 });
    const remote = await api.invoke<{ taskId: string }>("files/upload/start", { remotePath: "/remote-upload", size: 0 });
    const list = async (connectionId?: unknown) => {
      const result = await api.invoke<{ jobs: Array<{ jobId?: string; taskId?: string }> }>("files/transfers/list", { connectionId });
      return result.jobs.map((job) => job.jobId ?? job.taskId);
    };
    expect(await list("__local__")).toEqual([dir.jobId, local.taskId]);
    expect(await list("mock-conn")).toEqual([dir.jobId, remote.taskId]);
    expect(await list()).toHaveLength(3);
    expect(await list(null)).toHaveLength(3);
    expect(await list("")).toEqual([]);
    expect(await list("unknown")).toEqual([]);
    await expect(list(1)).rejects.toThrow("connectionId must be a string");
    expect(await api.invoke("files/transfer/status", { jobId: local.taskId })).toMatchObject({ kind: "transfer", job: { taskId: local.taskId, connectionId: "__local__", status: "running" } });
    expect(await api.invoke("files/transfer/status", { jobId: dir.jobId })).toMatchObject({ kind: "dirJob", job: { sourceConnectionId: "mock-conn", targetConnectionId: "__local__" } });
    await vi.advanceTimersByTimeAsync(450);
    await api.invoke("files/upload/finish", { taskId: local.taskId });
    await api.invoke("files/transfers/clear", { connectionId: "__local__" });
    expect(await list()).toEqual([remote.taskId]);
    expect(await api.invoke("files/transfer/status", { jobId: remote.taskId })).toMatchObject({ job: { status: "running" } });
    await expect(api.invoke("files/transfers/clear", { connectionId: false })).rejects.toThrow("connectionId must be a string");
    await expect(api.invoke("files/transfer/cancel", { taskId: dir.jobId })).rejects.toThrow("Transfer task was not found");
  });

  it.each(["files/copyDir", "files/syncDir"])("requires explicit source and target connections for %s", async (method) => {
    const api = await setup();
    await expect(api.invoke(method, { connectionId: "mock-conn", sourcePath: "/docs", targetPath: "/copy" })).rejects.toThrow("sourceConnectionId and targetConnectionId are required");
    const result = await api.invoke(method, { sourceConnectionId: "mock-conn", targetConnectionId: "mock-conn", sourcePath: "/docs", targetPath: "/copy" });
    expect(result).toEqual({ jobId: expect.any(String) });
  });

  it("tests a temporary connection without retaining it and rejects malformed lifecycle payloads", async () => {
    const api = await setup();
    expect(await api.invoke("connection/test", connection("probe"))).toEqual({ success: true, message: "Storage backend reachable" });
    await expect(api.invoke("files/stat", { connectionId: "probe", path: "/" })).rejects.toThrow("Unknown connectionId 'probe'; connect first");
    await expect(api.invoke("connection/test", {})).rejects.toThrow("Missing connection payload");
    await expect(api.invoke("connection/test", { connection: { id: "  " } })).rejects.toThrow("Missing connection id");
    await expect(api.invoke("connection/connect", { connection: { id: "x" } })).rejects.toThrow("Missing protocol in external_config");
    await expect(api.invoke("connection/connect", { connection: { id: "x", external_config: { protocol: "unknown" } } })).rejects.toThrow("Unsupported protocol");
    await expect(api.invoke("connection/connect", { connection: { id: "x", external_config: { protocol: "opendal-custom", service: "memory", config: "[]" } } })).rejects.toThrow("Service config JSON must be an object");
    await expect(api.invoke("connection/connect", connection("__local__"))).rejects.toThrow("reserved for the built-in local filesystem");
  });

  it("supports a deterministic failed probe without preventing valid connect", async () => {
    const api = await setup("&connectionTest=fail");
    await expect(api.invoke("connection/test", connection("probe"))).rejects.toThrow("Storage check failed");
    expect(await api.invoke("connection/connect", connection("probe"))).toEqual({ success: true });
    expect(await api.invoke("files/capabilities", { connectionId: "probe" })).toMatchObject({ readOnly: false });
  });

  it("keeps connect/disconnect idempotent and cancels only tasks involving the disconnected connection", async () => {
    const api = await setup();
    await api.invoke("connection/connect", connection("other"));
    await api.invoke("files/write", { connectionId: "other", path: "/note", dataBase64: "aGk=" });
    await api.invoke("connection/connect", connection("other"));
    await expect(api.invoke("connection/connect", { connection: { id: "other", external_config: { protocol: "invalid" } } })).rejects.toThrow("Unsupported protocol");
    expect(await api.invoke("files/stat", { connectionId: "other", path: "/note" })).toMatchObject({ entry: { size: 2 } });
    const upload = await api.invoke<{ taskId: string }>("files/upload/start", { connectionId: "other", remotePath: "/upload", size: 0 });
    const download = await api.invoke<{ taskId: string }>("files/download/start", { connectionId: "other", remotePath: "/note" });
    const dir = await api.invoke<{ jobId: string }>("files/copy", { sourceConnectionId: "mock-conn", targetConnectionId: "other", sourcePath: "/docs", targetPath: "/copy" });
    const unrelated = await api.invoke<{ taskId: string }>("files/upload/start", { remotePath: "/unrelated", size: 0 });
    await api.invoke("connection/disconnect", { connection: { id: "other" } });
    await vi.advanceTimersByTimeAsync(450);
    for (const jobId of [upload.taskId, download.taskId, dir.jobId]) {
      expect(await api.invoke("files/transfer/status", { jobId })).toMatchObject({ job: { status: "canceled" } });
    }
    expect(await api.invoke("files/transfer/status", { jobId: unrelated.taskId })).toMatchObject({ job: { status: "running" } });
    await expect(api.invoke("files/stat", { connectionId: "other", path: "/note" })).rejects.toThrow("connect first");
    expect(await api.invoke("connection/disconnect", { connection: { id: "other" } })).toEqual({ success: true });
    expect(await api.invoke("connection/disconnect", { connection: { id: "__local__" } })).toEqual({ success: true });
    expect(await api.invoke("files/stat", { connectionId: "__local__", path: "/tmp" })).toMatchObject({ entry: { kind: "dir" } });
  });

  it("mirrors current host requests, getters, context callbacks and environment events", async () => {
    window.history.replaceState(null, "", "/?mock=1&locale=en");
    const { installMockHost } = await import("./mockHost");
    const host = installMockHost()!;
    const api = window.dbxPlugin;
    const initial = vi.fn();
    const stopInit = api.onInit!(initial);
    expect(initial).toHaveBeenCalledWith(api.context);
    stopInit();
    const context = vi.fn();
    const stopContext = api.onContext!(context);
    host.setContext({ connectionId: "next" });
    expect(context).toHaveBeenCalledWith({ connectionId: "next" });
    const snapshot = await api.request<Record<string, unknown>>("host.getContext");
    snapshot.connectionId = "mutated";
    expect(api.context).toEqual({ connectionId: "next" });
    stopContext();
    host.setContext({ connectionId: "third" });
    expect(context).toHaveBeenCalledTimes(1);
    const environment = vi.fn((event: DbxPluginEvent) => expect(api.locale).toBe("ja"));
    const stopEvent = api.onEvent(environment);
    host.setEnvironment({ locale: "ja" });
    expect(environment).toHaveBeenCalledWith({ type: "env", locale: "ja" });
    stopEvent();
    host.setEnvironment({ locale: "es" });
    expect(environment).toHaveBeenCalledTimes(1);
    const connections = await api.request<Array<{ id: string }>>("host.listConnections");
    expect(connections.map((item) => item.id)).toEqual(["mock-conn", "__local__"]);
    await expect(api.request("host.unknown")).rejects.toThrow("Unsupported plugin host method 'host.unknown'");
    expect(api).not.toHaveProperty("onLocaleChange");
    expect(api).not.toHaveProperty("onContextChange");
  });
});

// R5-P2-8 回归：mock files/download/start 必须按 connectionId 路由——此前写死
// 远端树，双栏本地面（__local__）下载报 NotFound，浏览器验证中该旅程不可自证。
describe("mockHost download/start connection routing (R5-P2-8)", () => {
  it("files/download/start with connectionId=__local__ serves from the local tree", async () => {
    const api = await setup();
    await api.invoke("files/write", { path: "/probe-dl.txt", dataBase64: "aGk=", connectionId: "__local__" });

    const result = await api.invoke<{ taskId: string; size: number }>("files/download/start", {
      remotePath: "/probe-dl.txt",
      connectionId: "__local__",
    });

    expect(result.taskId).toBeTruthy();
    expect(result.size).toBe(2);
  });

  it("files/download/start keeps defaulting to the remote tree", async () => {
    const api = await setup();
    await api.invoke("files/write", { path: "/probe-dl-remote.txt", dataBase64: "aGk=" });

    const result = await api.invoke<{ taskId: string; size: number }>("files/download/start", {
      remotePath: "/probe-dl-remote.txt",
    });

    expect(result.taskId).toBeTruthy();
    // 本地树从未有该文件：误路由到本地树会 NotFound
    await expect(api.invoke("files/download/start", { remotePath: "/probe-dl-remote.txt", connectionId: "__local__" })).rejects.toThrow(/NotFound/);
  });
});

describe("mockHost transfer/cancel mirrors sidecar slot cancellation", () => {
  beforeEach(() => {
    Reflect.deleteProperty(window, "dbxPlugin");
    vi.useFakeTimers();
  });
  afterEach(() => {
    vi.clearAllTimers();
    vi.useRealTimers();
    Reflect.deleteProperty(window, "dbxPlugin");
  });

  it("discards canceled uploads; a late frame and idempotent finish cannot publish the file", async () => {
    const api = await setup();
    const events: DbxPluginEvent[] = [];
    api.onEvent((event) => events.push(event));
    const { taskId } = await api.invoke<{ taskId: string }>("files/upload/start", {
      remotePath: "/canceled-upload.txt", size: 1, connectionId: "__local__",
    });
    const frame = new Uint8Array(9);
    frame[8] = 65;
    await api.sendBinary(`files/upload/${taskId}`, frame);
    await api.invoke("files/transfer/cancel", { taskId });
    await api.sendBinary(`files/upload/${taskId}`, frame);
    await api.invoke("files/upload/finish", { taskId });
    await expect(api.invoke("files/stat", { path: "/canceled-upload.txt", connectionId: "__local__" })).rejects.toThrow(/NotFound/);
    expect(events.at(-1)).toMatchObject({
      method: "files/transfer/progress",
      params: { taskId, kind: "upload", state: "canceled", connectionId: "__local__", size: 1, transferred: 1 },
    });
    await expect(api.invoke("files/transfer/cancel", { taskId })).rejects.toThrow("Transfer task was not found");
  });

  it.each([0, 10])("stops download frames when canceled after %i ms, without canceling another task", async (elapsed) => {
    const api = await setup();
    const frames: DbxPluginBinaryEvent[] = [];
    const events: DbxPluginEvent[] = [];
    api.onBinary((event) => frames.push(event));
    api.onEvent((event) => events.push(event));
    await api.invoke("files/write", {
      path: "/cancel-download.bin", dataBase64: api.encodeBase64(new Uint8Array(256 * 1024 + 1)),
    });
    const first = await api.invoke<{ taskId: string }>("files/download/start", { remotePath: "/cancel-download.bin" });
    const other = await api.invoke<{ taskId: string }>("files/download/start", { remotePath: "/cancel-download.bin" });
    await vi.advanceTimersByTimeAsync(elapsed);
    const countBeforeCancel = frames.filter((event) => event.channel === `files/download/${first.taskId}`).length;
    expect(countBeforeCancel).toBe(elapsed === 0 ? 0 : 1);
    await api.invoke("files/transfer/cancel", { taskId: first.taskId });
    await vi.advanceTimersByTimeAsync(100);
    expect(frames.filter((event) => event.channel === `files/download/${first.taskId}`)).toHaveLength(countBeforeCancel);
    expect(frames.filter((event) => event.channel === `files/download/${other.taskId}`)).toHaveLength(2);
    expect(events.at(-1)).toMatchObject({ params: { taskId: first.taskId, kind: "download", state: "canceled" } });
    await api.invoke("files/download/finish", { taskId: other.taskId });
  });

  it("requires taskId and rejects unknown tasks and the unregistered upload/cancel alias", async () => {
    const api = await setup();
    await expect(api.invoke("files/transfer/cancel", { jobId: "unknown" })).rejects.toThrow(/taskId/);
    await expect(api.invoke("files/transfer/cancel", { taskId: "unknown" })).rejects.toThrow("Transfer task was not found");
    await expect(api.invoke("files/upload/cancel", { taskId: "unknown" })).rejects.toThrow(/Method not found/);
  });

  it("still cancels directory jobs before the copy is applied", async () => {
    const api = await setup();
    const { jobId } = await api.invoke<{ jobId: string }>("files/copy", { sourcePath: "/docs", targetPath: "/canceled-copy" });
    await api.invoke("files/transfer/cancel", { taskId: jobId });
    await vi.advanceTimersByTimeAsync(1000);
    await expect(api.invoke("files/stat", { path: "/canceled-copy" })).rejects.toThrow(/NotFound/);
    expect(await api.invoke("files/transfer/status", { jobId })).toMatchObject({ job: { status: "canceled" } });
  });
});
