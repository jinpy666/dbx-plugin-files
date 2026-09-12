# MCP 集成（M2）

files 插件的 MCP 工具面（设计来源 `shared/IMPL_PLAN_PLUGIN_MCP.zh-CN.md`
v2，ssh `mcp.rs` 骨架的 Rust 实现，形状对齐 ldap Go 版参考实现）。实现在
sidecar `backend/src/mcp.rs`，经 DBX MCP 桥（`dbx_list_plugin_tools` /
`dbx_call_plugin_tool`）调用插件协议方法 `mcp/tools`、`mcp/call`、
`mcp/settings/get|set`。桥按 `connectionId` 转发标准 lifecycle payload
（`mcp/call` 的 `lifecycle` 字段），凭据由宿主解析，**工具参数里不出现
任何密码**。

核心设计（详见设计文档）：

1. **读：UI 优先 + 强本地化**——大目录树不进 MCP；AI 用 UI intent 把
   path 填进工作台触发导航（用户可视可继续操作），或用 `files_scan_digest`
   在 sidecar 本地递归扫描 + 谓词过滤 + 聚合 + cursor 翻页，扫描过程数据
   一条不出 sidecar（二进制/文件正文不出 MCP）。
2. **写：入 MCP 面 + 两阶段确认**——`write/mkdir/rename` 单阶段直执行；
   `delete/purge` 强制 preview → confirmToken（purge 拒绝连接根红线沿用）。
3. **token 经济**——默认 `format:"digest"`（计数 + 扩展名分组 + topN +
   样本），显式要 `rows` 才出行且 clamp ≤20 行；单响应上限 16 KiB，
   每 cell 截 120 字符（**path 定位字段不截断**）。

## 事件：`files/ui/intent`（sidecar → 前端）

sidecar 收到 UI 驱动类工具调用时：生成 `intentId` → intent 状态表登记
（进程内 map，TTL 60s，LRU 20 条，`pending`）→ 发本事件 → 等待前端
report（默认 5s，`mcp/settings/set` 的 `reportWaitMs` 可调）。

```json
{ "intentId": "i-1a2b3c4d", "action": "search",
  "params": { "path": "/data/reports", "trigger": "listPaged",
              "connectionId": "…" } }
```

| action | params | 前端行为 |
| --- | --- | --- |
| `search` | `path` 必填、`connectionId?` | PathField 填 path → 触发既有 listPaged 管线导航（锚点 = FileTable 行 path） |
| `focus` | `panel`（browse \| transfers \| audit）、`connectionId?` | 切换/聚焦面板，关闭遮挡弹窗 |
| `select` | `path`、`connectionId?` | 当前文件表内按 path 定位命中行并高亮 |

前端消费统一走 `shared/frontend/uiIntent.ts` 的 `useUiIntent("files",
handlers)`（公共层单点维护，插件不各抄一份）。

> **状态（2026-09-12）**：sidecar 侧全部落地；前端接线（App.vue 挂
> useUiIntent + PathField/FileTable handler + ui/state/report 快照上报）
> 待下一轮任务。接线前 intent 工具走降级矩阵「pending + hint」路径，
> 不假死。

## 方法：`files/ui/state/report`（前端 → sidecar）

- **intent 回报**：`{intentId, status: "applied"|"rejected", summary,
  reason?}`。summary = `{count, anchor?{path}, rows?[≤5]}`，每 cell 截
  120 字符（`cellWidth` 可调），**path 定位字段不截断**。
- **快照型**：无 `intentId`、`status:"snapshot"`——前端在关键动作后
  （面板切换、导航完成、选中行变化）主动上报 `{panel, path?, count?,
  anchor?}`，sidecar 缓存最新快照；`files_ui_state` 不带 intentId 时返回。
- 未知/已过期 intentId 回报报业务错误（-32000）。

## 工具一览（12 个）

