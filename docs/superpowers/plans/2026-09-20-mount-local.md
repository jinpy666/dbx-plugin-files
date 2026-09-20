# 本地挂载（rclone mount 优先 + WebDAV 网关兜底）实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 把一个连接（或其子目录）挂到本机文件系统：优先用已打包 rclone fork 的 `mount/mount`（经常驻 rcd），探测到 OS 缺 FUSE 驱动时自动退到 sidecar 内嵌只读 WebDAV 网关。

**Architecture:** 新增 `backend/src/mount/` 模块：`mod.rs`（类型 + auto 策略 + MountRecord 表）、`rclone_mount.rs`（rc mount 调用 + 驱动探测 + 错误分类）、`webdav_gateway.rs`（零依赖手写最小只读 WebDAV 服务）。生命周期对齐 `tunnel.rs` 的 supervisor 形态；挂载期间持有 `engine.start_work(group_key)` 防 rcd 被 idle 回收；门禁与 `files/write` 同路（`PathPolicy` + `ensure_binding_writable`）。

**Tech Stack:** tokio（已有，`features=["full"]` 含 net）+ 既有 RcClient（rc.rs）+ reqwest（测试打环回）。**零新增 crate**（Cargo.lock 归 integrator 所有，AGENTS.md 禁改）。

**Spec:** `docs/MOUNT.zh-CN.md`（2026-09-17 设计决策，M1 只读范围）。偏离点：
- 设计文档建议 dav-server crate；本计划改为手写最小只读 WebDAV（OPTIONS/PROPFIND/HEAD/GET），因 M1 只读 + 不动 Cargo.lock。若真实客户端暴露协议怪癖，M2 再引入 dav-server。
- 设计文档写"OpenDAL Operator 适配"，引擎已迁 rclone（backend/Cargo.toml:15），适配层包 ops/rc。
- 设计文档备选 C（rclone mount）原判"不推荐"基于"用户自装 rclone + 凭据走环境变量"；现 fork 已内置 mount 且凭据走 rcd 内存通道（proc.rs:99 注释），用户指令明确改为 mount 优先。

**范围（用户已定）：** M1 只读。rclone mount 用 `vfsOpt.ReadOnly=true`；网关只放行 OPTIONS/PROPFIND/HEAD/GET，其余 405。不做写支持、不做 VFS 缓存调优。

## Global Constraints（来自 AGENTS.md / agent-flow.yml）

- 分支 `codex/files/mount-local`（已建独立 worktree），不共享检出、不动主检出未提交文件。
- 所有权：只改 `backend/`、`frontend/`、`docs/`、`scripts/`；**不改 Cargo.lock、ui/、dist/、manifest.json（无 UI 贡献点新增需要）、版本号**。
- 验证收敛于 `validation.local` 全绿：`python3 scripts/validate_repo.py`、`node scripts/connection-forms/verify.mjs files`、`pnpm --dir frontend typecheck/test/build`、`cargo test --locked --manifest-path backend/Cargo.toml`。
- 不自动 push / 不建 PR / 不合并；完成后汇报 changed files + 测试结果 + 风险。
- 凭据红线：token 只在 sidecar 内存；不落盘、不进日志/审计（沿用 store 审计只记连接 id）。
- 凭据/secret 不进测试；容器测试用一次性凭据。

---

### Task 1: `backend/src/mount/rclone_mount.rs` — rc mount 调用 + 探测 + 错误分类

**Files:**
- Create: `backend/src/rclone/mount.rs`（挂在 rclone 模块内，与 sync.rs 同层）
- Modify: `backend/src/rclone/mod.rs`（`pub mod mount;`）

**Interfaces (Produces):**
```rust
pub struct MountSpec { pub fs: String, pub mount_point: PathBuf, pub read_only: bool }
pub async fn mount(client: &RcClient, spec: &MountSpec) -> Result<(), String>;
pub async fn unmount(client: &RcClient, mount_point: &Path) -> Result<(), String>;
pub async fn list_mount_points(client: &RcClient) -> Result<Vec<String>, String>;
pub enum DriverProbe { Available, Missing(String) }
pub fn probe_driver() -> DriverProbe;                 // 平台探测，仅提示不拦截
pub enum MountError { DriverMissing(String), MountPointBusy(String), Other(String) }
pub fn classify_mount_error(message: &str) -> MountError;  // 纯函数，可测
```
- rc 调用：`client.call("mount/mount", &json!({"fs": spec.fs, "mountPoint": mp, "vfsOpt": {"ReadOnly": true}}))`；`mount/unmount {"mountPoint": ...}`；`mount/listmounts {}`（宽容解析，缺字段按空）。
- `RcError` 归一：`Http{body}` / `Rclone{message}` → 取 message 串进 `classify_mount_error`。
- `classify_mount_error` 关键词：`fusermount`、`OSXFUSE`/`mount_macfuse`/`libfuse`、`WinFsp`、`/dev/fuse` → DriverMissing；`already`、`busy`、`mountpoint`+`not empty` → MountPointBusy；其余 Other。
- `probe_driver()`：Linux 查 `/dev/fuse` 存在 + PATH 有 `fusermount3`/`fusermount`；macOS 查 PATH `mount_macfuse`/`mount_fuse-t`；Windows 查 `C:\Program Files (x86)\WinFsp\bin`。

