<script setup lang="ts">
import { computed, nextTick, onBeforeUnmount, onMounted, reactive, ref, watch } from "vue";
import { ArrowLeft, ArrowRight, Copy, ArrowUp, RefreshCw, Search, X } from "@lucide/vue";
import FileTable from "./components/FileTable.vue";
import FileToolbar from "./components/FileToolbar.vue";
import TransferPanel from "./components/TransferPanel.vue";
import ConfirmDialog from "./components/ConfirmDialog.vue";
import AuditPanel from "./components/AuditPanel.vue";
import PreviewPane from "./components/PreviewPane.vue";
import CustomConfigEditor from "./components/CustomConfigEditor.vue";
import PathField from "./components/PathField.vue";
import SideNavPanel from "./components/SideNavPanel.vue";
import { isDbxPluginTheme, onHostThemeChange, themeToAppearance } from "./lib/hostTheme";
import { DBX_POPOVER, resolveAppearance, type DbxPluginAppearanceInput } from "./lib/appearance";
import { bridgeBinaryBytes } from "../../../shared/frontend/binaryEvent";
import {
  bindApi,
  baseName,
  call,
  errorMessage,
  isMethodMissing,
  joinPath,
  normalizeEntries,
  parentPath,
  type FileCapabilities,
  type FileEntry,
} from "./lib/api";
import { createTransferTracker, isActive, isRetryableKind, type TransferJob, type TransferKind } from "./lib/transfers";
import { inspect, type DangerousHit } from "./lib/dangerousPaths";
import { workbenchMessage } from "./lib/i18n";
import { isArchivePath } from "./lib/archive";
import { loadUiPrefs, saveUiPrefs } from "./lib/prefs";
import { sortEntries, toggleSortState, type SortColumn, type SortState } from "./lib/sorting";
import { filterEntries } from "./lib/searchFilter";
import { isLargeDirectory } from "./lib/largeDir";
import { applyTreeChildren, createTreeRoot, markTreeStale, type DirTreeNode } from "./lib/dirTree";
import { normalizeQuickPaths, type QuickPath } from "./lib/quickPaths";
import { isNarrowViewport } from "./lib/responsive";
import { resolveUploadTarget, type UploadTarget } from "./lib/uploadTarget";
import { friendlyError, isNotFoundMessage, isTransportFailure } from "./lib/friendlyError";
import { createNavGuard } from "./lib/navGuard";
import { resolveToolbarTarget } from "./lib/toolbarTarget";
import { validateFileName } from "./lib/fileName";
import { runBatchTasks } from "./lib/batchRunner";

type ConfirmKind = "delete" | "purge" | "syncDir" | "copyDir" | "newFolder" | "newFile" | "rename" | "copy" | "move" | "extract" | "compress" | "overwrite";
type PaneSide = "left" | "right";
type MenuAction =
  | "open" | "preview" | "download" | "rename" | "delete" | "copyPath" | "copyName"
  | "syncDir" | "copyDir" | "copy" | "move" | "extract" | "archiveContents" | "compress"
  // 批量（多选右键，P-FILES 压缩轮）
  | "downloadSelected" | "copySelected" | "moveSelected" | "deleteSelected" | "compressSelected";

interface ConnectionSummary {
  name?: string;
  host?: string;
  username?: string;
  readOnly?: boolean;
  protocol?: string;
  color?: string;
}

// UI 偏好（A-FILES ①/④c）：布局偏好存 localStorage（非敏感）。
const prefs = loadUiPrefs();

const hostContext = ref<Record<string, unknown>>({});
const locale = ref("zh-CN");
const connectionId = computed(() => String(hostContext.value.connectionId || ""));
const connection = computed<ConnectionSummary>(() => {
  const value = hostContext.value.connection;
  return value && typeof value === "object" ? (value as ConnectionSummary) : {};
});
const connectionLabel = computed(() => {
  const host = connection.value.host || connection.value.name || connectionId.value || "storage";
  const user = connection.value.username ? `${connection.value.username}@` : "";
  return `${user}${host}`;
});
// 写权限 = 宿主 context 未标记只读 且 后端策略层未开启只读门禁
// （files/capabilities.readOnly：表单 read_only ∥ 宿主标准 read_only）。
const canWrite = computed(() => !connection.value.readOnly && !capabilities.value?.readOnly);

const t = (key: string, values: Record<string, string | number> = {}) => workbenchMessage(locale.value, key, values);

// R3-P2-8：html lang 跟随 locale——mock.html 写死 lang="en"，locale 切 zh/ja
// 后屏幕阅读器按英语音素读中文；真实宿主下同样由插件侧兜底同步。
watch(locale, (next) => (document.documentElement.lang = next || "zh-CN"), { immediate: true });

// ---- 源栏（左栏）-----------------------------------------------------------
const path = ref("/");
const entries = ref<FileEntry[]>([]);
const selection = ref<string[]>([]);
const activePath = ref("");
const sort = ref<SortState>(prefs.sort);
const loading = ref(false);
// 顶栏连接状态 pill（对标 ssh session-pill）：存储连接（非本地栏）最近一次
// files/list 成败，在 fetchListing 统一挂钩。
const connState = ref<"connecting" | "connected" | "disconnected">("connecting");

const error = ref("");
// P2-1：错误横幅悬停展示 sidecar 原文（friendlyError 映射后的文案为主显示）。
const errorDetail = ref("");
// P2-4：错误所属栏位（重试按出错栏位重放，而不是永远只刷左栏）。
const errorSide = ref<PaneSide | "global">("global");
const notice = ref("");
const capabilities = ref<FileCapabilities | undefined>();
const initialized = ref(false);

// ---- 目标栏（右栏，A-FILES ①）---------------------------------------------
const dualPane = ref(prefs.dualPane);
// 侧栏形态偏好：tree/quick tab（默认 tree）与收起状态，随布局偏好持久化。
const sideTab = ref<"tree" | "quick">(prefs.sideTab);
const sideCollapsed = ref(prefs.sideCollapsed);
const rightPath = ref("/");
const rightEntries = ref<FileEntry[]>([]);
const rightSelection = ref<string[]>([]);
const rightActivePath = ref("");
const rightLoading = ref(false);
/** "" = 与左栏同连接；宿主提供连接枚举时可切换其它连接（cross-connection 走 targetConnectionId）。 */
const targetConnectionId = ref("");
const targetConnections = ref<Array<{ id: string; name: string }>>([]);
const dragOverSide = ref<PaneSide | null>(null);

// ---- 本地文件系统（双栏左栏默认面，对标 tiny-rdm/FileZilla 本地栏）-------------
// sidecar 内置保留连接 `__local__`（engine 合成的 root="/" fs 连接），无需
// 用户建连；双栏开启时左栏默认指向本地，右栏保持当前（远端）连接。
const LOCAL_CONNECTION_ID = "__local__";
/** 左栏连接（仅双栏模式路由）：__local__=本地，""=当前连接，其余=宿主其它连接。 */
const leftConnectionId = ref<string>(LOCAL_CONNECTION_ID);
const leftConnections = computed(() => [
  { id: LOCAL_CONNECTION_ID, name: t("localFiles") },
  { id: "", name: t("sameConnection") },
  ...targetConnections.value,
]);

// ---- 快速目录（tiny-rdm quick paths 对标）------------------------------------
// §8.1：后端按协议/根约束/stat 过滤后返回候选（根目录 + fs 协议的用户目录族）；
// 展示形态为侧栏 quick tab（SideNavPanel），tree tab 为懒加载目录树（默认）。
const leftQuickPaths = ref<QuickPath[]>([]);
const rightQuickPaths = ref<QuickPath[]>([]);

// ---- 侧栏目录树（tree tab）----------------------------------------------------
// 每栏一棵：根 = 连接根目录 "/"，展开时经 files/list 懒加载子目录（仅目录），
// collapse 保留缓存，侧栏刷新按钮 markTreeStale 后重拉根。
const leftTree = ref<DirTreeNode>(createTreeRoot("/", "/"));
const rightTree = ref<DirTreeNode>(createTreeRoot("/", "/"));

async function expandTreeNode(side: PaneSide, node: DirTreeNode) {
  if (node.expanded) {
    node.expanded = false;
    return;
  }
  if (!node.loaded) {
    node.loading = true;
    try {
      const list = await fetchListing(node.path, sideConnectionId(side));
      const tree = side === "left" ? leftTree.value : rightTree.value;
      applyTreeChildren(tree, node.path, list);
    } catch (cause) {
      showError(cause); // 树展开失败要有反馈，不能静默（P-FILES 用户反馈）
    } finally {
      node.loading = false;
    }
    return;
  }
  node.expanded = true;
}

/** tree tab 可见时确保根已展开（两侧各拉一次；quick tab 下不预取）。 */
function ensureTreeRoots() {
  for (const side of ["left", "right"] as const) {
    const tree = side === "left" ? leftTree.value : rightTree.value;
    if (!tree.loaded && !tree.loading) void expandTreeNode(side, tree);
  }
}

function refreshTree(side: PaneSide) {
  const tree = side === "left" ? leftTree.value : rightTree.value;
  markTreeStale(tree);
  tree.expanded = false;
  void expandTreeNode(side, tree);
}

/** 该栏显式使用的连接 id；undefined = 当前连接（由 api 层默认注入）。 */
function sideConnectionId(side: PaneSide): string | undefined {
  if (side === "right") return targetConnectionId.value || undefined;
  // 左栏仅双栏模式按选择路由（默认本地）；单栏即当前连接本体。
  return dualPane.value && leftConnectionId.value ? leftConnectionId.value : undefined;
}

/** files/quickPaths（§8.1）：方法缺失或探针失败时下拉隐藏（旧 sidecar 降级）。 */
async function loadQuickPaths(side: PaneSide) {
  try {
    const params: Record<string, unknown> = {};
    const explicit = sideConnectionId(side);
    if (explicit) params.connectionId = explicit;
    const result = await call<{ paths: QuickPath[] }>("files/quickPaths", params);
    const list = normalizeQuickPaths(result.paths);
    if (side === "left") leftQuickPaths.value = list;
    else rightQuickPaths.value = list;
  } catch {
    /* 方法缺失或探针失败：隐藏下拉（旧 sidecar 降级） */
  }
}

// R3-P1-1：按栏请求序号守卫——慢响应晚到不得覆盖新导航（先点慢 /docs 再点
// 快 /10k，晚到的 docs 响应会把面包屑/列表/选中态整体拖回 /docs）。响应到达
// 时验号，过期序号的结果（含错误与 loading 收尾）一律丢弃。
const leftNav = createNavGuard();
const rightNav = createNavGuard();

function navigateQuickPath(side: PaneSide, targetPath: string) {
  markActiveSide(side);
  if (side === "left") void loadDirectory(targetPath).catch(() => undefined);
  else void loadRightDirectory(targetPath).catch(() => undefined);
}

const dockOpen = ref(true);
const dockTab = ref<"transfers" | "audit" | "connection">("transfers");
const auditRef = ref<InstanceType<typeof AuditPanel>>();

const previewPath = ref<string | null>(null);
/** 预览条目所属栏连接：openPreview 时固化为快照——单栏预览会顺手开启双栏
 * （左栏随即切到本地），read/write 必须仍指向预览来源连接而非切换后的左栏。 */
