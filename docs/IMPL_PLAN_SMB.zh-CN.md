# Files 插件 SMB 后端方案（Rust 原生 SMB client）

> 上游：`IMPL_PLAN_DBX_FILES.zh-CN.md`（v3 OpenDAL + Rust 架构）。
> 目标：为 `io.dbx.files` 新增 SMB/CIFS 快捷协议，采用 **纯 Rust SMB2/3
> client crate + OpenDAL 自定义 Access 适配层**，不引入 FFI、不依赖系统
> libsmbclient、不做文件系统挂载。
>
> 背景缺口：OpenDAL 无 `services-smb`（63 个 service 不含 SMB）；tiny-rdm
> 时代的 SMB 能力来自 rclone 的 smb 后端，v3 迁移后随之丢失。本方案补齐。

## 0. 选型决策

### 0.1 候选对比（2026-08-29 调研）

| 方案 | 形态 | 结论 |
|---|---|---|
| **`smb2` crate（vdavid/smb2）v0.20.1** | 纯 Rust SMB2/3 client，tokio 异步，pipelined I/O（credit window 满窗并发），NTLM + Kerberos，签名（CMAC/HMAC）/加密（AES-GCM/CCM），自带 `list_shares`、`ReconnectPolicy`/`SessionReviver`、`Watcher`（change notify）、server-side copy（CopyChunk）；MIT OR Apache-2.0；edition 2021、**rust-version 1.85**（工具链锁定 1.88.0 可用）；依赖树干净（RustCrypto 0.11/0.13 新线 + `ccm =0.6.0-rc.3` 一处 RC pin） | **选定** |
| `smb` crate（afiffon/smb-rs）v0.11.2 | 纯 Rust SMB2/3，auth 走 `sspi` crate；sspi 锁 `digest =0.11.0-rc.5`，与新版 digest 0.11.2 冲突（smb2 仓库因此将 smb 基准排除在其 workspace 外）——本插件依赖树中同样存在 RustCrypto 新线依赖，**版本冲突风险高**；文档覆盖 ~48% | 备选，仅当 smb2 阻塞时重评 |
| `pavao`（libsmbclient FFI） | 绑定系统 libsmbclient，阻塞 API，携带 C 依赖 | 弃——违背"原生 Rust"约束，且 sidecar 需携带/依赖系统 samba 库 |
| 系统挂载（mount_smbfs / net use） | OS 层挂载点 | 弃——需特权、破坏沙箱与凭据红线 |

### 0.2 决策

**Files 插件 SMB = `smb2` crate（精确 pin `=0.20.1`）+ OpenDAL 0.57 自定义
Access 适配层（`backend/src/engine/smb/`）**，作为第 6 个快捷协议接入。
早期项目（2026 首发、star 数低）的成熟度风险以"适配层隔离 + 精确 pin +
Samba 容器冒烟"缓解（§6）。

## 1. 架构：OpenDAL Access 适配层（为什么不是旁路引擎）

OpenDAL 0.57 公开 `opendal::raw::Access` trait（RPITIT 异步风格，默认实现
返回 `ErrorKind::Unsupported`，按 `Capability` 声明能力），并支持从自定义
Builder 构造：`Operator::new(builder)?.finish()`（静态分派）。

据此 SMB 不做旁路引擎，而是**伪装成第 6 个 OpenDAL service**：

```
StoredConnection(protocol="smb")
  → protocol_kv() 组装 SmbBuilder kv          （engine/mod.rs 新分支）
  → Operator::new(SmbBuilder{...}).finish()   （静态分派，非 via_iter 注册）
  → 入连接表 OperatorEntry                     （连接表 / 门禁 / 审计零改动）
```

红利（全部复用既有实现，零新代码）：

| 既有机制 | SMB 下表现 |
|---|---|
| `files/*` 22 方法（§8.1–8.4） | 经 OpenDAL 抽象自动可用 |
| 跨服务 copy/rename 降级 job（§5.2） | capability 未声明时自动生效 |
| 异步传输 job + 二进制通道（§7/§8.3） | `Reader`/`Writer` 槽直接对接 SMB 流 |
| policy 门禁 / audit / secret binding | 不感知协议差异 |
| `files/capabilities` 透出 | `AccessorInfo.capability()` 自动驱动前端显隐 |

## 2. 适配层设计（backend/src/engine/smb/）

```
engine/smb/
├── mod.rs       # SmbBuilder（Builder + Configurator）+ kv 解析
├── access.rs    # SmbAccess: impl opendal::raw::Access
└── pool.rs      # SmbClient 生命周期与并发封装
```

### 2.1 连接与客户端持有

