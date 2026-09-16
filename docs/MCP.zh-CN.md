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

## 方式二：独立 stdio 模式（`--mcp`，无 DBX 宿主时）

sidecar 二进制可直接作为 MCP 服务器运行（ssh 插件同款，真机验证过这是
插件 MCP 工具被 AI 客户端调用的现实暴露路径）：

```bash
dbx-plugin-files --mcp
```

- **协议**：MCP `2024-11-05`，换行分隔 JSON-RPC 2.0；`initialize`
  （serverInfo `io.dbx.files`）、`notifications/initialized`（不回包）、
  `tools/list`、`tools/call`、`ping`。**错误码分档（族内统一，
  ssh/ldap/kafka 同码）**：传输层/结构性错误用 JSON-RPC 标准码——非 JSON
  行 `-32700` Parse error、未知方法 `-32601` Method not found、
  `tools/call` 缺工具名/arguments 非对象 `-32602` Invalid params、
  无 id 请求 / 缺失或非字符串 `method` / id 为 object/array/boolean
  `-32600` Invalid request（id ∈ string/number/null）；工具级/应用级
  错误（Unknown tool、UNAVAILABLE、连接/两阶段/参数业务错误）保持
  `-32000` 不变。smoke 的 SKIP 判定认 "Method not found" 文本，与数字码
  解耦。
- **传输层健壮性（第五轮）**：按字节读行（`read_until`）+ lossy UTF-8
  解码——非法 UTF-8 字节流、二进制噪声行一律回 `-32700` 且**进程与连接
  存活**（`BufRead::lines()` 的 UTF-8 校验错误不再杀会话）；单行上限
  16 MiB（`DBX_FILES_MCP_STDIO_MAX_LINE` 可调，超限回 `-32700` 点名上限
  与 env，连接继续）；空行/纯空白行静默跳过；CRLF 结尾请求正常解析。
  `jsonrpc` 版本字段刻意**宽容不校验**（族一致：ssh/ldap/kafka 均不校验，
  真实客户端可能省略或变体，钉测试记录该形状）。
- **互斥**：`--mcp` 与 DBX 插件 framed 协议在同一进程互斥——带 `--mcp`
  启动即不进入 framed 协议循环，反之亦然。
- **工具面不变**：stdio 只是新入口，`tools/call` 复用 `mcp/call` 的同一
  分派（digest/cursor、两阶段 confirmToken、审计 `source:"mcp"`、16 KiB
  响应上限全部同源），tools/list 全量 12 工具。
- **数据目录**：与工作台模式共用解析规则，可用 `DBX_PLUGIN_DATA_DIR`
  重定向（`mcp-settings.json` 与 `audit.jsonl` 都在其中）。

### 内联凭据连接（camelCase，与连接表单字段对齐）

stdio 模式没有宿主连接存储，OpenDAL 连接参数随调用内联传入——工具参数
带 `connection` 对象（每次调用或只在首次皆可），sidecar 按**参数 hash**
生成 `mcp-inline-<hash>` 连接 id 池化进进程内 engine，重复相同参数的调用
复用同一 Operator：

```json
{ "name": "files_scan_digest",
  "arguments": { "connection": { "protocol": "s3", "bucket": "demo",
      "endpoint": "http://127.0.0.1:9000", "region": "us-east-1",
      "accessKeyId": "minioadmin", "secretAccessKey": "…", "root": "/data" },
    "path": "/data", "glob": "*.log" } }
```

| `connection` 字段（camelCase） | 对应表单字段 | 说明 |
| --- | --- | --- |
| `protocol` | protocol | `local`/`localFs` 为 `fs` 便捷别名；其余 `s3`/`oss`/`cos`/`webdav`/`ftp`/`sftp`/`smb`/`sftp-native`/`opendal-custom` 原样 |
| `root` | root | OpenDAL 根前缀 |
| `bucket` / `region` / `endpoint` / `accessKeyId` / `enableVirtualHostStyle` | bucket/region/endpoint/access_key_id/enable_virtual_host_style | s3/oss/cos |
| `secretAccessKey` | secret_access_key（secret） | s3/oss |
| `secretId` / `secretKey` / `securityToken` | secret_id / secret_key / security_token（均为 secret） | cos；endpoint 使用腾讯云地域端点，如 `https://cos.ap-guangzhou.myqcloud.com` |
| `username`（webdav/smb）/ `user`（ftp/sftp/sftp-native） | username / user | 按协议 |
| `password` | password（secret） | webdav/ftp/smb/sftp-native |
| `key` / `knownHostsStrategy` | key（secret）/ known_hosts_strategy | sftp/sftp-native |
| `share` / `domain` | share / domain | smb |
| `service` / `config` | service / config | opendal-custom |
| `readOnly` / `allowDelete` / `lockToRoot` / `timeoutSecs` | read_only/allow_delete/lock_to_root/timeout_secs | 门禁与超时；只读连接写工具照常被拒 |
| `id` / `name` | id / name | 可选；`id` 缺省用参数 hash 池化 id |