const previewConnectionId = ref<string | undefined>(undefined);
const contextMenu = ref<{ x: number; y: number; entry: FileEntry; side: PaneSide; selection: string[] }>();
// 空白区右键（P-FILES）：列表空白处不再弹浏览器菜单，改弹新建/刷新动作面。
const blankMenu = ref<{ x: number; y: number; side: PaneSide }>();
// 侧栏（目录树/快捷目录）行右键：打开 / 在另一栏打开 / 复制路径、文件名。
const sideMenu = ref<{ x: number; y: number; side: PaneSide; path: string; name: string }>();
// 三个右键菜单互斥，共用同一模板 ref；渲染后按视口钳位，
// 避免右键屏幕边缘时菜单溢出被裁。
const menuEl = ref<HTMLElement>();
watch([contextMenu, blankMenu, sideMenu], async () => {
  await nextTick();
  const element = menuEl.value;
  const current = contextMenu.value ?? blankMenu.value ?? sideMenu.value;
  if (!element || !current) return;
  const rect = element.getBoundingClientRect();
  if (rect.right > window.innerWidth - 8) current.x = Math.max(8, window.innerWidth - rect.width - 8);
  if (rect.bottom > window.innerHeight - 8) current.y = Math.max(8, window.innerHeight - rect.height - 8);
});

/** 该栏当前所在目录（新建文件夹/新建文件落点）。 */
function paneDirPath(side: PaneSide): string {
  return side === "left" ? path.value : rightPath.value;
}

const tracker = createTransferTracker();
const transferJobs = computed(() => Object.values(tracker.jobs));
// P2-5：审计面板写操作后自动刷新（面板开着才刷；另有面板内手动刷新钮）。
function refreshAuditPanel() {
  if (dockOpen.value && dockTab.value === "audit") auditRef.value?.refresh();
}
// P-FILES ①b：等待终态后刷新目录的 job 集（transport=job 的 copy/move/rename）。
const awaitingRefresh = new Set<string>();

// P-FILES ⑦：失败任务重试——提交时登记原始请求（方法 + 栏位 + 参数），
// 失败后 TransferPanel 的 ↻ 按原样重发。会话级 Map（store 里的历史记录
// 无参数形态，跨会话的历史任务不显示重试按钮）。
interface TransferRetryParams {
  method: string;
  side: PaneSide;
  params: Record<string, unknown>;
}
const transferRetryParams = new Map<string, TransferRetryParams>();

/** 登记 sidecar 侧异步 job（提交后 jobId 已返回，首个进度事件未到达前的占位）。 */
function trackSidecarJob(jobId: string, kind: TransferKind, remotePath: string, retry?: TransferRetryParams) {
  registerJob({
    jobId,
    connectionId: connectionId.value,
    kind,
    remotePath,
    state: "queued",
    size: 0,
    transferred: 0,
    updatedAt: Date.now(),
  });
  if (retry) transferRetryParams.set(jobId, retry);
  else transferRetryParams.delete(jobId);
  awaitingRefresh.add(jobId);
}

/** P-FILES ⑦：失败任务一键重试——按登记的原始请求原样重发，产生新 job。 */
const retryTransferBusy = new Set<string>();
async function retryTransfer(jobId: string) {
  const retry = transferRetryParams.get(jobId);
  if (!retry || retryTransferBusy.has(jobId)) return;
  retryTransferBusy.add(jobId);
  try {
    const result = await callFor<{ jobId?: string | null; transport?: string }>(retry.side, retry.method, retry.params);
    const newJobId = result.jobId;
    if (!newJobId) {
      error.value = t("operationFailed", { error: t("featureMissing") });
      return;
    }
    const job = tracker.jobs[jobId];
    trackSidecarJob(String(newJobId), job?.kind ?? "copy", job?.remotePath ?? "", retry);
    awaitingRefresh.add(String(newJobId));
    showNotice(t("jobStarted", { name: baseName(String(retry.params.targetPath ?? retry.params.newPath ?? "")) }));
  } catch (cause) {
    showError(cause);
  } finally {
    retryTransferBusy.delete(jobId);
  }
}

/** TransferPanel 的重试按钮可用集：失败 + kind 可重发 + 有登记参数。 */
const retryableTransferIds = computed(() => {
  const ids: string[] = [];
  for (const job of Object.values(tracker.jobs)) {
    if (job.state === "failed" && isRetryableKind(job.kind) && transferRetryParams.has(job.jobId)) ids.push(job.jobId);
  }
  return ids;
});

const confirmOpen = ref(false);
const confirmKind = ref<ConfirmKind>();
const confirmTitle = ref("");
const confirmBody = ref("");
const confirmDanger = ref(false);
const confirmHits = ref<DangerousHit[]>([]);
const confirmBusy = ref(false);
const confirmDraft = ref("");
const confirmTarget = ref<{ path?: string; entry?: FileEntry; targets?: FileEntry[] }>({});
const confirmSide = ref<PaneSide>("left");
// R3-P2-4：新建/重命名文件名行内校验提示（七语），弹层保持打开可直接改名重提。
const confirmNameIssue = ref("");
// R3-P2-5：copy/move 目标冲突——首次提交预检命中后转入覆盖确认（二次确认），
// confirmForcePath 记录已确认的路径；草稿再改动则重新预检。
const confirmForce = ref(false);
const confirmForcePath = ref("");
// R3-P2-5：跨栏 copy/move 冲突预检命中时挂起整批传输，弹「覆盖确认」后原样执行。
const pendingPaneTransfer = ref<{ from: PaneSide; move: boolean; list: FileEntry[]; destPath: string }>();
const confirmInput = computed(() => confirmKind.value === "newFolder" || confirmKind.value === "newFile" || confirmKind.value === "rename" || confirmKind.value === "syncDir" || confirmKind.value === "copyDir" || confirmKind.value === "copy" || confirmKind.value === "move" || confirmKind.value === "extract" || confirmKind.value === "compress");
// P2-2：危险确认列表走 i18n 七语（lib 侧 label 为英文兜底，路径类条目原样展示）。
const confirmDangerList = computed(() =>
  confirmHits.value.map((hit) => {
    switch (hit.id) {
      case "purge-root":
        return t("dangerPurgeRoot");
      case "purge":
        return t("dangerPurge", { path: hit.path ?? hit.label });
      case "recursive-delete":
        return t("dangerRecursiveDelete");
      case "bulk-delete":
        return t("dangerBulkDelete", { count: hit.count ?? 0 });
      // system-path / sync-overwrite / copy-overwrite 的 label 本身就是路径
      default:
        return hit.label;
    }
  }),
);
const confirmLabel = computed(() => {
  if (confirmKind.value === "overwrite" || confirmForce.value) return t("overwrite");
  if (confirmKind.value === "newFolder" || confirmKind.value === "newFile") return t("create");
  if (confirmKind.value === "rename") return t("save");
  if (confirmKind.value === "compress") return t("compressAction");
  return t("confirm");
});

const sortedEntries = computed(() => sortEntries(entries.value, sort.value));
// 当前目录文件名过滤（tiny-rdm 对标缺口#7）：仅影响展示，不影响选择/删除语义。
const searchQuery = ref("");
const filteredEntries = computed(() => filterEntries(sortedEntries.value, searchQuery.value));
// P2-12：右栏排序状态独立（双栏各自连接，排序互不联动；左栏排序仍持久化）。
const rightSort = ref<SortState>(prefs.sort);
const rightSorted = computed(() => sortEntries(rightEntries.value, rightSort.value));
// 右栏同款过滤（双栏对称性修复）：与左栏共用 filterEntries 语义。
const rightSearchQuery = ref("");
const filteredRightEntries = computed(() => filterEntries(rightSorted.value, rightSearchQuery.value));

// R3-P1-2：最近活动栏（最后点击/键盘操作侧）——工具栏「新建/上传/下载/删除」
// 路由到该栏（上传除外：P1-5 已定「上传=传向远端」，固定走 resolveUploadTarget）。
const activeSide = ref<PaneSide>("left");
const toolbarTarget = computed(() =>
  resolveToolbarTarget({
    dualPane: dualPane.value,
    activeSide: activeSide.value,
    leftSelection: selection.value,
    rightSelection: rightSelection.value,
  }),
);
function markActiveSide(side: PaneSide) {
  activeSide.value = side;
}
/** FileTable 选择回写（同时把该栏标记为活动栏）。 */
function setPaneSelection(side: PaneSide, paths: string[]) {
  markActiveSide(side);
  if (side === "left") selection.value = paths;
  else rightSelection.value = paths;
}
function setPaneActivePath(side: PaneSide, value: string) {
  markActiveSide(side);
  if (side === "left") activePath.value = value;
  else rightActivePath.value = value;
}
function sortRouted(side: PaneSide, column: SortColumn) {
  markActiveSide(side);
  toggleSort(side, column);
}
function openContextMenu(side: PaneSide, payload: { entry: FileEntry; x: number; y: number }) {
  markActiveSide(side);
  contextMenu.value = { ...payload, side, selection: [...(side === "left" ? selection.value : rightSelection.value)] };
}
function openBlank(side: PaneSide, payload: { x: number; y: number }) {
  markActiveSide(side);
  openBlankMenu(side, payload);
}
/** 工具栏「删除所选」：按活动栏解析选择集对应条目。 */
function toolbarSelectionEntries(side: PaneSide): FileEntry[] {
  const pool = side === "right" ? rightSorted.value : sortedEntries.value;
  const sel = side === "right" ? rightSelection.value : selection.value;
  return pool.filter((entry) => sel.includes(entry.path));
}

watch([sort, dualPane, sideTab, sideCollapsed], () => {
  saveUiPrefs({ sort: sort.value, dualPane: dualPane.value, sideTab: sideTab.value, sideCollapsed: sideCollapsed.value });
}, { deep: true });

// 切到 tree tab 时懒加载根目录子项（首次进入/从 quick 切回均适用）。
watch(sideTab, (tab) => {
  if (tab === "tree") ensureTreeRoots();
});

// 双栏切换：开启时左栏默认本地（quickPaths 到位后若仍在根目录则落到主目录，
// 对标 FileZilla/tiny-rdm 本地栏起点），右栏 quickPaths 随开启重取；关闭时
// 左栏回到当前连接根目录。
async function enterLocalPaneIfAtRoot() {
  const home = leftQuickPaths.value.find((item) => item.key === "home");
  if (home && path.value === "/") await loadDirectory(home.path).catch(() => undefined);
}

watch(dualPane, async (on) => {
  if (on) {
    await loadDirectory("/").catch(() => undefined);
    await loadQuickPaths("left");
    await enterLocalPaneIfAtRoot();
    void loadQuickPaths("right");
  } else {
    // 收起双栏时清掉右栏残留选择/焦点，避免工具栏与跨栏动作引用幽灵选中集。
    rightSelection.value = [];
    rightActivePath.value = "";
    await loadDirectory("/").catch(() => undefined);
    void loadQuickPaths("left");
  }
});
watch(targetConnectionId, () => {
  if (dualPane.value) void loadQuickPaths("right");
});

let noticeTimer = 0;
let pollTimer = 0;
let pollingDisabled = false;
let unsubscribeEvent: (() => void) | undefined;
let unsubscribeBinary: (() => void) | undefined;
let unsubscribeLocale: (() => void) | undefined;
let unsubscribeContext: (() => void) | undefined;
let unsubscribeTheme: (() => void) | undefined;

