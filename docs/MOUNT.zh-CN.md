# 远程存储本地挂载：设计决策

> 日期：2026-09-17。依据：rclone 对齐差距可行性审计（计划工作区
> `_plans/audit-rclone.md` 挂载章节）、`docs/COMPARISON.zh-CN.md` 定位、
> `docs/IMPL_PLAN_DBX_FILES.zh-CN.md` §0 体积与栈选型。
> 结论先行：**近期不做自研 FUSE/WinFsp 挂载**；推荐方案为 sidecar 内嵌
> 本地 WebDAV 服务网关，挂载动作交给操作系统自带的 WebDAV 客户端。

## 1. 背景与决策

用户对"像本地盘一样用远端存储"有真实诉求：把一个 S3/WebDAV/SFTP 连接
挂进 Finder 或资源管理器，直接双击打开文档。审计确认当前插件**零挂载
代码**，因此这是一次从零的方案选择，而不是补齐。

自研文件系统驱动路线（方案先排除项）的成本结构如下：

| 平台 | 挂载栈 | 分发成本 | 签名/公证 | 用户引导 |
| --- | --- | --- | --- | --- |
| macOS | macFUSE（内核扩展，需降低系统安全策略）或 FUSE-T（用户态） | 高/中：macFUSE 需用户手动放行降级，FUSE-T 生态年轻 | kext/system extension 签名与公证流程重 | 需逐步指导安装与放行 |
| Windows | WinFsp（第三方内核驱动） | 高：随插件分发或引导安装驱动 | 驱动签名、UAC 安装提示 | 对非技术用户不友好 |
| Linux | FUSE（内核接口内建）+ fusermount | 低：发行版自带 | 无额外要求 | 安装 fuse3 包即可 |

三平台中只有 Linux 接近零成本，macOS 与 Windows 的驱动分发、签名与
用户引导成本都远超一个文件管理插件的合理投入；再叠加自研 VFS 层的
体量（见 §5，rclone 级），该路线与产品定位（`COMPARISON.zh-CN.md`：
图形化浏览 + 传输任务 + 归档 + 受控策略，不替代 rclone 命令行生态）
不匹配。

**决策：近期不做自研 FUSE/WinFsp 挂载。** 把"像本地盘一样"的需求收敛
为轻量的 WebDAV 网关体验（§2），并在远期保留按需评估的空间。

## 2. 推荐方案：sidecar 内嵌 WebDAV 服务网关

sidecar 进程内起一个仅监听环回的 WebDAV 服务：用 `dav-server` crate
实现 WebDAV 协议层，适配器把 OpenDAL `Operator` 包装成其
`DavFileSystem` trait——远端协议差异全部由既有引擎消化，网关不做任何
协议特判。

### 2.1 架构

```
OS WebDAV 客户端（系统组件，零安装）
   │  http://127.0.0.1:<随机端口>/<token>/<connId>/...
   ▼
sidecar 进程
   ├─ WebDAV 网关（dav-server）：路径 token 校验 → 方法门禁 → 超时
   ├─ DavFileSystem 适配器：WebDAV 语义 ↔ OpenDAL ops
   └─ 既有引擎：Operator 连接表 + policy 门禁 + 凭据（仅内存）
   ▼
远端存储（S3 / WebDAV / SFTP / OSS / ...）
```

- **监听**：`127.0.0.1` + 随机端口，随 sidecar 生命周期启停。
- **路径鉴权**：网关启动时生成随机 token（≥128bit），每个连接一个
  挂载点 `/token/connId/`；无 token 的请求一律 404，不暴露网关存在。
  token 只存在于 sidecar 内存，宿主重启即轮换。
- **门禁在前**：WebDAV 方法先过连接级策略门禁，再落到 DavFileSystem，
  保证挂载面与插件内操作共享同一条授权路径。

### 2.2 各平台挂载方式

系统自带 WebDAV 客户端完成挂载，插件侧零驱动分发：

| 平台 | 挂载方式 | 说明 |
| --- | --- | --- |
| macOS | Finder → 前往 → 连接服务器（Cmd+K）→ 粘贴网关 URL | macOS 内置 WebDAVFS，无需第三方组件 |
| Windows | 资源管理器 → 映射网络驱动器（或 `net use`） | 依赖 WebClient 服务；基础认证/大小上限等客户端怪癖列入 M2 验证清单（§4） |
| Linux | GNOME Files "连接到服务器"（GVfs `dav://`）或 `mount.davfs`（davfs2） | GVfs 走用户会话无需 root；davfs2 需安装 fuse3 包 |

工作台提供一键复制网关 URL 与按平台检测的挂载指引（M2 向导，见 §4）。