**localFs 便捷路径**：`{"protocol": "local", "root": "/tmp/x"}` 即可（无
远端依赖，冒烟/本机场景首选）。也可直接用 `connectionId: "__local__"`
引用内置本地文件系统。

**连接寻址**：缺 `connectionId` 且无 `connection` 时报"Missing required
parameter: connectionId"并附内联参数写法指引；引用了本进程未池化的
id（非 `__local__`）时先尝试 **DBX 应用桥转发**（见下节），桥不可用则
fail-closed 合并错误，给出三条出路（内联参数 / `__local__` / 启动 DBX
应用走桥）。

### 桥接兜底（L1，2026-09-13 第三轮；ssh L1 / ldap M14 同构）

standalone stdio 会话引用一个未池化的 saved-connection `connectionId`
时，sidecar 经 **DBX 应用本地 TCP 桥**把调用转发给运行中 DBX 应用自己的
files sidecar（与工作台同进程）——保存的连接因此可用，凭据从不出现在
工具参数里：

- **发现**：应用把桥端口写在 `<app_data_dir>/mcp-bridge-port`
  （`DBX_APP_DATA_DIR` 优先，回落
  `$HOME/Library/Application Support/com.dbx.app`）；文件缺失/损坏一律
  视为不可用，绝不猜端口。
- **防陈旧端口**：转发前对端口做 TCP connect 探测（端口文件比被杀的
  应用活得久）；不可达时经 `DBX_APP_LAUNCH_CMD`（缺省 macOS
  `open -a DBX.app`）尽力拉起应用并每 500ms 重读+重探，唤醒预算 30s，
  超时报带 "DBX app bridge" 前缀的可行动错误（不假死）。
- **契约**：`POST /call-plugin-tool`，snake_case 五字段
  `plugin_id`（io.dbx.files）/`connection_id`/`tool`/`arguments`/
  `timeout_ms`（固定 300s——files 工具无逐调用超时参数，读余量
  +150s 覆盖应用侧长 digest）；请求单次写入上限 64 KiB。200 响应即应用
  侧 `mcp/call` 的 MCP content envelope，**逐字透传**为 stdio 响应；
  非 envelope 形状（防御性）按成功 content 包装。HTTP 非 200 / 非法
  JSON 同样 fail-closed 并携带原因。
- **转发范围**：仅"需要连接"的工具（`files_cursor_next` 等会话类工具的
  未知 id 是本地错误；UI 类工具 stdio 下本就 UNAVAILABLE；带内联
  `connection` 的调用走本地池化路径不转发）。转发不污染本地连接池。
- **fail-closed 出路（files 特化）**：桥不可用时合并错误点名 files 自己
  的内联参数字段——`"connection": {"protocol": "local", "root": "/data"}`
  或 `{"protocol": "s3", "bucket": "…", "endpoint": "…", "region": "…",
  "accessKeyId": "…", "secretAccessKey": "…"}`；腾讯云 COS 使用
  `{"protocol": "cos", "bucket": "bucket-appid", "endpoint": "https://cos.ap-guangzhou.myqcloud.com", "secretId": "…", "secretKey": "…"}`——外加 `__local__` 与
  启动 DBX 应用两条出路。

### stdio 语义差异与后续项

- **tools/list 声明内联连接参数**：连接类工具的 inputSchema 显式声明
  `connection` 对象（camelCase 全键，required `protocol`），并把 required
  的 `connectionId` 放宽为 anyOf 二选一（`connectionId` 或内联
  `connection`）——严格校验的 MCP 宿主会丢弃未声明参数（ssh/ldap/kafka
  同因显式声明，2026-09-14 对齐轮）。UI 类工具与免连接的
  `files_cursor_next` 保持原 schema。