- [ ] Step1 写 classify/probe 失败测试（字符串用真实 rclone 报错样例）
- [ ] Step2 跑 `cargo test -p dbx-plugin-files mount::` 确认编译失败
- [ ] Step3 实现至测试绿
- [ ] Step4 可选 rcd 集成测试（仿 sync.rs：无 rclone 二进制则 `eprintln!` 跳过；mount 结果为 DriverMissing 分类也跳过）
- [ ] Step5 commit `feat(backend): rclone rc mount/unmount + driver probe/classification`

### Task 2: `backend/src/mount/webdav_gateway.rs` — 只读 WebDAV 网关（零依赖）

**Files:**
- Create: `backend/src/mount/webdav_gateway.rs`
- Create: `backend/src/mount/mod.rs`（本任务先只放 `pub mod webdav_gateway;`）
- Modify: `backend/src/main.rs` 顶部（`mod mount;`）

**Interfaces (Produces):**
```rust
pub trait GatewaySource: Send + Sync + 'static {
    fn stat(&self, rel: &str) -> BoxFuture<'_, Result<StatEntry, String>>;
    fn list_dir(&self, rel: &str) -> BoxFuture<'_, Result<Vec<StatEntry>, String>>;
    fn read(&self, rel: &str) -> BoxFuture<'_, Result<Vec<u8>, String>>; // M1 全文件读，无 Range
}
pub struct StatEntry { pub name: String, pub size: u64, pub is_dir: bool, pub mod_iso8601: String }
pub struct GatewayHandle { pub port: u16, pub token: String, pub shutdown: Arc<Notify> }
pub async fn start(cfg: GatewayConfig) -> Result<GatewayHandle, String>; // 绑 127.0.0.1:0
// URL 形态: http://127.0.0.1:{port}/{token}/{connId}/{rel...}
```
- 方法门禁：`OPTIONS`→200（`DAV: 1`，`Allow: OPTIONS, GET, HEAD, PROPFIND`）；`PROPFIND`→207 multistatus（Depth 0/1，其余按 1）；`GET/HEAD`→文件 200（Content-Length=stat.size，HEAD 只发头）+ 目录 405；其余一律 405；token 不符→404。
- 实现：`tokio::net::TcpListener` 每连接 spawn；手解请求行+头（大小写不敏感）；响应 `Connection: close`；请求体读后即弃（上限 64KB 防 DoS）。
- XML：DAV: 命名空间 multistatus，字段 displayname/getcontentlength/getlastmodified(RFC1123)/creationdate(ISO8601)/resourcetype；href 百分号编码。

- [ ] Step1 写失败测试：内存版 GatewaySource + reqwest 打环回——PROPFIND depth1 含条目、GET 返回字节、PUT 405、错 token 404、HEAD 无 body
- [ ] Step2 跑测试确认失败
- [ ] Step3 实现（手写 HTTP/1.1 + XML 模板）
- [ ] Step4 测试绿（`cargo test mount::webdav`）
- [ ] Step5 commit `feat(backend): minimal read-only WebDAV gateway on loopback`

### Task 3: 策略接线 — `files/mount|unmount|mountStatus` + auto 回退

**Files:**
- Create: `backend/src/mount/mod.rs` 补全（MountRecord/MountOutcome/auto 策略/注册表）
- Modify: `backend/src/model.rs`（`MountRequest`/`MountStatusRequest`/`MountUnRequest` 结构，紧邻 `DirJobRequest`）
- Modify: `backend/src/main.rs`：`Plugin` 增 `mounts: Arc<std::sync::Mutex<HashMap<String, mount::MountRecord>>>`（仿 sync_jobs main.rs:57）；`handle_request_via_rclone` 在 syncDir 臂后新增三臂；新增 `async fn rclone_start_mount(...)`（仿 main.rs:1750 `rclone_start_dir_job`）；`connection/disconnect` 臂联动卸载（main.rs:195 `release_tunnel` 旁）

