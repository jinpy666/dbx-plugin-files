# UI 完善波次进度报告（io.dbx.files — 自定义服务动态表单 + 双栏对称 + manifest 契约）

> 记录日期：2026-08-29。仓库 `/Users/Jinpy/btroot/dbx-plugins/files`。
> 本轮四件事：① opendal-custom 编辑器 Form/JSON 双模 + 参数 schema + 安全校验；
> ② 双栏对称性修复（右栏搜索/路径输入）+ 快速目录 chips（新协议方法
> `files/quickPaths`）；③ manifest connection-provider 字段显隐/必填搭配核查
> + 契约单测；④ 全量验证（cargo / smoke / 前端三件套 / scripts/test.sh）。
> 浏览器/宿主端到端截图验证按约定另行安排（见 §4 遗留项）。

## 0. 改动清单（2026-08-29 小节）

### ① OpenDAL 配置化 UI：动态表单 + JSON 双模

- `frontend/src/lib/opendalServices.ts`：
  - 新增 `CUSTOM_SERVICE_SCHEMAS`（fs/s3/webdav/ftp/sftp/gcs/azblob/oss/memory
    九个服务的参数元数据：key/type/required/placeholder/options/secret/security），
    key 即 OpenDAL Builder 配置键，后端零翻译；
  - 新增校验纯函数：`validateHostField`（拒绝 localhost/环回/私有/链路本地/
    CGNAT/保留地址，容忍 `scheme://user@host:port/path` 与 IPv6 括号写法）、
    `validateUrlField`（仅 http/https 且主机名过 host 除非名单）、
    `validateFieldInput` / `validateConfigAgainstSchema`；
  - 新增表单⇄JSON 同步：`formValuesFromConfig` / `configFromFormValues`
    （schema 外键进 extras 原样保留，来回切换不丢自定义键）。
- `frontend/src/components/CustomConfigEditor.vue`：重构为 Form/JSON 双 Tab。
  - 已知服务按 schema 渲染动态控件（文本/密码/数字/布尔/下拉、必填 `*` 标记、
    placeholder、逐字段错误行）；表单改动实时序列化进 JSON，合法 JSON 切回
    表单时反填；**JSON 非法时禁止切到表单并提示**（`customJsonInvalidSwitch`）；
  - 未知服务（无 schema）隐藏 Form Tab，仅保留纯 JSON；
  - 测试按钮在表单模式先过 schema 校验（含安全护栏）再走 `connection/test`；
    JSON 模式保持原有逐字透传语义（power user 出口）；
  - 移除独立的 root 输入框：root 统一为普通 Builder 键（fs 表单必填字段；
    其余服务经 JSON `{"root": ...}` 传入并被 extras 保留）。i18n 键
    `rootLabel` 保留未删（宿主/后续 UI 可复用）。
- 单测：`frontend/src/lib/opendalServices.spec.ts` 7 → 24 个用例（schema 覆盖
  白名单、必填/安全标注一致性、host/url 拒绝与放行表、表单⇄JSON 双向同步
  （含 extras 保真）、整份配置校验）。

### ② 双栏对称性修复 + 快速目录

- 右栏（wb-pane-target）补齐与左栏对等的面包屑 + 路径输入框（enter 跳转 /
  esc 还原）+ 文件名过滤框（复用 `lib/searchFilter.ts` 的 `filterEntries`，
  新增 `rightSearchQuery` / `filteredRightEntries`）。
- `wb-pane-header` 与左栏 `wb-toolbar` 对齐：min-height 36px、同 border-bottom、
  同 padding/背景（style.css）。
- 快速目录 chips（tiny-rdm `ToolsFilePage.vue` quick paths 对标）：
  - 新协议方法 `files/quickPaths`（§8.1，文档已同步）：无参数 →
    `{paths:[{key,path}]}`。仅 fs 协议（root=`/` 整盘、未锁 root）透出
    `home/desktop/downloads/documents/pictures`，逐个目录 stat 校验（缺失
    目录不出 chip）；其余协议与受限连接只返回 `{key:"root",path:"/"}`。
    实测发现 OpenDAL fs 的 root 为必填项（root 缺省即 ConfigInvalid），
    「未受限」语义按 root=`/` 落地（连接探针 smoke 因此改传 root=/）。
  - `backend/src/engine/ops.rs`：`quick_paths`（+ 可注入 home 的可测核心
    `quick_paths_with_home`、资格判定 `quick_paths_eligible`、候选表
    `fs_quick_path_candidates`、`fs_home_dir`=HOME→USERPROFILE 回退）；
  - `backend/src/main.rs` 注册分支（connectionId 必填，走既有 -32601/-32000
    语义）；`frontend/src/lib/mockHost.ts` 补 mock 分支；
  - 前端 `App.vue`：两栏 chips 行（lucide 图标 + wb-* 令牌样式，`wb-quick-chips`
    /`wb-quick-chip`），初始化/开双栏/右栏切连接时刷新；方法缺失时整行隐藏
    （旧 sidecar 降级）；
  - smoke 新增 `quick-paths` 场景 ×3（fs 受限 → 仅 root；memory → 仅 root；
    fs-quickpaths 探针连接 → home chip 必现 + 键集合法）。

