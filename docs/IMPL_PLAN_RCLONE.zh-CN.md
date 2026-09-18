# F-RCLONE：OpenDAL 协议层替换为 rclone rc 实施方案

状态：执行中（Phase A 已开工） · 分支：`codex/files/rclone-engine` · 决策日期：2026-09-18

## 0. 背景与决策记录

- **决策**：保留 Rust sidecar 与现有前端，将 `engine/` 的 OpenDAL 协议层替换为 **rclone rcd 子进程 + rc HTTP API**（对应用户 2026-09-18 指令「将 opendal 实现的协议层，更改为 rclone rpc」）。
- **已否决**：Go 全量重写 sidecar（原型分支已清理）。理由：改造本质是整体后端移植（约 2 万行），风险收益比不佳；引擎替换可保留 2 万行已验证的方法面/传输状态机/MCP 层。
- **三个参考实现**：
  - 现状（Rust/OpenDAL）：OpenDAL 接触面约 6-7k 行，其中 `engine/smb/`、`engine/sftp_native/`、`engine/bucket_ns/` 共约 2,900 行是为补 OpenDAL 缺口自研的——恰是 rclone 原生能力。
  - McpCTL（用户 Go 版）：librclone 库内嵌模式，`buildRemoteConfig` 的参数组装可参考，但无实时进度/取消。
  - YARD（`/Users/Jinpy/btroot/yet-another-rclone-dashboard`）：纯前端 SPA，证明 rc API 端点组合足够（`operations/*`、`sync/*`、`_async`+`job/status`+`job/stop`、`core/stats`、`config/create`、`--rc-serve`）。任务模型与 UI 优点照此借鉴（§9）。

## 1. 目标与非目标

**目标**
1. 协议面从 OpenDAL 22 个入口升级为 rclone 全部后端（SMB、SFTP 密码认证、桶枚举原生覆盖，删除 3 个自研适配器）。
2. 依赖瘦身：移除 `opendal`（18 service features）、`smb2`、`russh`/`russh-sftp`、`hmac`/`sha1`/`sha2`/`quick-xml` 及其传递依赖；消除 Cargo.toml 记录的 rustc 1.91 / smb2-russh 版本冲突面。
3. 解锁 rclone 原生能力：运行时限速（`core/bwlimit`）、全局统计（`core/stats`）、异步任务（`_async` job）——为 §9 的 UI 借鉴提供数据面。
4. 方法面/事件面/帧协议零变更：前端、宿主、MCP 工具不感知引擎替换。

**非目标**
- 不改 `manifest.json` 方法契约与贡献点（字段文案的 OpenDAL 措辞在 Phase D 由 integrator 统一更新）。
- `aliyun-drive` 暂不支持（rclone 上游无此后端）：连接测试返回明确 unsupported 提示；后续以 rclone `RegisterBackend` 自定义后端补齐（独立任务）。
- 不做 rclone mount（与 YARD 同样的非目标判断：进程/权限复杂度不适合插件场景）。

## 2. 总体架构

```
DBX 宿主 ⇄ stdio 帧协议（不变） ⇄ Rust sidecar
                                    ├─ main.rs 方法分发（不变）
                                    ├─ [新] engine 选择器：DBX_FILES_ENGINE=rclone|opendal（缺省 opendal）
                                    ├─ [新] rclone 模块（backend/src/rclone/）
                                    │    ├─ proc.rs      rcd 进程管理（已落地 ✅）
                                    │    ├─ rc.rs        rc API 客户端（已落地 ✅）
                                    │    ├─ registry.rs  连接 → remote 注册表（Agent A）
                                    │    └─ ops.rs       方法 → rc 操作实现（Agent B）
                                    ├─ engine/（OpenDAL，过渡期完整保留，逐阶段退役）
                                    └─ transfers.rs 状态机保留，执行端换 rc job（Phase C）
```