function showNotice(message: string) {
  notice.value = message;
  window.clearTimeout(noticeTimer);
  noticeTimer = window.setTimeout(() => (notice.value = ""), 4000);
}

function showError(cause: unknown, side: PaneSide | "global" = "global") {
  const message = errorMessage(cause);
  errorSide.value = side;
  if (isMethodMissing(cause)) {
    const method = cause instanceof Error && "method" in cause ? String((cause as { method?: string }).method) : "";
    error.value = t("featureMissing", { method });
    errorDetail.value = message;
    return;
  }
  // P2-1：已知错误类别映射七语文案；未知错误原文透传（横幅 title 保留原文）。
  error.value = t("operationFailed", { error: friendlyError(message, t) });
  errorDetail.value = message;
}

/** 错误横幅上的重试（④ UI 三态：错误可恢复）。P2-4：按出错栏位重放——
 * 左/右栏失败只重载该栏，全局错误（操作类）两栏都重载。 */
async function retryAfterError() {
  error.value = "";
  const side = errorSide.value;
  try {
    if (side === "right") await loadRightDirectory();
    else if (side === "left") await loadDirectory();
    else {
      await loadDirectory();
      if (dualPane.value) await loadRightDirectory();
    }
  } catch {
    /* banner already shows the error */
  }
}

// ---- host bridge ---------------------------------------------------------

async function waitForHostApi(timeoutMs = 8000) {
  const deadline = Date.now() + timeoutMs;
  while (!window.dbxPlugin && Date.now() < deadline) await new Promise((resolve) => setTimeout(resolve, 50));
  if (!window.dbxPlugin) throw new Error(t("hostApiUnavailable"));
  return window.dbxPlugin;
}

// 响应式外观：宿主令牌/规范色板解析结果，供 CodeMirror 编辑器等
// 组件消费（与 ssh sftp 编辑器同方案，ssh/lib/appearance 对齐）。
const appearance = ref(resolveAppearance());

function applyAppearance(root: HTMLElement, next: DbxPluginAppearanceInput | null) {
  const resolved = resolveAppearance(next);
  appearance.value = resolved;
  for (const [key, value] of Object.entries(resolved.colors)) {
    root.style.setProperty(`--${key.replace(/([A-Z])/g, "-$1").toLowerCase()}`, value);
  }
  if (next?.colorScheme) root.dataset.theme = next.colorScheme;
  // 弹层背景按 DBX --popover 规范值（与 ldap/kafka/ssh 一致），Host API 1.0
  // 无 --color-popover 令牌时兜底；真实宿主由主题令牌桥直接下发。
  root.style.setProperty("--popover", DBX_POPOVER[resolved.colorScheme]);
}

function handleEvent(event: { method: string; params: Record<string, unknown> }) {
  if (event.method === "files/transfer/progress") {
    const job = tracker.onProgress(event.params as Parameters<typeof tracker.onProgress>[0]);
    // P-FILES ①b：transport=job 的 copy/move/rename 在终态后自动刷新目录。
    if (job && awaitingRefresh.has(job.jobId) && !isActive(job.state)) {
      awaitingRefresh.delete(job.jobId);
      void loadDirectory().catch(() => undefined);
      if (dualPane.value) void loadRightDirectory().catch(() => undefined);
      refreshAuditPanel();
    }
  }
}

interface DownloadChunk {
  offset: number;
  data: Uint8Array;
}

interface FrameWaiter {
  offset: number;
  resolve: (chunk: DownloadChunk) => void;
  reject: (cause: Error) => void;
  timer: number;
}

const frameQueue = new Map<string, DownloadChunk[]>();
const frameWaiters = new Map<string, FrameWaiter[]>();

function handleBinary(event: DbxPluginBinaryEvent) {
  if (!event.channel.startsWith("files/download/")) return;
  const data = bridgeBinaryBytes(event, window.dbxPlugin.decodeBase64);
  if (data.byteLength < 8) return;
  const offset = Number(new DataView(data.buffer, data.byteOffset, 8).getBigUint64(0, false));
  const chunk: DownloadChunk = { offset, data: data.slice(8) };
  const waiters = frameWaiters.get(event.channel);
  if (waiters?.length) {
    const waiter = waiters.shift()!;
    waiter.resolve(chunk);
    return;
  }
  const queue = frameQueue.get(event.channel) ?? [];
  queue.push(chunk);
  frameQueue.set(event.channel, queue);
}

function waitForFrame(channel: string, offset: number, timeoutMs = 30_000): Promise<DownloadChunk> {
  const queued = frameQueue.get(channel);
  if (queued?.length) {
    const index = queued.findIndex((chunk) => chunk.offset === offset);
    if (index >= 0) return Promise.resolve(queued.splice(index, 1)[0]);
  }
  return new Promise((resolve, reject) => {
    const waiter: FrameWaiter = {
      offset,
      resolve: (chunk) => {
        window.clearTimeout(waiter.timer);
        resolve(chunk);
      },
      reject: (cause) => {
        window.clearTimeout(waiter.timer);
        reject(cause);
      },
      timer: 0,
    };
    waiter.timer = window.setTimeout(() => {
      const pending = frameWaiters.get(channel) ?? [];
      const position = pending.findIndex((entry) => entry.offset === offset);
      if (position >= 0) pending.splice(position, 1);
      reject(new Error(`download frame timeout at offset ${offset}`));
    }, timeoutMs);
    const waiters = frameWaiters.get(channel) ?? [];
    waiters.push(waiter);
    frameWaiters.set(channel, waiters);
  });
}

function releaseFrames(channel: string) {
  frameQueue.delete(channel);
  const waiters = frameWaiters.get(channel);
  if (waiters) {
    for (const waiter of waiters) waiter.reject(new Error("download channel released"));
    frameWaiters.delete(channel);
  }
}

// ---- directory -----------------------------------------------------------

async function fetchListing(target: string, explicitConnectionId?: string) {
  const params: Record<string, unknown> = { path: target };
  if (explicitConnectionId) params.connectionId = explicitConnectionId;
  // 连接状态 pill：任一非本地栏的 files/list 都反映存储连接健康度（本地
  // __local__ 恒可用，不代表连接）；主连接 id 在部分宿主/mock 的 context
  // 里缺失，无法按 id 精确归因，按"非本地"判定。
  const hitsHost = explicitConnectionId !== LOCAL_CONNECTION_ID;
  if (hitsHost) connState.value = "connecting";
  try {
    const result = await call<{ entries: FileEntry[] }>("files/list", params);
    if (hitsHost) connState.value = "connected";
    return normalizeEntries(result.entries ?? []);
  } catch (cause) {
    // P2-3：pill 与单次业务失败解耦——仅网络/传输层失败置「已断开」；业务错误
    // （NotFound、权限、参数类）说明 sidecar 应答了连接，置「已连接」而非断开，
    // 也避免失败期间停留在「连接中」抖动。
    if (hitsHost) connState.value = isTransportFailure(errorMessage(cause)) ? "disconnected" : "connected";
    throw cause;
  }
}

async function loadDirectory(target?: string) {
  const next = target ?? path.value;
  const token = leftNav.next();
  loading.value = true;
  try {
    const list = await fetchListing(next, sideConnectionId("left"));
    // 晚到的过期响应：直接丢弃，面包屑/列表/选中态保持最新导航的结果。
    if (!leftNav.isCurrent(token)) return;
    entries.value = list;
    path.value = next;
    selection.value = [];
    activePath.value = "";
    error.value = "";
    // 大目录提示（第 3 轮）：浏览仍走全量 files/list（排序/过滤/全选语义
    // 不回归），仅当条目数达阈值时提示用户列表已虚拟滚动（largeDir.ts 记录
    // 了不切 listPaged 的 bench 依据）。
    if (isLargeDirectory(entries.value.length)) showNotice(t("largeDirectory", { count: entries.value.length }));
  } catch (cause) {
    // 过期请求的失败同样不打扰新目录（横幅不闪旧导航的错误）。
    if (!leftNav.isCurrent(token)) return;
    showError(cause, "left");
    throw cause;
  } finally {
    // loading 由最新一次请求收尾（过期请求不抢着关，避免闪烁）。
    if (leftNav.isCurrent(token)) loading.value = false;
  }
}

async function loadRightDirectory(target?: string) {
  const next = target ?? rightPath.value;
  const token = rightNav.next();
  rightLoading.value = true;
  try {
    const list = await fetchListing(next, targetConnectionId.value || undefined);
    if (!rightNav.isCurrent(token)) return;
    rightEntries.value = list;
    rightPath.value = next;
    rightSelection.value = [];
    rightActivePath.value = "";
    if (isLargeDirectory(rightEntries.value.length)) showNotice(t("largeDirectory", { count: rightEntries.value.length }));
  } catch (cause) {
    if (!rightNav.isCurrent(token)) return;
    showError(cause, "right");
    throw cause;
  } finally {
    if (rightNav.isCurrent(token)) rightLoading.value = false;
  }
}

async function refreshDirectory() {
  try {
    await loadDirectory();
    // 大目录时 loadDirectory 已给出 largeDirectory 提示，避免被 refreshed 覆盖。
    if (!isLargeDirectory(entries.value.length)) showNotice(t("refreshed"));
  } catch {
    /* banner already shows the error */
  }
}

async function refreshRightDirectory() {
  try {
    await loadRightDirectory();
    if (!isLargeDirectory(rightEntries.value.length)) showNotice(t("refreshed"));
  } catch {
    /* banner already shows the error */
  }
}

function openPreview(target: string, side: PaneSide = "left") {
  // 文件概览弹窗：固化为来源栏连接快照；不再切右栏 Tab/强制开双栏，
  // 弹窗期间两侧栏保持各自连接面可继续导航。
  previewPath.value = target;
  previewConnectionId.value = sideConnectionId(side);
}

async function openEntry(entry: FileEntry, side: PaneSide = "left") {
  if (entry.kind === "directory") {
    try {
      if (side === "left") await loadDirectory(entry.path);
      else await loadRightDirectory(entry.path);
    } catch {
      /* keep current listing */
    }
    return;
  }
  openPreview(entry.path, side);
}

function toggleSort(side: PaneSide, column: SortColumn) {
  if (side === "right") rightSort.value = toggleSortState(rightSort.value, column);
  else sort.value = toggleSortState(sort.value, column);
}

async function loadCapabilities() {
  try {
    capabilities.value = await call<FileCapabilities>("files/capabilities");
  } catch (cause) {
    if (!isMethodMissing(cause)) showError(cause);
  }
}

/**
 * A-FILES ①：探测宿主是否提供连接枚举（host.listConnections）。
 * 宿主未提供时目标栏仅支持「同连接另一路径」；跨连接能力见交接文档。
 */
async function probeConnections() {
  try {
    const list = await window.dbxPlugin.request<Array<Record<string, unknown>>>("host.listConnections");
    if (Array.isArray(list)) {
      targetConnections.value = list
        .map((item) => ({ id: String(item.id ?? item.connectionId ?? ""), name: String(item.name ?? item.id ?? item.connectionId ?? "") }))
        .filter((item) => item.id && item.id !== connectionId.value);
    }
  } catch {
    targetConnections.value = [];
  }
}

