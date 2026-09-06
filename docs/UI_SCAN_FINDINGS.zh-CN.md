# Files 插件前端 UI 体验扫描报告（UI_SCAN_FINDINGS）

> 扫描角色：场景驱动的 UI 体验扫描 agent（只读扫描 + 报告，不实施修复，未改任何生产代码）。
> 扫描对象：`files/frontend`（`mock.html?mock=1` 可视化夹具，Vue 3 双栏文件工作台）。本报告只记录发现与方向建议。

## 一、扫描环境

| 项 | 值 |
| --- | --- |
| 轮次 | 第 1 轮基线走查（2026-09-06，约 11 轮 playwright 脚本，场景优先端到端旅程） |
| Dev server | 本轮新起 `pnpm vite --port 5293 --strictPort`（files/frontend 工作目录，扫描结束已 kill） |
| 自动化 | playwright-core + 系统 Chrome（`chromium.launch({ channel: "chrome" })`），独立装于 `/tmp/uiscan-files`（未进项目依赖） |
| 视口矩阵 | 1280×900（默认）、720×900（窄）、1440×900（宽） |
| URL 参数矩阵 | `?mock=1`（必带）/ `&theme=light` / `&job=1`（copy/move 强制降级 job）/ `&delay=600`（加载态观察）/ `&locale=en-US`；**夹具无 `?ro=1` / `?err=1` / `?noconn=1`**（错误注入靠路径含 `error`、job 失败靠目标含 `fail`，只读态无法注入，见 P2-13） |
| 交互核验 | Tab 焦点顺序、Esc 分层关闭、Enter 提交、方向键/Shift 扩选/Home/End/Space/Ctrl+A 键盘导航、右键菜单（文件/空白区/侧栏树三套）、视口钳位、按钮禁用态、空态/加载骨架/错误态、明暗主题 |
| 证据 | 截图 20+ 张（仅走查自用，收尾已全部删除，未入仓库） |
| 备注 | `--success`/`--warning`/`--overlay` 变量由 `shared/frontend/themeSync` 注入 DBX 规范兜底值，mock 环境实测解析正常（静态疑点排除）；light 正文对比 19.8、次要文字 4.74（过线） |

## 二、发现清单

统计：**P0 × 0，P1 × 5，P2 × 14**。无阻断使用级问题；P1 集中在「面包屑渲染、窄视口布局、弹层/编辑焦点、上传语义」四类。

> **第 2 轮修复（2026-09-06）**：P1 全部 5 项已修复并通过浏览器复验（26/26 断言，
> 覆盖 dark/light × 1280/720、焦点链全流程）；P2 已顺手修复 4 项半
> （P2-2、P2-5 时间格式化半项、P2-13③⑤、P2-14），详见各条目内「修复」标注与
> `PROGRESS-P-FILES.zh-CN.md` 同日小节。
>
> **第 3 轮清理（2026-09-06）**：其余 P2 全部 12 项（P2-1、P2-3～P2-12、
> P2-13①②④）修复完毕并通过浏览器复验（26/26 断言 + 压缩包预览 UI 走查）；
> 「遗留未定论」的 Space 首按问题已在 mock 环境定位根因并修复（见 P2-9 后
> 附录与 `PROGRESS-P-FILES.zh-CN.md` 同日小节）。至此报告内全部条目闭环。

### P1（明显可感知的体验债）

**P1-1 面包屑根段渲染双斜杠，所有路径栏常态显示 `//Users/demo` 形态**
- 位置：PathField → Breadcrumbs（`wb-breadcrumbs`）；根 crumb（name="/"）后仍输出 `wb-crumb-sep` 分隔符。
- 复现：任意页面看任一栏面包屑。DOM 证据：`<button>/</button><span class="wb-crumb-sep">/</span><button>Users</button><span>/</span>…` → 视觉 `//Users/demo`。进入 `/docs` 显示 `//docs`；深层路径 `/a/b/c/d/e/f` 折叠后显示 `//…/e/f`，双斜杠更扎眼。dark/light、zh/en 全组合一致（`?locale=en-US` 下同为 `//Users/demo`）。
- 影响：工作台最显眼的常驻控件，每一栏每一屏都错；易被误读为路径拼接 bug。
- 建议：根段特判——首段为根时不输出分隔符（或根段直接作为单一 `/` crumb 不参与 sep 序列）；`breadcrumbs.spec.ts` 补根路径用例。
- **修复（2026-09-06）**：`Breadcrumbs.vue` 分隔符改 `index > 1` 渲染（根段后第一段不补 sep，折叠态同理）。复验：dark/light、双栏两侧、`/a/b/c/d/e/f` 折叠态（`/…/e/f`）均无 `//`；新增组件 spec `Breadcrumbs.spec.ts` 4 例。✅ 已修复并复验。

**P1-2 720px 窄视口下左栏核心控件被压缩至不可用**
- 位置：`wb-content` 布局（style.css `.wb-side-panel` 固定 148px、`.wb-dock` `flex: 0 0 300px`、`.wb-pane-target` `min-width: 260px`，均无窄视口收缩策略）。
- 复现：720×900 打开 mock（默认双栏 + dock 开启）。实测：左栏主区仅 **134px**、路径栏 **14px**、搜索框 **7px**、面包屑 **0px**；左栏文件名几乎不可读，路径输入与过滤完全不可用；720 下双击打开预览也失败（内容区过窄）。文档无水平溢出（`overflow: hidden` 掩盖），是"静默挤压"而非滚动。
- 影响：笔记本半屏/分屏窄窗口下左栏（默认本地栏）基本不可操作。
- 建议：窄视口断点（如 <900px）默认收起 dock 与双栏、侧栏转窄条；或给各区域设 min-width 并允许整体横向滚动/堆叠，搜索框可折叠进 ⋯ 溢出菜单。
- **修复（2026-09-06）**：新 lib `responsive.ts`（<900px 判定）；App 跨入窄视口时默认一次性收起 dock 与双栏（用户仍可手动重开，不改持久化偏好）；`style.css` ≤900px media query 收缩侧栏（112px）/过滤框（112px）/目标栏与 dock min-width。复验：720×900 下 dock 与双栏自动收起、左栏主区 597px、搜索框 88px、面包屑 371px、工具栏核心按钮可达、无水平溢出、双击预览可用；新增 `responsive.spec.ts`。✅ 已修复并复验。

**P1-3 确认弹层无焦点管理：打开不聚焦、Tab 跑出弹层、背景按钮可被误触发**（kafka 报告 P1-2/P1-3 同款问题）
- 位置：ConfirmDialog（新建文件夹/新建文件/重命名/删除/复制/移动/压缩/解压/同步全部弹层共用）。
- 复现：点工具栏"新建文件夹"打开弹层 → 焦点仍停留在触发按钮（`BUTTON 新建文件夹`）；连续 Tab：取消 → 确认 → **第 3 步跑出弹层回 BODY**、第 4 步到背景"新建文件夹"按钮（无 focus trap）。此时按 Enter 会再次触发背景按钮把弹层重新打开（草稿重置，未误提交但焦点流断裂）。删除等危险弹层同理。
- 期望 vs 实际：期望打开时焦点进弹窗首控件（表单类弹层应直接落输入框）、Tab 在弹窗内循环、关闭归还触发元素；实际三者皆无。
- 建议：open 时 focus 输入框（有表单）或取消/确认按钮 + 简易 focus trap；close 时归还触发按钮。可参考 kafka 侧 `decideModalKeydown` 已收口方案。
- **修复（2026-09-06）**：`ConfirmDialog.vue` 收口三件事——open 即聚焦（表单输入直落并全选，无表单聚焦安全项取消钮）、Tab/Shift+Tab 弹层内循环（焦点意外落 BODY 也拉回）、关闭（确认/取消/Esc 共路）后焦点归还触发元素。复验：新建文件夹弹层聚焦输入框、Tab 8 次不出弹层、Esc 关闭后焦点回触发钮、危险弹层聚焦取消；dark/light 与 zh/en-US 双语同过；新增 `ConfirmDialog.spec.ts` 5 例。✅ 已修复并复验。

