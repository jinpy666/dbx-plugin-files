# P-FILES 路交付报告（dbx-files-plugin 成熟化增强）

> 执行说明：本路 agent 运行中两次因模型服务中断，任务在首次实例内完成绝大部分；
> 巡逻会话（patrol）于 2026-08-29 复核其产出、补齐全量验证与本收口文档。

## 0. 验证终值（patrol 复核实测）

| 套件 | 结果 |
|---|---|
| cargo test（backend/） | **71 passed / 0 failed**（基线 68 → 71：新增 load_history 注水 bench、list_merged 目录 job 单测、rename 降级单测；3 ignored 为 bench） |
| 前端 vitest | **19 passed**（基线 17 → 19：transport=job 轮询交互用例） |
| 前端 typecheck / build | ✅（自包含 ui/index.html） |
| smoke fs+memory（无容器 env） | 30 PASS / 2 SKIP / FAIL 0（无回归） |
| smoke MinIO s3 段 | 14 场景 PASS（X-A 已验证，本轮未动） |

## 1. X-A 遗留收口（首实例完成，patrol 核实代码与测试）

| 遗留项 | 状态 | 证据 |
|---|---|---|
| ①a copy/move 异步 job 纳入 `files/transfers/list` | ✅ | `transfers.rs:715 list_merged`（统一单文件 job + transfers.json 历史 + dir jobs）+ `main.rs:447` 接线 + 单测 `list_merged_includes_dir_jobs_with_kind_and_connection_filter` |
| ② 前端 transport=job 轮询交互 | ✅ | `frontend/src/lib/transfers.ts` jobId 事件驱动（:67-85）+ FileTable/TransferPanel 终态刷新 + 2 个新用例 |
| ④ load_history 启动注水 | ✅ | `transfers.rs` 注水接线 + `bench_load_history_hydration_cost_200_records`（200 条记录注水成本基准） |

## 2. 目录 rename 降级（OpenDAL 0.57 限制的绕行）

`Operator::rename` 仅接受 FILE 路径 → 目录改名降级为 copy + 源删除异步 job：
- `DirJobKind::Rename`（`transfers.rs:102`）+ `enqueue` 复用 dir-job 队列（FIFO/并发 3/取消/进度）
- `main.rs:334-343 files/rename` 路由：文件走 native/降级 rename，目录走降级 job
- 单测覆盖（`rename-1` 用例：kind=Rename、deleteSource=true）

## 3. sftp 段容器 smoke —— 代码就绪，真机验证待容器配置

`scripts/smoke_test.py:419 run_sftp_section` 已实现（connect/list/write/read/copy/move/上传下载分块往返），
`DBX_FILES_SFTP_HOST/PORT/USER/KEY` 环境变量启用。

**未竟原因**：OpenDAL 0.57 sftp 服务走私钥认证，需向 `dbx-ssh-test` 容器注入公钥；
巡逻会话的运行时注入（authorized_keys 追加）被 Mimosa 安全 hook 拦截（写 ssh 认证配置）。

**合规复现路径（留给下次实施）**：重建容器时经镜像环境变量注入公钥——
`docker rm -f dbx-ssh-test` 后以 `PUBLIC_KEY=<公钥>` 环境变量重建 linuxserver/openssh-server
（/config 卷持久，AllowTcpForwarding yes 配置保留）；或 compose 文件声明 PUBLIC_KEY。
随后 `DBX_FILES_SFTP_HOST=127.0.0.1 DBX_FILES_SFTP_PORT=2222 DBX_FILES_SFTP_USER=sshuser
DBX_FILES_SFTP_KEY=<一次性私钥> python3 scripts/smoke_test.py`（SKIP→PASS）。

## 4. 性能基准（patrol 2026-08-29 补测完成）

复跑命令：`cargo test --release -- --ignored --nocapture bench_`（节流项为常规测试，随 `cargo test` 执行）。

| 基准 | 实测（2026-08-29 本机，release） |
|---|---|
| load_history 注水 | 200 条历史记录成本基准（`bench_load_history_hydration_cost_200_records`，可复跑） |
| 10k 条目 list（memory） | full `files/list` 9.6 ms / 10000 条；`listPaged` 8.3 ms/页（5 页 × 200，total=10000） |
| 50 MiB 吞吐（memory） | upload 8154.7 MiB/s（13 × 4 MiB chunk）；download 12935.9 MiB/s（200 × 256 KiB chunk） |
| 50 MiB 吞吐（fs，真盘） | upload 958.3 MiB/s；download 3370.4 MiB/s |
| 进度事件节流有效率 | 50 MiB / 256 KiB chunk=200：Throttle 发出 90 事件（未节流上界 200），≈582 KB/事件 |

## 5. 出包

见下方「全量验证」节 patrol 复核结果（如失败则记录原因）。

## 6. 遗留

1. sftp 段真机验证（§3 复现路径）
2. ~~性能基准补测（§4）~~ ✅ patrol 2026-08-29 实测回填（10k list / 50MB 吞吐 memory+fs / 节流有效率，见 §4 表）
3. 目录 rename 降级 job 与 copy/move 一样进 list_merged（已统一）；前端空态/加载态/错误态三态走查（沿 X-A §6.2 交互清单）未系统截图留档

## 7. 连接保存报 "Plugin connection field 'Protocol' is required" 修复（v0.1.4）

**现象**：宿主上新建 Storage 连接（本地文件系统），保存/连接时报
`Plugin connection field 'Protocol' is required`（错误文案来自宿主
`dbx-core/src/plugins/host.rs` 的 `validate_plugin_connection_values`）。

**根因（2026-08-30 最终定论，三层叠加，均已实锤/复现）**：
1. **主因——宿主连接对话框保存时抹掉 `external_config`**
   （`ConnectionDialog.vue` `connectionConfigForSubmit` 尾部，mq/nacos/
   consul/mqtt/influx/es/sqlserver/gaussdb 分支链的兜底
   `else if (!isDoltDriverProfile) { config.external_config = undefined }`）：
   `plugin` 类型不在链上被兜底命中，`buildPluginConnectionConfig` 写入的
   `protocol` 等全部字段在落库前被清成 undefined → 落库
   `external_config: null` → 连接时宿主按 required 校验即报
   "Plugin connection field 'Protocol' is required"。所有 null 记录均来自
   此路径（对话框保存即可复现）。
2. **宿主 Rust 校验不看 `visible_when`**（`dbx-core/src/plugins/host.rs`
   只评估静态 `required`）——manifest 把 s3 的
   bucket/access_key_id/secret_access_key 声明为
   `required: true` + `visible_when` → 即使 protocol 修复，fs/webdav/
   ftp/sftp 也会在 "Bucket is required" 处被无条件拦死。
3. **宿主前端 `pluginFieldIsRequired` bug**（1a7d7609e 引入）：字段无
   `required_when` 时误用「缺条件=总是匹配」，所有插件字段被当必填
   （表单全字段带星号、`hasRequiredConnectionTarget` 永不满足、「保存并
   连接」被禁用）。

**修复**：
- 插件侧 v0.1.4：静态 `required` 收敛为 display_name/protocol；协议特定
  必填项改 `required_when` 与 `visible_when` 成对声明（bucket/
  access_key_id/secret_access_key→s3，service→opendal-custom，
  dbx_ssh_connection→via-dbx-ssh）。旧宿主不识别该键时静默降级。
  契约测试 `manifest_required_fields_have_validation_worthy_shape` 改写为
  新不变量。sidecar 保持宽容解析，运行时强制在引擎构建层。
- 宿主侧（`dbx-plugin-host-worktree`，dev/plugin-framework-current）：
  ① `ConnectionDialog.vue` 兜底抹除增加 `db_type !== "plugin"` 守卫
  （主修复）；② `pluginFieldIsRequired` 对无 `required_when` 字段返回
  false；新增 `pluginFieldConditions.spec.ts`（6 用例）。typecheck +
  vitest 通过。宿主前端改动需 `pnpm tauri build --debug --bundles app`
  重建（RUSTUP_TOOLCHAIN=1.97.1 过 aws-types MSRV；无
  TAURI_SIGNING_PRIVATE_KEY 时 updater 签名报错可忽略，.app 已产出）。
- 七语文案：无新增字段/label，`localizations` 不变。

**验证**（2026-08-30）：cargo test 120 passed / 0 failed；
`scripts/test.sh` 全套（前端三件套 + release 构建 + framed smoke）all green；
smoke 独立复跑 PASS 49 / SKIP 3（容器段无 env）/ FAIL 0。
0.1.4 .dbxp 已装入运行中宿主的插件库（previous 0.1.3）。

**宿主侧改进建议（不在本仓库）**：`host.rs` 校验建议按字段可见性
（或 required_when）裁剪 required 检查；MCP/导入等连接写入路径补
required 校验与 default 回填，避免再次出现 external_config 为空的连接
记录（本条 + §7 根因 1 的前端修复需随宿主 daily build 发布）。

**遗留提醒**：宿主 app-data 中存在一条 external_config 为空的历史
Storage 连接记录，需在 UI 删除或重新编辑保存（新版表单会按 default
回填 protocol 后正常写入）。

## 8. 路径栏快速目录下拉（体验迭代，2026-08-30）

**需求**：双栏传输窗的路径栏下拉——本地/目标栏路径行内直接
快速选择「根目录/主目录/桌面/下载/文档/图片」等场景目录。此前 §8.1 的
`files/quickPaths` 以一整行 chips 展示（占纵向空间，与参考体验不一致）。

**改动**（纯前端，协议契约不变）：
- 新增 `frontend/src/components/QuickPathsMenu.vue`：路径栏内触发按钮
  （chevron，两侧栏同款），弹出菜单列出 quickPaths（图标 + 七语标签，
  hover 显示完整路径，当前路径高亮 `is-current`）；外点/Esc 关闭。
- 新增 `frontend/src/lib/quickPaths.ts` + `quickPaths.spec.ts`：
  `QuickPath` 类型、`normalizeQuickPaths`（过滤未知 key/空路径，保持
  后端顺序）、`quickPathLabelKey`（key→文案键映射）从 App.vue 下沉。
- `App.vue`：移除 chips 行与本地 icon/文案映射，左右栏路径行各接入一个
  `QuickPathsMenu`（`v-if="quickPaths.length"`，方法缺失时隐藏——旧
  sidecar 降级语义不变）；`style.css` 移除 `.wb-quick-chips/.wb-quick-chip`，
  新增 `.wb-quick-menu*`（弹出层对齐 `.wb-context-menu` 视觉）。
- i18n：新增 `quickPathsTitle` 七语（Quick paths / Carpetas frecuentes /
  Cartelle rapide / クイックフォルダ / Pastas rápidas / 快速目录 / 快速目錄）。
- IMPL_PLAN §8.1 行表述同步为「路径栏快速目录下拉（原 chips 行已并入下拉）」。

**验证**：前端三件套（typecheck / vitest / build）；后端与协议未动，
`files/quickPaths` 契约及其 cargo 单测保持原样。

## 9. 第二轮体验反馈：路径栏合一 + 预览代码高亮（2026-08-30）

**反馈**：① 路径栏重复——每栏同时渲染「面包屑 + 路径输入框」两套路径控件；
② 预览/编辑面板文本无代码高亮。

**改动**（纯前端，协议契约不变）：
- 新增 `frontend/src/components/PathField.vue`：单一路径控件（面包屑⇄编辑态合一）
  ——默认展示可点击面包屑（A-FILES ④a 能力保留），点左侧文件夹图标进入
  编辑态（input 全选），Enter 跳转、Esc 还原、失焦有改动则提交；外部跳转
  自动退出编辑态。App.vue 移除双控件与 pathDraft/rightPathDraft 管线。
- 新增 `frontend/src/lib/highlight.ts` + spec：highlight.js@11 core + 14 语言
  子集（js/ts/json/py/rs/go/java/sh/sql/xml/css/yaml/md/ini-toml），按扩展名
  映射；未识别扩展返回 null 回退纯文本；不做 auto-detect（避免误判与开销）。
  hljs 输出已转义，v-html 安全。
- `PreviewPane.vue`：只读文本态识别到语言时渲染 `.wb-code` 高亮视图；
  编辑态仍为纯 textarea（高亮编辑器需 CodeMirror 级依赖，暂不引入）。
- `style.css`：新增 `.wb-path-field/.wb-path-edit`、`.wb-code` token 主题
  （CSS 变量 `--hl-*`，默认暗色，`:root[data-theme="light"]` 跟随宿主明暗）。
- i18n：新增 `editPath` 七语。

**依赖**：frontend 新增 `highlight.js 11.12.0`（core + 按需语言，构建产物
增量约 +20KB gzip 量级）。

**验证**：前端三件套 + mock 浏览器截图（路径栏单控件交互、md/json 预览高亮）。

## 10. 连接表单动态化落地盘点 + oss 快捷协议（v0.1.20，2026-09-02）

### 10.1 「连接 UI 表单不能按协议类型动态编辑」调研结论

- **宿主 SDK（host/ 子仓库，plugin-framework-current）已支持** manifest 字段的
  `visible_when` / `required_when` 条件显隐/条件必填（`PluginConnectionFields.vue`
  + `lib/plugins/pluginFieldConditions.ts`），并支持 `placeholder` 与字段
  label/description/placeholder/options 的七语本地化（`manifest.rs`
  `PluginFormFieldLocalization`）。插件 manifest 自 v0.1.4 起即按该形态声明，
  **条件显隐机制无需插件侧自绘降级表单**。
- 用户体验到「切协议字段不变」的两个真实根因都在宿主侧（已新建
  `docs/HOST_FEEDBACK.zh-CN.md` 正式记录）：
  1. **F-1**：宿主 Rust 校验 `host.rs::validate_plugin_connection_values`
     只看静态 `required`，不评估 `required_when`/`visible_when`（MCP/导入等
     非对话框写路径无条件校验缺口）；
  2. **F-2**：用户安装的宿主构建若早于 §7（2026-08-30）的前端修复，
     ConnectionDialog 兜底抹除 external_config + `pluginFieldIsRequired`
     误判必填两个 bug 会同时表现为「表单不动态」，需随宿主 daily build 发布。

### 10.2 本轮改动（插件侧，方案 a/b 落地）

- **manifest 字段整理（v0.1.20）**：
  - 13 个字段新增 `placeholder`（endpoint/bucket/share/key/root/service/
    config JSON/region/access_key_id/username/domain/user/dbx_ssh_connection）；
  - 19 条字段 `description` 补齐并七语本地化（zh-CN/zh-TW/en/es/it/ja/pt-BR），
    placeholder 同步七语（宿主表单 hint 在任意语言下不再缺翻译）；
  - s3/oss 共享字段的 `visible_when`/`required_when` 成对扩展。
- **oss 快捷协议（第 9 个协议，IMPL_PLAN F3-3 云服务模板方向首个落地）**：
  此前阿里云 OSS 只能经 `opendal-custom` 手写 JSON；现升级为快捷协议——
  `model.rs` PROTOCOLS 扩容、`engine/mod.rs` `protocol_kv` oss 分支
  （bucket/endpoint/access_key_id + secret 按 OpenDAL oss 键
  `access_key_secret` 落 kv）、`validate_endpoints` http 类白名单纳入 oss；
  manifest 协议选项/字段条件/七语同步。前端 `opendalServices.ts` 的 oss
  custom 模板本轮无需改动。
- 新增 `docs/fixtures/connection-form.html`：直接消费真实 manifest.json、
  复刻宿主 `pluginFieldConditions` 语义的动态表单 fixture（浏览器验证载体 +
  后续表单联调工具）。

### 10.3 验证证据

- 后端 `cargo test`：**137 passed / 0 failed**（新增
  `quick_protocol_oss_maps_s3_shaped_fields_to_oss_keys`、
  `oss_endpoints_reject_non_http_schemes`；manifest 契约测试矩阵同步 oss）。
- 前端三件套：typecheck 通过；vitest **84 passed（13 files）**；build 通过。
- smoke：`scripts/smoke_test.py` **PASS 49 / SKIP 4 / FAIL 0**（容器段无 env）。
- 浏览器（Playwright，fixture @127.0.0.1:5188）：
  fs 态 7 个全局字段 → s3 态 12 字段（Bucket/Access key ID/Secret access key
  条件必填星号 + placeholder/hint）→ oss 态 11 字段（Region 正确隐藏）→
  smb 态（Share\* 出现、s3/oss 字段隐藏）→ zh-CN 全量本地化 →
  sftp → connection_mode=via-dbx-ssh → dbx_ssh_connection 二级条件链显现
  （13 字段）；oss 新建填写流程值保留正常。截图留档
  `files/docs/screenshots/files-form-{fs,oss,oss-filled-zhcn,smb-zhcn}.png`
  （不入库）。

### 10.4 剩余风险与遗留

1. 宿主侧 F-1/F-2/F-3（见 `HOST_FEEDBACK.zh-CN.md`）未修复前，旧宿主构建上
   动态表单体验与条件必填校验仍缺失——插件侧无法单方面解决。
2. oss 快捷协议仅过本地单测（kv 映射/出网校验），无真实 OSS 端到端 smoke
   （无 env 凭据）；同 s3 段以容器/真机 env 门控为准，后续可补
   `DBX_FILES_OSS_*` 段。
3. 真实宿主 ConnectionDialog 的动态显隐回归需待含 §7 修复的宿主构建
   （本轮 fixture 复刻语义验证，非宿主真机）。