/** 左栏切换连接（双栏）：新连接回到根目录，quickPaths 随连接面刷新。 */
async function onLeftConnectionChange() {
  markActiveSide("left");
  await loadDirectory("/").catch(() => undefined);
  await loadQuickPaths("left");
  await enterLocalPaneIfAtRoot();
}

// ---- dialogs ---------------------------------------------------------------

/** P2-10 重名预检：true=已存在；false=确认不存在；undefined=无法判定（放行给后端）。 */
async function pathExists(side: PaneSide, target: string): Promise<boolean | undefined> {
  try {
    await callFor(side, "files/stat", { path: target });
    return true;
  } catch (cause) {
    return isNotFoundMessage(errorMessage(cause)) ? false : undefined;
  }
}

/** 重名预检失败：横幅提示 + 弹层保持打开（用户可直接改名重提）。 */
function rejectDuplicate(name: string) {
  error.value = t("nameExists", { name });
}

function openConfirm(kind: ConfirmKind, options: {
  title: string;
  body?: string;
  danger?: boolean;
  hits?: DangerousHit[];
  target?: { path?: string; entry?: FileEntry; targets?: FileEntry[] };
  draft?: string;
  side?: PaneSide;
}) {
  confirmKind.value = kind;
  confirmTitle.value = options.title;
  confirmBody.value = options.body ?? "";
  confirmDanger.value = Boolean(options.danger);
  confirmHits.value = options.hits ?? [];
  confirmTarget.value = options.target ?? {};
  confirmDraft.value = options.draft ?? "";
  confirmSide.value = options.side ?? "left";
  confirmNameIssue.value = "";
  confirmForce.value = false;
  confirmForcePath.value = "";
  confirmOpen.value = true;
}

function closeConfirm() {
  confirmOpen.value = false;
  confirmBusy.value = false;
  confirmForce.value = false;
  confirmForcePath.value = "";
  pendingPaneTransfer.value = undefined;
}

/** R3-P2-4：新建/重命名提交前文件名校验（禁 `/`、禁 `.`/`..`、禁空值）。
 * 命中即行内提示且弹层保持打开，可直接改名重提。 */
function checkConfirmName(): boolean {
  const issue = validateFileName(confirmDraft.value);
  if (!issue) {
    confirmNameIssue.value = "";
    return true;
  }
  confirmNameIssue.value = issue === "empty" ? t("fileNameRequired") : t("invalidFileName");
  return false;
}

function startNewFolder(side: PaneSide = "left") {
  openConfirm("newFolder", { title: t("newFolderTitle"), draft: "", side });
}

/** 新建文件（P-FILES）：复用 files/write 写空内容（≤MAX_INLINE_WRITE_BYTES）。 */
function startNewFile(side: PaneSide = "left") {
  openConfirm("newFile", { title: t("newFileTitle"), draft: "", side });
}

function startRename(entry: FileEntry, side: PaneSide) {
  confirmDraft.value = entry.name;
  openConfirm("rename", { title: t("renameTitle"), target: { entry }, draft: entry.name, side });
}

function startDelete(targets: FileEntry[], side: PaneSide = "left") {
  if (!targets.length) return;
  const inspection = inspect("delete", { targets: targets.map((entry) => ({ path: entry.path, kind: entry.kind })) });
  const dirs = targets.filter((entry) => entry.kind === "directory");
  if (dirs.length) {
    // 目录删除即递归清空：走 purge 规则确认
    const worst = dirs.reduce<{ level: "none" | "warn" | "danger"; hits: DangerousHit[] }>(
      (acc, dir) => {
        const result = inspect("purge", { path: dir.path });
        return result.level === "danger" || acc.level === "danger" ? { level: "danger", hits: [...acc.hits, ...result.hits] } : result;
      },
      { level: "none", hits: [] },
    );
    openConfirm("delete", {
      title: t("deleteTitle", { count: targets.length }),
      body: `${t("deleteBody")} ${t("deleteRecursiveWarn")}`,
      danger: worst.level === "danger",
      hits: worst.hits,
      target: { targets },
      side,
    });
    return;
  }
  openConfirm("delete", {
    title: t("deleteTitle", { count: targets.length }),
    body: inspection.level === "none" ? t("deleteBody") : `${t("deleteBody")} ${t("deleteRecursiveWarn")}`,
    danger: inspection.level === "danger",
    hits: inspection.hits,
    target: { targets },
    side,
  });
}

function startDirJob(kind: "syncDir" | "copyDir", entry: FileEntry, side: PaneSide) {
  const defaultTarget = joinPath("/", `${baseName(entry.path) || "copy"}`);
  openConfirm(kind, {
    title: kind === "syncDir" ? t("syncDirTitle", { path: defaultTarget }) : t("copyDirTitle", { path: defaultTarget }),
    body: kind === "syncDir" ? t("syncDirBody") : t("copyDirBody"),
    danger: true,
    target: { entry },
    draft: defaultTarget,
    side,
  });
}

/** P-FILES ①b：copy/move 动作（native 同步返回 / 降级 jobId 由后端决定）。 */
function startCopyMove(kind: "copy" | "move", entry: FileEntry, side: PaneSide) {
  const parent = parentPath(entry.path);
  const draft = kind === "copy"
    ? joinPath(parent, `${entry.name}-copy`)
    : joinPath("/", entry.name);
  openConfirm(kind, {
    title: kind === "copy" ? t("copyTitle") : t("moveTitle"),
    body: kind === "copy" ? t("copyBody") : t("moveBody"),
    target: { entry },
    draft,
    side,
  });
}

/** A-FILES ③：压缩包解压骨架——等后端 files/extract（交接），方法未注册时给出 featureMissing 提示。 */
function startExtract(entry: FileEntry, side: PaneSide) {
  openConfirm("extract", {
    title: t("extractTitle", { name: entry.name }),
    body: t("extractBody"),
    target: { entry },
    draft: parentPath(entry.path) || "/",
    side,
  });
}

/** 指定栏的调用：该栏选择了其它连接（含左栏本地 __local__）时显式带 connectionId（当前连接走默认注入）。 */
function callFor<T = Record<string, unknown>>(side: PaneSide, method: string, params: Record<string, unknown> = {}): Promise<T> {
  const explicit = sideConnectionId(side);
  if (explicit) params.connectionId = explicit;
  return call<T>(method, params);
}

/** 压缩（P-FILES）：单选/多选共用；目标默认源目录下 <名称>.tar.gz。 */
function startCompress(targets: FileEntry[], side: PaneSide) {
  if (!targets.length) return;
  const base = targets.length === 1 ? baseName(targets[0].path) || "archive" : "archive";
  openConfirm("compress", {
    title: t("compressTitle"),
    body: t("compressBody"),
    target: { targets },
    draft: joinPath(parentPath(targets[0].path), `${base}.tar.gz`),
    side,
  });
}

// ---- 批量操作（P2-7）：并发分批 + 进度落传输面板 --------------------------------
// 此前批量删除为串行逐条 RPC（1 万项 = 1 万次 await），弹层只有 busy 态、
// 无整体进度。这里以固定并发执行，并把进度写入传输面板（本地伪 job：
// filesDone/filesTotal 驱动计数与百分比，终态 completed/failed）。

const BATCH_CONCURRENCY = 8;
let batchSeq = 0;
// R3-P2-9：本地伪 job 的取消标志（jobId → flag）。取消真实中断 runBatch
// （worker 跳过剩余分批），job 置已取消态——此前对 local-batch-* 调
// files/transfer/cancel，mock/真实 sidecar 无此记录仍返回 success（假成功）。
const batchCancelFlags = new Map<string, { canceled: boolean }>();

async function runBatch(kind: TransferKind, remotePath: string, count: number, task: (index: number) => Promise<void>): Promise<{ canceled: boolean }> {
  if (count <= 0) return { canceled: false };
  const jobId = `local-batch-${++batchSeq}`;
  registerJob({
    jobId,
    connectionId: connectionId.value,
    kind,
    remotePath,
    state: "running",
    size: count,
    transferred: 0,
    filesDone: 0,
    filesTotal: count,
    updatedAt: Date.now(),
  });
  const cancelFlag = { canceled: false };
  batchCancelFlags.set(jobId, cancelFlag);
  const result = await runBatchTasks({
    count,
    concurrency: BATCH_CONCURRENCY,
    isCanceled: () => cancelFlag.canceled,
    task,
    onProgress: (done) => {
      tracker.onProgress({ jobId, kind, state: "running", transferred: done, size: count, filesDone: done, filesTotal: count });
    },
  });
  batchCancelFlags.delete(jobId);
  if (result.canceled) {
    tracker.onProgress({ jobId, kind, state: "canceled", transferred: result.done, size: count, filesDone: result.done, filesTotal: count });
    return { canceled: true };
  }
  if (result.error) {
    tracker.onProgress({ jobId, kind, state: "failed", error: errorMessage(result.error), transferred: result.done, size: count, filesDone: result.done, filesTotal: count });
    throw result.error;
  }
  tracker.onProgress({ jobId, kind, state: "completed", transferred: count, size: count, filesDone: count, filesTotal: count });
  return { canceled: false };
}