### ③ manifest 显隐/必填搭配核查

- 核查结论（已修正项）：
  - `bucket` / `access_key_id` / `secret_access_key` 补 `required: true`
    （s3 无 bucket/密钥必然连不上，宿主表单应前置拦截）；
  - `known_hosts_strategy` 由 text 改 select（Tolerate/Strict/Trust，
    与前端 schema 的 options 对齐，杜绝任意值进 russh）。
- 核查结论（自洽、未改动）：`service`/`config` 仅 opendal-custom 可见；
  `username` 仅 webdav、`user` 仅 ftp、`password` 绑定 secret 且
  webdav/ftp/sftp 可见、`key` 仅 sftp；`read_only`/`allow_delete`/
  `lock_to_root`/`root`/`timeout_secs` 全局可见；`connection_mode` 仅
  ftp/sftp、`dbx_ssh_connection` 随 `connection_mode=via-dbx-ssh`（后端
  model.rs 已强校验二者搭配）。webdav/ftp 的 endpoint 必填无法用现有
  visible_when schema 按协议表达（字段被 4 协议共享），见 §4 遗留项。
- 契约单测（`backend/src/model.rs`，风格对齐既有 tests）：3 个新测试 ——
  字段 key 唯一、binding 合法且 secret⇒password、protocol options ≡
  PROTOCOLS、visible_when 引用存在且 one_of ⊆ 目标字段 options、协议门控
  矩阵逐字段断言、required 字段形状断言、七语 localizations 字段键集与 en
  完全一致（多键/缺键都算失败）。

## 1. 验证实测值（2026-08-29 实跑）

| 验证项 | 命令 | 结果 |
|---|---|---|
| 后端单测 | `cargo test --manifest-path backend/Cargo.toml` | **103 passed, 0 failed**（基线 94 + 本轮 9：quickPaths 5 + manifest 契约 3 + 无 home 回退 1；3 ignored 为既有） |
| smoke | `python3 scripts/smoke_test.py` | **PASS 45 / SKIP 2 / FAIL 0**（新增 fs/quick-paths、memory/quick-paths、fs-quickpaths/quick-paths 全 PASS；SKIP 为 s3/sftp 容器段，见下） |
| 前端 | `pnpm typecheck` / `vitest run` / `pnpm build` | typecheck 绿；**68 passed**（基线 50 + 18）；build 产出自包含 ui/index.html |
| 全链路 | `./scripts/test.sh` | **all green**，`.dbxp` 打包成功（dist/io.dbx.files-0.1.0-darwin-arm64.dbxp，unsigned review candidate） |

容器段 SKIP 说明（不做凭据伪造）：
- `docker ps` 实测本机只有 `dbx-files-sftp-test`（:2299）与 `dbx-ssh-test`
  （:2222）两个 openssh 容器，**无 MinIO**；`DBX_FILES_S3_*` 环境变量未设 →
  s3 段按脚本语义 SKIP（原因已由脚本输出记录）。
- sftp 段需要 `DBX_FILES_SFTP_KEY`（已注册进容器 authorized_keys 的私钥
  路径），本轮未拿到合法测试私钥，不伪造凭据 → SKIP（既有语义，环境就绪
  后 `DBX_FILES_SFTP_HOST=127.0.0.1 DBX_FILES_SFTP_PORT=2299 ...` 即可启用）。

每个改动参数的「真实跑通」证据映射：
- `files/quickPaths`：ops 单测 5 个（资格表/候选表/stat 过滤/无 home 回退/
  memory 根回退）+ smoke 3 场景实跑 PASS；
- 自定义服务 schema/校验：opendalServices.spec 17 个新用例；
- manifest 契约：model.rs 3 个新测试直读 `../manifest.json` 实测；
- 七语文案：i18n.spec 键集一致断言（含本轮新增 15 键 ×7）。

## 2. 涉及文件

- 前端：`src/lib/opendalServices.ts`、`src/lib/opendalServices.spec.ts`、
  `src/components/CustomConfigEditor.vue`、`src/App.vue`、
  `src/lib/i18n.ts`（15 新键 ×7）、`src/lib/mockHost.ts`、`src/style.css`。
- 后端：`src/engine/ops.rs`（quick_paths 族 + 单测）、`src/main.rs`
  （注册分支）、`src/model.rs`（manifest 契约 3 测试）。
- 契约/脚本：`manifest.json`（required ×3、known_hosts_strategy 改 select）、
  `docs/IMPL_PLAN_DBX_FILES.zh-CN.md`（§8.1 增 `files/quickPaths` 行）、
  `scripts/smoke_test.py`（quick-paths 场景 ×3 + 探针连接）。