- **工具级错误为 MCP isError 结果**：stdio `tools/call` 的工具执行错误
  （含 UI UNAVAILABLE、缺连接指引、写门拒绝）以
  `{content:[{type:text}], isError:true}` 结果返回，非 JSON-RPC 协议级
  错误（MCP 规约：调用方须能在带内看到失败以自我纠正；ldap/kafka stdio
  同形，2026-09-14 对齐轮）。结构性错误（缺工具名/arguments 非对象）
  仍是 -32602。
- **UI 类工具明确 UNAVAILABLE**：`files_ui_focus` / `files_ui_search` /
  `files_ui_select` / `files_ui_state` 返回
  `UNAVAILABLE: 此工具需要 DBX 工作台（工作台模式可用）`并引导走
  `files_scan_digest` 或 DBX MCP 桥——不做 5s intent 等待，不假死
  （设计 §5 stdio 行）。`files_ui_quick_paths` 是纯元发现，不受影响。
- **目录 rename**：目录 rename 降级的异步 copy+delete job 依赖工作台事件
  通道，stdio 下直接返回明确错误（改用单文件 rename 或工作台）。
- **凭据暴露面**：内联凭据会进工具参数与 LLM 上下文，仅限本机可信会话
  使用；生产优先走方式一（DBX MCP 桥，凭据由宿主解析转发）或对保存连接
  直接引用 `connectionId` 走桥接兜底（凭据不进参数）。

### 接入 ZCode（stdio 客户端）

```json
{
  "mcp": {
    "servers": {
      "dbx-files": {
        "type": "stdio",
        "command": "/绝对路径/backend/target/release/dbx-plugin-files",
        "args": ["--mcp"]
      }
    }
  }
}
```

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
| `files_scan_digest` | `connectionId` 必填；`path?`（缺省 `/`）、`glob?`、`minSizeBytes?`、`maxSizeBytes?`、`modifiedSince?`、`modifiedUntil?`、`depth?`（1–16，缺省 8）、`format?`（digest 缺省 / rows） | **本地读核心**：递归扫描（10 万条 clamp）+ 谓词过滤全在 sidecar 本地，返回 `{matched, scanned, scanTruncated, stats:{totalBytes, byExtension[≤20], largest[≤10], newest[≤10]}, sample[≤5], cursorId}`；`format:"rows"` 出 ≤20 行。二进制/文件正文不出 MCP。可选字符串参数（`path`/`glob`/`format`）传了非字符串/空串时 fail-fast 报错点名，绝不静默回落默认值 |
| `files_cursor_next` | `cursorId` 必填；`n?`（≤20，缺省 20）、`offset?`（缺省续读） | digest 会话翻页：只取 `{path}` 定位行，条件不重发、远端不重扫。会话 TTL 10 分钟、LRU ≤8、物化上限 1 万行；过期报错建议重发 digest（报文携带实际生效 TTL） |
| `files_write` | `connectionId`、`path`、`dataBase64` 必填 | 单阶段直执行；硬上限 4 MiB（沿用 inline 写上限），**MCP 侧建议 ≤1 MiB**，超出返回 hint 引导走工作台/传输通道；`dataBase64` 允许空串（建空文件，与工作台写路径同语义），非标准 base64 报错并点名 RFC 4648 格式；审计 `source:"mcp"` |
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
  TTL 默认 60s（`confirmTtlSecs` 可调）、与请求参数 hash 绑定——参数被改
  即作废（`arguments changed…`），过期（报文携带实际 `TTL <n>s`）或复用
  （`unknown or already used`）都要求重开预览。**TTL 调整不追溯**（已签发
  token 的过期时间在签发时固定）。令牌表有界（第五轮）：每次签发前
  prune 过期条目，超过 1024 条硬上限时淘汰**最早过期**者——大量
  preview→consume/expire churn（500+ 轮）后表停在活跃集大小，绝不单调
  增长；最新签发的 token 永远存活。