async function onConfirm() {
  if (!confirmKind.value) return;
  confirmBusy.value = true;
  const side = confirmSide.value;
  // transport=job 的 copy/move/rename/syncDir/copyDir：job 已登记、终态后
  // 由事件驱动刷新（handleEvent），跳过立即刷新。
  let jobStarted = false;
  try {
    switch (confirmKind.value) {
      case "newFolder": {
        const name = confirmDraft.value.trim();
        // R3-P2-4：文件名校验（禁 /、禁 . / ..、禁空值），行内提示不关弹层。
        if (!checkConfirmName()) return;
        const fullPath = joinPath(paneDirPath(side), name);
        // P2-10：提交前重名预检，命中即提示且弹层保持打开。
        if ((await pathExists(side, fullPath)) === true) return rejectDuplicate(name);
        await callFor(side, "files/mkdir", { path: fullPath });
        showNotice(t("folderCreated"));
        break;
      }
      case "newFile": {
        const name = confirmDraft.value.trim();
        if (!checkConfirmName()) return;
        const fullPath = joinPath(paneDirPath(side), name);
        if ((await pathExists(side, fullPath)) === true) return rejectDuplicate(name);
        await callFor(side, "files/write", { path: fullPath, dataBase64: "" });
        showNotice(t("fileCreated"));
        break;
      }
      case "rename": {
        const entry = confirmTarget.value.entry;
        const name = confirmDraft.value.trim();
        if (!entry) return;
        if (!checkConfirmName()) return;
        if (!name || name === entry.name) return;
        const newPath = joinPath(parentPath(entry.path), name);
        if ((await pathExists(side, newPath)) === true) return rejectDuplicate(name);
        const result = await callFor<{ transport?: string; jobId?: string | null }>(side, "files/rename", {
          path: entry.path,
          newPath,
        });
        if (result.transport === "job" && result.jobId) {
          // 目录 rename 降级（P-FILES ②）：等终态再刷新。
          trackSidecarJob(result.jobId, "rename", `${entry.path} → ${newPath}`, {
            method: "files/rename",
            side,
            params: { path: entry.path, newPath },
          });
          jobStarted = true;
        }
        showNotice(t("renamed"));
        break;
      }
      case "copy":
      case "move": {
        const entry = confirmTarget.value.entry;
        const targetPath = confirmDraft.value.trim();
        if (!entry || !targetPath) return;
        if (targetPath.replace(/\/+$/, "") === entry.path.replace(/\/+$/, "")) {
          error.value = t("operationFailed", { error: t("destMustDiffer") });
          return;
        }
        // R3-P2-5：目标冲突预检——同名即静默覆盖（数据丢失风险），命中后转入
        // 覆盖确认（弹层保持打开、危险态、按钮变「覆盖」），确认后才执行。
        // 草稿在确认后又被改动则重新预检。
        if (!confirmForce.value || confirmForcePath.value !== targetPath) {
          if ((await pathExists(side, targetPath)) === true) {
            confirmForce.value = true;
            confirmForcePath.value = targetPath;
            confirmDanger.value = true;
            confirmBody.value = t("overwriteAsk", { path: targetPath });
            return;
          }
        }
        const result = await callFor<{ success: boolean; transport?: string; jobId?: string | null }>(side, `files/${confirmKind.value}`, {
          sourcePath: entry.path,
          targetPath,
        });
        if (result.transport === "job" && result.jobId) {
          trackSidecarJob(result.jobId, confirmKind.value, `${entry.path} → ${targetPath}`, {
            method: `files/${confirmKind.value}`,
            side,
            params: { sourcePath: entry.path, targetPath },
          });
          jobStarted = true;
        }
        showNotice(t("jobStarted", { name: baseName(targetPath) }));
        break;
      }
      // R3-P2-5：跨栏 copy/move 的覆盖确认终站——确认后原样执行挂起的整批传输。
      case "overwrite": {
        const pending = pendingPaneTransfer.value;
        pendingPaneTransfer.value = undefined;
        if (!pending) return;
        await executePaneTransfer(pending.from, pending.move, pending.list, pending.destPath);
        break;
      }
      case "delete": {
        const targets = confirmTarget.value.targets ?? [];
        // P2-7：批量删除并发分批执行，进度实时落传输面板（单项目为 1 批同语义）。
        // R3-P2-9：取消真实中断剩余分批；已取消时不再补「已删除」通知。
        const batch = await runBatch("delete", paneDirPath(side), targets.length, async (index) => {
          const target = targets[index];
          if (target.kind === "directory") await callFor(side, "files/purge", { path: target.path });
          else await callFor(side, "files/delete", { path: target.path });
        });
        if (!batch.canceled) showNotice(t("deleted"));
        break;
      }
      case "purge": {
        const target = confirmTarget.value.path;
        if (!target) return;
        await callFor(side, "files/purge", { path: target });
        showNotice(t("deleted"));
        break;
      }
      case "syncDir":
      case "copyDir": {
        const entry = confirmTarget.value.entry;
        const targetPath = confirmDraft.value.trim();
        if (!entry || !targetPath) return;
        const result = await callFor<{ jobId: string }>(side, `files/${confirmKind.value}`, {
          sourcePath: entry.path,
          targetPath,
        });
        if (result.jobId) {
          trackSidecarJob(result.jobId, confirmKind.value, `${entry.path} → ${targetPath}`, {
            method: `files/${confirmKind.value}`,
            side,
            params: { sourcePath: entry.path, targetPath },
          });
        }
        jobStarted = true;
        showNotice(t("jobStarted", { name: baseName(targetPath) }));
        break;
      }
      case "extract": {
        const entry = confirmTarget.value.entry;
        const targetPath = confirmDraft.value.trim();
        if (!entry || !targetPath) return;
        // files/extract 为后端交接方法：未实现时走 featureMissing 提示，
        // 实现后（transport=job）复用 copy/move 的 job 进度语义。
        const result = await callFor<{ success?: boolean; transport?: string; jobId?: string | null }>(side, "files/extract", {
          path: entry.path,
          targetPath,
        });
        if (result.transport === "job" && result.jobId) {
          trackSidecarJob(result.jobId, "copy", `${entry.path} → ${targetPath}`);
          jobStarted = true;
        }
        showNotice(t("jobStarted", { name: baseName(targetPath) }));
        break;
      }
      case "compress": {
        const targets = confirmTarget.value.targets ?? [];
        const targetPath = confirmDraft.value.trim();
        if (!targets.length || !targetPath) return;
        const result = await callFor<{ success: boolean; transport?: string; jobId?: string | null }>(side, "files/compress", {
          paths: targets.map((entry) => entry.path),
          targetPath,
        });
        if (result.transport === "job" && result.jobId) {
          trackSidecarJob(result.jobId, "compress", targetPath);
          jobStarted = true;
          showNotice(t("jobStarted", { name: baseName(targetPath) }));
        } else {
          showNotice(t("compressDone"));
        }
        break;
      }
    }
    closeConfirm();
    refreshAuditPanel();
    if (!jobStarted) {
      await loadDirectory().catch(() => undefined);
      if (side === "right" && dualPane.value) await loadRightDirectory().catch(() => undefined);
    }
  } catch (cause) {
    showError(cause);
    closeConfirm();
  } finally {
    confirmBusy.value = false;
  }
}

// ---- cross-pane transfers（A-FILES ①）-----------------------------------------

function pickSideEntries(side: PaneSide, paths: string[]): FileEntry[] {
  const pool = side === "left" ? sortedEntries.value : rightSorted.value;
  return pool.filter((entry) => paths.includes(entry.path));
}

/**
 * 跨栏 copy/move：同一连接直接传路径；跨连接（目标栏选择了其它连接）时
 * 契约字段 targetConnectionId / sourceConnectionId 由方向决定。
 * R3-P2-1：copy 与 move 的写都发生在目标栏，同受 canWrite 门禁。
 * R3-P2-5：目标冲突预检——命中同名即挂起整批，弹覆盖确认后原样执行。
 */
async function transferBetween(from: PaneSide, move: boolean, dragged?: FileEntry[]) {
  if (!canWrite.value) return;
  const to: PaneSide = from === "left" ? "right" : "left";
  const paths = dragged ? dragged.map((entry) => entry.path) : from === "left" ? selection.value : rightSelection.value;
  const list = dragged ?? pickSideEntries(from, paths);
  if (!list.length) return;
  const destPath = to === "left" ? path.value : rightPath.value;
  // 目标冲突预检（R3-P2-5）：一次 list 目标目录取同名集合（避免逐条 stat），
  // 命中即整批挂起等覆盖确认；目标不可列（不存在/失败）交后端兜底。
  const conflicts = await findTargetConflicts(to, destPath, list);
  if (conflicts.length) {
    pendingPaneTransfer.value = { from, move, list, destPath };
    openConfirm("overwrite", {
      title: move ? t("moveTitle") : t("copyTitle"),
      body: t("overwriteBatch", { count: conflicts.length }),
      danger: true,
      side: from,
    });
    return;
  }
  await executePaneTransfer(from, move, list, destPath);
}

/** R3-P2-5：目标目录一次性拉取，返回与传入列表同名的冲突条目。 */
async function findTargetConflicts(to: PaneSide, destPath: string, list: FileEntry[]): Promise<FileEntry[]> {
  try {
    const existing = await fetchListing(destPath, sideConnectionId(to));
    const names = new Set(existing.map((entry) => entry.name));
    return list.filter((item) => names.has(item.name));
  } catch {
    return [];
  }
}

/** 跨栏传输执行体（R3-P2-5 拆出）：overwrite 确认与无冲突路径共用。 */
async function executePaneTransfer(from: PaneSide, move: boolean, list: FileEntry[], destPath: string) {
  const to: PaneSide = from === "left" ? "right" : "left";
  // 该栏显式连接（右栏其它连接 / 双栏左栏本地 __local__）；当前连接为 undefined（默认注入）。
  const sourceConnectionId = sideConnectionId(from);
  const targetConnection = sideConnectionId(to);
  try {
    for (const item of list) {
      const params: Record<string, unknown> = {
        sourcePath: item.path,
        targetPath: joinPath(destPath, item.name),
      };
      if (sourceConnectionId) params.sourceConnectionId = sourceConnectionId;
      if (targetConnection) params.targetConnectionId = targetConnection;
      const result = await call<{ transport?: string; jobId?: string | null }>(`files/${move ? "move" : "copy"}`, params);
      if (result.transport === "job" && result.jobId) {
        trackSidecarJob(result.jobId, move ? "move" : "copy", `${item.path} → ${String(params.targetPath)}`);
      }
    }
    showNotice(t("paneTransferred", { count: list.length }));
    if (to === "left") await loadDirectory().catch(() => undefined);
    else await loadRightDirectory().catch(() => undefined);
    if (move && from === "right") await loadRightDirectory().catch(() => undefined);
  } catch (cause) {
    showError(cause);
  }
}

// ---- drag & drop（A-FILES ①）---------------------------------------------------

function onDropTo(side: PaneSide, event: DragEvent) {
  dragOverSide.value = null;
  const raw = event.dataTransfer?.getData("application/x-dbx-files");
  if (!raw) return;
  let payload: { paneId?: string; paths?: string[] };
  try {
    payload = JSON.parse(raw);
  } catch {
    return;
  }
  const from = payload.paneId === "left" ? "left" : payload.paneId === "right" ? "right" : null;
  if (!from || from === side || !payload.paths?.length) return;
  void transferBetween(from, false, pickSideEntries(from, payload.paths));
}

// ---- transfers ---------------------------------------------------------------

const CHUNK_SIZE = 256 * 1024;

function registerJob(job: TransferJob) {
  tracker.register(job);
}

function writeU64(bytes: Uint8Array, value: number) {
  new DataView(bytes.buffer).setBigUint64(0, BigInt(value), false);
}

/** 上传目标（P1-5）：上传=传向远端。双栏固定右栏（远端目标面），单栏为当前连接当前目录。 */
function uploadTarget(): UploadTarget {
  return resolveUploadTarget({
    dualPane: dualPane.value,
    leftPath: path.value,
    rightPath: rightPath.value,
    leftConnectionId: sideConnectionId("left"),
    rightConnectionId: sideConnectionId("right"),
  });
}

async function uploadSource(name: string, size: number, readChunk: (offset: number, length: number) => Promise<Uint8Array>) {
  // P1-5 上传方向语义：上传=传向远端（对标 FileZilla/tiny-rdm）。双栏时固定
  // 落到目标栏（右栏连接面恒为远端，不含本地 __local__）；单栏时落到当前
  // 连接的当前目录（原行为）。不再固定写左栏——双栏默认左栏是本地面板，
  // 把「上传」写进本地盘与直觉相反。
  const target = uploadTarget();
  const remotePath = joinPath(target.path, name);
  const startParams: Record<string, unknown> = { remotePath, size };
  if (target.connectionId) startParams.connectionId = target.connectionId;
  const start = await call<{ taskId: string; chunkSize?: number }>("files/upload/start", startParams);
  const taskId = start.taskId;
  const chunkSize = start.chunkSize && start.chunkSize > 0 ? start.chunkSize : CHUNK_SIZE;
  registerJob({
    jobId: taskId,
    taskId,
    connectionId: target.connectionId ?? connectionId.value,
    kind: "upload",
    remotePath,
    state: "running",
    size,
    transferred: 0,
    updatedAt: Date.now(),
  });
  try {
    let offset = 0;
    while (offset < size) {
      const chunk = await readChunk(offset, chunkSize);
      if (!chunk.byteLength) throw new Error("local file ended before its declared size");
      const payload = new Uint8Array(8 + chunk.byteLength);
      writeU64(payload, offset);
      payload.set(chunk, 8);
      await window.dbxPlugin.sendBinary(`files/upload/${taskId}`, payload);
      offset += chunk.byteLength;
      // 经 onProgress 走统一入口：保留速率采样（而非直接改 job 字段）。
      tracker.onProgress({ jobId: taskId, taskId, state: "running", transferred: offset });
    }
    await window.dbxPlugin.invoke("files/upload/finish", { taskId }, { timeoutMs: 30 * 60 * 1000 });
    tracker.onProgress({ jobId: taskId, taskId, state: "completed", transferred: size });
  } catch (cause) {
    tracker.onProgress({ jobId: taskId, taskId, state: "failed", error: errorMessage(cause) });
    await window.dbxPlugin.invoke("files/transfer/cancel", { taskId }).catch(() => undefined);
    throw cause;
  }
}