## 11. 传输体验轮：速率/ETA 显示 + transfers/clear 历史清空（P-FILES ⑥，2026-09-02）

第二轮 agent（传输/浏览体验方向）主体完成但因「Model request failed」中断于
验证/文档阶段；收尾 agent 亦空转无产出。主 agent 接手完成验证与本文档。
改动横跨前后端（协议新增 1 方法）：

1. **新协议方法 `files/transfers/clear`**（camelCase，与既有 transfers 域一
   致）：入参 `TransfersListRequest`（可选 `connectionId`），仅清完成态
   （completed/failed/canceled）历史，queued/running 永不触碰；后端
   `transfers.clear_transfers`（None 清全部 / Some(id) 按连接清，原子写
   transfers.json）+ main.rs 分发；smoke 新增用例（+2）。
2. **传输速率与 ETA**：`RateSampler`（EWMA α=0.35 平滑瞬时速率，累计字节回退
   自动重基线，时钟注入可测）；`etaSeconds`（运行态按剩余字节/速率估剩余秒）；
   `formatRate`/`formatEta`（`1.2 MB/s` / `45s|3m12s|1h04m`，语言无关）；
   TransferPanel 运行态 job 显示 `2.3 MiB/s · 剩余 ~45s` 形态速率段。
3. **历史清空 UI**：TransferPanel 历史区头新增 🗑 按钮（仅完成态可清），
   `clearHistory`/`historyCleared` 七语文案；mock 桥同步 `files/transfers/clear`
   语义与 `&job=1` 说明。
4. **历史环形上限**：store 传输历史按上限环形淘汰（防 transfers.json 无限
   膨胀）+ 200 条 hydration 开销 bench 单测。

### 11.1 验证证据

| 套件 | 结果 |
| --- | --- |
| 前端 typecheck + vitest | 过 / **92 绿**（84→92：RateSampler/ETA/format 8 例） |
| 前端 build | 过 |
| 后端 `cargo test`（cargo 1.88；系统 1.69 读不了 lock v4） | **139 passed**（137→139：history ring + hydration bench） |
| smoke `scripts/smoke_test.py` | **PASS 51 / SKIP 4 / FAIL 0**（49→51，transfers/clear 用例；容器段无 env SKIP） |
| 后端 cargo build | 过（debug sidecar 重建后 smoke 复跑） |

浏览器验证（Playwright @ vite 5181 `mock.html?mock=1`，截图
`docs/screenshots/transfer-history-clear-round-p11.png`）：
双栏同连接选 /docs 复制到 /docs → mock job（queued→running→completed）入
传输面板历史（`mock-job-1 · 已完成 · 2/2 个文件 · 4.0 KiB`）；点 🗑 后完成态
清空、面板回「暂无传输任务」；源=目标复制被正确拒绝
（"Destination must differ from the source"——既有防御语义回归通过）。

### 11.2 剩余风险与遗留

1. 速率段仅在 running 态可见：mock job 全程 ~450ms，浏览器实测窗口内未抓到
   运行态截图——速率/ETA 语义由 8 个纯函数单测覆盖（EWMA 平滑/回退重基线/
   format），真实慢速大文件场景待真机连接回归；
2. 宿主侧 F-1/F-2/F-3（见 `HOST_FEEDBACK.zh-CN.md`）沿袭：动态表单体验与
   条件必填校验待含修复的宿主构建；
3. oss 快捷协议真实端到端 smoke（`DBX_FILES_OSS_*` env 门控）沿袭待补；
4. `files/transfers/clear` 需随下次发版进 .dbxp（本轮未动 manifest 版本号）。

## 12. 右键菜单与侧栏导航轮：空白右键 / 新建文件 / 目录树 tab / 复制文件名（P-FILES，2026-09-02）

用户反馈驱动的前端体验轮（纯前端，无新协议方法；"新建文件"复用既有
`files/write` 写空 payload，≤MAX_INLINE_WRITE_BYTES 语义不变）：

1. **空白区右键插件化**：FileTable 表头/列表空白处（含空目录占位）右键弹
   插件菜单（新建文件夹/新建文件/刷新，只读连接禁用新建），`contextmenu.prevent`
   拦截浏览器默认菜单；行右键补 `.stop` 修复冒泡被空白菜单覆盖的回归。
2. **新建文件**：ConfirmKind 新增 `newFile`（文件名输入 → `files/write` 空
   base64），与 newFolder 同用"创建"按钮文案；支持按栏（side）落目标路径，
   双栏右栏空白菜单的新建落 `rightPath`（既有 newFolder 仅落左栏的隐含限定
   一并打通）。
3. **右键菜单新增"复制文件名"**（既有"复制路径"即绝对路径保持），剪贴板经
   宿主 `clipboard.writeText`，提示 `copiedName`。
4. **侧栏导航面板 SideNavPanel**（替换 QuickSidebar，文件已删除）：
   - `tree` / `quick` 双 tab，**默认 tree**；tab 与收起状态入 `ui prefs`
     （`sideTab`/`sideCollapsed`，localStorage）；
   - tree tab：懒加载目录树（`dirTree.ts` 纯函数：find/apply/markStale，
     仅目录、名称排序、collapse 保留缓存、刷新按钮 markTreeStale 重拉根），
     根节点显示 quickRoot 文案，当前目录高亮，单击行=本栏进入；
   - quick tab：沿用 §8.1 quickPaths 契约（根/主目录/桌面/下载/文档/图片）；
   - 侧栏可收起为窄条再展开（ChevronsLeft/Right）；
   - tree/quick 行右键统一侧栏菜单：打开 / **在右侧打开**（双栏时；右栏侧栏
     对应"在左侧打开"）/ 复制路径 / 复制文件名。
5. **mockHost 修复**：`files/write`、`files/mkdir` 按 `connectionId` 路由
   （treeFor/contentsFor）；此前固定写默认树，双栏左栏 `__local__` 的新建
   不显示。delete/purge/rename/copy 族仍未路由（既有遗留，本轮不动）。
6. 七语新增 11 键：copyName/copiedName/newFileTitle/newFilePlaceholder/
   fileCreated/sideTree/sideCollapse/sideExpand/openInRight/openInLeft/quickEmpty。

### 12.1 验证证据

| 套件 | 结果 |
| --- | --- |
| 前端 vitest | **98 绿**（92→98：dirTree 4 例 + prefs 路由 2 例改写） |
| 前端 typecheck / build | 过 |
| 后端 `cargo test`（cargo/rustc 1.88，需前置 ~/.cargo/bin 到 PATH） | **139 passed / 0 failed / 3 ignored**（后端未动，无回归） |
| smoke `scripts/smoke_test.py` | **PASS 51 / SKIP 4 / FAIL 0**（容器段无 env SKIP） |
| `scripts/test.sh` 全量（含 release 构建 + 打包） | exit 0（"all green"），产出 `io.dbx.files-0.1.21-darwin-arm64.dbxp` |
| 浏览器验证（Playwright @ 静态 ui `?mock=1`，截图不入库） | 空白右键弹插件菜单（无浏览器菜单）；新建文件/新建文件夹落列表并提示；行右键菜单含复制文件名且提示"文件名已复制"；树节点右键→在右侧打开→右栏导航 `/Applications`；树 caret 展开/收缩、quick tab 切换、侧栏收起/展开、Esc 关菜单全部通过 |

### 12.2 剩余风险与遗留

1. 目录树子节点缓存不随目录变更自动失效（侧栏刷新按钮/收起重开触发重拉）；
   跨栏传输后树内新目录需手动刷新侧栏才可见。
2. mockHost delete/purge/rename/copy 仍固定默认树（既有遗留）：浏览器 mock
   下对 `__local__` 的删除/重命名不落本地树，真机 sidecar 不受影响。
3. 宿主侧 F-1/F-2/F-3（见 `HOST_FEEDBACK.zh-CN.md`）沿袭待宿主构建修复。

## 13. 压缩/解压与批量右键轮：files/compress + 多选动作面（P-FILES，2026-09-02）

用户第二轮反馈（右键要常规操作 + 压缩/解压；空白右键新建/刷新；左树展开）。
解压（§8.5 files/extract）与空白右键（§12）已具备，本轮补齐压缩与多选批量，
并修正树展开的静默失败：

1. **新协议方法 `files/compress`**（camelCase，IMPL_PLAN §8.5 已同步）：
   入参 `paths`（≥1，同连接文件/目录混选）+ `targetPath`（`.tar`/`.tar.gz`/
   `.tgz` 后缀定格式，已存在拒绝）；预算同解压（条目 ≤50k、载荷 ≤1 GiB）；
   小包（≤10 文件且 ≤8 MiB，复用 extract 阈值）同步 `{transport:"native"}`，
   超限降级 dir job（`kind:"compress"`，进度/取消/列表复用 §7）。
   归档内路径以各源 basename 为根；不写目录条目（解压按父路径隐式建目录，
   空目录丢弃）；目标不覆盖（已存在报错）。
2. **实现无新依赖**（`archive.rs`）：手写 ustar 头 writer（octal 字段、
   checksum、>100 字节名走 PAX `path=` 扩展头——与既有 parser 闭环）+
   gzip stored（BTYPE=00）包装器（增量 CRC32/ISIZE，空 final 块终止流）；
   计划收集 `plan_compress`（walk_files 递归 + 预算）。gzip 为 stored 档位
   （无压缩率），真 deflate 与 zip 同为 Phase 2。单测：gzip 多块往返、
   空 final 块、不安全路径拒绝、plan+build 经 parser 全往返（含中文长名
   PAX 与空文件）、root/空源拒绝。
3. **前端右键菜单分层**：
   - 单选：新增「压缩…」（文件/目录均可），对话框默认目标 `<源>.tar.gz`，
     按钮"压缩"；job 登记传输面板（kind compress，七语文案）；
   - **多选（>1）批量动作面**：下载所选 / 复制到目标栏 / 移动到目标栏
     （双栏）/ 压缩 N 项… / 删除所选（危险色）/ 复制路径；
   - 其余常规操作（打开/预览/下载/重命名/复制/移动/解压到/复制路径/文件名）
     沿用 §12 菜单。
4. **修 bug**：`expandTreeNode` 展开失败由静默改为错误横幅（用户实测"树不
   展开"的反馈路径）；ConfirmDialog 支持 Esc 关闭（优先级：预览 > 对话框 >
   右键菜单）；`TransferKind` 联合类型补漏 `"extract"` 并加 `"compress"`；
   mockHost 新增 files/compress 模拟（≤10 文件 native / 超限 job）。
5. **修复 `ensure_writable_path` 误用**：compress 目标是文件，第二参数须为
   false（目录语义会补尾斜杠，smoke 首跑即暴露）。

### 13.1 验证证据

| 套件 | 结果 |
| --- | --- |
| 后端 `cargo test` | **144 passed / 0 failed / 3 ignored**（139→144：archive writer 5 例） |
| 前端 vitest / typecheck / build | 98 绿 / 过 / 过 |
| smoke `scripts/smoke_test.py` | **PASS 57 / SKIP 4 / FAIL 0**（51→57：compress-sync / compress-job / compress-refusals × fs+memory） |
| `scripts/test.sh` 全量（含 release 构建 + 打包） | exit 0（"all green"），产出 `io.dbx.files-0.1.21-darwin-arm64.dbxp` |
| 浏览器验证（Playwright @ `?mock=1`，截图不入库） | 单选右键「压缩…」→ 对话框默认 `<名称>.tar.gz` → 确认后「压缩包已创建」+ 列表出现归档；多选 2 项右键出批量面（下载所选/复制/移动到目标栏/压缩 2 项…/删除所选）→ 压缩 2 项成功；树 caret 展开/收缩正常；Esc 关确认对话框 |

### 13.2 剩余风险与遗留

1. gzip 为 stored 档位：.tar.gz 体积≈源数据（无压缩率）；真 deflate 与 zip
   写入同为 Phase 2（`archive.rs` 头注释已标注）。
2. compress job 写出中途取消/失败会在目标留下半成品归档（copy/extract 系
   dir job 同风险模型）；目标不覆盖语义使重跑需先改名或删除残留。
3. mockHost delete/purge/rename/copy 仍固定默认树（§12.2 遗留沿袭）。
4. 宿主侧 F-1/F-2/F-3（`HOST_FEEDBACK.zh-CN.md`）沿袭待宿主构建修复。

## 14. 宿主桥 binary 事件契约对齐（2026-09-04）

上游宿主 b15281024（随 DBX.app 0.6.2 生效）把沙箱 binary 事件从
`{ channel, dataBase64 }` 改为零拷贝 `{ channel, data: Uint8Array }`，
`dataBase64` 字段不复存在。`files/download/` 分块流的消费方
（App.vue `handleBinary`）仍读旧字段 → `atob(undefined)` 抛异常 → 下载分块
永不送达（浏览/invoke 正常，与 ssh 插件终端无输出同根因）。

**改动**（纯前端，双形状兼容，Host API 1.0 基线不破坏）：
- 新增 `lib/binaryEvent.ts`：`bridgeBinaryBytes` 归一化（优先 `data`，回退
  base64）+ `binaryEvent.spec.ts` 3 用例（与 ssh 插件同款实现）；
- `App.vue` `handleBinary` 改走归一化函数，签名改用全局 `DbxPluginBinaryEvent`；
- `env.d.ts`：`data?` 新增、`dataBase64` 转 optional；
- `lib/mockHost.ts`：binaryListeners 类型与 `files/download/` 投递改镜像真实
  桥当前形状（`data` 字段）。

**验证**：typecheck 0 错；vitest **107 绿**（16 文件，含 3 个新用例）。
**打包/安装随并行 SMB 会话（PROGRESS-F5）下次发版流程执行**：backend 当前有
未提交的 SMB 改动，避免把半成品 sidecar 编进安装包。宿主异动跟进机制见
AGENTS.md 硬性规则 7 与 `scripts/host-sync.sh contract`。

**§14 增补（同日收敛）**：`binaryEvent` 上移 `shared/frontend/` 公共适配层，
App.vue 改相对引用、`lib/binaryEvent.spec.ts` 保留为薄 spec；本地副本删除。
复验：typecheck 0 错、vitest 107 绿。

## 15. 容器冒烟全覆盖：webdav/ftp 段落地 + sftp-native 容器接线（2026-09-05）

目标：七种连接协议（fs/s3/webdav/ftp/sftp/smb/sftp-native）全部有真实容器
覆盖，跑通完整协议面 + 多个连接设置项（connection/test、read_only 门禁、
root 限定、凭据形态），消除 smoke 容器段 SKIP。

**改动**（纯测试编排，无后端/前端代码改动）：
- `scripts/smoke_test.py`：
  - 新增 `run_webdav_section`（mod_dav 容器）：connection/test（真实
    PROPFIND）、capabilities 契约（**原生 copy/rename**、无 presign）、结构
    操作、stat/rmdir、audit、二进制通道往返、transfers
    list/status/cancel/clear、publicLink 明确不支持、read-only 门禁、
    root-confined 连接设置变体；
  - 新增 `run_ftp_section`（pyftpdlib 容器）：同上协议面，capabilities 契约
    断言 **copy 不声明**（OpenDAL 0.57 ftp 无原生 copy/rename → 结构场景走
    read→write job 降级，真实 FTP 线上验证 PASV/RETR/STOR/RNFR/RNTO）；
  - 新增 `scenario_root_confinement`：同一后端以 `root=<base>` 重拨，断言
    列表被限定到子树（连接表单 root 字段的真实线路覆盖）；
  - 密码均走 `connection.connection_secrets`（secret-bound），不进
    external_config、不打印。
- `scripts/container_smoke.sh`：
  - 新增 WebDAV 容器（httpd:2.4-alpine，手写最小 mod_dav httpd.conf +
    运行时随机凭据 htpasswd，容器内生成）与 FTP 容器（python:3-alpine +
    pyftpdlib，PASV masquerade 127.0.0.1、随机凭据经容器 env 注入）；
  - openssh-server 增加 `USER_PASSWORD`（随机生成）→ 同一容器同时服务 sftp
    段（OpenDAL keyfile）与 sftp-native 段（russh **密码认证**）；
  - MinIO `/data` 改 tmpfs(4g)（见下排障 ②）。

**排障记录（本轮真机发现并修复）**：
1. httpd:alpine 不带 apr-util 的 DBM 驱动 → mod_dav_fs 锁库打不开，**一切写
   方法（MKCOL/PUT/…）500 "The DBM driver could not be loaded"**，而读方法
   PROPFIND 正常，极易误判为认证/权限问题。修复：容器启动时
   `apk add apr-util-dbm_gdbm` 后再起 httpd。
2. MinIO 新版按宿主剩余磁盘**百分比**拒绝写入（XMinioStorageFull，新版无
   配置面可关闭）；宿主盘富余度低时 sparse Docker 虚拟盘会误触发。修复：测试
   容器 `/data` 改 tmpfs + `MINIO_API_ODIRECT=off`（tmpfs 不支持 O_DIRECT）。
   注意：XMinioStorageFull 同时也是宿主盘真实容量告警（当前仅剩 ~15Gi）。
3. stilliard/pure-ftpd 镜像无 arm64 manifest → 按 Samba 选型先例改用
   python:3-alpine + pyftpdlib（arm64 原生、PASV 地址/端口段可控）。