**双引擎过渡**：`DBX_FILES_ENGINE` 环境变量选择引擎，缺省 `opendal`。`main.rs::handle_request` 顶部路由：rclone 引擎已实现的方法走 rc，未实现的方法回落 OpenDAL——过渡期全功能可用、可 A/B 对拍。Phase D 移除回落。

## 3. rcd 生命周期（已落地，验收基线）

`backend/src/rclone/proc.rs`（测试 5/5 通过，含 2 个真实 rclone v1.75.1 活进程测试）：

- **二进制解析**：`DBX_FILES_RCLONE_BIN` → 插件自带（exe 同目录 `rclone[.exe]`）→ PATH；`rclone version` 解析版本，`< 1.68` 拒绝（`operations/uploadfile` 门槛）。
- **启动参数**：`rcd --rc-addr=127.0.0.1:<随机端口> --rc-user=<uuid> --rc-pass=<uuid> --rc-serve --config=<临时目录>/rclone.conf --log-level=INFO`。
- **安全**：凭据走 Basic Auth（uuid 随机，非固定值）；config 文件 `0600`，remote 凭据经 `config/create` 的 `opt.obscure` 混淆落盘；临时目录随 Drop 删除；凭据不进 argv 以外的进程可见面（rcd 参数含 user/pass，属 loopback 单机面，可接受；rclone 无免参数的等价机制）。
- **健康与重启**：spawn 后轮询 `rc/noopauth`（10s 上限）；`RcdSupervisor::client()` 惰性拉起/崩溃重启；`Drop` 杀进程 + 清临时目录。
- **packaging**：Phase D 在 build.sh 按 target 放置 pinned 版本 rclone 二进制（内置优先、系统回退）。

## 4. 协议映射表（manifest protocol → rclone remote）

通用规则：连接 id → remote 名 `dbx<f(prefix)><id 前 8 位>`（ registry.rs 定义，保证 config 文件安全字符集）；敏感参数经 `config/create` 的 `opt.obscure=true`；路径统一 `remote:` + `remote:path` 形式传给 rc（`fs` 参数必须带冒号——活进程测试已确认无冒号会被当本地相对路径）。

| manifest protocol | rclone type | 参数映射要点 | 备注 |
|---|---|---|---|
| `fs` | （免注册） | rc 调用 `fs` 参数直接用本地路径；`root` 为空时用 `/` | 与 OpenDAL root 语义对齐 |
| `s3` | `s3` | `provider`（自定义 endpoint→`Minio`/`Other`，否则 AWS）、`access_key_id`、`secret_access_key`(obscure)、`region`(缺省 us-east-1)、`endpoint`、`force_path_style`（virtual-host 关闭时 true） | 桶留空 → 根目录 `operations/list` 原生列桶（退役 bucket_ns） |
| `oss` | `s3` | `provider=Alibaba` + endpoint/access_key_id/secret_access_key | |
| `cos` | `s3` | `provider=TencentCOS`；`secret_id`/`secret_key` → `access_key_id`/`secret_access_key`；`security_token` → s3 STS 参数（键名以 `rclone config providers s3` 实测为准） | |
| `obs` | `s3` | `provider=HuaweiOBS` | |
| `gcs` | `gcs` | `service_account_credentials` = base64 解码后的凭据 JSON（secret）；`bucket` 留空列桶待实测验证 | |
| `azblob` | `azureblob` | `account`、`key`(obscure)、`endpoint` 可选、`container` 留空列容器 | |
| `webdav` | `webdav` | `url=endpoint`、`vendor=other`、`user`、`pass`(obscure) | |
| `ftp` | `ftp` | endpoint 剥 scheme 取 host(:port)、`user`（空=anonymous）、`pass`(obscure)、ftps→`tls=true` | |
| `sftp` | `sftp` | endpoint `ssh://user@host:port` 拆解、`pass`(obscure) **或** `key_file`/`key_pem`；known_hosts 策略映射键名实测确认 | 密码认证原生支持——`sftp` 与 `sftp-native` 两个 manifest 值映射同一后端，`sftp-native` 保留为别名（表单兼容），退役 russh 适配器 |
| `smb` | `smb` | endpoint `host:port`、`user`、`pass`(obscure)、`domain`；`share` 进路径（`remote:share/sub`），留空 → 根目录原生列共享（退役 share 发现适配器） | |
| `gdrive` | `drive` | `client_id`/`client_secret`；token 由 `access_token`/`refresh_token` 组装 JSON 传入（obscure 行为实测确认） | Phase C |
| `onedrive` | `onedrive` | token 同上；`drive_id`/`drive_type` 可能需补 API 调用 | Phase C，实测确认 |
| `dropbox` | `dropbox` | token 同上 | Phase C |
| `yandex-disk` | `yandex` | token 同上 | Phase C |
| `seafile` | `seafile` | `url`、`user`、`pass`(obscure)、`repo_name` 进路径 | Phase C |
| `koofr` | `koofr` | `endpoint`、`user`、`pass`(obscure)、`email` | Phase C |
| `pcloud` | `pcloud` | rclone 标准为 OAuth token；表单的 user/pass 是否可用实测确认，不可用则在表单提示 | Phase C |
| `opendal-custom` | 透传 | `service` → rclone type；`config` JSON → parameters | Phase C；Phase D manifest 值更名 `rclone-custom`（integrator） |
| `aliyun-drive` | — | 返回明确 unsupported（方法级错误信息含迁移说明） | 后续自定义后端独立任务 |

