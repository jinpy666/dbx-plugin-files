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