**P1-4 预览弹窗与 CodeMirror 编辑器焦点缺失，编辑流多一次必点**
- 位置：App.vue 预览遮罩 + PreviewPane/TextPreview。
- 复现 A：双击 readme.md 打开预览 → `document.activeElement` 仍在背景列表（不在 `.wb-preview-overlay` 内），键盘用户无法直达弹层内控件（Esc 能关是靠 document 级监听兜底）。
- 复现 B：点预览头部"编辑"进入编辑态 → CodeMirror 未聚焦（`.cm-focused` 不存在），必须再点一次编辑器才能输入；键盘路径完全不通。
- 影响：编辑保存是核心写路径，每次进入编辑都多一次点击；预览弹窗对键盘用户不可达。
- 建议：弹窗打开后 focus 弹窗容器或关闭钮；进入编辑态 `nextTick` 后调 `EditorView.focus()`。
- **修复（2026-09-06）**：`PreviewPane.vue` 根容器加 `tabindex="-1"` 并在挂载后程序化聚焦；`TextPreview.vue` 编辑实例就绪即 `view.focus()`（权威时机，覆盖语言包异步装载）并 expose `focus()`，PreviewPane 进入编辑态兜底调用。复验：预览打开焦点在弹窗内、点「编辑」后 CodeMirror `cm-focused`/activeElement 命中（Chrome 实测），720px 下同样成立；新增 `TextPreview.spec.ts` 3 例、`PreviewPane.spec.ts` 2 例。✅ 已修复并复验。

**P1-5 双栏默认（左栏=本地）下工具栏"上传文件"上传到本地栏，语义反直觉且 mock 下自相矛盾**
- 位置：App.vue `uploadSource`（固定 `joinPath(path.value(左栏), name)` + `connectionId=左栏连接`）。
- 复现：默认双栏（左栏本地 `__local__`）→ 工具栏"上传文件"选文件 → 通知"已上传 1 个文件"、dock 显示"已完成"，**但本地栏列表看不到新文件**（mock `files/upload/finish` 写死远端树，不按 connectionId 落树）。
- 影响：两层问题——① 产品语义：用户预期"上传=传到远端存储"，实际却写左栏（本地盘），与 tiny-rdm/FileZilla 心智相反，至少需产品确认；② 夹具缺陷：mock 不按 connectionId 落树，导致该路径在浏览器验证里永远"上传成功但文件消失"，无法自洽走查。
- 建议：产品侧确认双栏时的上传目标（建议双栏=传到对侧栏目录，单栏=当前连接目录）；mock 侧修复 `upload/finish` 按 start 记录的 connectionId 落对应树；通知文案写明目标路径。
- **修复（2026-09-06，语义定为「上传=传向远端」）**：双栏时右栏恒为远端连接面（连接选项不含 `__local__`），故上传目标固定为右栏当前目录；单栏时保持当前连接当前目录（原行为）。新纯函数 `lib/uploadTarget.ts` 收口，上传后刷新目标栏、通知带目标路径（`uploaded` 键七语补 `{path}` 占位）。mock 夹具 `upload/start` 记录 connectionId、`finish` 按其落对应树/内容（并补齐 slot.bytes 内容落盘，②③两层问题同收口）。复验：默认双栏上传后文件出现在右栏（远端 `/docs`）、左栏本地面无残留、通知「已上传 1 个文件到 /docs」；新增 `uploadTarget.spec.ts` 4 例。✅ 已修复并复验（若产品后续要「跟随焦点侧」语义，仅需改 `resolveUploadTarget` 单点）。

### P2（打磨项）

**P2-1 错误文案透传英文原文**：路径含 `error` 时横幅显示"操作失败：mock backend failure for /error-dir"；`NotFound: …`、`Destination must differ…` 等同。`errorMessage`（api.ts）直通 message，files 无 friendlyError 映射层。建议参照 kafka `friendlyKafkaError` 收口方案：网络/不存在/权限类规则映射 + 原文留 title 悬停。
- **修复（2026-09-06，第 3 轮清理）**：新增 `lib/friendlyError.ts`（对标 ldap `friendlyLdapError`）：按 not found / permission / exists / network 四类正则映射到 i18n 七语新键 `errNotFound/errPermission/errExists/errNetwork`，未知错误保底原文透传；App `showError` 收口，横幅主显友好文案、原文挂 `title` 悬停（`errorDetail`）。mock 夹具补 `notfound` 路径注入（与 `error` 并列的错误注入轴）。复验：右栏导航 `/notfound-dir` 横幅显示「操作失败：目标不存在或已被删除」，title 保留 `NotFound: …` 原文；`mock backend failure`（未知类）按设计原文透传。新增 `friendlyError.spec.ts` 6 例。✅ 已修复并复验。
**P2-2 危险路径确认列表英文硬编码**：删除 `/etc` 目录弹层危险列表显示 `/etc`、`purge /etc`；批量删除显示 `10000 items`、`recursive delete`（dangerousPaths.ts label 为英文字面量，ConfirmDialog 直接渲染）。建议改走 i18n key（注意七语）。
  **修复（2026-09-06）**：`dangerousPaths.ts` 的 hit 增加结构化 `path`/`count` 字段，App 按 `hit.id` 映射 i18n（`dangerPurgeRoot`/`dangerPurge`/`dangerRecursiveDelete`/`dangerBulkDelete` 七语新增；路径类条目原样展示路径本身）。复验：删除 docs 目录弹层显示「递归清空 /docs」，无英文原文。✅ 已修复。
**P2-3 单次 list 业务失败即把连接 pill 置"已断开"**：导航到不存在目录（业务错误）→ 顶栏 pill 变红"已断开"，下次成功又翻回"已连接"。业务错误 ≠ 连接断开，状态抖动误导。建议仅网络/传输层错误置 disconnected，业务错误保持原态。
- **修复（2026-09-06，第 3 轮清理）**：`fetchListing` 失败分支改 `friendlyError.isTransportFailure` 判定——仅网络/超时类置 disconnected；业务错误视为「sidecar 应答了=连接活着」置 connected（同时消除失败期间停留「连接中」的抖动）。复验：右栏 `/notfound-dir` 失败后 pill 保持「已连接」。✅ 已修复并复验。
**P2-4 错误横幅"重试"只刷新左栏**：`retryAfterError` 仅 `loadDirectory()`；右栏加载失败时点重试不恢复右栏。建议按出错栏位重试。
- **修复（2026-09-06，第 3 轮清理）**：App 新增 `errorSide`（left/right/global），`loadDirectory/loadRightDirectory` 失败时记账；`retryAfterError` 按出错栏位重放（右栏失败只重载右栏，全局操作错误两栏都重载）。复验（`?delay=1200`）：右栏报错点重试，仅右栏出加载骨架、左栏无 skeleton，重载完成右栏恢复列表。✅ 已修复并复验。
**P2-5 审计面板不自动刷新且无手动刷新按钮**：面板打开期间新建文件夹等写操作后条目数不变（实测 1→1），须切走再切回 dock tab 触发 watch 才更新；时间戳显示 ISO 原文（`2026-09-05T16:56:32.559Z files/mkdir`），未走 formatTime 本地化格式。建议加刷新钮 + 写操作后自动刷新（或复用轮询）+ 时间格式化。
  **修复半项（2026-09-06）**：时间戳已走 `formatTime`（复验显示 `2026-09-06 02:14 files/upload`）。
  **修复剩余项（2026-09-06，第 3 轮清理）**：`AuditPanel.vue` 头部加手动刷新钮（lucide RefreshCw，loading 时禁用+旋转）；App 在 `onConfirm` 成功、job 终态（handleEvent）、`afterUpload` 三处调 `refreshAuditPanel()` 写后自动刷新（dock 开在 audit tab 才触发）。复验：面板打开期间新建文件夹 `audit-probe-xyz` 后审计列表自动出现该条目。✅ 已修复并复验（全部闭环）。
