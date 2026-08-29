# F-C 波次进度报告（io.dbx.files — 异步传输 job / 传输槽 / 前端 / 脚本）

> 波次：F-C（分波并发第 3 波）。上游契约：`IMPL_PLAN_DBX_FILES.zh-CN.md`
> §7（异步 job）、§8.3（传输槽）、§8.5（前端）、§10（测试）。
> 前置：F-A（骨架 + 契约冻结）、F-B（ops 层，另波）。

## 1. 完成项

### 1.1 后端（Rust）

- **`backend/src/transfers.rs` 补齐全部 `todo!()` 占位**（冻结签名未动）：
  - 单文件 job 状态机 `queued → running → completed | failed | canceled`，
    终态迁移唯一化（`complete_job`），历史恰好落盘一次；
  - 全局并发信号量 3 + 同 `connectionId` FIFO（`tokio::sync::Mutex` per
    connection，按 id 排序获取，无锁序环）；上传为宿主驱动帧序，不入队等待；
  - 取消：`tokio CancellationToken` 语义经 `Arc<AtomicBool>` 协作标志实现
    （不新增 crate 依赖），上传取消 abort Writer、下载在块间检查标志、目录
    job 逐文件检查；`cancel_connection_jobs` 供 disconnect（M0 §3.2）调用；
  - 进度事件 `files/transfer/progress`：≥200ms 或 ≥1% 才发（`Throttle`），
    终态迁移必发；payload 为冻结核心 `{taskId, transferred, total}` +
    `size/state/kind/connectionId/remotePath`（目录 job 为
    `filesDone/filesTotal/bytesDone/bytesTotal + state/sync`）——只含
    路径/ID，绝无凭据；
  - `files/syncDir` / `files/copyDir` 自研遍历 job：源递归枚举 → 逐文件
    native copy（同连接 + capability.copy）或 256KiB 分块 read→write 流式
    拷贝 → 汇总进度；`sync` 删除目标多余文件（空目录保留，见遗留）；
    入队前门禁：目标 `read_only` 拒绝、`sync` 且 `allow_delete=false` 拒绝；
  - `files/transfers/list`（进行中 + `transfers.json` 历史合并去重，按
    connectionId 过滤）、`files/transfer/status`（单文件 + 目录 job 双表）、
    `files/transfer/cancel`、`load_history`（注水内存镜像；list 始终直读
    store 保证重启可见）；
  - 历史持久化：`JobTable::new()` 保持零副作用，`Store` 经
    `OnceCell` 在首条记录时惰性挂载（默认数据目录），环形上限 200。
- **`backend/src/engine/transfer.rs`（新建）**：传输槽流式原语 —— 上传
  `writer_with(chunk(4MiB).concurrent(4))`；下载先 stat 再开 Reader（内建
  预取），`read(offset..end)` 严格夹在 stat 尺寸内（EOF 越界即错）；
  8 字节 BE offset 帧编码；`walk_files` 手工递归（避免引入 futures 依赖），
  返回相对被遍历目录的路径（rclone copy 语义：`copyDir /s → /d` 产出
  `/d/a.txt` 而非 `/d/s/a.txt`）。
- **`backend/src/engine/mod.rs`**：仅新增一行 `pub mod transfer;`（新模块
  必需的声明，最小跨界改动，已注释说明）。
- 协议对齐（ssh-sftp `sftp/upload|download` 语义）：上传帧 = 8B BE offset +
  ≤256KiB，offset 必须等于服务端期待值（错位硬错），超出声明 size 硬错，
  finish 时 close + 校验声明 size（backend close 返回的 content_length 非 0
  时再校验一次）；下载为 start 后泵任务按 offset 顺序自动推送帧，
  `finish_download` 幂等。

### 1.2 脚本（`scripts/smoke_test.py`，F-A 移交 bug 归零）

- **连接名不匹配**：`scenario_read_only` 原用 `{section}-readonly`，实际
  连接名是 `smoke-{section}-readonly` → `read-only-list-ok` 假 FAIL。已改为
  由 `run_core_sections` 传入真实连接 id；
- **FAIL 后挂死（exit 124）**：`drain_stderr()` 在进程存活时阻塞到 EOF，
  原 `finally` 前置导致挂起。已改为 close 前置再 drain；
- **下载对齐泵式协议**：删除 `files/download/chunk` RPC 依赖，改为消费
  `files/download/{taskId}` 通道按 offset 拼装（sidecar 泵式推送）；
- **状态字段**：`transfers-list-status` / `transfer-cancel` 断言改读
  `job.status`（兼容 `state`）；
