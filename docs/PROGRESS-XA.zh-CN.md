# X-A 波次进度报告（io.dbx.files — 跨路收口 + s3 集成冒烟）

> 波次：X-A（F-A/F-B/F-C 全部落地后的收口波）。上游契约：
> `IMPL_PLAN_DBX_FILES.zh-CN.md` §8（方法契约）、§9（安全门禁）、§10（测试）。
> 输入：F-B 交接三项（见 §2）、F-C 交接（s3/sftp 容器 smoke 就绪）。
> 记录日期：2026-08-28。仓库 `/Users/Jinpy/btroot/dbx-plugins/files`。

## 1. 收口三项结果（F-B 交接）

### ① 门禁语义统一（更严语义，已落地 + 补跨层等价单测）

- `backend/src/main.rs::ensure_deletable`：`read_only` 拒绝一切变更操作
  （delete/purge/rmdir 及 move/rename 的隐式删源），`allow_delete` 独立拒绝
  delete 类操作——与 `policy::PathPolicy::check_delete` /
  `engine::ops::Gate::ensure_deletable_path` 同一语义（§9 更严侧）。
- 新增对齐单测 `main.rs::tests::gates_match_policy_layer_semantics`：遍历
  `read_only` × `allow_delete` 2×2 全矩阵，断言 main.rs 门禁与
  `policy::PathPolicy::check_write/check_delete` 判定逐格一致。
- 原 `read_only_gate_blocks_writes_and_deletes` /
  `allow_delete_gate_blocks_deletes` 保持通过。

### ② capabilities 收口（已落地）

- `main.rs` 的 `files/capabilities` 分支由内联投影改为调用
  `engine::ops::capabilities(&operator)`（`info().full_capability()` 单一
  逻辑源），wire 形态不变（camelCase：scheme/list/write/read/stat/delete/
  createDir/copy/rename/presign）。smoke `capabilities` 场景 fs/memory/s3
  三段全绿。

### ③ 降级 copy/move 异步化（本轮实施，改动量适中故落地）

- 决策：跨实例/能力缺失的 `files/copy|files/move` 降级路径上移 main.rs，
  经 `JobTable` 入队返回**真 jobId**（此前 `jobId: null`、内联同步执行）。
- `backend/src/engine/ops.rs`：
  - 新增决策谓词 `native_copy_available` / `native_move_available`
    （same-instance + capability 判定单一逻辑源，`ops::copy` /
    `ops::move_path` 与 main.rs 路由共用）；
  - `is_dir_path` 改 pub（transfer 层目录/文件 plan 分支复用同一 stat 规则）；
  - `ops::copy/move_path` 的内联降级实现保留（`files/rename` 降级路径与
    单测仍依赖，作为单一逻辑参考实现）。
- `backend/src/transfers.rs`：
  - 新增 `JobTable::enqueue_copy_job`：目标 `read_only` 拒绝、拒绝以连接根
    `/` 为源；入 dir-job 表（kind `dirJob`），复用 `run_dir_job` 的
    FIFO/全局并发 3/取消/进度节流/终态迁移机制；
  - `DirJob` 新增 `delete_source` 字段（wire 名 `deleteSource`）；
  - `run_dir_job` 扩展：单文件源 plan（`relative=""`，cp 语义拷贝到
    targetPath 本身）；`delete_source`（降级 move）仅在拷贝成功后删源；
    目标基目录 `create_dir` 仅对目录源执行（修复点，见 §3.1）。
- `backend/src/main.rs`：copy/move 分支按谓词路由——native → 内联同步
  （`transport: "native"`）；否则 `enqueue_copy_job` →
  `{transport: "job", jobId: <uuid>}`。审计与 -32000 错误语义不变。
- `scripts/smoke_test.py`：`copy`/`move` 场景对返回 `jobId` 的降级路径轮询
  `files/transfer/status` 至终态（`wait_job`，30s 超时），再断言结果。

### 修复的两个真实缺陷（异步化过程中暴露）

1. **`join_target` 空 relative 尾斜杠**：`format!("{base}/{relative}")` 在
   `relative=""`（单文件 plan）时产出 `"base/"`，导致 memory 后端
   NotFound。修复为空 relative 直接映射 base 本身（含单测）。
2. **单文件目标被误建目录标记**：`run_dir_job` 原无条件对 target_path 执行
   `create_dir`，单文件 copy/move 会把目标文件路径建成目录标记
   （memory/s3 等前缀后端列表出现同名 dir 项）。修复为仅目录源预建目标基
   目录（文件目标的父目录由逐 plan 项逻辑创建）。

