<script setup lang="ts">
import { computed, nextTick, onBeforeUnmount, onMounted, reactive, ref, watch, type Component } from "vue";
import {
  Archive,
  ArrowLeft,
  ArrowRight,
  ArrowRightLeft,
  Calculator,
  FileCheck,
  FolderMinus,
  Copy,
  ArrowUp,
  Download,
  Eject,
  ExternalLink,
  Eye,
  FileArchive,
  FileOutput,
  FilePlus,
  FileText,
  FolderInput,
  FolderOpen,
  FolderPlus,
  FolderSymlink,
  Globe,
  HardDrive,
  Keyboard,
  Link,
  Link2,
  PanelLeft,
  PanelRight,
  Pencil,
  RefreshCw,
  Search,
  AppWindow,
  Gauge,
  Scale,
  Share2,
  ShieldCheck,
  Star,
  StarOff,
  Trash2,
  X,
} from "@lucide/vue";
import FileTable from "./components/FileTable.vue";
import FileToolbar from "./components/FileToolbar.vue";
import TransferPanel from "./components/TransferPanel.vue";
import StatsPanel from "./components/StatsPanel.vue";
import SettingsPanel from "./components/SettingsPanel.vue";
import MountDialog from "./components/MountDialog.vue";
import type { SettingsSection } from "./components/SettingsPanel.vue";
import SyncDialog, { type SyncDialogOptions } from "./components/SyncDialog.vue";
import DesktopOnlyCard from "./components/DesktopOnlyCard.vue";
import OpenWithDialog from "./components/OpenWithDialog.vue";
import ConfirmDialog from "./components/ConfirmDialog.vue";
import DropActionDialog from "./components/DropActionDialog.vue";
import AuditPanel from "./components/AuditPanel.vue";
import PreviewPane from "./components/PreviewPane.vue";
import BatchRenameDrawer from "./components/BatchRenameDrawer.vue";
import ShortcutsHelp from "./components/ShortcutsHelp.vue";
import CustomConfigEditor from "./components/CustomConfigEditor.vue";
import PathField from "./components/PathField.vue";
import SideNavPanel from "./components/SideNavPanel.vue";
import { isDbxPluginTheme, onHostThemeChange, themeToAppearance } from "./lib/hostTheme";
import { DBX_POPOVER, resolveAppearance, type DbxPluginAppearanceInput } from "./lib/appearance";
import { bridgeBinaryBytes } from "../../shared/frontend/binaryEvent";
import { useUiIntent, type UiIntentOutcome, type UiIntentSummary } from "../../shared/frontend/uiIntent";
import {
  bindApi,
  baseName,
  call,
  errorMessage,
  formatBytes,
  isMethodMissing,
  joinPath,
  normalizeEntries,
  parentPath,
  type FileCapabilities,
  type FileEntry,
} from "./lib/api";
import { createTransferTracker, isActive, isRetryableKind, type TransferJob, type TransferKind } from "./lib/transfers";
import { inspect, type DangerousHit } from "./lib/dangerousPaths";
import { errorBannerOf, i18nTextOf, workbenchMessage, type ErrorBannerState, type I18nInput, type I18nText } from "./lib/i18n";
import { isArchivePath } from "./lib/archive";
import { PREVIEW_MIN, loadDownloadDir, loadFavorites, loadOpenAppPrefs, loadUiPrefs, persistDownloadDir, persistFavorites, persistOpenAppPrefs, saveUiPrefs, resolveOpenApp, type FavoriteMap, type OpenAppPrefs, type PreviewWin } from "./lib/prefs";
import { sortEntries, toggleSortState, type SortColumn, type SortState } from "./lib/sorting";
import { filterEntries } from "./lib/searchFilter";
import { isLargeDirectory } from "./lib/largeDir";
import { applyTreeChildren, createTreeRoot, markTreeStale, type DirTreeNode } from "./lib/dirTree";
import { normalizeQuickPaths, type QuickPath } from "./lib/quickPaths";
import { isNarrowViewport } from "./lib/responsive";
import { resolveUploadTarget, type UploadTarget } from "./lib/uploadTarget";
import { MENU_ITEM_SELECTOR, onMenuArrowKeys, onTablistArrowKeys, trapTabKey } from "./lib/a11y";
import { isNotFoundMessage, isTransportFailure } from "./lib/friendlyError";
import { createNavGuard } from "./lib/navGuard";
import { resolveToolbarTarget } from "./lib/toolbarTarget";
import { validateFileName } from "./lib/fileName";
import { runBatchTasks } from "./lib/batchRunner";

type ConfirmKind = "delete" | "purge" | "newFolder" | "newFile" | "rename" | "copy" | "move" | "extract" | "compress" | "overwrite" | "check" | "cleanup" | "copyurl";
type PaneSide = "left" | "right";
type MenuAction =
  | "open" | "preview" | "download" | "rename" | "delete" | "copyPath" | "copyName"
  | "syncDir" | "copyDir" | "copy" | "move" | "extract" | "archiveContents" | "compress"
  // 对标 rclone-dashboard：目录体积统计（files/size）与公开链接（files/publicLink）
  | "computeSize" | "copyPublicLink"
  // rclone 深度能力：SUM 校验文件 / 清理空目录 / 目录内容比对（files/check）
  // 批次7：右键 SUM 校验文件 → 核验所在目录（files/checksum/verify）
  | "hashsum" | "rmdirs" | "checkDir" | "verifySum"
  // rclone 双向同步（sync/bisync，beta）
  | "bisyncDir"
  // rclone URL 导入（operations/copyurl）
  | "copyurl"
  // 本地挂载（docs/MOUNT.zh-CN.md M1）：rclone mount 优先，WebDAV 网关兜底
  | "mountLocal"
  // 本机共享（对标 rclone serve 家族）：远端目录经回环 HTTP/WebDAV 分享
  | "serveHttp" | "serveWebdav"
  // 目录打包下载（files/archiveDownload，download/start 同形任务）
  | "archiveDownload"
  // 批量（多选右键，P-FILES 压缩轮 + parity-tools 批量重命名）
  | "downloadSelected" | "copySelected" | "moveSelected" | "deleteSelected" | "compressSelected" | "archiveDownloadSelected" | "batchRenameSelected"
  // 打开方式（远程编辑本地副本，FinalShell 式）：选应用 → 拉临时副本 →
  // 本地保存自动回传远端
  | "openWith";

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
let hostContextVersion = 0;
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
const canWrite = computed(() => !capabilitiesLoading.value && !connection.value.readOnly && !capabilities.value?.readOnly);

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
const loading = ref(true);
const listingFailed = ref(false);
// 顶栏连接状态 pill（对标 ssh session-pill）：存储连接（非本地栏）最近一次
// files/list 成败，在 fetchListing 统一挂钩。
const connState = ref<"connecting" | "connected" | "disconnected">("connecting");

const error = ref<ErrorBannerState>("");
// R5-P2-7 同类收尾：错误横幅/顶部提示不存已翻译字符串，渲染时经 locale 求值
// ——横幅存活期间切 locale 文案即时跟随。errorDetail 为悬停展示的 sidecar 原文
// （P2-1：friendlyError 映射后的文案为主显示）。
const errorText = computed(() => errorBannerOf(error.value, locale.value));
const errorDetail = computed(() => (error.value ? error.value.detail : ""));
// P2-4：错误所属栏位（重试按出错栏位重放，而不是永远只刷左栏）。
const errorSide = ref<PaneSide | "global">("global");
const notice = ref<I18nInput>("");
const noticeText = computed(() => i18nTextOf(notice.value, locale.value));
const capabilities = ref<FileCapabilities | undefined>();
const capabilitiesLoading = ref(false);
const initialized = ref(false);