**P2-6 选中目录点工具栏"下载"零反馈**：`downloadSelection` 静默过滤掉目录后什么都不发生，提示条停留上一次文案（实测仍显示"已下载 notes.txt"）。建议过滤后为空时提示"所选不含可下载文件"。
- **修复（2026-09-06，第 3 轮清理）**：`downloadSelection` 与右键批量 `downloadSelected` 过滤后为空时 `showNotice(t("downloadNoneSelected"))`（七语新键）。复验：左栏（本地面）单选 Downloads 目录点「下载」，提示「所选内容中没有可下载的文件」。✅ 已修复并复验。
**P2-7 大目录批量删除为串行逐条 RPC**：App.vue `onConfirm` delete 分支 `for` 循环逐条 `await files/delete`——全选 1 万项即 1 万次串行调用，弹层仅 busy 态、无整体进度；真实 sidecar 下可能长时间无反馈。建议评估批量删除协议方法或并发分批 + 进度落传输面板。
- **修复（2026-09-06，第 3 轮清理）**：App 新增 `runBatch`（并发 8 分批执行，本地伪 job 登记 `TransferKind.delete`），进度经 `tracker.onProgress` 实时落传输面板（`filesDone/filesTotal` 计数 + 百分比条，终态 completed/failed）；delete 分支改走 `runBatch`。协议侧未新增批量方法（避免动后端），性能由并发分批兜底。复验：/docs 全选删除，传输面板出现「删除 · /docs」任务并实时推进至 completed。`transferKind.delete` 七语新增。✅ 已修复并复验。
**P2-8 传输任务标题 5s 轮询后退化为裸 jobId**：实测提交时显示 `/docs/notes.txt → /docs/notes-copy.txt`，6s 后变 `mock-job-1`。根因：mock `files/transfers/list` 返回缺 `remotePath`（真实 sidecar `transfers.rs` 带 `remotePath`，camelCase），前端 `applyList` 无条件覆盖本地字段 → mock 与真实契约脱节 + 覆盖策略脆弱。建议 mock 补 `remotePath`；`applyList` 对缺失字段保留现值（undefined 不覆盖）。
- **修复（2026-09-06，第 3 轮清理）**：双层同修——mock `runJob` 的 job 记录补 `remotePath: "${source} → ${target}"`（对齐 transfers.rs 契约）；`applyList` 对缺失 `remotePath`/`fileName` 时保留 `existing?.remotePath`（undefined 不覆盖）。复验：job=1 提交 copy 后等 7s（>5s 轮询窗口），传输面板标题保持 `/docs/notes.txt → …` 不退化。`transfers.spec.ts` 补 2 例（缺失保留现值 / 有值以 payload 为准）。✅ 已修复并复验。
**P2-9 目录树/文件行键盘不可达**：`wb-tree-row` 与 `wb-file-row` 均为纯 div 无 tabindex；文件表靠容器 tabindex+方向键可用，目录树则完全无键盘等价操作（caret 按钮仅展开/收起，不能进入目录）。建议树行加 roving tabindex 或提供 Enter 进入目录的键盘路径。
- **修复（2026-09-06，第 3 轮清理）**：`SideNavPanel.vue` tree tab 容器加 `tabindex="0"` + `role="tree"`（键盘可达入口），↑↓ 在已挂载行间 roving focus、Enter/Space 打开（进入目录，与单击同语义）、←/→ 点击行内 caret 展开/收起；`DirTree.vue` 行加 `tabindex="-1"`（可聚焦不进 Tab 序）。复验：容器聚焦后 ArrowDown 焦点依次落行、Enter 触发导航。新增 `SideNavPanel.spec.ts` 5 例。✅ 已修复并复验。
**P2-10 重名操作无前端预检与提示**：新建同名目录直接成功、重命名到已存在名静默成功（mock 不拒），UI 无冲突警示，依赖后端策略兜底。建议提交前 stat 预检或至少依赖后端错误做友好提示。
- **修复（2026-09-06，第 3 轮清理）**：`onConfirm` 的 newFolder/newFile/rename 分支提交前经 `files/stat` 预检（`pathExists`：确认存在→拦截；NotFound→放行；其他 stat 错误→不阻塞放行给后端兜底），命中重名显示七语提示 `nameExists`（「{name}」已存在）且弹层保持打开、草稿不丢，可直接改名重提。复验：本地面新建「Documents」→ 横幅提示 + 弹层未关。✅ 已修复并复验。
**P2-11 右栏 topbar 空条**：无可切换连接（mock `host.listConnections` 为空）时右栏仍渲染 28px 空 topbar（`v-if` 在 select 上而非容器上），与左栏（有下拉）不等价，浪费纵向空间。建议容器级 v-if 或左右栏形态对齐。
- **修复（2026-09-06，第 3 轮清理）**：右栏 `.wb-pane-topbar` 的 `v-if="targetConnections.length"` 提到容器级。复验：mock 单连接下右栏空 topbar 不再渲染。✅ 已修复并复验。
**P2-12 左右栏共享同一排序状态**：App 仅一个 `sort` ref 绑两栏，左栏点"大小"排序右栏表头与行序同步变化（实测确认）。与双栏独立连接的架构不一致，FileZilla 类工具惯例独立排序。建议按栏拆分 sort 状态（或产品确认全局一致排序）。
- **修复（2026-09-06，第 3 轮清理）**：新增 `rightSort` ref（初值沿用持久化偏好），`toggleSort(side, column)` 按栏路由，两 FileTable 分别绑定；左栏 `sort` 仍持久化（prefs 结构不变），右栏排序不落盘。复验：左栏点「大小」后左表头出排序箭头、右表头无标记。✅ 已修复并复验。
**P2-13 mock 夹具缺口，阻碍 UI 自证**：① 无 `?ro=1` 只读注入参数（kafka 有；只读徽章/禁用态本轮无法走查）；② `files/copy` 不路由 `sourceConnectionId/targetConnectionId`（跨栏复制本地→远端报 `NotFound`，跨连接核心旅程无法验证）；③ `files/upload/finish` 不按 connectionId 落树（见 P1-5）；④ `files/archiveList` 未实现，压缩包预览恒为占位文案（B-ARCHIVE 已交付能力在夹具层缺位）；⑤ mock.html 无 icon 声明，控制台每次一条 404 噪音（kafka P2-14 同款已修，files 未跟）。
  **修复部分（2026-09-06）**：③ 已随 P1-5 修复（upload/start 记录 connectionId，finish 按其落树/内容）；⑤ 已修（mock.html 加空 data URI icon 声明，复验无 404）。
  **修复剩余项（2026-09-06，第 3 轮清理）**：
  - ① `?ro=1` 只读注入：mock 读 `ro` 参数，同时注入 `connection.readOnly`（含 `ready` 返回的完整 context——此前 ready 只带 connectionId，注入到不了 App 的 canWrite）与 `files/capabilities.readOnly`。复验：`?mock=1&ro=1` 下只读徽章可见、新建文件夹/上传禁用、右键删除项 disabled。
  - ② `files/copy/move/syncDir/copyDir` 按 `sourceConnectionId`/`targetConnectionId`（缺省回落 connectionId）路由源/目标树与内容仓（新 `copyEntryBetween`），rename 同步按连接路由。复验（API 层）：`sourceConnectionId:"__local__"` 复制本地面文件到远端 `/docs` 成功落树。
  - ④ `files/archiveList` 落地：mock 用 `archiveSources` 表（compress 时记录源路径，三个预置样例预登记）按当前树展开为归档内相对路径条目，契约对齐 backend `archive.rs::ArchiveEntry`（name/path/kind/size + page/pageSize clamp 1..1000 → `{entries,total}`）。复验：预览 site.tar.gz 显示「压缩包内 7 个条目」及条目列表（占位文案不再出现）。
  ✅ ①②④ 全部修复并复验（13 项闭环）。
**P2-14 dock 图标体系不一致**：传输任务取消按钮用文本 `✕`、清空历史用 emoji `🗑`，与工具栏 lucide SVG 图标体系不统一，跨平台 emoji 渲染有差异。建议换 lucide 图标（X / Trash2）。
  **修复（2026-09-06）**：`TransferPanel.vue` 已换 lucide `X`/`Trash2`，复验清空历史按钮渲染 SVG。✅ 已修复。

### 走查中表现良好、无需处理的项

- Esc 分层关闭链路完整：预览 → 确认弹层 → 三类右键菜单，逐层互不误伤（实测菜单 Esc 关闭、弹层 Esc 关闭、预览 Esc 关闭）。
- 路径栏单一控件（面包屑⇄编辑态）：点文件夹图标进入编辑态全选、Enter 跳转、Esc 还原草稿、失焦有改动自动提交（实测通过；编辑态体验良好）。
- 右键菜单视口钳位（三菜单互斥 + 越界回拉）、空白区右键新建/刷新菜单（拦截浏览器默认菜单）。
- 大目录 1 万项：虚拟滚动正常（可见行 43）、大目录提示与计数准确、Ctrl+A 全选/删除弹层数量如实（"删除 10000 项？"）、搜索过滤即时且计数联动。
- 预览三态完备：文本（CodeMirror）/图片/hex dump/压缩包占位、>2MiB 截断提示 + 编辑自动禁用（防截断写回）、5000 行大文本弹窗高度受视口约束（860×828）。
- job 生命周期：排队/传输中/已完成/失败/已取消五态 + 进度条 + 失败重试按钮 + 取消 + 清空历史（仅清完成态）全部实测可用。
- 明暗主题：light/dark 无拼色，`--success`/`--warning`/`--overlay` 经 themeSync 兜底解析正常；七语（抽查 en-US）工具栏/右键菜单/空态文案齐全。
- 双栏桥（复制/移动到目标/源栏）按钮禁用态与选择联动正确。