## 3. 遗留 / 已知限制

1. **URL/host 安全护栏是 UI 层语义**（CustomConfigEditor + 单测）；JSON 模式
   与宿主表单仍逐字透传（后端 `validate_endpoints` 仅做既有校验）。若要
   后端硬门禁（SSRF 防线后移），需在 `engine/mod.rs::validate_endpoints`
   扩展同名拒绝规则——本轮刻意未做，避免破坏本机 MinIO（127.0.0.1）冒烟链路。
2. **webdav/ftp 的 endpoint 必填**无法用 manifest 现有 visible_when 表达
   （endpoint 字段被 s3/webdav/ftp/sftp 共享，required 是字段级开关）；
   如需精确表达须拆分 per-protocol 字段（协议面变化，留待后续波次）。
3. 非 fs 协议（如 s3/sftp 的远端）当前只有根目录 chip；`/tmp`、`/etc` 等
   tiny-rdm remote quick paths 未做（远端路径约定因服务而异，可后续按
   capabilities/scheme 扩展）。
4. 浏览器/宿主端到端验证未做（按约定另行安排）；chips 行、双 Tab 的视觉
   细节（dark 主题下的边框对比）建议宿主 e2e 时顺带核对。
5. `files/quickPaths` 的 Windows 语义：`fs_home_dir` 回退 `USERPROFILE`，
   但候选目录仍按 Unix 拼接（`~/Desktop` 等在 Windows 上会 stat 失败而
   自动隐藏 chip，仅剩 root chip）——可接受的降级，后续可按平台补齐。

## 4. 阻塞

无。

### OpenDAL 服务范围收敛（2026-08-29 任务轮 2）

- 决策（用户指示，参照 opendal.apache.org/services 分类）：配置面只收 **文件存储**
  （fs/sftp/ftp/webdav）与 **云对象存储**（s3/gcs/azblob/oss/obs/cos）；db/cache/
  memory 类不再进 UI（memory 保留 backend feature 供 smoke/单测，经 JSON 模式仍可用）。
- 新增云存储：**obs（华为）**、**cos（腾讯）**——后端 Cargo.toml 增 services-obs/
  services-cos feature；前端 schema 键取自本地锁定 opendal-service-{obs,cos}-0.57.0
  crate 源的 config.rs（权威）：obs=bucket/endpoint/access_key_id/secret_access_key，
  cos=bucket/endpoint/secret_id/secret_key。
- 测试：新增 `cloud_custom_service_operators_build_with_official_config_keys`
  （obs/cos/oss 用 schema 键真实构建 Operator，构建不联网）；spec 增「db/cache 类
  不在配置面」断言与 obs/cos 必填键断言；CustomConfigEditor 默认服务 memory→fs。
- 验证：cargo **104 passed**、前端 vitest **69 passed**、typecheck/build 绿、
  smoke **PASS 45 / SKIP 2 / FAIL 0**。

### 第 3 轮对抗式审查（2026-08-29 任务轮 3，agent 超时由主会话核验收尾）

- **`files/audit/list` 自洽缺口关单**（对标 gap1，最优先项）：backend 新增只读分发臂
  （limit clamp 1..=1000、connectionId 过滤、newest-first、凭据红线注释）+ 4 个单测；
  mockHost 对齐；smoke 新增审计场景（45→49 PASS）。AuditPanel 至此恢复可用。
- **大目录策略**（对标 gap5 关单，选提示方案而非分页切换）：`lib/largeDir.ts` 阈值
  2000 + `largeDirectory` 七语提示。依据（文档内载）：后端 listPaged 每页重新全量枚举
  （bench 实测单页 7.3ms vs 全量 8.8ms，5 页 36ms 反而更贵）；前端排序/过滤/全选语义
  建立在完整数组上；FileTable 已虚拟滚动。spec 覆盖。
- 验证：cargo **108 passed**（+4）、前端 vitest **72 passed**（+3）、typecheck/build 绿、
  smoke **PASS 49 / SKIP 2 / FAIL 0**。

### 后端 endpoint scheme 护栏（2026-08-29 任务轮 4，遗留收口）

- 第 3 轮遗留「URL/host 护栏仅 UI 层」部分收口：`validate_endpoints` 扩展覆盖
  **opendal-custom config JSON 内的 endpoint**（此前只查 quick protocol 字段），
  按服务区分允许 scheme——HTTP 类（s3/gcs/azblob/oss/obs/cos/webdav/memory）限
  http/https，ftp 限 ftp/ftps，sftp 限 ssh，未知服务仅拒 file://（最小惊讶）。
  本机 MinIO（http://127.0.0.1）不受影响；私有地址放行决策保留（本地存储工作流
  合法场景），私有网段硬门禁仍留待 policy 层设计。
- 新增测试 `custom_config_endpoints_are_scheme_gated_per_service`（7 断言）。
- 验证：cargo **109 passed**（+1）、smoke PASS 49 / FAIL 0、前端三件套绿。