- 参数：`host[:port]`（缺省 445）、`share`、`username`、`password`（secret）、
  `domain?`（NTLM 域/工作组，可空）。连接目标 = `\\host\share` UncPath。
  **嵌套 share**（真机回归 2026-09，jobnote 服务器）：`share` 允许
  `共享名/子路径`（Icewind/smbclient 惯例，`/`、`\` 均可）——Windows 服务器
  拒绝对 share 子目录做 tree connect（`STATUS_BAD_NETWORK_NAME`），故
  `SmbBuilder::build` 先拆分：首段做 tree connect 目标，其余段落并入 root
  前缀（用户 `root` 拼在其下），后续路径代数全部复用既有 root 映射红线。
- `SmbClient` 持有 TCP 连接 + session，**非 Clone**——适配层以
  `Arc<SmbAccess>` 单客户端承载全部请求，利用 crate 的 pipelined credit
  window 做块级并发；方法级并发初版用 `tokio::sync::Mutex` 串行化目录类
  操作（list/stat），读写流走独立 file handle 不互斥。基准数据回写本文档后
  再评估是否升级为客户端池。
- 断线恢复：接入 crate 的 `ReconnectPolicy`/`SessionReviver`；恢复失败映射
  业务错误（宿主层重连语义不变：`connection/connect` 幂等重建 Operator）。
- `connection/test`：临时 Operator 上探测（真机回归 2026-09 起改法）：smb
  不走 OpenDAL `check()`——适配器对根路径 stat 是本地合成的 DIR 应答（smb2
  crate 无法 Create share 根路径），`check()` 从不拨号、坏 share 也假通过；
  改为真实 `files/list` 根列表（negotiate + session setup + tree connect +
  QUERY_DIRECTORY）。其余协议维持 `check()`。与 sftp_native 的真探测 stat
  同源教训（假阳性 test）。

### 2.2 操作映射与 capability 声明

| OpenDAL op | SMB 实现 | capability |
|---|---|---|
| `create_dir` | CREATE directory（mkdir -p 语义逐级建） | `create_dir=true` |
| `stat` | directory query / FileAllInformation | `stat=true` |
| `list`（Lister） | QUERY_DIRECTORY 流式枚举 | `list=true` |
| `read`（Reader） | FileReader 流式（pipelined 预取） | `read=true` |
| `write`（Writer） | FileWriter（chunk 聚合 + flush） | `write=true` |
| `delete`（Deleter） | delete disposition；递归走 delete_with | `delete=true` |
| `rename` | SET_INFORMATION FileRenameInfo | `rename=true` |
| `copy` | 初版**不声明**（`copy=false`）→ 同连接 copy 自动走既有 read→write 降级 job；后续用 crate 的 CopyChunk/server-side copy 升级 | `copy=false`（初版） |
| `presign` | 永不声明 → `files/publicLink` 走既有「后端不支持」路径（IMPL_PLAN §5.5 同款） | `presign=false` |

root 语义：OpenDAL `root` = share 内可选子路径（缺省 `/` = share 根）；
`lock_to_root` 门禁语义与既有协议一致。

### 2.3 出网校验（engine/mod.rs `validate_endpoints` 扩展）

- `smb` 协议 endpoint 接受两种形态：`host[:445]`（裸 host:port，同 ftp 先例）
  或 `smb://host[:445]`；拒绝 `file://` 等其余 scheme。
- 用户显式配置的可信输入（内网 NAS 为核心场景），不套衍生 URL 严格规则，
  与 §6.1 分流一致。

### 2.4 凭据红线

- username/password/domain 仅在内存组装 `SmbBuilder`，随 Operator 丢弃；
  不落盘、不进环境变量、不进日志/审计（沿用 §5.4 redact 约定）。
- NTLM 签名/加密由服务端协商决定，插件侧不做降级配置开关（简化初版）。
- Kerberos：crate 已有 `KerberosAuthenticator`，初版不接入，作为后续任务。

## 3. 配置面（manifest + 前端）

### 3.1 manifest.json

- connection-provider `protocol` select 新增 `{"label": "SMB / CIFS", "value": "smb"}`。
- 新字段（`visible_when: protocol ∈ [smb]`，camelCase、binding 沿 §4 规则）：

| key | 类型 | binding | 默认 | 说明 |
|---|---|---|---|---|
| `endpoint` | text | config | 空 | `host[:445]` 或 `smb://host[:445]` |
| `share` | text | config | 空 | 共享名或 `共享名/子路径`（嵌套段转 root 前缀，见 §2.1） |
| `username` | text | config | 空 | NTLM 账号 |
| `password` | password | **secret** | 空 | secret binding |
| `domain` | text | config | 空 | NTLM 域/工作组，可空 |
| `root` / `lock_to_root` / `read_only` / `allow_delete` | 通用 | config | 同现况 | 沿用 §4 通用字段 |

### 3.2 前端

- `opendalServices.ts`：新增 `smb` quick 模板（字段同 §3.1）；同步
  `opendalServices.spec.ts`。
- `model.rs` `StoredConnection` 增加 `share`/`domain` 字段（lifecycle 解析
  单测同步）。
- i18n 七语：SMB 协议名、字段 label/placeholder、错误文案（连接失败/
  不支持 publicLink 等）。
- FileToolbar/QuickPaths：`smb` 不透出 quickPaths（仅 fs 规则不变）。

## 4. 依赖与体积

