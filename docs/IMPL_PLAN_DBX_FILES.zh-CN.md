# dbx-files-plugin 可实施文档

> 公共基线：`IMPL_PLAN_M0_COMMON.zh-CN.md`（M0 文档）。
> 插件：`io.dbx.files`，仓库 `~/btroot/dbx-plugins/files`，sidecar
> `dbx-plugin-files`。
>
> **v3 修订（弃 rclone+Go，改 Apache OpenDAL + Rust）**：
> - 引擎 `rclone v1.73.5（Go，librclone）` → **`opendal` Rust crate（v0.58.x）**；
> - sidecar 语言 Go → **Rust**（dbx-plugin-sdk Rust crate，与 ssh-sftp 插件同栈）；
> - 传输 stdio-jsonl（JSON+base64 分块）→ **stdio-framed + 二进制通道**
>   （复用 ssh-sftp 的 `sftp/upload|download/*` 帧语义，256KiB 块 + 8 字节
>   BE offset 前缀，无需 base64）；
> - 凭据 obscure/config-create 红线整体消失——OpenDAL Operator 全内存构建，
>   凭据不落盘、不进环境变量、无 rclone.conf；
> - 能力面对齐目标重定义：早期为 rclone 全后端 → 现为 OpenDAL 服务矩阵
>   （63 个 services feature，编译期白名单启用），能力缺口显式列表见 §5.5。

## 0. 为什么换 OpenDAL + Rust（调研依据）