实现期硬约束：**每个 type 的参数键名一律以 `rclone config providers <type>` 的实测输出为准**，不得凭记忆写键名；registry 单测对「键名存在性」用 provider 清单做离线断言（有 rclone 时）。

## 5. 方法面映射表（分阶段）

生命周期参数形状不变（`connection.external_config` / `connection_secrets`，`model.rs::from_lifecycle_params` 复用）。

| 方法 | rc 端点/机制 | 阶段 | 要点 |
|---|---|---|---|
| `connection/test` | `config/create` → `operations/stat` root → `config/delete` | A | 不入注册表；错误原文透传 |
| `connection/connect` | `config/create` + 注册表登记 | A | 幂等：重复 connect 先 `config/delete` 再建 |
| `connection/disconnect` | 取消连接任务（状态机已有）→ `config/delete` → 注册表移除 | A | |
| `files/list` | `operations/list`（`opt.recurse`） | A | 排序 by path（对齐 ops.rs 语义）；目录标记项过滤；返回 `{entries:[FileEntry]}` |
| `files/listPaged` | `operations/list` 全量 + 切片 | A | 上限 100,000 条超限报错（Phase C 若需要再演进游标） |
| `files/stat` | `operations/stat` | A | **`item:null` = 不存在**（活进程测试确认），映射为现有 NotFound 错误语义 |
| `files/capabilities` | `backend/features` + 每协议静态矩阵 | A | 输出 `Capabilities{scheme,list,write,read,stat,delete,createDir,copy,rename,presign}` + `readOnly`；scheme 填 rclone type；静态矩阵保守取交集，`backend/features` 可用时覆盖 |
| `files/size` | `operations/size` | A | `{count, bytes}` |
| `files/quickPaths` | registry 协议信息本地推导 | A | 形状不变 |
| `files/read` | `--rc-serve` GET（带 Range，按 `maxBytes` 截断） | B | 返回 base64 + truncated，2MiB 上限不变 |
| `files/write` | `operations/uploadfile`（multipart） | B | 4MiB 内联上限不变 |
| `files/mkdir` / `files/rmdir` | `operations/mkdir` / `operations/rmdir` | B | |
| `files/delete` / `files/purge` | `operations/deletefile` / `operations/purge` | B | 门禁（read_only/allow_delete/lock_to_root）沿用 policy.rs，先门禁后 rc |
| `files/copy` / `files/move` | `operations/copyfile` / `movefile`（跨连接 = 两个 fs 字符串） | B | 同实例降级语义改由 rclone `--server-side-across-configs` 等价机制承担 |
| `files/rename` | `operations/movefile`（同 fs） | B | |
| `files/publicLink` | `operations/publiclink` | B | 不支持的后端返回原文错误 |
| `files/upload/start|finish` + 二进制通道 `files/upload/{taskId}` | 帧写入本地 staging 文件，finish 时 `uploadfile`（≥1.68）；进度由字节泵计数 | B | 256KiB 帧、8B BE offset 协议不变 |
| `files/download/start|finish` + `files/download/{taskId}` | rc-serve GET 流式泵 | B | save_to_local 直落本地（跳过二进制帧）保持现有行为 |
| `files/copyDir` / `files/syncDir` | `sync/copy` / `sync/sync`（`_async:true`） | C | `dryRun`/`maxDelete`/`maxDepth` 直接映射 rc 参数；门禁矩阵用例从 mcp/tests.rs 平移 |
| `transfer/status` / `transfer/cancel` | `job/status` + `core/stats`（group） / `job/stop` | C | 轮询 500ms，节流沿用 200ms/1%；`transfers.rs` 状态机、并发 3 + 按连接 FIFO、停滞判定、transfers.json 全保留 |
| `files/archiveList` / `extract` / `compress` | rc-serve GET 取流 / `uploadfile` 回写 | D | archive.rs 的 zip 逻辑保留，仅替换 IO 端 |
| `local/*`、`audit/*`、`mcp/*`、`files/ui/state/report` | 不涉及引擎 | — | 零改动 |