- **删除目标拼写容错**：执行拼写由 preview stat 的 kind 决定——目录标记
  保留一个尾斜杠（前缀型后端只认该形式），文件路径去尾斜杠；LLM 回显的
  `a.txt/` 会真正删除 `a.txt`，而不是对不存在的标记静默 no-op 成功。
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
| `maxCursorSessions` | 8 | 1–32 | cursor LRU 容量；调整**即时生效**（每次物化按当前值执行，缩容在下一次 digest 物化时立即驱逐溢出会话） |
| `confirmTtlSecs` | 60 | 10–600 | confirmToken TTL（已签发 token 不追溯） |

`mcp/settings/get` 返回 `{settings: {…}, responseLimitBytes}`（ldap 同构）；
`mcp/settings/set` 白名单部分更新，白名单外字段容忍、非法值报错。

## LLM 输入变体容错（2026-09-13 增）

面向 AI agent 真实调用习惯的容错语义（都是确定性规则，不猜意图）：

- **数值参数**（`depth`/`minSizeBytes`/`maxSizeBytes`/`modifiedSince`/
  `modifiedUntil`/`n`/`offset`/`limit`）：数字、数字字符串（`"1024"`，
  trim 后解析）、整型浮点（`8.0`）均接受；其他类型（含负数、`1.5`、
  布尔）报 `{key} must be a non-negative integer` 并点名参数。
- **布尔参数**（内联 `connection` 的 `readOnly`/`allowDelete`/`lockToRoot`
  等）：字符串变体 `"true"/"false"/"1"/"0"/"yes"/"no"/"on"/"off"` 按意图
  解析（安全相关：字符串 `"true"` 的只读意图不会静默降级为可写）；
  无法解析时回退字段默认值。
- **path 尾斜杠**：写/rename 目标去尾斜杠（根路径本身拒绝）；delete/purge
  目标按 preview kind 决定拼写（目录保留尾斜杠、文件去除）；digest 起点
  path 去尾斜杠后回显。
- **空串**：`dataBase64: ""` 合法（建空文件）；其余必填字符串参数空串报
  `Parameter '<key>' must be a non-empty string`。
- **缺参枚举式（第七轮拉齐 ssh 口径，MCP_ACCEPTANCE §3.9）**：业务 required
  参数缺失（absent / null）时一次枚举全部缺口，报
  `Missing required parameters: <a>, <b>`（按 schema required 顺序），LLM
  一轮补齐全部参数而非逐轮 fail-fast。`connectionId`（及内联 `connection`
  二选一）不在枚举内：连接寻址缺失仍报单数 `Missing required parameter:
  connectionId` + 内联参数指引——与 stdio schema `anyOf` 放宽后的 required
  集合一致（核对器空参探针比对口径），连接门先于业务参数校验的顺序张力
  登记为设计（核对器 WARN）。present-but-类型错误不混入枚举，由
  `Parameter '<key>' must be ...` 精确单独点名（两类错误可区分，避免 LLM
  原样重发）。
- **枚举参数（format）大小写归一（第七轮，§3.3）**：`files_scan_digest`
  的 `format` 接受任意大小写/首尾空白（`"ROWS"`、`" Rows "`、`"Digest"`），
  归一到 `digest`/`rows`；非法值报 `format must be 'digest' or 'rows'
  (got '<原值>')` 列出合法值，绝不静默降级为 digest。`depth` 非数字报错
  附合法范围：`depth must be a non-negative integer; depth accepts an
  integer in 1..=16`。
- **cursor/confirm 过期消息**：报文携带实际生效 TTL（`TTL <n>s`，随
  `mcp/settings/set` 变化），并引导重发 digest / 重开预览；未知 cursorId
  同样引导 `re-run files_scan_digest`。**真实时间轴实测**（smoke M13）：
  把两个 TTL 调到 10s 下限后真实等待过期，两条报错在真实时钟上携带
  `TTL 10s`，且过期 token 从未执行删除（fail-safe）。
- **未知参数**：一律容忍忽略（部分更新语义）。
- **未知 intentId**：报错附引导（intent 进程内 60s 过期；重发 files_ui_*
  调用，或省略 intentId 读最新快照）。
- **未知工具名**：报错附 `Did you mean` 变体建议（`files-scandigest`、
  `FILES_SCAN_DIGEST` 这类分隔符/大小写变体直指注册名）+ 全部 12 个注册
  工具名 + 发现面（桥 `mcp/tools` / stdio `tools/list`）；stdio 入口对
  未注册名先报 Unknown tool，再谈连接参数，避免 LLM 被引去补
  connectionId。