// ---- 目标栏（右栏，A-FILES ①）---------------------------------------------
// 双栏为会话内开关（工具栏可切），不再持久化：每次打开默认只开远程单栏。
const dualPane = ref(false);
// 左右侧栏状态完全独立；本地左栏默认收藏（quick），右栏默认目录树。
// tab 三态（tree/quick/fav）随偏好持久化（SideTab 含 fav）。
const leftSideTab = ref<"tree" | "quick" | "fav">(prefs.leftSideTab);
const rightSideTab = ref<"tree" | "quick" | "fav">(prefs.rightSideTab);
const leftSideCollapsed = ref(prefs.leftSideCollapsed);
const rightSideCollapsed = ref(prefs.rightSideCollapsed);
const rightPath = ref("/");
const rightEntries = ref<FileEntry[]>([]);
const rightSelection = ref<string[]>([]);
const rightActivePath = ref("");
const rightLoading = ref(false);
const rightListingFailed = ref(false);
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
    const tree = side === "left" ? leftTree.value : rightTree.value;
    try {
      const list = await fetchListing(node.path, sideConnectionId(side));
      if (tree !== (side === "left" ? leftTree.value : rightTree.value)) return;
      applyTreeChildren(tree, node.path, list);
    } catch (cause) {
      if (tree === (side === "left" ? leftTree.value : rightTree.value)) showError(cause);
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
  const id = sideConnectionId(side) ?? connectionId.value;
  const usesHostConnection = sideConnectionId(side) === undefined;
  const version = hostContextVersion;
  try {
    const result = await call<{ paths: QuickPath[] }>("files/quickPaths", { connectionId: id });
    if ((usesHostConnection && version !== hostContextVersion) || id !== (sideConnectionId(side) ?? connectionId.value)) return;
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

// 右侧 dock（transfers/audit/connection）不持久化，默认收起；settings 已拆为
// 独立弹窗（对标 ssh 插件 settings-modal），不再占 dock 页签。
const dockOpen = ref(false);
const dockTab = ref<"transfers" | "audit" | "connection" | "stats">("transfers");
const auditRef = ref<InstanceType<typeof AuditPanel>>();

// ---- 独立设置弹窗（对标 ssh 插件 settings-modal）：左导航分类 + 内容面板 --------
type SettingsCategory = "downloads" | "openWith" | "transfer" | "mounts";
// 每个分类的导航图标（设置弹窗左侧），扫读时先见图再读字。
const SETTINGS_CATEGORY_ICONS = { downloads: Download, openWith: AppWindow, transfer: Gauge, mounts: HardDrive } as const;
// 「本地挂载」分类仅桌面端（canSaveLocal）可见：挂载发生在 sidecar 所在机器。
const settingsCategories = computed<ReadonlyArray<{ id: SettingsCategory; labelKey: string; icon: Component }>>(() =>
  [
    { id: "downloads", labelKey: "settingsNav.downloads", icon: SETTINGS_CATEGORY_ICONS.downloads },
    { id: "openWith", labelKey: "settingsNav.openWith", icon: SETTINGS_CATEGORY_ICONS.openWith },
    { id: "transfer", labelKey: "settingsNav.transfer", icon: SETTINGS_CATEGORY_ICONS.transfer },
    // Web 下也可见：点进去给出「仅桌面客户端支持」的友好卡片，而不是隐藏入口。
    { id: "mounts", labelKey: "settingsNav.mounts", icon: SETTINGS_CATEGORY_ICONS.mounts },
  ],
);
const settingsOpen = ref(false);
// 统一「保存更改」：任一区块草稿变化即点亮；保存走当前区块面板暴露的 save()。
const settingsDirty = ref(false);
/** 设置弹窗记忆尺寸（拖右下角调节；对齐预览浮窗的持久化模式）。 */
const settingsWin = ref<PreviewWin | undefined>(prefs.settingsWin);
const settingsWinStyle = computed(() => {
  if (!settingsWin.value) return undefined;
  return {
    width: `${settingsWin.value.width}px`,
    height: `${settingsWin.value.height}px`,
  };
});

const SETTINGS_MIN = { width: 680, height: 480 };
function clampSettingsSize(width: number, height: number): PreviewWin {
  const maxWidth = Math.max(SETTINGS_MIN.width, window.innerWidth - 40);
  const maxHeight = Math.max(SETTINGS_MIN.height, window.innerHeight - 40);
  return {
    width: Math.min(maxWidth, Math.max(SETTINGS_MIN.width, Math.round(width))),
    height: Math.min(maxHeight, Math.max(SETTINGS_MIN.height, Math.round(height))),
  };
}

let settingsGripActive = false;
function onSettingsGripPointerdown(event: PointerEvent) {
  const start = { x: event.clientX, y: event.clientY, width: settingsWin.value?.width ?? 0, height: settingsWin.value?.height ?? 0 };
  const modal = document.querySelector<HTMLElement>(".wb-settings-modal");
  if (!modal) return;
  if (!settingsWin.value) {
    const rect = modal.getBoundingClientRect();
    start.width = rect.width;
    start.height = rect.height;
  }
  settingsGripActive = true;
  (event.currentTarget as HTMLElement).setPointerCapture(event.pointerId);
  const onMove = (move: PointerEvent) => {
    if (!settingsGripActive) return;
    settingsWin.value = clampSettingsSize(start.width + (move.clientX - start.x), start.height + (move.clientY - start.y));
  };
  const onUp = () => {
    settingsGripActive = false;
    window.removeEventListener("pointermove", onMove);
    window.removeEventListener("pointerup", onUp);
    prefs.settingsWin = settingsWin.value;
    saveUiPrefs({ ...prefs, settingsWin: settingsWin.value });
  };
  window.addEventListener("pointermove", onMove);
  window.addEventListener("pointerup", onUp);
}
const settingsSaving = ref(false);
const settingsPanelRef = ref<{ save: () => Promise<void> } | null>(null);
/** 当前挂到 SettingsPanel 的 section（mounts 是自定义面板，不在组件内）。 */
const panelSection = computed<SettingsSection | undefined>(() =>
  settingsCategory.value === "mounts" ? undefined : settingsCategory.value,
);
const settingsCategory = ref<SettingsCategory>(
  ["downloads", "openWith", "transfer", "mounts"].includes(prefs.settingsCategory as string)
    ? (prefs.settingsCategory as SettingsCategory)
    : "downloads",
);
const settingsOverlayEl = ref<HTMLElement>();

function openSettings(category?: SettingsCategory) {
  // 不带参（工具栏齿轮）= 回到上次停留的分类；显式传参（挂载/下载入口）按意图直达。
  settingsDirty.value = false;
  settingsCategory.value = category ?? (prefs.settingsCategory ?? "downloads");
  settingsOpen.value = true;
  if (category === "mounts") {
    void loadMounts();
    void loadShares();
  }
  void nextTick(() => document.querySelector<HTMLElement>(".wb-settings-nav .is-active")?.focus());
}

function closeSettings() {
  settingsOpen.value = false;
}

watch(settingsCategory, (category) => {
  // 记忆上次停留的分类（重开设置回到原地）；同步回快照避免下次打开读旧值。
  prefs.settingsCategory = category;
  saveUiPrefs({ ...prefs, settingsCategory: category });
  if (category === "mounts") settingsDirty.value = false;
  if (category === "transfer") void loadBwlimit();
  if (category === "mounts") {
    void loadMounts();
    void loadShares();
  }
});

/** 焦点陷阱：Tab 在设置弹窗内循环（同预览/确认弹窗实现）。 */
function onSettingsTabKeydown(event: KeyboardEvent) {
  trapTabKey(event, settingsOverlayEl.value);
}

const previewPath = ref<string | null>(null);
/** 预览条目所属栏连接：openPreview 时固化为快照——单栏预览会顺手开启双栏
 * （左栏随即切到本地），read/write 必须仍指向预览来源连接而非切换后的左栏。 */
const previewConnectionId = ref<string | undefined>(undefined);
/** 预览条目所属栏：关闭时用于把焦点归还该栏列表容器。 */
const previewSide = ref<PaneSide>("left");
const contextMenu = ref<{ x: number; y: number; entry: FileEntry; side: PaneSide; selection: string[] }>();
// 空白区右键（P-FILES）：列表空白处不再弹浏览器菜单，改弹新建/刷新动作面。
const blankMenu = ref<{ x: number; y: number; side: PaneSide }>();
// 侧栏（目录树/快捷目录）行右键：打开 / 在另一栏打开 / 复制路径、文件名。
const sideMenu = ref<{ x: number; y: number; side: PaneSide; path: string; name: string }>();
// 三个右键菜单互斥，共用同一模板 ref；渲染后按视口钳位，
// 避免右键屏幕边缘时菜单溢出被裁。
const menuEl = ref<HTMLElement>();
// 审计#8：右键菜单键盘可达——打开聚焦首项，↑↓/Home/End 在项间移动（Enter
// 由 menuitem 按钮原生触发），Esc 关闭并归还焦点到触发元素；鼠标行为不变。
let menuReturnFocus: HTMLElement | null = null;

function captureMenuOrigin() {
  menuReturnFocus = document.activeElement instanceof HTMLElement ? document.activeElement : null;
}

/** Esc 关闭菜单并归还焦点；点击关闭路径（点击菜单项/空白处）不动焦点。 */
function closeMenusRestoreFocus() {
  contextMenu.value = undefined;
  blankMenu.value = undefined;
  sideMenu.value = undefined;
  menuReturnFocus?.focus();
  menuReturnFocus = null;
}

watch([contextMenu, blankMenu, sideMenu], async () => {
  await nextTick();
  const element = menuEl.value;
  const current = contextMenu.value ?? blankMenu.value ?? sideMenu.value;
  if (!element || !current) return;
  const rect = element.getBoundingClientRect();
  if (rect.right > window.innerWidth - 8) current.x = Math.max(8, window.innerWidth - rect.width - 8);
  if (rect.bottom > window.innerHeight - 8) current.y = Math.max(8, window.innerHeight - rect.height - 8);
  // 审计#8：菜单渲染完成后聚焦首个可用项（禁用项跳过），键盘即可继续操作。
  element.querySelector<HTMLElement>(MENU_ITEM_SELECTOR)?.focus();
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

// P-FILES ⑦：失败任务重试——提交时登记原始请求（方法 + 连接快照 + 参数），
// 失败后 TransferPanel 的 ↻ 按原样重发。会话级 Map（store 里的历史记录
// 无参数形态，跨会话的历史任务不显示重试按钮）。
interface TransferRetryParams {
  method: string;
  params: Record<string, unknown>;
}
const transferRetryParams = reactive(new Map<string, TransferRetryParams>());

function transferRequest(id: string, method: string, params: Record<string, unknown>): TransferRetryParams {
  return {
    method,
    params: {
      ...params,
      connectionId: id,
      // DirJobRequest 不接受 connectionId 作为源/目标连接的默认值。
      ...(method === "files/copyDir" || method === "files/syncDir" || method === "files/check" || method === "files/bisync/start" ? { sourceConnectionId: id, targetConnectionId: id } : {}),
    },
  };
}

/** 登记 sidecar 侧异步 job（提交后 jobId 已返回，首个进度事件未到达前的占位）。 */
function trackSidecarJob(jobId: string, kind: TransferKind, remotePath: string, retry?: TransferRetryParams) {
  registerJob({
    jobId,
    connectionId: String(retry?.params.sourceConnectionId ?? retry?.params.connectionId ?? connectionId.value),
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
  const job = tracker.jobs[jobId];
  if (!retry || job?.state !== "failed" || !isRetryableKind(job.kind) || retryTransferBusy.has(jobId)) return;
  retryTransferBusy.add(jobId);
  try {
    const result = await call<{ success?: boolean; jobId?: string | null; transport?: string }>(retry.method, retry.params);
    const newJobId = result.jobId;
    if (!newJobId) {
      if (result.success !== true || result.transport === "job") {
        showError({ key: "featureMissing", values: { method: retry.method } });
        return;
      }
      transferRetryParams.delete(jobId);
      await Promise.all([
        loadDirectory().catch(() => undefined),
        dualPane.value ? loadRightDirectory().catch(() => undefined) : Promise.resolve(),
      ]);
      refreshAuditPanel();
      showNotice({ key: "transferStatus.completed" });
      return;
    }
    trackSidecarJob(String(newJobId), job.kind, job.remotePath ?? "", retry);
    transferRetryParams.delete(jobId);
    showNotice({ key: "jobStarted", values: { name: baseName(String(retry.params.targetPath ?? retry.params.newPath ?? "")) } });
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
// R5-P2-7：弹层标题/正文改存 i18n key + 参数（i18nTextOf 渲染时经 t 求值）——
// 弹层打开中途切 locale（宿主/ mock 顶栏切换），标题正文即时跟随新语言，
// 不再固化打开瞬间的译文形成混语言窗口。数组形态用于既有双段拼接正文。
const confirmTitle = ref<I18nText>({ key: "" });
const confirmBody = ref<I18nText | I18nText[]>({ key: "" });
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
const confirmInput = computed(() => confirmKind.value === "newFolder" || confirmKind.value === "newFile" || confirmKind.value === "rename" || confirmKind.value === "copy" || confirmKind.value === "move" || confirmKind.value === "extract" || confirmKind.value === "compress" || confirmKind.value === "check" || confirmKind.value === "copyurl");
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
/** R5-P2-7：弹层标题/正文渲染时求值，随 locale 响应式刷新。 */
const confirmTitleText = computed(() => i18nTextOf(confirmTitle.value, locale.value));
const confirmBodyText = computed(() => i18nTextOf(confirmBody.value, locale.value));

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
/** FileTable 选择回写（同时把该栏标记为活动栏）；选中行变化后发快照型 report。 */
function setPaneSelection(side: PaneSide, paths: string[]) {
  markActiveSide(side);
  if (side === "left") selection.value = paths;
  else rightSelection.value = paths;
  reportPaneSnapshot(side);
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
  captureMenuOrigin();
  contextMenu.value = { ...payload, side, selection: [...(side === "left" ? selection.value : rightSelection.value)] };
}
/** 多选菜单的条目集（批量打包下载「全为目录」门控据此计算）。 */
const contextMenuEntries = computed(() =>
  contextMenu.value ? pickSideEntries(contextMenu.value.side, contextMenu.value.selection) : [],
);
const contextMenuAllDirs = computed(() =>
  contextMenuEntries.value.length > 0 && contextMenuEntries.value.every((entry) => entry.kind === "directory"),
);
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

// ---- 收藏夹（rclone-ui parity）------------------------------------------------
// 按连接键入的星标目录（prefs 落 localStorage，重启恢复）：切换连接/本地栏
// 即切换列表。星标入口 = 工具栏星标（活动栏当前目录）+ 侧栏行右键「收藏」；
// SideNavPanel fav tab 只读列表，增删全部回流 App 单一数据源。
const favoriteMap = ref<FavoriteMap>(loadFavorites());

/** 收藏键 = 该栏当前生效的连接 id（undefined 即宿主当前连接；本地栏为 __local__）。 */
function sideFavoriteKey(side: PaneSide): string {
  return sideConnectionId(side) ?? connectionId.value;
}

function favoritesOf(side: PaneSide): string[] {
  return favoriteMap.value[sideFavoriteKey(side)] ?? [];
}

const leftFavorites = computed(() => favoritesOf("left"));
const rightFavorites = computed(() => favoritesOf("right"));

/** 工具栏星标态：活动栏当前目录是否已收藏（点亮即再次点击取消）。 */
const toolbarStarred = computed(() => favoritesOf(toolbarTarget.value.side).includes(paneDirPath(toolbarTarget.value.side)));

/** 收藏切换：target 缺省取该栏当前目录；空连接 id（宿主未就绪）不动。 */
function toggleFavorite(side: PaneSide, target?: string) {
  const key = sideFavoriteKey(side);
  const targetPath = target ?? paneDirPath(side);
  if (!key || !targetPath) return;
  const current = favoriteMap.value[key] ?? [];
  const next = current.includes(targetPath) ? current.filter((item) => item !== targetPath) : [...current, targetPath];
  const map = { ...favoriteMap.value };
  if (next.length) map[key] = next;
  else delete map[key];
  favoriteMap.value = map;
  persistFavorites(map);
}

/** 侧栏右键菜单的收藏态：决定「收藏 / 从收藏移除」文案与图标。 */
const sideMenuFavorited = computed(() => {
  const menu = sideMenu.value;
  return Boolean(menu && favoritesOf(menu.side).includes(menu.path));
});

/** 工具栏挂载入口：挂活动栏当前目录；本地 __local__ 栏没有远端可挂（禁用）。 */
// 挂载是桌面能力：web/docker 下 sidecar 不在用户本机，挂载无从谈起
// （mountStatus/mount 走的都是 sidecar 所在机器），入口整体隐藏。
const canUseMount = computed(() => canSaveLocal.value);
const canMountToolbar = computed(() => canUseMount.value && sideConnectionId(toolbarTarget.value.side) !== LOCAL_CONNECTION_ID);

function mountToolbarTarget() {
  if (!canMountToolbar.value) return;
  openMountDialog(paneDirPath(toolbarTarget.value.side), sideConnectionId(toolbarTarget.value.side) ?? connectionId.value);
}

// 双栏开关不持久化；两侧侧栏状态分别持久化，切换一侧不影响另一侧。
watch([sort, leftSideTab, rightSideTab, leftSideCollapsed, rightSideCollapsed], () => {
  // 合并进快照再整体写入：这里曾用"只含本组键的新对象"覆盖，抹掉
  // previewWin/settingsWin/settingsCategory 等其他键（跨键互踩 bug）。
  Object.assign(prefs, {
    sort: sort.value,
    leftSideTab: leftSideTab.value,
    rightSideTab: rightSideTab.value,
    leftSideCollapsed: leftSideCollapsed.value,
    rightSideCollapsed: rightSideCollapsed.value,
  });
  saveUiPrefs({ ...prefs });
}, { deep: true });

// 任一侧切到 tree tab 时懒加载对应根目录。
watch(leftSideTab, (tab) => {
  if (tab === "tree") void expandTreeNode("left", leftTree.value);
});
watch(rightSideTab, (tab) => {
  if (tab === "tree") void expandTreeNode("right", rightTree.value);
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
    // 两栏独立首载，远端不等待本地 quick paths 探测；右栏默认 tree
    // 不会触发 rightSideTab watcher（值没有发生变化），所以这里显式首展开。
    void loadRightDirectory().catch(() => undefined);
    if (rightSideTab.value === "tree") void expandTreeNode("right", rightTree.value);
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
let errorTimer = 0;
let pollTimer = 0;
let pollingDisabled = false;
let unsubscribeEvent: (() => void) | undefined;
let unsubscribeBinary: (() => void) | undefined;
let unsubscribeContext: (() => void) | undefined;
let unsubscribeInit: (() => void) | undefined;
let unsubscribeTheme: (() => void) | undefined;

/** 立即清掉当前横幅：行内报错出现时不让旧的成功提示同屏误导。 */
function hideNotice() {
  window.clearTimeout(noticeTimer);
  notice.value = "";
}

function showNotice(message: I18nInput) {
  notice.value = message;
  window.clearTimeout(noticeTimer);
  noticeTimer = window.setTimeout(() => (notice.value = ""), 4000);
}

function showError(cause: unknown, side: PaneSide | "global" = "global") {
  errorSide.value = side;
  notice.value = "";
  window.clearTimeout(noticeTimer);
  window.clearTimeout(errorTimer);
  // 组件 emit 的 key + 参数错误（CustomConfigEditor）：整条横幅存 I18nText 惰性求值。
  if (cause && typeof cause === "object" && "key" in (cause as Record<string, unknown>)) {
    error.value = { kind: "i18n", text: cause as I18nText };
  } else {
    const message = errorMessage(cause);
    if (isMethodMissing(cause)) {
      const method = cause instanceof Error && "method" in cause ? String((cause as { method?: string }).method) : "";
      error.value = { kind: "i18n", text: { key: "featureMissing", values: { method } }, detail: message };
    } else {
      // P2-1：已知错误类别映射七语文案；未知错误原文透传（横幅 title 保留原文）。
      // friendlyRaw 在渲染时重跑 friendlyError，横幅存活期间切 locale 内层同步跟随。
      error.value = { kind: "failure", detail: message, friendlyRaw: message };
    }
  }
  // 审计快赢#1：错误横幅 3s 自动消失来不及读完，延长到 8s；role=alert 保证
  // 屏幕阅读器即时播报（横幅条件渲染，插入即触发播报）。
  errorTimer = window.setTimeout(() => (error.value = ""), 8000);
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

function handleEvent(event: DbxPluginEvent) {
  // 当前 SDK 先更新 api.locale，再经 onEvent 投递 env；这里只更新 Files 的状态。
  if (event.type === "env") {
    locale.value = window.dbxPlugin.locale || "zh-CN";
    return;
  }
  if (event.method === "files/remote-edit/state") {
    // 打开方式（远程编辑）会话状态：opened/synced 顶部提示，error 错误条。
    const state = event.params as { remotePath?: string; state?: string; error?: string };
    const name = state.remotePath ? baseName(state.remotePath) : "";
    if (state.state === "opened") showNotice(t("remoteEditOpened", { name }));
    else if (state.state === "synced") showNotice(t("remoteEditSynced", { name }));
    else if (state.state === "error") showError(new Error(t("remoteEditFailed", { error: state.error ?? "" })));
    return;
  }
  if (event.method === "files/transfer/progress") {
    const progress = event.params as Parameters<typeof tracker.onProgress>[0];
    const job = tracker.onProgress(progress);
    // 失败事件必须给出全局错误条；部分宿主只返回 failed 状态而不带 error，
    // 仍给出可操作的兜底提示，避免移动失败时只在传输面板里静默结束。
    if (progress.state === "failed") showError(progress.error ? new Error(progress.error) : { key: "transferFailed" });
    // P-FILES ①b：transport=job 的 copy/move/rename 在终态后自动刷新目录。
    if (job && awaitingRefresh.has(job.jobId) && !isActive(job.state)) {
      awaitingRefresh.delete(job.jobId);
      // 失败/取消时不要用一次成功的目录刷新把错误条清掉；只有真正完成
      // 的 copy/move/rename 才需要同步两侧目录。
      if (job.state === "completed") {
        void loadDirectory().catch(() => undefined);
        if (dualPane.value) void loadRightDirectory().catch(() => undefined);
      }
      refreshAuditPanel();
    }
  }
}

// -- MCP UI intent 通道（M2，shared/frontend/uiIntent 公共层） -----------------
// sidecar 的 files_ui_focus / files_ui_search / files_ui_select 生成 intentId
// 后经 `files/ui/intent` 事件下发（files/backend/src/mcp.rs 为行为权威）；
// 本节落 UI（面板切换 / 填 path 触发既有导航 / 按 path 定位高亮）并回报
// `files/ui/state/report`；关键动作后另发无 intentId 的快照型 report。

const INTENT_CELL_WIDTH = 120;

/** 当前工作台面板（快照/摘要用）：设置弹窗 > dock 页签 > 主区 browse。 */
function currentIntentPanel(): string {
  if (settingsOpen.value) return "settings";
  return dockOpen.value ? dockTab.value : "browse";
}

/** 每 cell 截 120 字符（与 sidecar digest 的 cellWidth 默认一致）；非字符串原样。 */
function truncateIntentCell(value: unknown): unknown {
  if (typeof value !== "string") return value;
  return [...value].length > INTENT_CELL_WIDTH ? `${[...value].slice(0, INTENT_CELL_WIDTH).join("")}…` : value;
}

/** summary 行（前 5 行）：path 是定位字段不截断，其余 cell 截 120。 */
function intentRowOf(entry: FileEntry) {
  return { path: entry.path, name: truncateIntentCell(entry.name), kind: entry.kind, size: entry.size, modifiedAt: truncateIntentCell(entry.modifiedAt) };
}

/** intent 摘要：count + 前 5 行（形状对齐 files/docs/MCP.zh-CN.md report 段）。 */
function summarizeListing(): UiIntentSummary {
  const rows = sortedEntries.value.slice(0, 5).map(intentRowOf);
  return { panel: currentIntentPanel(), count: entries.value.length, rows, ...(rows[0] ? { anchor: rows[0].path } : {}) };
}

/** 快照型 report：当前面板、path、结果计数、选中项 path（设计 §1「UI 快照」，
 * `files_ui_state` 不带 intentId 时返回）。回报失败由公共层静默收敛。 */
function reportPaneSnapshot(side: PaneSide) {
  const left = side === "left";
  const sel = left ? selection.value : rightSelection.value;
  uiIntent.reportSnapshot({
    panel: currentIntentPanel(),
    path: left ? path.value : rightPath.value,
    count: (left ? entries.value : rightEntries.value).length,
    ...(sel.length ? { anchor: sel[0] } : {}),
  });
}

const uiIntentHandlers = {
  focus: async (params: Record<string, unknown>): Promise<UiIntentOutcome> => {
    const panel = String(params.panel ?? "");
    if (panel === "browse") {
      // 主区常驻：收起 dock 并关掉遮挡弹层，让浏览面可见（ldap 同语义）。
      dockOpen.value = false;
      previewPath.value = null;
      uiIntent.reportSnapshot({ panel: "browse", path: path.value, count: entries.value.length });
      return { status: "applied", summary: { panel } };
    }
    if (panel === "transfers" || panel === "audit" || panel === "stats") {
      dockOpen.value = true;
      dockTab.value = panel;
      if (panel === "audit") auditRef.value?.refresh();
      uiIntent.reportSnapshot({ panel });
      return { status: "applied", summary: { panel } };
    }
    if (panel === "settings") {
      // 设置已是独立弹窗：关掉 dock 让位，弹窗按 intent 面板打开。
      dockOpen.value = false;
      openSettings("downloads");
      uiIntent.reportSnapshot({ panel });
      return { status: "applied", summary: { panel } };
    }
    return { status: "rejected", reason: t("intent.unknownPanel") };
  },
  search: async (params: Record<string, unknown>): Promise<UiIntentOutcome> => {
    const target = String(params.path ?? "").trim();
    if (!target) return { status: "rejected", reason: "path is required" };
    if (loading.value) return { status: "rejected", reason: "directory listing is busy" };
    // 复用既有导航管线（loadDirectory → files/list）：PathField 随 path 变化
    // 同步展示，结果留在 UI，用户可继续操作/返回——条件可视可撤销。
    markActiveSide("left");
    try {
      await loadDirectory(target);
    } catch (cause) {
      return { status: "rejected", reason: errorMessage(cause) };
    }
    showNotice(t("intent.applied"));
    return { status: "applied", summary: summarizeListing() };
  },
  select: async (params: Record<string, unknown>): Promise<UiIntentOutcome> => {
    const target = String(params.path ?? "").trim();
    if (!target) return { status: "rejected", reason: "path is required" };
    const leftRow = sortedEntries.value.find((entry) => entry.path === target);
    const rightRow = dualPane.value ? rightSorted.value.find((entry) => entry.path === target) : undefined;
    const row = leftRow ?? rightRow;
    if (!row) return { status: "rejected", reason: t("intent.selectMissing") };
    // FileTable 按 selection/activePath 高亮命中行；快照上报由 setPaneSelection 承接。
    const side: PaneSide = leftRow ? "left" : "right";
    setPaneSelection(side, [target]);
    setPaneActivePath(side, target);
    return { status: "applied", summary: { count: 1, anchor: target, rows: [intentRowOf(row)] } };
  },
};

const uiIntent = useUiIntent("files", uiIntentHandlers);

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
  const version = hostContextVersion;
  const params: Record<string, unknown> = { path: target };
  if (explicitConnectionId) params.connectionId = explicitConnectionId;
  // 连接状态 pill：任一非本地栏的 files/list 都反映存储连接健康度（本地
  // __local__ 恒可用，不代表连接）；主连接 id 在部分宿主/mock 的 context
  // 里缺失，无法按 id 精确归因，按"非本地"判定。
  const hitsHost = explicitConnectionId !== LOCAL_CONNECTION_ID;
  if (hitsHost) connState.value = "connecting";
  try {
    const result = await call<{ entries: FileEntry[] }>("files/list", params);
    if (hitsHost && version === hostContextVersion) connState.value = "connected";
    return normalizeEntries(result.entries ?? []);
  } catch (cause) {
    // P2-3：pill 与单次业务失败解耦——仅网络/传输层失败置「已断开」；业务错误
    // （NotFound、权限、参数类）说明 sidecar 应答了连接，置「已连接」而非断开，
    // 也避免失败期间停留在「连接中」抖动。
    if (hitsHost && version === hostContextVersion) connState.value = isTransportFailure(errorMessage(cause)) ? "disconnected" : "connected";
    throw cause;
  }
}

async function loadDirectory(target?: string) {
  const next = target ?? path.value;
  const token = leftNav.next();
  loading.value = true;
  listingFailed.value = false;
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
    // 导航完成后的快照型 report（含 intent search 触发的导航）。
    reportPaneSnapshot("left");
  } catch (cause) {
    // 过期请求的失败同样不打扰新目录（横幅不闪旧导航的错误）。
    if (!leftNav.isCurrent(token)) return;
    listingFailed.value = true;
    selection.value = [];
    activePath.value = "";
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
  rightListingFailed.value = false;
  try {
    const list = await fetchListing(next, targetConnectionId.value || undefined);
    if (!rightNav.isCurrent(token)) return;
    rightEntries.value = list;
    rightPath.value = next;
    rightSelection.value = [];
    rightActivePath.value = "";
    if (isLargeDirectory(rightEntries.value.length)) showNotice(t("largeDirectory", { count: rightEntries.value.length }));
    reportPaneSnapshot("right");
  } catch (cause) {
    if (!rightNav.isCurrent(token)) return;
    rightListingFailed.value = true;
    rightSelection.value = [];
    rightActivePath.value = "";
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
  previewConnectionId.value = sideConnectionId(side) ?? connectionId.value;
  previewSide.value = side;
  previewMinimized.value = false;
}

// 审计#7：预览弹窗 dialog 化——Esc/遮罩/关闭钮此前会静默丢弃 CodeMirror
// 未保存草稿。关闭统一走 closePreview：脏草稿（editing 且内容已改）先弹
// 丢弃确认，确认后才真正关闭；干净态维持原直关行为。
const previewRef = ref<InstanceType<typeof PreviewPane>>();
const previewDiscardOpen = ref(false);
const previewOverlayEl = ref<HTMLElement>();
const previewTitle = computed(() => (previewPath.value ? baseName(previewPath.value) : ""));

// 对标 rclone-dashboard media-preview-overlay：浮窗可拖拽缩放（右下角握把，
// 尺寸记忆进 prefs），可最小化成右下角悬浮 pill（面板保持挂载，编辑草稿不丢）。
const previewMinimized = ref(false);
const previewWin = ref<PreviewWin | undefined>(prefs.previewWin);
const previewPanelStyle = computed(() => {
  if (!previewWin.value) return undefined;
  return {
    "--preview-w": `${previewWin.value.width}px`,
    "--preview-h": `${previewWin.value.height}px`,
  };
});
let previewGripActive = false;

function clampPreviewSize(width: number, height: number): PreviewWin {
  const maxWidth = Math.max(PREVIEW_MIN.width, window.innerWidth - 40);
  const maxHeight = Math.max(PREVIEW_MIN.height, window.innerHeight - 40);
  return {
    width: Math.min(maxWidth, Math.max(PREVIEW_MIN.width, Math.round(width))),
    height: Math.min(maxHeight, Math.max(PREVIEW_MIN.height, Math.round(height))),
  };
}

function onPreviewGripPointerdown(event: PointerEvent) {
  const start = { x: event.clientX, y: event.clientY, width: previewWin.value?.width ?? 0, height: previewWin.value?.height ?? 0 };
  const overlay = previewOverlayEl.value;
  const panel = overlay?.querySelector<HTMLElement>(".wb-preview");
  if (!panel) return;
  if (!previewWin.value) {
    const rect = panel.getBoundingClientRect();
    start.width = rect.width;
    start.height = rect.height;
  }
  previewGripActive = true;
  (event.currentTarget as HTMLElement).setPointerCapture(event.pointerId);
  const onMove = (move: PointerEvent) => {
    if (!previewGripActive) return;
    previewWin.value = clampPreviewSize(start.width + (move.clientX - start.x) * 2, start.height + (move.clientY - start.y) * 2);
  };
  const onUp = () => {
    previewGripActive = false;
    window.removeEventListener("pointermove", onMove);
    window.removeEventListener("pointerup", onUp);
    if (previewWin.value) {
      prefs.previewWin = previewWin.value;
      saveUiPrefs({ ...prefs, previewWin: previewWin.value });
    }
  };
  window.addEventListener("pointermove", onMove);
  window.addEventListener("pointerup", onUp);
}

function minimizePreview() {
  previewMinimized.value = true;
  void nextTick(() => document.querySelector<HTMLElement>(".wb-preview-pill")?.focus());
}

function restorePreview() {
  previewMinimized.value = false;
  void nextTick(() => previewOverlayEl.value?.focus());
}

function closePreview() {
  if (previewRef.value?.isDirty) {
    previewDiscardOpen.value = true;
    return;
  }
  const side = previewSide.value;
  previewPath.value = null;
  previewMinimized.value = false;
  // 关闭后把焦点确定性归还所属栏列表容器：PreviewPane 卸载时的 returnFocusTo
  // 是打开瞬间捕获的 activeElement，经右键菜单等触发元素已随菜单卸载的路径打开
  // 时捕获到 body，归还落空——Esc 关闭后焦点悬空，紧随的双击存在首击被吞、
  // dblclick 判定失效的窗口。这里等 overlay 卸载完成后按来源栏 data-pane-id 找回
  // 列表容器（viewport tabindex=0，键盘 Enter/方向键链路随之恢复）。
  void nextTick(() => {
    document.querySelector<HTMLElement>(`.wb-file-scroll[data-pane-id="${side}"]`)?.focus();
  });
}

/** 焦点陷阱：Tab 在预览内循环（同 ConfirmDialog 实现）；defaultPrevented
 * （CodeMirror 已消费 Tab 缩进）时由 a11y 层跳过，不与编辑器键位冲突。
 * 最小化态面板隐藏，陷阱不参与（焦点落在悬浮 pill 上）。 */
function onPreviewTabKeydown(event: KeyboardEvent) {
  if (previewMinimized.value) return;
  trapTabKey(event, previewOverlayEl.value);
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
  const version = hostContextVersion;
  capabilitiesLoading.value = true;
  try {
    const result = await call<FileCapabilities>("files/capabilities");
    if (version === hostContextVersion) capabilities.value = result;
  } catch (cause) {
    if (version === hostContextVersion && !isMethodMissing(cause)) showError(cause);
  } finally {
    if (version === hostContextVersion) capabilitiesLoading.value = false;
  }
}

/**
 * A-FILES ①：探测宿主是否提供连接枚举（host.listConnections）。
 * 宿主未提供时目标栏仅支持「同连接另一路径」；跨连接能力见交接文档。
 */
async function probeConnections() {
  const version = hostContextVersion;
  try {
    const list = await window.dbxPlugin.request<Array<Record<string, unknown>>>("host.listConnections");
    if (version !== hostContextVersion) return;
    if (Array.isArray(list)) {
      targetConnections.value = list
        .map((item) => ({ id: String(item.id ?? item.connectionId ?? ""), name: String(item.name ?? item.id ?? item.connectionId ?? "") }))
        .filter(
          (item) =>
            item.id && item.id !== connectionId.value && item.id !== LOCAL_CONNECTION_ID,
        );
    }
  } catch {
    if (version === hostContextVersion) targetConnections.value = [];
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
async function pathExists(id: string, target: string): Promise<boolean | undefined> {
  try {
    await call("files/stat", { connectionId: id, path: target });
    return true;
  } catch (cause) {
    return isNotFoundMessage(errorMessage(cause)) ? false : undefined;
  }
}

/** 重名预检失败：横幅提示 + 弹层保持打开（用户可直接改名重提）。 */
function rejectDuplicate(name: string) {
  error.value = { kind: "i18n", text: { key: "nameExists", values: { name } } };
}

function openConfirm(kind: ConfirmKind, options: {
  title: I18nText;
  body?: I18nText | I18nText[];
  danger?: boolean;
  hits?: DangerousHit[];
  target?: { path?: string; entry?: FileEntry; targets?: FileEntry[] };
  draft?: string;
  side?: PaneSide;
}) {
  confirmKind.value = kind;
  confirmTitle.value = options.title;
  confirmBody.value = options.body ?? { key: "" };
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
  openConfirm("newFolder", { title: { key: "newFolderTitle" }, draft: "", side });
}

/** 新建文件（P-FILES）：复用 files/write 写空内容（≤MAX_INLINE_WRITE_BYTES）。 */
function startNewFile(side: PaneSide = "left") {
  openConfirm("newFile", { title: { key: "newFileTitle" }, draft: "", side });
}

function startRename(entry: FileEntry, side: PaneSide) {
  markActiveSide(side);
  confirmDraft.value = entry.name;
  openConfirm("rename", { title: { key: "renameTitle" }, target: { entry }, draft: entry.name, side });
}

function startDelete(targets: FileEntry[], side: PaneSide = "left") {
  if (!targets.length) return;
  markActiveSide(side);
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
      title: { key: "deleteTitle", values: { count: targets.length } },
      body: [{ key: "deleteBody" }, { key: "deleteRecursiveWarn" }],
      danger: worst.level === "danger",
      hits: worst.hits,
      target: { targets },
      side,
    });
    return;
  }
  openConfirm("delete", {
    title: { key: "deleteTitle", values: { count: targets.length } },
    body: inspection.level === "none" ? { key: "deleteBody" } : [{ key: "deleteBody" }, { key: "deleteRecursiveWarn" }],
    danger: inspection.level === "danger",
    hits: inspection.hits,
    target: { targets },
    side,
  });
}

function startDirJob(kind: "syncDir" | "copyDir" | "bisync", entry: FileEntry, side: PaneSide) {
  const defaultTarget = joinPath("/", `${baseName(entry.path) || "copy"}`);
  // 同步选项走独立顶层 SyncDialog（dry-run/过滤/备份/并发参数），连接在打开
  // 时固化——双栏下右键动作挂该栏连接，而不是活动栏。
  syncDialogKind.value = kind;
  syncDialogEntry.value = entry;
  syncDialogSide.value = side;
  syncDialogDraft.value = defaultTarget;
  syncDialogSourceConnectionId.value = sideConnectionId(side) ?? connectionId.value;
  syncDialogOpen.value = true;
  // 双向同步：先查路径对状态（决定 resync 引导），查完前 state 为 null。
  if (kind === "bisync") {
    const source = entry;
    const target = defaultTarget;
    const id = syncDialogSourceConnectionId.value;
    syncDialogBisyncState.value = null;
    void call<{ state: "synced" | "new" }>("files/bisync/state", {
      connectionId: id,
      sourceConnectionId: id,
      targetConnectionId: id,
      sourcePath: source.path,
      targetPath: target,
    })
      .then((result) => {
        if (syncDialogOpen.value && syncDialogEntry.value?.path === source.path) {
          syncDialogBisyncState.value = result.state;
        }
      })
      .catch(() => {
        if (syncDialogOpen.value) syncDialogBisyncState.value = "new";
      });
  } else {
    syncDialogBisyncState.value = null;
  }
}

// ---- 目录同步/复制（独立顶层弹窗 SyncDialog）--------------------------------
const syncDialogOpen = ref(false);
const syncDialogKind = ref<"syncDir" | "copyDir" | "bisync">("syncDir");
const syncDialogEntry = ref<FileEntry | null>(null);
const syncDialogSide = ref<PaneSide>("right");
const syncDialogDraft = ref("");
const syncDialogSourceConnectionId = ref("");
const syncDialogBusy = ref(false);
const syncDialogBisyncState = ref<"synced" | "new" | null>(null);

function closeSyncDialog() {
  syncDialogOpen.value = false;
  syncDialogEntry.value = null;
}

/** SyncDialog 确认：非空字段才下发（保持 rclone 默认）；dry-run 同样入传输
 * 面板跟踪，完成即终态、不落任何写。 */
async function onSyncDialogConfirm(options: SyncDialogOptions) {
  const entry = syncDialogEntry.value;
  const kind = syncDialogKind.value;
  if (!entry || syncDialogBusy.value) return;
  syncDialogBusy.value = true;
  const side = syncDialogSide.value;
  const id = syncDialogSourceConnectionId.value;
  let jobStarted = false;
  try {
    const method = kind === "bisync" ? "files/bisync/start" : `files/${kind}`;
    const retry = transferRequest(id, method, {
      sourcePath: entry.path,
      targetPath: options.targetPath,
      ...(kind === "bisync" && options.bisyncResync ? { mode: "resync", resyncMode: "newer" } : {}),
      ...(options.dryRun ? { dryRun: true } : {}),
      ...(options.include.length ? { include: options.include } : {}),
      ...(options.exclude.length ? { exclude: options.exclude } : {}),
      ...(options.backupDir ? { backupDir: options.backupDir } : {}),
      ...(options.suffix ? { suffix: options.suffix } : {}),
      // 批次6条件过滤：仅 sync/copy 下发（bisync 不带过滤器，后端对
      // check 也不注入——语义会收窄比对报告）。
      ...(kind !== "bisync" && options.metadata ? { metadata: true } : {}),
      ...(kind !== "bisync" && options.minSize ? { minSize: options.minSize } : {}),
      ...(kind !== "bisync" && options.maxSize ? { maxSize: options.maxSize } : {}),
      ...(kind !== "bisync" && options.minAge ? { minAge: options.minAge } : {}),
      ...(kind !== "bisync" && options.maxAge ? { maxAge: options.maxAge } : {}),
      ...(options.transfers !== null ? { transfers: options.transfers } : {}),
      ...(options.checkers !== null ? { checkers: options.checkers } : {}),
      ...(options.retries !== null ? { retries: options.retries } : {}),
    });
    const result = await call<{ jobId: string }>(retry.method, retry.params);
    if (result.jobId) {
      trackSidecarJob(result.jobId, kind === "bisync" ? "bisync" : kind, `${entry.path} ⇄ ${options.targetPath}`, retry);
    }
    jobStarted = true;
    closeSyncDialog();
    refreshAuditPanel();
    showNotice(
      kind === "bisync"
        ? t("bisyncStarted")
        : t(options.dryRun ? "syncDryRunStarted" : "jobStarted", { name: baseName(options.targetPath) }),
    );
  } catch (cause) {
    showError(cause);
  } finally {
    syncDialogBusy.value = false;
  }
  if (jobStarted) {
    await loadDirectory().catch(() => undefined);
    if (side === "right" && dualPane.value) await loadRightDirectory().catch(() => undefined);
  }
}

/** P-FILES ①b：copy/move 动作（native 同步返回 / 降级 jobId 由后端决定）。 */
function startCopyMove(kind: "copy" | "move", entry: FileEntry, side: PaneSide) {
  const parent = parentPath(entry.path);
  const draft = kind === "copy"
    ? joinPath(parent, `${entry.name}-copy`)
    : joinPath("/", entry.name);
  openConfirm(kind, {
    title: { key: kind === "copy" ? "copyTitle" : "moveTitle" },
    body: { key: kind === "copy" ? "copyBody" : "moveBody" },
    target: { entry },
    draft,
    side,
  });
}

/** A-FILES ③：压缩包解压骨架——等后端 files/extract（交接），方法未注册时给出 featureMissing 提示。 */
function startExtract(entry: FileEntry, side: PaneSide) {
  openConfirm("extract", {
    title: { key: "extractTitle", values: { name: entry.name } },
    body: { key: "extractBody" },
    target: { entry },
    draft: parentPath(entry.path) || "/",
    side,
  });
}

/** 压缩（P-FILES）：单选/多选共用；目标默认源目录下 <名称>.tar.gz。 */
function startCompress(targets: FileEntry[], side: PaneSide) {
  if (!targets.length) return;
  const base = targets.length === 1 ? baseName(targets[0].path) || "archive" : "archive";
  openConfirm("compress", {
    title: { key: "compressTitle" },
    body: { key: "compressBody" },
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

// R5-P2-4：上传/下载泵的本地取消标志（taskId → flag）。此前取消只调
// files/transfer/cancel，前端泵无检查点——「取消」后任务继续跑完并最终置
// 已完成（假成功，文件照常落盘/流量照耗）。取消时翻转标志，泵循环在下一个
// 分片检查点终止并置 canceled 终态；等待下载帧时由 cancelTransfer 调
// releaseFrames 立即打断等待。
const pumpCancelFlags = new Map<string, { canceled: boolean }>();
/** 泵取消哨兵：upload/download 泵检查到取消标志时抛出，与真实错误区分
 * （canceled 终态不落 error 文案、不弹错误横幅、批量上传不再继续后续文件）。 */
class TransferCanceled extends Error {
  constructor() {
    super("transfer canceled");
  }
}

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
  const kind = confirmKind.value;
  confirmBusy.value = true;
  const side = confirmSide.value;
  const id = sideConnectionId(side) ?? connectionId.value;
  // 确认后的异步预检/批量请求共用该快照；宿主随后换连接也不改变已提交动作。
  const invokeConfirmed = <T = Record<string, unknown>>(method: string, params: Record<string, unknown>) =>
    call<T>(method, { ...params, connectionId: id });
  // transport=job 的 copy/move/rename/syncDir/copyDir：job 已登记、终态后
  // 由事件驱动刷新（handleEvent），跳过立即刷新。
  let jobStarted = false;
  try {
    switch (kind) {
      case "newFolder": {
        const name = confirmDraft.value.trim();
        // R3-P2-4：文件名校验（禁 /、禁 . / ..、禁空值），行内提示不关弹层。
        if (!checkConfirmName()) return;
        const fullPath = joinPath(paneDirPath(side), name);
        // P2-10：提交前重名预检，命中即提示且弹层保持打开。
        if ((await pathExists(id, fullPath)) === true) return rejectDuplicate(name);
        await invokeConfirmed("files/mkdir", { path: fullPath });
        showNotice(t("folderCreated"));
        break;
      }
      case "newFile": {
        const name = confirmDraft.value.trim();
        if (!checkConfirmName()) return;
        const fullPath = joinPath(paneDirPath(side), name);
        if ((await pathExists(id, fullPath)) === true) return rejectDuplicate(name);
        await invokeConfirmed("files/write", { path: fullPath, dataBase64: "" });
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
        if ((await pathExists(id, newPath)) === true) return rejectDuplicate(name);
        const retry = transferRequest(id, "files/rename", {
          path: entry.path,
          newPath,
        });
        const result = await call<{ transport?: string; jobId?: string | null }>(retry.method, retry.params);
        if (result.transport === "job" && result.jobId) {
          // 目录 rename 降级（P-FILES ②）：等终态再刷新。
          trackSidecarJob(result.jobId, "rename", `${entry.path} → ${newPath}`, retry);
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
          error.value = { kind: "failure", detail: "", inner: { key: "destMustDiffer" } };
          return;
        }
        // R3-P2-5：目标冲突预检——同名即静默覆盖（数据丢失风险），命中后转入
        // 覆盖确认（弹层保持打开、危险态、按钮变「覆盖」），确认后才执行。
        // 草稿在确认后又被改动则重新预检。
        if (!confirmForce.value || confirmForcePath.value !== targetPath) {
          if ((await pathExists(id, targetPath)) === true) {
            confirmForce.value = true;
            confirmForcePath.value = targetPath;
            confirmDanger.value = true;
            confirmBody.value = { key: "overwriteAsk", values: { path: targetPath } };
            return;
          }
        }
        const retry = transferRequest(id, `files/${kind}`, {
          sourcePath: entry.path,
          targetPath,
        });
        const result = await call<{ success: boolean; error?: string; transport?: string; jobId?: string | null }>(retry.method, retry.params);
        if (result.success === false) throw new Error(result.error || t("transferFailed"));
        if (result.transport === "job" && result.jobId) {
          trackSidecarJob(result.jobId, kind, `${entry.path} → ${targetPath}`, retry);
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
        // R5-P2-2：确认后立即关弹层——进度已实时落传输面板，busy 弹层的全屏
        // 遮罩此前会挡住面板取消钮，长批次期间用户只能干等（取消藏在弹层后）。
        // 弹层关闭后本函数继续执行批次；目录刷新等批次结束后由 switch 外统一收尾。
        closeConfirm();
        // P2-7：批量删除并发分批执行，进度实时落传输面板（单项目为 1 批同语义）。
        // R3-P2-9：取消真实中断剩余分批；已取消时不再补「已删除」通知。
        const batch = await runBatch("delete", paneDirPath(side), targets.length, async (index) => {
          const target = targets[index];
          if (target.kind === "directory") await invokeConfirmed("files/purge", { path: target.path });
          else await invokeConfirmed("files/delete", { path: target.path });
        });
        if (!batch.canceled) showNotice(t("deleted"));
        break;
      }
      case "purge": {
        const target = confirmTarget.value.path;
        if (!target) return;
        await invokeConfirmed("files/purge", { path: target });
        showNotice(t("deleted"));
        break;
      }
      case "check": {
        const entry = confirmTarget.value.entry;
        const targetPath = confirmDraft.value.trim();
        if (!entry || !targetPath) return;
        const retry = transferRequest(id, "files/check", {
          sourcePath: entry.path,
          targetPath,
          oneWay: false,
        });
        const result = await call<{ jobId: string }>(retry.method, retry.params);
        if (result.jobId) {
          trackSidecarJob(result.jobId, "check", `${entry.path} ⨯ ${targetPath}`, retry);
        }
        jobStarted = true;
        showNotice(t("checkStarted"));
        break;
      }
      case "cleanup": {
        await invokeConfirmed("files/cleanup", {});
        showNotice(t("cleanupDone"));
        break;
      }
      case "copyurl": {
        const entry = confirmTarget.value.entry;
        const url = confirmDraft.value.trim();
        if (!entry || !url) return;
        const result = await invokeConfirmed<{ filename: string }>("files/copyurl", {
          dirPath: entry.path,
          url,
        });
        showNotice(t("copyurlDone", { name: result.filename }));
        break;
      }
      case "extract": {
        const entry = confirmTarget.value.entry;
        const targetPath = confirmDraft.value.trim();
        if (!entry || !targetPath) return;
        // files/extract 为后端交接方法：未实现时走 featureMissing 提示，
        // 实现后（transport=job）复用 copy/move 的 job 进度语义。
        const result = await invokeConfirmed<{ success?: boolean; transport?: string; jobId?: string | null }>("files/extract", {
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
        const result = await invokeConfirmed<{ success: boolean; transport?: string; jobId?: string | null }>("files/compress", {
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
  const version = hostContextVersion;
  const sourceId = sideConnectionId(from) ?? connectionId.value;
  const targetId = sideConnectionId(to) ?? connectionId.value;
  const paths = dragged ? dragged.map((entry) => entry.path) : from === "left" ? selection.value : rightSelection.value;
  const list = dragged ?? pickSideEntries(from, paths);
  if (!list.length) return;
  const destPath = to === "left" ? path.value : rightPath.value;
  // 目标冲突预检（R3-P2-5）：一次 list 目标目录取同名集合（避免逐条 stat），
  // 命中即整批挂起等覆盖确认；目标不可列（不存在/失败）交后端兜底。
  const conflicts = await findTargetConflicts(to, destPath, list);
  if (version !== hostContextVersion || sourceId !== (sideConnectionId(from) ?? connectionId.value) || targetId !== (sideConnectionId(to) ?? connectionId.value)) return;
  if (conflicts.length) {
    pendingPaneTransfer.value = { from, move, list, destPath };
    openConfirm("overwrite", {
      title: move ? { key: "moveTitle" } : { key: "copyTitle" },
      body: { key: "overwriteBatch", values: { count: conflicts.length } },
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
  // 在提交前固定两端连接，后续切栏/宿主换连接不能重定向重试。
  const sourceConnectionId = sideConnectionId(from) ?? connectionId.value;
  const targetConnection = sideConnectionId(to) ?? connectionId.value;
  const requestConnectionId = connectionId.value;
  try {
    for (const item of list) {
      const params: Record<string, unknown> = {
        connectionId: requestConnectionId,
        sourceConnectionId,
        targetConnectionId: targetConnection,
        sourcePath: item.path,
        targetPath: joinPath(destPath, item.name),
      };
      const method = `files/${move ? "move" : "copy"}`;
      const result = await call<{ success?: boolean; error?: string; transport?: string; jobId?: string | null }>(method, params);
      if (result.success === false) throw new Error(result.error || t("transferFailed"));
      if (result.transport === "job" && result.jobId) {
        trackSidecarJob(result.jobId, move ? "move" : "copy", `${item.path} → ${String(params.targetPath)}`, { method, params });
      }
    }
    showNotice(t("paneTransferred", { count: list.length }));
    if (to === "left") await loadDirectory().catch(() => undefined);
    else await loadRightDirectory().catch(() => undefined);
    // R5-P2-1：move 的源栏也必须刷新——此前只有「右→左移动」被覆盖，左→右
    // 移动后源栏（左）没有任何刷新路径，已移走条目残留、可重复误操作。
    if (move) {
      if (from === "left") await loadDirectory().catch(() => undefined);
      else await loadRightDirectory().catch(() => undefined);
    }
  } catch (cause) {
    showError(cause);
  }
}

// ---- drag & drop（A-FILES ①）---------------------------------------------------

// 拖放动作选择（rclone-ui parity）：跨栏内部拖放不再立即复制，先弹
// 「复制（保留原件）/ 移动（传输后删除原件）」选择；确认后才走
// transferBetween（既有冲突预检/覆盖确认原样复用）。OS 文件拖入不受影响。
const dropActionOpen = ref(false);
const dropActionPending = ref<{ from: PaneSide; list: FileEntry[]; destPath: string }>();

function onDropActionChoose(action: "copy" | "move") {
  const pending = dropActionPending.value;
  dropActionOpen.value = false;
  dropActionPending.value = undefined;
  if (!pending) return;
  void transferBetween(pending.from, action === "move", pending.list);
}

function onDropActionCancel() {
  dropActionOpen.value = false;
  dropActionPending.value = undefined;
}

/**
 * 审计#16：OS 拖入的文件列表（DataTransfer.files）。优先走 items 映射——
 * 目录条目 getAsFile() 返回 null，天然剔除（目录暂不支持递归上传）；items
 * 无文件条目时（个别环境/测试桩）回退 files 列表。文件名取 file.name、
 * size 取 file.size，内容经既有上传泵（file.slice → binary 帧）分片上传。
 */
function osDroppedFiles(event: DragEvent): File[] {
  const transfer = event.dataTransfer;
  if (!transfer) return [];
  const items = transfer.items;
  let sawFileItem = false;
  const files: File[] = [];
  for (let index = 0; index < (items?.length ?? 0); index += 1) {
    const item = items?.[index];
    if (item?.kind !== "file") continue;
    sawFileItem = true;
    const file = item.getAsFile();
    if (file) files.push(file);
  }
  if (sawFileItem) return files;
  return Array.from(transfer.files ?? []);
}

function onDropTo(side: PaneSide, event: DragEvent) {
  dragOverSide.value = null;
  // R5-P2-5：拖放目标即用户当前关注侧，记账活动栏（此前拖拽是 activeSide
  // 唯一漏网入口——拖放后点工具栏「新建文件夹」会落错栏）。
  markActiveSide(side);
  const raw = event.dataTransfer?.getData("application/x-dbx-files");
  if (raw) {
    let payload: { paneId?: string; paths?: string[] };
    try {
      payload = JSON.parse(raw);
    } catch {
      return;
    }
    const from = payload.paneId === "left" ? "left" : payload.paneId === "right" ? "right" : null;
    if (!from || from === side || !payload.paths?.length) return;
    // 拖放动作选择（rclone-ui parity）：先弹「复制/移动」选择再执行。只读态
    // 与旧行为一致静默忽略（transferBetween 的 canWrite 门禁前置到这里）。
    const list = pickSideEntries(from, payload.paths);
    if (!list.length || !canWrite.value) return;
    dropActionPending.value = { from, list, destPath: side === "left" ? path.value : rightPath.value };
    dropActionOpen.value = true;
    return;
  }
  // 审计#16：OS 文件拖入此前被静默忽略。上传恒落目标侧（双栏右栏；单栏为
  // 当前连接当前目录，同 P1-5「上传=传向远端」语义），复用既有 onUpload
  // 上传泵逐文件上传；双栏时拖到源侧（左栏）给出「请拖到目标侧」提示。
  const files = osDroppedFiles(event);
  if (!files.length) return;
  if (dualPane.value && side !== "right") {
    showNotice(t("dropToTargetPane"));
    return;
  }
  if (!canWrite.value) {
    showNotice(t("readOnly"));
    return;
  }
  void onUpload(files);
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
  const target = resolveUploadTarget({
    dualPane: dualPane.value,
    leftPath: path.value,
    rightPath: rightPath.value,
    leftConnectionId: sideConnectionId("left"),
    rightConnectionId: sideConnectionId("right"),
  });
  return { ...target, connectionId: target.connectionId ?? connectionId.value };
}

async function uploadSource(name: string, size: number, readChunk: (offset: number, length: number) => Promise<Uint8Array>, target: UploadTarget) {
  // P1-5 上传方向语义：上传=传向远端（对标 FileZilla/tiny-rdm）。双栏时固定
  // 落到目标栏（右栏连接面恒为远端，不含本地 __local__）；单栏时落到当前
  // 连接的当前目录（原行为）。不再固定写左栏——双栏默认左栏是本地面板，
  // 把「上传」写进本地盘与直觉相反。
  const remotePath = joinPath(target.path, name);
  const startParams: Record<string, unknown> = { remotePath, size };
  if (target.connectionId) startParams.connectionId = target.connectionId;
  const start = await call<{ taskId: string; chunkSize?: number }>("files/upload/start", startParams);
  const taskId = start.taskId;
  const chunkSize = start.chunkSize && start.chunkSize > 0 ? start.chunkSize : CHUNK_SIZE;
  const cancelFlag = { canceled: false };
  pumpCancelFlags.set(taskId, cancelFlag);
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
      // R5-P2-4：泵级取消检查点——置 canceled 终态并终止循环，不再读/发后续分片。
      if (cancelFlag.canceled) {
        tracker.onProgress({ jobId: taskId, taskId, state: "canceled", transferred: offset });
        throw new TransferCanceled();
      }
      const chunk = await readChunk(offset, chunkSize);
      if (!chunk.byteLength) throw new Error("local file ended before its declared size");
      const payload = new Uint8Array(8 + chunk.byteLength);
      writeU64(payload, offset);
      payload.set(chunk, 8);
      await window.dbxPlugin.sendBinary(`files/upload/${taskId}`, payload);
      offset += chunk.byteLength;
      // issue#6-5：上传进度以 sidecar 收到的权威字节为准（节流事件 + 5s 轮询
      // 兜底），不再上报本地已入队的 offset——宿主 sendBinary 即发即忘，本地
      // offset 按内存速度跑到 100%（视频里 265 MiB/s 的假速度），真实网络
      // 写入远未完成，进度条严重失真。
    }
    await window.dbxPlugin.invoke("files/upload/finish", { taskId }, { timeoutMs: 30 * 60 * 1000 });
    tracker.onProgress({ jobId: taskId, taskId, state: "completed", transferred: size });
  } catch (cause) {
    // 已取消：终态已置，不再重复报错/请 sidecar 取消（cancelTransfer 已处理）。
    if (cause instanceof TransferCanceled) return;
    tracker.onProgress({ jobId: taskId, taskId, state: "failed", error: errorMessage(cause) });
    await window.dbxPlugin.invoke("files/transfer/cancel", { taskId }).catch(() => undefined);
    throw cause;
  } finally {
    pumpCancelFlags.delete(taskId);
  }
}

/** 上传后刷新目标栏并按目标路径提示（P1-5）：双栏刷新右栏，单栏刷新左栏。 */
async function afterUpload(count: number, target: UploadTarget, hadFailure = false) {
  // 单栏刷新成功时 loadDirectory 会清理 error；保存本批次最后一个上传错误，
  // 避免权限失败在目录刷新后变成“无提示”。失败批次也不显示成功 notice。
  const uploadError = hadFailure ? error.value : "";
  if (dualPane.value) await loadRightDirectory().catch(() => undefined);
  else await loadDirectory().catch(() => undefined);
  if (hadFailure && uploadError) error.value = uploadError;
  refreshAuditPanel();
  if (count && !hadFailure) showNotice(t("uploaded", { count, path: target.path }));
}

async function uploadLocalFiles(files: readonly File[], target: UploadTarget) {
  // issue#6-3：只统计真正成功的文件数。此前无条件按 files.length 提示
  // 「已上传 N 个」，目标目录不可写时逐文件报错后仍被成功提示覆盖（假成功）。
  let uploaded = 0;
  let hadFailure = false;
  for (const file of files) {
    try {
      await uploadSource(file.name, file.size, async (offset, length) =>
        new Uint8Array(await file.slice(offset, offset + length).arrayBuffer()),
        target,
      );
      uploaded += 1;
    } catch (cause) {
      // R5-P2-4：用户取消当前文件后不再继续上传剩余文件（也不补「已上传 N 个」）。
      if (cause instanceof TransferCanceled) return;
      hadFailure = true;
      showError(cause);
    }
  }
  await afterUpload(uploaded, target, hadFailure);
}

async function uploadHostFiles(files: Array<{ handleId: string; name: string; size: number }>, target: UploadTarget) {
  const fileTransfer = window.dbxPlugin.fileTransfer;
  if (!fileTransfer) return;
  // issue#6-3：同 uploadLocalFiles——成功计数替代「按总数报成功」。
  let uploaded = 0;
  let hadFailure = false;
  for (const file of files) {
    try {
      await uploadSource(file.name, file.size, async (offset, length) => {
        const result = await fileTransfer.read(file.handleId, offset, length);
        return window.dbxPlugin.decodeBase64(result.dataBase64);
      }, target);
      uploaded += 1;
    } catch (cause) {
      // R5-P2-4：同 uploadLocalFiles——取消即终止整个批量上传（handle 释放由
      // finally 统一处理）。
      if (cause instanceof TransferCanceled) return;
      hadFailure = true;
      showError(cause);
    } finally {
      await fileTransfer.cancel(file.handleId).catch(() => undefined);
    }
  }
  await afterUpload(uploaded, target, hadFailure);
}

async function onUpload(files: File[] | null) {
  if (!canWrite.value) return;
  const target = uploadTarget();
  if (files === null) {
    const fileTransfer = window.dbxPlugin.fileTransfer;
    if (!fileTransfer) return;
    try {
      const picked = await fileTransfer.pick({ multiple: true });
      await uploadHostFiles(picked.files, target);
    } catch (cause) {
      showError(cause);
    }
    return;
  }
  await uploadLocalFiles(files, target);
}

async function downloadEntry(entry: FileEntry, side: PaneSide = "left", id = sideConnectionId(side) ?? connectionId.value) {
  const fileTransfer = window.dbxPlugin.fileTransfer;
  let taskId: string | undefined;
  // R5-P2-4：泵级取消标志（cancelTransfer 置位 + releaseFrames 打断帧等待）。
  const cancelFlag = { canceled: false };
  let channel: string | undefined;
  // 桌面端优先 sidecar 本机落盘：完成的下载保留经过校验的 localPath，
  // 传输面板才能提供「定位/打开」，历史重开也能恢复。本地落盘不可用
  // （web/docker）时回退宿主 fileTransfer 保存对话框，再退浏览器下载。
  const local = await probeLocalCapabilities();
  const saveToLocal = !!local?.canSaveLocal;
  const hostTransfer = saveToLocal ? undefined : fileTransfer;
  try {
    const startParams: Record<string, unknown> = { remotePath: entry.path, connectionId: id };
    if (saveToLocal) {
      startParams.saveToLocal = true;
      const downloadDir = saveDirDraft.value.trim();
      if (downloadDir) startParams.downloadDir = downloadDir;
    }
    const info = await call<{ taskId: string; size: number; fileName?: string; chunkSize?: number }>("files/download/start", startParams);
    taskId = info.taskId;
    pumpCancelFlags.set(taskId, cancelFlag);
    channel = `files/download/${taskId}`;
    const size = info.size;
    releaseFrames(channel);
    registerJob({
      jobId: taskId,
      taskId,
      connectionId: id,
      kind: "download",
      remotePath: entry.path,
      state: "running",
      size,
      transferred: 0,
      updatedAt: Date.now(),
    });
    // 泵式下载：sidecar 在 start 后自行按 offset 顺序推送
    // files/download/{taskId} 帧（8 字节 BE offset + <=256KiB），前端只收帧。
    // saveToLocal 时字节由 sidecar 写盘（前端纯跟进度）；否则宿主 beginSave
    // 或浏览器 blob 兜底，语义与此前一致。
    const target = hostTransfer ? await hostTransfer.beginSave({ name: info.fileName ?? entry.name, size }) : undefined;
    const chunks = saveToLocal ? undefined : target ? undefined : ([] as Uint8Array[]);
    let offset = 0;
    while (offset < size) {
      // R5-P2-4：泵级取消检查点——等待中的帧由 cancelTransfer 的 releaseFrames
      // 拒绝（TransferCanceled 语义走 canceled 终态），不再写后续分块。
      if (cancelFlag.canceled) throw new TransferCanceled();
      const chunk = await waitForFrame(channel, offset);
      if (!chunk.data.byteLength) throw new Error("download frame carried no data");
      if (chunks) {
        chunks.push(chunk.data);
        offset += chunk.data.byteLength;
      } else if (target) {
        const write = await hostTransfer!.write(target.handleId, chunk.offset, chunk.data);
        offset = Math.max(offset + chunk.data.byteLength, write.nextOffset);
      } else {
        // saveToLocal：字节已在 sidecar 侧写入暂存文件，这里只跟进进度。
        offset += chunk.data.byteLength;
      }
      tracker.onProgress({ jobId: taskId, taskId, state: "running", transferred: offset });
    }
    let localPath: string | undefined;
    if (target) {
      await hostTransfer!.finish(target.handleId);
    } else if (chunks) {
      saveBrowserDownload(chunks, info.fileName ?? entry.name);
    }
    const finishResult = await call<{ localPath?: string }>("files/download/finish", { taskId });
    localPath = finishResult?.localPath;
    releaseFrames(channel);
    tracker.onProgress({ jobId: taskId, taskId, state: "completed", transferred: size, localPath });
    if (localPath) showNotice(t("downloadedTo", { name: info.fileName ?? entry.name, path: localPath }));
    else showNotice(t("downloaded", { name: info.fileName ?? entry.name }));
  } catch (cause) {
    // 下载泵失败同样落终态（此前漏标，job 会永远停在 running）；取消走
    // canceled 终态（无 error 文案、不弹横幅），失败仍落 failed。
    if (taskId) {
      const canceled = cause instanceof TransferCanceled || cancelFlag.canceled;
      tracker.onProgress({ jobId: taskId, taskId, state: canceled ? "canceled" : "failed", error: canceled ? undefined : errorMessage(cause) });
    }
    if (!(cause instanceof TransferCanceled || cancelFlag.canceled)) showError(cause);
  } finally {
    if (channel) releaseFrames(channel);
    if (taskId) pumpCancelFlags.delete(taskId);
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
  const id = sideConnectionId(side) ?? connectionId.value;
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
    await downloadEntry(entry, side, id);
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
  const batchFlag = batchCancelFlags.get(jobId);
  // R5-P2-4：上传/下载泵取消同样本地置位（泵循环在下一个分片检查点终止并
  // 落 canceled 终态）；下载泵的等待中帧由 releaseFrames 立即打断。这类 job
  // 在 sidecar 有真实记录（start 已建），额外调 cancel 释放 sidecar 侧资源。
  const pumpFlag = pumpCancelFlags.get(jobId);
  if (batchFlag) batchFlag.canceled = true;
  if (pumpFlag) {
    pumpFlag.canceled = true;
    // 真实 sidecar 上 start 已建 job，取消以释放 sidecar 侧资源（mock 无记录
    // 返回 success，同样无害）；下载泵的等待中帧由 releaseFrames 立即打断。
    await call("files/transfer/cancel", { taskId: jobId }).catch(() => undefined);
    if (frameQueue.has(`files/download/${jobId}`) || frameWaiters.has(`files/download/${jobId}`)) {
      releaseFrames(`files/download/${jobId}`);
    }
  }
  if (!batchFlag && !pumpFlag) {
    try {
      await call("files/transfer/cancel", { taskId: jobId });
      showNotice(t("jobCanceled"));
    } catch (cause) {
      showError(cause);
    }
  } else {
    showNotice(t("jobCanceled"));
  }
  void tracker.refresh(invokeAdapter);
}

/** 清理传输历史（P-FILES ⑥）：sidecar 清持久化历史 + 内存完成态 job，本地同步清。
 * 审计中#15：清空不可恢复，接入 ConfirmDialog 二次确认（transferHistoryConfirmOpen）。 */
const transferHistoryConfirmOpen = ref(false);
async function clearTransferHistory() {
  try {
    await call("files/transfers/clear", {});
    tracker.clearFinished();
    showNotice(t("historyCleared"));
  } catch (cause) {
    showError(cause);
  }
}

/** 单条删除传输记录（对标 ssh 面板 + 新增）：sidecar 三面齐删，本地同步移除。 */
async function deleteTransferRecord(jobId: string) {
  try {
    await call("files/transfers/delete", { taskId: jobId });
    tracker.remove(jobId);
    showNotice(t("recordDeleted"));
  } catch (cause) {
    showError(cause);
  }
}

// —— 下载本机落盘（对标 ssh 插件 local_downloads）———————————————

// canSaveLocal=false（web/docker / 宿主无下载目录）时回退宿主 fileTransfer
// 保存或浏览器 <a download>。结果按工作台生命周期缓存。
let localCapabilities: Promise<{ canSaveLocal: boolean; downloadsDir: string } | undefined> | undefined;
const localDownloadDir = ref("");
const canSaveLocal = ref(false);
// 宿主切到 web（canSaveLocal=false）时挂载分类/入口消失：当前分类回落到下载。
watch(canSaveLocal, (can) => {
  if (!can && settingsCategory.value === "mounts") settingsCategory.value = "downloads";
});
/** sidecar 平台标签（macos/windows/linux/other），驱动「打开方式」预设与文案。 */
const localPlatform = ref("");
function probeLocalCapabilities() {
  localCapabilities ??= window.dbxPlugin
    .invoke<{ canSaveLocal: boolean; downloadsDir: string; platform?: string }>("files/local/capabilities")
    .then((result) => {
      localDownloadDir.value = result.downloadsDir || "";
      canSaveLocal.value = !!result.canSaveLocal;
      localPlatform.value = result.platform || "";
      return result;
    })
    .catch(() => undefined);
  return localCapabilities;
}

// —— 平台「打开方式」预设（issue #11 延伸）——————————————————————
// files/local/detect-apps 只回报告本机真实存在的候选（WPS/Excel/LibreOffice/...
// 按平台默认安装路径探测）；方法缺失（旧 sidecar）时整组隐藏，手动输入仍可用。
interface AppPreset {
  id: string;
  name: string;
  path: string;
}
const appPresets = ref<AppPreset[]>([]);
async function probeAppPresets() {
  try {
    const result = await window.dbxPlugin.invoke<{ apps?: AppPreset[] }>("files/local/detect-apps");
    appPresets.value = Array.isArray(result.apps) ? result.apps : [];
  } catch {
    appPresets.value = [];
  }
}

/** 「保存到」偏好（localStorage），空串 = 跟随 sidecar 默认下载目录。 */
const saveDirDraft = ref(loadDownloadDir());
const saveDirError = ref("");
let saveDirValidationSerial = 0;
// ---- 深度搜索（files/search，远端递归；回车触发，当前目录即时过滤不受影响）----
const deepSearchOpen = ref(false);
const deepSearchSide = ref<PaneSide>("left");
const deepSearchBusy = ref(false);
const deepSearchResults = ref<Array<{ path: string; size: number; modifiedAt: string }>>([]);
const deepSearchTruncated = ref(false);
/** 键盘 ↑↓ 选中的结果行（-1 = 未选）；Enter 打开选中项或发起搜索。 */
const deepSearchIndex = ref(-1);

/** 回车触发：从该栏当前目录递归搜索文件名子串。 */
async function runDeepSearch(side: PaneSide, query: string) {
  const term = query.trim();
  if (!term) return;
  const id = sideConnectionId(side) ?? connectionId.value;
  deepSearchOpen.value = true;
  deepSearchSide.value = side;
  deepSearchBusy.value = true;
  deepSearchResults.value = [];
  deepSearchTruncated.value = false;
  try {
    const result = await call<{ entries: Array<{ path: string; size: number; modifiedAt: string }>; truncated: boolean }>(
      "files/search",
      { connectionId: id, root: paneDirPath(side), pattern: term },
    );
    deepSearchResults.value = result.entries ?? [];
    deepSearchTruncated.value = Boolean(result.truncated);
    deepSearchIndex.value = result.entries?.length ? 0 : -1;
  } catch (cause) {
    deepSearchOpen.value = false;
    showError(cause);
  } finally {
    deepSearchBusy.value = false;
  }
}

/** 搜索框 ↑↓：在结果间移动选中行（循环）；Enter 打开选中项。 */
function onSearchKeydown(side: PaneSide, key: string, query: string) {
  const count = deepSearchResults.value.length;
  if (!deepSearchOpen.value || !count) {
    if (key === "Enter") runDeepSearch(side, query);
    return;
  }
  if (key === "ArrowDown") deepSearchIndex.value = (deepSearchIndex.value + 1) % count;
  else if (key === "ArrowUp") deepSearchIndex.value = (deepSearchIndex.value - 1 + count) % count;
  else if (key === "Enter" && deepSearchIndex.value >= 0) {
    void openDeepSearchResult(deepSearchResults.value[deepSearchIndex.value]!);
  }
}

/** 点击结果：跳到其父目录（保留目标栏语义）。 */
async function openDeepSearchResult(entry: { path: string }) {
  const side = deepSearchSide.value;
  const parent = parentPath(entry.path) || "/";
  deepSearchOpen.value = false;
  if (side === "left") {
    await loadDirectory(parent).catch(() => undefined);
  } else {
    await loadRightDirectory(parent).catch(() => undefined);
  }
}

function closeDeepSearch() {
  deepSearchOpen.value = false;
}

// ---- 远端空间占用（files/about，sidecar 60s 缓存；左右栏各自显示）----------
// 每栏独立取数：单栏时左栏即当前连接（默认可见），双栏时两栏按各自连接显示。
const leftRemoteUsage = ref<{ used: number; total: number } | null>(null);
const rightRemoteUsage = ref<{ used: number; total: number } | null>(null);

async function loadRemoteUsage(side: PaneSide) {
  const id = sideConnectionId(side) ?? connectionId.value;
  const target = side === "left" ? leftRemoteUsage : rightRemoteUsage;
  if (!id || id === "__local__") {
    target.value = null;
    return;
  }
  try {
    const result = await call<{ used?: number; total?: number }>("files/about", { connectionId: id });
    target.value = result.total ? { used: result.used ?? 0, total: result.total } : null;
  } catch {
    // 后端不支持（旧 sidecar/特殊协议）时静默隐藏，不打扰用户。
    target.value = null;
  }
}

// 连接或栏内目录变化时刷新占用（about 有 60s 缓存，频率无虞）；挂载即拉一次。
// 右栏仅双栏模式可见，单栏时不必取数（about 按连接缓存，双栏开启首刷即到）。
watch([connectionId, dualPane, leftConnectionId, path], () => { void loadRemoteUsage("left"); }, { immediate: true });
watch([connectionId, dualPane, targetConnectionId, rightPath], () => { if (dualPane.value) void loadRemoteUsage("right"); else rightRemoteUsage.value = null; }, { immediate: true });

/** 统计页签展示活动栏连接的空间占用。 */
const activeRemoteUsage = computed(() => (activeSide.value === "right" ? rightRemoteUsage.value : leftRemoteUsage.value));

// ---- 传输带宽（files/bwlimit：sidecar prefs 持久化，每个 rcd 启动时重放）----
const bwlimitDraft = ref("");
const bwlimitError = ref("");
/** 当前生效限速（非空=顶栏徽标可见）；设置保存后即时更新。 */
const bwlimitActive = ref<string | null>(null);

/** 统一保存：把当前区块草稿交给面板暴露的 save()（内部走既有校验/持久化链路，
 * 行内错误就地展示；成功通知由各链路自己发）。 */
async function onSettingsSave() {
  if (!settingsDirty.value || settingsSaving.value) return;
  settingsSaving.value = true;
  try {
    await settingsPanelRef.value?.save();
  } finally {
    settingsSaving.value = false;
  }
}

/** 设置面板打开传输页签时拉取当前持久化限速（空 = 不限）。 */
async function loadBwlimit() {
  try {
    const result = await call<{ rate: string | null }>("files/bwlimit", {});
    bwlimitDraft.value = result.rate ?? "";
    bwlimitActive.value = result.rate ?? null;
    bwlimitError.value = "";
  } catch {
    bwlimitError.value = t("bwlimitLoadFailed");
  }
}

/** 保存限速：空 = 取消（off）。非法值由 rclone 拒绝，行内提示不关闭设置。 */
async function onBwlimitSave(rate: string) {
  const normalized = rate.trim();
  try {
    const result = await call<{ rate: string | null }>(
      "files/bwlimit",
      normalized ? { rate: normalized } : { rate: "off" },
    );
    bwlimitDraft.value = result.rate ?? "";
    bwlimitActive.value = result.rate ?? null;
    bwlimitError.value = "";
    showNotice(t("settingsSaved"));
  } catch {
    hideNotice();
    bwlimitError.value = t("bwlimitInvalid");
  }
}

async function onSaveDirChange(dir: string) {
  const normalized = dir.trim();
  const serial = ++saveDirValidationSerial;
  if (!normalized) {
    saveDirError.value = "";
    persistDownloadDir("");
    saveDirDraft.value = "";
    return;
  }
  try {
    await window.dbxPlugin.invoke("files/local/validate-directory", { path: normalized });
    if (serial !== saveDirValidationSerial) return;
    saveDirError.value = "";
    persistDownloadDir(normalized);
    saveDirDraft.value = loadDownloadDir();
    showNotice(t("settingsSaved"));
  } catch {
    if (serial !== saveDirValidationSerial) return;
    saveDirError.value = t("invalidDownloadDirectory");
  }
}

// 在文件管理器中定位本机落盘的下载（sidecar 校验过该路径确为本插件记录）。
async function revealTransferTarget(path: string) {
  try {
    await call("files/local/reveal", { path });
  } catch (cause) {
    showError(cause);
  }
}

// 在系统默认应用中打开已完成的下载；sidecar 会校验路径必须来自本插件的
// 完成历史，避免把这个按钮变成任意本机路径打开入口。
async function openTransferTarget(path: string) {
  try {
    await call("files/local/open", { path });
  } catch (cause) {
    showError(cause);
  }
}

// —— 外部打开应用偏好（issue #11）—————————————————————————————
// 设置面板改动先经 sidecar 校验（files/local/validate-open-app：存在的绝对
// 路径可执行文件）再持久化；打开时按扩展名映射或全局默认解析出 app 下发。
const openAppPrefs = ref<OpenAppPrefs>(loadOpenAppPrefs());
const openAppError = ref("");
let openAppValidationSerial = 0;
async function onOpenAppPrefsChange(prefs: OpenAppPrefs) {
  const serial = ++openAppValidationSerial;
  // 与 prefs.ts 同一清洗规则；半成品映射行（缺扩展名或缺应用）不参与校验。
  const normalized: OpenAppPrefs = {
    defaultApp: prefs.defaultApp.trim(),
    mappings: prefs.mappings
      .map((mapping) => ({
        ext: mapping.ext.trim().replace(/^\.+/, "").toLowerCase(),
        app: mapping.app.trim(),
      }))
      .filter((mapping) => mapping.ext && mapping.app),
  };
  if (!normalized.defaultApp && !normalized.mappings.length) {
    openAppError.value = "";
    openAppPrefs.value = normalized;
    persistOpenAppPrefs(normalized);
    showNotice(t("settingsSaved"));
    return;
  }
  try {
    const apps = [...new Set([normalized.defaultApp, ...normalized.mappings.map((mapping) => mapping.app)])];
    for (const app of apps) {
      await window.dbxPlugin.invoke("files/local/validate-open-app", { path: app });
    }
    if (serial !== openAppValidationSerial) return;
    openAppError.value = "";
    openAppPrefs.value = normalized;
    persistOpenAppPrefs(normalized);
    showNotice(t("settingsSaved"));
  } catch {
    if (serial !== openAppValidationSerial) return;
    openAppError.value = t("invalidExternalApp");
  }
}

// 用用户配置的外部应用打开已完成的下载：按扩展名映射或全局默认解析出 app；
// sidecar 仍按完成历史白名单二次校验。未配置时提示去设置页，不静默降级成
// 系统默认应用（那会让这个入口失去意义）。
// ---- 打开方式（远程编辑本地副本，FinalShell 式）-----------------------------
// 右键「打开方式…」：选系统默认 / 预设 / 手输应用后，sidecar 把文件拉到本机
// 临时副本并启动应用；本地保存由 sidecar 监视循环自动回传远端原路径。
// 目标连接在打开时固化——双栏下用右键所在栏的连接，而不是活动连接。

const openWithState = ref<{ entry: FileEntry; connectionId: string }>();

function openOpenWithDialog(entry: FileEntry, side: PaneSide) {
  openWithState.value = { entry, connectionId: sideConnectionId(side) ?? connectionId.value };
}

function closeOpenWithDialog() {
  openWithState.value = undefined;
}

/** 对话框确认：app 空串 = 系统默认应用。open RPC 立即返回会话，拉取/启动/
 * 回传进度经 files/remote-edit/state 事件回报（见 handleEvent）。 */
async function onOpenWithConfirm(app: string) {
  const state = openWithState.value;
  if (!state) return;
  closeOpenWithDialog();
  showNotice(t("openWithOpening"));
  try {
    await call("files/remote-edit/open", {
      connectionId: state.connectionId,
      remotePath: state.entry.path,
      ...(app ? { app } : {}),
    });
  } catch (cause) {
    showError(cause);
  }
}

async function openTransferWithApp(path: string) {
  const app = resolveOpenApp(openAppPrefs.value, path);
  if (!app) {
    showNotice(t("openAppNotConfigured"));
    return;
  }
  try {
    await call("files/local/open", { path, app });
  } catch (cause) {
    showError(cause);
  }
}

function invokeAdapter<T>(method: string, params?: unknown) {
  return window.dbxPlugin.invoke<T>(method, params);
}

// ---- 目录打包下载（files/archiveDownload，parity-tools）----------------------
// 契约：参数 { connectionId, path }（远端目录）→ 返回 download/start 同形任务；
// 进度经既有 files/transfer/progress 事件（kind=archiveDownload）汇入传输面板，
// 终态 completed 的 localPath 让 reveal/open/open-with 按既有记录 affordance 生效。
async function archiveDownloadDirs(targets: FileEntry[], side: PaneSide) {
  if (!targets.length) return;
  const id = sideConnectionId(side) ?? connectionId.value;
  let started = 0;
  for (const target of targets) {
    try {
      const result = await call<{ taskId: string }>("files/archiveDownload", {
        connectionId: id,
        path: target.path,
      });
      if (!result.taskId) throw new Error("archiveDownload returned no taskId");
      registerJob({
        jobId: result.taskId,
        taskId: result.taskId,
        connectionId: id,
        kind: "archiveDownload",
        remotePath: target.path,
        state: "queued",
        size: 0,
        transferred: 0,
        updatedAt: Date.now(),
      });
      started += 1;
    } catch (cause) {
      // 首个失败即停（旧 sidecar 无此方法时避免整批报错刷屏）。
      showError(cause);
      return;
    }
  }
  if (started) showNotice(t("archiveDownloadStarted", { name: baseName(targets[0].path) }));
}

// ---- 批量重命名抽屉（parity-tools，对标 rclone-ui）----------------------------
// 入口：多选右键 / FileTable 多选时的底部按钮（键盘可达）。计划计算与逐行
// files/rename 在抽屉组件内完成；这里承接关闭、汇总通知与目录刷新。
const batchRenameOpen = ref(false);
const batchRenameSide = ref<PaneSide>("left");
const batchRenameEntries = ref<FileEntry[]>([]);
/** 目录内全体条目名（抽屉的「目录已存在」行级预检集）。 */
const batchRenameSiblingNames = computed(() =>
  (batchRenameSide.value === "left" ? sortedEntries.value : rightSorted.value).map((entry) => entry.name),
);

function openBatchRename(side: PaneSide) {
  markActiveSide(side);
  batchRenameSide.value = side;
  // 计划只覆盖多选集；「目录已存在」预检用当前目录完整列表（siblingNames）。
  batchRenameEntries.value = pickSideEntries(side, side === "left" ? selection.value : rightSelection.value);
  batchRenameOpen.value = true;
}

function closeBatchRename() {
  batchRenameOpen.value = false;
}

async function onBatchRenameApplied(result: { ok: number; total: number }) {
  refreshAuditPanel();
  showNotice(t("batchRenameApplied", { ok: result.ok, total: result.total }));
  if (batchRenameSide.value === "left") await loadDirectory().catch(() => undefined);
  else if (dualPane.value) await loadRightDirectory().catch(() => undefined);
}

// ---- 快捷键速查弹层（parity-tools）：`?` 触发 / 空白区右键入口 / Esc 关闭 ----
const shortcutsOpen = ref(false);

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
    case "openWith":
      openOpenWithDialog(entry, side);
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
    case "hashsum":
      void runHashsum(entry, side);
      break;
    case "rmdirs":
      void runRmdirs(entry, side);
      break;
    case "bisyncDir":
      startDirJob("bisync", entry, side);
      break;
    case "copyurl":
      openConfirm("copyurl", {
        title: { key: "copyurlTitle", values: { path: baseName(entry.path) || entry.path } },
        body: { key: "copyurlBody" },
        target: { entry },
        side,
      });
      break;
    case "checkDir":
      openConfirm("check", {
        title: { key: "checkTitle", values: { path: baseName(entry.path) || entry.path } },
        body: { key: "checkBody" },
        target: { entry },
        // 缺省与源同级的父目录（整个目录树都可作为比对目标）。
        draft: parentPath(entry.path) || "/",
        side,
      });
      break;
    case "verifySum":
      void runVerifySum(entry, side);
      break;
    case "computeSize":
      void computeEntrySize(entry, side);
      break;
    case "mountLocal":
      openMountDialog(entry.path, sideConnectionId(side) ?? connectionId.value);
      break;
    case "serveHttp":
    case "serveWebdav":
      void startServe(entry, side, action === "serveHttp" ? "http" : "webdav");
      break;
    case "copyPublicLink":
      void copyPublicLink(entry, side);
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
    case "archiveDownload":
      void archiveDownloadDirs([entry], side);
      break;
    // ---- 批量（多选右键）------------------------------------------------
    case "downloadSelected":
      void (async () => {
        const id = sideConnectionId(side) ?? connectionId.value;
        const files = pickSideEntries(side, menuSelection).filter((item) => item.kind === "file");
        if (!files.length) {
          showNotice(t("downloadNoneSelected"));
          return;
        }
        for (const item of files) await downloadEntry(item, side, id);
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
    case "archiveDownloadSelected":
      void archiveDownloadDirs(pickSideEntries(side, menuSelection).filter((item) => item.kind === "directory"), side);
      break;
    case "batchRenameSelected":
      openBatchRename(side);
      break;
  }
}

/** 对标 rclone-dashboard 目录体积卡：files/size 汇总后以顶部提示汇报。 */
async function computeEntrySize(entry: FileEntry, side: PaneSide) {
  showNotice(t("computingSize"));
  try {
    const result = await call<{ count: number; bytes: number }>("files/size", {
      path: entry.path,
      connectionId: sideConnectionId(side) ?? connectionId.value,
    });
    showNotice(t("sizeResult", { count: result.count, size: formatBytes(result.bytes) }));
  } catch (cause) {
    showNotice(t("operationFailed", { error: errorMessage(cause) }));
  }
}

/** 生成 SUM 校验文件（files/hashsum）：写入目录旁 `<名称>.<hash>`，同级可见。 */
async function runHashsum(entry: FileEntry, side: PaneSide) {
  const id = sideConnectionId(side) ?? connectionId.value;
  try {
    const result = await call<{ path: string; files: number }>("files/hashsum", {
      connectionId: id,
      path: entry.path,
      hashType: "md5",
    });
    showNotice(t("hashsumDone", { path: result.path, files: result.files }));
  } catch (cause) {
    showNotice(t("operationFailed", { error: errorMessage(cause) }));
  }
}

// SUM 校验文件（批次7）：右键 → 「校验所在目录」。哈希类型缺省由后端按
// 扩展名推断，前端不做二次猜测。
const SUM_EXTENSIONS = new Set(["md5", "sha1", "sha256", "sha512", "crc32"]);
function isSumFile(entry: FileEntry): boolean {
  if (entry.kind !== "file") return false;
  const extension = entry.path.split(".").pop()?.toLowerCase() ?? "";
  return SUM_EXTENSIONS.has(extension);
}

/** 校验 SUM 文件所在目录（files/checksum/verify）：复用 check 作业与 checkSummary 展示。 */
async function runVerifySum(entry: FileEntry, side: PaneSide) {
  const id = sideConnectionId(side) ?? connectionId.value;
  try {
    const result = await call<{ jobId: string }>("files/checksum/verify", {
      connectionId: id,
      sumPath: entry.path,
    });
    if (result.jobId) {
      trackSidecarJob(result.jobId, "check", `⨯ ${entry.path}`);
    }
    showNotice(t("verifySumStarted"));
  } catch (cause) {
    showNotice(t("operationFailed", { error: errorMessage(cause) }));
  }
}

/** 清理空目录（files/rmdirs）：递归删除 path 下的空目录。 */
async function runRmdirs(entry: FileEntry, side: PaneSide) {
  const id = sideConnectionId(side) ?? connectionId.value;
  try {
    await call("files/rmdirs", { connectionId: id, path: entry.path });
    showNotice(t("rmdirsDone"));
    await loadDirectory().catch(() => undefined);
    if (side === "right" && dualPane.value) await loadRightDirectory().catch(() => undefined);
  } catch (cause) {
    showNotice(t("operationFailed", { error: errorMessage(cause) }));
  }
}

/** 公开链接：presign 能力门控（菜单项仅在 capabilities.presign 时出现）。 */
async function copyPublicLink(entry: FileEntry, side: PaneSide) {
  try {
    const result = await call<{ url: string }>("files/publicLink", {
      path: entry.path,
      connectionId: sideConnectionId(side) ?? connectionId.value,
    });
    await window.dbxPlugin.clipboard?.writeText(result.url);
    showNotice(t("copiedPublicLink"));
  } catch (cause) {
    showNotice(t("operationFailed", { error: errorMessage(cause) }));
  }
}

/** 本机共享（对标 rclone serve）：回环 HTTP/WebDAV 暴露远端目录；URL 复制
 * 到剪贴板（与 copyPublicLink 同法），剪贴板失败不吞成功提示。 */
async function startServe(entry: FileEntry, side: PaneSide, serveType: "http" | "webdav") {
  const id = sideConnectionId(side) ?? connectionId.value;
  try {
    const result = await call<{ serveId: string; url: string; serveType: string }>("files/serve/start", {
      connectionId: id,
      path: entry.path,
      serveType,
    });
    await window.dbxPlugin.clipboard?.writeText(result.url).catch(() => undefined);
    showNotice(t("shareStarted", { url: result.url }));
  } catch (cause) {
    showNotice(t("operationFailed", { error: errorMessage(cause) }));
  }
}

/** 空白区右键：弹插件菜单前先关掉其它菜单（三菜单互斥）。 */
function openBlankMenu(side: PaneSide, payload: { x: number; y: number }) {
  captureMenuOrigin();
  contextMenu.value = undefined;
  sideMenu.value = undefined;
  blankMenu.value = { ...payload, side };
}

function blankMenuAction(action: "newFolder" | "newFile" | "refresh" | "cleanup" | "shortcuts") {
  const menu = blankMenu.value;
  blankMenu.value = undefined;
  if (!menu) return;
  if (action === "shortcuts") {
    shortcutsOpen.value = true;
    return;
  }
  if (action === "refresh") {
    if (menu.side === "left") void refreshDirectory();
    else void refreshRightDirectory();
    return;
  }
  if (action === "cleanup") {
    openConfirm("cleanup", {
      title: { key: "cleanupTitle" },
      body: { key: "cleanupBody" },
      danger: true,
      target: {},
      side: menu.side,
    });
    return;
  }
  if (action === "newFolder") startNewFolder(menu.side);
  else startNewFile(menu.side);
}

/** 侧栏（目录树/快捷目录）行右键：打开 / 在另一栏打开 / 复制路径、文件名。 */
function openSideMenu(side: PaneSide, payload: { path: string; name: string; x: number; y: number }) {
  captureMenuOrigin();
  contextMenu.value = undefined;
  blankMenu.value = undefined;
  sideMenu.value = { ...payload, side };
}

function sideMenuAction(action: "open" | "openOther" | "toggleFav" | "copyPath" | "copyName" | "mountLocal") {
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
  if (action === "toggleFav") {
    // 收藏/取消收藏该行目录（fav tab 行右键即「从收藏移除」）。
    toggleFavorite(side, target);
    return;
  }
  if (action === "mountLocal") {
    openMountDialog(target, sideConnectionId(side) ?? connectionId.value);
    return;
  }
  const value = action === "copyPath" ? target : name;
  void window.dbxPlugin.clipboard?.writeText(value).then(() => showNotice(t(action === "copyPath" ? "copiedPath" : "copiedName")));
}

// ---- 本地挂载（docs/MOUNT.zh-CN.md M1）：策略由 sidecar 决定 ---------------------
// auto：rclone mount 优先；缺 FUSE 驱动时兜底 WebDAV 网关（URL 直接进剪贴板，
// 交给系统「连接服务器」完成挂载）。M1 全程只读。

interface MountResult {
  mountId: string;
  strategy: "rclone" | "webdav";
  mountPoint?: string;
  /** webdav 策略：系统 WebDAV 客户端是否已把网关挂载成功（用户位置或卷）。 */
  mounted?: boolean;
  /** 用户选的挂载点不可用时退成了 /Volumes 网络卷（macOS mount volume）。 */
  volumeFallback?: boolean;
  gatewayUrl?: string;
  fallbackReason?: string;
}

// ---- 挂载到本机（独立顶层弹窗 MountDialog）-------------------------------------
// 工具栏/右键/侧栏/设置面板共用：先选本机目录再挂载。从设置弹窗发起时先收起
// 设置弹窗，保证挂载弹窗永远可见（此前复用 ConfirmDialog 被 z-index 遮挡）。
// 目标连接在打开时固化——双栏下设置面板入口挂主连接，而不是活动栏（可能为本地）。

const mountDialogOpen = ref(false);
const mountDialogRemotePath = ref("");
const mountTargetConnectionId = ref("");

function openMountDialog(remotePath = "", connectionIdOverride?: string) {
  mountDialogRemotePath.value = remotePath;
  mountTargetConnectionId.value = connectionIdOverride ?? connectionId.value;
  // 挂载弹窗置顶展示：设置弹窗让位（遮罩叠遮罩既挡视线也挡交互）。
  settingsOpen.value = false;
  mountDialogOpen.value = true;
}

function closeMountDialog() {
  mountDialogOpen.value = false;
}

/** MountDialog 确认：位置空 = sidecar 默认；显式位置由后端
 * ensure_empty_mount_dir 校验（不存在自动建，非空报错）。 */
async function onMountDialogConfirm(mountPoint: string) {
  mountDialogOpen.value = false;
  try {
    const result = await call<MountResult>("files/mount", {
      connectionId: mountTargetConnectionId.value || connectionId.value,
      strategy: "auto",
      ...(mountDialogRemotePath.value ? { path: mountDialogRemotePath.value } : {}),
      ...(mountPoint ? { mountPoint } : {}),
    });
    await handleMountResult(result);
  } catch (cause) {
    showError(cause);
  }
}

async function handleMountResult(result: MountResult) {
  // webdav 自动挂载成功：优先挂到用户选的位置（mount_webdav），选点失败
  // 退成 /Volumes 网络卷（macOS mount volume）。两种形态都直接 reveal。
  if (result.strategy === "webdav" && result.mounted && result.mountPoint) {
    showNotice(
      result.volumeFallback
        ? t("mountVolumeFallback", { point: result.mountPoint })
        : t("mountWebdavOk", { point: result.mountPoint }),
    );
    await call("files/local/reveal", { path: result.mountPoint }).catch(() => undefined);
    if (settingsOpen.value && settingsCategory.value === "mounts") void loadMounts();
    return;
  }
  if (result.strategy === "webdav" && result.gatewayUrl) {
    await window.dbxPlugin.clipboard?.writeText(result.gatewayUrl);
    showNotice(t("mountGatewayFallback", { reason: result.fallbackReason ?? "" }));
    return;
  }
  showNotice(t("mountRcloneOk", { point: result.mountPoint ?? "" }));
  // 「直接挂载上去」：挂完立即在文件管理器里打开挂载点（后端 reveal 只
  // 放行活跃挂载点，不是任意路径入口）；reveal 失败不影响挂载结果。
  if (result.mountPoint) await call("files/local/reveal", { path: result.mountPoint }).catch(() => undefined);
  if (settingsOpen.value && settingsCategory.value === "mounts") void loadMounts();
}

// ---- 设置弹窗「本地挂载」面板：files/mountStatus 列表 + 逐条卸载 ----------------
// 工具栏/右键挂载只在完成时给 notice；这里提供常驻视图（策略/挂载点/失效态），
// status 按当前连接过滤（call 注入 connectionId），换连接时弹窗整体关闭。

interface MountRow {
  mountId: string;
  strategy: string;
  mountPoint?: string;
  gatewayPort?: number;
  /** rclone 策略行：rcd 已不再报告该挂载点时为 false（用户侧自行卸载清理）。 */
  mounted?: boolean;
  fallbackReason?: string;
}

const mountsLoading = ref(false);
const mountsError = ref(false);
const mountsList = ref<MountRow[]>([]);
const unmountBusyId = ref("");
const refreshBusyId = ref("");
// vfs/stats 摘要（批次5）：mountId → 关键字段。diskCache 只有 VFS 缓存
// 打开时才有，字段一律容错缺失。
interface MountVfsStats {
  diskCache?: { bytesUsed?: number };
  metadataCache?: { dirs?: number; files?: number };
}
const mountStats = ref<Record<string, MountVfsStats>>({});

async function loadMounts() {
  if (mountsLoading.value) return;
  mountsLoading.value = true;
  mountsError.value = false;
  try {
    const result = await call<{ mounts: MountRow[] }>("files/mountStatus", {});
    mountsList.value = Array.isArray(result.mounts) ? result.mounts : [];
  } catch {
    mountsError.value = true;
  } finally {
    mountsLoading.value = false;
  }
  void loadMountStats();
}

/** vfs/stats 摘要（best-effort）：失败只清空摘要，不影响挂载列表本身。 */
async function loadMountStats() {
  try {
    const result = await call<{ mounts: Array<{ mountId: string; stats?: MountVfsStats }> }>("files/mount/stats", {});
    const next: Record<string, MountVfsStats> = {};
    for (const row of result.mounts ?? []) {
      if (row.stats) next[row.mountId] = row.stats;
    }
    mountStats.value = next;
  } catch {
    mountStats.value = {};
  }
}

/** rclone 行展示缓存占用 + 目录/条目数（字段缺失就跳过该段）。 */
function mountStatsText(mountId: string) {
  const stats = mountStats.value[mountId];
  if (!stats) return "";
  const parts: string[] = [];
  const bytes = stats.diskCache?.bytesUsed;
  if (typeof bytes === "number") parts.push(t("mountStats.cacheBytes", { bytes: formatBytes(bytes) }));
  const dirs = stats.metadataCache?.dirs;
  if (typeof dirs === "number") parts.push(t("mountStats.dirs", { count: dirs }));
  const files = stats.metadataCache?.files;
  if (typeof files === "number") parts.push(t("mountStats.files", { count: files }));
  return parts.join(" · ");
}

async function refreshMountCache(row: MountRow) {
  if (refreshBusyId.value) return;
  refreshBusyId.value = row.mountId;
  try {
    const result = await call<{ refreshed: number; skipped: number }>("files/mount/refresh", { mountId: row.mountId });
    if (result.refreshed > 0) {
      showNotice(t("mountRefresh.done", { refreshed: result.refreshed, skipped: result.skipped ?? 0 }));
    } else {
      showNotice(t("mountRefresh.skipped"));
    }
    await loadMounts();
  } catch (cause) {
    showError(cause);
  } finally {
    refreshBusyId.value = "";
  }
}

async function unmountMount(row: MountRow) {
  if (unmountBusyId.value) return;
  unmountBusyId.value = row.mountId;
  try {
    await call("files/unmount", { mountId: row.mountId });
    showNotice(t("mounts.unmounted"));
    await loadMounts();
  } catch (cause) {
    showError(cause);
  } finally {
    unmountBusyId.value = "";
  }
}

function mountStrategyLabel(strategy: string) {
  return strategy === "rclone" ? t("mountStrategy.rclone") : t("mountStrategy.webdav");
}

// ---- 设置弹窗「本机共享」区块（对标 rclone serve 家族）：files/serve/list
// 列表 + 逐条停止。与 loadMounts 同触发点（打开设置/切到本地挂载分类时），
// sidecar 按 connectionId 过滤（call 注入）。

interface ShareRow {
  serveId: string;
  url: string;
  serveType: string;
}

const sharesLoading = ref(false);
const sharesError = ref("");
const sharesList = ref<ShareRow[]>([]);
const shareBusyId = ref("");

async function loadShares() {
  if (sharesLoading.value) return;
  sharesLoading.value = true;
  sharesError.value = "";
  try {
    const result = await call<{ serves: ShareRow[] }>("files/serve/list", {});
    sharesList.value = Array.isArray(result.serves) ? result.serves : [];
  } catch (cause) {
    sharesError.value = errorMessage(cause);
  } finally {
    sharesLoading.value = false;
  }
}

async function stopShare(row: ShareRow) {
  if (shareBusyId.value) return;
  shareBusyId.value = row.serveId;
  try {
    await call("files/serve/stop", { serveId: row.serveId });
    showNotice(t("shareStopDone"));
    await loadShares();
  } catch (cause) {
    showError(cause);
  } finally {
    shareBusyId.value = "";
  }
}

// ---- lifecycle -----------------------------------------------------------------

/** SDK context 消费：重新绑定默认连接，只丢弃依赖该连接的栏位缓存。 */
function updateHostContext(context: Record<string, unknown>) {
  const previous = connectionId.value;
  hostContext.value = context;
  if (!initialized.value || previous === connectionId.value) return;
  hostContextVersion += 1;
  bindApi((method, params, options) => window.dbxPlugin.invoke(method, params, options), connectionId.value || null);
  capabilities.value = undefined;
  connState.value = "connecting";
  error.value = "";
  notice.value = "";
  onContextClick();
  closeConfirm();
  previewPath.value = null;
  // 挂载状态面板按连接过滤：换连接时关闭设置弹窗，避免展示旧连接的挂载行。
  settingsOpen.value = false;
  if (!sideConnectionId("left")) {
    leftNav.next();
    path.value = "/";
    entries.value = [];
    selection.value = [];
    activePath.value = "";
    searchQuery.value = "";
    leftQuickPaths.value = [];
    leftTree.value = createTreeRoot("/", "/");
    void loadDirectory("/").catch(() => undefined);
    void loadQuickPaths("left");
  }
  if (!sideConnectionId("right")) {
    rightNav.next();
    rightPath.value = "/";
    rightEntries.value = [];
    rightSelection.value = [];
    rightActivePath.value = "";
    rightSearchQuery.value = "";
    rightQuickPaths.value = [];
    rightTree.value = createTreeRoot("/", "/");
    if (dualPane.value) {
      void loadRightDirectory("/").catch(() => undefined);
      void loadQuickPaths("right");
    }
  }
  void loadCapabilities();
  if (leftSideTab.value === "tree") {
    const tree = leftTree.value;
    if (!tree.loaded && !tree.loading) void expandTreeNode("left", tree);
  }
  if (rightSideTab.value === "tree") {
    const tree = rightTree.value;
    if (!tree.loaded && !tree.loading) void expandTreeNode("right", tree);
  }
  void probeConnections();
  refreshAuditPanel();
}

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
  // ready 保存 init 时的快照；request 期间 context 可能已经更新。
  hostContext.value = api.context ?? hostContext.value;
  const applyHostEnvironment = () => {
    locale.value = api.locale || "zh-CN";
    if (api.appearance) applyAppearance(document.documentElement, api.appearance);
    else if (isDbxPluginTheme(api.theme)) applyAppearance(document.documentElement, themeToAppearance(api.theme));
  };
  applyHostEnvironment();
  // appearance 契约缺失（当前 1.1 桥只推 theme）时订阅 env 主题推送，两套不同时挂。
  if (!api.onAppearanceChange) unsubscribeTheme = onHostThemeChange((theme) => applyAppearance(document.documentElement, themeToAppearance(theme)));
  unsubscribeContext = api.onContext?.(updateHostContext);
  // host.getContext 可能先于 ready 完成；迟到的 init 仍需接住 locale/theme/context。
  unsubscribeInit = api.onInit?.((context) => {
    updateHostContext(context);
    applyHostEnvironment();
  });
  unsubscribeEvent = api.onEvent(handleEvent);
  unsubscribeBinary = api.onBinary(handleBinary);
  bindApi((method, params, options) => api.invoke(method, params, options), connectionId.value || null);
  initialized.value = true;
  const version = hostContextVersion;
  await loadCapabilities();
  if (version === hostContextVersion) {
    await loadDirectory("/").catch(() => undefined);
    if (version === hostContextVersion) {
      if (dualPane.value) await loadRightDirectory("/").catch(() => undefined);
      await loadQuickPaths("left");
      if (leftSideTab.value === "tree") {
        const tree = leftTree.value;
        if (!tree.loaded && !tree.loading) void expandTreeNode("left", tree);
      }
      if (dualPane.value) {
        if (rightSideTab.value === "tree") {
          const tree = rightTree.value;
          if (!tree.loaded && !tree.loading) void expandTreeNode("right", tree);
        }

        await enterLocalPaneIfAtRoot();
        void loadQuickPaths("right");
      }
    }
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
  // 快捷键速查：仅无输入焦点时响应 `?`（输入框/文本域/可编辑区不拦截）。
  if (event.key === "?" && !event.metaKey && !event.ctrlKey && !event.altKey) {
    const target = event.target as HTMLElement | null;
    const tag = target?.tagName;
    if (tag !== "INPUT" && tag !== "TEXTAREA" && !target?.isContentEditable) {
      event.preventDefault();
      shortcutsOpen.value = true;
      return;
    }
  }
  if (event.key !== "Escape") return;
  // 审计#7：脏草稿确认弹层先收（保留预览与草稿），再按一次才触发关闭确认。
  if (previewDiscardOpen.value) {
    previewDiscardOpen.value = false;
    return;
  }
  if (shortcutsOpen.value) {
    shortcutsOpen.value = false;
    return;
  }
  if (batchRenameOpen.value) {
    closeBatchRename();
    return;
  }
  // 拖放动作选择（焦点在弹层内时组件自身 Esc 已取消；这里兜底焦点在外的情况）。
  if (dropActionOpen.value) {
    onDropActionCancel();
    return;
  }
  if (previewPath.value) {
    closePreview();
    return;
  }
  if (confirmOpen.value) {
    closeConfirm();
    return;
  }
  if (transferHistoryConfirmOpen.value) {
    transferHistoryConfirmOpen.value = false;
    return;
  }
  if (mountDialogOpen.value) {
    closeMountDialog();
    return;
  }
  if (settingsOpen.value) {
    closeSettings();
    return;
  }
  closeMenusRestoreFocus();
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

watch([dockOpen, dockTab], ([, tab]) => {
  if (tab === "audit") auditRef.value?.refresh();
});
// 设置弹窗关闭即清掉偏好编辑期的行内错误（下次打开重新校验）。
watch(settingsOpen, (open) => {
  if (!open) {
    saveDirError.value = "";
    openAppError.value = "";
  }
});

onMounted(() => {
  document.addEventListener("click", onContextClick);
  // 顶栏限速徽标：启动即拉取当前持久化限速（空 = 不限，徽标隐藏）。
  void loadBwlimit();
  document.addEventListener("keydown", onDocumentKeydown);
  window.addEventListener("resize", syncViewportLayout);
  syncViewportLayout();
  // 本机落盘能力探测（决定下载走 sidecar 落盘还是宿主/浏览器兜底）+ 平台
  // 打开方式预设探测（旧 sidecar 方法缺失时隐藏预设区）。
  void probeLocalCapabilities();
  void probeAppPresets();
  void initialize().catch((cause) => {
    loading.value = false;
    listingFailed.value = true;
    error.value = { kind: "failure", detail: errorMessage(cause), inner: errorMessage(cause) };
  });
});

onBeforeUnmount(() => {
  document.removeEventListener("click", onContextClick);
  document.removeEventListener("keydown", onDocumentKeydown);
  window.removeEventListener("resize", syncViewportLayout);
  window.clearTimeout(noticeTimer);
  window.clearTimeout(errorTimer);
  window.clearInterval(pollTimer);
  uiIntent.stop();
  unsubscribeEvent?.();
  unsubscribeBinary?.();
  unsubscribeContext?.();
  unsubscribeInit?.();
  unsubscribeTheme?.();
});
</script>

<template>
  <main class="wb-workbench">
    <div v-if="error" class="wb-error-banner" role="alert">
      <span :title="errorDetail">{{ errorText }}</span>
      <!-- R3-P2-10：文本字符 ↻/✕ 换 lucide 图标（对齐 P2-14 先例）。 -->
      <button class="wb-icon-button wb-icon-neutral" v-tip="t('retry')" @click="retryAfterError"><RefreshCw /></button>
      <button class="wb-icon-button wb-icon-neutral" v-tip="t('close')" @click="error = ''"><X /></button>
    </div>
    <div v-if="notice" class="wb-notice" role="status">{{ noticeText }}</div>

    <!-- 审计#17：工具栏为单一 dock 开关（不带 tab = 切换当前页签开/关）；
         带 tab 的旧语义保留，供既有调用方/MCP intent 兼容。 -->
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
      :show-mount="canUseMount"
      :can-mount="canMountToolbar"
      :bwlimit="bwlimitActive"
      :starred="toolbarStarred"
      :t="t"
      @new-folder="startNewFolder(toolbarTarget.side)"
      @upload="onUpload"
      @download="downloadSelection(toolbarTarget.side)"
      @delete="startDelete(toolbarSelectionEntries(toolbarTarget.side), toolbarTarget.side)"
      @toggle-dual-pane="dualPane = !dualPane"
      @mount="mountToolbarTarget"
      @open-settings="openSettings()"
      @bwlimit-click="openSettings('transfer')"
      @bwlimit-set="onBwlimitSave"
      @toggle-favorite="toggleFavorite(toolbarTarget.side)"
      @toggle-dock="(tab) => { const target = tab ?? dockTab; if (dockOpen && dockTab === target) dockOpen = false; else { dockOpen = true; dockTab = target; if (target === 'audit') auditRef?.refresh(); } }"
    />

    <div class="wb-content">
      <!-- 源栏（左栏）：双栏默认本地 __local__，单栏为当前连接 -->
      <section class="wb-pane wb-pane-source" @dragover.prevent @dragenter="dragOverSide = 'left'" @dragleave="dragOverSide = dragOverSide === 'left' ? null : dragOverSide" @drop.prevent="onDropTo('left', $event)">
        <!-- pane 顶条：双栏时放左栏连接选择（与右栏顶条等高对齐）；单栏时整行隐藏 -->
        <div v-if="dualPane" class="wb-pane-topbar">
          <select v-model="leftConnectionId" class="wb-target-connection" :aria-label="t('sourceConnection')" @change="onLeftConnectionChange">
            <option v-for="item in leftConnections" :key="item.id" :value="item.id">{{ item.name }}</option>
          </select>
        </div>
        <div class="wb-pane-body">
          <!-- 侧栏导航：tree（目录树，默认）/ quick（快捷目录）双 tab，可收起 -->
          <SideNavPanel
            side="left"
            :tab="leftSideTab"
            :collapsed="leftSideCollapsed"
            :tree-root="leftTree"
            :quick-paths="leftQuickPaths"
            :favorites="leftFavorites"
            :current-path="path"
            :usage="leftRemoteUsage"
            :t="t"
            @update:tab="leftSideTab = $event"
            @update:collapsed="leftSideCollapsed = $event"
            @navigate="navigateQuickPath('left', $event)"
            @toggle-node="expandTreeNode('left', $event)"
            @refresh-tree="refreshTree('left')"
            @node-context="openSideMenu('left', $event)"
          />
          <div class="wb-pane-main">
            <div class="wb-pane-header">
              <button class="wb-icon-button wb-icon-neutral" v-tip="t('up')" :disabled="!path || path === '/'" @click="onToolbarNavigate(parentPath(path))"><ArrowUp /></button>
              <button class="wb-icon-button wb-icon-neutral" v-tip="t('refresh')" :disabled="loading" @click="markActiveSide('left'); refreshDirectory()"><RefreshCw :class="{ 'wb-spin': loading }" /></button>
              <div class="wb-path-toolbar">
                <PathField :path="path" :t="t" @navigate="onToolbarNavigate" />
                <span class="wb-search-box">
                  <Search class="wb-search-icon" aria-hidden="true" />
                  <input
                    :value="searchQuery"
                    class="wb-search-input"
                    :placeholder="t('searchPlaceholder')"
                    :aria-label="t('searchLabel')"
                    type="search"
                    spellcheck="false"
                    @input="searchQuery = ($event.target as HTMLInputElement).value"
                    @keydown.down.prevent="onSearchKeydown('left', 'ArrowDown', ($event.target as HTMLInputElement).value)"
                    @keydown.up.prevent="onSearchKeydown('left', 'ArrowUp', ($event.target as HTMLInputElement).value)"
                    @keydown.enter.prevent="onSearchKeydown('left', 'Enter', ($event.target as HTMLInputElement).value)"
                    @keydown.esc.prevent="searchQuery = ''; closeDeepSearch()"
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
              :failed="listingFailed"
              :can-write="canWrite && !confirmOpen && !previewPath"
              :filtered="Boolean(searchQuery.trim())"
              :t="t"
              @update:selection="setPaneSelection('left', $event)"
              @update:active-path="setPaneActivePath('left', $event)"
              @open="(entry) => { markActiveSide('left'); openEntry(entry, 'left'); }"
              @delete="startDelete(toolbarSelectionEntries('left'), 'left')"
              @rename="startRename($event, 'left')"
              @retry="refreshDirectory"
              @contextmenu="openContextMenu('left', $event)"
              @blank-context="openBlank('left', $event)"
              @sort="(column) => sortRouted('left', column)"
              @batch-rename="openBatchRename('left')"
            />
          </div>
        </div>
        <div v-if="dragOverSide === 'left' && dualPane" class="wb-drop-overlay">{{ t("dropToCopy") }}</div>
      </section>

      <!-- 双栏桥：跨栏 copy/move 按钮（A-FILES ①）。R3-P2-1：copy 与 move 的写
           都发生在目标栏，同受 canWrite 门禁（只读态「复制到目标/源栏」禁用）。 -->
      <div v-if="dualPane" class="wb-pane-bridge">
        <button class="wb-icon-button wb-icon-neutral" v-tip="t('copyToTarget')" :disabled="!selection.length || !canWrite" @click="transferBetween('left', false)"><Copy /></button>
        <button class="wb-icon-button wb-icon-neutral" v-tip="t('moveToTarget')" :disabled="!selection.length || !canWrite" @click="transferBetween('left', true)"><ArrowRight /></button>
        <span class="wb-toolbar-separator" />
        <button class="wb-icon-button wb-icon-neutral" v-tip="t('copyToSource')" :disabled="!rightSelection.length || !canWrite" @click="transferBetween('right', false)"><Copy class="wb-flip-h" /></button>
        <button class="wb-icon-button wb-icon-neutral" v-tip="t('moveToSource')" :disabled="!rightSelection.length || !canWrite" @click="transferBetween('right', true)"><ArrowLeft /></button>
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
          <select v-model="targetConnectionId" class="wb-target-connection" :aria-label="t('targetConnection')" @change="markActiveSide('right'); loadRightDirectory(rightPath)">
            <option value="">{{ t("sameConnection") }}</option>
            <option v-for="item in targetConnections" :key="item.id" :value="item.id">{{ item.name }}</option>
          </select>
        </div>
        <div class="wb-pane-body">
          <SideNavPanel
            side="right"
            :tab="rightSideTab"
            :collapsed="rightSideCollapsed"
            :tree-root="rightTree"
            :quick-paths="rightQuickPaths"
            :favorites="rightFavorites"
            :current-path="rightPath"
            :usage="rightRemoteUsage"
            :t="t"
            @update:tab="rightSideTab = $event"
            @update:collapsed="rightSideCollapsed = $event"
            @navigate="navigateQuickPath('right', $event)"
            @toggle-node="expandTreeNode('right', $event)"
            @refresh-tree="refreshTree('right')"
            @node-context="openSideMenu('right', $event)"
          />
          <div class="wb-pane-main">
            <div class="wb-pane-header">
              <button class="wb-icon-button wb-icon-neutral" v-tip="t('up')" :disabled="!rightPath || rightPath === '/'" @click="onRightNavigate(parentPath(rightPath))"><ArrowUp /></button>
              <button class="wb-icon-button wb-icon-neutral" v-tip="t('refresh')" :disabled="rightLoading" @click="markActiveSide('right'); refreshRightDirectory()"><RefreshCw :class="{ 'wb-spin': rightLoading }" /></button>
              <div class="wb-path-toolbar">
                <PathField :path="rightPath" :t="t" @navigate="onRightNavigate" />
                <span class="wb-search-box">
                  <Search class="wb-search-icon" aria-hidden="true" />
                  <input
                    :value="rightSearchQuery"
                    class="wb-search-input"
                    :placeholder="t('searchPlaceholder')"
                    :aria-label="t('searchLabel')"
                    type="search"
                    spellcheck="false"
                    @input="rightSearchQuery = ($event.target as HTMLInputElement).value"
                    @keydown.down.prevent="onSearchKeydown('right', 'ArrowDown', ($event.target as HTMLInputElement).value)"
                    @keydown.up.prevent="onSearchKeydown('right', 'ArrowUp', ($event.target as HTMLInputElement).value)"
                    @keydown.enter.prevent="onSearchKeydown('right', 'Enter', ($event.target as HTMLInputElement).value)"
                    @keydown.esc.prevent="rightSearchQuery = ''; closeDeepSearch()"
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
              :failed="rightListingFailed"
              :can-write="canWrite && !confirmOpen && !previewPath"
              :filtered="Boolean(rightSearchQuery.trim())"
              :t="t"
              @update:selection="setPaneSelection('right', $event)"
              @update:active-path="setPaneActivePath('right', $event)"
              @open="(entry) => { markActiveSide('right'); openEntry(entry, 'right'); }"
              @delete="startDelete(toolbarSelectionEntries('right'), 'right')"
              @rename="startRename($event, 'right')"
              @retry="refreshRightDirectory"
              @contextmenu="openContextMenu('right', $event)"
              @blank-context="openBlank('right', $event)"
              @sort="(column) => sortRouted('right', column)"
              @batch-rename="openBatchRename('right')"
            />
          </div>
        </div>
        <div v-if="dragOverSide === 'right' && dualPane" class="wb-drop-overlay">{{ t("dropToCopy") }}</div>
      </section>

      <aside v-if="dockOpen" class="wb-dock">
        <!-- 审计#12：页签补 tablist/tab 语义 + roving tabindex + ←→ 循环切换。 -->
        <div class="wb-dock-tabs" role="tablist" @contextmenu.prevent @keydown="onTablistArrowKeys">
          <button role="tab" :aria-selected="dockTab === 'transfers'" :tabindex="dockTab === 'transfers' ? 0 : -1" :class="{ 'is-active': dockTab === 'transfers' }" @click="dockTab = 'transfers'">{{ t("transferPanel") }}</button>
          <button role="tab" :aria-selected="dockTab === 'stats'" :tabindex="dockTab === 'stats' ? 0 : -1" :class="{ 'is-active': dockTab === 'stats' }" @click="dockTab = 'stats'">{{ t("statsPanel") }}</button>
          <button role="tab" :aria-selected="dockTab === 'audit'" :tabindex="dockTab === 'audit' ? 0 : -1" :class="{ 'is-active': dockTab === 'audit' }" @click="dockTab = 'audit'">{{ t("auditPanel") }}</button>
          <button role="tab" :aria-selected="dockTab === 'connection'" :tabindex="dockTab === 'connection' ? 0 : -1" :class="{ 'is-active': dockTab === 'connection' }" @click="dockTab = 'connection'">{{ t("connectionPanel") }}</button>
        </div>
        <div class="wb-dock-body">
          <TransferPanel
            v-if="dockTab === 'transfers'"
            :jobs="transferJobs"
            :t="t"
            :retryable-ids="retryableTransferIds"
            @cancel="cancelTransfer"
            @clear-history="transferHistoryConfirmOpen = true"
            @retry="retryTransfer"
            @delete="deleteTransferRecord"
            @reveal="revealTransferTarget"
            @open="openTransferTarget"
            @open-app="openTransferWithApp"
          />
          <StatsPanel
            v-else-if="dockTab === 'stats'"
            :jobs="transferJobs"
            :usage="activeRemoteUsage"
            :bwlimit="bwlimitActive"
            :t="t"
          />
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

    <!-- 文件概览弹窗：来自任一栏的预览/压缩包列表；遮罩点击 / Esc / 关闭按钮均可关闭。
         审计#7：dialog 语义 + aria-modal + Tab 焦点陷阱；脏草稿关闭先确认。
         对标 rclone-dashboard：面板可拖拽缩放（右下握把）、可最小化为悬浮 pill
         （is-minimized 仅隐藏面板，PreviewPane 保持挂载，草稿不丢）。 -->
    <div
      v-if="previewPath"
      ref="previewOverlayEl"
      class="wb-preview-overlay"
      :class="{ 'is-minimized': previewMinimized }"
      role="dialog"
      aria-modal="true"
      :aria-label="previewTitle"
      :style="previewPanelStyle"
      @click.self="closePreview"
      @keydown="onPreviewTabKeydown"
    >
      <PreviewPane
        ref="previewRef"
        :path="previewPath"
        :can-write="canWrite"
        :appearance="appearance"
        :connection-id="previewConnectionId"
        :allow-minimize="true"
        :t="t"
        @close="closePreview"
        @saved="onPreviewSaved"
        @download="onPreviewDownload"
        @minimize="minimizePreview"
        @open-settings="openSettings('openWith')"
      />
      <div
        class="wb-preview-grip"
        aria-hidden="true"
        @pointerdown.prevent="onPreviewGripPointerdown"
      />
    </div>

    <!-- 最小化 pill：点击还原预览；仅预览存在时出现。 -->
    <button v-if="previewPath && previewMinimized" type="button" class="wb-preview-pill" :title="previewTitle" @click="restorePreview">
      <FileText aria-hidden="true" />
      <span>{{ previewTitle }}</span>
    </button>

    <!-- 挂载到本机：独立顶层弹窗（内嵌本机目录浏览器，直接选目录）。 -->
    <MountDialog
      v-if="mountDialogOpen"
      :t="t"
      @close="closeMountDialog"
      @confirm="onMountDialogConfirm"
    />

    <SyncDialog
      v-if="syncDialogOpen"
      :t="t"
      :kind="syncDialogKind"
      :source-path="syncDialogEntry?.path ?? ''"
      :default-target="syncDialogDraft"
      :bisync-state="syncDialogBisyncState"
      @close="closeSyncDialog"
      @confirm="onSyncDialogConfirm"
    />

    <!-- 打开方式：远程编辑本地副本（FinalShell 式），选默认/预设/手输应用 -->
    <OpenWithDialog
      v-if="openWithState"
      :t="t"
      :presets="appPresets"
      :initial-app="resolveOpenApp(openAppPrefs, openWithState.entry.name)"
      @close="closeOpenWithDialog"
      @confirm="onOpenWithConfirm"
    />

    <!-- 独立设置弹窗（对标 ssh 插件 settings-modal）：左侧分类导航 + 右侧内容
         面板，Esc/遮罩/关闭钮均可关闭；Tab 焦点陷阱同预览弹窗。dock 只保留
         transfers/audit/connection，设置不再挤在 dock 页签里。 -->
    <div
      v-if="settingsOpen"
      ref="settingsOverlayEl"
      class="wb-settings-backdrop"
      role="dialog"
      aria-modal="true"
      :aria-label="t('settings')"
      @click.self="closeSettings"
      @keydown="onSettingsTabKeydown"
    >
      <div class="wb-settings-modal" :style="settingsWinStyle">
        <header>
          <strong>{{ t("settings") }}</strong>
          <button class="wb-icon-button wb-icon-neutral" v-tip="t('close')" @click="closeSettings"><X /></button>
        </header>
        <div class="wb-settings-layout">
          <nav class="wb-settings-nav" aria-label="settings categories">
            <button
              v-for="cat in settingsCategories"
              :key="cat.id"
              type="button"
              class="wb-settings-nav-item"
              :class="{ 'is-active': settingsCategory === cat.id }"
              @click="settingsCategory = cat.id"
            ><component :is="cat.icon" class="wb-settings-nav-icon" /> {{ t(cat.labelKey) }}</button>
          </nav>
          <div class="wb-settings-content">
            <!-- 单实例常驻：切换分类不卸载组件，各区块未保存草稿得以保留。 -->
            <SettingsPanel
              v-if="settingsCategory !== 'mounts'"
              ref="settingsPanelRef"
              @dirty="settingsDirty = $event"
              :section="panelSection"
              :t="t"
              :can-save-local="canSaveLocal"
              :save-dir="saveDirDraft"
              :default-save-dir="localDownloadDir"
              :download-dir-error="saveDirError"
              :open-app="openAppPrefs"
              :open-app-error="openAppError"
              :presets="appPresets"
              :bwlimit="bwlimitDraft"
              :bwlimit-error="bwlimitError"
              @save-dir="onSaveDirChange"
              @save-open-app="onOpenAppPrefsChange"
              @save-bwlimit="onBwlimitSave"
            />
            <div v-else class="wb-settings-pane" :aria-busy="mountsLoading">
              <DesktopOnlyCard v-if="!canSaveLocal" :t="t" />
              <template v-else>
              <p class="wb-settings-help">{{ t("mounts.help") }}</p>
              <div class="wb-mounts-actions">
                <button class="wb-toolbar-button" :disabled="mountsLoading" @click="openMountDialog()"><HardDrive /> {{ t("mounts.mountNow") }}</button>
                <button class="wb-icon-button wb-icon-neutral" v-tip="t('refresh')" :disabled="mountsLoading" @click="loadMounts"><RefreshCw :class="{ 'wb-spin': mountsLoading }" /></button>
              </div>
              <p v-if="mountsError" class="wb-settings-error" role="alert">{{ t("mounts.loadFailed") }}</p>
              <p v-else-if="!mountsLoading && !mountsList.length" class="wb-settings-help">{{ t("mounts.empty") }}</p>
              <ul v-else-if="mountsList.length" class="wb-mounts-list">
                <li v-for="row in mountsList" :key="row.mountId">
                  <div class="wb-mounts-main">
                    <strong>{{ mountStrategyLabel(row.strategy) }}</strong>
                    <span class="wb-mono">{{ row.mountPoint ?? `:${row.gatewayPort ?? ""}` }}</span>
                    <span v-if="mountStatsText(row.mountId)" class="wb-mounts-stats">{{ mountStatsText(row.mountId) }}</span>
                    <span v-if="row.mounted === false" class="wb-settings-error">{{ t("mounts.stale") }}</span>
                  </div>
                  <span class="wb-mounts-badge">{{ t("mounts.readOnly") }}</span>
                  <button class="wb-icon-button wb-icon-neutral" :disabled="refreshBusyId === row.mountId" v-tip="t('mountRefresh.button')" @click="refreshMountCache(row)"><RefreshCw /></button>
                  <button class="wb-icon-button wb-icon-neutral" :disabled="unmountBusyId === row.mountId" v-tip="t('mounts.unmount')" @click="unmountMount(row)"><Eject /></button>
                </li>
              </ul>
              </template>
              <!-- 本机共享（files/serve/*）：挂载列表下方常驻区块 -->
              <p class="wb-settings-help wb-shares-title">{{ t("shareSectionTitle") }}</p>
              <div class="wb-mounts-actions">
                <button class="wb-icon-button wb-icon-neutral" v-tip="t('refresh')" :disabled="sharesLoading" @click="loadShares"><RefreshCw :class="{ 'wb-spin': sharesLoading }" /></button>
              </div>
              <p v-if="sharesError" class="wb-settings-error" role="alert">{{ t("operationFailed", { error: sharesError }) }}</p>
              <p v-else-if="!sharesLoading && !sharesList.length" class="wb-settings-help">{{ t("shareEmpty") }}</p>
              <ul v-else-if="sharesList.length" class="wb-shares-list">
                <li v-for="row in sharesList" :key="row.serveId">
                  <div class="wb-mounts-main">
                    <strong>{{ row.serveType.toUpperCase() }}</strong>
                    <span class="wb-mono">{{ row.url }}</span>
                  </div>
                  <button class="wb-icon-button wb-icon-neutral" :disabled="shareBusyId === row.serveId" v-tip="t('shareStop')" @click="stopShare(row)"><X /></button>
                </li>
              </ul>
            </div>
          </div>
          <!-- 统一保存：任一区块有未保存修改时点亮；成功通知由各链路自发。 -->
          <footer class="wb-settings-footer">
            <span class="wb-muted">{{ settingsDirty ? t("settingsUnsavedHint") : "" }}</span>
            <button class="wb-toolbar-button wb-settings-save" type="button" :disabled="!settingsDirty || settingsSaving" @click="onSettingsSave">{{ t("settingsSave") }}</button>
          </footer>
        <!-- 右下角拉伸柄：拖动调尺寸，松手即记忆（prefs.settingsWin）。 -->
        <div class="wb-settings-grip" aria-hidden="true" @pointerdown="onSettingsGripPointerdown"></div>
        </div>
      </div>
    </div>

    <!-- 统一右键菜单（A-FILES ④b）：源栏/目标栏共用；多选时切批量动作面。
         R3-P2-8：role="menu"/menuitem 语义。 -->
    <div v-if="contextMenu" ref="menuEl" class="wb-context-menu" role="menu" :style="{ left: `${contextMenu.x}px`, top: `${contextMenu.y}px` }" @click.stop @keydown="onMenuArrowKeys">
      <template v-if="contextMenu.selection.length > 1">
        <button role="menuitem" @click="menuAction('open')"><FolderOpen /> {{ t("openDirectory") }}</button>
        <!-- 打包下载：仅当所选全部为目录（parity-tools）。 -->
        <button v-if="contextMenuAllDirs" role="menuitem" @click="menuAction('archiveDownloadSelected')"><FileArchive /> {{ t("archiveDownloadSelected", { count: contextMenu.selection.length }) }}</button>
        <button role="menuitem" @click="menuAction('downloadSelected')"><Download /> {{ t("downloadSelected") }}</button>
        <!-- R3-P2-1：只读态「复制到目标栏」与 move 同受 canWrite 门禁（写发生在目标栏）。 -->
        <button v-if="dualPane && canWrite" role="menuitem" @click="menuAction('copySelected')"><Copy /> {{ t("copyToTarget") }}</button>
        <button v-if="dualPane && canWrite" role="menuitem" @click="menuAction('moveSelected')"><FolderInput /> {{ t("moveToTarget") }}</button>
        <button v-if="canWrite" role="menuitem" @click="menuAction('compressSelected')"><FileArchive /> {{ t("compressSelected", { count: contextMenu.selection.length }) }}</button>
        <!-- 批量重命名（parity-tools）：只读态禁用，与删除项同一门禁形态。 -->
        <button role="menuitem" :disabled="!canWrite" @click="menuAction('batchRenameSelected')"><Pencil /> {{ t("batchRenameMenu") }}</button>
        <hr />
        <button role="menuitem" class="is-danger" :disabled="!canWrite" @click="menuAction('deleteSelected')"><Trash2 /> {{ t("deleteSelected") }}</button>
        <hr />
        <button role="menuitem" @click="menuAction('copyPath')"><Link2 /> {{ t("copyPath") }}</button>
      </template>
      <template v-else>
        <!-- 常用置顶：打开/预览/下载 → 编辑变换 → 分析校验 → 同步导入分享 → 维护 → 删除独立危险区 → 剪贴板。 -->
        <button v-if="contextMenu.entry.kind === 'directory'" role="menuitem" @click="menuAction('open')"><FolderOpen /> {{ t("openDirectory") }}</button>
        <button v-if="contextMenu.entry.kind === 'file' && !isArchivePath(contextMenu.entry.path)" role="menuitem" @click="menuAction('preview')"><Eye /> {{ t("preview") }}</button>
        <!-- 打开方式（桌面端）：远程编辑本地副本，选默认/预设/手输应用 -->
        <button v-if="contextMenu.entry.kind === 'file' && canSaveLocal && !isArchivePath(contextMenu.entry.path)" role="menuitem" @click="menuAction('openWith')"><ExternalLink /> {{ t("openWithMenu") }}</button>
        <button v-if="contextMenu.entry.kind === 'file' && isArchivePath(contextMenu.entry.path)" role="menuitem" @click="menuAction('archiveContents')"><Archive /> {{ t("archiveContents") }}</button>
        <button v-if="contextMenu.entry.kind === 'file'" role="menuitem" @click="menuAction('download')"><Download /> {{ t("download") }}</button>
        <!-- 打包下载（parity-tools）：目录条目的「压缩包下载」动作。 -->
        <button v-if="contextMenu.entry.kind === 'directory'" role="menuitem" @click="menuAction('archiveDownload')"><FileArchive /> {{ t("archiveDownloadMenu") }}</button>
        <hr />
        <button v-if="canWrite" role="menuitem" @click="menuAction('rename')"><Pencil /> {{ t("rename") }}</button>
        <button v-if="canWrite" role="menuitem" @click="menuAction('copy')"><Copy /> {{ t("transferKind.copy") }}…</button>
        <button v-if="canWrite" role="menuitem" @click="menuAction('move')"><FolderInput /> {{ t("transferKind.move") }}…</button>
        <button v-if="contextMenu.entry.kind === 'file' && isArchivePath(contextMenu.entry.path) && canWrite" role="menuitem" @click="menuAction('extract')"><FileOutput /> {{ t("extractTo") }}</button>
        <button v-if="canWrite" role="menuitem" @click="menuAction('compress')"><FileArchive /> {{ t("compress") }}</button>
        <hr />
        <button v-if="contextMenu.entry.kind === 'directory'" role="menuitem" @click="menuAction('computeSize')"><Calculator /> {{ t("computeSize") }}</button>
        <button v-if="contextMenu.entry.kind === 'directory' && canWrite" role="menuitem" @click="menuAction('hashsum')"><FileCheck /> {{ t("hashsumMenu") }}</button>
        <button v-if="contextMenu.entry.kind === 'directory'" role="menuitem" @click="menuAction('checkDir')"><Scale /> {{ t("checkDirMenu") }}</button>
        <button v-if="isSumFile(contextMenu.entry)" role="menuitem" @click="menuAction('verifySum')"><ShieldCheck /> {{ t("verifySumMenu") }}</button>
        <button v-if="contextMenu.entry.kind === 'directory' && canWrite" role="menuitem" @click="menuAction('syncDir')"><ArrowRightLeft /> {{ t("transferKind.syncDir") }}…</button>
        <button v-if="contextMenu.entry.kind === 'directory' && canWrite" role="menuitem" @click="menuAction('copyDir')"><FolderSymlink /> {{ t("transferKind.copyDir") }}…</button>
        <button v-if="contextMenu.entry.kind === 'directory' && canWrite" role="menuitem" @click="menuAction('bisyncDir')"><ArrowRightLeft /> {{ t("bisyncMenu") }}…</button>
        <button v-if="contextMenu.entry.kind === 'directory' && canWrite" role="menuitem" @click="menuAction('copyurl')"><Link /> {{ t("copyurlMenu") }}</button>
        <hr />
        <button v-if="contextMenu.entry.kind === 'directory' && canWrite" role="menuitem" @click="menuAction('rmdirs')"><FolderMinus /> {{ t("rmdirsMenu") }}</button>
        <button v-if="canUseMount && contextMenu.entry.kind === 'directory'" role="menuitem" @click="menuAction('mountLocal')"><HardDrive /> {{ t("mountToLocal") }}</button>
        <button v-if="contextMenu.entry.kind === 'directory'" role="menuitem" @click="menuAction('serveHttp')"><Globe /> {{ t("shareHttpMenu") }}</button>
        <button v-if="contextMenu.entry.kind === 'directory'" role="menuitem" @click="menuAction('serveWebdav')"><Share2 /> {{ t("shareWebdavMenu") }}</button>
        <hr />
        <button role="menuitem" class="is-danger" :disabled="!canWrite" @click="menuAction('delete')"><Trash2 /> {{ t("delete") }}</button>
        <hr />
        <button role="menuitem" @click="menuAction('copyPath')"><Link2 /> {{ t("copyPath") }}</button>
        <button role="menuitem" @click="menuAction('copyName')"><FileText /> {{ t("copyName") }}</button>
        <button v-if="capabilities?.presign" role="menuitem" @click="menuAction('copyPublicLink')"><Link /> {{ t("copyPublicLink") }}</button>
      </template>
    </div>

    <!-- 空白区右键菜单（P-FILES）：拦截浏览器默认菜单，给出新建/刷新动作 -->
    <div v-if="blankMenu" ref="menuEl" class="wb-context-menu" role="menu" :style="{ left: `${blankMenu.x}px`, top: `${blankMenu.y}px` }" @click.stop @keydown="onMenuArrowKeys">
      <button :disabled="!canWrite" role="menuitem" @click="blankMenuAction('newFolder')"><FolderPlus /> {{ t("newFolder") }}</button>
      <button :disabled="!canWrite" role="menuitem" @click="blankMenuAction('newFile')"><FilePlus /> {{ t("newFileTitle") }}</button>
      <hr />
      <button role="menuitem" @click="blankMenuAction('refresh')"><RefreshCw /> {{ t("refresh") }}</button>
      <hr />
      <button role="menuitem" @click="blankMenuAction('shortcuts')"><Keyboard /> {{ t("shortcutsMenu") }}</button>
      <hr />
      <button role="menuitem" @click="blankMenuAction('cleanup')"><Trash2 /> {{ t("cleanupMenu") }}</button>
    </div>

    <!-- 侧栏右键菜单（P-FILES）：目录树/快捷目录行 → 打开 / 在另一栏打开 / 复制 -->
    <div v-if="sideMenu" ref="menuEl" class="wb-context-menu" role="menu" :style="{ left: `${sideMenu.x}px`, top: `${sideMenu.y}px` }" @click.stop @keydown="onMenuArrowKeys">
      <button role="menuitem" @click="sideMenuAction('open')"><FolderOpen /> {{ t("openDirectory") }}</button>
      <button v-if="dualPane" role="menuitem" @click="sideMenuAction('openOther')">
        <PanelRight v-if="sideMenu.side === 'left'" />
        <PanelLeft v-else />
        {{ sideMenu.side === "left" ? t("openInRight") : t("openInLeft") }}
      </button>
      <!-- 收藏切换（rclone-ui parity）：树行/快捷目录行按当前收藏态切文案；fav 行恒为移除。 -->
      <button role="menuitem" @click="sideMenuAction('toggleFav')">
        <StarOff v-if="sideMenuFavorited" />
        <Star v-else />
        {{ sideMenuFavorited ? t("favRemove") : t("favAdd") }}
      </button>
      <hr />
      <button role="menuitem" @click="sideMenuAction('copyPath')"><Link2 /> {{ t("copyPath") }}</button>
      <button role="menuitem" @click="sideMenuAction('copyName')"><FileText /> {{ t("copyName") }}</button>
      <hr />
      <button v-if="canUseMount" role="menuitem" @click="sideMenuAction('mountLocal')"><HardDrive /> {{ t("mountToLocal") }}</button>
    </div>

    <!-- 深度搜索结果（files/search）：点击行跳到该文件所在目录 -->
    <div v-if="deepSearchOpen" class="wb-deepsearch" role="dialog" :aria-label="t('deepSearchTitle')">
      <header>
        <strong>{{ t("deepSearchTitle") }}</strong>
        <button class="wb-icon-button wb-icon-neutral" v-tip="t('close')" @click="closeDeepSearch"><X /></button>
      </header>
      <p v-if="deepSearchBusy" class="wb-muted">{{ t("loading") }}</p>
      <p v-else-if="!deepSearchResults.length" class="wb-muted">{{ t("deepSearchNoResults") }}</p>
      <template v-else>
        <ul class="wb-deepsearch-list">
          <li v-for="(entry, index) in deepSearchResults" :key="entry.path">
            <button type="button" :class="{ 'is-active': deepSearchIndex === index }" @click="openDeepSearchResult(entry)">
              <span class="wb-mono">{{ entry.path }}</span>
              <span class="wb-muted">{{ formatBytes(entry.size) }}</span>
            </button>
          </li>
        </ul>
        <p v-if="deepSearchTruncated" class="wb-muted">{{ t("deepSearchTruncated") }}</p>
      </template>
    </div>

    <ConfirmDialog
      :open="confirmOpen"
      :title="confirmTitleText"
      :body="confirmBodyText"
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
        <span v-else-if="confirmKind === 'copyurl'">{{ t("copyurlUrlLabel") }}</span>
        <span v-else>{{ t("pathPlaceholder") }}</span>
        <input v-model="confirmDraft" spellcheck="false" @keydown.enter.prevent="!confirmDanger && onConfirm()" />
      </label>
    </ConfirmDialog>

    <!-- 跨栏拖放动作选择（rclone-ui parity）：复制/移动二选一，确认后才执行
         传输（冲突预检/覆盖确认在既有 transferBetween 链路内）。 -->
    <DropActionDialog
      :open="dropActionOpen"
      :target-path="dropActionPending?.destPath ?? ''"
      :count="dropActionPending?.list.length ?? 0"
      :t="t"
      @choose="onDropActionChoose"
      @cancel="onDropActionCancel"
    />

    <!-- 审计中#15：清空传输历史二次确认（危险度低于删文件，无需 danger 态）。 -->
    <ConfirmDialog
      :open="transferHistoryConfirmOpen"
      :title="t('clearHistoryTitle')"
      :body="t('clearHistoryBody')"
      :confirm-label="t('clearHistory')"
      :cancel-label="t('cancel')"
      @confirm="transferHistoryConfirmOpen = false; clearTransferHistory()"
      @cancel="transferHistoryConfirmOpen = false"
    />

    <!-- 审计#7：脏草稿关闭确认——danger 态首焦点落「继续编辑」（安全项），
         Enter 不会一步丢稿。 -->
    <ConfirmDialog
      :open="previewDiscardOpen"
      :title="t('previewDiscardTitle')"
      :body="t('previewDiscardBody')"
      :danger="true"
      :confirm-label="t('discard')"
      :cancel-label="t('keepEditing')"
      @confirm="previewDiscardOpen = false; previewPath = null"
      @cancel="previewDiscardOpen = false"
    />

    <!-- 批量重命名抽屉（parity-tools）：多选右键 / 列表底部按钮入口；
         连接在打开时按发起栏固化（双栏下左栏可为本地连接）。 -->
    <BatchRenameDrawer
      v-if="batchRenameOpen"
      :entries="batchRenameEntries"
      :sibling-names="batchRenameSiblingNames"
      :connection-id="sideConnectionId(batchRenameSide) ?? connectionId"
      :t="t"
      @close="closeBatchRename"
      @applied="onBatchRenameApplied"
    />

    <!-- 快捷键速查（parity-tools）：`?` / 空白区右键入口，Esc 关闭。 -->
    <ShortcutsHelp v-if="shortcutsOpen" :t="t" @close="shortcutsOpen = false" />
  </main>
</template>