## 6. 传输与进度设计（Phase C）

- 作业发起：rc 调用带 `_async:true` 立即返回 `{jobid}`；`JobTable` 记录 `jobid ↔ taskId`。
- 进度：每个作业 500ms 轮询 `core/stats`（`group` 参数隔离作业；rclone 1.72+ 支持 `jobid` 过滤时优先），换算 bytes/transferred/rate，沿用 `PROGRESS_INTERVAL_MS=200` / `PROGRESS_MIN_DELTA=0.01` 节流发 `files/transfer/progress`。
- 取消：`job/stop`；完成后 `core/stats-reset`（group）防泄漏。
- 语义对齐：mirror 删除 = `sync/sync` 的 `deleteMode`（默认 trackRenames 语义与现有 mtime ±2s 容差的差异在用例中固化：rclone 用 size+modtime 精确比对，不再手工容差）。

## 7. 字节通道与流式

- 下载：`GET {base}/{remote:}/{path}`（`--rc-serve` 已开启），Basic Auth 头由 sidecar 注入（无 YARD 浏览器 no-auth 的限制）；Range 支持用于 `files/read` 的 `maxBytes` 截断与媒体预览拖动（Phase E）。
- 上传：`POST {base}/operations/uploadfile` multipart（1.68+，内置 pinned 版本保证）；`< 1.68` 的系统回退路径 = staging 临时文件 + `operations/copyfile`。
- 帧协议（8 字节 BE offset + ≤256KiB）与 SDK 不动。

## 8. 移除清单（Phase D，一次 PR 完成）

- 模块：`engine/smb/`、`engine/sftp_native/`、`engine/bucket_ns/`、`engine/ops.rs` 的 OpenDAL 实现体、`engine/mod.rs` 的 Operator 注册表与 `protocol_kv`/`from_iter` 分支、`bench.rs` 的 memory/fs 基准。
- 依赖：`opendal`、`smb2`、`russh`、`russh-sftp`、`hmac`、`sha1`、`sha2`、`quick-xml`（reqwest/chrono 视残留使用决定）。
- 同步更新：`dbx-plugin.toml` 注释、`docs/PROTOCOL.zh-CN.md`（contract 所有，走 integrator）、manifest 描述文案与 `opendal-custom` 更名、README。
- 保留：`policy.rs`、`store.rs`、`archive.rs`（zip 编解码）、`mcp/`、`local_downloads.rs`、`fingerprint.rs`、transfers 状态机。