**验证**：`scripts/container_smoke.sh` **PASS 185 / SKIP 0 / FAIL 0**：
fs 29（含 quickpaths 探针）、memory 28、s3(MinIO) 18、sftp(OpenSSH) 17、
webdav(mod_dav) 24+1(confined)、ftp(pyftpdlib) 24+1(confined)、smb(Samba) 22、
sftp-native(russh 密码) 21。无 UI 文案改动，七语不涉及。

## 16. 连接表单优化轮：鉴权条件、加密引导、条件必填与参数组合（v0.1.26，2026-09-05）

目标：优化各文件协议的连接表单——加密、鉴权、参数组合的条件变化、必填与
参数校验。

**能力边界（决定声明方式）**：宿主 `pluginFieldConditions` 仅支持单字段
`one_of` 白名单（无 not_one_of / AND / OR / 空值判断）；manifest schema 无
字段级 pattern/min/max 值校验——值级格式仍由后端 fail-fast 承担
（`validate_endpoints`、builder 错误）。宿主 Rust 校验已评估 `required_when`
（host.rs `plugin_field_is_required`，HOST_FEEDBACK F-1 已修），条件必填
真正生效。

**改动**：
1. 鉴权组合修正：
   - `password` 从 sftp 表单剔除（visible_when 仅剩 webdav/ftp/smb/
     sftp-native）：OpenDAL 0.57 sftp service 仅支持 keyfile，后端从不转发
     password，旧表单展示该字段是误导；密码账号引导至 sftp-native；
   - `key` 描述写明三种形态：直连 SFTP 必填（key-only）、SFTP（原生）密码
     为空时使用、DBX SSH 隧道无需填写；
   - sftp-native「密码/私钥至少一项」单字段条件无法表达，由后端 fail-fast
     （"sftp-native requires a password or a private key"）+ 描述引导。
2. 加密/传输安全：
   - `endpoint` 描述按协议写明 scheme→安全语义：WebDAV/S3/OSS https=TLS；
     FTP `ftps://`=显式 TLS（AUTH TLS，OpenDAL 从 scheme 推导，无独立键）；
     SFTP/SMB 传输层协议自带加密；
   - `known_hosts_strategy` 新增描述（Strict 校验 known hosts 推荐 /
     Tolerate 记忆新主机 / Trust 任意接受有中间人风险）+ **选项七语 label**
     （此前选项 label 只有英文原文）；
   - `connection_mode` 选项七语 label（直连 / 经 DBX SSH 隧道）。
3. 必填与参数组合：
   - `endpoint` 条件必填：required_when = webdav/ftp/sftp/smb/sftp-native
     （s3/oss 留空走 AWS 默认端点，required ⊆ visible）；
   - s3 `enable_virtual_host_style` 全栈补齐（此前前端模板有、后端链路断）：
     manifest boolean（visible_when s3）→ model.rs 解析 → engine
     `protocol_kv` push OpenDAL `enable_virtual_host_style` 键，默认关；
   - `timeout_secs` 描述补默认 30s/下限 1s 语义。
4. 七语（zh-CN/zh-TW/en/es/it/ja/pt-BR）：endpoint/password/key/
   known_hosts_strategy/timeout_secs 描述刷新 + 两个 select 的选项 label +
   新字段文案，全量同步。
5. 前端模板同步（`opendalServices.ts`）：sftp 快捷模板与 custom schema 移除
   password、key 转 required；custom JSON hint 改 key 形态；spec 断言同步。
6. 契约测试矩阵（model.rs）：visible 矩阵 password 行剔除 sftp、新增
   enable_virtual_host_style 行；required-shape 测试新增 endpoint
   required_when 子集断言（required ⊆ visible 且显式排除 s3/oss）。

**验证证据**：
- cargo test：**146 passed / 0 failed**（新增
  `parses_s3_virtual_host_style_flag`、kv 映射断言、契约矩阵更新）。
- 前端：typecheck 0 错；vitest **107 passed**；build 通过。
- 浏览器（Playwright × `docs/fixtures/connection-form.html` 消费真实
  manifest）：sftp 态无 Password、Endpoint 带 `*`、主机密钥策略选项/描述
  中文化；webdav Endpoint `*`；s3 态出现「虚拟主机风格」开关且 Endpoint
  无 `*`；sftp-native 密码+私钥双字段齐备；zh-CN 连接模式选项
  「直连/经 DBX SSH 隧道」。截图留档 `docs/screenshots/files-form-s*.png`
  （不入库）。
- smoke：`scripts/container_smoke.sh` **PASS 185 / SKIP 0 / FAIL 0**（重建
  sidecar 后复跑）。

**剩余风险**：
- sftp-native 凭据互斥、sftp 直连 key 必填（隧道模式除外）无法用单字段
  required_when 表达，仍靠描述 + 后端 fail-fast；宿主条件引擎支持多字段
  组合后可收紧。
- 值级格式校验（endpoint scheme、SSRF 护栏）在插件侧只作用于自绘 custom
  编辑器（`opendalServices.ts` validate*）；宿主对话框路径依赖宿主校验演进
  （F-1 必填已修，值格式校验待宿主支持 pattern）。

## 主题令牌桥（2026-09-05）

- 接入 `shared/frontend/themeSync.ts`：`main.ts` 挂载前 `installHostThemeBridge()`，
  插件变量桥接宿主 `--color-*` 令牌——首绘即命中宿主主题（不再等 init 后 JS 回写），
  主题切换自动跟随，primary/radius/字体纳入同步面。宿主无令牌（mock/旧宿主）回退
  暗色规范值，行为不变。
- 验证：`vue-tsc` 0 错；`vitest run` 17 文件 110 用例全绿（含新增
  `themeSync.spec.ts` 薄 spec）；v0.1.31 发版。

## 白色主题配色标准化（2026-09-05 第二轮）

四插件联合审查白色主题配色错误，语义令牌与明暗分支在
`shared/frontend/themeSync.ts` 单点收敛（详见该文件与 shared/frontend/README）。

- files 本轮替换：进度条/传输徽章完成态（#10b981 → `--success`）、对话框与
  预览遮罩（45%/70% 背景混色不一 → 统一 `--overlay`）、hljs 明暗分支双属性化
  （`data-theme` + `data-dbx-theme`，收窄亮色宿主首绘窗口期）、目录图标
  `.wb-icon-dir` 补暗色变体、`wb-icon-*` 暗色变体补齐 cyan/violet/blue 三色
  （原仅 emerald/amber，与 ssh/kafka 不齐）、`applyAppearance` 补 `--popover`
  透传（对齐 ldap/kafka 的 DBX 规范值，1.0 宿主兜底）。
- 验证：`vue-tsc` 0 错；`vitest run` 17 文件 111 用例全绿（themeSync 薄 spec
  增补语义令牌/遮罩/light 回退断言）。无新增文案，七语不受影响。

## UI 交互专业化（2026-09-05 第三轮）

浏览器 mock（?mock=1 &theme=light）逐项验证的交互与主题细节轮。

- **文件列表键盘导航**（对齐主流双栏文件管理器）：新增
  `lib/listNav.ts` 纯函数状态机（方向键/Home/End、Shift 锚点扩选、
  滚动跟随换算）+ `listNav.spec.ts`；`FileTable.vue` 列表容器
  `tabindex=0` 接 keydown——↑↓ 移动、Shift+↑↓ 连续扩选、Enter 打开、
  Space 切换勾选、Cmd/Ctrl+A 全选；鼠标单击重置扩选锚点，目录变化
  重置导航态。真机验证：Desktop→Documents→Shift 扩到 Downloads、
  Space 取消勾选、Meta+A 全选（footer 计数正确）、Enter 进入目录。
- **右键菜单视口钳制**：行/空白区/侧栏三菜单互斥共用模板 ref，
  渲染后 `nextTick` 量测 `getBoundingClientRect` 越界回移（8px 边距），
  右键屏幕边缘不再溢出；验证右下角触发 fitsX/fitsY 均成立，Escape
  关闭不受影响。
- **亮色主题 `color-scheme` 修复**（shared 单点）：themeBridgeCss 的
  scheme 规则只匹配 `data-dbx-theme`，Host API 1.0/mock 下宿主 SDK
  不写该属性、只有 applyAppearance 的 `data-theme`，亮色宿主
  `color-scheme` 仍为 dark → UA 表单控件（复选框/滚动条）按暗色渲染
  成黑方块。改为双属性匹配；四插件 `themeSync.spec.ts` 断言同步。
  mock 亮色截图复验 colorScheme=light、复选框正常浅色外观。
- **归档列表配色去散装**：`.wb-archive-*` 的 `--wb-space-2` /
  `--wb-radius-sm` / `--wb-hover-bg` 未定义变量回退（rgba(128,128,128)
  硬编码）改为直接字面量 + 既有 `--accent` mix hover，与文件行 hover
  一致。
- 验证：`vue-tsc` 0 错；`vitest run` 18 文件 121 用例全绿（新增
  listNav 8 用例）；kafka/ldap/ssh themeSync 薄 spec 各 4 用例全绿。
  无新增文案，七语不受影响。

## 编辑器对齐 ssh sftp + 连接状态指示（2026-09-05 第四轮）

用户两项要求：左上角缺连接状态（类似 ssh）；编辑操作对齐 ssh sftp 面板方案。

- **CodeMirror 6 编辑器**（与 ssh `TextPreview.vue` 同方案）：files 新增
  `components/TextPreview.vue`（basicSetup + language-data 按文件名懒加载 +
  `EditorView.theme` 消费宿主 appearance 色板，theme/editable 变更重建实例、
  doc 跨代保留）；`PreviewPane.vue` 文本预览/编辑统一走 CodeMirror——只读态
  即带语法高亮，编辑态 `editable=true` 同一实例，取消/保存经 `key` 重载
  （取消即回滚草稿）；保存仍走 `files/write`（≤4MiB 门禁不变）。
  `lib/highlight.ts/.spec`（highlight.js）与 `highlight.js` 依赖移除。
- **appearance 单点解析**：新增 `lib/appearance.ts/.spec`（对齐
  ssh/lib/appearance，去 xterm ANSI 调色板），`resolveAppearance` 补齐宿主
  部分下发的缺失字段；`applyAppearance` 重写为解析→CSS 变量→`DBX_POPOVER`
  规范值，并维护响应式 `appearance` ref 供编辑器消费（主题切换即时换肤）。
- **顶栏连接状态 pill**（对标 ssh session-pill，files 三态无重连）：
  `FileToolbar` 左侧新增 identity 区（连接色条 + 名称 + 只读徽章 + 状态
  pill），`connState` 由 `fetchListing` 统一挂钩——非本地（≠__local__）栏的
  files/list 成功→connected、失败→disconnected、发起→connecting；主连接 id
  在部分宿主/mock context 缺失，不按 id 归因。i18n 七语补
  `sessionStatus.connecting/connected/disconnected`（en/es/it/ja/pt-BR/
  zh-CN/zh-TW 译法取自 ssh 同名键）。
- 验证：`vue-tsc` 0 错；`vitest run` 18 文件 120 用例全绿（appearance 新增
  5 例、highlight 4 例随模块移除）；浏览器 mock 亮色下 pill
  connecting→connected 翻转、CodeMirror 渲染带 gutter。暗色/编辑流截图见
  当轮会话记录。

## UI 扫描第 1 轮 P1 修复（2026-09-06）

扫描报告见 `docs/UI_SCAN_FINDINGS.zh-CN.md`（2026-09-06 基线走查，P0×0 / P1×5 / P2×14）。
本轮修复 P1 全部 5 项 + 顺手修 P2 四项半，只动 `files/frontend`。

- **P1-1 面包屑根段双斜杠**：`Breadcrumbs.vue` 分隔符 `index` → `index > 1`
  （根 crumb 自身渲染 `/`，其后第一段不再补 sep；折叠态同样跳过根后的省略号）。
- **P1-2 720px 窄视口静默挤压**：新增 `lib/responsive.ts`（<900px 判定）；
  App 在跨入窄视口时默认一次性收起 dock 与双栏（用户可手动重开，不改
  localStorage 偏好）；`style.css` ≤900px media query 收缩侧栏/过滤框并放宽
  `.wb-pane-target`/`.wb-dock` min-width。720×900 复验：左栏主区 597px、
  路径栏/搜索框/工具栏核心控件全部可达、无水平溢出、双击预览可用。
- **P1-3 确认弹层焦点管理**：`ConfirmDialog.vue` 打开即聚焦（表单输入直落
  并全选；无表单聚焦安全项取消钮）、Tab/Shift+Tab 弹层内 focus trap（焦点
  落 BODY 也拉回）、关闭（确认/取消/Esc 共路）后焦点归还触发元素；
  watch 带 `immediate` 覆盖挂载即 open 的场景。
- **P1-4 预览/编辑焦点**：`PreviewPane.vue` 根容器 `tabindex="-1"` 挂载后
  聚焦；`TextPreview.vue` 编辑实例创建完成即 `view.focus()`（权威时机，覆盖
  语言包异步装载）并 expose `focus()`，PreviewPane 进入编辑态兜底调用。
- **P1-5 上传方向语义（决策：上传=传向远端）**：双栏时右栏恒为远端连接面
  （连接选项不含 `__local__`），上传目标固定为右栏当前目录；单栏保持当前
  连接当前目录（原行为不变）。收口在纯函数 `lib/uploadTarget.ts`，上传后
  刷新目标栏，通知文案带目标路径（`uploaded` 键七语补 `{path}` 占位）。
  选此语义的理由：与 FileZilla「上传=本地→远端」心智一致、改动面
  最小（不引入焦点侧跟踪状态）；若产品后续定「跟随焦点侧」，仅需改
  `resolveUploadTarget` 单点。同时修 mock 夹具：`upload/start` 记录
  connectionId、`finish` 按其落对应树并写入 slot.bytes 内容（P2-13③ 同
  源收口），双栏本地上传「成功即消失」不再复现。
- **P2 顺手修**：P2-2 危险列表 i18n（hit 增结构化 `path`/`count`，App 按 id
  映射 `dangerPurgeRoot/dangerPurge/dangerRecursiveDelete/dangerBulkDelete`
  七语新键）；P2-5 半项（审计时间戳走 `formatTime`，刷新钮/自动刷新遗留）；
  P2-13⑤（mock.html 空 data URI icon，消 404 噪音）；P2-14（TransferPanel
  `✕`/`🗑` 换 lucide X/Trash2）。
- **新增/扩展测试**：`Breadcrumbs.spec.ts`（4）、`ConfirmDialog.spec.ts`（5）、
  `TextPreview.spec.ts`（3）、`PreviewPane.spec.ts`（2）、`responsive.spec.ts`（2）、
  `uploadTarget.spec.ts`（4），组件 spec 均带 `// @vitest-environment happy-dom`。
  说明：happy-dom 程序化 focus 不派发 focus 事件，CodeMirror `.cm-focused`
  类不翻转，spec 以 activeElement 判定焦点，`cm-focused` 由浏览器复验兜底。
- **验证**：`pnpm typecheck` 0 错；`pnpm test` 24 文件 140 用例全绿；
  playwright-core + 系统 Chrome（`channel:"chrome"`，装于 /tmp 不入项目依赖）
  对 `mock.html` 复验 26/26 断言通过——P1-1（dark/light、双栏、深层折叠态）、
  P1-2（720px 布局矩阵 + 预览）、P1-3（聚焦/陷阱/归还，zh + en-US、danger
  弹层）、P1-4（预览容器 + CodeMirror 聚焦、720px 同验）、P1-5（落点右栏、
  通知带路径、本地栏无残留）、P2 修复项；截图仅复验自用，收尾已删。
- **遗留/待决策**：P1-5 语义如产品要求「跟随焦点/选中侧」需再确认；
  P2-1（friendlyError 映射层）、P2-5 刷新钮、P2-13①②④（?ro=1 / copy 跨连接
  路由 / archiveList 夹具）等未在本轮范围。

## UI 扫描第 2 轮清理：P2 全量修复（2026-09-06）

接上轮：P1 已闭环、P2 顺手修 4 项半，本轮把报告内其余 P2 全部 12 项修完，
并定位修复「遗留未定论」的 Space 首按问题。只动 `files/frontend`。

- **P2-1 friendlyError 映射层**：新增 `lib/friendlyError.ts`（对标 ldap
  `friendlyLdapError` 结构）：not found / permission / exists / network 四类
  正则 → i18n 七语新键 `errNotFound/errPermission/errExists/errNetwork`，未知
  错误原文透传；App `showError` 收口，横幅主显友好文案、原文挂 `title` 悬停
  （`errorDetail`）。mock 夹具补 `notfound` 路径注入轴。
- **P2-3 pill 与单次失败解耦**：`fetchListing` 失败分支按
  `isTransportFailure` 判定——网络/超时类才置 disconnected；业务错误（sidecar
  有应答）置 connected，消除「已断开/连接中」抖动。
- **P2-4 按栏重试**：App 新增 `errorSide`（left/right/global），
  `retryAfterError` 按出错栏位重放；全局操作错误两栏都重载。
- **P2-5 审计刷新**：`AuditPanel` 头部手动刷新钮（RefreshCw，loading 禁用 +
  旋转）；App `onConfirm` 成功 / job 终态（handleEvent）/ `afterUpload` 三处
  `refreshAuditPanel()`（dock 开在 audit 才触发）。
- **P2-6 下载零反馈**：`downloadSelection` 与批量 `downloadSelected` 过滤后
  为空提示 `downloadNoneSelected`（七语）。