/** 上传后刷新目标栏并按目标路径提示（P1-5）：双栏刷新右栏，单栏刷新左栏。 */
async function afterUpload(count: number) {
  const target = uploadTarget();
  if (dualPane.value) await loadRightDirectory().catch(() => undefined);
  else await loadDirectory().catch(() => undefined);
  refreshAuditPanel();
  if (count) showNotice(t("uploaded", { count, path: target.path }));
}

async function uploadLocalFiles(files: readonly File[]) {
  for (const file of files) {
    try {
      await uploadSource(file.name, file.size, async (offset, length) =>
        new Uint8Array(await file.slice(offset, offset + length).arrayBuffer()),
      );
    } catch (cause) {
      showError(cause);
    }
  }
  await afterUpload(files.length);
}

async function uploadHostFiles(files: Array<{ handleId: string; name: string; size: number }>) {
  const fileTransfer = window.dbxPlugin.fileTransfer;
  if (!fileTransfer) return;
  for (const file of files) {
    try {
      await uploadSource(file.name, file.size, async (offset, length) => {
        const result = await fileTransfer.read(file.handleId, offset, length);
        return window.dbxPlugin.decodeBase64(result.dataBase64);
      });
    } catch (cause) {
      showError(cause);
    } finally {
      await fileTransfer.cancel(file.handleId).catch(() => undefined);
    }
  }
  await afterUpload(files.length);
}

async function onUpload(files: File[] | null) {
  if (!canWrite.value) return;
  if (files === null) {
    const fileTransfer = window.dbxPlugin.fileTransfer;
    if (!fileTransfer) return;
    try {
      const picked = await fileTransfer.pick({ multiple: true });
      await uploadHostFiles(picked.files);
    } catch (cause) {
      showError(cause);
    }
    return;
  }
  await uploadLocalFiles(files);
}

async function downloadEntry(entry: FileEntry, side: PaneSide = "left") {
  const fileTransfer = window.dbxPlugin.fileTransfer;
  let taskId: string | undefined;
  try {
    const startParams: Record<string, unknown> = { remotePath: entry.path };
    const explicit = sideConnectionId(side);
    if (explicit) startParams.connectionId = explicit;
    const info = await call<{ taskId: string; size: number; fileName?: string; chunkSize?: number }>("files/download/start", startParams);
    taskId = info.taskId;
    const size = info.size;
    const channel = `files/download/${taskId}`;
    releaseFrames(channel);
    registerJob({
      jobId: taskId,
      taskId,
      connectionId: connectionId.value,
      kind: "download",
      remotePath: entry.path,
      state: "running",
      size,
      transferred: 0,
      updatedAt: Date.now(),
    });
    // 泵式下载：sidecar 在 start 后自行按 offset 顺序推送
    // files/download/{taskId} 帧（8 字节 BE offset + <=256KiB），前端只收帧。
    const target = fileTransfer ? await fileTransfer.beginSave({ name: info.fileName ?? entry.name, size }) : undefined;
    const chunks = target ? undefined : ([] as Uint8Array[]);
    let offset = 0;
    while (offset < size) {
      const chunk = await waitForFrame(channel, offset);
      if (!chunk.data.byteLength) throw new Error("download frame carried no data");
      if (chunks) {
        chunks.push(chunk.data);
      } else if (target) {
        const write = await fileTransfer!.write(target.handleId, chunk.offset, chunk.data);
        offset = Math.max(offset + chunk.data.byteLength, write.nextOffset);
      }
      if (!target && chunks) offset += chunk.data.byteLength;
      tracker.onProgress({ jobId: taskId, taskId, state: "running", transferred: offset });
    }
    if (target) {
      await fileTransfer!.finish(target.handleId);
    } else if (chunks) {
      saveBrowserDownload(chunks, info.fileName ?? entry.name);
    }
    await window.dbxPlugin.invoke("files/download/finish", { taskId });
    releaseFrames(channel);
    tracker.onProgress({ jobId: taskId, taskId, state: "completed", transferred: size });
    showNotice(t("downloaded", { name: info.fileName ?? entry.name }));
  } catch (cause) {
    // 下载泵失败同样落终态（此前漏标，job 会永远停在 running）。
    if (taskId) tracker.onProgress({ jobId: taskId, taskId, state: "failed", error: errorMessage(cause) });
    showError(cause);
  }
}

function saveBrowserDownload(chunks: Uint8Array[], fileName: string) {
  const blob = new Blob(chunks as unknown as BlobPart[]);
  const url = URL.createObjectURL(blob);
  const anchor = document.createElement("a");
  anchor.href = url;
  anchor.download = fileName;
  anchor.click();
  window.setTimeout(() => URL.revokeObjectURL(url), 10_000);
}

async function downloadSelection(side: PaneSide = "left") {
  // R3-P1-2：按活动栏解析选择集（此前只读左栏，右栏选中时工具栏下载灰置）。
  const pool = side === "right" ? rightSorted.value : sortedEntries.value;
  const sel = side === "right" ? rightSelection.value : selection.value;
  const files = pool.filter((entry) => sel.includes(entry.path) && entry.kind === "file");
  // P2-6：过滤后为空（如只选了目录）不再静默，给出明确提示。
  if (!files.length) {
    showNotice(t("downloadNoneSelected"));
    return;
  }
  for (const entry of files) {
    await downloadEntry(entry, side);
  }
}

function onPreviewDownload(target: string | null) {
  if (!target) return;
  const entry = sortedEntries.value.find((item) => item.path === target)
    ?? rightSorted.value.find((item) => item.path === target);
  if (entry) void downloadEntry(entry, rightSorted.value.some((item) => item.path === target) ? "right" : "left");
}

function onPreviewSaved() {
  void loadDirectory().catch(() => undefined);
  if (dualPane.value) void loadRightDirectory().catch(() => undefined);
  showNotice(t("saved"));
}

async function cancelTransfer(jobId: string) {
  // R3-P2-9：本地伪 job 取消走本地标志（真实中断 runBatch），不再调
  // files/transfer/cancel——sidecar 无该 job 记录仍返回 success（假成功）。
  const flag = batchCancelFlags.get(jobId);
  if (flag) {
    flag.canceled = true;
    showNotice(t("jobCanceled"));
    return;
  }
  try {
    await call("files/transfer/cancel", { taskId: jobId });
    showNotice(t("jobCanceled"));
  } catch (cause) {
    showError(cause);
  }
  void tracker.refresh(invokeAdapter);
}

/** 清理传输历史（P-FILES ⑥）：sidecar 清持久化历史 + 内存完成态 job，本地同步清。 */
async function clearTransferHistory() {
  try {
    await call("files/transfers/clear", {});
    tracker.clearFinished();
    showNotice(t("historyCleared"));
  } catch (cause) {
    showError(cause);
  }
}

function invokeAdapter<T>(method: string, params?: unknown) {
  return window.dbxPlugin.invoke<T>(method, params);
}

// ---- context menu（A-FILES ④b：统一动作面）------------------------------------

function menuAction(action: MenuAction) {
  const menu = contextMenu.value;
  contextMenu.value = undefined;
  if (!menu) return;
  const { entry, side, selection: menuSelection } = menu;
  switch (action) {
    case "open":
      void openEntry(entry, side);
      break;
    case "preview":
    case "archiveContents":
      openPreview(entry.path, side);
      break;
    case "download":
      if (entry.kind === "file") void downloadEntry(entry, side);
      break;
    case "rename":
      startRename(entry, side);
      break;
    case "delete":
      startDelete([entry], side);
      break;
    case "copyPath":
      void window.dbxPlugin.clipboard?.writeText(entry.path).then(() => showNotice(t("copiedPath")));
      break;
    case "copyName":
      void window.dbxPlugin.clipboard?.writeText(baseName(entry.path)).then(() => showNotice(t("copiedName")));
      break;
    case "syncDir":
    case "copyDir":
      startDirJob(action, entry, side);
      break;
    case "copy":
    case "move":
      startCopyMove(action, entry, side);
      break;
    case "extract":
      startExtract(entry, side);
      break;
    case "compress":
      startCompress([entry], side);
      break;
    // ---- 批量（多选右键）------------------------------------------------
    case "downloadSelected":
      void (async () => {
        const files = pickSideEntries(side, menuSelection).filter((item) => item.kind === "file");
        if (!files.length) {
          showNotice(t("downloadNoneSelected"));
          return;
        }
        for (const item of files) await downloadEntry(item, side);
        showNotice(t("downloaded", { name: files[0].name }));
      })();
      break;
    case "copySelected":
    case "moveSelected":
      void transferBetween(side, action === "moveSelected");
      break;
    case "deleteSelected":
      startDelete(pickSideEntries(side, menuSelection), side);
      break;
    case "compressSelected":
      startCompress(pickSideEntries(side, menuSelection), side);
      break;
  }
}

/** 空白区右键：弹插件菜单前先关掉其它菜单（三菜单互斥）。 */
function openBlankMenu(side: PaneSide, payload: { x: number; y: number }) {
  contextMenu.value = undefined;
  sideMenu.value = undefined;
  blankMenu.value = { ...payload, side };
}

function blankMenuAction(action: "newFolder" | "newFile" | "refresh") {
  const menu = blankMenu.value;
  blankMenu.value = undefined;
  if (!menu) return;
  if (action === "refresh") {
    if (menu.side === "left") void refreshDirectory();
    else void refreshRightDirectory();
    return;
  }
  if (action === "newFolder") startNewFolder(menu.side);
  else startNewFile(menu.side);
}

/** 侧栏（目录树/快捷目录）行右键：打开 / 在另一栏打开 / 复制路径、文件名。 */
function openSideMenu(side: PaneSide, payload: { path: string; name: string; x: number; y: number }) {
  contextMenu.value = undefined;
  blankMenu.value = undefined;
  sideMenu.value = { ...payload, side };
}

function sideMenuAction(action: "open" | "openOther" | "copyPath" | "copyName") {
  const menu = sideMenu.value;
  sideMenu.value = undefined;
  if (!menu) return;
  const { side, path: target, name } = menu;
  if (action === "open") {
    navigateQuickPath(side, target);
    return;
  }
  if (action === "openOther") {
    navigateQuickPath(side === "left" ? "right" : "left", target);
    return;
  }
  const value = action === "copyPath" ? target : name;
  void window.dbxPlugin.clipboard?.writeText(value).then(() => showNotice(t(action === "copyPath" ? "copiedPath" : "copiedName")));
}