- s3/sftp 段 env 缺省 SKIP 语义保持；F-B 未落地方法 SKIP 语义保持。

### 1.3 前端

- 骨架此前波次已基本齐全（App.vue + 7 组件 + 5 lib + 七语全量
  zh-CN/zh-TW/en/es/it/ja/pt-BR），本轮核对后补两处：
  - `App.vue::downloadEntry` 对齐泵式下载（删除 `files/download/chunk`
    RPC 轮询，直接收帧按 offset 落盘/组装）；
  - `lib/transfers.ts::applyList` 兼容后端 `totalBytes/transferredBytes`
    字段（原只认 `size/transferred/bytesTotal/bytesDone`，列表回显恒 0）。
- 依赖零新增（沿用 vue/@lucide/vue/vite/vitest/vue-tsc）。

## 2. 验证结果

| 项 | 结果 |
|---|---|
| `cargo test` | **40 passed / 0 failed**（含 F-A 原 31 个，零回归；新增 9：帧编解码 round-trip、walk 递归+尺寸、原语级上传下载 round-trip、目录拒载、throttle、join_target、dir job 门禁、list 合并去重） |
| `cargo build --release` | 通过（~5s 增量 / 出包全量 1m18s） |
| 前端 `pnpm typecheck` | 通过（vue-tsc 无错误） |
| 前端 `pnpm test` | 3 文件 17 测试全过 |
| 前端 `pnpm build` | 通过，产物写入 `ui/index.html`（129.97 kB js） |
| `smoke_test.py` | **PASS 10 / SKIP 22 / FAIL 0**：fs+memory 双段 `upload-download-roundtrip`、`transfers-list-status`、`transfer-cancel`、`purge-refuses-root`、`read-only-gate` 全绿；结构类场景因 F-B 未落地按 SKIP 语义跳过；s3/sftp 容器段 env 缺省 SKIP |
| syncDir/copyDir 端到端（临时脚本，内存后端跨连接） | copyDir 2/2 文件内容逐字节一致；syncDir 删除目标多余文件；read-only 目标门禁拒绝；目录 job 即时取消 → canceled；`files/transfers/list` 正确返回已完成 job |
| `dbx-plugin package .` | `dist/io.dbx.files-0.1.0-darwin-arm64.dbxp`（+ artifact.json，unsigned review candidate） |

## 3. 遗留 / 已知限制

1. **syncDir 空目录**：删除目标多余文件后，被清空的目录不回收
   （rclone sync 会删空目录）；数据正确性不受影响。
2. **上传 job 不占并发信号量**：上传进度由宿主推帧节奏驱动，与
   §7「全局 3 并发」严格语义存在解释空间；下载/目录 job 已严格执行。
3. **JSON 降级路径（§8.3 web 兜底 `files/upload/chunkJson` 等）未实现**：
   main.rs 未注册这些方法（main.rs 非 F-C 所有），宿主无 `host.binary` 时
   前端上传依赖 `window.dbxPlugin.fileTransfer`。
4. **`load_history` 未被 main.rs 调用**（启动注水钩子未接线）：`list` 直读
   store，重启后历史仍可见，功能无损；接线属 main.rs 所有方。
5. **walk_files 对「Unknown 模式」条目按文件处理**：个别后端 list 不回
   mode 时可能把目录当文件；主流 fs/s3/memory 已验证。
6. 进度事件中上传 append 的 inline 事件仅带 `state`，完整 kind/connectionId
   上下文由状态迁移事件携带（前端以 register 占位 + 轮询兜底）。

## 4. 交接

- **F-B（ops 层）**：smoke 结构场景（mkdir/write/list/read/copy/move/
  rename/delete/purge/read-only-list-ok）等待 F-B 落地后应自动转绿，harness
  无需再动；`files/copy|move` 若走 Job 降级，返回 `jobId` 命名空间与
  `files/transfer/status` 兼容（status 双表查询已就绪，但 ops 的降级 job
  需要调 `JobTable` 时另行接线——F-B 如需可将 `enqueue_copy_job` 挂到
  `JobTable`，接口已在 `Inner` 层面可复用）。
- **main.rs 所有方**：可选接线 `load_history`（启动注水，非必须）；
  §8.3 JSON 降级方法如需实现，`parse_upload_frame` / 帧编码可复用。
- **s3/sftp 容器 smoke**：设置 `DBX_FILES_S3_*` / `DBX_FILES_SFTP_*` 即可
  启用，传输槽场景已就绪。

## 5. 阻塞

无。F-B 未落地部分以 SKIP 语义隔离，未阻塞 F-C 交付。
