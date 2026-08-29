# F5-SMB 交付报告（files 插件 Rust 原生 SMB client）

> 方案：`IMPL_PLAN_SMB.zh-CN.md`（`smb2 =0.20.1` + OpenDAL 0.57 自定义
> Access 适配层，第 6 个快捷协议 `smb`）。实施日期：2026-08-29。
> 三轨道并行实施（后端适配层 / 配置面前端 / smoke 容器段），收口集成验证。

## 0. 验证终值

| 套件 | 结果 |
|---|---|
| `cargo test`（backend/ 全量） | **120 passed / 0 failed / 3 ignored**（新增 ~10 个 smb 单测：endpoint 解析表、kv 解析、capability 契约、lifecycle 含 share/domain、路径代数、endpoint 卫生） |
| 前端 vitest | **73 passed**（26 例 opendalServices 含 smb 模板用例；i18n 七语 key-set 对齐自动覆盖新增 key） |
| 前端 typecheck（vue-tsc） | ✅ |
| `scripts/test.sh` 全套 | **all green**（cargo test → 前端三件套 → release 构建 → framed smoke → 打包 .dbxp） |
| `scripts/container_smoke.sh`（真实 MinIO + OpenSSH + **Samba** 容器） | **PASS 103 / SKIP 0 / FAIL 0**；smb 段 21 场景全过：connection-test、capabilities 契约、mkdir/write/list/read/copy/move/rename/**rename-dir-degrade**、delete、purge 拒根、purge-directory、stat、rmdir、上传/下载二进制通道往返、transfers list/status/cancel、publicLink 明确不支持、read-only 门禁 |
| release 二进制体积 | 23.47 MiB → **24.51 MiB（+1.04 MiB / +4.4%）**，符合方案 §4 预估 |

## 1. 逐任务完成

| # | 任务 | 状态 |
|---|---|---|
| F5-S1 | `smb2 =0.20.1` 接入 + 体积/lock 基线 | ✅（`aes` 0.9.3 MSRV 1.89 与工具链 1.88.0 冲突，`cargo update --precise` pin 0.9.2） |
| F5-S2 | `engine/smb/` 适配层（Builder + Access + ReconnectPolicy，懒连接） | ✅ |
| F5-S3 | 结构操作 + capability 声明 + engine/mod.rs 接线（protocol_kv/build_operator 特判/出网校验 smb 分支） | ✅ |
| F5-S4 | 传输槽零改动验证（OpenDAL Reader/Writer 抽象自动生效） | ✅（transfer.rs/transfers.rs/main.rs/ops.rs 无 diff） |
| F5-S5 | manifest smb 字段（share/domain 新增；endpoint/username/password 复用扩 visible_when）+ opendalServices 模板/spec + i18n 七语 | ✅ |
| F5-S6 | smoke smb 段（env 门控 `DBX_FILES_SMB_*`）+ container_smoke.sh Samba 编排 + 契约单测 | ✅ |

## 2. 实施中发现并修复的真机问题（Samba 实测）

1. **目录路径尾 `/` 直传 SMB**：OpenDAL 目录条目路径带尾 `/`，原样进 SMB
   Create 报 `STATUS_OBJECT_NAME_INVALID`（crate 映射为 `InvalidName`，绕过了
   `IsADirectory` 回退）。修法：`SmbDeleter::delete_once` 先规范化（trim 尾
   `/`）并把 dir-ness 作为显式 hint 直达 `DeleteDirectory`。
2. **`FileReader` 句柄泄漏**：smb2 crate 的 `FileReader` drop 不关句柄（仅
   debug 日志），必须显式 `close().await`；而 OpenDAL `oio::Read` 无异步
   close 钩子。表现为 rename 降级 job 复制完立即删源时，源文件挂着未关读
   句柄（DELETE_PENDING）→ 删父目录报 `DIRECTORY_NOT_EMPTY`。修法：
   `SmbReader` 句柄改 `Option`，读到 EOF/范围尽头时主动 close；错误/取消路径
   的早 drop 仍按 crate 文档语义泄漏到会话结束（已注释）。

## 3. 关键实现要点（与方案差异点）

- 懒连接：`SmbBuilder::build` 同步、SMB 握手异步 → `SmbPool` 首次操作时
  dial（`Mutex<Option<SmbState>>`），断线靠 crate `auto_reconnect`，救不回
  则丢弃客户端下次重拨。
- 目录类操作（stat/list/mkdir/delete/rename）经 `tokio::sync::Mutex` 串行；
  读/写流打开后持独立 `Connection` clone 的 `'static` 句柄，不占互斥锁。
- capability：stat/read/write/create_dir/delete/list/rename + 递归变体全开
  （递归删除为适配器自实现自底向上栈式 purge）；**copy=false**（同连接 copy
  自动走既有 read→write 降级 job）、**presign=false**（publicLink 走既有
  「不支持」路径）。smoke 契约断言锁定该矩阵。
- 凭据红线：`SmbConnectParams` 手写 `Debug`（`finish_non_exhaustive`），错误
  映射仅回显 NTSTATUS；password 走 `connection_secrets`（宿主 secret binding）。
- manifest 复用既有字段（key 唯一）：endpoint/username/password 扩
  `visible_when` 加 smb；新增 share/domain 两字段；七语 label 同步入
  manifest.json 与 i18n.ts。
- smoke smb 段 password 走 `connection_secrets`（红线），引擎未落地时整段
  SKIP（`smb backend not landed yet`）。

## 4. 遗留与后续

1. **perf 基线未跑**（方案 §5）：传输通道按块重开 SMB 文件句柄（correctness
   优先），pipelined 预取优化空间与基线数据待 `perf_files_test.py` smb 段。
2. **`ccm =0.6.0-rc.3` RC 依赖 + `aes` 0.9.2 pin**：待上游 stable 收敛，
   升级走对标清单回归。
3. Kerberos 认证（crate 已有 `KerberosAuthenticator`）初版未接，NTLM 密码
   认证覆盖 smoke。
4. i18n 错误文案 key（`smbConnectFailed` 等）暂无组件消费点，UI 接线时用。
5. Windows 真机（非 Samba）行为差异未验：SMB1 不支持属预期；真机差异记本档。