// ---- lifecycle -----------------------------------------------------------------

// P1-2 窄视口（<900px）策略：进入窄视口时默认一次性收起 dock 与双栏——
// 720px 下「双栏 + dock + 双侧栏」会把左栏主区挤到 ~134px（路径栏/搜索框
// 不可用且被 overflow:hidden 静默裁切）。收起后单栏 + 侧栏仍有 ~570px 主区。
// 只做默认收起：用户随后仍可手动重开（窄窗口下自行取舍），跨回宽视口不改
// 持久化偏好。
let viewportWasNarrow = false;
function syncViewportLayout() {
  const narrow = isNarrowViewport(window.innerWidth);
  if (narrow === viewportWasNarrow) return;
  viewportWasNarrow = narrow;
  if (!narrow) return;
  if (dockOpen.value) dockOpen.value = false;
  if (dualPane.value) dualPane.value = false;
}

async function initialize() {
  const api = await waitForHostApi();
  try {
    hostContext.value = await Promise.any([api.ready, api.request<Record<string, unknown>>("host.getContext")]);
  } catch {
    hostContext.value = api.context ?? {};
  }
  locale.value = api.locale || "zh-CN";
  if (api.appearance) applyAppearance(document.documentElement, api.appearance);
  else if (isDbxPluginTheme(api.theme)) applyAppearance(document.documentElement, themeToAppearance(api.theme));
  // appearance 契约缺失（当前 1.1 桥只推 theme）时订阅 env 主题推送，两套不同时挂。
  if (!api.onAppearanceChange) unsubscribeTheme = onHostThemeChange((theme) => applyAppearance(document.documentElement, themeToAppearance(theme)));
  unsubscribeLocale = api.onLocaleChange?.((next) => (locale.value = next || "zh-CN"));
  unsubscribeContext = api.onContextChange?.((context) => {
    hostContext.value = context;
  });
  unsubscribeEvent = api.onEvent(handleEvent);
  unsubscribeBinary = api.onBinary(handleBinary);
  bindApi((method, params, options) => api.invoke(method, params, options), connectionId.value || null);
  initialized.value = true;
  await loadCapabilities();
  await loadDirectory("/").catch(() => undefined);
  if (dualPane.value) await loadRightDirectory("/").catch(() => undefined);
  await loadQuickPaths("left");
  if (sideTab.value === "tree") ensureTreeRoots();
  if (dualPane.value) {
    // 双栏左栏默认本地（__local__）：起点落到主目录（对标 tiny-rdm/FileZilla）。
    await enterLocalPaneIfAtRoot();
    void loadQuickPaths("right");
  }
  void probeConnections();
  pollTimer = window.setInterval(() => {
    if (pollingDisabled) return;
    tracker
      .refresh(invokeAdapter, connectionId.value)
      .catch((cause) => {
        if (isMethodMissing(cause)) pollingDisabled = true;
      });
  }, 5000);
}

function onContextClick() {
  contextMenu.value = undefined;
  blankMenu.value = undefined;
  sideMenu.value = undefined;
}

function onDocumentKeydown(event: KeyboardEvent) {
  if (event.key !== "Escape") return;
  if (previewPath.value) {
    previewPath.value = null;
    return;
  }
  if (confirmOpen.value) {
    closeConfirm();
    return;
  }
  contextMenu.value = undefined;
  blankMenu.value = undefined;
  sideMenu.value = undefined;
}

function onToolbarNavigate(target: string) {
  markActiveSide("left");
  void loadDirectory(target).catch(() => undefined);
}

/** 右栏路径跳转（路径栏/上一级/刷新）：同时把右栏标记为活动栏（R3-P1-2）。 */
function onRightNavigate(target: string) {
  markActiveSide("right");
  void loadRightDirectory(target).catch(() => undefined);
}

watch(dockTab, (tab) => {
  if (tab === "audit") auditRef.value?.refresh();
});

onMounted(() => {
  document.addEventListener("click", onContextClick);
  document.addEventListener("keydown", onDocumentKeydown);
  window.addEventListener("resize", syncViewportLayout);
  syncViewportLayout();
  void initialize().catch((cause) => {
    error.value = t("operationFailed", { error: errorMessage(cause) });
  });
});

onBeforeUnmount(() => {
  document.removeEventListener("click", onContextClick);
  document.removeEventListener("keydown", onDocumentKeydown);
  window.removeEventListener("resize", syncViewportLayout);
  window.clearTimeout(noticeTimer);
  window.clearInterval(pollTimer);
  unsubscribeEvent?.();
  unsubscribeBinary?.();
  unsubscribeLocale?.();
  unsubscribeContext?.();
  unsubscribeTheme?.();
});
</script>