## 路径形状门（2026-09-13 第五轮，对抗输入）

MCP 面所有路径参数（write/mkdir/rename/delete/purge/digest）在业务语义
之前统一过一道**形状门**（`validate_path_shape`），拒绝没有任何合法调用
会发出、任何后端都不该被要求解释的拼写——每一类都给清晰报错并点名参数：

- **`..` 段**（父遍历）：`/..`、`/a/../..`、`a/../../etc` 等一律拒绝——
  MCP 面只讲连接根下的绝对路径；这同时封死 `purge "/.."` 类根红线绕过
  面（framed 面由 `policy.sanitize` 的栈式归一拒绝，两面对齐）。
- **`.` 段**：`/.`、`//.`、`/a/./b` 一律拒绝——`/.` 语义上就是连接根的
  混淆拼写，必须撞红线而不是被 trim 吞掉后绕过 `refuse_root_purge`；
  `/a/./b` 属冗余拼写，fail-fast 优于猜测。（framed 面 `.` 段等价吞并
  是记录在案的双面语义差异。）
- **控制字符**（JSON 可携带 `\u0000`/`\n`）：拒绝并点名，绝不透传后端
  产生 OS 层怪错或不可管理文件名。
- **超长路径**（>4096 字节）：主动边界，替代后端 `ENAMETOOLONG` 类
  透传错误。

**设计内保守行为（钉测试 + 此处存档，不是缺陷）**：协议**不**做 `~`
展开（`~/x` 是连接根下的字面 `~` 目录）、**不**做 URL 解码（`%2e%2e`
是字面目录名，绝不逃逸）、**不**做 unicode NFC/NFD 归一化（两个拼写
是两个独立条目，与 fs 字节语义一致）——三者都无法走私遍历或根绕过，
只会寻址到「字面同名」条目。purge/delete 根红线对 `/`、`//`、空白
（归根）、`/.`、`/..`、`/../` 等对抗拼写在 MCP 面全部拒绝且磁盘树完好
（smoke T 段 + Rust 单测钉死）。

## 降级矩阵（设计 §5）

| 场景 | `files_ui_*` | digest / cursor | 写工具 |
| --- | --- | --- | --- |
| 工作台打开、前端在线 | applied + summary | 可用 | 可用 |
| 前端在线但面板无该 action | rejected + reason | 可用 | 可用 |
| 工作台未打开 / 前端未响应（含前端接线未完成） | pending → 超时 hint（引导走 digest） | 可用 | 可用 |
| 独立 stdio `--mcp` | 明确 `UNAVAILABLE`（"此工具需要 DBX 工作台"），不假死 | 可用（凭据内联/localFs） | 可用（两阶段照旧；目录 rename 除外） |
| 连接只读 / allow_delete=false | 不受影响 | 可用 | 不注册进工具清单 |
| Scoped AI 会话 | 宿主禁 `dbx_call_plugin_tool` | 同左 | 同左 |

## 状态机验收用例

digest/cursor/confirmToken/intent 纯逻辑的验收用例清单（三插件同表，防
形状漂移）单点维护在 `shared/frontend/README.zh-CN.md`「MCP 两阶段/
digest/cursor 验收用例清单」，files 侧对应 `backend/src/mcp.rs`
`#[cfg(test)]`（settings/glob/digest 聚合/cursor LRU+TTL/confirmToken/
intent 状态表/16KiB 截断/只读工具清单/写门禁/数值字符串与整型浮点容错/
删除目标拼写/文件目标根拒绝/TTL 实值报错/stdio 内联 localFs 全链路含
purge 成功路径与空写尾斜杠用例；第五轮增：stdio 结构校验 -32600、
jsonrpc 宽容、行上限 env 解析、confirm 表 churn+上限淘汰、TTL 不追溯、
cursor churn 与 settings 即时生效、物化上限 churn 循环、intent churn 与
快照、digest 幂等 ×100、路径形状门对抗表、根红线对抗拼写、`~`/URL
编码/NFC-NFD 字面语义）。

## smoke