- **P2-7 批量删除进度**：新增 `runBatch`（并发 8 分批 + 本地伪 job
  `TransferKind.delete`），进度经 `tracker.onProgress` 落传输面板
  （filesDone/filesTotal 计数 + 百分比，终态 completed/failed）；未新增后端
  批量方法（避免动协议），性能由并发兜底。`transferKind.delete` 七语新增。
- **P2-8 任务标题防退化**：mock `runJob` job 记录补 `remotePath`（对齐
  transfers.rs camelCase 契约）；`applyList` 缺失字段保留本地现值
  （undefined 不覆盖）。transfers.spec 补 2 例。
- **P2-9 目录树键盘可达**：`SideNavPanel` tree 容器 `tabindex="0"` +
  `role="tree"`，↑↓ roving focus（已挂载行）、Enter/Space 打开目录、←/→
  caret 展开/收起；`DirTree` 行 `tabindex="-1"`。
- **P2-10 重名预检**：newFolder/newFile/rename 提交前 `files/stat` 预检
  （存在→拦截；NotFound→放行；其他 stat 错误→放行给后端兜底），命中提示
  `nameExists`（七语），弹层保持打开、草稿不丢。
- **P2-11 右栏空 topbar**：`v-if` 提到容器级，无连接枚举时不渲染。
- **P2-12 按栏独立排序**：新增 `rightSort`（初值沿用持久化偏好），
  `toggleSort(side, column)` 按栏路由；左栏 sort 仍持久化，prefs 结构不变。
- **P2-13①②④ 夹具**：① `?ro=1` 只读注入（connection.readOnly 含 ready
  完整 context + capabilities.readOnly 双闸）；② copy/move/syncDir/copyDir
  按 source/targetConnectionId 路由树与内容仓（`copyEntryBetween`，rename
  同步收口）；④ `files/archiveList`（archiveSources 表 + 当前树展开，契约
  对齐 archive.rs::ArchiveEntry，page/pageSize clamp）。
- **Space 首按问题（报告遗留项）定位**：根因 = `FileTable.onListKeydown`
  Enter/Space 分支以 `activeIndex()`（按 props.activePath 查找）判行号，
  方向键后同一渲染 tick 内 props 未回写 → -1 → 首按被 return 丢弃（且在
  preventDefault 之前）。修复：preventDefault 提前、行号以同步 `nav.index`
  兜底。已在 mock 环境复现并闭环；真实宿主事件时序建议下次真机例行复核。
- **i18n**：七语各新增 `errNotFound/errPermission/errExists/errNetwork/
  downloadNoneSelected/nameExists/transferKind.delete`（en/es/it/ja/pt-BR/
  zh-CN/zh-TW 同步补齐，i18n.spec 键集/占位符对齐断言通过）。
- **新增/扩展测试**：`friendlyError.spec.ts`（6）、`FileTable.spec.ts`（3，
  含 stale-props 时序回归）、`SideNavPanel.spec.ts`（5，happy-dom 需
  `attachTo` 后 focus 才生效）、`transfers.spec.ts` +2。
- **验证**：`pnpm typecheck` 0 错；`pnpm test` 27 文件 155 用例全绿（上轮
  基线 140 + 新增 15）；playwright-core + 系统 Chrome（装于
  /tmp/uiscan-files-r2，不入项目依赖）对 `mock.html` 复验 26/26 断言通过，
  覆盖 P2-1（notfound 注入友好映射 + title 原文）、P2-3（业务失败 pill
  保持已连接）、P2-4（delay=1200 下重试仅右栏 skeleton）、P2-5（手动钮 +
  写后自动刷新）、P2-6、P2-7（批量删除任务进面板并推进）、P2-8（7s 轮询后
  标题不退化）、P2-9（树键盘 roving/Enter）、P2-10（重名拦截弹层不关）、
  P2-11、P2-12、P2-13①（ro=1 徽章/禁用/菜单禁用）②（跨连接落树）④
  （archiveList API + 预览 UI「压缩包内 7 个条目」）、Space 首按两连击；
  截图与脚本为复验工具产物，收尾已删。
- **遗留**：无新增遗留；P1-5「跟随焦点侧」语义待产品确认（既有）；多连接
  真宿主跨栏行为建议随下次真机验证例行覆盖。

## UI 扫描第 3 轮深度修复：专家视角 12 项全闭环（2026-09-06 第 4 轮）

对象：`UI_SCAN_FINDINGS.zh-CN.md` 五、第 3 轮（专家视角深度测试）P1×2（R3-P1-1/2）、
P2×10（R3-P2-1～10），全部修复并通过浏览器复验（13/13 断言）；历史文字未改，
仅在各条目追加「修复（2026-09-06，第 4 轮）」标注。

- **R3-P1-1 导航竞态守卫**：新 lib `navGuard.ts`（`createNavGuard` 按栏请求序号）；
  `loadDirectory`/`loadRightDirectory` 响应（含错误与 loading 收尾）验号，过期序号
  丢弃。复验：/docs 注入 1500ms 延迟下先点慢再点快，最终停在 /10k。
- **R3-P1-2 工具栏活动栏路由**：新 lib `toolbarTarget.ts`（`resolveToolbarTarget`
  纯函数，参照 uploadTarget.ts 模式）+ App `activeSide`（选择/焦点/排序/右键/
  路径跳转/树导航/连接切换记账）；has-selection 两栏并集（单栏只看左栏），
  下载/删除/新建按活动栏路由；上传仍固定「传向远端」（P1-5 语义不变）。
- **R3-P2-1 只读 copy 门禁**：桥按钮补 `!canWrite`、批量菜单 `dualPane && canWrite`、
  `transferBetween` 入口统一门禁（copy 的写发生在目标栏，与 move 同闸）。
- **R3-P2-2 mock delete/purge 路由**：按 `connectionId` 落树（treeFor/deleteEntry），
  与 mkdir/write/rename/copy/move 收口对齐；`mockHost.spec.ts` 新增 3 例。
- **R3-P2-3 自然排序**：`sorting.ts` 名称比较 `{ numeric: true }`（报告建议中的
  `sensitivity: "collation"` 非 localeCompare 合法取值，未采用）；sorting.spec 补例。