<template>
  <main class="wb-workbench">
    <div v-if="error" class="wb-error-banner">
      <span :title="errorDetail">{{ error }}</span>
      <!-- R3-P2-10：文本字符 ↻/✕ 换 lucide 图标（对齐 P2-14 先例）。 -->
      <button class="wb-icon-button wb-icon-neutral" :title="t('retry')" @click="retryAfterError"><RefreshCw /></button>
      <button class="wb-icon-button wb-icon-neutral" :title="t('close')" @click="error = ''"><X /></button>
    </div>
    <div v-if="notice" class="wb-notice">{{ notice }}</div>

    <FileToolbar
      :can-write="canWrite"
      :busy="loading"
      :has-selection="toolbarTarget.hasSelection"
      :dock-open="dockOpen"
      :dock-tab="dockTab"
      :dual-pane="dualPane"
      :connection-name="connectionLabel"
      :connection-color="connection.color"
      :read-only="!canWrite"
      :conn-state="connState"
      :t="t"
      @new-folder="startNewFolder(toolbarTarget.side)"
      @upload="onUpload"
      @download="downloadSelection(toolbarTarget.side)"
      @delete="startDelete(toolbarSelectionEntries(toolbarTarget.side), toolbarTarget.side)"
      @toggle-dual-pane="dualPane = !dualPane"
      @toggle-dock="(tab) => { if (dockOpen && dockTab === tab) dockOpen = false; else { dockOpen = true; dockTab = tab; if (tab === 'audit') auditRef?.refresh(); } }"
    />

    <div class="wb-content">
      <!-- 源栏（左栏）：双栏默认本地 __local__，单栏为当前连接 -->
      <section class="wb-pane wb-pane-source" @dragover.prevent @dragenter="dragOverSide = 'left'" @dragleave="dragOverSide = dragOverSide === 'left' ? null : dragOverSide" @drop.prevent="onDropTo('left', $event)">
        <!-- pane 顶条：双栏时放左栏连接选择（与右栏顶条等高对齐）；单栏时整行隐藏 -->
        <div v-if="dualPane" class="wb-pane-topbar">
          <select v-model="leftConnectionId" class="wb-target-connection" :title="t('sourceConnection')" @change="onLeftConnectionChange">
            <option v-for="item in leftConnections" :key="item.id" :value="item.id">{{ item.name }}</option>
          </select>
        </div>
        <div class="wb-pane-body">
          <!-- 侧栏导航：tree（目录树，默认）/ quick（快捷目录）双 tab，可收起 -->
          <SideNavPanel
            side="left"
            :tab="sideTab"
            :collapsed="sideCollapsed"
            :tree-root="leftTree"
            :quick-paths="leftQuickPaths"
            :current-path="path"
            :t="t"
            @update:tab="sideTab = $event"
            @update:collapsed="sideCollapsed = $event"
            @navigate="navigateQuickPath('left', $event)"
            @toggle-node="expandTreeNode('left', $event)"
            @refresh-tree="refreshTree('left')"
            @node-context="openSideMenu('left', $event)"
          />
          <div class="wb-pane-main">
            <div class="wb-pane-header">
              <button class="wb-icon-button wb-icon-neutral" :title="t('up')" :disabled="!path || path === '/'" @click="onToolbarNavigate(parentPath(path))"><ArrowUp /></button>
              <button class="wb-icon-button wb-icon-neutral" :title="t('refresh')" :disabled="loading" @click="markActiveSide('left'); refreshDirectory()"><RefreshCw :class="{ 'wb-spin': loading }" /></button>
              <div class="wb-path-toolbar">
                <PathField :path="path" :t="t" @navigate="onToolbarNavigate" />
                <span class="wb-search-box">
                  <Search class="wb-search-icon" aria-hidden="true" />
                  <input
                    :value="searchQuery"
                    class="wb-search-input"
                    :placeholder="t('searchPlaceholder')"
                    type="search"
                    spellcheck="false"
                    @input="searchQuery = ($event.target as HTMLInputElement).value"
                    @keydown.esc.prevent="searchQuery = ''"
                  />
                </span>
              </div>
            </div>
            <FileTable
              pane-id="left"
              :entries="filteredEntries"
              :selection="selection"
              :active-path="activePath"
              :sort="sort"
              :loading="loading"
              :filtered="Boolean(searchQuery.trim())"
              :t="t"
              @update:selection="setPaneSelection('left', $event)"
              @update:active-path="setPaneActivePath('left', $event)"
              @open="(entry) => { markActiveSide('left'); openEntry(entry, 'left'); }"
              @contextmenu="openContextMenu('left', $event)"
              @blank-context="openBlank('left', $event)"
              @sort="(column) => sortRouted('left', column)"
            />
          </div>
        </div>
        <div v-if="dragOverSide === 'left' && dualPane" class="wb-drop-overlay">{{ t("dropToCopy") }}</div>
      </section>

      <!-- 双栏桥：跨栏 copy/move 按钮（A-FILES ①）。R3-P2-1：copy 与 move 的写
           都发生在目标栏，同受 canWrite 门禁（只读态「复制到目标/源栏」禁用）。 -->
      <div v-if="dualPane" class="wb-pane-bridge">
        <button class="wb-icon-button wb-icon-neutral" :title="t('copyToTarget')" :disabled="!selection.length || !canWrite" @click="transferBetween('left', false)"><Copy /></button>
        <button class="wb-icon-button wb-icon-neutral" :title="t('moveToTarget')" :disabled="!selection.length || !canWrite" @click="transferBetween('left', true)"><ArrowRight /></button>
        <span class="wb-toolbar-separator" />
        <button class="wb-icon-button wb-icon-neutral" :title="t('copyToSource')" :disabled="!rightSelection.length || !canWrite" @click="transferBetween('right', false)"><Copy class="wb-flip-h" /></button>
        <button class="wb-icon-button wb-icon-neutral" :title="t('moveToSource')" :disabled="!rightSelection.length || !canWrite" @click="transferBetween('right', true)"><ArrowLeft /></button>
      </div>

      <!-- 目标栏（右栏）：目标连接浏览（文件概览已改为弹窗，不占右栏 Tab） -->
      <section
        v-if="dualPane"
        class="wb-pane wb-pane-target"
        @dragover.prevent
        @dragenter="dragOverSide = 'right'"
        @dragleave="dragOverSide = dragOverSide === 'right' ? null : dragOverSide"
        @drop.prevent="onDropTo('right', $event)"
      >
        <!-- P2-11：无可切换连接时整个 topbar 不渲染（v-if 提到容器级），不再留 28px 空条 -->
        <div v-if="targetConnections.length" class="wb-pane-topbar">
          <select v-model="targetConnectionId" class="wb-target-connection" :title="t('targetConnection')" @change="markActiveSide('right'); loadRightDirectory(rightPath)">
            <option value="">{{ t("sameConnection") }}</option>
            <option v-for="item in targetConnections" :key="item.id" :value="item.id">{{ item.name }}</option>
          </select>
        </div>
        <div class="wb-pane-body">
          <SideNavPanel
            side="right"
            :tab="sideTab"
            :collapsed="sideCollapsed"
            :tree-root="rightTree"
            :quick-paths="rightQuickPaths"
            :current-path="rightPath"
            :t="t"
            @update:tab="sideTab = $event"
            @update:collapsed="sideCollapsed = $event"
            @navigate="navigateQuickPath('right', $event)"
            @toggle-node="expandTreeNode('right', $event)"
            @refresh-tree="refreshTree('right')"
            @node-context="openSideMenu('right', $event)"
          />
          <div class="wb-pane-main">
            <div class="wb-pane-header">
              <button class="wb-icon-button wb-icon-neutral" :title="t('up')" :disabled="!rightPath || rightPath === '/'" @click="onRightNavigate(parentPath(rightPath))"><ArrowUp /></button>
              <button class="wb-icon-button wb-icon-neutral" :title="t('refresh')" :disabled="rightLoading" @click="markActiveSide('right'); refreshRightDirectory()"><RefreshCw :class="{ 'wb-spin': rightLoading }" /></button>
              <div class="wb-path-toolbar">
                <PathField :path="rightPath" :t="t" @navigate="onRightNavigate" />
                <span class="wb-search-box">
                  <Search class="wb-search-icon" aria-hidden="true" />
                  <input
                    :value="rightSearchQuery"
                    class="wb-search-input"
                    :placeholder="t('searchPlaceholder')"
                    type="search"
                    spellcheck="false"
                    @input="rightSearchQuery = ($event.target as HTMLInputElement).value"
                    @keydown.esc.prevent="rightSearchQuery = ''"
                  />
                </span>
              </div>
            </div>
            <FileTable
              pane-id="right"
              :entries="filteredRightEntries"
              :selection="rightSelection"
              :active-path="rightActivePath"
              :sort="rightSort"
              :loading="rightLoading"
              :filtered="Boolean(rightSearchQuery.trim())"
              :t="t"
              @update:selection="setPaneSelection('right', $event)"
              @update:active-path="setPaneActivePath('right', $event)"
              @open="(entry) => { markActiveSide('right'); openEntry(entry, 'right'); }"
              @contextmenu="openContextMenu('right', $event)"
              @blank-context="openBlank('right', $event)"
              @sort="(column) => sortRouted('right', column)"
            />
          </div>
        </div>
        <div v-if="dragOverSide === 'right' && dualPane" class="wb-drop-overlay">{{ t("dropToCopy") }}</div>
      </section>

      <aside v-if="dockOpen" class="wb-dock">
        <div class="wb-dock-tabs">
          <button :class="{ 'is-active': dockTab === 'transfers' }" @click="dockTab = 'transfers'">{{ t("transferPanel") }}</button>
          <button :class="{ 'is-active': dockTab === 'audit' }" @click="dockTab = 'audit'">{{ t("auditPanel") }}</button>
          <button :class="{ 'is-active': dockTab === 'connection' }" @click="dockTab = 'connection'">{{ t("connectionPanel") }}</button>
        </div>
        <div class="wb-dock-body">
          <TransferPanel v-if="dockTab === 'transfers'" :jobs="transferJobs" :t="t" :retryable-ids="retryableTransferIds" @cancel="cancelTransfer" @clear-history="clearTransferHistory" @retry="retryTransfer" />
          <AuditPanel v-else-if="dockTab === 'audit'" ref="auditRef" :t="t" />
          <div v-else style="display: flex; flex-direction: column; gap: 10px">
            <div class="wb-transfer-item">
              <div class="wb-transfer-title"><strong>{{ connectionLabel }}</strong></div>
              <div class="wb-transfer-meta">
                <span>{{ connection.protocol ?? "" }}</span>
                <span v-if="connection.readOnly || capabilities?.readOnly" class="wb-readonly-badge">{{ t("readOnly") }}</span>
              </div>
            </div>
            <div class="wb-muted" style="font-size: 11px">{{ t("customConfigTitle") }}</div>
            <CustomConfigEditor :t="t" @notice="showNotice" @error="showError" />
          </div>
        </div>
      </aside>
    </div>

    <!-- 文件概览弹窗：来自任一栏的预览/压缩包列表；遮罩点击 / Esc / 关闭按钮均可关闭 -->
    <div v-if="previewPath" class="wb-preview-overlay" @click.self="previewPath = null">
      <PreviewPane
        :path="previewPath"
        :can-write="canWrite"
        :appearance="appearance"
        :connection-id="previewConnectionId"
        :t="t"
        @close="previewPath = null"
        @saved="onPreviewSaved"
        @download="onPreviewDownload"
      />
    </div>

    <!-- 统一右键菜单（A-FILES ④b）：源栏/目标栏共用；多选时切批量动作面。
         R3-P2-8：role="menu"/menuitem 语义。 -->
    <div v-if="contextMenu" ref="menuEl" class="wb-context-menu" role="menu" :style="{ left: `${contextMenu.x}px`, top: `${contextMenu.y}px` }" @click.stop>
      <template v-if="contextMenu.selection.length > 1">
        <button role="menuitem" @click="menuAction('open')">{{ t("openDirectory") }}</button>
        <button role="menuitem" @click="menuAction('downloadSelected')">{{ t("downloadSelected") }}</button>
        <!-- R3-P2-1：只读态「复制到目标栏」与 move 同受 canWrite 门禁（写发生在目标栏）。 -->
        <button v-if="dualPane && canWrite" role="menuitem" @click="menuAction('copySelected')">{{ t("copyToTarget") }}</button>
        <button v-if="dualPane && canWrite" role="menuitem" @click="menuAction('moveSelected')">{{ t("moveToTarget") }}</button>
        <button v-if="canWrite" role="menuitem" @click="menuAction('compressSelected')">{{ t("compressSelected", { count: contextMenu.selection.length }) }}</button>
        <hr />
        <button role="menuitem" class="is-danger" :disabled="!canWrite" @click="menuAction('deleteSelected')">{{ t("deleteSelected") }}</button>
        <hr />
        <button role="menuitem" @click="menuAction('copyPath')">{{ t("copyPath") }}</button>
      </template>
      <template v-else>
        <button v-if="contextMenu.entry.kind === 'directory'" role="menuitem" @click="menuAction('open')">{{ t("openDirectory") }}</button>
        <button v-if="contextMenu.entry.kind === 'file' && !isArchivePath(contextMenu.entry.path)" role="menuitem" @click="menuAction('preview')">{{ t("preview") }}</button>
        <button v-if="contextMenu.entry.kind === 'file' && isArchivePath(contextMenu.entry.path)" role="menuitem" @click="menuAction('archiveContents')">{{ t("archiveContents") }}</button>
        <button v-if="contextMenu.entry.kind === 'file'" role="menuitem" @click="menuAction('download')">{{ t("download") }}</button>
        <button v-if="contextMenu.entry.kind === 'file' && isArchivePath(contextMenu.entry.path) && canWrite" role="menuitem" @click="menuAction('extract')">{{ t("extractTo") }}</button>
        <button v-if="contextMenu.entry.kind === 'directory' && canWrite" role="menuitem" @click="menuAction('syncDir')">{{ t("transferKind.syncDir") }}…</button>
        <button v-if="contextMenu.entry.kind === 'directory' && canWrite" role="menuitem" @click="menuAction('copyDir')">{{ t("transferKind.copyDir") }}…</button>
        <button v-if="canWrite" role="menuitem" @click="menuAction('compress')">{{ t("compress") }}</button>
        <hr />
        <button v-if="canWrite" role="menuitem" @click="menuAction('copy')">{{ t("transferKind.copy") }}…</button>
        <button v-if="canWrite" role="menuitem" @click="menuAction('move')">{{ t("transferKind.move") }}…</button>
        <button v-if="canWrite" role="menuitem" @click="menuAction('rename')">{{ t("rename") }}</button>
        <button role="menuitem" class="is-danger" :disabled="!canWrite" @click="menuAction('delete')">{{ t("delete") }}</button>
        <hr />
        <button role="menuitem" @click="menuAction('copyPath')">{{ t("copyPath") }}</button>
        <button role="menuitem" @click="menuAction('copyName')">{{ t("copyName") }}</button>
      </template>
    </div>

    <!-- 空白区右键菜单（P-FILES）：拦截浏览器默认菜单，给出新建/刷新动作 -->
    <div v-if="blankMenu" ref="menuEl" class="wb-context-menu" role="menu" :style="{ left: `${blankMenu.x}px`, top: `${blankMenu.y}px` }" @click.stop>
      <button :disabled="!canWrite" role="menuitem" @click="blankMenuAction('newFolder')">{{ t("newFolder") }}</button>
      <button :disabled="!canWrite" role="menuitem" @click="blankMenuAction('newFile')">{{ t("newFileTitle") }}</button>
      <hr />
      <button role="menuitem" @click="blankMenuAction('refresh')">{{ t("refresh") }}</button>
    </div>

    <!-- 侧栏右键菜单（P-FILES）：目录树/快捷目录行 → 打开 / 在另一栏打开 / 复制 -->
    <div v-if="sideMenu" ref="menuEl" class="wb-context-menu" role="menu" :style="{ left: `${sideMenu.x}px`, top: `${sideMenu.y}px` }" @click.stop>
      <button role="menuitem" @click="sideMenuAction('open')">{{ t("openDirectory") }}</button>
      <button v-if="dualPane" role="menuitem" @click="sideMenuAction('openOther')">{{ sideMenu.side === "left" ? t("openInRight") : t("openInLeft") }}</button>
      <hr />
      <button role="menuitem" @click="sideMenuAction('copyPath')">{{ t("copyPath") }}</button>
      <button role="menuitem" @click="sideMenuAction('copyName')">{{ t("copyName") }}</button>
    </div>

    <ConfirmDialog
      :open="confirmOpen"
      :title="confirmTitle"
      :body="confirmBody"
      :danger="confirmDanger"
      :danger-list="confirmDangerList"
      :warning="confirmNameIssue"
      :confirm-label="confirmLabel"
      :cancel-label="t('cancel')"
      :busy="confirmBusy"
      @confirm="onConfirm"
      @cancel="closeConfirm"
    >
      <label v-if="confirmInput" style="display: flex; flex-direction: column; gap: 4px">
        <span v-if="confirmKind === 'newFolder'">{{ t("newFolderPlaceholder") }}</span>
        <span v-else-if="confirmKind === 'newFile'">{{ t("newFilePlaceholder") }}</span>
        <span v-else-if="confirmKind === 'rename'">{{ t("renameTitle") }}</span>
        <span v-else>{{ t("pathPlaceholder") }}</span>
        <input v-model="confirmDraft" spellcheck="false" @keydown.enter.prevent="onConfirm" />
      </label>
    </ConfirmDialog>
  </main>
</template>