## 三、场景走查矩阵

| 场景 | dark 1280 | light | 720 | 1440 | 备注 |
| --- | --- | --- | --- | --- | --- |
| 初始加载/双栏默认（左本地右远端） | ✓ | ✓ | ✓(挤压) | ✓ | 左栏落 home 对齐 FileZilla 习惯 |
| 目录浏览/双击进入/上一级/刷新 | ✓ | ✓ | ✓ | ✓ | 刷新提示"目录已刷新"正常 |
| 面包屑/路径输入跳转 | ✓(P1-1) | ✓(P1-1) | 挤压 | ✓ | 折叠省略号 + title 正常 |
| 搜索过滤（含 Esc 清空） | ✓ | ✓ | 挤压 | ✓ | 过滤计数联动 footer |
| 多选/键盘导航（↑↓/Shift/Home/End/Ctrl+A） | ✓ | — | — | — | Space 首按问题已定位修复（第 3 轮清理，见遗留）；目录树键盘导航已补（P2-9） |
| 右键菜单（文件/批量/空白区/侧栏树） | ✓ | ✓ | — | ✓ | 多选自动切批量动作面 |
| 新建文件夹/新建文件（弹层） | ✓(P1-3) | — | — | — | Enter 提交、空名拦截 |
| 删除/危险路径确认 | ✓(P2-2) | — | — | — | /etc 触发 danger 红框列表 |
| 上传 | — | — | — | — | P1-5（语义 + 夹具缺陷） |
| 下载（单选/批量/预览内） | ✓ | — | — | — | 浏览器下载事件 + 通知正常 |
| 传输面板（job 进度/取消/重试/清历史） | ✓(P2-8) | — | — | — | job=1 强制降级全链路通过 |
| 预览（文本/编辑/图片/hex/压缩包/截断） | ✓(P1-4) | ✓ | — | — | light 下 CodeMirror 白底正常 |
| 审计面板 | ✓(P2-5) | — | — | — | mock 下审计写入正常、渲染正常 |
| 连接面板（自定义配置编辑器） | ✓ | — | — | — | Form/JSON 双模、坏 JSON 拦截切换、测试按钮校验拦截均正常 |
| 错误注入（路径含 error） | ✓(P2-1/3) | — | — | — | 横幅 + 重试 + pill 断开态 |
| 大目录 /10k | ✓(P2-7) | — | — | — | 虚拟滚动 + 提示 + 全选删除 |
| locale=en-US | ✓ | — | — | — | 无漏翻（危险列表 P2-2 已修） |

## 四、验证与遗留

- 走查脚本 11 轮，覆盖面板/组件 12 个（App 工作台 + SideNavPanel/DirTree/Breadcrumbs/PathField/FileTable/FileToolbar/TransferPanel/ConfirmDialog/PreviewPane/TextPreview/CustomConfigEditor/AuditPanel）。
- 发现条数：**P0=0、P1=5、P2=14**。
- 遗留未定论：
  - ~~Space 勾选首按偶发无效~~ **已定位并修复（2026-09-06，第 3 轮清理）**：根因是 `FileTable.onListKeydown` 的 Enter/Space 分支用 `activeIndex()`（按 `props.activePath` 查找）判定行号，而 activePath 由父组件经 props 回写——方向键按下后的同一渲染 tick 内 props 仍是旧值，`activeIndex()` 得 -1，首按被 `return` 丢弃（且该分支在 `preventDefault` 之前）。修复：preventDefault 提前，行号改用组件内同步维护的 `nav.index` 兜底判定。复验：Home 后同 tick 连按 Space，首按即生效（选中态切换）、二按取反；`FileTable.spec.ts` 3 例覆盖 stale-props 时序。真机（真实宿主事件时序）建议例行复核，机制层面已闭合。
  - ~~只读态 UI（徽章、写按钮禁用、右键菜单禁用）因夹具无 `?ro=1` 未走查~~ 已随 P2-13① 补夹具并走查（见上）。
  - ~~跨连接双栏（`host.listConnections` 多连接）真实宿主行为未验证，mock 下跨栏复制因夹具缺路由报错（P2-13②）~~ 夹具路由已修（API 层复验通过）；多连接真机行为仍建议随下次真机验证例行覆盖。
- 走查用截图与 `/tmp/uiscan-files` 下脚本为扫描工具产物，不入库；报告落盘后已清理。

## 五、第 3 轮（专家视角深度测试，2026-09-06）

> 扫描角色：文件管理/存储工程师（FileZilla/WinSCP 重度用户视角）+ 软件测试专家。
> 只读扫描，未改任何代码；不重复前 2 轮已收口项，全部为本轮新发现。
> 脚本 10 轮（t1/t1b/t1c/t2/t2b/t3/t3b/t3d/t45/t6/t7/t7b），装于 `/tmp/uiscan-files-r3`（playwright-core + 系统 Chrome，收尾已清理，截图本轮零留存）。

### 一、本轮环境

| 项 | 值 |
| --- | --- |
| Dev server | 5293 端口复用上一轮遗留 vite（PID 30394，vite 从磁盘实时读源码，与新建等价；本轮结束已 kill） |
| 视口 | 1280×900 / 1440×900 |
| 夹具参数 | `?mock=1` 基线；`&delay=900/300/200`（加载/竞态观察）；`&job=1`（job 失败注入：目标含 `fail`）；`&ro=1`（只读矩阵）；路径含 `error`/`notfound`（业务错误）；另用页面内 monkey-patch `window.dbxPlugin.invoke` 对特定路径注入不对称延迟（竞态实证用，不改源码） |
| 数据播种 | 经 `window.dbxPlugin.invoke` 真实写入 mock 树（文件名边界/排序样例/删除探针），与后端返回等价 |

### 二、新发现清单

统计：**P0 × 0，P1 × 2，P2 × 10**。

> **第 4 轮修复（2026-09-06）**：本章 P1 × 2、P2 × 10 全部修复完毕并通过浏览器
> 复验（playwright-core + 系统 Chrome，13/13 断言通过，覆盖竞态/活动栏/只读
> 门禁/自然序/文件名校验/覆盖确认/两态空文案/a11y/批量取消/图标），详见各条目
> 「修复」标注与 `PROGRESS-P-FILES.zh-CN.md` 同日小节。至此本章全部条目闭环。

#### P1

**R3-P1-1 目录导航无竞态守卫：慢响应晚到覆盖，最终展示目录与用户最后点击不符**
- 位置：`App.vue` `loadDirectory` / `loadRightDirectory`（无请求序号/AbortController，`entries.value`/`path.value` 由最后完成者决定）。
- 复现：`?mock=1&delay=200` + monkey-patch 对 `path` 含 `/docs` 的 `files/list` 额外延迟 1500ms。右栏先双击 `/docs`（慢，~1700ms 返回），400ms 后双击 `/10k`（快，~200ms 返回）。实测结果：10k 列表先渲染，随后 docs 响应晚到覆盖——最终面包屑 `/docs`、7 项、无任何提示，而用户最后点击的是 `/10k`。
- 影响：慢连接/大目录下快速切换目录是 FileZilla 类工具高频操作；晚到覆盖后 ① 用户停在非预期目录；② 后续写操作（新建文件夹/上传落点、删除目标）全部基于错误目录上下文。竞态窗口与延迟成正比，真实远端下远大于 mock。
- 建议：按栏维护请求序号（或 AbortController），响应到达时丢弃过期序号结果；至少在路径跳转时使前一请求失效。两栏同修。
- **修复（2026-09-06，第 4 轮）**：新 lib `navGuard.ts`（`createNavGuard` 按栏请求序号守卫），`App.vue` `loadDirectory`/`loadRightDirectory` 发请求前取号，响应到达（含错误与 loading 收尾）验号，过期序号一律丢弃——面包屑/列表/选中态/加载态均以最新导航为准。复验：monkey-patch 对 `/docs` 的 `files/list` 注入 1500ms 延迟，先双击 /docs、400ms 后双击 /10k，最终停在 /10k（file-*.txt 列表、无 readme.md），docs 晚到响应不再覆盖。新增 `navGuard.spec.ts` 4 例。✅ 已修复并复验。