`mcp/tools` 可带可选 `{connectionId}`：该连接配置为只读（表单
`read_only` ∥ 宿主标准 read_only；delete 类另受 `allow_delete` 约束）
时，写工具**不进清单**并附 `omittedWriteTools` 原因说明；未带
connectionId 时全量列出（调用时仍有只读门拒绝，纵深防御）。Scoped AI
会话由宿主禁 `dbx_call_plugin_tool`。

| 工具 | 参数（camelCase） | 语义 |
| --- | --- | --- |
| `files_ui_search` | `path` 必填；`connectionId?` | 填 path 并触发 listPaged 导航，结果留 UI；返回 `{intentId, state, summary}`。前端未响应 → `state:"pending"` + 引导走 digest |
| `files_ui_focus` | `panel` 必填（browse/transfers/audit）；`connectionId?` | 聚焦面板；同 intent 回报语义 |
| `files_ui_select` | `path` 必填；`connectionId?` | 文件表按 path 定位并高亮；命中失败 → `rejected` + reason |
| `files_ui_state` | `intentId?` | 带 intentId 读 intent 结果（applied/rejected/pending/expired）；不带读最新 UI 快照 `{snapshot}` |
| `files_ui_quick_paths` | `connectionId` 必填；`limit?`（1–50，缺省 50） | 连接 quickPaths 清单（纯定位用元发现） |
| `files_scan_digest` | `connectionId` 必填；`path?`（缺省 `/`）、`glob?`、`minSizeBytes?`、`maxSizeBytes?`、`modifiedSince?`、`modifiedUntil?`、`depth?`（1–16，缺省 8）、`format?`（digest 缺省 / rows） | **本地读核心**：递归扫描（10 万条 clamp）+ 谓词过滤全在 sidecar 本地，返回 `{matched, scanned, scanTruncated, stats:{totalBytes, byExtension[≤20], largest[≤10], newest[≤10]}, sample[≤5], cursorId}`；`format:"rows"` 出 ≤20 行。二进制/文件正文不出 MCP |
| `files_cursor_next` | `cursorId` 必填；`n?`（≤20，缺省 20）、`offset?`（缺省续读） | digest 会话翻页：只取 `{path}` 定位行，条件不重发、远端不重扫。会话 TTL 10 分钟、LRU ≤8、物化上限 1 万行；过期报错建议重发 digest |
| `files_write` | `connectionId`、`path`、`dataBase64` 必填 | 单阶段直执行；硬上限 4 MiB（沿用 inline 写上限），**MCP 侧建议 ≤1 MiB**，超出返回 hint 引导走工作台/传输通道；审计 `source:"mcp"` |
| `files_mkdir` | `connectionId`、`path` 必填 | 单阶段直执行（mkdir -p 语义）；审计 `source:"mcp"` |
| `files_rename` | `connectionId`、`path`、`newPath` 必填 | 单阶段直执行；目录 rename 降级异步 copy+delete job（返回 jobId）；审计 `source:"mcp"` |
| `files_delete` | `connectionId`、`path` 必填；`confirmToken?` | 强制两阶段，见下节 |
| `files_purge` | `connectionId`、`path` 必填；`confirmToken?` | 强制两阶段；**拒绝连接根与 `/`**（§8.2 红线，与工作台同源判定） |

## 写路径与两阶段确认（§4）

```
第一次  files_delete {connectionId, path:"…"}                （无 confirmToken）
  → {preview:{tool, connectionId, path, kind, size},
     confirmToken:"c-…", expiresAt, note}                    （不执行任何写）
第二次  同参数 + confirmToken 且参数 hash 一致 → 执行 + 审计
```

- **单阶段直执行**：`files_write` / `files_mkdir` / `files_rename`
  （非破坏、可逆；照常过 `read_only` / `allow_delete` 门）。
- **强制两阶段**：`files_delete` / `files_purge`。confirmToken 一次性、
  60s TTL、与请求参数 hash 绑定——参数被改即作废（`arguments changed…`），
  过期（`expired`）或复用（`unknown or already used`）都要求重开预览。
