// @vitest-environment happy-dom
import { describe, expect, it } from "vitest";

// R3-P2-2 回归：mock files/delete / files/purge 必须按 connectionId 路由到
// 对应树——此前写死远端树，双栏左栏（__local__）删除「通知成功但条目原样存在」。

async function setup() {
  window.history.replaceState(null, "", "/?mock=1");
  const { installMockHost } = await import("./mockHost");
  installMockHost();
  return window.dbxPlugin!;
}

function names(payload: unknown): string[] {
  const entries = (payload as { entries: Array<{ name: string }> }).entries ?? [];
  return entries.map((entry) => entry.name);
}

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
