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
