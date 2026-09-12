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
