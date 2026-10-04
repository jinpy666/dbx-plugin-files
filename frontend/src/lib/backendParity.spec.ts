// 后端 ↔ mockHost 方法清单对齐（架构审查 2026-10 的防漂移 tripwire）。
// mock 是后端行为的第二实现（纯浏览器三态走查 + 全部 App.*.spec 跑在它上
// 面），任何一侧静默增删方法都会让两套实现漂移——api.ts 记录的 kind/
// modifiedAt 真实错位正是从这个缝隙漏进来的。规则：
//   1) mock 不得实现 GOLDEN 之外的方法（挡拼写错误与越权面）；
//   2) GOLDEN 中 mock 未实现的方法必须逐条在 MOCK_EXEMPT_METHODS 登记理由
//      （新增缺口必须显式入册，不得静默）；
//   3) 豁免表自身保持诚实：不许登记已实现或不存在的方法。
// GOLDEN 与 backend/src/main.rs 分发臂的同步是人肉纪律：后端增删方法时
// 必须同步此表。选择硬编码而非测试时读 main.rs 源码解析，是因为前端工程
// 刻意不带 @types/node（浏览器目标），而加依赖要动锁文件；硬编码表让全部
// 全部分发臂（含 kebab-case 方法名）在一处可评审，漂移时 diff 一眼可见。
import { describe, expect, it } from "vitest";
// Vite ?raw：编译期由 vite/client 类型声明为 string，运行时（vitest/vite-node）
// 内联 mockHost.ts 源码文本——不需要 node:fs，也不引入 @types/node。
import mockHostSource from "./mockHost.ts?raw";

/** 后端 main.rs 全部分发臂（含 kebab-case 方法名的提取基线）。 */
const GOLDEN_BACKEND_METHODS: ReadonlySet<string> = new Set([
  "connection/connect",
  "connection/disconnect",
  "connection/test",
  "files/about",
  "files/archiveDownload",
  "files/archiveList",
  "files/audit/list",
  "files/bisync/start",
  "files/bisync/state",
  "files/bwlimit",
  "files/schedule/cancel",
  "files/schedule/create",
  "files/schedule/delete",
  "files/schedule/history",
  "files/schedule/list",
  "files/schedule/runNow",
  "files/schedule/update",
  "files/capabilities",
  "files/check",
  "files/checksum/verify",
  "files/cleanup",
  "files/compress",
  "files/copy",
  "files/copyDir",
  "files/copyurl",
  "files/delete",
  "files/download/finish",
  "files/download/start",
  "files/extract",
  "files/hashsum",
  "files/list",
  "files/listCancel",
  "files/listPaged",
  "files/listStream",
  "files/local/capabilities",
  "files/local/detect-apps",
  "files/local/editors/list",
  "files/local/exists",
  "files/local/open",
  "files/local/open-with",
  "files/local/reveal",
  "files/local/validate-directory",
  "files/local/validate-open-app",
  "files/mkdir",
  "files/mount",
  "files/mount/refresh",
  "files/mount/stats",
  "files/mountStatus",
  "files/move",
  "files/publicLink",
  "files/purge",
  "files/quickPaths",
  "files/read",
  "files/readRange",
  "files/remote-edit/close",
  "files/remote-edit/decide",
  "files/remote-edit/open",
  "files/remote-edit/status",
  "files/rename",
  "files/rmdir",
  "files/rmdirs",
  "files/search",
  "files/serve/list",
  "files/serve/start",
  "files/serve/stop",
  "files/size",
  "files/stat",
  "files/syncDir",
  "files/transfer/cancel",
  "files/transfer/status",
  "files/transfers/clear",
  "files/transfers/delete",
  "files/transfers/list",
  "files/ui/state/report",
  "files/unmount",
  "files/upload/finish",
  "files/upload/start",
  "files/write",
  "mcp/call",
  "mcp/settings/get",
  "mcp/settings/set",
  "mcp/tools",
]);

/** mock 分支：mockHost.ts 的 `case "…"` 与 `method === "…"`（源码文本提取）。 */
function mockMethods(): Set<string> {
  const methods = new Set<string>();
  // 连字符入集（detect-apps/validate-open-app/remote-edit 等 kebab 方法名）：
  // 提取器历史上漏掉连字符方法，与 GOLDEN 双侧一起修复盲区。
  for (const match of mockHostSource.matchAll(/(?:case\s+|method\s*===\s*)"([a-zA-Z][a-zA-Z0-9-]*(?:\/[a-zA-Z0-9-]+)+)"/g)) {
    methods.add(match[1]!);
  }
  return methods;
}

/** mock 无需实现的分发臂，逐条登记理由。 */
const MOCK_EXEMPT_METHODS: Record<string, string> = {
  // 解压依赖 sidecar 侧归档能力，纯浏览器演示态没有对应实现；显式
  // method not found 优于假装成功（与后端错误形态一致）。
  "files/extract": "OS/归档能力，纯浏览器态不适用",
  // 打开本机文件（默认应用/外部应用）是桌面 OS 能力。
  "files/local/open": "OS 能力，纯浏览器态不适用",
  // 工作台只调用 files/rmdirs（清理空目录），rmdir 无前端调用方。
  "files/rmdir": "无前端调用方",
  // MCP 工具面方法（mcp/tools.rs 独立实现），工作台前端不调用。
  "mcp/call": "MCP 面方法，工作台不调用",
  "mcp/settings/get": "MCP 面方法，工作台不调用",
  "mcp/settings/set": "MCP 面方法，工作台不调用",
  "mcp/tools": "MCP 面方法，工作台不调用",
};

describe("backend ↔ mockHost 方法清单对齐", () => {
  it("mock 分支提取健全：锚点方法都能解析到，规模符合当前基线", () => {
    const mock = mockMethods();
    for (const anchor of ["files/list", "files/delete", "files/listCancel", "connection/test"]) {
      expect(mock.has(anchor), `mock 提取应含 ${anchor}`).toBe(true);
    }
    expect(mock.size).toBeGreaterThanOrEqual(45);
  });

  it("mock 不得实现 GOLDEN 之外的方法（挡拼写错误）", () => {
    const mock = mockMethods();
    const extra = [...mock].filter((method) => !GOLDEN_BACKEND_METHODS.has(method));
    expect(extra, `mock 多出的方法: ${extra.join(", ")}`).toEqual([]);
  });

  it("GOLDEN 中 mock 未实现的方法必须全部登记豁免理由", () => {
    const mock = mockMethods();
    const missing = [...GOLDEN_BACKEND_METHODS].filter((method) => !mock.has(method) && !(method in MOCK_EXEMPT_METHODS));
    expect(missing, `mock 缺失且未豁免的方法: ${missing.join(", ")}`).toEqual([]);
  });

  it("豁免表保持诚实：不登记已实现的方法，也不登记 GOLDEN 之外的方法", () => {
    const mock = mockMethods();
    for (const [method, reason] of Object.entries(MOCK_EXEMPT_METHODS)) {
      expect(reason.trim().length, `${method} 的豁免理由不能为空`).toBeGreaterThan(0);
      expect(mock.has(method), `${method} 已被 mock 实现，应从豁免表移除`).toBe(false);
      expect(GOLDEN_BACKEND_METHODS.has(method), `${method} 不在 GOLDEN 中，豁免无效`).toBe(true);
    }
  });
});