- **只读门**：只读连接上写工具不进 `mcp/tools` 清单，`mcp/call` 侧再拒绝
  一道；门禁语义与工作台 `ensure_writable`/`ensure_deletable` 同源。
- **审计**：所有 MCP 写路径审计记 `source:"mcp"`（`files/audit/list`
  与 audit.jsonl 同条携带；M0 审计事件形状不变，新增可选字段）。
- **响应上限**：单工具响应 16 KiB（`mcp/settings/set` 的
  `responseLimitBytes`，1 KiB–1 MiB）。超限先按 `cellWidth` 截单元格
  （定位字段不截断），再逐个丢弃最长数组，最后兜底硬截断，全程置
  `truncated:true`。
- **文本预览上限**：64 KiB 且按行截断（远小于工作台 2 MiB read 上限）；
  二进制走 files/download 二进制通道，不进 MCP。

## mcp/settings（可调参数，`mcp-settings.json` 持久化）

| 字段 | 默认 | 范围 | 说明 |
| --- | --- | --- | --- |
| `reportWaitMs` | 5000 | 1–30000 | UI intent report 等待时长 |
| `cellWidth` | 120 | 1–2000 | 单元格截断宽度（path 不截断） |
| `digestGroupLimit` | 20 | 1–20 | 扩展名分组组数上限 |
| `digestTopN` | 10 | 1–10 | 最大/最新 topN 上限 |
| `digestSampleRows` | 5 | 1–5 | digest 样本行数 |
| `digestRowLimit` | 20 | 1–20 | `format:"rows"` 行数 |
| `responseLimitBytes` | 16384 | 1024–1048576 | 单响应上限 |
| `maxCursorRows` | 10000 | 100–100000 | cursor 物化行数上限 |
| `cursorTtlSecs` | 600 | 10–3600 | cursor 会话 TTL |
| `maxCursorSessions` | 8 | 1–32 | cursor LRU 容量 |
| `confirmTtlSecs` | 60 | 10–600 | confirmToken TTL |

`mcp/settings/get` 返回 `{settings: {…}, responseLimitBytes}`（ldap 同构）；
`mcp/settings/set` 白名单部分更新，白名单外字段容忍、非法值报错。

## 降级矩阵（设计 §5）

| 场景 | `files_ui_*` | digest / cursor | 写工具 |
| --- | --- | --- | --- |
| 工作台打开、前端在线 | applied + summary | 可用 | 可用 |
| 前端在线但面板无该 action | rejected + reason | 可用 | 可用 |
| 工作台未打开 / 前端未响应（含前端接线未完成） | pending → 超时 hint（引导走 digest） | 可用 | 可用 |
| 连接只读 / allow_delete=false | 不受影响 | 可用 | 不注册进工具清单 |
| Scoped AI 会话 | 宿主禁 `dbx_call_plugin_tool` | 同左 | 同左 |

## 状态机验收用例

digest/cursor/confirmToken/intent 纯逻辑的验收用例清单（三插件同表，防
形状漂移）单点维护在 `shared/frontend/README.zh-CN.md`「MCP 两阶段/
digest/cursor 验收用例清单」，files 侧对应 `backend/src/mcp.rs`
`#[cfg(test)]`（settings/glob/digest 聚合/cursor LRU+TTL/confirmToken/
intent 状态表/16KiB 截断/只读工具清单/写门禁）。

## smoke

```bash
python3 scripts/smoke_mcp.py
# 或 DBX_PLUGIN_SIDECAR=/path/to/dbx-plugin-files python3 scripts/smoke_mcp.py
# 场景 M1–M10（与 ldap smoke_mcp.py 同表）：M1–M9 离线（settings/tools/
# 只读门/intent pending 与 applied/快照/门禁/未知方法）+ M10 fs 临时目录
# 场景（建树走 MCP 写工具 + digest+cursor+两阶段删除+purge 红线+审计
# source:"mcp"）。未注册方法 SKIP 不 FAIL（M0 §5.2）。
```