**R3-P1-2 工具栏动作只绑定左栏（源栏）：右栏选中时「下载/删除所选」禁用，「新建文件夹」无视焦点永远落左栏**
- 位置：`App.vue` `:has-selection="selection.length > 0"`（只绑左栏 `selection`）；`@delete="startDelete(sortedEntries.filter(...))"`（只读左栏）；`startNewFolder()` 默认 `side="left"`；`downloadSelection` 只读 `sortedEntries`。
- 复现（实测）：右栏选中 1 项 → 工具栏「下载」「删除所选」仍 disabled（左栏选中 1 项则正常启用）；右栏 10000 项全选 → 工具栏删除仍不可用，只能走右键菜单。
- 影响：① 双栏语义割裂——「上传文件」固定传向右栏（P1-5 已定语义），「新建文件夹/新建文件」却永远落左栏；用户焦点在右栏（远端主工作面）时点工具栏新建，会在本地栏凭空创建目录，属跨栏误操作且无提示。② 右栏选中的删除/下载在工具栏灰置，与右键菜单可达性不一致，用户易判定为「功能坏了」。
- 建议：工具栏动作跟随「当前活动栏」（最近交互的 pane，FileZilla 惯例）路由 side 与 selection；至少：has-selection 取两栏并集、delete/download 按最近活动栏解析、新建类动作落活动栏。
- **修复（2026-09-06，第 4 轮）**：新 lib `toolbarTarget.ts`（`resolveToolbarTarget` 纯函数，参照 uploadTarget.ts 模式）+ App `activeSide` 活动栏状态（FileTable 选择/焦点行/排序/右键、路径跳转/上一级/刷新、树导航、连接切换均记账）；工具栏「有所选」取两栏并集（单栏只看左栏，防收起双栏后的幽灵选中集），下载/删除/新建文件夹按活动栏路由（`toolbarSelectionEntries` 按栏解析选择集），`downloadSelection(side)` 按栏取池；上传不受 activeSide 影响（P1-5「上传=传向远端」语义保持，仍走 `resolveUploadTarget`）。复验：右栏选中一项后工具栏「下载/删除所选」立即可用，工具栏新建文件夹落右栏（右栏出现、左栏无残留）。新增 `toolbarTarget.spec.ts` 6 例。✅ 已修复并复验。

#### P2

**R3-P2-1 只读态下跨栏「复制」不受写门禁约束**
- 位置：桥按钮 `:title="t('copyToTarget')" :disabled="!selection.length"`（无 `!canWrite`，同排 move 按钮有）；批量菜单 `copySelected` 同样只判 `dualPane`。
- 复现（`?ro=1` 实测）：桥按钮组中「复制到目标栏」enabled、「移动到目标栏/源栏」disabled；多选右键「复制到目标栏」同样可用。
- 影响：复制进只读存储与只读徽章承诺矛盾；真实后端会拒绝，前端却放行提交。建议与 move 同门禁（copy 的写发生在目标栏）。
- **修复（2026-09-06，第 4 轮）**：桥按钮「复制到目标栏/复制到来源栏」disabled 补 `|| !canWrite`；批量右键「复制到目标栏」改 `v-if="dualPane && canWrite"`；`transferBetween` 入口对 copy/move 统一 `canWrite` 门禁（写发生在目标栏，与 move 同闸）。复验（`?ro=1`）：选中左栏条目后「复制到目标栏」与「移动到目标栏」均 disabled。✅ 已修复并复验。

**R3-P2-2 mock `files/delete`/`files/purge` 不按 connectionId 路由：本地栏删除假成功**
- 位置：`mockHost.ts` 两 case 仍写死 `tree`（远端树）；mkdir/write/rename/copy/move 已按连接路由（P2-13②/P1-5 收口），唯独 delete/purge 漏掉。
- 复现（实测）：左栏（`__local__`）播种文件/目录 → 删除 → 通知成功、弹层关闭，刷新后条目原样存在；stat 确认本地树与远端树均未变。
- 影响：双栏左栏的删除旅程在浏览器验证中「成功即无效」，与 P1-5 修复前的上传假成功同构；也阻碍 `?ro=1` 之外的本地面操作走查。夹具缺口（同 P2-13 族），建议补路由。
- **修复（2026-09-06，第 4 轮）**：`mockHost.ts` `files/delete`/`files/purge` 改按 `connectionId` 路由（`treeFor(p.connectionId)` / `deleteEntry(path, treeFor(...))`），与 mkdir/write/rename/copy/move 的既有收口对齐。复验：左栏（`__local__`）播种文件→工具栏删除→通知后条目真实消失、本地树确认不含该条目。新增 `mockHost.spec.ts` 3 例（delete/purge 按连接路由 + 默认连接远端树回归）。✅ 已修复并复验。

**R3-P2-3 文件名排序非自然序（数字字典序）**
- 位置：`sorting.ts` `localeCompare` 未传 `{ numeric: true }`。
- 复现（实测播种 `a2/a10/A3/B/b1/文件9/文件10`）：默认升序为 `文件10 < 文件9 < a10 < a2 < A3`。FileZilla/Finder 惯例为自然序 `a2 < a3 < a10`。
- 影响：10k 序号文件（日志切片、分卷）浏览时次序错乱，与用户心智不符。建议 `a.name.localeCompare(b.name, undefined, { numeric: true, sensitivity: "collation" })` 单点修复。
- **修复（2026-09-06，第 4 轮）**：`sorting.ts` `compareEntries` 名称比较改 `localeCompare(b.name, undefined, { numeric: true })`（数字感知自然序；报告建议中的 `sensitivity: "collation"` 并非 `localeCompare` 合法取值，未采用）。复验：播种 a2/a10/A3 → 升序 a2 < A3 < a10。`sorting.spec.ts` 补自然序用例。✅ 已修复并复验。