## 9. YARD UI 借鉴清单（Phase E，与 A-D 并行可做）

数据面均来自 rc，引擎替换后即可实现：

| # | 功能 | YARD 参照 | 我方落点 |
|---|---|---|---|
| 1 | 运行时调优：bwlimit、transfers/checkers/retries/timeout、log 级别 | `settings-page.tsx` | SettingsPanel 增设「传输引擎」区块；`options/set` + `core/bwlimit`，按连接作用域写入文档化默认值 |
| 2 | 全局概览仪表盘：累计传输、实时吞吐曲线、最近错误 | `overview-page.tsx` + `shared-stats-runtime.tsx` | TransferPanel 顶部概览条或独立抽屉；`core/stats(group=global_stats)` + `core/transferred`，1s 轮询上限 |
| 3 | 任务分组与历史徽章：按目标分组、整组停止、成功/失败/混合过滤 | `jobs-page.tsx` | TransferPanel 列表改造；数据来自 JobTable（已有）+ core/stats 补充速率 |
| 4 | 视频/音频流式预览（横竖屏自适应） | `media-preview-overlay.tsx` | PreviewPane 扩展 mode：rc-serve Range 流 → 沙箱内 data URL/分段缓冲；受 CSP 与帧协议约束需与宿主确认大文件策略 |
| 5 | 多标签会话浏览 | `explorer-toolbar.tsx` sessions/tabs | 工作台多 tab（低优先级，双栏布局已覆盖核心场景） |

## 10. 风险与缓解

| 风险 | 缓解 |
|---|---|
| rclone 参数键名凭记忆写错 | 硬约束：以 `rclone config providers <type>` 实测为准；registry 离线单测断言键名 |
| 包体积 +50MB/平台（内置 rclone） | 解析顺序内置优先；已装 rclone 的用户无感；Phase D 实测 .dbxp 体积后再决定是否默认系统优先 |
| rc 每调用 30s 超时 vs 长传输 | 传输全部 `_async`，同步调用只做短操作；`RcClient` 超时参数化 |
| capabilities 矩阵失真致前端按钮误启停 | 静态保守矩阵 + `backend/features` 覆盖；对拍 OpenDAL 版行为（过渡期双引擎可 diff） |
| Windows 进程树清理 | Phase B 实测 `kill_on_drop` 行为，必要时 job object（后续任务） |
| 双引擎并存期代码混淆 | 文件所有权隔离（§12）；Phase D 一次性删除 |
| **过渡期限制（实测确认）**：两引擎连接表独立，`DBX_FILES_ENGINE=rclone` 模式下未移植方法对 rclone 连接不可用（`Unknown connectionId`） | 过渡期 rclone 模式仅用于验收测试（smoke 脚本守护该行为），日常使用缺省 OpenDAL；Phase B 移植文件面后此面收窄；smoke 已断言为回归守护 |

## 11. 里程碑与验收

| 阶段 | 内容 | 验收 |
|---|---|---|
| A（进行中） | registry.rs + ops.rs + 双引擎路由：test/connect/disconnect、list/listPaged/stat/capabilities/size/quickPaths | `cargo test`（含活进程：tempdir 本地远端全链路）；`DBX_FILES_ENGINE=rclone` 下前端浏览本地目录可用；`DBX_FILES_ENGINE` 缺省行为与主干一致 |
| B | 文件面 + 二进制通道 | 上传/下载/读写/删除/复制/移动全链路活进程测试；宿主实测编辑器与拖拽 |
| C | sync/copyDir + 传输进度/取消 | 门禁矩阵用例平移全绿；进度条/取消在宿主可见可点 |
| D | 移除 OpenDAL 依赖与自研适配器；manifest/docs 更新；build.sh 内置 rclone | `cargo test` 全绿、validate_repo.py、build.sh 出包、依赖树 diff 报告 |
| E | §9 五项 UI 借鉴 | 浏览器截图验证（全局规范要求） |