## 2. s3（MinIO）集成冒烟

- 容器：`minio/minio` 单节点单盘（RELEASE.2025-09-07），`-p 127.0.0.1:9000`，
  凭据经 shell 环境变量注入（`-e MINIO_ROOT_USER/-e MINIO_ROOT_PASSWORD`），
  随机生成、零字面量落盘；bucket `mc mb local/dbx-files-smoke`；收尾
  `docker stop`（容器已停止删除，`--rm`）。
- **发现并修复 `smoke_test.py` s3/sftp 段凭据封装缺陷**：
  `secret_access_key`/`password` 是 secret 绑定字段，`model.rs` 只从
  `connection.connection_secrets` 解析，脚本原样放进了 `external_config` →
  sidecar 拿到空密钥 → reqsign 回落 env/IMDS 凭据链 → 表现为请求挂起/超时。
  修复：`connect()` 新增 `secrets` 参数，s3 段
  `secrets={"secret_access_key": ...}`，sftp 段 `secrets={"password": ...}`
  （与宿主 lifecycle 封装一致）。
- s3 段结果（SKIP → 全 PASS）：
  mkdir / write / list / read / copy(native) / move(native) / rename /
  delete / purge-refuses-root / purge-directory / publicLink-presign /
  upload-download-roundtrip / transfers-list-status / transfer-cancel
  **全绿**。
- 排查过程中产出的临时 example（`backend/examples/xa_s3_probe.rs`）与
  /tmp 探针文件已删除；凭据未写入任何文件/日志/报告。

## 3. 全量验证

| 项 | 结果 |
|---|---|
| `cargo build` | 通过（dev profile） |
| `cargo test` | **68 passed / 0 failed**（基线 67 无回归 + 新增 1：跨层门禁等价；join_target 用例扩展计入原测试） |
| `cargo build --release` | 通过；release 二进制 21,773,904 B（≈20.8 MiB，含全套 TLS/HTTP 栈，与 F2-0 基线同量级） |
| `smoke_test.py`（fs+memory，无容器 env） | **PASS 30 / SKIP 2 / FAIL 0**（fs 15 + memory 15，无回归；SKIP 为 s3/sftp 容器段 env 缺省语义） |
| `smoke_test.py`（+MinIO env，容器内运行时） | **PASS 44 / SKIP 1 / FAIL 0**（s3 段 14 场景 SKIP→PASS；仅 sftp 段保留 SKIP，属 M3） |
| `dbx-plugin package .` | `dist/io.dbx.files-0.1.0-darwin-arm64.dbxp`（9,110,585 B）+ artifact.json，unsigned review candidate |
| 错误语义 | 业务错误 -32000（`to_plugin_error`）、未注册 -32601，未改动 |
| 凭据红线 | MinIO 凭据仅存在于 shell 环境变量与容器 env；代码/脚本/文档/日志零凭据字面量 |

复现（凭据经环境变量传入，切勿写文件）：

```
export DBX_FILES_S3_ENDPOINT=http://127.0.0.1:9000
export DBX_FILES_S3_BUCKET=dbx-files-smoke
export DBX_FILES_S3_ACCESS_KEY=... DBX_FILES_S3_SECRET_KEY=...
python3 scripts/smoke_test.py
```

## 4. 遗留 / 已知限制

1. **copy/move 降级 job 不出现在 `files/transfers/list`**：与 syncDir/
   copyDir 一致（list 仅合并单文件 job + transfers.json 历史），可经
   `files/transfer/status`（kind `dirJob`）与 progress 事件查询。如需进
   list，需 DirJob→列表项映射（前端适配），留待后续波次。
2. **前端对降级 copy/move 的 jobId 轮询**：后端契约 `{success, transport,
   jobId}` 已就绪；前端 `lib/transfers.ts` 已支持 jobId 事件驱动，但
   FileTable 的 copy/move 动作若仍按同步结果刷新列表，需要补「transport=job
   时等待终态再刷新」的交互（本轮未动前端）。
3. **rename 目录仍明确拒绝**（OpenDAL rename 仅文件，F-B 已知限制）；
   目录改名/移动走 copy + purge。
4. sftp 容器段（M3）保持 SKIP；本轮顺带修好其 password 封装，落地后
   `DBX_FILES_SFTP_*` 即可启用。
5. `connection/disconnect` 的 `catch_unwind` 兜底仍在（F-C 已真实现
   `cancel_connection_jobs`，兜底仅为防御）；`load_history` 启动注水仍未
   接线（list 直读 store，功能无损）。

## 5. 阻塞

无。