### 2.3 复用项与凭据红线

直接复用既有设施，网关不新增授权体系：

- **策略门禁**（`backend/src/policy.rs`）：`root`/`lock_to_root` 路径
  白名单；`read_only` 在网关层拒绝全部写方法（PUT/MKCOL/DELETE/MOVE/
  PROPPATCH）；`allow_delete=false` 额外拒绝 DELETE 与 MOVE；M2 打开
  写支持时仍按同一门禁放行，挂载面与插件内行为一致。
- **连接与超时**：网关直接引用 `Operator` 连接表的既有 Operator，
  `timeout_secs` 逐操作超时照常生效，网关层再加单请求整体上限。
- **审计红线**：沿用 store 约定，审计与日志只记连接 id 与操作类别，
  不记 token、不记完整网关 URL。
- **凭据红线**：凭据仅存在于 sidecar 进程内存（DBX 宿主 secret
  binding → `connection_secrets` → Operator Builder），**WebDAV 层零
  凭据暴露**——客户端匿名访问，鉴权完全靠路径内 token；系统挂载时
  无需把远端凭据写入 OS 钥匙串/凭据管理器，也不落盘、不进环境变量。

### 2.4 已知限制（WebDAV 语义 vs POSIX）

| 语义 | 本地盘/POSIX | WebDAV 挂载（本方案） | 影响 |
| --- | --- | --- | --- |
| 随机写 | 原地随机写 | 不支持：PUT 为整文件上传，保存即整文件重传 | 大文件原地改写低效 |
| 锁 | flock/建议锁 | dav-server Class 2 锁可选，OS 客户端支持度不一 | 并发编辑冲突检测弱 |
| 元数据 | 权限/属主/xattr | 仅 size/mtime（PROPFIND 可表达范围） | 权限语义丢失 |
| 原子替换 | rename 原子 | MOVE 尽力而为，语义随 OpenDAL 后端 | "写临时文件再改名"模式对挂载方可见 |

**适用场景**：浏览目录树、按需取文件、文档/表格等中小文件的常规
编辑。**不适用**：数据库文件、大型工程目录、随机写密集负载——这些
场景应继续使用插件内传输与同步任务。

### 2.5 安全边界

- **默认仅环回**：只绑定 `127.0.0.1`，永不绑定 `0.0.0.0`，不提供
  "局域网共享"开关（与单机插件定位一致）。
- **token 必需**：无 token 请求一律 404；token 仅内存保存，随
  sidecar 生命周期轮换，不落盘、不进日志与审计。
- **连接级超时**：逐操作 `timeout_secs` + 网关单请求上限，防止慢后端
  拖住 OS 客户端。
- **Host 校验**：拒绝非预期 Host 头，阻断 DNS rebinding 把环回网关
  暴露给浏览器页面的路径。
- **关停联动**：连接删除或宿主断开时挂载点立即失效（404/410），提示
  用户卸载对应挂载。

## 3. 备选方案对比

| 方案 | 平台覆盖 | 工作量 | 凭据暴露面 | 主要风险 | 结论 |
| --- | --- | --- | --- | --- | --- |
| **A. WebDAV 网关（推荐）** | 三平台 | M1 起 M | 零（仅内存 + token 路径） | WebDAV 语义弱于 POSIX（§2.4） | 推荐 |
| B. Linux-only fuser 只读挂载 | 仅 Linux | M | 零（Operator 内存构建） | 仍需维护一套 FUSE 实现；fusermount 权限与发行版差异；单平台收益不成比例；只读进一步收窄场景 | 条件推荐：Linux 用户占比显著上升，或作为 M1 只读阶段的替代实现 |
| C. 拉起用户自装 rclone mount | 三平台（取决于用户自装驱动） | M | 经 `RCLONE_CONFIG_*` 环境变量传递，不落盘，但进程环境可被同用户进程读取 | 依赖外部二进制与版本；macOS 仍需用户自装 macFUSE/FUSE-T；CLI 报错直通、体验失控；env 传递与凭据红线有摩擦 | 不推荐 |

方案 C 的唯一吸引力是"白得" rclone 的 VFS 能力，但它把最重的成本
（驱动安装、版本兼容、报错口径）转嫁给用户，且凭据从"全内存"退化为
"进程环境变量"，红线松动不可接受。方案 B 技术上干净，但只服务一个
平台、且与方案 A 的只读阶段工作量相同，性价比不如 A。

## 4. 分期落地建议

