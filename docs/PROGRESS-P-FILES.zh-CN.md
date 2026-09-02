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
   bucket/access_key_id/secret_access_key、smb 的 share 声明为
   `required: true` + `visible_when` → 即使 protocol 修复，fs/webdav/
   ftp/sftp 也会在 "Bucket is required" 处被无条件拦死。
3. **宿主前端 `pluginFieldIsRequired` bug**（1a7d7609e 引入）：字段无
   `required_when` 时误用「缺条件=总是匹配」，所有插件字段被当必填
   （表单全字段带星号、`hasRequiredConnectionTarget` 永不满足、「保存并
   连接」被禁用）。

**修复**：
- 插件侧 v0.1.4：静态 `required` 收敛为 display_name/protocol；协议特定
  必填项改 `required_when` 与 `visible_when` 成对声明（bucket/
  access_key_id/secret_access_key→s3，share→smb，service→opendal-custom，
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

## 8. 路径栏快速目录下拉（tiny-rdm 对标体验迭代，2026-08-30）

**需求**：对标 tiny-rdm 双栏传输窗的路径栏下拉——本地/目标栏路径行内直接
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
- 新增 `frontend/src/components/PathField.vue`：单一路径控件（tiny-rdm 对标）
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