每阶段 PR 遵守 `.github/agent-flow.yml`：分支 `codex/files/rclone-engine*`、integrator 合并、`validation.local` 命令全绿。

## 12. 并行分工与文件所有权

| 所有者 | 文件 | 约束 |
|---|---|---|
| Agent A（registry） | `backend/src/rclone/registry.rs` | 不碰 mod.rs/main.rs/engine/Cargo.toml/ops.rs；公开 API 以报告为准 |
| Agent B（ops） | `backend/src/rclone/ops.rs` | 同上；可复用 `crate::engine::policy`（pub）与 model 形状 |
| integrator（主代理） | `rclone/mod.rs`、`main.rs` 路由、engine 选择器、Cargo.toml | 两个 agent 交付后接线 |
| 共同 | 新增模块的 inline tests | 活进程测试一律「无 rclone 二进制则跳过」，保证 CI 无 rclone 也绿 |

验证命令（worktree `.worktrees/rclone-engine`）：

```bash
export PATH="$HOME/.cargo/bin:$PATH" CARGO_TARGET_DIR="$PWD/backend/target"
cargo test --manifest-path .worktrees/rclone-engine/backend/Cargo.toml rclone::
cargo check --manifest-path .worktrees/rclone-engine/backend/Cargo.toml
```

已确认语义（活进程实测，实现勿再假设）：
1. `operations/stat` 对合法 remote 上不存在的对象返回 `Ok({"item":null})`，不报错。
2. rc `fs` 参数必须带冒号（`remote:` / `remote:path`），无冒号会被当本地相对路径静默成功。
3. ModTime 为 RFC3339 字符串（含纳秒与时区）；`operations/list` 项键名首字母大写（`Path`/`Name`/`Size`/`IsDir`）。
4. `operations/size` 只读 `fs` 参数，`remote` 被忽略——子路径 sizing 用 `fs+path` 合并。
5. stock rcd 无 `backend/features` 端点（404），特性查询用 `operations/fsinfo`。
6. `config/create` 的 `opt.obscure` 只混淆 IsPassword 键；`secret_access_key`/`key`/`session_token` 等明文落 0600 临时配置（随进程删除）。
7. **Phase B 待验证**：命名 local remote + 绝对路径的组合（`dbxName:/abs`）实测列到进程 CWD 而非组合根——fs 协议因此一律走绝对路径直连（`call_fs` 已如此），Phase B 为 fs 连接注册 rc-serve 用 remote 时需改用 local 连接串语法（`local,root=/abs`）或实测确认语义。
8. **rc-serve GET 路由必须 `[fs]` 括号形式**（`{base}/[{fs}]/{remote}`，正则 `^\[(.*?)\](.*)$`）：裸形式对一切路径恒 404。补偿已内置在 `ops.rs::read_prefix` 与 `bytes_channel.rs` 各自单点；`rc.rs::serve_url` 仍为裸形式（当前无人调用）——接线步统一为括号形式并去重补偿（双包裹会 404）。
9. `operations/uploadfile` 落点 = `path.Join(remote, multipart part 文件名)`，rc.rs part 名固定 `payload`：精确落点需瞬态目录 + `movefile` 归位（bytes_channel::finish 与 ops::write_bytes 均如此）；后续 rc.rs 参数化 part 文件名可简化为单步。
10. `operations/mkdir` 是 mkdir -p 且幂等；`deletefile`/`purge` 对缺失路径报 404/500，而 OpenDAL 语义为静默 no-op——ops.rs 已 stat-first 容错对齐。
11. `RcClient` 的 30s 超时覆盖整个 body 读取/上传——大文件传输会超时，Phase C 需传输路径专用长超时（或无超时）client。
12. uploadfile 响应恒 `{}`（无 item 可校验）；上传尺寸校验靠 staging 收到的字节数。