**Interfaces:**
```rust
// model.rs
pub struct MountRequest { pub strategy: Option<String>, /* auto|rclone|webdav */ pub path: Option<String>, pub mount_point: Option<String> }
pub struct MountUnRequest { pub mount_id: Option<String> }   // 缺省卸本连接全部
pub struct MountStatusRequest { pub mount_id: Option<String> }
// mount::start_mount(engine, mounts, conn_id, req) -> Value
// 响应: {mountId, strategy: "rclone"|"webdav", mountPoint?, gatewayUrl?, fallbackReason?, hint}
```
- auto 策略：probe=Available 且 strategy≠webdav → 试 rclone mount；`MountError::DriverMissing|Other` 且 strategy=auto → 起网关并在 `fallbackReason` 带上原始报错；strategy=rclone 显式指定时不回退直接报错。strategy=webdav 直接网关。
- 门禁：`engine.binding()` 后按 `files/write` 臂同款检查；子路径经 `PathPolicy::check_read`（policy.rs:181）。M1 全程只读，`read_only` 连接天然兼容，`allow_delete` 不放行任何写。
- 挂载点默认：macOS/Linux `~/dbx-files-mounts/<remote_name>`（不存在则 `create_dir_all`，已存在且非空报 MountPointBusy）；Windows 扫 `Z:`→`D:` 取首个空闲盘符。
- 生命周期：MountRecord 持有 `WorkGuard`（mod.rs:403 `start_work(group_key)`）+ GatewayHandle/挂载点；`connection/disconnect`、`files/unmount`、进程退出（Plugin Drop 不保证 → 依赖 sidecar 生命周期文档说明）时卸载/关停。

- [ ] Step1 model.rs 结构 + 序列化测试（serde round-trip）
- [ ] Step2 mount::start_mount 纯策略单测（注入 probe/mount 假实现：trait 注入或 cfg(test) 假 client）——rclone 成功、DriverMissing→webdav 回退、strategy=rclone 不回退
- [ ] Step3 main.rs 三臂 + disconnect 联动
- [ ] Step4 `cargo test` 全绿 + `cargo build` 无 warning 新增
- [ ] Step5 commit `feat(backend): files/mount|unmount|mountStatus with rclone-first webdav fallback`

### Task 4: UI 挂载入口（frontend/）

**Files:**
- Modify: `frontend/src/App.vue`：side-nav 菜单（~2799）+ `menuAction` 分发（~2112）增 `mountLocal` / `unmountLocal`；结果用现有 toast/对话框形态展示 mountPoint 或 gatewayUrl + 复制按钮 + 平台提示
- Create: `frontend/src/App.mountLocal.spec.ts`（仿 App.freshReview.spec.ts：`vi.spyOn(window.dbxPlugin,'invoke')` 断言 `files/mount` 调用与结果渲染）

- [ ] Step1 失败 spec（invoke 调用参数 + 策略文案渲染）
- [ ] Step2 App.vue 实现
- [ ] Step3 `pnpm --dir frontend test` / `typecheck` 绿
- [ ] Step4 commit `feat(frontend): mount-to-local action with strategy result dialog`

### Task 5: 文档 + 全量验证

**Files:**
- Modify: `docs/MOUNT.zh-CN.md`：追加「实现状态（M1）」——两策略、回退条件、限制（无 Range/写/锁）、平台指引
- Modify: `scripts/smoke_test.py`：新增 `files/mount`→`files/mountStatus`→`files/unmount` 冒烟段（本机连接，webdav 兜底必然可用；SKIP 容错保持既有风格）

- [ ] Step1 文档 + smoke 段
- [ ] Step2 跑 validation.local 全套（worktree 内先 `pnpm --dir frontend install --frozen-lockfile`）
- [ ] Step3 汇报：changed files / 测试输出 / 风险（手写 WebDAV 客户端兼容性、Cargo.lock 未动、MCP 工具未加为后续项）

## Self-Review 结论

- Spec 覆盖：M1 只读网关 = Task2/3/5；回退策略 = Task3；平台指引 = Task5；门禁/凭据红线 = Task2/3 约束。M2/M3（写支持、向导 UI、缓存）不在本计划。
- 类型一致性：`MountSpec/MountError/GatewaySource/StatEntry` 在 Task1/2 定义、Task3 消费；响应字段在 Task3 定、Task4 断言。
- 无占位符：所有代码块给出可编译级细节，接口签名精确到文件。