```bash
python3 scripts/smoke_mcp.py
# 或 DBX_PLUGIN_SIDECAR=/path/to/dbx-plugin-files python3 scripts/smoke_mcp.py
# 场景 M1–M13 + R1–R7 + S1–S4 + B1 + T1–T6（与 ldap smoke_mcp.py 同表 + files 扩展）：
# M1–M9 离线（settings/tools/只读门/intent pending 与 applied/快照/门禁/
# 未知方法）+ M10 fs 临时目录场景（建树走 MCP 写工具 + digest+cursor+
# 两阶段删除+purge 成功路径+purge 红线+quick_paths limit+审计
# source:"mcp"）+ M11 LLM 输入变体（数字字符串谓词/空 dataBase64 建
# 空文件/尾斜杠真删/未知参数容忍/cursor 未知引导）+ M12 真实 10 万条
# 大树 clamp（scanned 计至 budget+1 的越界探测条目即停、matched 封顶
# 10 万、scanTruncated；cursor 物化封顶 1 万行，cursorTruncated + 显式
# offset 10000 起翻页为空；DBX_FILES_MCP_CLAMP_FILES 可调树大小）+
# M13 TTL 真实超时（cursorTtlSecs/confirmTtlSecs 调到 10s 下限，真实
# 等待过期，报错携带实际 TTL 且不执行删除）。
# R1–R7 远端协议容器段（stdio 内联凭据真跑全链路：write/mkdir → digest
# 聚合 → cursor 翻页 → 两阶段 delete（篡改作废+确认执行）→ 两阶段
# purge，删除结果以 re-digest matched 校验；建树在唯一可写 base 目录
# 下，不依赖连接 root 的自动创建）：R1 s3（MinIO，env
# DBX_FILES_S3_*）、R2 webdav（mod_dav，env DBX_FILES_WEBDAV_*）、
# R3 ftp（pyftpdlib，env DBX_FILES_FTP_*）、R4 sftp（OpenSSH，
# keyfile-only：内联 payload 不带 password，ssh:// URI endpoint +
# knownHostsStrategy accept，env DBX_FILES_SFTP_*）、R5 smb（Samba，
# share 限定 + 裸 host:port endpoint + 自研适配器递归 purge，env
# DBX_FILES_SMB_*）、R6 sftp-native（russh，password 认证——OpenDAL
# sftp 服务做不到的形态，env DBX_FILES_SFTP_NATIVE_*）、R7 oss（env
# DBX_FILES_OSS_*：无 OSS API 兼容容器——MinIO 只讲 S3 API 不讲阿里云
# OSS API，常驻 SKIP 并如实记录原因；对真实 OSS endpoint 设 env 即可
# 启用）。env 缺失自动 SKIP 不 FAIL，
# scripts/container_smoke.sh 提供全部容器与 env。
# S1–S4 独立 stdio 模式（真实 `--mcp` 进程：initialize/tools/list/
# notifications/未知方法 -32601/结构性 -32602/工具级 -32000/parse error
# -32700、UI 工具 UNAVAILABLE、localFs 内联凭据 digest+cursor+quick_paths
# 真实往返、两阶段 delete 全流程含参数篡改作废与磁盘校验）。
# B1 桥接兜底段（进程内 mock 宿主桥）：桥未发布 30s 唤醒预算后 fail-closed
# （"DBX app bridge" 原因 + 内联凭据出路）、mock 桥转发契约五字段 + envelope
# 逐字透传、桥 404 透出。本地段无外部依赖，必跑不 SKIP；未注册方法
# SKIP 不 FAIL（M0 §5.2）。
# T1–T6 stdio 传输层健壮性段（第五轮，每条对抗断言后跟一次合法请求健康
# 探针——进程不崩、不假死、管道不丢）：T1 非法 JSON/非法 UTF-8 字节流/
# 二进制噪声 → -32700 且进程存活；T2 notifications 静默 + 无 id 请求
# -32600 + 连接可用；T3 缺 method/非字符串 method/id 为 object → -32600、
# jsonrpc 字段宽容（族一致形状钉死）；T4 超长行——8 MiB 参数（≈10.7 MiB
# 行，行上限 16 MiB 内）走 4 MiB 写上限的 -32000 错误路径、调小
# DBX_FILES_MCP_STDIO_MAX_LINE 后超限行在读取层被拒（-32700 点名 env）；
# T5 pipelining：4 个不同 id 不等响应连发（夹一条垃圾行），响应与 id 一
# 一对应；T6 空行/空白行静默 + CRLF 请求正常解析。
```