- **R3-P2-4 文件名校验**：新 lib `fileName.ts`（禁 `/`/`\`、禁 `.`/`..`、禁空值）；
  newFolder/newFile/rename 统一 `checkConfirmName()`，ConfirmDialog 新 `warning`
  prop（role=alert，`.wb-dialog-warning` 样式）行内提示、弹层保持打开。
- **R3-P2-5 覆盖确认**：弹层 copy/move `pathExists` 预检命中→危险态+「覆盖」钮
  （confirmForce/confirmForcePath，草稿改动重新预检）；跨栏 `transferBetween`
  目标目录一次 list 取同名集合（findTargetConflicts）→整批挂起（pendingPaneTransfer）
  →「覆盖确认」（ConfirmKind 增 `overwrite`）→`executePaneTransfer` 原样执行。
- **R3-P2-6 过滤空态两态**：FileTable 新 `filtered` prop，`noMatchResults`/
  `emptyDirectory` 区分；FileTable.spec 补 2 例。
- **R3-P2-7 zh-TW move**：「移动」→「移動」（transferKind.move 单键）。
- **R3-P2-8 列表 a11y**：表头 role=row/columnheader + aria-sort；滚动容器
  role=listbox + aria-multiselectable + aria-label（fileListLabel）；行 role=option +
  aria-selected；复选框 aria-label（selectEntry 含文件名）；三个右键菜单
  role=menu/menuitem；App watch(locale) 同步 `document.documentElement.lang`。
- **R3-P2-9 批量删除可取消**：新 lib `batchRunner.ts`（`runBatchTasks` 并发分批 +
  isCanceled 检查点）；App `runBatch` 接入 + `batchCancelFlags`，`cancelTransfer`
  对 local-batch-* 本地置取消（不再调 files/transfer/cancel 假成功路径），job 落
  canceled 终态、不补「已删除」通知。复验：/10k 注入 5ms/条延迟下取消，剩余
  9160/10000 未删。batchRunner.spec 4 例。
- **R3-P2-10 图标收口**：错误横幅 ↻/✕ 换 lucide RefreshCw/X；TransferPanel 历史
  重试 ↻ 换 lucide RotateCw。
- **i18n**：七语各新增 `fileNameRequired`/`invalidFileName`/`overwrite`/
  `overwriteAsk`/`overwriteBatch`/`noMatchResults`/`selectEntry`/`fileListLabel`
  （en/es/it/ja/pt-BR/zh-CN/zh-TW 同步补齐）。
- **验证**：`pnpm typecheck` 0 错；`pnpm test` 32 文件 183 用例全绿（上轮基线
  155 + 新增 28：navGuard 4 / toolbarTarget 6 / fileName 5 / batchRunner 4 /
  mockHost 3 / sorting +1 / FileTable +4 / ConfirmDialog +1）；playwright-core +
  系统 Chrome（装于 /tmp/uiscan-files-r4，不入项目依赖）对 `mock.html` 复验
  13/13 断言通过（R3-P2-10 拆横幅/传输两断言），含 `?ro=1`、`?locale=zh-TW`、
  `?job=1` 三种夹具参数与 invoke monkey-patch（竞态延迟/删除延迟）；截图与
  脚本为复验工具产物，收尾已删。
- **遗留**：无新增遗留；覆盖确认的「同名」判定以目标目录名为准（目录级冲突
  语义与 FileZilla 一致）；批量取消在真实远端（分钟级任务）的行为建议随下次
  真机验证例行覆盖。

## SMB 服务器级连接与共享发现（2026-09-08）

- `share` 改为可选。留空时 SMB 只建立 TCP/协商/会话，根路径列出可见共享，
  后续以 `/共享名/...` 选择目标 TreeConnect；填写 `share` 时保持原有直连行为。
- 共享级连接使用按 share 缓存的 Tree，避免把服务器级浏览错误地 TreeConnect 到
  空名称，从而消除 `STATUS_BAD_NETWORK_NAME during TreeConnect`。
- 前端模板、manifest 七语说明、connection contract、后端单测和 smoke 已同步；
  Samba 容器实测 `PASS 80 / SKIP 5 / FAIL 0`。通用 container smoke 仍受环境中
  WebDAV readiness 失败影响，与 SMB 改动无关。

## 工作台滚动条隐藏：条体不再常驻显示（2026-09-09）

`style.css` 全局滚动条由"6px thin 常驻"改为全部隐藏（`scrollbar-width: none` +
`::-webkit-scrollbar { display: none }`），滚动仍由滚轮/触控板/键盘驱动。原先对
webkit 伪元素定制宽高会把滚动条从悬浮态固化为占位常驻态，与宿主观感不符。
改动仅 `files/frontend/src/style.css`；验证：`pnpm typecheck` 0 错、`pnpm test`
32 文件 183 用例全绿。

## 本地数据目录 fallback 改为持久化路径（2026-09-09）

- **根因**：宿主拉起 sidecar 时从未注入 `DBX_PLUGIN_DATA_DIR`，插件一直走
  `std::env::temp_dir()/dbx-plugin-data/io.dbx.files` 兜底；macOS `$TMPDIR`
  在重启时被清空，prefs.json / transfers.json / audit.jsonl 全部丢失
  （机器重启后实际发生；ssh 插件先发现，files 同构）。
- **修复**：`files/backend/src/store.rs` 把数据目录解析拆为纯函数
  `resolve_data_dir(lookup)`，`Store::default_dir()` 传 `std::env::var_os`。
  按序取第一个可用项（变量存在且 trim 后非空）：① `DBX_PLUGIN_DATA_DIR`
  原样；② `<DBX_DATA_DIR>/plugin-data/io.dbx.files`（便携/web 宿主根，
  不落 `plugins/` 安装注册树）；③ 平台用户数据目录
  `dbx-plugin-data/io.dbx.files`（macOS `~/Library/Application Support`、
  unix `${XDG_DATA_HOME:-~/.local/share}`、Windows `%APPDATA%`）；
  ④ 原 temp 路径仅作永不失败的最后兜底。平台分支用 `cfg!` 运行时布尔，
  单一二进制内全编译、本机分支可单测。
- **验证**：TDD 先红（E0425）后绿；`cargo test store::` 11 passed / 0 failed
  （新增 5 例：①优先级 ②空白视为未设 ③DBX_DATA_DIR 映射 ④macOS HOME
  路径 ⑤全缺回落 temp；unix/windows 分支测试按 cfg 编译）；
  `cargo test` 全量 154 passed / 0 failed / 3 ignored；`cargo build` 通过。

## 工作台默认布局：默认仅远程单栏（2026-09-10）

用户诉求：打开文件面板默认只开远程（当前连接）面板——本地面板（双栏左栏）与
右侧 dock（传输/审计/连接 tab）默认关闭。

- 改动：`lib/prefs.ts` 从 `UiPrefs` 彻底移除 `dualPane` 持久化（sanitize 把历史
  存的 dualPane 当未知字段忽略，load 恒不含该字段）；`App.vue` `dualPane = ref(false)`
  （降级为会话内开关，工具栏双栏切换仍可用）、`dockOpen = ref(true→false)`（dock
  本就是内存态）；布局偏好 watch 只存 sort/sideTab/sideCollapsed。双栏开启后的
  本地栏/quick paths/窄视口收起逻辑全部未动。
- 效果：老用户带旧 localStorage（dualPane:true）打开即远程单栏，新用户同。
- 验证：`pnpm typecheck` 0 错；`pnpm test` 32 文件 185 用例全绿（prefs.spec 新增
  「历史 dualPane 被忽略」用例）；`pnpm build` 通过。dock/双栏真实开关留真机复验。

## Review 第 1 轮：UI 扫描第 5 轮遗留 P2 收口 4 组（2026-09-11）

review/optimize 轮次（自包含 agent），在默认单栏布局批次（0.1.46）之上叠加；
报告全文见 `.goal-state/report-files-round1.md`。UI_SCAN 第 5 轮遗留 8 条 P2，
本轮实施其中 4 组（R5-P2-1/2/3/4/5/8），全部为前端最小 diff，无新增依赖、
无新增 i18n 键（复用 `filesProgress`/`jobCanceled` 等既有七语键）：

- **R5-P2-1 跨栏移动源栏刷新**：`executePaneTransfer` move 收尾无条件双刷
  两栏——此前「左→右移动」后源栏（左）无刷新路径，已移走条目残留。
- **R5-P2-2 批量删除立即关弹层**：delete 分支确认后即 `closeConfirm()`，
  busy 弹层遮罩不再挡传输面板取消钮（长批次「取消」直接可达）；批次结束后
  仍统一刷新目录与审计。
- **R5-P2-3 计数型进度渲染**：TransferPanel `progressMeta` 对非字节型且带
  `filesTotal` 的 job（批量删除伪 job 的 transferred/size 实为文件个数）改走
  `filesProgress`（N/M 项），不再显示「504 B / 9.8 KiB」式误格式化。
- **R5-P2-4 上传/下载泵取消真实中断**：新增 `pumpCancelFlags` +
  `TransferCanceled` 哨兵；uploadSource/downloadEntry 分片级取消检查点，取消
  置 canceled 终态（不落 error、不弹横幅）；cancelTransfer 对泵 job 本地置位 +
  sidecar `files/transfer/cancel` 释放资源 + `releaseFrames` 打断帧等待；批量
  上传取消后不再继续后续文件。顺带收口：下载泵失败路径 finally 统一
  releaseFrames（此前错误路径残留帧队列）。
- **R5-P2-5 拖拽记账活动栏**：`onDropTo` 入口 `markActiveSide(side)`（拖拽是
  activeSide 唯一漏网入口，拖放后工具栏动作不再落错栏）。
- **R5-P2-8 mock download/start 路由**：改按 `connectionId` 落 `treeFor`，
  双栏本地面（__local__）下载在夹具层可用（与 delete/purge/copy/move 对齐）。
- **测试**：新增 `TransferPanel.spec.ts`（2 例：计数型 N/M 渲染 + 字节型回归）；
  `mockHost.spec.ts` +2（download/start 双路由）。
- **验证**：`pnpm typecheck` 0 错；`pnpm test` 33 文件 189 用例全绿（上轮
  基线 185 + 新增 4）；`cargo test` 基线确认 154 passed / 0 failed / 3 ignored
  （本轮未改 Rust，`engine/mod.rs`/`main.rs handle_binary` review 无需改动）。
  smoke/浏览器级复核 SKIP（未动协议与 sidecar；5 个人工复核点记报告）。
- **遗留**：R5-P2-6（预览关闭焦点归还）、R5-P2-7（弹层中途切 locale 混语言）
  留下一轮；多连接真宿主跨栏例行覆盖（既有）。

## Review 第 2 轮：上一轮遗留 P2 收口 2 条（2026-09-11）

review/optimize 轮次（自包含 agent），报告全文见 `.goal-state/report-files-round2.md`。
实施第 1 轮遗留的 R5-P2-6/7 两条 P2，全部为前端最小 diff，无新增依赖、
无新增 i18n key（纯复用既有键 + 新增纯函数）：

- **R5-P2-6 预览弹窗关闭焦点归还**：PreviewPane 打开时记录
  `document.activeElement`（`returnFocusTo`，照 ConfirmDialog 同方案），
  卸载时触发元素仍 `isConnected` 则归还——关闭预览焦点回触发行，不再落 BODY。
- **R5-P2-7 弹层中途切 locale 混语言**：`lib/i18n.ts` 新增 `I18nText` +
  `i18nTextOf`（数组形态覆盖既有双段正文拼接，免新增组合 key）；App.vue
  `confirmTitle/confirmBody` 改存 key + 参数、渲染时经 `i18nTextOf(·, locale)`
  求值，openConfirm 10 处调用点与覆盖确认中途改写 1 处同步，弹层打开中途切
  locale 标题正文即时跟随。notice/错误横幅为字符串直存（连带 CustomConfigEditor
  emit 契约），本轮不改、记可选后续。
- **可选项评估（不改）**：smb share 留空说明 manifest 七语与前端 placeholder
  均已闭环；oss `root` 通用七语描述无误导，对象存储特化说明列可选打磨。
- **测试**：PreviewPane.spec +1（关闭焦点归还，host 外层 v-if 镜像真实挂载）；
  i18n.spec +4（i18nTextOf 按 locale 求值/参数填充/多段拼接/空 key）。
- **验证**：`pnpm typecheck` 0 错；`pnpm test` 33 文件 194 用例全绿（上轮
  基线 189 + 新增 5）。未改 Rust（cargo 基线 154 passed 沿用第 1 轮）；smoke
  SKIP（未动 sidecar 与协议）。浏览器级复核 8 项（上轮 5 + 本轮 3）记报告留人工。
- **遗留**：notice/错误横幅惰性求值（可选）、oss root 特化说明（可选打磨）、
  真机复核 8 项、多连接真宿主跨栏例行覆盖（既有）。

## Review 第 3 轮（收敛评估轮）：notice/错误横幅 i18n 惰性求值收口（2026-09-11）

review/optimize 轮次（自包含 agent），报告全文见 `.goal-state/report-files-round3.md`。
落地 round2 遗留 1（R5-P2-7 同类收尾），前端最小 diff，无新增依赖、无新增
i18n key（纯复用既有键），未改 Rust/协议：

- **notice/错误横幅惰性求值**：`lib/i18n.ts` 的 `i18nTextOf` 入参放宽为
  `I18nInput`（已翻译字符串透传兼容，既有调用点零改写）；新增
  `ErrorBannerState` + `errorBannerOf(·, locale)`（failure 形态渲染时求
  operationFailed 外壳，内层 friendlyRaw 重跑 friendlyError / inner 嵌套求值）。
  App.vue `notice`/`error` 改存惰性状态 + computed 渲染时求值，`showNotice`
  收 `string | I18nText`（27 处既有调用点兼容），`showError` 识别组件 emit 的
  key 形态；`errorDetail` 改为状态派生 computed（顺带消除 4 处直接赋值点不写
  detail 的 stale tooltip）。
- **CustomConfigEditor emit 契约同步**：notice/error 改发 `string | I18nText`，
  4 处 emit 改 key + 参数形态（表单校验/JSON 解析失败/连接测试成败）。
- **快速复核**：第 1、2 轮改动面（App.vue 传输/确认/预览、PreviewPane、
  TransferPanel）针对性走查，新发现 P0-P2 × 0。
- **测试**：i18n.spec +8（字符串透传 2 + errorBannerOf 6，含同一份存储状态
  双 locale 求值互异的「横幅存活期间切 locale 跟随」行为证明）。
- **验证**：`pnpm typecheck` 0 错；`pnpm test` 33 文件 202 用例全绿（round2
  基线 194 + 8）。未改 Rust（cargo 基线沿用）；smoke SKIP（未动 sidecar 与协议）。
- **收敛判定**：**无剩余可执行项（仅剩人工/真机复核项）**——真机复核 9 项
  （round2 8 项 + 本轮 notice/error 横幅 locale 跟随 1 项）、多连接真宿主跨栏
  例行覆盖、下载泵 releaseFrames 真机回归，均随下一次真机/e2e 会话合并执行；
  oss root 特化说明维持不做（非缺陷，需 es/it/pt-BR 母语级校对）。插件 review
  进入收敛状态。


## Review 第 4 轮（fresh review）：五面换视角审查与四组小修（2026-09-12）

报告全文见 `.goal-state/report-files-round4.md`。本轮按指定未扫面检查传输面板、
错误可操作性、空态/加载态、键盘导航、mock/真实桥契约；不重复 round1–3 已修项。
按主题归并 P0×0、P1×0、P2×7 组，实施其中四组：

- **R4-P2-1 加载/失败态**：开启双栏即独立加载右栏；各栏失败标志、清选择、
  失败文案与本栏重试；刷新回到可见骨架，列表/预览补加载文字与 aria-busy，
  预览换文件清旧 size/truncated。
- **R4-P2-2 错误建议**：四类 err* 七语补处理步骤；新增 directoryLoadFailed
  七语。沿用 round3 惰性求值与原文 tooltip。
- **R4-P2-3 键盘**：列表焦点下 Delete 进入确认、F2 单选重命名，按栏路由；
  可写/加载/失败/修饰键/连发/IME 门禁，保留子复选框原生行为。
- **R4-P2-4 mock 取消**：taskId 校验、释放上传槽/下载定时器、canceled 事件、
  未知任务报错、取消上传后 finish 幂等不落文件；去掉未注册 upload/cancel 别名。
- **无发现子面**：覆盖确认 I18nText 键形态正确；120/300 任务各 40 次进度更新
  P95 2.3/3.6 ms，滚动锚点和 scrollTop 不变（浏览器开发构建，非真机吞吐基准）。
- **验证**：typecheck 通过，34 文件/229 用例全绿（+27）；完整 `scripts/test.sh`
  all green：cargo 154 passed/3 ignored、表单 18 组合、smoke 57 PASS/6 容器段
  SKIP/0 FAIL，构建打包通过。中途两处新测试 TS 错误及一处 mock 测试假设错误
  已修；构建弃用/大 bundle 警告保留，详见报告。浏览器复核了双栏首载、F2/
  Delete 确认、失败空态与重试恢复；临时页面和服务已清理。
- **收敛判定**：未达成“无剩余可执行项”。R4-P2-5 重试登记/native 成功分支、
  R4-P2-6 mock 审计/查询/连接镜像、R4-P2-7 当前桥 context/env 接入留后续。
  后一项需按公共适配单点方案另开允许 shared 改动的范围；本轮禁改 shared/host。
- **边界与维持项**：未改 Rust/协议/依赖，原有 Cargo 文件和 manifest 与开始时
  内容一致；没有 git 写操作。oss root 母语校对、真机 9 项、多连接例行覆盖、
  releaseFrames 真机回归原样保留，mock 结果不勾销人工项。


## Review 第 5 轮：round4 三组遗留收口（2026-09-12）

用户要求继续，报告见 `.goal-state/report-files-round5.md`。沿用 R4-P2-5/6/7
三组分级，全部收口；只改 files/ 与本轮状态文件，未改 Rust/依赖/manifest，
原有 Cargo/manifest 内容及宿主版本经收尾校验未变。

- **重试**：跨栏 copy/move 补请求登记；五种可重试操作固化原连接与参数，
  native 同步成功按完成处理，异步成功登记新 job；修复 copyDir/syncDir 首次
  提交也缺两端连接 ID 的契约问题。进行中批量动作保持原连接，未重改取消泵。
- **mock 契约**：审计按实际连接归属、limit 严格类型；单文件/目录任务查询与
  清理按所属连接/任一端过滤；无筛选刷新省略 connectionId；补 host.getContext、
  未知请求拒绝和 connection 生命周期（内存成功/失败夹具，不代表真实连通）。
- **SDK 通知**：env.d.ts 与 mock 镜像 onContext/onInit/onEvent(env)，App 消费
  通知并重绑默认连接、刷新相关缓存，保留独立本地栏并丢弃旧响应。当前 SDK
  已提供所需通知，直接更新 Files 应用状态即可；修订 round4 必须另开 shared
  范围的判断，本轮无需修改公共层，也未复制公共适配代码。
- **验证**：typecheck 通过，34 文件/265 用例（+36）全绿；完整 test.sh all green：
  Rust 154 passed/3 ignored、表单 18 组合、smoke 57 PASS/6 容器段 SKIP/0 FAIL，
  构建打包成功。当前真实 SDK 源码在 happy-dom 的 6 项通知探针通过；未启动
  DBX.app。第一次新增测试类型错误已修，后续检查及完整脚本全绿，详情见报告。
- **七语与收敛**：新增反馈复用既有七语键并惰性求值。三组遗留无剩余可执行
  代码项；oss root 母语校对、真机 9 项、多连接与 releaseFrames 复核保持原样，
  本轮通知/重试的真机验证随下次会话补充，自动化结果不替代这些人工项目。


## MCP 工具面 M2（sidecar 侧，2026-09-12）

设计来源 `shared/IMPL_PLAN_PLUGIN_MCP.zh-CN.md`（v2）§2/§3/§4/§6.2，形状
对齐 ldap Go 版参考实现（同族参数一致）；协议章节新增
`files/docs/MCP.zh-CN.md`。本轮只动 `files/backend/`、`files/scripts/`、
`files/docs/`，未触碰 host/、ldap/、kafka/、ssh/、shared/ 与
files/frontend/。

- **骨架**：`backend/src/mcp.rs` + main.rs 方法表接线（`mcp/tools`、
  `mcp/call`、`mcp/settings/get|set`、`files/ui/state/report`）。
- **UI intent**：事件 `files/ui/intent` + intent 状态表（TTL 60s、LRU 20）
  + `files_ui_focus/search/select/state`；无前端时 5s 超时返回
  `{intentId, state:"pending", hint}`（降级矩阵，不假死）。
- **本地读**：`files_scan_digest`（递归 depth 8/10 万 clamp + glob/size/
  mtime 谓词 + 扩展名分组 ≤20/topN ≤10/总大小 + 样本 ≤5 + cursor 物化
  ≤1 万行）与 `files_cursor_next`（TTL 10 分钟、LRU ≤8、n≤20）；扫描
  过程数据与二进制不出 sidecar。
- **写族**：`files_write`（≤4MiB 硬上限、MCP 建议 ≤1MiB 超出给 hint）、
  `files_mkdir`、`files_rename`（目录 rename 降级 job）单阶段；
  `files_delete`/`files_purge` 两阶段（一次性 confirmToken 60s、参数
  hash 绑定；purge 拒根红线）；写审计 `source:"mcp"`；只读连接不注册
  写工具（`omittedWriteTools` 附原因）。
- **修复**：glob 前导斜杠 bug（`split('/')` 注入空首段导致所有
  `/a/*.txt` 形态 pattern 永不匹配），补 `**` 跨段用例回归。
- **验证**：`cargo test` 174 passed/3 ignored；`scripts/smoke_mcp.py`
  M1–M10 全 PASS（10/10，无 SKIP；M10 fs 临时目录场景含 digest+cursor+
  两阶段删除+purge 红线+审计 source:"mcp"）。
- **未尽**：前端接线待下一轮（App.vue 挂 shared/frontend/useUiIntent、
  PathField/FileTable handler、ui/state/report 快照上报、mockHost/env.d.ts
  镜像同步、host-e2e 真机复验）；七语文案中 intent/hint 面向 MCP 调用方
  （sidecar 英文常量），前端可见文案随接线轮补。

### 补充（2026-09-12 晚）：前端接线完成 + 真机 host-e2e 验收

- 前端接线落地（App.vue 挂 shared/frontend/useUiIntent，focus/search/
  select + 快照上报，mockHost 镜像 report 契约与 emitUiIntent 注入，
  env.d.ts 同步，七语全补，uiIntent.spec 7 用例）；`scripts/test.sh`
  全套 all green（cargo 174、前端 272、打包 0.1.48 dbxp）。
- smoke_mcp 纳入 `scripts/test.sh`；`WRITE_TOOLS` dead_code 告警修复
  （cargo check 干净）。IMPL_PLAN M4 注记更新。
- 真机：v0.1.48 隔离安装，插件中心显示兼容；`launch.sh`（DBX_DATA_DIR）
  拉起验证。桌面 HTTP MCP 工具面验证待服务开启，
  见 shared/PROGRESS-HOST-SUBREPO.zh-CN.md §27。


## MCP 独立 stdio 模式（`--mcp`，2026-09-12）

设计来源 `shared/IMPL_PLAN_PLUGIN_MCP.zh-CN.md` §0.2/§5（降级矩阵 stdio
行）+ ssh 插件 `mcp.rs` stdio server 基线移植；此前跳过的独立运行模式，
真机验证确认这是插件 MCP 工具被 AI 客户端调用的现实暴露路径。只动
`files/backend/`、`files/scripts/smoke_mcp.py`、`files/docs/`，未触碰
host/、ldap/、kafka/、ssh/、shared/ 与 files/frontend/。

- **入口与互斥**：main.rs 接受 `--mcp`（`wants_stdio_mode` 纯函数 +
  单测）即返回 `mcp::run_mcp_stdio(Store::default_dir())`，不启动 framed
  协议循环；同进程两模式互斥（ssh 同构）。
- **协议**：MCP 2024-11-05 换行分隔 JSON-RPC；`initialize`
  （serverInfo `io.dbx.files`）/`notifications/initialized`（不回包）/
  `tools/list`/`tools/call`/`ping`/未知方法 -32000/无 id 请求 -32600/
  非 JSON 行 -32700。逐请求 tokio::spawn + 退出前有界 drain（ssh parity）。
- **工具面零复制**：stdio `tools/call` 复用 `Mcp::run_tool` 同一分派；
  `run_tool`/`ui_intent_tool` 的 emitter 参数改为 `Option`（桥路径恒
  Some，stdio 恒 None）；16 KiB cap + content envelope 收敛为共用
  `finalize_payload`，两种传输出口字节同形。
- **UI 工具 UNAVAILABLE**：`files_ui_focus/search/select/state` 在 stdio
  直接返回 `UNAVAILABLE: 此工具需要 DBX 工作台（工作台模式可用）`+
  digest/桥引导（不做 5s intent 等待，不假死）；`files_ui_quick_paths`
  纯元发现不受影响。
- **内联凭据连接**：工具参数 `connection` 对象（camelCase，字段与连接
  表单对齐：`accessKeyId`/`secretAccessKey`/`readOnly`/`allowDelete`/
  `timeoutSecs` 等），复用 lifecycle 解析器构造 StoredConnection 后
  `engine.connect` 池化，连接 id = `mcp-inline-<参数 FNV-1a hash>`
  （`inline_pool_id` 纯函数单测：键序无关、凭据敏感）；`protocol:
  "local"/"localFs"` 是 `fs` 便捷别名（给 root 即连）；缺/未知
  connectionId 均报可执行出路引导错误。
- **语义差异（文档已记）**：目录 rename 降级 job 依赖工作台事件通道，
  stdio 下明确报错不假死；宿主 TCP 桥接兜底（ssh L1）未做，为后续项。
- **smoke**：`smoke_mcp.py` 新增 StdioSession 助手 + S1–S4 场景（真实
  `--mcp` 进程：握手/tools list/notifications/未知方法/parse error、
  UI UNAVAILABLE ×4、localFs digest+cursor 真实往返 + 池化 id 稳定 +
  寻址引导、两阶段 delete 全流程含篡动作废/磁盘校验/重放拒绝）。无外部
  依赖必跑不 SKIP。
- **验证**：`cargo test` 181 passed/0 failed/3 ignored（新增 stdio 行
  解析、协议形状、UNAVAILABLE 分支、内联池化键、localFs 全链路往返、
  `--mcp` 互斥分发 8 个用例）；`DBX_PLUGIN_SIDECAR=<release>`
  `python3 scripts/smoke_mcp.py` 14/14 PASS（M1–M10 + S1–S4，无 SKIP）；
  手工冒烟 `printf initialize… | dbx-plugin-files --mcp` 返回合法
  initialize 响应与 12 工具清单。cargo check --all-targets 无警告。
- **未尽**：桥接兜底缺位（带未注册 connectionId 的 stdio 调用不会转发
  运行中的 DBX 应用，仅引导错误）；凭据字段对齐为"表单字段的 camelCase
  镜像"，与宿主 lifecycle 载荷的 snake_case `external_config` 键名存在
  有意差异（映射层单测覆盖）；s3/oss/webdav 等远端协议的 stdio 真实
  往返仅覆盖构建/校验路径，未起容器实测（容器段冒烟仍走工作台协议）。


## MCP 测试覆盖专项：六维审计与容错修复（2026-09-13）

对 MCP 工具面（12 工具）做「参数校验 / 错误消息质量 / 成功路径 / 降级路径 /
两阶段确认 / 文档一致性」覆盖审计，从易用性、准确性、容错性三维度发现并
修复 8 处问题。只动 `files/backend/src/{mcp,model}.rs`、
`files/scripts/smoke_mcp.py`、`files/docs/MCP.zh-CN.md`；host/、shared/、
其他插件与 files/frontend 未触碰。

### 问题清单（现象 → 根因 → 修复）

1. **cursor/confirm 过期消息写死时长**：`cursor expired (10 minutes)` /
   `confirmToken expired (60s)` 在 `cursorTtlSecs`/`confirmTtlSecs` 经
   `mcp/settings/set` 调整后误导 AI。→ 报文携带实际生效值
   `TTL <n>s`（新单测 `expiry_errors_report_the_tuned_ttl`）。
2. **未知 cursorId 无引导**：只报 `unknown cursorId: …`。→ 追加
   `re-run files_scan_digest to get a fresh cursorId`（与过期同构）。
3. **空 dataBase64 被误拒**：`required_str` 拒绝空串，MCP 无法建空文件，
   与工作台写路径（允许空 payload）不一致。→ files_write 单独校验
   dataBase64（存在且为字符串即可，空串建空文件），schema 描述同步。
4. **必填参数类型错误误报缺失**：`path: 123` 报
   `Missing required parameter: path`，LLM 易原样重发。→ `required_str`
   区分缺失（Missing required parameter）与类型/空串错误
   （`Parameter '<key>' must be a non-empty string`）。
5. **数值参数不接受数字字符串**：谓词参数报错而 `depth`/`n`/`offset`/
   `limit` 静默回退默认值，行为不一致且 `"8"` 的意图被丢弃。→ 统一
   `numeric_arg_u64`：数字 / trim 后数字字符串 / 整型浮点（`8.0`）均接受，
   其余报 `{key} must be a non-negative integer` 并点名参数。
6. **内联连接布尔字符串静默反转意图**（安全相关）：`readOnly: "true"`
   经 `bool_field`（仅 `as_bool`）解析为 false → 意图只读的连接实际可写。
   → `bool_field` 容忍 `"true"/"1"/"yes"/"on"`、`"false"/"0"/"no"/"off"`
   （大小写/空白不敏感），无法解析回退字段默认（model.rs 单测覆盖）。
7. **尾斜杠路径陷阱**（OpenDAL 语义）：`files_write`/`files_rename` 目标
   带尾斜杠报难懂的 NotFound；`files_delete` 文件路径带尾斜杠（LLM 回显
   误拼）preview kind=missing → 确认执行 → success 但文件还在（静默
   no-op）。→ 新增 `normalize_slashes`/`file_target_path`（写/rename 目标
   去尾斜杠、拒绝连接根）/`canonical_delete_target`（delete/purge 执行
   拼写由 preview kind 决定：目录保留一个尾斜杠、文件去除），digest 起点
   path 归一化后回显。
8. **stdio `tools/call` arguments 非对象无清晰报错**：静默按空对象处理。
   → `prepare_arguments` 入口校验，报 `arguments must be a JSON object`
   （与 DBX 桥 mcp/call 同语义）。

### 新增覆盖

- Rust 单测 +8（181→189）：数值字符串/整型浮点容错（filter + cursor）；
  required_str 两类错误；canonical_delete_target / file_target_path 纯函数
  表；TTL 实值报错（cursor+confirm）；未知 cursorId 引导；stdio localFs
  全链路扩展（quick_paths limit="1"、空写磁盘校验、尾斜杠真删、两阶段
  purge 成功路径磁盘校验、readOnly:"true" 拒写、arguments 非对象）；
  model.rs 布尔字符串变体。
- smoke_mcp 15 场景（14→15）：M10 扩展（两阶段 purge 成功 + quick_paths
  limit）；新增 M11 LLM 输入变体（数字字符串谓词、越界 depth clamp、
  空 dataBase64、尾斜杠真删磁盘校验、未知参数容忍、非数字报错点名、
  cursor 未知引导）；S3 扩展（stdio quick_paths）。

### 回归证据

- `cargo test`：189 passed / 0 failed / 3 ignored；`cargo check
  --all-targets` 0 warning。
- `cargo build --release`：Finished（无警告）。
- `DBX_PLUGIN_SIDECAR=<release> python3 scripts/smoke_mcp.py`：
  total=15 PASS=15 FAIL=0 SKIP=0（M1–M11 + S1–S4）。
- `scripts/test.sh` 全绿（后端单测 + 连接表单校验 + 前端
  typecheck/test/build + release 构建 + framed smoke PASS 57 / SKIP 6 /
  FAIL 0（容器段按环境 SKIP）+ MCP smoke 15 PASS + 打包
  io.dbx.files-0.1.50 dbxp）。

### 剩余风险

- 10 万条扫描 clamp、cursor TTL 真实超时（10 分钟）依赖真实大树/长等待，
  单测以纯逻辑等价覆盖（`max_cursor_rows` 截断、手动过期），未起大目录
  实测。
- s3/oss/webdav 等远端协议的 MCP 往返仍只有构建/校验路径（容器未起），
  与上轮记录一致。
- `bool_field` 容忍面收敛在字符串变体；数字 `0/1` 未纳入（JSON 数字布尔
  属罕见变体，避免过度猜测）。
- stdio 宿主 TCP 桥接兜底（ssh L1 同构）仍未做，未注册 connectionId 维持
  引导错误（前轮已记，非本轮范围）。

## MCP 测试覆盖专项第二轮：远端往返 / 深水区实测 / 错误消息走查（2026-09-13）

第一轮遗留的深水区全部落地：远端协议 MCP 真实往返（上轮最大遗留）、
10 万条 clamp 真实大树实测、TTL 真实时钟超时实测、12 工具错误消息全量
走查补引导、同族一致性交叉核对。改动仅 `files/backend/src/mcp.rs`、
`files/scripts/{smoke_mcp.py,container_smoke.sh}` 与两份 docs；host/、
shared/、其他插件只读。

### 问题清单（现象 → 根因 → 修复 → 验证）

1. **stdio 对未知工具名先报 connectionId 引导**（走查新发现）：
   `tools/call {name:"files_nonexistent"}` 经 `prepare_arguments`
   （`needs_connection_id` 白名单不含未知名）先答
   `Missing required parameter: connectionId`——LLM 会被引去补一个
   无意义的连接参数，而非纠正工具名。→ stdio `call_tool` 入口对不在
   `ALL_TOOL_NAMES` 的名字直接返回 `unknown_tool_message`（桥/stdio
   两路同源）；单测 `unknown_tool_suggests_variants_and_lists_discovery`
   钉死该顺序。
2. **`Unknown tool: {other}` 无自纠出路**：对照 ssh 基线
   （`unknown_tool_message`：Did-you-mean + tools/list 指引）与 ldap
   （`available:` 全列）存在引导缺口。→ 新增 `ALL_TOOL_NAMES` 常量 +
   `unknown_tool_message`：分隔符/大小写变体（`FILES-SCANDIGEST`）给
   Did-you-mean，全部报错列出 12 个注册名与发现面（桥 `mcp/tools` /
   stdio `tools/list`）。
3. **`files_scan_digest` 可选字符串参数静默降级**：`path: 123` 被忽略
   后静默按 `/` 扫描（扫错整棵树）、`glob: true`/`format: 7` 静默忽略——
   与第一轮「数值参数静默回退」同型的容错缺口。→ 新增 `optional_str`
   fail-fast：存在但非字符串/空串一律报 `Parameter '<key>' must be a
   non-empty string` 并点名参数；缺省值路径不变（单测
   `digest_rejects_non_string_optional_args_instead_of_silent_defaults`）。
4. **`unknown intentId` 无引导**：→ 补「intent 进程内 60s 过期；重发
   files_ui_* 调用或省略 intentId 读最新快照」（单测
   `unknown_intent_id_error_guides_caller`）。
5. **`Invalid base64 file data` 无格式期望**：→ 补「standard base64
   (RFC 4648, no data-URI prefix)」（单测 `base64_error_names_the_expected_format`）。
6. **allow_delete=false 拒绝无出路**：→ 补「enable allowDelete on the
   connection…」（单测 `allow_delete_refusal_names_the_way_out`）。
7. **M12 首跑即暴露 clamp 精确语义**（实测价值）：断言按
   `scanned==budget` 写，真实行为是 `scanned=100001`（越界探测条目计入
   scanned 以发现截断、不保留进 matched）——修正断言为
   `scanned==min(count,budget+1) / matched==min(count,budget)`，并在
   MCP.zh-CN.md 固化该语义，修正而非放宽。

### 新增覆盖

- Rust 单测 +5（189→194）：未知工具建议/全列/dispatch 顺序、unknown
  intentId 引导、digest 非字符串可选参数 fail-fast、allow_delete 出路、
  base64 格式点名。
- smoke_mcp 20 场景（15→20）：
  - **M12 真实 10 万条 clamp**：扁平 100 001 文件真实目录树
    （`DBX_FILES_MCP_CLAMP_FILES` 可调做参数化等效压测）→
    `scanned=100001/matched=100000/scanTruncated=true`；cursor 物化
    10 万→1 万截断（`cursorTruncated=true`），offset 9999 翻页剩 1 行
    done=true，显式 `offset:10000` 起恒空——「物化上限 1 万条」首次
    在真实大树上验证（此前仅纯逻辑截断单测）。
  - **M13 TTL 真实超时**：`cursorTtlSecs:10`+`confirmTtlSecs:10`（sanitize
    下限）真实等待 10.6s → `cursor expired (TTL 10s)` 与
    `confirmToken expired (TTL 10s)` 在真实时钟上携带实际生效 TTL（第
    一轮为单测注入时钟）；过期 token 未执行删除（digest 复查文件仍在，
    fail-safe）；finally 恢复默认 TTL 不污染后续场景。
  - **R1–R3 远端协议 stdio MCP 真实往返**（上轮最大遗留关闭）：MinIO
    (s3) / mod_dav (webdav) / pyftpdlib (ftp) 真实容器上，`--mcp` stdio
    内联凭据（`mcp-inline-<hash>` 池化）跑 agent 全链路：files_mkdir/
    files_write → files_scan_digest 聚合 → files_cursor_next 翻页 →
    files_delete 两阶段（参数篡改作废→重开预览→确认执行）→ files_purge
    两阶段；删除结果以 re-digest matched 计数校验（MCP 面不读文件正文，
    matched 即真相源），purge 后根下精确剩 1 文件强断言。env 缺失自动
    SKIP（硬性规则 5）。
- `container_smoke.sh` 末尾新增 MCP smoke 段：复用同一份运行时随机凭据
  env，七协议容器下 MCP R 段从 SKIP 翻成真跑（本机实跑 R1/R2/R3 全
  PASS）。

### 回归证据

- `cargo test`：194 passed / 0 failed / 3 ignored。
- `cargo build --release`：Finished（无警告）。
- `DBX_PLUGIN_SIDECAR=<release> python3 scripts/smoke_mcp.py`（无容器）：
  total=20 PASS=17 FAIL=0 SKIP=3（R1–R3 按设计 SKIP）。
- `DBX_PLUGIN_SIDECAR=<release> bash scripts/container_smoke.sh`（七协议
  容器全起）：framed smoke PASS 186 / SKIP 0 / FAIL 0 + MCP smoke
  total=20 PASS=20 FAIL=0 SKIP=0（R1 s3 / R2 webdav / R3 ftp 真跑）。
- `bash scripts/test.sh` 全绿（后端单测 + 连接表单校验 + 前端
  typecheck/test/build + release 构建 + framed smoke + MCP smoke +
  打包 io.dbx.files-0.1.50 dbxp）。

### 同族一致性交叉核对（files 只读对照 ssh/ldap/kafka）

- **settings 字段表**：files `McpSettings` 与 ldap `settingsFields` 字段
  名/默认值/上限逐项一致（reportWaitMs 1–30000、cellWidth 1–2000、
  digestGroupLimit 1–20、digestTopN 1–10、digestSampleRows 1–5、
  digestRowLimit 1–20、responseLimitBytes 1KiB–1MiB、maxCursorRows
  100–100000、cursorTtlSecs 10–3600、maxCursorSessions 1–32、
  confirmTtlSecs 10–600）——无漂移，files 侧无需改动。
- **digest/cursor/两阶段形状**：`{matched, scanned, scanTruncated,
  cursorId}`、`{rows, offset, nextOffset, done}`、
  `{preview, confirmToken, expiresAt, note}` 三家同构（kafka digest
  stats 段为领域特化属设计内差异）。
- **发现的别家形状漂移（只报告不动）**：
  1. `ldap/backend/internal/mcp/server.go:407`：cursor 过期消息仍写死
     `cursor expired (10 minutes)`——files 第一轮已改实值 TTL 报文
     （`TTL <n>s`），ldap 未跟进；`cursorTtlSecs` 可调后同样误导。
  2. `ldap/backend/internal/mcp/server.go:271`：`unknown intentId: %s`
     无引导（files 本轮已补 60s/重发/省略 intentId 三要素）。
  3. `kafka/backend/internal/mcp/server.go:150`：`unknown tool: %s` 无
     自纠提示——ssh（Did-you-mean + tools/list）、ldap（available 全
     列）、files（本轮对齐）均有引导，kafka 是唯一缺口。

### 剩余风险

- oss/smb/sftp/sftp-native 协议的 MCP R 段未单独建场景（container_smoke
  已提供全部 env，`run_remote_stdio_roundtrip` 单函数可复用，按需加
  R4+ 即可）；本轮按任务要求覆盖 3 个协议（s3/webdav/ftp）。
- M12 真实大树使无容器 smoke 增加约 10–15s（文件生成 + 双次 digest），
  可用 `DBX_FILES_MCP_CLAMP_FILES` 调小做参数化等效压测。
- M13 真实等待固定 +10.6s（TTL 下限 10s 所限，属确定性成本）。
- stdio 宿主 TCP 桥接兜底（ssh L1 同构）仍未做（前轮已记，非本轮范围）。

## MCP 专项第三轮（收敛轮）：stdio 桥接兜底 + 错误码族内统一（2026-09-13）

### 桥接兜底（L1，ssh L1 / ldap M14 同构）

- **`mcp.rs` 内新增 `mod appbridge`**（参照 ssh `app_bridge.rs` 结构、
  ldap `appbridge.go` 同族，模块化放置不外开新文件）：端口发现
  （`<app_data_dir>/mcp-bridge-port`，`DBX_APP_DATA_DIR` → macOS 默认
  app-data）、十进制空白容忍解析、TCP connect 探测防陈旧端口、
  `DBX_APP_LAUNCH_CMD`（缺省 `open -a DBX.app`）尽力拉起 + 500ms 轮询
  ensure（预算 30s）、`POST /call-plugin-tool` snake_case 五字段契约
  （`plugin_id=io.dbx.files`）、64 KiB 单写上限、手写 HTTP/1.1（零新
  依赖）、全部失败路径带 "DBX app bridge" 统一前缀（fail-closed，不假
  死不静默）。
- **stdio 转发集成**（`StdioServer` 新增 `bridge_fallback` 开关 + 
  `bridge_ensure_wait` 预算字段，`bridge_forward_plan` 纯决策 +
  `forward_via_bridge` 执行）：仅"需要连接"的工具且显式 `connectionId`
  未池化（非 `__local__`、非内联 `connection` 在场）才转发——
  `files_cursor_next` 会话类、UI 类（本就 UNAVAILABLE）、内联凭据调用
  均本地路径；转发参数原样、超时固定 300s（files 工具无逐调用超时参
  数，读余量 +150s）、200 envelope 逐字透传、非 envelope 防御性按成功
  content 包装、转发不污染本地池。
- **fail-closed 合并引导错误（files 特化）**：桥失败时以
  `Unknown connectionId '<id>'` 开头，携带桥失败原因与三条出路，内联
  参数出路点名 files 自己的字段（`protocol/root/bucket/endpoint/region/
  accessKeyId/secretAccessKey`）；fallback 关闭（测试 hermetic）时同一
  消息形状注明会话内禁用。替换原"桥接兜底未实现"的临时文案。

### 错误码族内统一（stdio JSON-RPC 分档）

- **未知方法 -32000 → -32601**（ssh 本轮同步改、ldap/kafka 原生
  -32601，四插件 stdio 对齐）；`tools/call` 缺工具名与 arguments 非对
  象两类结构性校验升 **-32602**（ldap/kafka Invalid params 分档同构，
  消息保留 "tool name" / "arguments must be a JSON object" 关键字）；
  工具级/应用级错误维持 **-32000**（-32700 parse / -32600 无 id 不
  变）。smoke SKIP 判定认 "Method not found" 文本与数字码解耦（M9 无
  需改动），S1 改钉 -32601/-32602/-32000 三档。

### 测试

- Rust 单测 +11（194→205）：端口解析/垃圾拒绝、五字段契约、HTTP 响应
  拆分、ensure 未发布 fail-closed（短预算限时）、ensure 已发布立即返
  回、mock 桥转发契约+envelope 逐字+非 200/非法 JSON、forward_plan 六
  决策分支、stdio mock 桥转发端到端、非 envelope 包装、桥不可达
  fail-closed 引导（含 files 字段点名）；mock 宿主桥为进程内 std
  TcpListener（探测连接即空数据跳过），env 变更用模块级锁串行化。
- smoke +1 场景（20→21）：**B1 桥接兜底段**（ssh 场景 8/12 + ldap M14
  同构）——①桥未发布（空 app-data + `:` no-op launch）30s 唤醒预算后
  fail-closed 断言四要素；②进程内 MockBridge 转发契约（path/五字段/
  plugin_id）+ envelope 逐字；③桥 404 透出 "DBX app bridge returned
  HTTP 404"。S3 的未知 id 断言迁移至 B1（避免常规段触发 30s 唤醒预
  算）；`StdioSession` 支持 `extra_env`。

### 回归证据

- `cd backend && cargo test`：205 passed / 0 failed / 3 ignored（测试
  构建零警告；顺手修掉第二轮遗留的 1 处 `unused_mut`）。
- `cargo build --release --manifest-path backend/Cargo.toml`：Finished，
  0 警告。
- `DBX_PLUGIN_SIDECAR=… python3 scripts/smoke_mcp.py`（无容器）：
  total=21 PASS=18 FAIL=0 SKIP=3（R1–R3 按设计 SKIP，B1 PASS）。
- `bash scripts/test.sh`：all green（后端单测 + 连接表单校验 + 前端
  typecheck/test/build + release 构建 + framed smoke + MCP smoke +
  打包 io.dbx.files-0.1.50 dbxp）。

### 剩余风险

- B1 场景①固定 +30s（与 ssh 场景 8 / ldap M14 同为"唤醒预算即被测行
  为"的确定性成本）；无容器 smoke 总时长相应增加。
- 真机对真 DBX.app 的桥转发（saved connection 端到端）与
  `/list-plugin-connections` 路由（files 无连接名选择器，本轮未移植，
  与 ldap 一致）未覆盖——前者依赖真机 DBX.app 运行态，契约已由 mock
  段钉死；后续如需连接名/endpoint 寻址再引入 list 路由。
- 转发超时固定 300s（files 工具无逐调用超时参数；应用侧长 digest 由
  读余量 +150s 兜底），未来若引入工具级超时参数需同步转发超时 clamp。

## MCP 专项第四轮（实施轮）：oss/smb/sftp/sftp-native 远端 R 段补齐（2026-09-13）

第三轮遗留「R 段只有 s3/webdav/ftp」本轮收口：smoke_mcp.py 场景
R1–R3 → R1–R7（25 场景），七种远端协议全部有 stdio MCP 真实往返
场景；oss 无 OSS API 兼容容器（MinIO 只讲 S3 API，OpenDAL oss
service 讲阿里云 API），按任务要求常驻 SKIP 并如实记录原因，env
钩子（`DBX_FILES_OSS_ENDPOINT/BUCKET/ACCESS_KEY/SECRET_KEY`）保留，
对真实 OSS endpoint 设 env 即可启用（secret 经 s3 形状绑定落到
OpenDAL `access_key_secret` 键，这正是 R7 启用后要钉的特化点）。

### 新增场景与协议特化点

- **R4 sftp（OpenSSH 容器）**：keyfile-only 认证全链路——内联 payload
  不带 password（OpenDAL 0.57 sftp service 无 password 选项，engine
  也从不转发）；`ssh://user@host:port` URI endpoint（openssh crate 只
  从 URI 解析 user/port）+ `knownHostsStrategy: "accept"` 容忍容器
  临时主机密钥。
- **R5 smb（Samba 容器）**：裸 `host:port` endpoint + share 限定 +
  username/password（password 走 secret 绑定）；共享循环的两阶段
  purge 首次在 stdio 面钉住自研适配器实现的 delete_with_recursive
  （smb2 crate 无服务端递归删除）。
- **R6 sftp-native（russh 双栈，同一 OpenSSH 容器）**：password 认证
  全链路——该协议存在的理由（OpenDAL sftp service 做不到 password）；
  裸 `user@host:port` endpoint，默认主机密钥策略 Tolerate（accept-new）
  适配临时密钥；KEY 环境备选与 framed 段对齐。
- **R7 oss**：env 门控 SKIP（见上），场景结构与 R1–R3 同构。

### 真实缺陷发现与修复（R4/R6 首跑）

现象：直接复用 R1–R3 的 `root=/mcp-smoke-<uuid>` 连接隔离法，R4 报
`files_write 'NotFound (permanent) … NoSuchFile'`、R6 报
`files_mkdir 'PermissionDenied'`，framed 面同协议全 PASS——纯 stdio
新增形状踩坑。

根因（两层）：
1. 场景层：`root` 指向 sftpuser 无权创建的文件系统 `/` 下；受限 home
   服务器上「root 隔离法」前提（对象存储/webdav/ftp 的 root 可随意
   自物化）不成立。
2. 上游层（记录不 patch）：OpenDAL 0.57 sftp service 的 root 自动创建
   循环用 `is_sftp_protocol_error` 判「已存在即忽略」，而该函数对
   **任意**协议错误码（含 PermissionDenied）返回 true——root 建失败
   被静默吞掉，错误延迟到写路径 `canonicalize`（SSH_FXP_REALPATH）
   才以 `NoSuchFile` 透出，误导性强（错误点与根因相距两层）。自研
   sftp-native 适配器的 stat-first mkdir -p 则诚实报出 PermissionDenied
   （行为正确）。`connection/test` 的 `check()`（root stat）可提前暴露
   该类配置错误；上游是否收紧错误分类记为观察项，本仓不 vendor
   patch OpenDAL。

修复（场景侧，对齐 framed smoke 已验证形状）：共享循环
`run_remote_stdio_roundtrip` 改为「唯一可写 base 目录」建树——
`files_mkdir` 在各后端都是 mkdir -p，base 自物化，不依赖任何协议的
root 自动创建；sftp/sftp-native 的 base 落在 `/config`（容器用户
home）。R1–R3 同步迁移（行为等价，容器下复验通过）。可行动结论：
sftp/sftp-native 连接的 root 应指向已存在或父目录可写的路径。

### 回归证据

- `cd backend && cargo test`：205 passed / 0 failed / 3 ignored
  （0 warning）。
- `cargo build --release --manifest-path backend/Cargo.toml`：Finished，
  0 警告。
- `DBX_PLUGIN_SIDECAR=… python3 scripts/smoke_mcp.py`（无容器）：
  total=25 PASS=18 FAIL=0 SKIP=7（R1–R7 按设计 SKIP）。
- `DBX_PLUGIN_SIDECAR=… bash scripts/container_smoke.sh`（七协议容器
  全起）：framed smoke PASS 186 / SKIP 0 / FAIL 0 + MCP smoke
  total=25 PASS=24 FAIL=0 SKIP=1（R1–R6 真跑、R7 按设计 SKIP）。
- `bash scripts/test.sh`：all green（后端单测 + 连接表单校验 + 前端
  typecheck/test/build + release 构建 + framed smoke + MCP smoke +
  打包 io.dbx.files-0.1.54 dbxp）。

### 剩余风险

- R7 oss 常驻 SKIP：真实 OSS endpoint 往返仍未验证（无 OSS API 兼容
  容器可用；如后续引入 oss emulator 容器可把 SKIP 翻成真跑）。
- R1–R3 迁移到 base 目录建树后行为等价但路径形状变了（`/mcp-smoke-*`
  在协议默认根下而非 root 前缀下）——依赖旧路径形状的外部脚本（如有）
  需知悉。
- OpenDAL 0.57 sftp service root 自动创建吞 PermissionDenied 的上游
  行为仍在（connection/test 可提前暴露；升级 0.58+ 时复查该循环是否
  收紧）。
- stdio 内联 sftp 的 key 字段只支持路径/内容二选一经 connection
  secrets 传递（与工作台同源），key 内容含敏感信息的转义/长度上限未
  专项压测（容器跑用的是路径形态）。

## 第四轮（2026-09-13）桥回环：真机 DBX.app 端到端验证

（接上节遗留项①）本机隔离 app-data（`shared/host-e2e/app-data`）+
测试 DBX.app（host debug bundle，经 launch.sh 注入 `DBX_DATA_DIR`），
files 0.1.54 重打并随四插件装入同一 app-data；桥端口发布后 TCP 探测
通过。

- **转发契约（核心验收）**：standalone `dbx-plugin-files --mcp`
  （`DBX_APP_DATA_DIR` 指向隔离 app-data）`tools/call files_scan_digest
  {connectionId:"no-such-files-conn", path:"/"}` → 未池化 connectionId
  经桥转发 → 宿主 resolve_connection 404
  `{"error":"Connection with id 'no-such-files-conn' not found"}` 并入
  引导错误（files 的「Ways out」内联出路文案）——转发路径端到端通，
  上一节遗留项①的「真机对真 DBX.app 的桥转发」闭环。
- **fail-closed 对照**：同调用换空 `DBX_APP_DATA_DIR` → `DBX app bridge
  unreachable after 30s` 本地 fail-closed，与「app 侧 returned HTTP
  404」明确区分。
- **app-data 内无 files 保存连接**（仅 ssh 的 vagrant 固件），「转发 →
  宿主 → files workbench sidecar → 真实结果」全链路未覆盖，待后续
  seed 一个 localFs/s3 连接后补。
- **沉淀**：`shared/host-e2e/mcp_bridge_e2e.sh`（四插件统一回环脚本，
  本轮真机 4/4 全绿；files 探针为其中一环）。
- 本轮插件源码只读，未改代码。

**剩余风险**：全链路真实结果层待 seed 连接补齐；偶发观察到跨插件转发
调用偶发 90s 无响应、单独重跑立即成功（疑似宿主侧/GUI 渲染竞态）。

## MCP 专项第五轮（可靠性纵深轮）：stdio 传输层 / 存储 churn / 路径门对抗（2026-09-13）

三类主题（stdio 传输层健壮性、会话/存储长期 churn、路径门对抗输入），
发现问题立即修复、立即回归。只动 `files/backend/src/{mcp,policy}.rs`、
`files/scripts/smoke_mcp.py`、`files/docs/MCP.zh-CN.md`；host/、shared/
与其他插件未触碰。

### 问题清单（现象 → 根因 → 修复 → 验证）

1. **非法 UTF-8 字节流杀死 stdio 会话**：`run_mcp_stdio` 用
   `BufRead::lines()` 读行，其 UTF-8 校验错误经 `line?` 直接传播出主
   函数 → 进程退出（合法客户端不会发，但对抗/故障输入一个坏行就终结
   整个会话）。→ 改 `read_until(b'\n')` + `from_utf8_lossy`：坏字节行
   lossy 解码后解析失败 → `-32700`（null id），连接继续。smoke T1
   端到端验证（`BufRead::lines()` 语义保真对照：ssh 同构实现仍在，
   族内建议已记「剩余风险」）。
2. **超长行无界读取**：读行无上限，恶意/故障客户端可拖无限行打爆
   内存。→ 单行上限 16 MiB（`DBX_FILES_MCP_STDIO_MAX_LINE` 可调；
   8 MiB 参数 base64 后 ≈10.7 MiB 行仍在限内），超限在读取层回
   `-32700` 点名上限与 env，连接继续。smoke T4 双路径验证（8 MiB
   参数走 4 MiB 写上限的 -32000；调小 env 后超限行 -32700）。
3. **confirm 令牌表无界增长**：`confirms: HashMap` 只在 verify 时
   remove 单条，TTL 只在消费时检查——大量签发 preview 而从不 confirm
   的调用让表单调增长直至进程结束。→ `confirm_begin` 先 prune 过期
   条目，再加 1024 硬上限（超限淘汰**最早过期**者：最接近自然到期、
   最不可能是调用方正持有的 token）。单测：1200 次 churn 后表恒
   ≤1024 且全未过期、过期条目下次签发即清、上限淘汰语义逐项断言、
   TTL 调整不追溯（旧 token 保留原窗口可用）。
4. **`maxCursorSessions` settings 对 LRU 容量不生效**：`Mcp::new` 把
   LRU cap 冻结为默认 8，`mcp/settings/set maxCursorSessions`（1–32）
   只写进 settings，实际容量纹丝不动。→ `LruTable::set_cap`（缩容
   立即驱逐溢出），`cursor_put` 每次按当前 settings 执行。单测：
   2 会话下 LRU 逐出最旧、活跃会话（touch）不被误逐、8→1 缩容下一
   次物化即收敛到 1。
5. **MCP 面路径不过形状门**：framed 面有 `policy.sanitize`，MCP 面
   的 write/mkdir/rename/delete/purge/digest 直通 OpenDAL——`..` 段
   （`/..`、`a/../../etc`）依赖各后端自行拒绝、`/.` 语义上等价连接根
   却能绕过 `refuse_root_purge` 的 trim 判定、控制字符（JSON
   `\u0000`）与 >4096 路径透传 OS 层怪错。→ 新增
   `validate_path_shape`（`..`/`.` 段、控制字符、4096 字节上限）接入
   全部路径工具。单测：对抗拼写表 + purge/delete 根红线对抗（`/`、
   `//`、`/.`、`/..`、空白等全拒绝且磁盘树完好）；smoke/Rust 双面
   钉 `~` 不展开、URL 编码不解码、NFC/NFD 不归一（设计内保守行为，
   只寻址字面同名条目）。
6. **缺 method / 非法 id 类型无结构化分档**：缺 method 落入
   "Method not found: "（-32601），id 为 object 被原样回显——均与
   `shared/MCP_ACCEPTANCE.zh-CN.md` §2 的 -32600 invalid request 档
   不齐。→ dispatch 前置校验：缺失/非字符串 method → `-32600
   Invalid request: missing method`；id ∉ {string, number, null} →
   `-32600 Invalid request: id must be a string, number, or null`
   （id 不回显）。`jsonrpc` 版本字段**宽容不校验**（ssh/ldap/kafka
   同形状，真实客户端可能省略/变体；拒绝无行为收益），钉测试存档。
   smoke T2/T3 验证且每条后健康探针。
7. **smoke T1 自身缺陷（首跑暴露）**：二进制噪声行
   `bytes(range(1,32))` 内含 `\n`(10)——一次 send 实际产生两行、两份
   -32700，只读一份，残留响应污染管线 → 后续场景 recv/read 互等
   死锁（python 卡 readline、sidecar 卡 read，sample 栈定位）。→
   噪声字节集排除 `\n`/`\r`（恰好一行、响应数可预期）；T5 顺带改为
   收集式断言（响应乱序不假设先到顺序）。重跑全绿。

### 新增覆盖

- Rust 单测 +17（205→222）：stdio 结构校验 -32600（缺 method/非标量
  id/合法 id 三形状/无 id 非 notification）、jsonrpc 宽容钉测试、
  parse_request_line 空白行表、行上限 env 解析、confirm churn+上限
  淘汰+TTL 不追溯（2 例）、cursor churn LRU+settings 即时生效、
  物化上限 churn 循环（100×1 配置 50 轮）、intent churn+快照
  last-write-wins ×100、digest 幂等 ×100、validate_path_shape 对抗
  表、file_target_path 组合门、purge/delete 根红线对抗拼写（端到端
  磁盘校验）、`~`/`%2e%2e`/NFC-NFD 字面语义（端到端）；policy.rs
  对抗输入表（拒绝 vs 字面透传 + `.` 段 purge 根红线）。
- smoke_mcp 25→31 场景（新增 T1–T6，无外部依赖必跑不 SKIP）：
  T1 垃圾行/非法 UTF-8 进程存活、T2 通知静默+无 id -32600、T3 结构
  校验+jsonrpc 宽容、T4 超长行双路径、T5 pipelining 4 id+垃圾行
  逐一对应、T6 空行/CRLF。每条对抗断言后跟合法请求健康探针（进程
  poll is None + ping 成功）。

### 回归证据

- `cd backend && cargo test`：**222 passed / 0 failed / 3 ignored**
  （0 warning）。
- `cargo build --release --manifest-path backend/Cargo.toml`：
  Finished，0 警告。
- `DBX_PLUGIN_SIDECAR=… python3 scripts/smoke_mcp.py`（无容器）：
  **total=31 PASS=24 FAIL=0 SKIP=7**（R1–R7 按设计 SKIP，T1–T6 全
  PASS）。
- `bash scripts/test.sh`：**all green**（后端单测 + 连接表单 18 组合
  + 前端 typecheck/test/build + release 构建 + framed smoke + MCP
  smoke 31 场景 + 打包 io.dbx.files-0.1.55 dbxp）。

### 并行会话冲突记录（本轮真实发生）

回归中途 `files/backend/src/mcp.rs` 被另一并行会话（stdio tools/list
schema 增强方向：连接类工具显式声明内联 `connection` 参数 + required
connectionId 放宽 anyOf）多次写入，其中两次中间态造成编译断裂
（`stdio_tool_list` 调用先于定义、`const … = json!` 非法——后者由该
会话自行收敛为 `fn`）。本会话处置：等待写入稳定（mtime 窗口）后在其
最终态上回归，未回退对方改动；T5 段对方亦改进为收集式断言（保留）。
最终 222 全绿为两方改动共存态。

### 剩余风险

- **族内沿袭（只报告不动）**：ssh `run_mcp_stdio` 同用 `lines()`，
  非法 UTF-8 同样会杀会话；ldap ConfirmStore 同样无签发期 prune/
  上限（Go json.Unmarshal 对无效 UTF-8 宽容，传输层不受影响）；
  files 的 `-32600` 结构校验（缺 method/非标量 id）与 confirm 上限
  是族内首例，`MCP_ACCEPTANCE` §2 的 -32600「无 id 等」语义已覆盖，
  其余三家跟进与否由各自轮次决策。
- **`std_tool_list` 增强的 schema 断言**（并行会话新增）与本轮 T 段
  共存于 smoke_mcp.py，后续轮次改任一侧时需对侧回归。
- 行上限 16 MiB 是读取层防线而非协议约定；MCP 宿主若引入自己的
  请求大小上限（通常更小），超出部分在宿主侧先被拒。
- `/.` 段 MCP 面拒绝 vs framed 面吞并的双面语义已文档化
  （MCP.zh-CN.md「路径形状门」），不改 framed 面既有行为。

### 2026-09-14 ZCode MCP 接入与真机 agent 调用测试（MCP 集成会话）

- **ZCode 接入**：用户级 `~/.zcode/cli/config.json` → `mcp.servers` 新增
  `dbx-files`（`backend/target/release/dbx-plugin-files --mcp` +
  `DBX_PLUGIN_DATA_DIR=~/.dbx-plugin-data/io.dbx.files`，镜像 dbx-ssh
  条目形状；ldap/kafka 同轮接入）。会话重启后自动连接。
- **stdio 错误形状对齐**（本会话实现，见上文冲突记录的共存态）：
  `tools/call` 工具级错误改 MCP `isError:true` content（原 -32000 协议
  错误为族内旧约定，ldap/kafka stdio 均已按 MCP 规约用 isError；结构性
  -32602 分档保持）。`mcp.rs` 单测 + smoke S1/S2/T4 断言同步更新。
- **stdio tools/list 内联 schema**：`stdio_tool_list` 为连接类工具声明
  `connection` 对象（`fn inline_connection_properties` 全 camelCase 键）
  并把 required `connectionId` 放宽 anyOf（与并行会话的同名增强为同一
  改动的两个会话视角，最终态见 mcp.rs）。
- **真机 agent 调用测试**：`__local__`/内联 localFs 全链路（digest/rows/
  cursor/write/mkdir/rename/两阶段 delete+purge/根拒绝/UI UNAVAILABLE/
  缺连接指引/非法 base64）实测通过；改进后复验 isError 与 schema 声明
  生效。smoke 终态 **total=31 PASS=24 FAIL=0 SKIP=7**。
- **smoke 客户端**：`StdioSession.wait_for` 乱序帧缓冲化（不丢弃）+
  `os.read` 自管行缓冲（与 ldap M18/kafka K17 同源修复）；T5 改收集式
  断言。
- **引导 skill**：新增 `.agents/skills/dbx-mcp-usage`（agent 使用指引：
  服务器配置、内联连接、两阶段写、常见坑速查），AGENTS.md skill 表加行。

## MCP 专项第七轮（并发安全与口径拉齐轮，2026-09-13）

三个任务：并发 store 压力测试（Rust 侧等价 Go -race 轮）、缺参报错枚举
式拉齐（对齐 ssh，契约表 §3.9）、enum 报错在线冒烟补强（§3.3 钉进容器段）。

### 任务一：并发 store 压力测试

**锁用法审查结论（mcp.rs confirm 表 / cursor LRU 表 / intent 表）——无真
竞态/逻辑缺陷，测试 + 注释钉死**：

- `confirm_verify` 的「消费即原子」：remove 发生在锁内，hash/TTL 校验在
  已移除的条目上进行——并发双 verify 同一 token 恰有一次 Ok，输家见
  "unknown or already used"，无 verify-then-remove 窗口。
- `confirm_begin` 的 prune → 上限淘汰 → insert 全在同一临界区，并发签发
  不会越过 1024 硬上限（无跨锁 check-then-act）。已注释钉死：同毫秒签发
  的 token 共享 `expires_at_millis`，淘汰为「文档化的任选」——被逐方重开
  预览即可，无双重写入。
- `cursor_next` 的 lookup → 过期判定 → offset 读/推进全程单临界区，并发
  翻同一 cursor 不可能观察到（或返回）同一 offset 窗口。
- `cursor_put` 的 settings 读取与表锁是两个临界区：与 `settings_set` 竞争
  时单次物化可能用旧 `maxCursorSessions`——但每次 put 都按新读到的值
  `set_cap`，语义是「调参后下一次物化收敛」（第六轮既有契约），注释明确
  为「收敛而非原子」。锁序唯一嵌套点 cursor_next（cursors→settings）无
  反向嵌套，无死锁面。
- intent 表 register/report/lookup 各自单临界区；LruTable::get 的
  remove+push touch 在锁内，活跃会话并发 touch 不失踪。

**新增并发压力测试（std::thread + Arc，3 个）**：

- `confirm_table_survives_concurrent_churn_without_double_consumption`：
  8 线程 × 250 轮 = 2000+ 次签发（超 1024 硬上限，淘汰参与）混
  preview/立即消费/持有后消费。断言：一次性（成功消费后重放必败）、
  被逐 token 只允许明确 unknown（绝不出现 hash/expiry 类错误）、表容量
  收敛 ≤1024、无过期残留。
- `cursor_table_survives_concurrent_materialize_page_evict`：阶段 A
  物化驻留 2 条（< cap 4）+ 5 线程 × 1000 高并发 touch——活跃会话全程
  必活、翻页窗口恒定；阶段 B 8 线程 × 100 唯一会话超压物化 + 4 线程并发
  翻同一会话——成功翻页不重叠（offset 推进原子）、路径集合不越界、表
  收敛 ≤cap、无过期残留。
- `stdio_concurrent_dispatch_keeps_id_correlation`：stdio dispatch 是
  spawn **并发**（`run_mcp_stdio` 每行一个 tokio task，"Spawned handlers
  may finish out of order" 注释既有）；16 个并发 dispatch 唯一 id，响应
  id 一一对应。pipelining 线上断言已有 T5，本轮补进程内并发面。

**测试过程中的一次自纠错**：cursor 阶段 A 初版把 churn 设计成反复 put
同一路径——实际 `cursor_put` 每次物化都是**新 UUID 会话**（无 upsert），
800 个唯一会话把 active 挤出属正确 LRU 行为而非缺陷；用最小复现探针
（临时 mod，验证后删除）确认驱逐路径后改为「有限物化 + 高并发 touch」
设计。

### 任务二：缺参报错枚举式拉齐（§3.9）

- 新增 `missing_required`（ssh 同款）：absent/null 视为缺失，一次报
  `Missing required parameters: a, b`（按 schema required 顺序）；保留
  "Missing required" 关键词兼容 smoke 匹配。
- **口径决策**：枚举范围 = 工具业务参数（stdio schema `anyOf` 放宽后的
  required 集合，即去 `connectionId`）；`connectionId` 缺失保持单数
  `Missing required parameter: connectionId` + 内联指引——既与 stdio
  schema required 一致（核对器 B1 多报即 DRIFT 的约束），也与 §3.7
  「连接参数门先于业务参数校验」的登记顺序一致（核对器归一 WARN）。
- 接入 8 处：ui_focus(panel)/ui_search(path)/ui_select(path)/
  cursor_next(cursorId)/write(path,dataBase64)/mkdir(path)/
  rename(path,newPath)/delete|purge(path)。present-but-类型错误不混入
  枚举，仍由 `Parameter '<key>' must be ...` 精确单独点名。
- 单测 2 个：纯函数枚举表（全缺/部分缺/null 同罪/空串豁免
  dataBase64=""、类型错误不混入）+ stdio dispatch 穿透（空参先连接门
  WARN 口径、内联连接在场时业务枚举达、rename 单点、类型错单独点名）；
  smoke M11/S4 各补断言。

### 任务三：enum 报错在线冒烟补强（§3.3）

- `normalized_format` 纯函数（case-insensitive + trim）：`"ROWS"` /
  `" Rows "` / `"Digest"` 归一工作；非法值报
  `format must be 'digest' or 'rows' (got '<原值>')` 列合法值，不静默
  降级。`depth` 非数字报错附合法范围 `depth accepts an integer in
  1..=16`（单测 `depth_error_spells_the_legal_range_inline` 在真实
  localFs 连接上断言）。
- `run_remote_stdio_roundtrip`（R1–R6 共享）补在线 enum 段：`format:
  "bogus"` / `depth: "bogus"` 真实连接上报错列合法值 + `format: "ROWS"`
  归一后 rows 形状正常返回。无容器时随 R 场景 SKIP（合规）。
- **容器实测**：MinIO + OpenSSH 临时容器（凭据运行时随机、跑完即焚）
  下 R1（s3）+ R4（sftp）真实往返含新 enum 断言 PASS。

### 新增覆盖

- Rust 单测 +7（222→229）：并发 confirm churn、并发 cursor churn、
  并发 dispatch id 对应、missing_required 枚举表、missing_required
  dispatch 穿透、normalized_format、depth 范围报错。
- smoke_mcp：M11（depth 范围 + 缺参枚举两条）、S4（stdio 缺参枚举）
  断言增强；R 段共享 roundtrip 在线 enum 探针段。

### 回归证据

- `cd backend && cargo test`：**229 passed / 0 failed / 3 ignored**。
- `cargo build --release --manifest-path backend/Cargo.toml`：
  Finished，0 警告。
- `DBX_PLUGIN_SIDECAR=… python3 scripts/smoke_mcp.py`（无容器）：
  **total=31 PASS=24 FAIL=0 SKIP=7**。
- 容器段（MinIO+OpenSSH）：**total=31 PASS=26 FAIL=0 SKIP=5**
  （R1/R4 全链路含在线 enum 断言 PASS）。
- `python3 shared/mcp_schema_check.py --binary backend/target/release/
  dbx-plugin-files`：**RESULT: CLEAN**（12 工具；18 WARN 为既有连接门
  /门禁归一，与第六轮一致）。
- `bash scripts/test.sh`：all green（见下）。

### 剩余风险

- confirm 上限淘汰在同毫秒并发签发下是「任选淘汰」语义（1024 容量 +
  TTL 60s 下概率极低），被逐方收到明确 unknown 指引重开预览，无数据
  损坏面；族内 ssh/ldap/kafka 的同位表语义由各自轮次决策是否跟进。
- files 的 stdio schema `anyOf` 放宽（connectionId 出 required）与缺参
  枚举「业务参数口径」互相咬合：后续若把 `connectionId` 拉回 required
  或改变 anyOf 形状，需同步 `missing_required` 口径与核对器 B1 比对
  （§3.7 张力随之消解或再登记）。
- R2/R3/R5/R6/R7 在本轮容器窗口外，enum 在线断言由共享 roundtrip
  函数覆盖，待下次全容器冒烟自然带跑。

## 第八轮（2026-09-14）终验：全容器组合冒烟 + MCP 后性能基线

MCP 专项收口轮：第七轮代码之后首次完整全容器组合验证（framed + MCP
local/R 段），并采集 MCP 专项七轮改动后的性能基线。本插件源码本轮只读。

### 全容器冒烟（`bash scripts/container_smoke.sh`）

- 首跑 FTP 段 FAIL（pyftpdlib 容器未就绪、启动即退）：手动最小复现
  （同镜像同挂载同启动命令）容器正常拉起、pyftpdlib 正常监听，宿主侧
  2121/30100-30109 端口绑定无冲突——判定为 pip 联网安装/容器时序的瞬时
  环境问题，非代码问题，无法稳定复现。
- 重跑全绿：framed smoke **PASS 186 / SKIP 0 / FAIL 0**；MCP smoke
  **total=31 PASS=30 FAIL=0 SKIP=1**，唯一 SKIP=R7（oss 按设计 env-gated）。
- **第七轮遗留闭环**：R1–R6 全 PASS，且 R2/R3/R5/R6 的 enum 在线断言
  （bogus depth/format 拒绝并列合法值、`ROWS` 归一化）随全容器冒烟自然
  带跑全部通过——上一节「待下次全容器冒烟自然带跑」风险消除（R7 仍按
  设计 env-gated）。

### 性能基线（`scripts/perf_files_test.py`，release 二进制，协议层口径）

| 项 | 实测（2026-09-14） |
|---|---|
| ingest 10k 条目（memory） | 0.27s（约 37k ops/s） |
| listPaged 全扫 10k 条（500/页） | 165ms |
| 50 MiB upload（memory） | 1919.3 MiB/s |
| 50 MiB download（memory） | 1069.6 MiB/s |
| 50 MiB upload（fs 真盘） | 478.7 MiB/s |
| 50 MiB download（fs 真盘） | 1131.9 MiB/s |
| 进度事件节流 | up/down 各 92 事件（200 帧上界，~200ms 地板） |

对比 §4（2026-08-29 cargo bench，内部层口径）：两者口径不同层（协议
roundtrip vs Rust 内部），不做直接数值对齐；协议层数值同数量级、无一个
数量级级别的回退。**以本表作为 MCP 专项改动后的首个协议层基线存档**
（采集时本机有并行 cargo 构建负载，数据偏保守）。

## 第九轮（2026-09-15）i18n 清理：冗余 key 删除、失准译文更正、manifest 顶层描述补翻

多语言专项（只动 `frontend/src/lib/i18n.ts` 与 `manifest.json` 文案，零逻辑改动）。

### 冗余 key 删除（23 键 × 七语，共 161 行）

审计脚本（临时 vitest spec，跑完即删）做三件事：① 以 `messages.en` 的叶子 key
为准，全前端源码（.vue/.ts，排除 spec 与 i18n.ts 自身）静态字符串扫描，动态
key 家族（`sessionStatus.*`/`transferStatus.*`/`transferKind.*`，模板字面量
拼接）整族视为可达；② 模板硬编码属性字面量（title/placeholder/aria-label/alt）
与文本节点扫描——零发现（唯一命中 `&nbsp;*` 为必填星号）；③ manifest 七语
localizations 结构路径对齐——完全一致。

23 个无引用 key 全部逐一 grep 复核后删除（后端/scripts/spec 均无引用）：
`confirmTitle`（openConfirm 调用点全部显式传 title）、`connectionUnknown`、
`intent.rejected`（拒绝原因只回报 MCP 调用方，UI 不展示）、`notConnected`、
`previewBinary`（二进制走 hex dump，兜底用 `previewUnsupported`）、`purgeTitle/
purgeBody`（删除弹层实际用 `deleteTitle/deleteBody + danger*` 系）、
`rootLabel/lockToRootLabel/readOnlyLabel/protocolLabel`（连接表单由 manifest
宿主渲染）、`smb*` 11 键（同因；SMB 错误走 `friendlyError` 通用类别）。
`operationFailed` 表面无 UI 引用，实际被 `errorBannerOf`（i18n.ts 内部）使用，
保留。

### 失准译文更正（8 处）

- ja `copyBody`：正文误用「移動先」（move 用词），与 `copyTitle` 的「コピー先…」
  自相矛盾 →「コピー先」。
- ja `overwriteAsk`/`overwriteBatch`：「コピー先」→「配置先」——覆盖确认在
  copy 与 move 两种跨栏传输都会触发（App.vue `openConfirm("overwrite")`），
  原文在 move 场景语义错误。
- zh-TW `transferKind.rename`：「重命名」（大陆用语，简体字形残留，同前轮
  `transferKind.move` 同类问题）→「重新命名」。
- es `transferStatus.failed`：`Error`（名词）→ `Fallida`——与同组
  `Completada`/`Cancelada` 阴性形容词一致（修饰 transferencia）。
- ja `active`：`実行中` 与 `transferStatus.running` 撞词（Active 是传输面板
  分组标题）→「進行中」。
- ja `sessionStatus.disconnected`：「切断」（动作名词）→「切断済み」，与
  `接続済み` 对仗。
- it `sessionStatus.connecting`：`Connessione`（名词）→ `Connessione in
  corso`（进行态）。

### manifest.json 顶层描述补翻（五语）

en 顶层 description 在 a3802c9 已更新为「统一文件工作台」新文案（zh-CN 同步），
zh-TW/es/it/ja/pt-BR 仍滞留 fcef37f 时代的旧描述（"Apache OpenDAL 多后端
存储工作台插件"）——本次按当前 en 源文补翻五语。各语 `name` 保留「DBX 檔案/
DBX Archivos/…」别名（README 明示 "Files Studio（io.dbx.files，DBX 文件）"
为有意命名）。字段级 localizations 经逐条抽查与长度异常检测，七语一致且
质量良好，未改动。

### 回归证据

- `npx vitest run`（frontend）：**35 files / 272 tests 全过**（含 i18n 七语
  key 集合一致、占位符一致断言）。
- `npx vue-tsc --noEmit`：0 错误。
- 冗余审计脚本复扫：无静态引用且不属于动态家族的 key 剩余 0 个
  （`operationFailed` 为 i18n.ts 内部使用，预期内）。
- `scripts/`、`backend/src` 对被删 key 零引用（grep 确认）。

### 剩余风险

- `lib/opendalServices.ts` 的 `SERVICE_TEMPLATES`（快捷协议字段模板）内含
  英文 placeholder，当前无任何渲染入口（连接表单由 manifest 宿主渲染、
  opendal-custom 编辑器用 `CUSTOM_SERVICE_SCHEMAS`），属未接线的导出；
  若未来前端自渲染快捷协议表单需先补七语。
- 译文审校为单轮人工比对（en 基准 × 六语），es/it 语域混用（tú/usted）等
  风格层面差异未统一，仅修语义错误。

## Linux 发布产物 glibc 基线修复（2026-09-18，跨插件 CI 收口）

issue dbx-plugin-ssh#8/#58（官方 web docker 镜像 bookworm/glibc 2.36 装
插件后 sidecar 启动即退 `exited with status 1`）根因适用于本插件：v0.1.59
linux 产物实测同样钉死 GLIBC_2.39（ubuntu-24.04 原生构建），bookworm 容器
内复现 loader 失败。修复收口在上层仓（`build-candidates.yml` Linux 走
cargo-zigbuild 低 glibc 基线 + `CGO_ENABLED=0`；`validate_artifact_set.py`
新增 ELF GLIBC ≤2.31 守卫，files v0.1.59 坏包实测被拦），本仓唯一改动是
`scripts/build.sh` 的 `~/.cargo/bin` PATH 前置改条件式（防压回 CI 包装
器）。详见上层仓 `docs/CI_MULTI_PLATFORM.zh-CN.md`「Linux glibc 基线」节
与 ssh PROGRESS 同日条目。