**R3-P2-4 文件名零校验：`/`、`..` 可作为名字创建（`a/b` 隐形、`..` 条目可见可操作）**
- 复现（实测）：新建文件夹名 `a/b` → 提交成功、无横幅，列表**不显示**该条目（children 按 `/` 切层过滤，父段 `a` 不存在）；名 `..` → 列表出现可点击、可选中、可删除的「..」目录条目。
- 影响：`a/b` 隐形成功让用户以为操作丢失；`..` 条目在文件管理器属反模式（误点击后导航/删除行为不可预期，真实后端语义依赖实现）。建议提交前校验：名字不得含 `/`、不得为 `.`/`..`（`newFolder/newFile/rename` 三分支共用校验）。
- **修复（2026-09-06，第 4 轮）**：新 lib `fileName.ts`（`validateFileName`：trim 后空→empty、含 `/` 或 `\`→slash、`.`/`..`→dot），`App.vue` newFolder/newFile/rename 三分支提交前统一 `checkConfirmName()`；命中行内提示（ConfirmDialog 新 `warning` prop，`role="alert"`，新样式 `.wb-dialog-warning`）且弹层保持打开、草稿不丢，可直接改名重提。七语新键 `fileNameRequired`/`invalidFileName`。复验：`a/b`、`..` 均行内拦截不提交，合法名正常创建。新增 `fileName.spec.ts` 5 例、`ConfirmDialog.spec.ts` warning 1 例。✅ 已修复并复验。

**R3-P2-5 复制/移动目标冲突无预检无确认：静默覆盖**
- 复现（实测）：复制 `a2.txt` 到已存在的 `B.txt` → 无横幅无确认弹层，通知「已开始」，目标内容被覆盖（stat 确认）。跨栏拖拽/`transferBetween`、`move` 同路。
- 影响：rename/newFolder/newFile 已有 `pathExists` 预检（P2-10），copy/move/跨栏拖拽没有——同名即覆盖可能造成数据丢失。建议复用 `pathExists` 预检 + 覆盖确认弹层（或后端冲突语义透出后前端确认）。
- **修复（2026-09-06，第 4 轮）**：双层收口——① 弹层 copy/move 提交前 `pathExists` 预检（沿用 P2-10 通道），命中后弹层转危险态、确认钮变「覆盖」（`overwriteAsk` 七语），二次确认才执行；确认后草稿再改动则重新预检（`confirmForce`/`confirmForcePath`）。② 跨栏 copy/move（含拖拽）`transferBetween`：目标目录一次 list 取同名集合（`findTargetConflicts`，避免逐条 stat 的万级开销），命中即整批挂起（`pendingPaneTransfer`）弹覆盖确认（`overwriteBatch` 七语），确认后经 `executePaneTransfer` 原样执行。七语新键 `overwrite`/`overwriteAsk`/`overwriteBatch`。复验：复制左栏 overwrite-probe.txt 到右栏 /docs 同名条目 → 弹「覆盖」确认 → 确认后目标内容变为左栏版本。✅ 已修复并复验。

**R3-P2-6 过滤无匹配时误显示「此文件夹为空」**
- 复现（实测）：搜索框输入无匹配关键字 → 空态文案为「此文件夹为空」、计数 0 项。目录明明非空，文案误导（应区分「无匹配结果」并给清空过滤入口）。建议 `FileTable` 增加 `is-filtered` 空态（新 i18n 键）。
- **修复（2026-09-06，第 4 轮）**：`FileTable` 新 `filtered` prop，空态区分两态——过滤态显示 `noMatchResults`（「没有符合过滤条件的条目」）、真空目录保持 `emptyDirectory`；App 按过滤框非空传参（左右栏各自）。七语新键 `noMatchResults`。复验：过滤无匹配显示新文案，清空过滤后列表恢复。`FileTable.spec.ts` 补 2 例。✅ 已修复并复验。

**R3-P2-7 zh-TW 语言包简体残留：`move: "移动"`（应为「移動」）**
- 复现（实测 `?locale=zh-TW`）：目录右键菜单显示「移动…」，其余相邻项（移動到目標欄/移動到來源欄）均为繁体。全块扫描仅此一处简体残留（i18n.ts zh-TW 段 `move` 键）。同报七语约定（规约#1）。
- **修复（2026-09-06，第 4 轮）**：i18n.ts zh-TW 段 `transferKind.move` 「移动」→「移動」（最小改动，仅此一键）。复验（`?locale=zh-TW`）：目录右键菜单显示「移動…」，全菜单无简体残留。✅ 已修复并复验。

**R3-P2-8 文件列表 a11y 语义缺失（表格/排序/复选框/菜单/lang）**
- 实测结构：`wb-file-row` 无 role（无 grid/table/option 语义，屏幕阅读器读不出行/列）；表头排序按钮无 `aria-sort`；行复选框无 aria-label（读作「未命名复选框」）；三个右键菜单无 `role="menu"`/`menuitem`；文件滚动容器无 role/aria-label（键盘可达但 AT 不可知）。另 `document.documentElement.lang` 恒为 mock.html 写死的 `"en"`，locale 切 zh/ja 后不更新，屏幕阅读器按英语音素读中文。对比度无问题（次要文字 6.4:1）。建议按 WAI-ARIA treegrid/listbox 模式补语义 + locale 变更时同步 `lang`。
- **修复（2026-09-06，第 4 轮）**：`FileTable` 表头 role=row + 各列 role=columnheader + `aria-sort`（ascending/descending 跟随排序状态，非活动列省略）；滚动容器 role=listbox + `aria-multiselectable` + `aria-label`（新键 `fileListLabel`）；行 role=option + `aria-selected`；行复选框 `aria-label`（新键 `selectEntry`，含文件名）。App 三个右键菜单补 role=menu、全部按钮 role=menuitem；App watch(locale)（immediate）同步 `document.documentElement.lang`。七语新键 `selectEntry`/`fileListLabel`。复验：listbox/option/aria-selected/aria-sort/checkbox aria-label 全命中、菜单 role=menu（10 个 menuitem）、lang 随 locale=zh-CN/zh-TW 正确切换；`FileTable.spec.ts` 补 a11y 2 例。✅ 已修复并复验。

**R3-P2-9 批量删除（本地伪 job）无取消能力：取消按钮假成功**
- 位置：`runBatch` 无中断机制；传输面板对 running 态 job 一律渲染取消按钮；`cancelTransfer` 对 `local-batch-*` 调 `files/transfer/cancel`，mock（及真实 sidecar）无此 job 记录仍返回 success → 提示「已取消」，任务继续跑完。
- 影响：万级批量删除（实测 2.2s，真实远端分钟级）期间取消无效且被误报成功。建议 runBatch 支持取消标志（worker 循环检查），伪 job 的 cancel 直接本地置 failed/canceled。
- **修复（2026-09-06，第 4 轮）**：新 lib `batchRunner.ts`（`runBatchTasks`：固定并发 + `isCanceled` 检查点，取消后 worker 不再领取新分片，在跑的等待自然结束）；App `runBatch` 改走 batchRunner 并登记 `batchCancelFlags`（jobId→flag），`cancelTransfer` 对 `local-batch-*` 直接翻转本地标志并置 job `canceled` 终态——不再调 `files/transfer/cancel`（sidecar 无该记录仍返回 success 的假成功路径）；已取消时不补「已删除」通知。七语无新增（复用 jobCanceled/transferStatus.canceled）。复验：/10k 全选删除（注入 5ms/条延迟打开取消窗口）→ 传输面板取消 → job 状态「已取消」、剩余 9160/10000 未删（真实中断）。新增 `batchRunner.spec.ts` 4 例（并发上限/取消中断/错误收集/空批次）。✅ 已修复并复验。

**R3-P2-10 图标体系统一漏网：错误横幅与传输重试按钮仍用文本字符 `↻`/`✕`**
- 位置：`App.vue` 错误横幅重试 `↻`、关闭 `✕`；`TransferPanel.vue` 历史重试按钮 `↻`（取消/清空历史已换 lucide，P2-14 只收口了这两处）。与工具栏 lucide SVG 体系不统一，跨平台字形不一致。
- **修复（2026-09-06，第 4 轮）**：App 错误横幅 ↻/✕ 换 lucide `RefreshCw`/`X`（补 title，关闭钮用既有 `close` 键）；TransferPanel 历史重试 ↻ 换 lucide `RotateCw`。复验：错误横幅两按钮均渲染 SVG；`?job=1` 注入目标含 fail 的复制失败后，传输历史重试按钮渲染 SVG。✅ 已修复并复验。

### 三、本轮验证无问题的维度（含测法）

| 维度 | 测法与结论 |
| --- | --- |
| 性能/压力 | `?mock=1` 默认参：10k 目录列表加载 273ms（可见 50 行虚拟滚动）；过滤逐键 40ms/键（10k 全量过滤）；Ctrl+A 全选 10000 项 13ms；End 跳底 310ms 且滚入行正确；双栏同时浏览两个 10k 目录 554ms、左右滚动互不干扰；10k 批量删除（并发 8 + 进度落面板）2.2s 完成、终态后空态/计数正确；heap 稳定在 20–22MB，无泄漏迹象。全部无问题 |
| 键盘全流程 | 方向键→Shift 扩选→Space 勾选（首按生效）→Home/End（焦点行滚入视口）→Enter 打开，全链路实测通过；Ctrl+A 两栏各自生效 |
| 拖拽跨栏 | DataTransfer 注入实测：左栏拖 `drag-probe.txt` 放到右栏 `/docs` → 落地成功、通知「已向对侧发起 1 项传输」；drop 遮罩（「松开以复制到此处」）dragenter 出现/dragleave 消失；方向语义与 P1-5 定义的「上传=传向远端」一致 |
| 传输失败重试链 | `?job=1` + 目标含 `fail`：job 五态/失败原因展示正常，失败项「重试传输」按钮可用，重试产生新 job 并再次落到失败态（登记参数原样重发） |
| 防重 | 删除确认按钮双击：busy 禁用生效、弹层仅出现一次、后端仅一次删除（stat 验证）；刷新按钮 busy 期禁用（disabled 标志实测翻转）；重试按钮有 `retryTransferBusy` 防抖（代码审查） |
| 只读矩阵（`?ro=1`） | 只读徽章、新建/上传/删除禁用、右键写项隐藏（syncDir/copyDir/压缩/复制/移动/重命名 v-if canWrite）、空白区新建禁用、移动桥按钮禁用——均正确；唯一漏网见 R3-P2-1 |
| 文件名边界（其余） | emoji+中文+空格名创建/显示/面包屑正常；前导点 `.hidden-dir` 正常创建与显示；200 字符极长名正常；首尾空格名 trim 后落盘（合理）；空文件预览（0 B 正常展示+编辑入口）；面包屑⇄路径编辑往返一致（含 emoji 路径与尾斜杠输入） |
| 归档流 | 压缩包内容列表（backup.zip → 3 条目）正常；压缩 `/docs → /docs.tar.gz` 默认名/落盘/通知正常；解压在方法未实现时按设计给「该能力暂不可用：files/extract」+ title 原文 |
| P1 抽查（前 2 轮收口项复核） | P1-1 面包屑无双斜杠（根段 `/` 单独渲染）✓；P1-3 弹层打开即聚焦输入框、Tab 8 次不出弹层、Esc 后焦点归还触发钮 ✓；P1-5 双栏上传落右栏远端目录、通知带目标路径（「已上传 1 个文件到 /docs」）、左栏无残留 ✓ |
| i18n（ja/zh-TW 抽查） | ja-JP：工具栏 8 项/右键菜单 10 项/dock 三 tab/传输空态/上一级/刷新/编辑路径全部成句无漏翻；zh-TW：同位抽查除 R3-P2-7 的 `move` 外全部为繁体；en-US 对照正常 |
| 对比度 | dark 下次要文字/页脚 `rgb(151,152,157)` on `rgb(19,20,22)` = 6.4:1（AA 过线）；图标按钮 title 覆盖率 100%（0 个缺失） |

### 四、本轮统计与说明

- 新发现：P0 × 0、P1 × 2（R3-P1-1/2）、P2 × 10（R3-P2-1～10）；零删除前 2 轮任何条目。
- 测试脚本与运行日志均在 `/tmp/uiscan-files-r3`（未入库），未拍摄留存截图；未改任何源码，未提交 git。
- 夹具缺口（R3-P2-2）虽记在 mock 层，但其暴露面是「双栏左栏写操作在浏览器验证中不可自证」，建议随下一轮夹具收口一并处理。

## 六、第 5 轮（复核扫描，2026-09-06）

> 扫描角色：文件管理/存储工程师 + 软件测试专家（收敛判定轮）。
> 双重任务：① 逐条复验第 4 轮 12 项修复；② 换此前未覆盖的角度找新问题
> （跨栏长距离复制、多任务并发传输、大文本预览内存/焦点、排序连按、
> i18n 运行时切换动态内容）。只读扫描，未改任何代码、未提交 git。

### 一、本轮环境

| 项 | 值 |
| --- | --- |
| Dev server | 5293 端口复用当日遗留 vite（PID 57597，实时读盘与新建等价；本轮结束已 kill） |
| 自动化 | playwright-core + 系统 Chrome（`channel: "chrome"`），独立装于 `/tmp/uiscan-files-r5`（未进项目依赖） |
| 脚本 | t1（P1 双项）/ t2+t2b（P2-1/2/3/10）/ t3（P2-4/5/6）/ t4（P2-7/8/9）/ t5（跨栏长距离+并发+拖拽）/ t6（大文本预览+上传取消+i18n 切换） |
| 断言 | 复核 87 条 + 新角度 32 条（含 5 条「发现记录」型探针，FAIL 即实锤）；另有 3 条脚本自身缺陷（断言方向写反、macOS Ctrl+click 变右键、kind 文本带时间后缀解析）已修正后重验 |
| 数据播种 | 经 `window.dbxPlugin.invoke` 真实写入 mock 树（含 1.5MB 大文本、300 项待删目录）；竞态/取消窗口用页面内 monkey-patch `window.dbxPlugin.invoke` 注入延迟（不改源码） |

### 二、第 4 轮修复复核结论（12/12 ✅）

| 条目 | 复验要点与实测证据 | 结论 |
| --- | --- | --- |
| R3-P1-1 导航竞态守卫 | `delay=200` + monkey-patch 对 `/docs` 的 `files/list` 注入 1500ms：右栏先双击 /docs、400ms 后双击 /10k，终态面包屑 `/10k`、列表全为 `file-*.txt`、无 readme.md，晚到响应不再覆盖 | ✅ |
| R3-P1-2 工具栏按活动栏路由 | 右栏选中 1 项 → 下载/删除立即解禁；工具栏新建落活动栏右栏；**键盘导航记账正确**：右栏列表聚焦 + ArrowDown 后新建落右栏（方向键 emit selection/activePath → markActiveSide 链路闭合）；上传不受影响（P1-5 语义保持） | ✅ |
| R3-P2-1 只读 copy 门禁 | `?ro=1`：四个桥按钮全 disabled；批量右键菜单（Shift 连选 2 项）无「复制到目标栏/移动到目标栏」项（整项 v-if 隐藏）、删除项 disabled | ✅ |
| R3-P2-2 mock delete 路由 | 左栏（`__local__`）播种文件/目录 → UI 删除 → stat 确认本地树真实删除（NotFound）、远端树未受牵连；目录走 purge 同样路由 | ✅ |
| R3-P2-3 自然序 | 播种 a2/a10/A3/B/b1/文件9/文件10 → 升序 `a2 < A3 < a10`、`文件9 < 文件10`（numeric collation 生效） | ✅ |
| R3-P2-4 文件名校验 | `a/b`、`..`、纯空格 均行内拦截（role=alert）、弹层保持打开、草稿不丢；rename 分支同样拦截；合法名正常创建 | ✅ |
| R3-P2-5 覆盖确认 | ① 弹层内 copy 到已存在名：预检命中 → 弹层转危险态、确认钮变「覆盖」、二次确认才执行、内容实测被覆盖；② 跨栏冲突：桥复制同名 → 整批挂起弹「目标位置已存在 1 个同名条目」，取消不覆盖、确认后目标变本地版本；③ **不阻断正常路径**：无冲突的弹层内复制与跨栏复制均一次确认直接执行（不弹覆盖） | ✅ |
| R3-P2-6 过滤空态 | 过滤无匹配 → 「没有符合过滤条件的条目」（role=status）；清空恢复；真空目录 `/empty` 保持「此文件夹为空」；左右栏对称 | ✅ |
| R3-P2-7 zh-TW 移動 | `?locale=zh-TW`：目录右键菜单显示「移動…」、无简体「移动」残留；lang 同步 `zh-TW` | ✅ |
| R3-P2-8 a11y | 表头 role=row/columnheader、活动列 `aria-sort=ascending` 非活动列省略；listbox + aria-multiselectable + aria-label（「文件列表」）；行 role=option + aria-selected；复选框 aria-label 含文件名；菜单 role=menu（10 个 menuitem）；lang 随 locale 即时同步 | ✅ |
| R3-P2-9 批量取消 | /10k 全选删除（8ms/条注入）→ 面板删除任务运行中 → 取消 → 任务置「已取消」、取消钮消失、无「已删除」通知、远端实测剩 9266 项（真实中断）；并发场景复核：300 项删除取消后 remain=141，上传/下载不受波及 | ✅（附带 2 项新发现，见 R5-P2-2/3） |
| R3-P2-10 图标 | 错误横幅两按钮均渲染 lucide SVG（2/2）；`?job=1` 失败任务的历史重试按钮为 SVG | ✅ |

复核中未发现任何「修复引入新问题」：覆盖确认不阻断无冲突复制（本条目③）、键盘导航下活动栏记账正确、批量取消后传输面板状态正确（已取消态进历史、不补已删除通知）均专项实测通过。

### 三、新发现清单

统计：**P0 × 0，P1 × 0，P2 × 8**。无阻断使用级问题；全部为打磨项、边缘场景或夹具缺口。

**R5-P2-1 跨栏移动（左→右）后源栏列表不刷新：UI 残留已移走条目**
- 位置：`App.vue` `executePaneTransfer`——刷新条件不对称：`if (to === "left") loadDirectory() else loadRightDirectory()` + `if (move && from === "right") loadRightDirectory()`。右→左移动时源栏（右）被第二个条件覆盖；**左→右移动时源栏（左）没有任何刷新路径**。
- 复现（实测）：双栏，左栏 `/Users/demo/Documents` 选中 5 个新文件 → 桥「移动到目标栏」→ 后端确认源已删（stat NotFound）、目标 `/docs` 已落，但**左栏列表仍显示这 5 条**；点击残留条目再操作即 NotFound。
- 影响：数据不一致级体验债（copy 方向源栏本就不该刷新、右→左移动正确，唯独左→右移动漏刷新）；用户易对残留条目重复操作。
- 建议：`executePaneTransfer` 末尾对 `move` 无条件双刷两栏（或按 from/to 各刷一次源栏与目标栏）。

**R5-P2-2 批量删除期间确认弹层保持 busy 打开，遮罩挡住传输面板取消钮**
- 位置：`App.vue` `onConfirm` delete 分支 `await runBatch(...)` 完成后才 `closeConfirm()`；ConfirmDialog backdrop 为全屏遮罩。
- 复现（实测）：万级/数百项批量删除（注入延迟）确认后，弹层 busy 态持续整个批次时长，面板取消钮被 backdrop 拦截（playwright click 被 `wb-dialog-backdrop` intercepts，鼠标不可达）；需先 Esc（或点遮罩）关掉 busy 弹层才能点面板取消——关闭后批量仍在跑、取消依然有效（语义正确），但该路径隐蔽。
- 影响：R3-P2-9 的取消能力在长批次期间实际「藏」在弹层后面；用户感知为「只能干等」。
- 建议：delete 分支提交后立即 `closeConfirm()`（进度已落传输面板，弹层无需等待）；或 busy 弹层显示「后台执行中，可去传输面板取消」提示。

**R5-P2-3 批量删除传输任务的文件计数被渲染成字节形态**
- 位置：`TransferPanel.vue` `progressMeta`——非 byte-based 分支兜底 `${formatBytes(job.transferred)} / ${formatBytes(job.size)}`；delete 伪 job 的 `transferred/size` 实为**文件个数**（runBatch 写入 filesDone/filesTotal 的同时写入了 transferred/size）。
- 复现（实测）：批量删除任务进度显示「删除 **504 B / 9.8 KiB** · 5%」「删除 · **686 B / 9.8 KiB**」——把 10000 个文件格式化成 9.8 KiB。
- 影响：进度语义误导（7% 与 686 B 并存）；`filesProgress` 键（N/M 项）只服务于 byte-based 分支，delete 用不上。
- 建议：`progressMeta` 对含 `filesTotal` 的 job 优先走 filesProgress 文案（与 isByteBased 解耦），计数不走 formatBytes。

**R5-P2-4 上传/下载任务取消假成功（R3-P2-9 同族漏网）**
- 位置：`App.vue` `uploadSource`/`downloadEntry` 的传输泵无取消检查点；`cancelTransfer` 对 `mock-upload-*`/`mock-download-*` 调 `files/transfer/cancel`——mock（及可能的真实 sidecar）无该记录仍返回 success；TransferPanel 对所有 running 任务渲染取消钮。
- 复现（实测，`upload/finish` 注入 3s 延迟）：上传运行中点取消 → 通知「传输已取消」→ 任务最终置「**已完成**」且文件真实落盘。下载泵同理（无中断机制）。
- 影响：与 R3-P2-9 修复前同构的假成功反馈；用户以为已停止，实际继续消耗带宽/写盘。
- 建议：upload/download 泵增加本地取消标志（对齐 batchRunner 的 isCanceled 检查点），取消时本地置 canceled 终态并终止泵循环。

**R5-P2-5 拖拽跨栏不记账活动栏（R3-P1-2 漏网入口）**
- 位置：`App.vue` `onDropTo`（drop 目标侧不 `markActiveSide`）；`FileTable.onDragStart`（拖拽源侧同样不记账）。
- 复现（实测）：点击左栏行（activeSide=left）→ 从左栏拖文件放到右栏 → 立即点工具栏「新建文件夹」→ **落在左栏**（用户最后交互的是右栏拖放目标）。键盘导航/选择/排序/右键的记账均正确（T1 实测），拖拽是唯一漏网入口。
- 影响：低频但真实的错侧写操作（新建落错栏、下载/删除解析错选择集）。
- 建议：`onDropTo` 入口 `markActiveSide(side)`（drop 目标即用户当前关注侧）。

**R5-P2-6 预览弹窗关闭后焦点落 BODY，不归还触发行**
- 位置：`PreviewPane.vue`（P1-4 修复只做了打开时焦点入容器，未做关闭归还）；对照 ConfirmDialog 已有 `returnFocusTo` 机制。
- 复现（实测）：键盘/鼠标打开大文本预览 → Esc 关闭 → `document.activeElement` 为 **BODY**，键盘用户丢失列表位置，需重新 Tab 导航。
- 影响：键盘可达性断点（预览是高频动作）；与弹层类的焦点归还标准不一致。
- 建议：PreviewPane 记录打开前 activeElement，close 时归还（照抄 ConfirmDialog 方案）。

**R5-P2-7 确认弹层打开中途切换 locale：标题/正文不换语，与按钮形成混语言窗口**
- 位置：`App.vue` `openConfirm`——`confirmTitle/confirmBody` 在打开时刻用 `t()` 固化为字符串；而 `confirmLabel`/`confirmDangerList` 是 computed（即时换语）。notice 提示条同理（4s 生命周期内不换语）。
- 复现（实测，运行时切语通道）：en-US 下打开新建文件夹弹层 → 切 ja-JP → 标题停留「New folder」、取消钮已变「キャンセル」。弹层外一切（工具栏/传输面板状态与类型标签/html lang）均即时换语正确。
- 影响：边缘场景（真实宿主 locale 切换多发生在弹层关闭时）；纯一致性打磨。
- 建议：`confirmTitle/confirmBody` 改存 i18n key + 参数（渲染时求值），或接受现状并记录为已知限制。

**R5-P2-8 mock `files/download/start` 不按 connectionId 路由（R3-P2-2 同族夹具缺口）**
- 位置：`mockHost.ts` download/start 写死查远端 `tree`（delete/purge/copy/move/rename/mkdir/write/upload 均已按连接路由，唯独 download 漏）。
- 复现（实测）：双栏左栏（`__local__`）本地面文件右键「下载」→ 横幅 `NotFound: /Users/demo/Documents/notes-local.txt`，本地面下载旅程在浏览器验证中不可用。
- 影响：夹具缺口，阻碍「本地面 → 下载」旅程自证；与真实 sidecar 契约的形状差异需在下次夹具收口时一并对齐。
- 建议：download/start 改 `treeFor(p.connectionId).get(...)`（与 files/read 同款一行收口）。

### 四、零发现维度证据（本轮新覆盖角度中表现良好项）

| 维度 | 测法与结论 |
| --- | --- |
| 跨栏长距离路径复制 | 双栏各自深处导航（左 `/Users/demo/Documents`、右 `/docs`）后 Ctrl+A 30 项桥复制：30/30 落地、通知计数准确（31=30 文件+1 目录文件同批）、源栏保留、两栏列表与后端一致——结果与进度语义正确（native 路径即时完成无逐项进度，符合设计） |
| 多任务并发传输 | `?mock=1` 删除（80ms/条注入）+ 上传 + 下载同时跑：面板三类任务并存、进度/终态互不串扰；**取消精确命中删除任务**（已取消），上传/下载继续到已完成、文件真实落盘；删除真中断（remain=141 > 1）。传输面板并发隔离无问题（取消假成功问题单列 R5-P2-4） |
| 大文本（1.5MB）预览 打开→编辑→取消→重开 | 每阶段 `.cm-editor` 恒为单实例（编辑/只读重建均正常销毁）；打开焦点在预览容器内、编辑态 CodeMirror 自聚焦（P1-4 复验通过）；取消后草稿不写回（重开无编辑标记）；heap 25→29→33→39 MiB（无强制 GC，未见泄漏迹象，CodeMirror destroy 链路正常）；「关闭后焦点落 BODY」单列为 R5-P2-6 |
| 快速连按排序表头 5 次 | 40ms 间隔连按 name 表头：状态确定性翻转（asc→desc→…第 5 次落 desc）、行序与 `aria-sort` 一致、左栏排序实时持久化到 localStorage（`{"column":"name","direction":"desc"}`）、零 pageerror；右栏排序独立不联动（P2-12 无回归） |
| i18n 运行时切换动态内容 | 注入 `onLocaleChange` 通道切 en-US/ja-JP：html lang 即时同步、工具栏/弹层按钮/传输面板状态与类型标签即时换语、传输标题语言无关（路径原样）；仅弹层标题/正文固化导致混语言（单列 R5-P2-7） |
| 拖拽跨栏链路（回归） | DataTransfer 注入 left→right：落地成功、drop 遮罩正常；仅活动栏记账缺口（单列 R5-P2-5） |
| 上传链路（回归） | 大文件（300KB）upload/start→分片→finish 全链路、落盘按右栏远端目录（P1-5 语义）、传输面板任务进度正常 |

### 五、本轮统计与收敛判定

- 修复复核：**12/12 ✅**（第 3 轮报告标注的全部条目闭环确认，未发现修复引入的新问题）。
- 新发现：**P0 × 0、P1 × 0、P2 × 8**（R5-P2-1～8）；零删除历史条目。
- 收敛判定：**结构性问题已清零**——连续两轮 P0/P1 为零，第 4 轮修复全部经端到端复验成立；本轮 8 条 P2 中 4 条为第 4 轮修复的「配套打磨」（R5-P2-1/2/3/4/5 均产生于 batch/copy-move/activeSide 修复的邻接面），2 条为键盘/a11y 一致性尾项（R5-P2-6/7），1 条为夹具缺口（R5-P2-8）。**扫描趋于收敛，可按 P2 清单择机收口后结束轮次**；若按「零新发现」口径严格要求，尚不宣告收敛。
- 测试脚本与运行产物在 `/tmp/uiscan-files-r5`（未入库），截图本轮零留存；未改任何源码，未提交 git。