### 深水区实测记录（2026-09-13 第二轮）

- **10 万条 clamp（M12，真实目录树）**：扁平 100 001 文件目录 →
  `scanned=100001 / matched=100000 / scanTruncated=true`——walk 在越过
  预算的第一条目停止，该条目计入 `scanned`（这就是截断的发现方式）但
  不保留，`matched` 精确封顶 10 万；cursor 物化 10 万 matched 行截到
  1 万，`cursorTruncated=true`，翻页在 offset 9999 处剩 1 行 done=true，
  显式 `offset:10000` 起恒为空。
- **TTL 真实超时（M13，真实时钟）**：`cursorTtlSecs:10` +
  `confirmTtlSecs:10`（两者 sanitize 下限即 10s）→ 真实等 10.6s →
  `cursor expired (TTL 10s)` 与 `confirmToken expired (TTL 10s)` 均在
  真实时间轴上携带实际生效 TTL（第一轮为单测注入时钟），且过期的
  confirm 从未执行删除（digest 复查文件仍在）。
- **远端协议 stdio MCP 往返（R1–R3，真实容器）**：s3/webdav/ftp 三协议
  上 `mcp-inline-<hash>` 内联凭据连接真跑 agent 全链路（write → digest
  → cursor → 两阶段 delete → 两阶段 purge），删除以 re-digest matched
  计数校验（MCP 面不读文件正文，matched 即真相源）；对象存储目录标记
  的 kind 宽松断言（`dir|missing`），执行结果以强断言（matched==1）
  兜底。第一轮「远端协议只有构建/校验路径」的遗留至此关闭。

### 远端 R 段全覆盖与 root 隔离陷阱（2026-09-13 第四轮）

- **R 段扩到 7 协议（R1–R7）**：新增 R4 sftp（OpenSSH，keyfile-only——
  内联 payload 不带 password，`ssh://` URI endpoint + knownHosts
  accept）、R5 smb（Samba，裸 host:port + share 限定 + 自研适配器递归
  purge）、R6 sftp-native（russh，password 认证全链路——该协议存在的
  理由）、R7 oss（常驻 SKIP：无 OSS API 兼容容器，MinIO 只讲 S3
  API；对真实 OSS endpoint 设 `DBX_FILES_OSS_*` 即可启用，secret 经
  s3 形状绑定落到 OpenDAL `access_key_secret` 键）。共享循环
  `run_remote_stdio_roundtrip` 七协议复用。
- **root 隔离陷阱（R4/R6 首跑真实发现）**：共享循环原样复制
  R1–R3 的 `root=/mcp-smoke-<uuid>` 隔离法，SSH 类受限 home 服务器
  首跑双双 FAIL——
  - **R4（OpenDAL 0.57 sftp 服务）**：root 自动创建循环的
    `is_sftp_protocol_error` 把**任意**协议错误码（含 PermissionDenied）
    当"目录已存在"吞掉，`/mcp-smoke-<uuid>`（sftpuser 无权在文件系统
    `/` 下创建）根本没建成，写路径 `canonicalize`（SSH_FXP_REALPATH）
    才报 `NoSuchFile`——错误点与根因相距两层，误导性强。
    `connection/test` 的 `check()`（root stat）可提前暴露；上游缺陷，
    本仓不 patch OpenDAL。
  - **R6（sftp-native 自研适配器）**：stat-first mkdir -p 诚实地把
    `mkdir /mcp-smoke-<uuid>` 的 PermissionDenied 报了出来（行为正确，
    同一 root 选址错误的另一种表现）。
  - **修复（场景侧）**：共享循环改为"唯一可写 base 目录"建树
    （`files_mkdir` 在各后端都是 mkdir -p，base 自物化；sftp/sftp-native
    的 base 落在 `/config`（容器用户 home）下，与 framed smoke 同形），
    不再依赖任何协议的 root 自动创建语义；R1–R3 同步迁移后容器下
    25 场景 24 PASS 1 SKIP（R7 按设计 SKIP）。连接 root 指向用户不可
    创建位置时的可行动结论：sftp/sftp-native 的 root 应指向已存在或
    父目录可写的路径（如用户 home 下）。
