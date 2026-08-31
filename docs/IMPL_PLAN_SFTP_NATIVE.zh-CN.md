# Files 插件 SFTP Native 后端方案（russh + russh-sftp 双栈适配层）

状态：已落地（本轮实例测试驱动）。关联：`IMPL_PLAN_DBX_FILES.zh-CN.md` §5（引擎）、
`IMPL_PLAN_SMB.zh-CN.md`（同构先例：自研 `opendal::raw::Access` 适配器）。

## 0. 双栈决策（2026-08-31）

OpenDAL 0.57 的 `sftp` service 底层走 ssh 二进制，**只支持 keyfile 认证**
（无 `password` 配置项，engine 对密码刻意不转发）。密码型账号是企业 MFT
（mft-int-test 等）的常态，当前接不进来。真机实例测试实证：password-only
连接在 `connection/test` 与 `files/list` 上均失败。

决策：**双栈并行，新增 `sftp-native` 快捷协议**，OpenDAL `sftp` 原样保留：

- `sftp`：OpenDAL service（keyfile-only），存量连接不受影响；
- `sftp-native`：`engine/sftp_native/` 自研适配器，russh 0.63 + russh-sftp
  2.4（ssh 插件同款技术栈），密码 + keyboard-interactive + 私钥认证；
- 若 OpenDAL 后续版本支持密码认证，`sftp` 可整体切回 via_iter 路径，
  `engine/sftp_native/` 退役。

**不直接复用 ssh 插件代码**：`ssh.rs` 与 SshRuntime/session/PromptBroker
深耦合；files 只需要 connect+auth+sftp 通道的薄层。复用的是"栈与经验"，
不是代码。

## 1. 协议契约（sftp-native）

`StoredConnection(protocol="sftp-native")` → `engine::build_operator` →
`Operator::new(SftpNativeBuilder)` 静态分发（"sftp-native" 非 OpenDAL
scheme，与 smb 同理）。门控/审计/传输 job/能力声明全部协议无关，零改动。

external_config 字段（builder 键即 manifest/UI 字段键）：

| 键 | 必填 | 说明 |
| --- | --- | --- |
| `endpoint` | ✅ | `host[:port]` 或 `ssh://[user@]host[:port]`，端口缺省 22 |
| `user` / `username` / endpoint `user@` | ✅ 三选一 | 登录用户，优先级 user > username > endpoint |
| `password`（secret binding） | 二选一 | 密码认证，失败自动回退 keyboard-interactive（PAM 网关） |
| `key` | 二选一 | 私钥内容或路径（`~/` 展开）；密码为空时生效；加密私钥暂不支持 |
| `root` | - | 远端子路径（normalize_root）；lock_to_root 由策略层执行 |
| `known_hosts_strategy` | - | `Strict` / `Tolerate`（默认）/ `Trust`，见 §2 |

能力契约：`rename: true`（SSH_FXP_RENAME 原生，不覆盖已存在目标）、
`copy: false`（走既有读→写降级 job）、`presign: false`（publicLink 报
unsupported），其余与 smb 一致。

## 2. 安全与兼容性红线

- **凭据**：password/私钥只在内存 Builder 与连接参数中，secret 走宿主
  binding；Debug 手工实现不渲染凭据，日志/错误只含路径与状态码。
- **known_hosts**：Strict 校验 `<data_dir>/known_hosts`（缺失即拒绝）；
  Tolerate = accept-new（已知 key 变更仍拒绝，未持久化新 key）；Trust 全放行。
  证书主机（Certificate）仅 Trust 放行。
- **绝对路径红线**（SMB SmbDeleter 回归的教训）：SFTP 相对路径按**服务器端
  cwd** 解析，因此所有 wire 路径（deleter 在内）必须经 `sftp_path()`
  重加前导 `/`，映射为 root 内绝对路径。`create_dir` 的逐级前缀同样带
  `/`——曾因丢前导斜杠把目录建到 cwd 下（本地容器确定性复现后修复）。

## 3. 传输兼容性（真机实测驱动）

mft-int-test.int.kn（非 OpenSSH 的 MFT 网关）实测：

- 单个 147 KiB 的 SSH_FXP_WRITE 请求会**挂死**（无 ack、无错误），128 KiB
  通过；OpenSSH CLI 默认 32 KiB 分块；
- russh-sftp 默认 8 深写管线在真实服务器上偶发卡死。

参数定为 `max_packet_len = 64 KiB`、`max_concurrent_writes = 4`、
`request_timeout_secs = 30`（russh-sftp `Config`）。真机复测：147 KiB→2 MiB
上传+下载全部通过。吞吐受 RTT 限制，属兼容性优先的取舍。

## 4. 依赖与版本

`russh = "0.63.1"`（crate 搜索确认 0.63.1 为该线最新）：0.60.x 整线把
aes-gcm 钉在 `=0.11.0-rc.3`，与 `smb2 =0.20.1`（要求 aes-gcm 稳定
`0.11.0`）同系列不可共存，cargo 无法同时满足；0.63.1 已迁稳定
pkcs5/pkcs8，MSRV 1.85 适配宿主 1.88 工具链。ssh 插件维持 0.60.3
（无 smb2 冲突），两插件版本暂不一致，russh 升级时需重新评估 smb2 约束。

`russh-sftp = "2.3"`（解析至 2.4.0）：`SftpSession` 方法 `&self` + 独立
File 句柄（tokio AsyncRead/Write），文件句柄绕开会话互斥锁，流式传输不阻塞目录操作。

## 5. 测试计划与记录

- 单测（backend cargo test，131 全绿）：endpoint 解析表、strategy 解析、
  Configurator kv、离线建 Operator + 能力断言、user 优先级、坏 key 拒绝、
  **绝对路径映射**（含 root 为 `/`）；
- 容器回归：linuxserver/openssh-server（密码形态）200 KiB→600 KiB 上传+
  下载全过（0.0–0.1s）；
- **真机实例**（mft-int-test.int.kn，密码形态，root=`/pub`）：smoke
  `sftp-native` 段 20 场景全绿（含 rename-dir-degrade、purge、stat、rmdir、
  二进制通道 round-trip、取消、publicLink 拒绝、只读门），整体
  PASS 69 / SKIP 3 / FAIL 0；只动自建 `/pub/smoke-native-*` 沙盒，收尾
  purge 恢复原状。

已知边界：加密私钥（passphrase）暂不支持（与 OpenDAL sftp 对齐，报清晰
错误）；`via-dbx-ssh` 隧道模式不暴露给 sftp-native（仅 direct）；目录改名
`rename` 语义为不覆盖（SFTP 无 posix-rename 扩展协商）。

## 6. 任务记录

| 项 | 状态 |
| --- | --- |
| F-SFTP-N-1 engine/sftp_native 三件套（mod/pool/access） | ✅ |
| F-SFTP-N-2 engine/model 接线 + PROTOCOLS 扩容 | ✅ |
| F-SFTP-N-3 manifest 协议/字段/七语 | ✅ |
| F-SFTP-N-4 前端 opendalServices 模板 + spec | ✅ |
| F-SFTP-N-5 smoke sftp-native 段（密码形态） | ✅ |
| F-SFTP-N-6 真机实例验证（mft-int-test） | ✅ |
| F-SFTP-N-7 本轮发现的 SMB SmbDeleter root 缺陷修复 | ✅（`engine/smb/access.rs` deleter 补 root 映射 + 单测守卫，回归记 `PROGRESS-F5-SMB.zh-CN.md` §5） |