| 阶段 | 内容 | 规模 |
| --- | --- | --- |
| **M1：WebDAV 网关（只读）** | dav-server + DavFileSystem 适配；token 鉴权与环回监听；`read_only` 门禁下仅放行 OPTIONS/PROPFIND/HEAD/GET；复用 `timeout_secs` 与审计红线；文档化三平台手工挂载指引 + 工作台一键复制 URL | M |
| **M2：写支持与挂载向导 UI** | 按 `allow_delete`/`read_only` 放行 PUT/MKCOL/DELETE/MOVE；ETag/If-Match 冲突防护；挂载向导（平台检测、指引页、Windows WebClient 前置检查）；连接级挂载开关 | M-L |
| **M3：性能缓存（vfs 层）** | 网关内目录项缓存、读块 LRU、Range 请求与预取、大目录分页列举 | L |

M1 交付后即可验证真实使用率：如果只读挂载已覆盖绝大多数诉求，M2/M3
的优先级可再评估，避免为低频写场景提前投入。

## 5. 与 rclone mount 的对照

- **体积来源**：rclone `mount` 的能力主要来自其 VFS 层（目录缓存、
  `--vfs-cache-mode` 系列写缓存、句柄管理），叠加 CLI/配置体系与全
  后端后二进制达 40-80MB 量级（`IMPL_PLAN_DBX_FILES.zh-CN.md` §0）。
  本插件 feature 白名单预估 ~10-20MB，其中并不包含 VFS。
- **本方案的取舍**：挂载语义交给 OS 自带 WebDAV 客户端（macOS
  WebDAVFS、Windows WebClient、Linux GVfs/davfs2，均为系统组件），
  sidecar 只新增一个 HTTP 服务与 DavFileSystem 适配层（亚 MB 级增量，
  无内核驱动、无外部依赖）。代价是放弃 VFS 的高级语义（随机写、
  落盘缓存模式），换取三平台零驱动分发。
- **定位一致**：与 `COMPARISON.zh-CN.md` 相同——不替代 rclone 的
  命令行与专业挂载生态；"像本地盘一样用"在本插件中定位为轻量的浏览
  与文档编辑体验，重同步、批量迁移仍走插件内传输与同步任务。

## 6. 实现状态（M1，2026-09-20 落地）

**策略与最终取舍**：与 §3 备选对比相比有两次修正——打包的 rclone fork
（`scripts/fetch-rclone.sh`，`v1.75.1-dbx.1`）构建时已保留 mount 能力，
且凭据经常驻 rcd 的 `config/create` 通道传递（不进 argv/env），备选 C 的
"用户自装二进制 + 环境变量凭据"两条否决理由均已消失。**最终采用
"rclone mount 优先、WebDAV 网关兜底"**（用户决策），而非 §2 的纯网关
路线：

| 策略 | 触发条件 | 语义 |
| --- | --- | --- |
| **rclone mount**（优先） | 平台探测有 FUSE 驱动，rcd `mount/mount` 成功 | 内核挂载，`vfsOpt.ReadOnly` 强制只读 |
| **WebDAV 网关**（兜底） | 探测缺驱动（macFUSE/WinFsp/fusermount）或 mount 报错分类为环境不可用 | sidecar 内嵌只读 WebDAV（`/{token}/{connId}/` 路径鉴权，仅环回） |

- **命令面**：`files/mount`（`strategy: auto|rclone|webdav`，`path` 相对
  连接根，`mountPoint` 可指定）、`files/unmount`、`files/mountStatus`；
  `connection/disconnect` 联动卸载。auto 失败回退时响应带
  `fallbackReason`。
- **实现偏差**：§2 的 dav-server 方案改为**零依赖手写最小只读 WebDAV**
  （OPTIONS/PROPFIND/HEAD/GET，其余 405）——Cargo.lock 归 integrator
  所有、不新增 crate；真实客户端暴露协议怪癖时 M2 再评估 dav-server。
  §2 的"OpenDAL Operator 适配层"已随引擎迁移改为 rclone ops/serve_get
  适配（`backend/src/mount/webdav_gateway.rs::EngineSource`）。
- **门禁对齐**：网关数据面走 `ops::stat/list`/`serve_get`（与插件内浏览
  同一条 `PathPolicy` 路径），协议层再封一道方法白名单；挂载期间持有
  `start_work` 计数，keepalive 不会回收 rcd。
- **M1 限制**：全程只读；网关无 Range/随机写/锁，单文件读取上限
  256 MiB；Windows 映射 WebClient 对匿名+路径 token 的兼容性列入 M2
  验证清单。M2（写支持/挂载向导）/M3（缓存）保持 §4 规划不变。
- **入口**：工作台目录右键与侧栏目录行「挂载到本机」；webdav 兜底时
  网关 URL 自动进剪贴板并给出平台指引。
