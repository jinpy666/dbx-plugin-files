# Files MCP 使用说明

Files MCP 的协议细节、参数表和安全边界见 [MCP 参考](MCP.zh-CN.md)。本页只讲怎么用。

## 启动方式

- **推荐：DBX MCP 桥**。在支持 DBX 插件 MCP 的客户端（如 ZCode）中启用 `dbx-files`
  连接，调用会复用已保存连接、审批与权限边界，凭据不经过客户端。
- **独立 stdio 模式**（无宿主时调试用）：

```bash
cargo build --release --manifest-path backend/Cargo.toml
backend/target/release/dbx-plugin-files --mcp
```

stdio 帧上限默认 16 MiB，可用 `DBX_FILES_MCP_STDIO_MAX_LINE` 调整（超限返回
`-32700` 并点名上限）。

## 工具清单（13 个，与 smoke 一致）

| 工具 | 用途 |
| --- | --- |
| `files_scan_digest` | 扫描目录，返回聚合计数/分布/样本与翻页游标 |
| `files_cursor_next` | 用 cursorId 按批取 locator 行，无需重发条件 |
| `files_ui_focus` / `files_ui_search` / `files_ui_select` | 驱动宿主工作台面板/导航/定位 |
| `files_ui_state` | 读 UI intent 结果或最新工作台快照 |
| `files_ui_quick_paths` | 连接的快捷路径 |
| `files_write` | 写小文件（base64，≤4 MiB，更大走传输通道） |
| `files_mkdir` | 建目录 |
| `files_rename` | 改名/移动 |
| `files_delete` / `files_purge` | 删文件 / 递归清目录（均两阶段） |
| `files_sync` | 跨连接目录同步/复制（增量对比；`sync:true` 镜像删除目标多余文件，先 `dryRun:true` 预演；返回 jobId 轮询 `files/transfer/status`） |

## 典型模式

**大目录检索**：先 `files_scan_digest`（digest 聚合 + 样本），要明细时用返回的
`cursorId` 调 `files_cursor_next` 翻页；游标 10 分钟过期，过期后重新 digest。

**两阶段删除**（`files_delete` / `files_purge`）：

```text
第一次  files_delete {connectionId, path:"…"}          → 预览 + 一次性 confirmToken（60 秒）
第二次  同参数 + confirmToken（参数 hash 须一致）        → 执行 + 审计
```

`files_purge` 拒绝连接根与 `/`（红线）。所有 MCP 写操作在审计日志中标记 `source:"mcp"`。

## 离线验证

```bash
cargo build --release --manifest-path backend/Cargo.toml
python3 scripts/smoke_mcp.py --binary backend/target/release/dbx-plugin-files
```

该 smoke 不需要真实存储服务；涉及真实后端（S3/WebDAV/FTP/SFTP/SMB 等）的用例必须
显式提供环境变量，并在环境不可用时按脚本输出 `SKIP`，不能把离线通过当作 live 连接
通过。Windows 平台 `sftp` 快捷协议不可用，相关 live 用例请使用 `sftp-native`。