| 维度 | rclone + Go（v2 方案） | OpenDAL + Rust（v3 方案） |
|---|---|---|
| 引擎形态 | 完整工具内嵌为库（rc API），携带 CLI/配置体系 | 纯数据访问层库，无 CLI/无配置文件 |
| 二进制体积 | backend/all ~40-80MB | feature 白名单（fs/s3/webdav/ftp/sftp/gcs/azblob/oss/memory）预估 **~10-20MB** |
| 凭据 | 需 obscure/config-create 分流红线 | Builder 全内存，凭据只在进程内 |
| Go 侧可用性 | librclone 原生 | Go 绑定经 opendal-c + purego/**libffi**，运行时依赖系统 libffi，成熟度 "Released and usable"（非 production-ready）——弃用 Go 路线的主因 |
| DBX 插件栈 | stdio-jsonl，无二进制帧 | **stdio-framed + 二进制通道**（ssh-sftp 同款，已验证） |
| 服务覆盖 | ~70 后端 | 63 个 services feature + memory；主流全覆盖（fs/s3(MinIO)/webdav/ftp/sftp/gcs/azblob/oss/cos/swift/hdfs/网盘） |
| 许可 | MIT + 间接依赖面宽（Proton 等） | Apache-2.0，依赖面窄 |

调研来源：[docs.rs/opendal services](https://docs.rs/opendal/latest/opendal/services/index.html)、
[Operator API](https://docs.rs/opendal/latest/opendal/struct.Operator.html)、
[Go binding 文档](https://opendal.apache.org/docs/bindings/go/)（libffi/purego、状态标注）。

**决策**：Files 插件 = Rust sidecar（dbx-plugin-sdk + stdio-framed）+ opendal
（feature 白名单）。LDAP 插件不受影响，维持 Go。

## 1. 仓库结构

```
dbx-files-plugin/
├── manifest.json / dbx-plugin.toml / assets/plugin.svg
├── backend/
│   ├── Cargo.toml                     # dbx-plugin-sdk + opendal(features)
│   └── src/
│       ├── main.rs                    # PluginServer(Framed) 装配 + 方法分发 + handle_binary
│       ├── model.rs                   # 请求/响应/条目类型 + lifecycle 解析（参考 ssh-sftp model.rs）
│       ├── engine/                    # OpenDAL 引擎层
│       │   ├── mod.rs                 # Operator 构建（含 from_iter 通用透传）
│       │   ├── ops.rs                 # list/stat/read/write/delete/copy/rename 封装
│       │   └── transfer.rs            # 流式读写对接传输槽（Reader/Writer chunk/concurrent）
│       ├── transfers.rs               # 异步 job 表（§7）
│       ├── policy.rs                  # path 白名单 / read_only / allow_delete 门禁 + 出网校验（§9）
│       └── store.rs                   # prefs/transfers.json/audit.jsonl（M0 公共语义）
├── frontend/                          # Vue3 沙箱
│   └── src/{App.vue, components/, lib/}
├── scripts/{build,test}.sh, smoke_test.py, smoke_s3_test.py, smoke_sftp_test.py,
│           sidecar_client.py          # 直接复用 ssh-sftp 的 framed 客户端！
└── docs/{PROTOCOL.zh-CN.md, FEATURE_PARITY.zh-CN.md}
```

> Rust 侧无「M0 公共 Go 包」：lifecycle 解析/store 参照 ssh-sftp 的
> `model.rs` / `main.rs` 同构实现（两 Rust 插件模式一致，M0 文档 §附录
> 的 framed 协议即其契约）。smoke 复用现有 `sidecar_client.py`（framed），
> 不需要 jsonl 客户端。

## 2. 依赖（Cargo.toml）

| crate | 版本 | 用途 |
|---|---|---|
| `dbx-plugin-sdk` | CLI sdk-root 同源（ssh-sftp 同款） | stdio-framed 服务、事件、二进制通道 |
| `opendal` | 0.58.x | 引擎；features：`services-fs, services-s3, services-webdav, services-ftp, services-sftp, services-gcs, services-azblob, services-oss, services-memory` |
| `tokio` | 与 SDK 对齐 | 异步运行时 + job 任务 |
| `serde` / `serde_json` | — | 协议 |
| `uuid` | — | taskId/jobId/operationId 兜底 |

- 服务白名单为**编译期 feature**：新增服务 = 加 feature 重编（不裁
  `opendal-custom` 透传——透传只对已编译服务生效，UI 列表与白名单同步生成）。
- Rust 固定 1.88.0（宿主 rust-version 要求，同 ssh-sftp）。
- **dbx-plugin-sdk 引用方式（照 ssh-sftp `backend/Cargo.toml` 同款）**：
  依赖声明 `dbx-plugin-sdk = "0.1.0"` + `[patch.crates-io]` 段指到本地
  worktree `path = "../../dbx-plugin-host-worktree/plugins/sdk/rust/dbx-plugin-sdk"`
  （或走 CLI sdk-root）；CLI 打包时自动携带 SDK。
- 法务：Apache-2.0 + 依赖清单随对标清单归档（M2-T0）。

## 3. 初版代码迁移映射（外部参照 → 本插件，历史）

外部参照实现的 `fs/manager.go`（rclone rc 封装）、`fs/local.go`、`fs/transfer.go`、
`buildRemoteConfig` **全部不移植**——它们是 rclone 集成件，被 OpenDAL 替代。
仍具迁移价值的：

| 参照基线（已退役） | 去处 | 改造 |
|---|---|---|
| `fs_service.go`（会话/门禁/审计/传输记录/方法语义） | `main.rs` + `policy.rs` + `transfers.rs` | 方法语义（22 个 Fs 方法的参数/行为）逐一对齐；门禁/审计语义照 M0 基线 |
| `types/fs_types.go`（条目结构） | `model.rs` | `{name,path,kind,size,modifiedAt}` camelCase 对齐 |
| 上传/下载槽语义 | `engine/transfer.rs` | 对齐 ssh-sftp `sftp/upload|download/*` 协议（start/chunk/finish/cancel + 256KiB 块 + 8 字节 BE offset） |
| `rcloneConfig.js` 模板 | `frontend/src/lib/opendalServices.ts` | 改写为 OpenDAL 服务的字段模板（纯 UI 糖） |
| `transferRuntime.js` | `frontend/src/lib/transfers.ts` | job 事件驱动 + 轮询兜底 |
| `FileBrowserPane/FileConnectionDialog/FileTransfersPanel/FileAuditPanel` | `frontend/src/components/` 拆分 | §8.5 |

## 4. manifest 贡献点

connection-provider `io.dbx.files.connection`，`database_type: "storage"`，
`capabilities: ["test","connect","disconnect"]`；workbench
`io.dbx.files.workbench`；permissions：`host.workbench` + `host.events` +
`host.binary`（传输二进制通道）+ `host.filesystem`（本地下载落盘）。

**字段命名规则**：快捷协议字段 `key` = OpenDAL 配置键
（存 `external_config`），label 友好文案 + 七语；后端零翻译直入 Builder。

| key | 类型 | binding | 默认 | visible_when (protocol ∈) | OpenDAL 键 |
|---|---|---|---|---|---|
| `display_name` | text | name | `Storage` | — | — |
| `protocol` | select | config | `fs` | —（fs/s3/oss/webdav/ftp/sftp/smb/sftp-native/opendal-custom） |
| `root` / `lock_to_root` | text/boolean | config | 空/false | — | Operator root |
| `service` | text | config | 空 | opendal-custom | 任意已编译服务名（gcs/azblob/oss/…） |
| `config` | textarea(JSON) | config | `{}` | opendal-custom | 整体作为 Builder 配置 kv |
| — fs — | | | | | |
| `root`（同上通用字段） | | | | | fs 的根路径 |
| — s3 — | | | | | |
| `bucket` | text | config | 空 | s3 | 同名 |
| `endpoint` / `region` | text | config | 空 | s3 | 同名（MinIO 填 endpoint + `enable_virtual_host_style=false` 类似项照 OpenDAL 键名） |
| `access_key_id` | text | config | 空 | s3 | 同名 |
| `secret_access_key` | password | secret | 空 | s3 | 同名 |
| — oss —（v0.1.20，F3-3 云服务快捷模板首个落地） | | | | | |
| `bucket` | text | config | 空 | s3/oss | 同名 |
| `endpoint` | text | config | 空 | s3/oss | oss 键 `endpoint`（如 `https://oss-cn-hangzhou.aliyuncs.com`） |
| `access_key_id` | text | config | 空 | s3/oss | 同名 |
| `secret_access_key` | password | secret | 空 | s3/oss | 映射 OpenDAL oss 键 `access_key_secret` |
| — webdav — | | | | | |
| `endpoint` / `username` / `password` | text/password | config/secret | 空 | webdav | OpenDAL 键 `endpoint`/`username`/`password` |
| — ftp — | | | | | |
| `endpoint` / `user` / `password` | text/password | config/secret | 空/anonymous | ftp | 同名 |
| — sftp — | | | | | |
| `endpoint` | text | config | 空 | sftp | OpenSSH 格式 `user@host` / `ssh://user@host:port` |
| `key` / `known_hosts_strategy` | text | config | 空/Tolerate | sftp | 私钥内容或路径、主机密钥策略 |
| `password` | password | secret | 空 | sftp | 同名 |
| — 通用门禁/网络 — | | | | | |
| `read_only` / `allow_delete` | boolean | config | false/true | — | 插件门禁 |
| `connection_mode` | select | config | `direct` | sftp/ftp | `direct` / `via-dbx-ssh`（§6.1） |
| `dbx_ssh_connection` | text | config | 空 | connection_mode=via-dbx-ssh | 宿主 SSH 连接 id |
| `timeout_secs` | number | config | 30 | — | 插件超时 |

**必填规则（v0.1.4 起）**：静态 `required: true` 仅限 `display_name` 与
`protocol` 两个无条件字段。协议特定必填项（s3/oss 的
bucket/access_key_id/secret_access_key、opendal-custom 的
service、via-dbx-ssh 的 dbx_ssh_connection）一律 `required_when` 与
`visible_when` 成对声明——宿主校验静态 `required` 时不评估 `visible_when`，
静态必填会把其它协议全部拦死（"Plugin connection field 'Bucket' is
required"）。`required_when` 由连接表单按条件拦截；运行时兜底强制在引擎
构建层（OpenDAL builder / smb adapter 自带清晰报错）。

> manifest 快捷协议覆盖 fs/s3/oss/webdav/ftp/sftp 六类（v0.1.20 起 oss
> 从 custom 透传升级为快捷协议）；其余全部走
> `opendal-custom`（service + config JSON 透传，能力面 = 编译白名单内
> 全部服务）。

## 5. OpenDAL 引擎（engine/）

### 5.1 Operator 构建与连接表

- `connection/connect`：由 lifecycle 参数组装
  `opendal::Operator`（Builder kv 来自 external_config + secrets 合并），
  存入连接表 `Arc<Mutex<HashMap<connectionId, Operator>>>`；
  Operator 为 `Clone + Send + Sync`，天然适合共享。
- 快捷协议 = 固定 Builder（S3::from_map 等）；`opendal-custom` 走
  `Operator::via_iter(service, config_map)`（官方支持的通用构造，
  等价 rclone 的参数透传）。
- `connection/test`：临时 Operator 上执行 `check()`（OpenDAL 内建可用性
  探测，docs.rs Operator::check）。

### 5.2 操作映射

| 领域操作 | OpenDAL | 备注 |
|---|---|---|
| list / listPaged | `list` / `lister`（流式）+ `ListOptions{recursive, limit, start_after}` | 大目录用 lister 流式聚合；start_after 后端相关，通用分页为内存切片 |
| stat / exists | `stat` / `exists` | Metadata：content_length / last_modified / mode |
| read（预览） | `read` / `reader.read(range)` | ≤2MiB base64 返回 |
| write（小文件） | `write` | 解码 ≤4MiB |
| mkdir | `create_dir`（mkdir -p 语义） | 路径需 `/` 结尾（NotADirectory 陷阱写进实现） |
| delete | `delete`（幂等）；递归 = `delete_with(recursive(true))` | remove_all 已废弃（0.55+），用 delete_with |
| copy / move | `copy` / `rename`（同服务、能力相关）；跨服务 = 流式 read→write job | fs/s3 等支持情况按 `info().capability()` 运行时探测，不支持自动走 job 降级 |
| 公开链接 | `presign_read`（S3 SigV4 类签名 URL） | 仅签名型后端；WebDAV/FTP 无 → 返回明确「不支持」错误（§5.5） |

### 5.3 传输槽（对接二进制通道）

- 上传：`files/upload/start {remotePath,size}` → job + `Writer`
  （`chunk(4MiB).concurrent(4)`）→ 二进制通道 `files/upload/{taskId}`
  收块（8 字节 BE offset + ≤256KiB，逐块确认，对齐 ssh-sftp `append_upload`
  语义）→ `files/upload/finish` 关闭 Writer 校验 size。
- 下载：`files/download/start` → `Reader` 预取 → 通道
  `files/download/{taskId}` 按 offset 出块 → finish 释放。
- 进度事件 `files/transfer/progress`（§7.3）。

### 5.4 凭据与安全红利

- **无 obscure/config-create 层**：凭据经 Builder 内存注入 Operator，
  不落 rclone.conf 类物、不进环境变量；连接表只存 Operator，不存明文配置。
- 日志/审计/事件一律不含凭据字段（redact 宏统一处理）。

### 5.5 能力缺口表（对照外部基线，已收口）

| 参照基线（已退役） | v3 状态 | 说明 |
|---|---|---|
| `About`（容量查询） | **不适用** | OpenDAL 无容量 API；返回字段缺省（前端隐藏面板） |
| `PublicLink` | 部分对齐 | `presign_read` 覆盖签名型后端（s3/gcs/azblob/oss）；其余报「后端不支持」 |
| `Chmod`（local） | **不适用** | OpenDAL 无 chmod；如需可后续对 fs 后端加 `std::fs` 特例（暂不做） |
| `SyncDir`（rclone sync 语义） | 自研 job | 遍历 + 差异 + copy/rename 循环（§7）；无服务端原生 sync |
| rclone 特有后端（crypt/union/compress/cache） | 不对齐 | OpenDAL 无对应层；用户侧影响极小 |
| 跨服务 copy | 降级路径 | 同服务走原生 copy；跨服务自动 read→write job |

## 6. 连接与会话设计

### 6.1 拨号模式与出网校验（Mimosa 约束）

- `direct`：sidecar 直连 `runtime.host:runtime.port`（DBX 传输层出口）。
  fs 后端无网络。
- `via-dbx-ssh`（sftp/ftp）：本地临时端口 + 宿主 SSH 连接正向隧道，
  OpenDAL endpoint 指向本地端口；依赖宿主隧道 API（M0-T6 确认形态，
  不可用则降级 direct 并文档化）。
- **出网 URL 校验（硬性验收条件）**：
  1. 所有 HTTP 类后端（s3/webdav/http/gcs…）仅允许 `http`/`https` scheme，
     配置层显式校验拒绝其他 scheme；
  2. host 校验（域名解析合法、格式合法）应用于全部出站 endpoint；
  3. **localhost/环回/私有/保留地址拒绝**应用于一切**非用户显式配置来源**
     的 URL（如 presign 返回、重定向跟随、目录内容衍生的 URL）——
     用户在连接表单显式配置的存储 endpoint 属于可信输入（插件的
     核心场景即连接内网 MinIO/NAS），不在拒绝范围，但走独立审计标记；
  4. 重定向跟随默认关闭，需要时逐后端开启并套用上述校验。

### 6.2 生命周期

`connection/test`（临时 Operator 上 `check()`，不留状态）→
`connection/connect`（组装 Builder kv 建 Operator 入连接表，幂等：重连先
丢弃旧 Operator）→ `connection/disconnect`（取消该连接全部 job + 丢弃
Operator，幂等）。

## 7. 异步传输 job（transfers.rs）

状态机 `queued → running → completed | failed | canceled`；全局并发信号量 3、
同 `connectionId` 串行（per-connection FIFO）；进度经 `files/transfer/progress`
事件（≥200ms 或 ≥1% 变化才发）+ `files/transfers/list|status|cancel` 轮询
兜底；历史清理 `files/transfers/clear`（P-FILES ⑥，§8.4）；完成态历史持久化 `store/transfers.json`（上限 200 条环形覆盖）。

差异点：
- **目录 sync/copyDir 为自研遍历 job**：lister 递归枚举源 → 逐文件
  copy（同服务）或 read→write（跨服务/降级）→ 汇总进度
  `{files_done, files_total, bytes_done, bytes_total}`；支持取消
  （tokio ctx）。
- 无 rclone `_async`/`job/status` 可依赖 → F2-5 spike 改为
  **lister 大目录（10k 条目）流式性能基准**。

## 8. Sidecar 方法契约

公共约定：参数/返回 camelCase；除生命周期方法外必填 `connectionId`；
业务错误 -32000、方法未注册 -32601；路径均在 `root`（若配置）之内，
越界拒绝；写操作过 `read_only` / `allow_delete` 门禁。

**保留连接 `__local__`**（双栏本地面，2026-09）：engine 内置保留
connectionId `__local__`——按需合成的 root=`/` fs 连接（可写、可删、
不锁 root，与用户自建「本地文件系统」连接同策略），不占连接表、
`connection/connect` 拒绝该 id 防遮蔽。所有 `files/*` 方法对该 id
直接可用（浏览/读写/传输/quickPaths/capabilities），工作台双栏左栏
默认指向它（左=本地、右=远端，对齐主流双栏文件管理器心智）；无新增协议方法。

### 8.1 浏览与元数据

| 方法 | 请求 | 返回 |
|---|---|---|
| `files/list` | `path`、`recurse?` | `{entries:[{name,path,kind,size?,modifiedAt?}]}` |
| `files/listPaged` | `path`、`page`、`pageSize` | `{entries,total}` |
| `files/stat` | `path` | `{entry}` |
| `files/capabilities` | — | `{copy:bool,rename:bool,presign:bool,write:bool,…}`（`info().capability()` 透出，前端按能力显隐） |
| `files/size` | `path` | `{count,bytes}`（lister 聚合） |
| `files/publicLink` | `path`、`expireSecs?` | `{url}`；后端不支持时 -32000 |
| `files/quickPaths` | — | `{paths:[{key,path}]}`；工作台路径栏快速目录下拉（原 chips 行已并入下拉）。仅 fs 协议（root 为 `/` 即整盘、且未锁 root；OpenDAL fs 的 root 必填，无法「未配置」）透出 `home/desktop/downloads/documents/pictures`（逐个 stat 校验，缺失目录不出现）；其余协议与受限连接只返回 `{key:"root",path:"/"}` |

（`About` 删除——OpenDAL 无容量 API，前端隐藏入口。）

### 8.2 读写与结构操作

| 方法 | 请求 | 约束/说明 |
|---|---|---|
| `files/read` | `path`、`maxBytes?` | 预览用途，≤2MiB；返回 `{dataBase64, truncated}` |
| `files/write` | `path`、`dataBase64` | 覆写小文件，解码 ≤4MiB；超限报错引导走上传槽 |
| `files/mkdir` | `path` | `create_dir`（mkdir -p 语义；路径补 `/` 结尾） |
| `files/rmdir` | `path` | 仅空目录 |
| `files/delete` | `path` | `allow_delete=false` 拒绝；幂等（OpenDAL 语义） |
| `files/purge` | `path` | `delete_with(recursive(true))`；**拒绝等于 root 或 `/`**；`allow_delete` 门禁 |
| `files/copy` / `files/move` | `sourceConnectionId?`（缺省同连接）、`sourcePath`、`targetConnectionId?`、`targetPath` | 同服务走原生 copy/rename（capability 探测）；跨连接或不支持自动降级 read→write job |
| `files/rename` | `path`、`newPath` | 同连接；capability 不支持时降级 copy+delete job |

（`chmod` 与 `About` 不提供——OpenDAL 无对应能力，见 §5.5。）

### 8.3 大文件传输（二进制通道，对齐 ssh-sftp 协议）

| 方法 | 请求 | 返回/说明 |
|---|---|---|
| `files/upload/start` | `remotePath`、`size` | `{taskId}`；建 job + Writer |
| （二进制）通道 `files/upload/{taskId}` | 8 字节 BE offset + ≤256KiB | **顺序推送、帧内 offset 校验**（错位即错误，无逐块 RPC 确认）；完成与 size 校验由 finish 承担——对齐 ssh-sftp `sftp/upload` 协议（ssh-sftp 仓 `docs/PROTOCOL.zh-CN.md` §二进制通道、`backend/src/main.rs:666-671` append_upload 模式） |
| `files/upload/finish` | `taskId` | close Writer，校验 size |
| `files/download/start` | `remotePath` | `{taskId,size}` |
| （二进制）通道 `files/download/{taskId}` | 按 offset 出块 | 8 字节 BE offset + ≤256KiB（sidecar → 宿主方向推送） |
| `files/download/finish` | `taskId` | 释放 |
| `files/transfer/cancel` | `taskId` | canceled |

web 模式兜底：宿主无 `host.binary` 能力时，降级提供 JSON+base64 分块方法
（`files/upload/chunkJson|download/chunkJson`，1MiB/块）——能力探测，
两种路径共用 job。

### 8.4 目录同步与 job 查询

| 方法 | 请求 | 返回 |
|---|---|---|
| `files/syncDir` | `sourceConnectionId/path`、`targetConnectionId/path` | `{jobId}`（异步遍历 job，§7） |
| `files/copyDir` | 同上 | `{jobId}` |
| `files/transfers/list` | `connectionId?` | `{jobs:[…]}`（进行中 + 历史） |
| `files/transfers/clear` | `connectionId?`（缺省全量） | `{cleared:n}`（清理完成态历史：单文件 job 表 + dir job 表 + transfers.json 持久化记录；queued/running 不受影响。P-FILES ⑥） |
| `files/transfer/status` | `jobId` | `{job}` |
| `files/transfer/cancel` | `jobId` | `{success:true}` |
| `files/audit/list` | `limit?`（缺省 100，clamp 1..=1000）、`connectionId?`（缺省全量） | `{entries:[{at,action,connectionId,path,result}]}`（audit.jsonl 只读透出，最新在前；shape 以 AuditPanel.vue 解析为准。凭据红线：AuditRecord 仅含路径，无密文字段） |

### 8.5 压缩包（B-ARCHIVE 路，tar / tar.gz / tgz；zip Phase 2）

| 方法 | 请求 | 返回 |
|---|---|---|
| `files/archiveList` | `path`、`page?`、`pageSize?`（缺省 1/200，clamp ≤1000） | `{entries:[{name,path(条目内路径，无前导/),kind:"file"\|"directory",size,modifiedAt?}],total}` |
| `files/extract` | `path`、`targetPath`（目录，不存在则创建） | 小包（≤10 文件且 ≤8MiB）同步 `{success,transport:"native",jobId:null}`；超限降级 `{success,transport:"job",jobId}`（§7 dir job，`kind:"extract"`，进度/取消/列表全复用） |
| `files/compress`（P-FILES 13 轮） | `paths`（≥1，同连接文件/目录混选）、`targetPath`（`.tar`/`.tar.gz`/`.tgz` 后缀定格式；已存在则拒绝） | 预算同解压（条目 ≤50k、载荷 ≤1GiB）；小包（≤10 文件且 ≤8MiB）同步 `{success,transport:"native",jobId:null}`；超限降级 `{success,transport:"job",jobId}`（`kind:"compress"`）。归档内路径以各源 basename 为根；目录条目不落盘（解压按父路径隐式建目录，空目录丢弃）；gzip 为 stored 档位（无压缩率、零新依赖，真 deflate 与 zip 同为 Phase 2） |

约束：`read_only` 拒绝 extract（`allow_delete` 不适用——不删源归档）；归档路径与
targetPath 均过 policy 白名单；条目路径拒绝 `..`/绝对/盘符/反斜杠与链接条目
（zip-slip）；防呆上限——条目 50k、gzip 解压/载荷 1 GiB、源文件 1 GiB；zip 输入
返回明确 -32000（Phase 2）。实现见 `backend/src/archive.rs`（tar header 与
inflate 均为手写，无新依赖），交付细节见 git 历史。

### 8.6 前端实施要点

**工作台与宿主桥（先读再写代码）**：连接生命周期由宿主驱动（表单保存/
连接时宿主调 `connection/test|connect`）；工作台经 `window.dbxPlugin`
取 connection 上下文——照 ssh-sftp `App.vue` 既有模式，不发明新机制。
Files **无 session 概念**：所有 `files/*` 方法只带 `connectionId`；
跨连接 copy/move 的目标连接由 UI 选择后传 `targetConnectionId`。

```
src/
├── App.vue                      # 外壳 + 连接上下文（照 ssh-sftp 模式）
├── components/
│   ├── FileTable.vue            # 列表/多选/右键菜单（自 FileBrowserPane 拆分）
│   ├── FileToolbar.vue          # 导航/刷新/新建/上传/下载/视图切换
│   ├── TransferPanel.vue        # 进行中 job + 历史（进度条/取消）
│   ├── ConfirmDialog.vue        # 危险操作确认（dangerousPaths 规则）
│   ├── AuditPanel.vue           # audit.jsonl 只读视图
│   ├── PreviewPane.vue          # files/read 预览（复用 ssh-sftp TextPreview 模式）
│   └── CustomConfigEditor.vue   # opendal-custom 的 service+config JSON 编辑
└── lib/
    ├── api.ts                   # sidecar 调用封装（connectionId 注入；错误 showError(cause,"files")）
    ├── transfers.ts             # job 事件驱动（files/transfer/progress）+ 轮询兜底（重写 transferRuntime.js 语义）
    ├── dangerousPaths.ts        # purge/recursive delete/syncDir 覆盖目标的确认规则
    ├── opendalServices.ts       # 服务字段模板（纯 UI 糖，自 rcloneConfig.js 改写）
    └── i18n.ts                  # 七语
```

- 上传/下载默认走宿主 `fileTransfer` 能力（若 1.1 可用）直连二进制通道；
  web/docker 无该能力时走 §8.3 的 JSON 降级方法（File API 切片 / Blob 组装）。
- 大目录列表虚拟滚动；进度事件节流由 sidecar 负责，前端只渲染。

## 9. 安全

1. 凭据：§5.4（内存 Builder、无 obscure 层、redact）。
2. 门禁：read_only / allow_delete / lock_to_root+root 前缀白名单、
   purge 拒根；危险操作前端确认（`dangerousPaths.ts`，参考 ssh-sftp
   `dangerousCommands.ts` 模式）+ `audit.jsonl`。
3. 出网校验：§6.1 四条（Mimosa 约束落地，属实现验收条件）。

## 10. 测试计划

**单测**：lifecycle 解析（含 secret 合并）、path 白名单边界、分页切片、
job 状态机、redact、出网校验（scheme/host/私有地址分流用例）、
`memory://` 后端全操作（OpenDAL 内建，无需容器）。

**smoke（framed 客户端复用 `sidecar_client.py`）**：

| 脚本 | 后端 | 场景 |
|---|---|---|
| `smoke_test.py` | fs（临时目录） | list/mkdir/write/read/copy/move/rename/delete/purge(拒根)/上传下载二进制通道往返/取消/read_only 门禁/capabilities |
| `smoke_test.py`（memory 段） | `memory://` | 同上子集，零依赖 |
| `smoke_s3_test.py` | MinIO 容器 | connect/check/list/传输槽/publicLink(presign)/自定义分页 |
| `smoke_custom_test.py` | `opendal-custom`（service=memory） | 透传构造路径 |
| `smoke_sftp_test.py`（M3） | openssh 容器 | sftp 全量 + via-dbx-ssh（视宿主隧道 API） |

**能力清单**：Fs 22 方法 × 状态（对齐/降级/不适用+原因，含 §5.5
缺口表）+ OpenDAL 服务覆盖矩阵（快捷协议 vs custom 透传 vs 编译白名单）。

## 11. 里程碑任务分解

### M2（核心，依赖 M0 验收；M0 的 Go 部分仅 LDAP 需要，Files 走 Rust 路线）

| # | 任务 | DoD |
|---|---|---|
| F2-0 | Cargo 骨架 + opendal feature 白名单 + 体积基线 + 许可清单 | cargo build 过；体积/启动基线入对标清单 |
| F2-1 | manifest §4 + Rust sidecar 装配（framed + binary，参照 ssh-sftp main.rs） | 宿主表单联动正确；echo/test 联通 |
| F2-2 | lifecycle → Operator 构建器（快捷五协议 + custom 透传）+ 连接表 + check() | memory:// 单测 + smoke connect 段 |
| F2-3 | 浏览与结构操作（list/listPaged/stat/capabilities/size/mkdir/rmdir/delete/purge/rename/read/write） | `smoke_test.py` fs+memory 段全绿 |
| F2-4 | copy/move（能力探测 + 跨服务降级 job） | fs↔memory 跨服务用例 |
| F2-5 | lister 大目录（10k 条目）流式基准 | 基准数据写回本文档 §7 |
| F2-6 | 传输槽：上传/下载二进制通道 + JSON 降级路径 + job 表 + 进度事件 + 取消 | fs 往返/断点取消/publicLink(presign) 用例 |
| F2-7 | 目录 sync/copyDir 自研 job（遍历+进度+取消） | fs→memory 目录同步用例 |
| F2-8 | 前端：FileBrowserPane 拆分 + 连接面板 + 传输面板 + 确认/审计面板 + `opendalServices.ts` 模板 | MinIO + fs 浏览器全流程；七语齐 |

### M3（sftp 与云服务模板）

| # | 任务 | DoD |
|---|---|---|
| F3-1 | sftp 后端（endpoint/key/password/known_hosts 策略） | `smoke_sftp_test.py` 全绿 |
| F3-2 | via-dbx-ssh 模式（依赖 M0-T6 宿主隧道结论） | 走宿主 SSH 连接打通；不可用则文档化降级 |
| F3-3 | ftp 后端 + 云服务快捷模板（gcs/azblob/oss——纯前端模板，必要时扩 feature） | 每模板 custom 透传冒烟 |
| F3-4 | 传输历史持久化 + 重启展示 | transfers.json 环形覆盖正确 |

### F5（SMB 后端，Rust 原生 client；方案与任务表已随专项文档退役删除，见 git 历史）——**已落地（2026-08-29）**

`smb2` crate + OpenDAL 自定义 Access 适配层，新增第 6 个快捷协议 `smb`；
方法面零新增，配置面/前端模板/七语/Samba 容器 smoke 为主要增量。
交付报告与真机修复记录随批次文档退役删除（见 git 历史）。SMB 的 `share`
现为可选：留空时连接服务器根并枚举可见共享，路径首段选择共享；填写后
保持指定共享直连。服务器级连接不能设置 share-relative 的 `root`。

### M4（MCP 工具）——**sidecar 侧已落地（2026-09-12），前端接线待下一轮**

`backend/src/mcp.rs`：`mcp/tools`（注册 + JSON Schema；12 工具：UI 驱动 4 +
本地读 2 + 元发现 1 + 写 5）、`mcp/call`（lifecycle payload 转发 + 分派 +
16 KiB 响应上限）、`mcp/settings/get|set`（可调参数，`mcp-settings.json`
持久化）、事件 `files/ui/intent` + intent 状态表（TTL 60s/LRU 20）+
`files/ui/state/report`、`files_scan_digest`/`files_cursor_next` 本地读、
写族两阶段确认（`delete`/`purge` 强制 preview→confirmToken；purge 拒根
红线）+ 审计 `source:"mcp"`。形状对齐 ldap Go 版参考实现（同族参数一致），
设计来源 `shared/IMPL_PLAN_PLUGIN_MCP.zh-CN.md`（v2）§2/§3/§4/§6.2；
完整协议章节见 `files/docs/MCP.zh-CN.md`。验收：`cargo test`（mcp 模块
纯逻辑单测：digest 聚合/cursor LRU+TTL/confirmToken hash+过期+一次性/
intent 状态表/16KiB 截断/只读不注册写工具）+ `scripts/smoke_mcp.py`
（M1–M10，与 ldap 同表）。**待下一轮**：前端接线——`App.vue` 挂
`shared/frontend/useUiIntent`（已由 ldap agent 落地）订阅 `files/ui/intent`、
PathField/FileTable handler、ui/state/report 快照上报，以及 mockHost/env.d.ts
镜像同步与真机复验。

## 12. 风险与备注

| 风险 | 缓解 |
|---|---|
| OpenDAL 服务能力差异（rename/copy/presign 按后端） | `files/capabilities` 运行时透出 + 前端按能力显隐 + job 降级路径（§5.2/§5.5） |
| 目录 sync 无服务端原生（自研遍历） | job 进度/取消齐备；F2-5 基准先行 |
| s3 兼容端点行为差异（MinIO/OSS/COS） | smoke 覆盖 MinIO；其余经 custom 透传由用户校验 |
| via-dbx-ssh 依赖宿主隧道 API | sftp 排 M3；M0-T6 前置确认 |
| 出网校验误伤内网 endpoint | 校验规则区分「用户显式配置（可信）」与「衍生 URL（严格）」（§6.1），并审计标记 |
| opendal 版本升级 feature 改名/废弃（如 remove_all 0.55 废弃） | 锁 0.58.x；升级走对标清单回归 |
