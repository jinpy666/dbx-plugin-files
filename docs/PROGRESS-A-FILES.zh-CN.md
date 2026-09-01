# A-FILES 路交付报告（dbx-files-plugin UI 专业化增强）

> 路线：对标 tiny-rdm 文件控制台（FileBrowserPane 双栏/预览/编辑交互）+
> DBX UI 规范（wb-*/CSS 变量、无硬编码色值、双主题适配）。
> 范围：仅 `frontend/**`（backend/src/** 只读）；`scripts/*.py` 未动；
> 无新 npm 依赖；无 git commit/push。

## 0. 验证终值

| 套件 | 结果 |
|---|---|
| 前端 vitest | **47 passed / 0 failed**（基线 19 → 47：面包屑 6、排序 6、压缩包识别 4、预览辅助 4、prefs 3、i18n 对齐 5） |
| 前端 typecheck（vue-tsc） | ✅ |
| 前端 build | ✅（自包含 `ui/index.html` 已重建） |
| cargo test（backend/ 全量） | **71 passed / 0 failed / 3 ignored**（无回归，backend 未改动） |
| mock 手动走查 | 13 张截图入 `docs/screenshots-a-files/`（见 §2） |

## 1. 逐项完成

### ① 双栏结构（对标 tiny-rdm 双窗格）
- `App.vue` 改造为双栏：左栏=源（当前连接），右栏=目标栏/预览（Tab 切换「目标面板/预览」）。
- 中间「双栏桥」：跨栏 copy/move 四按钮（→复制/→移动/←复制/←移动），同连接直接传路径，
  跨连接按方向带 `targetConnectionId` / `sourceConnectionId`（§8.2 契约字段，代码路径已打通）。
- 双栏间 HTML5 拖拽：FileTable 行 draggable（`application/x-dbx-files` dataTransfer），
  拖入对侧栏触发复制，带 `wb-drop-overlay` 松手提示（`dropToCopy`）。
- 单栏模式：工具栏「双栏」按钮折叠右栏；小屏/偏好均可关闭。
- 布局偏好持久化：`lib/prefs.ts`（localStorage key `dbx-files.ui`，仅排序/双栏/右栏 Tab，
  非敏感；损坏数据 sanitize 回退；沙箱 try/catch 兜底）。
- mock 走查实测：目录跨栏 copy 走降级 job → 传输面板出现任务 → 终态后右栏自动刷新（job 语义复用 copy/move）。

### ② 预览与编辑（`PreviewPane.vue` 重写）
- 文本预览（files/read ≤2MiB）+ **编辑模式**：铅笔进入 textarea 编辑 → 保存走
  `files/write`（≤4MiB，`canEditBytes` 判断；超限提示「编辑内容超过 4.0 MiB，请改用上传」）；
  **truncated 时禁编辑**（防截断内容写回覆盖整文件）。
- 图片预览：`dataBase64 → data:` URI（png/jpg/jpeg/gif/webp/bmp/svg 按扩展名，`imageMimeFor`）。
- 十六进制预览：二进制类（不可打印占比 >8%）渲染前 512B hex dump（偏移 + hex 双列 + ASCII，`hexDump`）。
- truncated 提示升级：「文件过大，仅预览前 2.0 MiB」+ 头部/横幅双「下载」引导。
- 走查实测：编辑 readme.md 保存后 174 B → 219 B，列表与预览同步刷新（mock files/write 落盘）。

### ③ 压缩包操作（前端骨架 + 后端交接）
- `lib/archive.ts`：`.zip/.tar.gz/.tgz/.tar` 扩展名识别（大小写不敏感）。
- 右键菜单：压缩包条目显示「查看压缩包内容」（预览 Tab 占位说明）与「解压到…」。
- 「解压到…」：确认框（目标路径输入）→ 调 `files/extract {path, targetPath}`；
  后端返回 `transport=job` 时复用 copy/move 的 job 进度语义；方法未注册时走既有
  `featureMissing` 横幅（走查截图 10）。**zip 解压标注 Phase 2**。

### ④ 细节专业化
- 面包屑：新 `Breadcrumbs.vue`（可点击逐级导航 + 中间层级折叠为「…」点击展开，
  `lib/breadcrumbs.ts` 纯函数）；源栏工具栏与右栏头部共用；当前级高亮。
- 右键菜单统一（源/目标栏共用一份）：打开/预览|查看压缩包内容/下载/解压到…/
  同步|复制文件夹/复制…/移动…/重命名/删除/复制路径。
- 排序持久化：`lib/sorting.ts` 比较器抽出（目录恒置顶）+ SortState 进 localStorage。
- 空态/加载骨架屏：`wb-skeleton` shimmer（wb-* 变量体系），FileTable 加载时渲染
  12 行骨架行；PreviewPane 加载骨架；10k 目录虚拟滚动走查无回归。

## 2. 截图清单（docs/screenshots-a-files/）

| 文件 | 内容 |
|---|---|
| 01-dual-pane.png | 双栏初始态（源栏 + 目标栏 + 双栏桥 + 停靠面板） |
| 02-dual-pane-docs.png | 源栏导航 /docs（面包屑） |
| 03-preview-text.png | 文本预览（右栏预览 Tab） |
| 04-preview-edit.png | 编辑模式（textarea + 取消/保存） |
| 05-preview-hex.png | 二进制 hex dump 预览（512B） |
| 06-preview-image.png | 图片预览（data URI；样例为 1x1 PNG） |
| 07-preview-archive.png | 压缩包占位预览（需 files/archiveList 说明） |
| 08-context-menu-archive.png | 压缩包条目统一右键菜单 |
| 09-extract-dialog.png | 「解压到…」确认框 |
| 10-extract-feature-missing.png | 解压骨架：files/extract 未注册 → featureMissing 横幅 |
| 11-cross-pane-copy.png | 跨栏复制：目录 copy job 完成，右栏 /empty 自动刷新 |
| 12-loading-skeleton.png | 加载骨架屏（10k 目录刷新中） |
| 13-single-pane.png | 单栏折叠模式（偏好持久化，刷新后仍单栏） |

走查入口：`frontend/mock.html?mock=1`（vite dev；mock 宿主本次补齐 files/read|write
与文本/PNG/二进制/压缩包样例文件，见 `lib/mockHost.ts`）。mock.html 不进插件构建
（build.mjs 仅打包 index.html）。

## 3. 交接（后端需求规格：backend/src/** 本路只读）

### 3.1 `files/archiveList`（M2.5，tar 优先）
- 请求：`{ connectionId, path, page?, pageSize? }`
- 返回：`{ entries: [{ name, path(条目内路径), kind: "file"|"directory", size, modifiedAt? }], total }`
- 边界：
  - tar/tar.gz/tgz：手写 tar header 解析（512B 块，name/type/size/mtime；ustar/PAX 兼容读法），
    gzip 经 flate2 解流——**无新增重依赖**（flate2 已在常见依赖树；若未引入可用
    `libflate` 级替代或先支持纯 `.tar`，`.tar.gz` 标注随后）。
  - 仅支持「列条目」不落盘；`path` 必须在连接 root 白名单内；加密/分卷不支持 → -32000。
  - zip（central directory 解析）标注 **Phase 2**。
- 前端已就位点：`lib/archive.ts` 识别 + PreviewPane `archive` 模式占位（提示文案
  `archivePreviewUnsupported`）+ 走查截图 07。实现后把占位替换为 `files/archiveList`
  分页渲染即可。

### 3.2 `files/extract`
- 请求：`{ connectionId, path, targetPath }`（targetPath 为目录，不存在则创建）
- 返回：同步 `{ success: true }` 或降级 `{ transport: "job", jobId }`（大包必须走 job，
  语义复用 §7 dir job：`filesDone/filesTotal/bytesDone/bytesTotal` + 取消）。
- 边界：
  - 逐条目 read→write（OpenDAL），目标条目路径必须仍落在 root 白名单内
    （防 zip-slip/tar 路径穿越：拒绝 `..` 与绝对路径条目）；
  - `read_only` 门禁适用；同名覆盖遵循连接门禁；`allow_delete=false` 不影响解压覆盖
    （解压不删源）。
- 前端已就位点：`startExtract` 确认流 + `callFor(side, "files/extract", …)` +
  `transport=job` 时 `trackSidecarJob`（进度/取消/终态刷新全部复用既有管线）。

### 3.3 跨连接目标栏（宿主侧，可选）
- 目标栏连接选择器依赖宿主连接枚举：前端探测 `window.dbxPlugin.request("host.listConnections")`
  （期望 `[{ id, name }]`；未提供时选择器隐藏，仅支持「同连接另一路径」，已走查）。
  宿主如确定方法名/形状不同，改 `App.vue probeConnections()` 一处即可。

## 4. 七语确认

新增键（`lib/i18n.ts`）：`dualPane, targetPane, targetConnection, sameConnection,
copyToTarget, moveToTarget, copyToSource, moveToSource, dropToCopy, paneTransferred,
extractTo, extractTitle, extractBody, archiveContents, archivePreviewUnsupported, edit,
editTooLarge, saved`（18 键）+ `previewTruncated` 文案更新——**七语（zh-CN/zh-TW/en/es/it/ja/pt-BR）全量补齐**。
新增 `i18n.spec.ts` 全量对齐断言：七语键集递归相等 + 占位符一致 + 回退解析，护航后续增量。

## 5. 遗留

1. `files/archiveList` / `files/extract` 后端实现（§3 规格），zip 为 Phase 2。
2. 宿主连接枚举方法（`host.listConnections`）未确认——跨连接目标栏当前默认仅同连接；
   UI 与 `targetConnectionId` 管线已就绪。
3. 浅色主题截图未单独留档：样式全部走 CSS 变量/`color-mix`，无硬编码色值（骨架/桥/
   编辑区同套令牌），由宿主 `applyAppearance` 注入即适配；如需可后续在宿主内补拍。
4. 跨栏 copy/move 不做逐项覆盖确认（后端对同路径返回明确错误并以横幅呈现）；
   如需 tiny-rdm 式冲突对话框可在 `transferBetween` 前挂 `ConfirmDialog`。
5. 拖拽移动（move）未区分修饰键：拖入对侧固定为复制，移动走桥按钮（防误拖删源）。

## 6. 阻塞

无。全部任务在本次实例内完成，三件套 + cargo test 全绿。

## 7. 双栏本地/远端分面（2026-09-01，__local__ 内置连接）

**需求**：双栏模式默认左=本地文件、右=远端文件（对标 tiny-rdm/FileZilla），
而非两侧都打开当前（远端）连接。

**改动**：
- 后端（`backend/src/engine/mod.rs`）：内置保留连接 `__local__`——
  `Engine::entry` 按需合成 root=`/` 的 fs 连接记录（可写、可删、不锁 root，
  与用户自建本地 fs 连接同策略），不占连接表；`connection/connect` 拒绝该 id
  防遮蔽。全部 `files/*` 方法（浏览/读写/传输/quickPaths/capabilities）对
  `__local__` 直接生效，无新增协议方法。quickPaths 因 root=`/` + fs 自动透出
  home 家族（逐个 stat 校验）。新增单测 2 例（合成记录解析 + 保留 id 拒绝）。
- 前端（`App.vue`）：
  - `leftConnectionId`（默认 `__local__`）+ `sideConnectionId(side)` 统一按栏
    路由：loadDirectory / loadQuickPaths / callFor / upload / download /
    transferBetween 全部改走该 helper（原右栏特判收编）；
  - 双栏开启：左栏重载本地根目录，quickPaths 到位后若仍在 `/` 落到主目录
    （对标 FileZilla 起点）；关闭：左栏回到当前连接根目录；
  - 左栏连接选择器（双栏时显示）：本地文件 / 同连接 / 宿主其它连接
    （`leftConnections`），切换后回根目录 + 重取 quickPaths；
  - 预览：`openPreview(path, side)` 固化来源栏连接快照
    （`previewConnectionId`）——单栏预览会顺手开启双栏、左栏随即切本地，
    read/write/archiveList 必须仍指向预览来源连接；`PreviewPane` 新增
    `connectionId` prop（缺省走默认注入，旧 sidecar 语义不变）。
  - mock 宿主（`lib/mockHost.ts`，dev/验证专用）：`__local__` 路由到独立
    本地树 + home 家族 quickPaths；写路径仍落远端树（写面由 sidecar 真实
    实现覆盖）。
- i18n：新增 `sourceConnection`、`localFiles` 七语（zh-CN/zh-TW/en/es/it/ja/pt-BR）。
- 文档：IMPL_PLAN §8 公共约定补「保留连接 `__local__`」语义。

**验证**（2026-09-01）：cargo test 135 passed / 3 ignored（含 2 新增）；前端三件套
（typecheck / vitest 84 / build）全绿；`scripts/test.sh` 全套 all green
（release 构建 + 打包 + framed smoke）。mock 浏览器走查：双栏初始左=本地树
（选择器「本地文件」选中、面包屑落到主目录）右=远端树；本地目录导航、
本地文件预览（read 路由正确读本地树）、左栏切「同连接」变远端树，均符合预期。

**遗留**：真实宿主（dbx-host-e2e）双栏截图留档待下一轮 patrol；web Docker
部署下 `__local__` 语义为「sidecar 所在机器的文件系统」（服务器本地盘），
如需对 web 端隐藏可后续在 capabilities 加开关。

## 8. 快速定位侧栏 + 路径行瘦身 + 概览弹窗（2026-09-01 第二轮）

**需求**：① 本地/各栏支持高频目录快速定位（桌面/下载等，文件管理器对标）；
② 路径行与搜索过挤，优化布局；③ 文件概览改弹窗，不再切换右栏 Tab。

**改动**（纯前端，协议契约不变）：
- 快速定位侧栏：新增 `components/QuickSidebar.vue`（根/主目录/桌面/下载/
  文档/图片一列直达，图标 + 七语标签 + 当前目录高亮；quickPaths 多于
  root 一项时挂载，非 fs/受限连接自动隐藏）。图标映射 `quickPathIcon`
  下沉 `lib/quickPaths.ts`；`QuickPathsMenu.vue` 路径栏下拉删除（含
  `.wb-quick-menu*` 样式）——快速定位统一走侧栏，root-only 时从面包屑
  /路径栏到达根目录。
- 布局：`wb-pane-tabs`（右栏 Tab 行 + 左栏 ghost 空条）退役，改为
  `wb-pane-topbar`（等高顶条，承载各栏连接选择器，右对齐）；pane 内容
  分层 `wb-pane-body`（侧栏 + `wb-pane-main`）。路径行瘦身为
  上一级/刷新/面包屑/搜索四件，不再塞快速目录下拉与连接选择器。
- 概览弹窗：`PreviewPane` 移入 `wb-preview-overlay` 居中浮层
  （860px 上限，遮罩点击 / Esc / 关闭按钮均可关闭）；`openPreview`
  不再强制开双栏、不再切 Tab——单栏模式预览同样弹窗，主视图保持不动；
  `previewConnectionId` 快照语义保留（预览/编辑/压缩包列表仍指向来源
  栏连接）。`rightTab` 状态与持久化删除（prefs 兼容旧 localStorage，
  未知字段忽略），右栏恒为目标面板。
- i18n：删除 `targetPane`、`previewTitle`（七语同步）；`quickPathsTitle`
  复用为侧栏标题。

**验证**（2026-09-01）：前端三件套全绿（typecheck / vitest 84 / build），
`scripts/test.sh` 全套 all green（cargo 135 / smoke PASS 49 SKIP 4 / 打包
0.1.18）。mock 浏览器走查：侧栏渲染与当前高亮、侧栏点击直达桌面目录、
双栏/单栏双击文件均弹窗预览（右栏保持目标面板、单栏不再强制开双栏）、
Esc 关闭、关闭双栏后左栏回当前连接根目录。

**遗留**：窄窗（<900px）下双栏 + 双侧栏的表格列宽偏窄（名称列省略号），
如需可加侧栏折叠按钮；概览弹窗编辑态 Esc 直接关闭（未保存内容丢失，
与关闭按钮一致）。