| 项 | 内容 |
|---|---|
| 新增 crate | `smb2 = "=0.20.1"`（精确 pin；升级走对标清单回归） |
| 间接依赖 | RustCrypto 新线（sha2/aes/aes-gcm/ccm/cmac/md4/pbkdf2 0.11–0.13、`ccm =0.6.0-rc.3`）、lz4_flex、tokio 子集——与 opendal 现有依赖树并行共存。**实施实测**：`aes` 0.9.3 MSRV 1.89 与工具链 1.88.0 冲突，已 `cargo update --precise` pin **0.9.2** |
| 工具链 | rust-version 1.85 ≤ 宿主 1.88.0；edition 2021 一致 |
| opendal | **不加新 feature**——SMB 不是 opendal service，自定义 Access 在本 crate 内实现 |
| 体积 | **实测（2026-08-29）：23.47 MiB → 24.51 MiB（+1.04 MiB / +4.4%）**，落在预估区间内 |

## 5. 测试计划

**单测**（cargo test，无容器）：
- `SmbBuilder` kv 解析（endpoint 形态/share/domain/root 缺省与非法值）；
- 出网校验 smb 分支（裸 host:port ✓、`smb://` ✓、`file://` ✗、`http://` ✗）；
- lifecycle 解析含 secret 合并（password 只在内存）；
- 路径规范化（`\\`/反斜杠拒绝、`..` 拒绝，与 dangerousPaths 语义对齐）；
- capability 透出（copy/presign=false）。

**smoke**（`smoke_test.py` 新增 smb 段，env 门控 SKIP，沿 DBX_FILES_S3_*
/ DBX_FILES_SFTP_* 先例）：

| env | 后端 | 场景 |
|---|---|---|
| `DBX_FILES_SMB_HOST/PORT/SHARE/USER/PASSWORD(/DOMAIN)` | Samba 容器（`ghcr.io/servercontainers/samba`，arm64 可用） | connect/test、list/stat、mkdir/rmdir、write/read、rename、delete、purge(拒根)、上传/下载二进制通道往返、read_only 门禁、capabilities 断言、publicLink 明确报不支持 |

容器编排入 `container_smoke.sh`；未设置 env 时整段 SKIP（并行开发不阻塞）。

**性能**：`perf_files_test.py` 补 smb 传输基线（pipelined 读写 vs 串行预期差
异显著，数据回写本文档 §7）。

## 6. 风险与缓解

| 风险 | 缓解 |
|---|---|
| `smb2` crate 早期项目（2026 首发、低 star） | 适配层把 crate 隔离在 `engine/smb/` 内，协议面（Access trait）稳定，必要时可换实现重评选型；精确 pin + 升级走回归 |
| `ccm =0.6.0-rc.3` RC 依赖 | 记入对标清单依赖归档；上游 stable 0.6 发布后随升级收敛 |
| 服务端差异（Windows / Samba / NAS 内嵌 SMB1-only 设备） | 仅支持 SMB2/3（SMB1 明确不支持，属安全红利）；smoke 用 Samba 对齐；真机差异记 PROGRESS |
| 并发模型初版串行化目录操作 | 传输走 pipelined 流，不受目录锁影响；perf 基准后再决定客户端池 |
| Kerberos / 域环境认证 | 初版 NTLM 密码认证；Kerberos 列后续任务 |
| 宿主 1.1 optional 降级 | 无新宿主能力依赖（沿用 host.binary + JSON 降级路径，零改动） |

## 7. 任务分解（F5-SMB）

> 状态（2026-08-29）：**S1–S6 全部完成**，交付细节、真机问题修复记录
> （目录尾 `/` 删除、`FileReader` 句柄 EOF 主动 close）与遗留项见
> `docs/PROGRESS-F5-SMB.zh-CN.md`。

| # | 任务 | DoD | 状态 |
|---|---|---|---|
| F5-S1 | 依赖 spike：`smb2 =0.20.1` 接入，1.88.0 构建通过，体积/lock 基线回写 §4 | cargo build/test 过；基线数据入对标清单 | ✅ |
| F5-S2 | `engine/smb/` 适配层：Builder + Access（stat/list/read/write/delete）+ ReconnectPolicy | memory 不可用，单测 + 本地 Samba 手验；check() 连通 | ✅ |
| F5-S3 | 结构操作补齐：create_dir/rename/递归 delete/purge 拒根 + capability 声明 | 单测绿；smoke smb 段（无传输）绿 | ✅ |
| F5-S4 | 传输槽联通：Reader/Writer 对接 job + 二进制通道（OpenDAL 抽象下应零改动，验证即可） | smoke 上传/下载往返 + 取消用例 | ✅ |
| F5-S5 | 配置面：manifest smb 字段 + opendalServices 模板/spec + model.rs 字段 + i18n 七语 + 出网校验 | 宿主表单联动正确；三件套绿 | ✅ |
| F5-S6 | smoke_smb 段 + container_smoke.sh 编排 + 对标清单/PROGRESS 更新 | `scripts/test.sh` 全套绿（容器段按 env SKIP/运行）；四件套齐 | ✅ |

完成定义四件套（单测 + smoke + 对标/任务清单更新 + 七语文案）适用于
F5 全部任务；协议方法面零新增（无 `smb/*` 方法族，仅新连接协议），故无
协议文档同步项。
